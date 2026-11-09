//! Commit signing as configured (read-only): `commit.gpgsign`, `gpg.format`, `user.signingkey`.
//! MaomiTerminal never signs or verifies anything itself; git does when `commit.gpgsign` is on, and
//! shows the same prompts as in a terminal.

use std::path::Path;

use crate::error::Result;
use crate::runner::{GitCommand, GitRunner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningStatus {
    /// `commit.gpgsign`: commits are signed automatically.
    pub enabled: bool,
    /// `gpg.format`: `openpgp` (default), `ssh` or `x509`.
    pub format: String,
    /// `user.signingkey`, when set (a key id or, for ssh, a path / public key).
    pub key: Option<String>,
}

impl Default for SigningStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            format: "openpgp".into(),
            key: None,
        }
    }
}

impl SigningStatus {
    /// One line for the Settings page.
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "Commits are not signed (commit.gpgsign is off).".into();
        }
        let how = match self.key.as_deref().filter(|k| !k.is_empty()) {
            Some(key) => format!("{} key {}", self.format, shorten(key)),
            None => format!("{}, git picks the key", self.format),
        };
        format!("Commits are signed automatically ({how}).")
    }
}

/// Keys can be long fingerprints or paths; show the tail.
fn shorten(key: &str) -> String {
    let count = key.chars().count();
    if count <= 24 {
        key.to_string()
    } else {
        let tail: String = key.chars().skip(count - 16).collect();
        format!("\u{2026}{tail}")
    }
}

/// Parses `git config --get-regexp` lines (`key value`).
pub fn parse(output: &str) -> SigningStatus {
    let mut status = SigningStatus::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "commit.gpgsign" => {
                status.enabled = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "yes" | "on" | "1"
                )
            }
            "gpg.format" if !value.is_empty() => status.format = value.to_ascii_lowercase(),
            "user.signingkey" if !value.is_empty() => status.key = Some(value.to_string()),
            _ => {}
        }
    }
    status
}

/// Reads the signing config as git sees it from `cwd` (a repository, or any folder for the
/// global config).
pub async fn signing_status(runner: &GitRunner, cwd: &Path) -> Result<SigningStatus> {
    let cmd = GitCommand::read(cwd).args([
        "config",
        "--get-regexp",
        r"^(commit\.gpgsign|gpg\.format|user\.signingkey)$",
    ]);
    // Exit 1 just means none of them is set.
    let out = runner.output(cmd).await?;
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_lines() {
        assert_eq!(parse(""), SigningStatus::default());
        let on = parse("commit.gpgsign true\ngpg.format ssh\nuser.signingkey ~/.ssh/id.pub\n");
        assert!(on.enabled);
        assert_eq!(on.format, "ssh");
        assert_eq!(on.key.as_deref(), Some("~/.ssh/id.pub"));
        assert!(on.summary().contains("ssh key"));
        assert!(!parse("commit.gpgsign false").enabled);
        assert!(parse("commit.gpgSign Yes").enabled);
        assert!(
            parse("commit.gpgsign false")
                .summary()
                .contains("not signed")
        );
    }

    #[test]
    fn long_keys_are_shortened() {
        let key = "0123456789abcdef0123456789abcdef01234567";
        let status = SigningStatus {
            enabled: true,
            format: "openpgp".into(),
            key: Some(key.into()),
        };
        assert!(status.summary().contains(&key[key.len() - 16..]));
        assert!(!status.summary().contains(key));
    }
}
