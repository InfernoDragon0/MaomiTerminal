//! MaomiTerminal's own `known_hosts` file (OpenSSH format, plain host names), so trusting a
//! host once lasts and a changed key is caught.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use russh::keys::{HashAlg, PublicKey};

use crate::HostKey;

/// What the file says about a host key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    Known,
    /// No key of this type is stored for the host.
    New,
    /// A different key of the same type is stored: possibly an attack.
    Changed {
        old_fingerprint: String,
    },
}

/// One line of the file, for the Known hosts list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
    /// As written: `host` or `[host]:port`.
    pub host: String,
    pub algorithm: String,
    pub fingerprint: String,
    line: String,
}

pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    /// The `known_hosts` file in `dir` (the app's config folder).
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join("known_hosts"),
        }
    }

    pub fn check(&self, key: &HostKey) -> io::Result<Trust> {
        let pattern = pattern(&key.host, key.port);
        let mut changed = None;
        for entry in self.entries()? {
            if !matches_host(&entry.host, &pattern) || entry.algorithm != key.algorithm {
                continue;
            }
            if entry.fingerprint == key.fingerprint {
                return Ok(Trust::Known);
            }
            changed = Some(entry.fingerprint);
        }
        Ok(match changed {
            Some(old_fingerprint) => Trust::Changed { old_fingerprint },
            None => Trust::New,
        })
    }

    /// Stores `key`, replacing any other key of its type for the host.
    pub fn trust(&self, key: &HostKey) -> io::Result<()> {
        let pattern = pattern(&key.host, key.port);
        let mut lines: Vec<String> = self
            .entries()?
            .into_iter()
            .filter(|e| !(matches_host(&e.host, &pattern) && e.algorithm == key.algorithm))
            .map(|e| e.line)
            .collect();
        // `openssh` is "<algorithm> <base64> [comment]"; the comment is dropped.
        let mut parts = key.openssh.split_whitespace();
        let (Some(algorithm), Some(data)) = (parts.next(), parts.next()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty host key",
            ));
        };
        lines.push(format!("{pattern} {algorithm} {data}"));
        self.write(&lines)
    }

    pub fn remove(&self, entry: &KnownHost) -> io::Result<()> {
        let lines: Vec<String> = self
            .entries()?
            .into_iter()
            .filter(|e| e.line != entry.line)
            .map(|e| e.line)
            .collect();
        self.write(&lines)
    }

    /// Every readable entry; comments and lines that do not parse are skipped.
    pub fn entries(&self) -> io::Result<Vec<KnownHost>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(text.lines().filter_map(parse_line).collect())
    }

    fn write(&self, lines: &[String]) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        let mut text = lines.join("\n");
        text.push('\n');
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }
}

fn parse_line(line: &str) -> Option<KnownHost> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let host = parts.next()?.to_owned();
    let algorithm = parts.next()?;
    let data = parts.next()?;
    let key = PublicKey::from_openssh(&format!("{algorithm} {data}")).ok()?;
    Some(KnownHost {
        host,
        algorithm: algorithm.to_owned(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        line: trimmed.to_owned(),
    })
}

/// OpenSSH's host pattern: the bare name on port 22, else `[host]:port`.
fn pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

fn matches_host(hosts: &str, pattern: &str) -> bool {
    hosts.split(',').any(|h| h.eq_ignore_ascii_case(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_PUB: &str = include_str!("../tests/fixtures/host_ed25519.pub");
    const OTHER_PUB: &str = include_str!("../tests/fixtures/client_ed25519.pub");

    fn host_key(host: &str, port: u16, openssh: &str) -> HostKey {
        let key = PublicKey::from_openssh(openssh.trim()).unwrap();
        HostKey {
            host: host.into(),
            port,
            algorithm: key.algorithm().as_str().into(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            openssh: openssh.trim().into(),
        }
    }

    #[test]
    fn trust_on_first_use_then_catch_a_change() {
        let dir = std::env::temp_dir().join(format!("maomi-known-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        let known = KnownHosts::in_dir(&dir);
        let key = host_key("fennec.lan", 22, HOST_PUB);
        let on_other_port = host_key("fennec.lan", 2222, HOST_PUB);

        assert_eq!(known.check(&key).unwrap(), Trust::New);
        known.trust(&key).unwrap();
        assert_eq!(known.check(&key).unwrap(), Trust::Known);
        assert_eq!(known.check(&on_other_port).unwrap(), Trust::New);

        let swapped = host_key("fennec.lan", 22, OTHER_PUB);
        assert_eq!(
            known.check(&swapped).unwrap(),
            Trust::Changed {
                old_fingerprint: key.fingerprint.clone()
            }
        );
        known.trust(&swapped).unwrap();
        assert_eq!(known.check(&swapped).unwrap(), Trust::Known);

        known.trust(&on_other_port).unwrap();
        let entries = known.entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].host, "[fennec.lan]:2222");
        known.remove(&entries[0]).unwrap();
        assert_eq!(known.entries().unwrap().len(), 1);
        _ = fs::remove_dir_all(dir);
    }
}
