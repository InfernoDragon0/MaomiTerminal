//! MaomiTerminal desktop app entry point.

// Release builds are GUI-only on Windows (no console window); debug keeps the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::component::{ActiveTheme as _, TitleBar};
use gpui_kit::{
    App, AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, Window,
    WindowOptions, div, px, size,
};
use tracing_subscriber::EnvFilter;

/// Placeholder root view: one centred, muted label.
struct Root;

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .text_color(cx.theme().muted_foreground)
            .child("MaomiTerminal")
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
        cx.new(|_| Root)
    });
    if let Err(err) = opened {
        tracing::error!(%err, "failed to open the main window");
        cx.quit();
    }
}
