//! One terminal pane: a 28 px header (cwd pill, shell name, running dot, split and close
//! buttons) over a [`TerminalView`], wrapped in the focus ring.

use std::path::{Path, PathBuf};

use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, linear_color_stop,
    linear_gradient, px,
};
use maomi_term::{PromptState, PtySession, SessionOptions};

use crate::{Palette, TerminalView, TerminalViewEvent, Theme};

/// What a pane tells the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Title, cwd or running state changed; tab titles and the dock refresh.
    Changed,
    SplitRight,
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
    exited: bool,
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
        let terminal = cx.new(|cx| TerminalView::with_session(session, options, window, cx));
        let subscription = cx.subscribe(&terminal, |pane, _, event: &TerminalViewEvent, cx| {
            pane.on_terminal_event(event, cx)
        });
        Self {
            terminal,
            shell_name,
            title: None,
            cwd,
            prompt: PromptState::Unknown,
            exited: false,
            _terminal_events: subscription,
        }
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
            TerminalViewEvent::CwdChanged(path) => self.cwd = Some(path.clone()),
            TerminalViewEvent::PromptChanged(state) => self.prompt = *state,
            TerminalViewEvent::Bell => return,
            TerminalViewEvent::Restarted => {
                self.exited = false;
                self.prompt = PromptState::Unknown;
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

        let header = div()
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
            .child(cwd_pill(&self.cwd_name(), p))
            .child(div().flex_none().child(self.shell_name.clone()))
            .when(running, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(p.now.to_hsla())
                        .child("●"),
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
            .child(div().flex_1().min_h_0().child(self.terminal.clone()));

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
        ring.child(inner)
    }
}

fn cwd_pill(name: &str, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .flex_none()
        .max_w(px(180.))
        .h(px(20.))
        .px(px(7.))
        .rounded(px(6.))
        .text_color(p.fg.to_hsla())
        .bg(p.surface_3.to_hsla())
        .child(
            Icon::new(IconName::Folder)
                .size(px(12.))
                .text_color(p.muted.to_hsla()),
        )
        .child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name.to_owned()),
        )
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
