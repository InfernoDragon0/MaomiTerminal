//! The secrets vault as a GPUI global: unlocking (silently through the OS keystore, or with the
//! passphrase prompt), auto-lock after idle, and switching the unlock method from Settings.
//!
//! Slow work (scrypt, the keystore, Windows Hello) runs on the background executor. The vault
//! moves there and back, and [`status`] reports [`VaultStatus::Busy`] meanwhile.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Icon, IconName, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, Global, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, px,
};
use maomi_vault::{
    Keystore, Method, OsKeystore, Presence, SecretString, Unlock, Vault, VaultError,
};

use crate::{Theme, settings};

/// Values of `security.unlock` in the config.
pub const UNLOCK_KEYSTORE: &str = "keystore";
pub const UNLOCK_PASSPHRASE: &str = "passphrase";
pub const UNLOCK_ASK: &str = "ask";

const FILE_NAME: &str = "vault.age";
const AUTO_LOCK_CHECK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultStatus {
    /// No vault file yet; the first unlock creates it.
    Missing,
    Locked,
    Unlocked,
    /// An unlock or re-encrypt is running.
    Busy,
}

pub(crate) struct VaultState {
    /// None when the config folder cannot be found.
    path: Option<PathBuf>,
    keystore: Arc<dyn Keystore>,
    vault: Option<Vault>,
    busy: bool,
}

impl Global for VaultState {}

/// Runs after the vault opens.
type Then = Box<dyn FnOnce(&mut Window, &mut App)>;

/// Why a background step failed.
enum Failure {
    Vault(VaultError),
    /// The user cancelled Windows Hello.
    Declined,
    PresenceUnavailable,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Vault(err) => err.fmt(f),
            Failure::Declined => f.write_str("Windows Hello check cancelled"),
            Failure::PresenceUnavailable => f.write_str(
                "Windows Hello is not available. Turn the check off in Settings > Security.",
            ),
        }
    }
}

impl From<VaultError> for Failure {
    fn from(err: VaultError) -> Self {
        Failure::Vault(err)
    }
}

/// What a background step gets: the vault (when open) moves in and comes back out.
struct Job {
    path: PathBuf,
    keystore: Arc<dyn Keystore>,
    vault: Option<Vault>,
}

/// Installs the global and the auto-lock timer. Called once from [`crate::init`].
pub(crate) fn install(cx: &mut App) {
    let path = maomi_config::Config::path()
        .ok()
        .map(|p| p.with_file_name(FILE_NAME));
    cx.set_global(VaultState {
        path,
        keystore: Arc::new(OsKeystore::default()),
        vault: None,
        busy: false,
    });
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(AUTO_LOCK_CHECK).await;
            cx.update(auto_lock);
        }
    })
    .detach();
}

pub fn status(cx: &App) -> VaultStatus {
    let Some(state) = cx.try_global::<VaultState>() else {
        return VaultStatus::Missing;
    };
    if state.busy {
        VaultStatus::Busy
    } else if state.vault.is_some() {
        VaultStatus::Unlocked
    } else if state.path.as_ref().is_some_and(|p| p.exists()) {
        VaultStatus::Locked
    } else {
        VaultStatus::Missing
    }
}

/// Runs `f` on the open vault; None while it is locked.
pub fn with_vault<R>(cx: &mut App, f: impl FnOnce(&mut Vault) -> R) -> Option<R> {
    cx.try_global::<VaultState>()?.vault.as_ref()?;
    cx.global_mut::<VaultState>().vault.as_mut().map(f)
}

/// Drops the open vault, which zeroes its secrets.
pub fn lock(cx: &mut App) {
    if cx
        .try_global::<VaultState>()
        .is_some_and(|s| s.vault.is_some() && !s.busy)
    {
        cx.global_mut::<VaultState>().vault = None;
    }
}

/// Runs `then` once the vault is open. Unlocks first: silently through the OS keystore (after
/// Windows Hello when Settings ask for it), or with the passphrase prompt. A missing vault is
/// created with the configured method.
pub fn unlock(
    window: &mut Window,
    cx: &mut App,
    then: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let Some(state) = cx.try_global::<VaultState>() else {
        return;
    };
    if state.vault.is_some() {
        then(window, cx);
        return;
    }
    if state.busy {
        return;
    }
    let Some(path) = state.path.clone() else {
        toast_error("Cannot find the config folder for the vault", window, cx);
        return;
    };
    let then: Then = Box::new(then);
    let security = settings::config(cx).security;
    let wants_keystore = security.unlock == UNLOCK_KEYSTORE;
    match Vault::method_of(&path) {
        Err(err) => toast_error(err.to_string(), window, cx),
        Ok(None) if wants_keystore => {
            run_job(
                window,
                cx,
                |job| {
                    let created = Vault::create(&job.path, Unlock::Keystore, job.keystore);
                    match created {
                        Ok(vault) => (Some(vault), Ok(())),
                        Err(err) => (None, Err(err.into())),
                    }
                },
                move |result, window, cx| match result {
                    Ok(()) => then(window, cx),
                    // No usable keystore: fall back to a passphrase.
                    Err(Failure::Vault(VaultError::Keystore(_))) => {
                        open_form(FormKind::Create, true, Some(then), window, cx);
                    }
                    Err(err) => toast_error(err.to_string(), window, cx),
                },
            );
        }
        Ok(None) => open_form(FormKind::Create, false, Some(then), window, cx),
        Ok(Some(Method::Keystore)) => {
            let gate = security.require_presence;
            run_job(
                window,
                cx,
                move |job| {
                    if gate {
                        match maomi_vault::verify_presence("Unlock the MaomiTerminal vault") {
                            Presence::Verified => {}
                            Presence::Declined => return (None, Err(Failure::Declined)),
                            Presence::Unavailable => {
                                return (None, Err(Failure::PresenceUnavailable));
                            }
                        }
                    }
                    match Vault::unlock(&job.path, Unlock::Keystore, job.keystore) {
                        Ok(vault) => (Some(vault), Ok(())),
                        Err(err) => (None, Err(err.into())),
                    }
                },
                move |result, window, cx| match result {
                    Ok(()) => then(window, cx),
                    Err(Failure::Declined) => {}
                    Err(err) => toast_error(err.to_string(), window, cx),
                },
            );
        }
        Ok(Some(Method::Passphrase)) => {
            open_form(FormKind::Unlock, wants_keystore, Some(then), window, cx);
        }
    }
}

/// Switches the unlock method from Settings, re-encrypting the vault when the key changes.
pub fn set_method(method: &'static str, window: &mut Window, cx: &mut App) {
    if settings::config(cx).security.unlock == method {
        return;
    }
    let file_method = cx
        .try_global::<VaultState>()
        .and_then(|s| s.path.as_ref())
        .and_then(|p| Vault::method_of(p).ok().flatten());
    let wanted = if method == UNLOCK_KEYSTORE {
        Method::Keystore
    } else {
        Method::Passphrase
    };
    // No vault yet, or the same key (passphrase <-> always ask): only the setting changes.
    if file_method.is_none_or(|m| m == wanted) {
        settings::update_config(cx, |c| c.security.unlock = method.into());
        return;
    }
    unlock(window, cx, move |window, cx| {
        if wanted == Method::Passphrase {
            open_form(FormKind::Change { method }, false, None, window, cx);
            return;
        }
        run_job(
            window,
            cx,
            |mut job| {
                let Some(vault) = job.vault.as_mut() else {
                    return (None, Ok(()));
                };
                let result = vault.set_unlock(Unlock::Keystore).map_err(Failure::from);
                (job.vault, result)
            },
            move |result, window, cx| match result {
                Ok(()) => settings::update_config(cx, |c| c.security.unlock = method.into()),
                Err(err) => toast_error(err.to_string(), window, cx),
            },
        );
    });
}

/// Asks for a new passphrase and re-encrypts the vault with it.
pub fn change_passphrase(window: &mut Window, cx: &mut App) {
    let method = match settings::config(cx).security.unlock.as_str() {
        UNLOCK_ASK => UNLOCK_ASK,
        _ => UNLOCK_PASSPHRASE,
    };
    unlock(window, cx, move |window, cx| {
        open_form(FormKind::Change { method }, false, None, window, cx);
    });
}

/// Locks an idle vault. "Passphrase" asks once per launch, so it never auto-locks.
fn auto_lock(cx: &mut App) {
    let security = settings::config(cx).security;
    if security.auto_lock_minutes == 0 || security.unlock == UNLOCK_PASSPHRASE {
        return;
    }
    let limit = Duration::from_secs(u64::from(security.auto_lock_minutes) * 60);
    let idle = cx
        .try_global::<VaultState>()
        .and_then(|s| s.vault.as_ref())
        .is_some_and(|v| v.idle_for() >= limit);
    if idle {
        lock(cx);
    }
}

/// Runs `job` on the background executor with the vault moved in, then `done` on the window.
fn run_job(
    window: &Window,
    cx: &mut App,
    job: impl FnOnce(Job) -> (Option<Vault>, Result<(), Failure>) + Send + 'static,
    done: impl FnOnce(Result<(), Failure>, &mut Window, &mut App) + 'static,
) {
    let state = cx.global_mut::<VaultState>();
    let Some(path) = state.path.clone() else {
        return;
    };
    let input = Job {
        path,
        keystore: state.keystore.clone(),
        vault: state.vault.take(),
    };
    state.busy = true;
    let task = cx.background_executor().spawn(async move { job(input) });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let (vault, result) = task.await;
        cx.update(|cx| {
            let state = cx.global_mut::<VaultState>();
            state.busy = false;
            state.vault = vault;
        });
        _ = handle.update(cx, |_, window, cx| done(result, window, cx));
    })
    .detach();
}

fn toast_error(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::error(message).title("Vault"), cx);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormKind {
    Unlock,
    /// First passphrase for a new vault.
    Create,
    /// New passphrase for the open vault; `method` is stored once it is saved.
    Change {
        method: &'static str,
    },
}

/// The passphrase prompt inside the dialog.
struct PassphraseForm {
    kind: FormKind,
    passphrase: Entity<InputState>,
    /// Second field for a new passphrase.
    confirm: Option<Entity<InputState>>,
    /// Shows "OS keystore unavailable": the config asks for the keystore, the file uses a passphrase.
    keystore_notice: bool,
    error: Option<SharedString>,
    busy: bool,
    /// The passphrase field took focus on the first render (the dialog takes it on open).
    focused: bool,
    then: Option<Then>,
    _subscriptions: Vec<Subscription>,
}

fn open_form(
    kind: FormKind,
    keystore_notice: bool,
    then: Option<Then>,
    window: &mut Window,
    cx: &mut App,
) {
    let masked = |window: &mut Window, cx: &mut App, placeholder: &'static str| {
        cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(placeholder)
        })
    };
    let passphrase = masked(
        window,
        cx,
        if kind == FormKind::Unlock {
            "Passphrase"
        } else {
            "New passphrase"
        },
    );
    let confirm = (kind != FormKind::Unlock).then(|| masked(window, cx, "Type it again"));
    let form = cx.new(|cx| {
        // Enter in either field submits; the inputs keep Enter from reaching the dialog.
        let subscriptions = [Some(&passphrase), confirm.as_ref()]
            .into_iter()
            .flatten()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |form: &mut PassphraseForm, _, event, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            form.submit(window, cx);
                        }
                    },
                )
            })
            .collect();
        PassphraseForm {
            kind,
            passphrase,
            confirm,
            keystore_notice,
            error: None,
            busy: false,
            focused: false,
            then,
            _subscriptions: subscriptions,
        }
    });
    let (title, ok) = match kind {
        FormKind::Unlock => ("Unlock vault", "Unlock"),
        FormKind::Create => ("Set a vault passphrase", "Create vault"),
        FormKind::Change { .. } => ("Change passphrase", "Save"),
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let busy = form.read(cx).busy;
        let submit = form.clone();
        dialog
            .title(title)
            .w(px(420.))
            .child(form.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                    .child(
                        DialogAction::new()
                            .child(Button::new("vault-ok").primary().label(ok).loading(busy)),
                    ),
            )
            .on_ok(move |_, window, cx| {
                submit.update(cx, |form, cx| form.submit(window, cx));
                false
            })
    });
}

impl PassphraseForm {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.passphrase.read(cx).value();
        if text.is_empty() {
            return;
        }
        if let Some(confirm) = &self.confirm
            && confirm.read(cx).value() != text
        {
            self.error = Some("The passphrases do not match.".into());
            cx.notify();
            return;
        }
        let passphrase = SecretString::from(text.to_string());
        let kind = self.kind;
        self.busy = true;
        self.error = None;
        cx.notify();
        let form = cx.entity();
        run_job(
            window,
            cx,
            move |mut job| {
                let unlock = Unlock::Passphrase(passphrase);
                match kind {
                    FormKind::Unlock => match Vault::unlock(&job.path, unlock, job.keystore) {
                        Ok(vault) => (Some(vault), Ok(())),
                        Err(err) => (None, Err(err.into())),
                    },
                    FormKind::Create => match Vault::create(&job.path, unlock, job.keystore) {
                        Ok(vault) => (Some(vault), Ok(())),
                        Err(err) => (None, Err(err.into())),
                    },
                    FormKind::Change { .. } => {
                        let result = match job.vault.as_mut() {
                            Some(vault) => vault.set_unlock(unlock).map_err(Failure::from),
                            None => Ok(()),
                        };
                        (job.vault, result)
                    }
                }
            },
            move |result, window, cx| {
                let then = form.update(cx, |form, cx| {
                    form.busy = false;
                    cx.notify();
                    match &result {
                        Err(Failure::Vault(VaultError::WrongKey)) => {
                            form.error = Some("Wrong passphrase. Try again.".into());
                            None
                        }
                        Err(err) => {
                            form.error = Some(err.to_string().into());
                            None
                        }
                        Ok(()) => Some(form.then.take()),
                    }
                });
                let Some(then) = then else {
                    return;
                };
                match kind {
                    // A vault made because the keystore failed records that it uses a passphrase.
                    FormKind::Create => settings::update_config(cx, |c| {
                        if c.security.unlock == UNLOCK_KEYSTORE {
                            c.security.unlock = UNLOCK_PASSPHRASE.into();
                        }
                    }),
                    FormKind::Change { method } => {
                        settings::update_config(cx, |c| c.security.unlock = method.into());
                    }
                    FormKind::Unlock => {}
                }
                window.close_dialog(cx);
                if let Some(then) = then {
                    then(window, cx);
                }
            },
        );
    }
}

impl Render for PassphraseForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            self.passphrase
                .update(cx, |input, cx| input.focus(window, cx));
        }
        let p = Theme::current(cx).palette();
        let sub = match self.kind {
            FormKind::Unlock => "Enter the vault passphrase to use saved credentials.",
            FormKind::Create => {
                "Saved credentials are encrypted with this passphrase. It cannot be recovered if you forget it."
            }
            FormKind::Change { .. } => "The vault is encrypted again with the new passphrase.",
        };
        let notice = self.keystore_notice.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .p(px(10.))
                .rounded(px(8.))
                .border_1()
                .border_color(p.warn.with_alpha(0x66).to_hsla())
                .bg(p.warn.with_alpha(0x1f).to_hsla())
                .child(
                    div()
                        .text_color(p.fg_emph.to_hsla())
                        .child("OS keystore unavailable."),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.muted.to_hsla())
                        .child(
                            "Using a passphrase instead. You can change this in Settings > Security.",
                        ),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().text_color(p.muted.to_hsla()).child(sub))
            .children(notice)
            .child(Input::new(&self.passphrase).mask_toggle())
            .when_some(self.confirm.as_ref(), |d, confirm| {
                d.child(Input::new(confirm).mask_toggle())
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(p.err.to_hsla())
                        .child(Icon::new(IconName::CircleX).size_3p5())
                        .child(error),
                )
            })
    }
}
