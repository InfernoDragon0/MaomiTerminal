//! A shell running in a PTY, parsed into an alacritty_terminal grid.
//!
//! Threads per session: a reader (PTY → OSC scan → `Term`), a writer (input queue → PTY) and a
//! waiter (child exit → close the PTY → `Exited`). The UI reads through [`PtySession::snapshot`]
//! and the [`TermEvent`] channel.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Processor, Rgb, StdSyncHandler};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::integration;
use crate::osc::{OscScanner, PromptMark, ShellMark};
use crate::shell::{Shell, default_shell};
use crate::snapshot::GridSnapshot;

/// Bytes read from the PTY per lock of the terminal.
const READ_BUFFER: usize = 0x10000;
/// How long the waiter waits for the reader to drain the last output after the child exits.
/// A background job that keeps the PTY open would otherwise hold the `Exited` event forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Grid and pixel size of a terminal view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl TermSize {
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            cell_width: 8,
            cell_height: 16,
        }
    }

    fn clamped(self) -> Self {
        Self {
            columns: self.columns.max(2),
            rows: self.rows.max(1),
            ..self
        }
    }

    fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.columns,
            pixel_width: self.columns.saturating_mul(self.cell_width),
            pixel_height: self.rows.saturating_mul(self.cell_height),
        }
    }

    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows,
            num_cols: self.columns,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.columns as usize
    }
}

/// How to start a session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// None = [`default_shell`].
    pub shell: Option<Shell>,
    /// None = the user's home folder.
    pub cwd: Option<PathBuf>,
    /// Extra environment on top of the app's own.
    pub env: Vec<(OsString, OsString)>,
    pub size: TermSize,
    /// Lines of scrollback kept.
    pub scrollback: usize,
    /// Inject the cwd / prompt-mark snippets (see `integration`).
    pub shell_integration: bool,
    /// Let programs copy to the clipboard with OSC 52. Paste through OSC 52 is never allowed.
    pub osc52_copy: bool,
    /// Cursor shape until a program sets its own.
    pub cursor_shape: CursorShape,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: Vec::new(),
            size: TermSize::new(80, 24),
            scrollback: 10_000,
            shell_integration: true,
            osc52_copy: false,
            cursor_shape: CursorShape::Block,
        }
    }
}

/// Whether the shell is waiting for input, from OSC 133 marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptState {
    /// No marks seen: the shell has no integration. Callers treat this as busy.
    #[default]
    Unknown,
    /// Prompt shown; the user is typing or idle.
    Prompt,
    /// A command is running.
    Running,
}

/// Something the UI should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// New output. Sent once until the next [`PtySession::snapshot`], so a burst of output
    /// costs one redraw per frame.
    Wakeup,
    Title(String),
    ResetTitle,
    Bell,
    /// OSC 52 copy (only when [`SessionOptions::osc52_copy`] is on).
    ClipboardCopy(String),
    CwdChanged(PathBuf),
    PromptChanged(PromptState),
    /// The shell exited and its output is fully parsed. `None` if the exit code was unreadable.
    Exited(Option<u32>),
}

/// Default colours used to answer OSC 4/10/11 queries (vim, bat and others ask for the
/// background to pick a light or dark scheme). The UI sets them from the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryPalette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for QueryPalette {
    fn default() -> Self {
        let rgb = |hex: u32| Rgb {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
        };
        Self {
            foreground: rgb(0xd8d8d8),
            background: rgb(0x181818),
            cursor: rgb(0xd8d8d8),
            ansi: [
                0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6,
                0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
            ]
            .map(rgb),
        }
    }
}

impl QueryPalette {
    /// Colour for alacritty's colour index: 0-255 palette, 256 fg, 257 bg, 258 cursor.
    fn color(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
                Rgb {
                    r: level(i / 36),
                    g: level(i / 6 % 6),
                    b: level(i % 6),
                }
            }
            232..=255 => {
                let v = (8 + (index - 232) * 10) as u8;
                Rgb { r: v, g: v, b: v }
            }
            257 => self.background,
            258 => self.cursor,
            _ => self.foreground,
        }
    }
}

#[derive(Debug, Default)]
struct ShellState {
    title: Option<String>,
    cwd: Option<PathBuf>,
    /// Once the shell reports its cwd, the process-cwd fallback stops.
    cwd_reported: bool,
    prompt: PromptState,
    last_command_exit: Option<i32>,
    exited: Option<Option<u32>>,
}

/// State shared by the session, its threads and the terminal's event listener.
#[derive(Debug)]
struct Shared {
    events: async_channel::Sender<TermEvent>,
    wakeup_pending: AtomicBool,
    /// ConPTY asked for win32-input-mode keys (`CSI ? 9001 h`).
    win32_input: AtomicBool,
    state: Mutex<ShellState>,
    size: Mutex<TermSize>,
    palette: Mutex<QueryPalette>,
    input: mpsc::Sender<Vec<u8>>,
}

impl Shared {
    fn send(&self, event: TermEvent) {
        // Unbounded; fails only when every receiver is gone, which is fine to ignore.
        let _ = self.events.try_send(event);
    }

    fn wake(&self) {
        if !self.wakeup_pending.swap(true, Ordering::AcqRel) {
            self.send(TermEvent::Wakeup);
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ShellState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `reported` = the shell said so (OSC); false for the process-cwd fallback.
    fn set_cwd(&self, path: PathBuf, reported: bool) {
        {
            let mut state = self.state();
            state.cwd_reported |= reported;
            if state.cwd.as_ref() == Some(&path) {
                return;
            }
            state.cwd = Some(path.clone());
        }
        self.send(TermEvent::CwdChanged(path));
    }

    fn apply_mark(&self, mark: ShellMark) {
        match mark {
            ShellMark::Cwd(path) => self.set_cwd(path, true),
            ShellMark::Prompt(mark) => self.set_prompt(mark),
            ShellMark::Win32InputMode(on) => self.win32_input.store(on, Ordering::Release),
        }
    }

    fn set_prompt(&self, mark: PromptMark) {
        let next = {
            let mut state = self.state();
            let next = match mark {
                PromptMark::PromptStart | PromptMark::CommandStart => PromptState::Prompt,
                PromptMark::CommandExecuted => PromptState::Running,
                PromptMark::CommandFinished(code) => {
                    state.last_command_exit = code;
                    PromptState::Prompt
                }
            };
            if state.prompt == next {
                return;
            }
            state.prompt = next;
            next
        };
        self.send(TermEvent::PromptChanged(next));
    }
}

/// Receives events from inside `Term` (called with the terminal locked).
#[derive(Debug, Clone)]
pub struct EventProxy {
    shared: Arc<Shared>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let shared = &self.shared;
        match event {
            Event::PtyWrite(text) => {
                let _ = shared.input.send(text.into_bytes());
            }
            Event::Title(title) => {
                shared.state().title = Some(title.clone());
                shared.send(TermEvent::Title(title));
            }
            Event::ResetTitle => {
                shared.state().title = None;
                shared.send(TermEvent::ResetTitle);
            }
            Event::Bell => shared.send(TermEvent::Bell),
            Event::ClipboardStore(_, text) => shared.send(TermEvent::ClipboardCopy(text)),
            Event::ColorRequest(index, format) => {
                let rgb = shared
                    .palette
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .color(index);
                let _ = shared.input.send(format(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *shared.size.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = shared.input.send(format(size.window_size()).into_bytes());
            }
            // Wakeup comes from the reader instead; the rest have no UI meaning here.
            Event::Wakeup
            | Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::ClipboardLoad(..)
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}

/// A running shell and its terminal state.
pub struct PtySession {
    term: Arc<FairMutex<Term<EventProxy>>>,
    shared: Arc<Shared>,
    events: async_channel::Receiver<TermEvent>,
    /// Taken (and so closed) when the child exits; ConPTY only ends the output pipe on close.
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    shell: Shell,
    osc52_copy: bool,
}

fn term_config(scrollback: usize, cursor_shape: CursorShape, osc52_copy: bool) -> TermConfig {
    TermConfig {
        scrolling_history: scrollback,
        // Blinking is the view's job (the cursor_blink setting), so the style never blinks.
        default_cursor_style: CursorStyle {
            shape: cursor_shape,
            blinking: false,
        },
        osc52: if osc52_copy {
            Osc52::OnlyCopy
        } else {
            Osc52::Disabled
        },
        ..TermConfig::default()
    }
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession")
            .field("shell", &self.shell)
            .field("pid", &self.pid)
            .finish()
    }
}

impl PtySession {
    /// Starts the shell. Output begins arriving on [`Self::events`] right away.
    pub fn spawn(options: SessionOptions) -> anyhow::Result<Self> {
        let shell = options.shell.clone().unwrap_or_else(default_shell);
        let size = options.size.clamped();

        let pair = native_pty_system().openpty(size.pty_size())?;
        let mut cmd = CommandBuilder::new(&shell.program);
        cmd.args(&shell.args);
        let cwd = options.cwd.clone().or_else(home_dir);
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }
        if cfg!(unix) {
            cmd.env("TERM", "xterm-256color");
        }
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "MaomiTerminal");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        for (key, value) in &options.env {
            cmd.env(key, value);
        }
        if options.shell_integration {
            integration::inject(&shell, &mut cmd);
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        // The slave must close here: on Unix the reader only sees EOF once no slave fd is left,
        // and on Windows it holds a reference to the pseudo console.
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let (events_tx, events_rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            events: events_tx,
            wakeup_pending: AtomicBool::new(false),
            win32_input: AtomicBool::new(false),
            state: Mutex::new(ShellState {
                cwd,
                ..ShellState::default()
            }),
            size: Mutex::new(size),
            palette: Mutex::new(QueryPalette::default()),
            input: input_tx,
        });

        let config = term_config(options.scrollback, options.cursor_shape, options.osc52_copy);
        let proxy = EventProxy {
            shared: shared.clone(),
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy)));
        let master = Arc::new(Mutex::new(Some(pair.master)));

        thread::Builder::new()
            .name("maomi-pty-writer".into())
            .spawn(move || {
                for bytes in input_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let (drained_tx, drained_rx) = mpsc::channel::<()>();
        {
            let term = term.clone();
            let shared = shared.clone();
            thread::Builder::new()
                .name("maomi-pty-reader".into())
                .spawn(move || {
                    read_loop(reader, &term, &shared);
                    let _ = drained_tx.send(());
                })?;
        }

        {
            let master = master.clone();
            let shared = shared.clone();
            thread::Builder::new()
                .name("maomi-pty-waiter".into())
                .spawn(move || {
                    let code = child.wait().ok().map(|status| status.exit_code());
                    drop(master.lock().unwrap_or_else(PoisonError::into_inner).take());
                    let _ = drained_rx.recv_timeout(DRAIN_TIMEOUT);
                    shared.state().exited = Some(code);
                    shared.wake();
                    shared.send(TermEvent::Exited(code));
                })?;
        }

        Ok(Self {
            term,
            shared,
            events: events_rx,
            master,
            killer: Mutex::new(killer),
            pid,
            shell,
            osc52_copy: options.osc52_copy,
        })
    }

    /// Changes the scrollback size and default cursor shape of a running session.
    pub fn set_defaults(&self, scrollback: usize, cursor_shape: CursorShape) {
        let config = term_config(scrollback, cursor_shape, self.osc52_copy);
        self.with_term(|term| term.set_options(config));
    }

    /// Event stream for this session. Clones share one queue, so keep a single consumer.
    pub fn events(&self) -> async_channel::Receiver<TermEvent> {
        self.events.clone()
    }

    /// Copies the visible grid and re-arms [`TermEvent::Wakeup`].
    pub fn snapshot(&self) -> GridSnapshot {
        // Cleared before reading so output that lands during the copy wakes the UI again.
        self.shared.wakeup_pending.store(false, Ordering::Release);
        GridSnapshot::from_term(&self.term.lock())
    }

    /// Like [`Self::snapshot`], then prepends `rows_above(&viewport)` history rows, all under
    /// one lock so the rows match the viewport.
    pub fn snapshot_with_rows_above(
        &self,
        rows_above: impl FnOnce(&GridSnapshot) -> usize,
    ) -> GridSnapshot {
        self.shared.wakeup_pending.store(false, Ordering::Release);
        let term = self.term.lock();
        let mut snapshot = GridSnapshot::from_term(&term);
        let rows = rows_above(&snapshot);
        snapshot.add_rows_above(&term, rows);
        snapshot
    }

    /// Current DEC modes (app cursor, bracketed paste, mouse reporting, alt screen).
    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// True while the program asked for win32-input-mode keys (`CSI ? 9001 h`, sent by ConPTY
    /// on Windows). Keys are then sent as `CSI Vk;Sc;Uc;Kd;Cs;Rc _` records.
    pub fn win32_input_mode(&self) -> bool {
        self.shared.win32_input.load(Ordering::Acquire)
    }

    /// Text of the current selection, if any.
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    /// Sends input bytes (keys, paste) to the shell. Never blocks.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.shared.input.send(bytes.into());
    }

    /// Resizes the PTY and the grid. Cheap when the size is unchanged.
    pub fn resize(&self, size: TermSize) {
        let size = size.clamped();
        {
            let mut current = self
                .shared
                .size
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *current == size {
                return;
            }
            *current = size;
        }
        if let Some(master) = self
            .master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && let Err(err) = master.resize(size.pty_size())
        {
            tracing::warn!(%err, "pty resize failed");
        }
        self.term.lock().resize(size);
        self.shared.wake();
    }

    pub fn size(&self) -> TermSize {
        *self
            .shared
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Scrolls the view by `lines` (positive = back into history).
    pub fn scroll(&self, lines: i32) {
        self.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_page_up(&self) {
        self.scroll_display(Scroll::PageUp);
    }

    pub fn scroll_page_down(&self) {
        self.scroll_display(Scroll::PageDown);
    }

    pub fn scroll_to_bottom(&self) {
        self.scroll_display(Scroll::Bottom);
    }

    fn scroll_display(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
        self.shared.wake();
    }

    /// Direct access to the terminal for selection, search and mode checks.
    /// Holds the lock for the duration of `f`, which blocks the reader.
    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<EventProxy>) -> R) -> R {
        let result = f(&mut self.term.lock());
        self.shared.wake();
        result
    }

    /// Colours used to answer colour queries from programs; set from the theme.
    pub fn set_query_palette(&self, palette: QueryPalette) {
        *self
            .shared
            .palette
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = palette;
    }

    /// Ends the shell. `Exited` follows once the output is drained.
    pub fn kill(&self) {
        if let Err(err) = self
            .killer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill()
        {
            tracing::debug!(%err, "kill failed (child probably exited already)");
        }
    }

    pub fn shell(&self) -> &Shell {
        &self.shell
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Title set by the program (OSC 0/2), if any.
    pub fn title(&self) -> Option<String> {
        self.shared.state().title.clone()
    }

    /// Last known working directory: from shell integration, else the spawn folder or the
    /// last [`Self::poll_cwd`] result.
    pub fn cwd(&self) -> Option<PathBuf> {
        self.shared.state().cwd.clone()
    }

    /// Reads the shell process's cwd from the OS when the shell does not report it.
    /// Sends `CwdChanged` when it moved. Does nothing once the shell has reported a cwd.
    ///
    /// PowerShell's `Set-Location` does not change the process cwd, so this only helps for
    /// cmd and Unix shells without integration.
    pub fn poll_cwd(&self) -> Option<PathBuf> {
        if self.shared.state().cwd_reported || self.exit_code().is_some() {
            return self.cwd();
        }
        if let Some(path) = self.pid.and_then(process_cwd) {
            self.shared.set_cwd(path, false);
        }
        self.cwd()
    }

    pub fn prompt_state(&self) -> PromptState {
        self.shared.state().prompt
    }

    /// Some(true) at the prompt, Some(false) while a command runs, None without integration.
    pub fn at_prompt(&self) -> Option<bool> {
        match self.prompt_state() {
            PromptState::Unknown => None,
            PromptState::Prompt => Some(true),
            PromptState::Running => Some(false),
        }
    }

    /// Exit code of the last command, when the shell reports it (OSC 133;D).
    pub fn last_command_exit(&self) -> Option<i32> {
        self.shared.state().last_command_exit
    }

    /// `Some` once the shell has exited; the inner value is its exit code if readable.
    pub fn exit_code(&self) -> Option<Option<u32>> {
        self.shared.state().exited
    }

    pub fn is_running(&self) -> bool {
        self.exit_code().is_none()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.is_running() {
            self.kill();
        }
        drop(
            self.master
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    term: &FairMutex<Term<EventProxy>>,
    shared: &Shared,
) {
    let mut buf = vec![0u8; READ_BUFFER];
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    let mut scanner = OscScanner::default();
    let mut marks = Vec::new();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO on Linux once the child side closes; a broken pipe on Windows.
            Err(_) => break,
        };
        let bytes = &buf[..n];
        scanner.feed(bytes, &mut marks);
        parser.advance(&mut *term.lock(), bytes);
        for mark in marks.drain(..) {
            shared.apply_mark(mark);
        }
        shared.wake();
    }
    // Flush a synchronized update (DEC 2026) the program never ended.
    if parser.sync_timeout().sync_timeout().is_some() {
        parser.stop_sync(&mut *term.lock());
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

fn process_cwd(pid: u32) -> Option<PathBuf> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    system
        .process(pid)
        .and_then(|p| p.cwd())
        .map(Path::to_path_buf)
}
