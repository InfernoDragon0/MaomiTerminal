//! The card drawer: one card of a Workbench plan, floating over the centre (terminals or a board).
//! Same layout as the Workbench's CardDrawer: a header tinted in the column colour (the column
//! pill doubles as the Move to… picker), the title, one meta line and a progress edge, then the
//! Overview, Tasks, Notes and Activity tabs. Moves, task ticks and review answers go to the board
//! server as the user (see [`edit_card`]); everything else is read-only here.
//!
//! The card is read from the store each frame, so the drawer follows live edits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, linear_color_stop, linear_gradient, px,
};
use maomi_workbench::{Activity, Card, Column, Plan, ProjectSettings};

use super::board::{ago, avatars, chip, dot, model_tag, muted_text, note_box, person, size_tag};
use super::wlo;
use super::workbench::{
    CardEdit, WorkbenchStore, answer_question, column_color, column_label, edit_card, load_board,
    percent, reload_handoff, slot_color,
};
use crate::{Palette, Theme};

/// Asks the workspace to close the drawer.
pub struct CloseDrawer;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Tasks,
    Notes,
    Activity,
}

pub struct CardDrawer {
    target: Option<(PathBuf, String)>,
    tab: Tab,
    /// Send back / approve message for a card waiting in Review.
    feedback: Entity<TextareaState>,
    /// An edit is on its way to the board server; the controls wait for it.
    busy: bool,
    column_menu: bool,
    files_open: bool,
    /// The card's activity log lines, newest first; read when the Activity tab opens.
    activity: Vec<Activity>,
    /// Instead of a card: the plan's whole "where we left off".
    handoff: Option<PathBuf>,
    /// One answer box per question, made when it first shows.
    answers: HashMap<String, Entity<InputState>>,
    answer_subs: Vec<Subscription>,
    /// The answered question whose box is open again (one at a time).
    editing: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDrawer> for CardDrawer {}

impl CardDrawer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            target: None,
            tab: Tab::Overview,
            feedback: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(2)
                    .placeholder("Feedback for the agent (optional)")
            }),
            busy: false,
            column_menu: false,
            files_open: false,
            activity: Vec::new(),
            handoff: None,
            answers: HashMap::new(),
            answer_subs: Vec::new(),
            editing: None,
            _subscriptions: vec![cx.observe_global::<WorkbenchStore>(|drawer, cx| {
                if drawer.is_open() {
                    cx.notify();
                }
            })],
        }
    }

    pub fn show(&mut self, plan: PathBuf, card: String, cx: &mut Context<Self>) {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        if self.card() != Some(card.as_str()) {
            self.tab = Tab::Overview;
            self.column_menu = false;
            self.files_open = false;
            self.activity.clear();
        }
        self.target = Some((plan, card));
        self.handoff = None;
        cx.notify();
    }

    /// Shows the plan's "where we left off" instead of a card.
    pub fn show_handoff(&mut self, plan: PathBuf, cx: &mut Context<Self>) {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        self.target = None;
        self.column_menu = false;
        if self.handoff.as_ref() != Some(&plan) {
            self.answers.clear();
            self.answer_subs.clear();
            self.editing = None;
        }
        self.handoff = Some(plan);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.target = None;
        self.handoff = None;
        self.column_menu = false;
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.target.is_some() || self.handoff.is_some()
    }

    pub fn card(&self) -> Option<&str> {
        self.target.as_ref().map(|(_, card)| card.as_str())
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == Tab::Activity {
            self.load_activity(cx);
        }
        cx.notify();
    }

    fn load_activity(&mut self, cx: &mut Context<Self>) {
        let Some((plan, card)) = self.target.clone() else {
            return;
        };
        let read = cx.background_executor().spawn(async move {
            let path = maomi_workbench::activity_path(&plan);
            let mut lines: Vec<Activity> = maomi_workbench::read_activity(&path, 400)
                .into_iter()
                .filter(|a| a.milestone_id.as_deref() == Some(card.as_str()))
                .collect();
            lines.reverse();
            lines
        });
        cx.spawn(async move |this, cx| {
            let lines = read.await;
            _ = this.update(cx, |drawer, cx| {
                drawer.activity = lines;
                cx.notify();
            });
        })
        .detach();
    }

    /// Sends `edit` to the board server; the store refreshes from the watcher once it lands.
    fn edit(&mut self, edit: CardEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some((plan, card)) = self.target.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.column_menu = false;
        let task = edit_card(&plan, &card, edit, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |drawer, window, cx| {
                drawer.busy = false;
                match result {
                    // The card itself shows the change; a toast would cover the drawer header.
                    Ok(()) => drawer
                        .feedback
                        .update(cx, |f, cx| f.set_value("", window, cx)),
                    Err(err) => window.push_notification(Notification::error(err), cx),
                }
                if drawer.tab == Tab::Activity {
                    drawer.load_activity(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_header(
        &self,
        plan: &Plan,
        card: &Card,
        color: Hsla,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pct = percent(card.progress());
        let done = card.tasks.iter().filter(|t| t.done).count();
        let working =
            card.column == Column::InProgress && card.assignees.iter().any(|a| a != "user");
        let mut meta: Vec<AnyElement> = Vec::new();
        let strong = |text: String, rest: &'static str| {
            div()
                .flex()
                .gap(px(3.))
                .child(
                    div()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(p.fg.to_hsla())
                        .child(text),
                )
                .child(rest)
                .into_any_element()
        };
        if !card.tasks.is_empty() {
            meta.push(strong(format!("{done}/{}", card.tasks.len()), "tasks"));
        }
        meta.push(strong(format!("{pct}%"), ""));
        if card.updated_at > 0 {
            meta.push(
                div()
                    .child(format!("updated {}", ago(card.updated_at)))
                    .into_any_element(),
            );
        }
        if working {
            let who = card
                .assignees
                .iter()
                .find(|a| *a != "user")
                .map(|a| person(a));
            meta.push(
                div()
                    .text_color(p.fg_emph.to_hsla())
                    .child(format!("{} working", who.unwrap_or_default()))
                    .into_any_element(),
            );
        }
        let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
        let icon_button = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded(px(7.))
                .text_color(p.muted.to_hsla())
                .hover(move |s| s.bg(hover).text_color(emph))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(Icon::new(icon).size(px(15.)))
        };
        let short_ref = card.id.strip_prefix("m-").unwrap_or(&card.id).to_owned();
        let _ = plan;

        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .pt(px(12.))
            .pb(px(14.))
            .rounded_t(px(12.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(color.opacity(0.13), 0.),
                linear_color_stop(color.opacity(0.), 1.),
            ))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(self.column_pill(card, color, p, cx))
                    .child(
                        div()
                            .font_family(crate::TERMINAL_FAMILY)
                            .text_size(px(11.5))
                            .text_color(p.muted.to_hsla())
                            .child(format!("#{short_ref}")),
                    )
                    .child(div().flex_1())
                    .child(
                        icon_button("wb-drawer-close", IconName::Close, "Close").on_click(
                            cx.listener(|drawer, _, _, cx| {
                                drawer.close(cx);
                                cx.emit(CloseDrawer);
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .line_height(px(21.))
                    .text_color(p.fg_emph.to_hsla())
                    .child(card.title.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x(px(12.))
                    .gap_y(px(4.))
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .children(meta),
            )
            // Progress as a 3px edge along the header's bottom.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(-1.))
                    .h(px(3.))
                    .bg(p.surface_3.to_hsla())
                    .child(
                        div()
                            .h_full()
                            .w(gpui_kit::relative(pct.min(100) as f32 / 100.))
                            .bg(color),
                    ),
            )
            .into_any_element()
    }

    /// The column pill; a click opens the Move to… list under it.
    fn column_pill(
        &self,
        card: &Card,
        color: Hsla,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.busy;
        let pill = div()
            .id("wb-drawer-column")
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(9.))
            .py(px(2.))
            .rounded(px(99.))
            .border_1()
            .border_color(color.opacity(0.45))
            .bg(color.opacity(0.16))
            .text_size(px(11.5))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(p.fg_emph.to_hsla())
            .cursor_pointer()
            .tooltip(|window, cx| Tooltip::new("Move to another column").build(window, cx))
            .on_click(cx.listener(move |drawer, _, _, cx| {
                if !busy {
                    drawer.column_menu = !drawer.column_menu;
                    cx.notify();
                }
            }))
            .child(dot(color, 7.))
            .child(column_label(&card.column).to_owned())
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(11.))
                    .text_color(p.muted.to_hsla()),
            );
        let menu = self.column_menu.then(|| {
            let hover = p.surface_3.to_hsla();
            let rows: Vec<AnyElement> = Column::ALL
                .iter()
                .map(|column| {
                    let here = *column == card.column;
                    let target = column.clone();
                    div()
                        .id(SharedString::from(format!("wb-move-{}", column.as_str())))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(7.))
                        .text_size(px(12.5))
                        .map(|d| {
                            if here {
                                d.text_color(p.muted.to_hsla())
                            } else {
                                d.text_color(p.fg.to_hsla())
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .on_click(cx.listener(move |drawer, _, window, cx| {
                                        drawer.edit(
                                            CardEdit::Move {
                                                column: target.clone(),
                                                index: None,
                                            },
                                            window,
                                            cx,
                                        );
                                    }))
                            }
                        })
                        .child(dot(column_color(column, p), 7.))
                        .child(if here {
                            format!("{} (here)", column_label(column))
                        } else {
                            column_label(column).to_owned()
                        })
                        .into_any_element()
                })
                .collect();
            div()
                .absolute()
                .top(px(26.))
                .left_0()
                .w(px(180.))
                .p(px(4.))
                .flex()
                .flex_col()
                .rounded(px(10.))
                .border_1()
                .border_color(p.line_strong.to_hsla())
                .bg(p.surface_2.to_hsla())
                .shadow_lg()
                .child(
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .text_size(px(11.))
                        .text_color(p.muted.to_hsla())
                        .child("Move to"),
                )
                .children(rows)
        });
        div()
            .relative()
            .child(pill)
            .children(menu.map(|menu| gpui_kit::deferred(menu).with_priority(1)))
            .into_any_element()
    }

    fn render_tabs(&self, card: &Card, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let done = card.tasks.iter().filter(|t| t.done).count();
        let tabs = [
            (Tab::Overview, "Overview", None),
            (
                Tab::Tasks,
                "Tasks",
                (!card.tasks.is_empty()).then(|| format!("{done}/{}", card.tasks.len())),
            ),
            (
                Tab::Notes,
                "Notes",
                (!card.notes.is_empty()).then(|| card.notes.len().to_string()),
            ),
            (Tab::Activity, "Activity", None),
        ];
        let emph = p.fg_emph.to_hsla();
        div()
            .flex()
            .gap(px(2.))
            .px(px(10.))
            .pt(px(8.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .children(tabs.into_iter().map(|(tab, label, badge)| {
                let on = self.tab == tab;
                div()
                    .id(SharedString::from(format!("wb-tab-{label}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .pt(px(6.))
                    .pb(px(7.))
                    .border_b_2()
                    .text_size(px(12.5))
                    .map(|d| {
                        if on {
                            d.border_color(p.accent.to_hsla()).text_color(emph)
                        } else {
                            d.border_color(gpui_kit::transparent_black())
                                .text_color(p.muted.to_hsla())
                                .cursor_pointer()
                                .hover(move |s| s.text_color(emph))
                        }
                    })
                    .on_click(cx.listener(move |drawer, _, _, cx| drawer.select_tab(tab, cx)))
                    .child(label)
                    .when_some(badge, |d, badge| {
                        d.child(
                            div()
                                .px(px(6.))
                                .rounded(px(99.))
                                .bg(p.surface_2.to_hsla())
                                .text_size(px(10.5))
                                .text_color(p.muted.to_hsla())
                                .child(badge),
                        )
                    })
            }))
            .into_any_element()
    }

    fn render_overview(
        &self,
        plan: &Plan,
        card: &Card,
        settings: &ProjectSettings,
        path: &Path,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut body = Vec::new();
        if card.column == Column::Review
            && let Some(review) = &card.review
        {
            body.push(self.render_review(card, review, p, cx));
        }
        let mono = crate::TERMINAL_FAMILY;
        body.push(section(
            "Verify",
            match &card.verify {
                Some(cmd) => div()
                    .flex()
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .bg(p.surface_2.to_hsla())
                            .font_family(mono)
                            .text_size(px(12.))
                            .text_color(p.fg_emph.to_hsla())
                            .child(cmd.clone()),
                    )
                    .into_any_element(),
                None => muted_text("No verify command.", p),
            },
            p,
        ));
        body.push(section(
            "Description",
            if card.description.trim().is_empty() {
                muted_text("No description", p)
            } else {
                div()
                    .text_size(px(12.5))
                    .child(TextView::markdown(
                        SharedString::from(format!("wb-desc-{}", card.id)),
                        card.description.clone(),
                    ))
                    .into_any_element()
            },
            p,
        ));

        let card_type = settings.card_type(card.type_id.as_deref());
        let row = |label: &'static str, value: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .text_size(px(12.5))
                .child(
                    div()
                        .flex_none()
                        .w(px(110.))
                        .text_color(p.muted.to_hsla())
                        .child(label),
                )
                .child(div().flex().items_center().gap(px(6.)).child(value))
                .into_any_element()
        };
        let mut props = vec![row(
            "Type",
            match card_type {
                Some(t) => {
                    let color = slot_color(&t.color, p).unwrap_or(p.muted.to_hsla());
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .text_color(color)
                        .child(div().size(px(7.)).rounded(px(2.)).bg(color))
                        .child(t.label.clone())
                        .into_any_element()
                }
                None => muted_text("none", p),
            },
        )];
        if let Some(size) = &card.size {
            props.push(row("Size", size_tag(size.as_str(), p).into_any_element()));
        }
        if let Some(agent) = &card.agent {
            props.push(row(
                "Suggested agent",
                model_tag(&agent.model, p).into_any_element(),
            ));
        }
        props.push(row(
            "Assignees",
            if card.assignees.is_empty() {
                muted_text("none", p)
            } else {
                avatars(&card.assignees, p).into_any_element()
            },
        ));
        if let Some(weight) = card.weight.filter(|w| *w > 0.) {
            props.push(row(
                "Weight",
                div().child(format!("{weight}%")).into_any_element(),
            ));
        }
        if card.gate {
            props.push(row(
                "Sign-off gate",
                div().child("You sign it off").into_any_element(),
            ));
        }
        props.push(
            div()
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child("Properties are read-only here for now.")
                .into_any_element(),
        );
        body.push(section(
            "Properties",
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(props)
                .into_any_element(),
            p,
        ));

        if !card.depends_on.is_empty() {
            let deps: Vec<AnyElement> = card
                .depends_on
                .iter()
                .enumerate()
                .map(|(i, dep)| {
                    let other = plan.cards.iter().find(|c| &c.id == dep);
                    let (title, color) = match other {
                        Some(c) => (c.title.clone(), column_color(&c.column, p)),
                        None => (dep.clone(), p.muted.to_hsla()),
                    };
                    let (hover, path, dep) = (p.surface_3.to_hsla(), path.to_owned(), dep.clone());
                    div()
                        .id(("wb-dep", i))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(p.surface_2.to_hsla())
                        .text_size(px(12.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |drawer, _, _, cx| {
                            drawer.show(path.clone(), dep.clone(), cx)
                        }))
                        .child(dot(color, 8.))
                        .child(title)
                        .into_any_element()
                })
                .collect();
            body.push(section(
                "Depends on",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(deps)
                    .into_any_element(),
                p,
            ));
        }
        if !card.files.is_empty() {
            let open = self.files_open;
            let emph = p.fg_emph.to_hsla();
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .mt(px(16.))
                    .child(
                        div()
                            .id("wb-files")
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(p.muted.to_hsla())
                            .cursor_pointer()
                            .hover(move |s| s.text_color(emph))
                            .on_click(cx.listener(|drawer, _, _, cx| {
                                drawer.files_open = !drawer.files_open;
                                cx.notify();
                            }))
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(px(12.)),
                            )
                            .child(format!("Files it touches ({})", card.files.len())),
                    )
                    .when(open, |d| {
                        d.children(card.files.iter().map(|f| {
                            div()
                                .pl(px(16.))
                                .font_family(mono)
                                .text_size(px(12.))
                                .text_color(p.fg_emph.to_hsla())
                                .child(f.clone())
                        }))
                    })
                    .into_any_element(),
            );
        }
        body
    }

    /// Approve / Send back for a card waiting on the user.
    fn render_review(
        &self,
        card: &Card,
        review: &maomi_workbench::ReviewRequest,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let verify = card
            .verify_result
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(|ok| ok.as_bool());
        let verify_chip = match (card.verify.is_some(), verify) {
            (true, Some(true)) => {
                chip("Verify passed".to_owned(), p.ok.to_hsla(), p).into_any_element()
            }
            (true, Some(false)) => {
                chip("Verify failed".to_owned(), p.err.to_hsla(), p).into_any_element()
            }
            (true, None) => {
                chip("Verify not run".to_owned(), p.muted.to_hsla(), p).into_any_element()
            }
            (false, _) => {
                chip("No verify on this card".to_owned(), p.muted.to_hsla(), p).into_any_element()
            }
        };
        let busy = self.busy;
        let warn = p.warn.to_hsla();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .mt(px(14.))
            .p(px(12.))
            .rounded(px(10.))
            .border_1()
            .border_color(warn.opacity(0.35))
            .bg(warn.opacity(0.07))
            .child(section_title("Review", p))
            .when_some(review.reason.clone(), |d, reason| {
                d.child(div().text_size(px(12.5)).child(reason))
            })
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .child(format!(
                        "Asked by {} \u{b7} {}",
                        person(&review.by),
                        ago(review.at)
                    )),
            )
            .child(div().flex().child(verify_chip))
            .child(Textarea::new(&self.feedback))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        Button::new("wb-review-approve")
                            .primary()
                            .small()
                            .label("Approve")
                            .disabled(busy)
                            .on_click(cx.listener(|drawer, _, window, cx| {
                                let feedback = drawer.feedback_text(cx);
                                drawer.edit(
                                    CardEdit::Review {
                                        approve: true,
                                        feedback,
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("wb-review-back")
                            .small()
                            .label("Send back")
                            .disabled(busy)
                            .on_click(cx.listener(|drawer, _, window, cx| {
                                let feedback = drawer.feedback_text(cx);
                                drawer.edit(
                                    CardEdit::Review {
                                        approve: false,
                                        feedback,
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child(
                        "Approve moves it to Done; Send back to In progress with your feedback.",
                    ),
            )
            .into_any_element()
    }

    fn feedback_text(&self, cx: &Context<Self>) -> Option<String> {
        let text = self.feedback.read(cx).value().trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Tasks tick and untick with a click (approval tasks too: the person is the one approving).
    fn render_tasks(&self, card: &Card, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if card.tasks.is_empty() {
            return vec![
                div()
                    .mt(px(12.))
                    .child(muted_text("No tasks", p))
                    .into_any_element(),
            ];
        }
        let hover = p.surface_2.to_hsla();
        let busy = self.busy;
        let rows = card.tasks.iter().enumerate().map(|(i, task)| {
            let (id, done, approval) = (task.id.clone(), task.done, task.approval);
            div()
                .id(("wb-task", i))
                .flex()
                .items_start()
                .gap(px(9.))
                .px(px(6.))
                .py(px(6.))
                .rounded(px(7.))
                .text_size(px(12.5))
                .when(busy, |d| d.opacity(0.55))
                .when(!busy, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
                .tooltip(move |window, cx| {
                    Tooltip::new(if done {
                        "Click to untick"
                    } else {
                        "Click to tick"
                    })
                    .build(window, cx)
                })
                .on_click(cx.listener(move |drawer, _, window, cx| {
                    drawer.edit(
                        CardEdit::Task {
                            id: id.clone(),
                            done: !done,
                            approval,
                        },
                        window,
                        cx,
                    );
                }))
                .child(
                    Icon::new(if task.done {
                        AssetIcon::SquareCheck
                    } else {
                        AssetIcon::Square
                    })
                    .size(px(15.))
                    .text_color(if task.done {
                        p.ok.to_hsla()
                    } else {
                        p.muted.to_hsla()
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(task.done, |d| {
                            d.text_color(p.muted.to_hsla()).line_through()
                        })
                        .child(task.text.clone()),
                )
                .when(task.approval && !task.done, |d| {
                    d.child(chip("needs you".to_owned(), p.warn.to_hsla(), p))
                })
                .into_any_element()
        });
        vec![
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .mt(px(10.))
                .children(rows)
                .into_any_element(),
        ]
    }

    fn render_notes(&self, card: &Card, p: &Palette) -> Vec<AnyElement> {
        let mut body: Vec<AnyElement> = card
            .notes
            .iter()
            .rev()
            .map(|n| {
                let phase = n.phase.as_deref().map(|ph| format!(" \u{b7} {ph}"));
                div()
                    .mt(px(10.))
                    .child(note_box(
                        SharedString::from(format!("wb-note-{}-{}", card.id, n.id)),
                        format!(
                            "{}{} \u{b7} {}",
                            person(&n.author),
                            phase.unwrap_or_default(),
                            ago(n.at)
                        ),
                        n.text.clone(),
                        p,
                    ))
                    .into_any_element()
            })
            .collect();
        if body.is_empty() {
            body.push(
                div()
                    .mt(px(12.))
                    .child(muted_text("No notes", p))
                    .into_any_element(),
            );
        }
        body.push(
            div()
                .mt(px(10.))
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child("Notes are read-only here for now.")
                .into_any_element(),
        );
        body
    }

    fn render_activity(&self, p: &Palette) -> Vec<AnyElement> {
        if self.activity.is_empty() {
            return vec![
                div()
                    .mt(px(12.))
                    .child(muted_text("No activity yet", p))
                    .into_any_element(),
            ];
        }
        let rows = self.activity.iter().map(|a| {
            div()
                .flex()
                .gap(px(10.))
                .px(px(4.))
                .py(px(6.))
                .border_b_1()
                .border_color(p.line.to_hsla())
                .text_size(px(12.))
                .child(
                    div()
                        .flex_none()
                        .w(px(64.))
                        .text_color(p.muted.to_hsla())
                        .child(ago(a.at)),
                )
                .child(div().flex_1().min_w_0().child(format!(
                    "{}: {}",
                    person(&a.actor),
                    a.summary
                )))
                .into_any_element()
        });
        vec![
            div()
                .mt(px(8.))
                .flex()
                .flex_col()
                .children(rows)
                .into_any_element(),
        ]
    }
}

impl Render for CardDrawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        if let Some(path) = self.handoff.clone() {
            return self.render_handoff(&path, p, window, cx);
        }
        let Some((path, card_id)) = self.target.clone() else {
            return div().into_any_element();
        };
        let found = cx
            .global::<WorkbenchStore>()
            .plan(&path)
            .map(|(board, entry)| (entry.plan.clone(), board.settings.clone()));
        let Some((plan, settings)) = found else {
            return div().into_any_element();
        };
        let Some(card) = plan.cards.iter().find(|c| c.id == card_id) else {
            return div().into_any_element();
        };
        let color = column_color(&card.column, p);
        let header = self.render_header(&plan, card, color, p, cx);
        let tabs = self.render_tabs(card, p, cx);
        let body = match self.tab {
            Tab::Overview => self.render_overview(&plan, card, &settings, &path, p, cx),
            Tab::Tasks => self.render_tasks(card, p, cx),
            Tab::Notes => self.render_notes(card, p),
            Tab::Activity => self.render_activity(p),
        };

        frame(p)
            .child(header)
            .child(tabs)
            .child(
                div()
                    .id("wb-drawer-body")
                    .flex_1()
                    .min_h_0()
                    .px(px(16.))
                    .pb(px(20.))
                    .overflow_y_scroll()
                    .children(body),
            )
            .into_any_element()
    }
}

/// The floating panel both views share.
fn frame(p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("wb-drawer")
        .occlude()
        .absolute()
        .top(px(8.))
        .right(px(8.))
        .bottom(px(8.))
        .w(px(420.))
        .flex()
        .flex_col()
        .rounded(px(12.))
        .border_1()
        .border_color(p.line_strong.to_hsla())
        .bg(p.surface.to_hsla())
        .shadow_lg()
        // Clicks inside must not reach the board under it (that closes the drawer).
        .on_click(|_, _, cx| cx.stop_propagation())
}

fn section_title(title: &'static str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(p.muted.to_hsla())
        .child(title.to_uppercase())
}

fn section(title: &'static str, content: AnyElement, p: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .mt(px(16.))
        .child(section_title(title, p))
        .child(content)
        .into_any_element()
}

/// "Where we left off" in the drawer, like the Workbench's HandoffPanel: Questions (answer, or edit
/// an answer), Blockers, Next (numbered), Done.
impl CardDrawer {
    /// The answer box of one question, made the first time it shows.
    fn answer_box(
        &mut self,
        question: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.answers.get(question) {
            return input.clone();
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Or type your own answer\u{2026}"));
        let id = question.to_owned();
        self.answer_subs.push(cx.subscribe_in(
            &input,
            window,
            move |drawer, _, event, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => drawer.submit_answer(&id, None, window, cx),
                    // Send / Save enable as soon as there is something new to send.
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }
            },
        ));
        self.answers.insert(question.to_owned(), input.clone());
        input
    }

    /// Edit opens an answered question's box again, filled with the answer.
    fn start_edit(
        &mut self,
        question: &str,
        answer: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.answer_box(question, window, cx);
        input.update(cx, |input, cx| {
            input.set_value(answer.to_owned(), window, cx)
        });
        self.editing = Some(question.to_owned());
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.editing.take()
            && let Some(input) = self.answers.get(&id)
        {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Sends a picked option, or the question's answer box, as the answer to `question`.
    fn submit_answer(
        &mut self,
        question: &str,
        option: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.handoff.clone() else {
            return;
        };
        let text = option.unwrap_or_else(|| {
            self.answers
                .get(question)
                .map(|input| input.read(cx).value().trim().to_string())
                .unwrap_or_default()
        });
        if self.busy || text.is_empty() {
            return;
        }
        let handoff = cx
            .global::<WorkbenchStore>()
            .plan(&path)
            .and_then(|(_, entry)| entry.handoff.handoff.as_ref().map(|h| h.id.clone()));
        let Some(handoff) = handoff else {
            return;
        };
        self.busy = true;
        let question = question.to_owned();
        let task = answer_question(&path, &handoff, &question, &text, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |drawer, window, cx| {
                drawer.busy = false;
                match result {
                    Ok(()) => {
                        if let Some(input) = drawer.answers.get(&question) {
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        if drawer.editing.as_deref() == Some(question.as_str()) {
                            drawer.editing = None;
                        }
                        // The answer lands in `<planId>.d`, which the watcher does not see.
                        reload_handoff(path, cx);
                    }
                    Err(err) => window.push_notification(Notification::error(err), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_question(
        &mut self,
        index: usize,
        q: &maomi_workbench::HandoffQuestion,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.busy;
        let editing = self.editing.as_deref() == Some(q.id.as_str());
        let answered = q.answer.is_some();
        let (status, status_color) = if answered {
            ("ANSWERED", p.ok.to_hsla())
        } else {
            ("QUESTION", p.now.to_hsla())
        };
        let mut block = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(9.))
            .border_1()
            .map(|d| {
                if answered {
                    d.border_color(p.line.to_hsla()).bg(p.surface_2.to_hsla())
                } else {
                    let now = p.now.to_hsla();
                    d.border_color(now.opacity(0.35)).bg(now.opacity(0.07))
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(10.5))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(status_color)
                    .child(dot(status_color, 6.))
                    .child(status),
            )
            .child(div().text_size(px(12.5)).child(q.text.clone()));

        match &q.answer {
            Some(answer) if !editing => {
                let (id, text) = (q.id.clone(), answer.text.clone());
                let (emph, muted) = (p.fg_emph.to_hsla(), p.muted.to_hsla());
                block = block.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(emph)
                                .child(answer.text.clone()),
                        )
                        .child(format!(
                            "\u{2014} {}, {} \u{b7}",
                            person(&answer.by),
                            ago(answer.at)
                        ))
                        .child(
                            div()
                                .id(("wb-answer-edit", index))
                                .text_color(p.accent.to_hsla())
                                .when(!busy, |d| {
                                    d.cursor_pointer()
                                        .hover(move |s| s.text_color(emph))
                                        .on_click(cx.listener(move |drawer, _, window, cx| {
                                            drawer.start_edit(&id, &text, window, cx)
                                        }))
                                })
                                .child("Edit"),
                        ),
                );
            }
            _ => {
                let input = self.answer_box(&q.id, window, cx);
                let draft = input.read(cx).value().trim().to_string();
                let unchanged =
                    draft.is_empty() || q.answer.as_ref().is_some_and(|a| a.text.trim() == draft);
                if !q.options.is_empty() {
                    block = block.child(div().flex().flex_wrap().gap(px(6.)).children(
                        q.options.iter().enumerate().map(|(j, option)| {
                            let (id, option) = (q.id.clone(), option.clone());
                            Button::new(SharedString::from(format!("wb-option-{index}-{j}")))
                                .xsmall()
                                .label(wlo::clip(&option, 40))
                                .tooltip("Send this answer")
                                .disabled(busy)
                                .on_click(cx.listener(move |drawer, _, window, cx| {
                                    drawer.submit_answer(&id, Some(option.clone()), window, cx)
                                }))
                        }),
                    ));
                }
                let id = q.id.clone();
                block = block.child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(
                            Button::new(("wb-answer-send", index))
                                .primary()
                                .small()
                                .label(if editing { "Save" } else { "Send" })
                                .disabled(busy || unchanged)
                                .on_click(cx.listener(move |drawer, _, window, cx| {
                                    drawer.submit_answer(&id, None, window, cx)
                                })),
                        )
                        .when(editing, |d| {
                            d.child(
                                Button::new(("wb-answer-cancel", index))
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|drawer, _, window, cx| {
                                        drawer.cancel_edit(window, cx)
                                    })),
                            )
                        }),
                );
            }
        }
        block.into_any_element()
    }

    fn render_handoff(
        &mut self,
        path: &Path,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let found = cx
            .global::<WorkbenchStore>()
            .plan(path)
            .map(|(_, entry)| (entry.plan.clone(), entry.handoff.clone()));
        let Some((plan, info)) = found else {
            return div().into_any_element();
        };
        let warn = p.warn.to_hsla();
        let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
        let copy_plan = plan.clone();
        let mut meta: Vec<String> = Vec::new();
        if let Some(h) = &info.handoff {
            meta.push(person(&h.session));
            meta.push(ago(h.at));
            if h.drafted {
                meta.push("drafted from the activity log".into());
            }
        }
        if info.newer_activity > 0 {
            meta.push(wlo::changes_since(info.newer_activity));
        }

        let header = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .pt(px(12.))
            .pb(px(14.))
            .rounded_t(px(12.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(warn.opacity(0.13), 0.),
                linear_color_stop(warn.opacity(0.), 1.),
            ))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(warn)
                            .child(Icon::new(AssetIcon::Flag).size(px(12.)))
                            .child("WHERE WE LEFT OFF"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("wb-handoff-copy")
                            .primary()
                            .xsmall()
                            .label("Copy prompt")
                            .on_click(move |_, window, cx| {
                                wlo::copy_continue_prompt(&copy_plan, window, cx)
                            }),
                    )
                    .child(
                        div()
                            .id("wb-handoff-close")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(26.))
                            .rounded(px(7.))
                            .text_color(p.muted.to_hsla())
                            .hover(move |s| s.bg(hover).text_color(emph))
                            .tooltip(|window, cx| Tooltip::new("Close").build(window, cx))
                            .on_click(cx.listener(|drawer, _, _, cx| {
                                drawer.close(cx);
                                cx.emit(CloseDrawer);
                            }))
                            .child(Icon::new(IconName::Close).size(px(15.))),
                    ),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .line_height(px(21.))
                    .text_color(p.fg_emph.to_hsla())
                    .child(plan.title.clone()),
            )
            .when(!meta.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.muted.to_hsla())
                        .child(meta.join(" \u{b7} ")),
                )
            });

        let mut body: Vec<AnyElement> = Vec::new();
        match &info.handoff {
            None => body.push(
                div()
                    .mt(px(16.))
                    .child(muted_text(
                        "No handoff yet. One appears when a Claude session ends with end_session.",
                        p,
                    ))
                    .into_any_element(),
            ),
            Some(h) => {
                if !h.questions.is_empty() {
                    let open = h.open_questions().count();
                    let total = h.questions.len();
                    let title = if open > 0 && open < total {
                        format!("Questions {total} \u{b7} {open} open")
                    } else {
                        format!("Questions {total}")
                    };
                    let rows: Vec<AnyElement> = h
                        .questions
                        .iter()
                        .enumerate()
                        .map(|(i, q)| self.render_question(i, q, p, window, cx))
                        .collect();
                    body.push(section_owned(
                        title,
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .children(rows)
                            .into_any_element(),
                        p,
                    ));
                }
                let list = |items: &[String], numbered: bool| {
                    div()
                        .flex()
                        .flex_col()
                        .children(items.iter().enumerate().map(|(i, item)| {
                            let row = wlo::item_row(item, p);
                            if numbered {
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(16.))
                                            .pt(px(3.))
                                            .text_size(px(12.))
                                            .text_color(p.muted.to_hsla())
                                            .child(format!("{}.", i + 1)),
                                    )
                                    .child(div().flex_1().min_w_0().child(row))
                                    .into_any_element()
                            } else {
                                row
                            }
                        }))
                        .into_any_element()
                };
                if !h.blockers.is_empty() {
                    body.push(section("Blockers", list(&h.blockers, false), p));
                }
                if !h.next.is_empty() {
                    body.push(section("Next", list(&h.next, true), p));
                }
                if !h.done.is_empty() {
                    body.push(section("Done", list(&h.done, false), p));
                }
            }
        }

        frame(p)
            .child(header)
            .child(
                div()
                    .id("wb-handoff-body")
                    .flex_1()
                    .min_h_0()
                    .px(px(16.))
                    .pb(px(20.))
                    .overflow_y_scroll()
                    .children(body),
            )
            .into_any_element()
    }
}

/// [`section`] with a title made at run time (counts).
fn section_owned(title: String, content: AnyElement, p: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .mt(px(16.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(p.muted.to_hsla())
                .child(title.to_uppercase()),
        )
        .child(content)
        .into_any_element()
}
