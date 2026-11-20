//! The Repo tab's toolbar: Branch, Stash, Pop | Fetch, Pull (mode menu), Push, with behind /
//! ahead badges for the current branch and the running operation on the right. Port of
//! GitDragon's `toolbar.rs` (without LFS and undo), plus the bits of its `motion` and `dialogs`
//! modules the toolbar uses.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClickEvent, Context, ElementId,
    Entity, Focusable as _, FontWeight, Hsla, IntoElement, ParentElement as _, Pixels, PromptLevel,
    SharedString, Styled, Subscription, WeakEntity, Window, div, ease_in_out, linear,
    prelude::FluentBuilder as _, pulsating_between, px, radians,
};
use maomi_git::{PullMode, PushOptions, StashAction};

use super::repo_dock::on;
use super::repo_page::RepoPage;
use crate::Palette;
use crate::repo::diff::sanitize_line;
use crate::repo::{OpKind, RepoModel};

/// One revolution of a busy spinner or one loop of a busy icon.
const CYCLE: Duration = Duration::from_millis(1000);
/// A slow "breathing" glow.
const BREATHE: Duration = Duration::from_millis(2400);
/// Frame cap for animations that repaint continuously.
const FPS_CAP: f32 = 30.;

/// What the buttons need from the repository.
struct Head {
    branch: Option<String>,
    upstream: bool,
    /// `origin/main`-style name of the upstream.
    upstream_ref: Option<String>,
    ahead: u32,
    behind: u32,
    stashes: usize,
    /// Uncommitted changes (Stash has nothing to save without them).
    changes: usize,
    remote: Option<String>,
    busy: Option<SharedString>,
    /// The running operation, `Other` when none.
    op: OpKind,
}

impl Head {
    fn of(m: &RepoModel) -> Self {
        let branch = m.status.as_ref().map(|s| &s.branch);
        let remote = m
            .remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(m.remotes.first())
            .map(|r| r.name.clone());
        Self {
            branch: branch.and_then(|b| b.head.clone()),
            upstream: branch.is_some_and(|b| b.upstream.is_some()),
            upstream_ref: branch.and_then(|b| b.upstream.clone()),
            ahead: branch.map_or(0, |b| b.ahead),
            behind: branch.map_or(0, |b| b.behind),
            stashes: m.stashes.len(),
            changes: m.status.as_ref().map_or(0, |s| s.entries.len()),
            remote,
            busy: m.op_label.clone(),
            op: if m.op_label.is_some() {
                m.op_kind
            } else {
                OpKind::Other
            },
        }
    }
}

// ---- actions --------------------------------------------------------------------------------

fn op<F, Fut>(model: &Entity<RepoModel>, label: String, cx: &mut App, op: F)
where
    F: FnOnce(maomi_git::CliRepo) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = maomi_git::Result<()>> + Send + 'static,
{
    model.update(cx, |m, cx| _ = m.run_op(label, cx, op));
}

fn pull(model: &Entity<RepoModel>, mode: PullMode, cx: &mut App) {
    model.update(cx, |m, cx| _ = m.pull(mode, cx));
}

/// Fetch all remotes.
fn fetch(model: &Entity<RepoModel>, cx: &mut App) {
    op(
        model,
        "Fetching all remotes".into(),
        cx,
        |repo| async move { repo.fetch(None, false).await },
    );
}

/// Pop the latest stash.
fn pop_stash(model: &Entity<RepoModel>, cx: &mut App) {
    op(model, "Popping stash@{0}".into(), cx, |repo| async move {
        repo.stash_action(StashAction::Pop, 0).await
    });
}

/// Pushes the current branch; with no upstream yet, to `origin` (or the first remote), setting
/// the upstream.
fn push(model: &Entity<RepoModel>, cx: &mut App) {
    let head = Head::of(model.read(cx));
    let Some(branch) = head.branch else {
        return;
    };
    if head.upstream {
        op(
            model,
            format!("Pushing {branch}"),
            cx,
            move |repo| async move { repo.push(None).await },
        );
        return;
    }
    let Some(remote) = head.remote else {
        model.update(cx, |m, cx| {
            m.op_error = Some("No remote to push to: add one with `git remote add`.".into());
            cx.notify();
        });
        return;
    };
    let opts = PushOptions {
        set_upstream: true,
        force_with_lease: false,
    };
    op(
        model,
        format!("Pushing {branch} to {remote}"),
        cx,
        move |repo| async move { repo.push_branch(&remote, &branch, opts).await },
    );
}

impl RepoPage {
    fn new_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model().clone();
        self.dialog_sub = Some(prompt(
            window,
            cx,
            "New branch from HEAD",
            vec![Field::new("Branch name, e.g. feature/login")],
            &["Create", "Create & checkout"],
            move |values, button, _, cx| {
                let (name, checkout) = (values[0].clone(), button == 1);
                op(
                    &model,
                    format!("Creating {name}"),
                    cx,
                    move |repo| async move { repo.create_branch(&name, "HEAD", checkout).await },
                );
            },
        ));
    }

    /// Asks, then pops the latest stash of this page's repo.
    fn pop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model().clone();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Pop the latest stash?",
            Some("Applies stash@{0} to the working copy, then drops it."),
            &["Pop", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            _ = this.update(cx, |_, cx| pop_stash(&model, cx));
        })
        .detach();
    }

    fn stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model().clone();
        self.dialog_sub = Some(prompt(
            window,
            cx,
            "Stash all changes",
            vec![Field::new("Message (optional)").value("WIP")],
            &["Stash"],
            move |values, _, _, cx| {
                let message = values[0].clone();
                op(&model, "Stashing".into(), cx, move |repo| async move {
                    repo.stash_push(&message, true, false, &[]).await
                });
            },
        ));
    }
}

// ---- rendering ------------------------------------------------------------------------------

/// A toolbar button: icon over label, no border, transparent until hovered, all the same width
/// so the group reads as evenly spaced. With a `tone` (a semantic colour, not the accent) the
/// icon and label glow: they take the colour, the label turns semibold and a soft light sits
/// behind the icon and slowly pulses (GPUI has no text shadow, and the backdrop stays clear).
fn tool(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    p: &Palette,
    tone: Option<Hsla>,
    cx: &App,
) -> Button {
    tool_anim(id, icon, label, p, tone, OpKind::Other, cx)
}

/// The icon of a toolbar button while its operation runs: Fetch spins, Pull drops the arrow into
/// a tray, Push lifts it out. `Other` is the plain static icon.
fn op_icon(
    id: &'static str,
    icon: IconName,
    anim: OpKind,
    color: Hsla,
    size: Pixels,
    reduce: bool,
) -> AnyElement {
    // Lucide draws on a 24 grid with a 2 unit stroke; `unit` is one grid unit at this size.
    let unit = size / 24.;
    match anim {
        OpKind::Other => Icon::new(icon).size(size).into_any_element(),
        OpKind::Fetch => spin(
            Icon::new(icon).size(size),
            SharedString::from(format!("{id}-spin")),
            CYCLE,
            reduce,
        ),
        OpKind::Pull | OpKind::Push => {
            let pull = anim == OpKind::Pull;
            // Lucide's download tray: x 3..21, y 15..21 on the 24 grid, 2 unit stroke, open at
            // the top, corners rounded (outer radius 3).
            let tray = div()
                .absolute()
                .top(unit * 15.)
                .left(unit * 2.)
                .w(unit * 20.)
                .h(unit * 7.)
                .border(unit * 2.)
                .border_t_0()
                .border_color(color)
                .rounded_b(unit * 3.);
            // The arrow at full icon size so its stroke matches the idle icon; the outer box clips
            // it where it sits above or below the tray.
            let arrow = div().absolute().left_0().size(size).child(
                // Pull's arrow points into the tray, Push's out of it.
                Icon::new(if pull {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                })
                .size(size),
            );
            if reduce {
                return Icon::new(icon).size(size).into_any_element();
            }
            let arrow = arrow.with_animation(
                SharedString::from(format!("{id}-move")),
                Animation::new(CYCLE)
                    .repeat()
                    .with_easing(ease_in_out)
                    .with_max_fps(FPS_CAP),
                move |d, t| {
                    // Pull: from above the tray down into it, fading in. Push: the reverse,
                    // fading out at the top.
                    let (from, to) = if pull { (-12., 0.) } else { (0., -12.) };
                    let y = from + (to - from) * t;
                    let fade = if pull { t * 3. } else { (1. - t) * 3. };
                    d.top(unit * y).opacity(fade.min(1.))
                },
            );
            div()
                .relative()
                .size(size)
                .overflow_hidden()
                .child(tray)
                .child(arrow)
                .into_any_element()
        }
    }
}

/// [`tool`] whose icon animates while `anim` (the kind of the running operation) matches.
fn tool_anim(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    p: &Palette,
    tone: Option<Hsla>,
    anim: OpKind,
    cx: &App,
) -> Button {
    // The system "reduce motion" setting keeps the icon still.
    let reduce = cx.reduce_motion();
    let anim = if reduce { OpKind::Other } else { anim };
    let size = px(18.);
    // The light breathes slowly; capped at 30 fps since it repaints for as long as it shows.
    let halo = tone.map(|t| {
        // Stacked translucent discs with exact radii: a blurred shadow on an element this small
        // renders boxy, these stay round. Densest in the middle, fading outwards.
        let disc = |scale: f32, alpha: f32| {
            let d = size * scale;
            div()
                .absolute()
                .size(d)
                .rounded(d / 2.)
                .bg(t.opacity(alpha))
        };
        let halo = div()
            .absolute()
            .size(size * 1.1)
            .flex()
            .items_center()
            .justify_center()
            .child(disc(1.1, 0.08))
            .child(disc(0.85, 0.12))
            .child(disc(0.6, 0.18));
        pulse(
            halo,
            SharedString::from(format!("{id}-glow")),
            0.35,
            1.,
            BREATHE,
            reduce,
        )
    });
    tone_button(Button::new(id), p, tone, cx)
        .min_w(px(64.))
        .px_2()
        .child(
            v_flex()
                .items_center()
                .gap_0p5()
                .child(
                    div()
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(size)
                        .children(halo)
                        .child(op_icon(
                            id,
                            icon,
                            anim,
                            tone.unwrap_or(p.fg.to_hsla()),
                            size,
                            reduce,
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .whitespace_nowrap()
                        .when(tone.is_some(), |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(label.into()),
                ),
        )
}

/// The look [`tool`] shares with the pull-mode chevron: borderless, clear backdrop, full
/// toolbar height; a `tone` only colours the content.
fn tone_button(button: Button, p: &Palette, tone: Option<Hsla>, cx: &App) -> Button {
    let clear = p.bg.to_hsla().opacity(0.);
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .foreground(tone.unwrap_or(p.fg.to_hsla()))
                .color(clear)
                .hover(p.surface_2.to_hsla())
                .active(p.surface_2.to_hsla()),
        )
        .bg(clear)
        .h(px(44.))
}

/// The toolbar for `m`. `trailing` sits at the far right (the page's close button).
pub(super) fn render(
    m: &RepoModel,
    p: &Palette,
    page: &WeakEntity<RepoPage>,
    trailing: AnyElement,
    cx: &App,
) -> AnyElement {
    let head = Head::of(m);
    let busy = head.busy.is_some();
    let badge = |text: String, color: Hsla| {
        div()
            .px_1()
            .rounded_sm()
            .text_xs()
            .text_color(color)
            .bg(color.opacity(0.14))
            .child(SharedString::from(text))
    };
    let pull_label = if head.behind > 0 {
        format!("Pull \u{2193}{}", head.behind)
    } else {
        "Pull".into()
    };
    let push_label = if head.ahead > 0 {
        format!("Push \u{2191}{}", head.ahead)
    } else {
        "Push".into()
    };
    // Glow when there is something to pull / push; semantic colours, never the accent.
    let push_tip: SharedString = match (&head.upstream_ref, &head.remote) {
        (Some(up), _) => format!("Push the current branch to {}", sanitize_line(up)).into(),
        (None, Some(remote)) => format!("Push and track on {}", sanitize_line(remote)).into(),
        (None, None) => "Push the current branch (no remote)".into(),
    };
    let pull_tone = (head.behind > 0 && head.upstream && !busy).then_some(p.warn.to_hsla());
    let push_tone = (head.ahead > 0 && !busy).then_some(p.ok.to_hsla());
    let menu_page = page.clone();
    // The pull-mode chevron sits flush against Pull, as tall as it, and glows with it.
    let pull_menu = tone_button(Button::new("toolbar-pull-mode"), p, pull_tone, cx)
        .px_0p5()
        .xsmall()
        .icon(IconName::ChevronDown)
        .disabled(busy || head.branch.is_none())
        .dropdown_menu(move |menu, _, _| {
            let item = |label: &'static str, mode: PullMode| {
                let page = menu_page.clone();
                PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    if let Some(page) = page.upgrade() {
                        let model = page.read(cx).model().clone();
                        pull(&model, mode, cx);
                    }
                })
            };
            menu.item(item("Pull (per git config)", PullMode::Config))
                .item(item("Pull, merge", PullMode::Merge))
                .item(item("Pull, rebase", PullMode::Rebase))
                .item(item("Pull, fast-forward only", PullMode::FastForwardOnly))
        });
    let divider = div().w_px().h(px(30.)).mx_2().bg(p.line.to_hsla());
    // Local actions on the left of the group, the remote ones (fetch, pull, push) on the right.
    let local = h_flex()
        .gap_1()
        .child(
            tool("toolbar-branch", IconName::GitBranch, "Branch", p, None, cx)
                .disabled(busy)
                .on_click(on(page, |page, _: &ClickEvent, window, cx| {
                    page.new_branch(window, cx)
                })),
        )
        .child(
            tool("toolbar-stash", IconName::Archive, "Stash", p, None, cx)
                .tooltip(if head.changes == 0 {
                    "Nothing to stash: the working copy is clean"
                } else {
                    "Stash all changes (untracked files too)"
                })
                .disabled(busy || head.changes == 0)
                .on_click(on(page, |page, _: &ClickEvent, window, cx| {
                    page.stash(window, cx)
                })),
        )
        .child(
            tool("toolbar-pop", IconName::ArchiveRestore, "Pop", p, None, cx)
                .tooltip("Apply and drop the latest stash")
                .disabled(busy || head.stashes == 0)
                .on_click(on(page, |page, _: &ClickEvent, window, cx| {
                    page.pop(window, cx)
                })),
        );
    let anim = |kind: OpKind| if head.op == kind { kind } else { OpKind::Other };
    let remote = h_flex()
        .gap_1()
        .child(
            tool_anim(
                "toolbar-fetch",
                IconName::RefreshCw,
                "Fetch",
                p,
                None,
                anim(OpKind::Fetch),
                cx,
            )
            .tooltip("Fetch all remotes")
            .disabled(busy && head.op != OpKind::Fetch)
            .on_click(on(page, |page, _: &ClickEvent, _, cx| {
                fetch(&page.model().clone(), cx)
            })),
        )
        .child(
            h_flex()
                .child(
                    tool_anim(
                        "toolbar-pull",
                        IconName::Download,
                        pull_label,
                        p,
                        pull_tone,
                        anim(OpKind::Pull),
                        cx,
                    )
                    .tooltip("Pull the current branch (per git config; ▾ for other modes)")
                    .disabled((busy && head.op != OpKind::Pull) || !head.upstream)
                    .on_click(on(page, |page, _: &ClickEvent, _, cx| {
                        pull(&page.model().clone(), PullMode::Config, cx)
                    })),
                )
                .child(pull_menu),
        )
        .child(
            tool_anim(
                "toolbar-push",
                IconName::Upload,
                push_label,
                p,
                push_tone,
                anim(OpKind::Push),
                cx,
            )
            .tooltip(push_tip)
            .disabled((busy && head.op != OpKind::Push) || head.branch.is_none())
            .on_click(on(page, |page, _: &ClickEvent, _, cx| {
                push(&page.model().clone(), cx)
            })),
        );
    // Equal flexible sides keep the button group centred whatever the right side shows.
    h_flex()
        .flex_none()
        .h(px(52.))
        .px_2()
        .border_b_1()
        .border_color(p.line.to_hsla())
        .bg(p.bg.to_hsla())
        .child(div().flex_1().min_w_0())
        .child(
            h_flex()
                .flex_none()
                .child(local)
                .child(divider)
                .child(remote),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .justify_end()
                .gap_2()
                .when_some(head.busy, |d, label| {
                    d.child(badge(format!("{label}\u{2026}"), p.accent.to_hsla()))
                })
                .when_some(head.branch, |d, branch| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(p.muted.to_hsla())
                            .truncate()
                            .child(SharedString::from(branch)),
                    )
                })
                .child(trailing),
        )
        .into_any_element()
}

// ---- motion (from GitDragon's `ui::motion`) -------------------------------------------------

/// Rotates `icon` one full turn every `period`, forever.
fn spin(icon: Icon, id: impl Into<ElementId>, period: Duration, reduce: bool) -> AnyElement {
    if reduce {
        return icon.into_any_element();
    }
    icon.with_animation(
        id,
        Animation::new(period)
            .repeat()
            .with_easing(linear)
            .with_max_fps(FPS_CAP),
        |icon, t| icon.rotate(radians(t * std::f32::consts::TAU)),
    )
    .into_any_element()
}

/// Breathes `el`'s opacity between `min` and `max` every `period`, forever. Reduced: static at
/// `max`.
fn pulse<E>(
    el: E,
    id: impl Into<ElementId>,
    min: f32,
    max: f32,
    period: Duration,
    reduce: bool,
) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if reduce {
        return el.opacity(max).into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(period)
            .repeat()
            .with_easing(pulsating_between(min, max))
            .with_max_fps(FPS_CAP),
        |el, alpha| el.opacity(alpha),
    )
    .into_any_element()
}

// ---- prompt dialog (from GitDragon's `dialogs.rs`) ------------------------------------------

/// Submits a [`prompt`] with the index of the pressed button.
type Submit = dyn Fn(usize, &mut Window, &mut App);

/// One input of a [`prompt`].
struct Field {
    placeholder: &'static str,
    value: String,
}

impl Field {
    fn new(placeholder: &'static str) -> Self {
        Self {
            placeholder,
            value: String::new(),
        }
    }

    fn value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
}

/// Text prompt. `buttons` are submit buttons, last = primary; `on_submit` gets the trimmed field
/// values and the index of the button (Enter = last, Ctrl+Enter = first). Submitting with an
/// empty first field does nothing. The returned subscription keeps Enter working: hold it until
/// the dialog closes.
fn prompt<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    title: impl Into<SharedString>,
    fields: Vec<Field>,
    buttons: &[&'static str],
    on_submit: impl Fn(Vec<String>, usize, &mut Window, &mut App) + 'static,
) -> Subscription {
    let inputs: Vec<Entity<InputState>> = fields
        .into_iter()
        .map(|f| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(f.placeholder)
                    .default_value(f.value)
            })
        })
        .collect();
    let buttons: Vec<&'static str> = buttons.to_vec();
    let submit: Rc<Submit> = {
        let inputs = inputs.clone();
        Rc::new(move |button, window, cx| {
            let values: Vec<String> = inputs
                .iter()
                .map(|i| i.read(cx).value().trim().to_string())
                .collect();
            if values.first().is_none_or(String::is_empty) {
                return;
            }
            on_submit(values, button, window, cx);
            window.close_dialog(cx);
        })
    };
    let last = buttons.len().saturating_sub(1);
    let sub = cx.subscribe_in(&inputs[0], window, {
        let submit = submit.clone();
        move |_, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary, .. } = event {
                submit(if *secondary { 0 } else { last }, window, cx);
            }
        }
    });
    let title: SharedString = title.into();
    let focus = inputs[0].focus_handle(cx);
    window.open_dialog(cx, move |dialog, _, _| {
        let fields = v_flex().gap_2().children(inputs.iter().map(Input::new));
        let mut row = h_flex().justify_end().gap_2().child(
            Button::new("prompt-cancel")
                .ghost()
                .label("Cancel")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        );
        for (i, label) in buttons.iter().enumerate() {
            let submit = submit.clone();
            let button = Button::new(ElementId::NamedInteger("prompt-ok".into(), i as u64))
                .label(*label)
                .on_click(move |_, window, cx| submit(i, window, cx));
            row = row.child(if i == last {
                button.primary()
            } else {
                button.outline()
            });
        }
        dialog
            .title(title.clone())
            .w(px(440.))
            .child(fields)
            .footer(row)
    });
    window.focus(&focus, cx);
    sub
}
