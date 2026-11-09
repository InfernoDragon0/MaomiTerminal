//! Branch, remote, tag and sync operations behind the sidebar and toolbar: rename / delete /
//! upstream, remotes add / edit / remove, fetch / pull / push, tag push / delete, and the
//! read-only submodule list. Names are checked by git (`check-ref-format`), URLs and revisions
//! by the runner's argument rules; force pushes only ever use `--force-with-lease`.

use bstr::ByteSlice as _;

use crate::cli::CliRepo;
use crate::error::{GitError, Result};
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// How `pull` integrates the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PullMode {
    /// Whatever `pull.rebase` / `pull.ff` say.
    #[default]
    Config,
    Merge,
    Rebase,
    FastForwardOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PushOptions {
    /// `--set-upstream`: track the pushed branch.
    pub set_upstream: bool,
    /// `--force-with-lease`: overwrite only if the remote is where we last saw it.
    pub force_with_lease: bool,
}

/// One line of `git submodule status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    pub path: String,
    /// Checked-out (or recorded, when not initialised) commit.
    pub oid: String,
    pub state: SubmoduleState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleState {
    UpToDate,
    /// `-`: not initialised.
    Uninitialized,
    /// `+`: checked-out commit differs from the one recorded in the superproject.
    Modified,
    /// `U`: merge conflicts.
    Conflicted,
}

impl CliRepo {
    fn sync_cmd(&self) -> GitCommand {
        GitCommand::write(self.root())
    }

    async fn run_write(&self, cmd: GitCommand) -> Result<()> {
        self.runner().run(cmd).await.map(drop)
    }

    // ---- branches ---------------------------------------------------------------------------

    pub async fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        let old = self.check_branch_name(old).await?;
        let new = self.check_branch_name(new).await?;
        let cmd = self
            .sync_cmd()
            .args(["branch", "--move", "--end-of-options"])
            .rev(&old)?
            .rev(&new)?;
        self.run_write(cmd).await
    }

    /// `git branch -d`; `force` uses `-D` (deletes unmerged work: confirm first).
    pub async fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        let name = self.check_branch_name(name).await?;
        let flag = if force { "-D" } else { "-d" };
        let cmd = self
            .sync_cmd()
            .args(["branch", flag, "--end-of-options"])
            .rev(&name)?;
        self.run_write(cmd).await
    }

    /// Tracks `upstream` (e.g. `origin/main`), or stops tracking with `None`.
    pub async fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<()> {
        let branch = self.check_branch_name(branch).await?;
        let cmd = match upstream {
            Some(upstream) => {
                crate::runner::validate_rev(upstream)?;
                self.sync_cmd()
                    .arg("branch")
                    .arg(format!("--set-upstream-to={upstream}"))
            }
            None => self.sync_cmd().args(["branch", "--unset-upstream"]),
        };
        self.run_write(cmd.arg("--end-of-options").rev(&branch)?)
            .await
    }

    // ---- remotes ----------------------------------------------------------------------------

    pub(crate) async fn check_remote_name(&self, name: &str) -> Result<String> {
        let name = name.trim();
        let invalid = || GitError::InvalidArgument {
            what: "remote name",
            value: name.to_string(),
        };
        if name.contains('/') {
            return Err(invalid());
        }
        // A remote name must make valid `refs/remotes/<name>/…` refs.
        self.check_ref_name("remote name", &format!("{name}/x"), false)
            .await
            .map_err(|_| invalid())?;
        Ok(name.to_string())
    }

    pub async fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        let name = self.check_remote_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["remote", "add", "--end-of-options"])
            .rev(&name)?
            .user_arg("remote URL", url.trim())?;
        self.run_write(cmd).await
    }

    pub async fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        let name = self.check_remote_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["remote", "set-url", "--end-of-options"])
            .rev(&name)?
            .user_arg("remote URL", url.trim())?;
        self.run_write(cmd).await
    }

    pub async fn rename_remote(&self, old: &str, new: &str) -> Result<()> {
        let old = self.check_remote_name(old).await?;
        let new = self.check_remote_name(new).await?;
        let cmd = self
            .sync_cmd()
            .args(["remote", "rename", "--end-of-options"])
            .rev(&old)?
            .rev(&new)?;
        self.run_write(cmd).await
    }

    /// Removes the remote and its remote-tracking branches.
    pub async fn remove_remote(&self, name: &str) -> Result<()> {
        let name = self.check_remote_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["remote", "remove", "--end-of-options"])
            .rev(&name)?;
        self.run_write(cmd).await
    }

    // ---- sync -------------------------------------------------------------------------------

    /// `git fetch <remote>`, or every remote with `None`; `prune` drops deleted branches.
    pub async fn fetch(&self, remote: Option<&str>, prune: bool) -> Result<()> {
        let cmd = fetch_command(self.sync_cmd(), prune);
        let cmd = match remote {
            Some(remote) => {
                let remote = self.check_remote_name(remote).await?;
                cmd.arg("--end-of-options").rev(&remote)?
            }
            None => cmd.arg("--all"),
        };
        self.run_write(cmd).await
    }

    /// Auto-fetch of every remote: no submodules, and the repo's own config can't choose
    /// programs to run (see [`GitCommand::background`]). One fetch per remote, since
    /// `--upload-pack` (the first configured value wins over `-c`) isn't passed on by `--all`.
    pub async fn fetch_background(&self, prune: bool) -> Result<()> {
        let mut first_error = None;
        for remote in self.remotes().await? {
            let cmd = fetch_command(self.sync_cmd().background(), prune)
                .args([
                    "--no-recurse-submodules",
                    "--upload-pack=git-upload-pack",
                    "--end-of-options",
                ])
                .rev(&remote.name)?;
            if let Err(e) = self.run_write(cmd).await {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Pulls the current branch's upstream.
    pub async fn pull(&self, mode: PullMode) -> Result<()> {
        let cmd = pull_command(self.sync_cmd(), mode);
        // A merge / rebase that stops for conflicts continues in the Conflicts view.
        self.run_may_stop(cmd.env("GIT_EDITOR", ":")).await
    }

    /// Pushes local `branch` to `remote` (same name there).
    pub async fn push_branch(&self, remote: &str, branch: &str, opts: PushOptions) -> Result<()> {
        let remote = self.check_remote_name(remote).await?;
        let branch = self.check_branch_name(branch).await?;
        let mut cmd = self.sync_cmd().args(["push", "--no-progress"]);
        if opts.set_upstream {
            cmd = cmd.arg("--set-upstream");
        }
        if opts.force_with_lease {
            cmd = cmd.arg("--force-with-lease");
        }
        let cmd = cmd
            .arg("--end-of-options")
            .rev(&remote)?
            .rev(&format!("refs/heads/{branch}:refs/heads/{branch}"))?;
        self.run_write(cmd).await
    }

    // ---- tags -------------------------------------------------------------------------------

    pub async fn delete_tag(&self, name: &str) -> Result<()> {
        let name = self.check_tag_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["tag", "--delete", "--end-of-options"])
            .rev(&name)?;
        self.run_write(cmd).await
    }

    pub async fn push_tag(&self, remote: &str, name: &str) -> Result<()> {
        let remote = self.check_remote_name(remote).await?;
        let name = self.check_tag_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["push", "--no-progress", "--end-of-options"])
            .rev(&remote)?
            .rev(&format!("refs/tags/{name}"))?;
        self.run_write(cmd).await
    }

    pub async fn delete_remote_tag(&self, remote: &str, name: &str) -> Result<()> {
        let remote = self.check_remote_name(remote).await?;
        let name = self.check_tag_name(name).await?;
        let cmd = self
            .sync_cmd()
            .args(["push", "--no-progress", "--delete", "--end-of-options"])
            .rev(&remote)?
            .rev(&format!("refs/tags/{name}"))?;
        self.run_write(cmd).await
    }

    // ---- submodules -------------------------------------------------------------------------

    /// `git submodule status` (read-only list; never initialises or updates anything).
    pub async fn submodules(&self) -> Result<Vec<Submodule>> {
        let cmd = GitCommand::read(self.root()).args(["submodule", "status"]);
        let out = self.runner().run(cmd).await?;
        Ok(parse_submodules(&out))
    }
}

/// `git fetch --no-progress [--prune]`, before the remote (or `--all`).
fn fetch_command(cmd: GitCommand, prune: bool) -> GitCommand {
    let cmd = cmd.args(["fetch", "--no-progress"]);
    if prune { cmd.arg("--prune") } else { cmd }
}

/// `git pull --no-progress [mode flag]`.
fn pull_command(cmd: GitCommand, mode: PullMode) -> GitCommand {
    let cmd = cmd.args(["pull", "--no-progress"]);
    match mode {
        PullMode::Config => cmd,
        PullMode::Merge => cmd.arg("--no-rebase"),
        PullMode::Rebase => cmd.arg("--rebase"),
        PullMode::FastForwardOnly => cmd.arg("--ff-only"),
    }
}

/// The command line `fetch(None, prune)` runs, for previews.
pub fn fetch_all_preview(prune: bool) -> String {
    fetch_command(GitCommand::read("."), prune)
        .arg("--all")
        .display()
}

/// The command line `pull(mode)` runs, for previews.
pub fn pull_preview(mode: PullMode) -> String {
    pull_command(GitCommand::read("."), mode).display()
}

/// ` <oid> <path> (<describe>)` per line, first column `-` / `+` / `U` / space.
pub fn parse_submodules(output: &[u8]) -> Vec<Submodule> {
    output
        .lines()
        .filter_map(|line| {
            let (&flag, rest) = line.split_first()?;
            let state = match flag {
                b'-' => SubmoduleState::Uninitialized,
                b'+' => SubmoduleState::Modified,
                b'U' => SubmoduleState::Conflicted,
                _ => SubmoduleState::UpToDate,
            };
            let rest = rest.to_str_lossy();
            let (oid, rest) = rest.split_once(' ')?;
            let path = match rest.rfind(" (") {
                Some(i) if rest.ends_with(')') => &rest[..i],
                _ => rest,
            };
            Some(Submodule {
                path: path.to_string(),
                oid: oid.to_string(),
                state,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_match_the_commands() {
        assert_eq!(fetch_all_preview(false), "git fetch --no-progress --all");
        assert_eq!(
            fetch_all_preview(true),
            "git fetch --no-progress --prune --all"
        );
        assert_eq!(pull_preview(PullMode::Config), "git pull --no-progress");
        assert_eq!(
            pull_preview(PullMode::Rebase),
            "git pull --no-progress --rebase"
        );
        assert_eq!(
            pull_preview(PullMode::FastForwardOnly),
            "git pull --no-progress --ff-only"
        );
    }

    #[test]
    fn submodule_status_lines() {
        let out = b" 1111111111111111111111111111111111111111 libs/a (v1.0)\n\
-2222222222222222222222222222222222222222 libs/b\n\
+3333333333333333333333333333333333333333 libs/c d (heads/main)\n\
U4444444444444444444444444444444444444444 libs/e\n";
        let subs = parse_submodules(out);
        assert_eq!(subs.len(), 4);
        assert_eq!(subs[0].path, "libs/a");
        assert_eq!(subs[0].state, SubmoduleState::UpToDate);
        assert_eq!(subs[1].state, SubmoduleState::Uninitialized);
        assert_eq!(subs[2].path, "libs/c d");
        assert_eq!(subs[2].state, SubmoduleState::Modified);
        assert_eq!(subs[3].state, SubmoduleState::Conflicted);
        assert!(parse_submodules(b"").is_empty());
    }
}
