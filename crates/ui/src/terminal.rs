//! Terminal colours derived from a theme.

use crate::color::Color;
use crate::{Mode, Skin, Theme};

/// Everything the terminal renderer needs to paint a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default text colour.
    pub fg: Color,
    /// Pane background: the panel colour, never the app background.
    pub bg: Color,
    pub cursor: Color,
    /// Glyph colour under a block cursor.
    pub cursor_text: Color,
    /// Selection fill (translucent).
    pub selection: Color,
    /// ANSI colours 0-7 (normal) and 8-15 (bright), in the standard order:
    /// black, red, green, yellow, blue, magenta, cyan, white.
    pub ansi: [Color; 16],
}

impl Theme {
    /// Terminal fg/bg/cursor/selection and the 16-colour ANSI palette for this theme.
    pub fn terminal_colors(&self) -> TerminalColors {
        let p = self.palette();
        let dark = self.effective_mode() == Mode::Dark;

        // Onyx's first palette colour is the ember accent, so its blue and magenta come from the cool slots.
        let (blue, magenta) = match self.skin {
            Skin::Aurora | Skin::Mochi => (p.c[0], p.c[1]),
            Skin::Onyx => (p.c[1], p.c[3]),
        };
        let normal = [
            if dark { p.surface_3 } else { p.fg_emph },
            p.err,
            p.ok,
            p.warn,
            blue,
            magenta,
            p.c[4],
            if dark { p.fg } else { p.muted },
        ];

        // Bright variants move toward the brightest text on dark and toward the darkest on light,
        // so they stay readable on the panel colour.
        let toward = if dark { p.white } else { p.fg_emph };
        let mut ansi = [p.fg; 16];
        ansi[..8].copy_from_slice(&normal);
        for (i, colour) in normal.iter().enumerate().take(7).skip(1) {
            ansi[8 + i] = colour.mix(toward, 0.2);
        }
        ansi[8] = if dark {
            p.muted
        } else {
            p.fg_emph.mix(p.muted, 0.5)
        };
        ansi[15] = if dark { p.white } else { p.fg };

        TerminalColors {
            fg: p.fg,
            bg: p.surface,
            cursor: p.accent,
            cursor_text: p.on_accent,
            selection: p.selection(),
            ansi,
        }
    }
}
