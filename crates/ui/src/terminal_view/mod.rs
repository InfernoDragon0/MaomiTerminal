//! A terminal pane: owns a [`PtySession`], redraws on output and blinks the cursor.

mod builtin;
mod element;
mod input;
mod layout;
mod paths;
mod smooth;
mod win32_input;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, px,
};
use maomi_term::{PromptState, PtySession, QueryPalette, Rgb, SessionOptions, TermEvent, TermMode};

pub(crate) use element::OUTLINE_OFFSETS;
use element::PaneState;
pub use element::{TerminalElement, TerminalFont};

use crate::Theme;
use crate::color::Color;
use crate::terminal::TerminalColors;

const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Key context of a terminal pane; bindings here beat the window root's Tab/Ctrl+C bindings.
pub const KEY_CONTEXT: &str = "Terminal";

gpui_kit::actions!(terminal, [SendTab, SendBackTab, CopyOrInterrupt]);

/// Keys the window root binds for focus moves and copying; inside a pane they go to the shell.
pub(crate) fn key_bindings() -> Vec<gpui_kit::KeyBinding> {
    use gpui_kit::KeyBinding;
    let mut bindings = vec![
        KeyBinding::new("tab", SendTab, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-tab", SendBackTab, Some(KEY_CONTEXT)),
    ];
    if !cfg!(target_os = "macos") {
        bindings.push(KeyBinding::new(
            "ctrl-c",
            CopyOrInterrupt,
            Some(KEY_CONTEXT),
        ));
    }
    bindings
}

/// What a pane reports to its container (tab titles, the file drawer, closing on exit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalViewEvent {
    /// None = the program reset its title.
    TitleChanged(Option<String>),
    CwdChanged(PathBuf),
    PromptChanged(PromptState),
    Bell,
    /// The program sent a desktop notification (OSC 9 / OSC 777).
    Notification(String),
    /// The user typed into the pane.
    Typed,
    Exited(Option<u32>),
    /// A fresh shell replaced the exited one (Enter on the exit banner).
    Restarted,
    /// A folder path in the output was Ctrl+clicked.
    OpenFolder(PathBuf),
    /// A remote pane's connection opened; [`TerminalView::ssh_handle`] has it.
    Connected,
}

/// A remote pane's connection state, shown in the bottom banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Connecting(SharedString),
    Failed(SharedString),
}

/// Starts a new connection for this pane (Enter on the exit or failure banner).
pub type Reconnect = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct TerminalView {
    session: Arc<PtySession>,
    /// How the session was started; reused by [`Self::restart`].
    options: SessionOptions,
    /// `Some(code)` once the shell has exited.
    exited: Option<Option<u32>>,
    connection: Option<Connection>,
    /// Set for remote panes: Enter reconnects instead of starting a local shell.
    reconnect: Option<Reconnect>,
    /// The open SSH connection of a remote pane, for SFTP.
    ssh: Option<maomi_ssh::SshHandle>,
    focus_handle: FocusHandle,
    font: TerminalFont,
    cursor_on: bool,
    last_activity: Instant,
    /// Bumped on blur so the running blink loop stops.
    blink_epoch: u64,
    /// Ease streaming output in.
    smooth_scroll: bool,
    cursor_blink: bool,
    pane: Rc<RefCell<PaneState>>,
    /// Theme whose colours the session last answered colour queries with.
    query_theme: Option<Theme>,
    /// Alpha of the backdrop behind the text; 0 when the parent paints it.
    backdrop: f32,
    /// Overlay text: white default text with a black outline (faded detached windows).
    text_outline: bool,
    _events: Task<()>,
    _focus: [Subscription; 2],
}

impl TerminalView {
    /// Starts a shell with `options` in a new pane.
    pub fn new(
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Self> {
        let session = PtySession::spawn(options.clone())?;
        Ok(Self::with_session(session, options, window, cx))
    }

    /// A pane for an already started `session`; `options` are used to restart it.
    pub fn with_session(
        session: PtySession,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = Arc::new(session);
        let events_task = Self::listen(&session, cx);
        let focus_handle = cx.focus_handle();
        let focus = Self::watch_focus(&focus_handle, window, cx);
        Self {
            session,
            options,
            exited: None,
            connection: None,
            reconnect: None,
            ssh: None,
            focus_handle,
            font: TerminalFont::default(),
            cursor_on: true,
            last_activity: Instant::now(),
            blink_epoch: 0,
            smooth_scroll: true,
            cursor_blink: true,
            pane: Rc::default(),
            query_theme: None,
            backdrop: 1.,
            text_outline: false,
            _events: events_task,
            _focus: focus,
        }
    }

    /// Focus and blur listeners belong to one window.
    fn watch_focus(
        focus_handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> [Subscription; 2] {
        [
            cx.on_focus_in(focus_handle, window, |this, _, cx| this.start_blink(cx)),
            cx.on_blur(focus_handle, window, |this, _, cx| {
                this.blink_epoch += 1;
                this.cursor_on = true;
                cx.notify();
            }),
        ]
    }

    /// The pane moved to `window` (detached into its own window): listen for focus there.
    pub fn rehome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._focus = Self::watch_focus(&self.focus_handle, window, cx);
    }

    pub fn set_backdrop_opacity(&mut self, opacity: f32, cx: &mut Context<Self>) {
        self.backdrop = opacity;
        cx.notify();
    }

    pub fn set_text_outline(&mut self, outline: bool, cx: &mut Context<Self>) {
        self.text_outline = outline;
        cx.notify();
    }

    fn listen(session: &PtySession, cx: &mut Context<Self>) -> Task<()> {
        let events = session.events();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.on_term_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    /// Starts a fresh shell in this pane with the original options.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        match PtySession::spawn(self.options.clone()) {
            Ok(session) => self.replace_session(session, cx),
            Err(err) => tracing::error!(%err, "failed to restart the shell"),
        }
    }

    /// Shows `session` in this pane instead of the current one (a connection that opened, or
    /// a restart). Clears the connection banner.
    pub fn replace_session(&mut self, session: PtySession, cx: &mut Context<Self>) {
        let session = Arc::new(session);
        session.resize(self.session.size());
        self._events = Self::listen(&session, cx);
        self.session = session;
        self.exited = None;
        self.connection = None;
        self.ssh = None;
        self.query_theme = None;
        self.pane = Rc::default();
        cx.emit(TerminalViewEvent::Restarted);
        cx.notify();
    }

    pub fn set_connection(&mut self, connection: Option<Connection>, cx: &mut Context<Self>) {
        self.connection = connection;
        cx.notify();
    }

    /// Records the connection behind a remote session just shown with
    /// [`Self::replace_session`].
    pub fn set_ssh(&mut self, handle: maomi_ssh::SshHandle, cx: &mut Context<Self>) {
        self.ssh = Some(handle);
        cx.emit(TerminalViewEvent::Connected);
    }

    pub fn ssh_handle(&self) -> Option<&maomi_ssh::SshHandle> {
        self.ssh.as_ref()
    }

    pub fn set_reconnect(&mut self, reconnect: Reconnect) {
        self.reconnect = Some(reconnect);
    }

    /// Enter on a banner: reconnect a remote pane, restart a local one.
    fn restart_or_reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.reconnect.clone() {
            // Deferred: the reconnect updates this view, which is mid-update here.
            Some(reconnect) => window.defer(cx, move |window, cx| reconnect(window, cx)),
            None => self.restart(cx),
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exited.is_some()
    }

    pub fn session(&self) -> &Arc<PtySession> {
        &self.session
    }

    pub fn set_font(&mut self, font: TerminalFont, cx: &mut Context<Self>) {
        self.font = font;
        cx.notify();
    }

    pub fn set_smooth_scroll(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.smooth_scroll = enabled;
        cx.notify();
    }

    /// Turning blinking on takes effect the next time the pane gets focus.
    pub fn set_cursor_blink(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.cursor_blink != enabled && !enabled {
            // Stops the running blink loop.
            self.blink_epoch += 1;
            self.cursor_on = true;
        }
        self.cursor_blink = enabled;
        cx.notify();
    }

    /// Copies the selection to the clipboard; returns false when nothing is selected.
    fn copy(&self, cx: &mut Context<Self>) -> bool {
        match self.session.selection_text() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    fn paste(&self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.insert_text(&text);
    }

    /// Types `text` at the cursor as a paste (bracketed when the program asked for it).
    pub fn insert_text(&self, text: &str) {
        let bracketed = self.session.mode().contains(TermMode::BRACKETED_PASTE);
        self.send_input(input::encode_paste(text, bracketed));
    }

    /// Types `command` as one paste and presses Enter, so multi-line commands run as a block.
    pub fn run_command(&self, command: &str) {
        self.insert_text(command.trim_end());
        self.send_input(b"\r".to_vec());
    }

    /// Bytes for `keystroke`: win32-input-mode records when ConPTY asked for them, else VT.
    fn encode_key(&self, keystroke: &Keystroke, prefer_char: bool) -> Option<Vec<u8>> {
        // The inbox ConPTY accepts win32 key records without announcing ?9001h, but a tight
        // burst of records can get garbled, so records are only used where VT loses
        // information (Shift+Enter and friends) or when ConPTY asked for them.
        if cfg!(windows)
            && (self.session.win32_input_mode() || needs_win32_record(keystroke))
            && let Some(bytes) = win32_input::encode_win32_key(keystroke)
        {
            return Some(bytes);
        }
        input::encode_key(keystroke, prefer_char, self.session.mode())
    }

    /// A key bound through an action (Tab, Shift+Tab, Ctrl+C) rather than `on_key_down`.
    fn send_key(&mut self, keystroke: &str, cx: &mut Context<Self>) {
        if self.exited.is_none()
            && let Ok(keystroke) = Keystroke::parse(keystroke)
            && let Some(bytes) = self.encode_key(&keystroke, false)
        {
            self.last_activity = Instant::now();
            self.send_input(bytes);
        }
        cx.stop_propagation();
    }

    /// Typed or pasted input: jumps back to the live screen and drops the selection first.
    fn send_input(&self, bytes: Vec<u8>) {
        self.session.scroll_to_bottom();
        self.session.with_term(|term| term.selection = None);
        self.session.write(bytes);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if matches!(self.connection, Some(Connection::Connecting(_))) {
            cx.stop_propagation();
            return;
        }
        if self.exited.is_some() || self.connection.is_some() {
            if keystroke.key == "enter" && !keystroke.modifiers.modified() {
                self.restart_or_reconnect(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        let m = &keystroke.modifiers;
        let key = keystroke.key.as_str();
        let primary = if cfg!(target_os = "macos") {
            m.platform
        } else {
            m.control && m.shift
        };
        let mode = self.session.mode();

        // Copy: Ctrl+Shift+C (Cmd+C), or Ctrl+C while something is selected (Windows Terminal style).
        let plain_ctrl = m.control && !m.shift && !m.alt;
        if key == "c" && (primary || plain_ctrl) {
            let copied = self.copy(cx);
            if primary || copied {
                self.session.with_term(|term| term.selection = None);
                cx.stop_propagation();
                return;
            }
        }
        // Paste: Ctrl+Shift+V (Cmd+V), Shift+Insert, and Ctrl+V on Windows.
        let windows_paste = cfg!(windows) && m.control && !m.shift && !m.alt && key == "v";
        if (primary && key == "v") || (m.shift && !m.control && key == "insert") || windows_paste {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        // Shift+PageUp/PageDown scroll history while the shell owns the screen.
        if m.shift && !m.control && !mode.contains(TermMode::ALT_SCREEN) {
            match key {
                "pageup" => self.session.scroll_page_up(),
                "pagedown" => self.session.scroll_page_down(),
                _ => {}
            }
            if matches!(key, "pageup" | "pagedown") {
                cx.stop_propagation();
                return;
            }
        }
        if let Some(bytes) = self.encode_key(keystroke, event.prefer_character_input) {
            self.last_activity = Instant::now();
            self.send_input(bytes);
            cx.emit(TerminalViewEvent::Typed);
            cx.stop_propagation();
        }
    }

    fn on_term_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup => {
                // Keep the cursor solid while output or typing is happening.
                self.last_activity = Instant::now();
                self.cursor_on = true;
                cx.notify();
            }
            TermEvent::Title(title) => cx.emit(TerminalViewEvent::TitleChanged(Some(title))),
            TermEvent::ResetTitle => cx.emit(TerminalViewEvent::TitleChanged(None)),
            TermEvent::Bell => cx.emit(TerminalViewEvent::Bell),
            TermEvent::Notification(text) => cx.emit(TerminalViewEvent::Notification(text)),
            TermEvent::ClipboardCopy(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            TermEvent::CwdChanged(path) => cx.emit(TerminalViewEvent::CwdChanged(path)),
            TermEvent::PromptChanged(state) => cx.emit(TerminalViewEvent::PromptChanged(state)),
            TermEvent::Exited(code) => {
                self.exited = Some(code);
                cx.emit(TerminalViewEvent::Exited(code));
                cx.notify();
            }
        }
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_epoch += 1;
        self.cursor_on = true;
        cx.notify();
        if !self.cursor_blink {
            return;
        }
        let epoch = self.blink_epoch;
        let timer = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                timer.timer(BLINK_INTERVAL).await;
                let keep = this.update(cx, |this, cx| {
                    if this.blink_epoch != epoch {
                        return false;
                    }
                    let next = this.last_activity.elapsed() < BLINK_INTERVAL || !this.cursor_on;
                    if next != this.cursor_on {
                        this.cursor_on = next;
                        cx.notify();
                    }
                    true
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let colors = theme.terminal_colors();
        if self.query_theme != Some(theme) {
            self.query_theme = Some(theme);
            self.session.set_query_palette(query_palette(&colors));
        }
        let focused = self.focus_handle.is_focused(window);
        let p = theme.palette();
        let banner = match (&self.connection, self.exited) {
            (Some(Connection::Connecting(text)), _) => Some((text.clone(), p.muted)),
            (Some(Connection::Failed(text)), _) => Some((
                format!("{text} \u{00b7} Press Enter to reconnect").into(),
                p.err,
            )),
            (None, Some(code)) => {
                let code = code.map_or_else(|| "unknown".to_owned(), |c| c.to_string());
                let text = if self.reconnect.is_some() {
                    format!("Connection closed (code {code}) \u{00b7} Press Enter to reconnect")
                } else {
                    format!("Shell exited (code {code}) \u{00b7} Press Enter to restart")
                };
                Some((text.into(), p.muted))
            }
            (None, None) => None,
        };
        let banner = banner.map(|(text, color): (SharedString, crate::Color)| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .px(px(12.))
                .py(px(6.))
                .bg(p.surface_2.to_hsla())
                .text_color(color.to_hsla())
                .text_size(px(12.))
                .child(text)
        });
        div()
            .id("terminal")
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SendTab, _, cx| this.send_key("tab", cx)))
            .on_action(cx.listener(|this, _: &SendBackTab, _, cx| this.send_key("shift-tab", cx)))
            .on_action(cx.listener(|this, _: &CopyOrInterrupt, _, cx| {
                if this.copy(cx) {
                    this.session.with_term(|term| term.selection = None);
                } else {
                    this.send_key("ctrl-c", cx);
                }
            }))
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
            )
            .child(
                TerminalElement::new(
                    self.session.clone(),
                    colors,
                    self.font.clone(),
                    focused,
                    self.cursor_on,
                    self.smooth_scroll,
                    self.pane.clone(),
                    cx.entity().downgrade(),
                )
                .backdrop_opacity(self.backdrop)
                .text_outline(self.text_outline),
            )
            .children(banner)
    }
}

/// Keys whose modifiers plain VT cannot express (Shift+Enter arrives as Enter).
fn needs_win32_record(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    (m.shift || m.control || m.alt)
        && matches!(
            keystroke.key.as_str(),
            "enter" | "space" | "backspace" | "escape" | "tab"
        )
}

fn query_palette(colors: &TerminalColors) -> QueryPalette {
    let rgb = |c: Color| {
        let (r, g, b) = c.rgb();
        Rgb { r, g, b }
    };
    QueryPalette {
        foreground: rgb(colors.fg),
        background: rgb(colors.bg),
        cursor: rgb(colors.cursor),
        ansi: colors.ansi.map(rgb),
    }
}
