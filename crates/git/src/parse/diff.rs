//! `git diff -p` parser to File / Hunk / Line. Hunk headers are kept verbatim so a hunk can be
//! re-emitted as a patch for `git apply --cached` (hunk and line staging).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bstr::{BString, ByteSlice};

use super::{ParseError, ParseResult, parse_uint};

/// Arguments after `diff` (or `diff --cached`, `show`) that [`parse`] expects; callers append
/// revisions and then `--` + paths.
pub const ARGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "-p",
    "-U3",
    "--find-renames",
    "--submodule=short",
    // Override `diff.noprefix` / `diff.mnemonicPrefix` so paths always carry `a/` / `b/`.
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// Fewest / most context lines around a hunk the view settings allow. Zero is excluded: a hunk
/// with no context cannot be staged back with `git apply` without `--unidiff-zero`.
pub const CONTEXT_RANGE: std::ops::RangeInclusive<u32> = 1..=20;

static CONTEXT_LINES: AtomicU32 = AtomicU32::new(3);
static IGNORE_WHITESPACE: AtomicBool = AtomicBool::new(false);

/// How diffs are produced from now on (the Settings page): context lines around each hunk
/// (clamped to [`CONTEXT_RANGE`]) and, for committed history only, whether whitespace changes
/// are ignored (`-w`). Working-copy diffs never ignore whitespace: hunks staged from a `-w` diff
/// would not apply to the file.
pub fn set_view_options(context: u32, ignore_whitespace: bool) {
    CONTEXT_LINES.store(clamp_context(context), Ordering::Relaxed);
    IGNORE_WHITESPACE.store(ignore_whitespace, Ordering::Relaxed);
}

fn clamp_context(context: u32) -> u32 {
    context.clamp(*CONTEXT_RANGE.start(), *CONTEXT_RANGE.end())
}

/// [`ARGS`] with the current view options; `committed` is true for diffs between commits.
pub fn args(committed: bool) -> Vec<String> {
    build_args(
        CONTEXT_LINES.load(Ordering::Relaxed),
        IGNORE_WHITESPACE.load(Ordering::Relaxed),
        committed,
    )
}

fn build_args(context: u32, ignore_whitespace: bool, committed: bool) -> Vec<String> {
    let mut out: Vec<String> = ARGS
        .iter()
        .map(|a| {
            if *a == "-U3" {
                format!("-U{}", clamp_context(context))
            } else {
                (*a).to_string()
            }
        })
        .collect();
    if committed && ignore_whitespace {
        out.push("-w".into());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
    Renamed {
        similarity: u8,
    },
    Copied {
        similarity: u8,
    },
    /// Only the mode changed (no hunks).
    ModeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// `None` for an added file (`/dev/null`).
    pub old_path: Option<BString>,
    /// `None` for a deleted file.
    pub new_path: Option<BString>,
    pub change: FileChange,
    /// Octal modes as written by git (e.g. "100644"), when present.
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// `Binary files a/x and b/x differ`: no hunks.
    pub binary: bool,
    /// Lines from `diff --git` up to the first hunk, verbatim (needed to rebuild a patch).
    pub header: BString,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ section` line without the trailing newline.
    pub header: BString,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Text after the closing `@@` (function context), may be empty.
    pub section: BString,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Deleted,
    /// `\ No newline at end of file`; applies to the line before it.
    NoNewlineAtEof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// Without the leading ` `/`+`/`-` and without the trailing `\n` (a `\r` is kept).
    pub content: BString,
}

/// Parses a whole patch. Lines before the first `diff ` header (e.g. a `git show` commit header)
/// are skipped. Combined diffs (`diff --cc`, which `git diff` prints for unmerged paths) and
/// `* Unmerged path` lines are skipped too: [`FileDiff`] has a single old side.
pub fn parse(output: &[u8]) -> ParseResult<Vec<FileDiff>> {
    let mut lines = Lines {
        input: output,
        pos: 0,
    };
    let mut files = Vec::new();
    let mut started = false;
    while let Some((offset, line)) = lines.next() {
        if let Some(paths) = line.strip_prefix(b"diff --git ") {
            files.push(file(&mut lines, offset, paths)?);
        } else if line.starts_with(b"diff --cc ") || line.starts_with(b"diff --combined ") {
            skip_combined(&mut lines);
        } else if started && !line.starts_with(b"* Unmerged path ") {
            return Err(error(offset, "unexpected line between files"));
        }
        started |= starts_section(line);
    }
    Ok(files)
}

/// The start of a patch that stopped mid-stream, cut back to where its last file began: whole
/// files only, so it still parses (hunks are checked against their headers). Empty when the
/// first file is the one cut.
pub fn whole_files(truncated: &[u8]) -> &[u8] {
    // Hunk lines start with ` `, `+`, `-` or `\`, so `diff ` at a line start is a file header.
    match memchr::memmem::rfind(truncated, b"\ndiff ") {
        Some(at) => &truncated[..=at],
        None => &[],
    }
}

fn error(offset: usize, reason: &str) -> ParseError {
    ParseError::new("diff", offset, reason)
}

/// Cursor over `\n`-terminated lines; `Copy` so callers can look ahead and commit.
#[derive(Clone, Copy)]
struct Lines<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Lines<'a> {
    /// Next line without its `\n`, with its offset.
    fn next(&mut self) -> Option<(usize, &'a [u8])> {
        let rest = self.input.get(self.pos..).filter(|rest| !rest.is_empty())?;
        let start = self.pos;
        let len = memchr::memchr(b'\n', rest).unwrap_or(rest.len());
        self.pos = start + len + 1;
        Some((start, &rest[..len]))
    }

    /// Next line if `accept` takes it; otherwise the cursor stays put.
    fn next_if(&mut self, accept: impl Fn(&[u8]) -> bool) -> Option<(usize, &'a [u8])> {
        let mut ahead = *self;
        let line = ahead.next().filter(|(_, line)| accept(line))?;
        *self = ahead;
        Some(line)
    }

    fn consumed(&self) -> usize {
        self.pos.min(self.input.len())
    }
}

fn starts_section(line: &[u8]) -> bool {
    line.starts_with(b"diff ") || line.starts_with(b"* Unmerged path ")
}

fn skip_combined(lines: &mut Lines) {
    while lines.next_if(|line| !starts_section(line)).is_some() {}
}

/// One `diff --git` section; `start` is the offset of that line.
fn file(lines: &mut Lines, start: usize, git_paths: &[u8]) -> ParseResult<FileDiff> {
    let mut h = Header::default();
    while let Some((offset, line)) =
        lines.next_if(|line| !line.starts_with(b"@@ ") && !starts_section(line))
    {
        h.line(line)
            .ok_or_else(|| error(offset, "malformed file header line"))?;
    }
    let header = BString::from(lines.input.get(start..lines.consumed()).unwrap_or_default());

    let mut hunks = Vec::new();
    while let Some((offset, line)) = lines.next_if(|line| line.starts_with(b"@@ ")) {
        hunks.push(hunk(lines, offset, line)?);
    }

    let (git_old, git_new) = split_git_paths(git_paths).unzip();
    let old_path = match (h.new_file, h.from, h.minus) {
        (true, ..) => None,
        (false, Some(from), _) => Some(from),
        (false, None, Some(minus)) => minus,
        (false, None, None) => git_old,
    };
    let new_path = match (h.deleted, h.to, h.plus) {
        (true, ..) => None,
        (false, Some(to), _) => Some(to),
        (false, None, Some(plus)) => plus,
        (false, None, None) => git_new,
    };
    if (old_path.is_none() && !h.new_file) || (new_path.is_none() && !h.deleted) {
        return Err(error(start, "cannot determine file paths"));
    }

    let similarity = h.similarity.unwrap_or(0);
    let change = if h.new_file {
        FileChange::Added
    } else if h.deleted {
        FileChange::Deleted
    } else if h.rename {
        FileChange::Renamed { similarity }
    } else if h.copy {
        FileChange::Copied { similarity }
    } else if h.mode_change && hunks.is_empty() && !h.binary {
        FileChange::ModeChanged
    } else {
        FileChange::Modified
    };
    Ok(FileDiff {
        old_path,
        new_path,
        change,
        old_mode: h.old_mode,
        new_mode: h.new_mode,
        binary: h.binary,
        header,
        hunks,
    })
}

/// What the extended header lines of one file say.
#[derive(Default)]
struct Header {
    new_file: bool,
    deleted: bool,
    rename: bool,
    copy: bool,
    mode_change: bool,
    binary: bool,
    similarity: Option<u8>,
    from: Option<BString>,
    to: Option<BString>,
    /// `--- ` line; `Some(None)` is `/dev/null`.
    minus: Option<Option<BString>>,
    plus: Option<Option<BString>>,
    old_mode: Option<String>,
    new_mode: Option<String>,
}

impl Header {
    /// Unknown lines (e.g. a `GIT binary patch` payload) are ignored.
    fn line(&mut self, line: &[u8]) -> Option<()> {
        if let Some(mode) = line.strip_prefix(b"old mode ") {
            self.old_mode = Some(file_mode(mode)?);
            self.mode_change = true;
        } else if let Some(mode) = line.strip_prefix(b"new mode ") {
            self.new_mode = Some(file_mode(mode)?);
            self.mode_change = true;
        } else if let Some(mode) = line.strip_prefix(b"deleted file mode ") {
            self.old_mode = Some(file_mode(mode)?);
            self.deleted = true;
        } else if let Some(mode) = line.strip_prefix(b"new file mode ") {
            self.new_mode = Some(file_mode(mode)?);
            self.new_file = true;
        } else if let Some(index) = line.strip_prefix(b"index ") {
            if let Some((_, mode)) = index.split_once_str(" ") {
                let mode = file_mode(mode)?;
                self.old_mode.get_or_insert_with(|| mode.clone());
                self.new_mode.get_or_insert(mode);
            }
        } else if let Some(percent) = line.strip_prefix(b"similarity index ") {
            let similarity = parse_uint(percent.strip_suffix(b"%")?).filter(|&s| s <= 100)?;
            self.similarity = Some(similarity);
        } else if let Some(path) = line.strip_prefix(b"rename from ") {
            self.rename = true;
            self.from = Some(header_path(path)?);
        } else if let Some(path) = line.strip_prefix(b"rename to ") {
            self.rename = true;
            self.to = Some(header_path(path)?);
        } else if let Some(path) = line.strip_prefix(b"copy from ") {
            self.copy = true;
            self.from = Some(header_path(path)?);
        } else if let Some(path) = line.strip_prefix(b"copy to ") {
            self.copy = true;
            self.to = Some(header_path(path)?);
        } else if let Some(path) = line.strip_prefix(b"--- ") {
            self.minus = Some(side_path(path, b"a/")?);
        } else if let Some(path) = line.strip_prefix(b"+++ ") {
            self.plus = Some(side_path(path, b"b/")?);
        } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
            self.binary = true;
        }
        Some(())
    }
}

/// Octal mode such as `100644`.
fn file_mode(mode: &[u8]) -> Option<String> {
    let valid = (1..=6).contains(&mode.len()) && mode.iter().all(|b| (b'0'..=b'7').contains(b));
    valid.then(|| String::from_utf8_lossy(mode).into_owned())
}

/// A path as git prints it in a header line: C-quoted, or raw with a trailing tab that git adds
/// when the name contains a space.
fn header_path(text: &[u8]) -> Option<BString> {
    let path = if text.first() == Some(&b'"') {
        unquote(text)?.0
    } else {
        BString::from(text.strip_suffix(b"\t").unwrap_or(text))
    };
    (!path.is_empty()).then_some(path)
}

/// `--- a/x` / `+++ b/x` path; `None` for `/dev/null`.
fn side_path(text: &[u8], prefix: &[u8]) -> Option<Option<BString>> {
    if text.strip_suffix(b"\t").unwrap_or(text) == b"/dev/null" {
        return Some(None);
    }
    Some(Some(strip_prefix(header_path(text)?, prefix)))
}

fn strip_prefix(path: BString, prefix: &[u8]) -> BString {
    match path.strip_prefix(prefix) {
        Some(rest) if !rest.is_empty() => BString::from(rest),
        _ => path,
    }
}

/// Splits `a/x b/y` from a `diff --git` line. Only used when no `---`/`+++`/`rename` line names
/// the paths, in which case both sides are the same path, so an unquoted line splits in half.
fn split_git_paths(text: &[u8]) -> Option<(BString, BString)> {
    let (old, new) = if text.first() == Some(&b'"') {
        let (old, rest) = unquote(text)?;
        let rest = rest.strip_prefix(b" ")?;
        let new = match rest.first() {
            Some(b'"') => unquote(rest)?.0,
            _ => BString::from(rest),
        };
        (old, new)
    } else if text.last() == Some(&b'"') {
        // An unquoted path has no `"`, so the first ` "` separates the two.
        let space = text.find(" \"")?;
        let (old, new) = text.split_at(space);
        (BString::from(old), unquote(new.get(1..)?)?.0)
    } else {
        let mid = text.len() / 2;
        let (old, new) = match text.split_at_checked(mid) {
            Some((old, [b' ', new @ ..]))
                if text.len() % 2 == 1 && old.get(2..) == new.get(2..) =>
            {
                (old, new)
            }
            _ => {
                let space = text.find(" b/")?;
                (&text[..space], &text[space + 1..])
            }
        };
        (BString::from(old), BString::from(new))
    };
    Some((strip_prefix(old, b"a/"), strip_prefix(new, b"b/")))
}

/// Decodes a C-quoted string starting with `"`; returns it and what follows the closing quote.
fn unquote(text: &[u8]) -> Option<(BString, &[u8])> {
    let mut out = Vec::new();
    let mut bytes = text.strip_prefix(b"\"")?.iter();
    loop {
        match *bytes.next()? {
            b'"' => return Some((BString::from(out), bytes.as_slice())),
            b'\\' => {
                let escaped = *bytes.next()?;
                out.push(match escaped {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0b,
                    b'f' => 0x0c,
                    b'r' => b'\r',
                    b'"' | b'\\' => escaped,
                    b'0'..=b'3' => {
                        let mut octal = || bytes.next().filter(|d| (b'0'..=b'7').contains(d));
                        let (mid, low) = (*octal()? - b'0', *octal()? - b'0');
                        (escaped - b'0') << 6 | mid << 3 | low
                    }
                    _ => return None,
                });
            }
            byte => out.push(byte),
        }
    }
}

/// One hunk; `header` is its `@@` line at `offset`. Line counts must match the header.
fn hunk(lines: &mut Lines, offset: usize, header: &[u8]) -> ParseResult<Hunk> {
    let (old_start, old_lines, new_start, new_lines, section) =
        hunk_header(header).ok_or_else(|| error(offset, "malformed hunk header"))?;
    let (mut old_left, mut new_left) = (old_lines, new_lines);
    let (mut old_no, mut new_no) = (old_start, new_start);
    let capacity = old_lines.saturating_add(new_lines).min(1024);
    let mut out = Vec::with_capacity(usize::try_from(capacity).unwrap_or_default());
    loop {
        let mut ahead = *lines;
        let Some((line_offset, line)) = ahead.next() else {
            break;
        };
        let (kind, content) = match line.split_first() {
            Some((b'\\', text)) if !out.is_empty() => (
                LineKind::NoNewlineAtEof,
                text.strip_prefix(b" ").unwrap_or(text),
            ),
            _ if old_left == 0 && new_left == 0 => break,
            Some((b' ', text)) => (LineKind::Context, text),
            // `diff.suppressBlankEmpty` drops the space of an empty context line.
            None => (LineKind::Context, &[][..]),
            Some((b'-', text)) => (LineKind::Deleted, text),
            Some((b'+', text)) => (LineKind::Added, text),
            _ => return Err(error(line_offset, "hunk shorter than its header")),
        };
        let numbers = match kind {
            LineKind::Context => step(&mut old_left, &mut old_no)
                .zip(step(&mut new_left, &mut new_no))
                .map(|(old, new)| (Some(old), Some(new))),
            LineKind::Deleted => step(&mut old_left, &mut old_no).map(|old| (Some(old), None)),
            LineKind::Added => step(&mut new_left, &mut new_no).map(|new| (None, Some(new))),
            LineKind::NoNewlineAtEof => Some((None, None)),
        };
        let (old_lineno, new_lineno) =
            numbers.ok_or_else(|| error(line_offset, "hunk longer than its header"))?;
        *lines = ahead;
        out.push(Line {
            kind,
            old_lineno,
            new_lineno,
            content: BString::from(content),
        });
    }
    if old_left != 0 || new_left != 0 {
        return Err(error(lines.consumed(), "hunk shorter than its header"));
    }
    Ok(Hunk {
        header: BString::from(header),
        old_start,
        old_lines,
        new_start,
        new_lines,
        section,
        lines: out,
    })
}

/// Takes one line from a side: returns its number, or `None` if the side has none left.
fn step(left: &mut u32, lineno: &mut u32) -> Option<u32> {
    *left = left.checked_sub(1)?;
    let current = *lineno;
    *lineno = lineno.saturating_add(1);
    Some(current)
}

/// `@@ -a[,b] +c[,d] @@[ section]`; an omitted count is 1.
fn hunk_header(line: &[u8]) -> Option<(u32, u32, u32, u32, BString)> {
    let rest = line.strip_prefix(b"@@ -")?;
    let (old, rest) = rest.split_once_str(" +")?;
    let (new, rest) = rest.split_once_str(" @@")?;
    let range = |range: &[u8]| -> Option<(u32, u32)> {
        let (start, count): (u32, u32) = match range.split_once_str(",") {
            Some((start, count)) => (parse_uint(start)?, parse_uint(count)?),
            None => (parse_uint(range)?, 1),
        };
        start.checked_add(count)?;
        Some((start, count))
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    let section = BString::from(rest.strip_prefix(b" ").unwrap_or(rest));
    Some((old_start, old_lines, new_start, new_lines, section))
}

/// One file's diff from bare hunks (`@@ ... @@` onwards, no `diff --git` header), as hosting
/// APIs send them (GitHub's `patch`, GitLab's `diff`). `change` and the paths are taken as
/// given; the header is rebuilt so the result copies as a patch. An empty `hunks` gives a
/// file without hunks.
pub fn from_hunks(
    old_path: Option<&[u8]>,
    new_path: Option<&[u8]>,
    change: FileChange,
    hunks: &[u8],
) -> ParseResult<FileDiff> {
    // Paths a header line can hold as-is; anything else gets a stand-in (the real paths are
    // set below, the header only matters for copying).
    let plain = |p: &[u8]| {
        !p.is_empty()
            && !p.iter().any(|&b| {
                b.is_ascii_whitespace() || b.is_ascii_control() || b == b'"' || b == b'\\'
            })
    };
    let shown_old = old_path.filter(|p| plain(p)).unwrap_or(b"file");
    let shown_new = new_path.filter(|p| plain(p)).unwrap_or(shown_old);
    let shown_old = if old_path.is_some() {
        shown_old
    } else {
        shown_new
    };
    let mut text = Vec::with_capacity(hunks.len() + 128);
    text.extend_from_slice(b"diff --git a/");
    text.extend_from_slice(shown_old);
    text.extend_from_slice(b" b/");
    text.extend_from_slice(shown_new);
    text.push(b'\n');
    match change {
        FileChange::Added => text.extend_from_slice(b"new file mode 100644\n"),
        FileChange::Deleted => text.extend_from_slice(b"deleted file mode 100644\n"),
        _ => {}
    }
    let side = |text: &mut Vec<u8>, prefix: &[u8], path: Option<&[u8]>| {
        match path {
            Some(path) => {
                text.extend_from_slice(prefix);
                text.extend_from_slice(path);
            }
            None => text.extend_from_slice(b"/dev/null"),
        }
        text.push(b'\n');
    };
    if !hunks.trim().is_empty() {
        if !hunks.starts_with(b"@@ ") {
            return Err(error(0, "hunks must start with `@@ `"));
        }
        let old = (change != FileChange::Added).then_some(shown_old);
        let new = (change != FileChange::Deleted).then_some(shown_new);
        side(&mut text, b"--- a/", old);
        side(&mut text, b"+++ b/", new);
        text.extend_from_slice(hunks);
        if !hunks.ends_with(b"\n") {
            text.push(b'\n');
        }
    }
    let mut file = parse(&text)?
        .into_iter()
        .next()
        .ok_or_else(|| error(0, "no file"))?;
    file.old_path = match change {
        FileChange::Added => None,
        _ => old_path.or(new_path).map(BString::from),
    };
    file.new_path = match change {
        FileChange::Deleted => None,
        _ => new_path.or(old_path).map(BString::from),
    };
    file.change = change;
    Ok(file)
}

#[cfg(test)]
mod from_hunks_tests {
    use super::*;

    #[test]
    fn bare_hunks_become_a_file() {
        let hunks = b"@@ -1,2 +1,2 @@ fn main\n a\n-b\n+c\n\\ No newline at end of file";
        let file = from_hunks(
            Some(b"src/old name.rs"),
            Some(b"src/new.rs"),
            FileChange::Renamed { similarity: 90 },
            hunks,
        )
        .unwrap();
        assert_eq!(file.old_path, Some(BString::from("src/old name.rs")));
        assert_eq!(file.new_path, Some(BString::from("src/new.rs")));
        assert_eq!(file.change, FileChange::Renamed { similarity: 90 });
        assert_eq!(file.hunks.len(), 1);
        let kinds: Vec<LineKind> = file.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Deleted,
                LineKind::Added,
                LineKind::NoNewlineAtEof
            ]
        );
        assert_eq!(file.hunks[0].section, "fn main");

        let added = from_hunks(
            None,
            Some(b"a.txt"),
            FileChange::Added,
            b"@@ -0,0 +1 @@\n+x\n",
        )
        .unwrap();
        assert_eq!(added.old_path, None);
        assert_eq!(added.hunks[0].lines[0].new_lineno, Some(1));
        let deleted = from_hunks(
            Some(b"a.txt"),
            None,
            FileChange::Deleted,
            b"@@ -1 +0,0 @@\n-x\n",
        )
        .unwrap();
        assert_eq!(deleted.new_path, None);
        // No patch from the host (binary, too large): a file without hunks.
        let empty = from_hunks(Some(b"b.bin"), Some(b"b.bin"), FileChange::Modified, b"").unwrap();
        assert!(empty.hunks.is_empty());
        assert!(from_hunks(Some(b"x"), Some(b"x"), FileChange::Modified, b"garbage\n").is_err());
    }
}

#[cfg(test)]
mod option_tests {
    use super::*;

    #[test]
    fn view_options_change_context_and_whitespace_only_where_safe() {
        let default = build_args(3, false, true);
        assert_eq!(
            default,
            ARGS.iter().map(|a| a.to_string()).collect::<Vec<_>>()
        );

        let wide = build_args(8, true, true);
        assert!(wide.contains(&"-U8".to_string()) && !wide.contains(&"-U3".to_string()));
        assert_eq!(wide.last().map(String::as_str), Some("-w"));

        // Working-copy diffs keep whitespace: staged hunks must apply to the file.
        assert!(!build_args(8, true, false).contains(&"-w".to_string()));
        // Zero and huge contexts are clamped.
        assert!(build_args(0, false, false).contains(&"-U1".to_string()));
        assert!(build_args(500, false, false).contains(&"-U20".to_string()));
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    const TWO: &str = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n\
                       diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n";

    #[test]
    fn truncated_patch_is_cut_back_to_whole_files() {
        // Cut inside the second file's hunk: only the first file is left, and it parses.
        let cut = &TWO.as_bytes()[..TWO.len() - 6];
        let whole = whole_files(cut);
        assert!(whole.ends_with(b"+y\n"));
        let files = parse(whole).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        // The uncut patch parses to both files.
        assert_eq!(parse(TWO.as_bytes()).unwrap().len(), 2);
        // The first (only) file is cut: nothing whole to show.
        assert!(whole_files(&TWO.as_bytes()[..30]).is_empty());
        assert!(whole_files(b"").is_empty());
        assert!(
            parse(whole_files(&TWO.as_bytes()[..30]))
                .unwrap()
                .is_empty()
        );
    }
}
