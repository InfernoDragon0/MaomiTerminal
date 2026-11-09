//! `git log -z --format=<FORMAT>` parser, incremental so the graph can lay out lanes while
//! commits stream in (chunks of 500, design "Performance & app shell").

use bstr::{BString, ByteSlice};

use super::{ParseError, ParseResult, lossy, parse_i64, parse_oid, parse_uint};

/// `--format=` value [`LogParser`] expects. Fields are separated by `%x1f`; records by NUL (`-z`).
/// Dates come from `--date=raw` (`<unix> <+hhmm>`). Free text goes last: subject, then body.
pub const FORMAT: &str = "%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%D%x1f%s%x1f%b";

/// [`FORMAT`] with only the `Co-authored-by` trailers as the body, for the commit graph: bodies
/// can be large and the graph only shows subjects and avatars (the details panel reads the full
/// message on demand). [`LogParser`] accepts both; [`co_authors`] reads either body.
pub const GRAPH_FORMAT: &str = "%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%D%x1f%s%x1f%(trailers:key=Co-authored-by)";

/// Number of `%x1f` separators in [`FORMAT`] before the body.
const SEPARATORS: usize = 10;

/// Arguments after `log` (callers add revisions, `-n`, `--skip`, then `--end-of-options` / `--`).
pub fn args() -> Vec<String> {
    args_with(FORMAT)
}

/// [`args`] with [`FORMAT`] or [`GRAPH_FORMAT`].
pub fn args_with(format: &str) -> Vec<String> {
    vec![
        "-z".into(),
        format!("--format={format}"),
        "--date=raw".into(),
        "--decorate=full".into(),
        "--no-color".into(),
        "--no-show-signature".into(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: BString,
    pub email: BString,
    /// Unix seconds.
    pub time: i64,
    /// UTC offset in minutes (e.g. +0200 -> 120).
    pub tz_offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefLabel {
    /// `HEAD -> refs/heads/x` arrow target, or a plain local branch.
    Head,
    LocalBranch(String),
    RemoteBranch(String),
    Tag(String),
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full hex object id.
    pub id: String,
    /// Full hex ids; empty for a root commit, 2+ for merges.
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    pub subject: BString,
    /// Everything after the subject line, may be empty.
    pub body: BString,
    /// Decorations; `head_target` names the branch `HEAD ->` points at.
    pub refs: Vec<RefLabel>,
    pub head_target: Option<String>,
}

/// Feed raw stdout chunks of any size; complete commits come out as soon as their NUL arrives.
#[derive(Debug, Default)]
pub struct LogParser {
    /// Start of the record that is still incomplete (no NUL seen yet).
    partial: Vec<u8>,
    /// Stream offset of `partial`'s first byte, for errors.
    offset: usize,
}

impl LogParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the commits completed by this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> ParseResult<Vec<Commit>> {
        let mut commits = Vec::new();
        let mut rest = chunk;
        if !self.partial.is_empty() {
            let Some(end) = memchr::memchr(0, rest) else {
                self.partial.extend_from_slice(rest);
                return Ok(commits);
            };
            self.partial.extend_from_slice(&rest[..end]);
            let record = std::mem::take(&mut self.partial);
            self.record(&record, &mut commits)?;
            self.partial = record;
            self.partial.clear();
            rest = &rest[end + 1..];
        }
        let mut start = 0;
        for end in memchr::memchr_iter(0, rest) {
            self.record(&rest[start..end], &mut commits)?;
            start = end + 1;
        }
        self.partial.extend_from_slice(&rest[start..]);
        Ok(commits)
    }

    /// Call at EOF. A last record without its NUL (`--pretty=format:` semantics) is accepted;
    /// errors if what is left does not parse as a complete record.
    pub fn finish(mut self) -> ParseResult<Vec<Commit>> {
        let mut commits = Vec::new();
        let record = std::mem::take(&mut self.partial);
        self.record(&record, &mut commits)?;
        Ok(commits)
    }

    /// Parses one NUL-free record; a leading newline and empty records are skipped.
    fn record(&mut self, record: &[u8], commits: &mut Vec<Commit>) -> ParseResult<()> {
        let offset = self.offset;
        self.offset = offset.saturating_add(record.len()).saturating_add(1);
        let record = record.strip_prefix(b"\n").unwrap_or(record);
        if !record.is_empty() {
            commits.push(
                parse_commit(record).map_err(|reason| ParseError::new("log", offset, reason))?,
            );
        }
        Ok(())
    }
}

/// `Co-authored-by: Name <email>` trailers in a body, as (name, email), in order, without
/// repeats. Lines that are not such a trailer are skipped.
pub fn co_authors(body: &[u8]) -> Vec<(BString, BString)> {
    const KEY: &[u8] = b"co-authored-by:";
    let mut found: Vec<(BString, BString)> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.len() <= KEY.len() || !line[..KEY.len()].eq_ignore_ascii_case(KEY) {
            continue;
        }
        let value = line[KEY.len()..].trim();
        let (name, email) = match (value.find_byte(b'<'), value.rfind_byte(b'>')) {
            (Some(open), Some(close)) if open < close => {
                (value[..open].trim(), value[open + 1..close].trim())
            }
            _ => (value, &b""[..]),
        };
        if name.is_empty() && email.is_empty() {
            continue;
        }
        let entry = (BString::from(name), BString::from(email));
        if !found.iter().any(|(n, e)| {
            if e.is_empty() && entry.1.is_empty() {
                n == &entry.0
            } else {
                e.eq_ignore_ascii_case(&entry.1)
            }
        }) {
            found.push(entry);
        }
    }
    found
}

/// The first line of a commit body, as a one-line description beside the subject: trailers
/// (`Signed-off-by:`, `Co-authored-by:`, `Change-Id:` ...) are not descriptions. At most
/// `max` characters.
pub fn short_description(body: &[u8], max: usize) -> Option<String> {
    let is_trailer = |line: &[u8]| {
        line.find_byte(b':').is_some_and(|colon| {
            let key = &line[..colon];
            const KEYS: [&[u8]; 7] = [
                b"Fixes",
                b"Link",
                b"Cc",
                b"Closes",
                b"Resolves",
                b"Bug",
                b"Refs",
            ];
            !key.is_empty()
                && key.len() < 40
                && key.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-')
                && (key.contains(&b'-') || KEYS.contains(&key))
                && line.get(colon + 1) == Some(&b' ')
        })
    };
    let line = body
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !is_trailer(l))?;
    let text = line.to_str_lossy();
    Some(match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\u{2026}", &text[..cut]),
        None => text.into_owned(),
    })
}

/// Convenience for whole output.
pub fn parse(output: &[u8]) -> ParseResult<Vec<Commit>> {
    let mut parser = LogParser::new();
    let mut commits = parser.push(output)?;
    commits.extend(parser.finish()?);
    Ok(commits)
}

fn parse_commit(record: &[u8]) -> Result<Commit, &'static str> {
    let mut fields = record.splitn(SEPARATORS + 1, |&b| b == 0x1f);
    let mut field = || fields.next().ok_or("too few fields");
    let id = parse_oid(field()?).ok_or("bad commit id")?;
    let parents = field()?
        .split_str(" ")
        .filter(|p| !p.is_empty())
        .map(|p| parse_oid(p).ok_or("bad parent id"))
        .collect::<Result<_, _>>()?;
    let author = signature(field()?, field()?, field()?).ok_or("bad author")?;
    let committer = signature(field()?, field()?, field()?).ok_or("bad committer")?;
    let (refs, head_target) = decorations(field()?);
    let subject = BString::from(field()?);
    let body = fields.next().map(BString::from).unwrap_or_default();
    Ok(Commit {
        id,
        parents,
        author,
        committer,
        subject,
        body,
        refs,
        head_target,
    })
}

/// `name`, `email` and a `--date=raw` date: `1696766400 +0200`.
fn signature(name: &[u8], email: &[u8], date: &[u8]) -> Option<Signature> {
    let (time, tz) = date.split_once_str(" ")?;
    let (sign, hhmm) = match tz {
        [b'+', hhmm @ ..] => (1, hhmm),
        [b'-', hhmm @ ..] => (-1, hhmm),
        _ => return None,
    };
    let [h1, h2, m1, m2] = hhmm else { return None };
    let hours: i32 = parse_uint(&[*h1, *h2])?;
    let minutes: i32 = parse_uint(&[*m1, *m2])?;
    Some(Signature {
        name: BString::from(name),
        email: BString::from(email),
        time: parse_i64(time)?,
        tz_offset_minutes: sign * (hours * 60 + minutes),
    })
}

/// `%D` with `--decorate=full`: `HEAD -> refs/heads/main, tag: refs/tags/v1, refs/stash`.
fn decorations(text: &[u8]) -> (Vec<RefLabel>, Option<String>) {
    let mut refs = Vec::new();
    let mut head_target = None;
    for item in text.split_str(", ").filter(|item| !item.is_empty()) {
        if item == b"HEAD" {
            refs.push(RefLabel::Head);
        } else if let Some(target) = item.strip_prefix(b"HEAD -> ") {
            let label = ref_label(target);
            head_target = label_name(&label).map(str::to_owned);
            refs.extend([RefLabel::Head, label]);
        } else if let Some(tag) = item.strip_prefix(b"tag: ") {
            refs.push(RefLabel::Tag(lossy(
                tag.strip_prefix(b"refs/tags/").unwrap_or(tag),
            )));
        } else {
            let name = item.split_str(" -> ").next().unwrap_or(item);
            refs.push(ref_label(name));
        }
    }
    (refs, head_target)
}

fn ref_label(full: &[u8]) -> RefLabel {
    if let Some(name) = full.strip_prefix(b"refs/heads/") {
        RefLabel::LocalBranch(lossy(name))
    } else if let Some(name) = full.strip_prefix(b"refs/remotes/") {
        RefLabel::RemoteBranch(lossy(name))
    } else if let Some(name) = full.strip_prefix(b"refs/tags/") {
        RefLabel::Tag(lossy(name))
    } else {
        RefLabel::Other(lossy(full))
    }
}

fn label_name(label: &RefLabel) -> Option<&str> {
    match label {
        RefLabel::Head => None,
        RefLabel::LocalBranch(name)
        | RefLabel::RemoteBranch(name)
        | RefLabel::Tag(name)
        | RefLabel::Other(name) => Some(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_descriptions() {
        let body = b"\nThe lane layout now reuses freed columns.\nMore detail here.\n\nSigned-off-by: A <a@b>\n";
        assert_eq!(
            short_description(body, 200).as_deref(),
            Some("The lane layout now reuses freed columns.")
        );
        // Trailers only: no description.
        assert_eq!(
            short_description(b"Signed-off-by: A <a@b>\nChange-Id: I123\n", 200),
            None
        );
        assert_eq!(short_description(b"", 200), None);
        // A colon inside prose is not a trailer.
        assert_eq!(
            short_description(b"Note: see the docs", 200).as_deref(),
            Some("Note: see the docs")
        );
        assert_eq!(
            short_description("äöüäöü".as_bytes(), 3).as_deref(),
            Some("äöü…")
        );
    }
}
