//! Background repository upkeep the app does on open. Today: make sure the commit-graph file
//! exists and covers new packs, so `git log --topo-order` streams its first commits at once
//! instead of walking the whole history first (design "Performance & app shell").

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cli::CliRepo;
use crate::error::Result;
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// Above this much pack data, the graph is written without changed-path Bloom filters: on the
/// Linux kernel (~6 GB) they turn a ~2 minute background write into far longer, and they only
/// speed up per-file history.
const BLOOM_LIMIT_BYTES: u64 = 1 << 30;

/// What [`CliRepo::ensure_commit_graph`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitGraph {
    /// `core.commitGraph=false`: the user opted out; nothing written.
    Disabled,
    /// A graph exists and no pack is newer than it.
    UpToDate,
    /// Wrote a new graph (none existed).
    Written,
    /// Added a layer for packs newer than the graph.
    Updated,
}

impl CliRepo {
    /// Writes `objects/info/commit-graph` (split layers, with changed-path Bloom filters unless
    /// the repo's packs exceed [`BLOOM_LIMIT_BYTES`]) when it is missing or older than the
    /// newest pack. Long on big repos: run it in the background;
    /// dropping the future kills git.
    pub async fn ensure_commit_graph(&self) -> Result<CommitGraph> {
        if !self.commit_graph_enabled().await? {
            return Ok(CommitGraph::Disabled);
        }
        let info = self.git_path("objects/info").await?;
        let objects = info.parent().map(Path::to_path_buf).unwrap_or_default();
        let graph_files = [
            info.join("commit-graph"),
            info.join("commit-graphs").join("commit-graph-chain"),
        ];
        let graph = newest_mtime(graph_files.clone());
        let packs: Vec<PathBuf> = std::fs::read_dir(objects.join("pack"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "pack"))
            .collect();
        let outcome = match graph {
            None => CommitGraph::Written,
            Some(graph) => match newest_mtime(packs.iter().cloned()) {
                Some(pack) if pack > graph => CommitGraph::Updated,
                _ => return Ok(CommitGraph::UpToDate),
            },
        };
        let pack_bytes: u64 = packs
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let mut cmd = GitCommand::write(self.root()).args([
            "commit-graph",
            "write",
            "--reachable",
            "--split",
            "--no-progress",
        ]);
        if pack_bytes <= BLOOM_LIMIT_BYTES {
            cmd = cmd.arg("--changed-paths");
        }
        self.runner().run(cmd).await?;
        // A pack can be newer without new commits (repack, gc): then git writes nothing.
        if outcome == CommitGraph::Updated && newest_mtime(graph_files) == graph {
            return Ok(CommitGraph::UpToDate);
        }
        Ok(outcome)
    }

    /// `core.commitGraph` (default true).
    async fn commit_graph_enabled(&self) -> Result<bool> {
        let cmd = GitCommand::read(self.root()).args([
            "config",
            "--type=bool",
            "--default=true",
            "--get",
            "core.commitGraph",
        ]);
        let out = self.runner().run(cmd).await?;
        Ok(String::from_utf8_lossy(&out).trim() != "false")
    }

    /// `git rev-parse --git-path`, made absolute (it may answer relative to the worktree).
    async fn git_path(&self, path: &str) -> Result<PathBuf> {
        let cmd = GitCommand::read(self.root()).args(["rev-parse", "--git-path", path]);
        let out = self.runner().run(cmd).await?;
        let text = String::from_utf8_lossy(&out);
        let found = PathBuf::from(text.trim_end_matches(['\r', '\n']));
        Ok(if found.is_absolute() {
            found
        } else {
            self.root().join(found)
        })
    }
}

fn newest_mtime(paths: impl IntoIterator<Item = PathBuf>) -> Option<SystemTime> {
    paths
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}
