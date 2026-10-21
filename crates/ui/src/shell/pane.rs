//! One terminal pane: a 28 px header (drawer pill, shell name, running dot, split and close
//! buttons) over an optional file drawer and a [`TerminalView`], wrapped in the focus ring.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, linear_color_stop, linear_gradient, px, relative,
};
use maomi_config::DrawerLayout;
use maomi_term::{PromptState, PtySession, SessionOptions, ShellKind};

use super::actions::ToggleDrawer;
use super::drawer::{self, DragPreview, DraggedPath, DrawerEvent, FileDrawer};
use super::tree::DropZone;
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
    /// The shell exited with code 0; the pane goes away. Other codes keep the pane open
    /// with the restart banner so the error stays readable.
    ExitedCleanly,
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
    /// Created the first time the drawer opens.
    drawer: Option<(Entity<FileDrawer>, Subscription)>,
    drawer_open: bool,
    drawer_width: f32,
    /// Shell-follow for a drawer that is not created yet.
    drawer_follow: bool,
    /// Mouse x and drawer width when a drag on the drawer's edge began.
    resizing: Option<(f32, f32)>,
    _terminal_events: Subscription,
}

impl Pane {
    /// A pane for a started `session`; `options` are kept for restarting it.
    pub fn new(
        session: PtySession,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shell_name = session
            .shell()
            .program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".into());
        let cwd = session.cwd();
        let shell_kind = session.shell().kind;
        let terminal = cx.new(|cx| TerminalView::with_session(session, options, window, cx));
        let subscription = cx.subscribe_in(
            &terminal,
            window,
            |pane, _, event: &TerminalViewEvent, window, cx| match event {
                TerminalViewEvent::OpenFolder(path) => {
                    pane.show_in_drawer(path.clone(), window, cx)
                }
                event => pane.on_terminal_event(event, cx),
            },
        );
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
            drawer: None,
            drawer_open: false,
            drawer_width: drawer::DEFAULT_WIDTH,
            drawer_follow: settings::config(cx).shell.drawer_follow,
            resizing: None,
            _terminal_events: subscription,
        }
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
        let drawer = cx.new(|cx| FileDrawer::new(dir, follow, window, cx));
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
                if *state == PromptState::Running {
                    self.look_up_command(cx);
                }
                if let Some((drawer, _)) = &self.drawer {
                    let running = *state == PromptState::Running;
                    drawer.update(cx, |d, cx| d.set_follow_blocked(running, cx));
                }
            }
            TerminalViewEvent::Bell | TerminalViewEvent::OpenFolder(_) => return,
            TerminalViewEvent::Restarted => {
                self.exited = false;
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
        let running = self.prompt == PromptState::Running && !self.exited;

        let drag_label: SharedString = self.title().into();
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
            .bg(p.surface_2.to_hsla())
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                drawer_pill(&self.cwd_name(), self.drawer_open, p)
                    .on_click(cx.listener(|pane, _, window, cx| pane.toggle_drawer(window, cx))),
            )
            .child(div().flex_none().child(self.shell_name.clone()))
            .when(running, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(p.now.to_hsla())
                                .child("●"),
                        )
                        .when_some(self.command.clone(), |d, name| {
                            d.child(div().truncate().child(name))
                        }),
                )
            })
            .child(div().flex_1().min_w_0())
            .child(
                header_button(
                    "split",
                    IconName::PanelRight,
                    "Split right (Ctrl+Shift+D)",
                    p,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::SplitRight))),
            )
            .child(
                header_button(
                    "split-down",
                    IconName::PanelBottom,
                    "Split down (Ctrl+Shift+E)",
                    p,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::SplitDown))),
            )
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
            .bg(p.surface.to_hsla())
            .child(header)
            .child(self.render_body(p, cx));

        // Focus ring: a 1 px gradient border (the aurora / fire gradient), else the line colour.
        let ring = div().size_full().p(px(1.)).rounded(px(8.));
        let ring = if focused {
            ring.bg(linear_gradient(
                90.,
                linear_color_stop(p.gradient[0].to_hsla(), 0.),
                linear_color_stop(p.gradient[3].to_hsla(), 1.),
            ))
        } else {
            ring.bg(p.line.to_hsla())
        };
        ring.on_action(
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
        .child(inner)
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
                        pane.insert_path(&dragged.0, window, cx)
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
    let text = path.display().to_string();
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
    icon: IconName,
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
}
