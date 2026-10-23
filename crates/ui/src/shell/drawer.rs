//! A pane's file drawer: breadcrumb, filter and a virtual list of one folder (normally the
//! shell's cwd). Rows drag into the terminal to insert their path; double-click opens a folder
//! or inserts a file's path. The pane wires the events to the terminal.
//!
//! SSH panes get a remote drawer that lists the server over SFTP. Files dropped on it from
//! the OS or a local drawer upload; remote rows dropped on a local drawer download. Remote
//! rows have a context menu for downloading, renaming, permissions and deleting.

use std::cmp::Ordering;
use std::fs;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, ExternalPaths,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, Window, div, px, relative, uniform_list,
};
use maomi_ssh::{Progress, RemoteEntry, Sftp, SshError, SshHandle, join_remote};

use super::ssh::{self, TextPrompt};
use crate::{Palette, Theme};

pub const DEFAULT_WIDTH: f32 = 220.;
pub const MIN_WIDTH: f32 = 150.;
pub const MAX_WIDTH: f32 = 520.;
/// Larger folders show the first entries and a note; the list stays responsive.
const MAX_ENTRIES: usize = 10_000;
const ROW_HEIGHT: f32 = 24.;
/// How often running transfers redraw their progress.
const PROGRESS_TICK: Duration = Duration::from_millis(120);

/// Drag payload of a drawer row; the terminal accepts it as a drop.
#[derive(Debug, Clone)]
pub struct DraggedPath {
    pub path: PathBuf,
    /// Set for a remote row: dropping it on a local drawer downloads it.
    pub sftp: Option<Sftp>,
}

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
    /// Permission bits of a remote entry.
    mode: Option<u32>,
}

/// Where the drawer lists files.
enum Source {
    Local,
    /// None until the pane's connection is up and SFTP has started.
    Remote {
        sftp: Option<Sftp>,
        home: Option<PathBuf>,
    },
}

struct Transfer {
    id: u64,
    name: SharedString,
    upload: bool,
    progress: Arc<Progress>,
    error: Option<SharedString>,
}

pub struct FileDrawer {
    source: Source,
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
    transfers: Vec<Transfer>,
    next_transfer: u64,
    /// The progress redraw loop is running.
    ticking: bool,
    _filter_events: Subscription,
}

impl FileDrawer {
    pub fn new(dir: PathBuf, follow: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut drawer = Self::empty(Source::Local, follow, window, cx);
        drawer.set_dir(dir, cx);
        drawer
    }

    /// A drawer for an SSH pane; it lists files once [`Self::set_remote`] connects it.
    pub fn new_remote(follow: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let source = Source::Remote {
            sftp: None,
            home: None,
        };
        let mut drawer = Self::empty(source, follow, window, cx);
        drawer.error = Some("Waiting for the connection…".into());
        drawer
    }

    fn empty(source: Source, follow: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let filter_events = cx.subscribe(&filter, |drawer, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                drawer.refilter(cx);
            }
        });
        Self {
            source,
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
            transfers: Vec::new(),
            next_transfer: 0,
            ticking: false,
            _filter_events: filter_events,
        }
    }

    /// Starts SFTP on the pane's (new) connection. The folder stays across reconnects;
    /// the first connection opens the login folder.
    pub fn set_remote(&mut self, handle: SshHandle, cx: &mut Context<Self>) {
        self.error = Some("Starting SFTP…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let started = async {
                let sftp = handle.sftp().await?;
                let home = sftp.home().await.ok().map(PathBuf::from);
                Ok::<_, SshError>((sftp, home))
            }
            .await;
            _ = this.update(cx, |drawer, cx| match started {
                Ok((sftp, home)) => {
                    let dir = if drawer.dir.as_os_str().is_empty() {
                        home.clone().unwrap_or_else(|| PathBuf::from("/"))
                    } else {
                        drawer.dir.clone()
                    };
                    drawer.source = Source::Remote {
                        sftp: Some(sftp),
                        home,
                    };
                    drawer.error = None;
                    drawer.set_dir(dir, cx);
                }
                Err(err) => {
                    drawer.error = Some(format!("SFTP: {err}").into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn sftp(&self) -> Option<&Sftp> {
        match &self.source {
            Source::Remote { sftp, .. } => sftp.as_ref(),
            Source::Local => None,
        }
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
        let remote = match &self.source {
            Source::Local => None,
            Source::Remote {
                sftp: Some(sftp), ..
            } => Some(sftp.clone()),
            Source::Remote { sftp: None, .. } => return,
        };
        self.epoch += 1;
        let (epoch, dir) = (self.epoch, self.dir.clone());
        cx.spawn(async move |this, cx| {
            let listing = match remote {
                None => {
                    cx.background_executor()
                        .spawn(async move { list_dir(&dir).map_err(|e| e.to_string()) })
                        .await
                }
                Some(sftp) => {
                    let dir = remote_path(&dir);
                    sftp.list(&dir)
                        .await
                        .map(|entries| remote_listing(&dir, entries))
                        .map_err(|e| e.to_string())
                }
            };
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
                        drawer.error = Some(err.into());
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

    /// A drawer row or pane dropped here: remote rows download into a local drawer, local
    /// rows upload into a remote one.
    fn drop_path(&mut self, dragged: &DraggedPath, cx: &mut Context<Self>) {
        match (self.sftp().cloned(), &dragged.sftp) {
            (Some(_), None) => self.upload(vec![dragged.path.clone()], cx),
            (None, Some(sftp)) if matches!(self.source, Source::Local) => {
                self.download(sftp.clone(), dragged.path.clone(), self.dir.clone(), cx)
            }
            _ => {}
        }
    }

    /// Uploads local files and folders into the folder on show.
    fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let dir = remote_path(&self.dir);
        for path in paths {
            let name = file_name(&path);
            let (sftp, dir) = (sftp.clone(), dir.clone());
            self.start_transfer(name, true, cx, move |progress| async move {
                sftp.upload(path, &dir, progress).await
            });
        }
    }

    fn download(
        &mut self,
        sftp: Sftp,
        remote: PathBuf,
        local_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let name = file_name(&remote);
        let remote = remote_path(&remote);
        self.start_transfer(name, false, cx, move |progress| async move {
            sftp.download(&remote, local_dir, progress).await
        });
    }

    fn start_transfer<F>(
        &mut self,
        name: String,
        upload: bool,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<Progress>) -> F,
    ) where
        F: Future<Output = Result<(), SshError>> + 'static,
    {
        let progress = Arc::new(Progress::default());
        let id = self.next_transfer;
        self.next_transfer += 1;
        self.transfers.push(Transfer {
            id,
            name: name.into(),
            upload,
            progress: progress.clone(),
            error: None,
        });
        let work = work(progress);
        cx.spawn(async move |this, cx| {
            let result = work.await;
            _ = this.update(cx, |drawer, cx| {
                match result {
                    Ok(()) | Err(SshError::Cancelled) => drawer.transfers.retain(|t| t.id != id),
                    Err(err) => {
                        if let Some(transfer) = drawer.transfers.iter_mut().find(|t| t.id == id) {
                            transfer.error = Some(err.to_string().into());
                        }
                    }
                }
                // The folder on show may have new files.
                drawer.reload(cx);
            });
        })
        .detach();
        self.tick(cx);
    }

    /// Redraws while transfers run.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_TICK).await;
                let running = this
                    .update(cx, |drawer, cx| {
                        cx.notify();
                        let running = drawer.transfers.iter().any(|t| t.error.is_none());
                        drawer.ticking = running;
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    /// Runs a change on the server, then refreshes; a failure shows as a notification.
    fn change(
        &mut self,
        change: impl Future<Output = Result<(), SshError>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = change.await;
            _ = this.update_in(cx, |drawer, window, cx| {
                if let Err(err) = result {
                    window.push_notification(Notification::error(err.to_string()), cx);
                }
                drawer.reload(cx);
            });
        })
        .detach();
    }

    fn download_to(&mut self, entry: &Entry, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Download here".into()),
        });
        let remote = entry.path.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            _ = this.update(cx, |drawer, cx| drawer.download(sftp, remote, dir, cx));
        })
        .detach();
    }

    fn rename(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let (from, dir) = (remote_path(&entry.path), remote_path(&self.dir));
        let this = cx.entity().downgrade();
        ssh::ask_text(
            TextPrompt {
                title: format!("Rename {}", entry.name).into(),
                label: "New name",
                sub: "".into(),
                initial: entry.name.to_string(),
                ok_label: "Rename",
            },
            window,
            cx,
            move |name, window, cx| {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let to = if name.starts_with('/') {
                    name
                } else {
                    join_remote(&dir, &name)
                };
                _ = this.update(cx, |drawer, cx| {
                    drawer.change(async move { sftp.rename(&from, &to).await }, window, cx)
                });
            },
        );
    }

    fn permissions(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let path = remote_path(&entry.path);
        let mode = entry
            .mode
            .unwrap_or(if entry.is_dir { 0o755 } else { 0o644 });
        let this = cx.entity().downgrade();
        ssh::ask_text(
            TextPrompt {
                title: format!("Permissions of {}", entry.name).into(),
                label: "Mode (octal)",
                sub: format!(
                    "Now {} ({mode:o}). For example 644 for files, 755 for folders and scripts.",
                    mode_string(mode)
                )
                .into(),
                initial: format!("{mode:o}"),
                ok_label: "Apply",
            },
            window,
            cx,
            move |text, window, cx| {
                let Some(mode) = u32::from_str_radix(text.trim(), 8)
                    .ok()
                    .filter(|m| *m <= 0o7777)
                else {
                    window.push_notification(
                        Notification::error(format!("{text:?} is not an octal mode like 644")),
                        cx,
                    );
                    return;
                };
                _ = this.update(cx, |drawer, cx| {
                    drawer.change(async move { sftp.set_mode(&path, mode).await }, window, cx)
                });
            },
        );
    }

    fn delete(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let path = remote_path(&entry.path);
        let what = if entry.is_dir {
            "This deletes the folder and everything in it on the server. It cannot be undone."
        } else {
            "This deletes the file on the server. It cannot be undone."
        };
        let this = cx.entity().downgrade();
        ssh::confirm(
            format!("Delete {}?", entry.name),
            what,
            "Delete",
            window,
            cx,
            move |window, cx| {
                _ = this.update(cx, |drawer, cx| {
                    drawer.change(async move { sftp.remove(&path).await }, window, cx)
                });
            },
        );
    }

    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp().cloned() else {
            return;
        };
        let dir = remote_path(&self.dir);
        let this = cx.entity().downgrade();
        ssh::ask_text(
            TextPrompt {
                title: "New folder".into(),
                label: "Name",
                sub: "".into(),
                initial: String::new(),
                ok_label: "Create",
            },
            window,
            cx,
            move |name, window, cx| {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let path = join_remote(&dir, &name);
                _ = this.update(cx, |drawer, cx| {
                    drawer.change(async move { sftp.create_dir(&path).await }, window, cx)
                });
            },
        );
    }

    /// Rows `range` of the list: `..` first (when there is a parent), then the visible entries.
    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = Theme::current(cx).palette();
        let parent = self.dir.parent().map(Path::to_path_buf);
        let offset = usize::from(parent.is_some());
        let sftp = self.sftp().cloned();
        let this = cx.entity().downgrade();
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
                let drag = DraggedPath {
                    path: entry.path.clone(),
                    sftp: sftp.clone(),
                };
                let name = entry.name.clone();
                let label = entry.name.clone();
                let menu_entry = entry.clone();
                let row = row_base(ix, p)
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
                    });
                if sftp.is_none() {
                    return row.into_any_element();
                }
                let this = this.clone();
                row.context_menu(move |menu, _, _| {
                    let item = |label: &'static str,
                                act: fn(
                        &mut FileDrawer,
                        &Entry,
                        &mut Window,
                        &mut Context<FileDrawer>,
                    )| {
                        let (this, entry) = (this.clone(), menu_entry.clone());
                        PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            _ = this.update(cx, |drawer, cx| act(drawer, &entry, window, cx));
                        })
                    };
                    menu.item(item("Download to…", FileDrawer::download_to))
                        .item(item("Rename…", FileDrawer::rename))
                        .item(item("Permissions…", FileDrawer::permissions))
                        .separator()
                        .item(item("Delete…", FileDrawer::delete))
                })
                .into_any_element()
            })
            .collect()
    }

    fn render_crumb(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let home = match &self.source {
            Source::Local => std::env::home_dir(),
            Source::Remote { home, .. } => home.clone(),
        };
        let mut parts = crumbs(&self.dir, home.as_deref());
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
        let remote = matches!(self.source, Source::Remote { .. });

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
            .map(|d| {
                if remote {
                    d.child(
                        toggle_button("drawer-new-folder", false, false, p)
                            .tooltip(|window, cx| Tooltip::new("New folder…").build(window, cx))
                            .on_click(
                                cx.listener(|drawer, _, window, cx| drawer.new_folder(window, cx)),
                            )
                            .child(Icon::new(gpui_kit::assets::IconName::FolderPlus).size(px(14.))),
                    )
                    .child(
                        toggle_button("drawer-refresh", false, false, p)
                            .tooltip(|window, cx| Tooltip::new("Refresh").build(window, cx))
                            .on_click(cx.listener(|drawer, _, _, cx| drawer.reload(cx)))
                            .child(Icon::new(gpui_kit::assets::IconName::RefreshCw).size(px(14.))),
                    )
                } else {
                    d.child(
                        toggle_button("drawer-pick", false, false, p)
                            .tooltip(|window, cx| Tooltip::new("Go to a folder…").build(window, cx))
                            .on_click(
                                cx.listener(|drawer, _, window, cx| drawer.pick_folder(window, cx)),
                            )
                            .child(
                                Icon::new(gpui_kit::assets::IconName::FolderSearch).size(px(14.)),
                            ),
                    )
                }
            })
    }

    /// Running and failed transfers, newest last.
    fn render_transfers(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(6.))
            .px(px(10.))
            .py(px(8.))
            .border_t_1()
            .border_color(p.line.to_hsla())
            .children(self.transfers.iter().map(|transfer| {
                let done = transfer.progress.done.load(AtomicOrdering::Relaxed);
                let total = transfer.progress.total.load(AtomicOrdering::Relaxed);
                let fraction = if total == 0 {
                    0.
                } else {
                    (done as f64 / total as f64).min(1.) as f32
                };
                let (id, progress) = (transfer.id, transfer.progress.clone());
                let failed = transfer.error.is_some();
                let status: SharedString = match &transfer.error {
                    Some(error) => error.clone(),
                    None => format!("{}%", (fraction * 100.).round()).into(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                Icon::new(if transfer.upload {
                                    gpui_kit::assets::IconName::Upload
                                } else {
                                    gpui_kit::assets::IconName::Download
                                })
                                .size(px(13.))
                                .text_color(p.muted.to_hsla()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(p.fg.to_hsla())
                                    .child(transfer.name.clone()),
                            )
                            .when(!failed, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(p.muted.to_hsla())
                                        .child(status.clone()),
                                )
                            })
                            .child(
                                toggle_button_id(("transfer-close", id as usize), p)
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(if failed { "Dismiss" } else { "Cancel" })
                                            .build(window, cx)
                                    })
                                    .on_click(cx.listener(move |drawer, _, _, cx| {
                                        progress.cancel.store(true, AtomicOrdering::Relaxed);
                                        if failed {
                                            drawer.transfers.retain(|t| t.id != id);
                                        }
                                        cx.notify();
                                    }))
                                    .child(Icon::new(IconName::Close).size(px(12.))),
                            ),
                    )
                    .map(|d| {
                        if failed {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.err.to_hsla())
                                    .child(status),
                            )
                        } else {
                            d.child(
                                div()
                                    .h(px(3.))
                                    .rounded(px(2.))
                                    .bg(p.surface_3.to_hsla())
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(fraction))
                                            .rounded(px(2.))
                                            .bg(p.accent.to_hsla()),
                                    ),
                            )
                        }
                    })
            }))
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
        let accent_soft = p.accent_soft.to_hsla();
        let remote = self.sftp().is_some();
        div()
            .id("drawer")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .text_size(px(12.))
            .text_color(p.fg.to_hsla())
            .bg(p.surface_2.to_hsla())
            // The drawer sits in the pane's bottom-left corner; rounding it keeps the pane's
            // focus ring visible there (GPUI clips children to a square).
            .rounded_bl(px(7.))
            // Uploads: files from the OS, or rows of a local drawer.
            .when(remote, |d| {
                d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(accent_soft))
                    .on_drop(cx.listener(|drawer, paths: &ExternalPaths, _, cx| {
                        drawer.upload(paths.paths().to_vec(), cx)
                    }))
            })
            .drag_over::<DraggedPath>(move |s, _, _, _| s.bg(accent_soft))
            .on_drop(
                cx.listener(|drawer, dragged: &DraggedPath, _, cx| drawer.drop_path(dragged, cx)),
            )
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
            .when(!self.transfers.is_empty(), |d| {
                d.child(self.render_transfers(p, cx))
            })
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

/// A small icon button with a numbered id, for per-row buttons.
fn toggle_button_id(id: (&'static str, usize), p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_3.to_hsla(), p.fg_emph.to_hsla());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(18.))
        .rounded(px(5.))
        .text_color(p.muted.to_hsla())
        .hover(move |s| s.bg(hover).text_color(emph))
}

/// Breadcrumb segments, root first; paths under `home` start at `~`.
fn crumbs(dir: &Path, home: Option<&Path>) -> Vec<(SharedString, PathBuf)> {
    let mut parts: Vec<(SharedString, PathBuf)> = Vec::new();
    for ancestor in dir.ancestors() {
        if home == Some(ancestor) {
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

/// A remote path kept in a `PathBuf`, back as the POSIX string SFTP wants.
fn remote_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if path.is_empty() { "/".into() } else { path }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `rwxr-xr-x` for a mode.
fn mode_string(mode: u32) -> String {
    (0..9)
        .map(|bit| {
            if mode & (0o400 >> bit) == 0 {
                '-'
            } else {
                ['r', 'w', 'x'][bit % 3]
            }
        })
        .collect()
}

/// A remote listing as drawer entries, sorted and capped like a local one.
fn remote_listing(dir: &str, items: Vec<RemoteEntry>) -> (Vec<Entry>, bool) {
    let truncated = items.len() > MAX_ENTRIES;
    let mut entries: Vec<Entry> = items
        .into_iter()
        .take(MAX_ENTRIES)
        .map(|item| Entry {
            path: PathBuf::from(join_remote(dir, &item.name)),
            hidden: item.name.starts_with('.'),
            size: (!item.is_dir).then_some(item.size),
            is_dir: item.is_dir,
            mode: item.mode,
            name: item.name.into(),
        })
        .collect();
    sort_entries(&mut entries);
    (entries, truncated)
}

/// Folders first, then by name (case-insensitive).
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
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
            mode: None,
        });
    }
    sort_entries(&mut entries);
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
        let parts = crumbs(&dir, std::env::home_dir().as_deref());
        assert_eq!(parts.last().unwrap().1, dir);
    }

    #[test]
    fn remote_paths_list_and_crumb_with_slashes() {
        let (entries, truncated) = remote_listing(
            "/home/kit",
            vec![
                RemoteEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    size: 5,
                    mode: Some(0o644),
                },
                RemoteEntry {
                    name: ".config".into(),
                    is_dir: true,
                    size: 0,
                    mode: Some(0o700),
                },
            ],
        );
        assert!(!truncated);
        assert_eq!(entries[0].name.as_ref(), ".config");
        assert!(entries[0].hidden);
        assert_eq!(remote_path(&entries[1].path), "/home/kit/notes.txt");
        let parts = crumbs(Path::new("/home/kit/src"), Some(Path::new("/home/kit")));
        let labels: Vec<_> = parts.iter().map(|(l, _)| l.to_string()).collect();
        assert_eq!(labels, ["~", "src"]);
        assert_eq!(remote_path(&parts[0].1), "/home/kit");
    }

    #[test]
    fn modes_read_like_ls() {
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o640), "rw-r-----");
    }
}
