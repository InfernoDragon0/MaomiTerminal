//! Display model for one file's diff: rows for the unified and side-by-side layouts (both
//! virtualised by the view), tab-expanded text, and word-level emphasis between paired
//! deleted / added lines. Port of GitDragon's `diff/model.rs` without LFS or syntax colours.

use std::borrow::Cow;
use std::ops::Range;

use gpui_kit::SharedString;
use maomi_git::parse::diff::{FileDiff, LineKind};

use super::words;

/// Spaces per tab in the diff view.
const TAB: &str = "    ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Unified,
    Split,
}

/// One row of the virtual list. Indices point into [`FileDiff::hunks`] / `Hunk::lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow {
    Hunk {
        hunk: usize,
    },
    /// Unified layout.
    Line {
        hunk: usize,
        line: usize,
    },
    /// Split layout: old side left, new side right; a context line sits on both.
    Pair {
        hunk: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
}

#[derive(Debug, Clone)]
pub struct LineView {
    pub text: SharedString,
    /// Changed byte ranges inside `text` (word-level diff against the paired line).
    pub emphasis: Vec<Range<usize>>,
    /// Followed by `\ No newline at end of file`.
    pub no_eol: bool,
    /// Syntax colours (see [`super::syntax::highlight`]); empty when not highlighted.
    pub syntax: super::syntax::Spans,
}

pub struct DiffModel {
    pub file: FileDiff,
    /// Per hunk, per line (same indices as the hunk's lines).
    pub lines: Vec<Vec<LineView>>,
    pub unified: Vec<DiffRow>,
    pub split: Vec<DiffRow>,
    pub added: usize,
    pub removed: usize,
}

impl DiffModel {
    pub fn new(file: FileDiff) -> Self {
        let mut lines: Vec<Vec<LineView>> = file
            .hunks
            .iter()
            .map(|hunk| {
                let mut views: Vec<LineView> = hunk
                    .lines
                    .iter()
                    .map(|line| LineView {
                        text: display_text(&line.content),
                        emphasis: Vec::new(),
                        no_eol: false,
                        syntax: Vec::new(),
                    })
                    .collect();
                for (i, line) in hunk.lines.iter().enumerate() {
                    if line.kind == LineKind::NoNewlineAtEof && i > 0 {
                        views[i - 1].no_eol = true;
                    }
                }
                views
            })
            .collect();

        let (mut unified, mut split) = (Vec::new(), Vec::new());
        let (mut added, mut removed) = (0, 0);
        for (h, hunk) in file.hunks.iter().enumerate() {
            unified.push(DiffRow::Hunk { hunk: h });
            split.push(DiffRow::Hunk { hunk: h });
            let kinds: Vec<LineKind> = hunk.lines.iter().map(|l| l.kind).collect();
            let mut i = 0;
            while i < kinds.len() {
                match kinds[i] {
                    LineKind::NoNewlineAtEof => i += 1,
                    LineKind::Context => {
                        unified.push(DiffRow::Line { hunk: h, line: i });
                        split.push(DiffRow::Pair {
                            hunk: h,
                            left: Some(i),
                            right: Some(i),
                        });
                        i += 1;
                    }
                    LineKind::Deleted | LineKind::Added => {
                        // A block: deletions, then additions (markers may sit in between).
                        let mut dels = Vec::new();
                        let mut adds = Vec::new();
                        while i < kinds.len() && kinds[i] != LineKind::Context {
                            match kinds[i] {
                                LineKind::Deleted if adds.is_empty() => dels.push(i),
                                LineKind::Deleted => break,
                                LineKind::Added => adds.push(i),
                                _ => {}
                            }
                            i += 1;
                        }
                        removed += dels.len();
                        added += adds.len();
                        unified.extend(
                            dels.iter()
                                .chain(&adds)
                                .map(|&line| DiffRow::Line { hunk: h, line }),
                        );
                        for k in 0..dels.len().max(adds.len()) {
                            let (left, right) = (dels.get(k).copied(), adds.get(k).copied());
                            split.push(DiffRow::Pair {
                                hunk: h,
                                left,
                                right,
                            });
                            if let (Some(l), Some(r)) = (left, right) {
                                let views = &mut lines[h];
                                let (old, new) = words::changed(&views[l].text, &views[r].text);
                                views[l].emphasis = old;
                                views[r].emphasis = new;
                            }
                        }
                    }
                }
            }
        }
        Self {
            file,
            lines,
            unified,
            split,
            added,
            removed,
        }
    }

    pub fn rows(&self, layout: Layout) -> &[DiffRow] {
        match layout {
            Layout::Unified => &self.unified,
            Layout::Split => &self.split,
        }
    }
}

/// Lossy UTF-8 with tabs expanded, a trailing `\r` dropped and control characters escaped.
fn display_text(bytes: &[u8]) -> SharedString {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    let text = sanitize_line(text);
    if text.contains('\t') {
        text.replace('\t', TAB).into()
    } else {
        SharedString::from(text.into_owned())
    }
}

/// Is `c` drawn as a `<U+XXXX>` escape? Tabs stay; line breaks, control and text-direction
/// characters (which could disguise code) do not.
fn needs_escape(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{a}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{2028}' | '\u{2029}'
        | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{200e}' | '\u{200f}' | '\u{61c}')
}

/// Single-line text from a repository, made safe to draw (see [`needs_escape`]).
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    if !text.chars().any(needs_escape) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if needs_escape(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maomi_git::parse::diff;

    const PATCH: &str = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,4 +1,4 @@
 a
-let x = 1;
+let x = 2;
+extra
 c
-gone
\\ No newline at end of file
";

    fn model() -> DiffModel {
        DiffModel::new(diff::parse(PATCH.as_bytes()).unwrap().remove(0))
    }

    #[test]
    fn unified_rows_follow_the_patch() {
        let m = model();
        assert_eq!(m.unified.len(), 1 + 6);
        assert_eq!((m.added, m.removed), (2, 2));
        assert!(m.lines[0][5].no_eol);
    }

    #[test]
    fn split_pairs_deletions_with_additions() {
        let m = model();
        let pairs: Vec<_> = m.split[1..].to_vec();
        assert_eq!(
            pairs,
            vec![
                DiffRow::Pair {
                    hunk: 0,
                    left: Some(0),
                    right: Some(0)
                },
                DiffRow::Pair {
                    hunk: 0,
                    left: Some(1),
                    right: Some(2)
                },
                DiffRow::Pair {
                    hunk: 0,
                    left: None,
                    right: Some(3)
                },
                DiffRow::Pair {
                    hunk: 0,
                    left: Some(4),
                    right: Some(4)
                },
                DiffRow::Pair {
                    hunk: 0,
                    left: Some(5),
                    right: None
                },
            ]
        );
        // `1` -> `2` is the only change in the pair.
        assert_eq!(m.lines[0][1].emphasis, vec![8..9]);
        assert_eq!(m.lines[0][2].emphasis, vec![8..9]);
    }

    #[test]
    fn tabs_expand_and_controls_escape() {
        assert_eq!(display_text(b"\tx\r").as_ref(), "    x");
        assert_eq!(
            display_text("a\u{202e}b\u{1b}".as_bytes()).as_ref(),
            "a<U+202E>b<U+001B>"
        );
    }
}
