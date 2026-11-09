//! Undo for the destructive operations a reflog can restore: a branch that moved (reset, merge,
//! rebase, pull, cherry-pick) goes back with `git reset --keep <old>`, a deleted branch comes back
//! with `git branch <name> <tip>`. Nothing here guesses: the caller records the state before the
//! operation ([`CliRepo::head_state`] / [`CliRepo::branch_tip`]), and [`CliRepo::apply_undo`]
//! checks it is still safe (HEAD where the operation left it, branch name free, commit present)
//! before touching anything. Uncommitted changes thrown away by `reset --hard` are not in any
//! reflog and are not restored.

use crate::cli::CliRepo;
use crate::error::{GitError, Result};
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// Where HEAD points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    /// Full commit id; `None` in a repository without commits.
    pub oid: Option<String>,
    /// Short name of the checked-out branch; `None` when detached.
    pub branch: Option<String>,
}

/// One step that reverses an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoAction {
    /// `git reset --keep <to>`: HEAD (on `branch`, or detached) is at `from` and goes back to
    /// `to`. Local changes that do not clash are kept; git refuses the reset if some would.
    MoveHeadBack {
        branch: Option<String>,
        from: String,
        to: String,
    },
    /// `git branch <name> <tip>`: bring a deleted branch back.
    RestoreBranch { name: String, tip: String },
}

impl UndoAction {
    /// The command line it will run, for tooltips and the palette.
    pub fn preview(&self) -> String {
        match self {
            UndoAction::MoveHeadBack { to, .. } => format!("git reset --keep {to}"),
            UndoAction::RestoreBranch { name, tip } => {
                format!("git branch --end-of-options {name} {tip}")
            }
        }
    }
}

/// A full object id: 40 (SHA-1) or 64 (SHA-256) hex digits. Abbreviations and names are not
/// accepted: an undo target must not change meaning between recording and applying.
pub fn is_full_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn invalid(what: &'static str, value: &str) -> GitError {
    GitError::InvalidArgument {
        what,
        value: value.to_string(),
    }
}

impl CliRepo {
    /// HEAD's commit and branch (read-only; cheap enough to call before every operation).
    pub async fn head_state(&self) -> Result<HeadState> {
        let oid = self
            .runner()
            .output(GitCommand::read(self.root()).args(["rev-parse", "--verify", "-q", "HEAD"]))
            .await?;
        let oid = oid
            .success()
            .then(|| String::from_utf8_lossy(&oid.stdout).trim().to_string())
            .filter(|oid| is_full_oid(oid));
        let branch = self
            .runner()
            .output(GitCommand::read(self.root()).args(["symbolic-ref", "-q", "--short", "HEAD"]))
            .await?;
        let branch = branch
            .success()
            .then(|| String::from_utf8_lossy(&branch.stdout).trim().to_string())
            .filter(|b| !b.is_empty());
        Ok(HeadState { oid, branch })
    }

    /// The commit local branch `name` points at, `None` when there is no such branch.
    pub async fn branch_tip(&self, name: &str) -> Result<Option<String>> {
        let name = self.check_branch_name(name).await?;
        let cmd = GitCommand::read(self.root())
            .args(["rev-parse", "--verify", "-q"])
            .rev(&format!("refs/heads/{name}"))?;
        let out = self.runner().output(cmd).await?;
        Ok(out
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|oid| is_full_oid(oid)))
    }

    async fn commit_exists(&self, oid: &str) -> Result<bool> {
        let cmd = GitCommand::read(self.root())
            .args(["cat-file", "-e"])
            .rev(&format!("{oid}^{{commit}}"))?;
        Ok(self.runner().output(cmd).await?.success())
    }

    /// Reverses one recorded operation, after checking it is still safe.
    pub async fn apply_undo(&self, action: &UndoAction) -> Result<()> {
        match action {
            UndoAction::MoveHeadBack { branch, from, to } => {
                if !is_full_oid(from) {
                    return Err(invalid("undo (current commit)", from));
                }
                if !is_full_oid(to) {
                    return Err(invalid("undo (target commit)", to));
                }
                let head = self.head_state().await?;
                if head.oid.as_deref() != Some(from.as_str()) || head.branch != *branch {
                    return Err(invalid(
                        "undo (HEAD has moved since; nothing was changed)",
                        head.oid.as_deref().unwrap_or("none"),
                    ));
                }
                if !self.commit_exists(to).await? {
                    return Err(invalid("undo (commit is gone)", to));
                }
                // `reset` refuses `--end-of-options` after its mode; the id is plain hex.
                let cmd = GitCommand::write(self.root())
                    .args(["reset", "--keep"])
                    .rev(to)?
                    .paths::<_, &[u8]>([]);
                self.runner().run(cmd).await.map(drop)
            }
            UndoAction::RestoreBranch { name, tip } => {
                if !is_full_oid(tip) {
                    return Err(invalid("undo (branch tip)", tip));
                }
                let name = self.check_branch_name(name).await?;
                if self.branch_tip(&name).await?.is_some() {
                    return Err(invalid(
                        "undo (a branch with this name exists again)",
                        &name,
                    ));
                }
                if !self.commit_exists(tip).await? {
                    return Err(invalid("undo (commit is gone)", tip));
                }
                let cmd = GitCommand::write(self.root())
                    .args(["branch", "--end-of-options"])
                    .rev(&name)?
                    .rev(tip)?;
                self.runner().run(cmd).await.map(drop)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_hex_ids_are_targets() {
        assert!(is_full_oid(&"a".repeat(40)));
        assert!(is_full_oid(&"0F".repeat(32)));
        assert!(!is_full_oid("abc1234"));
        assert!(!is_full_oid("HEAD"));
        assert!(!is_full_oid(&format!("{}g", "a".repeat(39))));
        assert!(!is_full_oid(&format!("-{}", "a".repeat(39))));
    }

    #[test]
    fn previews_name_the_command() {
        let to = "b".repeat(40);
        let reset = UndoAction::MoveHeadBack {
            branch: Some("main".into()),
            from: "a".repeat(40),
            to: to.clone(),
        };
        assert_eq!(reset.preview(), format!("git reset --keep {to}"));
        let restore = UndoAction::RestoreBranch {
            name: "topic".into(),
            tip: to.clone(),
        };
        assert!(
            restore
                .preview()
                .starts_with("git branch --end-of-options topic ")
        );
    }
}
