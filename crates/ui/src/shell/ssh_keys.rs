//! Settings > SSH keys (private keys stored in the vault: generate, import, copy the public
//! key, delete; the ssh-agent switch) and Settings > Known hosts (trusted host keys).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, PromptLevel, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use maomi_config::Host;
use maomi_ssh::{KeyInfo, KeyType, KnownHost, SshError};
use maomi_vault::{Entry, EntryKind, ExposeSecret as _, SecretString};

use super::hosts::{self, HostStore};
use super::settings_page::{row, section};
use super::ssh;
use crate::{Palette, TERMINAL_FAMILY, Theme, settings, vault};

/// One stored key, described once per vault change (parsing RSA keys every frame is wasteful).
struct KeyRow {
    id: String,
    label: String,
    info: Option<KeyInfo>,
}

/// The two settings sections as one entity, so they keep their own cache.
pub struct SshKeysSection {
    /// None while the vault is locked.
    keys: Option<Vec<KeyRow>>,
    known: Vec<KnownHost>,
    /// How many keys the running agent holds, or why it cannot be reached; None while
    /// checking.
    agent: Option<Result<usize, String>>,
    _subscriptions: Vec<Subscription>,
}

impl SshKeysSection {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_global::<vault::VaultState>(|this, cx| this.refresh(cx)),
            cx.observe_global::<HostStore>(|_, cx| cx.notify()),
        ];
        let mut this = Self {
            keys: None,
            known: Vec::new(),
            agent: None,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this.check_agent(cx);
        this
    }

    fn check_agent(&mut self, cx: &mut Context<Self>) {
        if !settings::config(cx).ssh.use_agent {
            return;
        }
        self.agent = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let keys = maomi_ssh::agent_keys()
                .await
                .map(|keys| keys.len())
                .map_err(|err| err.to_string());
            _ = this.update(cx, |this, cx| {
                this.agent = Some(keys);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_agent(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let enabled = settings::config(cx).ssh.use_agent;
        let (chip, color): (SharedString, _) = match &self.agent {
            _ if !enabled => ("Not used".into(), p.line_strong),
            None => ("Checking\u{2026}".into(), p.line_strong),
            Some(Ok(keys)) => (
                format!(
                    "Agent running \u{b7} {keys} key{}",
                    if *keys == 1 { "" } else { "s" }
                )
                .into(),
                p.ok,
            ),
            Some(Err(_)) => ("No agent found".into(), p.err),
        };
        let reason = match &self.agent {
            Some(Err(reason)) if enabled => Some(SharedString::from(reason.clone())),
            _ => None,
        };
        row(
            "Use ssh-agent",
            Some("Try keys loaded in the running agent before each host's own login"),
            p,
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .id("agent-chip")
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(999.))
                        .bg(p.surface_2.to_hsla())
                        .text_size(px(11.5))
                        .text_color(p.muted.to_hsla())
                        .when_some(reason, |d, reason| {
                            d.tooltip(move |window, cx| {
                                Tooltip::new(reason.clone()).build(window, cx)
                            })
                        })
                        .child(div().size(px(6.)).rounded_full().bg(color.to_hsla()))
                        .child(chip),
                )
                .child(
                    Switch::new("switch-agent")
                        .checked(enabled)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            settings::update_config(cx, |c| c.ssh.use_agent = checked);
                            this.check_agent(cx);
                        })),
                )
                .into_any_element(),
        )
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.keys = vault::with_vault(cx, |v| {
            v.entries()
                .filter(|(_, e)| e.kind == EntryKind::PrivateKey)
                .map(|(id, e)| KeyRow {
                    id: id.to_owned(),
                    label: e.label.clone(),
                    info: maomi_ssh::key_info(e.secret.expose_secret()).ok(),
                })
                .collect()
        });
        self.known = ssh::known_hosts()
            .and_then(|k| k.entries().ok())
            .unwrap_or_default();
        cx.notify();
    }

    fn render_keys(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = Vec::new();
        match &self.keys {
            None => rows.push(row(
                "Vault locked",
                Some("Unlock the vault to see and add keys"),
                p,
                Button::new("keys-unlock")
                    .primary()
                    .small()
                    .label("Unlock…")
                    .on_click(|_, window, cx| vault::unlock(window, cx, |_, _| {}))
                    .into_any_element(),
            )),
            Some(keys) if keys.is_empty() => rows.push(
                div()
                    .py(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child("No keys yet. Generate one, or import an existing key.")
                    .into_any_element(),
            ),
            Some(keys) => rows.extend(keys.iter().map(|key| key_row(key, p, cx))),
        }
        rows.push(self.render_agent(p, cx));
        let unlocked = self.keys.is_some();
        rows.push(
            div()
                .flex()
                .gap(px(8.))
                .py(px(12.))
                .child(
                    Button::new("keys-generate")
                        .small()
                        .label("Generate key…")
                        .disabled(!unlocked)
                        .on_click(|_, window, cx| open_generate(window, cx)),
                )
                .child(
                    Button::new("keys-import")
                        .small()
                        .label("Import…")
                        .disabled(!unlocked)
                        .on_click(|_, window, cx| open_import(window, cx)),
                )
                .into_any_element(),
        );
        section("SSH keys", p, rows)
    }

    fn render_known(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        if self.known.is_empty() {
            rows.push(
                div()
                    .py(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child("No hosts trusted yet. Hosts are added when you first connect.")
                    .into_any_element(),
            );
        }
        for (i, entry) in self.known.iter().enumerate() {
            let remove = entry.clone();
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(p.line.to_hsla())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_color(p.fg_emph.to_hsla())
                                    .child(entry.host.clone()),
                            )
                            .child(
                                div()
                                    .font_family(TERMINAL_FAMILY)
                                    .text_size(px(11.5))
                                    .text_color(p.muted.to_hsla())
                                    .text_ellipsis()
                                    .child(format!("{} {}", entry.algorithm, entry.fingerprint)),
                            ),
                    )
                    .child(
                        Button::new(("known-remove", i))
                            .ghost()
                            .xsmall()
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(Err(err)) =
                                    ssh::known_hosts().map(|k| k.remove(&remove))
                                {
                                    window.push_notification(
                                        Notification::error(format!(
                                            "Could not update known_hosts: {err}"
                                        )),
                                        cx,
                                    );
                                }
                                this.refresh(cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        section("Known hosts", p, rows)
    }
}

impl Render for SshKeysSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        div()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(self.render_keys(p, cx))
            .child(self.render_known(p, cx))
    }
}

fn key_row(key: &KeyRow, p: &Palette, cx: &mut Context<SshKeysSection>) -> AnyElement {
    let used = hosts::hosts_using_key(&key.id, cx);
    let (kind, fingerprint) = match &key.info {
        Some(info) => (info.kind.clone(), info.fingerprint.clone()),
        None => ("unreadable".to_owned(), String::new()),
    };
    let public = key.info.as_ref().map(|i| i.public_openssh.clone());
    let (id, label) = (key.id.clone(), key.label.clone());
    div()
        .id(SharedString::from(format!("key-{}", key.id)))
        .flex()
        .items_center()
        .gap(px(12.))
        .py(px(10.))
        .border_b_1()
        .border_color(p.line.to_hsla())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_color(p.fg_emph.to_hsla())
                                .child(key.label.clone()),
                        )
                        .child(
                            div()
                                .px(px(7.))
                                .rounded(px(999.))
                                .bg(p.surface_2.to_hsla())
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child(kind),
                        ),
                )
                .child(
                    div()
                        .font_family(TERMINAL_FAMILY)
                        .text_size(px(11.5))
                        .text_color(p.muted.to_hsla())
                        .text_ellipsis()
                        .child(fingerprint),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.muted.to_hsla())
                        .child(match used {
                            0 => "Not used by any host".to_owned(),
                            1 => "Used by 1 host".to_owned(),
                            n => format!("Used by {n} hosts"),
                        }),
                ),
        )
        .child(
            Button::new(SharedString::from(format!("key-copy-{}", key.id)))
                .xsmall()
                .label("Copy public key")
                .disabled(public.is_none())
                .on_click(move |_, window, cx| {
                    if let Some(public) = &public {
                        cx.write_to_clipboard(ClipboardItem::new_string(public.clone()));
                        window.push_notification(
                            Notification::success(
                                "Public key copied. Add it to the server's authorized_keys.",
                            ),
                            cx,
                        );
                    }
                }),
        )
        .child(
            Button::new(SharedString::from(format!("key-delete-{}", key.id)))
                .ghost()
                .xsmall()
                .label("Delete")
                .on_click(cx.listener(move |_, _, window, cx| {
                    delete_key(id.clone(), label.clone(), used, window, cx)
                })),
        )
        .into_any_element()
}

/// Asks first, then removes the key from the vault and from the hosts that used it.
fn delete_key(id: String, label: String, used: usize, window: &mut Window, cx: &mut App) {
    let detail = if used == 0 {
        "The key is removed from the vault. This cannot be undone.".to_owned()
    } else {
        format!("{used} host(s) log in with it and will need another key. This cannot be undone.")
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Delete the key \"{label}\"?"),
        Some(&detail),
        &["Delete", "Cancel"],
        cx,
    );
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        if answer.await != Ok(0) {
            return;
        }
        _ = handle.update(cx, |_, window, cx| {
            match vault::with_vault(cx, |v| v.remove(&id)) {
                Some(Ok(_)) => hosts::forget_key(&id, cx),
                Some(Err(err)) => window.push_notification(
                    Notification::error(format!("Could not delete the key: {err}")),
                    cx,
                ),
                None => {}
            }
        });
    })
    .detach();
}

/// Stores a private key (OpenSSH text) in the open vault under a new id.
fn store_key(label: String, openssh: &str, cx: &mut App) -> Result<(), String> {
    let entry = Entry {
        label,
        kind: EntryKind::PrivateKey,
        secret: SecretString::from(openssh.to_owned()),
    };
    let id = format!("key:{}", Host::new_id());
    match vault::with_vault(cx, |v| v.put(&id, entry)) {
        Some(Ok(())) => Ok(()),
        Some(Err(err)) => Err(err.to_string()),
        None => Err("The vault locked. Unlock it and try again.".into()),
    }
}

/// The Generate and Import dialogs; they draw their own buttons (the footer would not redraw).
struct KeyDialog {
    import: bool,
    name: Entity<InputState>,
    kind: KeyType,
    /// Import only: the pasted key and its passphrase.
    text: Option<Entity<TextareaState>>,
    passphrase: Option<Entity<InputState>>,
    error: Option<SharedString>,
    busy: bool,
}

fn open_generate(window: &mut Window, cx: &mut App) {
    open_dialog(false, window, cx);
}

fn open_import(window: &mut Window, cx: &mut App) {
    open_dialog(true, window, cx);
}

fn open_dialog(import: bool, window: &mut Window, cx: &mut App) {
    let name = cx.new(|cx| {
        InputState::new(window, cx).placeholder(if import { "work-deploy" } else { "id_ed25519" })
    });
    let text = import.then(|| {
        cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
        })
    });
    let passphrase = import.then(|| {
        cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Only if the key is encrypted")
        })
    });
    name.update(cx, |input, cx| input.focus(window, cx));
    let dialog = cx.new(|_| KeyDialog {
        import,
        name,
        kind: KeyType::Ed25519,
        text,
        passphrase,
        error: None,
        busy: false,
    });
    let title = if import { "Import key" } else { "Generate key" };
    window.open_dialog(cx, move |d, _, _| {
        d.title(title).w(px(480.)).child(dialog.clone())
    });
}

impl KeyDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Give the key a name.".into());
            cx.notify();
            return;
        }
        let comment = name.clone();
        let (kind, import) = (self.kind, self.import);
        let text = self
            .text
            .as_ref()
            .map(|t| t.read(cx).value().to_string())
            .unwrap_or_default();
        let passphrase = self
            .passphrase
            .as_ref()
            .map(|p| p.read(cx).value().to_string());
        self.busy = true;
        self.error = None;
        cx.notify();
        // RSA generation and key decryption take a moment.
        let task = cx.background_executor().spawn(async move {
            if import {
                maomi_ssh::import_key(&text, passphrase.as_deref())
            } else {
                maomi_ssh::generate_key(kind, &comment)
            }
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            _ = handle.update(cx, |_, window, cx| {
                let stored = match result {
                    Ok(key) => store_key(name, &key, cx),
                    Err(SshError::KeyEncrypted) => {
                        Err("This key is encrypted. Enter its passphrase.".into())
                    }
                    Err(err) => Err(err.to_string()),
                };
                match stored {
                    Ok(()) => window.close_dialog(cx),
                    Err(error) => {
                        _ = this.update(cx, |this, cx| {
                            this.busy = false;
                            this.error = Some(error.into());
                            cx.notify();
                        });
                    }
                }
            });
        })
        .detach();
    }

    /// Fills the key box from a file the user picks.
    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import this key".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let read = std::fs::read_to_string(&path);
            _ = this.update_in(cx, |this, window, cx| match read {
                Ok(text) => {
                    if let Some(area) = &this.text {
                        area.update(cx, |area, cx| area.set_value(text, window, cx));
                    }
                    if this.name.read(cx).value().is_empty()
                        && let Some(stem) = path.file_stem()
                    {
                        let stem = stem.to_string_lossy().into_owned();
                        this.name
                            .update(cx, |name, cx| name.set_value(stem, window, cx));
                    }
                }
                Err(err) => {
                    this.error = Some(format!("Cannot read the file: {err}").into());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for KeyDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let label = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(p.fg_emph.to_hsla())
                .child(text)
        };
        let kinds =
            [(KeyType::Ed25519, "ed25519"), (KeyType::Rsa, "RSA-4096")].map(|(kind, name)| {
                let on = self.kind == kind;
                let emph = p.fg_emph.to_hsla();
                div()
                    .id(name)
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .text_size(px(12.))
                    .map(|d| {
                        if on {
                            d.text_color(emph).bg(p.surface.to_hsla())
                        } else {
                            d.text_color(p.muted.to_hsla())
                                .hover(move |s| s.text_color(emph))
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kind = kind;
                        cx.notify();
                    }))
                    .child(name)
            });
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().text_color(p.muted.to_hsla()).child(if self.import {
                "Paste an OpenSSH or PEM private key, or choose the file. It is stored in the vault."
            } else {
                "The private key is stored in the vault. Copy the public key to the server afterwards."
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(label("Name"))
                    .child(Input::new(&self.name)),
            )
            .when(!self.import, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(label("Type"))
                        .child(
                            div()
                                .flex()
                                .gap(px(2.))
                                .p(px(3.))
                                .rounded(px(9.))
                                .bg(p.surface_2.to_hsla())
                                .children(kinds),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(p.muted.to_hsla())
                                .child("ed25519 works with most servers. Pick RSA only for old ones."),
                        ),
                )
            })
            .when_some(self.text.as_ref(), |d, text| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(label("Private key"))
                                .child(
                                    Button::new("key-choose-file")
                                        .ghost()
                                        .xsmall()
                                        .label("Choose file…")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.choose_file(window, cx)
                                        })),
                                ),
                        )
                        .child(Textarea::new(text).font_family(TERMINAL_FAMILY)),
                )
            })
            .when_some(self.passphrase.as_ref(), |d, passphrase| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(label("Passphrase"))
                        .child(Input::new(passphrase).mask_toggle()),
                )
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(div().text_size(px(12.)).text_color(p.err.to_hsla()).child(error))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1())
                    .child(
                        Button::new("key-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("key-ok")
                            .primary()
                            .label(if self.import { "Import" } else { "Generate" })
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
