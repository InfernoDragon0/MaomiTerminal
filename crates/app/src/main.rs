//! MaomiTerminal desktop app entry point.

// Release builds are GUI-only on Windows (no console window); debug keeps the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use maomi_ui::shell::{self, LaunchOptions};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MAOMITERMINAL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // The bundled component icons (rail, tabs, window controls) come from this asset source.
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        maomi_ui::init(cx);
        shell::open_window(dev_launch_options(), cx);
    });
}

/// Dev switches: `MAOMI_PANES` (1-4) panes in the first tab, `MAOMI_RUN` typed into each of
/// them at start (render and perf checks), `MAOMI_SMOOTH=0` turns smooth scrolling off.
fn dev_launch_options() -> LaunchOptions {
    LaunchOptions {
        run: std::env::var("MAOMI_RUN")
            .ok()
            .filter(|cmd| !cmd.is_empty()),
        panes: std::env::var("MAOMI_PANES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .clamp(1, 4),
        smooth_scroll: std::env::var("MAOMI_SMOOTH").map_or(true, |v| v != "0"),
    }
}
