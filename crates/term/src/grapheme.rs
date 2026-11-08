//! Emoji grapheme clusters in one cell.
//!
//! alacritty_terminal gives every char its own width, so 😵‍💫 (😵 + ZWJ + 💫) takes 4 cells.
//! Apps that count graphemes (Claude Code's Ink, anything using string-width) put it in 2, and
//! every cell after it lands 2 columns off. [`GraphemeTerm`] sits between the parser and the
//! [`Term`] and folds the rest of a cluster into the cell of its first char: ZWJ joins, skin-tone
//! modifiers, U+FE0F (which widens a narrow emoji to 2 cells) and regional-indicator flag pairs.
//! The renderer already draws a cell's zero-width chars with it, so the font shapes one glyph.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use unicode_width::UnicodeWidthChar;

const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';
/// Stand-in written to make alacritty lay out a 2-cell slot; replaced by the real base char.
const WIDE_PLACEHOLDER: char = '\u{3000}';

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Emoji,
    Regional,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Last {
    base: char,
    kind: Kind,
    after_zwj: bool,
    paired: bool,
}

/// What the last printed char was. Lives across reads, since a cluster can be split between
/// two PTY reads.
#[derive(Default, Debug)]
pub struct Graphemes {
    last: Option<Last>,
}

/// A [`Handler`] that forwards to `term` and joins emoji clusters on `input`. Every other
/// handler call ends the current cluster.
pub struct GraphemeTerm<'a, T> {
    term: &'a mut Term<T>,
    state: &'a mut Graphemes,
}

impl<'a, T> GraphemeTerm<'a, T> {
    pub fn new(term: &'a mut Term<T>, state: &'a mut Graphemes) -> Self {
        Self { term, state }
    }
}

fn kind(c: char) -> Kind {
    if is_regional(c) {
        Kind::Regional
    } else if is_pictographic(c) {
        Kind::Emoji
    } else {
        Kind::Other
    }
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// Close to Unicode's Extended_Pictographic: the chars that start or join an emoji sequence.
fn is_pictographic(c: char) -> bool {
    matches!(c,
        '\u{A9}' | '\u{AE}' | '\u{203C}' | '\u{2049}' | '\u{2122}' | '\u{2139}'
        | '\u{2194}'..='\u{21AA}'
        | '\u{231A}'..='\u{23FF}'
        | '\u{24C2}'
        | '\u{25AA}'..='\u{25FE}'
        | '\u{2600}'..='\u{27BF}'
        | '\u{2934}' | '\u{2935}'
        | '\u{2B05}'..='\u{2B55}'
        | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}'
        | '\u{1F000}'..='\u{1FAFF}')
        && !is_regional(c)
        && !is_skin_tone(c)
}

impl<T: EventListener> GraphemeTerm<'_, T> {
    /// The cell of the last printed char, if the cursor still sits right after it.
    fn last_point(&self, last: Last) -> Option<Point> {
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let mut point = cursor.point;
        if !cursor.input_needs_wrap {
            point.column = Column(point.column.0.checked_sub(1)?);
        }
        if grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column = Column(point.column.0.checked_sub(1)?);
        }
        (grid[point].c == last.base).then_some(point)
    }

    fn attach(&mut self, point: Point, c: char) {
        self.term.grid_mut()[point].push_zerowidth(c);
    }

    /// Adds `extra` to the cell at `point` and makes it 2 cells wide if it was narrow. Returns
    /// false (and changes nothing) in insert mode, where re-laying the cell would shift the line.
    fn widen(&mut self, point: Point, extra: char) -> bool {
        let grid = self.term.grid_mut();
        if grid[point].flags.contains(Flags::WIDE_CHAR) {
            grid[point].push_zerowidth(extra);
            return true;
        }
        if self.term.mode().contains(TermMode::INSERT) {
            return false;
        }
        let grid = self.term.grid_mut();
        let base = grid[point].c;
        let marks = grid[point]
            .zerowidth()
            .map(<[char]>::to_vec)
            .unwrap_or_default();
        // Rewrite the slot as a wide char: alacritty places the spacer and wraps if needed.
        grid.cursor.point = point;
        grid.cursor.input_needs_wrap = false;
        Handler::input(self.term, WIDE_PLACEHOLDER);
        let grid = self.term.grid_mut();
        let back = if grid.cursor.input_needs_wrap { 1 } else { 2 };
        let mut at = grid.cursor.point;
        let Some(column) = at.column.0.checked_sub(back) else {
            return true;
        };
        at.column = Column(column);
        let cell = &mut grid[at];
        if cell.c != WIDE_PLACEHOLDER || !cell.flags.contains(Flags::WIDE_CHAR) {
            return true;
        }
        cell.c = base;
        for mark in marks {
            cell.push_zerowidth(mark);
        }
        cell.push_zerowidth(extra);
        true
    }

    /// Folds `c` into the last cluster when it continues it.
    fn join(&mut self, c: char) -> bool {
        let Some(mut last) = self.state.last else {
            return false;
        };
        let Some(point) = self.last_point(last) else {
            self.state.last = None;
            return false;
        };
        let joined = if c == ZWJ && last.kind == Kind::Emoji {
            self.attach(point, c);
            last.after_zwj = true;
            true
        } else if last.after_zwj && is_pictographic(c) {
            last.after_zwj = false;
            self.widen(point, c)
        } else if (c == VS16 || is_skin_tone(c)) && last.kind == Kind::Emoji {
            self.widen(point, c)
        } else if is_regional(c) && last.kind == Kind::Regional && !last.paired {
            last.paired = self.widen(point, c);
            last.paired
        } else if c.width() == Some(0) {
            self.attach(point, c);
            true
        } else {
            false
        };
        if joined {
            self.state.last = Some(last);
        }
        joined
    }
}

impl<T: EventListener> Handler for GraphemeTerm<'_, T> {
    fn input(&mut self, c: char) {
        if self.join(c) {
            return;
        }
        Handler::input(self.term, c);
        if c.width().is_some_and(|w| w > 0) {
            self.state.last = Some(Last {
                base: c,
                kind: kind(c),
                after_zwj: false,
                paired: false,
            });
        }
    }

    fn set_title(&mut self, a0: Option<String>) {
        self.state.last = None;
        Handler::set_title(self.term, a0);
    }
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) {
        self.state.last = None;
        Handler::set_cursor_style(self.term, a0);
    }
    fn set_cursor_shape(&mut self, a0: CursorShape) {
        self.state.last = None;
        Handler::set_cursor_shape(self.term, a0);
    }
    fn goto(&mut self, a0: i32, a1: usize) {
        self.state.last = None;
        Handler::goto(self.term, a0, a1);
    }
    fn goto_line(&mut self, a0: i32) {
        self.state.last = None;
        Handler::goto_line(self.term, a0);
    }
    fn goto_col(&mut self, a0: usize) {
        self.state.last = None;
        Handler::goto_col(self.term, a0);
    }
    fn insert_blank(&mut self, a0: usize) {
        self.state.last = None;
        Handler::insert_blank(self.term, a0);
    }
    fn move_up(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_up(self.term, a0);
    }
    fn move_down(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_down(self.term, a0);
    }
    fn identify_terminal(&mut self, a0: Option<char>) {
        self.state.last = None;
        Handler::identify_terminal(self.term, a0);
    }
    fn device_status(&mut self, a0: usize) {
        self.state.last = None;
        Handler::device_status(self.term, a0);
    }
    fn move_forward(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_forward(self.term, a0);
    }
    fn move_backward(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_backward(self.term, a0);
    }
    fn move_down_and_cr(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_down_and_cr(self.term, a0);
    }
    fn move_up_and_cr(&mut self, a0: usize) {
        self.state.last = None;
        Handler::move_up_and_cr(self.term, a0);
    }
    fn put_tab(&mut self, a0: u16) {
        self.state.last = None;
        Handler::put_tab(self.term, a0);
    }
    fn backspace(&mut self) {
        self.state.last = None;
        Handler::backspace(self.term);
    }
    fn carriage_return(&mut self) {
        self.state.last = None;
        Handler::carriage_return(self.term);
    }
    fn linefeed(&mut self) {
        self.state.last = None;
        Handler::linefeed(self.term);
    }
    fn bell(&mut self) {
        self.state.last = None;
        Handler::bell(self.term);
    }
    fn substitute(&mut self) {
        self.state.last = None;
        Handler::substitute(self.term);
    }
    fn newline(&mut self) {
        self.state.last = None;
        Handler::newline(self.term);
    }
    fn set_horizontal_tabstop(&mut self) {
        self.state.last = None;
        Handler::set_horizontal_tabstop(self.term);
    }
    fn scroll_up(&mut self, a0: usize) {
        self.state.last = None;
        Handler::scroll_up(self.term, a0);
    }
    fn scroll_down(&mut self, a0: usize) {
        self.state.last = None;
        Handler::scroll_down(self.term, a0);
    }
    fn insert_blank_lines(&mut self, a0: usize) {
        self.state.last = None;
        Handler::insert_blank_lines(self.term, a0);
    }
    fn delete_lines(&mut self, a0: usize) {
        self.state.last = None;
        Handler::delete_lines(self.term, a0);
    }
    fn erase_chars(&mut self, a0: usize) {
        self.state.last = None;
        Handler::erase_chars(self.term, a0);
    }
    fn delete_chars(&mut self, a0: usize) {
        self.state.last = None;
        Handler::delete_chars(self.term, a0);
    }
    fn move_backward_tabs(&mut self, a0: u16) {
        self.state.last = None;
        Handler::move_backward_tabs(self.term, a0);
    }
    fn move_forward_tabs(&mut self, a0: u16) {
        self.state.last = None;
        Handler::move_forward_tabs(self.term, a0);
    }
    fn save_cursor_position(&mut self) {
        self.state.last = None;
        Handler::save_cursor_position(self.term);
    }
    fn restore_cursor_position(&mut self) {
        self.state.last = None;
        Handler::restore_cursor_position(self.term);
    }
    fn clear_line(&mut self, a0: LineClearMode) {
        self.state.last = None;
        Handler::clear_line(self.term, a0);
    }
    fn clear_screen(&mut self, a0: ClearMode) {
        self.state.last = None;
        Handler::clear_screen(self.term, a0);
    }
    fn clear_tabs(&mut self, a0: TabulationClearMode) {
        self.state.last = None;
        Handler::clear_tabs(self.term, a0);
    }
    fn set_tabs(&mut self, a0: u16) {
        self.state.last = None;
        Handler::set_tabs(self.term, a0);
    }
    fn reset_state(&mut self) {
        self.state.last = None;
        Handler::reset_state(self.term);
    }
    fn reverse_index(&mut self) {
        self.state.last = None;
        Handler::reverse_index(self.term);
    }
    fn terminal_attribute(&mut self, a0: Attr) {
        self.state.last = None;
        Handler::terminal_attribute(self.term, a0);
    }
    fn set_mode(&mut self, a0: Mode) {
        self.state.last = None;
        Handler::set_mode(self.term, a0);
    }
    fn unset_mode(&mut self, a0: Mode) {
        self.state.last = None;
        Handler::unset_mode(self.term, a0);
    }
    fn report_mode(&mut self, a0: Mode) {
        self.state.last = None;
        Handler::report_mode(self.term, a0);
    }
    fn set_private_mode(&mut self, a0: PrivateMode) {
        self.state.last = None;
        Handler::set_private_mode(self.term, a0);
    }
    fn unset_private_mode(&mut self, a0: PrivateMode) {
        self.state.last = None;
        Handler::unset_private_mode(self.term, a0);
    }
    fn report_private_mode(&mut self, a0: PrivateMode) {
        self.state.last = None;
        Handler::report_private_mode(self.term, a0);
    }
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) {
        self.state.last = None;
        Handler::set_scrolling_region(self.term, a0, a1);
    }
    fn set_keypad_application_mode(&mut self) {
        self.state.last = None;
        Handler::set_keypad_application_mode(self.term);
    }
    fn unset_keypad_application_mode(&mut self) {
        self.state.last = None;
        Handler::unset_keypad_application_mode(self.term);
    }
    fn set_active_charset(&mut self, a0: CharsetIndex) {
        self.state.last = None;
        Handler::set_active_charset(self.term, a0);
    }
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) {
        self.state.last = None;
        Handler::configure_charset(self.term, a0, a1);
    }
    fn set_color(&mut self, a0: usize, a1: Rgb) {
        self.state.last = None;
        Handler::set_color(self.term, a0, a1);
    }
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) {
        self.state.last = None;
        Handler::dynamic_color_sequence(self.term, a0, a1, a2);
    }
    fn reset_color(&mut self, a0: usize) {
        self.state.last = None;
        Handler::reset_color(self.term, a0);
    }
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) {
        self.state.last = None;
        Handler::clipboard_store(self.term, a0, a1);
    }
    fn clipboard_load(&mut self, a0: u8, a1: &str) {
        self.state.last = None;
        Handler::clipboard_load(self.term, a0, a1);
    }
    fn decaln(&mut self) {
        self.state.last = None;
        Handler::decaln(self.term);
    }
    fn push_title(&mut self) {
        self.state.last = None;
        Handler::push_title(self.term);
    }
    fn pop_title(&mut self) {
        self.state.last = None;
        Handler::pop_title(self.term);
    }
    fn text_area_size_pixels(&mut self) {
        self.state.last = None;
        Handler::text_area_size_pixels(self.term);
    }
    fn text_area_size_chars(&mut self) {
        self.state.last = None;
        Handler::text_area_size_chars(self.term);
    }
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) {
        self.state.last = None;
        Handler::set_hyperlink(self.term, a0);
    }
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) {
        self.state.last = None;
        Handler::set_mouse_cursor_icon(self.term, a0);
    }
    fn report_keyboard_mode(&mut self) {
        self.state.last = None;
        Handler::report_keyboard_mode(self.term);
    }
    fn push_keyboard_mode(&mut self, a0: KeyboardModes) {
        self.state.last = None;
        Handler::push_keyboard_mode(self.term, a0);
    }
    fn pop_keyboard_modes(&mut self, a0: u16) {
        self.state.last = None;
        Handler::pop_keyboard_modes(self.term, a0);
    }
    fn set_keyboard_mode(&mut self, a0: KeyboardModes, a1: KeyboardModesApplyBehavior) {
        self.state.last = None;
        Handler::set_keyboard_mode(self.term, a0, a1);
    }
    fn set_modify_other_keys(&mut self, a0: ModifyOtherKeys) {
        self.state.last = None;
        Handler::set_modify_other_keys(self.term, a0);
    }
    fn report_modify_other_keys(&mut self) {
        self.state.last = None;
        Handler::report_modify_other_keys(self.term);
    }
    fn set_scp(&mut self, a0: ScpCharPath, a1: ScpUpdateMode) {
        self.state.last = None;
        Handler::set_scp(self.term, a0, a1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TermSize;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn run(cols: usize, chunks: &[&str]) -> Term<VoidListener> {
        let mut term = Term::new(
            Config::default(),
            &TermSize::new(cols as u16, 3),
            VoidListener,
        );
        let mut state = Graphemes::default();
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for chunk in chunks {
            parser.advance(
                &mut GraphemeTerm::new(&mut term, &mut state),
                chunk.as_bytes(),
            );
        }
        term
    }

    /// Column where `abc` starts on `line`.
    fn abc_column(term: &Term<VoidListener>, line: i32) -> usize {
        let row = &term.grid()[Line(line)];
        (0..term.columns())
            .find(|&c| row[Column(c)].c == 'a')
            .expect("abc on the line")
    }

    /// The first cell's char plus its zero-width chars.
    fn first_cell(term: &Term<VoidListener>, line: i32) -> (String, bool) {
        let cell = &term.grid()[Line(line)][Column(0)];
        let mut text = String::from(cell.c);
        text.extend(cell.zerowidth().unwrap_or_default());
        (text, cell.flags.contains(Flags::WIDE_CHAR))
    }

    fn assert_cluster(seq: &str) {
        let term = run(20, &[&format!("{seq}abc\r\n😊abc")]);
        assert_eq!(abc_column(&term, 0), 2, "{seq:?} takes 2 cells");
        assert_eq!(abc_column(&term, 1), 2);
        assert_eq!(first_cell(&term, 0), (seq.to_string(), true));
    }

    #[test]
    fn zwj_sequence_is_one_wide_cell() {
        assert_cluster("😵\u{200D}💫");
        // Family: three joins.
        assert_cluster("👨\u{200D}👩\u{200D}👧\u{200D}👦");
    }

    #[test]
    fn zwj_with_narrow_part_and_vs16() {
        // Woman running: 🏃 ZWJ ♀ FE0F.
        assert_cluster("🏃\u{200D}♀\u{FE0F}");
        // Rainbow flag: 🏳 FE0F ZWJ 🌈 (🏳 alone is narrow).
        assert_cluster("🏳\u{FE0F}\u{200D}🌈");
    }

    #[test]
    fn vs16_widens_a_narrow_emoji() {
        assert_cluster("❤\u{FE0F}");
        assert_cluster("☀\u{FE0F}");
    }

    #[test]
    fn skin_tone_joins_its_emoji() {
        assert_cluster("👍🏽");
        assert_cluster("👩🏽\u{200D}💻");
    }

    #[test]
    fn flag_pair_is_one_wide_cell() {
        assert_cluster("🇯🇵");
        // Two flags in a row: 4 cells.
        let term = run(20, &["🇯🇵🇫🇷abc"]);
        assert_eq!(abc_column(&term, 0), 4);
    }

    #[test]
    fn cluster_split_across_reads() {
        let term = run(20, &["😵\u{200D}", "💫abc"]);
        assert_eq!(abc_column(&term, 0), 2);
    }

    #[test]
    fn cursor_move_ends_the_cluster() {
        // A cursor move between ZWJ and 💫 starts a new cell.
        let term = run(20, &["😵\u{200D}\x1b[C💫abc"]);
        assert_eq!(abc_column(&term, 0), 5);
    }

    #[test]
    fn widening_at_the_last_column_wraps() {
        // ❤ lands in the last column; FE0F moves it to the next line as a wide char.
        let term = run(5, &["1234❤\u{FE0F}abc"]);
        assert_eq!(first_cell(&term, 1), ("❤\u{FE0F}".to_string(), true));
        assert_eq!(abc_column(&term, 1), 2);
    }

    #[test]
    fn cjk_and_plain_text_unchanged() {
        let term = run(20, &["中文abc\r\nxyabc\r\ne\u{301}abc"]);
        assert_eq!(abc_column(&term, 0), 4);
        assert_eq!(abc_column(&term, 1), 2);
        assert_eq!(
            abc_column(&term, 2),
            1,
            "combining accent stays in its cell"
        );
        // ZWJ after a non-emoji does not join the next char.
        let term = run(20, &["a\u{200D}😊abc"]);
        assert_eq!(term.grid()[Line(0)][Column(1)].c, '😊');
    }

    /// Dev aid: replays a `MAOMI_RECORD` capture and prints the screen after each frame
    /// (each time the cursor is shown again). `MAOMI_REPLAY=<file>`,
    /// `MAOMI_REPLAY_SIZE=<cols>x<rows>`, `MAOMI_REPLAY_PLAIN=1` skips cluster joining.
    #[test]
    #[ignore = "needs a capture file"]
    fn replay_capture() {
        let path = std::env::var("MAOMI_REPLAY").expect("MAOMI_REPLAY=<file>");
        let size = std::env::var("MAOMI_REPLAY_SIZE").unwrap_or_else(|_| "80x24".into());
        let (cols, rows) = size.split_once('x').expect("<cols>x<rows>");
        let (cols, rows): (u16, u16) = (cols.parse().unwrap(), rows.parse().unwrap());
        let plain = std::env::var_os("MAOMI_REPLAY_PLAIN").is_some();
        let bytes = std::fs::read(path).unwrap();
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
        let mut state = Graphemes::default();
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let show = b"\x1b[?25h";
        let mut frame = 0;
        let mut start = 0;
        while start < bytes.len() {
            let end = bytes[start..]
                .windows(show.len())
                .position(|w| w == show)
                .map_or(bytes.len(), |i| start + i + show.len());
            if plain {
                parser.advance(&mut term, &bytes[start..end]);
            } else {
                parser.advance(
                    &mut GraphemeTerm::new(&mut term, &mut state),
                    &bytes[start..end],
                );
            }
            start = end;
            frame += 1;
            println!("=== frame {frame} (byte {end})");
            let snap = crate::snapshot::GridSnapshot::from_term(&term);
            for line in snap.text().lines() {
                println!("|{line}");
            }
        }
    }
}
