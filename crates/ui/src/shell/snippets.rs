//! The Snippets dock: saved commands with a filter and tag chips, an add/edit form, and Run
//! buttons. The workspace does the running (see [`SnippetsEvent`]).
//!
//! Snippets live in a GPUI global shared by every window and are saved off the UI thread,
//! the same way as the config.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use maomi_config::{Snippet, Snippets};

use crate::{Palette, TERMINAL_FAMILY, TerminalView, Theme};

/// What the panel asks the workspace to do.
pub enum SnippetsEvent {
    RunInFocused(String),
    /// Opens the tab picker for this command.
    RunOnTabs(String),
    Collapse,
}

/// Every window's snippets plus the queue their saves go through.
pub(crate) struct SnippetStore {
    snippets: Snippets,
    saves: async_channel::Sender<Snippets>,
}

impl Global for SnippetStore {}

/// Loads `snippets.json` and starts the background saver. Called once from [`super::init`].
pub(crate) fn install(cx: &mut App) {
    let snippets = Snippets::load();
    let (saves, queue) = async_channel::unbounded::<Snippets>();
    cx.background_executor()
        .spawn(async move {
            while let Ok(mut snippets) = queue.recv().await {
                while let Ok(newer) = queue.try_recv() {
                    snippets = newer;
                }
                if let Err(err) = snippets.save() {
                    tracing::warn!(%err, "could not save the snippets");
                }
            }
        })
        .detach();
    cx.set_global(SnippetStore { snippets, saves });
}

fn update_store(cx: &mut App, f: impl FnOnce(&mut Vec<Snippet>)) {
    let store = cx.global_mut::<SnippetStore>();
    f(&mut store.snippets.items);
    _ = store.saves.try_send(store.snippets.clone());
}

/// The add/edit form's fields.
struct Form {
    /// None = a new snippet.
    index: Option<usize>,
    name: Entity<InputState>,
    command: Entity<TextareaState>,
    description: Entity<TextareaState>,
    tag: Entity<InputState>,
    error: Option<SharedString>,
}

pub struct SnippetsPanel {
    filter: Entity<InputState>,
    /// Tag chip in use; None = All.
    tag: Option<String>,
    selected: Option<usize>,
    form: Option<Form>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SnippetsEvent> for SnippetsPanel {}

impl SnippetsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter snippets"));
        let subscriptions = vec![
            cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe_global::<SnippetStore>(|panel, cx| {
                // Another window may have deleted rows.
                let len = cx.global::<SnippetStore>().snippets.items.len();
                if panel.selected.is_some_and(|i| i >= len) {
                    panel.selected = None;
                }
                cx.notify();
            }),
        ];
        Self {
            filter,
            tag: None,
            selected: None,
            form: None,
            _subscriptions: subscriptions,
        }
    }

    fn open_form(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let snippet = index
            .and_then(|i| cx.global::<SnippetStore>().snippets.items.get(i).cloned())
            .unwrap_or_default();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(snippet.name));
        let command = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).rows(4);
            state.set_value(snippet.command, window, cx);
            state
        });
        let description = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).rows(2);
            state.set_value(snippet.description, window, cx);
            state
        });
        let tag = cx.new(|cx| InputState::new(window, cx).default_value(snippet.tag));
        name.update(cx, |input, cx| input.focus(window, cx));
        self.form = Some(Form {
            index,
            name,
            command,
            description,
            tag,
            error: None,
        });
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let snippet = Snippet {
            name: form.name.read(cx).value().trim().to_string(),
            command: form.command.read(cx).value().trim_end().to_string(),
            description: form.description.read(cx).value().trim().to_string(),
            tag: form.tag.read(cx).value().trim().to_string(),
        };
        if snippet.name.is_empty() || snippet.command.trim().is_empty() {
            form.error = Some("A snippet needs a name and a command.".into());
            cx.notify();
            return;
        }
        let index = form.index;
        let mut saved_at = 0;
        update_store(cx, |items| match index.filter(|&i| i < items.len()) {
            Some(i) => {
                items[i] = snippet;
                saved_at = i;
            }
            None => {
                items.push(snippet);
                saved_at = items.len() - 1;
            }
        });
        self.selected = Some(saved_at);
        self.form = None;
        cx.notify();
    }

    fn delete_form_snippet(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.form.as_ref().and_then(|f| f.index) {
            update_store(cx, |items| {
                if index < items.len() {
                    items.remove(index);
                }
            });
        }
        self.selected = None;
        self.form = None;
        cx.notify();
    }

    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let snippets = &cx.global::<SnippetStore>().snippets;
        let query = self.filter.read(cx).value().to_lowercase();
        let tags: Vec<String> = snippets.tags().into_iter().map(Into::into).collect();
        let total = snippets.items.len();
        let visible: Vec<(usize, Snippet)> = snippets
            .items
            .iter()
            .enumerate()
            .filter(|(_, s)| self.tag.as_ref().is_none_or(|t| s.tag.trim() == t))
            .filter(|(_, s)| {
                query.is_empty()
                    || s.name.to_lowercase().contains(&query)
                    || s.command.to_lowercase().contains(&query)
                    || s.description.to_lowercase().contains(&query)
            })
            .map(|(i, s)| (i, s.clone()))
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
            "snip-tag-all".into(),
            "All".into(),
            self.tag.is_none(),
            None,
        )];
        chips.extend(tags.iter().map(|t| {
            chip(
                SharedString::from(format!("snip-tag-{t}")),
                SharedString::from(t.clone()),
                self.tag.as_deref() == Some(t.as_str()),
                Some(t.clone()),
            )
        }));

        let rows: Vec<AnyElement> = visible
            .into_iter()
            .map(|(i, s)| self.render_row(i, s, p, cx))
            .collect();
        let empty = rows.is_empty().then_some(if total == 0 {
            "No snippets yet. Add one with +."
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
                "SNIPPETS",
                p,
                vec![
                    icon_button("snip-add", IconName::Plus, "New snippet", p)
                        .on_click(
                            cx.listener(|panel, _, window, cx| panel.open_form(None, window, cx)),
                        )
                        .into_any_element(),
                    icon_button(
                        "snip-collapse",
                        IconName::PanelLeftClose,
                        "Collapse dock",
                        p,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SnippetsEvent::Collapse)))
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
                    .id("snip-list")
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows)
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
            .child(
                div()
                    .px(px(4.))
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child(format!(
                        "{total} snippet{} \u{b7} stored in snippets.json",
                        if total == 1 { "" } else { "s" }
                    )),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        snippet: Snippet,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(index);
        let hover = p.surface_2.to_hsla();
        let command = snippet.command.clone();
        let tabs_command = snippet.command.clone();
        div()
            .id(("snippet", index))
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
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.selected = Some(index);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .text_color(p.fg_emph.to_hsla())
                            .child(snippet.name.clone()),
                    )
                    .when(!snippet.tag.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(px(7.))
                                .rounded(px(999.))
                                .bg(p.surface_2.to_hsla())
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child(snippet.tag.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .font_family(TERMINAL_FAMILY)
                    .text_size(px(12.))
                    .text_color(p.fg.to_hsla())
                    .when(!selected, |d| d.whitespace_nowrap().text_ellipsis())
                    .child(snippet.command.clone()),
            )
            .when(!snippet.description.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.muted.to_hsla())
                        .child(snippet.description.clone()),
                )
            })
            .when(selected, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pt(px(4.))
                        .child(
                            Button::new(("snip-run", index))
                                .primary()
                                .xsmall()
                                .label("Run in focused pane")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SnippetsEvent::RunInFocused(command.clone()));
                                })),
                        )
                        .child(
                            Button::new(("snip-tabs", index))
                                .xsmall()
                                .label("Run on tabs…")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SnippetsEvent::RunOnTabs(tabs_command.clone()));
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            icon_button(
                                "snip-edit",
                                gpui_kit::assets::IconName::Pencil,
                                "Edit snippet",
                                p,
                            )
                            .on_click(cx.listener(
                                move |panel, _, window, cx| {
                                    panel.open_form(Some(index), window, cx);
                                },
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_form(&self, form: &Form, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, optional: bool, input: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .text_size(px(12.))
                        .text_color(p.fg_emph.to_hsla())
                        .child(label)
                        .when(optional, |d| {
                            d.child(div().text_color(p.muted.to_hsla()).child("(optional)"))
                        }),
                )
                .child(input)
        };
        let title = if form.index.is_some() {
            "Edit snippet"
        } else {
            "New snippet"
        };
        div()
            .id("snip-form")
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
                    Button::new("snip-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .label("Snippets")
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.form = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                ],
            ))
            .child(field(
                "Name",
                false,
                Input::new(&form.name).small().into_any_element(),
            ))
            .child(field(
                "Command",
                false,
                Textarea::new(&form.command)
                    .font_family(TERMINAL_FAMILY)
                    .into_any_element(),
            ))
            .child(field(
                "Description",
                true,
                Textarea::new(&form.description).into_any_element(),
            ))
            .child(field(
                "Tag",
                true,
                Input::new(&form.tag).small().into_any_element(),
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
                            .child("Snippets are saved unencrypted in snippets.json."),
                    )
                    .child(
                        div()
                            .text_color(p.muted.to_hsla())
                            .child("Keep passwords and tokens in the vault."),
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
                        Button::new("snip-save")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|panel, _, _, cx| panel.save_form(cx))),
                    )
                    .child(Button::new("snip-cancel").small().label("Cancel").on_click(
                        cx.listener(|panel, _, _, cx| {
                            panel.form = None;
                            cx.notify();
                        }),
                    ))
                    .child(div().flex_1())
                    .when(form.index.is_some(), |d| {
                        d.child(
                            Button::new("snip-delete")
                                .danger()
                                .xsmall()
                                .label("Delete")
                                .on_click(
                                    cx.listener(|panel, _, _, cx| panel.delete_form_snippet(cx)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for SnippetsPanel {
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

/// One tab offered by the Run on tabs dialog.
pub struct TabChoice {
    pub label: SharedString,
    pub detail: SharedString,
    /// A command is running there, so the snippet would be typed into it.
    pub busy: bool,
    pub checked: bool,
    /// The tab's focused pane.
    pub terminal: WeakEntity<TerminalView>,
}

/// The Run on tabs dialog's contents, buttons included: the dialog footer would not redraw
/// when the ticks change.
struct TabPicker {
    command: String,
    choices: Vec<TabChoice>,
}

/// Asks which tabs to run `command` on, then types it into each one's focused pane.
pub fn open_tab_picker(
    command: String,
    choices: Vec<TabChoice>,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|_| TabPicker { command, choices });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Run on tabs")
            .w(px(460.))
            .child(picker.clone())
    });
}

impl TabPicker {
    fn run(&self, window: &mut Window, cx: &mut Context<Self>) {
        for choice in self.choices.iter().filter(|c| c.checked) {
            if let Some(terminal) = choice.terminal.upgrade() {
                terminal.read(cx).run_command(&self.command);
            }
        }
        window.close_dialog(cx);
    }
}

impl Render for TabPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let count = self.choices.iter().filter(|c| c.checked).count();
        let rows = self.choices.iter().enumerate().map(|(i, choice)| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .when(choice.checked, |d| d.bg(p.surface_2.to_hsla()))
                .child(
                    Checkbox::new(("run-tab", i))
                        .checked(choice.checked)
                        .label(choice.label.clone())
                        .on_click(cx.listener(move |picker, checked: &bool, _, cx| {
                            picker.choices[i].checked = *checked;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .text_color(p.muted.to_hsla())
                        .child(choice.detail.clone())
                        .when(choice.busy, |d| {
                            d.child(
                                div()
                                    .px(px(7.))
                                    .rounded(px(999.))
                                    .bg(p.warn.with_alpha(0x26).to_hsla())
                                    .text_color(p.warn.to_hsla())
                                    .child("busy"),
                            )
                        }),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_color(p.muted.to_hsla())
                    .child("Sends the command to the focused pane of each selected tab."),
            )
            .child(
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(p.surface_2.to_hsla())
                    .font_family(TERMINAL_FAMILY)
                    .text_size(px(12.))
                    .text_color(p.fg_emph.to_hsla())
                    .child(self.command.clone()),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(rows))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        Button::new("run-tabs-all")
                            .ghost()
                            .small()
                            .label("Select all")
                            .on_click(cx.listener(|picker, _, _, cx| {
                                for choice in &mut picker.choices {
                                    choice.checked = true;
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("run-tabs-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("run-tabs-run")
                            .primary()
                            .label(match count {
                                1 => "Run on 1 tab".to_string(),
                                n => format!("Run on {n} tabs"),
                            })
                            .disabled(count == 0)
                            .on_click(cx.listener(|picker, _, window, cx| picker.run(window, cx))),
                    ),
            )
    }
}

/// Section title on the left, small buttons on the right.
pub(super) fn header(title: &'static str, p: &Palette, tools: Vec<AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .px(px(4.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(p.muted.to_hsla())
                .child(title),
        )
        .child(div().flex().items_center().gap(px(2.)).children(tools))
}

pub(super) fn icon_button(
    id: &'static str,
    icon: impl Into<Icon>,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(6.))
        .text_color(p.muted.to_hsla())
        .hover(move |s| s.bg(hover).text_color(emph))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(icon.into().size(px(15.)))
}
