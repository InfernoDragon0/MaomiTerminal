//! Block elements (U+2580-259F) and common box-drawing lines (U+2500-257F) drawn as shapes
//! that fill the whole cell. Font glyphs for these leave gaps when the line height is taller
//! than the font and seams where glyph edges fall between pixels.

use gpui_kit::{
    Bounds, Corners, Edges, PaintQuad, Pixels, fill, point, px, size, transparent_black,
};

use crate::color::Color;

/// Arm weights of a box-drawing character: left, right, up, down (0 none, 1 light, 2 heavy).
type Arms = [u8; 4];

/// Whether `c` is drawn here instead of by the font.
pub(crate) fn is_builtin(c: char) -> bool {
    matches!(c as u32, 0x2580..=0x259F) || box_arms(c).is_some() || rounded_corner(c).is_some()
}

/// One shape: a quad, plus the cell it must be clipped to (rounded corners overhang).
pub(crate) struct Shape {
    pub quad: PaintQuad,
    pub clip: Option<Bounds<Pixels>>,
}

/// Shapes for `c` in `cell`, with edges snapped to device pixels at `scale`.
pub(crate) fn shapes(c: char, cell: Bounds<Pixels>, color: Color, scale: f32) -> Vec<Shape> {
    let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
    let x0 = snap(cell.left());
    let x1 = snap(cell.right());
    let y0 = snap(cell.top());
    let y1 = snap(cell.bottom());
    let (w, h) = (x1 - x0, y1 - y0);
    // A rectangle given in fractions of the cell.
    let rect = |fx: f32, fy: f32, fw: f32, fh: f32| {
        let left = snap(x0 + w * fx);
        let top = snap(y0 + h * fy);
        let right = snap(x0 + w * (fx + fw));
        let bottom = snap(y0 + h * (fy + fh));
        Bounds::new(point(left, top), size(right - left, bottom - top))
    };
    let solid = |b: Bounds<Pixels>, color: Color| Shape {
        quad: fill(b, color.to_hsla()),
        clip: None,
    };

    let code = c as u32;
    if let 0x2580..=0x259F = code {
        let eighth = 1. / 8.;
        let shape = match code {
            0x2580 => rect(0., 0., 1., 0.5),
            0x2581..=0x2587 => {
                let n = (code - 0x2580) as f32;
                rect(0., 1. - n * eighth, 1., n * eighth)
            }
            0x2588 => rect(0., 0., 1., 1.),
            0x2589..=0x258F => rect(0., 0., (0x2590 - code) as f32 * eighth, 1.),
            0x2590 => rect(0.5, 0., 0.5, 1.),
            0x2591..=0x2593 => {
                let alpha = [64, 128, 191][(code - 0x2591) as usize];
                return vec![solid(rect(0., 0., 1., 1.), color.with_alpha(alpha))];
            }
            0x2594 => rect(0., 0., 1., eighth),
            0x2595 => rect(1. - eighth, 0., eighth, 1.),
            _ => {
                // Quadrants: bits are upper-left, upper-right, lower-left, lower-right.
                let bits: u8 = match code {
                    0x2596 => 0b0010,
                    0x2597 => 0b0001,
                    0x2598 => 0b1000,
                    0x2599 => 0b1011,
                    0x259A => 0b1001,
                    0x259B => 0b1110,
                    0x259C => 0b1101,
                    0x259D => 0b0100,
                    0x259E => 0b0110,
                    _ => 0b0111, // 0x259F
                };
                let quads = [
                    (0b1000, 0., 0.),
                    (0b0100, 0.5, 0.),
                    (0b0010, 0., 0.5),
                    (0b0001, 0.5, 0.5),
                ];
                return quads
                    .iter()
                    .filter(|(bit, _, _)| bits & bit != 0)
                    .map(|&(_, fx, fy)| solid(rect(fx, fy, 0.5, 0.5), color))
                    .collect();
            }
        };
        return vec![solid(shape, color)];
    }

    // Lines run through the cell centre; light is about 1/8 of the cell width, at least 1 device px.
    // From the unsnapped width: snapped cells differ by a pixel, and the thickness must not.
    let light = snap(px((f32::from(cell.size.width) / 8.).max(1. / scale)));
    let heavy = snap(light * 2.);
    let thickness = |weight: u8| if weight == 2 { heavy } else { light };
    // Top edge of a horizontal line and left edge of a vertical one, on whole device pixels so
    // neighbouring cells line up exactly and nothing is anti-aliased across two pixel rows.
    let line_top = |t: Pixels| snap(y0 + (h - t) / 2.);
    let line_left = |t: Pixels| snap(x0 + (w - t) / 2.);

    if let Some(arms) = box_arms(c) {
        let [left, right, up, down] = arms;
        let th = thickness(left.max(right));
        let tv = thickness(up.max(down));
        let (hy, vx) = (line_top(th), line_left(tv));
        let mut out = Vec::new();
        let mut rect = |left: Pixels, top: Pixels, right: Pixels, bottom: Pixels| {
            out.push(solid(
                Bounds::new(point(left, top), size(right - left, bottom - top)),
                color,
            ));
        };
        // Straight runs are one rectangle; otherwise each arm reaches across the junction.
        if left > 0 && left == right {
            rect(x0, hy, x1, hy + th);
        } else {
            if left > 0 {
                rect(x0, hy, vx + tv, hy + thickness(left));
            }
            if right > 0 {
                rect(vx, hy, x1, hy + thickness(right));
            }
        }
        if up > 0 && up == down {
            rect(vx, y0, vx + tv, y1);
        } else {
            if up > 0 {
                rect(vx, y0, vx + thickness(up), hy + th);
            }
            if down > 0 {
                rect(vx, hy, vx + thickness(down), y1);
            }
        }
        return out;
    }

    if let Some(corner) = rounded_corner(c) {
        // A rounded rectangle outline whose one corner sits in this cell; the clip keeps that corner.
        let t = light;
        let (hy, vx) = (line_top(t), line_left(t));
        let radius = (w.min(h)) / 2.;
        let (left, top) = match corner {
            Corner::DownRight => (vx, hy),
            Corner::DownLeft => (vx + t - w, hy),
            Corner::UpLeft => (vx + t - w, hy + t - h),
            Corner::UpRight => (vx, hy + t - h),
        };
        let mut radii = Corners::default();
        let mut borders = Edges::default();
        match corner {
            Corner::DownRight => {
                radii.top_left = radius;
                (borders.top, borders.left) = (t, t);
            }
            Corner::DownLeft => {
                radii.top_right = radius;
                (borders.top, borders.right) = (t, t);
            }
            Corner::UpLeft => {
                radii.bottom_right = radius;
                (borders.bottom, borders.right) = (t, t);
            }
            Corner::UpRight => {
                radii.bottom_left = radius;
                (borders.bottom, borders.left) = (t, t);
            }
        }
        let quad = PaintQuad {
            bounds: Bounds::new(point(left, top), size(w, h)),
            corner_radii: radii,
            background: transparent_black().into(),
            border_widths: borders,
            border_color: color.to_hsla(),
            border_style: gpui_kit::BorderStyle::Solid,
        };
        return vec![Shape {
            quad,
            clip: Some(Bounds::new(point(x0, y0), size(w, h))),
        }];
    }
    Vec::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Corner {
    /// ╭ arms go right and down.
    DownRight,
    /// ╮
    DownLeft,
    /// ╯
    UpLeft,
    /// ╰
    UpRight,
}

fn rounded_corner(c: char) -> Option<Corner> {
    Some(match c {
        '╭' => Corner::DownRight,
        '╮' => Corner::DownLeft,
        '╯' => Corner::UpLeft,
        '╰' => Corner::UpRight,
        _ => return None,
    })
}

fn box_arms(c: char) -> Option<Arms> {
    Some(match c {
        '─' => [1, 1, 0, 0],
        '━' => [2, 2, 0, 0],
        '│' => [0, 0, 1, 1],
        '┃' => [0, 0, 2, 2],
        '┌' => [0, 1, 0, 1],
        '┏' => [0, 2, 0, 2],
        '┐' => [1, 0, 0, 1],
        '┓' => [2, 0, 0, 2],
        '└' => [0, 1, 1, 0],
        '┗' => [0, 2, 2, 0],
        '┘' => [1, 0, 1, 0],
        '┛' => [2, 0, 2, 0],
        '├' => [0, 1, 1, 1],
        '┣' => [0, 2, 2, 2],
        '┤' => [1, 0, 1, 1],
        '┫' => [2, 0, 2, 2],
        '┬' => [1, 1, 0, 1],
        '┳' => [2, 2, 0, 2],
        '┴' => [1, 1, 1, 0],
        '┻' => [2, 2, 2, 0],
        '┼' => [1, 1, 1, 1],
        '╋' => [2, 2, 2, 2],
        '╴' => [1, 0, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 0, 1],
        '╸' => [2, 0, 0, 0],
        '╹' => [0, 0, 2, 0],
        '╺' => [0, 2, 0, 0],
        '╻' => [0, 0, 0, 2],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(20.)), size(px(8.), px(18.)))
    }

    fn rects(c: char) -> Vec<Bounds<Pixels>> {
        shapes(c, cell(), Color::hex(0xd97757), 1.)
            .into_iter()
            .map(|s| s.quad.bounds)
            .collect()
    }

    #[test]
    fn full_block_fills_the_cell() {
        assert_eq!(rects('█'), vec![cell()]);
    }

    #[test]
    fn halves_and_quadrants() {
        assert_eq!(
            rects('▀'),
            vec![Bounds::new(point(px(10.), px(20.)), size(px(8.), px(9.)))]
        );
        assert_eq!(
            rects('▐'),
            vec![Bounds::new(point(px(14.), px(20.)), size(px(4.), px(18.)))]
        );
        // ▛ = upper-left, upper-right, lower-left.
        assert_eq!(rects('▛').len(), 3);
        assert_eq!(
            rects('▗'),
            vec![Bounds::new(point(px(14.), px(29.)), size(px(4.), px(9.)))]
        );
    }

    #[test]
    fn vertical_line_spans_the_full_line_height() {
        let r = rects('│');
        assert_eq!(r.len(), 1, "a straight run is one rectangle");
        assert_eq!((r[0].top(), r[0].bottom()), (px(20.), px(38.)));
        // ┌: right arm and down arm share the junction square.
        let corner = rects('┌');
        assert_eq!(corner.len(), 2);
        assert_eq!(corner[0].origin, corner[1].origin);
    }

    #[test]
    fn coverage() {
        assert!(is_builtin('▟') && is_builtin('┼') && is_builtin('╭') && is_builtin('░'));
        assert!(!is_builtin('a') && !is_builtin('═'));
    }
}
