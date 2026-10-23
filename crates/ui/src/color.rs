//! Plain-data colour type shared by the palettes and the terminal renderer.

/// An 8-bit sRGB colour with alpha.
///
/// Core crates and the terminal renderer can use it without touching GPUI;
/// [`Color::to_hsla`] converts for GPUI call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Opaque colour from `0xRRGGBB`.
    pub const fn hex(rgb: u32) -> Self {
        Self::new((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255)
    }

    /// Colour from channels; `a` is 0 (clear) to 255 (opaque).
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// The same colour with a different alpha.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// True when alpha is 255.
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// `(r, g, b)` for consumers that ignore alpha.
    pub const fn rgb(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }

    /// Linear blend in sRGB space: `t` = 0.0 gives `self`, 1.0 gives `other`. Alpha stays `self`'s.
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Self {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
            a: self.a,
        }
    }

    /// Brightness from 0 (black) to 1 (white), Rec. 709 weights on the sRGB values: close
    /// enough for telling dark text from light.
    pub fn luminance(self) -> f32 {
        (0.2126 * f32::from(self.r) + 0.7152 * f32::from(self.g) + 0.0722 * f32::from(self.b))
            / 255.0
    }

    /// The colour moved toward white just far enough to reach `min` luminance (hue kept);
    /// lighter colours stay as they are.
    pub fn lifted(self, min: f32) -> Self {
        let l = self.luminance();
        if l >= min {
            return self;
        }
        // Mixing toward white raises luminance linearly: l + (1 - l) * t.
        let t = (min - l) / (1.0 - l);
        self.mix(Self::new(255, 255, 255, self.a), t)
    }

    /// GPUI colour for painting.
    pub fn to_hsla(self) -> gpui_kit::Hsla {
        gpui_kit::Rgba {
            r: f32::from(self.r) / 255.0,
            g: f32::from(self.g) / 255.0,
            b: f32::from(self.b) / 255.0,
            a: f32::from(self.a) / 255.0,
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifting_brightens_only_dark_colours() {
        let black = Color::hex(0x000000).lifted(0.7);
        assert!((black.luminance() - 0.7).abs() < 0.01, "{black:?}");
        let dark_blue = Color::hex(0x1a2a6c);
        let lifted = dark_blue.lifted(0.7);
        assert!(lifted.luminance() >= 0.69);
        // Still bluish: blue stays the largest channel.
        assert!(lifted.b >= lifted.r && lifted.b >= lifted.g);
        let light = Color::hex(0xe0e0e0);
        assert_eq!(light.lifted(0.7), light);
    }

    #[test]
    fn hex_splits_channels() {
        assert_eq!(Color::hex(0x8d7eff), Color::new(0x8d, 0x7e, 0xff, 255));
    }

    #[test]
    fn mix_endpoints_and_middle() {
        let a = Color::hex(0x000010);
        let b = Color::hex(0x2000f0);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, 0.5), Color::hex(0x100080));
    }

    #[test]
    fn mix_keeps_own_alpha() {
        let a = Color::new(0, 0, 0, 40);
        assert_eq!(a.mix(Color::hex(0xffffff), 1.0).a, 40);
    }
}
