//! Creating repositories: `git init` and `git clone` with progress.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};
use crate::progress::Progress;
use crate::runner::{GitCommand, GitRunner};

/// URL schemes a clone may use. `http://` stays accepted for self-hosted servers.
const ALLOWED_SCHEMES: &[&str] = &["https", "http", "ssh", "git", "file"];

/// Checks a clone URL / path typed by the user against an allow-list: `https://`, `http://`,
/// `ssh://`, `git://`, `file://`, scp-like `[user@]host:path` and local paths (absolute,
/// relative, drive letter, UNC). Everything else is refused, notably `ext::` / `fd::` and other
/// `<transport>::<address>` forms (they run programs) and anything git could read as an option.
pub fn validate_clone_url(url: &str) -> Result<()> {
    let url = url.trim();
    if is_clone_url(url) {
        Ok(())
    } else {
        Err(GitError::InvalidArgument {
            what: "repository URL",
            value: url.to_string(),
        })
    }
}

fn is_clone_url(url: &str) -> bool {
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return false;
    }
    // Git's own test for a transport helper: scheme characters, then `::`.
    let scheme_len = url
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .unwrap_or(url.len());
    if url[scheme_len..].starts_with("::") {
        return false;
    }
    if scheme_len > 0
        && let Some(rest) = url[scheme_len..].strip_prefix("://")
    {
        let scheme = &url[..scheme_len];
        return ALLOWED_SCHEMES
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
            && url_rest_ok(scheme, rest);
    }
    if is_local_path(url) {
        return true;
    }
    match url.split_once(':') {
        // No slash before the first colon: scp-like `[user@]host:path`.
        Some((host_part, path)) if !host_part.contains(['/', '\\']) => {
            let (user, host) = host_part.rsplit_once('@').unwrap_or(("", host_part));
            !path.is_empty()
                && !user.contains(char::is_whitespace)
                && !user.contains(['/', '\\'])
                && host_ok(host)
        }
        // No colon at all: a relative path.
        None => true,
        _ => false,
    }
}

/// What follows `scheme://`: a host (not for `file://`, where it is usually empty) and a path.
fn url_rest_ok(scheme: &str, rest: &str) -> bool {
    if scheme.eq_ignore_ascii_case("file") {
        return !rest.is_empty();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    !authority.contains(char::is_whitespace) && host_ok(host)
}

/// A host name or address that cannot be read as an ssh option (`-oProxyCommand=...`).
fn host_ok(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':' | '%')
        })
}

/// Absolute (`/x`, `\x`, `\\server\share`), explicitly relative (`./x`, `../x`, `~/x`) or a
/// drive path (`C:\x`, `C:/x`).
fn is_local_path(url: &str) -> bool {
    let b = url.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    drive
        || url.starts_with(['/', '\\'])
        || ["./", ".\\", "../", "..\\", "~/"]
            .iter()
            .any(|p| url.starts_with(p))
}

/// Folder name git would pick: last path segment without `.git` (`https://x/y/repo.git` -> `repo`).
pub fn default_dir_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// `git init` in `dir` (created if missing). Returns the worktree root.
pub async fn init(runner: &GitRunner, dir: &Path) -> Result<PathBuf> {
    init_with_branch(runner, dir, None).await
}

/// [`init`] with the first branch named `branch` (`--initial-branch`); `None` leaves it to
/// `init.defaultBranch` / git's default. The name is checked like any branch name.
pub async fn init_with_branch(
    runner: &GitRunner,
    dir: &Path,
    branch: Option<&str>,
) -> Result<PathBuf> {
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    let mut cmd = GitCommand::write(dir).args(["init", "--quiet"]);
    if let Some(branch) = branch {
        crate::runner::validate_rev(branch).map_err(|_| GitError::InvalidArgument {
            what: "branch name",
            value: branch.to_string(),
        })?;
        // `check-ref-format` needs no repository; run it in the folder that exists already.
        let probe_dir = dir
            .ancestors()
            .find(|d| d.is_dir())
            .unwrap_or(Path::new("."));
        let check = runner
            .output(
                GitCommand::read(probe_dir)
                    .args(["check-ref-format", "--branch"])
                    .arg(branch),
            )
            .await?;
        if !check.success() {
            return Err(GitError::InvalidArgument {
                what: "branch name",
                value: branch.to_string(),
            });
        }
        cmd = cmd.arg(format!("--initial-branch={branch}"));
    }
    std::fs::create_dir_all(dir)?;
    runner.run(cmd).await?;
    Ok(dir.to_path_buf())
}

/// `git clone --progress <url> <dest>`; `dest` must not exist or be an empty directory.
/// Drop the future to cancel (git is killed; the half-cloned folder is removed).
pub async fn clone(
    runner: &GitRunner,
    url: &str,
    dest: &Path,
    on_progress: impl FnMut(Progress) + Send,
) -> Result<PathBuf> {
    validate_clone_url(url)?;
    let parent = dest.parent().ok_or_else(|| GitError::InvalidArgument {
        what: "clone destination",
        value: dest.display().to_string(),
    })?;
    if dest.exists() && std::fs::read_dir(dest)?.next().is_some() {
        return Err(GitError::InvalidArgument {
            what: "clone destination (not empty)",
            value: dest.display().to_string(),
        });
    }
    std::fs::create_dir_all(parent)?;
    let created = !dest.exists();
    let cleanup = RemoveOnDrop {
        path: created.then(|| dest.to_path_buf()),
    };

    let cmd = GitCommand::write(parent)
        .args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.fd.allow=never",
        ])
        .args(["clone", "--progress", "--"])
        .user_arg("repository URL", url.trim())?
        .user_arg("clone destination", dest.as_os_str())?;
    runner.run_with_progress(cmd, on_progress).await?;
    cleanup.disarm();
    Ok(dest.to_path_buf())
}

/// Deletes a folder the clone created unless the clone finished.
struct RemoveOnDrop {
    path: Option<PathBuf>,
}

impl RemoveOnDrop {
    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(err) = std::fs::remove_dir_all(&path)
        {
            tracing::warn!(path = %path.display(), %err, "could not remove partial clone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation() {
        for ok in [
            "https://github.com/a/b.git",
            "HTTPS://github.com/a/b",
            "http://git.local:8080/a/b.git",
            "https://user:pw@host.example/a/b",
            "git@github.com:a/b.git",
            "github.com:a/b.git",
            "git@ssh.dev.azure.com:v3/org/proj/repo",
            "ssh://git@host:22/a/b",
            "ssh://host/~user/repo",
            "git://host.example/a/b.git",
            "file:///tmp/x",
            "file://C:/repos/x",
            "C:\\repos\\my repo",
            "c:/repos/x",
            "\\\\server\\share\\repo",
            "//server/share/repo",
            "/srv/git/x.git",
            "/srv/a://b",
            "./rel/repo",
            "../repo",
            "..\\repo",
            "~/repos/x",
            "repo",
            "dir/sub repo",
            "  https://github.com/a/b  ",
        ] {
            assert!(validate_clone_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "  ",
            "-",
            "--upload-pack=touch /tmp/pwn",
            "-oProxyCommand=x",
            "ext::sh -c touch% /tmp/pwn",
            "EXT::sh",
            "fd::3",
            "fd::17/foo",
            "hg::https://x/y",
            "svn::http://x",
            "ssh+x::foo",
            "foo://host/repo",
            "ftp://host/repo",
            "ftps://host/repo",
            "rsync://host/repo",
            "git+ssh://host/repo",
            "javascript://x",
            "https://",
            "ssh://-oProxyCommand=touch/x",
            "ssh://user@-oProxyCommand=touch/x",
            "ssh://ho st/x",
            "git@-oProxyCommand=x:path",
            "-oProxyCommand=x@host:path",
            "host:",
            ":path",
            "us er@host:path",
            "ho$(id)st:path",
            "https://x\n/y",
            "host:path\0",
        ] {
            assert!(validate_clone_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn dir_names() {
        assert_eq!(
            default_dir_name("https://github.com/a/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            default_dir_name("git@github.com:a/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            default_dir_name("https://host/a/repo/").as_deref(),
            Some("repo")
        );
        assert_eq!(default_dir_name("C:\\x\\proj").as_deref(), Some("proj"));
        assert_eq!(default_dir_name("https://host/..").as_deref(), None);
    }

    #[test]
    fn init_then_clone_locally_with_progress() {
        let runner = GitRunner::detect().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src repo");
        async_io::block_on(init(&runner, &src)).unwrap();
        std::fs::write(src.join("f.txt"), "x").unwrap();
        for args in [
            &["add", "f.txt"][..],
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "m",
            ],
        ] {
            async_io::block_on(runner.run(GitCommand::write(&src).args(args))).unwrap();
        }

        let dest = tmp.path().join("out").join("clone");
        let mut updates = Vec::new();
        // `file://` forces the pack protocol, which reports progress.
        let url = format!("file:///{}", src.display().to_string().replace('\\', "/"));
        async_io::block_on(clone(&runner, &url, &dest, |p| updates.push(p))).unwrap();
        assert!(dest.join("f.txt").is_file());
        assert!(!updates.is_empty(), "no progress lines");

        // Non-empty destination is refused and left alone.
        let err = async_io::block_on(clone(&runner, &url, &dest, |_| {})).unwrap_err();
        assert!(matches!(err, GitError::InvalidArgument { .. }));
        assert!(dest.join("f.txt").is_file());

        // A failed clone removes the folder it created.
        let missing = tmp.path().join("nope");
        let dest2 = tmp.path().join("clone2");
        let bad = format!(
            "file:///{}",
            missing.display().to_string().replace('\\', "/")
        );
        assert!(async_io::block_on(clone(&runner, &bad, &dest2, |_| {})).is_err());
        assert!(!dest2.exists());
    }
}
