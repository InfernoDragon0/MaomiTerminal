//! `git status --porcelain=v2 -z --branch --show-stash --untracked-files=all` parser.

use bstr::{BString, ByteSlice};

use super::{ParseError, ParseResult, lossy, parse_oid, parse_uint, records};

/// Arguments after `status` that [`parse`] expects.
pub const ARGS: &[&str] = &[
    "--porcelain=v2",
    "-z",
    "--branch",
    "--show-stash",
    "--untracked-files=all",
    "--ignore-submodules=none",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: BranchInfo,
    /// From `# stash <n>`; 0 when absent.
    pub stash_count: u32,
    pub entries: Vec<StatusEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchInfo {
    /// `branch.oid`; `None` for `(initial)` (no commits yet).
    pub oid: Option<String>,
    /// `branch.head`; `None` when detached.
    pub head: Option<String>,
    /// `branch.upstream`.
    pub upstream: Option<String>,
    /// `branch.ab +A -B`; both 0 when absent.
    pub ahead: u32,
    pub behind: u32,
}

/// One XY state letter from porcelain v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileState {
    /// `.`
    #[default]
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// `U` (only inside unmerged entries)
    Unmerged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// `1` line.
    Ordinary,
    /// `2` line: rename or copy; `score` is the `R100` / `C75` number.
    RenamedOrCopied { score: u8 },
    /// `u` line; which sides are involved is in `index` / `worktree` (e.g. UU, AA, DU).
    Unmerged,
    /// `?` line.
    Untracked,
    /// `!` line.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub kind: EntryKind,
    /// X: staged (index vs HEAD).
    pub index: FileState,
    /// Y: unstaged (worktree vs index).
    pub worktree: FileState,
    /// Path relative to the repo root, exact bytes (no quoting with `-z`).
    pub path: BString,
    /// Source path of a rename / copy.
    pub orig_path: Option<BString>,
    /// `<sub>` field: `N...` means not a submodule.
    pub submodule: bool,
}

/// Parses the whole output (records are NUL-terminated).
pub fn parse(output: &[u8]) -> ParseResult<Status> {
    let mut status = Status::default();
    let mut records = records(output, 0);
    while let Some((offset, record)) = records.next() {
        let err = |reason: &str| ParseError::new("status", offset, reason);
        let (tag, rest) = match record {
            [tag, b' ', rest @ ..] => (*tag, rest),
            _ => return Err(err("malformed record")),
        };
        let entry = match tag {
            b'#' => {
                header(&mut status, rest).ok_or_else(|| err("malformed header"))?;
                continue;
            }
            b'1' => {
                let ([xy, sub, ..], path) = fields::<7>(rest).ok_or_else(|| err("short entry"))?;
                entry(EntryKind::Ordinary, xy, sub, path)
            }
            b'2' => {
                let ([xy, sub, .., score], path) =
                    fields::<8>(rest).ok_or_else(|| err("short entry"))?;
                let kind = match score {
                    [b'R' | b'C', digits @ ..] => EntryKind::RenamedOrCopied {
                        score: parse_uint(digits).ok_or_else(|| err("bad rename score"))?,
                    },
                    _ => return Err(err("bad rename score")),
                };
                let (_, orig) = records.next().ok_or_else(|| err("missing original path"))?;
                entry(kind, xy, sub, path).map(|mut entry| {
                    entry.orig_path = Some(BString::from(orig));
                    entry
                })
            }
            b'u' => {
                let ([xy, sub, ..], path) = fields::<9>(rest).ok_or_else(|| err("short entry"))?;
                entry(EntryKind::Unmerged, xy, sub, path)
            }
            b'?' => entry(EntryKind::Untracked, b"..", b"N...", rest),
            b'!' => entry(EntryKind::Ignored, b"..", b"N...", rest),
            _ => return Err(err("unknown entry type")),
        };
        status
            .entries
            .push(entry.ok_or_else(|| err("malformed entry"))?);
    }
    Ok(status)
}

/// Splits `N` space-separated fields off the front; the rest is the path (may contain spaces).
fn fields<const N: usize>(rest: &[u8]) -> Option<([&[u8]; N], &[u8])> {
    let mut fields = [&[][..]; N];
    let mut rest = rest;
    for field in &mut fields {
        let (head, tail) = rest.split_once_str(" ")?;
        *field = head;
        rest = tail;
    }
    Some((fields, rest))
}

fn entry(kind: EntryKind, xy: &[u8], sub: &[u8], path: &[u8]) -> Option<StatusEntry> {
    let &[x, y] = xy else { return None };
    let submodule = match sub {
        [b'N', b'.', b'.', b'.'] => false,
        [b'S', _, _, _] => true,
        _ => return None,
    };
    if path.is_empty() {
        return None;
    }
    Some(StatusEntry {
        kind,
        index: file_state(x)?,
        worktree: file_state(y)?,
        path: BString::from(path),
        orig_path: None,
        submodule,
    })
}

fn file_state(letter: u8) -> Option<FileState> {
    Some(match letter {
        b'.' => FileState::Unmodified,
        b'M' => FileState::Modified,
        b'T' => FileState::TypeChanged,
        b'A' => FileState::Added,
        b'D' => FileState::Deleted,
        b'R' => FileState::Renamed,
        b'C' => FileState::Copied,
        b'U' => FileState::Unmerged,
        _ => return None,
    })
}

/// Applies one `# key value` line; unknown keys are ignored.
fn header(status: &mut Status, line: &[u8]) -> Option<()> {
    let (key, value) = line.split_once_str(" ").unwrap_or((line, b""));
    let name = || (!value.is_empty()).then(|| lossy(value));
    let branch = &mut status.branch;
    match key {
        b"branch.oid" if value == b"(initial)" => branch.oid = None,
        b"branch.oid" => branch.oid = Some(parse_oid(value)?),
        b"branch.head" if value == b"(detached)" => branch.head = None,
        b"branch.head" => branch.head = Some(name()?),
        b"branch.upstream" => branch.upstream = Some(name()?),
        b"branch.ab" => {
            let (ahead, behind) = value.split_once_str(" ")?;
            branch.ahead = parse_uint(ahead.strip_prefix(b"+")?)?;
            branch.behind = parse_uint(behind.strip_prefix(b"-")?)?;
        }
        b"stash" => status.stash_count = parse_uint(value)?,
        _ => {}
    }
    Some(())
}
