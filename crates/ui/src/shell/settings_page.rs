//! The full-width settings page: Appearance, Shell and a read-only Keybindings list.
//!
//! Every control writes straight into the config through [`settings::update_config`]; windows
//! observe the config and re-apply it, so changes show everywhere at once.

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use maomi_term::{CursorShape, Shell, available_shells, default_shell};

use crate::settings::{self, CURSOR_STYLES, SYSTEM_MODE};
use crate::{Mode, Palette, Skin, TERMINAL_FAMILY, Theme, set_theme};

type LabelSelect = SelectState<SearchableVec<SharedString>>;

const SECTIONS: [&str; 4] = ["Appearance", "Shell", "Updates", "Keybindings"];

pub struct SettingsPage {
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    /// Section highlighted in the left nav (the last one clicked).
    section: usize,
    font: Entity<LabelSelect>,
    shell: Entity<LabelSelect>,
    /// Shells offered by the shell select; index 0 is "Automatic" (None).
    shells: Vec<Option<Shell>>,
    font_size: Entity<InputState>,
    scrollback: Entity<InputState>,
    startup_dir: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = settings::config(cx);

        // The bundled font first, then every installed family (not only monospace ones: the
        // text system cannot tell them apart).
        let mut fonts = cx.text_system().all_font_names();
        fonts.sort_unstable_by_key(|f| f.to_lowercase());
        fonts.dedup();
        fonts.retain(|f| f != TERMINAL_FAMILY && !f.starts_with('.'));
        fonts.insert(0, TERMINAL_FAMILY.into());
        let font_ix = fonts
            .iter()
            .position(|f| *f == config.terminal.font_family)
            .unwrap_or(0);
        let font_labels: Vec<SharedString> = fonts.into_iter().map(Into::into).collect();
        let font = cx.new(|cx| {
            LabelSelect::new(
                SearchableVec::new(font_labels),
                Some(IndexPath::new(font_ix)),
                window,
                cx,
            )
            .searchable(true)
        });

        let mut shells: Vec<Option<Shell>> = vec![None];
        shells.extend(available_shells().into_iter().map(Some));
        let labels = shell_labels(&shells);
        let shell_ix = config
            .shell
            .program
            .as_ref()
            .and_then(|program| {
                shells
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| s.program == *program))
            })
            .unwrap_or(0);
        let shell = cx.new(|cx| {
            LabelSelect::new(
                SearchableVec::new(labels),
                Some(IndexPath::new(shell_ix)),
                window,
                cx,
            )
        });

        let font_size = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.0)
                .min(6.0)
                .max(72.0)
                .default_value(format_number(config.terminal.font_size))
        });
        let scrollback = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1000.0)
                .min(0.0)
                .max(1_000_000.0)
                .default_value(config.terminal.scrollback.to_string())
        });
        let startup_dir = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Home folder")
                .default_value(
                    config
                        .shell
                        .startup_dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                )
        });

        let subscriptions = vec![
            cx.subscribe_in(&font, window, |_, _, event: &SelectEvent<_>, _, cx| {
                let SelectEvent::Confirm(Some(family)) = event else {
                    return;
                };
                let family = family.to_string();
                settings::update_config(cx, |c| c.terminal.font_family = family);
            }),
            cx.subscribe_in(&shell, window, |page, _, event: &SelectEvent<_>, _, cx| {
                let SelectEvent::Confirm(Some(label)) = event else {
                    return;
                };
                let labels = shell_labels(&page.shells);
                let Some(ix) = labels.iter().position(|l| l == label) else {
                    return;
                };
                let program = page.shells[ix].as_ref().map(|s| s.program.clone());
                settings::update_config(cx, |c| {
                    c.shell.program = program;
                    c.shell.args = None;
                });
            }),
            cx.subscribe_in(&font_size, window, |_, input, event: &InputEvent, _, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if let Ok(size) = input.read(cx).value().trim().parse::<f32>() {
                    settings::update_config(cx, |c| c.terminal.font_size = size);
                }
            }),
            cx.subscribe_in(
                &scrollback,
                window,
                |_, input, event: &InputEvent, _, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    if let Ok(lines) = input.read(cx).value().trim().parse::<usize>() {
                        settings::update_config(cx, |c| c.terminal.scrollback = lines);
                    }
                },
            ),
            cx.subscribe_in(
                &startup_dir,
                window,
                |_, input, event: &InputEvent, _, cx| {
                    // Saved on Enter or leaving the field, so half-typed paths are not stored.
                    if !matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        return;
                    }
                    let text = input.read(cx).value().trim().to_string();
                    settings::update_config(cx, |c| {
                        c.shell.startup_dir = (!text.is_empty()).then(|| text.into());
                    });
                },
            ),
            // Redraw when the theme or config changes elsewhere (title bar, shortcuts).
            cx.observe_global::<settings::Settings>(|_, cx| cx.notify()),
        ];

        Self {
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            section: 0,
            font,
            shell,
            shells,
            font_size,
            scrollback,
            startup_dir,
            _subscriptions: subscriptions,
        }
    }

    fn render_nav(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let links = SECTIONS.iter().enumerate().map(|(i, name)| {
            let on = i == self.section;
            let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
            div()
                .id(("settings-nav", i))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(7.))
                .text_size(px(13.))
                .map(|d| {
                    if on {
                        d.bg(p.surface_2.to_hsla()).text_color(emph)
                    } else {
                        d.text_color(p.muted.to_hsla())
                            .hover(move |s| s.bg(hover).text_color(emph))
                    }
                })
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.section = i;
                    // Children of the scroll area: 0 is the heading, sections follow.
                    page.scroll.scroll_to_top_of_item(i + 1);
                    cx.notify();
                }))
                .child(*name)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .flex_none()
            .w(px(200.))
            .p(px(12.))
            .border_r_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .px(px(10.))
                    .pb(px(8.))
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child("SETTINGS"),
            )
            .children(links)
    }

    fn render_appearance(&self, theme: Theme, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let config = settings::config(cx);
        let follows_system = settings::follows_system(&config);

        let skins = [Skin::Aurora, Skin::Onyx, Skin::Mochi].map(|skin| SegItem {
            id: SharedString::from(format!("seg-skin-{}", skin.key())),
            label: skin.name().into(),
            on: theme.skin == skin,
            enabled: true,
            on_click: Box::new(move |_, cx| {
                let current = Theme::current(cx);
                set_theme(cx, Theme::new(skin, current.mode));
            }),
        });
        let mochi = !theme.mode_toggle_enabled();
        let mut modes: Vec<SegItem> = [Mode::Dark, Mode::Light]
            .map(|mode| SegItem {
                id: SharedString::from(format!("seg-mode-{}", mode.key())),
                label: if mode == Mode::Dark { "Dark" } else { "Light" }.into(),
                on: !follows_system && theme.effective_mode() == mode,
                enabled: !mochi,
                on_click: Box::new(move |_, cx| {
                    settings::update_config(cx, |c| c.appearance.mode = mode.key().into());
                    let current = Theme::current(cx);
                    set_theme(cx, Theme::new(current.skin, mode));
                }),
            })
            .into();
        modes.push(SegItem {
            id: "seg-mode-system".into(),
            label: "System".into(),
            on: follows_system,
            enabled: !mochi,
            on_click: Box::new(|_, cx| {
                settings::update_config(cx, |c| c.appearance.mode = SYSTEM_MODE.into());
                settings::sync_system_theme(cx);
            }),
        });
        let current_shape = settings::cursor_shape(&config);
        let cursors = CURSOR_STYLES.map(|(key, label, shape)| SegItem {
            id: SharedString::from(format!("seg-cursor-{key}")),
            label: label.into(),
            on: current_shape == shape,
            enabled: true,
            on_click: Box::new(move |_, cx| {
                settings::update_config(cx, |c| c.terminal.cursor_style = key.into());
            }),
        });
        let blink = config.terminal.cursor_blink;
        let smooth = config.terminal.smooth_scroll;

        section(
            "Appearance",
            p,
            vec![
                row(
                    "Skin",
                    Some("Colour family for the whole app"),
                    p,
                    segmented(skins, p),
                ),
                row(
                    "Mode",
                    Some("System follows your OS setting. Mochi is dark only."),
                    p,
                    segmented(modes, p),
                ),
                row(
                    "Font family",
                    Some("Fonts installed on this machine"),
                    p,
                    div()
                        .w(px(240.))
                        .child(Select::new(&self.font).menu_max_h(px(320.)))
                        .into_any_element(),
                ),
                row(
                    "Font size",
                    Some("In points"),
                    p,
                    div()
                        .w(px(120.))
                        .child(NumberInput::new(&self.font_size))
                        .into_any_element(),
                ),
                row("Cursor style", None, p, segmented(cursors, p)),
                row(
                    "Cursor blink",
                    None,
                    p,
                    Switch::new("switch-blink")
                        .checked(blink)
                        .on_click(|on, _, cx| {
                            let on = *on;
                            settings::update_config(cx, |c| c.terminal.cursor_blink = on);
                        })
                        .into_any_element(),
                ),
                row(
                    "Smooth scrolling",
                    Some("Eases new output in instead of jumping"),
                    p,
                    Switch::new("switch-smooth")
                        .checked(smooth)
                        .on_click(|on, _, cx| {
                            let on = *on;
                            settings::update_config(cx, |c| c.terminal.smooth_scroll = on);
                        })
                        .into_any_element(),
                ),
                preview(&config, theme, p),
            ],
        )
    }

    fn render_shell(&self, p: &Palette) -> AnyElement {
        section(
            "Shell",
            p,
            vec![
                row(
                    "Default shell",
                    Some("Used for new tabs and panes"),
                    p,
                    div()
                        .w(px(240.))
                        .child(Select::new(&self.shell))
                        .into_any_element(),
                ),
                row(
                    "Startup directory",
                    Some("Empty = the home folder. Saved on Enter."),
                    p,
                    div()
                        .w(px(280.))
                        .child(Input::new(&self.startup_dir))
                        .into_any_element(),
                ),
                row(
                    "Scrollback lines",
                    Some("Lines of history kept per pane"),
                    p,
                    div()
                        .w(px(140.))
                        .child(NumberInput::new(&self.scrollback))
                        .into_any_element(),
                ),
            ],
        )
    }

    fn render_updates(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let auto = settings::config(cx).updates.auto;
        section(
            "Updates",
            p,
            vec![
                row(
                    "Automatic updates",
                    Some(
                        "Downloads new releases in the background and installs them when you quit",
                    ),
                    p,
                    Switch::new("switch-updates")
                        .checked(auto)
                        .on_click(|on, _, cx| {
                            let on = *on;
                            settings::update_config(cx, |c| c.updates.auto = on);
                        })
                        .into_any_element(),
                ),
                row(
                    "Version",
                    None,
                    p,
                    div()
                        .text_color(p.muted.to_hsla())
                        .child(env!("CARGO_PKG_VERSION"))
                        .into_any_element(),
                ),
            ],
        )
    }
}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let p = theme.palette();
        let appearance = self.render_appearance(theme, p, cx);
        let shell = self.render_shell(p);
        let updates = self.render_updates(p, cx);
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .size_full()
            .min_h_0()
            .child(self.render_nav(p, cx))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(22.))
                            .max_w(px(760.))
                            .px(px(28.))
                            .py(px(24.))
                            .child(
                                div()
                                    .child(
                                        div()
                                            .text_size(px(20.))
                                            .text_color(p.fg_emph.to_hsla())
                                            .child("Settings"),
                                    )
                                    .child(
                                        div()
                                            .text_color(p.muted.to_hsla())
                                            .child("Changes apply to every window straight away."),
                                    ),
                            )
                            .child(appearance)
                            .child(shell)
                            .child(updates)
                            .child(keybindings(p)),
                    ),
            )
    }
}

/// Labels for the shell select; index 0 names the shell "Automatic" would pick.
fn shell_labels(shells: &[Option<Shell>]) -> Vec<SharedString> {
    let mut labels: Vec<String> = shells
        .iter()
        .map(|s| match s {
            None => format!("Automatic ({})", default_shell().display_name()),
            Some(shell) => shell.display_name(),
        })
        .collect();
    // Two shells with one name (e.g. two bash installs) get their paths to tell them apart.
    let names = labels.clone();
    for (label, shell) in labels.iter_mut().zip(shells) {
        if let Some(shell) = shell
            && names.iter().filter(|n| *n == label).count() > 1
        {
            *label = format!("{label} ({})", shell.program.display());
        }
    }
    labels.into_iter().map(Into::into).collect()
}

fn format_number(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

struct SegItem {
    id: SharedString,
    label: SharedString,
    on: bool,
    enabled: bool,
    on_click: ClickHandler,
}

type ClickHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// A row of toggle buttons, styled like the title bar's skin switcher.
fn segmented(items: impl IntoIterator<Item = SegItem>, p: &Palette) -> AnyElement {
    let emph = p.fg_emph.to_hsla();
    div()
        .flex()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(9.))
        .bg(p.surface_2.to_hsla())
        .children(items.into_iter().map(|item| {
            let SegItem {
                id,
                label,
                on,
                enabled,
                on_click,
            } = item;
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(7.))
                .text_size(px(12.))
                .map(|d| {
                    if on {
                        d.text_color(emph).bg(p.surface.to_hsla())
                    } else {
                        d.text_color(p.muted.to_hsla())
                    }
                })
                .when(!enabled, |d| d.opacity(0.5))
                .when(enabled && !on, |d| {
                    d.hover(move |s| s.text_color(emph))
                        .on_click(move |_, window, cx| on_click(window, cx))
                })
                .child(label)
        }))
        .into_any_element()
}

fn section(title: &'static str, p: &Palette, rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child(title.to_uppercase()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .px(px(16.))
                .rounded(px(12.))
                .border_1()
                .border_color(p.line.to_hsla())
                .bg(p.surface.to_hsla())
                .children(rows),
        )
        .into_any_element()
}

fn row(
    label: &'static str,
    hint: Option<&'static str>,
    p: &Palette,
    control: AnyElement,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(p.line.to_hsla())
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(div().text_color(p.fg_emph.to_hsla()).child(label))
                .when_some(hint, |d, hint| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted.to_hsla())
                            .child(hint),
                    )
                }),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

/// A few lines in the chosen font, size and cursor style, in the terminal colours.
fn preview(config: &maomi_config::Config, theme: Theme, p: &Palette) -> AnyElement {
    let colors = theme.terminal_colors();
    let font = settings::terminal_font(config);
    let span =
        |text: &'static str, color: crate::Color| div().text_color(color.to_hsla()).child(text);
    let cursor = {
        let c = colors.cursor.to_hsla();
        let cell = div().w(font.size * 0.6).h(font.size * font.line_height);
        match settings::cursor_shape(config) {
            CursorShape::Beam => cell.child(div().w(px(2.)).h_full().bg(c)),
            CursorShape::Underline => cell
                .flex()
                .flex_col()
                .justify_end()
                .child(div().h(px(2.)).bg(c)),
            _ => cell.bg(c),
        }
    };
    let prompt = |cmd: Vec<gpui_kit::Div>| {
        div()
            .flex()
            .gap(font.size * 0.6)
            .child(span("PS", colors.ansi[2]))
            .child(span("~/GitProjects>", colors.ansi[6]))
            .children(cmd)
    };
    div()
        .py(px(12.))
        .child(
            div()
                .pb(px(8.))
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child("PREVIEW"),
        )
        .child(
            div()
                .p(px(12.))
                .rounded(px(10.))
                .bg(colors.bg.to_hsla())
                .text_color(colors.fg.to_hsla())
                .font_family(font.family.clone())
                .text_size(font.size)
                .line_height(font.size * font.line_height)
                .child(prompt(vec![span("ls", colors.fg)]))
                .child(
                    div()
                        .flex()
                        .gap(font.size * 1.2)
                        .child(span("src", colors.ansi[4]))
                        .child(span("assets", colors.ansi[4]))
                        .child(span("build.ps1", colors.ansi[2]))
                        .child("Cargo.toml"),
                )
                .child(prompt(vec![
                    span("cargo", colors.ansi[3]),
                    div().flex().gap(font.size * 0.6).child("run").child(cursor),
                ])),
        )
        .into_any_element()
}

/// The shortcuts registered in `shell::init`, `crate::init` and the terminal view.
fn keybindings(p: &Palette) -> AnyElement {
    let primary = if cfg!(target_os = "macos") {
        "Cmd+"
    } else {
        "Ctrl+Shift+"
    };
    let settings_key = if cfg!(target_os = "macos") {
        "Cmd+,"
    } else {
        "Ctrl+,"
    };
    let rows: Vec<(&str, String)> = vec![
        ("New tab", format!("{primary}T")),
        ("Close pane", format!("{primary}W")),
        ("Split right", format!("{primary}D")),
        ("Split down", format!("{primary}E")),
        ("New window", format!("{primary}N")),
        ("Focus pane", "Alt+Arrows".into()),
        ("Next tab", "Ctrl+Tab".into()),
        ("Previous tab", "Ctrl+Shift+Tab".into()),
        ("Copy", format!("{primary}C")),
        ("Paste", format!("{primary}V")),
        ("Scroll history", "Shift+PageUp / PageDown".into()),
        ("Next skin", "Ctrl+Shift+K".into()),
        ("Toggle dark / light", "Ctrl+Shift+L".into()),
        ("Settings", settings_key.into()),
    ];
    let kbd_bg = p.surface_2.to_hsla();
    let rows = rows.into_iter().map(|(action, keys)| {
        div()
            .flex()
            .items_center()
            .justify_between()
            .py(px(7.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(action)
            .child(
                div()
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(5.))
                    .bg(kbd_bg)
                    .text_size(px(12.))
                    .text_color(p.fg_emph.to_hsla())
                    .child(keys),
            )
            .into_any_element()
    });
    let mut items = vec![
        div()
            .py(px(10.))
            .text_size(px(12.))
            .text_color(p.muted.to_hsla())
            .child("Read-only for now. Editing comes later.")
            .into_any_element(),
    ];
    items.extend(rows);
    section("Keybindings", p, items)
}
