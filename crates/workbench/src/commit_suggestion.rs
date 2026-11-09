//! The commit the board suggests for the last session: the tasks it ticked and the cards it
//! finished, as a title (≤ 72 chars), up to six bullets and a `Maomi-Session:` trailer. A port
//! of AgentSummary's `commitSuggestion.ts` and the activity reader in `commitSuggestionNode.ts`.
//! Reading never fails: anything missing or damaged reads as no suggestion.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::handoff::newest_handoff;
use crate::plan::{Activity, Plan};
use crate::store::{activity_path, read_activity, read_plans};

/// Longest commit title (the usual git convention).
pub const COMMIT_TITLE_MAX: usize = 72;
/// Most bullets in the body; the rest is summed up as `+N more`.
pub const COMMIT_BODY_BULLETS: usize = 6;
/// Longest bullet (one card + one task).
const BULLET_MAX: usize = 100;
/// Activity lines read per plan.
const ACTIVITY_LINES: usize = 2000;
/// The trailer that ties a commit to its session.
pub const SESSION_TRAILER: &str = "Maomi-Session:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSuggestion {
    pub title: String,
    /// Bullets, optional `+N more`, a blank line, then the `Maomi-Session:` trailer.
    pub body: String,
    pub session_id: String,
    /// When the session ended (epoch ms): a commit made at or after it retires the suggestion.
    pub ended_at: i64,
    /// Set when every card is from one plan.
    pub plan_id: Option<String>,
    /// Cards the session touched.
    pub cards: usize,
    /// Tasks the session ticked.
    pub tasks: usize,
}

impl CommitSuggestion {
    /// Title, blank line, body.
    pub fn text(&self) -> String {
        format!("{}\n\n{}", self.title, self.body)
    }

    /// The trailer line this suggestion's commit carries.
    pub fn trailer(&self) -> String {
        format!("{SESSION_TRAILER} {}", self.session_id)
    }
}

/// One card as a session changed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuggestionCard {
    pub title: String,
    /// Texts of the tasks the session ticked on this card.
    pub tasks_ticked: Vec<String>,
    /// The session finished the card (moved it to review or done).
    pub completed: bool,
    /// It went to review, not to done: the bullet says "(in review)".
    pub review: bool,
}

/// Collapses whitespace to single spaces and cuts to `max` characters with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max {
        let cut: String = t.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        t
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// The suggested commit, or `None` when nothing was ticked and no card finished. Title: the
/// card's title when one card was touched (`<card>: n tasks` while it is unfinished), else
/// `<plan>: n tasks across m cards`.
pub fn build_commit_suggestion(
    plan_title: &str,
    cards: &[SuggestionCard],
    session_id: &str,
    ended_at: i64,
    plan_id: Option<&str>,
) -> Option<CommitSuggestion> {
    let cards: Vec<SuggestionCard> = cards
        .iter()
        .map(|c| {
            let mut tasks: Vec<String> = Vec::new();
            for t in &c.tasks_ticked {
                let t = one_line(t, BULLET_MAX);
                if !t.is_empty() && !tasks.contains(&t) {
                    tasks.push(t);
                }
            }
            SuggestionCard {
                title: one_line(&c.title, BULLET_MAX),
                tasks_ticked: tasks,
                completed: c.completed,
                review: c.review,
            }
        })
        .filter(|c| !c.title.is_empty() && (!c.tasks_ticked.is_empty() || c.completed))
        .collect();
    let first = cards.first()?;
    let tasks: usize = cards.iter().map(|c| c.tasks_ticked.len()).sum();

    let title = if cards.len() == 1 {
        if first.completed {
            one_line(&first.title, COMMIT_TITLE_MAX)
        } else {
            let tail = format!(": {}", plural(tasks, "task"));
            let room = COMMIT_TITLE_MAX
                .saturating_sub(tail.chars().count())
                .max(8);
            format!("{}{tail}", one_line(&first.title, room))
        }
    } else {
        let plan = match plan_title.trim() {
            "" => "Maomi",
            t => t,
        };
        one_line(
            &format!("{plan}: {} across {} cards", plural(tasks, "task"), cards.len()),
            COMMIT_TITLE_MAX,
        )
    };

    let mut bullets = Vec::new();
    for c in &cards {
        if c.tasks_ticked.is_empty() {
            let state = if c.review { "in review" } else { "done" };
            bullets.push(format!("- {} ({state})", c.title));
        } else {
            for t in &c.tasks_ticked {
                bullets.push(format!("- {}", one_line(&format!("{}: {t}", c.title), BULLET_MAX)));
            }
        }
    }
    let mut lines: Vec<String> = bullets.iter().take(COMMIT_BODY_BULLETS).cloned().collect();
    if bullets.len() > COMMIT_BODY_BULLETS {
        lines.push(format!("+{} more", bullets.len() - COMMIT_BODY_BULLETS));
    }
    lines.push(String::new());
    lines.push(format!("{SESSION_TRAILER} {session_id}"));

    Some(CommitSuggestion {
        title,
        body: lines.join("\n"),
        session_id: session_id.to_string(),
        ended_at,
        plan_id: plan_id.map(str::to_string),
        cards: cards.len(),
        tasks,
    })
}

// ---- reading the board ------------------------------------------------------------------------

/// `task '<text>' on '<card>': done, ...`: the greedy text ends at the last `' on '`.
static TASK_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^task '(.*)' on '(.*)': (.*)$").expect("valid regex"));
static MOVED_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^moved '(.*)' to (?:done|review)$").expect("valid regex"));
/// `finished '<card>', for the user to check (...)` / `asked the user to review '<card>' (...)`.
static REVIEW_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^(?:finished|asked the user to review) '(.*?)'(?:,| \()").expect("valid regex")
});

struct CardTicks {
    title: String,
    /// Task text and whether it is still ticked (a later untick clears it), in tick order.
    ticked: Vec<(String, bool)>,
    completed: bool,
    review: bool,
}

/// One plan's cards as `session` changed them, in the order the session first touched them.
fn ticks_of(activity: &[Activity], session: &str, plan: Option<&Plan>) -> Vec<SuggestionCard> {
    let mut order: Vec<String> = Vec::new();
    let mut cards: HashMap<String, CardTicks> = HashMap::new();
    let mut card = |line: &Activity, fallback: &str| -> String {
        let id = line
            .milestone_id
            .clone()
            .unwrap_or_else(|| fallback.to_string());
        if !cards.contains_key(&id) {
            let known = plan.and_then(|p| {
                p.cards
                    .iter()
                    .find(|c| Some(&c.id) == line.milestone_id.as_ref())
                    .map(|c| c.title.clone())
            });
            cards.insert(
                id.clone(),
                CardTicks {
                    title: known.unwrap_or_else(|| fallback.to_string()),
                    ticked: Vec::new(),
                    completed: false,
                    review: false,
                },
            );
            order.push(id.clone());
        }
        id
    };
    let mut changes: Vec<(String, Change)> = Vec::new();
    for line in activity.iter().filter(|l| l.actor == session) {
        match line.op.as_str() {
            "updateTask" => {
                let Some(m) = TASK_LINE.captures(&line.summary) else {
                    continue;
                };
                let state = m[3].split(", ").next().unwrap_or_default();
                let done = if state.starts_with("not done") {
                    false
                } else if state.starts_with("done") {
                    true
                } else {
                    continue;
                };
                let id = card(line, &m[2]);
                changes.push((id, Change::Tick(m[1].to_string(), done)));
            }
            "moveMilestone" => {
                if let Some(m) = MOVED_LINE.captures(&line.summary) {
                    let id = card(line, &m[1]);
                    changes.push((id, Change::Finished { review: false }));
                }
            }
            "requestReview" => {
                if let Some(m) = REVIEW_LINE.captures(&line.summary) {
                    let id = card(line, &m[1]);
                    changes.push((id, Change::Finished { review: true }));
                }
            }
            _ => {}
        }
    }
    for (id, change) in changes {
        let Some(c) = cards.get_mut(&id) else { continue };
        match change {
            Change::Tick(text, done) => match c.ticked.iter_mut().find(|(t, _)| *t == text) {
                Some(entry) => entry.1 = done,
                None => c.ticked.push((text, done)),
            },
            Change::Finished { review } => {
                c.completed = true;
                c.review = review;
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| cards.remove(&id))
        .map(|c| SuggestionCard {
            title: c.title,
            tasks_ticked: c
                .ticked
                .into_iter()
                .filter(|(_, on)| *on)
                .map(|(t, _)| t)
                .collect(),
            completed: c.completed,
            review: c.review,
        })
        .collect()
}

enum Change {
    Tick(String, bool),
    Finished { review: bool },
}

/// The suggestion for one session from the ticks it wrote to `plan_id`, or to every plan in
/// `board_dir` when `None`. Activity before `since` (epoch ms) was committed already and is left
/// out. `project` names the commit when cards come from several plans.
pub fn read_session_suggestion(
    board_dir: &Path,
    project: &str,
    session: &str,
    plan_id: Option<&str>,
    ended_at: i64,
    since: i64,
) -> Option<CommitSuggestion> {
    if session.is_empty() {
        return None;
    }
    let mut cards = Vec::new();
    let mut titles = Vec::new();
    let mut only: Option<String> = None;
    let mut several = false;
    for (path, plan) in read_plans(board_dir) {
        if plan_id.is_some_and(|id| id != plan.id) {
            continue;
        }
        let activity: Vec<Activity> = read_activity(&activity_path(&path), ACTIVITY_LINES)
            .into_iter()
            .filter(|l| l.at >= since)
            .collect();
        if !activity.iter().any(|l| l.actor == session) {
            continue;
        }
        let found = ticks_of(&activity, session, Some(&plan));
        if found.is_empty() {
            continue;
        }
        cards.extend(found);
        titles.push(plan.title.clone());
        several |= only.is_some();
        only = Some(plan.id.clone());
    }
    let plan_title = match titles.as_slice() {
        [one] => one.as_str(),
        _ => project,
    };
    let plan_id = if several { None } else { only };
    build_commit_suggestion(plan_title, &cards, session, ended_at, plan_id.as_deref())
}

/// The suggestion for the most recently ended session (the newest handoff across the project
/// and its plans), counting activity from `since` (epoch ms) on.
pub fn latest_session_suggestion(
    board_dir: &Path,
    project: &str,
    since: i64,
) -> Option<CommitSuggestion> {
    let mut scopes: Vec<PathBuf> = vec![board_dir.join("_project.json")];
    scopes.extend(read_plans(board_dir).into_iter().map(|(path, _)| path));
    let best = scopes
        .iter()
        .filter_map(|p| newest_handoff(p))
        .filter(|h| !h.session.is_empty())
        .max_by_key(|h| h.at)?;
    // The commit covers everything the session wrote, whichever scope it handed off in.
    read_session_suggestion(board_dir, project, &best.session, None, best.at, since)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "agent-session-a";
    const OTHER: &str = "agent-session-b";

    fn card(title: &str, tasks: &[&str], completed: bool) -> SuggestionCard {
        SuggestionCard {
            title: title.into(),
            tasks_ticked: tasks.iter().map(|t| t.to_string()).collect(),
            completed,
            review: false,
        }
    }

    fn build(cards: &[SuggestionCard]) -> Option<CommitSuggestion> {
        build_commit_suggestion("Repo tab", cards, SESSION, 1, None)
    }

    #[test]
    fn finished_card_title_one_bullet_per_task_then_trailer() {
        let s = build(&[card("Branch switcher", &["Menu", "Tests"], true)]).unwrap();
        assert_eq!(s.title, "Branch switcher");
        assert_eq!((s.cards, s.tasks), (1, 2));
        assert_eq!(
            s.body,
            format!("- Branch switcher: Menu\n- Branch switcher: Tests\n\nMaomi-Session: {SESSION}")
        );
    }

    #[test]
    fn several_cards_sum_up_as_tasks_across_cards() {
        let s = build_commit_suggestion(
            "Repo tab",
            &[card("A", &["one"], false), card("B", &["two", "three"], true)],
            SESSION,
            1,
            Some("p-abc"),
        )
        .unwrap();
        assert_eq!(s.title, "Repo tab: 3 tasks across 2 cards");
        assert_eq!(s.plan_id.as_deref(), Some("p-abc"));
        assert_eq!((s.cards, s.tasks), (2, 3));
    }

    #[test]
    fn done_and_review_without_ticks_and_partial_single_card() {
        let done = build(&[card("Quick fix", &[], true)]).unwrap();
        assert!(done.body.starts_with("- Quick fix (done)\n"));
        assert_eq!(done.tasks, 0);
        let mut review = card("Review me", &[], true);
        review.review = true;
        assert!(build(&[review]).unwrap().body.starts_with("- Review me (in review)\n"));
        assert_eq!(build(&[card("Big card", &["a"], false)]).unwrap().title, "Big card: 1 task");
    }

    #[test]
    fn title_is_capped_at_72_with_an_ellipsis() {
        let long = "A".repeat(120);
        let one = build(&[card(&long, &[], true)]).unwrap();
        assert_eq!(one.title.chars().count(), COMMIT_TITLE_MAX);
        assert!(one.title.ends_with('…'));
        let many = build_commit_suggestion(
            &"P".repeat(100),
            &[card("a", &["x"], false), card("b", &["y"], false)],
            SESSION,
            1,
            None,
        )
        .unwrap();
        assert!(many.title.chars().count() <= COMMIT_TITLE_MAX);
        let partial = build(&[card(&long, &["x"], false)]).unwrap();
        assert!(partial.title.chars().count() <= COMMIT_TITLE_MAX);
        assert!(partial.title.ends_with(": 1 task"));
    }

    #[test]
    fn six_bullets_at_most_then_plus_n_more() {
        let tasks: Vec<String> = (1..=9).map(|i| format!("task {i}")).collect();
        let refs: Vec<&str> = tasks.iter().map(String::as_str).collect();
        let s = build(&[card("Card", &refs, true)]).unwrap();
        let lines: Vec<&str> = s.body.split('\n').collect();
        assert_eq!(lines.iter().filter(|l| l.starts_with("- ")).count(), COMMIT_BODY_BULLETS);
        assert_eq!(lines[COMMIT_BODY_BULLETS], "+3 more");
        assert_eq!(lines[lines.len() - 1], format!("Maomi-Session: {SESSION}"));
        assert_eq!(lines[lines.len() - 2], "");
        assert_eq!(s.tasks, 9);
        let six = build(&[card("C", &refs[..6], true)]).unwrap();
        assert!(!six.body.contains("more"));
    }

    #[test]
    fn none_when_nothing_ticked_or_finished() {
        assert_eq!(build(&[]), None);
        assert_eq!(build(&[card("A", &[], false)]), None);
        assert_eq!(build(&[card("A", &["  "], false)]), None);
    }

    #[test]
    fn one_line_fields_and_repeated_ticks_once() {
        let s = build(&[card("Card\nwith\nbreaks", &["same", "same"], true)]).unwrap();
        assert_eq!(s.tasks, 1);
        assert_eq!(s.title, "Card with breaks");
        assert_eq!(s.text(), format!("{}\n\n{}", s.title, s.body));
    }

    // ---- the reader over a board folder (summaries as the board writes them) -------------------

    const LONG: &str =
        "Branch switcher with a long enough title that the activity summary line cuts it off here";

    fn line(at: i64, actor: &str, card: &str, op: &str, summary: &str) -> String {
        serde_json::json!({
            "at": at, "actor": actor, "planId": "p-1", "milestoneId": card, "op": op,
            "summary": summary,
        })
        .to_string()
    }

    fn board() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maomi-commit-suggestion-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(dir.join("p-1.d")).unwrap();
        std::fs::write(
            dir.join("p-1.json"),
            serde_json::json!({
                "id": "p-1", "title": "Repo tab", "status": "active", "milestones": [
                    {"id": "m-a", "title": LONG, "column": "review", "tasks": []},
                    {"id": "m-b", "title": "Fetch button", "column": "backlog", "tasks": []}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let cut = "Branch switcher with a long enough title that the activity summ…";
        let lines = [
            line(10, SESSION, "m-a", "updateTask", &format!("task 'Menu' on '{cut}': done")),
            line(20, SESSION, "m-a", "updateTask", &format!("task 'Wrong' on '{cut}': done")),
            line(30, SESSION, "m-a", "updateTask", &format!("task 'Wrong' on '{cut}': not done")),
            line(40, SESSION, "m-a", "updateTask", &format!("task 'Tests' on '{cut}': done, by x")),
            line(50, SESSION, "m-a", "moveMilestone", &format!("moved '{cut}' to review")),
            line(60, SESSION, "m-b", "updateTask", "task 'Spinner' on 'Fetch button': done"),
            line(70, OTHER, "m-b", "updateTask", "task 'Foreign' on 'Fetch button': done"),
        ];
        std::fs::write(dir.join("p-1.activity.jsonl"), lines.join("\n") + "\n").unwrap();
        std::fs::write(
            dir.join("p-1.d").join("handoffs.json"),
            format!(
                r#"{{"handoffs":[{{"id":"h-1","session":"{OTHER}","at":80}},{{"id":"h-2","session":"{SESSION}","at":90}}]}}"#
            ),
        )
        .unwrap();
        dir
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn reads_one_sessions_ticks_and_finished_cards() {
        let dir = board();
        let s = read_session_suggestion(&dir, "Demo", SESSION, None, 100, 0).unwrap();
        assert_eq!((s.cards, s.tasks), (2, 3));
        assert_eq!(s.plan_id.as_deref(), Some("p-1"));
        assert_eq!(s.title, "Repo tab: 3 tasks across 2 cards");
        let lines: Vec<&str> = s.body.split('\n').collect();
        // Full card titles come from the plan; "Wrong" was unticked; the other session's tick is out.
        let menu = one_line(&format!("{LONG}: Menu"), BULLET_MAX);
        assert!(lines.contains(&format!("- {menu}").as_str()));
        assert!(!lines.iter().any(|l| l.contains("Wrong") || l.contains("Foreign")));
        assert!(lines.contains(&"- Fetch button: Spinner"));
        assert_eq!(lines[lines.len() - 1], format!("Maomi-Session: {SESSION}"));

        let other = read_session_suggestion(&dir, "Demo", OTHER, Some("p-1"), 100, 0).unwrap();
        assert_eq!((other.cards, other.tasks), (1, 1));
        assert_eq!(other.title, "Fetch button: 1 task");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn none_for_unknown_session_plan_or_folder() {
        let dir = board();
        assert_eq!(read_session_suggestion(&dir, "Demo", "nobody", None, 1, 0), None);
        assert_eq!(read_session_suggestion(&dir, "Demo", "", None, 1, 0), None);
        assert_eq!(read_session_suggestion(&dir, "Demo", SESSION, Some("p-x"), 1, 0), None);
        assert_eq!(
            read_session_suggestion(&dir.join("missing"), "Demo", SESSION, None, 1, 0),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_out_ticks_before_heads_commit() {
        let dir = board();
        let later = read_session_suggestion(&dir, "Demo", SESSION, None, 100, 60).unwrap();
        assert_eq!(later.title, "Fetch button: 1 task");
        assert_eq!(read_session_suggestion(&dir, "Demo", SESSION, None, 100, 1000), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn latest_follows_the_newest_handoff() {
        let dir = board();
        let s = latest_session_suggestion(&dir, "Demo", 0).unwrap();
        assert_eq!(s.session_id, SESSION);
        assert_eq!(s.tasks, 3);
        assert_eq!(s.ended_at, 90);
        assert_eq!(latest_session_suggestion(&dir.join("missing"), "Demo", 0), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
