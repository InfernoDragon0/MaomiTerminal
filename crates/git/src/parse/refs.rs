//! Refs, stashes, remotes and ahead/behind counts.

use std::collections::HashMap;

use bstr::{BString, ByteSlice};

use super::{ParseError, ParseResult, lossy, parse_i64, parse_oid, parse_uint, records};

/// Separator inside `for-each-ref` and `stash list` records.
const US: u8 = 0x1f;

// ---- for-each-ref ------------------------------------------------------------------------

/// `--format=` value for `git for-each-ref` that [`parse_refs`] expects: one ref per line
/// (ref names and `contents:subject` never contain newlines), fields separated by `%1f`.
pub const REFS_FORMAT: &str = "%(refname)%1f%(refname:short)%1f%(objectname)%1f%(*objectname)%1f\
%(HEAD)%1f%(upstream)%1f%(upstream:track,nobracket)%1f%(creatordate:unix)%1f%(contents:subject)";

/// Arguments after `for-each-ref`.
pub fn refs_args() -> Vec<String> {
    [
        &format!("--format={REFS_FORMAT}"),
        "--sort=refname",
        "refs/heads",
        "refs/remotes",
        "refs/tags",
        "refs/stash",
    ]
    .map(str::to_owned)
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Stash,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// Full upstream ref, e.g. `refs/remotes/origin/main`.
    pub name: String,
    pub ahead: u32,
    pub behind: u32,
    /// `[gone]`: the upstream branch was deleted.
    pub gone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// e.g. `refs/heads/main`.
    pub full_name: String,
    /// e.g. `main`, `origin/main`, `v1.0`.
    pub short_name: String,
    pub kind: RefKind,
    /// Object the ref points at.
    pub oid: String,
    /// For annotated tags: the commit it peels to.
    pub peeled_oid: Option<String>,
    /// `*` in `%(HEAD)`.
    pub is_head: bool,
    pub upstream: Option<Upstream>,
    /// Unix seconds of the tip commit (creator date for tags).
    pub date: i64,
    pub subject: BString,
}

pub fn parse_refs(output: &[u8]) -> ParseResult<Vec<RefEntry>> {
    records(output, b'\n')
        .map(|(offset, line)| {
            parse_ref(line).ok_or_else(|| ParseError::new("for-each-ref", offset, "malformed ref"))
        })
        .collect()
}

fn parse_ref(line: &[u8]) -> Option<RefEntry> {
    let mut fields = line.splitn(9, |&b| b == US);
    let mut field = || fields.next();
    let full_name = lossy(field().filter(|name| !name.is_empty())?);
    let short_name = lossy(field()?);
    let oid = parse_oid(field()?)?;
    let peeled_oid = match field()? {
        b"" => None,
        peeled => Some(parse_oid(peeled)?),
    };
    let is_head = match field()? {
        b"*" => true,
        b" " | b"" => false,
        _ => return None,
    };
    let upstream = field()?;
    let track = field()?;
    let upstream = match upstream {
        b"" => None,
        name => Some(parse_upstream(name, track)?),
    };
    let date = match field()? {
        b"" => 0,
        date => parse_i64(date)?,
    };
    let subject = BString::from(field()?);
    let kind = if full_name.starts_with("refs/heads/") {
        RefKind::LocalBranch
    } else if full_name.starts_with("refs/remotes/") {
        RefKind::RemoteBranch
    } else if full_name.starts_with("refs/tags/") {
        RefKind::Tag
    } else if full_name == "refs/stash" {
        RefKind::Stash
    } else {
        RefKind::Other
    };
    Some(RefEntry {
        full_name,
        short_name,
        kind,
        oid,
        peeled_oid,
        is_head,
        upstream,
        date,
        subject,
    })
}

/// `%(upstream:track,nobracket)`: empty, `gone`, `ahead 1`, `behind 2` or `ahead 1, behind 2`.
fn parse_upstream(name: &[u8], track: &[u8]) -> Option<Upstream> {
    let mut upstream = Upstream {
        name: lossy(name),
        ahead: 0,
        behind: 0,
        gone: false,
    };
    if track == b"gone" {
        upstream.gone = true;
        return Some(upstream);
    }
    for part in track.split_str(", ").filter(|part| !part.is_empty()) {
        if let Some(n) = part.strip_prefix(b"ahead ") {
            upstream.ahead = parse_uint(n)?;
        } else {
            upstream.behind = parse_uint(part.strip_prefix(b"behind ")?)?;
        }
    }
    Some(upstream)
}

// ---- stash list --------------------------------------------------------------------------

/// Arguments after `stash` (i.e. `stash list ...`) that [`parse_stashes`] expects.
pub fn stash_args() -> Vec<String> {
    ["list", "-z", "--format=%gd%x1f%H%x1f%ct%x1f%gs"]
        .map(str::to_owned)
        .into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// `n` in `stash@{n}`.
    pub index: u32,
    pub oid: String,
    /// Unix seconds.
    pub time: i64,
    /// Reflog subject, e.g. `WIP on main: abc123 msg`.
    pub message: BString,
}

pub fn parse_stashes(output: &[u8]) -> ParseResult<Vec<Stash>> {
    records(output, 0)
        .map(|(offset, record)| {
            parse_stash(record)
                .ok_or_else(|| ParseError::new("stash list", offset, "malformed stash"))
        })
        .collect()
}

fn parse_stash(record: &[u8]) -> Option<Stash> {
    let mut fields = record.splitn(4, |&b| b == US);
    let mut field = || fields.next();
    let selector = field()?;
    let index = selector.rfind("@{").and_then(|at| selector.get(at + 2..))?;
    Some(Stash {
        index: parse_uint(index.strip_suffix(b"}")?)?,
        oid: parse_oid(field()?)?,
        time: parse_i64(field()?)?,
        message: BString::from(field()?),
    })
}

// ---- remote -v ---------------------------------------------------------------------------

/// Arguments after `remote`.
pub const REMOTE_ARGS: &[&str] = &["-v"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

/// Merges the `(fetch)` / `(push)` lines per remote, in first-seen order. With several push
/// URLs the first one wins.
pub fn parse_remotes(output: &[u8]) -> ParseResult<Vec<Remote>> {
    let mut remotes: Vec<Remote> = Vec::new();
    let mut by_name = HashMap::new();
    for (offset, line) in records(output, b'\n') {
        let err = || ParseError::new("remote -v", offset, "malformed remote line");
        let (name, rest) = line.split_once_str("\t").ok_or_else(err)?;
        let (url, push) = match (
            rest.strip_suffix(b" (fetch)"),
            rest.strip_suffix(b" (push)"),
        ) {
            (Some(url), _) => (url, false),
            (None, Some(url)) => (url, true),
            (None, None) => return Err(err()),
        };
        if name.is_empty() {
            return Err(err());
        }
        let index = *by_name.entry(name).or_insert_with(|| {
            remotes.push(Remote {
                name: lossy(name),
                fetch_url: None,
                push_url: None,
            });
            remotes.len() - 1
        });
        let remote = &mut remotes[index];
        let slot = if push {
            &mut remote.push_url
        } else {
            &mut remote.fetch_url
        };
        slot.get_or_insert_with(|| lossy(url));
    }
    Ok(remotes)
}

// ---- rev-list --left-right --count A...B -------------------------------------------------

/// Arguments after `rev-list` (callers append `A...B`).
pub const AHEAD_BEHIND_ARGS: &[&str] = &["--left-right", "--count"];

/// Returns `(ahead, behind)` = (left-only, right-only) counts.
pub fn parse_ahead_behind(output: &[u8]) -> ParseResult<(u32, u32)> {
    let line = output.strip_suffix(b"\n").unwrap_or(output);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.split_once_str("\t")
        .and_then(|(ahead, behind)| Some((parse_uint(ahead)?, parse_uint(behind)?)))
        .ok_or_else(|| ParseError::new("rev-list --count", 0, "expected `<ahead>\t<behind>`"))
}
