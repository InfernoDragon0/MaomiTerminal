//! Merge, rebase, cherry-pick and revert: starting them, telling which one is in progress
//! (from the files git keeps in the git dir), conflicted files and their stages, resolving,
//! and continue / skip / abort. Git never opens an editor here: `GIT_EDITOR=:` keeps the
//! prepared messages.

pub mod markers;
pub mod rebase;

use std::path::{Path, PathBuf};

use bstr::{BString, ByteSlice as _};

use crate::cli::CliRepo;
use crate::error::{GitError, Result};
use crate::parse::status::{EntryKind, FileState, Status};
use crate::repo::GitRepo as _;
use crate::runner::{GitCommand, failed};

pub use markers::{Block, Choice, ConflictText, Segment, has_markers};
pub use rebase::{RebaseCommit, RebasePlan, TodoAction, TodoItem};

/// `GIT_EDITOR` value git treats as "do not edit" (it never spawns it).
const NO_EDITOR: &str = ":";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    /// `git am` (also the old apply-based rebase backend is reported as [`Self::Rebase`]).
    Am,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Operation::Merge => "Merge",
            Operation::Rebase => "Rebase",
            Operation::CherryPick => "Cherry-pick",
            Operation::Revert => "Revert",
            Operation::Am => "Apply patches",
        }
    }

    /// Has a `--skip` (drop the current commit and go on).
    pub fn can_skip(self) -> bool {
        !matches!(self, Operation::Merge)
    }

    fn subcommand(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
            Operation::Am => "am",
        }
    }
}

/// An operation git has stopped in the middle of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgress {
    pub op: Operation,
    /// The branch being rebased (`refs/heads/` stripped).
    pub branch: Option<String>,
    /// Commit the rebase replays onto.
    pub onto: Option<String>,
    /// `(current, total)` step of a rebase.
    pub step: Option<(u32, u32)>,
    /// The commit coming in: `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `REBASE_HEAD`.
    pub incoming: Option<String>,
    /// First line of the prepared message (`MERGE_MSG`).
    pub message: Option<String>,
}

/// How a conflicted path differs, from the unmerged status letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    BothModified,
    BothAdded,
    DeletedByUs,
    DeletedByThem,
    AddedByUs,
    AddedByThem,
    BothDeleted,
}

impl ConflictKind {
    pub fn label(self) -> &'static str {
        match self {
            ConflictKind::BothModified => "both modified",
            ConflictKind::BothAdded => "both added",
            ConflictKind::DeletedByUs => "deleted by us",
            ConflictKind::DeletedByThem => "deleted by them",
            ConflictKind::AddedByUs => "added by us",
            ConflictKind::AddedByThem => "added by them",
            ConflictKind::BothDeleted => "both deleted",
        }
    }

    /// The worktree file holds conflict markers to resolve block by block.
    pub fn has_markers(self) -> bool {
        matches!(self, ConflictKind::BothModified | ConflictKind::BothAdded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: BString,
    pub kind: ConflictKind,
}

/// The unmerged entries of `status`.
pub fn conflict_files(status: &Status) -> Vec<ConflictFile> {
    use FileState::*;
    status
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Unmerged)
        .map(|e| ConflictFile {
            path: e.path.clone(),
            kind: match (e.index, e.worktree) {
                (Deleted, Deleted) => ConflictKind::BothDeleted,
                (Added, Unmerged) => ConflictKind::AddedByUs,
                (Unmerged, Deleted) => ConflictKind::DeletedByThem,
                (Unmerged, Added) => ConflictKind::AddedByThem,
                (Deleted, Unmerged) => ConflictKind::DeletedByUs,
                (Added, Added) => ConflictKind::BothAdded,
                _ => ConflictKind::BothModified,
            },
        })
        .collect()
}

/// A conflicted file's index stages (`None` where that side has no file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stages {
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeMode {
    /// Fast-forward when possible (git's default / `merge.ff`).
    #[default]
    Default,
    /// Always a merge commit.
    NoFastForward,
    FastForwardOnly,
    /// Stage the result without committing.
    Squash,
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn read_number(path: &Path) -> Option<u32> {
    read_trimmed(path)?.parse().ok()
}

/// What the git dir says is in progress.
pub fn detect(git_dir: &Path) -> Option<InProgress> {
    let head = |name: &str| read_trimmed(&git_dir.join(name));
    let merge_message = || head("MERGE_MSG").and_then(|m| m.lines().next().map(str::to_owned));
    let rebase = [
        ("rebase-merge", "msgnum", "end"),
        ("rebase-apply", "next", "last"),
    ]
    .into_iter()
    .find(|(dir, ..)| git_dir.join(dir).is_dir());
    if let Some((dir, current, total)) = rebase {
        let dir = git_dir.join(dir);
        if dir.join("applying").exists() {
            return Some(InProgress {
                op: Operation::Am,
                branch: None,
                onto: None,
                step: read_number(&dir.join(current)).zip(read_number(&dir.join(total))),
                incoming: None,
                message: None,
            });
        }
        let branch = read_trimmed(&dir.join("head-name"))
            .filter(|b| b != "detached HEAD")
            .map(|b| {
                b.strip_prefix("refs/heads/")
                    .map(str::to_owned)
                    .unwrap_or(b)
            });
        return Some(InProgress {
            op: Operation::Rebase,
            branch,
            onto: read_trimmed(&dir.join("onto")),
            step: read_number(&dir.join(current)).zip(read_number(&dir.join(total))),
            incoming: head("REBASE_HEAD"),
            message: None,
        });
    }
    let simple = |op, incoming: Option<String>| InProgress {
        op,
        branch: None,
        onto: None,
        step: None,
        incoming,
        message: merge_message(),
    };
    if let Some(merge) = head("MERGE_HEAD") {
        let first = merge.lines().next().map(str::to_owned);
        return Some(simple(Operation::Merge, first));
    }
    if let Some(pick) = head("CHERRY_PICK_HEAD") {
        return Some(simple(Operation::CherryPick, Some(pick)));
    }
    if let Some(revert) = head("REVERT_HEAD") {
        return Some(simple(Operation::Revert, Some(revert)));
    }
    None
}

impl CliRepo {
    fn op_cmd(&self) -> GitCommand {
        GitCommand::write(self.root()).env("GIT_EDITOR", NO_EDITOR)
    }

    /// Runs a command that may stop for conflicts: stopping (git left an operation in
    /// progress) is not an error, the caller shows the conflicts instead.
    pub(crate) async fn run_may_stop(&self, cmd: GitCommand) -> Result<()> {
        let display = cmd.display();
        let out = self.runner().output(cmd).await?;
        if out.success() || self.in_progress().await?.is_some() {
            return Ok(());
        }
        Err(failed(display, out.code, &out.stderr))
    }

    /// The (per-worktree) git dir.
    pub async fn git_dir(&self) -> Result<PathBuf> {
        crate::watch::git_dir(self.runner(), self.root()).await
    }

    /// The operation git is in the middle of, if any.
    pub async fn in_progress(&self) -> Result<Option<InProgress>> {
        Ok(detect(&self.git_dir().await?))
    }

    /// `git merge <rev>` into the current branch.
    pub async fn merge(&self, rev: &str, mode: MergeMode) -> Result<()> {
        let mut cmd = self.op_cmd().args(["merge", "--no-edit"]);
        cmd = match mode {
            MergeMode::Default => cmd,
            MergeMode::NoFastForward => cmd.arg("--no-ff"),
            MergeMode::FastForwardOnly => cmd.arg("--ff-only"),
            MergeMode::Squash => cmd.arg("--squash"),
        };
        self.run_may_stop(cmd.arg("--end-of-options").rev(rev)?)
            .await
    }

    /// Rebases the current branch onto `onto`.
    pub async fn rebase(&self, onto: &str) -> Result<()> {
        let cmd = self
            .op_cmd()
            .args(["rebase", "--end-of-options"])
            .rev(onto)?;
        self.run_may_stop(cmd).await
    }

    /// Commits an interactive rebase from `base` (exclusive; `None` = from the root) would
    /// replay, oldest first.
    pub async fn rebase_plan(&self, base: Option<&str>) -> Result<RebasePlan> {
        let range = match base {
            Some(base) => format!("{base}..HEAD"),
            None => "HEAD".to_string(),
        };
        let read = || GitCommand::read(self.root());
        let log = read()
            .args([
                "log",
                "--reverse",
                "--no-merges",
                "--format=%H%x1f%s%x1f%B%x1e",
            ])
            .arg("--end-of-options")
            .rev(&range)?
            .paths::<_, &[u8]>([]);
        let commits = rebase::parse_plan(&self.runner().run(log).await?);
        let count = read()
            .args(["rev-list", "--count", "--merges", "--end-of-options"])
            .rev(&range)?
            .paths::<_, &[u8]>([]);
        let merges = String::from_utf8_lossy(&self.runner().run(count).await?)
            .trim()
            .parse()
            .unwrap_or(0);
        Ok(RebasePlan { commits, merges })
    }

    /// `git rebase -i` with `items` as the todo list (oldest first) instead of an editor.
    pub async fn interactive_rebase(&self, base: Option<&str>, items: &[TodoItem]) -> Result<()> {
        let dir = self.git_dir().await?.join("maomi-rebase");
        // Leftovers from an earlier run are ours to replace.
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let (todo, messages) = rebase::build_todo(items, &dir)?;
        let todo_file = dir.join("todo");
        std::fs::write(&todo_file, todo)?;
        for (file, message) in &messages {
            std::fs::write(file, message)?;
        }
        let mut cmd = self
            .op_cmd()
            .env(
                "GIT_SEQUENCE_EDITOR",
                format!("cp {}", rebase::sh_path(&todo_file)),
            )
            .args(["rebase", "--interactive"]);
        cmd = match base {
            Some(base) => cmd.arg("--end-of-options").rev(base)?,
            None => cmd.arg("--root"),
        };
        self.run_may_stop(cmd).await
    }

    /// `--continue` the operation in progress, keeping git's prepared message.
    pub async fn continue_operation(&self, op: Operation) -> Result<()> {
        let cmd = self.op_cmd().args([op.subcommand(), "--continue"]);
        self.run_may_stop(cmd).await
    }

    /// `--skip` the current commit of a rebase / cherry-pick / revert.
    pub async fn skip_operation(&self, op: Operation) -> Result<()> {
        if !op.can_skip() {
            return Err(GitError::InvalidArgument {
                what: "operation to skip",
                value: op.label().into(),
            });
        }
        let cmd = self.op_cmd().args([op.subcommand(), "--skip"]);
        self.run_may_stop(cmd).await
    }

    /// `--abort`: back to where the operation started.
    pub async fn abort_operation(&self, op: Operation) -> Result<()> {
        let cmd = self.op_cmd().args([op.subcommand(), "--abort"]);
        self.runner().run(cmd).await.map(drop)
    }

    /// The base / ours / theirs versions of a conflicted file from the index.
    pub async fn conflict_stages(&self, path: &[u8]) -> Result<Stages> {
        let cmd = GitCommand::read(self.root())
            .args(["ls-files", "--unmerged", "-z"])
            .paths([path]);
        let out = self.runner().run(cmd).await?;
        let mut stages = Stages::default();
        // `<mode> <sha> <stage>\t<path>\0`
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let Some((meta, _)) = record.split_once_str("\t") else {
                continue;
            };
            let fields: Vec<&[u8]> = meta.split_str(" ").collect();
            let (Some(sha), Some(stage)) = (fields.get(1), fields.get(2)) else {
                continue;
            };
            let sha = String::from_utf8_lossy(sha).into_owned();
            let blob = GitCommand::read(self.root())
                .args(["cat-file", "blob"])
                .rev(&sha)?;
            let content = Some(self.runner().run(blob).await?);
            match *stage {
                b"1" => stages.base = content,
                b"2" => stages.ours = content,
                b"3" => stages.theirs = content,
                _ => {}
            }
        }
        Ok(stages)
    }

    /// The worktree file (with its markers).
    pub async fn read_worktree_file(&self, path: &[u8]) -> Result<Vec<u8>> {
        Ok(std::fs::read(crate::ops::worktree_file(
            self.root(),
            path,
        )?)?)
    }

    /// Writes the resolved content and stages it.
    pub async fn resolve_with(&self, path: &[u8], content: &[u8]) -> Result<()> {
        std::fs::write(crate::ops::worktree_file(self.root(), path)?, content)?;
        self.mark_resolved(&[BString::from(path)]).await
    }

    /// Takes one side's whole file (or its deletion) and stages it.
    pub async fn take_side(&self, path: &[u8], side: Side) -> Result<()> {
        let stages = self.conflict_stages(path).await?;
        let present = match side {
            Side::Ours => stages.ours.is_some(),
            Side::Theirs => stages.theirs.is_some(),
        };
        let write = || GitCommand::write(self.root());
        let cmd = if present {
            let flag = match side {
                Side::Ours => "--ours",
                Side::Theirs => "--theirs",
            };
            write().args(["checkout", flag]).paths([path])
        } else {
            write()
                .args(["rm", "--quiet", "--ignore-unmatch"])
                .paths([path])
        };
        self.runner().run(cmd).await?;
        if present {
            self.mark_resolved(&[BString::from(path)]).await?;
        }
        Ok(())
    }

    /// Stages paths as resolved (`git add --all`, so deletions count too).
    pub async fn mark_resolved(&self, paths: &[BString]) -> Result<()> {
        let cmd = GitCommand::write(self.root())
            .args(["add", "--all"])
            .paths(paths);
        self.runner().run(cmd).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_states() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path();
        assert_eq!(detect(git), None);

        std::fs::write(git.join("MERGE_HEAD"), "abc123\n").unwrap();
        std::fs::write(
            git.join("MERGE_MSG"),
            "Merge branch 'feature'\n\n# Conflicts:\n",
        )
        .unwrap();
        let merge = detect(git).unwrap();
        assert_eq!(merge.op, Operation::Merge);
        assert_eq!(merge.incoming.as_deref(), Some("abc123"));
        assert_eq!(merge.message.as_deref(), Some("Merge branch 'feature'"));
        std::fs::remove_file(git.join("MERGE_HEAD")).unwrap();

        std::fs::write(git.join("CHERRY_PICK_HEAD"), "def456\n").unwrap();
        assert_eq!(detect(git).unwrap().op, Operation::CherryPick);
        std::fs::remove_file(git.join("CHERRY_PICK_HEAD")).unwrap();

        let rb = git.join("rebase-merge");
        std::fs::create_dir(&rb).unwrap();
        std::fs::write(rb.join("head-name"), "refs/heads/topic\n").unwrap();
        std::fs::write(rb.join("onto"), "0123abcd\n").unwrap();
        std::fs::write(rb.join("msgnum"), "2\n").unwrap();
        std::fs::write(rb.join("end"), "5\n").unwrap();
        let rebase = detect(git).unwrap();
        assert_eq!(rebase.op, Operation::Rebase);
        assert_eq!(rebase.branch.as_deref(), Some("topic"));
        assert_eq!(rebase.step, Some((2, 5)));
        assert!(rebase.op.can_skip() && !Operation::Merge.can_skip());
    }
}
