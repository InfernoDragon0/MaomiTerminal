//! An agent's state per pane (Claude Code: working, asking for permission, waiting).
//!
//! The Maomi mod's hooks write `$MAOMI_STATUS_DIR/$MAOMI_PANE_ID.json` (v1:
//! `{"v":1,"agent":"claude","state":…,"tool":…,"session":…,"pid":…,"at":<unix ms>}`); every
//! local pane gets both variables and polls its own file. Without the mod, a bell or desktop
//! notification while a command runs marks the pane as needing the user instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

pub(crate) const PANE_ID_VAR: &str = "MAOMI_PANE_ID";
pub(crate) const STATUS_DIR_VAR: &str = "MAOMI_STATUS_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    /// Waiting for the user to allow a tool.
    Permission,
    /// The turn is over; the agent waits for the next prompt.
    Waiting,
    Error,
    /// No status file: a bell or notification arrived while a command ran.
    NeedsYou,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub state: AgentState,
    /// The tool running or asking, e.g. `Bash`.
    pub tool: Option<String>,
    /// The agent's process; the status is dropped once it is gone.
    pub pid: Option<u32>,
    /// Unix ms when the hook wrote it.
    pub at: u64,
}

impl AgentStatus {
    /// Claude asks the user something (a question, a plan to approve) rather than for a tool.
    /// Both come through the permission flow.
    pub fn is_question(&self) -> bool {
        self.state == AgentState::Permission
            && matches!(
                self.tool.as_deref(),
                Some("AskUserQuestion" | "ExitPlanMode")
            )
    }

    /// Blocked on the user: the session row and the pane's border pulse.
    pub fn pulses(&self) -> bool {
        matches!(self.state, AgentState::Permission | AgentState::NeedsYou)
    }

    /// Short text for the session row and pane header.
    pub fn label(&self) -> String {
        let tool = self.tool.as_deref();
        match (self.state, tool) {
            (AgentState::Working, Some(tool)) => format!("Claude \u{b7} {tool}"),
            (AgentState::Working, None) => "Claude \u{b7} working".into(),
            // Claude Code asks its questions and shows its plans through the permission flow.
            (AgentState::Permission, Some("AskUserQuestion")) => "question for you".into(),
            (AgentState::Permission, Some("ExitPlanMode")) => "plan to review".into(),
            (AgentState::Permission, Some(tool)) => format!("allow {tool}?"),
            (AgentState::Permission, None) => "needs permission".into(),
            (AgentState::Waiting, _) => "waiting for you".into(),
            (AgentState::Error, _) => "Claude \u{b7} error".into(),
            (AgentState::NeedsYou, _) => "needs you".into(),
        }
    }
}

/// The badge colour: green while working, blue for a permission, orange for a question,
/// grey while it waits for the next prompt.
pub(crate) fn color(status: &AgentStatus, p: &crate::Palette) -> crate::Color {
    match status.state {
        AgentState::Working => p.ok,
        AgentState::Permission if status.is_question() => p.warn,
        AgentState::Permission => p.now,
        // Unknown which: a bell or notification only says the program wants the user.
        AgentState::NeedsYou => p.warn,
        AgentState::Waiting => p.muted,
        AgentState::Error => p.err,
    }
}

/// The pulse of a session row and pane border while the agent is blocked on the user; synced
/// so every pulsing pane beats together.
pub(crate) fn pulse() -> gpui_kit::Animation {
    gpui_kit::Animation::new(std::time::Duration::from_millis(1600))
        .repeat_synced()
        .with_easing(gpui_kit::pulsating_between(0.2, 1.))
}

#[derive(Deserialize)]
struct StatusFile {
    v: u32,
    state: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    at: u64,
}

/// What a pane's status file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusRead {
    /// Same modified time as last read.
    Unchanged,
    /// No file (no agent in this pane, or the mod is not installed).
    Missing,
    /// Not valid JSON yet: the mod writes in place, so a read can catch it half written.
    Partial,
    /// "ended", or a version or state this build does not know.
    Ended,
    Status(AgentStatus),
}

pub(crate) fn parse(text: &str) -> StatusRead {
    let Ok(file) = serde_json::from_str::<StatusFile>(text.trim()) else {
        return StatusRead::Partial;
    };
    if file.v != 1 {
        return StatusRead::Ended;
    }
    let state = match file.state.as_str() {
        "working" => AgentState::Working,
        "permission" => AgentState::Permission,
        "waiting" => AgentState::Waiting,
        "error" => AgentState::Error,
        _ => return StatusRead::Ended,
    };
    StatusRead::Status(AgentStatus {
        state,
        tool: file.tool.filter(|t| !t.is_empty()),
        pid: file.pid,
        at: file.at,
    })
}

/// Reads `path` unless its modified time is still `seen`.
pub(crate) fn read(path: &Path, seen: Option<SystemTime>) -> (Option<SystemTime>, StatusRead) {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return (None, StatusRead::Missing);
    };
    if Some(modified) == seen {
        return (seen, StatusRead::Unchanged);
    }
    match std::fs::read_to_string(path) {
        Ok(text) => (Some(modified), parse(&text)),
        Err(_) => (Some(modified), StatusRead::Partial),
    }
}

/// The folder status files go to, created on first use; None when it cannot be made.
pub(crate) fn status_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = maomi_config::agent_status_dir().ok()?;
        match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(dir),
            Err(err) => {
                tracing::warn!(%err, "cannot create the agent status folder");
                None
            }
        }
    })
    .as_deref()
}

/// A new pane id, `<app pid>-<n>`: unique across MaomiTerminal processes sharing the folder.
pub(crate) fn new_pane_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Environment for a local pane's shell, so agents started in it know where to report.
pub(crate) fn env(pane_id: &str) -> Vec<(OsString, OsString)> {
    let Some(dir) = status_dir() else {
        return Vec::new();
    };
    vec![
        (PANE_ID_VAR.into(), pane_id.into()),
        (STATUS_DIR_VAR.into(), dir.as_os_str().to_owned()),
    ]
}

/// The pane id in a session's extra environment.
pub(crate) fn pane_id(env: &[(OsString, OsString)]) -> Option<String> {
    env.iter()
        .find(|(key, _)| key == PANE_ID_VAR)
        .map(|(_, value)| value.to_string_lossy().into_owned())
}

pub(crate) fn status_file(pane_id: &str) -> Option<PathBuf> {
    Some(status_dir()?.join(format!("{pane_id}.json")))
}

/// Removes status files of MaomiTerminal processes that are gone (their panes cannot read
/// them any more). Files of other running instances stay.
pub(crate) fn sweep_stale() {
    let Some(dir) = status_dir() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let owner = name
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        if owner.is_some_and(|pid| !maomi_term::process::is_alive(pid)) {
            _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(text: &str) -> AgentStatus {
        match parse(text) {
            StatusRead::Status(status) => status,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_v1_status() {
        let s = status(
            r#"{"v":1,"agent":"claude","state":"permission","tool":"Bash","session":"s","pid":42,"at":7}"#,
        );
        assert_eq!(s.state, AgentState::Permission);
        assert_eq!(s.tool.as_deref(), Some("Bash"));
        assert_eq!(s.pid, Some(42));
        assert_eq!(s.label(), "allow Bash?");
        let question = status(r#"{"v":1,"state":"permission","tool":"AskUserQuestion","at":1}"#);
        assert_eq!(question.label(), "question for you");
        assert!(question.is_question() && question.pulses());
        assert!(!s.is_question() && s.pulses());
        let working = status(r#"{"v":1,"state":"working","tool":null,"pid":null,"at":1}"#);
        assert_eq!(working.label(), "Claude \u{b7} working");
        assert!(!working.pulses());
    }

    #[test]
    fn ended_half_written_and_unknown() {
        assert_eq!(
            parse(r#"{"v":1,"state":"ended","at":1}"#),
            StatusRead::Ended
        );
        assert_eq!(
            parse(r#"{"v":2,"state":"working","at":1}"#),
            StatusRead::Ended
        );
        assert_eq!(parse(r#"{"v":1,"state":"work"#), StatusRead::Partial);
        assert_eq!(parse(""), StatusRead::Partial);
    }

    #[test]
    fn reads_only_when_the_file_changed() {
        let dir = std::env::temp_dir().join(format!("maomi-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        assert_eq!(read(&path, None).1, StatusRead::Missing);
        std::fs::write(&path, r#"{"v":1,"state":"waiting","at":5}"#).unwrap();
        let (seen, first) = read(&path, None);
        assert!(matches!(first, StatusRead::Status(_)));
        assert_eq!(read(&path, seen).1, StatusRead::Unchanged);
        _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pane_ids_are_file_name_safe_and_unique() {
        let (a, b) = (new_pane_id(), new_pane_id());
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        let env = vec![(OsString::from(PANE_ID_VAR), OsString::from(a.clone()))];
        assert_eq!(pane_id(&env), Some(a));
    }
}
