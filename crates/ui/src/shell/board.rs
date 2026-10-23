//! A Workbench plan as a kanban board in the centre, and the card drawer that floats over
//! the centre. Both read the shared [`WorkbenchStore`], so a plan a Claude session writes redraws
//! by itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, linear_color_stop,
    linear_gradient, px,
};
use maomi_workbench::{Card, Column, HandoffInfo, Plan, ProjectSettings};

use super::drawer::DragPreview;
use super::wlo;
use super::workbench::{
    CardEdit, WorkbenchStore, answer_question, column_color, column_label, edit_card, load_board,
    model_color, percent, reload_handoff, shown_column, slot_color,
};
use crate::{Palette, Theme};

/// What the board asks the workspace to do.
pub enum BoardPageEvent {
    OpenCard(String),
    /// Show the plan's whole "where we left off" in the drawer.
    OpenHandoff,
    /// A click on the board background closes the drawer.
    CloseCard,
}

pub struct BoardPage {
    plan: PathBuf,
    /// The card the drawer shows, outlined on the board.
    open_card: Option<String>,
    /// "Where we left off" peek: open, and whether the pointer is on the button or the peek.
    peek_open: bool,
    peek_button: bool,
    peek_card: bool,
    /// Answer box for the handoff's first open question.
    answer: Entity<InputState>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BoardPageEvent> for BoardPage {}

impl BoardPage {
    pub fn new(plan: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if let Some(dir) = plan.parent() {
            load_board(dir.to_owned(), cx);
        }
        let answer = cx.new(|cx| InputState::new(window, cx).placeholder("Answer\u{2026}"));
        let enter = cx.subscribe_in(&answer, window, |page, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                page.submit_answer(None, window, cx);
            }
        });
        Self {
            plan,
            open_card: None,
            peek_open: false,
            peek_button: false,
            peek_card: false,
            answer,
            busy: false,
            _subscriptions: vec![
                cx.observe_global::<WorkbenchStore>(|_, cx| cx.notify()),
                enter,
            ],
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

    fn render_header(
        &self,
        plan: &Plan,
        info: &HandoffInfo,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child(plan.title.clone()),
                    )
                    .child(self.wlo_button(plan, info, p, cx))
                    .child(div().flex_1())
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
                    ),
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
        let drag = DraggedBoardCard {
            card: card.id.clone(),
            column: card.column.clone(),
        };
        let drag_name: SharedString = card.title.clone().into();

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
            .on_drag(drag, move |_, _, _, cx| {
                let name = drag_name.clone();
                cx.new(|_| DragPreview { name })
            })
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
            aurora_ring(face, 10., p)
        } else {
            face.into_any_element()
        }
    }
}

/// The aurora ring of a card being worked on: the gradient shows through a 1px gap around
/// `face`, whose corners have `radius`.
pub(super) fn aurora_ring(face: impl IntoElement, radius: f32, p: &Palette) -> AnyElement {
    div()
        .p(px(1.))
        .rounded(px(radius + 1.))
        .bg(linear_gradient(
            90.,
            linear_color_stop(p.gradient[0].to_hsla(), 0.),
            linear_color_stop(p.gradient[3].to_hsla(), 1.),
        ))
        .child(face)
        .into_any_element()
}

impl Render for BoardPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let store = cx.global::<WorkbenchStore>();
        let found = store.plan(&self.plan).map(|(board, entry)| {
            (
                entry.plan.clone(),
                board.settings.clone(),
                entry.handoff.clone(),
            )
        });
        let loaded = self.plan.parent().is_some_and(|d| store.board(d).is_some());
        let page = div()
            .id("wb-board-page")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(p.bg.to_hsla())
            .on_click(cx.listener(|_, _, _, cx| cx.emit(BoardPageEvent::CloseCard)));
        let Some((plan, settings, handoff)) = found else {
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
        let header = self.render_header(&plan, &handoff, p, cx);
        page.child(header).child(
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
        let target = column.clone();
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
            // Dropping a card here moves it to this column.
            .drag_over::<DraggedBoardCard>(move |style, _, _, _| {
                style.border_color(color).bg(color.opacity(0.08))
            })
            .on_drop(
                cx.listener(move |page, dragged: &DraggedBoardCard, window, cx| {
                    page.move_card(dragged, target.clone(), window, cx)
                }),
            )
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

/// The plan's main action, as in the Workbench toolbar: a rainbow "Where we left off" button. Hover
/// shows the peek (Next with Copy prompt, the first open question to answer); a click opens the
/// whole handoff in the drawer.
impl BoardPage {
    fn wlo_button(
        &self,
        plan: &Plan,
        info: &HandoffInfo,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let questions = wlo::open_questions(info);
        let hover = p.surface_2.to_hsla();
        let face = div()
            .id("wb-wlo")
            .relative()
            .flex()
            .items_center()
            .gap(px(7.))
            .h(px(30.))
            .px(px(12.))
            .rounded(px(9.))
            .bg(p.surface.to_hsla())
            .hover(move |s| s.bg(hover))
            .text_size(px(12.5))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(p.fg_emph.to_hsla())
            .cursor_pointer()
            .on_hover(cx.listener(|page, hovered: &bool, window, cx| {
                page.peek_button = *hovered;
                page.hover_changed(window, cx);
            }))
            .on_click(cx.listener(|page, _, _, cx| {
                cx.stop_propagation();
                page.peek_open = false;
                cx.emit(BoardPageEvent::OpenHandoff);
            }))
            .child(Icon::new(AssetIcon::Play).size(px(12.)))
            .child(if questions > 0 {
                format!(
                    "Where we left off ({questions} open {})",
                    if questions == 1 {
                        "question"
                    } else {
                        "questions"
                    }
                )
            } else {
                "Where we left off".to_owned()
            })
            .when(questions > 0, |d| {
                d.child(div().size(px(7.)).rounded(px(4.)).bg(p.warn.to_hsla()))
            });
        let peek = (self.peek_open && info.handoff.is_some())
            .then(|| gpui_kit::deferred(self.wlo_peek(plan, info, p, cx)).with_priority(1));
        div()
            .relative()
            .flex_none()
            .child(aurora_ring(face, 9., p))
            .children(peek)
            .into_any_element()
    }

    fn wlo_peek(
        &self,
        plan: &Plan,
        info: &HandoffInfo,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = |text: &'static str| {
            div()
                .text_size(px(10.5))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(p.muted.to_hsla())
                .child(text.to_uppercase())
        };
        let copy_plan = plan.clone();
        let question = info
            .handoff
            .as_ref()
            .and_then(|h| h.open_questions().next().cloned());
        let busy = self.busy;
        div()
            .id("wb-wlo-peek")
            .occlude()
            .absolute()
            .top(px(38.))
            .left_0()
            .w(px(360.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(12.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.line_strong.to_hsla())
            .bg(p.surface_2.to_hsla())
            .shadow_lg()
            .on_hover(cx.listener(|page, hovered: &bool, window, cx| {
                page.peek_card = *hovered;
                page.hover_changed(window, cx);
            }))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(label("Next"))
                    .child(div().flex_1())
                    .child(
                        Button::new("wb-wlo-copy")
                            .xsmall()
                            .icon(Icon::new(AssetIcon::Copy))
                            .label("Copy prompt")
                            .on_click(move |_, window, cx| {
                                wlo::copy_continue_prompt(&copy_plan, window, cx)
                            }),
                    ),
            )
            .child(match wlo::next_step(info) {
                Some(next) => div()
                    .text_size(px(12.5))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(p.fg_emph.to_hsla())
                    .child(wlo::clip(&next, 160)),
                None => div()
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child("No next step written."),
            })
            .when_some(question, |d, q| {
                d.child(div().mt(px(4.)).child(label("Open question")))
                    .child(div().text_size(px(12.5)).child(wlo::clip(&q.text, 200)))
                    .when(!q.options.is_empty(), |d| {
                        d.child(div().flex().flex_wrap().gap(px(6.)).children(
                            q.options.iter().enumerate().map(|(i, option)| {
                                let option = option.clone();
                                Button::new(("wb-wlo-option", i))
                                    .xsmall()
                                    .label(wlo::clip(&option, 40))
                                    .disabled(busy)
                                    .on_click(cx.listener(move |page, _, window, cx| {
                                        page.submit_answer(Some(option.clone()), window, cx)
                                    }))
                            }),
                        ))
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(div().flex_1().child(Input::new(&self.answer).small()))
                            .child(
                                Button::new("wb-wlo-send")
                                    .primary()
                                    .small()
                                    .label("Send")
                                    .disabled(busy)
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.submit_answer(None, window, cx)
                                    })),
                            ),
                    )
            })
            .child(
                div()
                    .mt(px(2.))
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child(if info.newer_activity > 0 {
                        format!(
                            "{} \u{b7} click the button for the full handoff",
                            wlo::changes_since(info.newer_activity)
                        )
                    } else {
                        "Click the button for the full handoff".to_owned()
                    }),
            )
            .into_any_element()
    }

    /// The peek opens at once and closes a moment after the pointer leaves both the button and the
    /// peek, so moving from one to the other (or typing an answer) keeps it.
    fn hover_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.peek_button || self.peek_card {
            self.peek_open = true;
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            _ = this.update_in(cx, |page, window, cx| {
                let typing = page.answer.focus_handle(cx).is_focused(window);
                if !page.peek_button && !page.peek_card && !typing {
                    page.peek_open = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Answers the first open question with the box's text or a picked option.
    fn submit_answer(
        &mut self,
        option: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = option.unwrap_or_else(|| self.answer.read(cx).value().trim().to_string());
        if self.busy || text.is_empty() {
            return;
        }
        let target = cx
            .global::<WorkbenchStore>()
            .plan(&self.plan)
            .and_then(|(_, entry)| {
                let handoff = entry.handoff.handoff.as_ref()?;
                let question = handoff.open_questions().next()?;
                Some((handoff.id.clone(), question.id.clone()))
            });
        let Some((handoff, question)) = target else {
            return;
        };
        self.busy = true;
        let path = self.plan.clone();
        let task = answer_question(&path, &handoff, &question, &text, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |page, window, cx| {
                page.busy = false;
                match result {
                    Ok(()) => {
                        page.answer.update(cx, |a, cx| a.set_value("", window, cx));
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
}

/// Drag payload of a card on the board page; a column takes it as a move.
#[derive(Debug, Clone)]
pub struct DraggedBoardCard {
    card: String,
    column: Column,
}

impl BoardPage {
    fn move_card(
        &mut self,
        dragged: &DraggedBoardCard,
        column: Column,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if dragged.column == column {
            return;
        }
        let edit = CardEdit::Move {
            column: column.clone(),
            index: None,
        };
        let task = edit_card(&self.plan, &dragged.card, edit, cx);
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            if let Err(err) = result {
                _ = cx.update(|window, cx| window.push_notification(Notification::error(err), cx));
            }
        })
        .detach();
    }
}

pub(super) fn note_box(id: SharedString, head: String, text: String, p: &Palette) -> AnyElement {
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
        // Agents write notes in Markdown.
        .when(!text.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(12.5))
                    .child(TextView::markdown(id, text)),
            )
        })
        .into_any_element()
}

pub(super) fn muted_text(text: &'static str, p: &Palette) -> AnyElement {
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
pub(super) fn chip(text: String, color: Hsla, p: &Palette) -> impl IntoElement {
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

pub(super) fn model_tag(model: &str, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .text_size(px(10.5))
        .text_color(p.muted.to_hsla())
        .child(dot(model_color(model, p), 6.))
        .child(model.to_owned())
}

pub(super) fn size_tag(size: &str, p: &Palette) -> impl IntoElement {
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
pub(super) fn avatars(ids: &[String], p: &Palette) -> impl IntoElement {
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

pub(super) fn dot(color: Hsla, size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size / 2.))
        .bg(color)
}

pub(super) fn bar(pct: u32, height: f32, p: &Palette) -> impl IntoElement {
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

pub(super) fn person(id: &str) -> String {
    if id == "user" {
        "you".into()
    } else {
        format!("session {}", id.chars().take(8).collect::<String>())
    }
}

/// "5m ago" style age of an epoch-ms timestamp.
pub(super) fn ago(at_ms: i64) -> String {
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
