//! Remote files over SFTP for the pane drawer: list, rename, delete, permissions, new
//! folders, and uploads / downloads of files or whole folders with progress.
//!
//! Remote paths are POSIX strings; join them with [`join`], never with `Path`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use russh::ChannelMsg;
use russh::client::Handle;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileAttributes;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{Client, SshError, on_runtime};

const CHUNK: usize = 64 * 1024;

/// The SSH connection behind a remote shell, for opening more channels on it.
#[derive(Clone)]
pub struct SshHandle(pub(crate) Arc<Handle<Client>>);

impl fmt::Debug for SshHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SshHandle")
    }
}

impl SshHandle {
    /// Starts an SFTP session on this connection.
    pub async fn sftp(&self) -> Result<Sftp, SshError> {
        let handle = self.0.clone();
        on_runtime(async move {
            let mut channel = handle.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            loop {
                match channel.wait().await {
                    Some(ChannelMsg::Success) => break,
                    Some(ChannelMsg::Failure) | None => {
                        return Err(SshError::Files("the server does not offer SFTP".into()));
                    }
                    _ => {}
                }
            }
            let session = SftpSession::new(channel.into_stream())
                .await
                .map_err(remote_error)?;
            Ok(Sftp(Arc::new(session)))
        })
        .await
    }
}

/// An SFTP session. Clones share it; every call runs on the SSH thread.
#[derive(Clone)]
pub struct Sftp(Arc<SftpSession>);

impl fmt::Debug for Sftp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sftp")
    }
}

impl PartialEq for Sftp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    /// A link to a folder counts as a folder.
    pub is_dir: bool,
    pub size: u64,
    /// Permission bits, e.g. 0o644.
    pub mode: Option<u32>,
}

/// A transfer's bytes so far and in total, read by the UI; set `cancel` to stop it.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

/// `dir/name` for remote paths.
pub fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// The last component of a remote path.
pub fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(path)
}

impl Sftp {
    /// The login folder.
    pub async fn home(&self) -> Result<String, SshError> {
        let sftp = self.0.clone();
        on_runtime(async move { sftp.canonicalize(".").await.map_err(remote_error) }).await
    }

    pub async fn list(&self, dir: &str) -> Result<Vec<RemoteEntry>, SshError> {
        let (sftp, dir) = (self.0.clone(), dir.to_owned());
        on_runtime(async move {
            let mut entries = Vec::new();
            for entry in sftp.read_dir(dir.as_str()).await.map_err(remote_error)? {
                let mut meta = entry.metadata();
                if meta.is_symlink() {
                    // Follow the link; a dangling one stays a file.
                    if let Ok(target) = sftp.metadata(join(&dir, &entry.file_name())).await {
                        meta = target;
                    }
                }
                entries.push(RemoteEntry {
                    name: entry.file_name(),
                    is_dir: meta.is_dir(),
                    size: meta.size.unwrap_or(0),
                    mode: meta.permissions.map(|p| p & 0o7777),
                });
            }
            Ok(entries)
        })
        .await
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SshError> {
        let (sftp, from, to) = (self.0.clone(), from.to_owned(), to.to_owned());
        on_runtime(async move { sftp.rename(from, to).await.map_err(remote_error) }).await
    }

    pub async fn create_dir(&self, path: &str) -> Result<(), SshError> {
        let (sftp, path) = (self.0.clone(), path.to_owned());
        on_runtime(async move { sftp.create_dir(path).await.map_err(remote_error) }).await
    }

    /// Sets the permission bits (e.g. 0o755).
    pub async fn set_mode(&self, path: &str, mode: u32) -> Result<(), SshError> {
        let (sftp, path) = (self.0.clone(), path.to_owned());
        on_runtime(async move {
            let attrs = FileAttributes {
                permissions: Some(mode & 0o7777),
                ..FileAttributes::empty()
            };
            sftp.set_metadata(path, attrs).await.map_err(remote_error)
        })
        .await
    }

    /// Deletes a file, or a folder with everything in it. Links are removed, not followed.
    pub async fn remove(&self, path: &str) -> Result<(), SshError> {
        let (sftp, path) = (self.0.clone(), path.to_owned());
        on_runtime(async move {
            if !sftp
                .symlink_metadata(path.as_str())
                .await
                .map_err(remote_error)?
                .is_dir()
            {
                return sftp.remove_file(path).await.map_err(remote_error);
            }
            // Files go on the way down, folders deepest first on the way back.
            let mut pending = vec![path];
            let mut folders = Vec::new();
            while let Some(dir) = pending.pop() {
                for entry in sftp.read_dir(dir.as_str()).await.map_err(remote_error)? {
                    let child = join(&dir, &entry.file_name());
                    if entry.file_type().is_dir() {
                        pending.push(child);
                    } else {
                        sftp.remove_file(child).await.map_err(remote_error)?;
                    }
                }
                folders.push(dir);
            }
            for dir in folders.into_iter().rev() {
                sftp.remove_dir(dir).await.map_err(remote_error)?;
            }
            Ok(())
        })
        .await
    }

    /// Copies the remote file or folder `remote` into the local folder `local_dir`,
    /// replacing what is there.
    pub async fn download(
        &self,
        remote: &str,
        local_dir: PathBuf,
        progress: Arc<Progress>,
    ) -> Result<(), SshError> {
        let (sftp, remote) = (self.0.clone(), remote.to_owned());
        on_runtime(async move {
            let target = local_dir.join(base_name(&remote));
            let meta = sftp.metadata(remote.as_str()).await.map_err(remote_error)?;
            let mut files = Vec::new();
            if meta.is_dir() {
                let mut pending = vec![(remote, target)];
                while let Some((dir, local)) = pending.pop() {
                    tokio::fs::create_dir_all(&local)
                        .await
                        .map_err(|e| local_error(&local, e))?;
                    for entry in sftp.read_dir(dir.as_str()).await.map_err(remote_error)? {
                        let (name, meta) = (entry.file_name(), entry.metadata());
                        let pair = (join(&dir, &name), local.join(&name));
                        // Links inside a folder are skipped, so a loop cannot run forever.
                        if meta.is_symlink() {
                            continue;
                        }
                        if meta.is_dir() {
                            pending.push(pair);
                        } else if meta.is_regular() {
                            progress.total.fetch_add(meta.len(), Ordering::Relaxed);
                            files.push(pair);
                        }
                    }
                }
            } else {
                progress.total.fetch_add(meta.len(), Ordering::Relaxed);
                files.push((remote, target));
            }
            for (remote, local) in files {
                let copied = copy_down(&sftp, &remote, &local, &progress).await;
                if copied.is_err() {
                    _ = tokio::fs::remove_file(&local).await;
                }
                copied?;
            }
            Ok(())
        })
        .await
    }

    /// Copies the local file or folder `local` into the remote folder `remote_dir`,
    /// replacing what is there.
    pub async fn upload(
        &self,
        local: PathBuf,
        remote_dir: &str,
        progress: Arc<Progress>,
    ) -> Result<(), SshError> {
        let (sftp, remote_dir) = (self.0.clone(), remote_dir.to_owned());
        on_runtime(async move {
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| SshError::Files(format!("cannot upload {}", local.display())))?;
            let target = join(&remote_dir, &name);
            let meta = tokio::fs::metadata(&local)
                .await
                .map_err(|e| local_error(&local, e))?;
            let mut files = Vec::new();
            if meta.is_dir() {
                let mut pending = vec![(local, target)];
                while let Some((dir, remote)) = pending.pop() {
                    if !sftp.try_exists(remote.as_str()).await.unwrap_or(false) {
                        sftp.create_dir(remote.as_str())
                            .await
                            .map_err(remote_error)?;
                    }
                    let mut items = tokio::fs::read_dir(&dir)
                        .await
                        .map_err(|e| local_error(&dir, e))?;
                    while let Some(item) =
                        items.next_entry().await.map_err(|e| local_error(&dir, e))?
                    {
                        let name = item.file_name().to_string_lossy().into_owned();
                        let path = item.path();
                        let Ok(meta) = tokio::fs::metadata(&path).await else {
                            continue;
                        };
                        let pair = (path, join(&remote, &name));
                        if meta.is_dir() {
                            pending.push(pair);
                        } else {
                            progress.total.fetch_add(meta.len(), Ordering::Relaxed);
                            files.push(pair);
                        }
                    }
                }
            } else {
                progress.total.fetch_add(meta.len(), Ordering::Relaxed);
                files.push((local, target));
            }
            for (local, remote) in files {
                let copied = copy_up(&sftp, &local, &remote, &progress).await;
                if copied.is_err() {
                    _ = sftp.remove_file(remote.as_str()).await;
                }
                copied?;
            }
            Ok(())
        })
        .await
    }
}

async fn copy_down(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    progress: &Progress,
) -> Result<(), SshError> {
    let mut source = sftp.open(remote).await.map_err(remote_error)?;
    let mut target = tokio::fs::File::create(local)
        .await
        .map_err(|e| local_error(local, e))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(SshError::Cancelled);
        }
        let n = source
            .read(&mut buf)
            .await
            .map_err(|e| SshError::Files(format!("{remote}: {e}")))?;
        if n == 0 {
            break;
        }
        target
            .write_all(&buf[..n])
            .await
            .map_err(|e| local_error(local, e))?;
        progress.done.fetch_add(n as u64, Ordering::Relaxed);
    }
    target.flush().await.map_err(|e| local_error(local, e))
}

async fn copy_up(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &Progress,
) -> Result<(), SshError> {
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|e| local_error(local, e))?;
    let mut target = sftp.create(remote).await.map_err(remote_error)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(SshError::Cancelled);
        }
        let n = source
            .read(&mut buf)
            .await
            .map_err(|e| local_error(local, e))?;
        if n == 0 {
            break;
        }
        target
            .write_all(&buf[..n])
            .await
            .map_err(|e| SshError::Files(format!("{remote}: {e}")))?;
        progress.done.fetch_add(n as u64, Ordering::Relaxed);
    }
    target
        .shutdown()
        .await
        .map_err(|e| SshError::Files(format!("{remote}: {e}")))
}

fn remote_error(err: russh_sftp::client::error::Error) -> SshError {
    SshError::Files(err.to_string())
}

fn local_error(path: &Path, err: std::io::Error) -> SshError {
    SshError::Files(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_join_and_split() {
        assert_eq!(join("/home/kit", "a b"), "/home/kit/a b");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(base_name("/home/kit/notes.txt"), "notes.txt");
        assert_eq!(base_name("/home/kit/"), "kit");
        assert_eq!(base_name("/"), "/");
    }
}
