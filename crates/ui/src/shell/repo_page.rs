//! The main Repo tab, shown in the centre like a plan's board: refs on the left, the full lane
//! graph in the middle (or a file's diff in its place), and the selected commit's details or
//! the working copy on the right. Opened from the Repo dock; clicking a terminal tab leaves it.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use bstr::{BString, ByteSlice as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, ElementId, Entity, EventEmitter,
    Hsla, InteractiveElement as _, IntoElement, ListHorizontalSizingBehavior, ParentElement as _,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};
use maomi_git::graph::RowKind;
use maomi_git::parse::diff::{FileChange, FileDiff};
use maomi_git::parse::log::{Commit, RefLabel};
use maomi_git::parse::refs::RefKind;
use maomi_git::parse::status::StatusEntry;
use maomi_git::{ChangedFile, DIFF_CAP, DiffTarget, GitRepo as _, LogQuery};

use super::repo_dock::{
    Group, Pick, RepoDock, ago, chip, count_badge, discard_for, file_letter, group_entries,
    now_secs, on, plural, ref_pill, segmented, small_icon,
};
use crate::repo::diff::{self, DiffModel, Layout, sanitize_line};
use crate::repo::lanes::{self, LaneColors};
use crate::repo::{LogScope, RepoModel};
use crate::{Palette, Theme};

const ROW_H: f32 = 28.;
const REFS_W: f32 = 210.;
const DETAILS_W: f32 = 340.;
const AUTHOR_W: f32 = 150.;
const SHA_W: f32 = 70.;
const DATE_W: f32 = 50.;
/// Most lanes the graph column draws before the rest collapse into a gutter.
const MAX_LANES: u32 = 10;
/// Commits kept loaded for going back to them.
const CACHE: usize = 32;
/// Opened file diffs kept (per commit, path and theme mode).
const DIFF_CACHE: usize = 64;
/// Files listed before "Show all" (huge commits stay quick to render).
const FILE_LIMIT: usize = 300;
/// Ask for the next log page when the visible rows come this close to the end.
const PREFETCH: usize = 60;

pub enum RepoPageEvent {
    /// Back to the terminals.
    Close,
}

/// What the details panel shows.
#[derive(Clone, PartialEq, Eq)]
enum Selection {
    Wip,
    Commit(String),
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(SharedString),
}

#[derive(Clone)]
enum DiffSource {
    Commit { id: String, file: ChangedFile },
    Wip { path: BString, staged: bool },
}

struct OpenDiff {
    path: SharedString,
    source: DiffSource,
    state: Load<Option<Arc<DiffModel>>>,
    /// The diff was longer than [`DIFF_CAP`] and is cut.
    truncated: bool,
}

pub struct RepoPage {
    model: Entity<RepoModel>,
    /// Lends its commit box (one message for both views) and discard prompt.
    dock: WeakEntity<RepoDock>,
    selection: Selection,
    /// The selected commit with its full message and parents.
    meta: Load<Rc<Commit>>,
    /// The selected commit's files.
    files: Load<Rc<Vec<ChangedFile>>>,
    diff: Option<OpenDiff>,
    layout: Layout,
    filter: Entity<InputState>,
    graph_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    /// The status generation the open working-copy diff was read at.
    diff_generation: u64,
    /// Scroll the graph to the selection once its row is loaded.
    reveal: bool,
    meta_task: Option<Task<()>>,
    files_task: Option<Task<()>>,
    meta_cache: Vec<(String, Rc<Commit>)>,
    files_cache: Vec<(String, Rc<Vec<ChangedFile>>)>,
    diff_cache: Vec<(DiffKey, Arc<DiffModel>)>,
    /// List every file of a huge commit, not just the first [`FILE_LIMIT`].
    show_all: bool,
    diff_task: Option<Task<()>>,
    /// Keeps Enter working in an open toolbar prompt (new branch, stash).
    pub(super) dialog_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<RepoPageEvent> for RepoPage {}

impl RepoPage {
    pub fn new(
        model: Entity<RepoModel>,
        dock: &Entity<RepoDock>,
        commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search message, author or sha…"));
        let subs = vec![
            cx.observe(&model, |page, _, cx| page.on_model_changed(cx)),
            // The commit box's counter and Clear link live in the dock's state.
            cx.observe(dock, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        let mut page = Self {
            model,
            dock: dock.downgrade(),
            selection: Selection::Wip,
            meta: Load::Loading,
            files: Load::Loading,
            diff: None,
            layout: Layout::default(),
            filter,
            graph_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            diff_generation: 0,
            reveal: false,
            meta_task: None,
            files_task: None,
            meta_cache: Vec::new(),
            files_cache: Vec::new(),
            diff_cache: Vec::new(),
            show_all: false,
            diff_task: None,
            dialog_sub: None,
            _subs: subs,
        };
        page.select(commit.map_or(Selection::Wip, Selection::Commit), true, cx);
        page
    }

    pub fn model(&self) -> &Entity<RepoModel> {
        &self.model
    }

    /// Shows `commit` (or the working copy) and scrolls the graph to it.
    pub fn show(&mut self, commit: Option<String>, cx: &mut Context<Self>) {
        self.diff = None;
        self.select(commit.map_or(Selection::Wip, Selection::Commit), true, cx);
    }

    fn on_model_changed(&mut self, cx: &mut Context<Self>) {
        let generation = self.model.read(cx).generation;
        if let Some(diff) = &self.diff
            && matches!(diff.source, DiffSource::Wip { .. })
            && generation != self.diff_generation
        {
            let source = diff.source.clone();
            let path = diff.path.clone();
            self.open_diff(path, source, cx);
        }
        if self.reveal {
            self.reveal_selection(cx);
        }
        cx.notify();
    }

    fn select(&mut self, selection: Selection, reveal: bool, cx: &mut Context<Self>) {
        if self.selection != selection {
            self.diff = None;
        }
        self.selection = selection.clone();
        self.reveal = reveal;
        self.meta = Load::Loading;
        self.files = Load::Loading;
        self.meta_task = None;
        self.files_task = None;
        self.show_all = false;
        if let Selection::Commit(id) = selection {
            let repo = self.model.read(cx).repo().clone();
            if let Some(commit) = recall(&mut self.meta_cache, &id) {
                self.meta = Load::Ready(commit);
            }
            if let Some(files) = recall(&mut self.files_cache, &id) {
                self.files = Load::Ready(files);
            }
            // The graph row paints the header at once; the message and the file list load
            // side by side and each fills in when it lands.
            let meta_job = matches!(self.meta, Load::Loading).then(|| {
                cx.background_executor().spawn({
                    let (repo, id) = (repo.clone(), id.clone());
                    async move {
                        let query = LogQuery {
                            revs: vec![id],
                            limit: Some(1),
                            bodies: true,
                            ..LogQuery::default()
                        };
                        repo.log(query)
                            .await
                            .map_err(|e| e.to_string())?
                            .into_iter()
                            .next()
                            .ok_or_else(|| "Commit not found.".to_string())
                    }
                })
            });
            let files_job = matches!(self.files, Load::Loading).then(|| {
                let id = id.clone();
                cx.background_executor().spawn(async move {
                    repo.commit_files(&id, true)
                        .await
                        .map_err(|e| e.to_string())
                })
            });
            self.meta_task = meta_job.map(|job| {
                let id = id.clone();
                cx.spawn(async move |this, cx| {
                    let result = job.await;
                    _ = this.update(cx, |page, cx| {
                        page.meta = match result {
                            Ok(commit) => {
                                let commit = Rc::new(commit);
                                remember(&mut page.meta_cache, id, commit.clone(), CACHE);
                                Load::Ready(commit)
                            }
                            Err(err) => Load::Failed(err.into()),
                        };
                        cx.notify();
                    });
                })
            });
            self.files_task = files_job.map(|job| {
                cx.spawn(async move |this, cx| {
                    let result = job.await;
                    _ = this.update(cx, |page, cx| {
                        page.files = match result {
                            Ok(files) => {
                                let files = Rc::new(files);
                                remember(&mut page.files_cache, id, files.clone(), CACHE);
                                Load::Ready(files)
                            }
                            Err(err) => Load::Failed(err.into()),
                        };
                        cx.notify();
                    });
                })
            });
        }
        if reveal {
            self.reveal_selection(cx);
        }
        cx.notify();
    }

    /// Scrolls the graph to the selected row, once it is loaded.
    fn reveal_selection(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let history = &model.log.history;
        let row = match &self.selection {
            Selection::Wip => history.wip_id().and(Some(0)),
            Selection::Commit(id) => history.find(id),
        };
        match row {
            Some(row) => {
                self.graph_scroll
                    .scroll_to_item(row, gpui_kit::ScrollStrategy::Center);
                self.reveal = false;
            }
            // Not loaded yet: page in more until it is (or the log ends).
            None if matches!(self.selection, Selection::Commit(_)) && !model.log.exhausted => {
                self.model.update(cx, |m, cx| m.load_more(cx));
            }
            None => self.reveal = false,
        }
    }

    fn open_diff(&mut self, path: SharedString, source: DiffSource, cx: &mut Context<Self>) {
        let repo = self.model.read(cx).repo().clone();
        self.diff_generation = self.model.read(cx).generation;
        let dark = Theme::current(cx).effective_mode() == crate::Mode::Dark;
        let theme = if dark {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        };
        // A commit's file never changes: an opened one comes back from the cache.
        let key = match &source {
            DiffSource::Commit { id, file } => Some((id.clone(), file.path().to_vec(), dark)),
            DiffSource::Wip { .. } => None,
        };
        if let Some(model) = key.as_ref().and_then(|k| recall(&mut self.diff_cache, k)) {
            self.diff_task = None;
            self.diff_scroll = UniformListScrollHandle::new();
            self.diff = Some(OpenDiff {
                path,
                source,
                state: Load::Ready(Some(model)),
                truncated: false,
            });
            cx.notify();
            return;
        }
        let job_source = source.clone();
        let job = cx.background_executor().spawn(async move {
            let capped = match job_source {
                DiffSource::Commit { id, file } => {
                    repo.commit_file_diff_capped(&id, &file, Some(DIFF_CAP))
                        .await?
                }
                DiffSource::Wip { path, staged } => {
                    let paths = vec![path];
                    let target = if staged {
                        DiffTarget::Staged { paths }
                    } else {
                        DiffTarget::Unstaged { paths }
                    };
                    let capped = repo.diff_capped(target, Some(DIFF_CAP)).await?;
                    maomi_git::Capped {
                        value: capped.value.into_iter().next(),
                        truncated: capped.truncated,
                    }
                }
            };
            let truncated = capped.truncated;
            let model = capped.value.map(|file: FileDiff| {
                let path = file
                    .new_path
                    .clone()
                    .or_else(|| file.old_path.clone())
                    .unwrap_or_default();
                let mut model = DiffModel::new(file);
                diff::syntax::highlight(&mut model, &path, &theme);
                Arc::new(model)
            });
            Ok::<_, maomi_git::GitError>((model, truncated))
        });
        let keep_scroll = self.diff.as_ref().is_some_and(|d| d.path == path);
        if !keep_scroll {
            self.diff_scroll = UniformListScrollHandle::new();
        }
        let state = match self.diff.take() {
            // A reload keeps the old rows until the new ones arrive.
            Some(old) if keep_scroll => old.state,
            _ => Load::Loading,
        };
        self.diff = Some(OpenDiff {
            path,
            source,
            state,
            truncated: false,
        });
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |page, cx| {
                if let Some(diff) = &mut page.diff {
                    match result {
                        Ok((model, truncated)) => {
                            if let (Some(key), Some(model), false) = (key, &model, truncated) {
                                remember(&mut page.diff_cache, key, model.clone(), DIFF_CACHE);
                            }
                            diff.state = Load::Ready(model);
                            diff.truncated = truncated;
                        }
                        Err(err) => diff.state = Load::Failed(err.to_string().into()),
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn checkout(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Check out {}?", short(&id)),
            Some("HEAD is detached at this commit. Create a branch to keep work made there."),
            &["Check out", "Cancel"],
            cx,
        );
        let model = self.model.clone();
        cx.spawn(async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            _ = model.update(cx, |m, cx| {
                let label = format!("Checking out {}", short(&id));
                m.run_op(label, cx, move |repo| async move {
                    repo.checkout_detached(&id).await
                })
            });
        })
        .detach();
    }

    // ---- rendering --------------------------------------------------------------------------

    fn render_toolbar(&self, m: &RepoModel, p: &Palette, page: &WeakEntity<Self>) -> AnyElement {
        // Rows minus the WIP and stash rows.
        let history = &m.log.history;
        let loaded = (0..history.len())
            .filter(|&ix| history.kind(ix) == RowKind::Commit)
            .count();
        let count = if m.log.exhausted {
            format!("{loaded} commits")
        } else {
            format!("{loaded} loaded")
        };
        let branch = m
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .unwrap_or_else(|| "detached HEAD".into());
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(Icon::new(gpui_kit::assets::IconName::GitBranch).size(px(15.)))
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(p.fg_emph.to_hsla())
                    .child(m.name.clone()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child(format!("on {branch}")),
            )
            .child(
                div()
                    .w(px(260.))
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(scope_switch(m.log_scope, p, page))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child(count),
            )
            .into_any_element()
    }

    fn render_refs(&self, m: &RepoModel, p: &Palette, page: &WeakEntity<Self>) -> AnyElement {
        let mut items: Vec<AnyElement> = Vec::new();
        let hover = p.surface_2.to_hsla();
        let selected_id = match &self.selection {
            Selection::Commit(id) => Some(id.as_str()),
            Selection::Wip => None,
        };
        let title = |text: &'static str| {
            div()
                .px(px(6.))
                .pt(px(10.))
                .pb(px(4.))
                .text_size(px(11.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(p.muted.to_hsla())
                .child(text)
                .into_any_element()
        };
        let row = |key: String, name: String, aside: Option<String>, oid: String, indent: bool| {
            let active = selected_id == Some(oid.as_str());
            div()
                .id(ElementId::from(SharedString::from(key)))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .pl(px(if indent { 22. } else { 6. }))
                .pr(px(6.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.5))
                .when(active, |d| d.bg(p.accent_soft.to_hsla()))
                .hover(move |s| s.bg(hover))
                .on_click(on(page, move |page, _, _, cx| {
                    page.select(Selection::Commit(oid.clone()), true, cx)
                }))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .when_some(aside, |d, aside| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(p.muted.to_hsla())
                            .child(aside),
                    )
                })
                .into_any_element()
        };

        let locals: Vec<_> = m
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::LocalBranch)
            .collect();
        let remotes: Vec<_> = m
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::RemoteBranch)
            .collect();
        let tags: Vec<_> = m.refs.iter().filter(|r| r.kind == RefKind::Tag).collect();
        let mut sections: Vec<(&'static str, Vec<AnyElement>)> = Vec::new();

        for r in &locals {
            let aside = match &r.upstream {
                Some(u) if u.gone => Some("gone".to_string()),
                Some(u) if u.ahead > 0 || u.behind > 0 => {
                    Some(format!("↑{} ↓{}", u.ahead, u.behind))
                }
                Some(_) => None,
                None => Some("no upstream".to_string()),
            };
            items.push(row(
                format!("ref-{}", r.full_name),
                r.short_name.clone(),
                aside,
                r.oid.clone(),
                false,
            ));
        }
        sections.push(("LOCAL", std::mem::take(&mut items)));
        for remote in &m.remotes {
            let prefix = format!("{}/", remote.name);
            let branches: Vec<_> = remotes
                .iter()
                .filter(|r| r.short_name.starts_with(&prefix) && !r.short_name.ends_with("/HEAD"))
                .collect();
            if branches.is_empty() {
                continue;
            }
            items.push(
                div()
                    .px(px(6.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .text_size(px(12.5))
                    .text_color(p.fg.to_hsla())
                    .child(remote.name.clone())
                    .into_any_element(),
            );
            for r in branches {
                let name = r.short_name[prefix.len()..].to_string();
                items.push(row(
                    format!("ref-{}", r.full_name),
                    name,
                    None,
                    r.oid.clone(),
                    true,
                ));
            }
        }
        sections.push(("REMOTES", std::mem::take(&mut items)));
        for r in tags.iter().rev().take(30) {
            let oid = r.peeled_oid.clone().unwrap_or_else(|| r.oid.clone());
            items.push(row(
                format!("ref-{}", r.full_name),
                r.short_name.clone(),
                None,
                oid,
                false,
            ));
        }
        sections.push(("TAGS", std::mem::take(&mut items)));
        for s in &m.stashes {
            let message = sanitize_line(&s.message.to_str_lossy()).into_owned();
            items.push(row(
                format!("stash-{}", s.index),
                message,
                Some(format!("@{{{}}}", s.index)),
                s.oid.clone(),
                false,
            ));
        }
        sections.push(("STASHES", items));

        div()
            .id("repo-refs")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(REFS_W))
            .h_full()
            .px(px(6.))
            .pb(px(8.))
            .overflow_y_scroll()
            .border_r_1()
            .border_color(p.line.to_hsla())
            .children(
                sections
                    .into_iter()
                    .filter(|(_, rows)| !rows.is_empty())
                    .flat_map(|(name, rows)| std::iter::once(title(name)).chain(rows)),
            )
            .into_any_element()
    }

    /// Rows matching the search box, or `None` without a query.
    fn matches(&self, m: &RepoModel, cx: &App) -> Option<Vec<usize>> {
        let query = self.filter.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            return None;
        }
        let history = &m.log.history;
        Some(
            (0..history.len())
                .filter(|&ix| {
                    history.row(ix).is_some_and(|row| {
                        row.id.starts_with(&query)
                            || row.subject.to_lowercase().contains(&query)
                            || row.author.name.to_lowercase().contains(&query)
                    })
                })
                .collect(),
        )
    }

    fn render_graph(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let matches = self.matches(m, cx).map(Rc::new);
        let count = matches.as_ref().map_or(m.log.history.len(), |v| v.len());
        let filtering = matches.is_some();
        let width = m.log.history.layout().width();
        let graph_w = if filtering {
            px(0.)
        } else {
            lanes::column_width(width, MAX_LANES, lanes::FULL)
        };
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .px(px(10.))
            .text_size(px(10.5))
            .text_color(p.muted.to_hsla())
            .border_b_1()
            .border_color(p.line.to_hsla())
            .when(!filtering, |d| {
                d.child(div().flex_none().w(graph_w).child("GRAPH"))
            })
            .child(div().flex_1().child("MESSAGE"))
            .child(div().flex_none().w(px(AUTHOR_W)).child("AUTHOR"))
            .child(div().flex_none().w(px(SHA_W)).child("SHA"))
            .child(div().flex_none().w(px(DATE_W)).child("DATE"));

        let note: Option<AnyElement> = if let Some(err) = &m.log.error {
            Some(muted_note(err.clone(), p.err.to_hsla()))
        } else if m.log.loading && count == 0 {
            Some(muted_note("Loading commits…".into(), p.muted.to_hsla()))
        } else if filtering && count == 0 {
            Some(muted_note(
                if m.log.exhausted {
                    "No loaded commit matches.".into()
                } else {
                    "No loaded commit matches. Scroll the graph to load older ones.".into()
                },
                p.muted.to_hsla(),
            ))
        } else {
            None
        };

        // The model read ends here; the list's row builder reads it again.
        let list = uniform_list(
            "repo-graph",
            count,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let page = cx.entity().downgrade();
                let model = this.model.clone();
                let p = Theme::current(cx).palette();
                let m = model.read(cx);
                let rows: Vec<usize> = match &matches {
                    Some(v) => v[range.clone()].to_vec(),
                    None => range.clone().collect(),
                };
                // Near the end of what is loaded: ask for the next page.
                let near_end = !filtering && range.end + PREFETCH >= m.log.history.len();
                let rendered = this.graph_rows(m, &rows, filtering, graph_w, p, &page);
                if near_end && !m.log.exhausted && !m.log.loading {
                    model.update(cx, |m, cx| m.load_more(cx));
                }
                rendered
            }),
        )
        .track_scroll(&self.graph_scroll)
        .flex_1()
        .min_h_0();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .children(note)
            .child(list)
            .into_any_element()
    }

    fn graph_rows(
        &self,
        m: &RepoModel,
        rows: &[usize],
        filtering: bool,
        graph_w: gpui_kit::Pixels,
        p: &Palette,
        page: &WeakEntity<Self>,
    ) -> Vec<AnyElement> {
        let history = &m.log.history;
        let width = history.layout().width();
        let gutter = lanes::gutter(width, MAX_LANES);
        let colors = LaneColors::new(p, p.bg.to_hsla());
        let now = now_secs();
        let hover = p.surface_2.to_hsla();
        let selected = p.accent_soft.to_hsla();
        let changes = m.status.as_ref().map_or(0, |s| s.entries.len());
        // Lanes for the contiguous range (one query), when not filtering.
        let lanes_for = if filtering || rows.is_empty() {
            Vec::new()
        } else {
            lanes::rows_lanes(history, rows[0]..rows[rows.len() - 1] + 1)
        };
        rows.iter()
            .enumerate()
            .filter_map(|(i, &ix)| {
                let row = history.row(ix)?;
                let kind = history.kind(ix);
                let selection = match kind {
                    RowKind::Wip => Selection::Wip,
                    _ => Selection::Commit(row.id.to_string()),
                };
                let active = self.selection == selection;
                let (text, italic): (SharedString, bool) = match kind {
                    RowKind::Wip => (
                        format!("// WIP · {changes} change{}", plural(changes)).into(),
                        true,
                    ),
                    RowKind::Stash(n) => (
                        format!("stash@{{{n}}}: {}", sanitize_line(row.subject)).into(),
                        true,
                    ),
                    RowKind::Commit => (sanitize_line(row.subject).into_owned().into(), false),
                };
                let pills: Vec<AnyElement> = row
                    .refs
                    .iter()
                    .filter_map(|r| match r {
                        RefLabel::LocalBranch(n) => Some(ref_pill(n, false, p)),
                        RefLabel::RemoteBranch(n) => Some(ref_pill(n, false, p)),
                        RefLabel::Tag(n) => Some(ref_pill(n, true, p)),
                        _ => None,
                    })
                    .take(3)
                    .collect();
                let is_commit = kind == RowKind::Commit;
                let author = row.author.name.clone();
                let lanes = lanes_for.get(i).cloned();
                Some(
                    div()
                        .id(("repo-graph-row", ix))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .h(px(ROW_H))
                        .px(px(10.))
                        .cursor_pointer()
                        .when(active, |d| d.bg(selected))
                        .when(!active, |d| d.hover(move |s| s.bg(hover)))
                        .on_click(on(page, move |page, _, _, cx| {
                            page.select(selection.clone(), false, cx)
                        }))
                        .when(!filtering, |d| {
                            d.child(div().flex_none().w(graph_w).h_full().children(lanes.map(
                                |lanes| {
                                    lanes::lanes_canvas(
                                        lanes,
                                        colors.clone(),
                                        lanes::FULL,
                                        None,
                                        gutter,
                                    )
                                },
                            )))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .when(italic, |d| d.italic().text_color(p.warn.to_hsla()))
                                        .child(text),
                                )
                                .children(pills),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(px(6.))
                                .w(px(AUTHOR_W))
                                .text_size(px(12.))
                                .text_color(p.muted.to_hsla())
                                .when(is_commit, |d| {
                                    d.child(avatar(&author, p))
                                        .child(div().min_w_0().truncate().child(author.clone()))
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(SHA_W))
                                .font_family(crate::TERMINAL_FAMILY)
                                .text_size(px(11.5))
                                .text_color(p.muted.to_hsla())
                                .when(is_commit, |d| d.child(short(row.id).to_string())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(DATE_W))
                                .text_size(px(11.5))
                                .text_color(p.muted.to_hsla())
                                .when(is_commit, |d| d.child(ago(now, row.time))),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_diff(&self, diff: &OpenDiff, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let page = cx.entity().downgrade();
        let ready = match &diff.state {
            Load::Ready(Some(model)) => Some(model.clone()),
            _ => None,
        };
        let (added, removed) = ready.as_ref().map_or((0, 0), |m| (m.added, m.removed));
        let layout = self.layout;
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                Button::new("repo-diff-back")
                    .small()
                    .outline()
                    .icon(IconName::ArrowLeft)
                    .label("Graph")
                    .on_click(on(&page, |page, _, _, cx| {
                        page.diff = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(crate::TERMINAL_FAMILY)
                    .text_size(px(12.))
                    .child(diff.path.clone()),
            )
            .when(ready.is_some(), |d| {
                d.child(chip(format!("+{added}"), p.ok.to_hsla(), p))
                    .child(chip(format!("−{removed}"), p.err.to_hsla(), p))
            })
            .child(layout_switch(layout, p, &page));

        let note = |text: &str, color: Hsla| muted_note(text.to_string().into(), color);
        let body: AnyElement = match &diff.state {
            Load::Loading => note("Loading the diff…", p.muted.to_hsla()),
            Load::Failed(err) => muted_note(err.clone(), p.err.to_hsla()),
            Load::Ready(None) => note("No changes to show.", p.muted.to_hsla()),
            Load::Ready(Some(model)) if model.file.binary => {
                note("Binary file: no text diff.", p.muted.to_hsla())
            }
            Load::Ready(Some(model)) if model.file.hunks.is_empty() => {
                note("Only the file mode changed.", p.muted.to_hsla())
            }
            Load::Ready(Some(model)) => {
                let model = model.clone();
                uniform_list(
                    "repo-diff",
                    model.rows(layout).len(),
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        let p = Theme::current(cx).palette();
                        range
                            .filter_map(|ix| diff::row(&model, layout, ix, p))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.diff_scroll)
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
                .flex_1()
                .min_h_0()
                .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .when(diff.truncated, |d| {
                d.child(note(
                    "This diff is too large to show in full; it is cut after the first files.",
                    p.warn.to_hsla(),
                ))
            })
            .child(body)
            .into_any_element()
    }

    fn render_details(
        &self,
        m: &RepoModel,
        p: &Palette,
        page: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let body = match &self.selection {
            Selection::Wip => self.render_wip(m, p, page, cx),
            Selection::Commit(id) => self.render_commit(id, m, p, page),
        };
        div()
            .id("repo-details")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(DETAILS_W))
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(p.line.to_hsla())
            .child(body)
            .into_any_element()
    }

    fn render_wip(
        &self,
        m: &RepoModel,
        p: &Palette,
        page: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let Some(status) = &m.status else {
            return muted_note("Loading…".into(), p.muted.to_hsla());
        };
        let changes = status.entries.len();
        let mut out = div().flex().flex_col().p(px(12.)).gap(px(4.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .pb(px(6.))
                .child(
                    div()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(p.fg_emph.to_hsla())
                        .child("Working copy"),
                )
                .child(div().flex_1())
                .child(if changes == 0 {
                    chip("Clean", p.ok.to_hsla(), p)
                } else {
                    chip(
                        format!("{changes} change{}", plural(changes)),
                        p.warn.to_hsla(),
                        p,
                    )
                }),
        );
        if changes == 0 {
            return out
                .child(muted_note(
                    "Nothing to commit. Working tree is clean.".into(),
                    p.muted.to_hsla(),
                ))
                .into_any_element();
        }
        if let Some(dock) = self.dock.upgrade() {
            out = out.children(
                dock.read(cx)
                    .commit_box_for(&self.model, m, p, &self.dock, cx),
            );
        }
        let busy = m.is_busy();
        for group in [Group::Conflicts, Group::Staged, Group::Changes] {
            let entries = group_entries(status, group);
            if entries.is_empty() {
                continue;
            }
            let title = match group {
                Group::Conflicts => "Conflicts",
                Group::Staged => "Staged",
                Group::Changes => "Changes",
            };
            out = out.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pt(px(8.))
                    .pb(px(2.))
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .child(title)
                    .child(count_badge(entries.len(), p))
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .children((!busy).then(|| group_actions(group, &entries, p, page))),
                    ),
            );
            for (ix, entry) in entries.iter().enumerate() {
                out = out.child(self.wip_file(group, ix, entry, busy, p, page));
            }
        }
        out.into_any_element()
    }

    fn wip_file(
        &self,
        group: Group,
        ix: usize,
        entry: &StatusEntry,
        busy: bool,
        p: &Palette,
        page: &WeakEntity<Self>,
    ) -> AnyElement {
        let (letter, color) = file_letter(entry, group, p);
        let path = sanitize_line(&entry.path.to_str_lossy()).into_owned();
        let untracked = letter == "U";
        let staged = group == Group::Staged;
        let open = self.diff.as_ref().is_some_and(|d| {
            matches!(&d.source, DiffSource::Wip { path, staged: s } if *path == entry.path && *s == staged)
        });
        let raw = entry.path.clone();
        let key = format!("repo-wip-{}-{ix}", group_key(group));
        let hover_group: SharedString = key.clone().into();
        let actions = (!busy).then(|| file_actions(group, ix, entry, p, page));
        file_row(key, letter, color, &path, None, open, p)
            .group(hover_group.clone())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(2.))
                    .invisible()
                    .group_hover(hover_group, |s| s.visible())
                    .children(actions),
            )
            .when(group != Group::Conflicts && !untracked, |d| {
                let shown: SharedString = path.clone().into();
                d.on_click(on(page, move |page, _, _, cx| {
                    page.open_diff(
                        shown.clone(),
                        DiffSource::Wip {
                            path: raw.clone(),
                            staged,
                        },
                        cx,
                    )
                }))
            })
            .when(untracked, |d| {
                d.tooltip(|window, cx| {
                    Tooltip::new("Untracked: stage it to see a diff.").build(window, cx)
                })
            })
            .into_any_element()
    }

    /// Asks, then discards `entries` in this page's own repo, whatever the dock follows now.
    fn discard(&mut self, entries: Vec<StatusEntry>, window: &mut Window, cx: &mut Context<Self>) {
        discard_for(self.model.downgrade(), entries, window, cx);
    }

    fn render_commit(
        &self,
        id: &str,
        m: &RepoModel,
        p: &Palette,
        page: &WeakEntity<Self>,
    ) -> AnyElement {
        let meta = match &self.meta {
            Load::Ready(c) => Some(c.clone()),
            _ => None,
        };
        // What the header needs, from the graph row (instant) or the loaded commit.
        let history = &m.log.history;
        let row = history.find(id).and_then(|ix| history.row(ix));
        let head = match (row, &meta) {
            (Some(row), _) => Some((
                row.author.name.clone(),
                row.time,
                row.refs.to_vec(),
                row.subject.to_string(),
            )),
            (None, Some(c)) => Some((
                c.author.name.to_str_lossy().into_owned(),
                c.author.time,
                c.refs.clone(),
                c.subject.to_str_lossy().into_owned(),
            )),
            (None, None) => None,
        };
        let Some((author, time, refs, subject)) = head else {
            return match &self.meta {
                Load::Failed(err) => muted_note(err.clone(), p.err.to_hsla()),
                _ => muted_note("Loading…".into(), p.muted.to_hsla()),
            };
        };
        let author = sanitize_line(&author).into_owned();
        let now = now_secs();
        let kv = |key: &'static str, value: AnyElement| {
            div()
                .flex()
                .gap(px(8.))
                .text_size(px(12.))
                .child(
                    div()
                        .flex_none()
                        .w(px(70.))
                        .text_color(p.muted.to_hsla())
                        .child(key),
                )
                .child(div().flex_1().min_w_0().child(value))
        };
        let pills: Vec<AnyElement> = refs
            .iter()
            .filter_map(|r| match r {
                RefLabel::LocalBranch(n) | RefLabel::RemoteBranch(n) => Some(ref_pill(n, false, p)),
                RefLabel::Tag(n) => Some(ref_pill(n, true, p)),
                _ => None,
            })
            .collect();
        let committer = meta
            .as_ref()
            .map(|c| sanitize_line(&c.committer.name.to_str_lossy()).into_owned())
            .filter(|c| *c != author);
        let parents = meta.as_ref().filter(|c| !c.parents.is_empty()).map(|c| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(c.parents.iter().map(|parent| {
                    let target = parent.clone();
                    div()
                        .id(ElementId::from(SharedString::from(format!(
                            "repo-parent-{parent}"
                        ))))
                        .font_family(crate::TERMINAL_FAMILY)
                        .text_color(p.accent.to_hsla())
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(short(parent).to_string())
                        .on_click(on(page, move |page, _, _, cx| {
                            page.select(Selection::Commit(target.clone()), true, cx)
                        }))
                }))
                .into_any_element()
        });
        let body: Option<AnyElement> = match &self.meta {
            Load::Loading => Some(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child("Loading the message…")
                    .into_any_element(),
            ),
            Load::Failed(err) => Some(
                div()
                    .text_size(px(12.))
                    .text_color(p.err.to_hsla())
                    .child(err.clone())
                    .into_any_element(),
            ),
            Load::Ready(c) => {
                let body = c.body.to_str_lossy().trim().to_string();
                (!body.is_empty()).then(|| {
                    div()
                        .text_color(p.fg.to_hsla())
                        .children(
                            body.lines()
                                .map(|l| div().min_h(px(16.)).child(sanitize_line(l).into_owned())),
                        )
                        .into_any_element()
                })
            }
        };
        let message = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .pt(px(8.))
            .text_size(px(12.5))
            .text_color(p.fg_emph.to_hsla())
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(sanitize_line(&subject).into_owned()),
            )
            .children(body);
        let id_owned = id.to_string();
        let copy_id = id.to_string();
        let busy = m.is_busy();
        let actions = div()
            .flex()
            .gap(px(6.))
            .pt(px(10.))
            .child(
                Button::new("repo-checkout")
                    .small()
                    .outline()
                    .label("Check out")
                    .when(busy, |b| b.loading(true))
                    .on_click(on(page, move |page, _, window, cx| {
                        page.checkout(id_owned.clone(), window, cx)
                    })),
            )
            .child(
                Button::new("repo-copy-sha")
                    .small()
                    .ghost()
                    .label("Copy sha")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()))
                    }),
            );

        let head = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(12.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(2.))
                    .pb(px(6.))
                    .child(
                        div()
                            .font_family(crate::TERMINAL_FAMILY)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child(short(id).to_string()),
                    )
                    .children(pills),
            )
            .child(kv(
                "Author",
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(avatar(&author, p))
                    .child(div().min_w_0().truncate().child(author.clone()))
                    .into_any_element(),
            ))
            .child(kv(
                "Date",
                div()
                    .child(format!("{} ago", ago(now, time)))
                    .into_any_element(),
            ))
            .when_some(committer, |d, committer| {
                d.child(kv("Committer", div().child(committer).into_any_element()))
            })
            .when_some(parents, |d, parents| d.child(kv("Parents", parents)))
            .child(message)
            .child(actions);

        div()
            .flex()
            .flex_col()
            .child(head)
            .child(self.render_files(id, p, page))
            .into_any_element()
    }

    fn render_files(&self, id: &str, p: &Palette, page: &WeakEntity<Self>) -> AnyElement {
        let title = |count: Option<usize>| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pb(px(4.))
                .text_size(px(11.5))
                .text_color(p.muted.to_hsla())
                .child("Files")
                .children(count.map(|n| count_badge(n, p)))
        };
        let list = div().flex().flex_col().p(px(12.)).gap(px(2.));
        let files = match &self.files {
            Load::Loading => {
                return list
                    .child(title(None))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted.to_hsla())
                            .child("Loading files…"),
                    )
                    .into_any_element();
            }
            Load::Failed(err) => {
                return list
                    .child(title(None))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.err.to_hsla())
                            .child(err.clone()),
                    )
                    .into_any_element();
            }
            Load::Ready(files) => files.clone(),
        };
        let open_path = self.diff.as_ref().and_then(|d| match &d.source {
            DiffSource::Commit { id: did, file } if did == id => Some(file.path().to_vec()),
            _ => None,
        });
        let total = files.len();
        let shown_count = if self.show_all {
            total
        } else {
            total.min(FILE_LIMIT)
        };
        list.child(title(Some(total)))
            .children(
                files
                    .iter()
                    .take(shown_count)
                    .enumerate()
                    .map(|(ix, file)| {
                        let (letter, color) = change_letter(file.change, p);
                        let path = sanitize_line(&file.path().to_str_lossy()).into_owned();
                        let counts = match (file.added, file.removed) {
                            (Some(a), Some(r)) => Some(format!("+{a} −{r}")),
                            _ => Some("binary".to_string()),
                        };
                        let open = open_path.as_deref() == Some(file.path());
                        let file = file.clone();
                        let commit = id.to_string();
                        let shown: SharedString = path.clone().into();
                        file_row(
                            format!("repo-file-{ix}"),
                            letter,
                            color,
                            &path,
                            counts,
                            open,
                            p,
                        )
                        .on_click(on(page, move |page, _, _, cx| {
                            page.open_diff(
                                shown.clone(),
                                DiffSource::Commit {
                                    id: commit.clone(),
                                    file: file.clone(),
                                },
                                cx,
                            )
                        }))
                        .into_any_element()
                    }),
            )
            .when(shown_count < total, |d| {
                d.child(
                    div().pt(px(6.)).child(
                        Button::new("repo-show-all-files")
                            .ghost()
                            .small()
                            .label(format!("Show all {total} files"))
                            .on_click(on(page, |page, _, _, cx| {
                                page.show_all = true;
                                cx.notify();
                            })),
                    ),
                )
            })
            .into_any_element()
    }
}

impl Render for RepoPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let page = cx.entity().downgrade();
        // Lists need `&mut Context` for their row builders: build the centre first.
        let centre = match &self.diff {
            Some(diff) => self.render_diff(diff, p, cx),
            None => self.render_graph(p, cx),
        };
        let cx: &App = cx;
        let m = self.model.read(cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(p.bg.to_hsla())
            .text_color(p.fg.to_hsla())
            .child(super::repo_toolbar::render(
                m,
                p,
                &page,
                small_icon(
                    "repo-page-close".into(),
                    IconName::Close,
                    "Back to the terminals",
                    p,
                )
                .on_click(on(&page, |_, _, _, cx| cx.emit(RepoPageEvent::Close)))
                .into_any_element(),
                cx,
            ))
            .child(self.render_toolbar(m, p, &page))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_refs(m, p, &page))
                    .child(centre)
                    .child(self.render_details(m, p, &page, cx)),
            )
    }
}

/// Stage / unstage / discard for a whole working-copy group.
fn group_actions(
    group: Group,
    entries: &[StatusEntry],
    p: &Palette,
    page: &WeakEntity<RepoPage>,
) -> AnyElement {
    let paths: Vec<BString> = entries.iter().map(|e| e.path.clone()).collect();
    let key = group_key(group);
    let id = |what: &str| ElementId::from(SharedString::from(format!("repo-page-{what}-{key}")));
    let mut row = div().flex().gap(px(2.));
    match group {
        Group::Conflicts => {}
        Group::Staged => {
            row = row.child(
                small_icon(id("unstage-all"), IconName::Minus, "Unstage all", p)
                    .on_click(on(page, |page, _, _, cx| {
                        page.model.update(cx, |m, cx| _ = m.unstage_all(cx))
                    })),
            );
        }
        Group::Changes => {
            let all = entries.to_vec();
            row = row
                .child(
                    small_icon(id("discard-all"), IconName::Undo2, "Discard all…", p)
                        .on_click(on(page, move |page, _, window, cx| {
                            page.discard(all.clone(), window, cx)
                        })),
                )
                .child(
                    small_icon(id("stage-all"), IconName::Plus, "Stage all", p).on_click(on(
                        page,
                        move |page, _, _, cx| {
                            let paths = paths.clone();
                            page.model.update(cx, |m, cx| _ = m.stage(paths, cx));
                        },
                    )),
                );
        }
    }
    row.into_any_element()
}

/// Stage / unstage / discard for one working-copy file. Clicks stop here so the row does not
/// also open the diff.
fn file_actions(
    group: Group,
    ix: usize,
    entry: &StatusEntry,
    p: &Palette,
    page: &WeakEntity<RepoPage>,
) -> AnyElement {
    let key = group_key(group);
    let id =
        |what: &str| ElementId::from((SharedString::from(format!("repo-page-{what}-{key}")), ix));
    let one = vec![entry.path.clone()];
    let stage = |label: &'static str| {
        let one = one.clone();
        small_icon(id("stage"), IconName::Plus, label, p).on_click(on(
            page,
            move |page, _, _, cx| {
                cx.stop_propagation();
                let paths = one.clone();
                page.model.update(cx, |m, cx| _ = m.stage(paths, cx));
            },
        ))
    };
    let mut row = div().flex().gap(px(2.));
    match group {
        Group::Conflicts => row = row.child(stage("Mark resolved (stage)")),
        Group::Staged => {
            let one = one.clone();
            row = row.child(
                small_icon(id("unstage"), IconName::Minus, "Unstage", p).on_click(on(
                    page,
                    move |page, _, _, cx| {
                        cx.stop_propagation();
                        let paths = one.clone();
                        page.model.update(cx, |m, cx| _ = m.unstage(paths, cx));
                    },
                )),
            );
        }
        Group::Changes => {
            let entry = entry.clone();
            row = row
                .child(
                    small_icon(id("discard"), IconName::Undo2, "Discard…", p).on_click(on(
                        page,
                        move |page, _, window, cx| {
                            cx.stop_propagation();
                            page.discard(vec![entry.clone()], window, cx)
                        },
                    )),
                )
                .child(stage("Stage"));
        }
    }
    row.into_any_element()
}

/// A cached commit file diff: commit, path, dark theme (colours differ per mode).
type DiffKey = (String, Vec<u8>, bool);

/// `key`'s cached value, moved to the most-recent end.
fn recall<K: PartialEq + ?Sized, Q, V: Clone>(cache: &mut Vec<(Q, V)>, key: &K) -> Option<V>
where
    Q: std::borrow::Borrow<K>,
{
    let ix = cache.iter().position(|(k, _)| k.borrow() == key)?;
    let entry = cache.remove(ix);
    let value = entry.1.clone();
    cache.push(entry);
    Some(value)
}

/// Adds `value`, dropping the oldest entry past `cap`.
fn remember<K, V>(cache: &mut Vec<(K, V)>, key: K, value: V, cap: usize) {
    cache.push((key, value));
    if cache.len() > cap {
        cache.remove(0);
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(7)]
}

fn group_key(group: Group) -> &'static str {
    match group {
        Group::Conflicts => "conflicts",
        Group::Staged => "staged",
        Group::Changes => "changes",
    }
}

fn change_letter(change: FileChange, p: &Palette) -> (&'static str, Hsla) {
    match change {
        FileChange::Added => ("A", p.ok.to_hsla()),
        FileChange::Deleted => ("D", p.err.to_hsla()),
        FileChange::Modified => ("M", p.warn.to_hsla()),
        FileChange::Renamed { .. } => ("R", p.c[1].to_hsla()),
        FileChange::Copied { .. } => ("C", p.c[1].to_hsla()),
        FileChange::ModeChanged => ("T", p.muted.to_hsla()),
    }
}

fn muted_note(text: SharedString, color: Hsla) -> AnyElement {
    div()
        .px(px(12.))
        .py(px(10.))
        .text_size(px(12.))
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// A round initial in a colour picked from the name, standing in for an avatar.
fn avatar(name: &str, p: &Palette) -> AnyElement {
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or('?', |c| c.to_ascii_uppercase());
    let hash = name
        .bytes()
        .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded_full()
        .text_size(px(9.5))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .bg(p.c[hash % p.c.len()].to_hsla())
        .text_color(p.on_accent.to_hsla())
        .child(initial.to_string())
        .into_any_element()
}

/// One file of the details panel: status letter, folder in muted text, name, optional counts.
fn file_row(
    key: String,
    letter: &'static str,
    color: Hsla,
    path: &str,
    counts: Option<String>,
    open: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (dir, name) = match path.rfind('/') {
        Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
        None => (String::new(), path.to_string()),
    };
    let hover = p.surface_2.to_hsla();
    div()
        .id(ElementId::from(SharedString::from(key)))
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(24.))
        .px(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .when(open, |d| d.bg(p.accent_soft.to_hsla()))
        .when(!open, |d| d.hover(move |s| s.bg(hover)))
        .child(
            div()
                .flex_none()
                .w(px(14.))
                .flex()
                .justify_center()
                .text_size(px(11.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .font_family(crate::TERMINAL_FAMILY)
                .text_color(color)
                .child(letter),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(12.5))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(p.muted.to_hsla())
                        .child(dir),
                )
                .child(div().flex_none().child(name)),
        )
        .when_some(counts, |d, counts| {
            d.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child(counts),
            )
        })
}

/// All refs / current branch, for the graph.
fn scope_switch(scope: LogScope, p: &Palette, page: &WeakEntity<RepoPage>) -> AnyElement {
    let pick = |value: LogScope| -> Pick<RepoPage> {
        Box::new(move |page, cx| {
            page.model.update(cx, |m, cx| m.set_log_scope(value, cx));
            page.graph_scroll = UniformListScrollHandle::new();
        })
    };
    segmented(
        page,
        vec![
            (
                "repo-page-scope-all",
                "All refs",
                scope == LogScope::All,
                pick(LogScope::All),
            ),
            (
                "repo-page-scope-branch",
                "Current branch",
                scope == LogScope::Branch,
                pick(LogScope::Branch),
            ),
        ],
        p,
    )
}

/// Unified / Split, for the diff.
fn layout_switch(layout: Layout, p: &Palette, page: &WeakEntity<RepoPage>) -> AnyElement {
    let pick = |value: Layout| -> Pick<RepoPage> { Box::new(move |page, _| page.layout = value) };
    segmented(
        page,
        vec![
            (
                "repo-diff-unified",
                "Unified",
                layout == Layout::Unified,
                pick(Layout::Unified),
            ),
            (
                "repo-diff-split",
                "Split",
                layout == Layout::Split,
                pick(Layout::Split),
            ),
        ],
        p,
    )
}
