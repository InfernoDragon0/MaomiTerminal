//! The GPUI element that paints a terminal grid and turns mouse input into selection,
//! scrolling, mouse reports and link clicks.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::{
    App, BorderStyle, Bounds, CursorStyle as MouseCursor, DispatchPhase, Element, ElementId,
    FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId,
    IntoElement, LayoutId, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Size, StrikethroughStyle,
    Style, TextAlign, TextRun, UnderlineStyle, Window, fill, font, outline, point, px, relative,
    size,
};
use maomi_term::alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use maomi_term::alacritty_terminal::selection::{Selection, SelectionType};
use maomi_term::{CellFlags, CursorShape, GridSnapshot, PtySession, TermMode, TermSize};

use super::builtin;
use super::input::{self, Link, ReportButton, ReportKind};
use super::layout::{self, TextStyle, UnderlineKind};
use super::smooth::SmoothScroll;
use crate::color::Color;
use crate::terminal::TerminalColors;

/// Inner padding between the pane edge and the grid.
const PADDING: Size<Pixels> = Size {
    width: px(10.0),
    height: px(6.0),
};
const SCROLLBAR_WIDTH: Pixels = px(4.0);
const SCROLLBAR_MIN_THUMB: Pixels = px(24.0);

/// Font settings for the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFont {
    pub family: SharedString,
    pub size: Pixels,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
}

impl Default for TerminalFont {
    fn default() -> Self {
        Self {
            family: crate::fonts::TERMINAL_FAMILY.into(),
            size: px(13.0),
            line_height: 1.4,
        }
    }
}

/// Per-pane state that outlives a frame; the view owns it, the element borrows it.
#[derive(Debug, Default)]
pub(crate) struct PaneState {
    /// Wheel movement smaller than a line, carried between events.
    scroll_remainder: f32,
    smooth: SmoothScroll,
    /// A left-button drag is extending the selection.
    selecting: bool,
    /// Button held while the program receives mouse reports.
    reporting: Option<ReportButton>,
    last_report_cell: Option<(usize, usize)>,
    /// Link under the pointer while Ctrl (Cmd on macOS) is held.
    hovered_link: Option<Link>,
}

/// Paints `session`'s grid, resizing the PTY to fit.
pub struct TerminalElement {
    session: Arc<PtySession>,
    colors: TerminalColors,
    font: TerminalFont,
    focused: bool,
    /// Blink phase; false hides a focused cursor.
    cursor_on: bool,
    smooth_scroll: bool,
    state: Rc<RefCell<PaneState>>,
}

impl TerminalElement {
    pub(crate) fn new(
        session: Arc<PtySession>,
        colors: TerminalColors,
        font: TerminalFont,
        focused: bool,
        cursor_on: bool,
        smooth_scroll: bool,
        state: Rc<RefCell<PaneState>>,
    ) -> Self {
        Self {
            session,
            colors,
            font,
            focused,
            cursor_on,
            smooth_scroll,
            state,
        }
    }
}

/// Where the grid sits and how big its cells are; maps pointer positions to cells.
#[derive(Debug, Clone, Copy)]
struct GridGeometry {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    columns: usize,
    rows: usize,
}

impl GridGeometry {
    /// Cell under `position`, clamped to the grid, plus which half of the cell it is in.
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize, Side) {
        let x = f32::from(position.x - self.origin.x).max(0.);
        let y = f32::from(position.y - self.origin.y).max(0.);
        let cw = f32::from(self.cell_width);
        let col = ((x / cw) as usize).min(self.columns.saturating_sub(1));
        let row = ((y / f32::from(self.line_height)) as usize).min(self.rows.saturating_sub(1));
        let side = if x - col as f32 * cw > cw / 2. {
            Side::Right
        } else {
            Side::Left
        };
        (col, row, side)
    }
}

pub struct LayoutState {
    hitbox: Hitbox,
    geometry: GridGeometry,
    snapshot: Rc<GridSnapshot>,
    /// Draw position of viewport row 0, including the smooth-scroll offset.
    rows_top: Pixels,
    pane: PaintQuad,
    backgrounds: Vec<PaintQuad>,
    selections: Vec<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    builtin: Vec<builtin::Shape>,
    cursor: Option<PaintQuad>,
    /// The glyph redrawn on top of a block cursor.
    cursor_text: Option<(Point<Pixels>, ShapedLine)>,
    scrollbar: Option<PaintQuad>,
    started: Instant,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> LayoutState {
        let started = Instant::now();
        let _span = tracing::trace_span!("terminal_prepaint").entered();
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        let base_font = font(self.font.family.clone());
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&base_font);
        let font_size = self.font.size;
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);
        let line_height = (font_size * self.font.line_height).round();

        // Fit the grid to the element, then read the grid at that size.
        let grid_origin = bounds.origin + point(PADDING.width, PADDING.height);
        let grid_size = size(
            (bounds.size.width - PADDING.width * 2.).max(px(0.)),
            (bounds.size.height - PADDING.height * 2.).max(px(0.)),
        );
        let columns = (grid_size.width / cell_width).floor().max(2.) as u16;
        let rows = (grid_size.height / line_height).floor().max(1.) as u16;
        self.session.resize(TermSize {
            columns,
            rows,
            cell_width: f32::from(cell_width).round() as u16,
            cell_height: f32::from(line_height).round() as u16,
        });

        // Smooth scroll: draw the content `offset` lines low, with history rows filling the gap.
        let mut offset = 0.;
        let snapshot = self.session.snapshot_with_rows_above(|viewport| {
            offset = self
                .state
                .borrow_mut()
                .smooth
                .update(viewport, started, self.smooth_scroll);
            offset.ceil() as usize
        });
        if self.state.borrow().smooth.is_animating() {
            window.request_animation_frame();
        }
        let rows_above = snapshot.rows_above as f32;
        // Whole device pixels: sub-pixel offsets make glyphs shimmer while they move.
        let scale = window.scale_factor();
        let rows_top =
            px((f32::from(grid_origin.y + line_height * offset) * scale).round() / scale);
        let cell_origin = |row: usize, col: usize| {
            point(
                grid_origin.x + cell_width * col as f32,
                rows_top + line_height * (row as f32 - rows_above),
            )
        };
        let span_bounds = |span: layout::Span| {
            Bounds::new(
                cell_origin(span.row, span.col),
                size(cell_width * span.cols as f32, line_height),
            )
        };

        let grid_layout = layout::layout_grid(&snapshot, &self.colors);
        let backgrounds = grid_layout
            .backgrounds
            .iter()
            .map(|&(span, color)| fill(span_bounds(span), color.to_hsla()))
            .collect();
        let selection = self.colors.selection.to_hsla();
        let selections = grid_layout
            .selections
            .iter()
            .map(|&span| fill(span_bounds(span), selection))
            .collect();

        let shape = |text: SharedString, style: &TextStyle, color: Option<Color>| {
            let run = text_run(&base_font, text.len(), style, color);
            text_system.shape_line(text, font_size, &[run], Some(cell_width))
        };
        let lines = grid_layout
            .batches
            .iter()
            .map(|batch| {
                (
                    cell_origin(batch.row, batch.col),
                    shape(batch.text.clone().into(), &batch.style, None),
                )
            })
            .collect();

        let builtin = grid_layout
            .builtin
            .iter()
            .flat_map(|&(row, col, c, color)| {
                let cell = Bounds::new(cell_origin(row, col), size(cell_width, line_height));
                builtin::shapes(c, cell, color, scale)
            })
            .collect();

        // Cursor: unfocused panes show a hollow block; a focused one blinks.
        let mut cursor = None;
        let mut cursor_text = None;
        if let Some(c) = snapshot.cursor
            && (!self.focused || self.cursor_on)
        {
            let cell = snapshot.cell(c.row, c.col);
            let wide = cell.is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR));
            let width = cell_width * if wide { 2. } else { 1. };
            let origin = cell_origin(c.row, c.col);
            let color = self.colors.cursor.to_hsla();
            let shape_kind = if self.focused {
                c.shape
            } else {
                CursorShape::HollowBlock
            };
            cursor = match shape_kind {
                CursorShape::Block => {
                    Some(fill(Bounds::new(origin, size(width, line_height)), color))
                }
                CursorShape::Beam => {
                    Some(fill(Bounds::new(origin, size(px(2.), line_height)), color))
                }
                CursorShape::Underline => Some(fill(
                    Bounds::new(
                        origin + point(px(0.), line_height - px(2.)),
                        size(width, px(2.)),
                    ),
                    color,
                )),
                CursorShape::HollowBlock => Some(outline(
                    Bounds::new(origin, size(width, line_height)),
                    color,
                    BorderStyle::Solid,
                )),
                CursorShape::Hidden => None,
            };
            if shape_kind == CursorShape::Block
                && let Some(cell) = cell
                && cell.c != ' '
            {
                let (fg, _) = layout::cell_colors(cell, &self.colors);
                let style = TextStyle {
                    fg,
                    bold: cell.flags.contains(CellFlags::BOLD),
                    italic: cell.flags.contains(CellFlags::ITALIC),
                    underline: None,
                    strike: false,
                };
                let mut text = String::from(cell.c);
                text.extend(&cell.zerowidth);
                cursor_text = Some((
                    origin,
                    shape(text.into(), &style, Some(self.colors.cursor_text)),
                ));
            }
        }

        // Scrollbar thumb while looking at history.
        let scrollbar = (snapshot.display_offset > 0).then(|| {
            let total = (snapshot.history_size + snapshot.screen_lines) as f32;
            let track = bounds.size.height;
            let thumb = (track * (snapshot.screen_lines as f32 / total))
                .max(SCROLLBAR_MIN_THUMB)
                .min(track);
            // 0 at the oldest line, 1 when following output.
            let position =
                1. - snapshot.display_offset as f32 / snapshot.history_size.max(1) as f32;
            let top = (track - thumb) * position;
            let mut quad = fill(
                Bounds::new(
                    point(
                        bounds.right() - SCROLLBAR_WIDTH - px(2.),
                        bounds.top() + top,
                    ),
                    size(SCROLLBAR_WIDTH, thumb),
                ),
                self.colors.fg.with_alpha(90).to_hsla(),
            );
            quad.corner_radii = (SCROLLBAR_WIDTH / 2.).into();
            quad
        });

        LayoutState {
            hitbox,
            geometry: GridGeometry {
                origin: grid_origin,
                cell_width,
                line_height,
                columns: columns as usize,
                rows: rows as usize,
            },
            snapshot: Rc::new(snapshot),
            rows_top,
            pane: fill(bounds, self.colors.bg.to_hsla()),
            backgrounds,
            selections,
            lines,
            builtin,
            cursor,
            cursor_text,
            scrollbar,
            started,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut LayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _span = tracing::trace_span!("terminal_paint").entered();
        let geometry = layout.geometry;
        let hovered_link = self.state.borrow().hovered_link.clone();
        let mouse_cursor = if hovered_link.is_some() {
            MouseCursor::PointingHand
        } else {
            MouseCursor::IBeam
        };
        window.set_cursor_style(mouse_cursor, &layout.hitbox);

        window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
            window.paint_quad(layout.pane.clone());
            for quad in layout.backgrounds.drain(..) {
                window.paint_quad(quad);
            }
            for quad in layout.selections.drain(..) {
                window.paint_quad(quad);
            }
            let line_height = geometry.line_height;
            for (origin, line) in &layout.lines {
                let _ = line.paint(*origin, line_height, TextAlign::Left, None, window, cx);
            }
            for shape in layout.builtin.drain(..) {
                match shape.clip {
                    Some(clip) => window.with_content_mask(
                        Some(gpui_kit::ContentMask { bounds: clip }),
                        |window| window.paint_quad(shape.quad),
                    ),
                    None => window.paint_quad(shape.quad),
                }
            }
            if let Some(link) = &hovered_link {
                let x = geometry.origin.x + geometry.cell_width * link.start as f32;
                let y = layout.rows_top + line_height * (link.row as f32 + 1.) - px(2.);
                let width = geometry.cell_width * (link.end - link.start) as f32;
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(width, px(1.))),
                    self.colors.cursor.to_hsla(),
                ));
            }
            if let Some(quad) = layout.cursor.take() {
                window.paint_quad(quad);
            }
            if let Some((origin, line)) = &layout.cursor_text {
                let _ = line.paint(*origin, line_height, TextAlign::Left, None, window, cx);
            }
            if let Some(quad) = layout.scrollbar.take() {
                window.paint_quad(quad);
            }
        });
        perf::record(layout.started.elapsed());

        self.register_mouse(layout, window);
    }
}

impl TerminalElement {
    fn register_mouse(&self, layout: &LayoutState, window: &mut Window) {
        let geometry = layout.geometry;
        let mode = layout.snapshot.mode;
        let display_offset = layout.snapshot.display_offset;
        let reporting =
            move |modifiers: &Modifiers| mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift;
        let grid_point = move |col: usize, row: usize| {
            GridPoint::new(Line(row as i32 - display_offset as i32), Column(col))
        };

        // Wheel: mouse reports, arrow keys in full-screen programs, or history scroll.
        {
            let (session, hitbox, state) = (
                self.session.clone(),
                layout.hitbox.clone(),
                self.state.clone(),
            );
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let line_height = f32::from(geometry.line_height);
                let mut state = state.borrow_mut();
                let dy = f32::from(event.delta.pixel_delta(geometry.line_height).y)
                    + state.scroll_remainder;
                let lines = (dy / line_height).trunc();
                state.scroll_remainder = dy - lines * line_height;
                if lines != 0. {
                    if reporting(&event.modifiers) {
                        let (col, row, _) = geometry.cell_at(event.position);
                        let button = if lines > 0. {
                            ReportButton::WheelUp
                        } else {
                            ReportButton::WheelDown
                        };
                        for _ in 0..lines.abs() as usize {
                            if let Some(bytes) = input::encode_mouse(
                                button,
                                ReportKind::Press,
                                col,
                                row,
                                &event.modifiers,
                                mode,
                            ) {
                                session.write(bytes);
                            }
                        }
                    } else {
                        scroll(&session, mode, lines as i32);
                    }
                }
                cx.stop_propagation();
            });
        }

        // Press: Ctrl+click opens a link, else report or start a selection.
        {
            let (session, hitbox, state) = (
                self.session.clone(),
                layout.hitbox.clone(),
                self.state.clone(),
            );
            let snapshot = layout.snapshot.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let (col, row, side) = geometry.cell_at(event.position);
                if event.button == MouseButton::Left
                    && link_modifier(&event.modifiers)
                    && let Some(link) = input::link_at(&snapshot, snapshot.rows_above + row, col)
                {
                    cx.open_url(&link.url);
                    return;
                }
                let mut state = state.borrow_mut();
                if reporting(&event.modifiers) {
                    let button = match event.button {
                        MouseButton::Left => ReportButton::Left,
                        MouseButton::Middle => ReportButton::Middle,
                        MouseButton::Right => ReportButton::Right,
                        _ => return,
                    };
                    if let Some(bytes) = input::encode_mouse(
                        button,
                        ReportKind::Press,
                        col,
                        row,
                        &event.modifiers,
                        mode,
                    ) {
                        session.write(bytes);
                    }
                    state.reporting = Some(button);
                    state.last_report_cell = Some((col, row));
                    return;
                }
                if event.button != MouseButton::Left {
                    return;
                }
                let point = grid_point(col, row);
                let kind = match event.click_count {
                    2 => SelectionType::Semantic,
                    n if n >= 3 => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                let extend = event.modifiers.shift && event.click_count == 1;
                session.with_term(|term| match (&mut term.selection, extend) {
                    (Some(selection), true) => selection.update(point, side),
                    _ => term.selection = Some(Selection::new(kind, point, side)),
                });
                state.selecting = true;
                window.refresh();
            });
        }

        // Move: extend the selection, report motion, track the hovered link.
        {
            let (session, hitbox, state) = (
                self.session.clone(),
                layout.hitbox.clone(),
                self.state.clone(),
            );
            let snapshot = layout.snapshot.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let (col, row, side) = geometry.cell_at(event.position);
                let mut state = state.borrow_mut();
                if state.selecting && event.pressed_button == Some(MouseButton::Left) {
                    let point = grid_point(col, row);
                    session.with_term(|term| {
                        if let Some(selection) = &mut term.selection {
                            selection.update(point, side);
                        }
                    });
                }
                let wants_motion = match event.pressed_button {
                    Some(_) => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
                    None => mode.contains(TermMode::MOUSE_MOTION) && hitbox.is_hovered(window),
                };
                if reporting(&event.modifiers)
                    && wants_motion
                    && state.last_report_cell != Some((col, row))
                {
                    let button = if event.pressed_button.is_some() {
                        state.reporting.unwrap_or(ReportButton::Left)
                    } else {
                        ReportButton::None
                    };
                    if let Some(bytes) = input::encode_mouse(
                        button,
                        ReportKind::Motion,
                        col,
                        row,
                        &event.modifiers,
                        mode,
                    ) {
                        session.write(bytes);
                    }
                    state.last_report_cell = Some((col, row));
                }

                let link = (hitbox.is_hovered(window) && link_modifier(&event.modifiers))
                    .then(|| input::link_at(&snapshot, snapshot.rows_above + row, col))
                    .flatten()
                    .map(|link| Link {
                        row: link.row - snapshot.rows_above,
                        ..link
                    });
                if link != state.hovered_link {
                    state.hovered_link = link;
                    window.refresh();
                }
            });
        }

        // Release: finish the selection or report the release.
        {
            let (session, state) = (self.session.clone(), self.state.clone());
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, _cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let mut state = state.borrow_mut();
                if let Some(button) = state.reporting.take() {
                    let (col, row, _) = geometry.cell_at(event.position);
                    if let Some(bytes) = input::encode_mouse(
                        button,
                        ReportKind::Release,
                        col,
                        row,
                        &event.modifiers,
                        mode,
                    ) {
                        session.write(bytes);
                    }
                    state.last_report_cell = None;
                }
                if std::mem::take(&mut state.selecting) {
                    // A click without a drag leaves an empty selection; drop it.
                    session.with_term(|term| {
                        if term.selection.as_ref().is_some_and(Selection::is_empty) {
                            term.selection = None;
                        }
                    });
                }
            });
        }
    }
}

/// Ctrl on Windows/Linux, Cmd on macOS.
fn link_modifier(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// Per-thread frame stats, logged once a second at debug level (target `maomi_ui::perf`).
mod perf {
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    struct Stats {
        since: Instant,
        panes_painted: u32,
        total: Duration,
        worst: Duration,
    }

    thread_local! {
        static STATS: RefCell<Stats> = RefCell::new(Stats {
            since: Instant::now(),
            panes_painted: 0,
            total: Duration::ZERO,
            worst: Duration::ZERO,
        });
    }

    /// Records the prepaint + paint time of one pane.
    pub(super) fn record(spent: Duration) {
        if !tracing::enabled!(target: "maomi_ui::perf", tracing::Level::DEBUG) {
            return;
        }
        STATS.with_borrow_mut(|s| {
            s.panes_painted += 1;
            s.total += spent;
            s.worst = s.worst.max(spent);
            let elapsed = s.since.elapsed();
            if elapsed >= Duration::from_secs(1) {
                tracing::debug!(
                    target: "maomi_ui::perf",
                    panes_per_sec = (f64::from(s.panes_painted) / elapsed.as_secs_f64()).round(),
                    avg_us = s.total.as_micros() / u128::from(s.panes_painted),
                    worst_us = s.worst.as_micros(),
                    "terminal frames"
                );
                *s = Stats {
                    since: Instant::now(),
                    panes_painted: 0,
                    total: Duration::ZERO,
                    worst: Duration::ZERO,
                };
            }
        });
    }
}

/// Scrolls history, or sends arrow keys to full-screen programs (less, vim) that asked for
/// alternate scroll and do not track the mouse themselves.
fn scroll(session: &PtySession, mode: TermMode, lines: i32) {
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        let app = mode.contains(TermMode::APP_CURSOR);
        let key: &[u8] = match (lines > 0, app) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        session.write(key.repeat(lines.unsigned_abs() as usize));
    } else {
        session.scroll(lines);
    }
}

fn text_run(base: &gpui_kit::Font, len: usize, style: &TextStyle, color: Option<Color>) -> TextRun {
    let mut font = base.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    let color: Hsla = color.unwrap_or(style.fg).to_hsla();
    TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: style.underline.map(|(kind, color)| UnderlineStyle {
            thickness: if kind == UnderlineKind::Double {
                px(2.)
            } else {
                px(1.)
            },
            color: Some(color.to_hsla()),
            wavy: kind == UnderlineKind::Curly,
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    }
}
