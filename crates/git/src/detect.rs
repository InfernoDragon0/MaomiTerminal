//! Finds the installed git once, as an absolute path, and caches its version.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::error::{GitError, Result};

/// Oldest supported git: `status --porcelain=v2 --show-stash` needs 2.35.
pub const MIN_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 35,
    patch: 0,
};

/// First git release without the 2024 clone / submodule remote-code-execution bugs
/// (CVE-2024-32002 and friends).
pub const SAFE_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 45,
    patch: 1,
};

/// First Git for Windows release with the 2025 fixes (CVE-2025-48384 and friends).
pub const SAFE_VERSION_WINDOWS: GitVersion = GitVersion {
    major: 2,
    minor: 50,
    patch: 1,
};

/// Env var that points at a specific git binary (absolute path only).
pub const GIT_OVERRIDE_ENV: &str = "MAOMI_GIT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for GitVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl GitVersion {
    /// Parses `git version 2.40.1.windows.1` / `git version 2.39.3 (Apple Git-145)`.
    pub fn parse(output: &str) -> Option<Self> {
        let rest = output.trim().strip_prefix("git version ")?;
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u32>().ok());
        Some(Self {
            major: nums.next()??,
            minor: nums.next()??,
            patch: nums.next().flatten().unwrap_or(0),
        })
    }
}

/// Warning for a git with known remote-code-execution bugs, or `None` when it is recent enough.
/// `version_line` is the full `git --version` output; `.windows.` marks Git for Windows, which
/// needs a newer release than other builds.
pub fn security_advisory(version: GitVersion, version_line: &str) -> Option<String> {
    let (safe, name) = if version_line.contains(".windows.") {
        (SAFE_VERSION_WINDOWS, "Git for Windows")
    } else {
        (SAFE_VERSION, "Git")
    };
    (version < safe).then(|| {
        format!(
            "{name} {version} has known security vulnerabilities (remote code execution on \
             untrusted repositories). Update to {safe} or newer."
        )
    })
}

/// The git binary every command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBinary {
    /// Absolute path.
    pub path: PathBuf,
    pub version: GitVersion,
    /// Full `git --version` line, e.g. `git version 2.40.1.windows.1`.
    pub version_line: String,
}

impl GitBinary {
    /// See [`security_advisory`].
    pub fn security_advisory(&self) -> Option<String> {
        security_advisory(self.version, &self.version_line)
    }
}

static DETECTED: OnceLock<GitBinary> = OnceLock::new();

/// Detects git on first success (blocking: runs `git --version`), then returns the cached
/// binary. Failures are not cached, so installing git while the app runs is picked up.
/// Call it from a background thread at startup.
pub fn git() -> Result<GitBinary> {
    if let Some(bin) = DETECTED.get() {
        return Ok(bin.clone());
    }
    let bin = detect()?;
    Ok(DETECTED.get_or_init(|| bin).clone())
}

/// Uncached detection: override env, then `PATH`, then well-known install dirs.
pub fn detect() -> Result<GitBinary> {
    let candidates = candidates();
    for path in &candidates {
        if !path.is_file() {
            continue;
        }
        match probe(path) {
            Ok(bin) if bin.version >= MIN_VERSION => return Ok(bin),
            Ok(bin) => {
                return Err(GitError::TooOld {
                    found: bin.version,
                    required: MIN_VERSION,
                });
            }
            Err(err) => tracing::warn!(path = %path.display(), %err, "git candidate failed"),
        }
    }
    Err(GitError::NotFound {
        searched: candidates,
    })
}

/// Runs `<path> --version`.
pub fn probe(path: &Path) -> Result<GitBinary> {
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    crate::runner::harden_std_command(&mut cmd);
    let output = cmd.output().map_err(|source| GitError::Spawn {
        program: path.to_path_buf(),
        source,
    })?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let version = GitVersion::parse(&line).ok_or_else(|| GitError::InvalidArgument {
        what: "git --version output",
        value: line.clone(),
    })?;
    Ok(GitBinary {
        path: path.to_path_buf(),
        version,
        version_line: line,
    })
}

/// Absolute path of another tool on `PATH` (e.g. `ssh`, `gpg`), skipping relative entries.
pub fn find_on_path(tool: &str) -> Option<PathBuf> {
    absolute_path_dirs(std::env::var_os("PATH"))
        .into_iter()
        .map(|dir| dir.join(exe_name(tool)))
        .find(|p| p.is_file())
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os(GIT_OVERRIDE_ENV).map(PathBuf::from) {
        if p.is_absolute() {
            out.push(p);
        } else {
            tracing::warn!(path = %p.display(), "{GIT_OVERRIDE_ENV} must be absolute; ignored");
        }
    }
    let exe = exe_name("git");
    out.extend(
        absolute_path_dirs(std::env::var_os("PATH"))
            .into_iter()
            .map(|dir| dir.join(&exe)),
    );
    out.extend(default_install_paths());
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// `PATH` entries that are absolute; `.`, empty and relative entries are dropped.
pub fn absolute_path_dirs(path: Option<OsString>) -> Vec<PathBuf> {
    path.map(|p| {
        std::env::split_paths(&p)
            .filter(|dir| dir.is_absolute())
            .collect()
    })
    .unwrap_or_default()
}

fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

#[cfg(windows)]
fn default_install_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            out.push(PathBuf::from(dir).join(r"Git\cmd\git.exe"));
        }
    }
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(dir).join(r"Programs\Git\cmd\git.exe"));
    }
    if let Some(dir) = std::env::var_os("USERPROFILE") {
        out.push(PathBuf::from(dir).join(r"scoop\apps\git\current\cmd\git.exe"));
    }
    out.retain(|p| p.is_absolute());
    out
}

#[cfg(not(windows))]
fn default_install_paths() -> Vec<PathBuf> {
    [
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/usr/bin/git",
        "/bin/git",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        let v = |s| GitVersion::parse(s);
        assert_eq!(
            v("git version 2.40.1.windows.1"),
            Some(GitVersion {
                major: 2,
                minor: 40,
                patch: 1
            })
        );
        assert_eq!(
            v("git version 2.39.3 (Apple Git-145)\n"),
            Some(GitVersion {
                major: 2,
                minor: 39,
                patch: 3
            })
        );
        assert_eq!(
            v("git version 2.45"),
            Some(GitVersion {
                major: 2,
                minor: 45,
                patch: 0
            })
        );
        assert_eq!(v("hello"), None);
        assert_eq!(v("git version x"), None);
    }

    #[test]
    fn version_order_and_minimum() {
        let old = GitVersion {
            major: 2,
            minor: 34,
            patch: 9,
        };
        assert!(old < MIN_VERSION);
        assert!(
            GitVersion {
                major: 3,
                minor: 0,
                patch: 0
            } > MIN_VERSION
        );
    }

    #[test]
    fn security_advisory_by_version() {
        let advisory = |line: &str| security_advisory(GitVersion::parse(line).unwrap(), line);
        // Generic builds: below 2.45.1 is flagged.
        assert!(advisory("git version 2.39.3 (Apple Git-145)").is_some());
        assert!(advisory("git version 2.45.0").is_some());
        assert!(advisory("git version 2.44.9").is_some());
        assert!(advisory("git version 2.45.1").is_none());
        assert!(advisory("git version 2.46.0").is_none());
        assert!(advisory("git version 2.50.0").is_none());
        assert!(advisory("git version 3.0.0").is_none());
        // Git for Windows needs 2.50.1.
        assert!(advisory("git version 2.40.1.windows.1").is_some());
        assert!(advisory("git version 2.49.0.windows.1").is_some());
        assert!(advisory("git version 2.50.0.windows.2").is_some());
        assert!(advisory("git version 2.50.1.windows.1").is_none());
        assert!(advisory("git version 2.51.0.windows.1").is_none());
        let msg = advisory("git version 2.40.1.windows.1").unwrap();
        assert!(msg.contains("Git for Windows 2.40.1"), "{msg}");
        assert!(msg.contains("2.50.1"), "{msg}");
        let msg = advisory("git version 2.30.2").unwrap();
        assert!(msg.contains("2.45.1") && !msg.contains("Windows"), "{msg}");
    }

    #[test]
    fn relative_path_entries_are_skipped() {
        let abs = if cfg!(windows) {
            r"C:\tools"
        } else {
            "/usr/bin"
        };
        let joined = std::env::join_paths([".", "", "relative/dir", abs]).unwrap();
        assert_eq!(absolute_path_dirs(Some(joined)), vec![PathBuf::from(abs)]);
    }

    #[test]
    fn finds_installed_git() {
        // The dev machine and CI both have git; this documents what detection picked.
        let bin = detect().expect("git is installed");
        assert!(bin.path.is_absolute());
        assert!(bin.version >= MIN_VERSION, "{}", bin.version_line);
    }
}
