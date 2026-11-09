//! End-to-end: CliRepo against real repositories built in a temp dir.

use std::path::Path;

use maomi_git::parse::diff::FileChange;
use maomi_git::parse::refs::RefKind;
use maomi_git::parse::status::{EntryKind, FileState};
use maomi_git::{CliRepo, DiffTarget, GitCommand, GitRepo, GitRunner, LogQuery};

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

/// Runs a setup command (user identity and fixed dates so ids are stable).
fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
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
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args);
    block(runner().run(cmd)).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// main with 2 commits, a pushed `origin` (bare), one local commit ahead, staged + unstaged +
/// untracked changes, a lightweight and an annotated tag, and one stash.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let work = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let w = work.path();
    git(remote.path(), &["init", "-q", "--bare"]);
    git(w, &["init", "-q"]);
    write(w, "a.txt", "one\ntwo\nthree\n");
    git(w, &["add", "a.txt"]);
    git(w, &["commit", "-q", "-m", "first"]);
    write(w, "b.txt", "bee\n");
    git(w, &["add", "b.txt"]);
    git(w, &["commit", "-q", "-m", "second", "-m", "body line"]);
    git(w, &["tag", "light"]);
    git(w, &["tag", "-a", "v1.0", "-m", "release"]);
    let url = remote.path().to_string_lossy().replace('\\', "/");
    git(w, &["remote", "add", "origin", &url]);
    git(w, &["push", "-q", "-u", "origin", "main"]);
    write(w, "c.txt", "sea\n");
    git(w, &["add", "c.txt"]);
    git(w, &["commit", "-q", "-m", "third"]);
    write(w, "stash.txt", "x\n");
    git(w, &["add", "stash.txt"]);
    git(w, &["stash", "push", "-q", "-m", "parked"]);
    write(w, "a.txt", "one\nTWO\nthree\n"); // unstaged
    write(w, "b.txt", "bee\nhive\n");
    git(w, &["add", "b.txt"]); // staged
    write(w, "new file.txt", "hi\n"); // untracked, with a space
    (work, remote)
}

#[test]
fn opens_from_a_subdirectory() {
    let (work, _remote) = fixture();
    std::fs::create_dir(work.path().join("sub")).unwrap();
    let repo = block(CliRepo::open(runner(), work.path().join("sub"))).unwrap();
    let expected = std::fs::canonicalize(work.path()).unwrap();
    assert_eq!(std::fs::canonicalize(repo.root()).unwrap(), expected);
}

#[test]
fn not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let err = block(CliRepo::open(runner(), dir.path())).err().unwrap();
    assert!(
        err.to_string()
            .to_lowercase()
            .contains("not a git repository"),
        "{err}"
    );
}

#[test]
fn status_refs_stash_remotes_ahead_behind() {
    let (work, _remote) = fixture();
    let repo = block(CliRepo::open(runner(), work.path())).unwrap();

    let status = block(repo.status()).unwrap();
    assert_eq!(status.branch.head.as_deref(), Some("main"));
    assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.branch.ahead, status.branch.behind), (1, 0));
    assert_eq!(status.stash_count, 1);
    let find = |p: &str| status.entries.iter().find(|e| e.path == p).unwrap();
    assert_eq!(find("a.txt").worktree, FileState::Modified);
    assert_eq!(find("a.txt").index, FileState::Unmodified);
    assert_eq!(find("b.txt").index, FileState::Modified);
    assert_eq!(find("new file.txt").kind, EntryKind::Untracked);

    let refs = block(repo.refs()).unwrap();
    let main = refs
        .iter()
        .find(|r| r.full_name == "refs/heads/main")
        .unwrap();
    assert!(main.is_head);
    assert_eq!(main.kind, RefKind::LocalBranch);
    let upstream = main.upstream.as_ref().unwrap();
    assert_eq!(
        (upstream.ahead, upstream.behind, upstream.gone),
        (1, 0, false)
    );
    let annotated = refs.iter().find(|r| r.short_name == "v1.0").unwrap();
    assert_eq!(annotated.kind, RefKind::Tag);
    assert!(annotated.peeled_oid.is_some());
    let light = refs.iter().find(|r| r.short_name == "light").unwrap();
    assert_eq!(Some(&light.oid), annotated.peeled_oid.as_ref());
    assert!(refs.iter().any(|r| r.kind == RefKind::RemoteBranch));

    let stashes = block(repo.stashes()).unwrap();
    assert_eq!(stashes.len(), 1);
    assert_eq!(stashes[0].index, 0);
    assert!(stashes[0].message.to_string().contains("parked"));

    let remotes = block(repo.remotes()).unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");
    assert!(remotes[0].fetch_url.is_some() && remotes[0].push_url.is_some());

    assert_eq!(
        block(repo.ahead_behind("main", "origin/main")).unwrap(),
        (1, 0)
    );
    assert!(block(repo.ahead_behind("--evil", "main")).is_err());
}

#[test]
fn diffs() {
    let (work, _remote) = fixture();
    let repo = block(CliRepo::open(runner(), work.path())).unwrap();

    let unstaged = block(repo.diff(DiffTarget::Unstaged { paths: vec![] })).unwrap();
    assert_eq!(unstaged.len(), 1);
    assert_eq!(unstaged[0].new_path, Some("a.txt".into()));
    assert_eq!(unstaged[0].hunks.len(), 1);

    let staged = block(repo.diff(DiffTarget::Staged { paths: vec![] })).unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].change, FileChange::Modified);

    let log = block(repo.log(LogQuery::default())).unwrap();
    assert_eq!(log.len(), 3);
    let root = log.last().unwrap();
    assert!(root.parents.is_empty());
    let root_diff = block(repo.diff(DiffTarget::Commit {
        id: root.id.clone(),
    }))
    .unwrap();
    assert_eq!(root_diff.len(), 1);
    assert_eq!(root_diff[0].change, FileChange::Added);

    let range = block(repo.diff(DiffTarget::Range {
        from: root.id.clone(),
        to: "main".into(),
    }))
    .unwrap();
    assert_eq!(range.len(), 2); // b.txt, c.txt
}

#[test]
fn log_query_and_decorations() {
    let (work, _remote) = fixture();
    let repo = block(CliRepo::open(runner(), work.path())).unwrap();
    // `--all` leaves `refs/stash` out: only main's 3 commits.
    let all = block(repo.log(LogQuery::graph())).unwrap();
    assert_eq!(all.len(), 3);
    // Stashes come in as extra tips (with their helper commits, which History hides).
    let stashes = block(repo.stashes()).unwrap();
    assert!(!stashes.is_empty());
    let with_stashes = block(repo.log(LogQuery {
        stashes: stashes.iter().map(|s| s.oid.clone()).collect(),
        ..LogQuery::graph()
    }))
    .unwrap();
    assert!(with_stashes.len() >= 4, "{}", with_stashes.len());
    assert!(with_stashes.iter().any(|c| c.id == stashes[0].oid));
    let head = &block(repo.log(LogQuery::default())).unwrap()[0];
    assert_eq!(head.subject, "third");
    assert_eq!(head.head_target.as_deref(), Some("main"));

    let limited = block(repo.log(LogQuery {
        skip: 1,
        limit: Some(1),
        ..LogQuery::default()
    }))
    .unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].subject, "second");
    assert_eq!(limited[0].body.to_string().trim(), "body line");

    let by_path = block(repo.log(LogQuery {
        paths: vec!["b.txt".into()],
        ..LogQuery::default()
    }))
    .unwrap();
    assert_eq!(by_path.len(), 1);
}

#[test]
fn log_streams_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    // 1200 commits via fast-import (each continues the branch): seconds, not minutes.
    let mut script = String::new();
    for i in 0..1200 {
        let msg = format!("commit {i}\n");
        script.push_str("commit refs/heads/main\n");
        script.push_str(&format!(
            "committer T <t@e> {} +0000\ndata {}\n{msg}\n",
            1_700_000_000 + i,
            msg.len()
        ));
    }
    block(
        runner().run(
            GitCommand::write(d)
                .args(["fast-import", "--quiet"])
                .stdin(script.into_bytes()),
        ),
    )
    .unwrap();

    let repo = block(CliRepo::open(runner(), d)).unwrap();
    let mut stream = repo.log_stream(&LogQuery::graph()).unwrap();
    let mut sizes = Vec::new();
    let mut total = 0;
    while let Some(batch) = block(stream.next_batch()).unwrap() {
        sizes.push(batch.len());
        total += batch.len();
    }
    assert_eq!(total, 1200);
    assert_eq!(sizes, vec![maomi_git::cli::LOG_BATCH; 1200 / maomi_git::cli::LOG_BATCH]);
}

#[test]
fn graph_history_from_a_streamed_log() {
    use maomi_git::graph::History;
    use maomi_git::parse::log::RefLabel;

    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    write(d, "a.txt", "1\n");
    git(d, &["add", "a.txt"]);
    git(d, &["commit", "-q", "-m", "root"]);
    git(d, &["checkout", "-q", "-b", "feature"]);
    write(d, "f.txt", "f\n");
    git(d, &["add", "f.txt"]);
    git(
        d,
        &[
            "commit",
            "-q",
            "-m",
            "feature work",
            "-m",
            "long body is not read",
        ],
    );
    git(d, &["tag", "v1"]);
    git(d, &["checkout", "-q", "main"]);
    write(d, "a.txt", "2\n");
    git(d, &["commit", "-q", "-am", "main work"]);
    git(
        d,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );

    let repo = block(CliRepo::open(runner(), d)).unwrap();
    let mut stream = repo.log_stream(&LogQuery::graph()).unwrap();
    let mut history = History::new();
    while let Some(batch) = block(stream.next_batch()).unwrap() {
        assert!(
            batch.iter().all(|c| c.body.is_empty()),
            "graph log skips bodies"
        );
        history.extend(batch);
    }

    assert_eq!(history.len(), 4);
    assert_eq!(history.head(), Some(0));
    assert_eq!(history.head_branch(), Some("main"));
    let top = history.row(0).unwrap();
    assert_eq!(top.subject, "merge feature");
    assert_eq!(top.node.column, 0);
    assert_eq!(top.edges.len(), 2, "merge opens a lane per parent");
    let root = history.row(3).unwrap();
    assert_eq!(root.subject, "root");
    assert_eq!(root.node.column, 0);
    let tagged = (0..4)
        .find(|&r| {
            history
                .row(r)
                .unwrap()
                .refs
                .contains(&RefLabel::Tag("v1".into()))
        })
        .unwrap();
    assert_eq!(history.row(tagged).unwrap().subject, "feature work");
    assert_eq!(history.row(tagged).unwrap().node.column, 1);
    assert_eq!(history.find(top.id), Some(0));
    assert_eq!(history.layout().width(), 2);
}

#[test]
fn descriptions_for_visible_rows() {
    let (work, _remote) = fixture();
    let repo = block(CliRepo::open(runner(), work.path())).unwrap();
    let ids: Vec<String> = block(repo.log(LogQuery::default()))
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    // Only "second" has a body ("body line"); the others come back without one.
    let found = block(repo.descriptions(&ids)).unwrap();
    assert_eq!(found, vec![(ids[1].clone(), "body line".to_string())]);
    assert!(block(repo.descriptions(&[])).unwrap().is_empty());
}

#[test]
fn commit_files_without_patches() {
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path();
    git(w, &["init", "-q"]);
    write(w, "keep.txt", "a\nb\nc\n");
    write(w, "gone.txt", "bye\n");
    write(
        w,
        "old name.txt",
        "same content here\nline two\nline three\n",
    );
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "base"]);
    write(w, "keep.txt", "a\nB\nc\nd\n");
    std::fs::remove_file(w.join("gone.txt")).unwrap();
    git(w, &["mv", "old name.txt", "new name.txt"]);
    write(w, "new.txt", "x\n");
    std::fs::write(w.join("img.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "changes"]);

    let repo = block(CliRepo::open(runner(), w)).unwrap();
    let head = String::from_utf8(git(w, &["rev-parse", "HEAD"])).unwrap();
    let head = head.trim();
    let files = block(repo.commit_files(head, true)).unwrap();
    let find = |p: &str| files.iter().find(|f| f.path() == p.as_bytes()).unwrap();
    assert_eq!(files.len(), 5, "{files:?}");
    let keep = find("keep.txt");
    assert_eq!(keep.change, FileChange::Modified);
    assert_eq!((keep.added, keep.removed), (Some(2), Some(1)));
    assert_eq!(find("gone.txt").change, FileChange::Deleted);
    assert_eq!(find("new.txt").change, FileChange::Added);
    let renamed = find("new name.txt");
    assert!(matches!(renamed.change, FileChange::Renamed { .. }));
    assert_eq!(
        renamed.old_path.as_ref().map(|p| p.as_slice()),
        Some(b"old name.txt".as_slice())
    );
    assert_eq!((renamed.added, renamed.removed), (Some(0), Some(0)));
    assert_eq!(find("img.bin").added, None);

    // The one file's diff, on demand.
    let diff = block(repo.commit_file_diff(head, keep)).unwrap().unwrap();
    assert_eq!(diff.hunks.len(), 1);
    let diff = block(repo.commit_file_diff(head, renamed))
        .unwrap()
        .unwrap();
    assert!(matches!(diff.change, FileChange::Renamed { .. }));

    // Root commit: against the empty tree.
    let root = String::from_utf8(git(w, &["rev-parse", "HEAD~1"])).unwrap();
    assert_eq!(
        block(repo.commit_files(root.trim(), true)).unwrap().len(),
        3
    );
    // Without counts: the same files, no numbers.
    let quick = block(repo.commit_files(head, false)).unwrap();
    assert_eq!(quick.len(), 5);
    assert!(quick.iter().all(|f| f.added.is_none()));
    assert_eq!(
        quick.iter().map(|f| f.path().to_vec()).collect::<Vec<_>>(),
        files.iter().map(|f| f.path().to_vec()).collect::<Vec<_>>()
    );
}
