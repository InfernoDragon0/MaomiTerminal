//! Structured errors for running git.

use std::path::PathBuf;
use std::time::Duration;

use crate::detect::GitVersion;
use crate::parse::ParseError;

pub type Result<T, E = GitError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git was not found (looked in {} places)", searched.len())]
    NotFound { searched: Vec<PathBuf> },

    #[error("git {found} is too old; MaomiTerminal needs {required} or newer")]
    TooOld {
        found: GitVersion,
        required: GitVersion,
    },

    #[error("could not start {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("i/o error talking to git: {0}")]
    Io(#[from] std::io::Error),

    #[error("`{command}` failed ({kind:?}, exit {code:?}): {stderr_tail}")]
    Failed {
        command: String,
        code: Option<i32>,
        kind: FailureKind,
        /// Last lines of stderr, for display.
        stderr_tail: String,
    },

    #[error("`{command}` timed out after {after:?}")]
    Timeout { command: String, after: Duration },

    /// A user-supplied ref / path / URL that could be read as an option or is malformed.
    #[error("invalid {what}: {:?}", crate::redact::redact(value))]
    InvalidArgument { what: &'static str, value: String },

    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// Why a git command failed, classified from its (LC_ALL=C) stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    NotARepository,
    /// `detected dubious ownership` (safe.directory).
    DubiousOwnership,
    /// Another git process holds `index.lock` / a ref lock.
    LockHeld,
    Conflict,
    AuthFailed,
    Network,
    /// Unknown revision / path / pathspec.
    BadRevision,
    Other,
}

impl FailureKind {
    pub fn classify(stderr: &str) -> Self {
        let s = stderr.to_ascii_lowercase();
        let has = |needle: &str| s.contains(needle);
        if has("not a git repository") {
            FailureKind::NotARepository
        } else if has("dubious ownership") {
            FailureKind::DubiousOwnership
        } else if has(".lock': file exists") || has("unable to create") && has(".lock") {
            FailureKind::LockHeld
        } else if has("conflict") || has("needs merge") || has("you have unmerged paths") {
            FailureKind::Conflict
        } else if has("authentication failed")
            || has("permission denied (publickey")
            || has("could not read username")
            || has("invalid username or password")
            || has("403")
        {
            FailureKind::AuthFailed
        } else if has("could not resolve host")
            || has("unable to access")
            || has("connection timed out")
            || has("connection refused")
            || has("could not read from remote repository")
        {
            FailureKind::Network
        } else if has("unknown revision")
            || has("bad revision")
            || has("ambiguous argument")
            || has("did not match any file")
            || has("invalid object name")
        {
            FailureKind::BadRevision
        } else {
            FailureKind::Other
        }
    }
}

/// Last `max_lines` non-empty lines of stderr, lossily decoded.
pub(crate) fn stderr_tail(stderr: &[u8], max_lines: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(max_lines)..].join("\n");
    crate::redact::redact(&tail).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_failures() {
        let cases = [
            (
                "fatal: not a git repository (or any of the parent directories): .git",
                FailureKind::NotARepository,
            ),
            (
                "fatal: detected dubious ownership in repository at 'D:/x'",
                FailureKind::DubiousOwnership,
            ),
            (
                "fatal: Unable to create 'C:/r/.git/index.lock': File exists.",
                FailureKind::LockHeld,
            ),
            (
                "CONFLICT (content): Merge conflict in a.txt",
                FailureKind::Conflict,
            ),
            (
                "fatal: Authentication failed for 'https://github.com/x/y.git/'",
                FailureKind::AuthFailed,
            ),
            (
                "fatal: unable to access 'https://x/': Could not resolve host: x",
                FailureKind::Network,
            ),
            (
                "fatal: ambiguous argument 'nope': unknown revision or path",
                FailureKind::BadRevision,
            ),
            ("error: something else", FailureKind::Other),
        ];
        for (stderr, kind) in cases {
            assert_eq!(FailureKind::classify(stderr), kind, "{stderr}");
        }
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(stderr_tail(b"a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(stderr_tail(b"", 3), "");
    }
}
