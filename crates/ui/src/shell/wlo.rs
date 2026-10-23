//! "Where we left off" pieces shared by the dock's plan flags, the board's rainbow button and the
//! handoff view in the drawer: tag badges, item rows, the summary peek and Copy prompt. Same
//! content as the Workbench (AgentSummary PlanWlo.vue / SidebarOverview.vue).

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Hsla, IntoElement, ParentElement as _, Styled as _, Window,
    div, px,
};
use maomi_workbench::{HandoffInfo, Plan, parse_handoff_item, tag_counts};

use super::board::{ago, chip};
use crate::Palette;

/// Colour of a handoff tag; semantic colours so every skin re-tints them.
pub(super) fn tag_color(tag: &str, p: &Palette) -> Hsla {
    match tag {
        "added" => p.ok,
        "changed" => p.now,
        "fixed" => p.c[4],
        "removed" => p.err,
        "decided" => p.accent,
        "user" | "review" => p.warn,
        _ => p.muted,
    }
    .to_hsla()
}

/// `[added] ×2` style badge.
pub(super) fn tag_badge(tag: &str, count: Option<usize>, p: &Palette) -> impl IntoElement {
    let color = tag_color(tag, p);
    div()
        .flex_none()
        .px(px(6.))
        .rounded(px(5.))
        .bg(color.opacity(0.14))
        .text_size(px(10.5))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(match count {
            Some(n) if n > 1 => format!("{tag} \u{d7}{n}"),
            _ => tag.to_owned(),
        })
}

/// One handoff bullet: its tag badge (when it has one) and the text.
pub(super) fn item_row(item: &str, p: &Palette) -> AnyElement {
    let (tag, text) = parse_handoff_item(item);
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .py(px(3.))
        .text_size(px(12.5))
        .when_some(tag, |d, tag| d.child(tag_badge(tag, None, p)))
        .child(div().flex_1().min_w_0().child(text.to_owned()))
        .into_any_element()
}

/// The first `next` item without its tag: the peek's headline.
pub(super) fn next_step(info: &HandoffInfo) -> Option<String> {
    let first = info.handoff.as_ref()?.next.first()?;
    Some(parse_handoff_item(first).1.to_owned())
}

pub(super) fn open_questions(info: &HandoffInfo) -> usize {
    info.handoff
        .as_ref()
        .map_or(0, |h| h.open_questions().count())
}

/// Puts the prompt that picks the plan up in a fresh session on the clipboard.
pub(super) fn copy_continue_prompt(plan: &Plan, window: &mut Window, cx: &mut App) {
    let prompt = maomi_workbench::continue_prompt(&plan.id, &plan.title);
    cx.write_to_clipboard(ClipboardItem::new_string(prompt));
    window.push_notification(
        Notification::success("Prompt copied: paste it into a Claude session."),
        cx,
    );
}

/// The summary under a flag or the rainbow button: when, Next, tag counts, open questions and
/// board changes since. The caller adds its own buttons.
pub(super) fn summary(info: &HandoffInfo, p: &Palette) -> Vec<AnyElement> {
    let Some(handoff) = &info.handoff else {
        return vec![
            div()
                .text_size(px(12.))
                .text_color(p.muted.to_hsla())
                .child("No handoff yet.")
                .into_any_element(),
        ];
    };
    let label = |text: &'static str| {
        div()
            .text_size(px(10.5))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(p.muted.to_hsla())
            .child(text.to_uppercase())
            .into_any_element()
    };
    let mut out = vec![
        div()
            .flex()
            .gap(px(4.))
            .text_size(px(12.))
            .text_color(p.muted.to_hsla())
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(p.fg_emph.to_hsla())
                    .child("Where we left off"),
            )
            .child(format!("\u{b7} {}", ago(handoff.at)))
            .when(handoff.drafted, |d| d.child("\u{b7} drafted"))
            .into_any_element(),
    ];
    out.push(label("Next"));
    out.push(match next_step(info) {
        Some(next) => div()
            .text_size(px(12.5))
            .font_weight(gpui_kit::FontWeight::MEDIUM)
            .text_color(p.fg_emph.to_hsla())
            .child(clip(&next, 300))
            .into_any_element(),
        None => div()
            .text_size(px(12.))
            .text_color(p.muted.to_hsla())
            .child("No next step written.")
            .into_any_element(),
    });
    let tags = tag_counts(&handoff.done);
    let questions = open_questions(info);
    if !tags.is_empty() || questions > 0 {
        out.push(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .children(tags.into_iter().map(|(tag, n)| tag_badge(tag, Some(n), p)))
                .when(questions > 0, |d| {
                    d.child(chip(
                        format!(
                            "{questions} {}",
                            if questions == 1 {
                                "question"
                            } else {
                                "questions"
                            }
                        ),
                        p.warn.to_hsla(),
                        p,
                    ))
                })
                .into_any_element(),
        );
    }
    if info.newer_activity > 0 {
        out.push(
            div()
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child(changes_since(info.newer_activity))
                .into_any_element(),
        );
    }
    out
}

pub(super) fn changes_since(n: usize) -> String {
    format!(
        "{n} board {} since this handoff",
        if n == 1 { "change" } else { "changes" }
    )
}

/// `text` cut to `max` characters with an ellipsis.
pub(super) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}
