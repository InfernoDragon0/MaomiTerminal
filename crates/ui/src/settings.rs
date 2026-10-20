//! The loaded [`Config`] as a GPUI global, saving it off the UI thread, and mapping its values
//! onto themes, fonts and sessions.

use std::ffi::OsString;

use gpui_kit::{App, Context, Global, WindowAppearance, px};
use maomi_config::Config;
use maomi_term::{CursorShape, SessionOptions, Shell};

use crate::{Mode, Skin, TerminalFont, TerminalView, Theme};

/// Config value of `appearance.mode` that follows the OS.
pub const SYSTEM_MODE: &str = "system";

/// The current config plus the queue its saves go through. Observe it to react to changes.
pub(crate) struct Settings {
    config: Config,
    saves: async_channel::Sender<Config>,
}

impl Global for Settings {}

/// Installs `config` and starts the background saver. Called once from [`crate::init`].
pub(crate) fn install(config: Config, cx: &mut App) {
    let (saves, queue) = async_channel::unbounded::<Config>();
    // One writer, so saves land in order; a burst of changes only writes the newest.
    cx.background_executor()
        .spawn(async move {
            while let Ok(mut config) = queue.recv().await {
                while let Ok(newer) = queue.try_recv() {
                    config = newer;
                }
                if let Err(err) = config.save() {
                    tracing::warn!(%err, "could not save the config");
                }
            }
        })
        .detach();
    cx.set_global(Settings { config, saves });
}

/// The current config (the defaults before [`crate::init`] runs).
pub fn config(cx: &App) -> Config {
    cx.try_global::<Settings>()
        .map_or_else(Config::default, |s| s.config.clone())
}

/// Changes the config, saves it in the background and notifies observers when anything changed.
pub fn update_config(cx: &mut App, f: impl FnOnce(&mut Config)) {
    if !cx.has_global::<Settings>() {
        return;
    }
    let mut next = config(cx);
    f(&mut next);
    let next = next.sanitized();
    if next == cx.global::<Settings>().config {
        return;
    }
    let settings = cx.global_mut::<Settings>();
    _ = settings.saves.try_send(next.clone());
    settings.config = next;
}

/// Whether the mode follows the OS.
pub fn follows_system(config: &Config) -> bool {
    config.appearance.mode.eq_ignore_ascii_case(SYSTEM_MODE)
}

/// The OS light/dark setting as a [`Mode`].
pub fn system_mode(cx: &App) -> Mode {
    match cx.window_appearance() {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
    }
}

/// The theme the config names; unknown names fall back to Aurora / dark.
pub(crate) fn theme_from(config: &Config, cx: &App) -> Theme {
    let skin = Skin::from_key(&config.appearance.skin).unwrap_or_default();
    let mode = if follows_system(config) {
        system_mode(cx)
    } else {
        Mode::from_key(&config.appearance.mode).unwrap_or_default()
    };
    Theme::new(skin, mode)
}

/// Records `theme` in the config. "system" stays while the theme still matches the OS, so only
/// an explicit mode change (Ctrl+Shift+L, the Dark/Light buttons) leaves it.
pub(crate) fn store_theme(theme: Theme, cx: &mut App) {
    let keep_system = follows_system(&config(cx)) && theme.mode == system_mode(cx);
    update_config(cx, |c| {
        c.appearance.skin = theme.skin.key().into();
        if !keep_system {
            c.appearance.mode = theme.mode.key().into();
        }
    });
}

/// Re-applies the theme when the mode follows the OS; call when the OS appearance changes.
pub(crate) fn sync_system_theme(cx: &mut App) {
    let config = config(cx);
    if follows_system(&config) {
        let theme = theme_from(&config, cx);
        if theme != Theme::current(cx) {
            crate::set_theme(cx, theme);
        }
    }
}

/// The terminal font the config names.
pub fn terminal_font(config: &Config) -> TerminalFont {
    let t = &config.terminal;
    TerminalFont {
        family: t.font_family.clone().into(),
        size: px(t.font_size),
        line_height: t.line_height,
    }
}

/// Config name of a cursor shape, and back. Unknown names give a block.
pub const CURSOR_STYLES: [(&str, &str, CursorShape); 3] = [
    ("block", "Block", CursorShape::Block),
    ("bar", "Bar", CursorShape::Beam),
    ("underline", "Underline", CursorShape::Underline),
];

pub fn cursor_shape(config: &Config) -> CursorShape {
    CURSOR_STYLES
        .iter()
        .find(|(key, ..)| key.eq_ignore_ascii_case(&config.terminal.cursor_style))
        .map_or(CursorShape::Block, |(.., shape)| *shape)
}

/// Pushes the terminal settings into a pane. `smooth_override` is the `MAOMI_SMOOTH` dev switch.
pub fn apply_to_view(
    view: &mut TerminalView,
    config: &Config,
    smooth_override: Option<bool>,
    cx: &mut Context<TerminalView>,
) {
    view.set_font(terminal_font(config), cx);
    view.set_smooth_scroll(smooth_override.unwrap_or(config.terminal.smooth_scroll), cx);
    view.set_cursor_blink(config.terminal.cursor_blink, cx);
    view.session()
        .set_defaults(config.terminal.scrollback, cursor_shape(config));
}

/// Session options from the config: shell, its arguments, startup folder, scrollback and cursor.
pub fn session_options(config: &Config) -> SessionOptions {
    let shell = config
        .shell
        .program
        .clone()
        .map(|program| match &config.shell.args {
            Some(args) => Shell::with_args(program, args.iter().map(OsString::from).collect()),
            None => Shell::new(program),
        });
    SessionOptions {
        shell,
        // A folder that is gone falls back to home instead of failing the spawn.
        cwd: config.shell.startup_dir.clone().filter(|dir| dir.is_dir()),
        scrollback: config.terminal.scrollback,
        cursor_shape: cursor_shape(config),
        ..SessionOptions::default()
    }
}
