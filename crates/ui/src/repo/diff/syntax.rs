//! Syntax colours for diff lines. Each hunk's old side (context + deleted) and new side
//! (context + added) is highlighted as one snippet, then split back into lines; snippets are
//! partial code, which syntect recovers from well enough for colouring. Port of GitDragon's
//! `diff/syntax.rs` (syntect engine): grammars from two-face, colours from gpui-component's
//! highlight theme.

use std::ops::Range;
use std::sync::LazyLock;

use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::component::input::HighlightStyleResolver as _;
use gpui_kit::{HighlightStyle, Hsla};
use maomi_git::parse::diff::LineKind;
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use super::model::DiffModel;

/// Styled byte ranges inside one line's display text.
pub type Spans = Vec<(Range<usize>, HighlightStyle)>;

/// A line longer than this (minified code, data) is left uncoloured: syntect slows down
/// badly on very long lines.
pub const MAX_LINE: usize = 4 * 1024;
/// Above this much text in the file's diff nothing is coloured.
pub const MAX_TOTAL: usize = 1024 * 1024;

/// Loaded on first use (a few ms), off the UI thread like the rest of diff building.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Whether `model` is small enough to colour at all.
fn within_budget(model: &DiffModel) -> bool {
    let total: usize = model
        .lines
        .iter()
        .flatten()
        .map(|line| line.text.len())
        .sum();
    total <= MAX_TOTAL
}

/// Lower-case extension of the last path component (`None` for dotfiles / no dot).
fn extension(path: &[u8]) -> Option<String> {
    let name = path.rsplit(|&b| b == b'/').next()?;
    let ext = name
        .rsplit(|&b| b == b'.')
        .next()
        .filter(|e| e.len() < name.len() && !e.is_empty())?;
    if ext.len() + 1 == name.len() {
        return None; // `.gitignore`: a dotfile, not an extension
    }
    Some(std::str::from_utf8(ext).ok()?.to_ascii_lowercase())
}

fn syntax_for(path: &[u8]) -> Option<&'static SyntaxReference> {
    SYNTAXES.find_syntax_by_extension(&extension(path)?)
}

/// Fills `LineView::syntax` for every line of `model` (`path` picks the language). Skips big
/// diffs and unknown languages.
pub fn highlight(model: &mut DiffModel, path: &[u8], theme: &HighlightTheme) {
    if !within_budget(model) {
        return;
    }
    let Some(syntax) = syntax_for(path) else {
        return;
    };
    for (h, hunk) in model.file.hunks.iter().enumerate() {
        for old_side in [true, false] {
            // Lines of this side, in order, with their offsets in the snippet.
            let mut text = String::new();
            let mut placed: Vec<(usize, Range<usize>)> = Vec::new();
            for (i, line) in hunk.lines.iter().enumerate() {
                let on_side = match line.kind {
                    LineKind::Context => true,
                    LineKind::Deleted => old_side,
                    LineKind::Added => !old_side,
                    LineKind::NoNewlineAtEof => false,
                };
                if !on_side || model.lines[h][i].text.len() > MAX_LINE {
                    continue;
                }
                let start = text.len();
                text.push_str(&model.lines[h][i].text);
                placed.push((i, start..text.len()));
                text.push('\n');
            }
            if placed.is_empty() {
                continue;
            }
            let styles = styles(syntax, &text, theme);
            for (i, range) in placed {
                // Context lines sit on both sides: take the new side's colours.
                if hunk.lines[i].kind == LineKind::Context && old_side {
                    continue;
                }
                model.lines[h][i].syntax = clip(&styles, &range);
            }
        }
    }
}

fn styles(syntax: &SyntaxReference, text: &str, theme: &HighlightTheme) -> Spans {
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Spans::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let Ok(ops) = state.parse_line(line, &SYNTAXES) else {
            break;
        };
        let mut pos = 0;
        for (at, op) in ops {
            push(&mut out, offset + pos..offset + at, &stack, theme);
            if stack.apply(&op).is_err() {
                return out;
            }
            pos = at;
        }
        push(&mut out, offset + pos..offset + line.len(), &stack, theme);
        offset += line.len();
    }
    out
}

fn push(out: &mut Spans, range: Range<usize>, stack: &ScopeStack, theme: &HighlightTheme) {
    if range.is_empty() {
        return;
    }
    if let Some(style) = category(stack).and_then(|name| theme.style(name)) {
        out.push((range, style));
    }
}

/// Theme style name for the innermost scope we know (TextMate scope -> tree-sitter name).
fn category(stack: &ScopeStack) -> Option<&'static str> {
    const MAP: &[(&str, &str)] = &[
        ("comment", "comment"),
        ("string.regexp", "string.regex"),
        ("constant.character.escape", "string.escape"),
        ("string", "string"),
        ("constant.numeric", "number"),
        ("constant.language", "boolean"),
        ("constant", "constant"),
        ("entity.name.function", "function"),
        ("support.function", "function"),
        ("meta.function-call", "function"),
        ("entity.name.type", "type"),
        ("entity.name.class", "type"),
        ("support.type", "type"),
        ("support.class", "type"),
        ("storage.type", "type"),
        ("keyword.operator", "operator"),
        ("keyword", "keyword"),
        ("storage", "keyword"),
        ("entity.name.tag", "tag"),
        ("entity.other.attribute-name", "attribute"),
        ("variable.parameter", "variable.special"),
        ("variable.language", "variable.special"),
        ("variable.other.member", "property"),
        ("variable.other.property", "property"),
        ("markup.heading", "title"),
        ("punctuation", "punctuation"),
    ];
    stack.as_slice().iter().rev().find_map(|scope| {
        let s = scope.build_string();
        MAP.iter()
            .find(|(prefix, _)| s.starts_with(prefix))
            .map(|(_, name)| *name)
    })
}

/// The styles overlapping `line`, shifted to start at 0.
fn clip(styles: &Spans, line: &Range<usize>) -> Spans {
    styles
        .iter()
        .filter(|(r, s)| {
            r.start < line.end && r.end > line.start && *s != HighlightStyle::default()
        })
        .map(|(r, s)| {
            let start = r.start.max(line.start) - line.start;
            let end = r.end.min(line.end) - line.start;
            (start..end, *s)
        })
        .filter(|(r, _)| !r.is_empty())
        .collect()
}

/// Syntax colours plus word-level emphasis backgrounds as one sorted, non-overlapping list
/// (what `StyledText::with_highlights` wants). Overlapping syntax spans: the later one wins.
pub fn merge(syntax: &Spans, emphasis: &[Range<usize>], strong: Hsla, len: usize) -> Spans {
    let mut cuts: Vec<usize> = vec![0, len];
    for (r, _) in syntax {
        cuts.extend([r.start, r.end]);
    }
    for r in emphasis {
        cuts.extend([r.start, r.end]);
    }
    cuts.retain(|&c| c <= len);
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Spans = Vec::new();
    for pair in cuts.windows(2) {
        let seg = pair[0]..pair[1];
        let mut style = syntax
            .iter()
            .rev()
            .find(|(r, _)| r.start <= seg.start && r.end >= seg.end)
            .map(|(_, s)| *s)
            .unwrap_or_default();
        if emphasis
            .iter()
            .any(|r| r.start <= seg.start && r.end >= seg.end)
        {
            style.background_color = Some(strong);
        }
        if style == HighlightStyle::default() {
            continue;
        }
        match out.last_mut() {
            Some((last, s)) if last.end == seg.start && *s == style => last.end = seg.end,
            _ => out.push((seg, style)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::hsla;
    use maomi_git::parse::diff;

    #[test]
    fn extensions() {
        assert_eq!(extension(b"src/main.rs").as_deref(), Some("rs"));
        assert_eq!(extension(b"a/b/App.TSX").as_deref(), Some("tsx"));
        assert_eq!(extension(b"Makefile"), None);
        assert_eq!(extension(b".gitignore"), None);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one emphasis range, not a range of indices
    fn merge_splits_overlaps() {
        let red = hsla(0., 1., 0.5, 1.);
        let blue = hsla(0.6, 1., 0.5, 1.);
        let kw = HighlightStyle {
            color: Some(red),
            ..Default::default()
        };
        let out = merge(&vec![(0..3, kw)], &[2..5], blue, 6);
        let with_bg = HighlightStyle {
            background_color: Some(blue),
            ..kw
        };
        let bg_only = HighlightStyle {
            background_color: Some(blue),
            ..Default::default()
        };
        assert_eq!(out, vec![(0..2, kw), (2..3, with_bg), (3..5, bg_only)]);
    }

    fn model_of(lines: &[String]) -> DiffModel {
        let mut patch = format!(
            "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for line in lines {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
        DiffModel::new(diff::parse(patch.as_bytes()).unwrap().remove(0))
    }

    #[test]
    fn long_lines_and_big_diffs_stay_plain() {
        let theme = HighlightTheme::default_dark();
        let long = format!("let s = \"{}\";", "a".repeat(MAX_LINE));
        let mut model = model_of(&["fn main() {}".into(), long]);
        highlight(&mut model, b"f.rs", &theme);
        assert!(!model.lines[0][0].syntax.is_empty());
        assert!(model.lines[0][1].syntax.is_empty());
        // Over the total budget: not even the short lines get colours.
        let mut lines = vec!["fn main() {}".to_string()];
        lines.extend(vec!["x".repeat(MAX_LINE); MAX_TOTAL / MAX_LINE + 1]);
        let mut model = model_of(&lines);
        highlight(&mut model, b"f.rs", &theme);
        assert!(model.lines[0].iter().all(|l| l.syntax.is_empty()));
    }

    #[test]
    fn rust_hunk_gets_colours_on_both_sides() {
        let patch = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    let a = 1;\n+    let a = 2;\n";
        let mut model = DiffModel::new(diff::parse(patch.as_bytes()).unwrap().remove(0));
        highlight(&mut model, b"f.rs", &HighlightTheme::default_dark());
        let lines = &model.lines[0];
        assert!(!lines[0].syntax.is_empty(), "context `fn` is a keyword");
        assert!(!lines[1].syntax.is_empty() && !lines[2].syntax.is_empty());
        for line in lines {
            assert!(line.syntax.iter().all(|(r, _)| r.end <= line.text.len()));
        }
    }
}
