//! Passthrough's title bar: a slim window of its own glued over a detached window's title bar.
//! Windows can only let clicks through a whole window, so the terminal window goes
//! click-through and this strip stays clickable; dragging it moves the terminal along.
//!
//! The strip never reads the detached window's state while rendering (that window may be
//! mid-update when the strip draws); [`Standalone`] pushes what it shows with [`TitleStrip::sync`].

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, TITLE_BAR_HEIGHT, TitleBar};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowOptions, div, px, size,
};

use super::standalone::{Standalone, outlined};
use crate::Theme;
use crate::window_ext::{self, NativeWindow};

/// What the strip shows; a copy of the detached window's state.
#[derive(Clone, PartialEq)]
pub(super) struct StripState {
    pub title: SharedString,
    pub outline: bool,
}

pub(super) struct TitleStrip {
    standalone: WeakEntity<Standalone>,
    /// The click-through terminal window that follows the strip.
    follower: NativeWindow,
    state: StripState,
    _bounds: Subscription,
}

impl TitleStrip {
    /// Opens the strip over `bounds` (the detached window's title bar, logical pixels).
    pub(super) fn open(
        standalone: WeakEntity<Standalone>,
        follower: NativeWindow,
        bounds: Bounds<Pixels>,
        state: StripState,
        cx: &mut App,
    ) -> Option<(AnyWindowHandle, Entity<Self>)> {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(120.), TITLE_BAR_HEIGHT)),
            window_background: WindowBackgroundAppearance::Transparent,
            is_resizable: false,
            is_minimizable: false,
            focus: false,
            ..TitleBar::window_options()
        };
        let opened = gpui_kit::open_window(options, cx, move |window, cx| {
            window.set_window_title("MaomiTerminal passthrough");
            cx.new(|cx| {
                let bounds = cx.observe_window_bounds(window, |strip: &mut Self, window, _| {
                    strip.follow(window)
                });
                let this = cx.entity().downgrade();
                // Alt+F4 on the strip ends passthrough.
                window.on_window_should_close(cx, move |_, cx| {
                    if let Some(strip) = this.upgrade() {
                        let standalone = strip.read(cx).standalone.clone();
                        _ = standalone.update(cx, |s, cx| s.exit_passthrough(cx));
                    }
                    true
                });
                Self {
                    standalone,
                    follower,
                    state,
                    _bounds: bounds,
                }
            })
        });
        let (handle, strip) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                tracing::error!(%err, "failed to open the passthrough title strip");
                return None;
            }
        };
        _ = handle.update(cx, |_, window, cx| {
            // Only what the strip draws may show; the kit's root would paint an opaque theme
            // background (see the standalone window).
            gpui_kit::base::Root::update(window, cx, |root, _, cx| {
                root.style().background = Some(gpui_kit::transparent_black().into());
                cx.notify();
            });
            if let Some(strip) = window_ext::native(window) {
                window_ext::hide_from_taskbar(strip);
                window_ext::set_topmost(strip, true);
            }
            // Hiding and showing it again (for the taskbar) leaves it blank until it redraws.
            window.refresh();
        });
        Some((handle, strip))
    }

    pub(super) fn sync(&mut self, state: StripState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    /// The strip moved: the terminal window's top-left goes to the strip's.
    fn follow(&self, window: &Window) {
        if let Some(at) = window_ext::native(window).and_then(window_ext::origin) {
            window_ext::move_to(self.follower, at);
        }
    }
}

impl Render for TitleStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let outline = self.state.outline;
        let title = self.state.title.clone();
        let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
        let standalone = self.standalone.clone();
        let off = div()
            .id("passthrough-off")
            .occlude()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(26.))
            .rounded(px(7.))
            .text_color(p.accent.to_hsla())
            .hover(move |s| s.bg(hover).text_color(emph))
            .tooltip(|window, cx| {
                Tooltip::new("Passthrough is on: click to take clicks again").build(window, cx)
            })
            .on_click(move |_, _, cx| {
                _ = standalone.update(cx, |s, cx| s.exit_passthrough(cx));
            })
            .child(outlined(outline, p.accent.to_hsla(), |c| {
                Icon::new(gpui_kit::assets::IconName::MousePointer2Off)
                    .size(px(15.))
                    .text_color(c)
            }));
        // The whole strip drags the window (and the terminal with it), except the button. The
        // button sits on the left, away from the pane's close button under the right end.
        div()
            .id("title-strip")
            .window_control_area(WindowControlArea::Drag)
            .size_full()
            .flex()
            .items_center()
            .gap(px(10.))
            .pl(px(6.))
            .pr(px(12.))
            .text_size(px(12.))
            .child(off)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(outlined(outline, p.fg_emph.to_hsla(), |c| {
                        div().truncate().text_color(c).child(title.clone())
                    })),
            )
    }
}
