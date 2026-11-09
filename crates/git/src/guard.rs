//! Repo-local config overrides for background commands.
//!
//! Before a [`Mode::Read`](crate::Mode) or background command, the repo's config is probed
//! once for keys that name a program to run. Each one set in the `local` / `worktree` scope is
//! overridden back to the user's global value (or a safe default), so background work behaves
//! as if the repo had never set it. The result is cached per working dir and re-probed when
//! the repo's config files change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Keys probed (git's canonical, lower-case form).
const KEYS: &str = concat!(
    r"^(filter\..+\.(clean|smudge|process)",
    r"|diff\..+\.(textconv|command)",
    r"|core\.(sshcommand|askpass|gitproxy|alternaterefscommand)",
    r"|credential\..*helper",
    r"|gpg\.program|gpg\..+\.program|gpg\.ssh\.defaultkeycommand",
    r"|lfs\.extension\..+\.(clean|smudge)|lfs\.customtransfer\..+\.path",
    r"|include\.path|includeif\..+\.path)$",
);

/// Config and environment applied on top of a command.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Overrides {
    /// Passed as `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`.
    pub config: Vec<(String, String)>,
    pub env: Vec<(&'static str, String)>,
}

struct Entry {
    stamps: Vec<Option<(u64, SystemTime)>>,
    overrides: Arc<Overrides>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Overrides for a background command run in `cwd`.
pub(crate) fn overrides(git: &Path, cwd: &Path) -> Arc<Overrides> {
    let files = config_files(cwd);
    let stamps = files
        .as_deref()
        .map(|files| files.iter().map(|f| stamp(f)).collect());
    if let Some(stamps) = &stamps {
        let cache = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = cache.get(cwd).filter(|e| &e.stamps == stamps) {
            return entry.overrides.clone();
        }
    }
    let Some(listing) = probe(git, cwd) else {
        return Arc::default();
    };
    let (overrides, cacheable) = compute(&parse(&listing));
    let overrides = Arc::new(overrides);
    // Includes can pull in files we don't stamp, so those repos are probed every time.
    if let (Some(stamps), true) = (stamps, cacheable) {
        let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
        cache.insert(
            cwd.to_path_buf(),
            Entry {
                stamps,
                overrides: overrides.clone(),
            },
        );
    }
    overrides
}

/// `git config --show-scope --null --get-regexp`; `None` when git fails (exit 1 = no match).
fn probe(git: &Path, cwd: &Path) -> Option<Vec<u8>> {
    let mut cmd = std::process::Command::new(git);
    cmd.args(["config", "--show-scope", "--null", "--get-regexp", KEYS])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());
    crate::runner::harden_std_command(&mut cmd);
    let out = cmd.output().ok()?;
    match out.status.code() {
        Some(0) => Some(out.stdout),
        Some(1) => Some(Vec::new()),
        _ => None,
    }
}

/// `(scope, key, value)` triples in git's read order (system, global, local, worktree).
fn parse(listing: &[u8]) -> Vec<(String, String, Option<String>)> {
    let mut fields = listing.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(scope), Some(item)) = (fields.next(), fields.next()) {
        let item = String::from_utf8_lossy(item);
        let (key, value) = match item.split_once('\n') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (item.into_owned(), None),
        };
        out.push((String::from_utf8_lossy(scope).into_owned(), key, value));
    }
    out
}

fn is_repo_scope(scope: &str) -> bool {
    matches!(scope, "local" | "worktree")
}

/// The overrides, and whether they can be cached.
fn compute(entries: &[(String, String, Option<String>)]) -> (Overrides, bool) {
    let mut out = Overrides::default();
    let mut cacheable = true;
    let mut keys: Vec<&str> = Vec::new();
    for (scope, key, _) in entries {
        if is_repo_scope(scope) && !keys.contains(&key.as_str()) {
            keys.push(key);
        }
    }
    for key in keys {
        let user: Vec<&str> = entries
            .iter()
            .filter(|(scope, k, _)| k == key && !is_repo_scope(scope))
            .map(|(_, _, v)| v.as_deref().unwrap_or(""))
            .collect();
        let last = user.last().copied();
        let config = &mut out.config;
        let set = |config: &mut Vec<(String, String)>, value: &str| {
            config.push((key.to_string(), value.to_string()));
        };
        if key == "include.path" || key.starts_with("includeif.") {
            cacheable = false;
        } else if key.starts_with("credential.") {
            // Multi-valued: an empty value clears the list, then the user's helpers return.
            set(config, "");
            for value in &user {
                set(config, value);
            }
        } else if key == "core.gitproxy" {
            // First match wins for this key, so config can't override it; the env var can.
            out.env.push(("GIT_PROXY_COMMAND", String::new()));
        } else if key == "core.sshcommand" {
            set(config, &last.map_or_else(default_ssh, str::to_string));
        } else if key.starts_with("filter.") {
            set(config, last.unwrap_or(""));
            if last.is_none() {
                let name = &key[..key.rfind('.').unwrap_or(key.len())];
                config.push((format!("{name}.required"), "false".into()));
            }
        } else {
            set(config, last.unwrap_or_else(|| default_for(key)));
        }
    }
    (out, cacheable)
}

/// What git runs when no `core.sshCommand` is set.
fn default_ssh() -> String {
    match std::env::var("GIT_SSH") {
        Ok(path) if !path.is_empty() => format!("'{}'", path.replace('\'', r"'\''")),
        _ => "ssh".into(),
    }
}

fn default_for(key: &str) -> &'static str {
    match key {
        "gpg.program" | "gpg.openpgp.program" => "gpg",
        "gpg.x509.program" => "gpgsm",
        "gpg.ssh.program" => "ssh-keygen",
        // An empty textconv fails the diff; `cat` shows the file as is.
        k if k.ends_with(".textconv") => "cat",
        _ => "",
    }
}

/// The repo's `config` and `config.worktree` for `cwd`, found like git's discovery; `None`
/// when no repo is found (nothing is cached then).
fn config_files(cwd: &Path) -> Option<Vec<PathBuf>> {
    let dot_git = cwd
        .ancestors()
        .map(|d| d.join(".git"))
        .find(|p| p.exists())?;
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.strip_prefix("gitdir:")?.trim();
        dot_git.parent()?.join(target)
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => git_dir.join(text.trim()),
        Err(_) => git_dir.clone(),
    };
    Some(vec![common.join("config"), git_dir.join("config.worktree")])
}

fn stamp(file: &Path) -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(file).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[(&str, &str, &str)]) -> Vec<(String, String, Option<String>)> {
        list.iter()
            .map(|(s, k, v)| (s.to_string(), k.to_string(), Some(v.to_string())))
            .collect()
    }

    #[test]
    fn parses_null_listing() {
        let listing = b"global\0filter.lfs.clean\ngit-lfs clean -- %f\0local\0core.sshcommand\0";
        assert_eq!(
            parse(listing),
            vec![
                (
                    "global".into(),
                    "filter.lfs.clean".into(),
                    Some("git-lfs clean -- %f".into())
                ),
                ("local".into(), "core.sshcommand".into(), None),
            ]
        );
    }

    #[test]
    fn user_scope_only_is_left_alone() {
        let (out, cacheable) = compute(&entries(&[
            ("global", "filter.lfs.process", "git-lfs filter-process"),
            ("global", "credential.helper", "manager"),
        ]));
        assert_eq!(out, Overrides::default());
        assert!(cacheable);
    }

    #[test]
    fn repo_filter_falls_back_to_user_value_or_is_disabled() {
        let (out, _) = compute(&entries(&[
            ("global", "filter.lfs.process", "git-lfs filter-process"),
            ("local", "filter.lfs.process", "calc"),
            ("local", "filter.x.clean", "calc"),
        ]));
        assert_eq!(
            out.config,
            vec![
                ("filter.lfs.process".into(), "git-lfs filter-process".into()),
                ("filter.x.clean".into(), String::new()),
                ("filter.x.required".into(), "false".into()),
            ]
        );
    }

    #[test]
    fn credential_helpers_reset_then_user_helpers_return() {
        let (out, _) = compute(&entries(&[
            ("system", "credential.helper", "manager"),
            ("local", "credential.helper", "!calc"),
        ]));
        assert_eq!(
            out.config,
            vec![
                ("credential.helper".into(), String::new()),
                ("credential.helper".into(), "manager".into()),
            ]
        );
    }

    #[test]
    fn defaults_and_proxy_and_includes() {
        let (out, cacheable) = compute(&entries(&[
            ("local", "diff.x.textconv", "calc"),
            ("local", "gpg.program", "calc"),
            ("local", "core.gitproxy", "calc"),
            ("local", "include.path", "../x.cfg"),
        ]));
        assert_eq!(
            out.config,
            vec![
                ("diff.x.textconv".into(), "cat".into()),
                ("gpg.program".into(), "gpg".into()),
            ]
        );
        assert_eq!(out.env, vec![("GIT_PROXY_COMMAND", String::new())]);
        assert!(!cacheable);
    }
}
