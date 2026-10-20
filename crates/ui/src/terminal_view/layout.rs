//! Turns a grid snapshot into paint-ready rows: merged background spans, style-uniform text
//! batches and selection spans. Plain data, so it is tested without a window.

use maomi_term::{CellColor, CellFlags, GridSnapshot, Rgb, SnapshotCell};

use crate::color::Color;
use crate::terminal::TerminalColors;

/// How much a DIM cell's foreground fades toward the background.
const DIM_FACTOR: f32 = 0.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnderlineKind {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Everything that decides how a glyph is drawn. Cells with equal styles share one shaped run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextStyle {
    pub fg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: Option<(UnderlineKind, Color)>,
    pub strike: bool,
}

/// Consecutive cells of one row drawn with one style, starting at `col`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextBatch {
    pub row: usize,
    pub col: usize,
    pub text: String,
    pub style: TextStyle,
    /// Columns covered; zero-width marks in `text` add none.
    pub cells: usize,
}

/// A run of cells on one row: `cols` cells starting at `col`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub row: usize,
    pub col: usize,
    pub cols: usize,
}

#[derive(Debug, Default)]
pub(crate) struct GridLayout {
    /// Backgrounds that differ from the pane colour.
    pub backgrounds: Vec<(Span, Color)>,
    pub selections: Vec<Span>,
    pub batches: Vec<TextBatch>,
    /// Block and box-drawing cells painted as shapes (see `builtin`): position, char, colour.
    pub builtin: Vec<(usize, usize, char, Color)>,
}

pub(crate) fn layout_grid(snapshot: &GridSnapshot, colors: &TerminalColors) -> GridLayout {
    let mut layout = GridLayout::default();
    for (row, cells) in snapshot.rows().enumerate() {
        layout_row(row, cells, snapshot, colors, &mut layout);
    }
    layout
}

fn layout_row(
    row: usize,
    cells: &[SnapshotCell],
    snapshot: &GridSnapshot,
    colors: &TerminalColors,
    out: &mut GridLayout,
) {
    let mut bg_span: Option<(Span, Color)> = None;
    let mut sel_span: Option<Span> = None;
    let mut batch: Option<TextBatch> = None;

    for (col, cell) in cells.iter().enumerate() {
        if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            // The wide char before it already covers this column.
            continue;
        }
        let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let (fg, bg) = cell_colors(cell, colors);

        // Backgrounds: merge equal neighbours, skip the pane colour (painted once for the pane).
        let bg = (bg != colors.bg).then_some(bg);
        match (&mut bg_span, bg) {
            (Some((span, color)), Some(bg)) if *color == bg && span.col + span.cols == col => {
                span.cols += width;
            }
            (current, bg) => {
                if let Some(done) = current.take() {
                    out.backgrounds.push(done);
                }
                *current = bg.map(|bg| {
                    (
                        Span {
                            row,
                            col,
                            cols: width,
                        },
                        bg,
                    )
                });
            }
        }

        if snapshot.is_selected(row, col) {
            match &mut sel_span {
                Some(span) if span.col + span.cols == col => span.cols += width,
                current => {
                    if let Some(done) = current.take() {
                        out.selections.push(done);
                    }
                    *current = Some(Span {
                        row,
                        col,
                        cols: width,
                    });
                }
            }
        }

        // Text: blank cells without decorations end the batch and draw nothing.
        let style = text_style(cell, fg, colors);
        let blank = (cell.c == ' ' || cell.c == '\0') && cell.zerowidth.is_empty();
        let visible = !blank || style.underline.is_some() || style.strike;
        if !visible || cell.flags.contains(CellFlags::HIDDEN) {
            out.batches.extend(batch.take());
            continue;
        }
        if super::builtin::is_builtin(cell.c) {
            out.batches.extend(batch.take());
            out.builtin.push((row, col, cell.c, fg));
            continue;
        }
        // Wide chars get a batch of their own: shaping forces every glyph to one cell width,
        // so anything after a wide char in the same batch would land a column early.
        let joins = |b: &TextBatch| width == 1 && b.style == style && b.col + b.cells == col;
        match &mut batch {
            Some(b) if joins(b) => b.push(cell),
            current => {
                out.batches.extend(current.take());
                let mut b = TextBatch {
                    row,
                    col,
                    text: String::new(),
                    style,
                    cells: 0,
                };
                b.push(cell);
                *current = Some(b);
            }
        }
        if width == 2 {
            out.batches.extend(batch.take());
        }
    }
    out.backgrounds.extend(bg_span);
    out.selections.extend(sel_span);
    out.batches.extend(batch);
}

impl TextBatch {
    fn push(&mut self, cell: &SnapshotCell) {
        self.text.push(if cell.c == '\0' { ' ' } else { cell.c });
        self.text.extend(&cell.zerowidth);
        self.cells += 1;
    }
}

/// Foreground and background after inverse, dim and hidden.
pub(crate) fn cell_colors(cell: &SnapshotCell, colors: &TerminalColors) -> (Color, Color) {
    let mut fg = resolve(cell.fg, colors);
    let mut bg = resolve(cell.bg, colors);
    if cell.flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags.contains(CellFlags::DIM) {
        fg = fg.mix(bg, DIM_FACTOR);
    }
    (fg, bg)
}

fn text_style(cell: &SnapshotCell, fg: Color, colors: &TerminalColors) -> TextStyle {
    let flags = cell.flags;
    let kind = if flags.contains(CellFlags::UNDERCURL) {
        Some(UnderlineKind::Curly)
    } else if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        Some(UnderlineKind::Double)
    } else if flags.contains(CellFlags::DOTTED_UNDERLINE) {
        Some(UnderlineKind::Dotted)
    } else if flags.contains(CellFlags::DASHED_UNDERLINE) {
        Some(UnderlineKind::Dashed)
    } else if flags.contains(CellFlags::UNDERLINE) || cell.hyperlink.is_some() {
        Some(UnderlineKind::Single)
    } else {
        None
    };
    let underline_color = cell.underline_color.map_or(fg, |c| resolve(c, colors));
    TextStyle {
        fg,
        bold: flags.contains(CellFlags::BOLD),
        italic: flags.contains(CellFlags::ITALIC),
        underline: kind.map(|k| (k, underline_color)),
        strike: flags.contains(CellFlags::STRIKEOUT),
    }
}

/// Maps a cell colour through the theme: 0-15 from the skin, 16-255 the xterm cube and greys.
pub(crate) fn resolve(color: CellColor, colors: &TerminalColors) -> Color {
    match color {
        CellColor::Foreground => colors.fg,
        CellColor::Background => colors.bg,
        CellColor::Rgb(Rgb { r, g, b }) => Color::new(r, g, b, 255),
        CellColor::Indexed(i @ 0..=15) => colors.ansi[i as usize],
        CellColor::Indexed(i @ 16..=231) => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color::new(level(i / 36), level(i / 6 % 6), level(i % 6), 255)
        }
        CellColor::Indexed(i) => {
            let v = 8 + (i - 232) * 10;
            Color::new(v, v, v, 255)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mode, Skin, Theme};
    use maomi_term::alacritty_terminal::Term;
    use maomi_term::alacritty_terminal::event::VoidListener;
    use maomi_term::alacritty_terminal::term::Config;
    use maomi_term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn snapshot(bytes: &[u8]) -> GridSnapshot {
        let size = maomi_term::TermSize::new(20, 2);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, bytes);
        GridSnapshot::from_term(&term)
    }

    fn colors() -> TerminalColors {
        Theme::new(Skin::Aurora, Mode::Dark).terminal_colors()
    }

    #[test]
    fn palette_mapping() {
        let c = colors();
        assert_eq!(resolve(CellColor::Indexed(1), &c), c.ansi[1]);
        assert_eq!(
            resolve(CellColor::Indexed(16), &c),
            Color::new(0, 0, 0, 255)
        );
        assert_eq!(
            resolve(CellColor::Indexed(231), &c),
            Color::new(255, 255, 255, 255)
        );
        assert_eq!(
            resolve(CellColor::Indexed(232), &c),
            Color::new(8, 8, 8, 255)
        );
        assert_eq!(
            resolve(CellColor::Indexed(255), &c),
            Color::new(238, 238, 238, 255)
        );
    }

    #[test]
    fn batches_split_on_style_and_blanks() {
        let layout = layout_grid(&snapshot(b"ab\x1b[1mcd\x1b[0m  ef"), &colors());
        let texts: Vec<_> = layout
            .batches
            .iter()
            .map(|b| (b.col, b.text.as_str()))
            .collect();
        assert_eq!(texts, vec![(0, "ab"), (2, "cd"), (6, "ef")]);
        assert!(layout.batches[1].style.bold);
    }

    #[test]
    fn spaces_inside_a_style_stay_in_the_batch_only_when_decorated() {
        let layout = layout_grid(&snapshot(b"\x1b[4ma b\x1b[0m"), &colors());
        assert_eq!(layout.batches.len(), 1);
        assert_eq!(layout.batches[0].text, "a b");
    }

    #[test]
    fn wide_chars_get_their_own_batch() {
        let layout = layout_grid(&snapshot("a日b".as_bytes()), &colors());
        let texts: Vec<_> = layout
            .batches
            .iter()
            .map(|b| (b.col, b.text.as_str()))
            .collect();
        assert_eq!(texts, vec![(0, "a"), (1, "日"), (3, "b")]);
    }

    #[test]
    fn selection_spans_follow_the_selected_cells() {
        use maomi_term::alacritty_terminal::index::{Column, Line, Point, Side};
        use maomi_term::alacritty_terminal::selection::{Selection, SelectionType};

        let size = maomi_term::TermSize::new(10, 2);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"hello\r\nworld");
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(3)),
            Side::Left,
        );
        selection.update(Point::new(Line(1), Column(1)), Side::Right);
        term.selection = Some(selection);

        let layout = layout_grid(&GridSnapshot::from_term(&term), &colors());
        assert_eq!(
            layout.selections,
            vec![
                Span {
                    row: 0,
                    col: 3,
                    cols: 7
                },
                Span {
                    row: 1,
                    col: 0,
                    cols: 2
                }
            ]
        );
    }

    #[test]
    fn block_chars_leave_the_text_batches() {
        let layout = layout_grid(&snapshot("a▐▛█b".as_bytes()), &colors());
        let texts: Vec<_> = layout
            .batches
            .iter()
            .map(|b| (b.col, b.text.as_str()))
            .collect();
        assert_eq!(texts, vec![(0, "a"), (4, "b")]);
        let chars: Vec<_> = layout
            .builtin
            .iter()
            .map(|&(_, col, c, _)| (col, c))
            .collect();
        assert_eq!(chars, vec![(1, '▐'), (2, '▛'), (3, '█')]);
    }

    #[test]
    fn backgrounds_merge_and_skip_pane_colour() {
        let c = colors();
        let layout = layout_grid(&snapshot(b"x\x1b[41mred\x1b[0my\x1b[7mi\x1b[0m"), &c);
        assert_eq!(
            layout.backgrounds,
            vec![
                (
                    Span {
                        row: 0,
                        col: 1,
                        cols: 3
                    },
                    c.ansi[1]
                ),
                // Inverse: the default foreground becomes the background.
                (
                    Span {
                        row: 0,
                        col: 5,
                        cols: 1
                    },
                    c.fg
                ),
            ]
        );
    }
}
