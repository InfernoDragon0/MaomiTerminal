//! The Workbench dock: Maomi Workbench plan boards, read-only. A project picker, then each plan as
//! a tree of its columns and cards. Opening a plan shows its board in the centre
//! ([`super::board::BoardPage`]); the workspace does the switching (see [`WorkbenchEvent`]).
//!
//! Boards live in a GPUI global shared by every window. They load on first use, and a background
//! watcher re-reads a plan whenever a Claude session writes it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use maomi_workbench::{
    BoardEvent, BoardWatcher, Column, Plan, PlanStatus, Project, ProjectSettings,
};

use super::snippets::{header, icon_button};
use crate::{Palette, Theme};

/// A plan and the file it was read from (the file path is its identity in the UI).
#[derive(Clone)]
pub(crate) struct PlanEntry {
    pub path: PathBuf,
    pub plan: Arc<Plan>,
}

/// One board folder's plans (newest first) and card types.
#[derive(Default)]
pub(crate) struct Board {
    pub plans: Vec<PlanEntry>,
    pub settings: Arc<ProjectSettings>,
}

#[derive(Default)]
pub(crate) struct WorkbenchStore {
    projects: Vec<Project>,
    /// False until the first project scan finishes.
    scanned: bool,
    /// Loaded boards by folder.
    boards: HashMap<PathBuf, Board>,
    loading: HashSet<PathBuf>,
    started: bool,
}

impl Global for WorkbenchStore {}

impl WorkbenchStore {
    pub fn board(&self, dir: &Path) -> Option<&Board> {
        self.boards.get(dir)
    }

    /// The plan read from `path`, with its board.
    pub fn plan(&self, path: &Path) -> Option<(&Board, &PlanEntry)> {
        let board = self.boards.get(path.parent()?)?;
        let entry = board.plans.iter().find(|e| e.path == path)?;
        Some((board, entry))
    }
}

/// Called once from [`super::init`]; nothing is read until a window shows the Workbench.
pub(crate) fn install(cx: &mut App) {
    cx.set_global(WorkbenchStore::default());
}

/// Finds the projects and starts the watcher, the first time any window asks.
pub(crate) fn start(cx: &mut App) {
    if cx.global::<WorkbenchStore>().started {
        return;
    }
    cx.global_mut::<WorkbenchStore>().started = true;
    let scan = cx
        .background_executor()
        .spawn(async { (maomi_workbench::projects(), BoardWatcher::start()) });
    cx.spawn(async move |cx| {
        // The watcher stops when dropped, so this task (alive for the whole app) keeps it.
        let (projects, watcher) = scan.await;
        cx.update(|cx| {
            let store = cx.global_mut::<WorkbenchStore>();
            store.projects = projects;
            store.scanned = true;
        });
        let events = watcher.events().clone();
        while let Ok(event) = events.recv().await {
            cx.update(|cx| on_board_event(event, cx));
        }
    })
    .detach();
}

/// Reads a board folder in the background unless it is loaded or loading already.
pub(crate) fn load_board(dir: PathBuf, cx: &mut App) {
    let store = cx.global::<WorkbenchStore>();
    if store.boards.contains_key(&dir) || store.loading.contains(&dir) {
        return;
    }
    cx.global_mut::<WorkbenchStore>()
        .loading
        .insert(dir.clone());
    let read = cx.background_executor().spawn({
        let dir = dir.clone();
        async move {
            let plans = maomi_workbench::read_plans(&dir)
                .into_iter()
                .map(|(path, plan)| PlanEntry {
                    path,
                    plan: Arc::new(plan),
                })
                .collect();
            let settings = Arc::new(maomi_workbench::read_settings(&dir));
            Board { plans, settings }
        }
    });
    cx.spawn(async move |cx| {
        let board = read.await;
        cx.update(|cx| {
            let store = cx.global_mut::<WorkbenchStore>();
            store.loading.remove(&dir);
            store.boards.insert(dir, board);
        });
    })
    .detach();
}

fn on_board_event(event: BoardEvent, cx: &mut App) {
    let loaded = |dir: &Path, cx: &App| cx.global::<WorkbenchStore>().boards.contains_key(dir);
    match event {
        BoardEvent::PlanChanged {
            board_dir, path, ..
        } if loaded(&board_dir, cx) => reload_plan(path, cx),
        BoardEvent::PlanRemoved {
            board_dir, path, ..
        } if loaded(&board_dir, cx) => {
            if let Some(board) = cx.global_mut::<WorkbenchStore>().boards.get_mut(&board_dir) {
                board.plans.retain(|e| e.path != path);
            }
        }
        BoardEvent::SettingsChanged { board_dir } if loaded(&board_dir, cx) => {
            let read = cx.background_executor().spawn({
                let dir = board_dir.clone();
                async move { maomi_workbench::read_settings(&dir) }
            });
            cx.spawn(async move |cx| {
                let settings = Arc::new(read.await);
                cx.update(|cx| {
                    let store = cx.global_mut::<WorkbenchStore>();
                    if let Some(board) = store.boards.get_mut(&board_dir) {
                        board.settings = settings;
                    }
                });
            })
            .detach();
        }
        BoardEvent::ProjectsChanged => {
            let scan = cx
                .background_executor()
                .spawn(async { maomi_workbench::projects() });
            cx.spawn(async move |cx| {
                let projects = scan.await;
                cx.update(|cx| cx.global_mut::<WorkbenchStore>().projects = projects);
            })
            .detach();
        }
        // Plans of boards nobody has opened are read when they are; activity is not shown yet.
        _ => {}
    }
}

fn reload_plan(path: PathBuf, cx: &mut App) {
    let read = cx.background_executor().spawn({
        let path = path.clone();
        async move { maomi_workbench::read_plan(&path) }
    });
    cx.spawn(async move |cx| {
        let plan = match read.await {
            Ok(plan) => Arc::new(plan),
            Err(err) => {
                tracing::warn!("cannot reload plan: {err}");
                return;
            }
        };
        cx.update(|cx| {
            let Some(dir) = path.parent() else {
                return;
            };
            let store = cx.global_mut::<WorkbenchStore>();
            let Some(board) = store.boards.get_mut(dir) else {
                return;
            };
            match board.plans.iter_mut().find(|e| e.path == path) {
                Some(entry) => entry.plan = plan,
                None => board.plans.push(PlanEntry {
                    path: path.clone(),
                    plan,
                }),
            }
            board.plans.sort_by(|a, b| {
                b.plan
                    .updated_at
                    .cmp(&a.plan.updated_at)
                    .then_with(|| a.plan.id.cmp(&b.plan.id))
            });
        });
    })
    .detach();
}

/// Cards in a column this build does not know show in Backlog, so none go missing.
pub(super) fn shown_column(column: &Column) -> Column {
    match column {
        Column::Other(_) => Column::Backlog,
        known => known.clone(),
    }
}

pub(super) fn column_label(column: &Column) -> &str {
    match column {
        Column::Backlog => "Backlog",
        Column::InProgress => "In progress",
        Column::Review => "Review",
        Column::Blocked => "Blocked",
        Column::Done => "Done",
        Column::Other(text) => text,
    }
}

/// Column tints use the semantic colours so every skin re-tints them.
pub(super) fn column_color(column: &Column, p: &Palette) -> Hsla {
    match column {
        Column::InProgress => p.now,
        Column::Review => p.warn,
        Column::Blocked => p.err,
        Column::Done => p.ok,
        Column::Backlog | Column::Other(_) => p.muted,
    }
    .to_hsla()
}

/// A card type's palette slot ("c1" to "c5").
pub(super) fn slot_color(slot: &str, p: &Palette) -> Option<Hsla> {
    let index: usize = slot.strip_prefix('c')?.parse().ok()?;
    Some(p.c.get(index.checked_sub(1)?)?.to_hsla())
}

pub(super) fn model_color(model: &str, p: &Palette) -> Hsla {
    match model {
        "opus" => p.agents[0],
        "sonnet" => p.agents[1],
        "haiku" => p.agents[2],
        _ => p.muted,
    }
    .to_hsla()
}

pub(super) fn percent(progress: f64) -> u32 {
    (progress * 100.0).round() as u32
}

/// What the panel asks the workspace to do.
pub enum WorkbenchEvent {
    /// Show this plan's board in the centre.
    OpenBoard(PathBuf),
    OpenCard {
        plan: PathBuf,
        card: String,
    },
    Collapse,
}

type ProjectSelect = SelectState<SearchableVec<SharedString>>;

pub struct WorkbenchPanel {
    project_select: Entity<ProjectSelect>,
    /// Keys of the projects in the select, in its order.
    project_keys: Vec<String>,
    project: Option<String>,
    filter: Entity<InputState>,
    /// Expanded tree rows: a plan's path, or `path|column` for a column section.
    open: HashSet<String>,
    selected_plan: Option<PathBuf>,
    show_archived: bool,
    /// The focused terminal's folder. Its project is shown when it has a board.
    cwd: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<WorkbenchEvent> for WorkbenchPanel {}

impl WorkbenchPanel {
    pub fn new(cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        start(cx);
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter plans and cards"));
        let project_select =
            cx.new(|cx| ProjectSelect::new(SearchableVec::new(Vec::new()), None, window, cx));
        let subscriptions = vec![
            cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &project_select,
                window,
                |panel, select, event: &SelectEvent<_>, _, cx| {
                    if !matches!(event, SelectEvent::Confirm(Some(_))) {
                        return;
                    }
                    let row = select.read(cx).selected_index(cx).map(|ix| ix.row);
                    if let Some(key) = row.and_then(|row| panel.project_keys.get(row)) {
                        panel.project = Some(key.clone());
                        panel.selected_plan = None;
                        panel.load_project(cx);
                        cx.notify();
                    }
                },
            ),
            cx.observe_global_in::<WorkbenchStore>(window, |panel, window, cx| {
                panel.sync_projects(window, cx)
            }),
        ];
        let mut panel = Self {
            project_select,
            project_keys: Vec::new(),
            project: None,
            filter,
            open: HashSet::new(),
            selected_plan: None,
            show_archived: false,
            cwd,
            _subscriptions: subscriptions,
        };
        panel.sync_projects(window, cx);
        panel
    }

    /// The plan "Open board" in the header shows: the selected one, else the newest.
    pub fn selected_plan(&self, cx: &App) -> Option<PathBuf> {
        self.selected_plan.clone().or_else(|| {
            let board = cx.global::<WorkbenchStore>().board(&self.board_dir(cx)?)?;
            board.plans.first().map(|e| e.path.clone())
        })
    }

    /// Follows the focused terminal: shows the project that holds `dir`, if any project does.
    pub fn follow_dir(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.cwd.as_ref() == Some(&dir) {
            return;
        }
        let key = project_containing(&cx.global::<WorkbenchStore>().projects, &dir);
        self.cwd = Some(dir);
        let Some(key) = key.filter(|k| self.project.as_ref() != Some(k)) else {
            return;
        };
        let index = self.project_keys.iter().position(|k| k == &key);
        self.project = Some(key);
        self.selected_plan = None;
        self.project_select.update(cx, |select, cx| {
            select.set_selected_index(index.map(IndexPath::new), window, cx)
        });
        self.load_project(cx);
        cx.notify();
    }

    fn board_dir(&self, cx: &App) -> Option<PathBuf> {
        let key = self.project.as_ref()?;
        let store = cx.global::<WorkbenchStore>();
        store
            .projects
            .iter()
            .find(|p| &p.key == key)
            .map(|p| p.board_dir.clone())
    }

    fn load_project(&self, cx: &mut Context<Self>) {
        if let Some(dir) = self.board_dir(cx) {
            load_board(dir, cx);
        }
    }

    /// Refills the project select when the project list changed.
    fn sync_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let projects = &cx.global::<WorkbenchStore>().projects;
        let keys: Vec<String> = projects.iter().map(|p| p.key.clone()).collect();
        if keys != self.project_keys {
            let labels: Vec<SharedString> =
                projects.iter().map(|p| p.name.clone().into()).collect();
            if self.project.as_ref().is_none_or(|k| !keys.contains(k)) {
                self.project = self
                    .cwd
                    .as_deref()
                    .and_then(|dir| project_containing(projects, dir))
                    .or_else(|| keys.first().cloned());
            }
            let index = self
                .project
                .as_ref()
                .and_then(|k| keys.iter().position(|x| x == k))
                .map(IndexPath::new);
            self.project_select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(labels), window, cx);
                select.set_selected_index(index, window, cx);
            });
            self.project_keys = keys;
        }
        self.load_project(cx);
        cx.notify();
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.open.remove(&key) {
            self.open.insert(key);
        }
        cx.notify();
    }

    fn render_tree(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = cx.global::<WorkbenchStore>();
        let message = |text: &'static str| {
            div()
                .px(px(6.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(p.muted.to_hsla())
                .child(text)
                .into_any_element()
        };
        if !store.scanned {
            return message("Looking for boards\u{2026}");
        }
        if store.projects.is_empty() {
            return message(
                "No boards yet. They show up here once a Claude session with Maomi Workbench \
                 creates a plan.",
            );
        }
        let Some(board) = self.board_dir(cx).and_then(|dir| store.board(&dir)) else {
            return message("Loading\u{2026}");
        };
        let plans = board.plans.clone();
        let settings = board.settings.clone();
        let query = self.filter.read(cx).value().to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let archived = plans
            .iter()
            .filter(|e| e.plan.status == PlanStatus::Archived)
            .count();

        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, entry) in plans.iter().enumerate() {
            let plan = &entry.plan;
            if plan.status == PlanStatus::Archived && !self.show_archived {
                continue;
            }
            let plan_hit = matches(&plan.title);
            if !plan_hit && !plan.cards.iter().any(|c| matches(&c.title)) {
                continue;
            }
            let key = entry.path.to_string_lossy().into_owned();
            let open = !query.is_empty() || self.open.contains(&key);
            rows.push(self.plan_row(i, entry, open, key, p, cx));
            if !open {
                continue;
            }
            for column in Column::ALL {
                let cards: Vec<_> = plan
                    .cards_in(&column)
                    .into_iter()
                    .chain(plan.cards.iter().filter(|c| {
                        column == Column::Backlog && matches!(c.column, Column::Other(_))
                    }))
                    .filter(|c| plan_hit || matches(&c.title))
                    .collect();
                if cards.is_empty() {
                    continue;
                }
                let section = format!("{}|{}", entry.path.to_string_lossy(), column.as_str());
                let section_open = !query.is_empty() || self.open.contains(&section);
                rows.push(
                    tree_row(
                        SharedString::from(format!("wb-sect-{i}-{}", column.as_str())),
                        1,
                        p,
                    )
                    .h(px(26.))
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child(chevron(section_open, p))
                    .child(dot(column_color(&column, p), true))
                    .child(label(column_label(&column).to_owned()))
                    .child(count(cards.len(), p))
                    .on_click(cx.listener(move |panel, _, _, cx| panel.toggle(section.clone(), cx)))
                    .into_any_element(),
                );
                if !section_open {
                    continue;
                }
                for card in cards {
                    let color = settings
                        .card_type(card.type_id.as_deref())
                        .and_then(|t| slot_color(&t.color, p))
                        .unwrap_or(p.muted.to_hsla());
                    let (plan_path, card_id) = (entry.path.clone(), card.id.clone());
                    let done = card.column == Column::Done;
                    let title: SharedString = card.title.clone().into();
                    rows.push(
                        tree_row(SharedString::from(format!("wb-card-{i}-{}", card.id)), 2, p)
                            .h(px(26.))
                            .text_size(px(12.))
                            .when(done, |d| d.text_color(p.muted.to_hsla()))
                            .child(div().flex_none().w(px(16.)))
                            .child(dot(color, false))
                            .child(label(card.title.clone()))
                            .tooltip(move |window, cx| {
                                Tooltip::new(title.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(WorkbenchEvent::OpenCard {
                                    plan: plan_path.clone(),
                                    card: card_id.clone(),
                                })
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        if rows.is_empty() {
            rows.push(message(if query.is_empty() {
                "No plans in this project."
            } else {
                "Nothing matches the filter."
            }));
        }
        if archived > 0 {
            let text = if self.show_archived {
                "Hide archived plans".to_owned()
            } else {
                format!("Show archived plans ({archived})")
            };
            let emph = p.fg_emph.to_hsla();
            rows.push(
                div()
                    .id("wb-archived")
                    .mt(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(8.))
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .hover(move |s| s.text_color(emph))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.show_archived = !panel.show_archived;
                        cx.notify();
                    }))
                    .child(text)
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .children(rows)
            .into_any_element()
    }

    fn plan_row(
        &self,
        index: usize,
        entry: &PlanEntry,
        open: bool,
        key: String,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plan = &entry.plan;
        let selected = self.selected_plan.as_ref() == Some(&entry.path);
        let pct = percent(plan.progress());
        let group = SharedString::from(format!("wb-plan-{index}"));
        let quiet = matches!(plan.status, PlanStatus::Done | PlanStatus::Archived);
        let (hover, emph) = (p.surface_3.to_hsla(), p.fg_emph.to_hsla());
        let (path, board_path) = (entry.path.clone(), entry.path.clone());
        let title: SharedString = match &plan.status {
            PlanStatus::Active => plan.title.clone().into(),
            status => format!("{} ({})", plan.title, status.as_str()).into(),
        };
        tree_row(group.clone(), 0, p)
            .group(group.clone())
            .font_weight(gpui_kit::FontWeight::MEDIUM)
            .text_color(if selected {
                p.fg_emph.to_hsla()
            } else if quiet {
                p.muted.to_hsla()
            } else {
                p.fg.to_hsla()
            })
            .child(chevron(open, p))
            .child(
                Icon::new(IconName::Folder)
                    .size(px(15.))
                    .text_color(if selected {
                        p.accent.to_hsla()
                    } else {
                        p.muted.to_hsla()
                    }),
            )
            .child(label(plan.title.clone()))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(if pct == 100 {
                        p.ok.to_hsla()
                    } else {
                        p.muted.to_hsla()
                    })
                    .child(format!("{pct}%")),
            )
            .child(
                div()
                    .id(SharedString::from(format!("wb-open-{index}")))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded(px(6.))
                    .text_color(p.muted.to_hsla())
                    .hover(move |s| s.bg(hover).text_color(emph))
                    .tooltip(|window, cx| Tooltip::new("Open board").build(window, cx))
                    .child(Icon::new(AssetIcon::SquareKanban).size(px(14.)))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        cx.stop_propagation();
                        panel.selected_plan = Some(board_path.clone());
                        cx.emit(WorkbenchEvent::OpenBoard(board_path.clone()));
                        cx.notify();
                    })),
            )
            .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx))
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.selected_plan = Some(path.clone());
                panel.toggle(key.clone(), cx);
            }))
            .into_any_element()
    }
}

impl Render for WorkbenchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let tree = self.render_tree(p, cx);
        let has_projects = !cx.global::<WorkbenchStore>().projects.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .flex_none()
            .w(px(280.))
            .h_full()
            .px(px(10.))
            .py(px(12.))
            .overflow_hidden()
            .bg(p.bg.to_hsla())
            .border_r_1()
            .border_color(p.line.to_hsla())
            .child(header(
                "WORKBENCH",
                p,
                vec![
                    Button::new("wb-open-board")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(AssetIcon::SquareKanban))
                        .label("Open board")
                        .on_click(cx.listener(|panel, _, _, cx| {
                            if let Some(path) = panel.selected_plan(cx) {
                                panel.selected_plan = Some(path.clone());
                                cx.emit(WorkbenchEvent::OpenBoard(path));
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    icon_button("wb-collapse", IconName::PanelLeftClose, "Collapse dock", p)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(WorkbenchEvent::Collapse)))
                        .into_any_element(),
                ],
            ))
            .when(has_projects, |d| {
                // The select fills its parent's height, so it gets a box of its own.
                d.child(
                    div().flex_none().child(
                        Select::new(&self.project_select)
                            .small()
                            .menu_max_h(px(320.)),
                    ),
                )
                .child(Input::new(&self.filter).small().cleanable(true))
            })
            .child(
                div()
                    .id("wb-tree")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(tree),
            )
    }
}

/// The project whose folder holds `dir` (the deepest one when they nest). Compared without case,
/// as Windows paths in `board-projects.json` vary in drive-letter case.
fn project_containing(projects: &[Project], dir: &Path) -> Option<String> {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().to_lowercase().replace('/', "\\");
        format!("{}\\", text.trim_end_matches('\\'))
    };
    let dir = norm(dir);
    projects
        .iter()
        .filter_map(|p| Some((p, norm(p.path.as_deref()?))))
        .filter(|(_, root)| dir.starts_with(root.as_str()))
        .max_by_key(|(_, root)| root.len())
        .map(|(p, _)| p.key.clone())
}

/// A tree row indented by `level`.
fn tree_row(id: SharedString, level: usize, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = p.surface_2.to_hsla();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w_0()
        .h(px(28.))
        .pl(px(4. + level as f32 * 16.))
        .pr(px(6.))
        .rounded(px(8.))
        .text_size(px(12.5))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
}

fn chevron(open: bool, p: &Palette) -> impl IntoElement {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .size(px(13.))
    .text_color(p.muted.to_hsla())
}

/// A small colour mark: round for columns, square for card types.
fn dot(color: Hsla, round: bool) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(7.))
        .rounded(px(if round { 4. } else { 2. }))
        .bg(color)
}

fn label(text: String) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text)
}

fn count(n: usize, p: &Palette) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(6.))
        .rounded(px(99.))
        .text_size(px(10.5))
        .bg(p.surface_2.to_hsla())
        .child(n.to_string())
}
