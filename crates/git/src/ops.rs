//! Working-tree operations behind the Changes panel: stage / unstage / discard (files and
//! patches), commit, push and stashes. All run as [`Mode::Write`](crate::Mode) commands, so the
//! user's hooks and config apply as in a terminal.

use std::path::{Component, Path, PathBuf};

use bstr::{BString, ByteSlice};

use crate::cli::{Capped, CliRepo};
use crate::error::{GitError, Result};
use crate::parse::diff::{self, FileDiff};
use crate::patch::Direction;
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// Where a patch from [`crate::patch::build`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTarget {
    /// The index only (`--cached`): stage or unstage lines.
    Index,
    /// The working tree: discard lines.
    Worktree,
}

/// What `git commit` gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitRequest {
    pub summary: String,
    pub body: String,
    pub amend: bool,
    /// `--signoff`.
    pub signoff: bool,
    /// `Name <email>` overriding the configured author.
    pub author: Option<String>,
    /// `Name <email>` each; written as `Co-authored-by:` trailers.
    pub co_authors: Vec<String>,
}

impl CommitRequest {
    /// The full message: summary, blank line, body, trailers. Lines starting with `#` survive
    /// because the commit uses `--cleanup=whitespace`.
    pub fn message(&self) -> String {
        let mut msg = self.summary.trim().to_string();
        let body = self.body.trim();
        if !body.is_empty() {
            msg.push_str("\n\n");
            msg.push_str(body);
        }
        let trailers: Vec<String> = self
            .co_authors
            .iter()
            .map(|who| who.trim())
            .filter(|who| !who.is_empty())
            .map(|who| format!("Co-authored-by: {who}"))
            .collect();
        if !trailers.is_empty() {
            msg.push_str("\n\n");
            msg.push_str(&trailers.join("\n"));
        }
        msg.push('\n');
        msg
    }
}

/// Checks a `Name <email>` identity typed by the user.
pub fn validate_identity(who: &str) -> Result<()> {
    let who = who.trim();
    let ok = !who.starts_with('-')
        && !who.chars().any(char::is_control)
        && who
            .split_once('<')
            .is_some_and(|(name, rest)| !name.trim().is_empty() && rest.ends_with('>'));
    if ok {
        Ok(())
    } else {
        Err(GitError::InvalidArgument {
            what: "identity (Name <email>)",
            value: who.to_string(),
        })
    }
}

/// Rejects a path that is empty, absolute or climbs out of the worktree.
pub fn validate_repo_path(path: &[u8]) -> Result<()> {
    let text = path.to_str_lossy();
    let rel = Path::new(text.as_ref());
    let bad = path.is_empty()
        || path.contains(&0)
        || rel.has_root()
        || rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if bad {
        return Err(GitError::InvalidArgument {
            what: "path",
            value: text.into_owned(),
        });
    }
    Ok(())
}

/// What [`worktree_file`] reports for a path that runs through a link.
pub const LINKED_PATH: &str = "linked path";

/// The worktree file for repo-relative `path` under `root`, for reading or writing it directly.
/// Refuses invalid paths and any path whose existing parts include a symbolic link or junction;
/// the trailing parts may not exist yet.
pub fn worktree_file(root: &Path, path: &[u8]) -> Result<PathBuf> {
    validate_repo_path(path)?;
    let rel = path.to_path_lossy();
    let mut walked = root.to_path_buf();
    for part in rel.components() {
        walked.push(part);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(GitError::InvalidArgument {
                    what: LINKED_PATH,
                    value: rel.display().to_string(),
                });
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(root.join(rel))
}

fn checked(paths: &[BString]) -> Result<&[BString]> {
    if paths.is_empty() {
        return Err(GitError::InvalidArgument {
            what: "path list",
            value: String::new(),
        });
    }
    paths.iter().try_for_each(|p| validate_repo_path(p))?;
    Ok(paths)
}

impl CliRepo {
    fn write(&self) -> GitCommand {
        GitCommand::write(self.root())
    }

    /// `git add -A`: new, modified and deleted files alike.
    pub async fn stage(&self, paths: &[BString]) -> Result<()> {
        let cmd = self.write().args(["add", "-A"]).paths(checked(paths)?);
        self.runner().run(cmd).await.map(drop)
    }

    pub async fn stage_all(&self) -> Result<()> {
        self.runner()
            .run(self.write().args(["add", "-A"]))
            .await
            .map(drop)
    }

    /// `git reset -q`: works before the first commit too. Pass both sides of a rename.
    pub async fn unstage(&self, paths: &[BString]) -> Result<()> {
        let cmd = self.write().args(["reset", "-q"]).paths(checked(paths)?);
        self.runner().run(cmd).await.map(drop)
    }

    pub async fn unstage_all(&self) -> Result<()> {
        self.runner()
            .run(self.write().args(["reset", "-q"]))
            .await
            .map(drop)
    }

    /// Throws away unstaged changes to tracked files (worktree back to the index).
    pub async fn discard(&self, paths: &[BString]) -> Result<()> {
        let cmd = self
            .write()
            .args(["restore", "--worktree"])
            .paths(checked(paths)?);
        self.runner().run(cmd).await.map(drop)
    }

    /// Deletes untracked files (`git clean -f`; never directories, never ignored files).
    pub async fn delete_untracked(&self, paths: &[BString]) -> Result<()> {
        let cmd = self
            .write()
            .args(["clean", "-f", "-q"])
            .paths(checked(paths)?);
        self.runner().run(cmd).await.map(drop)
    }

    /// Applies a patch from [`crate::patch::build`]. `Reverse` undoes it (unstage / discard).
    pub async fn apply_patch(
        &self,
        patch: Vec<u8>,
        target: ApplyTarget,
        direction: Direction,
    ) -> Result<()> {
        let mut cmd = self.write().arg("apply");
        if target == ApplyTarget::Index {
            cmd = cmd.arg("--cached");
        }
        if direction == Direction::Reverse {
            cmd = cmd.arg("--reverse");
        }
        let cmd = cmd
            .args(["--whitespace=nowarn", "--recount", "-"])
            .stdin(patch);
        self.runner().run(cmd).await.map(drop)
    }

    /// An untracked file shown as all-added (`git diff --no-index /dev/null <path>`).
    pub async fn untracked_diff(&self, path: &BString) -> Result<Vec<FileDiff>> {
        Ok(self.untracked_diff_capped(path, None).await?.value)
    }

    /// [`Self::untracked_diff`], reading at most `cap` bytes of diff when given.
    pub async fn untracked_diff_capped(
        &self,
        path: &BString,
        cap: Option<usize>,
    ) -> Result<Capped<Vec<FileDiff>>> {
        validate_repo_path(path)?;
        let cmd = GitCommand::read(self.root())
            .args(["diff", "--no-index"])
            .args(diff::args(false))
            .paths([b"/dev/null".as_slice(), path.as_slice()]);
        let display = cmd.display();
        let cmd = match cap {
            Some(cap) => cmd.max_stdout(cap),
            None => cmd,
        };
        let out = self.runner().output(cmd).await?;
        // `--no-index` exits 1 when the sides differ, which they always do here.
        if !out.truncated && !matches!(out.code, Some(0 | 1)) {
            return Err(crate::runner::failed(display, out.code, &out.stderr));
        }
        crate::cli::parse_capped(out.stdout, out.truncated)
    }

    /// Commits the index; returns the new commit id.
    pub async fn commit(&self, req: &CommitRequest) -> Result<String> {
        if req.summary.trim().is_empty() && !req.amend {
            return Err(GitError::InvalidArgument {
                what: "commit summary",
                value: String::new(),
            });
        }
        req.co_authors
            .iter()
            .filter(|who| !who.trim().is_empty())
            .try_for_each(|who| validate_identity(who))?;
        let mut cmd = self
            .write()
            .args(["commit", "--quiet", "--cleanup=whitespace", "--file=-"]);
        if req.amend {
            cmd = cmd.arg("--amend");
        }
        if req.signoff {
            cmd = cmd.arg("--signoff");
        }
        if let Some(author) = req.author.as_deref().filter(|a| !a.trim().is_empty()) {
            validate_identity(author)?;
            cmd = cmd.arg("--author").user_arg("author", author.trim())?;
        }
        self.runner()
            .run(cmd.stdin(req.message().into_bytes()))
            .await?;
        let id = self
            .runner()
            .run(GitCommand::read(self.root()).args(["rev-parse", "HEAD"]))
            .await?;
        Ok(String::from_utf8_lossy(&id).trim().to_string())
    }

    /// Full message of `HEAD` (to prefill an amend), or `None` before the first commit.
    pub async fn head_message(&self) -> Result<Option<String>> {
        let cmd = GitCommand::read(self.root()).args(["log", "-1", "--format=%B", "HEAD", "--"]);
        match self.runner().output(cmd).await? {
            out if out.success() => Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            )),
            _ => Ok(None),
        }
    }

    /// `git push`; with `set_upstream`, `git push -u <remote> HEAD` for a branch with no upstream.
    pub async fn push(&self, set_upstream: Option<&str>) -> Result<()> {
        let mut cmd = self.write().arg("push");
        if let Some(remote) = set_upstream {
            cmd = cmd
                .args(["--set-upstream", "--end-of-options"])
                .rev(remote)?
                .arg("HEAD");
        }
        self.runner().run(cmd).await.map(drop)
    }

    /// `git stash push`; `paths` empty stashes everything.
    pub async fn stash_push(
        &self,
        message: &str,
        include_untracked: bool,
        keep_index: bool,
        paths: &[BString],
    ) -> Result<()> {
        let mut cmd = self.write().args(["stash", "push", "--quiet"]);
        if include_untracked {
            cmd = cmd.arg("--include-untracked");
        }
        if keep_index {
            cmd = cmd.arg("--keep-index");
        }
        let message = message.trim();
        if !message.is_empty() {
            if message.chars().any(char::is_control) {
                return Err(GitError::InvalidArgument {
                    what: "stash message",
                    value: message.to_string(),
                });
            }
            cmd = cmd.arg(format!("--message={message}"));
        }
        let cmd = if paths.is_empty() {
            cmd
        } else {
            cmd.paths(checked(paths)?)
        };
        self.runner().run(cmd).await.map(drop)
    }

    /// `git stash apply|pop|drop stash@{index}`.
    pub async fn stash_action(&self, action: StashAction, index: u32) -> Result<()> {
        let verb = match action {
            StashAction::Apply => "apply",
            StashAction::Pop => "pop",
            StashAction::Drop => "drop",
        };
        let mut cmd = self.write().args(["stash", verb, "--quiet"]);
        if action != StashAction::Drop {
            // Restore what was staged as staged.
            cmd = cmd.arg("--index");
        }
        let cmd = cmd.rev(&format!("stash@{{{index}}}"))?;
        self.runner().run(cmd).await.map(drop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashAction {
    Apply,
    Pop,
    Drop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_with_body_and_trailers() {
        let req = CommitRequest {
            summary: " Fix it ".into(),
            body: "\nWhy.\n".into(),
            co_authors: vec!["Ann <a@x>".into(), " ".into()],
            ..Default::default()
        };
        assert_eq!(
            req.message(),
            "Fix it\n\nWhy.\n\nCo-authored-by: Ann <a@x>\n"
        );
    }

    #[test]
    fn identities() {
        assert!(validate_identity("Ann Lee <ann@x.io>").is_ok());
        for bad in ["", "ann@x.io", "<a@x>", "-x <a@x>", "Ann <a@x>\nB"] {
            assert!(validate_identity(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn repo_paths() {
        for ok in ["a.txt", "dir/b c.rs", "./x"] {
            assert!(validate_repo_path(ok.as_bytes()).is_ok(), "{ok}");
        }
        for bad in ["", "../x", "a/../../x", "/etc/passwd", "a\0b"] {
            assert!(validate_repo_path(bad.as_bytes()).is_err(), "{bad:?}");
        }
        #[cfg(windows)]
        for bad in ["C:\\x", "\\\\server\\share\\x", "C:x"] {
            assert!(validate_repo_path(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }
}
