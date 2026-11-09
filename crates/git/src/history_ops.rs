//! Write operations started from the commit graph: checkout, branch / tag here, cherry-pick,
//! revert, reset. Every user-supplied name is checked by git itself (`check-ref-format`)
//! and every revision by [`crate::runner::validate_rev`] before it reaches the command line.

use crate::cli::{Capped, CliRepo};
use crate::error::{GitError, Result};
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// `git reset` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Keep index and worktree; only move the branch.
    Soft,
    /// Reset the index, keep the worktree.
    Mixed,
    /// Reset index and worktree: uncommitted changes are lost.
    Hard,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

/// One file of a commit, from `git diff --raw --numstat`: enough for the details list without
/// building any patch (the diff is read when the file is opened).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub change: crate::parse::diff::FileChange,
    pub old_path: Option<bstr::BString>,
    pub new_path: Option<bstr::BString>,
    /// `None` for binary files.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

impl ChangedFile {
    /// The new path, or the old one for a deletion.
    pub fn path(&self) -> &[u8] {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .map_or(&[][..], |p| p.as_ref())
    }
}

/// `git diff --raw --numstat -z`: the raw records (`:old new sha sha STATUS\0path\0[path\0]`),
/// then one numstat record per file in the same order (`a\tr\tpath\0`, or `a\tr\t\0old\0new\0`
/// for renames and copies; `-` counts for binary files).
fn parse_raw_numstat(out: &[u8]) -> Vec<ChangedFile> {
    use crate::parse::diff::FileChange;
    use bstr::{BString, ByteSlice as _};
    let mut toks = out.split(|&b| b == 0).peekable();
    let mut files = Vec::new();
    while let Some(head) = toks.next_if(|t| t.trim_start().starts_with(b":")) {
        let fields: Vec<&[u8]> = head.trim_start().split_str(" ").collect();
        let (Some(status), Some(old_sha), Some(new_sha)) =
            (fields.get(4), fields.get(2), fields.get(3))
        else {
            break;
        };
        let score = status
            .get(1..)
            .and_then(|s| s.to_str().ok()?.parse().ok())
            .unwrap_or(0);
        let path = |t: Option<&[u8]>| t.map(BString::from);
        let (change, old_path, new_path) = match status.first() {
            Some(b'R') => (
                FileChange::Renamed { similarity: score },
                path(toks.next()),
                path(toks.next()),
            ),
            Some(b'C') => (
                FileChange::Copied { similarity: score },
                path(toks.next()),
                path(toks.next()),
            ),
            Some(b'A') => (FileChange::Added, None, path(toks.next())),
            Some(b'D') => (FileChange::Deleted, path(toks.next()), None),
            _ => {
                let p = path(toks.next());
                let change = if old_sha == new_sha {
                    FileChange::ModeChanged
                } else {
                    FileChange::Modified
                };
                (change, p.clone(), p)
            }
        };
        files.push(ChangedFile {
            change,
            old_path,
            new_path,
            added: None,
            removed: None,
        });
    }
    for file in &mut files {
        let Some(record) = toks.next() else { break };
        let mut parts = record.splitn(3, |&b| b == b'\t');
        let count = |p: Option<&[u8]>| p.and_then(|p| p.to_str().ok()?.parse().ok());
        file.added = count(parts.next());
        file.removed = count(parts.next());
        // Renames and copies: the two paths follow as their own records.
        if parts.next().is_some_and(<[u8]>::is_empty) {
            toks.next();
            toks.next();
        }
    }
    files
}

impl CliRepo {
    /// `id`'s first parent, or the empty tree for a root commit.
    async fn first_parent_or_empty(&self, id: &str) -> Result<String> {
        let out = self
            .runner()
            .run(
                GitCommand::read(self.root())
                    .args(["rev-list", "--parents", "-n", "1", "--end-of-options"])
                    .rev(id)?,
            )
            .await?;
        let text = String::from_utf8_lossy(&out);
        Ok(text
            .split_whitespace()
            .nth(1)
            .unwrap_or(crate::cli::EMPTY_TREE)
            .to_string())
    }

    /// The files `id` changes against its first parent, without patches. `counts` adds line
    /// counts (`--numstat`), which reads every blob: ~10x slower on big commits, so the list
    /// is shown first and the counts follow.
    pub async fn commit_files(&self, id: &str, counts: bool) -> Result<Vec<ChangedFile>> {
        let base = self.first_parent_or_empty(id).await?;
        self.range_files(&base, id, counts).await
    }

    /// The files changed from `from` to `to` (two-dot, both commits or trees), without
    /// patches; `counts` as in [`Self::commit_files`].
    pub async fn range_files(
        &self,
        from: &str,
        to: &str,
        counts: bool,
    ) -> Result<Vec<ChangedFile>> {
        let mut cmd = GitCommand::read(self.root()).arg("diff");
        if counts {
            cmd = cmd.arg("--numstat");
        }
        let cmd = cmd
            .args([
                "--raw",
                "-z",
                "--no-color",
                "--no-ext-diff",
                "--no-abbrev",
                "--find-renames",
                "--end-of-options",
            ])
            .rev(from)?
            .rev(to)?
            .paths::<_, &[u8]>([]);
        Ok(parse_raw_numstat(&self.runner().run(cmd).await?))
    }

    /// One file's diff in `id` (both paths of a rename, so it stays a rename).
    pub async fn commit_file_diff(
        &self,
        id: &str,
        file: &ChangedFile,
    ) -> Result<Option<crate::parse::diff::FileDiff>> {
        Ok(self.commit_file_diff_capped(id, file, None).await?.value)
    }

    /// [`Self::commit_file_diff`], reading at most `cap` bytes of diff when given.
    pub async fn commit_file_diff_capped(
        &self,
        id: &str,
        file: &ChangedFile,
        cap: Option<usize>,
    ) -> Result<Capped<Option<crate::parse::diff::FileDiff>>> {
        let base = self.first_parent_or_empty(id).await?;
        self.range_file_diff_capped(&base, id, file, cap).await
    }

    /// One file's diff from `from` to `to` (both paths of a rename, so it stays a rename).
    pub async fn range_file_diff(
        &self,
        from: &str,
        to: &str,
        file: &ChangedFile,
    ) -> Result<Option<crate::parse::diff::FileDiff>> {
        Ok(self
            .range_file_diff_capped(from, to, file, None)
            .await?
            .value)
    }

    /// [`Self::range_file_diff`], reading at most `cap` bytes of diff when given.
    pub async fn range_file_diff_capped(
        &self,
        from: &str,
        to: &str,
        file: &ChangedFile,
        cap: Option<usize>,
    ) -> Result<Capped<Option<crate::parse::diff::FileDiff>>> {
        let mut paths: Vec<&[u8]> = Vec::new();
        for p in [&file.old_path, &file.new_path].into_iter().flatten() {
            if !paths.contains(&p.as_slice()) {
                paths.push(p);
            }
        }
        let cmd = GitCommand::read(self.root())
            .arg("diff")
            .args(crate::parse::diff::args(true))
            .arg("--end-of-options")
            .rev(from)?
            .rev(to)?
            .paths(paths);
        let (out, truncated) = match cap {
            Some(cap) => self.runner().run_capped(cmd, cap).await?,
            None => (self.runner().run(cmd).await?, false),
        };
        let Capped { value, truncated } = crate::cli::parse_capped(out, truncated)?;
        Ok(Capped {
            value: value.into_iter().next(),
            truncated,
        })
    }

    /// One-line descriptions ([`crate::parse::log::short_description`]) of `ids`, for the
    /// graph's visible rows; commits without one are left out.
    pub async fn descriptions(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut cmd = GitCommand::read(self.root()).args([
            "log",
            "--no-walk=unsorted",
            "-z",
            "--format=%H%x1f%b",
            "--end-of-options",
        ]);
        for id in ids {
            cmd = cmd.rev(id)?;
        }
        let out = self.runner().run(cmd.arg("--")).await?;
        Ok(out
            .split(|&b| b == 0)
            .filter_map(|record| {
                let record = record.strip_prefix(b"\n").unwrap_or(record);
                let sep = record.iter().position(|&b| b == 0x1f)?;
                let id = String::from_utf8_lossy(&record[..sep]).into_owned();
                let text = crate::parse::log::short_description(&record[sep + 1..], 100)?;
                Some((id, text))
            })
            .collect())
    }

    fn write_cmd(&self) -> GitCommand {
        GitCommand::write(self.root())
    }

    /// Normalised branch name, or an error when git would refuse it.
    pub async fn check_branch_name(&self, name: &str) -> Result<String> {
        self.check_ref_name("branch name", name, true).await
    }

    /// Tag name check (same rules as a ref under `refs/tags/`).
    pub async fn check_tag_name(&self, name: &str) -> Result<String> {
        self.check_ref_name("tag name", name, false).await
    }

    pub(crate) async fn check_ref_name(
        &self,
        what: &'static str,
        name: &str,
        branch: bool,
    ) -> Result<String> {
        let name = name.trim();
        let invalid = || GitError::InvalidArgument {
            what,
            value: name.to_string(),
        };
        crate::runner::validate_rev(name).map_err(|_| invalid())?;
        let cmd = GitCommand::read(self.root()).arg("check-ref-format");
        let cmd = if branch {
            cmd.arg("--branch").arg(name)
        } else {
            cmd.arg(format!("refs/tags/{name}"))
        };
        let out = self.runner().output(cmd).await?;
        if !out.success() {
            return Err(invalid());
        }
        Ok(if branch {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            name.to_string()
        })
    }

    /// `git switch <branch>` for a local branch.
    pub async fn switch_branch(&self, branch: &str) -> Result<()> {
        let name = self.check_branch_name(branch).await?;
        let cmd = self
            .write_cmd()
            .args(["switch", "--no-guess", "--end-of-options"]);
        self.runner().run(cmd.rev(&name)?).await.map(drop)
    }

    /// `git switch --detach <commit>`.
    pub async fn checkout_detached(&self, commit: &str) -> Result<()> {
        let cmd = self
            .write_cmd()
            .args(["switch", "--detach", "--end-of-options"]);
        self.runner().run(cmd.rev(commit)?).await.map(drop)
    }

    /// New branch at `commit`; `switch` also checks it out.
    pub async fn create_branch(&self, name: &str, commit: &str, switch: bool) -> Result<()> {
        let name = self.check_branch_name(name).await?;
        let cmd = if switch {
            self.write_cmd().args(["switch", "-c"]).rev(&name)?
        } else {
            self.write_cmd()
                .args(["branch", "--end-of-options"])
                .rev(&name)?
        };
        self.runner().run(cmd.rev(commit)?).await.map(drop)
    }

    /// Tag at `commit`: annotated when `message` is given, lightweight otherwise.
    pub async fn create_tag(&self, name: &str, commit: &str, message: Option<&str>) -> Result<()> {
        let name = self.check_tag_name(name).await?;
        let mut cmd = self.write_cmd().arg("tag");
        let message = message.map(str::trim).filter(|m| !m.is_empty());
        if message.is_some() {
            cmd = cmd.args(["--annotate", "--file=-"]);
        }
        cmd = cmd.arg("--end-of-options").rev(&name)?.rev(commit)?;
        if let Some(message) = message {
            cmd = cmd.stdin(message.as_bytes().to_vec());
        }
        self.runner().run(cmd).await.map(drop)
    }

    /// `git cherry-pick`; a merge commit is picked against its first parent (`-m 1`).
    pub async fn cherry_pick(&self, commit: &str, merge: bool) -> Result<()> {
        let mut cmd = self.write_cmd().arg("cherry-pick");
        if merge {
            cmd = cmd.args(["-m", "1"]);
        }
        let cmd = cmd.arg("--end-of-options").rev(commit)?;
        // Conflicts stop it in progress: not an error, the Conflicts view takes over.
        self.run_may_stop(cmd.env("GIT_EDITOR", ":")).await
    }

    /// `git revert --no-edit`; a merge commit is reverted against its first parent.
    pub async fn revert(&self, commit: &str, merge: bool) -> Result<()> {
        let mut cmd = self.write_cmd().args(["revert", "--no-edit"]);
        if merge {
            cmd = cmd.args(["-m", "1"]);
        }
        let cmd = cmd.arg("--end-of-options").rev(commit)?;
        self.run_may_stop(cmd.env("GIT_EDITOR", ":")).await
    }

    /// Moves the current branch (or detached HEAD) to `commit`.
    pub async fn reset(&self, mode: ResetMode, commit: &str) -> Result<()> {
        // `reset` refuses `--end-of-options` after its mode; the validated rev cannot start
        // with `-`, and `--` ends it.
        let cmd = self
            .write_cmd()
            .args(["reset", mode.flag()])
            .rev(commit)?
            .paths::<_, &[u8]>([]);
        self.runner().run(cmd).await.map(drop)
    }
}
