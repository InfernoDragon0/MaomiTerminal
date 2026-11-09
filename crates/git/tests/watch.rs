//! RepoWatcher against real repositories on the real filesystem.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use maomi_git::watch::{ChangeSet, RepoWatcher, git_dir, ignored_dirs};
use maomi_git::{GitCommand, GitRunner};

const WAIT: Duration = Duration::from_secs(4);
/// Long enough for a debounce window plus platform latency to prove nothing arrives.
const QUIET: Duration = Duration::from_millis(700);

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

fn git(dir: &Path, args: &[&str]) {
    let cmd = GitCommand::write(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args);
    block(runner().run(cmd)).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A repo with one commit, a `.gitignore` for `target/`, and a file inside `target/`.
fn repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let w = tmp.path();
    git(w, &["init", "-q"]);
    write(w, ".gitignore", "target/\n");
    write(w, "a.txt", "one\n");
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "first"]);
    write(w, "target/out.bin", "x");
    tmp
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<ChangeSet>>>);

impl Seen {
    fn sink(&self) -> impl Fn(ChangeSet) + Send + 'static {
        let seen = Arc::clone(&self.0);
        move |set| {
            assert!(!set.is_empty(), "callback got an empty ChangeSet");
            seen.lock().expect("lock").push(set);
        }
    }

    fn all(&self) -> Vec<ChangeSet> {
        self.0.lock().expect("lock").clone()
    }

    fn clear(&self) {
        self.0.lock().expect("lock").clear();
    }

    /// Waits until some delivered ChangeSet satisfies `pred`.
    fn wait(&self, what: &str, pred: impl Fn(&ChangeSet) -> bool) -> ChangeSet {
        let start = Instant::now();
        loop {
            if let Some(set) = self.all().into_iter().find(&pred) {
                return set;
            }
            assert!(
                start.elapsed() < WAIT,
                "timed out waiting for {what}; saw {:?}",
                self.all()
            );
            sleep(Duration::from_millis(25));
        }
    }

    /// Asserts nothing matching `pred` arrives for a while.
    fn expect_none(&self, what: &str, pred: impl Fn(&ChangeSet) -> bool) {
        sleep(QUIET);
        let hits: Vec<_> = self.all().into_iter().filter(pred).collect();
        assert!(hits.is_empty(), "unexpected {what}: {hits:?}");
    }
}

fn start(root: &Path, ignored: Vec<PathBuf>) -> (RepoWatcher, Seen) {
    let dir = block(git_dir(&runner(), root)).expect("git dir");
    let seen = Seen::default();
    let watcher = RepoWatcher::new(root, &dir, ignored, seen.sink()).expect("watcher");
    sleep(Duration::from_millis(300));
    seen.clear();
    (watcher, seen)
}

fn ignored(root: &Path) -> Vec<PathBuf> {
    block(ignored_dirs(&runner(), root)).expect("ignored dirs")
}

#[test]
fn worktree_edit_reports_the_path() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    write(tmp.path(), "src/new.rs", "fn main() {}\n");
    let set = seen.wait("worktree change", |s| s.worktree);
    let expected = PathBuf::from("src").join("new.rs");
    assert!(set.paths.contains(&expected), "{set:?}");
    assert!(!set.index && !set.refs && !set.head);
}

#[test]
fn staging_changes_the_index() {
    let tmp = repo();
    write(tmp.path(), "b.txt", "bee\n");
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    git(tmp.path(), &["add", "b.txt"]);
    seen.wait("index change", |s| s.index);
}

#[test]
fn commit_moves_refs() {
    let tmp = repo();
    write(tmp.path(), "b.txt", "bee\n");
    git(tmp.path(), &["add", "b.txt"]);
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    git(tmp.path(), &["commit", "-q", "-m", "second"]);
    seen.wait("refs change", |s| s.refs);
}

#[test]
fn switching_branches_moves_head() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    git(tmp.path(), &["switch", "-q", "-c", "topic"]);
    // The two can land in different debounce windows.
    seen.wait("head change", |s| s.head);
    seen.wait("refs change", |s| s.refs);
}

#[test]
fn branch_create_reports_refs_only() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    git(tmp.path(), &["branch", "feature"]);
    seen.wait("refs change", |s| s.refs);
    assert!(
        seen.all()
            .iter()
            .all(|s| !s.worktree && !s.head && !s.index)
    );
}

#[test]
fn merge_start_and_end_are_operations() {
    let tmp = repo();
    git(tmp.path(), &["switch", "-q", "-c", "topic"]);
    write(tmp.path(), "a.txt", "topic\n");
    git(tmp.path(), &["commit", "-q", "-am", "topic"]);
    git(tmp.path(), &["switch", "-q", "main"]);
    write(tmp.path(), "a.txt", "main\n");
    git(tmp.path(), &["commit", "-q", "-am", "main"]);
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    let cmd = GitCommand::write(tmp.path()).args(["merge", "topic"]);
    assert!(block(runner().run(cmd)).is_err(), "expected a conflict");
    seen.wait("operation start", |s| s.operation);
    seen.clear();
    git(tmp.path(), &["merge", "--abort"]);
    seen.wait("operation end", |s| s.operation);
}

#[test]
fn ignored_directories_are_silent() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    write(tmp.path(), "target/debug/out.bin", "y");
    write(tmp.path(), "target/out.bin", "z");
    seen.expect_none("worktree event for an ignored dir", |s| s.worktree);
    // Sanity: the watcher is alive.
    write(tmp.path(), "a.txt", "two\n");
    seen.wait("worktree change", |s| s.worktree);
}

#[test]
fn gitignore_edit_refreshes_the_rules() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    write(tmp.path(), ".gitignore", "target/\n*.log\n");
    let set = seen.wait("ignore rules", |s| s.ignore_rules);
    assert!(set.worktree);
    assert!(set.paths.contains(&PathBuf::from(".gitignore")), "{set:?}");
}

#[test]
fn info_exclude_edit_refreshes_the_rules() {
    let tmp = repo();
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    write(&tmp.path().join(".git"), "info/exclude", "*.tmp\n");
    let set = seen.wait("ignore rules", |s| s.ignore_rules);
    assert!(!set.worktree);
}

#[test]
fn set_ignored_takes_effect() {
    let tmp = repo();
    let (w, seen) = start(tmp.path(), Vec::new());
    write(tmp.path(), "target/a.bin", "1");
    seen.wait("worktree change while not ignored", |s| s.worktree);
    sleep(QUIET);
    seen.clear();
    w.set_ignored(vec![PathBuf::from("target")]);
    write(tmp.path(), "target/b.bin", "2");
    seen.expect_none("worktree event after set_ignored", |s| s.worktree);
}

#[test]
fn ignored_dirs_collapses_directories() {
    let tmp = repo();
    write(tmp.path(), "target/deep/x.bin", "x");
    write(tmp.path(), "keep.txt", "k");
    let dirs = ignored(tmp.path());
    assert_eq!(dirs, [PathBuf::from("target")]);
}

#[test]
fn git_dir_is_absolute() {
    let tmp = repo();
    let dir = block(git_dir(&runner(), tmp.path())).expect("git dir");
    assert!(dir.is_absolute(), "{dir:?}");
    assert!(dir.join("HEAD").is_file(), "{dir:?}");
}

#[test]
fn background_status_does_not_touch_the_index() {
    let tmp = repo();
    write(tmp.path(), "b.txt", "untracked\n");
    write(tmp.path(), "a.txt", "modified\n");
    let (_w, seen) = start(tmp.path(), ignored(tmp.path()));
    for _ in 0..3 {
        let cmd = GitCommand::read(tmp.path()).args(["status", "--porcelain=v2", "-z"]);
        block(runner().run(cmd)).expect("status");
    }
    seen.expect_none("index/refs/head event from a read-only status", |s| {
        s.index || s.refs || s.head
    });
}

#[test]
fn linked_worktree_sees_shared_refs_and_its_own_files() {
    let main = repo();
    let holder = tempfile::tempdir().expect("tempdir");
    let linked = holder.path().join("wt");
    let linked_arg = linked.to_str().expect("utf8");
    git(
        main.path(),
        &["worktree", "add", "-q", "-b", "side", linked_arg],
    );
    let (_w, seen) = start(&linked, Vec::new());
    git(main.path(), &["branch", "from-main"]);
    seen.wait("shared refs change", |s| s.refs);
    write(&linked, "w.txt", "w\n");
    let set = seen.wait("worktree change", |s| s.worktree);
    assert!(set.paths.contains(&PathBuf::from("w.txt")), "{set:?}");
    git(&linked, &["add", "w.txt"]);
    seen.wait("index change", |s| s.index);
}
