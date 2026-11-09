//! [`GitRepo`] over the git CLI.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};
use crate::parse::diff::{self, FileDiff};
use crate::parse::log::{self, Commit, LogParser};
use crate::parse::refs::{self, RefEntry, Remote, Stash};
use crate::parse::status::{self, Status};
use crate::repo::{DiffTarget, GitRepo, LogQuery};
use crate::runner::{GitCommand, GitRunner, GitStream};

/// Commits per batch from [`CliRepo::log_stream`]: one page of the Repo views' commit list,
/// loaded as the user scrolls.
pub const LOG_BATCH: usize = 200;

/// Bytes of `git diff` output the file-diff views read before asking the user (see
/// [`Capped`]). Syntax colouring has its own, lower limits in the `ui` crate.
pub const DIFF_CAP: usize = 4 * 1024 * 1024;

/// A diff read under a byte cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capped<T> {
    pub value: T,
    /// The output was longer than the cap: `value` holds whole files from its start only (a
    /// single huge file leaves it empty).
    pub truncated: bool,
}

/// Parses `git diff` output read with `cap` (`Some`: it may have stopped early).
pub(crate) fn parse_capped(out: Vec<u8>, truncated: bool) -> Result<Capped<Vec<FileDiff>>> {
    let text = if truncated {
        diff::whole_files(&out)
    } else {
        &out
    };
    Ok(Capped {
        value: diff::parse(text)?,
        truncated,
    })
}

/// Git's empty tree, for diffing a root commit.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone)]
pub struct CliRepo {
    root: PathBuf,
    runner: GitRunner,
}

impl CliRepo {
    /// Opens the repository containing `path` (any directory inside the worktree).
    pub async fn open(runner: GitRunner, path: impl AsRef<Path>) -> Result<Self> {
        let out = runner
            .run(GitCommand::read(path.as_ref()).args(["rev-parse", "--show-toplevel"]))
            .await?;
        let text = String::from_utf8_lossy(&out);
        let root = text.trim_end_matches(['\r', '\n']);
        if root.is_empty() {
            // Bare repositories and `.git` dirs have no worktree.
            return Err(GitError::InvalidArgument {
                what: "repository (no worktree)",
                value: path.as_ref().display().to_string(),
            });
        }
        Ok(Self {
            root: PathBuf::from(root),
            runner,
        })
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    fn read(&self) -> GitCommand {
        GitCommand::read(&self.root)
    }

    fn log_command(&self, query: &LogQuery) -> Result<GitCommand> {
        let format = if query.bodies {
            log::FORMAT
        } else {
            log::GRAPH_FORMAT
        };
        let mut cmd = self.read().arg("log").args(log::args_with(format));
        if query.topo_order {
            cmd = cmd.arg("--topo-order");
        }
        if query.skip > 0 {
            cmd = cmd.arg(format!("--skip={}", query.skip));
        }
        if let Some(limit) = query.limit {
            cmd = cmd.arg(format!("--max-count={limit}"));
        }
        if query.all {
            // The stash ref's tip would show with its index / untracked helper commits;
            // stashes come in through `query.stashes` instead. GitDragon's fetched pull requests
            // (`refs/gitdragon/*`) are not part of the repository's history.
            cmd = cmd
                .arg("--exclude=refs/stash")
                .arg("--exclude=refs/gitdragon/*")
                .arg("--all");
        }
        cmd = cmd.arg("--end-of-options");
        for rev in query.revs.iter().chain(&query.stashes) {
            cmd = cmd.rev(rev)?;
        }
        // Always end with `--` so a rev can never be taken for a path, even with no paths.
        Ok(cmd.paths(&query.paths))
    }

    /// [`GitRepo::diff`], reading at most `cap` bytes of output when given.
    pub async fn diff_capped(
        &self,
        target: DiffTarget,
        cap: Option<usize>,
    ) -> Result<Capped<Vec<FileDiff>>> {
        let cmd = match &target {
            DiffTarget::Unstaged { paths } => {
                self.read().arg("diff").args(diff::args(false)).paths(paths)
            }
            DiffTarget::Staged { paths } => self
                .read()
                .args(["diff", "--cached"])
                .args(diff::args(false))
                .paths(paths),
            DiffTarget::Commit { id } => {
                // First parent, or the empty tree for a root commit.
                let parents = self
                    .runner
                    .run(
                        self.read()
                            .args(["rev-list", "--parents", "-n", "1", "--end-of-options"])
                            .rev(id)?,
                    )
                    .await?;
                let text = String::from_utf8_lossy(&parents);
                let mut ids = text.split_whitespace().skip(1);
                let base = ids.next().unwrap_or(EMPTY_TREE).to_string();
                self.read()
                    .arg("diff")
                    .args(diff::args(true))
                    .arg("--end-of-options")
                    .rev(&base)?
                    .rev(id)?
                    .paths::<_, &[u8]>([])
            }
            DiffTarget::Range { from, to } => self
                .read()
                .arg("diff")
                .args(diff::args(true))
                .arg("--end-of-options")
                .rev(from)?
                .rev(to)?
                .paths::<_, &[u8]>([]),
        };
        match cap {
            Some(cap) => {
                let (out, truncated) = self.runner.run_capped(cmd, cap).await?;
                parse_capped(out, truncated)
            }
            None => parse_capped(self.runner.run(cmd).await?, false),
        }
    }

    /// Streams the log in batches of up to [`LOG_BATCH`] commits. Drop it to cancel.
    pub fn log_stream(&self, query: &LogQuery) -> Result<LogStream> {
        let stream = self.runner.stream(self.log_command(query)?)?;
        Ok(LogStream {
            stream: Some(stream),
            parser: Some(LogParser::new()),
            pending: Vec::new(),
        })
    }
}

impl GitRepo for CliRepo {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn status(&self) -> Result<Status> {
        let out = self
            .runner
            .run(self.read().arg("status").args(status::ARGS))
            .await?;
        Ok(status::parse(&out)?)
    }

    async fn log(&self, query: LogQuery) -> Result<Vec<Commit>> {
        let out = self.runner.run(self.log_command(&query)?).await?;
        Ok(log::parse(&out)?)
    }

    async fn diff(&self, target: DiffTarget) -> Result<Vec<FileDiff>> {
        Ok(self.diff_capped(target, None).await?.value)
    }

    async fn refs(&self) -> Result<Vec<RefEntry>> {
        let out = self
            .runner
            .run(self.read().arg("for-each-ref").args(refs::refs_args()))
            .await?;
        Ok(refs::parse_refs(&out)?)
    }

    async fn stashes(&self) -> Result<Vec<Stash>> {
        let out = self
            .runner
            .run(self.read().arg("stash").args(refs::stash_args()))
            .await?;
        Ok(refs::parse_stashes(&out)?)
    }

    async fn remotes(&self) -> Result<Vec<Remote>> {
        let out = self
            .runner
            .run(self.read().arg("remote").args(refs::REMOTE_ARGS))
            .await?;
        Ok(refs::parse_remotes(&out)?)
    }

    async fn ahead_behind(&self, local: &str, upstream: &str) -> Result<(u32, u32)> {
        crate::runner::validate_rev(local)?;
        crate::runner::validate_rev(upstream)?;
        let range = format!("{local}...{upstream}");
        let out = self
            .runner
            .run(
                self.read()
                    .arg("rev-list")
                    .args(refs::AHEAD_BEHIND_ARGS)
                    .arg("--end-of-options")
                    .rev(&range)?
                    .paths::<_, &[u8]>([]),
            )
            .await?;
        Ok(refs::parse_ahead_behind(&out)?)
    }
}

/// Batches of parsed commits from a running `git log`.
pub struct LogStream {
    stream: Option<GitStream>,
    parser: Option<LogParser>,
    pending: Vec<Commit>,
}

impl LogStream {
    /// Next batch of up to [`LOG_BATCH`] commits, or `None` when the log is exhausted.
    pub async fn next_batch(&mut self) -> Result<Option<Vec<Commit>>> {
        while self.pending.len() < LOG_BATCH {
            let Some(stream) = self.stream.as_mut() else {
                break;
            };
            match stream.next_chunk().await? {
                Some(chunk) => {
                    if let Some(parser) = self.parser.as_mut() {
                        self.pending.extend(parser.push(&chunk)?);
                    }
                }
                None => {
                    if let Some(parser) = self.parser.take() {
                        self.pending.extend(parser.finish()?);
                    }
                    if let Some(stream) = self.stream.take() {
                        stream.finish().await?;
                    }
                }
            }
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        let take = self.pending.len().min(LOG_BATCH);
        let rest = self.pending.split_off(take);
        Ok(Some(std::mem::replace(&mut self.pending, rest)))
    }
}
