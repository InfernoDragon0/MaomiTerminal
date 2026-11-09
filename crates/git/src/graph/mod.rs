//! Commit graph model: lane layout over a child-before-parent commit stream (design
//! "Performance & app shell": graph 60 fps on 1M commits).

pub mod history;
pub mod layout;

pub use history::{Author, CommitRow, History, Pseudo, RowKind};
pub use layout::{Edge, EdgeKind, GraphLayout, Node, OPEN, Span};
