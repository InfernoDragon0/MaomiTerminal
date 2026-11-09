//! Repository file watcher: turns raw filesystem events under the worktree and the git dir
//! into a coalesced [`ChangeSet`] so the app knows what to refresh (status, refs, ignore list).
//!
//! Background reads run with `--no-optional-locks`, so the app's own `git status` does not
//! rewrite `.git/index` and cannot feed back into the watcher.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bstr::ByteSlice;
use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, Debouncer, NoCache};

use crate::error::{GitError, Result};
use crate::parse::status::{self, EntryKind};
use crate::runner::{GitCommand, GitRunner};

/// Events are held back this long and delivered as one batch.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Most changed paths reported in one [`ChangeSet`].
const MAX_PATHS: usize = 64;

/// What changed since the last callback (coalesced over the debounce window).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Files in the worktree changed (not ignored, not inside .git).
    pub worktree: bool,
    /// .git/index changed (staging).
    pub index: bool,
    /// Anything under refs/ or packed-refs, FETCH_HEAD, ORIG_HEAD, the stash ref.
    pub refs: bool,
    /// HEAD moved (checkout / commit / detach).
    pub head: bool,
    /// A merge / rebase / cherry-pick / revert started or ended (MERGE_HEAD, rebase-merge/,
    /// rebase-apply/, CHERRY_PICK_HEAD, REVERT_HEAD, sequencer/).
    pub operation: bool,
    /// .gitignore / .git/info/exclude changed: the caller should refresh the ignore list.
    pub ignore_rules: bool,
    /// Up to 64 changed worktree paths relative to the root (for targeted refresh);
    /// `truncated` if more.
    pub paths: Vec<PathBuf>,
    pub truncated: bool,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        !(self.worktree
            || self.index
            || self.refs
            || self.head
            || self.operation
            || self.ignore_rules)
    }

    /// Events were lost: refresh everything.
    fn everything() -> Self {
        Self {
            worktree: true,
            index: true,
            refs: true,
            head: true,
            operation: true,
            ignore_rules: true,
            paths: Vec::new(),
            truncated: true,
        }
    }

    fn push_path(&mut self, rel: &Path) {
        if self.truncated || self.paths.iter().any(|p| p == rel) {
            return;
        }
        if self.paths.len() < MAX_PATHS {
            self.paths.push(rel.to_path_buf());
        } else {
            self.truncated = true;
        }
    }
}

/// Watches a worktree and its git dir; dropping it stops watching.
pub struct RepoWatcher {
    _debouncer: Debouncer<RecommendedWatcher, NoCache>,
    ignored: Arc<Mutex<Vec<PathBuf>>>,
}

impl RepoWatcher {
    /// `root`: worktree root. `git_dir`: absolute git dir (may be outside the worktree for
    /// linked worktrees / submodules, where `.git` is a file).
    /// `ignored`: ignored directory prefixes relative to root (from [`ignored_dirs`]); events
    /// under them are dropped.
    /// `on_change` runs on the watcher's thread, at most once per debounce window, never with
    /// an empty [`ChangeSet`].
    pub fn new(
        root: &Path,
        git_dir: &Path,
        ignored: Vec<PathBuf>,
        on_change: impl Fn(ChangeSet) + Send + 'static,
    ) -> Result<Self> {
        let layout = Layout::resolve(root, git_dir)?;
        let ignored = Arc::new(Mutex::new(clean_ignored(ignored)));

        let handler = {
            let layout = layout.clone();
            let ignored = Arc::clone(&ignored);
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let set = {
                        let ignored = ignored.lock().unwrap_or_else(PoisonError::into_inner);
                        collect(&layout, &ignored, &events)
                    };
                    if !set.is_empty() {
                        on_change(set);
                    }
                }
                Err(errors) => {
                    for err in errors {
                        tracing::warn!(%err, "file watcher error");
                    }
                }
            }
        };
        let mut debouncer =
            // No file-id cache: the default one walks the whole worktree (target/, node_modules/)
            // up front just to pair renames, which classification does not need.
            notify_debouncer_full::new_debouncer_opt(
                DEBOUNCE,
                None,
                handler,
                NoCache,
                notify::Config::default(),
            )
            .map_err(notify_error)?;

        debouncer
            .watch(&layout.root, RecursiveMode::Recursive)
            .map_err(notify_error)?;
        if strip_base(&layout.git_dir, &layout.root).is_none() {
            debouncer
                .watch(&layout.git_dir, RecursiveMode::Recursive)
                .map_err(notify_error)?;
        }
        // A linked worktree keeps its refs in the main repository's git dir. Only the parts
        // that matter are watched (not `objects/`).
        if let Some(common) = &layout.common_dir
            && strip_base(common, &layout.root).is_none()
        {
            debouncer
                .watch(common, RecursiveMode::NonRecursive)
                .map_err(notify_error)?;
            for (sub, mode) in [
                ("refs", RecursiveMode::Recursive),
                ("info", RecursiveMode::NonRecursive),
            ] {
                if let Err(err) = debouncer.watch(common.join(sub), mode) {
                    tracing::warn!(%err, sub, "could not watch part of the common git dir");
                }
            }
        }

        Ok(Self {
            _debouncer: debouncer,
            ignored,
        })
    }

    /// Replace the ignored prefixes (after `ignore_rules` changed).
    pub fn set_ignored(&self, ignored: Vec<PathBuf>) {
        *self.ignored.lock().unwrap_or_else(PoisonError::into_inner) = clean_ignored(ignored);
    }
}

/// Ignored directories (and files) git reports, relative to the root, using git's own rules.
/// Directories come back collapsed (`target/` becomes the path `target`).
pub async fn ignored_dirs(runner: &GitRunner, root: &Path) -> Result<Vec<PathBuf>> {
    let cmd = GitCommand::read(root).args([
        "status",
        "--porcelain=v2",
        "-z",
        "--ignored=matching",
        "--untracked-files=normal",
    ]);
    let out = runner.run(cmd).await?;
    let status = status::parse(&out)?;
    Ok(status
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Ignored)
        .map(|e| e.path.to_path_lossy().into_owned())
        .collect())
}

/// Absolute git dir for a worktree root (`git rev-parse --absolute-git-dir`).
pub async fn git_dir(runner: &GitRunner, root: &Path) -> Result<PathBuf> {
    let cmd = GitCommand::read(root).args(["rev-parse", "--absolute-git-dir"]);
    let out = runner.run(cmd).await?;
    Ok(out.trim().to_path_lossy().into_owned())
}

fn notify_error(err: notify::Error) -> GitError {
    GitError::Io(std::io::Error::other(err))
}

/// Canonical (no `\\?\`) locations the classifier compares event paths against.
#[derive(Debug, Clone)]
struct Layout {
    root: PathBuf,
    git_dir: PathBuf,
    /// The shared git dir of a linked worktree (`<git_dir>/commondir`), if different.
    common_dir: Option<PathBuf>,
}

impl Layout {
    fn resolve(root: &Path, git_dir: &Path) -> Result<Self> {
        let root = strip_verbatim(root.canonicalize()?);
        let git_dir = strip_verbatim(git_dir.canonicalize()?);
        let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .and_then(|rel| git_dir.join(rel.trim()).canonicalize().ok())
            .map(strip_verbatim)
            .filter(|common| *common != git_dir);
        Ok(Self {
            root,
            git_dir,
            common_dir,
        })
    }
}

/// Drops empty entries (they would match every path).
fn clean_ignored(ignored: Vec<PathBuf>) -> Vec<PathBuf> {
    ignored
        .into_iter()
        .filter(|p| p.components().next().is_some())
        .collect()
}

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\srv\share` becomes `\\srv\share`; what
/// `canonicalize` adds on Windows and `notify` never reports.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let stripped = path.to_str().and_then(|s| {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            Some(PathBuf::from(format!(r"\\{rest}")))
        } else {
            let rest = s.strip_prefix(r"\\?\")?;
            (rest.as_bytes().get(1) == Some(&b':')).then(|| PathBuf::from(rest))
        }
    });
    stripped.unwrap_or(path)
}

/// Windows paths are case-insensitive and may differ in case from what we were given.
fn same_component(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `path` relative to `base`, comparing whole components only (`target` never matches
/// `targets/x`).
fn strip_base<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
    let mut comps = path.components();
    for b in base.components() {
        let c = comps.next()?;
        if !same_component(c.as_os_str(), b.as_os_str()) {
            return None;
        }
    }
    Some(comps.as_path())
}

fn collect(layout: &Layout, ignored: &[PathBuf], events: &[DebouncedEvent]) -> ChangeSet {
    let mut set = ChangeSet::default();
    for event in events {
        classify_event(layout, ignored, &event.event, &mut set);
    }
    set
}

fn classify_event(layout: &Layout, ignored: &[PathBuf], event: &Event, set: &mut ChangeSet) {
    if event.need_rescan() {
        *set = ChangeSet::everything();
        return;
    }
    let content_change = match event.kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any => false,
        EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_)) => true,
        EventKind::Modify(ModifyKind::Name(_)) => false,
        EventKind::Modify(_) | EventKind::Access(_) | EventKind::Other => return,
    };
    // Renames carry both ends; `x.lock` -> `x` counts through its final path because a lock
    // path classifies as nothing on its own.
    for path in &event.paths {
        // Some platforms report a directory as modified when a child changes.
        if content_change && path.is_dir() {
            continue;
        }
        let path = strip_verbatim(path.clone());
        classify_path(layout, ignored, &path, set);
    }
}

fn classify_path(layout: &Layout, ignored: &[PathBuf], path: &Path, set: &mut ChangeSet) {
    if let Some(rel) = strip_base(path, &layout.git_dir) {
        classify_git(rel, true, set);
    } else if let Some(rel) = layout
        .common_dir
        .as_deref()
        .and_then(|common| strip_base(path, common))
    {
        classify_git(rel, false, set);
    } else if let Some(rel) = strip_base(path, &layout.root) {
        classify_worktree(rel, ignored, set);
    }
}

/// `rel` is relative to a git dir. `own` is false for the shared dir of a linked worktree,
/// where HEAD, the index and in-progress operations belong to another worktree.
fn classify_git(rel: &Path, own: bool, set: &mut ChangeSet) {
    let parts: Vec<&str> = rel.iter().filter_map(OsStr::to_str).collect();
    let Some((&first, _)) = parts.split_first() else {
        return;
    };
    if parts.last().is_some_and(|name| name.ends_with(".lock")) {
        return;
    }
    match first {
        // GitDragon's fetched pull requests (`refs/gitdragon/*`) are no ref the app shows.
        "refs" if parts.get(1) == Some(&"gitdragon") => {}
        "refs" | "packed-refs" | "FETCH_HEAD" => set.refs = true,
        "ORIG_HEAD" if own => set.refs = true,
        "logs" if parts == ["logs", "refs", "stash"] => set.refs = true,
        "info" if parts == ["info", "exclude"] => set.ignore_rules = true,
        "index" if own && parts.len() == 1 => set.index = true,
        "HEAD" if own && parts.len() == 1 => set.head = true,
        "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "sequencer" if own => {
            set.operation = true;
        }
        "rebase-merge" | "rebase-apply" if own => {
            set.operation = true;
            set.head = true;
        }
        _ => {}
    }
}

/// `rel` is relative to the worktree root.
fn classify_worktree(rel: &Path, ignored: &[PathBuf], set: &mut ChangeSet) {
    if rel.components().next().is_none()
        || rel.iter().any(|c| same_component(c, OsStr::new(".git")))
        || ignored.iter().any(|dir| strip_base(rel, dir).is_some())
    {
        return;
    }
    if rel
        .file_name()
        .is_some_and(|name| same_component(name, OsStr::new(".gitignore")))
    {
        set.ignore_rules = true;
    }
    set.worktree = true;
    set.push_path(rel);
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};
    use notify::{EventKind, event::AccessKind};

    use super::*;

    fn base() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\w" } else { "/w" })
    }

    fn layout() -> Layout {
        Layout {
            root: base().join("repo"),
            git_dir: base().join("repo").join(".git"),
            common_dir: None,
        }
    }

    fn linked_layout() -> Layout {
        Layout {
            root: base().join("wt"),
            git_dir: base()
                .join("main")
                .join(".git")
                .join("worktrees")
                .join("wt"),
            common_dir: Some(base().join("main").join(".git")),
        }
    }

    fn ev(kind: EventKind, paths: &[PathBuf]) -> Event {
        let mut event = Event::new(kind);
        event.paths = paths.to_vec();
        event
    }

    fn modify(path: PathBuf) -> Event {
        ev(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            &[path],
        )
    }

    fn classify(layout: &Layout, ignored: &[&str], event: &Event) -> ChangeSet {
        let ignored: Vec<PathBuf> = ignored.iter().map(PathBuf::from).collect();
        let mut set = ChangeSet::default();
        classify_event(layout, &ignored, event, &mut set);
        set
    }

    fn git_path(layout: &Layout, rel: &str) -> PathBuf {
        layout.git_dir.join(rel)
    }

    #[test]
    fn git_dir_files_map_to_flags() {
        let l = layout();
        type Expect = fn(&ChangeSet) -> bool;
        let cases: [(&str, Expect); 14] = [
            ("index", |s| s.index),
            ("HEAD", |s| s.head),
            ("refs/heads/main", |s| s.refs),
            ("refs/tags/v1", |s| s.refs),
            ("packed-refs", |s| s.refs),
            ("FETCH_HEAD", |s| s.refs),
            ("ORIG_HEAD", |s| s.refs),
            ("logs/refs/stash", |s| s.refs),
            ("MERGE_HEAD", |s| s.operation),
            ("CHERRY_PICK_HEAD", |s| s.operation),
            ("REVERT_HEAD", |s| s.operation),
            ("rebase-merge/done", |s| s.operation && s.head),
            ("sequencer/todo", |s| s.operation),
            ("info/exclude", |s| s.ignore_rules),
        ];
        for (rel, expect) in cases {
            let set = classify(&l, &[], &modify(git_path(&l, rel)));
            assert!(expect(&set), "{rel}: {set:?}");
            assert!(!set.worktree && set.paths.is_empty(), "{rel}: {set:?}");
        }
    }

    #[test]
    fn git_dir_noise_is_ignored() {
        let l = layout();
        for rel in [
            "index.lock",
            "HEAD.lock",
            "refs/heads/x.lock",
            "objects/ab/cdef",
            "logs/HEAD",
            "logs/refs/heads/main",
            "COMMIT_EDITMSG",
            "fsmonitor--daemon/cookies/1",
            "gc.pid",
            "config",
            "info/refs",
            "refs/gitdragon/pr/5/head",
            "hooks/pre-commit",
            "AUTO_MERGE",
        ] {
            let set = classify(&l, &[], &modify(git_path(&l, rel)));
            assert!(set.is_empty(), "{rel}: {set:?}");
        }
    }

    #[test]
    fn lock_files_only_count_once_renamed_into_place() {
        let l = layout();
        let rename = |from: &str, to: &str| {
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[git_path(&l, from), git_path(&l, to)],
            )
        };
        let set = classify(&l, &[], &rename("refs/heads/x.lock", "refs/heads/x"));
        assert!(set.refs && !set.index);
        let set = classify(&l, &[], &rename("index.lock", "index"));
        assert!(set.index && !set.refs);
        let set = classify(&l, &[], &rename("HEAD.lock", "HEAD"));
        assert!(set.head);
        let set = classify(
            &l,
            &[],
            &rename("refs/heads/x.lock", "refs/heads/x.lock2.lock"),
        );
        assert!(set.is_empty(), "{set:?}");
    }

    #[test]
    fn worktree_paths_are_relative_and_deduplicated() {
        let l = layout();
        let a = l.root.join("src").join("a.rs");
        let mut set = ChangeSet::default();
        for _ in 0..2 {
            classify_event(&l, &[], &modify(a.clone()), &mut set);
        }
        assert!(set.worktree && !set.ignore_rules && !set.truncated);
        assert_eq!(set.paths, [PathBuf::from("src").join("a.rs")]);
    }

    #[test]
    fn dot_git_inside_the_worktree_is_dropped() {
        let l = layout();
        // A submodule's or linked worktree's `.git` file, and unknown `.git` content.
        for rel in [".git", "sub/.git", ".git/whatever"] {
            let set = classify(&l, &[], &modify(l.root.join(rel)));
            assert!(set.is_empty(), "{rel}: {set:?}");
        }
    }

    #[test]
    fn ignored_prefixes_match_whole_components() {
        let l = layout();
        let ignored = ["target", "build/out.log"];
        let hit = |rel: &str| classify(&l, &ignored, &modify(l.root.join(rel))).worktree;
        assert!(!hit("target"));
        assert!(!hit("target/debug/x"));
        assert!(hit("targets/x"));
        assert!(hit("src/target/x"));
        assert!(!hit("build/out.log"));
        assert!(hit("build/out.log.txt"));
        assert!(hit("build/other"));
    }

    #[test]
    fn empty_ignored_entry_matches_nothing() {
        assert!(clean_ignored(vec![PathBuf::new(), PathBuf::from("a")]) == [PathBuf::from("a")]);
    }

    #[test]
    fn gitignore_anywhere_refreshes_rules_and_worktree() {
        let l = layout();
        for rel in [".gitignore", "sub/dir/.gitignore"] {
            let set = classify(&l, &[], &modify(l.root.join(rel)));
            assert!(set.ignore_rules && set.worktree, "{rel}: {set:?}");
            assert_eq!(set.paths, [PathBuf::from(rel)]);
        }
    }

    #[test]
    fn event_kinds() {
        let l = layout();
        let p = vec![l.root.join("a")];
        let create = ev(EventKind::Create(CreateKind::File), &p);
        let remove = ev(EventKind::Remove(RemoveKind::File), &p);
        let access = ev(EventKind::Access(AccessKind::Any), &p);
        let meta = ev(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            &p,
        );
        assert!(classify(&l, &[], &create).worktree);
        assert!(classify(&l, &[], &remove).worktree);
        assert!(classify(&l, &[], &access).is_empty());
        assert!(classify(&l, &[], &meta).is_empty());
    }

    #[test]
    fn directory_modify_events_are_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let l = Layout {
            root: tmp.path().to_path_buf(),
            git_dir: tmp.path().join(".git"),
            common_dir: None,
        };
        std::fs::create_dir(tmp.path().join("d")).expect("mkdir");
        let dir = tmp.path().join("d");
        assert!(classify(&l, &[], &modify(dir.clone())).is_empty());
        let create = ev(EventKind::Create(CreateKind::Folder), &[dir]);
        assert!(classify(&l, &[], &create).worktree);
    }

    #[test]
    fn rescan_flags_everything() {
        let l = layout();
        let mut event = Event::new(EventKind::Other);
        event = event.set_flag(notify::event::Flag::Rescan);
        let set = classify(&l, &[], &event);
        assert_eq!(set, ChangeSet::everything());
    }

    #[test]
    fn paths_are_capped() {
        let l = layout();
        let mut set = ChangeSet::default();
        for i in 0..(MAX_PATHS + 5) {
            classify_event(&l, &[], &modify(l.root.join(format!("f{i}"))), &mut set);
        }
        assert_eq!(set.paths.len(), MAX_PATHS);
        assert!(set.truncated && set.worktree);
    }

    #[test]
    fn linked_worktree_uses_its_own_head_and_the_shared_refs() {
        let l = linked_layout();
        let common = l.common_dir.clone().expect("common dir");
        let own = classify(&l, &[], &modify(l.git_dir.join("HEAD")));
        assert!(own.head);
        let own_index = classify(&l, &[], &modify(l.git_dir.join("index")));
        assert!(own_index.index);
        let refs = classify(
            &l,
            &[],
            &modify(common.join("refs").join("heads").join("x")),
        );
        assert!(refs.refs);
        let packed = classify(&l, &[], &modify(common.join("packed-refs")));
        assert!(packed.refs);
        // Another worktree's HEAD / index and the shared dir's noise.
        for rel in [
            "HEAD",
            "index",
            "MERGE_HEAD",
            "objects/ab/cd",
            "worktrees/o/HEAD",
        ] {
            let set = classify(&l, &[], &modify(common.join(rel)));
            assert!(set.is_empty(), "{rel}: {set:?}");
        }
        let exclude = classify(&l, &[], &modify(common.join("info").join("exclude")));
        assert!(exclude.ignore_rules);
    }

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\Volume{1}\x")),
            PathBuf::from(r"\\?\Volume{1}\x")
        );
        assert_eq!(strip_verbatim(PathBuf::from("rel")), PathBuf::from("rel"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_case_insensitively() {
        let l = layout();
        let set = classify(
            &l,
            &["Target"],
            &modify(PathBuf::from(r"c:\W\REPO\target\x")),
        );
        assert!(set.is_empty(), "{set:?}");
        let set = classify(&l, &[], &modify(PathBuf::from(r"c:\W\REPO\.GIT\INDEX")));
        assert!(set.is_empty(), "{set:?}");
        let set = classify(&l, &[], &modify(PathBuf::from(r"c:\w\Repo\src\a.rs")));
        assert!(set.worktree);
    }
}
