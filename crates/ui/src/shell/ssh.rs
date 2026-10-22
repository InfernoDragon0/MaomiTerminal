//! Opens SSH connections into panes. Reads the login from the vault, answers maomi-ssh's
//! prompts with dialogs, and swaps the pane's placeholder for the live session.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, WeakEntity, Window, div, px,
};
use maomi_config::{ForwardKind, Host, HostAuth, PortForward, RouteError};
use maomi_ssh::{
    AuthMethod, ConnectOptions, Forward, KeyboardPrompt, KnownHosts, Login, Prompt, SshError, Trust,
};
use maomi_term::{PtySession, SessionOptions};
use maomi_vault::{Entry, EntryKind, ExposeSecret as _, SecretString};
use zeroize::Zeroizing;

use super::hosts::{self, HostStatus};
use crate::{Connection, TERMINAL_FAMILY, TerminalView, Theme, settings, vault};

/// The app's known_hosts file, next to config.json.
pub(crate) fn known_hosts() -> Option<KnownHosts> {
    let config = maomi_config::Config::path().ok()?;
    Some(KnownHosts::in_dir(config.parent()?))
}

/// Shell name of a pane connected to `host`.
pub(crate) fn label(host: &Host) -> String {
    format!("ssh {}", hosts::display_name(host))
}

/// An empty session for a pane that is still connecting.
pub(crate) fn placeholder(host: &Host, options: SessionOptions) -> Option<PtySession> {
    PtySession::placeholder(label(host), options)
        .inspect_err(|err| tracing::error!(%err, "cannot create the SSH placeholder"))
        .ok()
}

/// Connects `view` (a pane's terminal) to the saved host `host_id`; Enter on the pane's
/// banner calls this again to reconnect.
pub(crate) fn connect(
    host_id: String,
    view: WeakEntity<TerminalView>,
    window: &mut Window,
    cx: &mut App,
) {
    let route = match hosts::route(&host_id, cx) {
        Ok(route) => route,
        Err(err) => {
            let reason = match err {
                RouteError::Missing => "This host or one of its jump hosts was deleted",
                RouteError::Loop => "The jump hosts lead back to a host already on the way",
            };
            set_connection(&view, Connection::Failed(reason.into()), cx);
            return;
        }
    };
    let reconnect = {
        let (host_id, view) = (host_id.clone(), view.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| {
            connect(host_id.clone(), view.clone(), window, cx)
        })
    };
    _ = view.update(cx, |view, _| view.set_reconnect(reconnect));

    let needs_vault = route.iter().any(|host| {
        (host.auth == HostAuth::Key && host.key_id.is_some())
            || (host.auth == HostAuth::Password && host.password_saved)
    });
    if !needs_vault {
        let saved = route.iter().map(|_| Vec::new()).collect();
        start(route, saved, view, window, cx);
        return;
    }
    // Stays if the unlock dialog is cancelled, so Enter tries again.
    set_connection(
        &view,
        Connection::Failed("Unlock the vault to connect".into()),
        cx,
    );
    vault::unlock(window, cx, move |window, cx| {
        let saved = vault::with_vault(cx, |v| {
            route.iter().map(|h| saved_login(h, v)).collect::<Vec<_>>()
        })
        .unwrap_or_else(|| route.iter().map(|_| None).collect());
        let missing_key = route
            .iter()
            .zip(&saved)
            .find(|(host, saved)| host.auth == HostAuth::Key && saved.is_none());
        if let Some((host, _)) = missing_key {
            let reason = format!(
                "The key for {} is missing from the vault",
                hosts::display_name(host)
            );
            set_connection(&view, Connection::Failed(reason.into()), cx);
            return;
        }
        let saved = saved.into_iter().map(Option::unwrap_or_default).collect();
        start(route, saved, view, window, cx);
    });
}

/// The host's key, or its saved password, from the open vault.
fn saved_login(host: &Host, vault: &mut maomi_vault::Vault) -> Option<Vec<AuthMethod>> {
    let entry = match host.auth {
        HostAuth::Key => host.key_id.as_deref()?,
        HostAuth::Password => &host.password_entry(),
        HostAuth::Agent => return None,
    };
    let secret = Zeroizing::new(vault.get(entry)?.secret.expose_secret().to_owned());
    Some(vec![if host.auth == HostAuth::Key {
        AuthMethod::PrivateKey {
            key: secret,
            passphrase: None,
        }
    } else {
        AuthMethod::Password(Some(secret))
    }])
}

/// Connects along `route` (jump hosts, then the target) with each hop's saved login.
fn start(
    route: Vec<Host>,
    saved: Vec<Vec<AuthMethod>>,
    view: WeakEntity<TerminalView>,
    window: &mut Window,
    cx: &mut App,
) {
    let (Some(size), Some(target)) = (
        view.upgrade().map(|v| v.read(cx).session().size()),
        route.last().cloned(),
    ) else {
        return;
    };
    let via: Vec<String> = route[..route.len() - 1]
        .iter()
        .map(hosts::display_name)
        .collect();
    let connecting = if via.is_empty() {
        format!("Connecting to {}…", target.address())
    } else {
        format!(
            "Connecting to {} via {}…",
            target.address(),
            via.join(" \u{2192} ")
        )
    };
    set_connection(&view, Connection::Connecting(connecting.into()), cx);
    let use_agent = settings::config(cx).ssh.use_agent;
    let logins = route
        .iter()
        .zip(saved)
        .map(|(host, saved)| {
            let mut auth = Vec::new();
            if use_agent || host.auth == HostAuth::Agent {
                auth.push(AuthMethod::Agent);
            }
            auth.extend(saved);
            if host.auth != HostAuth::Key {
                // A saved password that stopped working, or an agent without the right key,
                // falls back to asking.
                auth.push(AuthMethod::Password(None));
                auth.push(AuthMethod::KeyboardInteractive);
            }
            Login {
                host: host.host.clone(),
                port: host.port,
                user: host.user.clone(),
                auth,
            }
        })
        .collect();
    let options = ConnectOptions {
        route: logins,
        forwards: target.forwards.iter().map(ssh_forward).collect(),
        forward_agent: target.forward_agent,
        session: SessionOptions {
            size,
            ..settings::session_options(&settings::config(cx))
        },
        label: label(&target),
    };

    let (prompts, questions) = async_channel::unbounded::<Prompt>();
    let handle = window.window_handle();
    let route = Rc::new(route);
    cx.spawn(async move |cx| {
        while let Ok(prompt) = questions.recv().await {
            let route = route.clone();
            _ = handle.update(cx, |_, window, cx| ask(prompt, &route, window, cx));
        }
    })
    .detach();

    let host_id = target.id.clone();
    cx.spawn(async move |cx| {
        let result = maomi_ssh::connect(options, prompts).await;
        cx.update(|cx| {
            let result = result.map(|shell| {
                let failed: Vec<String> = target
                    .forwards
                    .iter()
                    .zip(&shell.forwards)
                    .filter_map(|(f, r)| r.as_ref().err().map(|e| format!("{}: {e}", f.summary())))
                    .collect();
                if !failed.is_empty() {
                    _ = handle.update(cx, |_, window, cx| {
                        window.push_notification(
                            Notification::error(format!(
                                "Port forwards not started:\n{}",
                                failed.join("\n")
                            )),
                            cx,
                        );
                    });
                }
                let states = target
                    .forwards
                    .iter()
                    .cloned()
                    .zip(shell.forwards)
                    .collect();
                hosts::set_forwards(&host_id, states, cx);
                shell.session
            });
            let status = match &result {
                Ok(_) => HostStatus::Connected,
                Err(_) => HostStatus::Error,
            };
            hosts::set_status(&host_id, status, cx);
            _ = view.update(cx, |view, cx| match result {
                Ok(session) => view.replace_session(session, cx),
                Err(SshError::Cancelled) => {
                    view.set_connection(Some(Connection::Failed("Cancelled".into())), cx);
                }
                Err(err) => {
                    view.set_connection(Some(Connection::Failed(err.to_string().into())), cx)
                }
            });
        });
    })
    .detach();
}

fn ssh_forward(forward: &PortForward) -> Forward {
    let (bind, port) = (forward.bind.clone(), forward.port);
    let (host, host_port) = (forward.host.clone(), forward.host_port);
    match forward.kind {
        ForwardKind::Local => Forward::Local {
            bind,
            port,
            host,
            host_port,
        },
        ForwardKind::Remote => Forward::Remote {
            bind,
            port,
            host,
            host_port,
        },
        ForwardKind::Dynamic => Forward::Dynamic { bind, port },
    }
}

fn set_connection(view: &WeakEntity<TerminalView>, connection: Connection, cx: &mut App) {
    _ = view.update(cx, |view, cx| view.set_connection(Some(connection), cx));
}

/// Answers one prompt, with a dialog unless the answer is already known. Closing the dialog
/// drops the reply, which cancels the connection. `route` names the host each hop is.
fn ask(prompt: Prompt, route: &[Host], window: &mut Window, cx: &mut App) {
    let hop_host = |hop: usize| route.get(hop).or(route.last()).cloned().unwrap_or_default();
    match prompt {
        Prompt::HostKey { key, reply } => {
            let known = known_hosts();
            let trust = known.as_ref().map_or(Trust::New, |k| {
                k.check(&key).unwrap_or_else(|err| {
                    tracing::warn!(%err, "cannot read known_hosts");
                    Trust::New
                })
            });
            let (title, sub, mono, ok_label, danger) = match &trust {
                Trust::Known => {
                    _ = reply.try_send(true);
                    return;
                }
                Trust::New => (
                    "Trust this host?",
                    format!(
                        "First connection to {}:{}. Check the fingerprint with the server's admin before you trust it.",
                        key.host, key.port
                    ),
                    format!("{}\n{}", key.algorithm, key.fingerprint),
                    "Trust and connect",
                    false,
                ),
                Trust::Changed { old_fingerprint } => (
                    "The host key changed",
                    format!(
                        "{}:{} sent a different key than last time. Someone may be intercepting the connection. Replace it only if the server's admin confirms the new key.",
                        key.host, key.port
                    ),
                    format!("Old  {old_fingerprint}\nNew  {}", key.fingerprint),
                    "Replace and connect",
                    true,
                ),
            };
            open(
                PromptDialog {
                    title: title.into(),
                    sub: sub.into(),
                    mono: Some(mono.into()),
                    fields: Vec::new(),
                    save: None,
                    ok_label: ok_label.into(),
                    danger,
                    answer: Some(Box::new(move |_, window, cx| {
                        if let Some(Err(err)) = known.map(|k| k.trust(&key)) {
                            window.push_notification(
                                Notification::error(format!("Could not save known_hosts: {err}")),
                                cx,
                            );
                        }
                        _ = reply.try_send(true);
                    })),
                },
                window,
                cx,
            );
        }
        Prompt::Password {
            hop,
            user,
            host: name,
            reply,
        } => {
            let host = hop_host(hop);
            let field = masked("Password", window, cx);
            open(
                PromptDialog {
                    title: format!("Password for {user}@{name}").into(),
                    sub: "Enter the password for this host.".into(),
                    mono: None,
                    fields: vec![field],
                    save: Some(false),
                    ok_label: "Connect".into(),
                    danger: false,
                    answer: Some(Box::new(move |answer, window, cx| {
                        let password = answer.values.into_iter().next().unwrap_or_default();
                        if answer.save {
                            save_password(&host, password.clone(), window, cx);
                        }
                        _ = reply.try_send(Some(Zeroizing::new(password)));
                    })),
                },
                window,
                cx,
            );
        }
        Prompt::KeyPassphrase { hop, retry, reply } => {
            let host = hop_host(hop);
            let field = masked("Passphrase", window, cx);
            open(
                PromptDialog {
                    title: "Key passphrase".into(),
                    sub: if retry {
                        "Wrong passphrase. Try again.".to_owned()
                    } else {
                        format!(
                            "The private key for {} is encrypted.",
                            hosts::display_name(&host)
                        )
                    }
                    .into(),
                    mono: None,
                    fields: vec![field],
                    save: None,
                    ok_label: "Unlock key".into(),
                    danger: false,
                    answer: Some(Box::new(move |answer, _, _| {
                        let passphrase = answer.values.into_iter().next().unwrap_or_default();
                        _ = reply.try_send(Some(Zeroizing::new(passphrase)));
                    })),
                },
                window,
                cx,
            );
        }
        Prompt::KeyboardInteractive {
            hop,
            name,
            instructions,
            prompts,
            reply,
        } => {
            let fields = prompts
                .iter()
                .map(|KeyboardPrompt { text, echo }| {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(!echo)
                            .placeholder(text.trim().to_owned())
                    });
                    (SharedString::from(text.trim().to_owned()), input)
                })
                .collect();
            let title = if name.trim().is_empty() {
                format!("{} asks", hop_host(hop).address())
            } else {
                name
            };
            open(
                PromptDialog {
                    title: title.into(),
                    sub: instructions.into(),
                    mono: None,
                    fields,
                    save: None,
                    ok_label: "Send".into(),
                    danger: false,
                    answer: Some(Box::new(move |answer, _, _| {
                        let answers = answer.values.into_iter().map(Zeroizing::new).collect();
                        _ = reply.try_send(Some(answers));
                    })),
                },
                window,
                cx,
            );
        }
    }
}

/// Stores the password in the vault (unlocking it first) and marks the host.
fn save_password(host: &Host, password: String, window: &mut Window, cx: &mut App) {
    let host = host.clone();
    vault::unlock(window, cx, move |window, cx| {
        let entry = Entry {
            label: format!("Password for {}", host.address()),
            kind: EntryKind::Password,
            secret: SecretString::from(password),
        };
        let saved = vault::with_vault(cx, |v| v.put(&host.password_entry(), entry));
        match saved {
            Some(Ok(())) => hosts::update_host(&host.id, |h| h.password_saved = true, cx),
            Some(Err(err)) => {
                window.push_notification(
                    Notification::error(format!("Could not save the password: {err}")),
                    cx,
                );
            }
            None => {}
        }
    });
}

fn masked(
    label: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> (SharedString, Entity<InputState>) {
    let input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(label));
    (label.into(), input)
}

/// What the user typed, and the "Save in vault" tick.
struct Answer {
    values: Vec<String>,
    save: bool,
}

type OnAnswer = Box<dyn FnOnce(Answer, &mut Window, &mut App)>;

/// A dialog for one prompt: text, an optional monospace box, fields and a save tick. It draws
/// its own buttons; the dialog footer would not redraw when the tick changes.
struct PromptDialog {
    title: SharedString,
    sub: SharedString,
    mono: Option<SharedString>,
    fields: Vec<(SharedString, Entity<InputState>)>,
    save: Option<bool>,
    ok_label: SharedString,
    /// The OK action is risky: it is drawn as a quiet danger button and Cancel is the primary.
    danger: bool,
    answer: Option<OnAnswer>,
}

struct PromptView {
    dialog: PromptDialog,
    focused: bool,
    _subscriptions: Vec<Subscription>,
}

fn open(dialog: PromptDialog, window: &mut Window, cx: &mut App) {
    let title = dialog.title.clone();
    let view = cx.new(|cx| {
        // Enter in the last field answers.
        let subscriptions = dialog
            .fields
            .last()
            .map(|(_, input)| {
                cx.subscribe_in(
                    input,
                    window,
                    |view: &mut PromptView, _, event, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            view.submit(window, cx);
                        }
                    },
                )
            })
            .into_iter()
            .collect();
        PromptView {
            dialog,
            focused: false,
            _subscriptions: subscriptions,
        }
    });
    window.open_dialog(cx, move |d, _, _| {
        d.title(title.clone()).w(px(460.)).child(view.clone())
    });
}

impl PromptView {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(answer) = self.dialog.answer.take() else {
            return;
        };
        let values = self
            .dialog
            .fields
            .iter()
            .map(|(_, input)| input.read(cx).value().to_string())
            .collect();
        let save = self.dialog.save.unwrap_or(false);
        window.close_dialog(cx);
        answer(Answer { values, save }, window, cx);
    }
}

impl Render for PromptView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            if let Some((_, input)) = self.dialog.fields.first() {
                input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
        let p = Theme::current(cx).palette();
        let d = &self.dialog;
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .when(!d.sub.is_empty(), |el| {
                el.child(div().text_color(p.muted.to_hsla()).child(d.sub.clone()))
            })
            .when_some(d.mono.clone(), |el, text| {
                el.child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(p.surface_2.to_hsla())
                        .font_family(TERMINAL_FAMILY)
                        .text_size(px(12.))
                        .text_color(p.fg_emph.to_hsla())
                        .children(text.split('\n').map(|line| div().child(line.to_owned()))),
                )
            })
            .children(d.fields.iter().map(|(label, input)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.fg_emph.to_hsla())
                            .child(label.clone()),
                    )
                    .child(Input::new(input).mask_toggle())
            }))
            .when_some(d.save, |el, save| {
                el.child(
                    Checkbox::new("prompt-save")
                        .checked(save)
                        .label("Save in vault")
                        .on_click(cx.listener(|view, checked: &bool, _, cx| {
                            view.dialog.save = Some(*checked);
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1())
                    .child(
                        Button::new("prompt-cancel")
                            .label("Cancel")
                            .when(d.danger, |b| b.primary())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("prompt-ok")
                            .map(|b| if d.danger { b.danger() } else { b.primary() })
                            .label(d.ok_label.clone())
                            .disabled(d.answer.is_none())
                            .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
                    ),
            )
    }
}
