//! Design tokens for each skin and mode, copied from the approved mockup
//! (`.maomi/mockups/_system/tokens.css`). Change both together.

use crate::color::Color;

/// Resolved tokens for one skin/mode.
///
/// Semantic colours (`ok`, `warn`, `err`, `now`) are fixed per palette and never follow `accent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// App background behind the panels.
    pub bg: Color,
    /// Panel fill; the terminal background too.
    pub surface: Color,
    /// Raised panel, inputs, cards.
    pub surface_2: Color,
    /// Hover and pressed fills.
    pub surface_3: Color,
    /// Hairline border (translucent).
    pub line: Color,
    /// Stronger border (translucent).
    pub line_strong: Color,
    /// Body text.
    pub fg: Color,
    /// Emphasised text.
    pub fg_emph: Color,
    /// Secondary text.
    pub muted: Color,
    /// Brightest text; a soft off-white, never #ffffff.
    pub white: Color,
    /// Text on a `warn` fill.
    pub on_warn: Color,
    /// Text on `ok`, `err` and `now` fills.
    pub on_now: Color,
    /// Brand accent: focus, selection, cursor, primary buttons.
    pub accent: Color,
    /// Translucent accent wash for selected rows.
    pub accent_soft: Color,
    /// Text on an `accent` fill.
    pub on_accent: Color,
    /// Glow behind the gradient button (first colour).
    pub glow_a: Color,
    /// Glow behind the gradient button (second colour).
    pub glow_b: Color,
    /// Dot-grid colour for empty backgrounds.
    pub dot: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    /// Status-dot blue ("now").
    pub now: Color,
    /// Five decorative palette colours.
    pub c: [Color; 5],
    /// Agent model colours: opus, sonnet, haiku.
    pub agents: [Color; 3],
    /// Focus-ring gradient stops at 0%, 35%, 65% and 100%.
    pub gradient: [Color; 4],
}

impl Palette {
    /// Text selection fill: the accent at 28% so text stays readable.
    pub const fn selection(&self) -> Color {
        self.accent.with_alpha(72)
    }
}

const fn h(rgb: u32) -> Color {
    Color::hex(rgb)
}

pub static AURORA_DARK: Palette = Palette {
    bg: h(0x111118),
    surface: h(0x18181f),
    surface_2: h(0x1f1f28),
    surface_3: h(0x282833),
    line: Color::new(255, 255, 255, 18),
    line_strong: Color::new(255, 255, 255, 33),
    fg: h(0xb8b8c4),
    fg_emph: h(0xcfcfd6),
    muted: h(0x9090a4),
    white: h(0xececf2),
    on_warn: h(0x1d1407),
    on_now: h(0x0c1624),
    accent: h(0x8d7eff),
    accent_soft: Color::new(139, 123, 255, 38),
    on_accent: h(0x111118),
    glow_a: Color::new(91, 140, 255, 41),
    glow_b: Color::new(255, 107, 181, 23),
    dot: Color::new(255, 255, 255, 11),
    ok: h(0x4cc38a),
    warn: h(0xf0a43a),
    err: h(0xff6b6b),
    now: h(0x4aa3ff),
    c: [
        h(0x5b8cff),
        h(0xaa71ff),
        h(0xff6bb5),
        h(0xffa25b),
        h(0x3ccfb4),
    ],
    agents: [h(0xb698f3), h(0x6ba0ef), h(0x4bc096)],
    gradient: [h(0x5b8cff), h(0xaa71ff), h(0xff6bb5), h(0xffa25b)],
};

pub static AURORA_LIGHT: Palette = Palette {
    bg: h(0xefeff4),
    surface: h(0xfafafc),
    surface_2: h(0xf4f4f8),
    surface_3: h(0xe8e8f0),
    line: Color::new(20, 20, 45, 20),
    line_strong: Color::new(20, 20, 45, 38),
    fg: h(0x40404f),
    fg_emph: h(0x25252f),
    muted: h(0x67677c),
    white: h(0xfafafc),
    on_warn: h(0xfafafc),
    on_now: h(0xfafafc),
    accent: h(0x594afa),
    accent_soft: Color::new(89, 74, 250, 26),
    on_accent: h(0xfafafc),
    glow_a: Color::new(91, 140, 255, 36),
    glow_b: Color::new(255, 107, 181, 26),
    dot: Color::new(20, 20, 45, 13),
    ok: h(0x18774a),
    warn: h(0x975a00),
    err: h(0xc13434),
    now: h(0x0966c9),
    c: [
        h(0x3b6cf0),
        h(0x7f45dd),
        h(0xe0458f),
        h(0xce722c),
        h(0x149b86),
    ],
    agents: [h(0x8a5cf0), h(0x2f74e0), h(0x139967)],
    gradient: [h(0x3b6cf0), h(0x7f45dd), h(0xe0458f), h(0xce722c)],
};

/// Onyx: charcoal greys and an ember-orange accent. Semantics stay cool-toned so they never read as the accent.
pub static ONYX_DARK: Palette = Palette {
    bg: h(0x0f0f13),
    surface: h(0x16161b),
    surface_2: h(0x1e1e25),
    surface_3: h(0x26262e),
    line: Color::new(255, 240, 220, 18),
    line_strong: Color::new(255, 240, 220, 33),
    fg: h(0xc2bbb0),
    fg_emph: h(0xddd2c2),
    muted: h(0x938d86),
    white: h(0xf3e6d4),
    on_warn: h(0x1d1407),
    on_now: h(0x0c1624),
    accent: h(0xff7a2e),
    accent_soft: Color::new(255, 122, 46, 38),
    on_accent: h(0x1a0b04),
    glow_a: Color::new(255, 179, 71, 36),
    glow_b: Color::new(201, 58, 18, 26),
    dot: Color::new(255, 240, 220, 11),
    ok: h(0x5cc28a),
    warn: h(0xf2c14e),
    err: h(0xf0506e),
    now: h(0x5aa9e6),
    c: [
        h(0xff8a2e),
        h(0x5aa9e6),
        h(0xffd27a),
        h(0xb78cdb),
        h(0x4fc1b0),
    ],
    agents: [h(0xb78cdb), h(0x5aa9e6), h(0x4fc1b0)],
    gradient: [h(0xffd27a), h(0xffb347), h(0xff7a2e), h(0xc93a12)],
};

pub static ONYX_LIGHT: Palette = Palette {
    bg: h(0xeeece9),
    surface: h(0xfaf9f7),
    surface_2: h(0xf4f2ef),
    surface_3: h(0xe6e2dc),
    line: Color::new(40, 30, 20, 23),
    line_strong: Color::new(40, 30, 20, 41),
    fg: h(0x3f3b37),
    fg_emph: h(0x24211e),
    muted: h(0x67655e),
    white: h(0xfaf9f7),
    on_warn: h(0xfaf9f7),
    on_now: h(0xfaf9f7),
    accent: h(0xc2410c),
    accent_soft: Color::new(194, 65, 12, 26),
    on_accent: h(0xfaf9f7),
    glow_a: Color::new(217, 154, 26, 36),
    glow_b: Color::new(156, 42, 10, 20),
    dot: Color::new(40, 30, 20, 13),
    ok: h(0x1f7a4a),
    warn: h(0x8f5d00),
    err: h(0xb8283f),
    now: h(0x1f6fb8),
    c: [
        h(0xb4530f),
        h(0x1f6fb8),
        h(0x8f6a00),
        h(0x7f4fb0),
        h(0x0f7a6c),
    ],
    agents: [h(0x7f4fb0), h(0x1f6fb8), h(0x0f7a6c)],
    gradient: [h(0xd99a1a), h(0xd97a12), h(0xc2410c), h(0x9c2a0a)],
};

/// Mochi: warm-grey dark skin with a mauve accent. `err` leans red-orange, away from the accent.
pub static MOCHI: Palette = Palette {
    bg: h(0x1b1a19),
    surface: h(0x21201e),
    surface_2: h(0x282624),
    surface_3: h(0x312f2c),
    line: Color::new(255, 248, 236, 18),
    line_strong: Color::new(255, 248, 236, 33),
    fg: h(0xbcb8b2),
    fg_emph: h(0xd6d2cb),
    muted: h(0xa0988e),
    white: h(0xf0ece6),
    on_warn: h(0x1d1407),
    on_now: h(0x0e1620),
    accent: h(0xbda1d3),
    accent_soft: Color::new(189, 161, 211, 36),
    on_accent: h(0x1d1820),
    glow_a: Color::new(91, 140, 255, 41),
    glow_b: Color::new(255, 107, 181, 23),
    dot: Color::new(255, 248, 236, 11),
    ok: h(0x58b789),
    warn: h(0xdea14c),
    err: h(0xe7766a),
    now: h(0x81a8cf),
    c: [
        h(0x89add2),
        h(0xbda1d3),
        h(0xe28da9),
        h(0xefa46b),
        h(0x4bc0ab),
    ],
    agents: [h(0xc2a7d7), h(0x6bb8c7), h(0x4bc096)],
    gradient: [h(0x5b8cff), h(0xa66bff), h(0xff6bb5), h(0xffa25b)],
};
