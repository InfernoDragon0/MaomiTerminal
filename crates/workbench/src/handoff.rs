//! "Where we left off": the handoffs agents write when a session ends (`end_session`), kept in
//! `<planId>.d/handoffs.json` next to the plan. Mirrors `Handoff` in AgentSummary's
//! `shared/core/src/board-history.ts`; only the newest one is shown.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::store::{activity_path, read_activity};

/// A question the agent left for the person.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HandoffQuestion {
    pub id: String,
    pub text: String,
    /// Suggested answers, shown as buttons next to a free-text box.
    pub options: Vec<String>,
    pub answer: Option<HandoffAnswer>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HandoffAnswer {
    pub text: String,
    pub by: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Handoff {
    pub id: String,
    /// The session that ended (for a drafted handoff, the one that forgot `end_session`).
    pub session: String,
    pub at: i64,
    pub done: Vec<String>,
    pub next: Vec<String>,
    pub blockers: Vec<String>,
    pub questions: Vec<HandoffQuestion>,
    /// Written by the Maomi hook from the activity log because the session never handed off.
    pub drafted: bool,
}

impl Handoff {
    pub fn open_questions(&self) -> impl Iterator<Item = &HandoffQuestion> {
        self.questions.iter().filter(|q| q.answer.is_none())
    }
}

/// The newest handoff of a plan and how much happened on the board after it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HandoffInfo {
    pub handoff: Option<Handoff>,
    /// Activity lines newer than the handoff; above zero the handoff is getting stale.
    pub newer_activity: usize,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HandoffsFile {
    handoffs: Vec<Handoff>,
}

/// `<planId>.d/handoffs.json` for the plan file at `plan_path`.
pub fn handoffs_path(plan_path: &Path) -> PathBuf {
    let stem = plan_path.file_stem().unwrap_or_default().to_string_lossy();
    plan_path
        .with_file_name(format!("{stem}.d"))
        .join("handoffs.json")
}

/// The plan's newest handoff (by time) and the activity after it. A missing or unreadable file
/// reads as no handoff.
pub fn read_handoff(plan_path: &Path) -> HandoffInfo {
    let path = handoffs_path(plan_path);
    let handoff = std::fs::read(&path)
        .ok()
        .and_then(|bytes| {
            let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
            serde_json::from_slice::<HandoffsFile>(bytes)
                .map_err(|err| tracing::warn!("cannot read {}: {err}", path.display()))
                .ok()
        })
        .and_then(|file| file.handoffs.into_iter().max_by_key(|h| h.at));
    let newer_activity = handoff.as_ref().map_or(0, |h| {
        read_activity(&activity_path(plan_path), usize::MAX)
            .iter()
            .filter(|a| a.at > h.at)
            .count()
    });
    HandoffInfo {
        handoff,
        newer_activity,
    }
}

/// Bullet tags agents start handoff items with, in display order.
pub const HANDOFF_TAGS: [&str; 8] = [
    "added", "changed", "fixed", "removed", "decided", "user", "review", "note",
];

/// Splits `[tag] text` into its known tag (case-insensitive) and the text; an unknown or missing
/// tag leaves the item as plain text.
pub fn parse_handoff_item(item: &str) -> (Option<&'static str>, &str) {
    let trimmed = item.trim_start();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((tag, text)) = rest.split_once(']')
        && let Some(known) = HANDOFF_TAGS
            .iter()
            .find(|t| t.eq_ignore_ascii_case(tag.trim()))
    {
        return (Some(known), text.trim());
    }
    (None, item.trim())
}

/// Items per tag, in [`HANDOFF_TAGS`] order, only the tags present.
pub fn tag_counts(items: &[String]) -> Vec<(&'static str, usize)> {
    HANDOFF_TAGS
        .iter()
        .filter_map(|tag| {
            let n = items
                .iter()
                .filter(|i| parse_handoff_item(i).0 == Some(*tag))
                .count();
            (n > 0).then_some((*tag, n))
        })
        .collect()
}

/// The prompt that picks a plan up in a fresh session (`continuePrompt` in the Workbench).
pub fn continue_prompt(plan_id: &str, title: &str) -> String {
    format!(
        "Continue plan \"{title}\" ({plan_id}) on the Maomi board: open_plan {{plan: \"{plan_id}\"}}, \
         then frontier {{plan}} with your own agent limits, then claim {{start: true}} the card it \
         ranks first."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_split_into_tag_and_text() {
        assert_eq!(
            parse_handoff_item("[added] New crate"),
            (Some("added"), "New crate")
        );
        assert_eq!(parse_handoff_item(" [FIXED]  Bug "), (Some("fixed"), "Bug"));
        assert_eq!(parse_handoff_item("[weird] keep"), (None, "[weird] keep"));
        assert_eq!(parse_handoff_item("plain"), (None, "plain"));
        let items = ["[added] a", "[note] b", "[added] c", "d"].map(String::from);
        assert_eq!(tag_counts(&items), vec![("added", 2), ("note", 1)]);
    }

    #[test]
    fn prompt_matches_the_workbench() {
        assert_eq!(
            continue_prompt("p-1", "Boards"),
            "Continue plan \"Boards\" (p-1) on the Maomi board: open_plan {plan: \"p-1\"}, then \
             frontier {plan} with your own agent limits, then claim {start: true} the card it ranks \
             first."
        );
    }

    #[test]
    fn newest_handoff_and_activity_after_it() {
        let dir = std::env::temp_dir().join(format!("maomi-handoff-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p-1.d")).unwrap();
        let plan = dir.join("p-1.json");
        std::fs::write(
            dir.join("p-1.d").join("handoffs.json"),
            r#"{"schemaVersion":1,"handoffs":[
                {"id":"h-old","session":"s","at":100,"done":[],"next":["old"],"blockers":[],"questions":[]},
                {"id":"h-new","session":"s","at":200,"done":["[added] x"],"next":["[user] test it"],
                 "blockers":[],"questions":[{"id":"q-1","text":"Ok?","options":["Yes","No"]},
                 {"id":"q-2","text":"Done?","answer":{"text":"yes","by":"user","at":250}}],"newField":1}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("p-1.activity.jsonl"),
            "{\"at\":150,\"actor\":\"s\",\"planId\":\"p-1\",\"op\":\"x\",\"summary\":\"a\"}\n\
             {\"at\":300,\"actor\":\"s\",\"planId\":\"p-1\",\"op\":\"x\",\"summary\":\"b\"}\n",
        )
        .unwrap();
        let info = read_handoff(&plan);
        let h = info.handoff.unwrap();
        assert_eq!(h.id, "h-new");
        assert_eq!(h.open_questions().count(), 1);
        assert_eq!(h.questions[0].options, ["Yes", "No"]);
        assert_eq!(info.newer_activity, 1);
        assert_eq!(read_handoff(&dir.join("p-2.json")), HandoffInfo::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
