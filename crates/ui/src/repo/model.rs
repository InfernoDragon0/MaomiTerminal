//! One open repository: status, refs, the commit log in pages, user operations and the watcher
//! that keeps it fresh. A slimmed port of GitDragon's `repo_tab.rs` (no LFS, locks, auto-fetch
//! or undo).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bstr::BString;
use gpui_kit::{Context, EventEmitter, SharedString, Task};
use maomi_git::conflicts::{self, ConflictFile, InProgress};
use maomi_git::graph::{History, Pseudo};
use maomi_git::parse::log::Commit;
use maomi_git::parse::refs::{RefEntry, Remote, Stash};
use maomi_git::parse::status::Status;
use maomi_git::watch::{self, ChangeSet, RepoWatcher};
use maomi_git::{
    CliRepo, CommandRecord, CommitRequest, GitError, GitRepo as _, GitRunner, LogQuery, Mode,
    PullMode,
};
use maomi_workbench::CommitSuggestion;

/// The message of a cancelled operation.
pub const CANCELLED: &str = "Cancelled.";

/// Wait after opening before the commit-graph write, so it does not compete with the first
/// status and log.
const MAINTENANCE_DELAY: Duration = Duration::from_secs(2);

/// Build and dependency folders skipped by the watcher even when they are not gitignored: a
/// build writes thousands of files there and each burst would refresh the status.
const NOISY_DIRS: [&str; 2] = ["target", "node_modules"];

/// Which commits the log lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogScope {
    /// The current branch (`HEAD`).
    Branch,
    /// Every branch, remote and tag, plus stashes.
    #[default]
    All,
}

/// The remote operations the toolbar animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Fetch,
    Pull,
    Push,
    Other,
}

/// For toasts: the view that owns a window turns these into notifications.
#[derive(Debug, Clone)]
pub enum RepoEvent {
    OpFailed(SharedString),
}

/// The commit log, one page ([`maomi_git::cli::LOG_BATCH`] commits) at a time.
#[derive(Default)]
pub struct LogPages {
    pub history: History,
    /// A page was asked for and has not arrived yet.
    pub loading: bool,
    /// `git log` ended: there are no more pages.
    pub exhausted: bool,
    pub error: Option<SharedString>,
    /// Pages received for the current load, asked again after a reload.
    pages: usize,
    /// Pages asked for in the current load; `loading` while more are asked than received.
    asked: usize,
    /// What the rows were loaded for; a refresh that changes it reloads the log.
    loaded_for: Option<LogSnapshot>,
    /// Each send asks the feed for one more page.
    want: Option<async_channel::Sender<()>>,
    task: Option<Task<()>>,
}

/// The parts of the repo state the log depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSnapshot {
    scope: LogScope,
    head: Option<String>,
    dirty: bool,
    refs: Vec<(String, String)>,
    stashes: Vec<(String, u32)>,
}

impl LogSnapshot {
    fn new(scope: LogScope, status: &Status, refs: &[RefEntry], stashes: &[Stash]) -> Self {
        Self {
            scope,
            head: status.branch.oid.clone(),
            dirty: !status.entries.is_empty(),
            refs: refs
                .iter()
                .map(|r| (r.full_name.clone(), r.oid.clone()))
                .collect(),
            stashes: stashes.iter().map(|s| (s.oid.clone(), s.index)).collect(),
        }
    }

    fn pseudo(&self) -> Pseudo {
        Pseudo {
            wip_parent: self.head.clone().filter(|_| self.dirty),
            stashes: match self.scope {
                LogScope::All => self.stashes.clone(),
                LogScope::Branch => Vec::new(),
            },
        }
    }

    fn query(&self) -> LogQuery {
        let pseudo = self.pseudo();
        LogQuery {
            all: self.scope == LogScope::All,
            stashes: pseudo.stashes.into_iter().map(|(id, _)| id).collect(),
            ..LogQuery::default()
        }
    }
}

pub struct RepoModel {
    repo: CliRepo,
    pub name: SharedString,
    pub status: Option<Status>,
    /// A merge / rebase / cherry-pick / revert git stopped in the middle of.
    pub in_progress: Option<InProgress>,
    /// Unmerged paths of [`Self::status`].
    pub conflicts: Vec<ConflictFile>,
    pub refs: Vec<RefEntry>,
    pub stashes: Vec<Stash>,
    pub remotes: Vec<Remote>,
    pub log: LogPages,
    pub log_scope: LogScope,
    /// A status refresh is running.
    pub loading: bool,
    /// The last refresh failed.
    pub error: Option<SharedString>,
    /// Bumped on every successful refresh, so views can tell the status changed.
    pub generation: u64,
    /// Label of the running user operation, e.g. `Pushing main`.
    pub op_label: Option<SharedString>,
    pub op_kind: OpKind,
    /// The last failed user operation; cleared by the next one.
    pub op_error: Option<SharedString>,
    /// The latest write command (for the output strip), updated live while it runs.
    pub last_command: Option<CommandRecord>,
    /// The board's suggested commit for the last session, until it is committed.
    pub suggestion: Option<CommitSuggestion>,
    /// HEAD the suggestion was read for (outer `None`: not read yet).
    suggested_for: Option<Option<String>>,
    suggest_task: Option<Task<()>>,
    /// The running user operation; dropping it kills git.
    op_task: Option<Task<()>>,
    /// Replaced on every refresh; dropping the old task kills its git processes.
    refresh_task: Option<Task<()>>,
    /// A refresh was asked for while one ran: run another when it ends.
    refresh_again: bool,
    ignore_task: Option<Task<()>>,
    watcher: Option<RepoWatcher>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<RepoEvent> for RepoModel {}

impl RepoModel {
    pub fn new(repo: CliRepo, runner: GitRunner, cx: &mut Context<Self>) -> Self {
        let name = repo
            .root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.root().display().to_string());
        let mut model = Self {
            repo,
            name: name.into(),
            status: None,
            in_progress: None,
            conflicts: Vec::new(),
            refs: Vec::new(),
            stashes: Vec::new(),
            remotes: Vec::new(),
            log: LogPages::default(),
            log_scope: LogScope::default(),
            loading: true,
            error: None,
            generation: 0,
            op_label: None,
            op_kind: OpKind::Other,
            op_error: None,
            last_command: None,
            suggestion: None,
            suggested_for: None,
            suggest_task: None,
            op_task: None,
            refresh_task: None,
            refresh_again: false,
            ignore_task: None,
            watcher: None,
            _tasks: Vec::new(),
        };
        model.follow_command_log(runner, cx);
        model.start_watcher(cx);
        model.refresh(cx);
        model.ensure_commit_graph(cx);
        model
    }

    pub fn root(&self) -> &Path {
        self.repo.root()
    }

    pub fn repo(&self) -> &CliRepo {
        &self.repo
    }

    pub fn is_busy(&self) -> bool {
        self.op_task.is_some()
    }

    /// Mirrors the runner's write commands into [`Self::last_command`] for the output strip.
    fn follow_command_log(&mut self, runner: GitRunner, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::unbounded::<CommandRecord>();
        runner.log().set_listener(move |record| {
            if record.mode == Mode::Write {
                _ = tx.try_send(record.clone());
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(mut record) = rx.recv().await {
                while let Ok(newer) = rx.try_recv() {
                    record = newer;
                }
                let alive = this.update(cx, |model, cx| {
                    model.last_command = Some(record);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        let runner = self.repo.runner().clone();
        let root = self.root().to_path_buf();
        let (tx, rx) = async_channel::unbounded::<ChangeSet>();
        let started = cx.background_executor().spawn(async move {
            let git_dir = watch::git_dir(&runner, &root).await?;
            let ignored = ignored_dirs(&runner, &root).await;
            RepoWatcher::new(&root, &git_dir, ignored, move |change| {
                _ = tx.try_send(change);
            })
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            match started.await {
                Ok(watcher) => {
                    if this.update(cx, |m, _| m.watcher = Some(watcher)).is_err() {
                        return;
                    }
                }
                // Still usable without live updates: views can refresh by hand.
                Err(err) => tracing::warn!(%err, "repo file watcher failed"),
            }
            while let Ok(mut change) = rx.recv().await {
                // Coalesce whatever queued up while we were busy.
                while let Ok(more) = rx.try_recv() {
                    merge_change(&mut change, more);
                }
                if this.update(cx, |m, cx| m.on_change(change, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn on_change(&mut self, change: ChangeSet, cx: &mut Context<Self>) {
        if change.ignore_rules {
            self.refresh_ignored(cx);
        }
        self.refresh(cx);
    }

    fn refresh_ignored(&mut self, cx: &mut Context<Self>) {
        let runner = self.repo.runner().clone();
        let root = self.root().to_path_buf();
        self.ignore_task = Some(cx.spawn(async move |this, cx| {
            let ignored = cx
                .background_executor()
                .spawn(async move { ignored_dirs(&runner, &root).await })
                .await;
            _ = this.update(cx, |m, _| {
                if let Some(watcher) = &m.watcher {
                    watcher.set_ignored(ignored);
                }
            });
        }));
    }

    /// Writes / extends the commit-graph file so `git log` starts fast on big repos.
    fn ensure_commit_graph(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        self._tasks.push(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(MAINTENANCE_DELAY).await;
            let result = cx
                .background_executor()
                .spawn(async move { repo.ensure_commit_graph().await })
                .await;
            if let Err(err) = result {
                tracing::warn!(%err, "commit-graph write failed");
            }
        }));
    }

    /// Reloads status, refs, stashes and remotes; reloads the log when they moved it. A call
    /// while a refresh runs queues one more instead of killing the running one, so a stream of
    /// changes still lands a result.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            self.refresh_again = true;
            return;
        }
        self.loading = true;
        cx.notify();
        let repo = self.repo.clone();
        let job = cx.background_executor().spawn(async move {
            let status = repo.status().await?;
            // Optional: the banner only.
            let in_progress = repo.in_progress().await.unwrap_or(None);
            let refs = repo.refs().await?;
            let stashes = repo.stashes().await?;
            let remotes = repo.remotes().await?;
            Ok::<_, GitError>((status, in_progress, refs, stashes, remotes))
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |m, cx| {
                m.refresh_task = None;
                m.loading = false;
                match result {
                    Ok((status, in_progress, refs, stashes, remotes)) => {
                        m.conflicts = conflicts::conflict_files(&status);
                        m.in_progress = in_progress;
                        m.status = Some(status);
                        m.refs = refs;
                        m.stashes = stashes;
                        m.remotes = remotes;
                        m.generation += 1;
                        m.error = None;
                        m.reload_log_if_moved(cx);
                        let head = m.status.as_ref().and_then(|s| s.branch.oid.clone());
                        if m.suggested_for.as_ref() != Some(&head) {
                            m.refresh_suggestion(cx);
                        }
                    }
                    Err(err) => m.error = Some(err.to_string().into()),
                }
                if std::mem::take(&mut m.refresh_again) {
                    m.refresh(cx);
                }
                cx.notify();
            });
        }));
    }

    /// Reads the board's commit suggestion again (after a session ended or HEAD moved).
    pub fn refresh_suggestion(&mut self, cx: &mut Context<Self>) {
        self.suggested_for = Some(self.status.as_ref().and_then(|s| s.branch.oid.clone()));
        let repo = self.repo.clone();
        let job = cx
            .background_executor()
            .spawn(async move { super::suggest::load(&repo).await });
        self.suggest_task = Some(cx.spawn(async move |this, cx| {
            let suggestion = job.await;
            _ = this.update(cx, |m, cx| {
                m.suggest_task = None;
                if m.suggestion != suggestion {
                    m.suggestion = suggestion;
                    cx.notify();
                }
            });
        }));
    }

    // ---- commit log -----------------------------------------------------------------------------

    pub fn set_log_scope(&mut self, scope: LogScope, cx: &mut Context<Self>) {
        if self.log_scope != scope {
            self.log_scope = scope;
            self.log.pages = 0;
            self.reload_log_if_moved(cx);
        }
    }

    fn reload_log_if_moved(&mut self, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let snapshot = LogSnapshot::new(self.log_scope, status, &self.refs, &self.stashes);
        if self.log.loaded_for.as_ref() != Some(&snapshot) {
            self.reload_log(snapshot, cx);
        }
    }

    /// Streams the log again as many pages deep as before. The old rows stay until the first
    /// new page arrives.
    fn reload_log(&mut self, snapshot: LogSnapshot, cx: &mut Context<Self>) {
        let pages = self.log.pages.max(1);
        let pseudo = snapshot.pseudo();
        let query = snapshot.query();
        self.log.loaded_for = Some(snapshot);
        self.log.pages = 0;
        self.log.exhausted = false;
        self.log.loading = true;

        let (want_tx, want_rx) = async_channel::unbounded::<()>();
        let (page_tx, page_rx) = async_channel::bounded::<Result<Vec<Commit>, GitError>>(1);
        let repo = self.repo.clone();
        // Pulls one batch per request; between requests `git log` waits on its full pipe.
        let feed = cx.background_executor().spawn(async move {
            let mut stream = match repo.log_stream(&query) {
                Ok(stream) => stream,
                Err(err) => {
                    _ = page_tx.send(Err(err)).await;
                    return;
                }
            };
            while want_rx.recv().await.is_ok() {
                let page = match stream.next_batch().await {
                    Ok(Some(commits)) => Ok(commits),
                    Ok(None) => return,
                    Err(err) => Err(err),
                };
                let failed = page.is_err();
                if page_tx.send(page).await.is_err() || failed {
                    return;
                }
            }
        });
        for _ in 0..pages {
            _ = want_tx.try_send(());
        }
        self.log.asked = pages;
        self.log.want = Some(want_tx);
        self.log.task = Some(cx.spawn(async move |this, cx| {
            // Dropping this task drops the feed, which kills `git log`.
            let _feed = feed;
            let mut fresh = Some(History::with_pseudo(pseudo));
            while let Ok(page) = page_rx.recv().await {
                let alive = this.update(cx, |m, cx| {
                    match page {
                        Ok(commits) => {
                            match fresh.take() {
                                Some(mut history) => {
                                    history.extend(commits);
                                    m.log.history = history;
                                }
                                None => m.log.history.extend(commits),
                            }
                            m.log.pages += 1;
                            m.log.error = None;
                            m.log.loading = m.log.pages < m.log.asked;
                        }
                        Err(err) => {
                            m.log.error = Some(err.to_string().into());
                            m.log.loading = false;
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            _ = this.update(cx, |m, cx| {
                // An empty log (no commits yet) still replaces the old rows.
                if let Some(history) = fresh.take() {
                    m.log.history = history;
                }
                m.log.loading = false;
                m.log.exhausted = true;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Asks for the next page of commits (the list scrolled near its end). Does nothing while a
    /// page is loading or when the log has ended.
    pub fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.log.loading || self.log.exhausted {
            return;
        }
        if let Some(want) = &self.log.want
            && want.try_send(()).is_ok()
        {
            self.log.asked += 1;
            self.log.loading = true;
            cx.notify();
        }
    }

    // ---- operations -----------------------------------------------------------------------------

    /// Runs a user operation (one at a time), then refreshes. Its error lands in
    /// [`Self::op_error`] and as a [`RepoEvent::OpFailed`]. False when another one is running.
    pub fn run_op<F, Fut>(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>, op: F) -> bool
    where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = maomi_git::Result<()>> + Send + 'static,
    {
        if self.op_task.is_some() {
            return false;
        }
        let label: SharedString = label.into();
        self.op_error = None;
        self.op_kind = op_kind(&label);
        self.op_label = Some(label);
        let job = cx.background_executor().spawn(op(self.repo.clone()));
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |m, cx| {
                m.op_task = None;
                m.op_label = None;
                if let Err(err) = result {
                    let text: SharedString = err.to_string().into();
                    m.op_error = Some(text.clone());
                    cx.emit(RepoEvent::OpFailed(text));
                }
                m.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
        true
    }

    /// Stops the running operation (its git process is killed).
    pub fn cancel_op(&mut self, cx: &mut Context<Self>) {
        if self.op_task.take().is_some() {
            self.op_label = None;
            self.op_error = Some(CANCELLED.into());
            cx.notify();
            self.refresh(cx);
        }
    }

    pub fn clear_op_error(&mut self, cx: &mut Context<Self>) {
        if self.op_error.take().is_some() {
            cx.notify();
        }
    }

    pub fn stage(&mut self, paths: Vec<BString>, cx: &mut Context<Self>) -> bool {
        self.run_op("Staging", cx, move |repo| async move { repo.stage(&paths).await })
    }

    pub fn stage_all(&mut self, cx: &mut Context<Self>) -> bool {
        self.run_op("Staging all", cx, |repo| async move { repo.stage_all().await })
    }

    pub fn unstage(&mut self, paths: Vec<BString>, cx: &mut Context<Self>) -> bool {
        self.run_op("Unstaging", cx, move |repo| async move { repo.unstage(&paths).await })
    }

    pub fn unstage_all(&mut self, cx: &mut Context<Self>) -> bool {
        self.run_op("Unstaging all", cx, |repo| async move { repo.unstage_all().await })
    }

    /// Throws away changes to tracked files and deletes the untracked ones. The view confirms
    /// first.
    pub fn discard(
        &mut self,
        tracked: Vec<BString>,
        untracked: Vec<BString>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.run_op("Discarding", cx, move |repo| async move {
            if !tracked.is_empty() {
                repo.discard(&tracked).await?;
            }
            if !untracked.is_empty() {
                repo.delete_untracked(&untracked).await?;
            }
            Ok(())
        })
    }

    /// Commits (staging everything first when `all`), then pushes when `push`.
    pub fn commit(
        &mut self,
        req: CommitRequest,
        all: bool,
        push: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let label = if push { "Committing and pushing" } else { "Committing" };
        self.run_op(label, cx, move |repo| async move {
            if all {
                repo.stage_all().await?;
            }
            repo.commit(&req).await?;
            if push {
                repo.push(None).await?;
            }
            Ok(())
        })
    }

    pub fn fetch(&mut self, cx: &mut Context<Self>) -> bool {
        self.run_op("Fetching", cx, |repo| async move { repo.fetch(None, false).await })
    }

    pub fn pull(&mut self, mode: PullMode, cx: &mut Context<Self>) -> bool {
        self.run_op("Pulling", cx, move |repo| async move { repo.pull(mode).await })
    }

    pub fn push(&mut self, cx: &mut Context<Self>) -> bool {
        self.run_op("Pushing", cx, |repo| async move { repo.push(None).await })
    }

    /// `git stash push` of everything (untracked files too).
    pub fn stash(&mut self, message: Option<String>, cx: &mut Context<Self>) -> bool {
        self.run_op("Stashing", cx, move |repo| async move {
            repo.stash_push(message.as_deref().unwrap_or_default(), true, false, &[])
                .await
        })
    }

    /// `main · 3 changed · ↑1 ↓2`-style summary for tooltips.
    pub fn summary(&self) -> String {
        if let Some(err) = &self.error {
            return err.to_string();
        }
        let Some(status) = &self.status else {
            return "Loading…".into();
        };
        summary(status)
    }
}

fn summary(status: &Status) -> String {
    let branch = status.branch.head.as_deref().unwrap_or("detached HEAD");
    let mut out = format!("{branch} · {} changed", status.entries.len());
    if status.branch.ahead > 0 || status.branch.behind > 0 {
        out.push_str(&format!(" · ↑{} ↓{}", status.branch.ahead, status.branch.behind));
    }
    out
}

fn op_kind(label: &str) -> OpKind {
    if label.starts_with("Fetching") {
        OpKind::Fetch
    } else if label.starts_with("Pulling") {
        OpKind::Pull
    } else if label.ends_with("pushing") || label.starts_with("Pushing") {
        OpKind::Push
    } else {
        OpKind::Other
    }
}

/// The gitignored directories plus [`NOISY_DIRS`]; empty (watch everything) when git fails.
async fn ignored_dirs(runner: &GitRunner, root: &Path) -> Vec<PathBuf> {
    let mut dirs = watch::ignored_dirs(runner, root).await.unwrap_or_default();
    for dir in NOISY_DIRS {
        let dir = PathBuf::from(dir);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn merge_change(into: &mut ChangeSet, more: ChangeSet) {
    into.worktree |= more.worktree;
    into.index |= more.index;
    into.refs |= more.refs;
    into.head |= more.head;
    into.operation |= more.operation;
    into.ignore_rules |= more.ignore_rules;
    into.truncated |= more.truncated;
    into.paths.extend(more.paths);
}

#[cfg(test)]
mod tests {
    use super::*;
    use maomi_git::parse::status::BranchInfo;

    fn status(oid: &str, dirty: bool) -> Status {
        let mut s = Status {
            branch: BranchInfo {
                oid: Some(oid.into()),
                head: Some("main".into()),
                ..BranchInfo::default()
            },
            ..Status::default()
        };
        if dirty {
            // One untracked file.
            s.entries = maomi_git::parse::status::parse(b"? notes.md\0").unwrap().entries;
        }
        s
    }

    #[test]
    fn wip_row_only_when_dirty() {
        let clean = LogSnapshot::new(LogScope::All, &status("abc", false), &[], &[]);
        assert_eq!(clean.pseudo().wip_parent, None);
        let dirty = LogSnapshot::new(LogScope::All, &status("abc", true), &[], &[]);
        assert_eq!(dirty.pseudo().wip_parent.as_deref(), Some("abc"));
    }

    #[test]
    fn branch_scope_leaves_out_stashes_and_other_refs() {
        let stash = Stash {
            index: 0,
            oid: "s1".into(),
            time: 0,
            message: "WIP".into(),
        };
        let snap = LogSnapshot::new(LogScope::Branch, &status("abc", false), &[], &[stash]);
        assert!(snap.pseudo().stashes.is_empty());
        let q = snap.query();
        assert!(!q.all);
        assert!(q.stashes.is_empty());
        assert!(q.topo_order);
    }

    #[test]
    fn snapshot_changes_when_head_or_dirtiness_moves() {
        let a = LogSnapshot::new(LogScope::All, &status("abc", false), &[], &[]);
        assert_eq!(a, LogSnapshot::new(LogScope::All, &status("abc", false), &[], &[]));
        assert_ne!(a, LogSnapshot::new(LogScope::All, &status("def", false), &[], &[]));
        assert_ne!(a, LogSnapshot::new(LogScope::All, &status("abc", true), &[], &[]));
        assert_ne!(a, LogSnapshot::new(LogScope::Branch, &status("abc", false), &[], &[]));
    }

    #[test]
    fn op_kinds_follow_labels() {
        assert_eq!(op_kind("Fetching"), OpKind::Fetch);
        assert_eq!(op_kind("Pulling"), OpKind::Pull);
        assert_eq!(op_kind("Pushing"), OpKind::Push);
        assert_eq!(op_kind("Committing and pushing"), OpKind::Push);
        assert_eq!(op_kind("Staging"), OpKind::Other);
    }

    #[test]
    fn summary_shows_branch_changes_and_ahead_behind() {
        let mut s = status("abc", true);
        assert_eq!(summary(&s), "main · 1 changed");
        s.branch.ahead = 2;
        s.branch.behind = 1;
        assert_eq!(summary(&s), "main · 1 changed · ↑2 ↓1");
    }

    #[test]
    fn queued_changes_merge() {
        let mut a = ChangeSet {
            worktree: true,
            ..ChangeSet::default()
        };
        merge_change(
            &mut a,
            ChangeSet {
                refs: true,
                paths: vec!["x".into()],
                ..ChangeSet::default()
            },
        );
        assert!(a.worktree && a.refs);
        assert_eq!(a.paths, vec![PathBuf::from("x")]);
    }
}
