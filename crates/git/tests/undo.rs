//! Undo helpers on real repositories: reset / merge go back with `reset --keep`, deleted
//! branches come back, and anything that is no longer safe is refused without changing a thing.

use std::path::Path;

use maomi_git::conflicts::MergeMode;
use maomi_git::{CliRepo, GitCommand, GitRunner, ResetMode, UndoAction};

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = GitCommand::write(dir);
    for c in [
        "user.name=Test",
        "user.email=test@example.com",
        "init.defaultBranch=main",
        "commit.gpgsign=false",
        "core.autocrlf=false",
    ] {
        cmd = cmd.args(["-c", c]);
    }
    let out = block(runner().run(cmd.args(args))).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap().trim().to_string()
}

fn commit(dir: &Path, file: &str, text: &str) -> String {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", &format!("edit {file}")]);
    git(dir, &["rev-parse", "HEAD"])
}

/// main with three commits.
fn fixture() -> (tempfile::TempDir, CliRepo, [String; 3]) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
    ] {
        git(d, &["config", key, value]);
    }
    // Whatever the default branch is called, call it main.
    git(d, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let a = commit(d, "a.txt", "a\n");
    let b = commit(d, "b.txt", "b\n");
    let c = commit(d, "c.txt", "c\n");
    let repo = block(CliRepo::open(runner(), d)).unwrap();
    (dir, repo, [a, b, c])
}

#[test]
fn head_state_reports_commit_and_branch() {
    let (dir, repo, [_, _, c]) = fixture();
    let head = block(repo.head_state()).unwrap();
    assert_eq!(head.oid.as_deref(), Some(c.as_str()));
    assert_eq!(head.branch.as_deref(), Some("main"));
    git(dir.path(), &["switch", "--detach", "HEAD"]);
    let detached = block(repo.head_state()).unwrap();
    assert_eq!(detached.oid.as_deref(), Some(c.as_str()));
    assert_eq!(detached.branch, None);
}

#[test]
fn undo_a_hard_reset_moves_the_branch_back() {
    let (dir, repo, [a, _, c]) = fixture();
    let before = block(repo.head_state()).unwrap();
    block(repo.reset(ResetMode::Hard, &a)).unwrap();
    let after = block(repo.head_state()).unwrap();
    assert_eq!(after.oid.as_deref(), Some(a.as_str()));
    assert!(!dir.path().join("c.txt").exists());

    let undo = UndoAction::MoveHeadBack {
        branch: after.branch.clone(),
        from: a.clone(),
        to: before.oid.clone().unwrap(),
    };
    block(repo.apply_undo(&undo)).unwrap();
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);
    assert!(dir.path().join("c.txt").exists());
    assert_eq!(after.branch, block(repo.head_state()).unwrap().branch);
}

#[test]
fn undo_keeps_unrelated_local_changes() {
    let (dir, repo, [a, _, c]) = fixture();
    block(repo.reset(ResetMode::Soft, &a)).unwrap();
    std::fs::write(dir.path().join("note.txt"), "mine\n").unwrap();
    let head = block(repo.head_state()).unwrap();
    let undo = UndoAction::MoveHeadBack {
        branch: head.branch,
        from: a,
        to: c.clone(),
    };
    block(repo.apply_undo(&undo)).unwrap();
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn undo_a_merge() {
    let (dir, repo, [a, _, c]) = fixture();
    git(dir.path(), &["switch", "-q", "-c", "topic", &a]);
    commit(dir.path(), "t.txt", "t\n");
    git(dir.path(), &["switch", "-q", "main"]);
    let before = block(repo.head_state()).unwrap();
    block(repo.merge("topic", MergeMode::NoFastForward)).unwrap();
    let after = block(repo.head_state()).unwrap();
    assert_ne!(after.oid, before.oid);
    assert!(dir.path().join("t.txt").exists());

    let undo = UndoAction::MoveHeadBack {
        branch: after.branch,
        from: after.oid.unwrap(),
        to: before.oid.unwrap(),
    };
    block(repo.apply_undo(&undo)).unwrap();
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);
    assert!(!dir.path().join("t.txt").exists());
}

#[test]
fn undo_is_refused_when_head_moved_again() {
    let (dir, repo, [a, b, c]) = fixture();
    let head = block(repo.head_state()).unwrap();
    let stale = UndoAction::MoveHeadBack {
        branch: head.branch.clone(),
        from: b,
        to: a.clone(),
    };
    let err = block(repo.apply_undo(&stale)).unwrap_err().to_string();
    assert!(err.contains("HEAD has moved"), "{err}");
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);

    // Wrong branch: same commit but not the recorded branch.
    let wrong_branch = UndoAction::MoveHeadBack {
        branch: Some("elsewhere".into()),
        from: c.clone(),
        to: a,
    };
    assert!(block(repo.apply_undo(&wrong_branch)).is_err());
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);
}

#[test]
fn undo_refuses_abbreviations_options_and_missing_commits() {
    let (dir, repo, [_, _, c]) = fixture();
    let head = block(repo.head_state()).unwrap();
    for to in ["HEAD~1", "--hard", &c[..7], &"0".repeat(40)] {
        let undo = UndoAction::MoveHeadBack {
            branch: head.branch.clone(),
            from: c.clone(),
            to: to.to_string(),
        };
        assert!(block(repo.apply_undo(&undo)).is_err(), "{to}");
    }
    let bad_branch = UndoAction::RestoreBranch {
        name: "--evil".into(),
        tip: c.clone(),
    };
    assert!(block(repo.apply_undo(&bad_branch)).is_err());
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);
}

#[test]
fn undo_a_branch_delete() {
    let (dir, repo, [a, _, c]) = fixture();
    git(dir.path(), &["switch", "-q", "-c", "topic", &a]);
    let tip = commit(dir.path(), "t.txt", "t\n");
    git(dir.path(), &["switch", "-q", "main"]);

    assert_eq!(block(repo.branch_tip("topic")).unwrap(), Some(tip.clone()));
    assert_eq!(block(repo.branch_tip("nope")).unwrap(), None);

    block(repo.delete_branch("topic", true)).unwrap();
    assert_eq!(block(repo.branch_tip("topic")).unwrap(), None);

    let undo = UndoAction::RestoreBranch {
        name: "topic".into(),
        tip: tip.clone(),
    };
    block(repo.apply_undo(&undo)).unwrap();
    assert_eq!(git(dir.path(), &["rev-parse", "topic"]), tip);
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), c);

    // The name is taken again: a second undo must not move the branch.
    assert!(block(repo.apply_undo(&undo)).is_err());
    assert_eq!(git(dir.path(), &["rev-parse", "topic"]), tip);
}

#[test]
fn init_with_a_named_first_branch() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("fresh");
    block(maomi_git::setup::init_with_branch(
        &runner(),
        &target,
        Some("trunk"),
    ))
    .unwrap();
    assert_eq!(git(&target, &["symbolic-ref", "--short", "HEAD"]), "trunk");
    for bad in ["--bad", "a b", "x..y", "x~"] {
        let t = dir.path().join(format!("bad-{}", bad.len()));
        assert!(
            block(maomi_git::setup::init_with_branch(&runner(), &t, Some(bad))).is_err(),
            "{bad}"
        );
    }
    // No name: git's own default, no error.
    block(maomi_git::setup::init_with_branch(
        &runner(),
        &dir.path().join("plain"),
        None,
    ))
    .unwrap();
}

#[test]
fn signing_status_reads_the_repo_config() {
    let (dir, _, _) = fixture();
    git(dir.path(), &["config", "commit.gpgsign", "true"]);
    git(dir.path(), &["config", "gpg.format", "ssh"]);
    git(dir.path(), &["config", "user.signingkey", "/keys/id.pub"]);
    let status = block(maomi_git::signing::signing_status(&runner(), dir.path())).unwrap();
    assert!(status.enabled);
    assert_eq!(status.format, "ssh");
    assert_eq!(status.key.as_deref(), Some("/keys/id.pub"));
}
