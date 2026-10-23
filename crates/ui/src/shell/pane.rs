//! One terminal pane: a 28 px header (drawer pill, shell name, running dot, split and close
//! buttons) over an optional file drawer and a [`TerminalView`], wrapped in the focus ring.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnimationExt as _, AnyElement, AnyWindowHandle, App, AppContext as _, BoxShadow, Context,
    Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, MouseButton, MouseMoveEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    linear_color_stop, linear_gradient, point, px, relative,
};
use maomi_config::DrawerLayout;
use maomi_term::{PromptState, PtySession, SessionOptions, Shell, ShellKind};

use super::actions::ToggleDrawer;
use super::agent::{self, AgentState, AgentStatus, StatusRead};
use super::drawer::{self, DragPreview, DraggedPath, DrawerEvent, FileDrawer};
use super::standalone::outlined;
use super::tree::DropZone;
use super::workbench::DraggedCard;
use crate::{Palette, TerminalView, TerminalViewEvent, Theme, settings};

/// Drag payload of a pane picked up by its header.
#[derive(Clone)]
pub struct DraggedPane {
    pane: Entity<Pane>,
}

/// What a pane tells the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Title, cwd or running state changed; tab titles and the dock refresh.
    Changed,
    SplitRight,
    SplitDown,
    /// Another pane was dropped on this one.
    PaneDropped(Entity<Pane>, DropZone),
    Close,
    /// Move this pane into a window of its own.
    Detach,
    /// A detached pane wants back into its tab.
    Reattach,
    /// A detached pane's window closed it (any confirmation is done); the pane goes away.
    Closed,
    /// The shell exited with code 0; the pane goes away. Other codes keep the pane open
    /// with the restart banner so the error stays readable.
    ExitedCleanly,
    /// A `maomi:` link in the output was Ctrl+clicked.
    OpenDeepLink(String),
}

pub struct Pane {
    terminal: Entity<TerminalView>,
    /// Short shell name for headers and titles (`pwsh`, `cmd`, `bash`).
    shell_name: String,
    /// Title set by the program (OSC 0/2), when it is a real title.
    title: Option<String>,
    cwd: Option<PathBuf>,
    prompt: PromptState,
    /// Program the running command started, shown beside the running dot.
    command: Option<String>,
    /// Bumped per command so a late lookup for an earlier one is dropped.
    command_epoch: u64,
    exited: bool,
    shell_kind: ShellKind,
    /// The shell picked from the new-tab menu; None = the configured default.
    picked_shell: Option<Shell>,
    /// An SSH pane: its cwd is a remote path and its drawer lists the server over SFTP.
    remote: bool,
    /// Created the first time the drawer opens.
    drawer: Option<(Entity<FileDrawer>, Subscription)>,
    drawer_open: bool,
    drawer_width: f32,
    /// Shell-follow for a drawer that is not created yet.
    drawer_follow: bool,
    /// Mouse x and drawer width when a drag on the drawer's edge began.
    resizing: Option<(f32, f32)>,
    /// What an agent (Claude Code) in this pane last reported through its status file.
    agent: Option<AgentStatus>,
    /// The status file's modified time when it was last read.
    agent_seen: Option<SystemTime>,
    /// Unix ms when the shell's prompt last came back: older statuses are stale.
    prompt_back_at: u64,
    /// A bell or notification arrived while a command ran (no status file); cleared by typing.
    needs_you: bool,
    /// The window this pane was detached into; its tab shows a placeholder meanwhile.
    detached: Option<AnyWindowHandle>,
    /// Alpha of the pane's backgrounds (below 1 in a faded standalone window).
    backdrop: f32,
    /// Overlay text: the header's name and status get the terminal's black outline too.
    text_outline: bool,
    _agent_poll: Option<Task<()>>,
    _terminal_events: Subscription,
}

/// How often a pane looks at its agent status file.
const AGENT_POLL: Duration = Duration::from_millis(400);

impl Pane {
    /// A pane for a started `session`; `options` are kept for restarting it.
    pub fn new(
        session: PtySession,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let remote = session.is_remote();
        // A remote label ("ssh fennec.lan") is not a path, so file_stem would cut it.
        let shell_name = if remote {
            session.shell().program.to_string_lossy().into_owned()
        } else {
            session
                .shell()
                .program
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shell".into())
        };
        let cwd = session.cwd();
        let shell_kind = session.shell().kind;
        let agent_poll = agent::pane_id(&options.env)
            .and_then(|id| agent::status_file(&id))
            .map(|path| Self::poll_agent(path, cx));
        let terminal = cx.new(|cx| {
            let mut view = TerminalView::with_session(session, options, window, cx);
            // The pane's rounded body paints the backdrop (same colour). The terminal's own
            // square one would cover the focus ring's bottom corners.
            view.set_backdrop_opacity(0., cx);
            view
        });
        let subscription = Self::watch_terminal(&terminal, window, cx);
        Self {
            terminal,
            shell_name,
            title: None,
            cwd,
            prompt: PromptState::Unknown,
            command: None,
            command_epoch: 0,
            exited: false,
            shell_kind,
            picked_shell: None,
            remote,
            drawer: None,
            drawer_open: false,
            drawer_width: drawer::DEFAULT_WIDTH,
            drawer_follow: settings::config(cx).shell.drawer_follow,
            resizing: None,
            agent: None,
            agent_seen: None,
            prompt_back_at: 0,
            needs_you: false,
            detached: None,
            backdrop: 1.,
            text_outline: false,
            _agent_poll: agent_poll,
            _terminal_events: subscription,
        }
    }

    /// Reads `path` whenever its modified time changes. A file that does not parse yet (the
    /// mod writes it in place) is read again on the next poll.
    fn poll_agent(path: PathBuf, cx: &mut Context<Self>) -> Task<()> {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(AGENT_POLL).await;
                let Ok((seen, pid)) = this.read_with(cx, |pane, _| {
                    (pane.agent_seen, pane.agent.as_ref().and_then(|a| a.pid))
                }) else {
                    return;
                };
                let path = path.clone();
                let (modified, read, alive) = executor
                    .spawn(async move {
                        let (modified, read) = agent::read(&path, seen);
                        (
                            modified,
                            read,
                            pid.is_none_or(maomi_term::process::is_alive),
                        )
                    })
                    .await;
                let updated =
                    this.update(cx, |pane, cx| pane.agent_read(modified, read, alive, cx));
                if updated.is_err() {
                    return;
                }
            }
        })
    }

    /// Applies one look at the status file; `alive` = the agent's process still runs.
    fn agent_read(
        &mut self,
        modified: Option<SystemTime>,
        read: StatusRead,
        alive: bool,
        cx: &mut Context<Self>,
    ) {
        let status = match read {
            StatusRead::Unchanged => self.agent.clone(),
            // Not seen as read, so the next poll tries again.
            StatusRead::Partial => return,
            StatusRead::Missing | StatusRead::Ended => {
                self.agent_seen = modified;
                None
            }
            StatusRead::Status(status) => {
                self.agent_seen = modified;
                Some(status)
            }
        };
        let status = status.filter(|s| alive && s.at >= self.prompt_back_at);
        if status != self.agent {
            self.agent = status;
            if self.agent.is_some() {
                self.needs_you = false;
            }
            cx.emit(PaneEvent::Changed);
            cx.notify();
        }
    }

    /// The agent's state for the session row and header: its status file, else "needs you"
    /// after a bell or notification.
    pub fn agent_status(&self) -> Option<AgentStatus> {
        self.agent.clone().or_else(|| {
            self.needs_you.then_some(AgentStatus {
                state: AgentState::NeedsYou,
                tool: None,
                pid: None,
                at: 0,
            })
        })
    }

    fn watch_terminal(
        terminal: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            terminal,
            window,
            |pane, _, event: &TerminalViewEvent, window, cx| match event {
                TerminalViewEvent::OpenFolder(path) => {
                    pane.show_in_drawer(path.clone(), window, cx)
                }
                event => pane.on_terminal_event(event, cx),
            },
        )
    }

    /// The pane moved to `window` (detached into its own window): its window-bound listeners
    /// follow it, and the drawer is rebuilt because its inputs belong to the old window.
    pub fn rehome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |view, cx| view.rehome(window, cx));
        self._terminal_events = Self::watch_terminal(&self.terminal, window, cx);
        if let Some(layout) = self.drawer_layout(cx) {
            self.drawer = None;
            self.restore_drawer(layout, window, cx);
        }
    }

    /// Marks the pane as shown in `window` (detached), or back in its tab (None). A detached
    /// pane has no split or detach buttons.
    pub fn set_detached(&mut self, window: Option<AnyWindowHandle>, cx: &mut Context<Self>) {
        self.detached = window;
        cx.emit(PaneEvent::Changed);
        cx.notify();
    }

    pub fn detached_window(&self) -> Option<AnyWindowHandle> {
        self.detached
    }

    pub fn is_detached(&self) -> bool {
        self.detached.is_some()
    }

    /// Asks the pane's tab to take it back (the detached window's Attach back button).
    pub fn request_reattach(&mut self, cx: &mut Context<Self>) {
        cx.emit(PaneEvent::Reattach);
    }

    /// The detached window closed this pane; its tab drops the placeholder.
    pub fn close_detached(&mut self, cx: &mut Context<Self>) {
        cx.emit(PaneEvent::Closed);
    }

    pub fn set_backdrop_opacity(&mut self, opacity: f32, cx: &mut Context<Self>) {
        self.backdrop = opacity;
        cx.notify();
    }

    /// Overlay text (white with a black outline) for a faded detached window.
    pub fn set_text_outline(&mut self, outline: bool, cx: &mut Context<Self>) {
        self.text_outline = outline;
        self.terminal
            .update(cx, |view, cx| view.set_text_outline(outline, cx));
        cx.notify();
    }

    /// The drawer's state for the saved layout; None while it was never opened.
    pub fn drawer_layout(&self, cx: &App) -> Option<DrawerLayout> {
        let (drawer, _) = self.drawer.as_ref()?;
        Some(DrawerLayout {
            open: self.drawer_open,
            width: self.drawer_width,
            follow: drawer.read(cx).follow(),
        })
    }

    pub fn restore_drawer(
        &mut self,
        layout: DrawerLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drawer_width = layout.width.clamp(drawer::MIN_WIDTH, drawer::MAX_WIDTH);
        self.drawer_follow = layout.follow;
        self.ensure_drawer(window, cx);
        self.drawer_open = layout.open;
        cx.notify();
    }

    pub fn toggle_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_drawer(window, cx);
        self.drawer_open = !self.drawer_open;
        cx.notify();
    }

    /// Opens the drawer on `dir`, e.g. a folder Ctrl+clicked in the output. The drawer goes
    /// back to the shell's cwd the next time that changes.
    fn show_in_drawer(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_drawer(window, cx);
        self.drawer_open = true;
        if let Some((drawer, _)) = &self.drawer {
            drawer.update(cx, |d, cx| d.set_dir(dir, cx));
        }
        cx.notify();
    }

    fn ensure_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drawer.is_some() {
            return;
        }
        let dir = self
            .cwd
            .clone()
            .or_else(std::env::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let follow = self.drawer_follow;
        let drawer = if self.remote {
            let handle = self.terminal.read(cx).ssh_handle().cloned();
            let drawer = cx.new(|cx| FileDrawer::new_remote(follow, window, cx));
            if let Some(handle) = handle {
                drawer.update(cx, |d, cx| d.set_remote(handle, cx));
            }
            drawer
        } else {
            cx.new(|cx| FileDrawer::new(dir, follow, window, cx))
        };
        drawer.update(cx, |d, cx| {
            d.set_follow_blocked(self.prompt == PromptState::Running, cx)
        });
        let events = cx.subscribe_in(
            &drawer,
            window,
            |pane, _, event: &DrawerEvent, window, cx| pane.on_drawer_event(event, window, cx),
        );
        self.drawer = Some((drawer, events));
    }

    fn on_drawer_event(
        &mut self,
        event: &DrawerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DrawerEvent::Insert(path) => self.insert_path(path, window, cx),
            DrawerEvent::Navigated(dir) => {
                let follow = self
                    .drawer
                    .as_ref()
                    .is_some_and(|(d, _)| d.read(cx).follow());
                // Only at a prompt: typing `cd` into a running program would feed it input.
                if follow && !self.exited && !self.is_busy(cx) {
                    let line = format!("{}\r", cd_command(self.shell_kind, dir));
                    self.terminal.read(cx).session().write(line);
                }
            }
        }
    }

    /// Types `path` (quoted for the shell) and a space at the cursor, then focuses the terminal.
    fn insert_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("{} ", quote_path(self.shell_kind, path));
        self.terminal.read(cx).insert_text(&text);
        self.terminal.focus_handle(cx).focus(window, cx);
    }

    pub fn terminal(&self) -> &Entity<TerminalView> {
        &self.terminal
    }

    pub fn shell_name(&self) -> &str {
        &self.shell_name
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// Splits and the saved layout reopen this shell.
    pub fn picked_shell(&self) -> Option<&Shell> {
        self.picked_shell.as_ref()
    }

    pub fn set_picked_shell(&mut self, shell: Option<Shell>) {
        self.picked_shell = shell;
    }

    /// A command is running (OSC 133); the header shows the running dot.
    pub fn is_running(&self) -> bool {
        self.prompt == PromptState::Running && !self.exited
    }

    /// Program the running command started, once it has been looked up.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref().filter(|_| self.is_running())
    }

    /// The pane's name in its header and the Sessions dock: the program's title (Claude Code
    /// sets one per conversation), else the shell's name.
    pub fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.shell_name)
    }

    /// The running program set a title (OSC 0/2).
    pub fn has_program_title(&self) -> bool {
        self.title.is_some()
    }

    /// Tab title: the program's title, else `shell cwd`.
    pub fn title(&self) -> String {
        match &self.title {
            Some(title) => title.clone(),
            None => match &self.cwd {
                Some(cwd) => format!("{} {}", self.shell_name, display_path(cwd)),
                None => self.shell_name.clone(),
            },
        }
    }

    /// Last path component of the cwd, for the header pill.
    pub fn cwd_name(&self) -> String {
        match &self.cwd {
            Some(cwd) if is_home(cwd) => "~".into(),
            Some(cwd) => cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.display().to_string()),
            None => "~".into(),
        }
    }

    /// Whether closing would kill something the user may care about: a running command
    /// (OSC 133), or child processes of a shell without integration.
    pub fn is_busy(&self, cx: &App) -> bool {
        if self.exited {
            return false;
        }
        match self.prompt {
            PromptState::Running => true,
            PromptState::Prompt => false,
            PromptState::Unknown => {
                let session = self.terminal.read(cx).session().clone();
                session.pid().is_some_and(super::process::has_children)
            }
        }
    }

    fn on_terminal_event(&mut self, event: &TerminalViewEvent, cx: &mut Context<Self>) {
        match event {
            TerminalViewEvent::TitleChanged(title) => {
                // ConPTY reports the shell's exe path as the title at start; that is not a title.
                self.title = title
                    .clone()
                    .filter(|t| !t.trim().is_empty() && !t.to_ascii_lowercase().ends_with(".exe"));
            }
            // A remote cwd only moves the remote drawer; splits open local shells.
            TerminalViewEvent::CwdChanged(path) if self.remote => {
                if let Some((drawer, _)) = &self.drawer {
                    drawer.update(cx, |d, cx| d.set_dir(path.clone(), cx));
                }
                return;
            }
            TerminalViewEvent::Connected => {
                let handle = self.terminal.read(cx).ssh_handle().cloned();
                if let (Some((drawer, _)), Some(handle)) = (&self.drawer, handle) {
                    drawer.update(cx, |d, cx| d.set_remote(handle, cx));
                }
                return;
            }
            TerminalViewEvent::CwdChanged(path) => {
                self.cwd = Some(path.clone());
                if let Some((drawer, _)) = &self.drawer {
                    drawer.update(cx, |d, cx| d.set_dir(path.clone(), cx));
                }
            }
            TerminalViewEvent::PromptChanged(state) => {
                self.prompt = *state;
                self.command_epoch += 1;
                self.command = None;
                if *state == PromptState::Prompt {
                    // The agent's program ended with the command; what it wrote is stale now.
                    self.prompt_back_at = agent::now_ms();
                    self.agent = None;
                    self.needs_you = false;
                }
                if *state == PromptState::Running {
                    self.look_up_command(cx);
                }
                if let Some((drawer, _)) = &self.drawer {
                    let running = *state == PromptState::Running;
                    drawer.update(cx, |d, cx| d.set_follow_blocked(running, cx));
                }
            }
            // Without a status file, a bell or notification while a command runs means the
            // program (an agent asking for permission, a finished build) wants the user.
            TerminalViewEvent::Bell | TerminalViewEvent::Notification(_) => {
                // Only while a command is known to run: shells without prompt marks also ring
                // for Backspace at an empty prompt.
                if self.agent.is_some() || self.needs_you || self.prompt != PromptState::Running {
                    return;
                }
                self.needs_you = true;
            }
            TerminalViewEvent::Typed => {
                if !self.needs_you {
                    return;
                }
                self.needs_you = false;
            }
            TerminalViewEvent::OpenFolder(_) => return,
            TerminalViewEvent::OpenDeepLink(url) => {
                cx.emit(PaneEvent::OpenDeepLink(url.clone()));
                return;
            }
            TerminalViewEvent::Restarted => {
                self.exited = false;
                self.agent = None;
                self.needs_you = false;
                self.prompt_back_at = agent::now_ms();
                self.prompt = PromptState::Unknown;
                self.command_epoch += 1;
                self.command = None;
                self.title = None;
            }
            TerminalViewEvent::Exited(code) => {
                self.exited = true;
                if *code == Some(0) {
                    cx.emit(PaneEvent::ExitedCleanly);
                    return;
                }
            }
        }
        cx.emit(PaneEvent::Changed);
        cx.notify();
    }

    /// Finds the program the running command started. OSC 133 C arrives before the shell
    /// spawns it, so the lookup retries a few times.
    fn look_up_command(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.terminal.read(cx).session().pid() else {
            return;
        };
        let epoch = self.command_epoch;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            for delay in [100, 400, 1500] {
                executor.timer(Duration::from_millis(delay)).await;
                let name = executor
                    .spawn(async move { super::process::newest_child_name(pid) })
                    .await;
                let done = this.update(cx, |pane, cx| {
                    if pane.command_epoch != epoch {
                        return true;
                    }
                    if name.is_some() {
                        pane.command = name;
                        cx.emit(PaneEvent::Changed);
                        cx.notify();
                        return true;
                    }
                    false
                });
                if done.unwrap_or(true) {
                    return;
                }
            }
        })
        .detach();
    }
}

impl EventEmitter<PaneEvent> for Pane {}

impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal.focus_handle(cx)
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let focused = self.terminal.focus_handle(cx).is_focused(window);
        let running = self.is_running();
        // An agent's state replaces the plain running dot and command name.
        let status = self
            .agent_status()
            .filter(|_| !self.exited)
            .map(|a| (agent::color(&a, p), Some(a.label())))
            .or_else(|| running.then(|| (p.now, self.command.clone())));

        let drag_label: SharedString = self.title().into();
        let outline = self.text_outline;
        let name = self.name().to_owned();
        // Overlay text lifts the header's text too, like the terminal's.
        let (name_color, muted) = if outline {
            (gpui_kit::white(), p.fg.lifted(0.82).to_hsla())
        } else {
            (p.muted.to_hsla(), p.muted.to_hsla())
        };
        let header = div()
            .id("pane-header")
            // Dragging the header moves the pane; drop zones appear on the other panes.
            .on_drag(DraggedPane { pane: cx.entity() }, move |_, _, _, cx| {
                let name = drag_label.clone();
                cx.new(|_| DragPreview { name })
            })
            .flex()
            .items_center()
            .gap(px(8.))
            .flex_none()
            .h(px(28.))
            .px(px(6.))
            .text_size(px(11.5))
            .text_color(p.muted.to_hsla())
            .bg(p.surface_2.to_hsla().opacity(self.backdrop))
            // GPUI clips children to a square, so the header rounds itself to keep the
            // ring's top corners visible.
            .rounded_t(px(7.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                drawer_pill(&self.cwd_name(), self.drawer_open, p)
                    .on_click(cx.listener(|pane, _, window, cx| pane.toggle_drawer(window, cx))),
            )
            // A long program title gives way to the status and buttons.
            .child(
                div()
                    .min_w_0()
                    .max_w(px(260.))
                    .child(outlined(outline, name_color, |c| {
                        div().truncate().text_color(c).child(name.clone())
                    })),
            )
            .when_some(status, |d, (color, label)| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .child(outlined(outline, color.to_hsla(), |c| {
                                    div().text_size(px(10.)).text_color(c).child("●")
                                })),
                        )
                        .when_some(label, |d, label| {
                            d.child(div().min_w_0().child(outlined(outline, muted, |c| {
                                div().truncate().text_color(c).child(label.clone())
                            })))
                        }),
                )
            })
            .child(div().flex_1().min_w_0())
            // A detached window holds one pane, so splitting and detaching do not apply.
            .when(!self.is_detached(), |d| {
                d.child(
                    header_button(
                        "detach",
                        gpui_kit::assets::IconName::SquareArrowOutUpRight,
                        "Detach into its own window",
                        p,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Detach))),
                )
            })
            .when(!self.is_detached(), |d| {
                d.child(
                    header_button(
                        "split",
                        IconName::PanelRight,
                        "Split right (Ctrl+Shift+D)",
                        p,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::SplitRight))),
                )
            })
            .when(!self.is_detached(), |d| {
                d.child(
                    header_button(
                        "split-down",
                        IconName::PanelBottom,
                        "Split down (Ctrl+Shift+E)",
                        p,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::SplitDown))),
                )
            })
            .child(
                header_button("close", IconName::Close, "Close pane (Ctrl+Shift+W)", p)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Close))),
            );

        let inner = div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .rounded(px(7.))
            // A detached window paints the backdrop once behind everything.
            .when(!self.is_detached(), |d| {
                d.bg(p.surface.to_hsla().opacity(self.backdrop))
            })
            .child(header)
            .child(self.render_body(p, cx));

        // Focus ring: a 1 px gradient border (the aurora / fire gradient), else the line colour.
        // It is a fill behind the pane showing through 1 px, so a faded detached window would
        // show it everywhere: there it is a real border, lit only while an agent pulses.
        let detached = self.is_detached();
        let ring = div().size_full().rounded(px(8.));
        let ring = if detached {
            ring.border_1().border_color(gpui_kit::transparent_black())
        } else if focused {
            let ring = ring.p(px(1.));
            ring.bg(linear_gradient(
                90.,
                linear_color_stop(p.gradient[0].to_hsla(), 0.),
                linear_color_stop(p.gradient[3].to_hsla(), 1.),
            ))
        } else {
            ring.p(px(1.)).bg(p.line.to_hsla())
        };
        let ring = ring
            .on_action(
                cx.listener(|pane, _: &ToggleDrawer, window, cx| pane.toggle_drawer(window, cx)),
            )
            // Dragging the drawer's edge: tracked on the whole pane so a fast mouse stays in it.
            .on_mouse_move(cx.listener(|pane, event: &MouseMoveEvent, _, cx| {
                let Some((start_x, start_width)) = pane.resizing else {
                    return;
                };
                if event.dragging() {
                    let width = start_width + (event.position.x / px(1.)) - start_x;
                    pane.drawer_width = width.clamp(drawer::MIN_WIDTH, drawer::MAX_WIDTH);
                    cx.notify();
                } else {
                    pane.resizing = None;
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|pane, _, _, _| pane.resizing = None),
            )
            .child(inner);

        // An agent blocked on the user pulses the border (and a soft glow) in its colour.
        let alert = self
            .agent_status()
            .filter(|a| a.pulses() && !self.exited)
            .map(|a| agent::color(&a, p).to_hsla());
        match alert {
            Some(color) => ring
                .with_animation(
                    ("pane-alert", cx.entity_id().as_u64() as usize),
                    agent::pulse(),
                    move |ring, t| {
                        let lit = color.opacity(0.35 + 0.65 * t);
                        let ring = if detached {
                            ring.border_color(lit)
                        } else {
                            ring.bg(lit)
                        };
                        ring.shadow(vec![BoxShadow {
                            color: color.opacity(0.45 * t),
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(8.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                    },
                )
                .into_any_element(),
            None => ring.into_any_element(),
        }
    }
}

impl Pane {
    /// Drawer (when open) and its resize edge beside the terminal, which takes dropped paths.
    fn render_body(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let drawer = self
            .drawer
            .as_ref()
            .filter(|_| self.drawer_open)
            .map(|(drawer, _)| drawer.clone());
        let accent = p.accent.to_hsla();
        let zones = cx.has_active_drag().then(|| self.render_drop_zones(p, cx));
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .when_some(drawer, |d, drawer| {
                d.child(
                    div()
                        .flex_none()
                        .w(px(self.drawer_width))
                        .h_full()
                        .border_r_1()
                        .border_color(p.line.to_hsla())
                        .child(drawer),
                )
                .child(
                    div()
                        .id("drawer-resize")
                        .flex_none()
                        .w(px(4.))
                        .h_full()
                        .cursor_col_resize()
                        .hover(move |s| s.bg(accent).opacity(0.5))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|pane, event: &gpui_kit::MouseDownEvent, _, cx| {
                                pane.resizing =
                                    Some((event.position.x / px(1.), pane.drawer_width));
                                cx.stop_propagation();
                            }),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .on_drop(cx.listener(|pane, dragged: &DraggedPath, window, cx| {
                        pane.insert_path(&dragged.path, window, cx)
                    }))
                    // A Workbench card's prompt, typed without Enter so it can be read first.
                    .on_drop(cx.listener(|pane, dragged: &DraggedCard, window, cx| {
                        pane.terminal.read(cx).insert_text(&dragged.prompt);
                        pane.terminal.focus_handle(cx).focus(window, cx);
                    }))
                    // Files dropped from Explorer / Finder too.
                    .on_drop(cx.listener(|pane, paths: &ExternalPaths, window, cx| {
                        for path in paths.paths() {
                            pane.insert_path(path, window, cx);
                        }
                    }))
                    .child(self.terminal.clone()),
            )
            .children(zones)
    }

    /// Five drop targets over the body while something is dragged: the outer quarters put the
    /// dragged pane on that side, the middle swaps. Only a dragged pane lights them up, so
    /// drawer rows still drop through onto the terminal.
    fn render_drop_zones(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let me = cx.entity_id();
        let (tint, ring) = (p.accent_soft.to_hsla(), p.accent.to_hsla());
        let zone = |zone: DropZone| {
            div()
                .id(("drop-zone", zone as usize))
                .absolute()
                .rounded(px(6.))
                .drag_over::<DraggedPane>(move |style, dragged, _, _| {
                    if dragged.pane.entity_id() == me {
                        style
                    } else {
                        style.bg(tint).border_2().border_color(ring)
                    }
                })
                .on_drop(cx.listener(move |pane, dragged: &DraggedPane, _, cx| {
                    if dragged.pane.entity_id() != cx.entity_id() {
                        cx.emit(PaneEvent::PaneDropped(dragged.pane.clone(), zone));
                    }
                    pane.resizing = None;
                }))
        };
        let (side, band) = (relative(0.25), relative(0.3));
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(zone(DropZone::Left).top_0().left_0().w(side).h_full())
            .child(zone(DropZone::Right).top_0().right_0().w(side).h_full())
            .child(
                zone(DropZone::Top)
                    .top_0()
                    .left(side)
                    .w(relative(0.5))
                    .h(band),
            )
            .child(
                zone(DropZone::Bottom)
                    .bottom_0()
                    .left(side)
                    .w(relative(0.5))
                    .h(band),
            )
            .child(
                zone(DropZone::Center)
                    .top(band)
                    .left(side)
                    .w(relative(0.5))
                    .h(relative(0.4)),
            )
            .into_any_element()
    }
}

/// The header's first item: folder icon, cwd folder name and a chevron; opens the drawer.
fn drawer_pill(name: &str, open: bool, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = p.surface_3.to_hsla();
    div()
        .id("drawer-pill")
        .flex()
        .items_center()
        .gap(px(6.))
        .flex_none()
        .max_w(px(170.))
        .h(px(22.))
        .pl(px(7.))
        .pr(px(8.))
        .rounded(px(11.))
        .border_1()
        .cursor_pointer()
        .text_size(px(12.))
        .map(|d| {
            if open {
                d.text_color(p.fg_emph.to_hsla())
                    .bg(p.accent_soft.to_hsla())
                    .border_color(p.accent.to_hsla())
            } else {
                d.text_color(p.fg.to_hsla())
                    .bg(p.surface_2.to_hsla())
                    .border_color(p.line_strong.to_hsla())
                    .hover(move |s| s.bg(hover))
            }
        })
        .tooltip(|window, cx| {
            gpui_kit::component::tooltip::Tooltip::new("Files (Ctrl+Shift+B)").build(window, cx)
        })
        .child(
            Icon::new(IconName::Folder)
                .size(px(14.))
                .text_color(if open { p.accent } else { p.muted }.to_hsla()),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name.to_owned()),
        )
        .child(
            div()
                .text_color(p.muted.to_hsla())
                .child(if open { "‹" } else { "›" }),
        )
}

/// `path` as one shell word: single quotes for PowerShell and POSIX shells, double quotes
/// for cmd (which has no single quotes). Plain paths stay unquoted.
fn quote_path(kind: ShellKind, path: &Path) -> String {
    let text = if kind == ShellKind::Wsl {
        wsl_path(path)
    } else {
        path.display().to_string()
    };
    let plain = text
        .chars()
        .all(|c| c.is_alphanumeric() || "/\\:._-+~".contains(c));
    if plain {
        return text;
    }
    match kind {
        ShellKind::Cmd => format!("\"{text}\""),
        ShellKind::Pwsh | ShellKind::WindowsPowerShell => format!("'{}'", text.replace('\'', "''")),
        _ => format!("'{}'", text.replace('\'', r"'\''")),
    }
}

/// A Windows path as WSL sees it: `C:\Users\me` → `/mnt/c/Users/me`. Others stay as they are.
fn wsl_path(path: &Path) -> String {
    let text = path.display().to_string();
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
            let rest = chars.as_str().replace('\\', "/");
            let rest = rest.trim_start_matches('/');
            format!("/mnt/{}/{rest}", drive.to_ascii_lowercase())
        }
        _ => text,
    }
}

/// The line that moves `kind`'s shell to `dir`.
fn cd_command(kind: ShellKind, dir: &Path) -> String {
    let dir = quote_path(kind, dir);
    match kind {
        // `/d` also switches drives.
        ShellKind::Cmd => format!("cd /d {dir}"),
        ShellKind::Pwsh | ShellKind::WindowsPowerShell => {
            format!("Set-Location -LiteralPath {dir}")
        }
        _ => format!("cd {dir}"),
    }
}

fn header_button(
    id: &'static str,
    icon: impl Into<Icon>,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = p.surface_3.to_hsla();
    let emph = p.fg_emph.to_hsla();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded(px(5.))
        .text_color(p.muted.to_hsla())
        .hover(move |s| s.bg(hover).text_color(emph))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
        .child(Icon::new(icon).size(px(13.)))
}

fn is_home(path: &Path) -> bool {
    std::env::home_dir().is_some_and(|home| home == path)
}

/// `~` for the home folder and paths under it.
pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        let rest = rest.display().to_string();
        return if rest.is_empty() {
            "~".into()
        } else {
            format!("~{}{rest}", std::path::MAIN_SEPARATOR)
        };
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_stay_unquoted() {
        let path = Path::new("src/main.rs");
        assert_eq!(quote_path(ShellKind::Bash, path), "src/main.rs");
        assert_eq!(quote_path(ShellKind::Cmd, path), "src/main.rs");
    }

    #[test]
    fn quoting_follows_the_shell() {
        let path = Path::new("my dir/it's");
        assert_eq!(quote_path(ShellKind::Bash, path), r"'my dir/it'\''s'");
        assert_eq!(quote_path(ShellKind::Pwsh, path), "'my dir/it''s'");
        assert_eq!(quote_path(ShellKind::Cmd, path), "\"my dir/it's\"");
    }

    #[test]
    fn cd_switches_drives_in_cmd() {
        let dir = Path::new("D:/a b");
        assert_eq!(cd_command(ShellKind::Cmd, dir), "cd /d \"D:/a b\"");
        assert_eq!(
            cd_command(ShellKind::Pwsh, dir),
            "Set-Location -LiteralPath 'D:/a b'"
        );
    }

    #[test]
    fn wsl_gets_mnt_paths() {
        assert_eq!(
            cd_command(ShellKind::Wsl, Path::new(r"C:\Users\me")),
            "cd /mnt/c/Users/me"
        );
        assert_eq!(
            quote_path(ShellKind::Wsl, Path::new(r"D:\a b\x.txt")),
            "'/mnt/d/a b/x.txt'"
        );
        assert_eq!(quote_path(ShellKind::Wsl, Path::new("src/x")), "src/x");
    }
}
