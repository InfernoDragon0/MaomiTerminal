//! The Hosts dock: saved SSH hosts in folders with a filter and tag chips, an editor, and
//! Connect buttons. The workspace opens the panes (see [`HostsEvent`]).
//!
//! Hosts live in a GPUI global shared by every window and are saved off the UI thread. The
//! connection state dots are kept in memory only.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use maomi_config::{Host, HostAuth, Hosts};
use maomi_vault::EntryKind;

use super::snippets::{header, icon_button};
use crate::{Color, Palette, TERMINAL_FAMILY, Theme, vault};

/// What the panel asks the workspace to do, by host id.
pub enum HostsEvent {
    Connect(String),
    ConnectInSplit(String),
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Connected,
    /// The last connection failed.
    Error,
}

pub(crate) struct HostStore {
    hosts: Hosts,
    status: HashMap<String, HostStatus>,
    saves: async_channel::Sender<Hosts>,
}

impl Global for HostStore {}

/// Loads `hosts.json` and starts the background saver. Called once from [`super::init`].
pub(crate) fn install(cx: &mut App) {
    let hosts = Hosts::load();
    let (saves, queue) = async_channel::unbounded::<Hosts>();
    cx.background_executor()
        .spawn(async move {
            while let Ok(mut hosts) = queue.recv().await {
                while let Ok(newer) = queue.try_recv() {
                    hosts = newer;
                }
                if let Err(err) = hosts.save() {
                    tracing::warn!(%err, "could not save the hosts");
                }
            }
        })
        .detach();
    cx.set_global(HostStore {
        hosts,
        status: HashMap::new(),
        saves,
    });
}

pub(crate) fn host(id: &str, cx: &App) -> Option<Host> {
    cx.try_global::<HostStore>()?.hosts.get(id).cloned()
}

pub(crate) fn set_status(id: &str, status: HostStatus, cx: &mut App) {
    if let Some(store) = cx.try_global::<HostStore>()
        && store.status.get(id) == Some(&status)
    {
        return;
    }
    cx.global_mut::<HostStore>()
        .status
        .insert(id.to_owned(), status);
}

/// Clears the dot after a connected session ended.
pub(crate) fn clear_status(id: &str, cx: &mut App) {
    if cx
        .try_global::<HostStore>()
        .is_some_and(|s| s.status.get(id) == Some(&HostStatus::Connected))
    {
        cx.global_mut::<HostStore>().status.remove(id);
    }
}

/// How many hosts log in with the vault key `key_id`.
pub(crate) fn hosts_using_key(key_id: &str, cx: &App) -> usize {
    cx.try_global::<HostStore>().map_or(0, |s| {
        s.hosts
            .items
            .iter()
            .filter(|h| h.key_id.as_deref() == Some(key_id))
            .count()
    })
}

/// Unsets a deleted key on every host that used it.
pub(crate) fn forget_key(key_id: &str, cx: &mut App) {
    if hosts_using_key(key_id, cx) == 0 {
        return;
    }
    update_hosts(cx, |items| {
        for host in items.iter_mut() {
            if host.key_id.as_deref() == Some(key_id) {
                host.key_id = None;
            }
        }
    });
}

/// Changes one host and saves.
pub(crate) fn update_host(id: &str, f: impl FnOnce(&mut Host), cx: &mut App) {
    update_hosts(cx, |items| {
        if let Some(host) = items.iter_mut().find(|h| h.id == id) {
            f(host);
        }
    });
}

fn update_hosts(cx: &mut App, f: impl FnOnce(&mut Vec<Host>)) {
    let store = cx.global_mut::<HostStore>();
    f(&mut store.hosts.items);
    _ = store.saves.try_send(store.hosts.clone());
}

/// The editor's fields.
struct Form {
    /// None = a new host.
    id: Option<String>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    folder: Entity<InputState>,
    tags: Entity<InputState>,
    auth: HostAuth,
    key_id: Option<String>,
    password_saved: bool,
    error: Option<SharedString>,
}

pub struct HostsPanel {
    filter: Entity<InputState>,
    tag: Option<String>,
    selected: Option<String>,
    collapsed: HashSet<String>,
    form: Option<Form>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HostsEvent> for HostsPanel {}

impl HostsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter hosts"));
        let subscriptions = vec![
            cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe_global::<HostStore>(|_, cx| cx.notify()),
            // The key list in the editor follows the vault's lock state.
            cx.observe_global::<vault::VaultState>(|_, cx| cx.notify()),
        ];
        Self {
            filter,
            tag: None,
            selected: None,
            collapsed: HashSet::new(),
            form: None,
            _subscriptions: subscriptions,
        }
    }

    fn open_form(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let host = id
            .as_deref()
            .and_then(|id| host(id, cx))
            .unwrap_or_default();
        let mut input = |value: String, placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let name = input(host.name.clone(), "fennec", cx);
        let address = input(host.host.clone(), "fennec.lan or 192.168.1.10", cx);
        let port = input(host.port.to_string(), "22", cx);
        let user = input(host.user.clone(), "kit", cx);
        let folder = input(host.folder.clone(), "Home lab", cx);
        let tags = input(host.tags.join(", "), "lab, prod", cx);
        name.update(cx, |input, cx| input.focus(window, cx));
        self.form = Some(Form {
            id,
            name,
            host: address,
            port,
            user,
            folder,
            tags,
            auth: host.auth,
            key_id: host.key_id,
            password_saved: host.password_saved,
            error: None,
        });
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let address = text(&form.host);
        let port = text(&form.port).parse::<u16>().ok().filter(|p| *p > 0);
        let (Some(port), false) = (port, address.is_empty()) else {
            form.error = Some("A host needs an address and a port from 1 to 65535.".into());
            cx.notify();
            return;
        };
        if form.auth == HostAuth::Key && form.key_id.is_none() {
            form.error = Some("Pick a key, or switch to Password.".into());
            cx.notify();
            return;
        }
        let edited = Host {
            id: form.id.clone().unwrap_or_else(Host::new_id),
            name: text(&form.name),
            host: address,
            port,
            user: text(&form.user),
            auth: form.auth,
            key_id: form.key_id.clone(),
            password_saved: form.password_saved,
            folder: text(&form.folder),
            tags: text(&form.tags)
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect(),
        };
        let id = edited.id.clone();
        update_hosts(cx, |items| match items.iter_mut().find(|h| h.id == id) {
            Some(host) => *host = edited,
            None => items.push(edited),
        });
        self.selected = Some(id);
        self.form = None;
        cx.notify();
    }

    fn delete_form_host(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.form.as_ref().and_then(|f| f.id.clone()) else {
            return;
        };
        if let Some(host) = host(&id, cx)
            && host.password_saved
        {
            // Only possible while the vault is open; a locked vault keeps the entry.
            vault::with_vault(cx, |v| v.remove(&host.password_entry()));
        }
        update_hosts(cx, |items| items.retain(|h| h.id != id));
        self.selected = None;
        self.form = None;
        cx.notify();
    }

    /// Drops the saved password from the vault (unlocking it first).
    fn forget_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        form.password_saved = false;
        cx.notify();
        let Some(id) = form.id.clone() else {
            return;
        };
        vault::unlock(window, cx, move |_, cx| {
            if let Some(host) = host(&id, cx) {
                vault::with_vault(cx, |v| v.remove(&host.password_entry()));
            }
            update_host(&id, |h| h.password_saved = false, cx);
        });
    }

    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = cx.global::<HostStore>();
        let query = self.filter.read(cx).value().to_lowercase();
        let tags: Vec<String> = store.hosts.tags().into_iter().map(Into::into).collect();
        let folders: Vec<String> = store.hosts.folders().into_iter().map(Into::into).collect();
        let total = store.hosts.items.len();
        let visible: Vec<(Host, Option<HostStatus>)> = store
            .hosts
            .items
            .iter()
            .filter(|h| {
                self.tag
                    .as_ref()
                    .is_none_or(|t| h.tags.iter().any(|ht| ht.trim() == t))
            })
            .filter(|h| {
                query.is_empty()
                    || h.name.to_lowercase().contains(&query)
                    || h.address().to_lowercase().contains(&query)
                    || h.folder.to_lowercase().contains(&query)
            })
            .map(|h| (h.clone(), store.status.get(&h.id).copied()))
            .collect();

        let chip = |id: SharedString, label: SharedString, on: bool, tag: Option<String>| {
            let emph = p.fg_emph.to_hsla();
            div()
                .id(id)
                .px(px(10.))
                .py(px(3.))
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
                .on_click(cx.listener(move |panel, _, _, cx| {
                    panel.tag = tag.clone();
                    cx.notify();
                }))
                .child(label)
        };
        let mut chips = vec![chip(
            "host-tag-all".into(),
            "All".into(),
            self.tag.is_none(),
            None,
        )];
        chips.extend(tags.iter().map(|t| {
            chip(
                SharedString::from(format!("host-tag-{t}")),
                SharedString::from(t.clone()),
                self.tag.as_deref() == Some(t.as_str()),
                Some(t.clone()),
            )
        }));

        let mut tree: Vec<AnyElement> = Vec::new();
        for folder in &folders {
            let hosts: Vec<&(Host, Option<HostStatus>)> = visible
                .iter()
                .filter(|(h, _)| h.folder.trim() == folder)
                .collect();
            if hosts.is_empty() {
                continue;
            }
            let collapsed = self.collapsed.contains(folder);
            if !folder.is_empty() {
                tree.push(self.render_folder(folder, hosts.len(), collapsed, p, cx));
            }
            if !collapsed {
                tree.extend(
                    hosts
                        .into_iter()
                        .map(|(h, status)| self.render_row(h, *status, p, cx)),
                );
            }
        }
        let empty = tree.is_empty().then_some(if total == 0 {
            "No hosts yet. Add one with +."
        } else {
            "Nothing matches the filter."
        });

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .size_full()
            .min_h_0()
            .child(header(
                "HOSTS",
                p,
                vec![
                    icon_button("host-add", IconName::Plus, "New host", p)
                        .on_click(
                            cx.listener(|panel, _, window, cx| panel.open_form(None, window, cx)),
                        )
                        .into_any_element(),
                    icon_button(
                        "host-collapse",
                        IconName::PanelLeftClose,
                        "Collapse dock",
                        p,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HostsEvent::Collapse)))
                    .into_any_element(),
                ],
            ))
            .child(Input::new(&self.filter).small().cleanable(true))
            .when(!tags.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(2.))
                        .p(px(3.))
                        .rounded(px(9.))
                        .bg(p.surface_2.to_hsla())
                        .children(chips),
                )
            })
            .child(
                div()
                    .id("host-tree")
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(tree)
                    .when_some(empty, |d, text| {
                        d.child(
                            div()
                                .px(px(4.))
                                .py(px(8.))
                                .text_size(px(12.))
                                .text_color(p.muted.to_hsla())
                                .child(text),
                        )
                    }),
            )
            .child(legend(total, p))
            .into_any_element()
    }

    fn render_folder(
        &self,
        folder: &str,
        count: usize,
        collapsed: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = folder.to_owned();
        div()
            .id(SharedString::from(format!("host-folder-{folder}")))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(4.))
            .pt(px(6.))
            .text_size(px(12.))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(p.muted.to_hsla())
            .on_click(cx.listener(move |panel, _, _, cx| {
                if !panel.collapsed.remove(&key) {
                    panel.collapsed.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size_3p5(),
            )
            .child(div().flex_1().child(folder.to_owned()))
            .child(count.to_string())
            .into_any_element()
    }

    fn render_row(
        &self,
        host: &Host,
        status: Option<HostStatus>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(host.id.as_str());
        let hover = p.surface_2.to_hsla();
        let id = host.id.clone();
        let (connect_id, split_id, edit_id) = (id.clone(), id.clone(), id.clone());
        let name = if host.name.is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };
        div()
            .id(SharedString::from(format!("host-{id}")))
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(9.))
            .border_1()
            .map(|d| {
                if selected {
                    d.bg(p.surface.to_hsla())
                        .border_color(p.line_strong.to_hsla())
                } else {
                    d.border_color(gpui_kit::transparent_black())
                        .hover(move |s| s.bg(hover))
                }
            })
            .on_click(
                cx.listener(move |panel, event: &gpui_kit::ClickEvent, _, cx| {
                    // A double-click connects straight away.
                    if event.click_count() >= 2 {
                        cx.emit(HostsEvent::Connect(id.clone()));
                    }
                    panel.selected = Some(id.clone());
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .size(px(8.))
                            .rounded_full()
                            .bg(status_color(status, p).to_hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .text_color(p.fg_emph.to_hsla())
                            .child(name),
                    )
                    .children(host.tags.first().map(|tag| {
                        div()
                            .flex_none()
                            .px(px(7.))
                            .rounded(px(999.))
                            .bg(p.surface_2.to_hsla())
                            .text_size(px(11.))
                            .text_color(p.muted.to_hsla())
                            .child(tag.clone())
                    })),
            )
            .child(
                div()
                    .font_family(TERMINAL_FAMILY)
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(host.address()),
            )
            .when(selected, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pt(px(4.))
                        .child(
                            Button::new("host-connect")
                                .primary()
                                .xsmall()
                                .label("Connect")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(HostsEvent::Connect(connect_id.clone()));
                                })),
                        )
                        .child(
                            Button::new("host-split")
                                .xsmall()
                                .label("Connect in split")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(HostsEvent::ConnectInSplit(split_id.clone()));
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            icon_button(
                                "host-edit",
                                gpui_kit::assets::IconName::Pencil,
                                "Edit host",
                                p,
                            )
                            .on_click(cx.listener(
                                move |panel, _, window, cx| {
                                    panel.open_form(Some(edit_id.clone()), window, cx);
                                },
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_form(&self, form: &Form, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, input: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .flex_1()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.fg_emph.to_hsla())
                        .child(label),
                )
                .child(input)
        };
        let auth_seg =
            [(HostAuth::Password, "Password"), (HostAuth::Key, "Key")].map(|(auth, label)| {
                let on = form.auth == auth;
                let emph = p.fg_emph.to_hsla();
                div()
                    .id(SharedString::from(format!("host-auth-{label}")))
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
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if let Some(form) = &mut panel.form {
                            form.auth = auth;
                            cx.notify();
                        }
                    }))
                    .child(label)
            });
        let auth_detail = match form.auth {
            HostAuth::Password => self.render_password_row(form, p, cx),
            HostAuth::Key => self.render_key_picker(form, p, cx),
        };
        let title = if form.id.is_some() {
            "EDIT HOST"
        } else {
            "NEW HOST"
        };
        div()
            .id("host-form")
            .flex()
            .flex_col()
            .gap(px(12.))
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .child(header(
                title,
                p,
                vec![
                    Button::new("host-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .label("Hosts")
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.form = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                ],
            ))
            .child(field(
                "Name",
                Input::new(&form.name).small().into_any_element(),
            ))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(field(
                        "Host",
                        Input::new(&form.host).small().into_any_element(),
                    ))
                    .child(div().w(px(72.)).child(field(
                        "Port",
                        Input::new(&form.port).small().into_any_element(),
                    ))),
            )
            .child(field(
                "Username",
                Input::new(&form.user).small().into_any_element(),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.fg_emph.to_hsla())
                            .child("Authentication"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .p(px(3.))
                            .rounded(px(9.))
                            .bg(p.surface_2.to_hsla())
                            .children(auth_seg),
                    )
                    .child(auth_detail),
            )
            .child(field(
                "Folder",
                Input::new(&form.folder).small().into_any_element(),
            ))
            .child(field(
                "Tags (comma-separated)",
                Input::new(&form.tags).small().into_any_element(),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.line.to_hsla())
                    .bg(p.surface.to_hsla())
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(p.fg_emph.to_hsla())
                            .child(Icon::new(IconName::Info).size_3p5())
                            .child("Hosts are saved unencrypted in hosts.json."),
                    )
                    .child(
                        div()
                            .text_color(p.muted.to_hsla())
                            .child("Passwords and keys live in the vault, never in this file."),
                    ),
            )
            .when_some(form.error.clone(), |d, error| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.err.to_hsla())
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        Button::new("host-save")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|panel, _, _, cx| panel.save_form(cx))),
                    )
                    .child(Button::new("host-cancel").small().label("Cancel").on_click(
                        cx.listener(|panel, _, _, cx| {
                            panel.form = None;
                            cx.notify();
                        }),
                    ))
                    .child(div().flex_1())
                    .when(form.id.is_some(), |d| {
                        d.child(
                            Button::new("host-delete")
                                .danger()
                                .xsmall()
                                .label("Delete")
                                .on_click(
                                    cx.listener(|panel, _, _, cx| panel.delete_form_host(cx)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_password_row(&self, form: &Form, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(p.muted.to_hsla());
        if form.password_saved {
            row.child("Password saved in vault")
                .child(
                    Button::new("host-forget-password")
                        .ghost()
                        .xsmall()
                        .label("Forget")
                        .on_click(
                            cx.listener(|panel, _, window, cx| panel.forget_password(window, cx)),
                        ),
                )
                .into_any_element()
        } else {
            row.child("Asked when you connect. You can save it in the vault then.")
                .into_any_element()
        }
    }

    fn render_key_picker(&self, form: &Form, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let keys: Option<Vec<(String, String)>> = vault::with_vault(cx, |v| {
            v.entries()
                .filter(|(_, e)| e.kind == EntryKind::PrivateKey)
                .map(|(id, e)| (id.to_owned(), e.label.clone()))
                .collect()
        });
        let hint = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(p.muted.to_hsla())
                .child(text)
        };
        let Some(keys) = keys else {
            return div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(hint("The vault is locked."))
                .child(
                    Button::new("host-unlock-keys")
                        .xsmall()
                        .label("Unlock to pick a key")
                        .on_click(|_, window, cx| vault::unlock(window, cx, |_, _| {})),
                )
                .into_any_element();
        };
        if keys.is_empty() {
            return hint("No keys in the vault yet. Add one in Settings > SSH keys.")
                .into_any_element();
        }
        let hover = p.surface_2.to_hsla();
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(keys.into_iter().map(|(id, label)| {
                let on = form.key_id.as_deref() == Some(id.as_str());
                let pick = id.clone();
                div()
                    .id(SharedString::from(format!("host-key-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(7.))
                    .text_size(px(12.))
                    .map(|d| {
                        if on {
                            d.bg(p.surface.to_hsla()).text_color(p.fg_emph.to_hsla())
                        } else {
                            d.text_color(p.fg.to_hsla()).hover(move |s| s.bg(hover))
                        }
                    })
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if let Some(form) = &mut panel.form {
                            form.key_id = Some(pick.clone());
                            cx.notify();
                        }
                    }))
                    .child(
                        if on {
                            Icon::new(IconName::CircleCheck)
                        } else {
                            Icon::new(gpui_kit::assets::IconName::Circle)
                        }
                        .size_3p5(),
                    )
                    .child(label)
            }))
            .into_any_element()
    }
}

impl Render for HostsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let body = match &self.form {
            Some(form) => self.render_form(form, p, cx),
            None => self.render_list(p, cx),
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(300.))
            .h_full()
            .px(px(10.))
            .py(px(12.))
            .overflow_hidden()
            .bg(p.bg.to_hsla())
            .border_r_1()
            .border_color(p.line.to_hsla())
            .child(body)
    }
}

fn status_color(status: Option<HostStatus>, p: &Palette) -> Color {
    match status {
        Some(HostStatus::Connected) => p.ok,
        Some(HostStatus::Error) => p.err,
        None => p.line_strong,
    }
}

/// Dot legend and the host count under the tree.
fn legend(total: usize, p: &Palette) -> impl IntoElement {
    let item = |label: &'static str, color: Color| {
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(div().size(px(6.)).rounded_full().bg(color.to_hsla()))
            .child(label)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .px(px(4.))
        .text_size(px(11.))
        .text_color(p.muted.to_hsla())
        .child(
            div()
                .flex()
                .gap(px(10.))
                .child(item("connected", p.ok))
                .child(item("idle", p.line_strong))
                .child(item("error", p.err)),
        )
        .child(format!(
            "{total} host{} \u{b7} stored in hosts.json",
            if total == 1 { "" } else { "s" }
        ))
}
