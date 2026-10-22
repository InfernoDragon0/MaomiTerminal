//! Saved SSH hosts for the Hosts panel, in plain `hosts.json` next to `config.json`.
//! Passwords and private keys live in the vault; hosts only point at vault entries.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, write_atomic};

const FILE_NAME: &str = "hosts.json";

/// Schema version written by this build.
pub const HOSTS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hosts {
    pub version: u32,
    pub items: Vec<Host>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAuth {
    /// A saved password from the vault, else asked when connecting.
    #[default]
    Password,
    /// A private key stored in the vault.
    Key,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Host {
    /// Stable id; vault entries for this host are named after it.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: HostAuth,
    /// Vault entry id of the private key, for [`HostAuth::Key`].
    pub key_id: Option<String>,
    /// The vault holds this host's password (see [`Host::password_entry`]).
    pub password_saved: bool,
    /// Folder in the Hosts tree; empty = top level.
    pub folder: String,
    pub tags: Vec<String>,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: HostAuth::default(),
            key_id: None,
            password_saved: false,
            folder: String::new(),
            tags: Vec::new(),
        }
    }
}

impl Host {
    /// A fresh id, unique enough for one user's host list.
    pub fn new_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("h{nanos:x}")
    }

    /// Vault entry id of the saved password.
    pub fn password_entry(&self) -> String {
        format!("host-password:{}", self.id)
    }

    /// `user@host:port`, as the dock shows it.
    pub fn address(&self) -> String {
        if self.user.is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

impl Hosts {
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(crate::config_dir()?.join(FILE_NAME))
    }

    /// The saved hosts; empty when there are none or the file cannot be read.
    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    pub fn get(&self, id: &str) -> Option<&Host> {
        self.items.iter().find(|h| h.id == id)
    }

    /// Distinct non-empty tags in first-seen order.
    pub fn tags(&self) -> Vec<&str> {
        let mut tags: Vec<&str> = Vec::new();
        for tag in self.items.iter().flat_map(|h| &h.tags) {
            let tag = tag.trim();
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags
    }

    /// Distinct folders in first-seen order; the top level ("") comes first when used.
    pub fn folders(&self) -> Vec<&str> {
        let mut folders: Vec<&str> = Vec::new();
        if self.items.iter().any(|h| h.folder.trim().is_empty()) {
            folders.push("");
        }
        for host in &self.items {
            let folder = host.folder.trim();
            if !folder.is_empty() && !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        folders
    }

    fn load_from(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%err, "cannot read hosts.json");
                }
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(hosts) => hosts,
            Err(err) => {
                // Keep the user's file: the next save would overwrite it.
                tracing::warn!(%err, "hosts.json is not valid; moved to hosts.bad.json");
                _ = fs::rename(path, path.with_file_name("hosts.bad.json"));
                Self::default()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let hosts = Self {
            version: HOSTS_VERSION,
            ..self.clone()
        };
        write_atomic(path, &serde_json::to_string_pretty(&hosts)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, folder: &str, tags: &[&str]) -> Host {
        Host {
            id: Host::new_id(),
            name: name.into(),
            host: format!("{name}.lan"),
            user: "kit".into(),
            folder: folder.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Host::default()
        }
    }

    #[test]
    fn round_trip_folders_and_tags() {
        let dir = std::env::temp_dir().join(format!("maomi-hosts-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        let path = dir.join(FILE_NAME);
        let hosts = Hosts {
            version: 0,
            items: vec![
                host("fennec", "Home lab", &["lab"]),
                host("web", "Work", &["prod", "lab"]),
                host("loose", "", &[]),
            ],
        };
        hosts.save_to(&path).unwrap();
        let loaded = Hosts::load_from(&path);
        assert_eq!(loaded.version, HOSTS_VERSION);
        assert_eq!(loaded.items, hosts.items);
        assert_eq!(loaded.folders(), ["", "Home lab", "Work"]);
        assert_eq!(loaded.tags(), ["lab", "prod"]);
        assert_eq!(loaded.items[0].address(), "kit@fennec.lan:22");
        _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_fields_take_defaults() {
        let hosts: Hosts =
            serde_json::from_str(r#"{"items": [{"id": "a", "host": "x"}]}"#).unwrap();
        assert_eq!(hosts.items[0].port, 22);
        assert_eq!(hosts.items[0].auth, HostAuth::Password);
        assert!(hosts.get("a").is_some());
    }
}
