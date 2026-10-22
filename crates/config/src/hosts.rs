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
    /// The keys loaded in the running ssh-agent; nothing is stored.
    Agent,
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
    /// Id of the saved host to connect through (ProxyJump). That host's own jump host comes
    /// before it, so jump hosts chain.
    pub jump: Option<String>,
    /// Started with every connection to this host.
    pub forwards: Vec<PortForward>,
    /// Lets the server use the local ssh-agent (`ssh -A`).
    pub forward_agent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// `ssh -L`: listen here, connect from the server.
    #[default]
    Local,
    /// `ssh -R`: the server listens, connect from here.
    Remote,
    /// `ssh -D`: a SOCKS5 proxy here.
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PortForward {
    pub kind: ForwardKind,
    /// Listen address; empty = loopback.
    pub bind: String,
    /// Listen port: here for local and dynamic, on the server for remote.
    pub port: u16,
    /// Where connections go; unused for dynamic.
    pub host: String,
    pub host_port: u16,
}

impl PortForward {
    /// Reads the editor's fields: `listen` is `[bind:]port`, `to` is `host:port` (an IPv6
    /// host in brackets) and is ignored for dynamic forwards.
    pub fn parse(kind: ForwardKind, listen: &str, to: &str) -> Result<Self, &'static str> {
        let (bind, port) = match listen.trim().rsplit_once(':') {
            Some((bind, port)) => (unbracket(bind), port),
            None => ("", listen.trim()),
        };
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or("The listen port must be a number from 1 to 65535.")?;
        let (host, host_port) = if kind == ForwardKind::Dynamic {
            (String::new(), 0)
        } else {
            let (host, host_port) = to
                .trim()
                .rsplit_once(':')
                .ok_or("Enter the destination as host:port.")?;
            let host_port = host_port
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or("The destination port must be a number from 1 to 65535.")?;
            let host = unbracket(host);
            if host.is_empty() {
                return Err("Enter the destination as host:port.");
            }
            (host.to_owned(), host_port)
        };
        Ok(Self {
            kind,
            bind: bind.to_owned(),
            port,
            host,
            host_port,
        })
    }

    /// One line for the editor, e.g. `8080 → localhost:80`.
    pub fn summary(&self) -> String {
        let listen = if self.bind.is_empty() {
            self.port.to_string()
        } else {
            format!("{}:{}", bracket(&self.bind), self.port)
        };
        let to = format!("{}:{}", bracket(&self.host), self.host_port);
        match self.kind {
            ForwardKind::Local => format!("{listen} \u{2192} {to}"),
            ForwardKind::Remote => format!("{listen} \u{2190} {to}"),
            ForwardKind::Dynamic => format!("{listen} (SOCKS5)"),
        }
    }
}

fn unbracket(host: &str) -> &str {
    host.trim()
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host.trim())
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
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
            jump: None,
            forwards: Vec::new(),
            forward_agent: false,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("this host or one of its jump hosts was deleted")]
    Missing,
    #[error("the jump hosts lead back to a host already on the way")]
    Loop,
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

    /// The hosts to connect through, first jump host first, ending with `id` itself.
    pub fn route(&self, id: &str) -> Result<Vec<&Host>, RouteError> {
        let mut route = vec![self.get(id).ok_or(RouteError::Missing)?];
        while let Some(jump) = route[0].jump.as_deref() {
            let host = self.get(jump).ok_or(RouteError::Missing)?;
            if route.iter().any(|h| h.id == host.id) {
                return Err(RouteError::Loop);
            }
            route.insert(0, host);
        }
        Ok(route)
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
    fn route_follows_jump_hosts_and_stops_at_a_loop() {
        let mut hosts = Hosts {
            version: 0,
            items: vec![
                Host {
                    id: "a".into(),
                    jump: Some("b".into()),
                    ..Host::default()
                },
                Host {
                    id: "b".into(),
                    jump: Some("c".into()),
                    ..Host::default()
                },
                Host {
                    id: "c".into(),
                    ..Host::default()
                },
            ],
        };
        let ids = |route: Vec<&Host>| route.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(hosts.route("a").unwrap()), ["c", "b", "a"]);
        assert_eq!(ids(hosts.route("c").unwrap()), ["c"]);
        hosts.items[2].jump = Some("a".into());
        assert_eq!(hosts.route("a").unwrap_err(), RouteError::Loop);
        hosts.items[2].jump = Some("gone".into());
        assert_eq!(hosts.route("a").unwrap_err(), RouteError::Missing);
    }

    #[test]
    fn forwards_parse_and_summarise() {
        let local = PortForward::parse(ForwardKind::Local, "8080", "localhost:80").unwrap();
        assert_eq!((local.bind.as_str(), local.port), ("", 8080));
        assert_eq!(local.summary(), "8080 \u{2192} localhost:80");
        let remote = PortForward::parse(ForwardKind::Remote, "0.0.0.0:9000", "[::1]:3000").unwrap();
        assert_eq!(remote.bind, "0.0.0.0");
        assert_eq!(remote.host, "::1");
        assert_eq!(remote.summary(), "0.0.0.0:9000 \u{2190} [::1]:3000");
        let socks = PortForward::parse(ForwardKind::Dynamic, "[::1]:1080", "ignored").unwrap();
        assert_eq!(socks.summary(), "[::1]:1080 (SOCKS5)");
        assert!(PortForward::parse(ForwardKind::Local, "80800", "x:1").is_err());
        assert!(PortForward::parse(ForwardKind::Local, "8080", "x").is_err());
        assert!(PortForward::parse(ForwardKind::Local, "8080", ":80").is_err());
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
