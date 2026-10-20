//! MaomiTerminal desktop app entry point.

// Release builds are GUI-only on Windows (no console window); debug keeps the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::component::{ActiveTheme as _, TitleBar};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, Window, WindowOptions, div, px, size,
};
use maomi_term::{PtySession, SessionOptions};
use maomi_ui::TerminalView;
use tracing_subscriber::EnvFilter;

/// Temporary root until the App shell card: a title bar over 1-4 terminal panes.
///
/// Dev switches: `MAOMI_PANES` (1-4) sets the pane count; `MAOMI_RUN` is typed into every pane
/// at start (for render and perf checks); `MAOMI_SMOOTH=0` turns smooth scrolling off.
struct Root {
    panes: Vec<Entity<TerminalView>>,
}

impl Root {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let count = std::env::var("MAOMI_PANES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 4);
        let run = std::env::var("MAOMI_RUN")
            .ok()
            .filter(|cmd| !cmd.is_empty());
        let smooth = std::env::var("MAOMI_SMOOTH").map_or(true, |v| v != "0");
        let mut panes = Vec::new();
        for _ in 0..count {
            let options = SessionOptions::default();
            match PtySession::spawn(options.clone()) {
                Ok(session) => {
                    if let Some(cmd) = &run {
                        session.write(format!("{cmd}\r"));
                    }
                    panes.push(cx.new(|cx| {
                        let mut view = TerminalView::with_session(session, options, window, cx);
                        view.set_smooth_scroll(smooth, cx);
                        view
                    }));
                }
                Err(err) => tracing::error!(%err, "failed to start the shell"),
            }
        }
        if let Some(first) = panes.first() {
            first.focus_handle(cx).focus(window, cx);
        }
        Self { panes }
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line = cx.theme().border;
        let row = |panes: &[Entity<TerminalView>]| {
            div().flex().flex_1().min_h_0().gap(px(1.)).children(
                panes
                    .iter()
                    .map(|pane| div().flex_1().min_w_0().child(pane.clone())),
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .child(TitleBar::new().child("MaomiTerminal"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(1.))
                    .bg(line)
                    .children(self.panes.chunks(2).map(row)),
            )
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MAOMITERMINAL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        maomi_ui::init(cx);
        open_main_window(cx);
    });
}

fn open_main_window(cx: &mut App) {
    let options = WindowOptions {
        window_min_size: Some(size(px(480.0), px(320.0))),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("MaomiTerminal");
        cx.new(|cx| Root::new(window, cx))
    });
    if let Err(err) = opened {
        tracing::error!(%err, "failed to open the main window");
        cx.quit();
    }
}
