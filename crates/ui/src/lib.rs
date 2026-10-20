//! MaomiTerminal theme and shared UI components.
//!
//! A [`Theme`] is a skin plus a mode. [`Theme::palette`] gives its tokens, [`Theme::terminal_colors`]
//! the terminal palette, and [`apply`] pushes it into gpui-component. [`init`] registers the
//! runtime-switching actions and installs the default theme.

mod color;
mod component;
mod fonts;
mod palette;
pub mod settings;
pub mod shell;
mod terminal;
mod terminal_view;

use gpui_kit::{App, Global, KeyBinding};

pub use color::Color;
pub use component::apply;
pub use fonts::TERMINAL_FAMILY;
pub use palette::Palette;
pub use terminal::TerminalColors;
pub use terminal_view::{TerminalElement, TerminalFont, TerminalView, TerminalViewEvent};

gpui_kit::actions!(maomi_ui, [CycleSkin, ToggleMode]);

/// Colour skin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Skin {
    #[default]
    Aurora,
    Onyx,
    Mochi,
}

impl Skin {
    /// The skin after this one: Aurora, Onyx, Mochi, then back to Aurora.
    pub const fn next(self) -> Self {
        match self {
            Self::Aurora => Self::Onyx,
            Self::Onyx => Self::Mochi,
            Self::Mochi => Self::Aurora,
        }
    }

    /// Display name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Aurora => "Aurora",
            Self::Onyx => "Onyx",
            Self::Mochi => "Mochi",
        }
    }

    /// Name in the config file.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Aurora => "aurora",
            Self::Onyx => "onyx",
            Self::Mochi => "mochi",
        }
    }

    /// Parses a config name; case is ignored.
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Aurora, Self::Onyx, Self::Mochi]
            .into_iter()
            .find(|s| s.key().eq_ignore_ascii_case(key))
    }

    /// False for skins that only ship a dark variant.
    pub const fn has_light(self) -> bool {
        !matches!(self, Self::Mochi)
    }
}

/// Light or dark variant of a skin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

impl Mode {
    /// The other mode.
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    /// Name in the config file.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// Parses a config name; case is ignored.
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Dark, Self::Light]
            .into_iter()
            .find(|m| m.key().eq_ignore_ascii_case(key))
    }
}

/// The selected skin and mode.
///
/// `mode` is kept while Mochi is active (Mochi is dark-only), so cycling back to Aurora or Onyx
/// restores the mode the user had. Read [`Theme::effective_mode`] for what is actually shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    pub skin: Skin,
    pub mode: Mode,
}

impl Theme {
    pub const fn new(skin: Skin, mode: Mode) -> Self {
        Self { skin, mode }
    }

    /// The mode that is shown: always dark for Mochi.
    pub const fn effective_mode(&self) -> Mode {
        if self.skin.has_light() {
            self.mode
        } else {
            Mode::Dark
        }
    }

    /// Design tokens for this skin and mode.
    pub fn palette(&self) -> &'static Palette {
        use palette::{AURORA_DARK, AURORA_LIGHT, MOCHI, ONYX_DARK, ONYX_LIGHT};
        match (self.skin, self.effective_mode()) {
            (Skin::Aurora, Mode::Dark) => &AURORA_DARK,
            (Skin::Aurora, Mode::Light) => &AURORA_LIGHT,
            (Skin::Onyx, Mode::Dark) => &ONYX_DARK,
            (Skin::Onyx, Mode::Light) => &ONYX_LIGHT,
            (Skin::Mochi, _) => &MOCHI,
        }
    }

    /// Whether the dark/light toggle does anything; it is off while Mochi is active.
    pub const fn mode_toggle_enabled(&self) -> bool {
        self.skin.has_light()
    }

    /// This theme with the next skin; the mode is kept.
    pub const fn with_next_skin(self) -> Self {
        Self {
            skin: self.skin.next(),
            ..self
        }
    }

    /// This theme with the other mode, or unchanged while the toggle is disabled.
    pub const fn with_toggled_mode(self) -> Self {
        if self.mode_toggle_enabled() {
            Self {
                mode: self.mode.toggled(),
                ..self
            }
        } else {
            self
        }
    }

    /// The theme currently set on the app (Aurora dark before [`init`] runs).
    pub fn current(cx: &App) -> Self {
        cx.try_global::<CurrentTheme>()
            .map_or_else(Self::default, |g| g.0)
    }
}

/// Makes `theme` the current theme, applies it to gpui-component and saves it in the config.
pub fn set_theme(cx: &mut App, theme: Theme) {
    cx.set_global(CurrentTheme(theme));
    apply(theme, cx);
    settings::store_theme(theme, cx);
}

/// Installs `config`, registers the skin/mode actions and their keybindings, and applies the
/// configured theme.
///
/// Call after `gpui_kit::init(cx)`.
pub fn init(config: maomi_config::Config, cx: &mut App) {
    settings::install(config, cx);
    let theme = settings::theme_from(&settings::config(cx), cx);
    fonts::load(cx);
    cx.bind_keys(terminal_view::key_bindings());
    shell::init(cx);
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-k", CycleSkin, None),
        KeyBinding::new("ctrl-shift-l", ToggleMode, None),
    ]);
    cx.on_action(|_: &CycleSkin, cx| {
        set_theme(cx, Theme::current(cx).with_next_skin());
    });
    cx.on_action(|_: &ToggleMode, cx| {
        let theme = Theme::current(cx);
        if theme.mode_toggle_enabled() {
            set_theme(cx, theme.with_toggled_mode());
        }
    });
    set_theme(cx, theme);
}

/// Holds the current [`Theme`] as a GPUI global.
struct CurrentTheme(Theme);

impl Global for CurrentTheme {}

#[cfg(test)]
mod tests {
    use super::*;

    const SKINS: [Skin; 3] = [Skin::Aurora, Skin::Onyx, Skin::Mochi];
    const MODES: [Mode; 2] = [Mode::Dark, Mode::Light];

    fn all_themes() -> impl Iterator<Item = Theme> {
        SKINS
            .into_iter()
            .flat_map(|s| MODES.into_iter().map(move |m| Theme::new(s, m)))
    }

    fn palette_colors(p: &Palette) -> Vec<Color> {
        let mut v = vec![
            p.bg,
            p.surface,
            p.surface_2,
            p.surface_3,
            p.line,
            p.line_strong,
            p.fg,
            p.fg_emph,
            p.muted,
            p.white,
            p.on_warn,
            p.on_now,
            p.accent,
            p.accent_soft,
            p.on_accent,
            p.glow_a,
            p.glow_b,
            p.dot,
            p.ok,
            p.warn,
            p.err,
            p.now,
            p.selection(),
        ];
        v.extend(p.c);
        v.extend(p.agents);
        v.extend(p.gradient);
        v
    }

    #[test]
    fn default_theme_is_aurora_dark() {
        let theme = Theme::default();
        assert_eq!(theme.skin, Skin::Aurora);
        assert_eq!(theme.mode, Mode::Dark);
    }

    #[test]
    fn every_skin_and_mode_resolves() {
        for theme in all_themes() {
            let p = theme.palette();
            assert!(p.bg.is_opaque() && p.surface.is_opaque(), "{theme:?}");
            assert_eq!(theme.terminal_colors().bg, p.surface, "{theme:?}");
        }
    }

    #[test]
    fn light_palettes_are_lighter_than_dark() {
        for skin in [Skin::Aurora, Skin::Onyx] {
            let dark = Theme::new(skin, Mode::Dark).palette();
            let light = Theme::new(skin, Mode::Light).palette();
            assert_ne!(dark, light);
            assert!(u32::from(light.bg.r) > u32::from(dark.bg.r));
        }
    }

    #[test]
    fn mochi_ignores_mode() {
        let dark = Theme::new(Skin::Mochi, Mode::Dark);
        let light = Theme::new(Skin::Mochi, Mode::Light);
        assert_eq!(dark.palette(), light.palette());
        assert_eq!(dark.terminal_colors(), light.terminal_colors());
        assert_eq!(light.effective_mode(), Mode::Dark);
    }

    #[test]
    fn no_opaque_colour_is_pure_white_or_black() {
        for theme in all_themes() {
            let term = theme.terminal_colors();
            let mut colors = palette_colors(theme.palette());
            colors.extend(term.ansi);
            colors.extend([term.fg, term.bg, term.cursor, term.cursor_text]);
            for c in colors.into_iter().filter(|c| c.is_opaque()) {
                assert_ne!(c.rgb(), (255, 255, 255), "{theme:?}");
                assert_ne!(c.rgb(), (0, 0, 0), "{theme:?}");
            }
        }
    }

    #[test]
    fn ansi_palette_has_sixteen_distinct_enough_entries() {
        for theme in all_themes() {
            let ansi = theme.terminal_colors().ansi;
            assert_eq!(ansi.len(), 16);
            // Hue slots 1-6 must differ from each other.
            for i in 1..7 {
                for j in (i + 1)..7 {
                    assert_ne!(ansi[i], ansi[j], "{theme:?} {i} {j}");
                }
            }
        }
    }

    #[test]
    fn ansi_semantic_slots_follow_palette() {
        for theme in all_themes() {
            let (p, ansi) = (theme.palette(), theme.terminal_colors().ansi);
            assert_eq!(ansi[1], p.err);
            assert_eq!(ansi[2], p.ok);
            assert_eq!(ansi[3], p.warn);
        }
    }

    #[test]
    fn semantics_do_not_follow_the_accent() {
        for theme in all_themes() {
            let p = theme.palette();
            for c in [p.ok, p.warn, p.err, p.now] {
                assert_ne!(c, p.accent, "{theme:?}");
            }
        }
    }

    #[test]
    fn mode_toggle_is_disabled_on_mochi_only() {
        assert!(Theme::new(Skin::Aurora, Mode::Dark).mode_toggle_enabled());
        assert!(Theme::new(Skin::Onyx, Mode::Light).mode_toggle_enabled());
        let mochi = Theme::new(Skin::Mochi, Mode::Dark);
        assert!(!mochi.mode_toggle_enabled());
        assert_eq!(mochi.with_toggled_mode(), mochi);
    }

    #[test]
    fn toggle_flips_mode_off_mochi() {
        let theme = Theme::new(Skin::Onyx, Mode::Dark).with_toggled_mode();
        assert_eq!(theme.mode, Mode::Light);
    }

    #[test]
    fn skin_cycle_visits_all_and_keeps_mode() {
        let start = Theme::new(Skin::Aurora, Mode::Light);
        let t1 = start.with_next_skin();
        let t2 = t1.with_next_skin();
        let t3 = t2.with_next_skin();
        assert_eq!(
            [t1.skin, t2.skin, t3.skin],
            [Skin::Onyx, Skin::Mochi, Skin::Aurora]
        );
        assert_eq!(t3, start);
    }

    #[test]
    fn component_colors_use_the_palette() {
        for theme in all_themes() {
            let p = theme.palette();
            let t = component::theme_color(theme);
            assert_eq!(t.background, p.bg.to_hsla());
            assert_eq!(t.primary, p.accent.to_hsla());
            assert_eq!(t.danger, p.err.to_hsla());
        }
    }

    #[test]
    fn config_keys_round_trip() {
        for skin in SKINS {
            assert_eq!(Skin::from_key(skin.key()), Some(skin));
        }
        for mode in MODES {
            assert_eq!(Mode::from_key(mode.key()), Some(mode));
        }
        assert_eq!(Skin::from_key("ONYX"), Some(Skin::Onyx));
        assert_eq!(Skin::from_key("neon"), None);
    }
}
