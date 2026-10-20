//! The app shell: windows, tabs, split panes, activity rail and sessions dock.

mod pane;
mod process;
mod tree;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext as _, KeyBinding, WindowOptions, px, size};

pub use pane::{Pane, PaneEvent};
pub use tree::{Direction, PaneTree};
pub use workspace::Workspace;

/// Key context of a window's workspace; its shortcuts work from inside any pane.
pub const KEY_CONTEXT: &str = "Workspace";

pub mod actions {
    gpui_kit::actions!(
        workspace,
        [
            NewTab, ClosePane, SplitRight, SplitDown, FocusLeft, FocusRight, FocusUp, FocusDown,
            NextTab, PrevTab, NewWindow
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
    pub smooth_scroll: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            run: None,
            panes: 1,
            smooth_scroll: true,
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
}

/// Opens a window with its own workspace and tab set.
pub fn open_window(launch: LaunchOptions, cx: &mut App) {
    let options = WindowOptions {
        window_min_size: Some(size(px(480.0), px(320.0))),
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
