//! Repo-local config that names a program (filter, textconv, uploadpack) never runs from a
//! background read or a background fetch, while the same config does run for a normal write.

use std::path::Path;

use maomi_git::{GitCommand, GitRunner};

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
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = GitCommand::write(dir);
    for c in CONFIG {
        cmd = cmd.args(["-c", c]);
    }
    let out = block(runner().run(cmd.args(args))).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap()
}

/// A shell command that leaves `marker` behind, for git to run.
fn planted(marker: &Path, then: &str) -> String {
    let marker = marker.to_string_lossy().replace('\\', "/");
    format!("echo x > '{marker}'; {then}")
}

/// A committed repo whose `a.txt` uses filter and diff driver `x`, now dirty.
fn trap_repo(root: &Path, marker: &Path) {
    git(root, &["init", "-q"]);
    std::fs::write(root.join(".gitattributes"), "a.txt filter=x diff=x\n").unwrap();
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
    git(root, &["config", "filter.x.clean", &planted(marker, "cat")]);
    git(
        root,
        &["config", "diff.x.textconv", &planted(marker, "cat \"$1\"")],
    );
    std::fs::write(root.join("a.txt"), "two\n").unwrap();
}

#[test]
fn background_reads_skip_repo_filters_and_textconv() {
    let dir = tempfile::tempdir().unwrap();
    let (root, marker) = (dir.path().join("repo"), dir.path().join("PWNED"));
    std::fs::create_dir(&root).unwrap();
    trap_repo(&root, &marker);

    let read = |args: &[&str]| block(runner().run(GitCommand::read(&root).args(args)));
    read(&["status", "--porcelain=v2"]).unwrap();
    read(&["diff"]).unwrap();
    read(&["blame", "a.txt"]).unwrap();
    assert!(!marker.exists(), "a background read ran repo config");

    // Control: a normal (user) write still honours the repo's config.
    git(&root, &["add", "a.txt"]);
    assert!(
        marker.exists(),
        "control: the planted filter should run on add"
    );
}

#[test]
fn background_fetch_skips_repo_uploadpack() {
    let dir = tempfile::tempdir().unwrap();
    let (origin, root) = (dir.path().join("origin"), dir.path().join("repo"));
    let marker = dir.path().join("PWNED");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q"]);
    git(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(
        &root,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    git(
        &root,
        &["remote", "add", "second", &origin.to_string_lossy()],
    );
    let upload = planted(&marker, "git-upload-pack \"$@\"");
    git(&root, &["config", "remote.origin.uploadpack", &upload]);
    git(&root, &["config", "remote.second.uploadpack", &upload]);

    let repo = block(maomi_git::CliRepo::open(runner(), &root)).unwrap();
    block(repo.fetch_background(false)).unwrap();
    assert!(!marker.exists(), "a background fetch ran a repo uploadpack");

    git(&root, &["fetch", "origin"]);
    assert!(marker.exists(), "control: a user fetch honours uploadpack");
}

#[test]
fn askpass_env_reaches_write_commands_only() {
    let dir = tempfile::tempdir().unwrap();
    let runner = runner();
    runner.set_env(vec![("MAOMI_TEST_ASKPASS".into(), "hook".into())]);
    // A shell alias prints what a hook or filter spawned by this git would see.
    let echo = |cmd: GitCommand| {
        let cmd = cmd.args(["-c", "alias.e=!echo \"[$MAOMI_TEST_ASKPASS]\"", "e"]);
        String::from_utf8(block(runner.run(cmd)).unwrap()).unwrap()
    };
    assert_eq!(echo(GitCommand::read(dir.path())).trim(), "[]");
    assert_eq!(echo(GitCommand::write(dir.path())).trim(), "[hook]");
}
