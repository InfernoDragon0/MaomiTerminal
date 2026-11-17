//! Kitty Unicode placeholder cells: U+10EEEE with combining marks that say which tile of an
//! image the cell shows.
//!
//! The cell's foreground colour holds the image id (`38;5;id`, or `38;2;r;g;b` = 24 bits), the
//! first mark the image row, the second the column and an optional third the id's top byte.
//! Missing marks are inherited from the placeholder cell to the left, as the protocol says.

use crate::snapshot::{CellColor, GridSnapshot, SnapshotCell};

/// The placeholder character; the renderer draws image pixels instead of a glyph.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column marks in order: the n-th one encodes n. From the Kitty graphics protocol spec
/// (`rowcolumn-diacritics.txt`).
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B,
    0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367,
    0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485,
    0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D,
    0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610,
    0x0611, 0x0612, 0x0613, 0x0614, 0x0615, 0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A,
    0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF,
    0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB, 0x06EC, 0x0730, 0x0732, 0x0733,
    0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747, 0x0749,
    0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817,
    0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822, 0x0823,
    0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75,
    0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F,
    0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0,
    0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1,
    0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC,
    0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE,
    0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9,
    0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6,
    0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1,
    0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA, 0x2DFB, 0x2DFC,
    0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1, 0xA8E2,
    0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF,
    0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185,
    0x1D186, 0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243,
    0x1D244,
];

/// A horizontal run of placeholder cells showing consecutive tiles of one image row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRun {
    /// Row in [`GridSnapshot::rows`] and first column of the run.
    pub row: usize,
    pub col: usize,
    /// Cells in the run.
    pub len: usize,
    pub image_id: u32,
    /// The image tile (in cells) shown by the run's first cell.
    pub image_row: u32,
    pub image_col: u32,
}

fn diacritic_index(c: char) -> Option<u32> {
    DIACRITICS.binary_search(&(c as u32)).ok().map(|i| i as u32)
}

/// The low 24 bits of the image id, from the cell's foreground colour.
fn id_from_color(color: CellColor) -> Option<u32> {
    match color {
        CellColor::Indexed(index) => Some(u32::from(index)),
        CellColor::Rgb(rgb) => {
            Some((u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b))
        }
        CellColor::Foreground | CellColor::Background => None,
    }
}

/// One placeholder cell, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tile {
    id: u32,
    row: u32,
    col: u32,
}

/// Decodes `cell`, inheriting missing marks from `left`, the decoded cell just before it.
fn tile(cell: &SnapshotCell, left: Option<Tile>) -> Option<Tile> {
    if cell.c != PLACEHOLDER {
        return None;
    }
    let low = id_from_color(cell.fg)?;
    let mut marks = cell.zerowidth.iter().map(|&c| diacritic_index(c));
    let row = marks.next().flatten();
    let col = marks.next().flatten();
    let high = marks.next().flatten();
    // The left cell continues this one when it has the same colour id (and row, when given).
    let left = left.filter(|l| l.id & 0x00ff_ffff == low && row.is_none_or(|r| r == l.row));
    let id = match (high, left) {
        (Some(high), _) => (high << 24) | low,
        (None, Some(l)) => l.id,
        (None, None) => low,
    };
    let (row, col) = match (row, col, left) {
        (Some(row), Some(col), _) => (row, col),
        (_, None, Some(l)) => (l.row, l.col + 1),
        (Some(row), None, None) => (row, 0),
        (None, _, None) => (0, 0),
        (None, Some(col), Some(l)) => (l.row, col),
    };
    (id != 0).then_some(Tile { id, row, col })
}

impl GridSnapshot {
    /// Runs of image placeholder cells, row by row, left to right.
    pub fn image_runs(&self) -> Vec<ImageRun> {
        let mut runs: Vec<ImageRun> = Vec::new();
        for (row, cells) in self.rows().enumerate() {
            // Most rows have no placeholder; skip them without decoding.
            if !cells.iter().any(|cell| cell.c == PLACEHOLDER) {
                continue;
            }
            let mut left = None;
            for (col, cell) in cells.iter().enumerate() {
                let Some(t) = tile(cell, left) else {
                    left = None;
                    continue;
                };
                left = Some(t);
                if let Some(run) = runs.last_mut()
                    && run.row == row
                    && run.col + run.len == col
                    && run.image_id == t.id
                    && run.image_row == t.row
                    && run.image_col + run.len as u32 == t.col
                {
                    run.len += 1;
                    continue;
                }
                runs.push(ImageRun {
                    row,
                    col,
                    len: 1,
                    image_id: t.id,
                    image_row: t.row,
                    image_col: t.col,
                });
            }
        }
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grapheme::{GraphemeTerm, Graphemes};
    use crate::session::TermSize;
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn mark(n: usize) -> char {
        char::from_u32(DIACRITICS[n]).unwrap()
    }

    /// The rows Claude Code prints for an image: colour, then U+10EEEE + row + column marks.
    fn claude_rows(id: u32, columns: usize, rows: usize) -> String {
        let colour = if id < 256 {
            format!("\x1b[38;5;{id}m")
        } else {
            format!("\x1b[38;2;{};{};{}m", id >> 16 & 255, id >> 8 & 255, id & 255)
        };
        (0..rows)
            .map(|r| {
                let cells: String = (0..columns)
                    .flat_map(|c| [PLACEHOLDER, mark(r), mark(c)])
                    .collect();
                format!("{colour}{cells}\x1b[39m")
            })
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    fn snapshot(bytes: &[u8]) -> GridSnapshot {
        let mut term = Term::new(Config::default(), &TermSize::new(20, 4), VoidListener);
        let mut graphemes = Graphemes::default();
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut GraphemeTerm::new(&mut term, &mut graphemes), bytes);
        GridSnapshot::from_term(&term)
    }

    #[test]
    fn table_is_sorted_and_complete() {
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(diacritic_index('\u{0305}'), Some(0));
        assert_eq!(diacritic_index('\u{1D244}'), Some(296));
    }

    #[test]
    fn claude_code_rows_become_runs() {
        let snap = snapshot(format!("ab{}", claude_rows(31, 3, 2)).as_bytes());
        // Each placeholder takes one cell and keeps its marks.
        assert_eq!(snap.row(0)[2].c, PLACEHOLDER);
        assert_eq!(snap.row(0)[3].zerowidth, vec![mark(0), mark(1)]);
        assert_eq!(snap.row(0)[5].c, ' ');
        assert_eq!(
            snap.image_runs(),
            vec![
                ImageRun {
                    row: 0,
                    col: 2,
                    len: 3,
                    image_id: 31,
                    image_row: 0,
                    image_col: 0
                },
                ImageRun {
                    row: 1,
                    col: 0,
                    len: 3,
                    image_id: 31,
                    image_row: 1,
                    image_col: 0
                },
            ]
        );
    }

    #[test]
    fn truecolour_ids_and_inherited_marks() {
        // Only the first cell carries marks; the rest inherit row and the next column.
        let id = 0x01_02_03;
        let row = format!(
            "\x1b[38;2;1;2;3m{PLACEHOLDER}{}{}{PLACEHOLDER}{PLACEHOLDER}\x1b[39m",
            mark(2),
            mark(5)
        );
        let runs = snapshot(row.as_bytes()).image_runs();
        assert_eq!(
            runs,
            vec![ImageRun {
                row: 0,
                col: 0,
                len: 3,
                image_id: id,
                image_row: 2,
                image_col: 5
            }]
        );
    }

    #[test]
    fn third_mark_sets_the_high_byte() {
        let row = format!("\x1b[38;5;7m{PLACEHOLDER}{}{}{}\x1b[39m", mark(0), mark(0), mark(1));
        assert_eq!(snapshot(row.as_bytes()).image_runs()[0].image_id, (1 << 24) | 7);
    }

    #[test]
    fn default_colour_is_not_an_image() {
        let row = format!("{PLACEHOLDER}{}{}", mark(0), mark(0));
        assert!(snapshot(row.as_bytes()).image_runs().is_empty());
    }
}
