//! Rebuilds a patch from parsed hunks so part of a file can be staged, unstaged or discarded with
//! `git apply` (hunk and line staging).
//!
//! Forward (stage from the unstaged diff): an unselected `+` line is dropped and an unselected
//! `-` line becomes context, so the patch still matches the index. Reverse (unstage from the
//! staged diff, discard from the unstaged diff; applied with `--reverse`): the target is the new
//! side, so an unselected `+` line becomes context and an unselected `-` line is dropped.

use crate::parse::diff::{FileDiff, Hunk, LineKind};

/// Which way the patch will be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `git apply` (e.g. `--cached` to stage unstaged lines).
    Forward,
    /// `git apply --reverse` (unstage staged lines, discard unstaged lines).
    Reverse,
}

/// Lines picked in one hunk: indices into [`Hunk::lines`]. Context and `\ No newline` lines are
/// ignored; only `+` / `-` lines are choosable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Every change in the hunk.
    All,
    Lines(Vec<usize>),
}

/// Builds a patch for `picks` (hunk index, lines) of `file`. Returns `None` when nothing
/// changes (no picked `+`/`-` line), for binary files, or for an out-of-range hunk.
pub fn build(file: &FileDiff, picks: &[(usize, Pick)], direction: Direction) -> Option<Vec<u8>> {
    if file.binary {
        return None;
    }
    let mut picks: Vec<&(usize, Pick)> = picks.iter().collect();
    picks.sort_by_key(|(hunk, _)| *hunk);
    picks.dedup_by_key(|(hunk, _)| *hunk);

    let mut out = Vec::from(file.header.as_slice());
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    // Lines added minus lines removed by the hunks emitted so far: shifts the other side's start.
    let mut delta: i64 = 0;
    let mut any = false;
    for (index, pick) in picks {
        let hunk = file.hunks.get(*index)?;
        if let Some((text, shift)) = hunk_text(hunk, pick, direction, delta) {
            out.extend_from_slice(&text);
            delta += shift;
            any = true;
        }
    }
    any.then_some(out)
}

/// One rewritten hunk and its `added - removed` count, or `None` if no change was picked.
fn hunk_text(hunk: &Hunk, pick: &Pick, direction: Direction, delta: i64) -> Option<(Vec<u8>, i64)> {
    let picked = |i: usize| match pick {
        Pick::All => true,
        Pick::Lines(lines) => lines.contains(&i),
    };
    let mut body = Vec::new();
    let (mut old_count, mut new_count, mut changes) = (0u32, 0u32, 0usize);
    // Whether the last emitted line was kept, so its `\ No newline` marker follows it.
    let mut last_kept = false;
    for (i, line) in hunk.lines.iter().enumerate() {
        let prefix = match (line.kind, picked(i), direction) {
            (LineKind::Context, ..) => b' ',
            (LineKind::NoNewlineAtEof, ..) => {
                if last_kept {
                    body.extend_from_slice(b"\\ No newline at end of file\n");
                }
                continue;
            }
            (LineKind::Added, true, _) => b'+',
            (LineKind::Deleted, true, _) => b'-',
            (LineKind::Added, false, Direction::Reverse)
            | (LineKind::Deleted, false, Direction::Forward) => b' ',
            (LineKind::Added, false, Direction::Forward)
            | (LineKind::Deleted, false, Direction::Reverse) => {
                last_kept = false;
                continue;
            }
        };
        match prefix {
            b'+' => new_count += 1,
            b'-' => old_count += 1,
            _ => {
                old_count += 1;
                new_count += 1;
            }
        }
        changes += usize::from(prefix != b' ');
        body.push(prefix);
        body.extend_from_slice(&line.content);
        body.push(b'\n');
        last_kept = true;
    }
    if changes == 0 {
        return None;
    }
    // The side the patch is matched against keeps its original start; the other side follows
    // from it and the hunks already emitted.
    let (old_start, new_start) = match direction {
        Direction::Forward => {
            let old = hunk.old_start;
            (old, other_start(old, old_count, new_count, delta))
        }
        Direction::Reverse => {
            let new = hunk.new_start;
            (other_start(new, new_count, old_count, -delta), new)
        }
    };
    let mut text = format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@");
    if !hunk.section.is_empty() {
        text.push(' ');
        text.push_str(&String::from_utf8_lossy(&hunk.section));
    }
    text.push('\n');
    let mut text = text.into_bytes();
    text.extend_from_slice(&body);
    Some((text, i64::from(new_count) - i64::from(old_count)))
}

/// Start line on the other side. A side with 0 lines names the line *before* the hunk, so
/// convert to "first line of the hunk" and back.
fn other_start(start: u32, count: u32, other_count: u32, delta: i64) -> u32 {
    let first = i64::from(start) + i64::from(count == 0);
    let other_first = first + delta;
    let other = other_first - i64::from(other_count == 0);
    u32::try_from(other.max(0)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::diff;

    fn file(patch: &str) -> FileDiff {
        diff::parse(patch.as_bytes()).unwrap().remove(0)
    }

    const TWO_HUNKS: &str = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,4 @@ fn top
 a
-b
+B
+B2
 c
@@ -10,2 +11,2 @@
 x
-y
+Y
";

    fn text(bytes: Option<Vec<u8>>) -> String {
        String::from_utf8(bytes.expect("a patch")).unwrap()
    }

    #[test]
    fn second_hunk_only_keeps_its_old_start() {
        let f = file(TWO_HUNKS);
        let out = text(build(&f, &[(1, Pick::All)], Direction::Forward));
        assert!(out.starts_with("diff --git a/f.txt b/f.txt\n"));
        assert!(out.contains("@@ -10,2 +10,2 @@\n x\n-y\n+Y\n"), "{out}");
    }

    #[test]
    fn forward_partial_lines() {
        let f = file(TWO_HUNKS);
        // Pick only `+B` (index 2): `-b` stays as context, `+B2` is dropped.
        let out = text(build(&f, &[(0, Pick::Lines(vec![2]))], Direction::Forward));
        assert!(
            out.contains("@@ -1,3 +1,4 @@ fn top\n a\n b\n+B\n c\n"),
            "{out}"
        );
    }

    #[test]
    fn reverse_partial_lines() {
        let f = file(TWO_HUNKS);
        // Unstage only `-b` (index 1): `+B` / `+B2` stay as context.
        let out = text(build(&f, &[(0, Pick::Lines(vec![1]))], Direction::Reverse));
        assert!(
            out.contains("@@ -1,5 +1,4 @@ fn top\n a\n-b\n B\n B2\n c\n"),
            "{out}"
        );
    }

    #[test]
    fn shifts_later_hunks_by_earlier_ones() {
        let f = file(TWO_HUNKS);
        let out = text(build(
            &f,
            &[(0, Pick::All), (1, Pick::All)],
            Direction::Forward,
        ));
        assert!(out.contains("@@ -1,3 +1,4 @@ fn top\n"), "{out}");
        assert!(out.contains("@@ -10,2 +11,2 @@\n"), "{out}");
    }

    #[test]
    fn nothing_picked_is_none() {
        let f = file(TWO_HUNKS);
        assert_eq!(
            build(&f, &[(0, Pick::Lines(vec![0]))], Direction::Forward),
            None
        );
        assert_eq!(build(&f, &[(9, Pick::All)], Direction::Forward), None);
    }

    #[test]
    fn pure_insert_counts_from_the_line_before() {
        let f = file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -2,0 +3,2 @@\n+n1\n+n2\n@@ -5,0 +8,1 @@\n+m\n",
        );
        let out = text(build(&f, &[(1, Pick::All)], Direction::Forward));
        assert!(out.contains("@@ -5,0 +6,1 @@\n+m\n"), "{out}");
    }

    #[test]
    fn keeps_no_newline_marker_with_its_line() {
        let f = file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        );
        // Only `-old`: the dropped `+new` takes its marker with it.
        let out = text(build(&f, &[(0, Pick::Lines(vec![0]))], Direction::Forward));
        assert!(
            out.ends_with("@@ -1,1 +0,0 @@\n-old\n\\ No newline at end of file\n"),
            "{out}"
        );
    }
}
