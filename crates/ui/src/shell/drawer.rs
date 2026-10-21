//! A pane's file drawer: breadcrumb, filter and a virtual list of one folder (normally the
//! shell's cwd). Rows drag into the terminal to insert their path; double-click opens a folder
//! or inserts a file's path. The pane wires the events to the terminal.

use std::cmp::Ordering;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, UniformListScrollHandle, Window,
    div, px, uniform_list,
};

use crate::{Palette, Theme};

pub const DEFAULT_WIDTH: f32 = 220.;
pub const MIN_WIDTH: f32 = 150.;
pub const MAX_WIDTH: f32 = 520.;
/// Larger folders show the first entries and a note; the list stays responsive.
const MAX_ENTRIES: usize = 10_000;
const ROW_HEIGHT: f32 = 24.;

/// Drag payload of a drawer row; the terminal accepts it as a drop.
#[derive(Debug, Clone)]
pub struct DraggedPath(pub PathBuf);

pub enum DrawerEvent {
    /// Put this path at the terminal's cursor.
    Insert(PathBuf),
    /// The user opened this folder; with shell-follow on, the pane runs `cd`.
    Navigated(PathBuf),
}

#[derive(Debug, Clone)]
struct Entry {
    name: SharedString,
    path: PathBuf,
    is_dir: bool,
    size: Option<u64>,
    hidden: bool,
}

pub struct FileDrawer {
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Indices into `entries` after the filter and hidden toggle.
    visible: Vec<usize>,
    error: Option<SharedString>,
    truncated: bool,
    filter: Entity<InputState>,
    show_hidden: bool,
    follow: bool,
    /// A command is running, so shell-follow cannot send `cd` right now.
    follow_blocked: bool,
    /// Bumped per listing; an older listing that finishes late is dropped.
    epoch: u64,
    scroll: UniformListScrollHandle,
    _filter_events: Subscription,
}

impl FileDrawer {
    pub fn new(dir: PathBuf, follow: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let filter_events = cx.subscribe(&filter, |drawer, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                drawer.refilter(cx);
            }
        });
        let mut drawer = Self {
            dir: PathBuf::new(),
            entries: Vec::new(),
            visible: Vec::new(),
            error: None,
            truncated: false,
            filter,
            show_hidden: false,
            follow,
            follow_blocked: false,
            epoch: 0,
            scroll: UniformListScrollHandle::new(),
            _filter_events: filter_events,
        };
        drawer.set_dir(dir, cx);
        drawer
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    /// Greys the shell-follow toggle while a command runs; the preference stays.
    pub fn set_follow_blocked(&mut self, blocked: bool, cx: &mut Context<Self>) {
        if self.follow_blocked != blocked {
            self.follow_blocked = blocked;
            cx.notify();
        }
    }

    /// Shows `dir`; the same folder again only refreshes it.
    pub fn set_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if dir != self.dir {
            self.dir = dir;
            self.scroll = UniformListScrollHandle::new();
        }
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        let (epoch, dir) = (self.epoch, self.dir.clone());
        cx.spawn(async move |this, cx| {
            let listing = cx
                .background_executor()
                .spawn(async move { list_dir(&dir) })
                .await;
            _ = this.update(cx, |drawer, cx| {
                if drawer.epoch != epoch {
                    return;
                }
                match listing {
                    Ok((entries, truncated)) => {
                        drawer.entries = entries;
                        drawer.truncated = truncated;
                        drawer.error = None;
                    }
                    Err(err) => {
                        drawer.entries.clear();
                        drawer.truncated = false;
                        drawer.error = Some(err.to_string().into());
                    }
                }
                drawer.refilter(cx);
            });
        })
        .detach();
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let needle = self.filter.read(cx).value().to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_hidden || !e.hidden)
            .filter(|(_, e)| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        cx.notify();
    }

    /// Opens a folder in the drawer (and the shell, with shell-follow on).
    fn navigate(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.filter
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.set_dir(dir.clone(), cx);
        cx.emit(DrawerEvent::Navigated(dir));
    }

    fn open(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.is_dir {
            self.navigate(entry.path.clone(), window, cx);
        } else {
            cx.emit(DrawerEvent::Insert(entry.path.clone()));
        }
    }

    /// Rows `range` of the list: `..` first (when there is a parent), then the visible entries.
    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = Theme::current(cx).palette();
        let parent = self.dir.parent().map(Path::to_path_buf);
        let offset = usize::from(parent.is_some());
        range
            .map(|ix| {
                if ix < offset {
                    let up = parent.clone().unwrap_or_default();
                    return row_base(ix, p)
                        .on_click(cx.listener(
                            move |drawer, event: &gpui_kit::ClickEvent, window, cx| {
                                if event.click_count() >= 2 {
                                    drawer.navigate(up.clone(), window, cx);
                                }
                            },
                        ))
                        .child(
                            Icon::new(IconName::FolderOpen)
                                .size(px(14.))
                                .text_color(p.muted.to_hsla()),
                        )
                        .child(div().flex_1().child(".."))
                        .into_any_element();
                }
                let Some(entry) = self
                    .visible
                    .get(ix - offset)
                    .map(|&i| self.entries[i].clone())
                else {
                    return div().into_any_element();
                };
                let size = entry.size.map(format_size);
                let (icon, icon_color) = if entry.is_dir {
                    (IconName::Folder, p.now.to_hsla())
                } else {
                    (IconName::File, p.muted.to_hsla())
                };
                let drag = DraggedPath(entry.path.clone());
                let name = entry.name.clone();
                let label = entry.name.clone();
                row_base(ix, p)
                    .when(entry.hidden, |d| d.opacity(0.6))
                    .on_click(cx.listener(
                        move |drawer, event: &gpui_kit::ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                drawer.open(&entry, window, cx);
                            }
                        },
                    ))
                    .on_drag(drag, move |_, _, _, cx| {
                        let name = name.clone();
                        cx.new(|_| DragPreview { name })
                    })
                    .child(Icon::new(icon).size(px(14.)).text_color(icon_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(label),
                    )
                    .when_some(size, |d, size| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child(size),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_crumb(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut parts = crumbs(&self.dir);
        let last = parts.len().saturating_sub(1);
        let segments = parts.drain(..).enumerate().map(|(i, (label, path))| {
            let is_last = i == last;
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .when(i > 0, |d| {
                    d.child(div().text_color(p.muted.to_hsla()).opacity(0.7).child("›"))
                })
                .child(
                    div()
                        .id(("crumb", i))
                        .rounded(px(4.))
                        .when(is_last, |d| {
                            d.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(p.fg_emph.to_hsla())
                        })
                        .when(!is_last, |d| {
                            let emph = p.fg_emph.to_hsla();
                            d.cursor_pointer().hover(move |s| s.text_color(emph))
                        })
                        .on_click(cx.listener(move |drawer, _, window, cx| {
                            if !is_last {
                                drawer.navigate(path.clone(), window, cx);
                            }
                        }))
                        .child(label),
                )
        });

        let blocked = self.follow_blocked;
        let follow_tip: SharedString = match (blocked, self.follow) {
            (true, _) => "A command is running: shell-follow waits for the prompt".into(),
            (false, true) => {
                "Shell follows drawer: on (opening a folder runs cd in this shell)".into()
            }
            (false, false) => "Shell follows drawer: off".into(),
        };
        let hidden_tip = if self.show_hidden {
            "Hide hidden files"
        } else {
            "Show hidden files"
        };

        div()
            .flex()
            .items_start()
            .gap(px(6.))
            .flex_none()
            .pt(px(6.))
            .pr(px(6.))
            .pb(px(6.))
            .pl(px(10.))
            .text_size(px(11.5))
            .text_color(p.muted.to_hsla())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .gap_x(px(5.))
                    .gap_y(px(2.))
                    .pt(px(3.))
                    .children(segments),
            )
            .child(
                toggle_button("drawer-hidden", self.show_hidden, false, p)
                    .tooltip(move |window, cx| Tooltip::new(hidden_tip).build(window, cx))
                    .on_click(cx.listener(|drawer, _, _, cx| {
                        drawer.show_hidden = !drawer.show_hidden;
                        drawer.refilter(cx);
                    }))
                    .child(
                        Icon::new(if self.show_hidden {
                            IconName::Eye
                        } else {
                            IconName::EyeOff
                        })
                        .size(px(14.)),
                    ),
            )
            .child(
                toggle_button("drawer-sync", self.follow, blocked, p)
                    .tooltip(move |window, cx| Tooltip::new(follow_tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |drawer, _, _, cx| {
                        if !blocked {
                            drawer.follow = !drawer.follow;
                            cx.notify();
                        }
                    }))
                    .child(Icon::new(gpui_kit::assets::IconName::Link).size(px(14.))),
            )
            .child(
                toggle_button("drawer-pick", false, false, p)
                    .tooltip(|window, cx| Tooltip::new("Go to a folder…").build(window, cx))
                    .on_click(cx.listener(|drawer, _, window, cx| drawer.pick_folder(window, cx)))
                    .child(Icon::new(gpui_kit::assets::IconName::FolderSearch).size(px(14.))),
            )
    }

    /// Opens the system folder picker and navigates to the chosen folder.
    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Go to folder".into()),
        });
        cx.spawn_in(window, async move |drawer, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            _ = drawer.update_in(cx, |drawer, window, cx| drawer.navigate(dir, window, cx));
        })
        .detach();
    }
}

impl EventEmitter<DrawerEvent> for FileDrawer {}

impl Render for FileDrawer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let rows = self.visible.len() + usize::from(self.dir.parent().is_some());
        let note = match (&self.error, self.truncated) {
            (Some(err), _) => Some(err.clone()),
            (None, true) => Some(format!("Showing the first {MAX_ENTRIES} entries").into()),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .text_size(px(12.))
            .text_color(p.fg.to_hsla())
            .bg(p.surface_2.to_hsla())
            .child(self.render_crumb(p, cx))
            .child(
                div()
                    .flex_none()
                    .px(px(8.))
                    .pb(px(6.))
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .when_some(note, |d, note| {
                d.child(
                    div()
                        .flex_none()
                        .px(px(10.))
                        .pb(px(4.))
                        .text_size(px(11.))
                        .text_color(p.muted.to_hsla())
                        .child(note),
                )
            })
            .child(
                uniform_list(
                    "drawer-rows",
                    rows,
                    cx.processor(|drawer, range, _, cx| drawer.render_rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0()
                .px(px(4.))
                .pb(px(4.)),
            )
    }
}

/// The floating label under the mouse while a row or pane is dragged.
pub(super) struct DragPreview {
    pub(super) name: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        div()
            .px(px(8.))
            .py(px(3.))
            .rounded(px(6.))
            .text_size(px(12.))
            .text_color(p.fg_emph.to_hsla())
            .bg(p.accent_soft.to_hsla())
            .border_1()
            .border_color(p.accent.to_hsla())
            .child(self.name.clone())
    }
}

fn row_base(ix: usize, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = p.surface_3.to_hsla();
    div()
        .id(("drawer-row", ix))
        .flex()
        .items_center()
        .gap(px(7.))
        .h(px(ROW_HEIGHT))
        .px(px(8.))
        .rounded(px(6.))
        .hover(move |s| s.bg(hover))
}

/// A 22 px icon toggle like the mockup's `.drawer-sync`: accent ring when on, dimmed when
/// `disabled`.
fn toggle_button(
    id: &'static str,
    on: bool,
    disabled: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_3.to_hsla(), p.fg_emph.to_hsla());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(px(6.))
        .map(|d| {
            if disabled {
                d.text_color(p.muted.to_hsla()).opacity(0.45)
            } else if on {
                d.text_color(p.accent.to_hsla())
                    .bg(p.accent_soft.to_hsla())
                    .border_1()
                    .border_color(p.accent.to_hsla())
            } else {
                d.text_color(p.muted.to_hsla())
                    .hover(move |s| s.bg(hover).text_color(emph))
            }
        })
}

/// Breadcrumb segments, root first; paths under the home folder start at `~`.
fn crumbs(dir: &Path) -> Vec<(SharedString, PathBuf)> {
    let home = std::env::home_dir();
    let mut parts: Vec<(SharedString, PathBuf)> = Vec::new();
    for ancestor in dir.ancestors() {
        if home.as_deref() == Some(ancestor) {
            parts.push(("~".into(), ancestor.to_path_buf()));
            break;
        }
        let label = ancestor
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ancestor.display().to_string());
        parts.push((label.into(), ancestor.to_path_buf()));
    }
    parts.reverse();
    parts
}

/// The folder's entries, folders first then by name (case-insensitive), at most
/// [`MAX_ENTRIES`]; the flag says whether more were left out.
fn list_dir(dir: &Path) -> std::io::Result<(Vec<Entry>, bool)> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in fs::read_dir(dir)? {
        if entries.len() == MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(item) = item else { continue };
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        // `fs::metadata` follows links, so a link to a folder lists as a folder.
        let meta = fs::metadata(&path).or_else(|_| item.metadata());
        let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
        let hidden = name.starts_with('.') || meta.as_ref().is_ok_and(is_hidden_attr);
        entries.push(Entry {
            size: (!is_dir)
                .then(|| meta.as_ref().map(|m| m.len()).ok())
                .flatten(),
            name: name.into(),
            path,
            is_dir,
            hidden,
        });
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok((entries, truncated))
}

#[cfg(windows)]
fn is_hidden_attr(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden_attr(_: &fs::Metadata) -> bool {
    false
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_short() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(20 * 1024 * 1024), "20 MB");
    }

    #[test]
    fn folders_list_first() {
        let dir = std::env::temp_dir().join(format!("maomi-drawer-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("zeta")).unwrap();
        fs::write(dir.join("Alpha.txt"), "a").unwrap();
        fs::write(dir.join(".hidden"), "").unwrap();
        let (entries, truncated) = list_dir(&dir).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["zeta", ".hidden", "Alpha.txt"]);
        assert!(entries.iter().find(|e| e.name == ".hidden").unwrap().hidden);
        assert!(!truncated);
        _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn crumbs_end_at_the_folder() {
        let dir = std::env::temp_dir();
        let parts = crumbs(&dir);
        assert_eq!(parts.last().unwrap().1, dir);
    }
}
