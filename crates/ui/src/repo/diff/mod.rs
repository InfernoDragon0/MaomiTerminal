//! Read-only diff rows (hunk header, unified line, split pair) for the Repo tab. Port of
//! GitDragon's `ui/src/diff`; the view owns the virtual list.

mod model;
pub mod syntax;
mod words;

pub use model::{DiffModel, DiffRow, Layout, LineView, sanitize_line};

use gpui_kit::{
    AnyElement, Div, Hsla, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    StyledText, div, prelude::FluentBuilder as _, px,
};
use maomi_git::parse::diff::LineKind;

use crate::Palette;

/// Height of every diff row (the list is uniform).
pub const ROW_HEIGHT: f32 = 22.;
const GUTTER: f32 = 48.;

/// Background tints for a line kind: (line, emphasised words).
fn tints(kind: LineKind, p: &Palette) -> Option<(Hsla, Hsla)> {
    match kind {
        LineKind::Added => Some((p.ok.to_hsla().opacity(0.12), p.ok.to_hsla().opacity(0.32))),
        LineKind::Deleted => Some((p.err.to_hsla().opacity(0.12), p.err.to_hsla().opacity(0.32))),
        _ => None,
    }
}

/// One side of a diff line. `numbers` are shown in order (one column for split, two for
/// unified).
fn line(view: &LineView, kind: LineKind, numbers: &[Option<u32>], p: &Palette) -> Div {
    let tint = tints(kind, p);
    let sign = match kind {
        LineKind::Added => "+",
        LineKind::Deleted => "-",
        _ => " ",
    };
    // Word emphasis only on changed lines; syntax colours on every line.
    let (strong, emphasis): (Hsla, &[std::ops::Range<usize>]) = match tint {
        Some((_, strong)) => (strong, &view.emphasis),
        None => (p.accent_soft.to_hsla(), &[]),
    };
    let text = StyledText::new(view.text.clone()).with_highlights(syntax::merge(
        &view.syntax,
        emphasis,
        strong,
        view.text.len(),
    ));
    let muted = p.muted.to_hsla();
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .w_full()
        .min_w_0()
        .font_family(crate::TERMINAL_FAMILY)
        .text_size(px(12.))
        .when_some(tint, |d, (bg, _)| d.bg(bg))
        .children(numbers.iter().map(|n| {
            div()
                .w(px(GUTTER))
                .flex_none()
                .pr(px(8.))
                .text_right()
                .text_color(muted)
                .child(n.map(|n| n.to_string()).unwrap_or_default())
        }))
        .child(
            div()
                .w(px(14.))
                .flex_none()
                .text_color(match kind {
                    LineKind::Added => p.ok.to_hsla(),
                    LineKind::Deleted => p.err.to_hsla(),
                    _ => muted,
                })
                .child(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(p.fg.to_hsla())
                .child(text),
        )
        .when(view.no_eol, |d| {
            d.child(
                div()
                    .flex_none()
                    .px(px(4.))
                    .text_color(p.warn.to_hsla())
                    .child("no newline"),
            )
        })
}

/// One row of a read-only diff.
pub fn row(model: &DiffModel, layout: Layout, ix: usize, p: &Palette) -> Option<AnyElement> {
    Some(match *model.rows(layout).get(ix)? {
        DiffRow::Hunk { hunk } => {
            let header = &model.file.hunks.get(hunk)?.header;
            div()
                .flex()
                .items_center()
                .h(px(ROW_HEIGHT))
                .w_full()
                .px(px(8.))
                .bg(p.surface_2.to_hsla())
                .font_family(crate::TERMINAL_FAMILY)
                .text_size(px(11.5))
                .text_color(p.c[1].to_hsla())
                .child(div().min_w_0().truncate().child(SharedString::from(
                    sanitize_line(&String::from_utf8_lossy(header)).into_owned(),
                )))
                .into_any_element()
        }
        DiffRow::Line { hunk, line } => {
            let l = model.file.hunks.get(hunk)?.lines.get(line)?;
            let view = model.lines.get(hunk)?.get(line)?;
            self::line(view, l.kind, &[l.old_lineno, l.new_lineno], p).into_any_element()
        }
        DiffRow::Pair { hunk, left, right } => {
            let side = |slot: Option<usize>, old: bool| -> AnyElement {
                let found = slot.and_then(|i| {
                    Some((
                        model.file.hunks.get(hunk)?.lines.get(i)?,
                        model.lines.get(hunk)?.get(i)?,
                    ))
                });
                match found {
                    Some((l, view)) => {
                        let number = if old { l.old_lineno } else { l.new_lineno };
                        self::line(view, l.kind, &[number], p).into_any_element()
                    }
                    None => div()
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .bg(p.surface_2.to_hsla())
                        .into_any_element(),
                }
            };
            div()
                .flex()
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .border_r_1()
                        .border_color(p.line.to_hsla())
                        .child(side(left, true)),
                )
                .child(div().flex_1().min_w_0().child(side(right, false)))
                .into_any_element()
        }
    })
}
