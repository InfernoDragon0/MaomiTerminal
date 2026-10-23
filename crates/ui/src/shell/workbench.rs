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
    BoardClient, BoardEvent, BoardWatcher, Column, HandoffInfo, McpError, Plan, PlanStatus,
    Project, ProjectSettings,
};

use super::board::{aurora_ring, avatars, bar, size_tag};
use super::drawer::DragPreview;
use super::snippets::{header, icon_button};
use super::wlo;
use crate::{Palette, Theme};

/// A plan and the file it was read from (the file path is its identity in the UI).
#[derive(Clone)]
pub(crate) struct PlanEntry {
    pub path: PathBuf,
    pub plan: Arc<Plan>,
    /// "Where we left off": the newest handoff and the board activity after it.
    pub handoff: Arc<HandoffInfo>,
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
    /// Our own board server for edits; started on the first edit.
    client: Option<Arc<BoardClient>>,
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
                    handoff: Arc::new(maomi_workbench::read_handoff(&path)),
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

/// An edit the person makes from MaomiTerminal. It goes to the board server as actor "user".
#[derive(Debug, Clone)]
pub(crate) enum CardEdit {
    Move {
        column: Column,
        index: Option<usize>,
    },
    Task {
        id: String,
        done: bool,
        approval: bool,
    },
    Review {
        approve: bool,
        feedback: Option<String>,
    },
}

/// Sends `edit` for `card` of the plan at `plan_path`.
pub(crate) fn edit_card(
    plan_path: &Path,
    card: &str,
    edit: CardEdit,
    cx: &mut App,
) -> gpui_kit::Task<Result<(), String>> {
    let card = card.to_owned();
    write_plan(
        plan_path,
        cx,
        move |client, project, plan, rev| match edit {
            CardEdit::Move { column, index } => {
                client.move_card(project, plan, &card, &column, index, rev)
            }
            CardEdit::Task { id, done, approval } => {
                client.set_task_done(project, plan, &card, &id, done, approval, rev)
            }
            CardEdit::Review { approve, feedback } => {
                client.review(project, plan, &card, approve, feedback.as_deref())
            }
        },
    )
}

/// Answers an open question of the plan's handoff, as the user.
pub(crate) fn answer_question(
    plan_path: &Path,
    handoff: &str,
    question: &str,
    text: &str,
    cx: &mut App,
) -> gpui_kit::Task<Result<(), String>> {
    let (handoff, question, text) = (handoff.to_owned(), question.to_owned(), text.to_owned());
    write_plan(plan_path, cx, move |client, project, plan, _| {
        client.answer_question(project, plan, &handoff, &question, &text)
    })
}

/// Runs one write against the plan at `plan_path` on a background thread. Resolves to a message
/// the user can read on failure; on success the watcher brings the change in.
fn write_plan(
    plan_path: &Path,
    cx: &mut App,
    write: impl FnOnce(&BoardClient, &str, &str, i64) -> Result<String, McpError> + Send + 'static,
) -> gpui_kit::Task<Result<(), String>> {
    let Some((_, entry)) = cx.global::<WorkbenchStore>().plan(plan_path) else {
        return gpui_kit::Task::ready(Err("This plan is not loaded yet.".into()));
    };
    // base_rev = the rev on screen, so an edit made after a Claude session changed the plan is
    // refused instead of applied to cards the person has not seen.
    let (project, plan, rev) = (
        entry.plan.project_key.clone(),
        entry.plan.id.clone(),
        entry.plan.rev,
    );
    let client = board_client(cx);
    cx.background_executor().spawn(async move {
        let Some(client) = client else {
            return Err(
                "No home folder, so the Maomi Workbench board server cannot be found.".into(),
            );
        };
        write(&client, &project, &plan, rev)
            .map(|_| ())
            .map_err(|err| match err {
                McpError::Conflict(_) => {
                    "The board changed since you opened it. It refreshes in a moment; try again."
                        .to_owned()
                }
                other => other.to_string(),
            })
    })
}

/// The shared board server client, made on first use (the child process starts lazily too).
fn board_client(cx: &mut App) -> Option<Arc<BoardClient>> {
    let store = cx.global_mut::<WorkbenchStore>();
    if store.client.is_none() {
        store.client =
            BoardClient::default_script().map(|script| Arc::new(BoardClient::new(script)));
    }
    store.client.clone()
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
        // end_session writes the handoff into `<planId>.d` (not watched) and logs a line, so the
        // activity log is the signal to re-read it.
        BoardEvent::ActivityAppended { board_dir, plan_id } if loaded(&board_dir, cx) => {
            reload_handoff(board_dir.join(format!("{plan_id}.json")), cx)
        }
        // Plans of boards nobody has opened are read when they are.
        _ => {}
    }
}

pub(crate) fn reload_handoff(path: PathBuf, cx: &mut App) {
    let read = cx.background_executor().spawn({
        let path = path.clone();
        async move { maomi_workbench::read_handoff(&path) }
    });
    cx.spawn(async move |cx| {
        let info = Arc::new(read.await);
        cx.update(|cx| {
            let store = cx.global_mut::<WorkbenchStore>();
            let entry = path
                .parent()
                .and_then(|dir| store.boards.get_mut(dir))
                .and_then(|board| board.plans.iter_mut().find(|e| e.path == path));
            if let Some(entry) = entry {
                entry.handoff = info;
            }
        });
    })
    .detach();
}

fn reload_plan(path: PathBuf, cx: &mut App) {
    let read = cx.background_executor().spawn({
        let path = path.clone();
        async move {
            maomi_workbench::read_plan(&path)
                .map(|plan| (plan, maomi_workbench::read_handoff(&path)))
        }
    });
    cx.spawn(async move |cx| {
        let (plan, handoff) = match read.await {
            Ok((plan, handoff)) => (Arc::new(plan), Arc::new(handoff)),
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
                Some(entry) => {
                    entry.plan = plan;
                    entry.handoff = handoff;
                }
                None => board.plans.push(PlanEntry {
                    path: path.clone(),
                    plan,
                    handoff,
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

/// Drag payload of a card in the plan tree; a terminal pane types the prompt at its cursor.
#[derive(Debug, Clone)]
pub struct DraggedCard {
    pub prompt: String,
}

/// What the panel asks the workspace to do.
pub enum WorkbenchEvent {
    /// Show this plan's board in the centre.
    OpenBoard(PathBuf),
    /// Show the plan's whole "where we left off" in the drawer.
    OpenHandoff(PathBuf),
    OpenCard {
        plan: PathBuf,
        card: String,
    },
    Collapse,
}

type LabelSelect = SelectState<SearchableVec<SharedString>>;

pub struct WorkbenchPanel {
    project_select: Entity<LabelSelect>,
    /// Keys of the projects in the select, in its order.
    project_keys: Vec<String>,
    project: Option<String>,
    filter: Entity<InputState>,
    /// Expanded tree rows: a plan's path, or `path|column` for a column section.
    open: HashSet<String>,
    selected_plan: Option<PathBuf>,
    show_archived: bool,
    /// The plan whose "where we left off" peek is open under its row.
    peek: Option<PathBuf>,
    /// The focused terminal's folder. Its project is shown when it has a board.
    cwd: Option<PathBuf>,
    /// Board view: the selected plan as stacked columns instead of the tree.
    mini: bool,
    plan_select: Entity<LabelSelect>,
    /// Plans in the plan select, in its order (path and title).
    plan_items: Vec<(PathBuf, SharedString)>,
    /// Mini board columns opened or closed against the default (all open but Done).
    mini_flipped: HashSet<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<WorkbenchEvent> for WorkbenchPanel {}

impl WorkbenchPanel {
    pub fn new(cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        start(cx);
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter plans and cards"));
        let project_select =
            cx.new(|cx| LabelSelect::new(SearchableVec::new(Vec::new()), None, window, cx));
        let plan_select =
            cx.new(|cx| LabelSelect::new(SearchableVec::new(Vec::new()), None, window, cx));
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
            cx.subscribe_in(
                &plan_select,
                window,
                |panel, select, event: &SelectEvent<_>, _, cx| {
                    if !matches!(event, SelectEvent::Confirm(Some(_))) {
                        return;
                    }
                    let row = select.read(cx).selected_index(cx).map(|ix| ix.row);
                    if let Some((path, _)) = row.and_then(|row| panel.plan_items.get(row)) {
                        panel.selected_plan = Some(path.clone());
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
            peek: None,
            cwd,
            mini: false,
            plan_select,
            plan_items: Vec::new(),
            mini_flipped: HashSet::new(),
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
        self.sync_plans(window, cx);
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
        self.sync_plans(window, cx);
        cx.notify();
    }

    /// Refills the plan select (Board view) from the shown project and points it at the
    /// selected plan. Archived plans are left out, as in the tree by default.
    fn sync_plans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = cx.global::<WorkbenchStore>();
        let items: Vec<(PathBuf, SharedString)> = self
            .board_dir(cx)
            .and_then(|dir| store.board(&dir))
            .map(|board| {
                board
                    .plans
                    .iter()
                    .filter(|e| e.plan.status != PlanStatus::Archived)
                    .map(|e| (e.path.clone(), e.plan.title.clone().into()))
                    .collect()
            })
            .unwrap_or_default();
        let selected = self.selected_plan(cx);
        let index = selected
            .and_then(|s| items.iter().position(|(path, _)| *path == s))
            .map(IndexPath::new);
        let changed = items != self.plan_items;
        let labels: Vec<SharedString> = items.iter().map(|(_, title)| title.clone()).collect();
        self.plan_select.update(cx, |select, cx| {
            if changed {
                select.set_items(SearchableVec::new(labels), window, cx);
            }
            if changed || select.selected_index(cx) != index {
                select.set_selected_index(index, window, cx);
            }
        });
        self.plan_items = items;
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
            if self.peek.as_ref() == Some(&entry.path) {
                rows.push(self.render_peek(entry, p, cx));
            }
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
                    let tip: SharedString =
                        format!("{title}\nDrag into a terminal to paste its prompt").into();
                    let drag = DraggedCard {
                        prompt: maomi_workbench::card_prompt(&plan.id, &card.id, &card.title),
                    };
                    rows.push(
                        tree_row(SharedString::from(format!("wb-card-{i}-{}", card.id)), 2, p)
                            .h(px(26.))
                            .text_size(px(12.))
                            .when(done, |d| d.text_color(p.muted.to_hsla()))
                            .child(div().flex_none().w(px(16.)))
                            .child(dot(color, false))
                            .child(label(card.title.clone()))
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            .on_drag(drag, move |_, _, _, cx| {
                                let name = title.clone();
                                cx.new(|_| DragPreview { name })
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

    /// Board view: the selected plan's columns stacked down the dock, Done closed by default
    /// (decision D14).
    fn render_mini(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: &'static str| {
            div()
                .px(px(6.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(p.muted.to_hsla())
                .child(text)
                .into_any_element()
        };
        let store = cx.global::<WorkbenchStore>();
        if !store.scanned {
            return message("Looking for boards\u{2026}");
        }
        let Some(path) = self.selected_plan(cx) else {
            return message("No plans in this project.");
        };
        let Some((board, entry)) = store.plan(&path) else {
            return message("Loading\u{2026}");
        };
        let (plan, settings) = (entry.plan.clone(), board.settings.clone());
        let pct = percent(plan.progress());

        let mut rows: Vec<AnyElement> = vec![
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(6.))
                .pb(px(4.))
                .text_size(px(11.))
                .text_color(p.muted.to_hsla())
                .child(bar(pct, 3., p))
                .child(format!("{pct}%"))
                .into_any_element(),
        ];
        for column in Column::ALL {
            let mut cards: Vec<_> = plan
                .cards
                .iter()
                .filter(|c| shown_column(&c.column) == column)
                .collect();
            cards.sort_by(|a, b| a.order.total_cmp(&b.order));
            let name = column.as_str().to_owned();
            let open = (column != Column::Done) != self.mini_flipped.contains(&name);
            rows.push(
                tree_row(SharedString::from(format!("wb-mini-{name}")), 0, p)
                    .h(px(26.))
                    .text_size(px(12.))
                    .text_color(p.muted.to_hsla())
                    .child(chevron(open && !cards.is_empty(), p))
                    .child(dot(column_color(&column, p), true))
                    .child(label(column_label(&column).to_owned()))
                    .child(count(cards.len(), p))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if !panel.mini_flipped.remove(&name) {
                            panel.mini_flipped.insert(name.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if !open || cards.is_empty() {
                continue;
            }
            let faces: Vec<AnyElement> = cards
                .into_iter()
                .map(|card| {
                    let color = settings
                        .card_type(card.type_id.as_deref())
                        .and_then(|t| slot_color(&t.color, p))
                        .unwrap_or(p.muted.to_hsla());
                    mini_card(&plan, card, color, &path, p, cx)
                })
                .collect();
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .pl(px(18.))
                    .pt(px(2.))
                    .pb(px(6.))
                    .children(faces)
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

    /// The plan's "where we left off" flag: aurora ring, amber dot for open questions; a click
    /// opens the peek under the row.
    fn flag(
        &self,
        index: usize,
        entry: &PlanEntry,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = entry.path.clone();
        let open = self.peek.as_ref() == Some(&entry.path);
        let questions = wlo::open_questions(&entry.handoff) > 0;
        let (hover, emph) = (p.surface_3.to_hsla(), p.fg_emph.to_hsla());
        let face = div()
            .id(SharedString::from(format!("wb-flag-{index}")))
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(6.))
            .bg(if open { p.surface_3 } else { p.surface_2 }.to_hsla())
            .text_color(if open { emph } else { p.muted.to_hsla() })
            .hover(move |s| s.bg(hover).text_color(emph))
            .tooltip(|window, cx| Tooltip::new("Where we left off").build(window, cx))
            .on_click(cx.listener(move |panel, _, _, cx| {
                cx.stop_propagation();
                panel.peek = if panel.peek.as_ref() == Some(&path) {
                    None
                } else {
                    Some(path.clone())
                };
                cx.notify();
            }))
            .child(Icon::new(AssetIcon::Flag).size(px(12.)))
            .when(questions, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(-2.))
                        .right(px(-2.))
                        .size(px(7.))
                        .rounded(px(4.))
                        .bg(p.warn.to_hsla()),
                )
            });
        aurora_ring(face, 6., p)
    }

    /// The peek under a plan row: the handoff summary with Copy prompt and Open handoff.
    fn render_peek(&self, entry: &PlanEntry, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let plan = entry.plan.clone();
        let path = entry.path.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mx(px(4.))
            .my(px(4.))
            .p(px(10.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.line_strong.to_hsla())
            .bg(p.surface_2.to_hsla())
            .children(wlo::summary(&entry.handoff, p))
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .pt(px(2.))
                    .child(
                        Button::new("wb-peek-copy")
                            .primary()
                            .xsmall()
                            .label("Copy prompt")
                            .on_click(move |_, window, cx| {
                                wlo::copy_continue_prompt(&plan, window, cx)
                            }),
                    )
                    .child(
                        Button::new("wb-peek-open")
                            .xsmall()
                            .label("Open handoff")
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.peek = None;
                                cx.emit(WorkbenchEvent::OpenHandoff(path.clone()));
                                cx.notify();
                            })),
                    ),
            )
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
            // Last, so the flags line up down the tree.
            .child(self.flag(index, entry, p, cx))
            .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx))
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.selected_plan = Some(path.clone());
                panel.sync_plans(window, cx);
                panel.toggle(key.clone(), cx);
            }))
            .into_any_element()
    }
}

impl Render for WorkbenchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let body = if self.mini {
            self.render_mini(p, cx)
        } else {
            self.render_tree(p, cx)
        };
        let has_projects = !cx.global::<WorkbenchStore>().projects.is_empty();
        let mini = self.mini;
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
                    view_switch(mini, p, cx),
                    icon_button(
                        "wb-open-board",
                        Icon::new(AssetIcon::SquareKanban),
                        "Open the board in the centre",
                        p,
                    )
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
                .map(|d| {
                    if mini {
                        d.child(
                            div()
                                .flex_none()
                                .child(Select::new(&self.plan_select).small().menu_max_h(px(320.))),
                        )
                    } else {
                        d.child(Input::new(&self.filter).small().cleanable(true))
                    }
                })
            })
            .child(
                div()
                    .id("wb-tree")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

/// A card on the mini board: type colour, title, size, progress and assignee. Clicking opens
/// the drawer; dragging it onto a terminal types its prompt, as in the tree.
fn mini_card(
    plan: &Plan,
    card: &maomi_workbench::Card,
    color: Hsla,
    plan_path: &Path,
    p: &Palette,
    cx: &mut Context<WorkbenchPanel>,
) -> AnyElement {
    let working = card.column == Column::InProgress && !card.assignees.is_empty();
    let title: SharedString = card.title.clone().into();
    let tip: SharedString = format!("{title}\nDrag into a terminal to paste its prompt").into();
    let drag = DraggedCard {
        prompt: maomi_workbench::card_prompt(&plan.id, &card.id, &card.title),
    };
    let (plan_path, card_id) = (plan_path.to_owned(), card.id.clone());
    let hover = p.surface_3.to_hsla();
    let face = div()
        .id(SharedString::from(format!("wb-mini-card-{}", card.id)))
        .flex()
        .flex_col()
        .gap(px(4.))
        .px(px(8.))
        .pt(px(6.))
        .pb(px(7.))
        .rounded(px(8.))
        .border_1()
        .border_color(if working {
            gpui_kit::transparent_black()
        } else {
            p.line.to_hsla()
        })
        .bg(p.surface_2.to_hsla())
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .min_w_0()
                .text_size(px(12.))
                .text_color(p.fg_emph.to_hsla())
                .child(dot(color, false))
                .child(label(card.title.clone()))
                .when_some(card.size.as_ref(), |d, size| {
                    d.child(size_tag(size.as_str(), p))
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(bar(percent(card.progress()), 3., p))
                .child(avatars(&card.assignees[..card.assignees.len().min(1)], p)),
        )
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.emit(WorkbenchEvent::OpenCard {
                plan: plan_path.clone(),
                card: card_id.clone(),
            })
        }))
        .on_drag(drag, move |_, _, _, cx| {
            let name = title.clone();
            cx.new(|_| DragPreview { name })
        });
    if working {
        aurora_ring(face, 8., p)
    } else {
        face.into_any_element()
    }
}

/// Tree / Board switch in the dock header.
fn view_switch(mini: bool, p: &Palette, cx: &mut Context<WorkbenchPanel>) -> AnyElement {
    let option = |id: &'static str, text: &'static str, on: bool, to_mini: bool| {
        let emph = p.fg_emph.to_hsla();
        div()
            .id(id)
            .px(px(8.))
            .py(px(1.))
            .rounded(px(6.))
            .text_size(px(11.5))
            .map(|d| {
                if on {
                    d.text_color(emph).bg(p.surface.to_hsla())
                } else {
                    d.text_color(p.muted.to_hsla())
                        .cursor_pointer()
                        .hover(move |s| s.text_color(emph))
                }
            })
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.mini = to_mini;
                panel.sync_plans(window, cx);
                cx.notify();
            }))
            .child(text)
    };
    div()
        .flex()
        .gap(px(2.))
        .p(px(2.))
        .mr(px(2.))
        .rounded(px(8.))
        .bg(p.surface_2.to_hsla())
        .child(option("wb-view-tree", "Tree", !mini, false))
        .child(option("wb-view-board", "Board", mini, true))
        .into_any_element()
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
