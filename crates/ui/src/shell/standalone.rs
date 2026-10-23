//! A pane detached into a window of its own: a slim title bar and the pane, without tabs, rail,
//! dock or status bar. The pane stays in its tab (which shows a placeholder), so Attach back
//! returns it there. The title bar can pin the window above others, fade the backdrop (text
//! stays opaque), outline the text, and turn on passthrough: clicks fall through the terminal
//! to what is behind, and a glued title strip ([`super::title_strip`]) stays clickable.

use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, TITLE_BAR_HEIGHT, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Focusable as _,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, PromptLevel, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, black, div, px, size,
};

use super::Workspace;
use super::pane::{Pane, PaneEvent};
use super::title_strip::{StripState, TitleStrip};
use crate::terminal_view::OUTLINE_OFFSETS;
use crate::{Palette, Theme, window_ext};

/// At 0 only the text, title and buttons stay visible.
const MIN_OPACITY: f32 = 0.;

pub struct Standalone {
    pane: Entity<Pane>,
    /// The workspace whose tab holds the pane's placeholder; gone once its window closed.
    origin: WeakEntity<Workspace>,
    /// This window, for changes made from outside its update (pin, passthrough).
    native: Option<window_ext::NativeWindow>,
    pinned: bool,
    /// Passthrough: this window lets clicks through; the strip window is its title bar.
    strip: Option<(AnyWindowHandle, Entity<TitleStrip>)>,
    /// Overlay text: white with a black outline, readable over any background.
    overlay_text: bool,
    opacity: f32,
    slider: Entity<SliderState>,
    /// Closing needs no more questions (confirmed, or the pane went back to its tab).
    close_confirmed: bool,
    _subscriptions: Vec<Subscription>,
}

/// Opens `pane`, which stays in a tab of `origin`, in a new standalone window.
pub(crate) fn open(pane: Entity<Pane>, origin: WeakEntity<Workspace>, cx: &mut App) {
    let options = WindowOptions {
        window_min_size: Some(size(px(320.), px(180.))),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(780.), px(480.)),
            cx,
        ))),
        // The backdrop fades through; GPUI paints the window's own pixels with alpha.
        window_background: WindowBackgroundAppearance::Transparent,
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, move |window, cx| {
        window.set_window_title("MaomiTerminal");
        cx.new(|cx| Standalone::new(pane, origin, window, cx))
    });
    let handle = match opened {
        Ok((handle, _)) => handle,
        Err(err) => {
            tracing::error!(%err, "failed to open a standalone terminal window");
            return;
        }
    };
    // The kit's root paints the theme background (opaque) under the view; only our own
    // backdrop may show here, so the slider can fade the whole window.
    _ = handle.update(cx, |_, window, cx| {
        gpui_kit::base::Root::update(window, cx, |root, _, cx| {
            root.style().background = Some(gpui_kit::transparent_black().into());
            cx.notify();
        });
    });
}

impl Standalone {
    fn new(
        pane: Entity<Pane>,
        origin: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = window.window_handle();
        pane.update(cx, |pane, cx| {
            pane.rehome(window, cx);
            pane.set_detached(Some(handle), cx);
        });
        let slider = cx.new(|_| {
            SliderState::new()
                .min(MIN_OPACITY)
                .max(1.)
                .step(0.05)
                .default_value(1.)
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &pane,
                window,
                |this, pane, event: &PaneEvent, window, cx| match event {
                    PaneEvent::Changed => {
                        this.sync_strip(cx);
                        cx.notify();
                    }
                    PaneEvent::Close => this.close(pane.clone(), window, cx),
                    // The tab drops the pane too.
                    PaneEvent::ExitedCleanly => {
                        this.close_confirmed = true;
                        window.remove_window();
                    }
                    // The tab handles these (Reattach also closes this window); one pane per
                    // window, so splits, drops and detaching do not apply.
                    PaneEvent::Reattach
                    | PaneEvent::Closed
                    | PaneEvent::SplitRight
                    | PaneEvent::SplitDown
                    | PaneEvent::PaneDropped(..)
                    | PaneEvent::Detach => {}
                },
            ),
            cx.subscribe_in(&slider, window, |this, _, event: &SliderEvent, _, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.set_opacity(value.start(), cx);
            }),
            // The strip goes with this window, however it closes.
            cx.on_release(|this, cx| {
                if let Some((strip, _)) = this.strip.take() {
                    cx.defer(move |cx| {
                        _ = strip.update(cx, |_, window, _| window.remove_window());
                    });
                }
            }),
        ];
        pane.focus_handle(cx).focus(window, cx);
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        Self {
            pane,
            origin,
            native: window_ext::native(window),
            pinned: false,
            strip: None,
            overlay_text: false,
            opacity: 1.,
            slider,
            close_confirmed: false,
            _subscriptions: subscriptions,
        }
    }

    fn set_opacity(&mut self, opacity: f32, cx: &mut Context<Self>) {
        let opacity = opacity.clamp(MIN_OPACITY, 1.);
        if (opacity - self.opacity).abs() < f32::EPSILON {
            return;
        }
        self.opacity = opacity;
        self.pane
            .update(cx, |pane, cx| pane.set_backdrop_opacity(opacity, cx));
        cx.notify();
    }

    fn toggle_overlay_text(&mut self, cx: &mut Context<Self>) {
        self.overlay_text = !self.overlay_text;
        let on = self.overlay_text;
        self.pane
            .update(cx, |pane, cx| pane.set_text_outline(on, cx));
        self.sync_strip(cx);
        cx.notify();
    }

    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        let pinned = !self.pinned;
        if let Some(native) = self.native
            && window_ext::set_topmost(native, pinned)
        {
            self.pinned = pinned;
            cx.notify();
        }
    }

    fn strip_state(&self, cx: &App) -> StripState {
        StripState {
            title: self.pane.read(cx).title().into(),
            outline: self.overlay_text,
        }
    }

    /// Pushes what the strip shows; it never reads this view while drawing.
    fn sync_strip(&self, cx: &mut Context<Self>) {
        let state = self.strip_state(cx);
        if let Some((_, strip)) = &self.strip {
            strip.update(cx, |strip, cx| strip.sync(state, cx));
        }
    }

    /// Clicks fall through this window; a strip window glued over the title bar stays
    /// clickable, and dragging it moves this window along. Both stay on top meanwhile.
    fn enter_passthrough(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(native) = self.native else { return };
        if self.strip.is_some() {
            return;
        }
        let bounds = window.bounds();
        let strip_bounds = Bounds::new(bounds.origin, size(bounds.size.width, TITLE_BAR_HEIGHT));
        let state = self.strip_state(cx);
        let this = cx.entity().downgrade();
        // On top before the strip opens: the strip, raised last, must stay above this window.
        window_ext::set_topmost(native, true);
        let Some(strip) = TitleStrip::open(this, native, strip_bounds, state, cx) else {
            window_ext::set_topmost(native, self.pinned);
            return;
        };
        window_ext::set_click_through(native, true);
        self.strip = Some(strip);
        cx.notify();
    }

    pub(super) fn exit_passthrough(&mut self, cx: &mut Context<Self>) {
        let Some((strip, _)) = self.strip.take() else {
            return;
        };
        if let Some(native) = self.native {
            window_ext::set_click_through(native, false);
            window_ext::set_topmost(native, self.pinned);
        }
        // Deferred: this often runs from the strip's own click, inside its window's update.
        cx.defer(move |cx| {
            _ = strip.update(cx, |_, window, _| window.remove_window());
        });
        cx.notify();
    }

    /// Attach back: the tab takes the pane and closes this window.
    fn attach_back(&mut self, cx: &mut Context<Self>) {
        self.close_confirmed = true;
        self.pane.update(cx, |pane, cx| pane.request_reattach(cx));
    }

    /// The pane's close button: ask first while a command runs.
    fn close(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        if pane.read(cx).is_busy(cx) {
            self.confirm_close(window, cx);
        } else {
            self.finish_close(window, cx);
        }
    }

    /// Ends the terminal: its tab drops the placeholder, then this window goes.
    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_confirmed = true;
        self.pane.update(cx, |pane, cx| pane.close_detached(cx));
        window.remove_window();
    }

    /// The window's close button / Alt+F4.
    fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let pane = self.pane.read(cx);
        if self.close_confirmed || !pane.is_detached() {
            return true;
        }
        if pane.is_busy(cx) {
            self.confirm_close(window, cx);
            return false;
        }
        self.close_confirmed = true;
        self.pane.update(cx, |pane, cx| pane.close_detached(cx));
        true
    }

    fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shell = self.pane.read(cx).shell_name().to_owned();
        let detail = format!("A command is still running in {shell}. Closing the window ends it.");
        let answer = window.prompt(
            PromptLevel::Warning,
            "Close this terminal?",
            Some(&detail),
            &["Close", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                _ = this.update_in(cx, |this, window, cx| this.finish_close(window, cx));
            }
        })
        .detach();
    }
}

/// `make(color)` with overlay text's black outline when `on`: eight black copies, one pixel
/// off in each direction, under the real one (GPUI has no text or icon stroke).
pub(super) fn outlined<E: IntoElement>(
    on: bool,
    color: Hsla,
    make: impl Fn(Hsla) -> E,
) -> AnyElement {
    if !on {
        return make(color).into_any_element();
    }
    div()
        .relative()
        .min_w_0()
        .children(OUTLINE_OFFSETS.map(|(dx, dy)| {
            div()
                .absolute()
                .left(px(dx))
                .top(px(dy))
                .size_full()
                .child(make(black()))
        }))
        .child(make(color))
        .into_any_element()
}

/// A 26 px title bar icon button; occluded so clicks miss the title bar's drag area.
fn title_icon(
    id: &'static str,
    icon: gpui_kit::assets::IconName,
    tooltip: &'static str,
    on: bool,
    outline: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
    let color = if on { p.accent } else { p.muted }.to_hsla();
    div()
        .id(id)
        .occlude()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(26.))
        .rounded(px(7.))
        .text_color(color)
        .hover(move |s| s.bg(hover).text_color(emph))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        // Outlined copies need explicit colours; otherwise the icon follows the hover colour.
        .child(if outline {
            outlined(true, color, |c| Icon::new(icon).size(px(15.)).text_color(c))
        } else {
            Icon::new(icon).size(px(15.)).into_any_element()
        })
}

impl Render for Standalone {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let backdrop = |c: crate::Color| c.to_hsla().opacity(self.opacity);
        let title: SharedString = self.pane.read(cx).title().into();
        // Overlay text also outlines the title bar, which shares the faded backdrop.
        let outline = self.overlay_text;
        let attach = self.origin.upgrade().is_some().then(|| {
            title_icon(
                "attach-back",
                gpui_kit::assets::IconName::SquareArrowDownLeft,
                "Attach back to its tab",
                false,
                outline,
                p,
            )
            .on_click(cx.listener(|this, _, _, cx| this.attach_back(cx)))
        });
        let pin = window_ext::SUPPORTED.then(|| {
            let (icon, tooltip) = if self.pinned {
                (
                    gpui_kit::assets::IconName::PinOff,
                    "Unpin (stop keeping on top)",
                )
            } else {
                (
                    gpui_kit::assets::IconName::Pin,
                    "Pin on top of other windows",
                )
            };
            title_icon("pin", icon, tooltip, self.pinned, outline, p)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_pin(cx)))
        });
        let passthrough = window_ext::SUPPORTED.then(|| {
            title_icon(
                "passthrough",
                gpui_kit::assets::IconName::MousePointer2,
                "Passthrough: clicks go to what is behind; only the title bar stays clickable",
                false,
                outline,
                p,
            )
            .on_click(cx.listener(|this, _, window, cx| this.enter_passthrough(window, cx)))
        });
        let overlay = title_icon(
            "overlay-text",
            gpui_kit::assets::IconName::Captions,
            if self.overlay_text {
                "Overlay text: on (white with a black outline)"
            } else {
                "Overlay text: white with a black outline, readable on any background"
            },
            self.overlay_text,
            outline,
            p,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_overlay_text(cx)));
        let opacity = div()
            .id("opacity")
            .occlude()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .text_color(p.muted.to_hsla())
            .tooltip(|window, cx| Tooltip::new("Backdrop opacity").build(window, cx))
            .child(outlined(outline, p.muted.to_hsla(), |c| {
                Icon::new(gpui_kit::assets::IconName::Droplet)
                    .size(px(14.))
                    .text_color(c)
            }))
            .child(div().w(px(96.)).child(Slider::new(&self.slider)));

        div()
            .size_full()
            .flex()
            .flex_col()
            // The one backdrop layer of the window: the title bar and the pane's body paint
            // none of their own here, so the slider's value is what shows (layers would add up).
            .bg(backdrop(p.surface))
            .text_color(p.fg.to_hsla())
            .text_size(px(13.))
            // In passthrough the strip window draws the title bar over this space.
            .when(self.strip.is_some(), |d| {
                d.child(div().flex_none().h(TITLE_BAR_HEIGHT))
            })
            .when(self.strip.is_none(), |d| {
                d.child(
                    TitleBar::new().bg(gpui_kit::transparent_black()).child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .size_full()
                            .min_w_0()
                            .pr(px(8.))
                            .child(div().flex_1().min_w_0().child(outlined(
                                outline,
                                p.fg_emph.to_hsla(),
                                |c| {
                                    div()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(c)
                                        .child(title.clone())
                                },
                            )))
                            .child(opacity)
                            .child(overlay)
                            .when_some(passthrough, |d, button| d.child(button))
                            .when_some(attach, |d, attach| d.child(attach))
                            .when_some(pin, |d, pin| d.child(pin)),
                    ),
                )
            })
            .child(div().flex_1().min_h_0().p(px(4.)).child(self.pane.clone()))
    }
}
