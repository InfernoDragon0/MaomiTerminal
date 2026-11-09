//! Git state for the Repo dock and the Repo tab.
//!
//! [`RepoHub`] keeps at most one open [`RepoModel`]: the repo of the folder it was last asked to
//! follow. Nothing runs until a view calls [`RepoHub::follow`], and [`RepoHub::release`] drops the
//! model, which stops its watcher and kills its git processes. All git work runs on the background
//! executor.

mod model;
mod suggest;

pub use model::{LogScope, OpKind, RepoEvent, RepoModel};

use std::path::{Path, PathBuf};

use gpui_kit::{AppContext as _, Context, Entity, Task};
use maomi_git::{CliRepo, GitRepo as _, GitRunner};

/// What the hub shows for the folder it follows.
#[derive(Clone)]
pub enum RepoState {
    /// Nothing followed yet, or released.
    Idle,
    /// Looking up the folder's repository.
    Probing(PathBuf),
    /// The folder is inside this repository.
    Open(Entity<RepoModel>),
    /// The folder is not inside a git work tree (or git is missing: `reason`).
    NoRepo { dir: PathBuf, reason: String },
}

pub struct RepoHub {
    state: RepoState,
    /// The folder last passed to [`Self::follow`], so repeated calls for it do nothing.
    followed: Option<PathBuf>,
    probe: Option<Task<()>>,
}

impl Default for RepoHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RepoHub {
    pub fn new() -> Self {
        Self {
            state: RepoState::Idle,
            followed: None,
            probe: None,
        }
    }

    pub fn state(&self) -> &RepoState {
        &self.state
    }

    /// The open repository, if the followed folder is in one.
    pub fn model(&self) -> Option<&Entity<RepoModel>> {
        match &self.state {
            RepoState::Open(model) => Some(model),
            _ => None,
        }
    }

    /// Shows the repository that contains `dir`. Keeps the open model when `dir` is in the same
    /// repository; otherwise opens the new one and drops the old (one watcher at a time).
    pub fn follow(&mut self, dir: &Path, cx: &mut Context<Self>) {
        if self.followed.as_deref() == Some(dir) {
            return;
        }
        self.followed = Some(dir.to_path_buf());
        if !matches!(self.state, RepoState::Open(_)) {
            self.state = RepoState::Probing(dir.to_path_buf());
            cx.notify();
        }
        let dir = dir.to_path_buf();
        let probe = cx.background_executor().spawn({
            let dir = dir.clone();
            async move {
                // detect() blocks on its first call (it runs `git --version`).
                let runner = GitRunner::detect()?;
                let repo = CliRepo::open(runner.clone(), &dir).await?;
                Ok::<_, maomi_git::GitError>((repo, runner))
            }
        });
        self.probe = Some(cx.spawn(async move |this, cx| {
            let opened = probe.await;
            _ = this.update(cx, |hub, cx| {
                hub.probe = None;
                match opened {
                    Ok((repo, runner)) => {
                        let same = hub
                            .model()
                            .is_some_and(|m| m.read(cx).root() == repo.root());
                        if !same {
                            let model = cx.new(|cx| RepoModel::new(repo, runner, cx));
                            hub.state = RepoState::Open(model);
                        }
                    }
                    Err(err) => {
                        hub.state = RepoState::NoRepo {
                            dir,
                            reason: err.to_string(),
                        };
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Drops the open model (the dock closed and no Repo tab uses it). The Repo tab keeps its own
    /// handle, so its model lives on until the tab closes.
    pub fn release(&mut self, cx: &mut Context<Self>) {
        self.probe = None;
        self.followed = None;
        if !matches!(self.state, RepoState::Idle) {
            self.state = RepoState::Idle;
            cx.notify();
        }
    }
}
