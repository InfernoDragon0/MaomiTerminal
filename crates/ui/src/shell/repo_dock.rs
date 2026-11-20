//! The Repo dock: git for the focused terminal's repository. A toolbar (branch, fetch / pull /
//! push, stash), the recent commits with their lanes, and the working copy with a commit box
//! the board's suggestion fills in. Clicking a commit asks the workspace for the full Repo tab
//! ([`RepoDockEvent::OpenRepoTab`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bstr::{BString, ByteSlice as _};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EntityId, EventEmitter,
    Focusable as _, Global, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    WeakEntity, Window, div, px,
};
use maomi_git::graph::RowKind;
use maomi_git::parse::log::RefLabel;
use maomi_git::parse::refs::RefKind;
use maomi_git::parse::status::{EntryKind, FileState, Status, StatusEntry};
use maomi_git::runner::Outcome;
use maomi_git::{CommitRequest, PullMode};

use super::snippets::icon_button;
use crate::repo::diff::sanitize_line;
use crate::repo::lanes::{self, LaneColors};
use crate::repo::{LogScope, OpKind, RepoHub, RepoModel, RepoState};
use crate::{Palette, Theme};

/// Dock width, as in the mockup.
const WIDTH: f32 = 300.;
/// Commits the fold shows at first and adds per "Show more".
const COMMITS_STEP: usize = 30;
/// Most lanes the dock's graph column draws; wider histories collapse into a gutter.
const MAX_LANES: u32 = 4;
const ROW_H: f32 = 26.;
/// Git's advice for the first line of a commit message.
const TITLE_LIMIT: usize = 72;

pub enum RepoDockEvent {
    /// Show the main Repo tab, on a commit or on the working copy (`None`).
    #[allow(dead_code)] // Read once the workspace has the main Repo tab.
    OpenRepoTab {
        commit: Option<String>,
    },
    Collapse,
}

/// What the Commit button does; the last one picked is kept for every window.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CommitMode {
    /// Staged changes, or everything when nothing is staged.
    #[default]
    Staged,
    All,
    /// Like [`CommitMode::Staged`], then push.
    Push,
    /// Rewrite the last commit with the staged changes and this message.
    Amend,
}

#[derive(Default)]
struct RepoPrefs {
    mode: CommitMode,
}

impl Global for RepoPrefs {}

/// One working-copy group.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    Conflicts,
    Staged,
    Changes,
}

/// A commit message kept for a repo while the commit box shows another one.
#[derive(Default)]
struct Draft {
    title: String,
    body: String,
    prefilled: Option<String>,
}

pub struct RepoDock {
    hub: Entity<RepoHub>,
    /// The shell of the terminal the dock follows, for "from pwsh".
    source: Option<SharedString>,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    /// The repo root whose message is in the commit box.
    draft_root: Option<PathBuf>,
    /// Messages of the other repos the box has shown.
    drafts: HashMap<PathBuf, Draft>,
    /// (Repo root, session) pairs whose suggestion was cleared; not filled in again this run.
    dismissed: HashSet<(PathBuf, String)>,
    /// The session whose suggestion fills the commit box.
    prefilled: Option<String>,
    /// A commit was started; its message is cleared once it succeeds.
    committing: bool,
    /// New-branch name field, while open.
    new_branch: Option<Entity<InputState>>,
    commits_open: bool,
    working_open: bool,
    /// Rows of the log the Commits fold shows.
    shown: usize,
    /// The model the subscriptions below watch.
    watched: Option<EntityId>,
    model_subs: Vec<Subscription>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<RepoDockEvent> for RepoDock {}

impl RepoDock {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        if !cx.has_global::<RepoPrefs>() {
            cx.set_global(RepoPrefs::default());
        }
        let hub = cx.new(|_| RepoHub::new());
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Commit title"));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("Description")
        });
        let subs = vec![
            cx.observe_in(&hub, window, |dock, _, window, cx| {
                dock.watch_model(window, cx)
            }),
            cx.subscribe(&title, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            hub,
            source: None,
            title,
            body,
            draft_root: None,
            drafts: HashMap::new(),
            dismissed: HashSet::new(),
            prefilled: None,
            committing: false,
            new_branch: None,
            commits_open: true,
            working_open: true,
            shown: COMMITS_STEP,
            watched: None,
            model_subs: Vec::new(),
            _subs: subs,
        }
    }

    /// Shows the repo containing `dir` (the focused terminal's folder).
    pub fn follow(&mut self, dir: &Path, source: SharedString, cx: &mut Context<Self>) {
        if self.source.as_ref() != Some(&source) {
            self.source = Some(source);
            cx.notify();
        }
        self.hub.update(cx, |hub, cx| hub.follow(dir, cx));
    }

    /// The dock is hidden: stop watching the repo.
    pub fn release(&mut self, cx: &mut Context<Self>) {
        self.hub.update(cx, |hub, cx| hub.release(cx));
    }

    pub(super) fn model(&self, cx: &App) -> Option<Entity<RepoModel>> {
        self.hub.read(cx).model().cloned()
    }

    /// The hub changed: subscribe to its new model, if it has one.
    fn watch_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model(cx);
        let id = model.as_ref().map(|m| m.entity_id());
        if id == self.watched {
            return;
        }
        self.watched = id;
        self.model_subs.clear();
        self.shown = COMMITS_STEP;
        self.committing = false;
        self.new_branch = None;
        if let Some(model) = model {
            let root = model.read(cx).root().to_path_buf();
            self.swap_draft(&root, window, cx);
            self.model_subs
                .push(cx.observe_in(&model, window, |dock, model, window, cx| {
                    dock.on_model_changed(&model, window, cx)
                }));
            self.on_model_changed(&model, window, cx);
        }
        cx.notify();
    }

    fn on_model_changed(
        &mut self,
        model: &Entity<RepoModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (busy, failed) = {
            let m = model.read(cx);
            (m.is_busy(), m.op_error.is_some())
        };
        if self.committing && !busy {
            self.committing = false;
            if !failed {
                self.set_message("", "", window, cx);
                self.prefilled = None;
            }
        }
        self.fill_suggestion(model, window, cx);
        cx.notify();
    }

    /// Fills an empty commit box with the board's suggestion, once per session.
    fn fill_suggestion(
        &mut self,
        model: &Entity<RepoModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = model.read(cx).suggestion.clone() else {
            return;
        };
        let root = model.read(cx).root().to_path_buf();
        if self.draft_root.as_ref() != Some(&root)
            || self.prefilled.as_deref() == Some(s.session_id.as_str())
            || self.dismissed.contains(&(root, s.session_id.clone()))
            || !self.title.read(cx).value().is_empty()
        {
            return;
        }
        self.set_message(&s.title, &s.body, window, cx);
        self.prefilled = Some(s.session_id);
    }

    fn set_message(
        &mut self,
        title: &str,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, body) = (title.to_string(), body.to_string());
        self.title
            .update(cx, |input, cx| input.set_value(title, window, cx));
        self.body
            .update(cx, |input, cx| input.set_value(body, window, cx));
    }

    /// Puts the message of the repo at `root` in the commit box and keeps the one it showed, so a
    /// message typed for one repo is never committed into another.
    fn swap_draft(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft_root.as_deref() == Some(root) {
            return;
        }
        if let Some(old) = self.draft_root.take() {
            let draft = Draft {
                title: self.title.read(cx).value().to_string(),
                body: self.body.read(cx).value().to_string(),
                prefilled: self.prefilled.take(),
            };
            if draft.title.is_empty() && draft.body.is_empty() {
                self.drafts.remove(&old);
            } else {
                self.drafts.insert(old, draft);
            }
        }
        let draft = self.drafts.remove(root).unwrap_or_default();
        self.set_message(&draft.title, &draft.body, window, cx);
        self.prefilled = draft.prefilled;
        self.draft_root = Some(root.to_path_buf());
    }

    /// The commit box still holds the board's suggestion.
    fn showing_suggestion(&self, model: &RepoModel, cx: &App) -> bool {
        if self.draft_root.as_deref() != Some(model.root()) {
            return false;
        }
        match (&model.suggestion, &self.prefilled) {
            (Some(s), Some(id)) => {
                *id == s.session_id && self.title.read(cx).value().as_ref() == s.title
            }
            _ => false,
        }
    }

    fn clear_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(id), Some(root)) = (self.prefilled.take(), self.draft_root.clone()) {
            self.dismissed.insert((root, id));
        }
        self.set_message("", "", window, cx);
        cx.notify();
    }

    // ---- operations -------------------------------------------------------------------------

    /// Commits the box's message to `model`, the repo the button was drawn for. A model that is
    /// gone, or whose repo the box no longer holds the message of, is left alone.
    fn commit_for(
        &mut self,
        model: WeakEntity<RepoModel>,
        mode: CommitMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.global_mut::<RepoPrefs>().mode = mode;
        let Some(model) = model.upgrade() else {
            return;
        };
        if self.draft_root.as_deref() != Some(model.read(cx).root()) {
            return;
        }
        let summary = self.title.read(cx).value().trim().to_string();
        if summary.is_empty() {
            self.title.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        let body = self.body.read(cx).value().trim_end().to_string();
        let staged = model
            .read(cx)
            .status
            .as_ref()
            .map_or(0, |s| group_entries(s, Group::Staged).len());
        let req = CommitRequest {
            summary,
            body,
            amend: mode == CommitMode::Amend,
            ..CommitRequest::default()
        };
        let all = match mode {
            CommitMode::All => true,
            CommitMode::Staged | CommitMode::Push => staged == 0,
            CommitMode::Amend => false,
        };
        let push = mode == CommitMode::Push;
        let model = model.downgrade();
        if mode != CommitMode::Amend {
            self.start_commit(&model, req, all, push, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Amend the last commit?",
            Some("Replaces the last commit with the staged changes and this message."),
            &["Amend", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            _ = this.update(cx, |dock, cx| dock.start_commit(&model, req, all, push, cx));
        })
        .detach();
    }

    fn start_commit(
        &mut self,
        model: &WeakEntity<RepoModel>,
        req: CommitRequest,
        all: bool,
        push: bool,
        cx: &mut Context<Self>,
    ) {
        let Ok(started) = model.update(cx, |m, cx| m.commit(req, all, push, cx)) else {
            return;
        };
        self.committing |= started && self.watched == Some(model.entity_id());
        cx.notify();
    }

    fn open_new_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("New branch name"));
        let sub = cx.subscribe_in(&input, window, |dock, input, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let name = input.read(cx).value().trim().to_string();
                if !name.is_empty() {
                    dock.create_branch(name, cx);
                }
            }
        });
        self.model_subs.push(sub);
        input.focus_handle(cx).focus(window, cx);
        self.new_branch = Some(input);
        cx.notify();
    }

    fn create_branch(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(model) = self.model(cx) else {
            return;
        };
        let label = format!("Creating {name}");
        let started = model.update(cx, |m, cx| {
            m.run_op(label, cx, move |repo| async move {
                repo.create_branch(&name, "HEAD", true).await
            })
        });
        if started {
            self.new_branch = None;
        }
        cx.notify();
    }

    fn switch_branch(model: &Entity<RepoModel>, branch: String, cx: &mut App) {
        let label = format!("Switching to {branch}");
        model.update(cx, |m, cx| {
            m.run_op(label, cx, move |repo| async move {
                repo.switch_branch(&branch).await
            })
        });
    }

    // ---- rendering --------------------------------------------------------------------------

    fn render_header(
        &self,
        model: &RepoModel,
        p: &Palette,
        dock: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let status = model.status.as_ref();
        let changes = status.map_or(0, |s| s.entries.len());
        let branch: SharedString = status
            .and_then(|s| {
                s.branch
                    .head
                    .as_deref()
                    .map(|h| sanitize_line(h).into_owned())
            })
            .unwrap_or_else(|| "detached HEAD".into())
            .into();
        let (ahead, behind) = status.map_or((0, 0), |s| (s.branch.ahead, s.branch.behind));
        // Where Push goes: the upstream, else the remote git would be given first.
        let push_target = status
            .and_then(|s| s.branch.upstream.clone())
            .or_else(|| {
                model
                    .remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .or(model.remotes.first())
                    .map(|r| r.name.clone())
            })
            .map(|t| sanitize_line(&t).into_owned());
        let push_tip: SharedString = match (&push_target, ahead > 0) {
            (Some(t), true) => format!("Push to {t} · ahead of the upstream").into(),
            (Some(t), false) => format!("Push to {t}").into(),
            (None, _) => "Push · no remote".into(),
        };
        let busy = model.is_busy();
        let running = |kind: OpKind| busy && model.op_kind == kind;
        let locals: Vec<(String, bool)> = model
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::LocalBranch)
            .map(|r| (r.short_name.clone(), r.is_head))
            .collect();
        let menu_dock = dock.clone();
        let model_handle = self.model(cx);

        let chip = if status.is_none() {
            None
        } else if changes == 0 {
            Some(chip("Clean", p.ok.to_hsla(), p))
        } else {
            Some(chip(
                format!("{changes} change{}", plural(changes)),
                p.warn.to_hsla(),
                p,
            ))
        };

        let branch_menu = Button::new("repo-branch")
            .outline()
            .small()
            .w_full()
            .icon(Icon::new(AssetIcon::GitBranch))
            .label(branch)
            .disabled(busy)
            .tooltip("Switch branch")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (name, head) in locals.clone() {
                    let model = model_handle.clone();
                    let label = sanitize_line(&name).into_owned();
                    menu = menu.item(PopupMenuItem::new(label).checked(head).on_click(
                        move |_, _, cx| {
                            if let (Some(model), false) = (&model, head) {
                                Self::switch_branch(model, name.clone(), cx);
                            }
                        },
                    ));
                }
                let dock = menu_dock.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new("New branch…").on_click(move |_, window, cx| {
                            _ = dock.update(cx, |dock, cx| dock.open_new_branch(window, cx));
                        }),
                    )
            });

        let count = |n: u32| (n > 0).then(|| SharedString::from(n.to_string()));
        let tools = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                Button::new("repo-fetch")
                    .small()
                    .outline()
                    .icon(IconName::RefreshCw)
                    .tooltip("Fetch")
                    .loading(running(OpKind::Fetch))
                    .disabled(busy)
                    .on_click(on(dock, |dock, _, _, cx| {
                        if let Some(m) = dock.model(cx) {
                            m.update(cx, |m, cx| m.fetch(cx));
                        }
                    })),
            )
            .child(
                Button::new("repo-pull")
                    .small()
                    .outline()
                    .icon(Icon::new(AssetIcon::ArrowDownToLine))
                    .when_some(count(behind), |b, n| b.label(n))
                    .tooltip(if behind > 0 {
                        "Pull · behind the upstream"
                    } else {
                        "Pull"
                    })
                    .loading(running(OpKind::Pull))
                    .disabled(busy)
                    .on_click(on(dock, |dock, _, _, cx| {
                        if let Some(m) = dock.model(cx) {
                            m.update(cx, |m, cx| m.pull(PullMode::Config, cx));
                        }
                    })),
            )
            .child(
                Button::new("repo-push")
                    .small()
                    .outline()
                    .icon(Icon::new(AssetIcon::ArrowUpFromLine))
                    .when_some(count(ahead), |b, n| b.label(n))
                    .tooltip(push_tip)
                    .loading(running(OpKind::Push))
                    .disabled(busy)
                    .on_click(on(dock, |dock, _, _, cx| {
                        if let Some(m) = dock.model(cx) {
                            m.update(cx, |m, cx| m.push(cx));
                        }
                    })),
            )
            .child(
                Button::new("repo-stash")
                    .small()
                    .outline()
                    .icon(Icon::new(AssetIcon::Archive))
                    .tooltip("Stash all changes")
                    .disabled(busy || changes == 0)
                    .on_click(on(dock, |dock, _, _, cx| {
                        if let Some(m) = dock.model(cx) {
                            m.update(cx, |m, cx| m.stash(None, cx));
                        }
                    })),
            )
            .child(div().flex_1())
            .child(
                icon_button(
                    "repo-open-tab",
                    IconName::ExternalLink,
                    "Open the full Repo tab",
                    p,
                )
                .on_click(on(dock, |_, _, _, cx| {
                    cx.emit(RepoDockEvent::OpenRepoTab { commit: None })
                })),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(8.))
            .px(px(10.))
            .pt(px(10.))
            .pb(px(8.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(section_label("REPO", p))
                    .child(div().flex_1())
                    .children(chip)
                    .child(self.collapse_button(p, dock)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_w_0()
                    .child({
                        let summary: SharedString = model.summary().into();
                        div()
                            .id("repo-name")
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(summary.clone())
                                    .build(window, cx)
                            })
                            .child(model.name.clone())
                    })
                    .when_some(self.source.clone(), |d, source| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child(format!("· from {source}")),
                        )
                    }),
            )
            .child(div().flex_none().child(branch_menu))
            .when_some(self.new_branch.clone(), |d, input| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(
                            Button::new("repo-new-branch-ok")
                                .small()
                                .primary()
                                .label("Create")
                                .on_click(on(dock, move |dock, _, _, cx| {
                                    let name = input.read(cx).value().trim().to_string();
                                    if !name.is_empty() {
                                        dock.create_branch(name, cx);
                                    }
                                })),
                        )
                        .child(
                            icon_button("repo-new-branch-cancel", IconName::Close, "Cancel", p)
                                .on_click(on(dock, |dock, _, _, cx| {
                                    dock.new_branch = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(tools)
            .into_any_element()
    }

    fn collapse_button(&self, p: &Palette, dock: &WeakEntity<Self>) -> impl IntoElement {
        icon_button(
            "repo-collapse",
            IconName::PanelLeftClose,
            "Collapse dock",
            p,
        )
        .on_click(on(dock, |_, _, _, cx| cx.emit(RepoDockEvent::Collapse)))
    }

    fn render_commits(
        &self,
        model: &RepoModel,
        p: &Palette,
        dock: &WeakEntity<Self>,
    ) -> AnyElement {
        let history = &model.log.history;
        let shown = self.shown.min(history.len());
        let cap = MAX_LANES;
        let width = history.layout().width();
        let gutter = lanes::gutter(width, cap);
        let graph_w = lanes::column_width(width, cap, lanes::COMPACT);
        let colors = LaneColors::new(p, p.surface_2.to_hsla());
        let now = now_secs();
        let hover = p.surface_3.to_hsla();
        let mut rows = Vec::with_capacity(shown);
        for (ix, row_lanes) in lanes::rows_lanes(history, 0..shown).into_iter().enumerate() {
            let Some(row) = history.row(ix) else {
                continue;
            };
            let kind = history.kind(ix);
            let id = row.id.to_string();
            let (text, italic): (SharedString, bool) = match kind {
                RowKind::Wip => ("Uncommitted changes".into(), true),
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
                    RefLabel::Tag(n) => Some(ref_pill(n, true, p)),
                    _ => None,
                })
                .take(2)
                .collect();
            let commit = (kind == RowKind::Commit).then_some(id);
            rows.push(
                div()
                    .id(("repo-commit", ix))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(ROW_H))
                    .pr(px(6.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(on(dock, move |_, _, _, cx| {
                        cx.emit(RepoDockEvent::OpenRepoTab {
                            commit: commit.clone(),
                        })
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(graph_w)
                            .h_full()
                            .child(lanes::lanes_canvas(
                                row_lanes,
                                colors.clone(),
                                lanes::COMPACT,
                                None,
                                gutter,
                            )),
                    )
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
                    .when(kind == RowKind::Commit, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child(ago(now, row.time)),
                        )
                    })
                    .into_any_element(),
            );
        }

        let more: Option<AnyElement> = if let Some(err) = &model.log.error {
            Some(note(err.clone(), p.err.to_hsla()).into_any_element())
        } else if model.log.loading {
            Some(note("Loading commits…".into(), p.muted.to_hsla()).into_any_element())
        } else if shown < history.len() || !model.log.exhausted {
            let accent = p.accent.to_hsla();
            Some(
                div()
                    .id("repo-more")
                    .py(px(6.))
                    .flex()
                    .justify_center()
                    .text_size(px(11.5))
                    .text_color(p.muted.to_hsla())
                    .cursor_pointer()
                    .hover(move |s| s.text_color(accent))
                    .child("Show more")
                    .on_click(on(dock, |dock, _, _, cx| {
                        dock.shown += COMMITS_STEP;
                        if let Some(m) = dock.model(cx)
                            && dock.shown > m.read(cx).log.history.len()
                        {
                            m.update(cx, |m, cx| m.load_more(cx));
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            )
        } else if history.is_empty() {
            Some(note("No commits yet.".into(), p.muted.to_hsla()).into_any_element())
        } else {
            None
        };

        let scope = model.log_scope;
        let open = self.commits_open;
        let header = fold_header("repo-fold-commits", "COMMITS", open, p)
            .on_click(on(dock, |dock, _, _, cx| {
                dock.commits_open = !dock.commits_open;
                cx.notify();
            }))
            .child(div().flex_1())
            .child(scope_switch(scope, p, dock));
        let body = div().flex().flex_col().children(rows).children(more);
        fold_section("repo-commits-scroll", open, header, body)
    }

    /// The commit box (with the Suggested badge) for the Repo tab, which shows it while this
    /// dock is hidden: one message, one suggestion, one remembered mode.
    ///
    /// `handle` is the model the buttons act on. The box holds one repo's message at a time, so
    /// it is only drawn for that repo.
    pub(super) fn commit_box_for(
        &self,
        handle: &Entity<RepoModel>,
        model: &RepoModel,
        p: &Palette,
        dock: &WeakEntity<Self>,
        cx: &App,
    ) -> Option<AnyElement> {
        if self.draft_root.as_deref() != Some(model.root()) {
            return None;
        }
        let status = model.status.as_ref().filter(|s| !s.entries.is_empty())?;
        let suggested = self.showing_suggestion(model, cx);
        let badge = suggested
            .then_some(model.suggestion.as_ref())
            .flatten()
            .map(|s| suggested_badge(suggestion_tip(s), p));
        Some(
            div()
                .flex()
                .flex_col()
                .children(badge.map(|b| div().flex().pb(px(4.)).child(b)))
                .child(self.render_commit_box(handle, model, status, suggested, p, dock, cx))
                .into_any_element(),
        )
    }

    fn render_working_copy(
        &self,
        handle: &Entity<RepoModel>,
        model: &RepoModel,
        p: &Palette,
        dock: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let Some(status) = &model.status else {
            return div().into_any_element();
        };
        let changes = status.entries.len();
        let suggested = self.showing_suggestion(model, cx);
        let open = self.working_open;
        let mut header = fold_header("repo-fold-working", "WORKING COPY", open, p).on_click(on(
            dock,
            |dock, _, _, cx| {
                dock.working_open = !dock.working_open;
                cx.notify();
            },
        ));
        if changes > 0 {
            header = header.child(count_badge(changes, p));
        }
        header = header.child(div().flex_1());
        if suggested && let Some(s) = &model.suggestion {
            header = header.child(suggested_badge(suggestion_tip(s), p));
        }

        let body: AnyElement = if changes == 0 {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(4.))
                .py(px(8.))
                .text_color(p.muted.to_hsla())
                .child(Icon::new(IconName::Check).size(px(14.)))
                .child("Nothing to commit. Working tree is clean.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .child(self.render_commit_box(handle, model, status, suggested, p, dock, cx))
                .children(
                    [Group::Conflicts, Group::Staged, Group::Changes]
                        .into_iter()
                        .filter_map(|g| {
                            self.render_group(
                                g,
                                status,
                                handle.downgrade(),
                                model.is_busy(),
                                p,
                                dock,
                            )
                        }),
                )
                .into_any_element()
        };

        fold_section("repo-working-scroll", open, header, body)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_commit_box(
        &self,
        handle: &Entity<RepoModel>,
        model: &RepoModel,
        status: &Status,
        suggested: bool,
        p: &Palette,
        dock: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let title_len = self.title.read(cx).value().chars().count();
        let staged = group_entries(status, Group::Staged).len();
        let all = status.entries.len();
        let mode = cx.global::<RepoPrefs>().mode;
        let label = commit_label(mode, staged, all);
        let busy = model.is_busy();
        let empty = title_len == 0;
        let has_commits = !model.log.history.is_empty();
        let menu_dock = dock.clone();
        let target = handle.downgrade();
        let menu_target = target.clone();
        let over = title_len > TITLE_LIMIT;

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(4.))
            .pt(px(4.))
            .pb(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child("Title")
                    .child(div().flex_1())
                    .child(
                        div()
                            .when(over, |d| d.text_color(p.warn.to_hsla()))
                            .child(format!("{title_len} / {TITLE_LIMIT}")),
                    )
                    .when(!empty || suggested, |d| {
                        let accent = p.accent.to_hsla();
                        d.child(
                            div()
                                .id("repo-clear-message")
                                .text_color(accent)
                                .cursor_pointer()
                                .child("Clear")
                                .on_click(on(dock, |dock, _, window, cx| {
                                    dock.clear_message(window, cx)
                                })),
                        )
                    }),
            )
            .child(Input::new(&self.title).small())
            .child(Textarea::new(&self.body).small())
            .child(
                div()
                    .flex()
                    .gap(px(1.))
                    .child(
                        div().flex_1().child(
                            Button::new("repo-commit")
                                .primary()
                                .small()
                                .w_full()
                                .label(label)
                                .loading(self.committing && busy)
                                .disabled(busy || empty)
                                .on_click(on(dock, move |dock, _, window, cx| {
                                    dock.commit_for(target.clone(), mode, window, cx)
                                })),
                        ),
                    )
                    .child(
                        Button::new("repo-commit-more")
                            .primary()
                            .small()
                            .icon(IconName::ChevronDown)
                            .tooltip("Commit all · Commit & push · Amend")
                            .disabled(busy)
                            .dropdown_menu(move |menu, _, _| {
                                let item = |text: &'static str, m: CommitMode| {
                                    let dock = menu_dock.clone();
                                    let target = menu_target.clone();
                                    PopupMenuItem::new(text).checked(mode == m).on_click(
                                        move |_, window, cx| {
                                            _ = dock.update(cx, |dock, cx| {
                                                dock.commit_for(target.clone(), m, window, cx)
                                            });
                                        },
                                    )
                                };
                                let menu = menu
                                    .item(item("Commit staged", CommitMode::Staged))
                                    .item(item("Commit all", CommitMode::All))
                                    .item(item("Commit and push", CommitMode::Push));
                                if has_commits {
                                    menu.separator()
                                        .item(item("Amend last commit", CommitMode::Amend))
                                } else {
                                    menu
                                }
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_group(
        &self,
        group: Group,
        status: &Status,
        target: WeakEntity<RepoModel>,
        busy: bool,
        p: &Palette,
        dock: &WeakEntity<Self>,
    ) -> Option<AnyElement> {
        let entries = group_entries(status, group);
        if entries.is_empty() {
            return None;
        }
        let (title, gid) = match group {
            Group::Conflicts => ("Conflicts", "conflicts"),
            Group::Staged => ("Staged", "staged"),
            Group::Changes => ("Changes", "changes"),
        };
        let paths = |entries: &[StatusEntry]| -> Vec<BString> {
            entries.iter().map(|e| e.path.clone()).collect()
        };

        let mut actions: Vec<AnyElement> = Vec::new();
        if !busy {
            match group {
                Group::Conflicts => {}
                Group::Staged => actions.push(
                    icon_button("repo-unstage-all", IconName::Minus, "Unstage all", p)
                        .on_click(on(dock, |dock, _, _, cx| {
                            if let Some(m) = dock.model(cx) {
                                m.update(cx, |m, cx| m.unstage_all(cx));
                            }
                        }))
                        .into_any_element(),
                ),
                Group::Changes => {
                    let all = entries.clone();
                    let discard_target = target.clone();
                    let unstaged = paths(&entries);
                    actions.push(
                        icon_button("repo-discard-all", IconName::Undo2, "Discard all…", p)
                            .on_click(on(dock, move |_, _, window, cx| {
                                discard_for(discard_target.clone(), all.clone(), window, cx)
                            }))
                            .into_any_element(),
                    );
                    actions.push(
                        icon_button("repo-stage-all", IconName::Plus, "Stage all", p)
                            .on_click(on(dock, move |dock, _, _, cx| {
                                if let Some(m) = dock.model(cx) {
                                    let paths = unstaged.clone();
                                    m.update(cx, |m, cx| m.stage(paths, cx));
                                }
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .px(px(4.))
            .text_size(px(11.5))
            .text_color(p.muted.to_hsla())
            .child(title)
            .child(count_badge(entries.len(), p))
            .child(div().flex_1())
            .child(div().flex().gap(px(2.)).children(actions));

        let rows = entries.iter().enumerate().map(|(ix, entry)| {
            self.render_file(group, gid, ix, entry, target.clone(), busy, p, dock)
        });

        Some(
            div()
                .flex()
                .flex_col()
                .mt(px(6.))
                .child(header)
                .children(rows)
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_file(
        &self,
        group: Group,
        gid: &'static str,
        ix: usize,
        entry: &StatusEntry,
        target: WeakEntity<RepoModel>,
        busy: bool,
        p: &Palette,
        dock: &WeakEntity<Self>,
    ) -> AnyElement {
        let (letter, color) = file_letter(entry, group, p);
        let path = sanitize_line(&entry.path.to_str_lossy()).into_owned();
        let (dir, name) = match path.rfind('/') {
            Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
            None => (String::new(), path.clone()),
        };
        let row_group: SharedString = format!("repo-file-{gid}-{ix}").into();
        let hover = p.surface_3.to_hsla();
        let id = |what: &'static str| {
            ElementId::from((SharedString::from(format!("repo-{what}-{gid}")), ix))
        };

        let mut actions: Vec<AnyElement> = Vec::new();
        if !busy {
            let one = vec![entry.path.clone()];
            match group {
                Group::Conflicts => {
                    actions.push(
                        small_icon(
                            id("open"),
                            IconName::ExternalLink,
                            "Open in the Repo tab",
                            p,
                        )
                        .on_click(on(dock, |_, _, _, cx| {
                            cx.emit(RepoDockEvent::OpenRepoTab { commit: None })
                        }))
                        .into_any_element(),
                    );
                    actions.push(
                        small_icon(id("resolve"), IconName::Plus, "Mark resolved (stage)", p)
                            .on_click(on(dock, move |dock, _, _, cx| {
                                if let Some(m) = dock.model(cx) {
                                    let paths = one.clone();
                                    m.update(cx, |m, cx| m.stage(paths, cx));
                                }
                            }))
                            .into_any_element(),
                    );
                }
                Group::Staged => actions.push(
                    small_icon(id("unstage"), IconName::Minus, "Unstage", p)
                        .on_click(on(dock, move |dock, _, _, cx| {
                            if let Some(m) = dock.model(cx) {
                                let paths = one.clone();
                                m.update(cx, |m, cx| m.unstage(paths, cx));
                            }
                        }))
                        .into_any_element(),
                ),
                Group::Changes => {
                    let entry = entry.clone();
                    actions.push(
                        small_icon(id("discard"), IconName::Undo2, "Discard…", p)
                            .on_click(on(dock, move |_, _, window, cx| {
                                discard_for(target.clone(), vec![entry.clone()], window, cx)
                            }))
                            .into_any_element(),
                    );
                    actions.push(
                        small_icon(id("stage"), IconName::Plus, "Stage", p)
                            .on_click(on(dock, move |dock, _, _, cx| {
                                if let Some(m) = dock.model(cx) {
                                    let paths = one.clone();
                                    m.update(cx, |m, cx| m.stage(paths, cx));
                                }
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        let tip: SharedString = match &entry.orig_path {
            Some(from) => format!("{} → {path}", sanitize_line(&from.to_str_lossy())).into(),
            None => path.clone().into(),
        };
        div()
            .id(id("file"))
            .group(row_group.clone())
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .px(px(4.))
            .rounded(px(6.))
            .hover(move |s| s.bg(hover))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
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
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(p.muted.to_hsla())
                            .child(dir),
                    )
                    .child(div().flex_none().child(name)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(2.))
                    .invisible()
                    .group_hover(row_group, |s| s.visible())
                    .children(actions),
            )
            .into_any_element()
    }

    /// The running operation, or the last write command and how it went.
    fn render_out_strip(
        &self,
        model: &RepoModel,
        p: &Palette,
        dock: &WeakEntity<Self>,
    ) -> Option<AnyElement> {
        let strip = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .text_size(px(11.5))
            .border_t_1()
            .border_color(p.line.to_hsla())
            .bg(p.surface.to_hsla());
        if let Some(label) = &model.op_label {
            // Killing git halfway through a commit, stage or discard leaves the repo in a state
            // the user did not ask for; only the network operations are safe to stop. "Committing
            // and pushing" counts as a push by name, so it is excluded by its label.
            let cancellable = matches!(model.op_kind, OpKind::Fetch | OpKind::Pull | OpKind::Push)
                && !label.starts_with("Committing");
            return Some(
                strip
                    .child(chip(label.clone(), p.accent.to_hsla(), p))
                    .child(div().flex_1())
                    .when(cancellable, |d| {
                        d.child(
                            Button::new("repo-cancel-op")
                                .ghost()
                                .xsmall()
                                .label("Cancel")
                                .on_click(on(dock, |dock, _, _, cx| {
                                    if let Some(m) = dock.model(cx) {
                                        m.update(cx, |m, cx| m.cancel_op(cx));
                                    }
                                })),
                        )
                    })
                    .into_any_element(),
            );
        }
        if let Some(err) = &model.op_error {
            return Some(
                strip
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(p.err.to_hsla())
                            .child(err.clone()),
                    )
                    .child(
                        small_icon("repo-dismiss-error".into(), IconName::Close, "Dismiss", p)
                            .on_click(on(dock, |dock, _, _, cx| {
                                if let Some(m) = dock.model(cx) {
                                    m.update(cx, |m, cx| m.clear_op_error(cx));
                                }
                            })),
                    )
                    .into_any_element(),
            );
        }
        let record = model.last_command.as_ref()?;
        let (text, color) = match record.outcome {
            Outcome::Running => ("Running", p.accent.to_hsla()),
            Outcome::Ok => ("Done", p.ok.to_hsla()),
            Outcome::Cancelled => ("Cancelled", p.muted.to_hsla()),
            Outcome::Failed { .. } | Outcome::TimedOut => ("Failed", p.err.to_hsla()),
        };
        let took = record
            .duration
            .map(|d| format!("{:.1} s", d.as_secs_f32()))
            .unwrap_or_default();
        Some(
            strip
                .child(chip(text, color, p))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(crate::TERMINAL_FAMILY)
                        .child(record.command.clone()),
                )
                .child(div().flex_none().text_color(p.muted.to_hsla()).child(took))
                .into_any_element(),
        )
    }

    fn render_empty(
        &self,
        title: &'static str,
        detail: String,
        p: &Palette,
        dock: &WeakEntity<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .pt(px(10.))
                    .child(section_label("REPO", p))
                    .child(div().flex_1())
                    .child(self.collapse_button(p, dock)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(40.))
                    .text_color(p.muted.to_hsla())
                    .child(Icon::new(AssetIcon::GitBranch).size(px(28.)))
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child(title),
                    )
                    .child(div().text_center().child(detail)),
            )
            .into_any_element()
    }
}

impl Render for RepoDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let dock = &cx.entity().downgrade();
        let cx: &App = cx;
        let frame = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(WIDTH))
            .h_full()
            .overflow_hidden()
            .bg(p.surface_2.to_hsla())
            .border_r_1()
            .border_color(p.line.to_hsla());
        let state = self.hub.read(cx).state().clone();
        let model = match state {
            RepoState::Open(model) => model,
            RepoState::Idle => {
                return frame.child(self.render_empty(
                    "No terminal is open",
                    "The Repo dock follows the focused terminal's folder.".into(),
                    p,
                    dock,
                ));
            }
            RepoState::Probing(dir) => {
                return frame.child(self.render_empty(
                    "Looking for a git repo…",
                    display_dir(&dir),
                    p,
                    dock,
                ));
            }
            RepoState::NoRepo { dir, reason } => {
                // Git's own "not a git repository" adds nothing; other reasons (no git) do.
                let reason = if reason.contains("not a git repository") {
                    String::new()
                } else {
                    format!(" ({reason})")
                };
                return frame.child(self.render_empty(
                    "No git repo here",
                    format!(
                        "The focused terminal is in {}. cd into a repo and this updates.{reason}",
                        display_dir(&dir)
                    ),
                    p,
                    dock,
                ));
            }
        };
        let m = model.read(cx);
        let header = self.render_header(m, p, dock, cx);
        let commits = self.render_commits(m, p, dock);
        let working = self.render_working_copy(&model, m, p, dock, cx);
        let strip = self.render_out_strip(m, p, dock);
        let accent = p.accent.to_hsla();
        frame
            .child(header)
            .child(
                // Both folds open: they share the height, each scrolling on its own.
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(6.))
                    .px(px(6.))
                    .pt(px(4.))
                    .pb(px(6.))
                    .child(commits)
                    .child(working),
            )
            .children(strip)
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(p.line.to_hsla())
                    .child(
                        div()
                            .id("repo-open-full")
                            .text_size(px(12.))
                            .text_color(accent)
                            .cursor_pointer()
                            .hover(|s| s.text_decoration_1())
                            .child("Open the full Repo tab →")
                            .on_click(on(dock, |_, _, _, cx| {
                                cx.emit(RepoDockEvent::OpenRepoTab { commit: None })
                            })),
                    ),
            )
    }
}

// ---- working copy ---------------------------------------------------------------------------

/// The entries of one group, in status order. A file staged and changed again is in both
/// Staged and Changes.
pub(super) fn group_entries(status: &Status, group: Group) -> Vec<StatusEntry> {
    status
        .entries
        .iter()
        .filter(|e| in_group(e, group))
        .cloned()
        .collect()
}

fn in_group(entry: &StatusEntry, group: Group) -> bool {
    match entry.kind {
        EntryKind::Ignored => false,
        EntryKind::Unmerged => group == Group::Conflicts,
        EntryKind::Untracked => group == Group::Changes,
        EntryKind::Ordinary | EntryKind::RenamedOrCopied { .. } => match group {
            Group::Conflicts => false,
            Group::Staged => entry.index != FileState::Unmodified,
            Group::Changes => entry.worktree != FileState::Unmodified,
        },
    }
}

/// The status letter of `entry` in `group` and its colour.
pub(super) fn file_letter(entry: &StatusEntry, group: Group, p: &Palette) -> (&'static str, Hsla) {
    let state = match (group, &entry.kind) {
        (Group::Conflicts, _) => return ("!", p.err.to_hsla()),
        (_, EntryKind::Untracked) => return ("U", p.muted.to_hsla()),
        (Group::Staged, _) => entry.index,
        (Group::Changes, _) => entry.worktree,
    };
    match state {
        FileState::Added => ("A", p.ok.to_hsla()),
        FileState::Deleted => ("D", p.err.to_hsla()),
        FileState::Renamed => ("R", p.c[1].to_hsla()),
        FileState::Copied => ("C", p.c[1].to_hsla()),
        FileState::TypeChanged => ("T", p.warn.to_hsla()),
        FileState::Unmerged => ("!", p.err.to_hsla()),
        FileState::Modified | FileState::Unmodified => ("M", p.warn.to_hsla()),
    }
}

fn commit_label(mode: CommitMode, staged: usize, all: usize) -> String {
    match mode {
        CommitMode::Staged if staged > 0 => format!("Commit staged ({staged})"),
        CommitMode::Staged | CommitMode::All => format!("Commit all ({all})"),
        CommitMode::Push if staged > 0 => format!("Commit staged and push ({staged})"),
        CommitMode::Push => format!("Commit all and push ({all})"),
        CommitMode::Amend => "Amend last commit".into(),
    }
}

// ---- small pieces ---------------------------------------------------------------------------

/// `cx.listener` for render code that only holds `&App`.
pub(super) fn on<T: 'static, E: ?Sized>(
    view: &WeakEntity<T>,
    f: impl Fn(&mut T, &E, &mut Window, &mut Context<T>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |event, window, cx| {
        _ = view.update(cx, |v, cx| f(v, event, window, cx));
    }
}

/// Asks, then discards `entries` in `model`, the repo the rows were drawn for. The model is
/// held weakly while the prompt is open; if it is gone by then nothing happens.
pub(super) fn discard_for<T: 'static>(
    model: WeakEntity<RepoModel>,
    entries: Vec<StatusEntry>,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    if entries.is_empty() {
        return;
    }
    let message = match entries.as_slice() {
        [one] => format!(
            "Discard changes to {}?",
            sanitize_line(&one.path.to_str_lossy())
        ),
        many => format!("Discard changes to {} files?", many.len()),
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some("Untracked files are deleted. This can't be undone."),
        &["Discard", "Cancel"],
        cx,
    );
    cx.spawn_in(window, async move |_, cx| {
        if answer.await != Ok(0) {
            return;
        }
        let (untracked, tracked): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|e| e.kind == EntryKind::Untracked);
        let paths = |v: Vec<StatusEntry>| v.into_iter().map(|e| e.path).collect();
        _ = model.update(cx, |m, cx| m.discard(paths(tracked), paths(untracked), cx));
    })
    .detach();
}

/// What a [`segmented`] option does to its view when picked.
pub(super) type Pick<T> = Box<dyn Fn(&mut T, &mut Context<T>)>;

/// A small segmented switch: (id, label, active, action) per option.
pub(super) fn segmented<T: 'static>(
    view: &WeakEntity<T>,
    options: Vec<(&'static str, &'static str, bool, Pick<T>)>,
    p: &Palette,
) -> AnyElement {
    let emph = p.fg_emph.to_hsla();
    div()
        .flex()
        .flex_none()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(8.))
        .bg(p.surface_3.to_hsla())
        .children(options.into_iter().map(|(id, text, active, pick)| {
            div()
                .id(id)
                .px(px(7.))
                .py(px(1.))
                .rounded(px(6.))
                .text_size(px(11.))
                .font_weight(gpui_kit::FontWeight::NORMAL)
                .map(|d| {
                    if active {
                        d.text_color(emph).bg(p.surface.to_hsla())
                    } else {
                        d.text_color(p.muted.to_hsla())
                            .cursor_pointer()
                            .hover(move |s| s.text_color(emph))
                    }
                })
                .on_click(on(view, move |v, _, _, cx| {
                    cx.stop_propagation();
                    pick(v, cx);
                    cx.notify();
                }))
                .child(text)
        }))
        .into_any_element()
}

pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(super) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `5m`, `3h`, `2d`, `4w`, `7mo`, `2y`.
pub(super) fn ago(now: i64, time: i64) -> String {
    let secs = (now - time).max(0);
    let (n, unit) = match secs {
        s if s < 60 => return "now".into(),
        s if s < 3600 => (s / 60, "m"),
        s if s < 86_400 => (s / 3600, "h"),
        s if s < 7 * 86_400 => (s / 86_400, "d"),
        s if s < 30 * 86_400 => (s / (7 * 86_400), "w"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "mo"),
        s => (s / (365 * 86_400), "y"),
    };
    format!("{n}{unit}")
}

fn display_dir(dir: &Path) -> String {
    super::pane::display_path(dir)
}

fn section_label(text: &'static str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(p.muted.to_hsla())
        .child(text)
}

pub(super) fn chip(text: impl Into<SharedString>, color: Hsla, p: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .px(px(7.))
        .py(px(1.))
        .rounded(px(9.))
        .text_size(px(11.))
        .bg(p.surface.to_hsla())
        .text_color(p.fg.to_hsla())
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(text.into())
        .into_any_element()
}

pub(super) fn count_badge(n: usize, p: &Palette) -> impl IntoElement {
    div()
        .px(px(6.))
        .rounded(px(9.))
        .text_size(px(11.))
        .bg(p.surface_3.to_hsla())
        .text_color(p.fg.to_hsla())
        .child(n.to_string())
}

fn suggestion_tip(s: &maomi_workbench::CommitSuggestion) -> SharedString {
    format!(
        "From the board: {} task{} ticked on {} card{}. Nothing is committed until you click Commit.",
        s.tasks,
        plural(s.tasks),
        s.cards,
        plural(s.cards)
    )
    .into()
}

fn suggested_badge(tip: SharedString, p: &Palette) -> impl IntoElement {
    div()
        .id("repo-suggested")
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(6.))
        .rounded(px(9.))
        .text_size(px(10.5))
        .bg(p.accent_soft.to_hsla())
        .text_color(p.fg_emph.to_hsla())
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .child(Icon::new(AssetIcon::Sparkles).size(px(11.)))
        .child("Suggested")
}

pub(super) fn ref_pill(name: &str, tag: bool, p: &Palette) -> AnyElement {
    div()
        .flex_none()
        .ml(px(6.))
        .px(px(5.))
        .rounded(px(5.))
        .text_size(px(10.5))
        .max_w(px(110.))
        .truncate()
        .bg(if tag {
            p.surface_3.to_hsla()
        } else {
            p.accent_soft.to_hsla()
        })
        .text_color(if tag {
            p.fg.to_hsla()
        } else {
            p.fg_emph.to_hsla()
        })
        .child(sanitize_line(name).into_owned())
        .into_any_element()
}

fn note(text: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(6.))
        .text_size(px(11.5))
        .text_color(color)
        .child(text)
}

fn fold_header(
    id: &'static str,
    title: &'static str,
    open: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(4.))
        .py(px(6.))
        .cursor_pointer()
        .text_size(px(11.5))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(p.muted.to_hsla())
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(13.)),
        )
        .child(title)
}

/// A fold with its header pinned. Open, it takes an equal share of the free height and its
/// body scrolls; closed, only the header shows.
fn fold_section(
    scroll_id: &'static str,
    open: bool,
    header: impl IntoElement,
    body: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .map(|d| {
            if open {
                d.flex_1().min_h_0()
            } else {
                d.flex_none()
            }
        })
        .child(div().flex_none().child(header))
        .when(open, |d| {
            d.child(
                div()
                    .id(scroll_id)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
        })
        .into_any_element()
}

pub(super) fn small_icon(
    id: ElementId,
    icon: IconName,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded(px(5.))
        .text_color(p.muted.to_hsla())
        .hover(move |s| s.bg(hover).text_color(emph))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
        .child(Icon::new(icon).size(px(13.)))
}

/// Branch / All switch in the Commits fold's header.
fn scope_switch(scope: LogScope, p: &Palette, dock: &WeakEntity<RepoDock>) -> AnyElement {
    let pick = |value: LogScope| -> Pick<RepoDock> {
        Box::new(move |dock, cx| {
            if let Some(m) = dock.model(cx) {
                m.update(cx, |m, cx| m.set_log_scope(value, cx));
            }
            dock.shown = COMMITS_STEP;
        })
    };
    segmented(
        dock,
        vec![
            (
                "repo-scope-branch",
                "Branch",
                scope == LogScope::Branch,
                pick(LogScope::Branch),
            ),
            (
                "repo-scope-all",
                "All",
                scope == LogScope::All,
                pick(LogScope::All),
            ),
        ],
        p,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use maomi_git::parse::status;

    fn parse(raw: &[u8]) -> Status {
        status::parse(raw).unwrap()
    }

    #[test]
    fn entries_land_in_their_groups() {
        // Staged add, modified-and-restaged, worktree-only change, untracked, conflict.
        let raw = b"1 A. N... 000000 100644 100644 0 1 new.rs\0\
1 MM N... 100644 100644 100644 1 2 both.rs\0\
1 .M N... 100644 100644 100644 1 1 work.rs\0\
? notes.md\0\
u UU N... 100644 100644 100644 100644 1 2 3 clash.rs\0";
        let s = parse(raw);
        let names = |g| -> Vec<String> {
            group_entries(&s, g)
                .iter()
                .map(|e| e.path.to_string())
                .collect()
        };
        assert_eq!(names(Group::Staged), ["new.rs", "both.rs"]);
        assert_eq!(names(Group::Changes), ["both.rs", "work.rs", "notes.md"]);
        assert_eq!(names(Group::Conflicts), ["clash.rs"]);
    }

    #[test]
    fn commit_label_falls_back_to_all_when_nothing_is_staged() {
        assert_eq!(commit_label(CommitMode::Staged, 2, 9), "Commit staged (2)");
        assert_eq!(commit_label(CommitMode::Staged, 0, 9), "Commit all (9)");
        assert_eq!(
            commit_label(CommitMode::Push, 0, 3),
            "Commit all and push (3)"
        );
        assert_eq!(commit_label(CommitMode::Amend, 1, 1), "Amend last commit");
    }

    #[test]
    fn ago_rounds_down_per_unit() {
        let now = 10_000_000;
        assert_eq!(ago(now, now - 5), "now");
        assert_eq!(ago(now, now - 300), "5m");
        assert_eq!(ago(now, now - 3 * 3600), "3h");
        assert_eq!(ago(now, now - 2 * 86_400), "2d");
        assert_eq!(ago(now, now - 15 * 86_400), "2w");
        assert_eq!(ago(now, now - 400 * 86_400), "1y");
        assert_eq!(ago(now, now + 50), "now");
    }
}
