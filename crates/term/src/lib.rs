//! Terminal backend: PTY sessions and terminal emulation. Has no GPUI dependency.

mod integration;
mod osc;
pub mod session;
pub mod shell;
pub mod snapshot;

pub use session::{
    EventProxy, PromptState, PtySession, QueryPalette, SessionOptions, TermEvent, TermSize,
};
pub use shell::{Shell, ShellKind, available_shells, default_shell};
pub use snapshot::{CellColor, GridSnapshot, SnapshotCell, SnapshotCursor};

// alacritty_terminal types that appear in this crate's API (cell flags, modes, colours, cursor
// shapes, selection), so callers need no direct dependency on it.
pub use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
pub use alacritty_terminal::{self, term::TermMode, term::cell::Flags as CellFlags};
