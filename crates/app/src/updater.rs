//! Background updates through Velopack. Installed builds check GitHub Releases at start, download
//! a newer release, and hand it to Velopack's updater when the app quits (or restarts on request).
//! Progress goes to `maomi_ui::updates`, which shows it in the title bar, Settings and toasts.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use gpui_kit::App;
use maomi_ui::settings;
use maomi_ui::updates::{self, UpdateHooks, UpdateState};
use velopack::sources::AutoSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

const REPO_URL: &str = "https://github.com/InfernoDragon0/MaomiTerminal";
/// Test switch: a local release folder or another feed URL instead of GitHub Releases.
const FEED_ENV: &str = "MAOMI_UPDATE_URL";

/// A downloaded release waiting for the app to quit.
struct Pending {
    manager: UpdateManager,
    update: UpdateInfo,
    /// Started from "Check now", so it installs even with automatic updates off.
    manual: bool,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// A check is running on the updater thread.
static CHECKING: AtomicBool = AtomicBool::new(false);
/// "Restart now" was clicked: relaunch after installing.
static RESTART: AtomicBool = AtomicBool::new(false);

/// Hooks the updater into the UI, starts the automatic check and announces a finished update.
/// Call after the first window opens, so the toasts have somewhere to show.
pub fn init(cx: &mut App) {
    let feed = std::env::var(FEED_ENV).unwrap_or_else(|_| REPO_URL.into());
    let manager = match UpdateManager::new(AutoSource::new(&feed), None, None) {
        Ok(manager) => manager,
        // `cargo run` builds are not installed; the UI shows updates as unavailable.
        Err(err) => {
            tracing::debug!(%err, "not an installed build, updates off");
            return;
        }
    };

    let (states, inbox) = async_channel::unbounded::<UpdateState>();
    cx.spawn(async move |cx| {
        while let Ok(state) = inbox.recv().await {
            cx.update(|cx| updates::set_state(state, cx));
        }
    })
    .detach();

    let check = {
        let (manager, states) = (manager.clone(), states.clone());
        Rc::new(move |_: &mut App| start(manager.clone(), states.clone(), true))
    };
    updates::install(
        UpdateHooks {
            check,
            restart: Rc::new(|cx| {
                RESTART.store(true, Ordering::Relaxed);
                // Quitting saves the layout first; the quit handler below installs and relaunches.
                cx.quit();
            }),
        },
        cx,
    );

    if settings::config(cx).updates.auto {
        start(manager, states, false);
    }
    cx.on_app_quit(|cx| {
        apply_pending(settings::config(cx).updates.auto);
        async {}
    })
    .detach();

    announce_update(cx);
}

/// Checks for and downloads an update on its own thread (Velopack's HTTP calls block).
fn start(manager: UpdateManager, states: async_channel::Sender<UpdateState>, manual: bool) {
    let has_pending = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    if has_pending || CHECKING.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            let state = check_and_download(&manager, &states, manual).unwrap_or_else(|err| {
                tracing::warn!(%err, "update check failed");
                UpdateState::Failed(err.to_string().into())
            });
            _ = states.send_blocking(state);
            CHECKING.store(false, Ordering::Relaxed);
        });
    if let Err(err) = spawned {
        CHECKING.store(false, Ordering::Relaxed);
        tracing::warn!(%err, "could not start the update check");
    }
}

fn check_and_download(
    manager: &UpdateManager,
    states: &async_channel::Sender<UpdateState>,
    manual: bool,
) -> Result<UpdateState, velopack::Error> {
    _ = states.send_blocking(UpdateState::Checking);
    let UpdateCheck::UpdateAvailable(update) = manager.check_for_updates()? else {
        return Ok(UpdateState::UpToDate);
    };
    let version: gpui_kit::SharedString = update.TargetFullRelease.Version.clone().into();
    _ = states.send_blocking(UpdateState::Downloading(version.clone()));
    tracing::info!(%version, "downloading update");
    manager.download_updates(&update, None)?;
    *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Pending {
        manager: manager.clone(),
        update: *update,
        manual,
    });
    Ok(UpdateState::Ready(version))
}

/// Hands a downloaded release to Velopack's updater, which waits (up to 60 s) for this process
/// to exit and installs it. A failure here is retried by `VelopackApp` on the next start.
fn apply_pending(auto: bool) {
    let Some(pending) = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let restart = RESTART.load(Ordering::Relaxed);
    // Turning automatic updates off mid-session skips an update it downloaded on its own.
    if !(auto || pending.manual || restart) {
        return;
    }
    if let Err(err) = pending.manager.wait_exit_then_apply_updates(
        &pending.update,
        true,
        restart,
        Vec::<String>::new(),
    ) {
        tracing::warn!(%err, "could not start the updater");
    }
}

/// Shows "Updated to vX" on the first start of a new version (not on the very first start).
fn announce_update(cx: &mut App) {
    let current = env!("CARGO_PKG_VERSION");
    let last = settings::config(cx).updates.last_version;
    if last.as_deref() == Some(current) {
        return;
    }
    settings::update_config(cx, |c| c.updates.last_version = Some(current.into()));
    if last.is_some() {
        let notes = format!("{REPO_URL}/releases/tag/v{current}");
        updates::show_updated(current.into(), notes.into(), cx);
    }
}
