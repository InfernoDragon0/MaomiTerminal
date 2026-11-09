//! End-to-end: merge / rebase / cherry-pick conflicts, resolving, continue / skip / abort, and
//! the editor-free interactive rebase on real repos.

use std::path::Path;

use maomi_git::conflicts::{
    self, Choice, ConflictKind, ConflictText, MergeMode, Operation, Side, TodoAction, TodoItem,
};
use maomi_git::{CliRepo, GitCommand, GitRepo, GitRunner};

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

const CONFIG: &[&str] = &[
    "user.name=Test",
    "user.email=test@example.com",
    "init.defaultBranch=main",
    "core.autocrlf=false",
    "commit.gpgsign=false",
    "merge.conflictStyle=diff3",
];

fn git(dir: &Path, args: &[&str]) -> String {
    let out = block(runner().run(GitCommand::write(dir).args(args)))
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap()
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn commit(d: &Path, name: &str, text: &str, message: &str) {
    write(d, name, text);
    git(d, &["add", "--", name]);
    git(d, &["commit", "-q", "-m", message]);
}

/// `main` and `feature` both change line 2 of `f.txt`; `feature` also deletes `gone.txt`,
/// which `main` edits.
fn fixture() -> (tempfile::TempDir, CliRepo) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(d, &["config", key, value]);
    }
    write(d, "gone.txt", "keep me\n");
    git(d, &["add", "gone.txt"]);
    commit(d, "f.txt", "one\ntwo\nthree\n", "base");
    git(d, &["switch", "-q", "-c", "feature"]);
    git(d, &["rm", "-q", "gone.txt"]);
    commit(d, "f.txt", "one\nTWO (feature)\nthree\n", "feature edit");
    git(d, &["switch", "-q", "main"]);
    write(d, "gone.txt", "edited on main\n");
    git(d, &["add", "gone.txt"]);
    commit(d, "f.txt", "one\nTWO (main)\nthree\n", "main edit");
    let repo = block(CliRepo::open(runner(), d)).unwrap();
    (dir, repo)
}

fn conflicts_of(repo: &CliRepo) -> Vec<(String, ConflictKind)> {
    let status = block(repo.status()).unwrap();
    conflicts::conflict_files(&status)
        .into_iter()
        .map(|c| (c.path.to_string(), c.kind))
        .collect()
}

#[test]
fn merge_conflict_resolve_and_continue() {
    let (dir, repo) = fixture();
    let d = dir.path();
    // Stopping for conflicts is not an error.
    block(repo.merge("feature", MergeMode::Default)).unwrap();
    let state = block(repo.in_progress())
        .unwrap()
        .expect("merge in progress");
    assert_eq!(state.op, Operation::Merge);
    assert_eq!(state.message.as_deref(), Some("Merge branch 'feature'"));
    let mut files = conflicts_of(&repo);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        files,
        [
            ("f.txt".to_string(), ConflictKind::BothModified),
            ("gone.txt".to_string(), ConflictKind::DeletedByThem),
        ]
    );

    let text = ConflictText::parse(&block(repo.read_worktree_file(b"f.txt")).unwrap());
    let conflict = text.blocks().next().unwrap();
    assert_eq!(conflict.ours, b"TWO (main)\n");
    assert_eq!(conflict.base.as_deref(), Some(&b"two\n"[..]));
    assert_eq!(conflict.theirs, b"TWO (feature)\n");

    let stages = block(repo.conflict_stages(b"gone.txt")).unwrap();
    assert_eq!(stages.ours.as_deref(), Some(&b"edited on main\n"[..]));
    assert_eq!(stages.theirs, None);

    let resolved = text.resolve(&[Some(Choice::Both)]);
    block(repo.resolve_with(b"f.txt", &resolved)).unwrap();
    block(repo.take_side(b"gone.txt", Side::Theirs)).unwrap();
    assert!(conflicts_of(&repo).is_empty());
    assert!(!d.join("gone.txt").exists());

    block(repo.continue_operation(Operation::Merge)).unwrap();
    assert_eq!(block(repo.in_progress()).unwrap(), None);
    assert_eq!(read(d, "f.txt"), "one\nTWO (main)\nTWO (feature)\nthree\n");
    let parents = git(d, &["log", "-1", "--format=%P"]);
    assert_eq!(parents.split_whitespace().count(), 2, "a merge commit");
    assert_eq!(
        git(d, &["log", "-1", "--format=%s"]).trim(),
        "Merge branch 'feature'"
    );
}

#[test]
fn merge_abort_and_real_failures() {
    let (dir, repo) = fixture();
    let d = dir.path();
    let head = git(d, &["rev-parse", "HEAD"]);
    block(repo.merge("feature", MergeMode::NoFastForward)).unwrap();
    block(repo.abort_operation(Operation::Merge)).unwrap();
    assert_eq!(block(repo.in_progress()).unwrap(), None);
    assert_eq!(git(d, &["rev-parse", "HEAD"]), head);
    // Not a stop: a failure (fast-forward impossible) is still an error.
    assert!(block(repo.merge("feature", MergeMode::FastForwardOnly)).is_err());
    assert!(block(repo.merge("--evil", MergeMode::Default)).is_err());
    assert!(block(repo.skip_operation(Operation::Merge)).is_err());
}

#[test]
fn rebase_conflict_take_side_and_continue() {
    let (dir, repo) = fixture();
    let d = dir.path();
    git(d, &["switch", "-q", "feature"]);
    block(repo.rebase("main")).unwrap();
    let state = block(repo.in_progress())
        .unwrap()
        .expect("rebase in progress");
    assert_eq!(state.op, Operation::Rebase);
    assert_eq!(state.branch.as_deref(), Some("feature"));
    assert_eq!(state.step, Some((1, 1)));
    // During a rebase "ours" is the branch being rebased onto.
    block(repo.take_side(b"f.txt", Side::Theirs)).unwrap();
    block(repo.take_side(b"gone.txt", Side::Theirs)).unwrap();
    block(repo.continue_operation(Operation::Rebase)).unwrap();
    assert_eq!(block(repo.in_progress()).unwrap(), None);
    assert_eq!(read(d, "f.txt"), "one\nTWO (feature)\nthree\n");
    assert_eq!(git(d, &["log", "-1", "--format=%s"]).trim(), "feature edit");
    let base = git(d, &["merge-base", "HEAD", "main"]);
    assert_eq!(base, git(d, &["rev-parse", "main"]), "rebased onto main");
}

#[test]
fn cherry_pick_conflict_and_skip() {
    let (dir, repo) = fixture();
    let d = dir.path();
    let head = git(d, &["rev-parse", "HEAD"]);
    block(repo.cherry_pick("feature", false)).unwrap();
    let state = block(repo.in_progress()).unwrap().unwrap();
    assert_eq!(state.op, Operation::CherryPick);
    assert_eq!(
        state.incoming.as_deref(),
        Some(git(d, &["rev-parse", "feature"]).trim())
    );
    block(repo.skip_operation(Operation::CherryPick)).unwrap();
    assert_eq!(block(repo.in_progress()).unwrap(), None);
    assert_eq!(git(d, &["rev-parse", "HEAD"]), head);
}

#[test]
fn interactive_rebase_reorders_rewords_squashes_and_drops() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(d, &["config", key, value]);
    }
    commit(d, "base.txt", "base\n", "base");
    for name in ["a", "b", "c", "d"] {
        commit(d, &format!("{name}.txt"), name, name);
    }
    let repo = block(CliRepo::open(runner(), d)).unwrap();
    let base = git(d, &["rev-parse", "HEAD~4"]).trim().to_string();
    let plan = block(repo.rebase_plan(Some(&base))).unwrap();
    assert_eq!(plan.merges, 0);
    let subjects: Vec<&str> = plan.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["a", "b", "c", "d"]);
    let id = |i: usize| plan.commits[i].id.clone();
    let items = [
        TodoItem {
            commit: id(2),
            action: TodoAction::Reword("C, reworded\n\nwith a body; $(not run) 'quoted'".into()),
        },
        TodoItem {
            commit: id(0),
            action: TodoAction::Pick,
        },
        TodoItem {
            commit: id(3),
            action: TodoAction::Fixup,
        },
        TodoItem {
            commit: id(1),
            action: TodoAction::Drop,
        },
    ];
    block(repo.interactive_rebase(Some(&base), &items)).unwrap();
    assert_eq!(block(repo.in_progress()).unwrap(), None);
    let log = git(d, &["log", "--format=%s", &format!("{base}..HEAD")]);
    assert_eq!(log.lines().collect::<Vec<_>>(), ["a", "C, reworded"]);
    let body = git(d, &["log", "-1", "--format=%B", "HEAD~1"]);
    assert_eq!(
        body.trim(),
        "C, reworded\n\nwith a body; $(not run) 'quoted'"
    );
    assert!(!d.join("b.txt").exists(), "dropped");
    assert!(d.join("d.txt").exists(), "fixed up into a");
}
