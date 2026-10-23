//! Keyboard, paste, mouse-report and link encoding (xterm conventions). Pure functions.

use std::path::{Path, PathBuf};

use gpui_kit::Keystroke;
use maomi_term::{CellFlags, GridSnapshot, SnapshotCell, TermMode};

use super::paths;

/// Bytes for a key press, or None when the key is not for the shell (app shortcuts,
/// lone modifiers, Ctrl+Shift+letter bindings).
pub(crate) fn encode_key(
    keystroke: &Keystroke,
    prefer_char: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let csi = |s: String| Some(s.into_bytes());

    let cursor_letter = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(letter) = cursor_letter {
        return if modifier > 1 {
            csi(format!("\x1b[1;{modifier}{letter}"))
        } else if mode.contains(TermMode::APP_CURSOR) {
            csi(format!("\x1bO{letter}"))
        } else {
            csi(format!("\x1b[{letter}"))
        };
    }
    let f_letter = match key {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    };
    if let Some(letter) = f_letter {
        return if modifier > 1 {
            csi(format!("\x1b[1;{modifier}{letter}"))
        } else {
            csi(format!("\x1bO{letter}"))
        };
    }
    let tilde = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(n) = tilde {
        return if modifier > 1 {
            csi(format!("\x1b[{n};{modifier}~"))
        } else {
            csi(format!("\x1b[{n}~"))
        };
    }

    let esc_if_alt = |bytes: &[u8]| {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if m.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        Some(out)
    };
    match key {
        // Ctrl+Enter sends a line feed (as Windows Terminal does), which apps can tell apart.
        "enter" if m.control => return esc_if_alt(b"\n"),
        "enter" => return esc_if_alt(b"\r"),
        "tab" if m.shift => return Some(b"\x1b[Z".to_vec()),
        "tab" if !m.control => return esc_if_alt(b"\t"),
        "escape" => return esc_if_alt(b"\x1b"),
        "backspace" if m.control => return esc_if_alt(b"\x08"),
        "backspace" => return esc_if_alt(b"\x7f"),
        "space" if m.control => return esc_if_alt(b"\0"),
        _ => {}
    }

    // AltGr (Ctrl+Alt on Windows) and macOS Option compose characters: send what was typed.
    if prefer_char && let Some(text) = keystroke.key_char.as_deref() {
        return Some(text.as_bytes().to_vec());
    }
    if m.control {
        if m.shift && key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() {
            // Ctrl+Shift+letter is reserved for app shortcuts.
            return None;
        }
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && let Some(code) = ctrl_code(c)
        {
            return esc_if_alt(&[code]);
        }
        return None;
    }
    let text = keystroke.key_char.as_deref().or(match key {
        "space" => Some(" "),
        _ => None,
    })?;
    // Alt + key that typed its plain character means Meta: prefix ESC.
    if m.alt && text == key {
        return esc_if_alt(text.as_bytes());
    }
    Some(text.as_bytes().to_vec())
}

fn ctrl_code(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' | '/' => 0x1f,
        '8' => 0x7f,
        _ => return None,
    })
}

/// Pasted text: wrapped in bracketed-paste markers when the program asked for them,
/// otherwise with line breaks turned into Enter.
pub(crate) fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // A pasted end marker would let the paste break out early and run as typed input.
        let body = text.replace("\x1b[201~", "");
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Mouse button codes for reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    /// Motion with no button held.
    None = 3,
    WheelUp = 64,
    WheelDown = 65,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportKind {
    Press,
    Release,
    Motion,
}

/// A mouse report for a cell (0-based `col`, `row`), in SGR form when the program enabled it.
pub(crate) fn encode_mouse(
    button: ReportButton,
    kind: ReportKind,
    col: usize,
    row: usize,
    modifiers: &gpui_kit::Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let mut code = button as u32;
    if kind == ReportKind::Motion {
        code += 32;
    }
    code += 4 * u32::from(modifiers.shift)
        + 8 * u32::from(modifiers.alt)
        + 16 * u32::from(modifiers.control);
    let (x, y) = (col + 1, row + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if kind == ReportKind::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    // Normal (X10-style) encoding has no release button; it reports button 3 instead.
    if kind == ReportKind::Release {
        code = 3 + (code & !3);
    }
    let encode = |v: usize| -> Option<Vec<u8>> {
        let v = v + 32;
        if mode.contains(TermMode::UTF8_MOUSE) {
            char::from_u32(v as u32).map(|c| c.to_string().into_bytes())
        } else {
            u8::try_from(v).ok().map(|b| vec![b])
        }
    };
    let mut out = b"\x1b[M".to_vec();
    out.push(u8::try_from(code + 32).ok()?);
    out.extend(encode(x)?);
    out.extend(encode(y)?);
    Some(out)
}

/// A link that starts on `row` and may wrap onto the rows below: `start..end` are offsets
/// into that wrapped line, `columns` wide per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub columns: usize,
    pub target: LinkTarget,
}

impl Link {
    /// The same link with rows counted from row `top`; any part above it is cut off.
    pub(crate) fn rebased(self, top: usize) -> Self {
        let cut = top.saturating_sub(self.row) * self.columns;
        Self {
            row: self.row.max(top) - top,
            start: self.start.max(cut) - cut,
            end: self.end.max(cut) - cut,
            ..self
        }
    }

    /// The link's part on each row: `(row, start column, end column)`.
    pub(crate) fn segments(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        let columns = self.columns.max(1);
        (self.start / columns..=(self.end - 1) / columns).map(move |i| {
            let row_start = i * columns;
            let start = self.start.max(row_start) - row_start;
            let end = self.end.min(row_start + columns) - row_start;
            (self.row + i, start, end)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    Url(String),
    /// Resolved against the cwd; it may not exist.
    Path(PathBuf),
}

const SCHEMES: [&str; 5] = ["https://", "http://", "file://", "mailto:", "ftp://"];

/// Rows looked at around the pointer for one wrapped line, so a huge line stays cheap.
const MAX_WRAPPED_ROWS: usize = 32;

/// The line `row` belongs to when the program's output wrapped: its first row, its cells and
/// the offset of (`row`, `col`) into them.
fn wrapped_line(
    snapshot: &GridSnapshot,
    row: usize,
    col: usize,
) -> Option<(usize, Vec<&SnapshotCell>, usize)> {
    let rows = snapshot.rows().count();
    if row >= rows || col >= snapshot.columns {
        return None;
    }
    let wraps = |r: usize| {
        snapshot
            .row(r)
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE))
    };
    let mut first = row;
    while first > 0 && row - first < MAX_WRAPPED_ROWS && wraps(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows && last - row < MAX_WRAPPED_ROWS && wraps(last) {
        last += 1;
    }
    let cells = (first..=last).flat_map(|r| snapshot.row(r)).collect();
    Some((first, cells, (row - first) * snapshot.columns + col))
}

/// The link under cell (`row`, `col`): an OSC 8 hyperlink, else a URL in the line's text.
/// `row` indexes [`GridSnapshot::rows`].
pub(crate) fn link_at(snapshot: &GridSnapshot, row: usize, col: usize) -> Option<Link> {
    let (first, cells, at) = wrapped_line(snapshot, row, col)?;
    let link = |start, end, target| Link {
        row: first,
        start,
        end,
        columns: snapshot.columns,
        target,
    };
    if let Some(hyperlink) = cells[at].hyperlink.as_ref() {
        let same = |c: &&SnapshotCell| {
            c.hyperlink
                .as_ref()
                .is_some_and(|h| h.id() == hyperlink.id())
        };
        let start = cells[..at]
            .iter()
            .rposition(|c| !same(c))
            .map_or(0, |i| i + 1);
        let end = cells[at..]
            .iter()
            .position(|c| !same(c))
            .map_or(cells.len(), |i| at + i);
        let url = LinkTarget::Url(hyperlink.uri().to_owned());
        return Some(link(start, end, url));
    }
    let text = line_text(&cells);
    find_urls(&text)
        .into_iter()
        .find(|(s, e)| (*s..*e).contains(&at))
        .map(|(start, end)| {
            let url = text[start..end].iter().collect();
            link(start, end, LinkTarget::Url(url))
        })
}

/// The file path token under cell (`row`, `col`), resolved against `cwd`; whether it exists
/// is not checked. `row` indexes [`GridSnapshot::rows`].
pub(crate) fn path_at(
    snapshot: &GridSnapshot,
    row: usize,
    col: usize,
    cwd: Option<&Path>,
) -> Option<Link> {
    let (first, cells, at) = wrapped_line(snapshot, row, col)?;
    let (start, end, raw) = paths::path_at(&line_text(&cells), at)?;
    Some(Link {
        row: first,
        start,
        end,
        columns: snapshot.columns,
        target: LinkTarget::Path(paths::resolve(&raw, cwd)?),
    })
}

/// One char per column; the right half of a wide char becomes a space so columns line up.
fn line_text(cells: &[&SnapshotCell]) -> Vec<char> {
    cells
        .iter()
        .map(|c| {
            if c.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                ' '
            } else {
                c.c
            }
        })
        .collect()
}

/// Column ranges of URLs in `text`.
pub(crate) fn find_urls(text: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let scheme = SCHEMES.iter().find(|s| starts_with(&text[i..], s));
        let Some(scheme) = scheme else {
            i += 1;
            continue;
        };
        // A scheme glued to a word (`xhttps://`) is not a link start.
        if i > 0 && text[i - 1].is_alphanumeric() {
            i += 1;
            continue;
        }
        let mut end = i + scheme.len();
        while end < text.len() && is_url_char(text[end]) {
            end += 1;
        }
        // Trailing punctuation usually ends the sentence, not the URL; keep balanced parens.
        while end > i + scheme.len() {
            let last = text[end - 1];
            let unbalanced_paren = last == ')'
                && text[i..end].iter().filter(|&&c| c == '(').count()
                    < text[i..end].iter().filter(|&&c| c == ')').count();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"') || unbalanced_paren {
                end -= 1;
            } else {
                break;
            }
        }
        if end > i + scheme.len() {
            found.push((i, end));
        }
        i = end.max(i + 1);
    }
    found
}

fn starts_with(text: &[char], prefix: &str) -> bool {
    let mut chars = text.iter();
    prefix
        .chars()
        .all(|p| chars.next().is_some_and(|c| c.eq_ignore_ascii_case(&p)))
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(
            c,
            '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^' | '\0'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Option<Vec<u8>> {
        encode_key(&Keystroke::parse(s).unwrap(), false, TermMode::empty())
    }

    fn typed(s: &str, ch: &str) -> Option<Vec<u8>> {
        let mut k = Keystroke::parse(s).unwrap();
        k.key_char = Some(ch.to_owned());
        encode_key(&k, false, TermMode::empty())
    }

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(typed("a", "a"), Some(b"a".to_vec()));
        assert_eq!(typed("shift-a", "A"), Some(b"A".to_vec()));
        assert_eq!(key("ctrl-c"), Some(vec![3]));
        assert_eq!(key("ctrl-["), Some(vec![0x1b]));
        assert_eq!(key("ctrl-space"), Some(vec![0]));
        assert_eq!(key("enter"), Some(b"\r".to_vec()));
        assert_eq!(key("ctrl-enter"), Some(b"\n".to_vec()));
        assert_eq!(key("backspace"), Some(vec![0x7f]));
        assert_eq!(key("ctrl-backspace"), Some(vec![0x08]));
        assert_eq!(key("shift-tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(typed("alt-b", "b"), Some(b"\x1bb".to_vec()));
        assert_eq!(key("ctrl-alt-c"), Some(vec![0x1b, 3]));
    }

    fn grid(columns: usize, bytes: &[u8]) -> GridSnapshot {
        use maomi_term::alacritty_terminal::Term;
        use maomi_term::alacritty_terminal::event::VoidListener;
        use maomi_term::alacritty_terminal::term::Config;
        use maomi_term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
        let size = maomi_term::TermSize::new(columns as u16, 4);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, bytes);
        GridSnapshot::from_term(&term)
    }

    #[test]
    fn urls_that_wrap_onto_the_next_row() {
        // "go https://example.com/a/b" in 12 columns: the URL runs from row 0 into row 2.
        let snap = grid(12, b"go https://example.com/a/b");
        let url = LinkTarget::Url("https://example.com/a/b".into());
        for (row, col) in [(0, 5), (1, 3), (2, 1)] {
            let link = link_at(&snap, row, col).expect("link");
            assert_eq!(link.target, url, "at ({row}, {col})");
            assert_eq!(
                link.segments().collect::<Vec<_>>(),
                [(0, 3, 12), (1, 0, 12), (2, 0, 2)]
            );
        }
        assert_eq!(link_at(&snap, 0, 1), None);
        // Cut to the rows from row 1 down, as when row 0 is scrolled out of view.
        let trimmed = link_at(&snap, 1, 3).unwrap().rebased(1);
        assert_eq!(
            trimmed.segments().collect::<Vec<_>>(),
            [(0, 0, 12), (1, 0, 2)]
        );
    }

    #[test]
    fn rows_without_a_wrap_stay_separate() {
        let snap = grid(40, b"https://a.example\r\nmore");
        let link = link_at(&snap, 0, 2).unwrap();
        assert_eq!(link.target, LinkTarget::Url("https://a.example".into()));
        assert_eq!(link_at(&snap, 1, 1), None);
    }

    #[test]
    fn app_shortcuts_are_not_sent() {
        assert_eq!(key("ctrl-shift-c"), None);
        assert_eq!(key("ctrl-shift-k"), None);
        assert_eq!(key("cmd-v"), None);
    }

    #[test]
    fn cursor_keys_follow_mode_and_modifiers() {
        assert_eq!(key("up"), Some(b"\x1b[A".to_vec()));
        let app = encode_key(
            &Keystroke::parse("up").unwrap(),
            false,
            TermMode::APP_CURSOR,
        );
        assert_eq!(app, Some(b"\x1bOA".to_vec()));
        assert_eq!(key("ctrl-right"), Some(b"\x1b[1;5C".to_vec()));
        assert_eq!(key("shift-home"), Some(b"\x1b[1;2H".to_vec()));
        assert_eq!(key("f1"), Some(b"\x1bOP".to_vec()));
        assert_eq!(key("f5"), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key("ctrl-delete"), Some(b"\x1b[3;5~".to_vec()));
        assert_eq!(key("pageup"), Some(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn altgr_characters_pass_through() {
        let mut k = Keystroke::parse("ctrl-alt-q").unwrap();
        k.key_char = Some("@".into());
        assert_eq!(encode_key(&k, true, TermMode::empty()), Some(b"@".to_vec()));
    }

    #[test]
    fn paste_modes() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            encode_paste("x\x1b[201~y", true),
            b"\x1b[200~xy\x1b[201~".to_vec()
        );
    }

    #[test]
    fn mouse_reports() {
        let none = gpui_kit::Modifiers::default();
        let sgr = TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse(ReportButton::Left, ReportKind::Press, 4, 9, &none, sgr),
            Some(b"\x1b[<0;5;10M".to_vec())
        );
        assert_eq!(
            encode_mouse(ReportButton::Left, ReportKind::Release, 4, 9, &none, sgr),
            Some(b"\x1b[<0;5;10m".to_vec())
        );
        assert_eq!(
            encode_mouse(ReportButton::Left, ReportKind::Motion, 0, 0, &none, sgr),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                ReportButton::Right,
                ReportKind::Release,
                0,
                0,
                &none,
                TermMode::empty()
            ),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33])
        );
        // Normal encoding cannot express columns past 222.
        assert_eq!(
            encode_mouse(
                ReportButton::Left,
                ReportKind::Press,
                300,
                0,
                &none,
                TermMode::empty()
            ),
            None
        );
    }

    fn urls(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        find_urls(&chars)
            .into_iter()
            .map(|(a, b)| chars[a..b].iter().collect())
            .collect()
    }

    #[test]
    fn url_detection() {
        assert_eq!(
            urls("see https://example.com/a?b=1."),
            ["https://example.com/a?b=1"]
        );
        assert_eq!(
            urls("(https://en.wikipedia.org/wiki/Cat_(disambiguation))"),
            ["https://en.wikipedia.org/wiki/Cat_(disambiguation)"]
        );
        assert_eq!(
            urls("x <http://a.b> file:///C:/x mailto:me@x.org"),
            ["http://a.b", "file:///C:/x", "mailto:me@x.org"]
        );
        assert!(urls("nohttps://x https:// plain").is_empty());
    }
}
