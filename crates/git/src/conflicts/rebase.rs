//! Interactive rebase without an editor: the app writes the todo list itself and hands it
//! to git through `GIT_SEQUENCE_EDITOR` (a `cp` of our file over git's, run by git's own
//! shell). Rewording is a `pick` followed by `exec git commit --amend -F <file>`, so no
//! editor ever opens. Only validated hex commit ids reach the todo; messages travel in
//! files, never on a command line.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    /// Keep the changes, use this message.
    Reword(String),
    /// Meld into the previous commit, keeping both messages.
    Squash,
    /// Meld into the previous commit, dropping this message.
    Fixup,
    Drop,
}

impl TodoAction {
    pub fn label(&self) -> &'static str {
        match self {
            TodoAction::Pick => "Pick",
            TodoAction::Reword(_) => "Reword",
            TodoAction::Squash => "Squash",
            TodoAction::Fixup => "Fixup",
            TodoAction::Drop => "Drop",
        }
    }
}

/// One line of the plan, oldest commit first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub commit: String,
    pub action: TodoAction,
}

/// A commit the rebase would replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseCommit {
    pub id: String,
    pub subject: String,
    /// Full message (for rewording).
    pub message: String,
}

/// The commits between `base` and HEAD that an interactive rebase replays (oldest first),
/// and how many merge commits it would flatten.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebasePlan {
    pub commits: Vec<RebaseCommit>,
    pub merges: usize,
}

fn is_commit_id(id: &str) -> bool {
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `'text'` for a POSIX shell.
pub(crate) fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A path as git's shell sees it (forward slashes work on Windows too).
pub(crate) fn sh_path(path: &Path) -> String {
    sh_quote(&path.to_string_lossy().replace('\\', "/"))
}

/// The todo text plus the message files it refers to (written by the caller into `dir`).
pub(crate) fn build_todo(
    items: &[TodoItem],
    dir: &Path,
) -> Result<(String, Vec<(PathBuf, String)>)> {
    let invalid = |value: String| GitError::InvalidArgument {
        what: "rebase plan",
        value,
    };
    let mut todo = String::new();
    let mut messages = Vec::new();
    let mut kept_any = false;
    for (i, item) in items.iter().enumerate() {
        if !is_commit_id(&item.commit) {
            return Err(invalid(item.commit.clone()));
        }
        let verb = match &item.action {
            TodoAction::Pick | TodoAction::Reword(_) => "pick",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        };
        if matches!(item.action, TodoAction::Squash | TodoAction::Fixup) && !kept_any {
            return Err(invalid(format!(
                "{} {}: nothing earlier to meld into",
                item.action.label(),
                item.commit
            )));
        }
        kept_any |= !matches!(item.action, TodoAction::Drop);
        todo.push_str(&format!("{verb} {}\n", item.commit));
        if let TodoAction::Reword(message) = &item.action {
            let message = message.trim();
            if message.is_empty() {
                return Err(invalid(format!("empty message for {}", item.commit)));
            }
            let file = dir.join(format!("message-{i}.txt"));
            todo.push_str(&format!(
                "exec git commit --amend --allow-empty --quiet -F {}\n",
                sh_path(&file)
            ));
            messages.push((file, format!("{message}\n")));
        }
    }
    if !kept_any {
        return Err(invalid("every commit is dropped".into()));
    }
    Ok((todo, messages))
}

/// `git log` records: `%H %x1f %s %x1f %B %x1e`.
pub(crate) fn parse_plan(log: &[u8]) -> Vec<RebaseCommit> {
    String::from_utf8_lossy(log)
        .split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches(['\n', '\r']).splitn(3, '\x1f');
            let id = fields.next()?.trim();
            if !is_commit_id(id) {
                return None;
            }
            Some(RebaseCommit {
                id: id.to_string(),
                subject: fields.next().unwrap_or_default().to_string(),
                message: fields.next().unwrap_or_default().trim_end().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(commit: &str, action: TodoAction) -> TodoItem {
        TodoItem {
            commit: commit.into(),
            action,
        }
    }

    #[test]
    fn todo_text() {
        let dir = Path::new("C:\\Users\\o'brien\\repo\\.git\\gitdragon");
        let (todo, messages) = build_todo(
            &[
                item("aaaa111", TodoAction::Pick),
                item("bbbb222", TodoAction::Reword("New subject\n\nBody".into())),
                item("cccc333", TodoAction::Fixup),
                item("dddd444", TodoAction::Drop),
            ],
            dir,
        )
        .unwrap();
        assert_eq!(
            todo,
            "pick aaaa111\npick bbbb222\nexec git commit --amend --allow-empty --quiet -F \
             'C:/Users/o'\\''brien/repo/.git/gitdragon/message-1.txt'\nfixup cccc333\ndrop dddd444\n"
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].1, "New subject\n\nBody\n");
    }

    #[test]
    fn rejects_bad_plans() {
        let dir = Path::new("/tmp");
        let bad = |items: &[TodoItem]| build_todo(items, dir).is_err();
        assert!(bad(&[item("aaaa111\nexec rm -rf /", TodoAction::Pick)]));
        assert!(bad(&[item("--root", TodoAction::Pick)]));
        assert!(bad(&[item("aaaa111", TodoAction::Squash)]));
        assert!(bad(&[
            item("aaaa111", TodoAction::Drop),
            item("bbbb222", TodoAction::Fixup)
        ]));
        assert!(bad(&[item("aaaa111", TodoAction::Drop)]));
        assert!(bad(&[item("aaaa111", TodoAction::Reword("  ".into()))]));
        assert!(!bad(&[
            item("aaaa111", TodoAction::Drop),
            item("bbbb222", TodoAction::Pick)
        ]));
    }

    #[test]
    fn plan_records() {
        let log = b"aaaa111\x1ffirst\x1ffirst\n\nbody\n\x1e\nbbbb222\x1fsecond\x1fsecond\n\x1e\n";
        let plan = parse_plan(log);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].message, "first\n\nbody");
        assert_eq!(plan[1].subject, "second");
    }
}
