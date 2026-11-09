//! End-to-end: branch / remote / sync operations against a bare remote in a temp dir.

use std::path::Path;

use maomi_git::{CliRepo, GitCommand, GitRepo, GitRunner, PullMode, PushOptions};

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
    "tag.gpgsign=false",
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = GitCommand::write(dir);
    for c in CONFIG {
        cmd = cmd.args(["-c", c]);
    }
    let out = block(runner().run(cmd.args(args))).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap()
}

fn commit(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", file]);
}

fn rev(dir: &Path, r: &str) -> String {
    git(dir, &["rev-parse", r]).trim().to_string()
}

/// A bare remote, and a work repo with one commit and identity in its config.
fn setup() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    CliRepo,
) {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("remote.git");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(root.path(), &["init", "-q", "--bare", "remote.git"]);
    git(&work, &["init", "-q"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(&work, &["config", key, value]);
    }
    commit(&work, "a.txt", "a\n");
    let repo = block(CliRepo::open(runner(), &work)).unwrap();
    (root, bare, work, repo)
}

fn url(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[test]
fn remotes_push_fetch_pull_and_upstream() {
    let (root, bare, work, repo) = setup();

    block(repo.add_remote("origin", &url(&bare))).unwrap();
    assert!(block(repo.add_remote("bad name", "x")).is_err());
    assert!(block(repo.add_remote("-x", "x")).is_err());
    assert!(block(repo.add_remote("ok", "--upload-pack=evil")).is_err());

    let opts = PushOptions {
        set_upstream: true,
        force_with_lease: false,
    };
    block(repo.push_branch("origin", "main", opts)).unwrap();
    assert_eq!(rev(&bare, "main"), rev(&work, "HEAD"));
    let refs = block(repo.refs()).unwrap();
    let main = refs
        .iter()
        .find(|r| r.full_name == "refs/heads/main")
        .unwrap();
    assert_eq!(
        main.upstream.as_ref().map(|u| u.name.as_str()),
        Some("refs/remotes/origin/main")
    );

    // Someone else pushes; fetch sees it, pull --ff-only brings it in.
    let other = root.path().join("other");
    git(root.path(), &["clone", "-q", &url(&bare), "other"]);
    commit(&other, "b.txt", "b\n");
    git(&other, &["push", "-q", "origin", "main"]);
    block(repo.fetch(Some("origin"), true)).unwrap();
    assert_eq!(rev(&work, "origin/main"), rev(&other, "HEAD"));
    block(repo.pull(PullMode::FastForwardOnly)).unwrap();
    assert_eq!(rev(&work, "HEAD"), rev(&other, "HEAD"));

    // Rewritten local history needs a force push; the lease protects remote work we haven't seen.
    git(&work, &["commit", "-q", "--amend", "-m", "rewritten"]);
    assert!(block(repo.push_branch("origin", "main", PushOptions::default())).is_err());
    let lease = PushOptions {
        set_upstream: false,
        force_with_lease: true,
    };
    block(repo.push_branch("origin", "main", lease)).unwrap();
    assert_eq!(rev(&bare, "main"), rev(&work, "HEAD"));
    // Remote moves behind our back (no fetch): the lease refuses.
    commit(&other, "c.txt", "c\n");
    git(&other, &["push", "-q", "--force", "origin", "main"]);
    git(&work, &["commit", "-q", "--amend", "-m", "again"]);
    assert!(block(repo.push_branch("origin", "main", lease)).is_err());

    // Upstream off and on again.
    block(repo.set_upstream("main", None)).unwrap();
    let upstream_of_main = || {
        block(repo.refs())
            .unwrap()
            .into_iter()
            .find(|r| r.full_name == "refs/heads/main")
            .and_then(|r| r.upstream)
    };
    assert_eq!(upstream_of_main(), None);
    block(repo.set_upstream("main", Some("origin/main"))).unwrap();
    assert_eq!(
        git(&work, &["rev-parse", "--abbrev-ref", "main@{upstream}"]).trim(),
        "origin/main"
    );

    // Edit / rename / remove remotes.
    block(repo.set_remote_url("origin", &url(&bare))).unwrap();
    block(repo.rename_remote("origin", "upstream")).unwrap();
    assert!(git(&work, &["remote"]).contains("upstream"));
    block(repo.remove_remote("upstream")).unwrap();
    assert!(git(&work, &["remote"]).trim().is_empty());
}

#[test]
fn branches_and_tags() {
    let (_root, bare, work, repo) = setup();
    block(repo.add_remote("origin", &url(&bare))).unwrap();

    git(&work, &["branch", "topic"]);
    block(repo.rename_branch("topic", "feature/x")).unwrap();
    assert!(git(&work, &["branch"]).contains("feature/x"));

    // An unmerged branch needs force to delete.
    git(&work, &["switch", "-q", "feature/x"]);
    commit(&work, "f.txt", "f\n");
    git(&work, &["switch", "-q", "main"]);
    assert!(block(repo.delete_branch("feature/x", false)).is_err());
    block(repo.delete_branch("feature/x", true)).unwrap();
    assert!(!git(&work, &["branch"]).contains("feature/x"));

    git(&work, &["tag", "v1"]);
    block(repo.push_tag("origin", "v1")).unwrap();
    assert_eq!(rev(&bare, "v1"), rev(&work, "v1"));
    block(repo.delete_remote_tag("origin", "v1")).unwrap();
    assert!(git(&bare, &["tag"]).trim().is_empty());
    block(repo.delete_tag("v1")).unwrap();
    assert!(git(&work, &["tag"]).trim().is_empty());

    assert!(block(repo.submodules()).unwrap().is_empty());
}
