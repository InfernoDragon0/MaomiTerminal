//! Paints one row of the lane graph: lanes crossing the row, the half-row edges into and out of
//! the node, then the node on top. Port of GitDragon's `graph/lanes.rs`; the sizes are a
//! [`Geometry`] so the narrow Repo dock and the full Repo tab share the painter.

use gpui_kit::{
    Bounds, ContentMask, Hsla, PathBuilder, Pixels, Window, canvas, fill, point, prelude::*, size,
};
use maomi_git::graph::{Edge, EdgeKind, History, Node, RowKind};

use crate::Palette;

/// Lane sizes in pixels.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    /// Distance between lane centres.
    pub lane: f32,
    /// Space left of the first lane and right of the last.
    pub pad: f32,
    pub stroke: f32,
    /// Dot radius.
    pub node: f32,
}

/// The Repo tab's graph column (GitDragon's sizes).
pub const FULL: Geometry = Geometry {
    lane: 24.,
    pad: 6.,
    stroke: 2.,
    node: 5.,
};

/// The Repo dock's narrow graph column.
pub const COMPACT: Geometry = Geometry {
    lane: 9.,
    pad: 4.,
    stroke: 1.5,
    node: 3.5,
};

/// Opacity of the other lanes while one is hovered.
const DIMMED: f32 = 0.25;
/// Opacity of edges leading into hidden lanes.
const FADED: f32 = 0.35;

/// Colours the painter needs, copied out of the palette so the canvas owns them.
#[derive(Debug, Clone)]
pub struct LaneColors {
    lanes: Vec<Hsla>,
    /// Behind the dots, so they read apart from the lines.
    panel: Hsla,
    /// The gutter's edge.
    border: Hsla,
}

impl LaneColors {
    pub fn new(p: &Palette, panel: Hsla) -> Self {
        Self {
            lanes: p.c.iter().map(|c| c.to_hsla()).collect(),
            panel,
            border: p.line.to_hsla(),
        }
    }

    fn lane(&self, color: u32) -> Hsla {
        self.lanes[color as usize % self.lanes.len()]
    }
}

/// Everything needed to paint one row, copied out of the [`History`] so the canvas owns it.
#[derive(Debug, Clone)]
pub struct RowLanes {
    node: Node,
    edges: Vec<Edge>,
    /// Lanes crossing the whole row: (column, colour).
    through: Vec<(u32, u32)>,
    merge: bool,
    head: bool,
    kind: RowKind,
}

impl RowLanes {
    /// The commit has two or more parents.
    #[allow(dead_code)]
    pub fn is_merge(&self) -> bool {
        self.merge
    }
}

/// Lanes for each row in `rows`, from one viewport query.
pub fn rows_lanes(history: &History, rows: std::ops::Range<usize>) -> Vec<RowLanes> {
    let rows = rows.start..rows.end.min(history.len());
    let layout = history.layout();
    let mut out: Vec<RowLanes> = rows
        .clone()
        .filter_map(|r| {
            let node = layout.node(r)?;
            Some(RowLanes {
                node,
                edges: layout.edges(r).to_vec(),
                through: Vec::new(),
                merge: false,
                head: history.head() == Some(r),
                kind: history.kind(r),
            })
        })
        .collect();
    for span in layout.spans(rows.clone()) {
        let first = (span.start as usize + 1).max(rows.start);
        let last = (span.end as usize).min(rows.end);
        for r in first..last {
            out[r - rows.start].through.push((span.column, span.color));
        }
    }
    for lanes in &mut out {
        let straight = lanes.through.iter().any(|&(c, _)| c == lanes.node.column);
        let outs = lanes
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Out)
            .count();
        lanes.merge = outs + usize::from(straight) > 1;
    }
    out
}

/// Width of the graph column for a layout `width` columns wide, capped at `cap` columns.
pub fn column_width(width: u32, cap: u32, geo: Geometry) -> Pixels {
    gpui_kit::px(geo.pad * 2. + geo.lane * width.clamp(1, cap) as f32)
}

/// The gutter column when a layout `width` columns wide does not fit in `cap` columns.
pub fn gutter(width: u32, cap: u32) -> Option<u32> {
    (width > cap).then(|| cap - 1)
}

/// A canvas filling its parent that paints `lanes`. With a `hovered` lane colour, that branch
/// is drawn thicker on top and every other one dimmed. `gutter`: the column hidden lanes
/// collapse into, when the history is too wide (see [`gutter`]).
pub fn lanes_canvas(
    lanes: RowLanes,
    colors: LaneColors,
    geo: Geometry,
    hovered: Option<u32>,
    gutter: Option<u32>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                let pass = Pass {
                    geo,
                    bounds,
                    hovered,
                    gutter,
                };
                // Dimmed lanes first so the hovered branch stays on top where lines cross.
                if hovered.is_some() {
                    paint(&lanes, &colors, &pass, false, window);
                }
                paint(&lanes, &colors, &pass, true, window);
            });
        },
    )
    .size_full()
}

/// Centre x of `column`, relative to the graph cell's left edge.
fn lane_x(column: u32, geo: Geometry) -> Pixels {
    gpui_kit::px(geo.pad + geo.lane * (column as f32 + 0.5))
}

/// Is `column` past the gutter's start (not drawn as its own lane)?
fn hidden(column: u32, gutter: Option<u32>) -> bool {
    gutter.is_some_and(|g| column >= g)
}

/// Where `column` is drawn: its own lane, or the gutter when hidden.
fn shown_x(column: u32, gutter: Option<u32>, geo: Geometry) -> Pixels {
    lane_x(gutter.filter(|&g| column >= g).unwrap_or(column), geo)
}

#[derive(Clone, Copy)]
struct Pass {
    geo: Geometry,
    bounds: Bounds<Pixels>,
    hovered: Option<u32>,
    gutter: Option<u32>,
}

/// Paints the hovered branch (`front`) or everything else (`!front`); with nothing hovered,
/// `front` paints it all at full strength.
fn paint(lanes: &RowLanes, colors: &LaneColors, pass: &Pass, front: bool, window: &mut Window) {
    let Pass {
        geo,
        bounds,
        hovered,
        gutter,
    } = *pass;
    let px = gpui_kit::px;
    let x = |column: u32| bounds.left() + shown_x(column, gutter, geo);
    let hidden = |column: u32| hidden(column, gutter);
    let top = bounds.top();
    let bottom = bounds.bottom();
    let mid = top + bounds.size.height / 2.;
    // In this pass? Then its colour and stroke.
    let style = |c: u32| -> Option<(Hsla, Pixels)> {
        let lane = colors.lane(c);
        match hovered {
            None => front.then_some((lane, px(geo.stroke))),
            Some(h) if h == c => front.then_some((lane, px(geo.stroke + 1.))),
            Some(_) => (!front).then_some((lane.opacity(DIMMED), px(geo.stroke))),
        }
    };

    let vline =
        |window: &mut Window, column: u32, from: Pixels, to: Pixels, (c, w): (Hsla, Pixels)| {
            window.paint_quad(fill(
                Bounds::new(point(x(column) - w / 2., from), size(w, to - from)),
                c,
            ));
        };

    // The gutter's edge, so the cut-off reads as a boundary.
    if let (Some(g), true) = (gutter, front) {
        let edge = bounds.left() + lane_x(g, geo) - px(geo.lane / 2.);
        window.paint_quad(fill(
            Bounds::new(point(edge, top), size(px(1.), bottom - top)),
            colors.border,
        ));
    }
    for &(column, c) in &lanes.through {
        if hidden(column) {
            continue;
        }
        if let Some(style) = style(c) {
            vline(window, column, top, bottom, style);
        }
    }
    let node_x = x(lanes.node.column);
    for edge in &lanes.edges {
        let Some((mut c, w)) = style(edge.color) else {
            continue;
        };
        match (hidden(edge.column), hidden(lanes.node.column)) {
            // Both in the gutter: nothing to show.
            (true, true) => continue,
            // Leading off into a hidden lane: fade out.
            (true, false) => c = c.opacity(c.a * FADED),
            _ => {}
        }
        let lane_x = x(edge.column);
        let (from, to) = match edge.kind {
            EdgeKind::Into => (point(lane_x, top), point(node_x, mid)),
            EdgeKind::Out => (point(node_x, mid), point(lane_x, bottom)),
        };
        if edge.column == lanes.node.column {
            vline(window, edge.column, from.y, to.y, (c, w));
            continue;
        }
        // Elbow: leaves the lane vertically, meets the node horizontally.
        let mut path = PathBuilder::stroke(w);
        path.move_to(from);
        path.curve_to(to, point(lane_x, mid));
        if let Ok(path) = path.build() {
            window.paint_path(path, c);
        }
    }

    // Dot inside a panel-coloured ring that separates it from the lines; merges are hollow.
    let Some((c, w)) = style(lanes.node.color) else {
        return;
    };
    let grow = if hovered == Some(lanes.node.color) {
        1.
    } else {
        0.
    };
    let r = px(if lanes.head { geo.node + 1. } else { geo.node } + grow);
    let ring = px(geo.stroke);
    let disc = |r: Pixels| Bounds::new(point(node_x - r, mid - r), size(r * 2., r * 2.));
    window.paint_quad(fill(disc(r + ring), colors.panel).corner_radii(r + ring));
    // WIP: a hollow ring in the panel colour's place; a stash: a hollow rounded square.
    if lanes.kind != RowKind::Commit {
        let square = matches!(lanes.kind, RowKind::Stash(_));
        let outline = fill(disc(r), colors.panel)
            .border_widths(w)
            .border_color(c.opacity(c.a * 0.85));
        window.paint_quad(outline.corner_radii(if square { px(2.) } else { r }));
        return;
    }
    let dot = if lanes.merge {
        fill(disc(r), colors.panel).border_widths(w).border_color(c)
    } else {
        fill(disc(r), c)
    };
    window.paint_quad(dot.corner_radii(r));
    // A commit of a hidden lane, pinned in the gutter: an arrow points at it.
    if hidden(lanes.node.column) {
        let tip = node_x - r - px(geo.stroke + 2.);
        let back = tip - px(4.);
        let mut arrow = PathBuilder::fill();
        arrow.move_to(point(back, mid - px(3.5)));
        arrow.line_to(point(tip, mid));
        arrow.line_to(point(back, mid + px(3.5)));
        arrow.close();
        if let Ok(arrow) = arrow.build() {
            window.paint_path(arrow, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use maomi_git::parse::log::{Commit, Signature};

    use super::*;

    fn commit(id: &str, parents: &[&str]) -> Commit {
        let sig = Signature {
            name: "t".into(),
            email: "t@e".into(),
            time: 0,
            tz_offset_minutes: 0,
        };
        Commit {
            id: format!("{id:0>40}"),
            parents: parents.iter().map(|p| format!("{p:0>40}")).collect(),
            author: sig.clone(),
            committer: sig,
            subject: "s".into(),
            body: Default::default(),
            refs: Vec::new(),
            head_target: None,
        }
    }

    /// m merges f; f runs beside main's c down to their base b.
    fn history() -> History {
        let mut h = History::new();
        h.extend([
            commit("m", &["c", "f"]),
            commit("f", &["b"]),
            commit("c", &["b"]),
            commit("b", &[]),
        ]);
        h
    }

    #[test]
    fn crossing_lanes_per_row() {
        let rows = rows_lanes(&history(), 0..4);
        assert_eq!(rows.len(), 4);
        // Row 1 (f, column 1): main's lane crosses in column 0, f's own lane runs through.
        assert_eq!(rows[1].node.column, 1);
        let mut through: Vec<u32> = rows[1].through.iter().map(|&(c, _)| c).collect();
        through.sort_unstable();
        assert_eq!(through, [0, 1]);
        assert_eq!(rows[2].through.len(), 2);
        assert!(rows[3].through.is_empty());
    }

    #[test]
    fn merge_flag_and_clamped_range() {
        let rows = rows_lanes(&history(), 0..10);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].merge);
        assert!(!rows[1].merge && !rows[2].merge && !rows[3].merge);
        let tail = rows_lanes(&history(), 2..4);
        assert_eq!(tail[0].through.len(), 2);
    }

    #[test]
    fn column_width_and_gutter_follow_the_cap() {
        assert_eq!(column_width(1, 4, COMPACT), gpui_kit::px(17.));
        assert_eq!(column_width(9, 4, COMPACT), gpui_kit::px(44.));
        assert_eq!(gutter(4, 4), None);
        assert_eq!(gutter(5, 4), Some(3));
    }
}
