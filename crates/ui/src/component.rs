//! Maps the theme tokens onto gpui-component's `ThemeColor` so docks, tabs and inputs match.

use gpui_kit::component::{Theme as ComponentTheme, ThemeColor, ThemeMode};
use gpui_kit::{App, Hsla};

use crate::color::Color;
use crate::palette::Palette;
use crate::{Mode, Theme};

/// Pushes `theme` into gpui-component's global theme and refreshes every window.
///
/// Needs `gpui_kit::init(cx)` to have run first.
pub fn apply(theme: Theme, cx: &mut App) {
    let mode = match theme.effective_mode() {
        Mode::Dark => ThemeMode::Dark,
        Mode::Light => ThemeMode::Light,
    };
    // Switching mode reloads the registered theme over any colours, so it goes in its own step.
    ComponentTheme::change(mode, None, cx);

    let colors = theme_color(theme);
    ComponentTheme::update(cx, |t| t.colors = colors);
}

/// Hover and pressed shades of `base`: toward the brightest text on dark, the darkest on light.
fn shades(p: &Palette, dark: bool, base: Color) -> (Color, Color) {
    let toward = if dark { p.white } else { p.fg_emph };
    (base.mix(toward, 0.12), base.mix(toward, 0.24))
}

/// The gpui-component colours for `theme`. Starts from the library default so fields we do not set stay valid.
// Setting ~150 fields one by one reads better than a struct literal that has to name them all.
#[allow(clippy::field_reassign_with_default)]
pub(crate) fn theme_color(theme: Theme) -> ThemeColor {
    let p = theme.palette();
    let dark = theme.effective_mode() == Mode::Dark;
    let term = theme.terminal_colors();
    let h = |c: Color| -> Hsla { c.to_hsla() };
    let clear = Hsla::transparent_black();

    let mut t = ThemeColor::default();

    // Surfaces and text.
    t.background = h(p.bg);
    t.foreground = h(p.fg);
    t.border = h(p.line_strong);
    t.input = h(p.line_strong);
    t.muted = h(p.surface_2);
    t.muted_foreground = h(p.muted);
    t.popover = h(p.surface_2);
    t.popover_foreground = h(p.fg_emph);
    t.overlay = h(p.bg.with_alpha(153));
    t.window_border = h(p.line_strong);
    t.group_box = h(p.surface_2);
    t.group_box_foreground = h(p.fg);
    t.skeleton = h(p.surface_3);
    t.accordion = h(p.surface);
    t.description_list_label = h(p.surface_2);
    t.description_list_label_foreground = h(p.muted);

    // gpui-component's `accent` is the hover/highlight fill, not the brand colour.
    t.accent = h(p.surface_3);
    t.accent_foreground = h(p.fg_emph);

    // Brand accent.
    let (accent_hover, accent_active) = shades(p, dark, p.accent);
    t.primary = h(p.accent);
    t.primary_hover = h(accent_hover);
    t.primary_active = h(accent_active);
    t.primary_foreground = h(p.on_accent);
    t.ring = h(p.accent);
    t.caret = h(p.accent);
    t.link = h(p.accent);
    t.link_hover = h(accent_hover);
    t.link_active = h(accent_active);
    t.selection = h(p.selection());
    t.drag_border = h(p.accent);
    t.drop_target = h(p.accent_soft);
    t.progress_bar = h(p.accent);
    t.slider_bar = h(p.accent);
    t.slider_thumb = h(p.fg_emph);
    t.switch = h(p.surface_3);
    t.switch_thumb = h(p.muted);

    // Secondary surfaces.
    t.secondary = h(p.surface_2);
    t.secondary_hover = h(p.surface_3);
    t.secondary_active = h(p.surface_3.mix(p.fg, 0.1));
    t.secondary_foreground = h(p.fg_emph);

    // Semantic fills.
    let semantic = [
        (p.ok, p.on_now),
        (p.warn, p.on_warn),
        (p.err, p.on_now),
        (p.now, p.on_now),
    ];
    let [ok, warn, err, info] = semantic.map(|(base, on)| {
        let (hover, active) = shades(p, dark, base);
        (h(base), h(hover), h(active), h(on))
    });
    (
        t.success,
        t.success_hover,
        t.success_active,
        t.success_foreground,
    ) = ok;
    (
        t.warning,
        t.warning_hover,
        t.warning_active,
        t.warning_foreground,
    ) = warn;
    (
        t.danger,
        t.danger_hover,
        t.danger_active,
        t.danger_foreground,
    ) = err;
    (t.info, t.info_hover, t.info_active, t.info_foreground) = info;

    // Buttons.
    t.button = h(p.surface_2);
    t.button_hover = h(p.surface_3);
    t.button_active = h(p.surface_3.mix(p.fg, 0.1));
    t.button_foreground = h(p.fg_emph);
    t.button_primary = t.primary;
    t.button_primary_hover = t.primary_hover;
    t.button_primary_active = t.primary_active;
    t.button_primary_foreground = t.primary_foreground;
    t.button_secondary = t.secondary;
    t.button_secondary_hover = t.secondary_hover;
    t.button_secondary_active = t.secondary_active;
    t.button_secondary_foreground = t.secondary_foreground;
    t.button_success = t.success;
    t.button_success_hover = t.success_hover;
    t.button_success_active = t.success_active;
    t.button_success_foreground = t.success_foreground;
    t.button_warning = t.warning;
    t.button_warning_hover = t.warning_hover;
    t.button_warning_active = t.warning_active;
    t.button_warning_foreground = t.warning_foreground;
    t.button_danger = t.danger;
    t.button_danger_hover = t.danger_hover;
    t.button_danger_active = t.danger_active;
    t.button_danger_foreground = t.danger_foreground;
    t.button_info = t.info;
    t.button_info_hover = t.info_hover;
    t.button_info_active = t.info_active;
    t.button_info_foreground = t.info_foreground;

    // Lists and tables.
    t.list = h(p.surface);
    t.list_even = h(p.surface);
    t.list_head = h(p.surface_2);
    t.list_hover = h(p.surface_3);
    t.list_active = h(p.accent_soft);
    t.list_active_border = h(p.accent);
    t.table = h(p.surface);
    t.table_even = h(p.surface);
    t.table_head = h(p.surface_2);
    t.table_head_foreground = h(p.muted);
    t.table_foot = h(p.surface_2);
    t.table_foot_foreground = h(p.muted);
    t.table_hover = h(p.surface_3);
    t.table_active = h(p.accent_soft);
    t.table_active_border = h(p.accent);
    t.table_row_border = h(p.line);

    // Tabs, docks and bars.
    t.tab_bar = h(p.bg);
    t.tab_bar_segmented = h(p.surface_2);
    t.tab = clear;
    t.tab_foreground = h(p.muted);
    t.tab_active = h(p.surface);
    t.tab_active_foreground = h(p.fg_emph);
    t.sidebar = h(p.surface);
    t.sidebar_foreground = h(p.fg);
    t.sidebar_border = h(p.line);
    t.sidebar_accent = h(p.accent_soft);
    t.sidebar_accent_foreground = h(p.fg_emph);
    t.sidebar_primary = h(p.accent);
    t.sidebar_primary_foreground = h(p.on_accent);
    t.title_bar = h(p.bg);
    t.title_bar_border = h(p.line);
    t.status_bar = h(p.bg);
    t.status_bar_border = h(p.line);

    // Scrollbars: the thumb is the muted text colour, faded.
    t.scrollbar = clear;
    t.scrollbar_thumb = h(p.muted.with_alpha(90));
    t.scrollbar_thumb_hover = h(p.muted.with_alpha(140));

    // Charts take the decorative palette; bullish/bearish take the semantics.
    t.chart_1 = h(p.c[0]);
    t.chart_2 = h(p.c[1]);
    t.chart_3 = h(p.c[2]);
    t.chart_4 = h(p.c[3]);
    t.chart_5 = h(p.c[4]);
    t.chart_bullish = h(p.ok);
    t.chart_bearish = h(p.err);
    t.chart_grid = h(p.line);

    // Named colours follow the ANSI palette (normal, then bright).
    let a = &term.ansi;
    t.red = h(a[1]);
    t.red_light = h(a[9]);
    t.green = h(a[2]);
    t.green_light = h(a[10]);
    t.yellow = h(a[3]);
    t.yellow_light = h(a[11]);
    t.blue = h(a[4]);
    t.blue_light = h(a[12]);
    t.magenta = h(a[5]);
    t.magenta_light = h(a[13]);
    t.cyan = h(a[6]);
    t.cyan_light = h(a[14]);

    t
}
