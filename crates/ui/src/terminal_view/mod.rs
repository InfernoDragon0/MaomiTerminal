//! A terminal pane: owns a [`PtySession`], redraws on output and blinks the cursor.

mod builtin;
mod element;
mod input;
mod layout;
mod smooth;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render, Styled as _, Subscription,
    Task, Window, div, px,
};
use maomi_term::{PromptState, PtySession, QueryPalette, Rgb, SessionOptions, TermEvent, TermMode};

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
    Exited(Option<u32>),
}

pub struct TerminalView {
    session: Arc<PtySession>,
    /// How the session was started; reused by [`Self::restart`].
    options: SessionOptions,
    /// `Some(code)` once the shell has exited.
    exited: Option<Option<u32>>,
    focus_handle: FocusHandle,
    font: TerminalFont,
    cursor_on: bool,
    last_activity: Instant,
    /// Bumped on blur so the running blink loop stops.
    blink_epoch: u64,
    /// Ease streaming output in (on by default; the settings card exposes it).
    smooth_scroll: bool,
    pane: Rc<RefCell<PaneState>>,
    /// Theme whose colours the session last answered colour queries with.
    query_theme: Option<Theme>,
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
        let focus = [
            cx.on_focus_in(&focus_handle, window, |this, _, cx| this.start_blink(cx)),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.blink_epoch += 1;
                this.cursor_on = true;
                cx.notify();
            }),
        ];
        Self {
            session,
            options,
            exited: None,
            focus_handle,
            font: TerminalFont::default(),
            cursor_on: true,
            last_activity: Instant::now(),
            blink_epoch: 0,
            smooth_scroll: true,
            pane: Rc::default(),
            query_theme: None,
            _events: events_task,
            _focus: focus,
        }
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
            Ok(session) => {
                let session = Arc::new(session);
                self._events = Self::listen(&session, cx);
                self.session = session;
                self.exited = None;
                self.query_theme = None;
                self.pane = Rc::default();
                cx.notify();
            }
            Err(err) => tracing::error!(%err, "failed to restart the shell"),
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
        let bracketed = self.session.mode().contains(TermMode::BRACKETED_PASTE);
        self.send_input(input::encode_paste(&text, bracketed));
    }

    /// A key bound through an action (Tab, Shift+Tab, Ctrl+C) rather than `on_key_down`.
    fn send_key(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.exited.is_none() {
            self.last_activity = Instant::now();
            self.send_input(bytes.to_vec());
        }
        cx.stop_propagation();
    }

    /// Typed or pasted input: jumps back to the live screen and drops the selection first.
    fn send_input(&self, bytes: Vec<u8>) {
        self.session.scroll_to_bottom();
        self.session.with_term(|term| term.selection = None);
        self.session.write(bytes);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if self.exited.is_some() {
            if keystroke.key == "enter" && !keystroke.modifiers.modified() {
                self.restart(cx);
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
        if let Some(bytes) = input::encode_key(keystroke, event.prefer_character_input, mode) {
            self.last_activity = Instant::now();
            self.send_input(bytes);
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
        let banner = self.exited.map(|code| {
            let p = theme.palette();
            let code = code.map_or_else(|| "unknown".to_owned(), |c| c.to_string());
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .px(px(12.))
                .py(px(6.))
                .bg(p.surface_2.to_hsla())
                .text_color(p.muted.to_hsla())
                .text_size(px(12.))
                .child(format!(
                    "Shell exited (code {code}) \u{00b7} Press Enter to restart"
                ))
        });
        div()
            .id("terminal")
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key_down(event, cx)),
            )
            .on_action(cx.listener(|this, _: &SendTab, _, cx| this.send_key(b"\t", cx)))
            .on_action(cx.listener(|this, _: &SendBackTab, _, cx| this.send_key(b"\x1b[Z", cx)))
            .on_action(cx.listener(|this, _: &CopyOrInterrupt, _, cx| {
                if this.copy(cx) {
                    this.session.with_term(|term| term.selection = None);
                } else {
                    this.send_key(&[3], cx);
                }
            }))
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
            )
            .child(TerminalElement::new(
                self.session.clone(),
                colors,
                self.font.clone(),
                focused,
                self.cursor_on,
                self.smooth_scroll,
                self.pane.clone(),
            ))
            .children(banner)
    }
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
