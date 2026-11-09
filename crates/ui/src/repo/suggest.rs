//! The board's commit suggestion for a repository: finds the repo's board, reads the latest
//! session's suggestion and retires it once it is committed. Port of `headCommitAt` and
//! `suggestionCommitted` in AgentSummary's `commitSuggestionNode.ts`. Never fails: no board, no
//! commits or a git error read as no suggestion / not committed.

use std::path::{Path, PathBuf};

use maomi_git::{CliRepo, GitCommand, GitRepo as _};
use maomi_workbench::{CommitSuggestion, latest_session_suggestion};

/// Commits looked at when asking whether a suggestion was committed.
const COMMITTED_SCAN: usize = 50;

/// The suggestion for the repo's last session, or `None` (no board, nothing ticked since HEAD's
/// commit, or already committed). Blocking file reads: run it on the background executor.
pub async fn load(repo: &CliRepo) -> Option<CommitSuggestion> {
    let (board_dir, project) = board_for(repo.root())?;
    let since = head_commit_at(repo).await.unwrap_or(0);
    let suggestion = latest_session_suggestion(&board_dir, &project, since)?;
    (!committed(repo, &suggestion).await).then_some(suggestion)
}

/// The board folder and project name of the Workbench project at `root`, else the repo's own
/// `.maomi/board`.
fn board_for(root: &Path) -> Option<(PathBuf, String)> {
    let key = path_key(root);
    if let Some(p) = maomi_workbench::projects()
        .into_iter()
        .find(|p| p.path.as_deref().is_some_and(|path| path_key(path) == key))
    {
        return Some((p.board_dir, p.name));
    }
    let local = root.join(".maomi").join("board");
    let name = root.file_name()?.to_string_lossy().into_owned();
    local.is_dir().then_some((local, name))
}

/// `root` for comparing: git prints `F:/x`, the Workbench may store `F:\x` or `f:\x\`.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// The end of the second HEAD was committed in (epoch ms): ticks before it went into that commit
/// or an earlier one. `None` on an unborn branch or a git failure.
async fn head_commit_at(repo: &CliRepo) -> Option<i64> {
    let out = repo
        .runner()
        .run(GitCommand::read(repo.root()).args(["log", "-1", "--format=%ct"]))
        .await
        .ok()?;
    commit_second_end(&String::from_utf8_lossy(&out))
}

fn commit_second_end(stdout: &str) -> Option<i64> {
    let secs: i64 = stdout.trim().parse().ok()?;
    Some((secs + 1) * 1000)
}

/// Whether a recent commit carries the suggestion's `Maomi-Session:` trailer or was made at or
/// after the session ended. False on an unborn branch or a git failure.
async fn committed(repo: &CliRepo, suggestion: &CommitSuggestion) -> bool {
    let scan = COMMITTED_SCAN.to_string();
    let out = repo
        .runner()
        .run(GitCommand::read(repo.root()).args([
            "log",
            "-n",
            scan.as_str(),
            "--format=%H%x1f%ct%x1f%B%x1e",
        ]))
        .await;
    out.is_ok_and(|out| log_has_commit_for(&String::from_utf8_lossy(&out), suggestion))
}

/// `log` is `%H\x1f%ct\x1f%B\x1e` records.
fn log_has_commit_for(log: &str, suggestion: &CommitSuggestion) -> bool {
    let trailer = suggestion.trailer();
    let ended_sec = suggestion.ended_at.div_euclid(1000);
    log.split('\x1e').any(|record| {
        let mut parts = record.trim().splitn(3, '\x1f');
        let (Some(_oid), Some(when), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        when.parse::<i64>().is_ok_and(|w| w >= ended_sec)
            || body.lines().any(|l| l.trim_end() == trailer)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion(session: &str, ended_at: i64) -> CommitSuggestion {
        CommitSuggestion {
            title: "t".into(),
            body: String::new(),
            session_id: session.into(),
            ended_at,
            plan_id: None,
            cards: 1,
            tasks: 1,
        }
    }

    fn record(oid: &str, when: i64, body: &str) -> String {
        format!("{oid}\x1f{when}\x1f{body}\x1e\n")
    }

    #[test]
    fn committed_when_a_body_carries_the_trailer() {
        let log = record("a", 100, "Title\n\n- x\n\nMaomi-Session: s-1\n");
        assert!(log_has_commit_for(&log, &suggestion("s-1", 500_000)));
    }

    #[test]
    fn another_sessions_trailer_or_a_substring_does_not_count() {
        let log = record("a", 100, "Maomi-Session: s-2\n") + &record("b", 90, "see Maomi-Session: s-1 above\n");
        assert!(!log_has_commit_for(&log, &suggestion("s-1", 500_000)));
    }

    #[test]
    fn committed_when_a_commit_is_at_or_after_the_session_end() {
        let log = record("a", 500, "plain\n");
        assert!(log_has_commit_for(&log, &suggestion("s-1", 500_000)));
        assert!(log_has_commit_for(&log, &suggestion("s-1", 500_999)));
        assert!(!log_has_commit_for(&log, &suggestion("s-1", 501_000)));
    }

    #[test]
    fn empty_or_damaged_log_is_not_committed() {
        assert!(!log_has_commit_for("", &suggestion("s-1", 1)));
        assert!(!log_has_commit_for("garbage\x1e", &suggestion("s-1", 1)));
    }

    #[test]
    fn head_time_is_the_end_of_its_second() {
        assert_eq!(commit_second_end("1700000000\n"), Some(1_700_000_001_000));
        assert_eq!(commit_second_end(""), None);
    }

    #[test]
    fn paths_compare_across_slash_styles() {
        assert_eq!(path_key(Path::new("F:/a/b/")), path_key(Path::new("F:\\a\\b")));
    }
}
