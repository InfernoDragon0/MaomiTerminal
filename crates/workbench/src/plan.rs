//! Plan, card and settings types, mirroring `shared/core/src/board.ts` in the AgentSummary repo
//! (the Workbench source of truth), plus its progress rules.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Declares an enum of known string values plus `Other` for values this build has not seen, so a
/// new Workbench release cannot break parsing of existing boards.
macro_rules! open_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            /// A value newer than this build, kept as written.
            Other(String),
        }

        impl $name {
            /// The value as written in the JSON.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)+
                    Self::Other(text) => text,
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(match Value::deserialize(deserializer)? {
                    $(Value::String(text) if text == $text => Self::$variant,)+
                    Value::String(text) => Self::Other(text),
                    other => Self::Other(other.to_string()),
                })
            }
        }
    };
}

open_enum! {
    /// Kanban column of a card. A card without a column reads as backlog.
    #[derive(Default)]
    Column {
        #[default]
        Backlog => "backlog",
        InProgress => "in-progress",
        Review => "review",
        Blocked => "blocked",
        Done => "done",
    }
}

impl Column {
    /// The known columns, left to right.
    pub const ALL: [Column; 5] = [
        Column::Backlog,
        Column::InProgress,
        Column::Review,
        Column::Blocked,
        Column::Done,
    ];
}

open_enum! {
    #[derive(Default)]
    PlanStatus {
        #[default]
        Active => "active",
        Paused => "paused",
        Done => "done",
        Archived => "archived",
    }
}

open_enum! {
    /// Estimated size of a card.
    Size {
        Xs => "XS",
        S => "S",
        M => "M",
        L => "L",
        Xl => "XL",
    }
}

open_enum! {
    /// What a review request does when the agent does not say (project or plan setting).
    ReviewPolicy {
        None => "none",
        Blocking => "blocking",
        Gates => "gates",
        Never => "never",
    }
}

/// Epoch milliseconds, revisions and similar counters: accepts floats and ignores other types
/// instead of failing the whole plan.
fn lenient_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    })
}

/// One checklist item on a card.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub text: String,
    pub done: bool,
    pub assignee: Option<String>,
    /// The user's approval ticks it; it does not count against a finished card.
    pub approval: bool,
}

/// A note on a card, optionally tagged with a phase ("design", "review", ...).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Note {
    pub id: String,
    pub author: String,
    pub phase: Option<String>,
    pub text: String,
    #[serde(deserialize_with = "lenient_i64")]
    pub at: i64,
}

/// A review request or a check request: a card handed to the user.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewRequest {
    pub by: String,
    #[serde(deserialize_with = "lenient_i64")]
    pub at: i64,
    pub reason: Option<String>,
}

/// Suggested agent for a card.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Agent {
    /// Free text such as "opus", "sonnet" or "user".
    pub model: String,
    pub reason: Option<String>,
}

/// One kanban card (a "milestone" in the JSON).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub title: String,
    /// Markdown.
    pub description: String,
    pub column: Column,
    /// Position inside the column, ascending.
    pub order: f64,
    /// Share of the plan in percent; absent means an equal share of what is left.
    pub weight: Option<f64>,
    pub tasks: Vec<Task>,
    pub assignees: Vec<String>,
    /// Manual progress in percent (0-100).
    pub progress_override: Option<f64>,
    pub notes: Vec<Note>,
    pub review: Option<ReviewRequest>,
    pub check: Option<ReviewRequest>,
    pub size: Option<Size>,
    /// Id of one of the project's [`CardType`]s.
    #[serde(rename = "type")]
    pub type_id: Option<String>,
    pub agent: Option<Agent>,
    pub depends_on: Vec<String>,
    pub files: Vec<String>,
    pub design: Vec<String>,
    /// The user signs this card off before dependent cards start.
    pub gate: bool,
    /// Linked host issue; the shape is not needed here.
    pub link: Option<Value>,
    /// Shell command that proves the card is done.
    pub verify: Option<String>,
    pub verify_result: Option<Value>,
    #[serde(deserialize_with = "lenient_i64")]
    pub created_at: i64,
    #[serde(deserialize_with = "lenient_i64")]
    pub updated_at: i64,
}

impl Card {
    /// Progress from 0.0 to 1.0. Same rule as `milestoneProgress` in board.ts: the override, else
    /// 1.0 for a done card with no open work tasks, else done tasks / tasks (0.0 without tasks).
    /// Open approval tasks do not count against a done card.
    pub fn progress(&self) -> f64 {
        self.percent() / 100.0
    }

    fn percent(&self) -> f64 {
        if let Some(manual) = self.progress_override {
            return manual.clamp(0.0, 100.0);
        }
        let open_work = self.tasks.iter().filter(|t| !t.done && !t.approval).count();
        if self.column == Column::Done && open_work == 0 {
            return 100.0;
        }
        if self.tasks.is_empty() {
            return 0.0;
        }
        let done = self.tasks.iter().filter(|t| t.done).count();
        done as f64 / self.tasks.len() as f64 * 100.0
    }

    /// Weight that counts in plan progress (`weight` only when it is above zero).
    fn positive_weight(&self) -> Option<f64> {
        self.weight.filter(|w| *w > 0.0)
    }
}

/// A plan: a brief plus its cards.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Plan {
    pub schema_version: u32,
    /// `p-xxxxxxxxxxxx`. Filled from the file name when the JSON has none.
    pub id: String,
    pub project_key: String,
    pub project_path: Option<String>,
    pub project_name: String,
    pub title: String,
    pub brief: String,
    pub status: PlanStatus,
    #[serde(rename = "milestones")]
    pub cards: Vec<Card>,
    /// Overrides the project's policy for this plan.
    pub review_policy: Option<ReviewPolicy>,
    pub created_by: String,
    #[serde(deserialize_with = "lenient_i64")]
    pub created_at: i64,
    #[serde(deserialize_with = "lenient_i64")]
    pub updated_at: i64,
    /// Incremented on every write.
    #[serde(deserialize_with = "lenient_i64")]
    pub rev: i64,
}

impl Plan {
    /// Cards of one column in display order.
    pub fn cards_in(&self, column: &Column) -> Vec<&Card> {
        let mut cards: Vec<&Card> = self.cards.iter().filter(|c| &c.column == column).collect();
        cards.sort_by(|a, b| a.order.total_cmp(&b.order));
        cards
    }

    /// Progress from 0.0 to 1.0. Same rule as `planProgress` in board.ts: cards are weighted by
    /// `weight`, and cards without one share what the weighted cards leave (at least an equal
    /// share when the weights already add up to 100 or more).
    pub fn progress(&self) -> f64 {
        if self.cards.is_empty() {
            return 0.0;
        }
        let weight_sum: f64 = self.cards.iter().filter_map(Card::positive_weight).sum();
        let unweighted = self
            .cards
            .iter()
            .filter(|c| c.positive_weight().is_none())
            .count();
        let rest = if unweighted == 0 {
            0.0
        } else {
            (100.0 - weight_sum).max(100.0 / self.cards.len() as f64) / unweighted as f64
        };
        let (mut total, mut done) = (0.0, 0.0);
        for card in &self.cards {
            let weight = card.positive_weight().unwrap_or(rest);
            total += weight;
            done += weight * card.percent() / 100.0;
        }
        if total == 0.0 {
            0.0
        } else {
            (done / total).clamp(0.0, 1.0)
        }
    }
}

/// A project-level card type ("bug", "ui", ...). Cards store its id.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CardType {
    pub id: String,
    pub label: String,
    /// Palette slot "c1" to "c5"; the UI maps it to a theme colour.
    pub color: String,
}

/// `_project.d/settings.json` of a board folder.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectSettings {
    pub review_policy: Option<ReviewPolicy>,
    pub card_types: Vec<CardType>,
}

impl ProjectSettings {
    /// The type for a card's type id. An id no type has any more reads as untyped, as in the
    /// Workbench.
    pub fn card_type(&self, type_id: Option<&str>) -> Option<&CardType> {
        let id = type_id?;
        self.card_types.iter().find(|t| t.id == id)
    }
}

/// One line of a plan's activity log.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Activity {
    #[serde(deserialize_with = "lenient_i64")]
    pub at: i64,
    pub actor: String,
    pub plan_id: String,
    pub milestone_id: Option<String>,
    /// Operation name such as "updateMilestone".
    pub op: String,
    pub summary: String,
    pub rev: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = include_str!("../tests/fixtures/plan.json");
    const SETTINGS: &str = include_str!("../tests/fixtures/settings.json");

    fn card(json: &str) -> Card {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_a_real_plan() {
        let plan: Plan = serde_json::from_str(PLAN).unwrap();
        assert_eq!(plan.id, "p-mvg50cth6ccf");
        assert_eq!(plan.status, PlanStatus::Active);
        assert_eq!(plan.cards.len(), 3);
        let first = &plan.cards[0];
        assert_eq!(first.column, Column::Done);
        assert_eq!(first.size, Some(Size::L));
        assert_eq!(first.type_id.as_deref(), Some("core"));
        assert_eq!(first.agent.as_ref().unwrap().model, "opus");
        assert_eq!(first.depends_on, ["m-mvg50ctibkma"]);
        assert!(first.files[0].contains('#'));
    }

    #[test]
    fn plan_progress_weights_cards() {
        // Weights 18 and 8 (both done) plus one unweighted open card that gets the other 74.
        let plan: Plan = serde_json::from_str(PLAN).unwrap();
        assert!((plan.progress() - 0.26).abs() < 1e-9);
    }

    #[test]
    fn cards_in_sorts_by_order() {
        let plan: Plan = serde_json::from_str(PLAN).unwrap();
        let ids: Vec<_> = plan
            .cards_in(&Column::Done)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["m-mvg50ctinvuq", "m-mvg50ctiei4b"]);
        assert!(plan.cards_in(&Column::Review).is_empty());
    }

    #[test]
    fn card_progress_rules() {
        let task = |done| format!(r#"{{"id":"t","text":"x","done":{done}}}"#);
        let with = |column: &str, tasks: &str, extra: &str| {
            card(&format!(
                r#"{{"column":"{column}","tasks":[{tasks}]{extra}}}"#
            ))
        };
        assert_eq!(with("backlog", "", "").progress(), 0.0);
        assert_eq!(with("done", "", "").progress(), 1.0);
        let half = format!("{},{}", task(true), task(false));
        assert_eq!(with("in-progress", &half, "").progress(), 0.5);
        // A done card with open work reads its task ratio.
        assert_eq!(with("done", &half, "").progress(), 0.5);
        // An open approval task does not hold a done card back.
        let approval = r#"{"id":"t","text":"ok?","done":false,"approval":true}"#;
        assert_eq!(with("done", approval, "").progress(), 1.0);
        // The override wins and is clamped.
        assert_eq!(
            with("backlog", "", r#","progressOverride":40"#).progress(),
            0.4
        );
        assert_eq!(
            with("done", "", r#","progressOverride":250.5"#).progress(),
            1.0
        );
        assert_eq!(
            with("done", "", r#","progressOverride":-3"#).progress(),
            0.0
        );
    }

    #[test]
    fn unweighted_cards_share_the_rest() {
        let plan: Plan = serde_json::from_str(
            r#"{"milestones":[
                {"column":"done"},
                {"column":"backlog"},
                {"column":"backlog","weight":0}
            ]}"#,
        )
        .unwrap();
        // No positive weights: three equal shares, one done.
        assert!((plan.progress() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(Plan::default().progress(), 0.0);
    }

    #[test]
    fn weights_over_100_still_leave_an_equal_share() {
        let plan: Plan = serde_json::from_str(
            r#"{"milestones":[
                {"column":"done","weight":150},
                {"column":"backlog"}
            ]}"#,
        )
        .unwrap();
        // The unweighted card gets max(100 - 150, 100 / 2) = 50 against 150 done.
        assert!((plan.progress() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn unknown_values_and_fields_still_parse() {
        let plan: Plan = serde_json::from_str(
            r#"{"id":"p-x","status":"frozen","futureField":{"a":1},"rev":3.0,
                "updatedAt":1792467736036.0,
                "milestones":[{"id":"m-1","column":"someday","size":"XXL","order":1.5,
                               "tasks":[{"id":"t","text":"x","done":true,"extra":1}],
                               "link":{"owner":"o"},"verifyResult":[1,2],"newThing":true}]}"#,
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::Other("frozen".into()));
        assert_eq!(plan.rev, 3);
        assert_eq!(plan.updated_at, 1_792_467_736_036);
        let card = &plan.cards[0];
        assert_eq!(card.column, Column::Other("someday".into()));
        assert_eq!(card.size, Some(Size::Other("XXL".into())));
        assert_eq!(card.column.as_str(), "someday");
        assert_eq!(card.order, 1.5);
        assert!(card.link.is_some());
        assert_eq!(plan.cards_in(&Column::Other("someday".into())).len(), 1);
    }

    #[test]
    fn card_types_resolve_and_dangle() {
        let settings: ProjectSettings = serde_json::from_str(SETTINGS).unwrap();
        assert_eq!(settings.card_types.len(), 5);
        let ui = settings.card_type(Some("ui")).unwrap();
        assert_eq!((ui.label.as_str(), ui.color.as_str()), ("ui", "c4"));
        assert!(settings.card_type(Some("deleted-type")).is_none());
        assert!(settings.card_type(None).is_none());
        assert_eq!(settings.review_policy, None);
    }
}
