//! Update status shown in the UI: the title-bar pill, the Settings > Updates row and toasts.
//!
//! The app crate owns the updater (Velopack) and reports through [`set_state`]; the UI calls
//! back through the [`UpdateHooks`] it installed. Observe the [`Updates`] global to re-render.

use std::rc::Rc;

use gpui_kit::component::{IconName, WindowExt as _, button::Button, notification::Notification};
use gpui_kit::{App, Global, SharedString};

/// Where the update check is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateState {
    /// Not an installed build (`cargo run`): updates are off.
    #[default]
    Unavailable,
    Idle,
    Checking,
    UpToDate,
    Downloading(SharedString),
    /// Downloaded; installs on quit or on [`restart`].
    Ready(SharedString),
    Failed(SharedString),
}

/// Calls back into the updater.
#[derive(Clone)]
pub struct UpdateHooks {
    pub check: Rc<dyn Fn(&mut App)>,
    /// Installs the downloaded release and starts the app again.
    pub restart: Rc<dyn Fn(&mut App)>,
}

pub struct Updates {
    state: UpdateState,
    hooks: Option<UpdateHooks>,
}

impl Global for Updates {}

/// Toast ids, so a newer toast replaces an older one of the same kind.
struct ReadyToast;
struct UpdatedToast;

/// Installs the updater's hooks. Without them the UI shows updates as unavailable.
pub fn install(hooks: UpdateHooks, cx: &mut App) {
    cx.set_global(Updates {
        state: UpdateState::Idle,
        hooks: Some(hooks),
    });
}

pub fn state(cx: &App) -> UpdateState {
    cx.try_global::<Updates>()
        .map(|u| u.state.clone())
        .unwrap_or_default()
}

/// Records the new state; reaching [`UpdateState::Ready`] shows the "Update ready" toast.
pub fn set_state(state: UpdateState, cx: &mut App) {
    let Some(updates) = cx.try_global::<Updates>() else {
        return;
    };
    if updates.state == state {
        return;
    }
    if let UpdateState::Ready(version) = &state {
        let version = version.clone();
        notify(cx, move || {
            Notification::new()
                .id::<ReadyToast>()
                .icon(IconName::ArrowUp)
                .title(format!("Update ready: v{version}"))
                .message("Installs when you quit.")
                .action(|_, _, _| {
                    Button::new("update-restart")
                        .label("Restart now")
                        .on_click(|_, _, cx| restart(cx))
                })
        });
    }
    cx.global_mut::<Updates>().state = state;
}

/// Shows the "Updated" toast; call on the first start of a new version.
pub fn show_updated(version: SharedString, release_notes_url: SharedString, cx: &mut App) {
    notify(cx, move || {
        let url = release_notes_url.clone();
        Notification::new()
            .id::<UpdatedToast>()
            .icon(IconName::CircleCheck)
            .title(format!("Updated to v{version}"))
            .action(move |_, _, _| {
                let url = url.clone();
                Button::new("update-notes")
                    .label("What's new")
                    .on_click(move |_, _, cx| cx.open_url(&url))
            })
            // An action turns autohide off; this one is only informational.
            .autohide(true)
    });
}

pub fn check(cx: &mut App) {
    if let Some(hooks) = hooks(cx) {
        (hooks.check)(cx);
    }
}

pub fn restart(cx: &mut App) {
    if let Some(hooks) = hooks(cx) {
        (hooks.restart)(cx);
    }
}

fn hooks(cx: &App) -> Option<UpdateHooks> {
    cx.try_global::<Updates>().and_then(|u| u.hooks.clone())
}

/// Pushes a toast to the active window, or the first window when none is active.
fn notify(cx: &mut App, build: impl Fn() -> Notification + 'static) {
    let window = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next());
    if let Some(window) = window {
        _ = window.update(cx, |_, window, cx| window.push_notification(build(), cx));
    }
}
