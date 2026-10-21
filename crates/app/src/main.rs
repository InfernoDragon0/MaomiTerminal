//! MaomiTerminal desktop app entry point.

// Release builds are GUI-only on Windows (no console window); debug keeps the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use maomi_config::Layout;
use maomi_ui::shell::{self, LaunchOptions};
use tracing_subscriber::EnvFilter;

mod updater;

fn main() {
    // Must run first: the installer and updater start the exe with hook arguments, and an update
    // downloaded earlier but never applied is installed here (which restarts the app).
    velopack::VelopackApp::build().run();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MAOMITERMINAL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (config, err) = maomi_config::Config::load_or_default();
    if let Some(err) = err {
        tracing::warn!(%err, "using the default config");
    }

    // The bundled component icons (rail, tabs, window controls) come from this asset source.
    let launch = dev_launch_options();
    // Dev switches start one fresh window so perf runs are repeatable.
    let dev = launch.run.is_some() || launch.panes > 1;
    let layout = if dev {
        Layout::default()
    } else {
        Layout::load()
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            maomi_ui::init(config, cx);
            if layout.windows.is_empty() {
                shell::open_window(launch.clone(), cx);
            }
            for window in layout.windows {
                let launch = LaunchOptions {
                    restore: Some(window),
                    ..launch.clone()
                };
                shell::open_window(launch, cx);
            }
            // After the windows: its toasts need one to show in.
            updater::init(cx);
        });
}

/// Dev switches: `MAOMI_PANES` (1-4) panes in the first tab, `MAOMI_RUN` typed into each of
/// them at start (render and perf checks), `MAOMI_SMOOTH=0`/`1` overrides the smooth-scroll setting.
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
        smooth_scroll: std::env::var("MAOMI_SMOOTH").ok().map(|v| v != "0"),
        restore: None,
    }
}
