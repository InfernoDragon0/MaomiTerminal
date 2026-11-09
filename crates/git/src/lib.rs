//! Git CLI runner and output parsers. Spawns the installed `git`; never links it
//! (design: "Git backend: CLI first").

pub mod cli;
pub mod conflicts;
pub mod detect;
pub mod dll_search;
pub mod error;
pub mod graph;
mod guard;
pub mod history_ops;
pub mod maintenance;
pub mod ops;
pub mod parse;
pub mod patch;
mod proctree;
pub mod progress;
pub mod redact;
pub mod repo;
pub mod runner;
pub mod setup;
pub mod signing;
pub mod sync_ops;
pub mod undo;
pub mod watch;

/// Read-only gix fast path (design: optional, behind the GitRepo trait). Not implemented yet.
#[cfg(feature = "gix")]
pub mod gix_repo {}

pub use cli::{Capped, CliRepo, DIFF_CAP, LogStream};
pub use detect::{GitBinary, GitVersion};
pub use error::{FailureKind, GitError, Result};
pub use history_ops::{ChangedFile, ResetMode};
pub use maintenance::CommitGraph;
pub use ops::{ApplyTarget, CommitRequest, StashAction};
pub use patch::{Direction, Pick};
pub use progress::Progress;
pub use repo::{DiffTarget, GitRepo, LogQuery};
pub use runner::{CommandLog, CommandRecord, GitCommand, GitOutput, GitRunner, GitStream, Mode};
pub use sync_ops::{PullMode, PushOptions, Submodule, SubmoduleState};
pub use undo::{HeadState, UndoAction};
