//! `GitRepo`: what the app asks of a repository. [`crate::CliRepo`] answers with the git CLI;
//! a read-only `gix` implementation may answer some of it later (cargo feature `gix`).

use std::future::Future;
use std::path::Path;

use bstr::BString;

use crate::error::Result;
use crate::parse::diff::FileDiff;
use crate::parse::log::Commit;
use crate::parse::refs::{RefEntry, Remote, Stash};
use crate::parse::status::Status;

/// Which commits `log` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// Start points (`main`, `origin/x`, `a..b`). Empty means `HEAD`.
    pub revs: Vec<String>,
    /// All refs (`--all`): the graph view.
    pub all: bool,
    /// `--topo-order` (graph) instead of date order.
    pub topo_order: bool,
    pub skip: usize,
    pub limit: Option<usize>,
    /// Only commits touching these paths.
    pub paths: Vec<BString>,
    /// Read commit bodies. The graph leaves them out (`parse::log::GRAPH_FORMAT`).
    pub bodies: bool,
    /// Stash commits to walk as extra tips. With [`Self::all`], `refs/stash` itself is left out
    /// so only these appear (see `graph::History::with_pseudo`).
    pub stashes: Vec<String>,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            revs: Vec::new(),
            all: false,
            topo_order: true,
            skip: 0,
            limit: None,
            paths: Vec::new(),
            bodies: true,
            stashes: Vec::new(),
        }
    }
}

impl LogQuery {
    /// Every ref, topological order, subjects only: what the commit graph shows.
    pub fn graph() -> Self {
        Self {
            all: true,
            bodies: false,
            ..Self::default()
        }
    }
}

/// What to diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// Worktree vs index (unstaged changes).
    Unstaged { paths: Vec<BString> },
    /// Index vs HEAD (staged changes).
    Staged { paths: Vec<BString> },
    /// A commit against its first parent (root commits against the empty tree).
    Commit { id: String },
    /// `from` to `to` (two-dot).
    Range { from: String, to: String },
}

pub trait GitRepo: Send + Sync {
    /// Worktree root.
    fn root(&self) -> &Path;
    fn status(&self) -> impl Future<Output = Result<Status>> + Send;
    fn log(&self, query: LogQuery) -> impl Future<Output = Result<Vec<Commit>>> + Send;
    fn diff(&self, target: DiffTarget) -> impl Future<Output = Result<Vec<FileDiff>>> + Send;
    fn refs(&self) -> impl Future<Output = Result<Vec<RefEntry>>> + Send;
    fn stashes(&self) -> impl Future<Output = Result<Vec<Stash>>> + Send;
    fn remotes(&self) -> impl Future<Output = Result<Vec<Remote>>> + Send;
    /// `(ahead, behind)` of `local` relative to `upstream`.
    fn ahead_behind(
        &self,
        local: &str,
        upstream: &str,
    ) -> impl Future<Output = Result<(u32, u32)>> + Send;
}
