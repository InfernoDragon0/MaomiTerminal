//! Access to the Maomi Workbench plan board. Has no GPUI dependency.
//!
//! Workbench keeps one JSON file per plan (plus an activity log and per-project settings) in a board
//! folder. This crate finds those folders ([`projects`]), lists and parses the plans in them and
//! computes progress the way the Workbench does. It never writes board files itself: the Workbench
//! and its MCP server own them and write them atomically, so a read always sees a whole file.
//! Changes go through [`BoardClient`], which asks that MCP server to make them.
//!
//! The format grows over time, so every field that may be missing is optional, unknown fields are
//! ignored and unknown enum values are kept as `Other` instead of failing the parse.

use std::io;
use std::path::PathBuf;

mod deeplink;
mod handoff;
mod mcp;
mod plan;
mod prompt;
mod store;
mod watch;

pub use deeplink::{DESKTOP_LINK_SCHEME, DeepLink, is_desktop_link, parse_desktop_link};
pub use handoff::{
    HANDOFF_TAGS, Handoff, HandoffAnswer, HandoffInfo, HandoffQuestion, continue_prompt,
    handoffs_path, parse_handoff_item, read_handoff, tag_counts,
};
pub use mcp::{
    BoardClient, McpError, ToolCall, answer_question_call, move_card_call, review_call,
    set_task_done_call,
};
pub use plan::{
    Activity, Agent, Card, CardType, Column, Note, Plan, PlanStatus, ProjectSettings, ReviewPolicy,
    ReviewRequest, Size, Task,
};
pub use prompt::card_prompt;
pub use store::{
    PlanSummary, Project, activity_path, list_plans, projects, projects_in, read_activity,
    read_plan, read_plans, read_settings, workbench_home,
};
pub use watch::{BoardEvent, BoardWatcher, Timing};

/// Errors from reading a board file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid board JSON: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
