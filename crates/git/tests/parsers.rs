//! Fixture-based parser tests. Fixtures are real git output from `fixtures/make-fixtures.sh`.

use bstr::{BString, ByteSlice};
use maomi_git::parse::diff::{self, FileChange, FileDiff, LineKind};
use maomi_git::parse::log::{self, LogParser, RefLabel};
use maomi_git::parse::refs::{self, RefKind};
use maomi_git::parse::status::{self, EntryKind, FileState};

const MERGE: &str = "31527adb4c5a860f20f8efeb3f0b6ea335dd361a";
const MAIN: &str = "bdd6f1205556643d75b0b7b6e7cf878c4cd2e187";
const FEATURE: &str = "c60e5d185088976b297d08ba2c1813f3cc0fa170";
const STASH: &str = "b38c3d0b53baac668440fa731a2f43d5f5233dad";

fn lines(args: &str) -> Vec<String> {
    args.lines().map(str::to_owned).collect()
}

#[test]
fn fixture_args_match_parsers() {
    assert_eq!(lines(include_str!("fixtures/args-log.txt")), log::args());
    assert_eq!(
        lines(include_str!("fixtures/args-refs.txt")),
        refs::refs_args()
    );
    assert_eq!(
        lines(include_str!("fixtures/args-stash.txt")),
        refs::stash_args()
    );
    assert_eq!(lines(include_str!("fixtures/args-diff.txt")), diff::ARGS);
    assert_eq!(
        lines(include_str!("fixtures/args-status.txt")),
        status::ARGS
    );
}

// ---- status --------------------------------------------------------------------------------

fn entry(
    kind: EntryKind,
    index: FileState,
    worktree: FileState,
    path: &str,
) -> status::StatusEntry {
    status::StatusEntry {
        kind,
        index,
        worktree,
        path: path.into(),
        orig_path: None,
        submodule: false,
    }
}

#[test]
fn status_every_entry_kind() {
    use EntryKind::*;
    use FileState::*;
    let status = status::parse(include_bytes!("fixtures/status-work.bin")).unwrap();
    assert_eq!(
        status.branch.oid.as_deref(),
        Some("0692497602b042ad75e9093b0b71047cf9f9e05a")
    );
    assert_eq!(status.branch.head.as_deref(), Some("main"));
    assert_eq!(status.branch.upstream, None);
    assert_eq!(status.stash_count, 0);

    let copy = RenamedOrCopied { score: 100 };
    let mut expected = vec![
        entry(Ordinary, Added, Deleted, "added-then-deleted.txt"),
        entry(Ordinary, Added, Unmodified, "added.txt"),
        entry(Ordinary, Modified, Modified, "both.txt"),
        entry(copy.clone(), Copied, Unmodified, "copy-dst.txt"),
        entry(Ordinary, Modified, Unmodified, "copy-src.txt"),
        entry(Ordinary, Deleted, Unmodified, "deleted.txt"),
        entry(Ordinary, Unmodified, Modified, "modified.txt"),
        entry(Ordinary, Unmodified, Modified, "name with spaces.txt"),
        entry(copy, Renamed, Unmodified, "renamed-dst.txt"),
        entry(Ordinary, Unmodified, Modified, "sub"),
        entry(Ordinary, TypeChanged, Modified, "type.txt"),
        entry(Ordinary, Modified, Unmodified, "ünïcödé.txt"),
        entry(Untracked, Unmodified, Unmodified, "café.md"),
        entry(
            Untracked,
            Unmodified,
            Unmodified,
            "untracked dir/new file.txt",
        ),
    ];
    expected[3].orig_path = Some("copy-src.txt".into());
    expected[8].orig_path = Some("renamed-src.txt".into());
    expected[9].submodule = true;
    assert_eq!(status.entries, expected);
}

#[test]
fn status_ignored_entries() {
    let status = status::parse(include_bytes!("fixtures/status-ignored.bin")).unwrap();
    let ignored: Vec<_> = status
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Ignored)
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(ignored, ["build/out.o", "debug.log"]);
    assert_eq!(status.entries.len(), 16);
}

#[test]
fn status_conflicts() {
    use FileState::*;
    let status = status::parse(include_bytes!("fixtures/status-conflict.bin")).unwrap();
    let entries: Vec<_> = status
        .entries
        .iter()
        .map(|e| (e.kind.clone(), e.index, e.worktree, e.path.to_string()))
        .collect();
    let u = EntryKind::Unmerged;
    assert_eq!(
        entries,
        [
            (u.clone(), Added, Added, "aa.txt".to_owned()),
            (u.clone(), Deleted, Unmerged, "du.txt".to_owned()),
            (u.clone(), Unmerged, Deleted, "ud.txt".to_owned()),
            (u, Unmerged, Unmerged, "uu.txt".to_owned()),
        ]
    );
}

#[test]
fn status_branch_headers() {
    let tracking = status::parse(include_bytes!("fixtures/status-tracking.bin")).unwrap();
    assert_eq!(
        tracking.branch,
        status::BranchInfo {
            oid: Some(MAIN.into()),
            head: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 2,
        }
    );
    assert_eq!(tracking.stash_count, 2);
    assert!(tracking.entries.is_empty());

    let gone = status::parse(include_bytes!("fixtures/status-gone.bin")).unwrap();
    assert_eq!(gone.branch.head.as_deref(), Some("gone-branch"));
    assert_eq!(gone.branch.upstream.as_deref(), Some("origin/gone-branch"));
    assert_eq!((gone.branch.ahead, gone.branch.behind), (0, 0));

    let detached = status::parse(include_bytes!("fixtures/status-detached.bin")).unwrap();
    assert_eq!(detached.branch.oid.as_deref(), Some(MERGE));
    assert_eq!(detached.branch.head, None);
    assert_eq!(detached.branch.upstream, None);

    let initial = status::parse(include_bytes!("fixtures/status-initial.bin")).unwrap();
    assert_eq!(initial.branch.oid, None);
    assert_eq!(initial.branch.head.as_deref(), Some("main"));
    let kinds: Vec<_> = initial
        .entries
        .iter()
        .map(|e| (e.kind.clone(), e.index))
        .collect();
    assert_eq!(
        kinds,
        [
            (EntryKind::Ordinary, FileState::Added),
            (EntryKind::Untracked, FileState::Unmodified)
        ]
    );
}

#[test]
fn status_edge_cases() {
    assert_eq!(status::parse(b"").unwrap(), status::Status::default());
    let unknown_header = status::parse(b"# branch.head main\0# future.thing x y\0").unwrap();
    assert_eq!(unknown_header.branch.head.as_deref(), Some("main"));
    let oid = "0".repeat(40);
    let line = |prefix: &str| format!("{prefix} N... 100644 100644 100644 {oid} {oid} a b\0");
    assert!(status::parse(line("1 .M").as_bytes()).is_ok());
    // No trailing NUL on the last record is fine.
    assert!(status::parse(line("1 .M").trim_end_matches('\0').as_bytes()).is_ok());
    for bad in [
        "3 .M x\0",
        "1 .M\0",
        "1 .Z N... 100644 100644 100644 a b path\0",
        "1 .M X... 100644 100644 100644 a b path\0",
        "# branch.ab +99999999999 -0\0",
        "# branch.ab 1 2\0",
        "# stash -1\0",
        "# branch.oid xyz\0",
        "? \0",
        "garbage",
    ] {
        assert!(status::parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    let rename = format!("2 R. N... 100644 100644 100644 {oid} {oid} R300 new\0old\0");
    assert!(status::parse(rename.as_bytes()).is_err());
    let missing_orig = format!("2 R. N... 100644 100644 100644 {oid} {oid} R90 new\0");
    assert!(status::parse(missing_orig.as_bytes()).is_err());
    let rename = format!("2 R. N... 100644 100644 100644 {oid} {oid} R90 new name\0old name\0");
    let parsed = status::parse(rename.as_bytes()).unwrap();
    assert_eq!(parsed.entries[0].path, "new name");
    assert_eq!(parsed.entries[0].orig_path.as_ref().unwrap(), "old name");
}

// ---- log -----------------------------------------------------------------------------------

#[test]
fn log_all_refs() {
    let commits = log::parse(include_bytes!("fixtures/log-all.bin")).unwrap();
    assert_eq!(commits.len(), 12);

    let stash = &commits[0];
    assert_eq!(stash.id, STASH);
    assert_eq!(stash.parents.len(), 2);
    assert_eq!(stash.refs, [RefLabel::Other("refs/stash".into())]);
    assert_eq!(stash.subject, "WIP on main: bdd6f12");

    assert_eq!(
        commits[2].refs,
        [RefLabel::LocalBranch("gone-branch".into())]
    );
    assert_eq!(
        commits[3].refs,
        [
            RefLabel::RemoteBranch("origin/main".into()),
            RefLabel::RemoteBranch("origin/HEAD".into())
        ]
    );

    let main = &commits[5];
    assert_eq!(main.id, MAIN);
    assert_eq!(
        main.refs,
        [RefLabel::Head, RefLabel::LocalBranch("main".into())]
    );
    assert_eq!(main.head_target.as_deref(), Some("main"));
    assert_eq!(main.subject, "");
    assert_eq!(main.body, "");

    assert_eq!(commits[6].subject, "Multi line subject paragraph");
    assert_eq!(commits[6].body, "And a body.\n");

    let merge = &commits[7];
    assert_eq!(merge.id, MERGE);
    assert_eq!(
        merge.parents,
        ["e948e8f2cda32535ceb8deb41b4e172f8bba8d95", FEATURE]
    );
    assert_eq!(
        merge.refs,
        [
            RefLabel::Tag("v1.0".into()),
            RefLabel::RemoteBranch("upstream/main".into())
        ]
    );
    assert_eq!(merge.head_target, None);
    assert_eq!(merge.subject, "Merge branch 'feature'");

    let feature = &commits[9];
    assert_eq!(feature.author.name, "Zoë Zürich");
    assert_eq!(feature.author.email, "ada@example.com");
    assert_eq!(feature.author.time, 1696777200);
    assert_eq!(feature.author.tz_offset_minutes, 120);
    assert_eq!(feature.committer.name, "Grace Hopper");
    assert_eq!(feature.committer.tz_offset_minutes, -330);
    assert_eq!(
        feature.refs,
        [
            RefLabel::RemoteBranch("origin/feature".into()),
            RefLabel::LocalBranch("feature".into())
        ]
    );

    assert_eq!(
        commits[10].body,
        "This body has two paragraphs.\nSecond line, unicode: héllo ✓\n\n    indented code\n"
    );

    let root = &commits[11];
    assert!(root.parents.is_empty());
    assert_eq!(root.refs, [RefLabel::Tag("v0.1".into())]);
    assert_eq!(root.subject, "Root commit");
    assert_eq!(root.author.time, 1696770000);
}

#[test]
fn log_detached_head() {
    let commits = log::parse(include_bytes!("fixtures/log-detached.bin")).unwrap();
    assert_eq!(commits.len(), 3);
    assert_eq!(
        commits[0].refs,
        [
            RefLabel::Head,
            RefLabel::Tag("v1.0".into()),
            RefLabel::RemoteBranch("upstream/main".into())
        ]
    );
    assert_eq!(commits[0].head_target, None);
}

#[test]
fn log_push_split_at_every_byte() {
    let input = include_bytes!("fixtures/log-all.bin");
    let whole = log::parse(input).unwrap();
    for split in 0..=input.len() {
        let mut parser = LogParser::new();
        let mut commits = parser.push(&input[..split]).unwrap();
        commits.extend(parser.push(&input[split..]).unwrap());
        commits.extend(parser.finish().unwrap());
        assert_eq!(commits, whole, "split at {split}");
    }
    let mut parser = LogParser::new();
    let mut commits = Vec::new();
    for byte in input {
        commits.extend(parser.push(std::slice::from_ref(byte)).unwrap());
    }
    commits.extend(parser.finish().unwrap());
    assert_eq!(commits, whole);
}

#[test]
fn log_streams_complete_records_only() {
    let input = include_bytes!("fixtures/log-all.bin");
    let first_nul = input.find_byte(0).unwrap();
    let mut parser = LogParser::new();
    assert!(parser.push(&input[..first_nul]).unwrap().is_empty());
    assert_eq!(parser.push(&input[first_nul..=first_nul]).unwrap().len(), 1);
}

#[test]
fn log_edge_cases() {
    assert!(log::parse(b"").unwrap().is_empty());
    let input = include_bytes!("fixtures/log-detached.bin");
    // `--pretty=format:` semantics: no NUL after the last record.
    assert_eq!(log::parse(&input[..input.len() - 1]).unwrap().len(), 3);
    // A leading newline before a record is tolerated.
    let mut newline = b"\n".to_vec();
    newline.extend_from_slice(input);
    assert_eq!(log::parse(&newline).unwrap().len(), 3);

    let oid = "a".repeat(40);
    let record = |date: &str| {
        format!("{oid}\x1f\x1fA\x1fa@x\x1f{date}\x1fC\x1fc@x\x1f1 +0000\x1f\x1fsubject\x1fbody\0")
    };
    let ok = log::parse(record("-5 -1230").as_bytes()).unwrap();
    assert_eq!(ok[0].author.time, -5);
    assert_eq!(ok[0].author.tz_offset_minutes, -(12 * 60 + 30));
    for bad in [
        "99999999999999999999 +0000",
        "1 +000",
        "1 0000",
        "1 +00a0",
        "x +0000",
        "1",
    ] {
        assert!(log::parse(record(bad).as_bytes()).is_err(), "{bad:?}");
    }
    // Truncated: the last record lacks fields.
    let truncated = record("1 +0000");
    let cut = truncated.find("\x1fsubject").unwrap();
    assert!(log::parse(&truncated.as_bytes()[..cut]).is_err());
    assert!(log::parse(b"garbage\0").is_err());
    let short_parent =
        format!("{oid}\x1fabc\x1fA\x1fa\x1f1 +0000\x1fC\x1fc\x1f1 +0000\x1f\x1fs\x1f\0");
    assert!(log::parse(short_parent.as_bytes()).is_err());
}

// ---- refs ----------------------------------------------------------------------------------

#[test]
fn refs_fixture() {
    let refs = refs::parse_refs(include_bytes!("fixtures/refs.bin")).unwrap();
    let names: Vec<_> = refs
        .iter()
        .map(|r| (r.full_name.as_str(), r.short_name.as_str(), r.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("refs/heads/feature", "feature", RefKind::LocalBranch),
            (
                "refs/heads/gone-branch",
                "gone-branch",
                RefKind::LocalBranch
            ),
            ("refs/heads/main", "main", RefKind::LocalBranch),
            ("refs/remotes/origin/HEAD", "origin", RefKind::RemoteBranch),
            (
                "refs/remotes/origin/feature",
                "origin/feature",
                RefKind::RemoteBranch
            ),
            (
                "refs/remotes/origin/main",
                "origin/main",
                RefKind::RemoteBranch
            ),
            (
                "refs/remotes/upstream/main",
                "upstream/main",
                RefKind::RemoteBranch
            ),
            ("refs/stash", "stash", RefKind::Stash),
            ("refs/tags/v0.1", "v0.1", RefKind::Tag),
            ("refs/tags/v1.0", "v1.0", RefKind::Tag),
        ]
    );

    let feature = &refs[0];
    assert_eq!(feature.oid, FEATURE);
    assert!(!feature.is_head);
    assert_eq!(
        feature.upstream,
        Some(refs::Upstream {
            name: "refs/remotes/origin/feature".into(),
            ahead: 0,
            behind: 0,
            gone: false,
        })
    );
    assert_eq!(feature.date, 1696777200);
    assert_eq!(feature.subject, "Feature work");

    assert!(refs[1].upstream.as_ref().unwrap().gone);

    let main = &refs[2];
    assert!(main.is_head);
    let upstream = main.upstream.as_ref().unwrap();
    assert_eq!(
        (upstream.ahead, upstream.behind, upstream.gone),
        (2, 2, false)
    );
    assert_eq!(main.subject, "");

    assert_eq!(refs[3].upstream, None);
    assert_eq!(refs[7].oid, STASH);

    assert_eq!(refs[8].peeled_oid, None);
    let annotated = &refs[9];
    assert_eq!(annotated.oid, "37eb8d50dd4ad0fa4b81dd309a8890dc3d96ca5c");
    assert_eq!(annotated.peeled_oid.as_deref(), Some(MERGE));
    assert_eq!(annotated.date, 1696788000);
    assert_eq!(annotated.subject, "Release 1.0");
}

#[test]
fn refs_edge_cases() {
    assert!(refs::parse_refs(b"").unwrap().is_empty());
    let oid = "b".repeat(40);
    let line = |track: &str| {
        format!("refs/heads/x\x1fx\x1f{oid}\x1f\x1f \x1frefs/remotes/o/x\x1f{track}\x1f12\x1fs\n")
    };
    let upstream = |track| refs::parse_refs(line(track).as_bytes()).map(|r| r[0].upstream.clone());
    assert_eq!(upstream("ahead 3").unwrap().unwrap().ahead, 3);
    assert_eq!(upstream("behind 4").unwrap().unwrap().behind, 4);
    for bad in ["ahead x", "sideways 1", "ahead 99999999999"] {
        assert!(upstream(bad).is_err(), "{bad:?}");
    }
    assert!(refs::parse_refs(b"refs/heads/x\x1fx\n").is_err());
    let bad_head = format!("refs/heads/x\x1fx\x1f{oid}\x1f\x1f?\x1f\x1f\x1f1\x1fs\n");
    assert!(refs::parse_refs(bad_head.as_bytes()).is_err());
}

#[test]
fn stashes_fixture() {
    let stashes = refs::parse_stashes(include_bytes!("fixtures/stashes.bin")).unwrap();
    assert_eq!(
        stashes,
        [
            refs::Stash {
                index: 0,
                oid: STASH.into(),
                time: 1696809600,
                message: "WIP on main: bdd6f12".into(),
            },
            refs::Stash {
                index: 1,
                oid: "000c1ea9f9ce6f6e11a0b549ba48299ccdd0111b".into(),
                time: 1696806000,
                message: "On main: first stash".into(),
            },
        ]
    );
    assert!(refs::parse_stashes(b"").unwrap().is_empty());
    let oid = "c".repeat(40);
    for bad in ["stash@{x}", "stash@{99999999999}", "stash", "stash@{1"] {
        let record = format!("{bad}\x1f{oid}\x1f1\x1fmsg\0");
        assert!(refs::parse_stashes(record.as_bytes()).is_err(), "{bad:?}");
    }
}

#[test]
fn remotes_fixture() {
    let remotes = refs::parse_remotes(include_bytes!("fixtures/remotes.bin")).unwrap();
    assert_eq!(
        remotes,
        [
            refs::Remote {
                name: "origin".into(),
                fetch_url: Some("../origin.git".into()),
                push_url: Some("../origin.git".into()),
            },
            refs::Remote {
                name: "upstream".into(),
                fetch_url: Some("../upstream.git".into()),
                push_url: Some("../upstream-push.git".into()),
            },
        ]
    );
    let spaced = refs::parse_remotes(b"o\thttps://h/a b (push)\n").unwrap();
    assert_eq!(spaced[0].push_url.as_deref(), Some("https://h/a b"));
    assert_eq!(spaced[0].fetch_url, None);
    assert!(refs::parse_remotes(b"origin https://x (fetch)\n").is_err());
    assert!(refs::parse_remotes(b"origin\thttps://x\n").is_err());
}

#[test]
fn ahead_behind() {
    assert_eq!(
        refs::parse_ahead_behind(include_bytes!("fixtures/ahead-behind.bin")).unwrap(),
        (2, 2)
    );
    assert_eq!(refs::parse_ahead_behind(b"3\t5\n").unwrap(), (3, 5));
    for bad in [
        &b""[..],
        b"3 5\n",
        b"3\t\n",
        b"-1\t2",
        b"4294967296\t0",
        b"1\t2\n\n",
    ] {
        assert!(refs::parse_ahead_behind(bad).is_err(), "{bad:?}");
    }
}

// ---- diff ----------------------------------------------------------------------------------

const DIFF_FIXTURES: &[(&str, &[u8])] = &[
    ("worktree", include_bytes!("fixtures/diff-worktree.bin")),
    ("cached", include_bytes!("fixtures/diff-cached.bin")),
    ("copies", include_bytes!("fixtures/diff-copies.bin")),
    ("empty", include_bytes!("fixtures/diff-empty.bin")),
    ("quoted", include_bytes!("fixtures/diff-quoted.bin")),
    ("quoted-off", include_bytes!("fixtures/diff-quoted-off.bin")),
    ("show", include_bytes!("fixtures/diff-show.bin")),
    ("submodule", include_bytes!("fixtures/diff-submodule.bin")),
];

/// Re-emits parsed files as a patch, the way hunk staging will.
fn render(files: &[FileDiff]) -> Vec<u8> {
    let mut out = Vec::new();
    for file in files {
        out.extend_from_slice(&file.header);
        for hunk in &file.hunks {
            out.extend_from_slice(&hunk.header);
            out.push(b'\n');
            for line in &hunk.lines {
                out.extend_from_slice(match line.kind {
                    LineKind::Context => b" ",
                    LineKind::Added => b"+",
                    LineKind::Deleted => b"-",
                    LineKind::NoNewlineAtEof => b"\\ ",
                });
                out.extend_from_slice(&line.content);
                out.push(b'\n');
            }
        }
    }
    out
}

#[test]
fn diff_fixtures_round_trip() {
    for (name, input) in DIFF_FIXTURES {
        let files = diff::parse(input).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(render(&files).as_bstr(), input.as_bstr(), "{name}");
    }
}

fn paths(files: &[FileDiff]) -> Vec<(Option<BString>, Option<BString>, FileChange)> {
    files
        .iter()
        .map(|f| (f.old_path.clone(), f.new_path.clone(), f.change))
        .collect()
}

fn same(path: &str) -> (Option<BString>, Option<BString>, FileChange) {
    (Some(path.into()), Some(path.into()), FileChange::Modified)
}

#[test]
fn diff_worktree() {
    let files = diff::parse(include_bytes!("fixtures/diff-worktree.bin")).unwrap();
    assert_eq!(
        paths(&files),
        [
            same("binary.bin"),
            same("crlf.txt"),
            same("gets-noeol.txt"),
            same("multi.txt"),
            same("name with spaces.txt"),
            same("noeol.txt"),
            same("ünïcödé.txt"),
        ]
    );

    let binary = &files[0];
    assert!(binary.binary);
    assert!(binary.hunks.is_empty());
    assert_eq!(binary.old_mode.as_deref(), Some("100644"));

    let crlf = &files[1];
    assert!(!crlf.binary);
    assert_eq!(
        crlf.header,
        "diff --git a/crlf.txt b/crlf.txt\nindex e1587ff..103b2f6 100644\n--- a/crlf.txt\n+++ b/crlf.txt\n"
    );
    let hunk = &crlf.hunks[0];
    assert_eq!(hunk.header, "@@ -1,3 +1,3 @@");
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (1, 3, 1, 3)
    );
    let lines: Vec<_> = hunk
        .lines
        .iter()
        .map(|l| (l.kind, l.old_lineno, l.new_lineno, l.content.to_string()))
        .collect();
    assert_eq!(
        lines,
        [
            (LineKind::Context, Some(1), Some(1), "one\r".into()),
            (LineKind::Deleted, Some(2), None, "two\r".into()),
            (LineKind::Added, None, Some(2), "TWO\r".into()),
            (LineKind::Context, Some(3), Some(3), "three\r".into()),
        ]
    );

    let gets_noeol = &files[2].hunks[0];
    assert_eq!((gets_noeol.old_lines, gets_noeol.new_lines), (1, 2));
    let last = gets_noeol.lines.last().unwrap();
    assert_eq!(last.kind, LineKind::NoNewlineAtEof);
    assert_eq!(last.content, "No newline at end of file");
    assert_eq!((last.old_lineno, last.new_lineno), (None, None));

    let multi = &files[3];
    assert_eq!(multi.hunks.len(), 2);
    let second = &multi.hunks[1];
    assert_eq!(second.header, "@@ -32,7 +32,7 @@ line 31");
    assert_eq!(second.section, "line 31");
    let deleted = second
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Deleted)
        .unwrap();
    assert_eq!(
        (deleted.old_lineno, deleted.content.to_string()),
        (Some(35), "line 35".into())
    );
    let added = second
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added)
        .unwrap();
    assert_eq!(added.new_lineno, Some(35));

    let kinds: Vec<_> = files[5].hunks[0].lines.iter().map(|l| l.kind).collect();
    use LineKind::*;
    assert_eq!(
        kinds,
        [Deleted, NoNewlineAtEof, Added, Added, NoNewlineAtEof]
    );
}

#[test]
fn diff_cached() {
    let files = diff::parse(include_bytes!("fixtures/diff-cached.bin")).unwrap();
    let p = |s: &str| Some(BString::from(s));
    assert_eq!(
        paths(&files),
        [
            (None, p("added.bin"), FileChange::Added),
            (None, p("added.txt"), FileChange::Added),
            (None, p("copy-dst.txt"), FileChange::Added),
            (p("delete-me.txt"), None, FileChange::Deleted),
            (
                p("empty-delete.txt"),
                p("empty-added.txt"),
                FileChange::Renamed { similarity: 100 }
            ),
            (p("mode.sh"), p("mode.sh"), FileChange::ModeChanged),
            (
                p("pure-rename-src.txt"),
                p("pure renamed.txt"),
                FileChange::Renamed { similarity: 100 }
            ),
            (
                p("rename-src.txt"),
                p("renamed.txt"),
                FileChange::Renamed { similarity: 94 }
            ),
        ]
    );
    assert!(files[0].binary);
    assert_eq!(
        (files[0].old_mode.as_deref(), files[0].new_mode.as_deref()),
        (None, Some("100644"))
    );

    let added = &files[1].hunks[0];
    assert_eq!(added.header, "@@ -0,0 +1,3 @@");
    assert_eq!((added.old_start, added.old_lines), (0, 0));
    let numbers: Vec<_> = added
        .lines
        .iter()
        .map(|l| (l.old_lineno, l.new_lineno))
        .collect();
    assert_eq!(numbers, [(None, Some(1)), (None, Some(2)), (None, Some(3))]);

    let deleted = &files[3];
    assert_eq!(deleted.old_mode.as_deref(), Some("100644"));
    assert_eq!(deleted.new_mode, None);
    assert_eq!(deleted.hunks[0].lines.len(), 10);
    assert_eq!(deleted.hunks[0].lines[9].old_lineno, Some(10));

    assert!(files[4].hunks.is_empty());
    let mode = &files[5];
    assert_eq!(
        (mode.old_mode.as_deref(), mode.new_mode.as_deref()),
        (Some("100644"), Some("100755"))
    );
    assert!(mode.hunks.is_empty());

    let renamed = &files[7];
    assert_eq!(renamed.hunks[0].section, "ren 11");
    assert_eq!(renamed.old_mode.as_deref(), Some("100644"));
}

#[test]
fn diff_copies_and_empty_files() {
    let copies = diff::parse(include_bytes!("fixtures/diff-copies.bin")).unwrap();
    let copy = copies
        .iter()
        .find(|f| matches!(f.change, FileChange::Copied { .. }))
        .unwrap();
    assert_eq!(copy.change, FileChange::Copied { similarity: 100 });
    assert_eq!(copy.old_path.as_ref().unwrap(), "copy-src.txt");
    assert_eq!(copy.new_path.as_ref().unwrap(), "copy-dst.txt");

    let empty = diff::parse(include_bytes!("fixtures/diff-empty.bin")).unwrap();
    let p = |s: &str| Some(BString::from(s));
    assert_eq!(
        paths(&empty),
        [
            (None, p("empty-added.txt"), FileChange::Added),
            (p("empty-delete.txt"), None, FileChange::Deleted),
        ]
    );
    assert!(empty.iter().all(|f| f.hunks.is_empty() && !f.binary));
}

#[test]
fn diff_quoted_paths() {
    let expected: Vec<BString> = [
        &b"back\\slash"[..],
        "café ✓.txt".as_bytes(),
        b"new\nline",
        b"quote\"d",
        b"renamed\ttab.txt",
        b"tab\there",
    ]
    .map(BString::from)
    .into();
    for input in [
        &include_bytes!("fixtures/diff-quoted.bin")[..],
        include_bytes!("fixtures/diff-quoted-off.bin"),
    ] {
        let files = diff::parse(input).unwrap();
        let new: Vec<_> = files.iter().map(|f| f.new_path.clone().unwrap()).collect();
        assert_eq!(new, expected);
        assert_eq!(files[4].old_path.as_ref().unwrap(), "name with spaces.txt");
        assert_eq!(files[4].change, FileChange::Renamed { similarity: 100 });
    }
}

#[test]
fn diff_submodule_show_and_conflict() {
    let sub = diff::parse(include_bytes!("fixtures/diff-submodule.bin")).unwrap();
    assert_eq!(paths(&sub), [same("sub")]);
    let lines = &sub[0].hunks[0].lines;
    assert_eq!(
        lines[0].content,
        "Subproject commit fe80333638bca6f07b94afa1258d005804de11ae"
    );
    assert!(lines[1].content.ends_with(b"-dirty"));

    let show = diff::parse(include_bytes!("fixtures/diff-show.bin")).unwrap();
    assert_eq!(paths(&show), [same("README.md")]);

    // Combined diffs and `* Unmerged path` lines are skipped.
    assert!(
        diff::parse(include_bytes!("fixtures/diff-conflict.bin"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn diff_edge_cases() {
    assert!(diff::parse(b"").unwrap().is_empty());
    // Commit header from `git show` is skipped.
    let show = b"commit abc\nAuthor: x\n\n    msg\n\ndiff --git a/x b/x\nold mode 100644\nnew mode 100755\n";
    assert_eq!(
        diff::parse(show).unwrap()[0].change,
        FileChange::ModeChanged
    );
    // Mode-only change: paths come from splitting the `diff --git` line in half.
    let spaced = b"diff --git a/a b/c b/a b/c\nold mode 100644\nnew mode 100755\n";
    assert_eq!(
        diff::parse(spaced).unwrap()[0].new_path.as_ref().unwrap(),
        "a b/c"
    );
    // `diff.suppressBlankEmpty`: empty context line; no trailing newline at EOF.
    let blank = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n\n-a\n+b";
    let hunk = &diff::parse(blank).unwrap()[0].hunks[0];
    assert_eq!(hunk.lines[0].kind, LineKind::Context);
    assert_eq!(hunk.lines[0].content, "");
    assert_eq!(hunk.lines[2].content, "b");

    let file = "diff --git a/x b/x\n--- a/x\n+++ b/x\n";
    for bad in [
        "@@ -1,2 +1,2 @@\n a\n",
        "@@ -1 +1 @@\n a\n b\n",
        "@@ -1 +1 @@\n-a\n-b\n+c\n",
        "@@ -1 +1 @@\n?a\n",
        "@@ -1,4294967295 +1 @@\n a\n",
        "@@ -4294967295,5 +1 @@\n a\n",
        "@@ -99999999999 +1 @@\n a\n",
        "@@ -a +1 @@\n a\n",
        "@@ -1 +1\n a\n",
        "@@ -1 +1 @@\n\\ No newline\n a\n",
    ] {
        assert!(
            diff::parse(format!("{file}{bad}").as_bytes()).is_err(),
            "{bad:?}"
        );
    }
    for bad in [
        &b"diff --git a/x b/x\nsimilarity index 101%\n"[..],
        b"diff --git a/x b/x\nold mode 10064a\n",
        b"diff --git a/x b/x\nrename from \"unterminated\n",
        b"diff --git a/x b/x\nrename to \"bad\\qescape\"\n",
        b"diff --git a/x b/x\n--- \"a/\\400\"\n",
        b"diff --git x\n",
        b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n a\ntrailing garbage\n",
    ] {
        assert!(diff::parse(bad).is_err(), "{:?}", bad.as_bstr());
    }
    let octal = b"diff --git \"a/\\303\\251\" \"b/\\303\\251\"\nnew file mode 100644\nindex 0000000..e69de29\n";
    assert_eq!(
        diff::parse(octal).unwrap()[0].new_path.as_ref().unwrap(),
        "é"
    );
}

// ---- performance ---------------------------------------------------------------------------

/// `cargo test -p maomi-git --release --test parsers -- --ignored --nocapture`
#[test]
#[ignore = "timing; run in release"]
fn large_inputs_parse_fast() {
    let record = include_bytes!("fixtures/log-detached.bin");
    let one = &record[..=record.find_byte(0).unwrap()];
    let log_input = one.repeat(100_000);
    let started = std::time::Instant::now();
    let mut parser = LogParser::new();
    let mut count = 0;
    for chunk in log_input.chunks(64 * 1024) {
        count += parser.push(chunk).unwrap().len();
    }
    count += parser.finish().unwrap().len();
    assert_eq!(count, 100_000);
    println!("log: 100k commits in {:?}", started.elapsed());

    let mut diff_input =
        b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,20000 +1,20000 @@\n".to_vec();
    for i in 0..20_000 {
        diff_input.extend_from_slice(format!(" line {i}\n").as_bytes());
    }
    let started = std::time::Instant::now();
    let files = diff::parse(&diff_input).unwrap();
    assert_eq!(files[0].hunks[0].lines.len(), 20_000);
    println!("diff: 20k lines in {:?}", started.elapsed());
}
