//! A copy of the visible grid that the renderer reads without holding the terminal lock.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};

/// A cell colour. Theme-dependent colours stay symbolic so a skin switch needs no re-parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// The theme's terminal foreground.
    Foreground,
    /// The theme's terminal background.
    Background,
    /// Palette index: 0-15 come from the theme, 16-255 are the xterm cube and grey ramp.
    Indexed(u8),
    /// A colour the program set directly (SGR 38;2 or an OSC 4/10/11 override).
    Rgb(Rgb),
}

/// One visible cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCell {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    /// Bold, italic, underline kinds, inverse, dim, wide-char halves and so on.
    pub flags: Flags,
    /// Combining marks drawn over `c`; empty for nearly every cell.
    pub zerowidth: Vec<char>,
    pub underline_color: Option<CellColor>,
    /// OSC 8 link on this cell.
    pub hyperlink: Option<Hyperlink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCursor {
    /// Viewport row, 0 = top.
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// The visible grid at one moment.
#[derive(Debug, Clone)]
pub struct GridSnapshot {
    pub columns: usize,
    pub screen_lines: usize,
    /// Row-major, `screen_lines * columns` long.
    pub cells: Vec<SnapshotCell>,
    /// None when the cursor is hidden or scrolled out of view.
    pub cursor: Option<SnapshotCursor>,
    /// Lines scrolled back from the bottom; 0 = following output.
    pub display_offset: usize,
    /// Lines of scrollback stored above the screen.
    pub history_size: usize,
    /// App cursor keys, bracketed paste, mouse reporting, alt screen and the other DEC modes.
    pub mode: TermMode,
    selection: Option<SelectionRange>,
}

impl GridSnapshot {
    pub(crate) fn capture<T: EventListener>(term: &Term<T>) -> Self {
        let content = term.renderable_content();
        let columns = term.columns();
        let screen_lines = term.screen_lines();
        let display_offset = content.display_offset;
        let colors = content.colors;

        let blank = SnapshotCell::from_cell(&Cell::default(), colors);
        let mut cells = vec![blank; columns * screen_lines];
        for indexed in content.display_iter {
            let Some(row) = viewport_row(display_offset, indexed.point.line) else {
                continue;
            };
            let col = indexed.point.column.0;
            if row < screen_lines && col < columns {
                cells[row * columns + col] = SnapshotCell::from_cell(indexed.cell, colors);
            }
        }

        let cursor = (content.cursor.shape != CursorShape::Hidden)
            .then(|| {
                let point = content.cursor.point;
                let row = viewport_row(display_offset, point.line)?;
                (row < screen_lines).then_some(SnapshotCursor {
                    row,
                    col: point.column.0.min(columns.saturating_sub(1)),
                    shape: content.cursor.shape,
                })
            })
            .flatten();

        Self {
            columns,
            screen_lines,
            cells,
            cursor,
            display_offset,
            history_size: term.history_size(),
            mode: content.mode,
            selection: content.selection,
        }
    }

    /// The cells of viewport row `row`.
    pub fn row(&self, row: usize) -> &[SnapshotCell] {
        &self.cells[row * self.columns..(row + 1) * self.columns]
    }

    pub fn rows(&self) -> impl Iterator<Item = &[SnapshotCell]> {
        self.cells.chunks(self.columns.max(1))
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&SnapshotCell> {
        (col < self.columns)
            .then(|| self.cells.get(row * self.columns + col))
            .flatten()
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Whether the cell at viewport `row`, `col` is inside the selection.
    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(selection) = &self.selection else {
            return false;
        };
        let line = Line(row as i32 - self.display_offset as i32);
        selection.contains(Point::new(line, Column(col)))
    }

    /// Plain text of the visible rows, trailing spaces trimmed; for tests and debugging.
    pub fn text(&self) -> String {
        self.rows()
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                    .map(|cell| cell.c)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl SnapshotCell {
    fn from_cell(cell: &Cell, colors: &Colors) -> Self {
        Self {
            c: cell.c,
            fg: resolve(cell.fg, colors),
            bg: resolve(cell.bg, colors),
            flags: cell.flags,
            zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
            underline_color: cell.underline_color().map(|c| resolve(c, colors)),
            hyperlink: cell.hyperlink(),
        }
    }
}

fn viewport_row(display_offset: usize, line: Line) -> Option<usize> {
    usize::try_from(line.0 + display_offset as i32).ok()
}

/// Maps a cell colour to [`CellColor`], applying any OSC 4/10/11 override the program set.
fn resolve(color: Color, colors: &Colors) -> CellColor {
    match color {
        Color::Spec(rgb) => CellColor::Rgb(rgb),
        Color::Indexed(index) => match colors[index as usize] {
            Some(rgb) => CellColor::Rgb(rgb),
            None => CellColor::Indexed(index),
        },
        Color::Named(named) => {
            if let Some(rgb) = colors[named] {
                return CellColor::Rgb(rgb);
            }
            match named {
                NamedColor::Foreground
                | NamedColor::BrightForeground
                | NamedColor::DimForeground
                | NamedColor::Cursor => CellColor::Foreground,
                NamedColor::Background => CellColor::Background,
                other if (other as usize) < 16 => CellColor::Indexed(other as u8),
                // DimRed and friends keep their base index; the DIM flag tells the renderer to fade.
                other => CellColor::Indexed(other.to_bright() as u8),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_colours_stay_symbolic() {
        let colors = Colors::default();
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors),
            CellColor::Indexed(1)
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::DimRed), &colors),
            CellColor::Indexed(1)
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::BrightBlue), &colors),
            CellColor::Indexed(12)
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &colors),
            CellColor::Background
        );
        assert_eq!(
            resolve(Color::Indexed(200), &colors),
            CellColor::Indexed(200)
        );
    }

    #[test]
    fn osc_overrides_win() {
        let mut colors = Colors::default();
        let pink = Rgb {
            r: 255,
            g: 0,
            b: 128,
        };
        colors[1] = Some(pink);
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors),
            CellColor::Rgb(pink)
        );
    }

    #[test]
    fn parses_sgr_into_snapshot() {
        use crate::session::TermSize;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::Config;
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        let mut term = Term::new(Config::default(), &TermSize::new(20, 3), VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(
            &mut term,
            b"\x1b[31mR\x1b[1;38;2;1;2;3mT\x1b[0m \x1b[48;5;200mB\x1b[0m\r\nline2\x1b[?25l",
        );
        let snap = GridSnapshot::capture(&term);

        assert_eq!(snap.text(), "RT B\nline2\n");
        assert_eq!(snap.row(0)[0].fg, CellColor::Indexed(1));
        assert_eq!(snap.row(0)[1].fg, CellColor::Rgb(Rgb { r: 1, g: 2, b: 3 }));
        assert!(snap.row(0)[1].flags.contains(Flags::BOLD));
        assert_eq!(snap.row(0)[3].bg, CellColor::Indexed(200));
        assert_eq!(snap.row(1)[0].fg, CellColor::Foreground);
        assert!(snap.cursor.is_none(), "DECTCEM off hides the cursor");
    }
}
