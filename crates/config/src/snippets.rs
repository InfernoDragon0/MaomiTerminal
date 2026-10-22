//! Saved commands for the Snippets panel, in plain `snippets.json` next to `config.json`.
//! Secrets belong in the vault, never here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, write_atomic};

const FILE_NAME: &str = "snippets.json";

/// Schema version written by this build.
pub const SNIPPETS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippets {
    pub version: u32,
    pub items: Vec<Snippet>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    pub name: String,
    /// Typed into the shell as one paste, then Enter. May span lines.
    pub command: String,
    pub description: String,
    /// One optional tag for the panel's tag filter; empty = none.
    pub tag: String,
}

impl Snippets {
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(crate::config_dir()?.join(FILE_NAME))
    }

    /// The saved snippets; empty when there are none or the file cannot be read.
    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    /// Distinct non-empty tags in first-seen order.
    pub fn tags(&self) -> Vec<&str> {
        let mut tags: Vec<&str> = Vec::new();
        for item in &self.items {
            let tag = item.tag.trim();
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags
    }

    fn load_from(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%err, "cannot read snippets.json");
                }
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(snippets) => snippets,
            Err(err) => {
                // Keep the user's file: the next save would overwrite it.
                tracing::warn!(%err, "snippets.json is not valid; moved to snippets.bad.json");
                _ = fs::rename(path, path.with_file_name("snippets.bad.json"));
                Self::default()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let snippets = Self {
            version: SNIPPETS_VERSION,
            ..self.clone()
        };
        write_atomic(path, &serde_json::to_string_pretty(&snippets)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(name: &str, tag: &str) -> Snippet {
        Snippet {
            name: name.into(),
            command: format!("echo {name}"),
            tag: tag.into(),
            ..Snippet::default()
        }
    }

    #[test]
    fn round_trip_and_tags() {
        let dir = std::env::temp_dir().join(format!("maomi-snippets-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        let path = dir.join(FILE_NAME);
        let snippets = Snippets {
            version: 0,
            items: vec![
                snippet("a", "git"),
                snippet("b", ""),
                snippet("c", " git "),
                snippet("d", "build"),
            ],
        };
        snippets.save_to(&path).unwrap();
        let loaded = Snippets::load_from(&path);
        assert_eq!(loaded.version, SNIPPETS_VERSION);
        assert_eq!(loaded.items, snippets.items);
        assert_eq!(loaded.tags(), ["git", "build"]);
        _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_file_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("maomi-snippets-bad-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, "[ not json").unwrap();
        assert_eq!(Snippets::load_from(&path), Snippets::default());
        assert!(dir.join("snippets.bad.json").exists());
        _ = fs::remove_dir_all(dir);
    }
}
