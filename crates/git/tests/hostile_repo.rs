//! Opening a hostile repository in the Repo dock must not run its programs. Derived from
//! `tools/git-guard-poc.sh`: a submodule's clean filter (reached through `git status`) and a
//! promisor remote's uploadpack (reached through a lazy fetch in `for-each-ref`, `log` and the
//! commit-graph write). Each payload only leaves a marker file. The control runs at the end
//! prove the traps fire on this machine's git when the guard is bypassed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use maomi_git::parse::status;
use maomi_git::{CliRepo, GitCommand, GitRepo as _, GitRunner, LogQuery, watch};

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

/// Runs a setup command with a fixed identity.
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

/// A shell command that creates `marker`; git runs it through its own `sh`.
fn payload(marker: &Path, then: &str) -> String {
    let marker = marker.to_string_lossy().replace('\\', "/");
    format!("echo x > '{marker}'; {then}")
}

/// Takes the marker away so the next check starts clean; true when it was there.
fn fired(marker: &Path) -> bool {
    let ran = marker.exists();
    if ran {
        std::fs::remove_file(marker).unwrap();
    }
    ran
}

/// Moves `file`'s mtime so git must re-hash it (through the clean filter) on the next status.
fn touch(file: &Path, step: &mut u64) {
    *step += 10;
    std::fs::File::options()
        .write(true)
        .open(file)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(*step))
        .unwrap();
}

/// A superproject whose submodule `sub` has a clean filter in its own config.
fn submodule_trap(root: &Path, marker: &Path) -> PathBuf {
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(root, &["init", "-q"]);
    git(&sub, &["init", "-q"]);
    std::fs::write(sub.join("f.txt"), "hi\n").unwrap();
    std::fs::write(sub.join(".gitattributes"), "* filter=x\n").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-q", "-m", "init"]);
    std::fs::write(
        root.join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n",
    )
    .unwrap();
    git(root, &["add", ".gitmodules", "sub"]);
    git(root, &["commit", "-q", "-m", "super"]);
    git(&sub, &["config", "filter.x.clean", &payload(marker, "cat")]);
    sub.join("f.txt")
}

/// A partial clone whose promisor remote names a payload as its uploadpack, and a branch that
/// points at a commit nobody has, so reading it asks the promisor.
fn promisor_trap(root: &Path, marker: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("f.txt"), "hi\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
    let nowhere = root.with_file_name("nowhere");
    let nowhere = nowhere.to_string_lossy().replace('\\', "/");
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.url", nowhere.as_str()),
        // A repo can't lift the transport ban this way either.
        ("protocol.file.allow", "always"),
    ] {
        git(root, &["config", key, value]);
    }
    git(
        root,
        &["config", "remote.origin.uploadpack", &payload(marker, "#")],
    );
    std::fs::write(
        root.join(".git").join("refs").join("heads").join("ghost"),
        "1234567890123456789012345678901234567890\n",
    )
    .unwrap();
}

#[test]
fn submodule_filters_never_run_from_the_dock() {
    let dir = tempfile::tempdir().unwrap();
    let (root, marker) = (dir.path().join("super"), dir.path().join("PWNED"));
    let file = submodule_trap(&root, &marker);
    let mut step = 0;
    let repo = block(CliRepo::open(runner(), &root)).unwrap();

    touch(&file, &mut step);
    block(repo.status()).unwrap();
    assert!(!fired(&marker), "status ran a submodule's clean filter");

    touch(&file, &mut step);
    let ignored = block(watch::ignored_dirs(repo.runner(), repo.root())).unwrap();
    assert!(
        !fired(&marker),
        "ignored_dirs ran a submodule's clean filter"
    );
    assert!(ignored.contains(&PathBuf::from("sub")), "{ignored:?}");

    // Control: the same read with git's default submodule check runs the filter.
    touch(&file, &mut step);
    let control = GitCommand::read(repo.root())
        .arg("status")
        .args(status::ARGS)
        .arg("--ignore-submodules=none");
    _ = block(repo.runner().output(control));
    assert!(fired(&marker), "control: the submodule trap never fired");
}

#[test]
fn promisor_uploadpack_never_runs_from_the_dock() {
    let dir = tempfile::tempdir().unwrap();
    let (root, marker) = (dir.path().join("pc"), dir.path().join("PWNED"));
    promisor_trap(&root, &marker);
    let repo = block(CliRepo::open(runner(), &root)).unwrap();

    // Errors are fine (the ghost commit is missing); only the payload matters.
    _ = block(repo.status());
    assert!(!fired(&marker), "status ran the promisor's uploadpack");
    _ = block(repo.refs());
    assert!(
        !fired(&marker),
        "for-each-ref ran the promisor's uploadpack"
    );
    _ = block(repo.stashes());
    _ = block(repo.remotes());
    assert!(
        !fired(&marker),
        "stash list / remotes ran the promisor's uploadpack"
    );
    _ = block(repo.log(LogQuery::graph()));
    assert!(!fired(&marker), "log --all ran the promisor's uploadpack");
    let mut stream = repo.log_stream(&LogQuery::graph()).unwrap();
    while let Ok(Some(_)) = block(stream.next_batch()) {}
    drop(stream);
    assert!(
        !fired(&marker),
        "the streamed log ran the promisor's uploadpack"
    );
    _ = block(repo.ensure_commit_graph());
    assert!(
        !fired(&marker),
        "the commit-graph write ran the promisor's uploadpack"
    );

    // Control: an unguarded command (as a user's own `git` would) runs the payload.
    let control = GitCommand::write(repo.root())
        .args(["for-each-ref", "--format=%(objectname) %(contents:subject)"]);
    _ = block(repo.runner().output(control));
    assert!(fired(&marker), "control: the promisor trap never fired");
}
