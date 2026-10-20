//! The app shell: windows, tabs, split panes, activity rail and sessions dock.

mod pane;
mod process;
mod settings_page;
mod tree;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, KeyBinding, WeakEntity, WindowBounds,
    WindowOptions, point, px, size,
};
use maomi_config::{Layout, WindowLayout};

pub use pane::{Pane, PaneEvent};
pub use settings_page::SettingsPage;
pub use tree::{Direction, PaneTree};
pub use workspace::Workspace;

/// Key context of a window's workspace; its shortcuts work from inside any pane.
pub const KEY_CONTEXT: &str = "Workspace";

pub mod actions {
    gpui_kit::actions!(
        workspace,
        [
            NewTab,
            ClosePane,
            SplitRight,
            SplitDown,
            FocusLeft,
            FocusRight,
            FocusUp,
            FocusDown,
            NextTab,
            PrevTab,
            NewWindow,
            OpenSettings
        ]
    );
}
use actions::*;

/// How a window starts.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Typed into every pane of the first tab at start (dev and perf checks).
    pub run: Option<String>,
    /// Panes in the first tab (1-4).
    pub panes: usize,
    /// Overrides the config's smooth scrolling (dev switch); None = use the config.
    pub smooth_scroll: Option<bool>,
    /// Tabs, splits and bounds saved by the last run; replaces `panes`.
    pub restore: Option<WindowLayout>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            run: None,
            panes: 1,
            smooth_scroll: None,
            restore: None,
        }
    }
}

pub(crate) fn init(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl-shift"
    };
    cx.bind_keys([
        KeyBinding::new(&format!("{primary}-t"), NewTab, ctx),
        KeyBinding::new(&format!("{primary}-w"), ClosePane, ctx),
        KeyBinding::new(&format!("{primary}-d"), SplitRight, ctx),
        KeyBinding::new(&format!("{primary}-e"), SplitDown, ctx),
        KeyBinding::new(&format!("{primary}-n"), NewWindow, ctx),
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-,"
            } else {
                "ctrl-,"
            },
            OpenSettings,
            ctx,
        ),
        KeyBinding::new("alt-left", FocusLeft, ctx),
        KeyBinding::new("alt-right", FocusRight, ctx),
        KeyBinding::new("alt-up", FocusUp, ctx),
        KeyBinding::new("alt-down", FocusDown, ctx),
        KeyBinding::new("ctrl-tab", NextTab, ctx),
        KeyBinding::new("ctrl-shift-tab", PrevTab, ctx),
    ]);
    // Closing the last window quits the app.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    // Quitting with windows still open (Cmd+Q). Closing the last window saves in
    // `Workspace::should_close_window` instead, since its workspace is gone by the time we quit.
    cx.on_app_quit(|cx| {
        let windows = snapshot_open_windows(cx);
        if !windows.is_empty() {
            save_layout(windows);
        }
        async {}
    })
    .detach();
}

/// Every open workspace, so the layout can be saved on quit.
#[derive(Default)]
struct OpenWorkspaces(Vec<(AnyWindowHandle, WeakEntity<Workspace>)>);

impl Global for OpenWorkspaces {}

pub(crate) fn register_workspace(
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let open = cx.default_global::<OpenWorkspaces>();
    open.0.retain(|(_, ws)| ws.upgrade().is_some());
    open.0.push((window, workspace));
}

fn snapshot_open_windows(cx: &mut App) -> Vec<WindowLayout> {
    let open = cx
        .try_global::<OpenWorkspaces>()
        .map(|o| o.0.clone())
        .unwrap_or_default();
    open.into_iter()
        .filter_map(|(handle, ws)| {
            let ws = ws.upgrade()?;
            handle
                .update(cx, |_, window, cx| ws.read(cx).snapshot(window, cx))
                .ok()
        })
        .collect()
}

/// Writes the layout the next start reopens; an empty list makes it open one fresh window.
pub(crate) fn save_layout(windows: Vec<WindowLayout>) {
    let layout = Layout {
        windows,
        ..Layout::default()
    };
    if let Err(err) = layout.save() {
        tracing::warn!(%err, "could not save the window layout");
    }
}

/// Bounds to reopen a window at, or None (OS placement) when no display shows its title bar.
fn restore_bounds(layout: &WindowLayout, cx: &App) -> Option<WindowBounds> {
    let b = layout.bounds?;
    let bounds = Bounds {
        origin: point(px(b.x), px(b.y)),
        size: size(px(b.width.max(480.0)), px(b.height.max(320.0))),
    };
    // A point just inside the top edge, where the tabs are: it must be on some screen.
    let grab = point(bounds.origin.x + px(60.), bounds.origin.y + px(10.));
    if !cx.displays().iter().any(|d| d.bounds().contains(&grab)) {
        return None;
    }
    Some(if layout.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    })
}

/// Opens a window with its own workspace and tab set.
pub fn open_window(launch: LaunchOptions, cx: &mut App) {
    let window_bounds = launch.restore.as_ref().and_then(|l| restore_bounds(l, cx));
    let options = WindowOptions {
        window_min_size: Some(size(px(480.0), px(320.0))),
        window_bounds,
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, move |window, cx| {
        window.set_window_title("MaomiTerminal");
        cx.new(|cx| Workspace::new(launch, window, cx))
    });
    if let Err(err) = opened {
        tracing::error!(%err, "failed to open a window");
    }
}
