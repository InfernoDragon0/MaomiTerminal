//! File paths in terminal output: the token under a cell, its `:line:col` suffix, and where
//! it points. Whether the path exists is checked elsewhere, off the UI thread.

use std::path::{Path, PathBuf};

/// The path-looking token around `col`: columns `start..end` (the suffix included, so the
/// underline covers it) and the path text without a `:line[:col]` or `(line[,col])` suffix.
/// Paths with spaces are found only inside quotes.
pub(crate) fn path_at(text: &[char], col: usize) -> Option<(usize, usize, String)> {
    if text.get(col).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let (mut start, mut end) = quoted_path(text, col).unwrap_or_else(|| bare_token(text, col));
    while start < end && matches!(text[start], '(' | '[' | '{' | '<') {
        start += 1;
    }
    while end > start {
        let last = text[end - 1];
        let strip = match last {
            ',' | ';' | ':' | '!' | '?' | ']' | '}' | '>' => true,
            // `file.cs(12,5)` keeps its location; other closing parens wrap the path.
            ')' => location_paren(&text[start..end]).is_none(),
            // A full stop after a name; `..` and `dir/.` stay.
            '.' => end - start >= 2 && text[end - 2].is_alphanumeric(),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= 1;
    }
    if !(start..end).contains(&col) {
        return None;
    }
    let token: String = text[start..end].iter().collect();
    let path = strip_location(&token);
    if path.is_empty()
        || path.contains("://")
        || !path.chars().any(|c| c.is_alphanumeric() || c == '.')
    {
        return None;
    }
    Some((start, end, path.to_owned()))
}

/// Where `raw` points: `~` is the home folder, relative paths start at `cwd`.
pub(crate) fn resolve(raw: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if raw == "~" {
        return std::env::home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return std::env::home_dir().map(|home| home.join(rest));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    // On Windows `\dir` is relative to the drive; joining keeps the cwd's drive.
    cwd.map(|cwd| cwd.join(path))
}

/// Contents of a `"…"` or `'…'` pair around `col` when they start like a path.
fn quoted_path(text: &[char], col: usize) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < text.len() {
        let quote = text[i];
        // An apostrophe inside a word (`don't`) opens nothing.
        let opens = quote == '"' || (quote == '\'' && (i == 0 || !text[i - 1].is_alphanumeric()));
        if !opens {
            i += 1;
            continue;
        }
        let len = text[i + 1..].iter().position(|&c| c == quote)?;
        let (start, end) = (i + 1, i + 1 + len);
        if (start..end).contains(&col) {
            return starts_like_path(&text[start..end]).then_some((start, end));
        }
        i = end + 1;
    }
    None
}

/// Drive, UNC, home, absolute or dot-relative start: worth reading past spaces for.
fn starts_like_path(text: &[char]) -> bool {
    match text {
        [d, ':', '\\' | '/', ..] => d.is_ascii_alphabetic(),
        ['\\' | '/', ..] | ['~', ..] | ['.', ..] => true,
        _ => false,
    }
}

fn bare_token(text: &[char], col: usize) -> (usize, usize) {
    let stop = |c: &char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '|');
    let start = text[..col].iter().rposition(stop).map_or(0, |i| i + 1);
    let end = text[col..]
        .iter()
        .position(stop)
        .map_or(text.len(), |i| col + i);
    (start, end)
}

/// Start of a trailing `(12)` or `(12,5)`.
fn location_paren(text: &[char]) -> Option<usize> {
    let (&')', body) = text.split_last()? else {
        return None;
    };
    let open = body.iter().rposition(|&c| c == '(')?;
    let mut parts = body[open + 1..].split(|&c| c == ',');
    let digits = |p: &[char]| !p.is_empty() && p.iter().all(char::is_ascii_digit);
    let ok = open > 0
        && parts.next().is_some_and(digits)
        && parts.next().is_none_or(digits)
        && parts.next().is_none();
    ok.then_some(open)
}

/// `src/main.rs:42:7`, `a.py:3` and `x.cs(12,5)` without the location.
fn strip_location(token: &str) -> &str {
    let chars: Vec<char> = token.chars().collect();
    if let Some(open) = location_paren(&chars) {
        return &token[..token
            .char_indices()
            .nth(open)
            .map_or(token.len(), |(i, _)| i)];
    }
    let mut path = token;
    for _ in 0..2 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        // `C:` is a drive, not a line number.
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) || head.len() <= 1 {
            break;
        }
        path = head;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: &str, needle: &str) -> Option<String> {
        let text: Vec<char> = line.chars().collect();
        let col = line[..line.find(needle).unwrap()].chars().count();
        path_at(&text, col).map(|(_, _, path)| path)
    }

    #[test]
    fn windows_paths() {
        assert_eq!(
            at(r"open C:\Users\me\notes.txt now", "Users").as_deref(),
            Some(r"C:\Users\me\notes.txt")
        );
        assert_eq!(
            at("see C:/tmp/a.log.", "tmp").as_deref(),
            Some("C:/tmp/a.log")
        );
        assert_eq!(
            at(r"\\server\share\x.doc", "share").as_deref(),
            Some(r"\\server\share\x.doc")
        );
        assert_eq!(
            at(r"At C:\s\run.ps1:12 char:5", "run").as_deref(),
            Some(r"C:\s\run.ps1")
        );
    }

    #[test]
    fn relative_and_home_paths() {
        assert_eq!(
            at("--> src/main.rs:42:7", "main").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            at(r"run .\tools\x.ps1", "tools").as_deref(),
            Some(r".\tools\x.ps1")
        );
        assert_eq!(at("cd ..", "..").as_deref(), Some(".."));
        assert_eq!(
            at("~/notes/todo.md:", "notes").as_deref(),
            Some("~/notes/todo.md")
        );
        assert_eq!(
            at("Cargo.toml  crates", "Cargo").as_deref(),
            Some("Cargo.toml")
        );
    }

    #[test]
    fn locations_and_wrappers() {
        assert_eq!(at("Foo.cs(12,5): error", "Foo").as_deref(), Some("Foo.cs"));
        assert_eq!(at("(see lib/a.rs)", "lib").as_deref(), Some("lib/a.rs"));
        assert_eq!(at("[src/x.rs:3]", "src").as_deref(), Some("src/x.rs"));
    }

    #[test]
    fn quoted_paths_keep_spaces() {
        let py = r#"File "C:\My Files\app.py", line 3"#;
        assert_eq!(at(py, "Files").as_deref(), Some(r"C:\My Files\app.py"));
        // A quoted sentence is not one path; the word under the pointer is.
        assert_eq!(
            at(r#""error in src/a.rs""#, "src").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            at("don't open 'C:\\x y\\z'", "x y").as_deref(),
            Some(r"C:\x y\z")
        );
    }

    #[test]
    fn not_paths() {
        assert_eq!(at("visit https://example.com/a", "example"), None);
        assert_eq!(at("a  --  b", "--"), None);
        let text: Vec<char> = "a b".chars().collect();
        assert_eq!(path_at(&text, 1), None);
    }

    #[test]
    fn resolving() {
        let cwd = Path::new("/work");
        assert_eq!(resolve("src/a.rs", Some(cwd)), Some(cwd.join("src/a.rs")));
        assert_eq!(resolve("src/a.rs", None), None);
        let abs = if cfg!(windows) { r"C:\x" } else { "/x" };
        assert_eq!(resolve(abs, Some(cwd)), Some(PathBuf::from(abs)));
        let home = std::env::home_dir().unwrap();
        assert_eq!(resolve("~/a", None), Some(home.join("a")));
    }
}
