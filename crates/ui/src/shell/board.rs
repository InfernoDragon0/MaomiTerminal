//! A Workbench plan as a read-only kanban board in the centre, and the card drawer that floats over
//! the centre. Both read the shared [`WorkbenchStore`], so a plan a Claude session writes redraws
//! by itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, EventEmitter, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, linear_color_stop, linear_gradient, px,
};
use maomi_workbench::{Card, Column, Plan, ProjectSettings};

use super::workbench::{
    WorkbenchStore, column_color, column_label, load_board, model_color, percent, shown_column,
    slot_color,
};
use crate::{Palette, Theme};

/// What the board asks the workspace to do.
pub enum BoardPageEvent {
    OpenCard(String),
    /// A click on the board background closes the drawer.
    CloseCard,
}

pub struct BoardPage {
    plan: PathBuf,
    /// The card the drawer shows, outlined on the board.
    open_card: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BoardPageEvent> for BoardPage {}

impl BoardPage {
    pub fn new(plan: PathBuf, cx: &mut Context<Self>) -> Self {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        Self {
            plan,
            open_card: None,
            _subscriptions: vec![cx.observe_global::<WorkbenchStore>(|_, cx| cx.notify())],
        }
    }

    pub fn plan(&self) -> &Path {
        &self.plan
    }

    pub fn set_plan(&mut self, plan: PathBuf, cx: &mut Context<Self>) {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        self.plan = plan;
        cx.notify();
    }

    pub fn set_open_card(&mut self, card: Option<String>, cx: &mut Context<Self>) {
        self.open_card = card;
        cx.notify();
    }

    fn render_header(&self, plan: &Plan, p: &Palette) -> impl IntoElement {
        let pct = percent(plan.progress());
        let in_review = plan
            .cards
            .iter()
            .filter(|c| c.column == Column::Review)
            .count();
        let cards = plan.cards.len();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(20.))
            .pt(px(16.))
            .pb(px(12.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child(plan.title.clone()),
                    )
                    .child(chip(
                        plan.status.as_str().to_owned(),
                        status_color(plan, p),
                        p,
                    ))
                    .child(
                        div()
                            .id("wb-live")
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(p.muted.to_hsla())
                            .tooltip(|window, cx| {
                                Tooltip::new("Re-reads the board when a Claude session writes it")
                                    .build(window, cx)
                            })
                            .child(dot(p.ok.to_hsla(), 7.))
                            .child(format!("Live \u{b7} rev {}", plan.rev)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(8.))
                            .w(px(240.))
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(p.fg_emph.to_hsla())
                                    .child(format!("{pct}%")),
                            )
                            .child(bar(pct, 6., p)),
                    )
                    .child(div().flex_none().child(format!(
                        "{cards} card{} \u{b7} {in_review} in review",
                        if cards == 1 { "" } else { "s" }
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(first_line(&plan.brief)),
                    )
                    .child(chip("read-only".to_owned(), p.muted.to_hsla(), p)),
            )
    }

    fn render_card(
        &self,
        plan: &Plan,
        card: &Card,
        settings: &ProjectSettings,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card_type = settings.card_type(card.type_id.as_deref());
        let type_color = card_type.and_then(|t| slot_color(&t.color, p));
        let open = self.open_card.as_deref() == Some(card.id.as_str());
        // A claimed card that is in progress is being worked on right now.
        let working = card.column == Column::InProgress && !card.assignees.is_empty();
        let waits_on = card
            .depends_on
            .iter()
            .filter(|id| {
                plan.cards
                    .iter()
                    .find(|c| &&c.id == id)
                    .is_some_and(|c| c.column != Column::Done)
            })
            .count();
        let tasks_done = card.tasks.iter().filter(|t| t.done).count();
        let needs_user = card.column == Column::Review && card.review.is_some();
        let id = card.id.clone();
        let (hover, line_strong) = (p.surface_3.to_hsla(), p.line_strong.to_hsla());

        let face = div()
            .id(SharedString::from(format!("wb-board-card-{}", card.id)))
            .relative()
            .flex()
            .flex_col()
            .gap(px(7.))
            .px(px(11.))
            .py(px(10.))
            .rounded(px(10.))
            .border_1()
            .bg(p.surface_2.to_hsla())
            .border_color(if open {
                p.accent.to_hsla()
            } else if working {
                gpui_kit::transparent_black()
            } else {
                p.line.to_hsla()
            })
            .cursor_pointer()
            .when(!open && !working, |d| {
                d.hover(move |s| s.bg(hover).border_color(line_strong))
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(BoardPageEvent::OpenCard(id.clone()));
            }))
            // Type colour as a soft top tint, never a side stripe.
            .when_some(type_color, |d, color| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(46.))
                        .rounded_t(px(10.))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(color.opacity(0.12), 0.),
                            linear_color_stop(color.opacity(0.), 1.),
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_h(px(18.))
                    .child(type_chip(
                        card_type.map(|t| t.label.as_str()),
                        type_color,
                        p,
                    ))
                    .child(div().flex_1())
                    .when_some(card.agent.as_ref(), |d, agent| {
                        d.child(model_tag(&agent.model, p))
                    })
                    .when_some(card.size.as_ref(), |d, size| {
                        d.child(size_tag(size.as_str(), p))
                    }),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .line_height(px(17.5))
                    .text_color(p.fg_emph.to_hsla())
                    .child(card.title.clone()),
            )
            .when(!card.tasks.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .text_color(p.muted.to_hsla())
                        .child(bar(percent(card.progress()), 4., p))
                        .child(format!("{tasks_done}/{}", card.tasks.len())),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(18.))
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .when(waits_on > 0, |d| {
                        d.child(meta(AssetIcon::Link, waits_on.to_string()))
                    })
                    .when(!card.notes.is_empty(), |d| {
                        d.child(meta(AssetIcon::MessageSquare, card.notes.len().to_string()))
                    })
                    .when(needs_user, |d| {
                        d.child(
                            div()
                                .text_color(p.warn.to_hsla())
                                .child(meta(AssetIcon::Clock, "your review".to_owned())),
                        )
                    })
                    .child(div().flex_1())
                    .child(avatars(&card.assignees, p)),
            );

        if working {
            // Aurora ring: the gradient shows through a 1px gap around the card.
            div()
                .p(px(1.))
                .rounded(px(11.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(p.gradient[0].to_hsla(), 0.),
                    linear_color_stop(p.gradient[3].to_hsla(), 1.),
                ))
                .child(face)
                .into_any_element()
        } else {
            face.into_any_element()
        }
    }
}

impl Render for BoardPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let store = cx.global::<WorkbenchStore>();
        let found = store
            .plan(&self.plan)
            .map(|(board, entry)| (entry.plan.clone(), board.settings.clone()));
        let loaded = self.plan.parent().is_some_and(|d| store.board(d).is_some());
        let page = div()
            .id("wb-board-page")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(p.bg.to_hsla())
            .on_click(cx.listener(|_, _, _, cx| cx.emit(BoardPageEvent::CloseCard)));
        let Some((plan, settings)) = found else {
            return page.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(p.muted.to_hsla())
                    .child(if loaded {
                        "This plan is gone. It may have been deleted."
                    } else {
                        "Loading\u{2026}"
                    }),
            );
        };
        let columns: Vec<AnyElement> = Column::ALL
            .iter()
            .map(|column| self.render_column(&plan, column, &settings, p, cx))
            .collect();
        page.child(self.render_header(&plan, p)).child(
            div()
                .id("wb-columns")
                .flex()
                .flex_1()
                .min_h_0()
                .gap(px(10.))
                .px(px(14.))
                .pt(px(12.))
                .pb(px(14.))
                .overflow_x_scroll()
                .children(columns),
        )
    }
}

impl BoardPage {
    fn render_column(
        &self,
        plan: &Arc<Plan>,
        column: &Column,
        settings: &Arc<ProjectSettings>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut cards: Vec<&Card> = plan
            .cards
            .iter()
            .filter(|c| &shown_column(&c.column) == column)
            .collect();
        cards.sort_by(|a, b| a.order.total_cmp(&b.order));
        let color = column_color(column, p);
        let body: Vec<AnyElement> = cards
            .iter()
            .map(|card| self.render_card(plan, card, settings, p, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(220.))
            .min_h_0()
            .rounded(px(12.))
            .border_1()
            .border_color(p.line.to_hsla())
            .bg(p.surface.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(8.))
                    .rounded_t(px(12.))
                    .border_t_2()
                    .border_color(color)
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(color.opacity(0.10), 0.),
                        linear_color_stop(color.opacity(0.), 1.),
                    ))
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(p.fg_emph.to_hsla())
                    .child(dot(color, 8.))
                    .child(column_label(column).to_owned())
                    .child(div().flex_1())
                    .child(
                        div()
                            .px(px(7.))
                            .rounded(px(99.))
                            .bg(p.surface_2.to_hsla())
                            .text_size(px(11.))
                            .font_weight(gpui_kit::FontWeight::NORMAL)
                            .text_color(p.muted.to_hsla())
                            .child(cards.len().to_string()),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("wb-col-{}", column.as_str())))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(8.))
                    .px(px(8.))
                    .pt(px(4.))
                    .pb(px(10.))
                    .overflow_y_scroll()
                    .children(body)
                    .when(cards.is_empty(), |d| {
                        d.child(
                            div()
                                .py(px(18.))
                                .rounded(px(10.))
                                .border_1()
                                .border_dashed()
                                .border_color(p.line_strong.to_hsla())
                                .text_size(px(12.))
                                .text_color(p.muted.to_hsla())
                                .text_center()
                                .child("No cards"),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// Asks the workspace to close the drawer.
pub struct CloseDrawer;

/// The card drawer. It floats over the centre (terminals or a board) and reads the card from the
/// store each frame, so it follows live edits.
pub struct CardDrawer {
    target: Option<(PathBuf, String)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDrawer> for CardDrawer {}

impl CardDrawer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            target: None,
            _subscriptions: vec![cx.observe_global::<WorkbenchStore>(|drawer, cx| {
                if drawer.target.is_some() {
                    cx.notify();
                }
            })],
        }
    }

    pub fn show(&mut self, plan: PathBuf, card: String, cx: &mut Context<Self>) {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        self.target = Some((plan, card));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.target = None;
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.target.is_some()
    }

    pub fn card(&self) -> Option<&str> {
        self.target.as_ref().map(|(_, card)| card.as_str())
    }
}

impl Render for CardDrawer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
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
        let card_type = settings.card_type(card.type_id.as_deref());
        let type_color = card_type.and_then(|t| slot_color(&t.color, p));
        let pct = percent(card.progress());

        let mut body: Vec<AnyElement> = vec![section(
            "Description",
            if card.description.trim().is_empty() {
                muted_text("No description", p)
            } else {
                div()
                    .text_size(px(12.5))
                    .child(TextView::markdown(
                        SharedString::from(format!("wb-desc-{card_id}")),
                        card.description.clone(),
                    ))
                    .into_any_element()
            },
            p,
        )];
        body.push(section("Tasks", self.render_tasks(card, p), p));
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
                    let (hover, path, dep) = (p.surface_3.to_hsla(), path.clone(), dep.clone());
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
        if !card.files.is_empty() || card.verify.is_some() {
            let mono = crate::TERMINAL_FAMILY;
            let mut files: Vec<AnyElement> = card
                .files
                .iter()
                .map(|f| {
                    div()
                        .font_family(mono)
                        .text_size(px(12.))
                        .text_color(p.fg_emph.to_hsla())
                        .child(f.clone())
                        .into_any_element()
                })
                .collect();
            if let Some(verify) = &card.verify {
                files.push(
                    div()
                        .flex()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(p.muted.to_hsla())
                        .child("Verify:")
                        .child(
                            div()
                                .font_family(mono)
                                .text_color(p.fg_emph.to_hsla())
                                .child(verify.clone()),
                        )
                        .into_any_element(),
                );
            }
            body.push(section(
                "Files",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children(files)
                    .into_any_element(),
                p,
            ));
        }
        let reviews: Vec<AnyElement> = [("Review asked", &card.review), ("Checked", &card.check)]
            .into_iter()
            .filter_map(|(what, request)| {
                let r = request.as_ref()?;
                let who = person(&r.by);
                let reason = r.reason.clone().unwrap_or_default();
                Some(note_box(
                    format!("{what} by {who} \u{b7} {}", ago(r.at)),
                    reason,
                    p,
                ))
            })
            .collect();
        if !reviews.is_empty() {
            body.push(section(
                "Review",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(reviews)
                    .into_any_element(),
                p,
            ));
        }
        if !card.notes.is_empty() {
            let notes: Vec<AnyElement> = card
                .notes
                .iter()
                .rev()
                .map(|n| {
                    let phase = n.phase.as_deref().map(|ph| format!(" \u{b7} {ph}"));
                    note_box(
                        format!(
                            "{}{} \u{b7} {}",
                            person(&n.author),
                            phase.unwrap_or_default(),
                            ago(n.at)
                        ),
                        n.text.clone(),
                        p,
                    )
                })
                .collect();
            body.push(section(
                "Notes",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(notes)
                    .into_any_element(),
                p,
            ));
        }

        div()
            .id("wb-drawer")
            .occlude()
            .absolute()
            .top(px(8.))
            .right(px(8.))
            .bottom(px(8.))
            .w(px(400.))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .border_1()
            .border_color(p.line_strong.to_hsla())
            .bg(p.surface.to_hsla())
            .shadow_lg()
            // Clicks inside must not reach the board under it (that closes the drawer).
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(16.))
                    .pt(px(14.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(p.line.to_hsla())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(type_chip(
                                card_type.map(|t| t.label.as_str()),
                                type_color,
                                p,
                            ))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("wb-drawer-close")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(26.))
                                    .rounded(px(7.))
                                    .text_color(p.muted.to_hsla())
                                    .hover({
                                        let (bg, fg) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
                                        move |s| s.bg(bg).text_color(fg)
                                    })
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
                            .text_color(p.fg_emph.to_hsla())
                            .child(card.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(chip(
                                column_label(&card.column).to_owned(),
                                column_color(&card.column, p),
                                p,
                            ))
                            .when_some(card.size.as_ref(), |d, size| {
                                d.child(size_tag(size.as_str(), p))
                            })
                            .when_some(card.agent.as_ref(), |d, agent| {
                                d.child(model_tag(&agent.model, p))
                            })
                            .child(div().flex_1())
                            .child(avatars(&card.assignees, p)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(11.))
                            .text_color(p.muted.to_hsla())
                            .child(bar(pct, 4., p))
                            .child(format!("{pct}%")),
                    ),
            )
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
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(p.line.to_hsla())
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .child(format!(
                        "{} \u{b7} read-only \u{b7} edits come in phase 2",
                        card.id
                    )),
            )
            .into_any_element()
    }
}

impl CardDrawer {
    fn render_tasks(&self, card: &Card, p: &Palette) -> AnyElement {
        if card.tasks.is_empty() {
            return muted_text("No tasks", p);
        }
        let rows: Vec<AnyElement> = card
            .tasks
            .iter()
            .map(|task| {
                div()
                    .flex()
                    .items_start()
                    .gap(px(9.))
                    .px(px(4.))
                    .py(px(5.))
                    .text_size(px(12.5))
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
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(rows)
            .into_any_element()
    }
}

fn section(title: &'static str, content: AnyElement, p: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .mt(px(14.))
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

fn note_box(head: String, text: String, p: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .px(px(10.))
        .py(px(9.))
        .rounded(px(9.))
        .bg(p.surface_2.to_hsla())
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child(head),
        )
        .when(!text.is_empty(), |d| {
            d.child(div().text_size(px(12.5)).child(text))
        })
        .into_any_element()
}

fn muted_text(text: &'static str, p: &Palette) -> AnyElement {
    div()
        .text_size(px(12.5))
        .text_color(p.muted.to_hsla())
        .child(text)
        .into_any_element()
}

fn status_color(plan: &Plan, p: &Palette) -> Hsla {
    match plan.status {
        maomi_workbench::PlanStatus::Active => p.now,
        maomi_workbench::PlanStatus::Done => p.ok,
        maomi_workbench::PlanStatus::Paused => p.warn,
        _ => p.muted,
    }
    .to_hsla()
}

/// A pill with a coloured dot.
fn chip(text: String, color: Hsla, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .px(px(8.))
        .py(px(1.))
        .rounded(px(99.))
        .border_1()
        .border_color(p.line_strong.to_hsla())
        .text_size(px(11.))
        .text_color(p.fg.to_hsla())
        .child(dot(color, 6.))
        .child(text)
}

fn type_chip(label: Option<&str>, color: Option<Hsla>, p: &Palette) -> impl IntoElement {
    let color = color.unwrap_or(p.muted.to_hsla());
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .text_size(px(10.5))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(color)
        .when_some(label, |d, label| {
            d.child(div().size(px(7.)).rounded(px(2.)).bg(color))
                .child(label.to_uppercase())
        })
}

fn model_tag(model: &str, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .text_size(px(10.5))
        .text_color(p.muted.to_hsla())
        .child(dot(model_color(model, p), 6.))
        .child(model.to_owned())
}

fn size_tag(size: &str, p: &Palette) -> impl IntoElement {
    div()
        .px(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(p.line_strong.to_hsla())
        .text_size(px(10.5))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(p.muted.to_hsla())
        .child(size.to_owned())
}

fn meta(icon: AssetIcon, text: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(3.))
        .child(Icon::new(icon).size(px(12.)))
        .child(text)
}

/// Assignee initials: Claude session ids, or "user" for the person.
fn avatars(ids: &[String], p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .children(ids.iter().take(3).enumerate().map(|(i, id)| {
            let user = id == "user";
            let initials: String = if user {
                "U".into()
            } else {
                id.chars().take(2).collect::<String>().to_uppercase()
            };
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(18.))
                .when(i > 0, |d| d.ml(px(-5.)))
                .rounded(px(9.))
                .border_1()
                .border_color(p.surface_2.to_hsla())
                .bg(if user { p.now } else { p.accent }.to_hsla())
                .text_size(px(9.5))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(if user { p.on_now } else { p.on_accent }.to_hsla())
                .child(initials)
        }))
}

fn dot(color: Hsla, size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size / 2.))
        .bg(color)
}

fn bar(pct: u32, height: f32, p: &Palette) -> impl IntoElement {
    let fill = if pct >= 100 { p.ok } else { p.accent };
    div()
        .flex_1()
        .h(px(height))
        .rounded(px(99.))
        .overflow_hidden()
        .bg(p.surface_3.to_hsla())
        .child(
            div()
                .h_full()
                .w(gpui_kit::relative(pct.min(100) as f32 / 100.))
                .rounded(px(99.))
                .bg(fill.to_hsla()),
        )
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_owned()
}

fn person(id: &str) -> String {
    if id == "user" {
        "you".into()
    } else {
        format!("session {}", id.chars().take(8).collect::<String>())
    }
}

/// "5m ago" style age of an epoch-ms timestamp.
fn ago(at_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(at_ms);
    let secs = (now - at_ms).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line("\n\n  \nHello\nworld"), "Hello");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn people_read_as_you_or_a_short_session() {
        assert_eq!(person("user"), "you");
        assert_eq!(person("0123456789abcdef"), "session 01234567");
    }

    #[test]
    fn slots_map_to_palette_colours() {
        let p = &crate::palette::AURORA_DARK;
        assert_eq!(slot_color("c1", p), Some(p.c[0].to_hsla()));
        assert_eq!(slot_color("c5", p), Some(p.c[4].to_hsla()));
        assert_eq!(slot_color("c0", p), None);
        assert_eq!(slot_color("c6", p), None);
        assert_eq!(slot_color("red", p), None);
    }
}
