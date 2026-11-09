//! The commit graph's data: commits from a streaming `git log` (see [`crate::LogQuery::graph`])
//! plus their [`GraphLayout`], kept compact enough for 1M commits.
//!
//! Ids and subjects live in two shared buffers and authors are interned, so a commit costs
//! about its id + subject + 40 bytes instead of a [`Commit`] (several heap strings each).
//! Bodies are not kept: the details panel reads the full message on demand.

use std::collections::{HashMap, HashSet};

use bstr::{BString, ByteSlice};

use super::layout::{Edge, GraphLayout, Node};
use crate::parse::log::{Commit, RefLabel, Signature};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// One row as the graph view draws it.
#[derive(Debug, Clone, Copy)]
pub struct CommitRow<'a> {
    pub id: &'a str,
    pub subject: &'a str,
    pub author: &'a Author,
    /// Author date, unix seconds.
    pub time: i64,
    pub tz_offset_minutes: i32,
    /// Decorations on this commit (HEAD, branches, tags), in git's order.
    pub refs: &'a [RefLabel],
    /// `Co-authored-by` trailers, as indexes for [`History::author`].
    pub co_authors: &'a [u32],
    pub node: Node,
    pub edges: &'a [Edge],
}

/// What a row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Commit,
    /// Uncommitted changes: the first row, a child of `HEAD`.
    Wip,
    /// `stash@{n}`, drawn on its own lane off the commit it was made on.
    Stash(u32),
}

/// Rows that are not plain commits, set before the log streams in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pseudo {
    /// `HEAD`'s commit when the worktree has changes: adds the WIP row on top.
    pub wip_parent: Option<String>,
    /// Stash commit ids with their `n` in `stash@{n}`; pass the ids to the log
    /// ([`crate::LogQuery::stashes`]) too.
    pub stashes: Vec<(String, u32)>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// End of the subject in `History::subjects`; it starts at the previous entry's end.
    subject_end: usize,
    time: i64,
    author: u32,
    tz_offset_minutes: i32,
}

#[derive(Debug, Default)]
pub struct History {
    layout: GraphLayout,
    entries: Vec<Entry>,
    /// Hex ids, `id_len` bytes each (40 for SHA-1, 64 for SHA-256; one repo never mixes).
    ids: String,
    id_len: usize,
    subjects: String,
    authors: Vec<Author>,
    author_index: HashMap<Author, u32>,
    /// Rows with decorations, in row order.
    refs: Vec<(u32, Box<[RefLabel]>)>,
    /// Rows with co-authors, in row order.
    co_authors: Vec<(u32, Box<[u32]>)>,
    /// Id prefix (first 16 hex digits) -> row. The rare second commit with the same prefix goes
    /// to `colliding` instead.
    index: HashMap<u64, u32>,
    colliding: Vec<u32>,
    head: Option<u32>,
    head_target: Option<String>,
    /// Row 0 is the WIP row.
    wip: bool,
    /// Stash commit id -> `n` in `stash@{n}`.
    stash_ids: HashMap<String, u32>,
    /// (row, n) of stash rows, in row order.
    stash_rows: Vec<(u32, u32)>,
    /// A stash's index / untracked helper commits: never shown.
    hidden: HashSet<String>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty history that will show `pseudo`'s WIP row (pushed now) and stash rows (as their
    /// commits stream in).
    pub fn with_pseudo(pseudo: Pseudo) -> Self {
        let mut history = Self {
            stash_ids: pseudo.stashes.into_iter().collect(),
            ..Self::default()
        };
        if let Some(head) = pseudo.wip_parent {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let nobody = Signature {
                name: BString::default(),
                email: BString::default(),
                time,
                tz_offset_minutes: 0,
            };
            history.push(&Commit {
                // The null id: never a real commit, and as long as HEAD's.
                id: "0".repeat(head.len()),
                parents: vec![head],
                author: nobody.clone(),
                committer: nobody,
                subject: BString::from("WIP"),
                body: BString::default(),
                refs: Vec::new(),
                head_target: None,
            });
            history.wip = true;
        }
        history
    }

    pub fn kind(&self, row: usize) -> RowKind {
        if self.wip && row == 0 {
            return RowKind::Wip;
        }
        match self
            .stash_rows
            .binary_search_by_key(&(row as u32), |(r, _)| *r)
        {
            Ok(i) => RowKind::Stash(self.stash_rows[i].1),
            Err(_) => RowKind::Commit,
        }
    }

    /// The WIP row's id (all zeros), when there is one.
    pub fn wip_id(&self) -> Option<&str> {
        self.wip.then(|| self.id(0)).flatten()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn layout(&self) -> &GraphLayout {
        &self.layout
    }

    /// Appends commits in log order (children before parents).
    pub fn extend(&mut self, commits: impl IntoIterator<Item = Commit>) {
        for commit in commits {
            self.push(&commit);
        }
    }

    /// Adds a commit; `None` for a stash's helper commits, which are skipped.
    pub fn push(&mut self, commit: &Commit) -> Option<usize> {
        if self.hidden.remove(&commit.id) {
            return None;
        }
        let stash = self.stash_ids.get(&commit.id).copied();
        let parents = match stash {
            // Only the base: the index and untracked commits are stash internals.
            Some(_) => {
                self.hidden.extend(commit.parents.iter().skip(1).cloned());
                &commit.parents[..commit.parents.len().min(1)]
            }
            None => &commit.parents[..],
        };
        let row = self.layout.push(&commit.id, parents);
        let r = row as u32;
        if let Some(n) = stash {
            self.stash_rows.push((r, n));
        }

        if self.id_len == 0 {
            self.id_len = commit.id.len();
        }
        // Ids come from `parse_oid`: 40 or 64 hex digits. Pad or cut a stray one so rows
        // stay aligned.
        let id = commit.id.get(..self.id_len).unwrap_or(&commit.id);
        self.ids.push_str(id);
        for _ in id.len()..self.id_len {
            self.ids.push('0');
        }

        self.subjects.push_str(&commit.subject.to_str_lossy());
        let author = self.intern(Author {
            name: commit.author.name.to_str_lossy().into_owned(),
            email: commit.author.email.to_str_lossy().into_owned(),
        });
        self.entries.push(Entry {
            subject_end: self.subjects.len(),
            time: commit.author.time,
            author,
            tz_offset_minutes: commit.author.tz_offset_minutes,
        });

        let co: Vec<u32> = crate::parse::log::co_authors(&commit.body)
            .into_iter()
            .map(|(name, email)| {
                self.intern(Author {
                    name: name.to_str_lossy().into_owned(),
                    email: email.to_str_lossy().into_owned(),
                })
            })
            .filter(|&i| i != author)
            .collect();
        if !co.is_empty() {
            self.co_authors.push((r, co.into_boxed_slice()));
        }

        if !commit.refs.is_empty() {
            if commit.refs.contains(&RefLabel::Head) && self.head.is_none() {
                self.head = Some(r);
                self.head_target.clone_from(&commit.head_target);
            }
            self.refs.push((r, commit.refs.clone().into_boxed_slice()));
        }

        match prefix_key(id) {
            Some(key) if !self.index.contains_key(&key) => {
                self.index.insert(key, r);
            }
            _ => self.colliding.push(r),
        }
        Some(row)
    }

    pub fn row(&self, row: usize) -> Option<CommitRow<'_>> {
        let entry = self.entries.get(row)?;
        let subject_start = row
            .checked_sub(1)
            .map_or(0, |prev| self.entries[prev].subject_end);
        let refs = match self.refs.binary_search_by_key(&(row as u32), |(r, _)| *r) {
            Ok(i) => &self.refs[i].1[..],
            Err(_) => &[],
        };
        let co_authors = match self
            .co_authors
            .binary_search_by_key(&(row as u32), |(r, _)| *r)
        {
            Ok(i) => &self.co_authors[i].1[..],
            Err(_) => &[],
        };
        Some(CommitRow {
            id: self.id(row)?,
            co_authors,
            subject: &self.subjects[subject_start..entry.subject_end],
            author: &self.authors[entry.author as usize],
            time: entry.time,
            tz_offset_minutes: entry.tz_offset_minutes,
            refs,
            node: self.layout.node(row)?,
            edges: self.layout.edges(row),
        })
    }

    pub fn id(&self, row: usize) -> Option<&str> {
        let start = row.checked_mul(self.id_len)?;
        self.ids.get(start..start + self.id_len)
    }

    /// Row of the commit with this full id.
    pub fn find(&self, id: &str) -> Option<usize> {
        let hit = prefix_key(id)
            .and_then(|key| self.index.get(&key))
            .map(|&r| r as usize)
            .filter(|&r| self.id(r) == Some(id));
        hit.or_else(|| {
            self.colliding
                .iter()
                .map(|&r| r as usize)
                .find(|&r| self.id(r) == Some(id))
        })
    }

    /// Row of the commit `HEAD` points at, once it has streamed in.
    pub fn head(&self) -> Option<usize> {
        self.head.map(|r| r as usize)
    }

    /// Branch `HEAD` is on; `None` when detached (or not seen yet).
    pub fn head_branch(&self) -> Option<&str> {
        self.head_target.as_deref()
    }

    pub fn authors(&self) -> &[Author] {
        &self.authors
    }

    /// An author by index (see [`CommitRow::co_authors`]).
    pub fn author(&self, index: u32) -> Option<&Author> {
        self.authors.get(index as usize)
    }

    fn intern(&mut self, author: Author) -> u32 {
        if let Some(&i) = self.author_index.get(&author) {
            return i;
        }
        let i = self.authors.len() as u32;
        self.authors.push(author.clone());
        self.author_index.insert(author, i);
        i
    }
}

/// First 16 hex digits as a number; `None` for anything shorter or not hex.
fn prefix_key(id: &str) -> Option<u64> {
    u64::from_str_radix(id.get(..16)?, 16).ok()
}

#[cfg(test)]
mod tests {
    use bstr::BString;

    use super::*;
    use crate::parse::log::Signature;

    fn sig(name: &str, time: i64) -> Signature {
        Signature {
            name: name.into(),
            email: format!("{name}@example.com").into(),
            time,
            tz_offset_minutes: 60,
        }
    }

    fn oid(n: u64) -> String {
        format!("{n:040x}")
    }

    fn commit(n: u64, parents: &[u64], subject: &str, refs: Vec<RefLabel>) -> Commit {
        Commit {
            id: oid(n),
            parents: parents.iter().map(|&p| oid(p)).collect(),
            author: sig(if n.is_multiple_of(2) { "ann" } else { "bob" }, n as i64),
            committer: sig("ann", n as i64),
            subject: BString::from(subject),
            body: BString::default(),
            head_target: refs.contains(&RefLabel::Head).then(|| "main".to_string()),
            refs,
        }
    }

    fn sample() -> History {
        let mut h = History::new();
        h.extend([
            commit(
                4,
                &[3, 2],
                "Merge feature",
                vec![RefLabel::Head, RefLabel::LocalBranch("main".into())],
            ),
            commit(2, &[1], "feature work", vec![RefLabel::Tag("v1".into())]),
            commit(3, &[1], "main work", vec![]),
            commit(1, &[], "root ✨", vec![]),
        ]);
        h
    }

    #[test]
    fn rows_read_back() {
        let h = sample();
        assert_eq!(h.len(), 4);
        let top = h.row(0).unwrap();
        assert_eq!(top.id, oid(4));
        assert_eq!(top.subject, "Merge feature");
        assert_eq!(top.author.name, "ann");
        assert_eq!(top.refs.len(), 2);
        assert_eq!(top.edges.len(), 2);
        let feature = h.row(1).unwrap();
        assert_eq!(feature.subject, "feature work");
        assert_eq!(feature.refs, &[RefLabel::Tag("v1".into())]);
        assert_eq!(feature.node.column, 1);
        let root = h.row(3).unwrap();
        assert_eq!(root.subject, "root ✨");
        assert_eq!(root.author.email, "bob@example.com");
        assert_eq!(root.time, 1);
        assert_eq!(root.tz_offset_minutes, 60);
        assert!(root.refs.is_empty());
        assert!(h.row(4).is_none());
    }

    #[test]
    fn co_authors_from_trailers() {
        let mut h = sample();
        h.push(&Commit {
            body: BString::from(
                "Co-authored-by: Cy <cy@example.com>\nco-authored-by: ann <ann@example.com>\n\
                 Co-authored-by: Cy <CY@example.com>\nSigned-off-by: Dee <dee@example.com>\n",
            ),
            ..commit(5, &[4], "pair work", vec![])
        });
        let row = h.row(4).unwrap();
        // ann is the author (5 is odd: bob) -> kept; the repeated Cy is dropped.
        let names: Vec<&str> = row
            .co_authors
            .iter()
            .map(|&i| h.author(i).unwrap().name.as_str())
            .collect();
        assert_eq!(names, ["Cy", "ann"]);
        assert!(h.row(0).unwrap().co_authors.is_empty());
    }

    #[test]
    fn wip_and_stash_rows() {
        // Stash 9 on commit 3 with index commit 8; 4 is HEAD.
        let mut h = History::with_pseudo(Pseudo {
            wip_parent: Some(oid(4)),
            stashes: vec![(oid(9), 0)],
        });
        assert_eq!(h.kind(0), RowKind::Wip);
        assert_eq!(h.wip_id(), Some("0".repeat(40).as_str()));
        h.extend([
            commit(
                4,
                &[3, 2],
                "Merge feature",
                vec![RefLabel::Head, RefLabel::LocalBranch("main".into())],
            ),
            commit(9, &[3, 8], "WIP on main: 3 main work", vec![]),
            commit(8, &[3], "index on main: 3 main work", vec![]),
            commit(2, &[1], "feature work", vec![]),
            commit(3, &[1], "main work", vec![]),
            commit(1, &[], "root", vec![]),
        ]);
        // WIP, 4, stash 9, 2, 3, 1: the index commit is gone.
        assert_eq!(h.len(), 6);
        assert_eq!(h.find(&oid(8)), None);
        assert_eq!(h.head(), Some(1));
        assert_eq!(h.kind(1), RowKind::Commit);
        assert_eq!(h.kind(2), RowKind::Stash(0));
        // WIP sits on HEAD's lane; the stash hangs off its base only.
        let wip = h.row(0).unwrap();
        assert_eq!(wip.node.column, h.row(1).unwrap().node.column);
        assert_eq!(h.row(2).unwrap().edges.len(), 1);
    }

    #[test]
    fn authors_are_interned() {
        let h = sample();
        assert_eq!(h.authors().len(), 2);
    }

    #[test]
    fn head_and_find() {
        let h = sample();
        assert_eq!(h.head(), Some(0));
        assert_eq!(h.head_branch(), Some("main"));
        assert_eq!(h.find(&oid(1)), Some(3));
        assert_eq!(h.find(&oid(9)), None);
        assert_eq!(h.find("nope"), None);
    }

    #[test]
    fn find_survives_prefix_collisions() {
        let mut h = History::new();
        let a = format!("{}{}", "ab".repeat(8), "0".repeat(24));
        let b = format!("{}{}", "ab".repeat(8), "1".repeat(24));
        for id in [&a, &b] {
            h.push(&Commit {
                id: id.clone(),
                ..commit(0, &[], "x", vec![])
            });
        }
        assert_eq!(h.find(&a), Some(0));
        assert_eq!(h.find(&b), Some(1));
    }

    #[test]
    fn sha256_ids() {
        let mut h = History::new();
        let id = "f".repeat(64);
        h.push(&Commit {
            id: id.clone(),
            ..commit(0, &[], "x", vec![])
        });
        assert_eq!(h.row(0).unwrap().id, id);
        assert_eq!(h.find(&id), Some(0));
    }
}
