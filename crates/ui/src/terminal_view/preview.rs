//! Large preview of an inline Kitty image: a modal over the whole window showing the stored
//! full-resolution pixels. Click the picture to switch between fit-to-window and 100 % (drag or
//! wheel pans), click outside or press Esc to close.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    AnyElement, Bounds, ClipboardItem, Context, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _, Pixels, Point, RenderImage,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    anchored, canvas, deferred, div, point, px, size,
};
use maomi_term::KittyImage;

use super::TerminalView;
use crate::Theme;
use crate::color::Color;

/// Space kept around the picture; the bottom leaves room for the toolbar.
const MARGIN: Pixels = px(40.);
const TOOLBAR: Pixels = px(64.);
/// Pointer travel that turns a press on the picture into a pan instead of a zoom toggle.
const DRAG_SLOP: f32 = 4.;

pub(super) struct Preview {
    image: KittyImage,
    render: Arc<RenderImage>,
    /// Showing actual pixels (true) or the whole picture fit to the window.
    actual: bool,
    /// Offset of the picture's centre from the area's centre at 100 %.
    pan: Point<Pixels>,
    drag: Option<Drag>,
    /// Where the last frame drew: the area and the picture inside it.
    geometry: Rc<Cell<(Bounds<Pixels>, Bounds<Pixels>)>>,
}

struct Drag {
    from: Point<Pixels>,
    pan: Point<Pixels>,
    moved: bool,
}

impl Preview {
    /// Keeps a 100 % picture covering the area where it is larger, centred where smaller.
    fn clamp_pan(&mut self, scale: f32) {
        let (area, _) = self.geometry.get();
        let (w, h) = logical_size(&self.image, scale);
        let limit = |image: f32, area: Pixels| ((image - f32::from(area)) / 2.).max(0.);
        let (lx, ly) = (limit(w, area.size.width), limit(h, area.size.height));
        self.pan = point(
            px(f32::from(self.pan.x).clamp(-lx, lx)),
            px(f32::from(self.pan.y).clamp(-ly, ly)),
        );
    }
}

/// The picture's size in logical pixels at 100 % (one image pixel per device pixel).
fn logical_size(image: &KittyImage, scale: f32) -> (f32, f32) {
    (image.width as f32 / scale, image.height as f32 / scale)
}

/// Where the picture goes inside `area`.
fn picture_bounds(
    area: Bounds<Pixels>,
    image: (f32, f32),
    actual: bool,
    pan: Point<Pixels>,
) -> Bounds<Pixels> {
    let factor = if actual {
        1.
    } else {
        // Fit, enlarging small pictures too.
        (f32::from(area.size.width) / image.0).min(f32::from(area.size.height) / image.1)
    };
    let drawn = size(px(image.0 * factor), px(image.1 * factor));
    let center = area.center() + if actual { pan } else { Point::default() };
    Bounds::new(
        point(center.x - drawn.width / 2., center.y - drawn.height / 2.),
        drawn,
    )
}

impl TerminalView {
    /// Opens the preview for image `id` if the pane has it on screen.
    pub(super) fn open_preview(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(image) = self.session.image(id) else {
            return;
        };
        let Some(render) = self.pane.borrow().images.render_image(id) else {
            return;
        };
        self.preview = Some(Preview {
            image,
            render,
            actual: false,
            pan: Point::default(),
            drag: None,
            geometry: Rc::default(),
        });
        cx.notify();
    }

    pub(super) fn close_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn render_preview(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let preview = self.preview.as_ref()?;
        let p = Theme::current(cx).palette();
        let viewport = window.viewport_size();
        let scale = window.scale_factor();
        let geometry = preview.geometry.clone();
        let (render, actual, pan) = (preview.render.clone(), preview.actual, preview.pan);
        let image_size = logical_size(&preview.image, scale);

        let picture = canvas(
            |_, _, _| {},
            move |area, (), window, _| {
                let drawn = picture_bounds(area, image_size, actual, pan);
                geometry.set((area, drawn));
                let _ = window.paint_image(area, drawn, Default::default(), render, 0, false);
            },
        )
        .absolute()
        .top(MARGIN)
        .left(MARGIN)
        .right(MARGIN)
        .bottom(TOOLBAR);

        let zoom_label = if actual { "Fit to window" } else { "Zoom to 100%" };
        let mut toolbar = div()
            .absolute()
            .bottom(px(16.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .gap(px(8.))
            .child(button("preview-zoom", zoom_label, p, cx, |this, window, cx| {
                if let Some(preview) = &mut this.preview {
                    preview.actual = !preview.actual;
                    preview.pan = Point::default();
                    preview.clamp_pan(window.scale_factor());
                    cx.notify();
                }
            }))
            .child(button("preview-copy", "Copy image", p, cx, |this, _, cx| {
                if let Some(preview) = &this.preview {
                    copy_image(&preview.image, cx);
                }
            }));
        if preview.image.path.is_some() {
            toolbar = toolbar.child(button("preview-open", "Open file", p, cx, |this, _, cx| {
                if let Some(path) = this.preview.as_ref().and_then(|v| v.image.path.clone()) {
                    cx.open_with_system(&path);
                }
            }));
        }

        let overlay = div()
            .id("image-preview")
            .occlude()
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .bg(Color::new(0, 0, 0, 214).to_hsla())
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let Some(preview) = &mut this.preview else {
                        return;
                    };
                    let (area, drawn) = preview.geometry.get();
                    let on_picture = area.contains(&event.position) && drawn.contains(&event.position);
                    if on_picture {
                        preview.drag = Some(Drag {
                            from: event.position,
                            pan: preview.pan,
                            moved: false,
                        });
                    } else {
                        this.close_preview(cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let Some(preview) = &mut this.preview else {
                    return;
                };
                let Some(drag) = &mut preview.drag else {
                    return;
                };
                let delta = event.position - drag.from;
                if f32::from(delta.x).abs().max(f32::from(delta.y).abs()) > DRAG_SLOP {
                    drag.moved = true;
                }
                if drag.moved && preview.actual {
                    preview.pan = drag.pan + delta;
                    preview.clamp_pan(window.scale_factor());
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let Some(preview) = &mut this.preview else {
                        return;
                    };
                    if preview.drag.take().is_some_and(|drag| !drag.moved) {
                        preview.actual = !preview.actual;
                        preview.pan = Point::default();
                        preview.clamp_pan(window.scale_factor());
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                cx.stop_propagation();
                let Some(preview) = &mut this.preview else {
                    return;
                };
                if preview.actual {
                    preview.pan += event.delta.pixel_delta(px(20.));
                    preview.clamp_pan(window.scale_factor());
                    cx.notify();
                }
            }))
            .child(picture)
            .child(toolbar)
            .child(
                div()
                    .absolute()
                    .top(px(12.))
                    .right(px(16.))
                    .child(button("preview-close", "\u{2715}", p, cx, |this, _, cx| {
                        this.close_preview(cx);
                    })),
            );

        Some(
            deferred(anchored().position(point(px(0.), px(0.))).child(overlay))
                .with_priority(10)
                .into_any_element(),
        )
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    p: &crate::Palette,
    cx: &mut Context<TerminalView>,
    on_click: impl Fn(&mut TerminalView, &mut Window, &mut Context<TerminalView>) + 'static,
) -> impl IntoElement {
    let hover = p.surface_3.to_hsla();
    div()
        .id(id)
        .px(px(12.))
        .py(px(6.))
        .rounded(px(8.))
        .text_size(px(12.))
        .text_color(p.fg_emph.to_hsla())
        .bg(p.surface_2.to_hsla())
        .border_1()
        .border_color(p.line_strong.to_hsla())
        .hover(move |s| s.bg(hover))
        // The backdrop closes on press; a press here must not reach it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        .child(SharedString::from(label))
}

/// Puts the picture on the clipboard as a PNG.
fn copy_image(image: &KittyImage, cx: &mut Context<TerminalView>) {
    let Some(buffer) =
        ::image::RgbaImage::from_raw(image.width, image.height, image.rgba.as_ref().clone())
    else {
        return;
    };
    let mut png = std::io::Cursor::new(Vec::new());
    if buffer.write_to(&mut png, ::image::ImageFormat::Png).is_err() {
        return;
    }
    let image = gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, png.into_inner());
    cx.write_to_clipboard(ClipboardItem::new_image(&image));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(400.), px(200.)))
    }

    #[test]
    fn fit_enlarges_and_centres() {
        let drawn = picture_bounds(area(), (100., 100.), false, Point::default());
        assert_eq!(drawn.size, size(px(200.), px(200.)));
        assert_eq!(drawn.origin, point(px(100.), px(0.)));
    }

    #[test]
    fn actual_size_follows_pan() {
        let drawn = picture_bounds(area(), (800., 100.), true, point(px(50.), px(0.)));
        assert_eq!(drawn.size, size(px(800.), px(100.)));
        assert_eq!(drawn.origin, point(px(-150.), px(50.)));
    }
}
