//! Async git runner: argv-only spawning with hardened env and flags, timeouts, cancellation
//! (drop the future: the child is killed), streamed stdout, and a command log for the UI's
//! operation strip.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_process::{Child, ChildStdout};
use bstr::ByteSlice;
use futures_lite::{AsyncReadExt as _, AsyncWriteExt as _, future};

use crate::detect::{self, GitBinary};
use crate::error::{FailureKind, GitError, Result, stderr_tail};
use crate::proctree::{self, ProcessTree};
use crate::progress::{self, LineSplitter, Progress};

/// Default timeout for [`Mode::Read`] commands.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of stderr kept per command (the tail matters, not the whole stream).
const STDERR_CAP: usize = 64 * 1024;
/// Records kept in the [`CommandLog`].
const LOG_CAP: usize = 200;
/// Chunk size for streamed stdout.
const CHUNK: usize = 64 * 1024;

/// Global options on every command.
const BASE_FLAGS: &[&str] = &[
    "-c",
    "core.quotepath=off",
    "-c",
    "color.ui=never",
    "-c",
    "core.pager=cat",
    "-c",
    "log.showSignature=false",
];

/// Extra global options for background reads.
fn read_flags() -> [&'static str; 8] {
    // A hooks dir that cannot exist, so no hook (e.g. post-index-change) can run.
    let no_hooks = if cfg!(windows) {
        "core.hooksPath=NUL"
    } else {
        "core.hooksPath=/dev/null"
    };
    [
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        no_hooks,
        "-c",
        "protocol.ext.allow=never",
        "--no-replace-objects",
    ]
}

/// Inherited variables removed from git's environment.
const ENV_REMOVE: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXTERNAL_DIFF",
    "GIT_EXEC_PATH",
];

/// Applies the env / window hardening shared by every git spawn (also `git --version`).
pub(crate) fn harden_std_command(cmd: &mut std::process::Command) {
    for var in ENV_REMOVE {
        cmd.env_remove(var);
    }
    cmd.env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Ok(path) = std::env::join_paths(detect::absolute_path_dirs(std::env::var_os("PATH"))) {
        cmd.env("PATH", path);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        // CreateProcess / SearchPath skip the current dir, so a repo can't plant ssh.exe etc.
        cmd.env("NoDefaultCurrentDirectoryInExePath", "1");
    }
}

/// Read-only background work vs user-initiated operations (which keep hooks, fsmonitor and
/// the user's config exactly as in a terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

/// One git invocation. Build with [`GitCommand::read`] / [`GitCommand::write`].
#[derive(Debug, Clone)]
pub struct GitCommand {
    cwd: PathBuf,
    mode: Mode,
    args: Vec<OsString>,
    literal_pathspecs: bool,
    timeout: Option<Duration>,
    stdin: Option<Vec<u8>>,
    /// `GIT_LFS_PROGRESS`: where git-lfs writes per-file transfer progress (set by the
    /// runner for write commands).
    lfs_progress: Option<PathBuf>,
    /// Extra variables for this command only (editor overrides for continue / rebase).
    env: Vec<(OsString, OsString)>,
    /// A write command nobody asked for (auto-fetch): hardened like a read.
    background: bool,
    /// Stdout bytes kept; past that the child is killed and the output is `truncated`.
    max_stdout: Option<usize>,
}

impl GitCommand {
    pub fn read(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd.into(), Mode::Read, Some(READ_TIMEOUT))
    }

    /// No timeout by default: fetch / push / rebase may legitimately take long.
    pub fn write(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd.into(), Mode::Write, None)
    }

    fn new(cwd: PathBuf, mode: Mode, timeout: Option<Duration>) -> Self {
        Self {
            cwd,
            mode,
            args: Vec::new(),
            literal_pathspecs: false,
            timeout,
            stdin: None,
            lfs_progress: None,
            env: Vec::new(),
            background: false,
            max_stdout: None,
        }
    }

    /// A fixed argument chosen by the app (subcommand, flag). Never pass user input here:
    /// use [`Self::rev`] or [`Self::paths`].
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// A user-supplied revision / ref / range (`main`, `origin/x`, `a...b`, `HEAD~2`).
    pub fn rev(mut self, rev: &str) -> Result<Self> {
        validate_rev(rev)?;
        self.args.push(rev.into());
        Ok(self)
    }

    /// Other user input placed after `--` (a URL, a folder): may contain spaces, but never
    /// starts with `-` or contains control characters.
    pub fn user_arg(mut self, what: &'static str, value: impl AsRef<OsStr>) -> Result<Self> {
        let value = value.as_ref();
        let text = value.to_string_lossy();
        if text.is_empty() || text.starts_with('-') || text.chars().any(char::is_control) {
            return Err(GitError::InvalidArgument {
                what,
                value: text.into_owned(),
            });
        }
        self.args.push(value.to_owned());
        Ok(self)
    }

    /// Appends `--` and the paths, matched literally (no glob or `:(magic)` pathspecs).
    pub fn paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<[u8]>,
    {
        self.args.push("--".into());
        self.args
            .extend(paths.into_iter().map(|p| bytes_to_os(p.as_ref())));
        self.literal_pathspecs = true;
        self
    }

    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets a variable for this command only. Values must not be secrets (see
    /// [`GitRunner::set_env`]).
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Marks a write command the user didn't start (auto-fetch): it gets the read hardening
    /// (no hooks, no fsmonitor) and the repo-local config overrides of [`crate::guard`].
    pub fn background(mut self) -> Self {
        self.background = true;
        self
    }

    /// Keeps at most `cap` bytes of stdout (see [`GitOutput::truncated`]).
    pub fn max_stdout(mut self, cap: usize) -> Self {
        self.max_stdout = Some(cap);
        self
    }

    /// Read or background: repo-local config must not pick programs to run.
    fn guarded(&self) -> bool {
        self.mode == Mode::Read || self.background
    }

    pub fn stdin(mut self, input: Vec<u8>) -> Self {
        self.stdin = Some(input);
        self
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `git status --porcelain=v2 ...` as shown in the operation strip (safety flags omitted,
    /// secrets and URL userinfo masked).
    pub fn display(&self) -> String {
        crate::redact::redact(&self.raw_display()).into_owned()
    }

    fn raw_display(&self) -> String {
        let mut out = String::from("git");
        for arg in &self.args {
            let arg = arg.to_string_lossy();
            out.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                out.push('"');
                out.push_str(&arg);
                out.push('"');
            } else {
                out.push_str(&arg);
            }
        }
        out
    }

    fn to_command(&self, git: &GitBinary) -> async_process::Command {
        let mut cmd = std::process::Command::new(&git.path);
        cmd.args(BASE_FLAGS);
        if self.guarded() {
            cmd.args(read_flags());
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
        }
        cmd.args(&self.args).current_dir(&self.cwd);
        harden_std_command(&mut cmd);
        proctree::prepare(&mut cmd);
        if self.guarded() {
            let guard = crate::guard::overrides(&git.path, &self.cwd);
            cmd.env("GIT_CONFIG_COUNT", guard.config.len().to_string());
            for (i, (key, value)) in guard.config.iter().enumerate() {
                cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
                    .env(format!("GIT_CONFIG_VALUE_{i}"), value);
            }
            for (key, value) in &guard.env {
                cmd.env(key, value);
            }
        }
        if self.literal_pathspecs {
            cmd.env("GIT_LITERAL_PATHSPECS", "1");
        }
        if let Some(file) = &self.lfs_progress {
            cmd.env("GIT_LFS_PROGRESS", file);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        // Stdio must be set after the conversion: `From<std::process::Command>` drops it.
        let mut cmd = async_process::Command::from(cmd);
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        cmd
    }
}

/// Rejects revisions that git could read as an option, or that can't be a revision at all.
pub fn validate_rev(rev: &str) -> Result<()> {
    let bad = rev.is_empty()
        || rev.starts_with('-')
        || rev.len() > 4096
        || rev.chars().any(|c| c.is_control() || c == ' ');
    if bad {
        return Err(GitError::InvalidArgument {
            what: "revision",
            value: rev.to_string(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(bytes.to_vec())
}

/// Git for Windows speaks UTF-8 paths.
#[cfg(not(unix))]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    OsString::from(bytes.to_str_lossy().into_owned())
}

/// Finished command.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Stdout hit [`GitCommand::max_stdout`]: it is the start of the output, and git was killed.
    pub truncated: bool,
}

impl GitOutput {
    /// Exit code 0, or stopped on purpose at the stdout cap.
    pub fn success(&self) -> bool {
        self.code == Some(0) || self.truncated
    }
}

/// Runs git commands for any number of repos; cheap to clone.
#[derive(Clone)]
pub struct GitRunner {
    git: Arc<GitBinary>,
    log: CommandLog,
    /// Extra variables for every write command (the askpass hook). Never holds secrets.
    env: Arc<Mutex<Vec<(OsString, OsString)>>>,
}

impl GitRunner {
    pub fn new(git: GitBinary) -> Self {
        Self {
            git: Arc::new(git),
            log: CommandLog::default(),
            env: Arc::default(),
        }
    }

    /// Uses the cached [`detect::git`] result (blocking on first call).
    pub fn detect() -> Result<Self> {
        Ok(Self::new(detect::git()?))
    }

    pub fn git(&self) -> &GitBinary {
        &self.git
    }

    pub fn log(&self) -> &CommandLog {
        &self.log
    }

    /// Sets variables added to every git this runner (and its clones) spawns from now on,
    /// e.g. `GIT_ASKPASS`. Values must not be secrets: child environments are visible to
    /// other processes of the same user.
    pub fn set_env(&self, vars: Vec<(OsString, OsString)>) {
        *self.env.lock().unwrap_or_else(|p| p.into_inner()) = vars;
    }

    /// Runs to completion; a non-zero exit is an error. Returns stdout.
    pub async fn run(&self, cmd: GitCommand) -> Result<Vec<u8>> {
        let display = cmd.display();
        let out = self.output(cmd).await?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(failed(display, out.code, &out.stderr))
        }
    }

    /// Like [`Self::run`], keeping at most `cap` bytes of stdout; the flag says whether there
    /// was more (git was then stopped, so that is not an error).
    pub async fn run_capped(&self, cmd: GitCommand, cap: usize) -> Result<(Vec<u8>, bool)> {
        let display = cmd.display();
        let out = self.output(cmd.max_stdout(cap)).await?;
        if out.success() {
            Ok((out.stdout, out.truncated))
        } else {
            Err(failed(display, out.code, &out.stderr))
        }
    }

    /// Runs to completion and returns the exit code instead of failing on it (for commands
    /// where a non-zero exit is an answer, e.g. `merge-base --is-ancestor`).
    pub async fn output(&self, mut cmd: GitCommand) -> Result<GitOutput> {
        let display = cmd.display();
        let mut guard = self.log.start(&cmd);
        let progress = LfsProgress::for_command(&mut cmd, guard.id);
        let mut child = self.spawn(&cmd)?;
        // Dropping `work` (cancel, timeout) drops the tree, which kills it; finishing releases it.
        let tree = ProcessTree::attach(&child);

        let stdin = child.stdin.take();
        let input = cmd.stdin.clone();
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let cap = cmd.max_stdout;
        let work = async move {
            let write_stdin = async {
                if let (Some(mut pipe), Some(input)) = (stdin, input) {
                    let written = async {
                        pipe.write_all(&input).await?;
                        pipe.close().await
                    };
                    match written.await {
                        // git stopped reading (usually it failed): its exit code and stderr
                        // explain why, so report those instead of the pipe error.
                        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                        other => other?,
                    }
                }
                Ok::<_, std::io::Error>(())
            };
            let read_stdout = async {
                let mut buf = Vec::new();
                let mut truncated = false;
                if let Some(pipe) = stdout_pipe.as_mut() {
                    match cap {
                        Some(cap) => truncated = read_head(pipe, &mut buf, cap).await?,
                        None => {
                            pipe.read_to_end(&mut buf).await?;
                        }
                    }
                }
                if truncated {
                    // Git is blocked writing to the pipe nobody reads: stop it, or stderr never
                    // reaches EOF.
                    tree.kill();
                    _ = child.kill();
                }
                Ok::<_, std::io::Error>((buf, truncated))
            };
            let read_stderr = async {
                match stderr_pipe.as_mut() {
                    Some(pipe) => read_capped(pipe, STDERR_CAP).await,
                    None => Ok(Vec::new()),
                }
            };
            let ((w, out), err) =
                future::zip(future::zip(write_stdin, read_stdout), read_stderr).await;
            w?;
            let (stdout, truncated) = out?;
            let status = child.status().await?;
            tree.release();
            Ok::<_, std::io::Error>(GitOutput {
                code: status.code(),
                stdout,
                stderr: err?,
                truncated,
            })
        };

        let run = async {
            match cmd.timeout {
                Some(after) => {
                    let timed_out = async {
                        async_io::Timer::after(after).await;
                        Err(GitError::Timeout {
                            command: display.clone(),
                            after,
                        })
                    };
                    future::or(async { work.await.map_err(GitError::Io) }, timed_out).await
                }
                None => work.await.map_err(GitError::Io),
            }
        };
        let result = match &progress {
            Some(progress) => {
                future::or(run, async { match progress.watch(&self.log).await {} }).await
            }
            None => run.await,
        };
        guard.finish(match &result {
            Ok(out) if out.success() => Outcome::Ok,
            Ok(out) => Outcome::Failed { code: out.code },
            Err(GitError::Timeout { .. }) => Outcome::TimedOut,
            Err(_) => Outcome::Failed { code: None },
        });
        result
    }

    /// Runs to completion, reporting `--progress` lines from stderr as they arrive (clone,
    /// fetch, push). A non-zero exit is an error. Drop the future to cancel.
    pub async fn run_with_progress(
        &self,
        mut cmd: GitCommand,
        on_progress: impl FnMut(Progress) + Send,
    ) -> Result<Vec<u8>> {
        let id = self.log.next_id();
        let progress = LfsProgress::for_command(&mut cmd, id);
        let run = self.run_with_progress_inner(cmd, on_progress);
        match &progress {
            Some(progress) => {
                future::or(run, async { match progress.watch(&self.log).await {} }).await
            }
            None => run.await,
        }
    }

    async fn run_with_progress_inner(
        &self,
        cmd: GitCommand,
        mut on_progress: impl FnMut(Progress) + Send,
    ) -> Result<Vec<u8>> {
        let display = cmd.display();
        let mut guard = self.log.start(&cmd);
        let mut child = self.spawn(&cmd)?;
        let tree = ProcessTree::attach(&child);
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let read_stdout = async {
            let mut buf = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                pipe.read_to_end(&mut buf).await?;
            }
            Ok::<_, std::io::Error>(buf)
        };
        let read_stderr = async {
            let mut tail = VecDeque::new();
            let mut lines = LineSplitter::default();
            let mut buf = [0u8; 4096];
            let mut emit = |line: &str| {
                if let Some(progress) = progress::parse_line(line) {
                    on_progress(progress);
                }
            };
            if let Some(pipe) = stderr_pipe.as_mut() {
                loop {
                    let n = pipe.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    lines.push(&buf[..n], &mut emit);
                    tail.extend(&buf[..n]);
                    let excess = tail.len().saturating_sub(STDERR_CAP);
                    tail.drain(..excess);
                }
            }
            lines.finish(&mut emit);
            Ok::<_, std::io::Error>(Vec::from(tail))
        };
        let (out, err) = future::zip(read_stdout, read_stderr).await;
        let (out, err) = (out?, err?);
        let status = child.status().await?;
        tree.release();
        if status.success() {
            guard.finish(Outcome::Ok);
            Ok(out)
        } else {
            guard.finish(Outcome::Failed {
                code: status.code(),
            });
            Err(failed(display, status.code(), &err))
        }
    }

    /// Spawns and streams stdout (e.g. `git log` for the graph). No timeout; drop the stream
    /// to cancel. Call [`GitStream::finish`] after the last chunk to surface a failure.
    pub fn stream(&self, cmd: GitCommand) -> Result<GitStream> {
        let guard = self.log.start(&cmd);
        let mut child = self.spawn(&cmd)?;
        let tree = ProcessTree::attach(&child);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take().map(|mut pipe| {
            // Drain stderr on its own thread so a chatty git can't block on a full pipe.
            std::thread::spawn(move || {
                async_io::block_on(read_capped(&mut pipe, STDERR_CAP)).unwrap_or_default()
            })
        });
        Ok(GitStream {
            display: cmd.display(),
            child,
            tree,
            stdout,
            stderr,
            guard,
        })
    }

    fn spawn(&self, cmd: &GitCommand) -> Result<Child> {
        tracing::debug!(command = %cmd.display(), cwd = %cmd.cwd.display(), "git");
        let mut command = cmd.to_command(&self.git);
        // Reads never authenticate, so their hooks / filters never see the askpass hook.
        if cmd.mode == Mode::Write {
            for (key, value) in self.env.lock().unwrap_or_else(|p| p.into_inner()).iter() {
                command.env(key, value);
            }
        }
        command.spawn().map_err(|source| GitError::Spawn {
            program: self.git.path.clone(),
            source,
        })
    }
}

/// A running command whose stdout is read in chunks.
pub struct GitStream {
    display: String,
    child: Child,
    /// Killed with the stream if it is dropped before [`Self::finish`].
    tree: ProcessTree,
    stdout: Option<ChildStdout>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
    guard: LogGuard,
}

impl GitStream {
    /// Next stdout chunk (up to 64 KiB), or `None` at EOF.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(pipe) = self.stdout.as_mut() else {
            return Ok(None);
        };
        let mut buf = vec![0; CHUNK];
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            self.stdout = None;
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some(buf))
    }

    /// Waits for exit; a non-zero exit becomes [`GitError::Failed`].
    pub async fn finish(mut self) -> Result<()> {
        self.stdout = None;
        let status = self.child.status().await?;
        self.tree.release();
        let stderr = self
            .stderr
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        if status.success() {
            self.guard.finish(Outcome::Ok);
            Ok(())
        } else {
            self.guard.finish(Outcome::Failed {
                code: status.code(),
            });
            Err(failed(self.display.clone(), status.code(), &stderr))
        }
    }
}

/// Appends up to `cap` bytes of `pipe` to `buf`; true when the pipe had more.
async fn read_head<R: futures_lite::AsyncRead + Unpin>(
    pipe: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<bool> {
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(false);
        }
        let room = cap.saturating_sub(buf.len());
        if n > room {
            buf.extend_from_slice(&chunk[..room]);
            return Ok(true);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn read_capped<R: futures_lite::AsyncRead + Unpin>(
    pipe: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let mut kept = VecDeque::with_capacity(4096);
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        kept.extend(&buf[..n]);
        let excess = kept.len().saturating_sub(cap);
        kept.drain(..excess);
    }
    Ok(kept.into())
}

pub(crate) fn failed(command: String, code: Option<i32>, stderr: &[u8]) -> GitError {
    let tail = stderr_tail(stderr, 8);
    GitError::Failed {
        command,
        code,
        kind: FailureKind::classify(&tail),
        stderr_tail: tail,
    }
}

// ---- command log --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Running,
    Ok,
    Failed {
        code: Option<i32>,
    },
    TimedOut,
    /// The future / stream was dropped before git finished (the child was killed).
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub id: u64,
    /// As shown to the user: `git status --porcelain=v2 ...`.
    pub command: String,
    pub cwd: PathBuf,
    pub mode: Mode,
    pub started: SystemTime,
    pub duration: Option<Duration>,
    pub outcome: Outcome,
    /// Latest transfer progress while running (Git LFS), e.g. `LFS download 3/12 · a.umap 50%`.
    pub progress: Option<String>,
}

type Listener = Arc<dyn Fn(&CommandRecord) + Send + Sync>;

/// The most recent commands with their outcome, for the operation strip.
#[derive(Clone, Default)]
pub struct CommandLog {
    inner: Arc<Mutex<LogInner>>,
}

#[derive(Default)]
struct LogInner {
    next_id: u64,
    records: VecDeque<CommandRecord>,
    listener: Option<Listener>,
}

impl CommandLog {
    /// Newest last.
    pub fn records(&self) -> Vec<CommandRecord> {
        self.lock().records.iter().cloned().collect()
    }

    /// Called on every start and finish (from whichever thread ran the command).
    pub fn set_listener(&self, listener: impl Fn(&CommandRecord) + Send + Sync + 'static) {
        self.lock().listener = Some(Arc::new(listener));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start(&self, cmd: &GitCommand) -> LogGuard {
        let (record, listener) = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let record = CommandRecord {
                id: inner.next_id,
                command: cmd.display(),
                cwd: cmd.cwd.clone(),
                mode: cmd.mode,
                started: SystemTime::now(),
                duration: None,
                outcome: Outcome::Running,
                progress: None,
            };
            inner.records.push_back(record.clone());
            if inner.records.len() > LOG_CAP {
                inner.records.pop_front();
            }
            (record, inner.listener.clone())
        };
        if let Some(listener) = listener {
            listener(&record);
        }
        LogGuard {
            log: self.clone(),
            id: record.id,
            started: Instant::now(),
            done: false,
        }
    }

    /// The id the next [`Self::start`] hands out (for files named after the record).
    fn next_id(&self) -> u64 {
        self.lock().next_id + 1
    }

    fn progress(&self, id: u64, text: String) {
        let (record, listener) = {
            let mut inner = self.lock();
            let Some(record) = inner.records.iter_mut().find(|r| r.id == id) else {
                return;
            };
            if record.progress.as_deref() == Some(text.as_str()) {
                return;
            }
            record.progress = Some(text);
            (record.clone(), inner.listener.clone())
        };
        if let Some(listener) = listener {
            listener(&record);
        }
    }

    fn finish(&self, id: u64, duration: Duration, outcome: Outcome) {
        let (record, listener) = {
            let mut inner = self.lock();
            let Some(record) = inner.records.iter_mut().find(|r| r.id == id) else {
                return;
            };
            record.duration = Some(duration);
            record.outcome = outcome;
            let record = record.clone();
            (record, inner.listener.clone())
        };
        if let Some(listener) = listener {
            listener(&record);
        }
    }
}

/// A write command's `GIT_LFS_PROGRESS` file: tailed into its log record while git runs,
/// deleted when done.
struct LfsProgress {
    id: u64,
    file: PathBuf,
}

impl LfsProgress {
    /// Write commands may move LFS objects (checkout, pull, push, clone, `lfs pull`).
    fn for_command(cmd: &mut GitCommand, id: u64) -> Option<Self> {
        if cmd.mode != Mode::Write {
            return None;
        }
        let file = tempfile::Builder::new()
            .prefix(&format!("maomi-lfs-{id}-"))
            .suffix(".progress")
            .tempfile()
            .ok()?
            .into_temp_path()
            .keep()
            .ok()?;
        cmd.lfs_progress = Some(file.clone());
        Some(Self { id, file })
    }

    /// Reports the newest progress line every 300 ms; never returns (race it against git).
    async fn watch(&self, log: &CommandLog) -> std::convert::Infallible {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut offset = 0u64;
        let mut tail = String::new();
        loop {
            async_io::Timer::after(Duration::from_millis(300)).await;
            let Ok(mut file) = std::fs::File::open(&self.file) else {
                continue;
            };
            let mut fresh = String::new();
            if file.seek(SeekFrom::Start(offset)).is_err()
                || file.read_to_string(&mut fresh).is_err()
            {
                continue;
            }
            offset += fresh.len() as u64;
            tail.push_str(&fresh);
            // Keep only the unfinished last line; report the last complete one.
            let complete = tail.rfind('\n').map(|end| tail[..end].to_string());
            if let Some(done) = complete {
                tail = tail[done.len() + 1..].to_string();
                if let Some(text) = done
                    .lines()
                    .next_back()
                    .and_then(crate::progress::describe_lfs)
                {
                    log.progress(self.id, text);
                }
            }
        }
    }
}

impl Drop for LfsProgress {
    fn drop(&mut self) {
        _ = std::fs::remove_file(&self.file);
    }
}

/// Marks its record finished; dropped unfinished means the command was cancelled.
struct LogGuard {
    log: CommandLog,
    id: u64,
    started: Instant,
    done: bool,
}

impl LogGuard {
    fn finish(&mut self, outcome: Outcome) {
        self.done = true;
        self.log.finish(self.id, self.started.elapsed(), outcome);
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if !self.done {
            self.log
                .finish(self.id, self.started.elapsed(), Outcome::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> GitRunner {
        GitRunner::detect().expect("git is installed")
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = runner();
        async_io::block_on(r.run(GitCommand::write(dir.path()).args(["init", "-q"]))).unwrap();
        dir
    }

    #[test]
    fn rejects_option_like_revisions() {
        for bad in ["", "-x", "--upload-pack=evil", "a b", "a\nb", "x\0y"] {
            assert!(validate_rev(bad).is_err(), "{bad:?}");
        }
        for ok in [
            "main",
            "origin/main",
            "HEAD~2",
            "a...b",
            "v1.0^{commit}",
            "refs/heads/x",
        ] {
            assert!(validate_rev(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn display_hides_safety_flags() {
        let cmd = GitCommand::read(".")
            .args(["log", "--format=%H %s"])
            .paths(["a b.txt"]);
        assert_eq!(cmd.display(), r#"git log "--format=%H %s" -- "a b.txt""#);
    }

    #[test]
    fn runs_and_logs() {
        let dir = repo();
        let r = runner();
        let out = async_io::block_on(
            r.run(GitCommand::read(dir.path()).args(["rev-parse", "--is-inside-work-tree"])),
        )
        .unwrap();
        assert_eq!(out.trim_ascii(), b"true");
        let records = r.log().records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Outcome::Ok);
        assert!(records[0].duration.is_some());
    }

    #[test]
    fn failure_is_structured() {
        let dir = tempfile::tempdir().unwrap();
        let err = async_io::block_on(
            runner().run(GitCommand::read(dir.path()).args(["rev-parse", "HEAD"])),
        )
        .unwrap_err();
        match err {
            GitError::Failed { kind, code, .. } => {
                assert_eq!(kind, FailureKind::NotARepository);
                assert_ne!(code, Some(0));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stdin_and_literal_paths() {
        let dir = repo();
        let r = runner();
        // hash-object reads stdin; `*` must be taken literally, not as a glob.
        let oid = async_io::block_on(
            r.run(
                GitCommand::read(dir.path())
                    .args(["hash-object", "--stdin"])
                    .stdin(b"hello\n".to_vec()),
            ),
        )
        .unwrap();
        assert_eq!(
            oid.trim_ascii(),
            b"ce013625030ba8dba906f756967f9e9ca394464a"
        );

        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let listed = async_io::block_on(
            r.run(
                GitCommand::read(dir.path())
                    .args(["ls-files", "--others"])
                    .paths(["*.txt"]),
            ),
        )
        .unwrap();
        assert!(listed.is_empty(), "glob matched: {:?}", listed.as_bstr());
    }

    #[test]
    fn head_reads_stop_at_the_cap() {
        let data = vec![7u8; 20_000];
        for (cap, expect_more) in [
            (100, true),
            (19_999, true),
            (20_000, false),
            (30_000, false),
        ] {
            let mut buf = Vec::new();
            let mut pipe = futures_lite::io::Cursor::new(data.clone());
            let more = async_io::block_on(read_head(&mut pipe, &mut buf, cap)).unwrap();
            assert_eq!(more, expect_more, "cap {cap}");
            assert_eq!(buf.len(), cap.min(data.len()), "cap {cap}");
        }
    }

    #[test]
    fn capped_run_kills_git_and_reports_it() {
        let dir = repo();
        let r = runner();
        let body = "x".repeat(2_000_000);
        // A 2 MB blob to read back through the cap.
        let oid = async_io::block_on(
            r.run(
                GitCommand::write(dir.path())
                    .args(["hash-object", "-w", "--stdin"])
                    .stdin(body.clone().into_bytes()),
            ),
        )
        .unwrap();
        let oid = String::from_utf8(oid).unwrap();
        let show = || GitCommand::read(dir.path()).args(["cat-file", "blob", oid.trim()]);
        let (out, truncated) = async_io::block_on(r.run_capped(show(), 100_000)).unwrap();
        assert!(truncated);
        assert_eq!(out.len(), 100_000);
        let (out, truncated) = async_io::block_on(r.run_capped(show(), 3_000_000)).unwrap();
        assert!(!truncated);
        assert_eq!(out.len(), body.len());
        // A real failure under a cap is still an error.
        let bad = GitCommand::read(dir.path()).args(["cat-file", "blob", "0000000"]);
        assert!(async_io::block_on(r.run_capped(bad, 100)).is_err());
    }

    #[test]
    fn timeout_kills() {
        let dir = repo();
        let r = runner();
        // A zero timeout fires before a freshly spawned git can possibly have exited.
        let result = async_io::block_on(
            r.output(
                GitCommand::read(dir.path())
                    .args(["log", "--all"])
                    .timeout(Some(Duration::ZERO)),
            ),
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert_eq!(r.log().records()[0].outcome, Outcome::TimedOut);
    }

    /// `git beat`: a shell alias, so the loop is a grandchild of git, that appends to `beat`
    /// every 0.2 s until killed.
    fn beat_cmd(dir: &Path, beat: &Path) -> GitCommand {
        let beat = beat.to_string_lossy().replace('\\', "/");
        GitCommand::write(dir).args([
            "-c".to_string(),
            format!("alias.beat=!while :; do echo x >> \"{beat}\"; sleep 0.2; done"),
            "beat".to_string(),
        ])
    }

    /// The loop of [`beat_cmd`] ran, and is gone now.
    fn assert_beat_stopped(beat: &Path) {
        let size = || std::fs::metadata(beat).map_or(0, |m| m.len());
        std::thread::sleep(Duration::from_millis(500));
        let stopped_at = size();
        assert!(stopped_at > 0, "the alias never started");
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(size(), stopped_at, "the grandchild survived the kill");
    }

    #[test]
    fn timeout_kills_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let beat = dir.path().join("beat");
        let cmd = beat_cmd(dir.path(), &beat).timeout(Some(Duration::from_secs(2)));
        let result = async_io::block_on(runner().output(cmd));
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert_beat_stopped(&beat);
    }

    #[test]
    fn cancel_kills_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let r = runner();
        let cancel_after = |secs| async move {
            async_io::Timer::after(Duration::from_secs(secs)).await;
        };

        // Dropping `output` (what a closed tab does).
        let beat = dir.path().join("beat-output");
        async_io::block_on(future::or(
            async {
                _ = r.output(beat_cmd(dir.path(), &beat)).await;
            },
            cancel_after(2),
        ));
        assert_beat_stopped(&beat);

        // Dropping `run_with_progress` (a cancelled clone / fetch).
        let beat = dir.path().join("beat-progress");
        async_io::block_on(future::or(
            async {
                _ = r
                    .run_with_progress(beat_cmd(dir.path(), &beat), |_| {})
                    .await;
            },
            cancel_after(2),
        ));
        assert_beat_stopped(&beat);

        // Dropping a stream.
        let beat = dir.path().join("beat-stream");
        let stream = r.stream(beat_cmd(dir.path(), &beat)).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        drop(stream);
        assert_beat_stopped(&beat);
    }

    #[test]
    fn finished_commands_leave_their_background_processes_alone() {
        // `git` exits right away; the background loop it started must outlive the command.
        let dir = tempfile::tempdir().unwrap();
        let ended = dir.path().join("loop-ended");
        let marker = ended.to_string_lossy().replace('\\', "/");
        let script = format!("!(sleep 1; echo y > \"{marker}\") > /dev/null 2>&1 &");
        let cmd = GitCommand::write(dir.path()).args([
            "-c".to_string(),
            format!("alias.bg={script}"),
            "bg".to_string(),
        ]);
        async_io::block_on(runner().output(cmd)).unwrap();
        // Slow machines get a generous wait; the marker normally shows up after ~1 s.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ended.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            ended.exists(),
            "the background process was killed on completion"
        );
    }

    #[test]
    fn streams_and_cancels() {
        let dir = repo();
        let r = runner();
        for i in 0..3 {
            let cmd = GitCommand::write(dir.path()).args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
            ]);
            async_io::block_on(r.run(cmd.arg(format!("c{i}")))).unwrap();
        }
        let mut stream = r
            .stream(GitCommand::read(dir.path()).args(["log", "--format=%s"]))
            .unwrap();
        let mut all = Vec::new();
        while let Some(chunk) = async_io::block_on(stream.next_chunk()).unwrap() {
            all.extend(chunk);
        }
        async_io::block_on(stream.finish()).unwrap();
        assert_eq!(all, b"c2\nc1\nc0\n");

        let stream = r
            .stream(GitCommand::read(dir.path()).args(["log", "--format=%s"]))
            .unwrap();
        drop(stream);
        let last = r.log().records().pop().unwrap();
        assert_eq!(last.outcome, Outcome::Cancelled);
    }
}
