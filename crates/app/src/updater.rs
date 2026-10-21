//! Background updates through Velopack. Installed builds check GitHub Releases at start, download
//! a newer release, and hand it to Velopack's updater when the app quits.

use std::sync::{Mutex, PoisonError};

use gpui_kit::App;
use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

const REPO_URL: &str = "https://github.com/InfernoDragon0/MaomiTerminal";

/// A downloaded release waiting for the app to quit.
static PENDING: Mutex<Option<(UpdateManager, UpdateInfo)>> = Mutex::new(None);

/// Starts the update check when `updates.auto` is on and installs a downloaded release on quit.
pub fn init(cx: &mut App) {
    if !maomi_ui::settings::config(cx).updates.auto {
        return;
    }
    // Velopack's HTTP calls block, so they get their own thread instead of the executor.
    let spawned = std::thread::Builder::new()
        .name("updater".into())
        .spawn(|| {
            if let Err(err) = download() {
                tracing::warn!(%err, "update check failed");
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(%err, "could not start the update check");
    }
    cx.on_app_quit(|cx| {
        // Turning the setting off mid-session also skips an update that already downloaded.
        if maomi_ui::settings::config(cx).updates.auto {
            apply_pending();
        }
        async {}
    })
    .detach();
}

fn download() -> Result<(), velopack::Error> {
    let source = GithubSource::new(REPO_URL, None, false);
    let manager = match UpdateManager::new(source, None, None) {
        Ok(manager) => manager,
        // `cargo run` and portable copies are not installed; nothing to update.
        Err(err) => {
            tracing::debug!(%err, "not an installed build, updates off");
            return Ok(());
        }
    };
    let UpdateCheck::UpdateAvailable(update) = manager.check_for_updates()? else {
        return Ok(());
    };
    tracing::info!(version = %update.TargetFullRelease.Version, "downloading update");
    manager.download_updates(&update, None)?;
    *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some((manager, *update));
    Ok(())
}

fn apply_pending() {
    let Some((manager, update)) = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    // The updater waits (up to 60 s) for this process to exit, then installs silently without
    // relaunching. A failure here is retried by `VelopackApp` on the next start.
    if let Err(err) =
        manager.wait_exit_then_apply_updates(&update, true, false, Vec::<String>::new())
    {
        tracing::warn!(%err, "could not start the updater");
    }
}
