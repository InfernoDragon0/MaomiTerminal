//! End-to-end: stage / unstage / discard / partial patches / commit / stash on real repos.

use std::path::Path;

use bstr::BString;
use maomi_git::ops::{ApplyTarget, CommitRequest, StashAction};
use maomi_git::parse::status::{EntryKind, FileState};
use maomi_git::patch::{self, Direction, Pick};
use maomi_git::{CliRepo, DiffTarget, GitCommand, GitRepo, GitRunner};

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

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// One commit with `f.txt` (10 lines); identity set in the repo config so `commit` works.
fn fixture() -> (tempfile::TempDir, CliRepo) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(d, &["config", key, value]);
    }
    let lines: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    write(d, "f.txt", &lines);
    git(d, &["add", "f.txt"]);
    git(d, &["commit", "-q", "-m", "first"]);
    let repo = block(CliRepo::open(runner(), d)).unwrap();
    (dir, repo)
}

fn paths(p: &[&str]) -> Vec<BString> {
    p.iter().map(|s| BString::from(*s)).collect()
}

/// Two separate hunks: line 1 and line 10 changed.
fn two_edits(d: &Path) {
    let text: String = (1..=10)
        .map(|i| match i {
            1 => "ONE\n".to_string(),
            10 => "TEN\n".to_string(),
            _ => format!("l{i}\n"),
        })
        .collect();
    write(d, "f.txt", &text);
}

#[test]
fn stage_unstage_discard_files() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "f.txt", "changed\n");
    write(d, "new.txt", "n\n");
    block(repo.stage(&paths(&["f.txt", "new.txt"]))).unwrap();
    let st = block(repo.status()).unwrap();
    assert!(st.entries.iter().all(|e| e.index != FileState::Unmodified));

    block(repo.unstage(&paths(&["f.txt", "new.txt"]))).unwrap();
    let st = block(repo.status()).unwrap();
    assert!(st.entries.iter().all(|e| e.index == FileState::Unmodified));
    assert!(st.entries.iter().any(|e| e.kind == EntryKind::Untracked));

    block(repo.discard(&paths(&["f.txt"]))).unwrap();
    assert!(read(d, "f.txt").starts_with("l1\n"));
    block(repo.delete_untracked(&paths(&["new.txt"]))).unwrap();
    assert!(!d.join("new.txt").exists());
    assert!(block(repo.status()).unwrap().entries.is_empty());
}

#[test]
fn untracked_file_as_all_added() {
    let (dir, repo) = fixture();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    write(dir.path(), "sub/new file.txt", "a\nb\n");
    let files = block(repo.untracked_diff(&BString::from("sub/new file.txt"))).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].new_path, Some(BString::from("sub/new file.txt")));
    assert_eq!(files[0].hunks[0].lines.len(), 2);
    assert!(block(repo.untracked_diff(&BString::from("../x"))).is_err());
}

#[test]
fn capped_diffs_stop_at_whole_files() {
    let (dir, repo) = fixture();
    let d = dir.path();
    // Two tracked files of ~90 KB each, edited so the worktree diff holds both.
    let body: String = (0..5000).map(|i| format!("line number {i:05}\n")).collect();
    for name in ["a.txt", "b.txt"] {
        write(d, name, &body);
        git(d, &["add", name]);
    }
    git(d, &["commit", "-q", "-m", "two"]);
    for name in ["a.txt", "b.txt"] {
        write(d, name, &body.replace("line", "LINE"));
    }
    let target = || DiffTarget::Unstaged { paths: vec![] };
    let all = block(repo.diff_capped(target(), None)).unwrap();
    assert!(!all.truncated && all.value.len() == 2);
    // Room for the first file only: whole files from the start.
    let one = block(repo.diff_capped(target(), Some(250_000))).unwrap();
    assert!(one.truncated);
    assert_eq!(one.value.len(), 1);
    assert_eq!(one.value[0].new_path, Some(BString::from("a.txt")));
    // The first file alone is over the cap: nothing whole, but not an error either.
    let none = block(repo.diff_capped(target(), Some(1000))).unwrap();
    assert!(none.truncated && none.value.is_empty());
    // A cap above the output changes nothing.
    let roomy = block(repo.diff_capped(target(), Some(10_000_000))).unwrap();
    assert!(!roomy.truncated && roomy.value.len() == 2);

    // An untracked file: `--no-index` exits 1, which must stay fine when capped too.
    write(d, "new.txt", &body);
    let new = BString::from("new.txt");
    let cut = block(repo.untracked_diff_capped(&new, Some(1000))).unwrap();
    assert!(cut.truncated && cut.value.is_empty());
    let full = block(repo.untracked_diff_capped(&new, Some(10_000_000))).unwrap();
    assert!(!full.truncated && full.value.len() == 1);
}

#[test]
fn rejects_paths_outside_the_worktree() {
    let (_dir, repo) = fixture();
    for bad in ["../x", "/tmp/x", ""] {
        assert!(block(repo.discard(&paths(&[bad]))).is_err(), "{bad}");
    }
    assert!(block(repo.stage(&[])).is_err());
}

#[test]
fn stage_and_unstage_one_hunk() {
    let (dir, repo) = fixture();
    two_edits(dir.path());
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    assert_eq!(file.hunks.len(), 2);
    let p = patch::build(&file, &[(1, Pick::All)], Direction::Forward).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Forward)).unwrap();

    let staged = block(repo.diff(DiffTarget::Staged { paths: vec![] })).unwrap();
    assert_eq!(staged[0].hunks.len(), 1);
    assert!(staged[0].hunks[0].lines.iter().any(|l| l.content == "TEN"));
    let unstaged = block(repo.diff(DiffTarget::Unstaged { paths: vec![] })).unwrap();
    assert!(
        unstaged[0].hunks[0]
            .lines
            .iter()
            .any(|l| l.content == "ONE")
    );
    assert_eq!(unstaged[0].hunks.len(), 1);

    // And back out of the index.
    let p = patch::build(&staged[0], &[(0, Pick::All)], Direction::Reverse).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Reverse)).unwrap();
    assert!(
        block(repo.diff(DiffTarget::Staged { paths: vec![] }))
            .unwrap()
            .is_empty()
    );
    assert_eq!(read(dir.path(), "f.txt").lines().next(), Some("ONE"));
}

#[test]
fn stage_single_line_then_discard_the_rest() {
    let (dir, repo) = fixture();
    let d = dir.path();
    // l2 -> two lines: stage only the first added line.
    let text = read(d, "f.txt").replace("l2\n", "A\nB\n");
    write(d, "f.txt", &text);
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    let added = file.hunks[0]
        .lines
        .iter()
        .position(|l| l.content == "A")
        .unwrap();
    let p = patch::build(&file, &[(0, Pick::Lines(vec![added]))], Direction::Forward).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Forward)).unwrap();
    let index = git(d, &["show", ":f.txt"]);
    assert!(index.starts_with("l1\nl2\nA\nl3\n"), "{index}");

    // Discard what is left unstaged: the worktree matches the index again.
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    let p = patch::build(&file, &[(0, Pick::All)], Direction::Reverse).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Worktree, Direction::Reverse)).unwrap();
    assert_eq!(read(d, "f.txt"), index);
}

#[test]
fn commit_amend_and_trailers() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "g.txt", "g\n");
    block(repo.stage(&paths(&["g.txt"]))).unwrap();
    let req = CommitRequest {
        summary: "Add g".into(),
        body: "# not a comment".into(),
        signoff: true,
        author: Some("Ann <ann@x.io>".into()),
        co_authors: vec!["Bo <bo@x.io>".into()],
        ..Default::default()
    };
    let id = block(repo.commit(&req)).unwrap();
    assert_eq!(id.len(), 40);
    let msg = block(repo.head_message()).unwrap().unwrap();
    assert!(msg.starts_with("Add g\n\n# not a comment"), "{msg}");
    assert!(msg.contains("Co-authored-by: Bo <bo@x.io>"), "{msg}");
    assert!(
        msg.contains("Signed-off-by: Test <test@example.com>"),
        "{msg}"
    );
    assert_eq!(git(d, &["log", "-1", "--format=%an"]).trim(), "Ann");

    let amend = CommitRequest {
        summary: "Add g (amended)".into(),
        amend: true,
        ..Default::default()
    };
    block(repo.commit(&amend)).unwrap();
    assert_eq!(git(d, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert!(
        block(repo.commit(&CommitRequest::default())).is_err(),
        "empty summary"
    );
}

#[test]
fn stash_push_pop_drop() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "f.txt", "stashed\n");
    write(d, "u.txt", "u\n");
    block(repo.stash_push("wip", true, false, &[])).unwrap();
    assert!(block(repo.status()).unwrap().entries.is_empty());
    assert_eq!(block(repo.stashes()).unwrap().len(), 1);

    block(repo.stash_action(StashAction::Apply, 0)).unwrap();
    assert_eq!(read(d, "f.txt"), "stashed\n");
    assert_eq!(block(repo.stashes()).unwrap().len(), 1);
    block(repo.stash_action(StashAction::Drop, 0)).unwrap();
    assert!(block(repo.stashes()).unwrap().is_empty());

    block(repo.stash_push("", false, false, &paths(&["f.txt"]))).unwrap();
    assert!(d.join("u.txt").exists());
    block(repo.stash_action(StashAction::Pop, 0)).unwrap();
    assert_eq!(read(d, "f.txt"), "stashed\n");
}

fn head(d: &Path) -> String {
    git(d, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn graph_actions_branch_tag_switch_and_reset() {
    use maomi_git::ResetMode;

    let (dir, repo) = fixture();
    let d = dir.path();
    let first = head(d);
    write(d, "g.txt", "g\n");
    git(d, &["add", "g.txt"]);
    git(d, &["commit", "-q", "-m", "second"]);
    let second = head(d);

    // Branch at the first commit without switching, then switch to it.
    block(repo.create_branch("topic", &first, false)).unwrap();
    assert_eq!(git(d, &["rev-parse", "topic"]).trim(), first);
    assert_eq!(git(d, &["branch", "--show-current"]).trim(), "main");
    block(repo.switch_branch("topic")).unwrap();
    assert_eq!(git(d, &["branch", "--show-current"]).trim(), "topic");
    // Create-and-switch.
    block(repo.create_branch("other", &second, true)).unwrap();
    assert_eq!(head(d), second);

    // Lightweight and annotated tags.
    block(repo.create_tag("v1", &first, None)).unwrap();
    block(repo.create_tag("v2", &second, Some("Release two"))).unwrap();
    assert_eq!(git(d, &["cat-file", "-t", "v1"]).trim(), "commit");
    assert_eq!(git(d, &["cat-file", "-t", "v2"]).trim(), "tag");
    assert!(git(d, &["tag", "-n1", "v2"]).contains("Release two"));

    // Bad names never reach git as options.
    assert!(block(repo.create_branch("-x", &first, false)).is_err());
    assert!(block(repo.create_branch("a..b", &first, false)).is_err());
    assert!(block(repo.create_tag("bad tag", &first, None)).is_err());

    // Detached checkout, then back.
    block(repo.checkout_detached(&first)).unwrap();
    assert_eq!(git(d, &["branch", "--show-current"]).trim(), "");
    block(repo.switch_branch("other")).unwrap();

    // Hard reset moves the branch and worktree; soft keeps the change staged.
    block(repo.reset(ResetMode::Soft, &first)).unwrap();
    assert_eq!(head(d), first);
    assert!(git(d, &["diff", "--cached", "--name-only"]).contains("g.txt"));
    block(repo.reset(ResetMode::Hard, &second)).unwrap();
    assert_eq!(head(d), second);
    assert_eq!(read(d, "g.txt"), "g\n");
}

#[test]
fn graph_actions_cherry_pick_and_revert() {
    let (dir, repo) = fixture();
    let d = dir.path();
    git(d, &["switch", "-q", "-c", "side"]);
    write(d, "s.txt", "side\n");
    git(d, &["add", "s.txt"]);
    git(d, &["commit", "-q", "-m", "side work"]);
    let side = head(d);
    git(d, &["switch", "-q", "main"]);

    block(repo.cherry_pick(&side, false)).unwrap();
    assert_eq!(read(d, "s.txt"), "side\n");
    assert_eq!(git(d, &["log", "-1", "--format=%s"]).trim(), "side work");

    let picked = head(d);
    block(repo.revert(&picked, false)).unwrap();
    assert!(!d.join("s.txt").exists());
    assert!(git(d, &["log", "-1", "--format=%s"]).starts_with("Revert"));

    // Reverting a merge needs -m 1.
    git(d, &["switch", "-q", "-c", "feature"]);
    write(
        d, "t.txt", "feature
",
    );
    git(d, &["add", "t.txt"]);
    git(d, &["commit", "-q", "-m", "feature work"]);
    git(d, &["switch", "-q", "main"]);
    git(
        d,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    assert!(d.join("t.txt").exists());
    let merge = head(d);
    block(repo.revert(&merge, true)).unwrap();
    assert!(!d.join("t.txt").exists());
}

#[test]
fn commit_graph_is_written_then_kept_up_to_date() {
    use maomi_git::CommitGraph;
    use std::time::{Duration, SystemTime};

    let (dir, repo) = fixture();
    let d = dir.path();
    let graph = d.join(".git/objects/info/commit-graphs/commit-graph-chain");
    assert!(!graph.exists());

    assert_eq!(
        block(repo.ensure_commit_graph()).unwrap(),
        CommitGraph::Written
    );
    assert!(graph.exists());
    assert_eq!(
        block(repo.ensure_commit_graph()).unwrap(),
        CommitGraph::UpToDate
    );

    // A new commit in a pack newer than the graph (as after a fetch) adds a layer.
    write(
        d, "n.txt", "new
",
    );
    git(d, &["add", "n.txt"]);
    git(d, &["commit", "-q", "-m", "new"]);
    git(d, &["repack", "-q", "-d"]);
    let old = SystemTime::now() - Duration::from_secs(3600);
    // git writes graph files read-only.
    let mut perms = std::fs::metadata(&graph).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&graph, perms).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&graph)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(
        block(repo.ensure_commit_graph()).unwrap(),
        CommitGraph::Updated
    );
    assert_eq!(
        block(repo.ensure_commit_graph()).unwrap(),
        CommitGraph::UpToDate
    );

    // core.commitGraph=false is respected.
    git(d, &["config", "core.commitGraph", "false"]);
    assert_eq!(
        block(repo.ensure_commit_graph()).unwrap(),
        CommitGraph::Disabled
    );
}

/// Makes `link` point at the directory `target`: a symlink, or a junction on Windows when
/// symlinks need privileges.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
    }
}

#[test]
fn worktree_file_refuses_paths_through_links() {
    use maomi_git::ops::{LINKED_PATH, worktree_file};
    let dir = tempfile::tempdir().unwrap();
    let (root, outside) = (dir.path().join("repo"), dir.path().join("outside"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    assert_eq!(
        worktree_file(&root, b"src/a.rs").unwrap(),
        root.join("src").join("a.rs")
    );
    assert!(worktree_file(&root, b"new/dir/file").is_ok());
    assert!(worktree_file(&root, b"../x").is_err());
    if !link_dir(&outside, &root.join("link")) {
        eprintln!("could not create a link: skipped");
        return;
    }
    for path in [&b"link"[..], b"link/secret", b"link/new/file"] {
        match worktree_file(&root, path) {
            Err(maomi_git::GitError::InvalidArgument { what, .. }) => assert_eq!(what, LINKED_PATH),
            other => panic!("{path:?}: {other:?}"),
        }
    }
}
