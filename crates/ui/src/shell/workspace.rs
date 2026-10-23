//! A window's contents: title bar with tabs, activity rail, sessions dock, split panes and the
//! status bar. Each window has its own workspace and tab set.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnimationExt as _, AnyElement, App, AppContext as _, Axis, Context, Entity, EntityId,
    Focusable as _, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    PathPromptOptions, PromptLevel, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, WindowBounds, div, linear_color_stop, linear_gradient, px,
};
use maomi_config::{PaneLayout, ShellLayout, TabLayout, WindowLayout};
use maomi_term::{PtySession, SessionOptions, Shell, available_shells, default_shell};

use super::agent;
use super::board::{BoardPage, BoardPageEvent, CardDrawer, CloseDrawer};
use super::drawer::DragPreview;
use super::hosts::{self, HostsEvent, HostsPanel};
use super::pane::{Pane, PaneEvent, display_path};
use super::settings_page::SettingsPage;
use super::snippets::{self, SnippetsEvent, SnippetsPanel, TabChoice};
use super::ssh;
use super::tree::{Direction, DropZone, PaneTree};
use super::workbench::{WorkbenchEvent, WorkbenchPanel};
use super::{LaunchOptions, actions::*};
use crate::updates::{self, UpdateState};
use crate::{Mode, Palette, Skin, TerminalViewEvent, Theme, set_theme, settings};
use maomi_workbench::DeepLink;

struct Tab {
    id: u64,
    tree: PaneTree<Entity<Pane>>,
    focused: Entity<Pane>,
}

/// What the left dock shows; each has a rail button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dock {
    Sessions,
    Hosts,
    Snippets,
    Workbench,
}

pub struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    dock_open: bool,
    dock: Dock,
    /// Created the first time the Snippets dock opens.
    snippets_panel: Option<Entity<SnippetsPanel>>,
    /// Created the first time the Hosts dock opens.
    hosts_panel: Option<Entity<HostsPanel>>,
    /// Created the first time the Workbench dock opens.
    workbench_panel: Option<Entity<WorkbenchPanel>>,
    /// A Workbench plan's board replaces the panes (not the dock) while shown.
    show_board: bool,
    board_page: Option<Entity<BoardPage>>,
    /// Floats over the centre; shows nothing until a card is opened.
    card_drawer: Entity<CardDrawer>,
    launch: LaunchOptions,
    next_id: u64,
    /// The user confirmed closing the window although commands were running.
    close_confirmed: bool,
    pane_subscriptions: HashMap<EntityId, [Subscription; 2]>,
    /// The settings page replaces the dock and panes while shown; created on first open.
    show_settings: bool,
    settings_page: Option<Entity<SettingsPage>>,
    /// Tabs (by id) whose section in the Sessions dock is collapsed.
    collapsed: HashSet<u64>,
    _observers: Vec<Subscription>,
}

/// Drag payload of a Sessions dock row; dropping it on another tab's section moves it there.
#[derive(Clone)]
struct DraggedSession {
    pane: Entity<Pane>,
    from_tab: u64,
}

impl Workspace {
    pub fn new(launch: LaunchOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            dock_open: true,
            dock: Dock::Sessions,
            snippets_panel: None,
            hosts_panel: None,
            workbench_panel: None,
            show_board: false,
            board_page: None,
            card_drawer: cx.new(CardDrawer::new),
            launch,
            next_id: 0,
            close_confirmed: false,
            pane_subscriptions: HashMap::new(),
            show_settings: false,
            settings_page: None,
            collapsed: HashSet::new(),
            _observers: Vec::new(),
        };
        this._observers = vec![
            cx.observe_global_in::<settings::Settings>(window, |ws, _, cx| ws.apply_settings(cx)),
            cx.observe_window_appearance(window, |_, _, cx| settings::sync_system_theme(cx)),
            cx.observe_global::<updates::Updates>(|_, cx| cx.notify()),
            cx.observe(&this.card_drawer, |_, _, cx| cx.notify()),
            cx.subscribe(&this.card_drawer, |ws, _, _: &CloseDrawer, cx| {
                ws.set_board_card(None, cx)
            }),
        ];
        if let Some(layout) = this.launch.restore.take() {
            this.restore(layout, window, cx);
        }
        if this.tabs.is_empty() {
            this.new_tab(window, cx);
        }
        // Dev switch: open the first tab with several panes (2 = side by side, 3-4 = grid).
        let extra = this.launch.panes.saturating_sub(1).min(3);
        for i in 0..extra {
            let axis = if i == 0 {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            // 2: [1 | 2], 3: [1 | 2/3], 4: [1/4 | 2/3].
            let target = if i == 2 {
                this.tabs[0].tree.leaves()[0].clone()
            } else {
                this.tabs[0].focused.clone()
            };
            this.split(&target, axis, window, cx);
        }
        if let Some(focused) = this.active_tab().map(|t| t.focused.clone()) {
            this.focus_pane(&focused, window, cx);
        }

        let workspace = cx.entity().downgrade();
        super::register_workspace(window.window_handle(), workspace.clone(), cx);
        window.on_window_should_close(cx, move |window, cx| {
            workspace
                .update(cx, |ws, cx| ws.should_close_window(window, cx))
                .unwrap_or(true)
        });
        this
    }

    /// Rebuilds saved tabs and splits; panes whose folder is gone start in the default one.
    fn restore(&mut self, layout: WindowLayout, window: &mut Window, cx: &mut Context<Self>) {
        self.dock_open = layout.dock_open;
        for tab in layout.tabs {
            let Some(tree) = self.build_tree(tab.root, window, cx) else {
                continue;
            };
            let leaves = tree.leaves();
            self.next_id += 1;
            self.tabs.push(Tab {
                id: self.next_id,
                focused: leaves[tab.focused.min(leaves.len() - 1)].clone(),
                tree,
            });
        }
        self.active = layout.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    fn build_tree(
        &mut self,
        node: PaneLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneTree<Entity<Pane>>> {
        match node {
            PaneLayout::Pane { cwd, drawer, shell } => {
                let cwd = cwd.filter(|dir| dir.is_dir());
                // A shell that was uninstalled since falls back to the default one.
                let shell = shell.filter(|s| s.program.is_file()).map(|s| {
                    Shell::with_args(s.program, s.args.into_iter().map(Into::into).collect())
                });
                let pane = self.spawn_pane(cwd, shell, window, cx)?;
                if let Some(drawer) = drawer {
                    pane.update(cx, |pane, cx| pane.restore_drawer(drawer, window, cx));
                }
                Some(PaneTree::Leaf(pane))
            }
            PaneLayout::Split {
                horizontal,
                children,
            } => {
                let mut children: Vec<_> = children
                    .into_iter()
                    .filter_map(|child| self.build_tree(child, window, cx))
                    .collect();
                if children.len() <= 1 {
                    return children.pop();
                }
                self.next_id += 1;
                Some(PaneTree::Split {
                    id: self.next_id,
                    axis: if horizontal {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    },
                    children,
                })
            }
        }
    }

    /// This window's tabs, splits, pane folders and bounds, for the next start.
    pub(crate) fn snapshot(&self, window: &Window, cx: &App) -> WindowLayout {
        fn node(tree: &PaneTree<Entity<Pane>>, cx: &App) -> PaneLayout {
            match tree {
                PaneTree::Leaf(pane) => PaneLayout::Pane {
                    cwd: pane.read(cx).cwd().map(PathBuf::from),
                    drawer: pane.read(cx).drawer_layout(cx),
                    shell: pane.read(cx).picked_shell().map(|s| ShellLayout {
                        program: s.program.clone(),
                        args: s
                            .args
                            .iter()
                            .map(|a| a.to_string_lossy().into_owned())
                            .collect(),
                    }),
                },
                PaneTree::Split { axis, children, .. } => PaneLayout::Split {
                    horizontal: *axis == Axis::Horizontal,
                    children: children.iter().map(|c| node(c, cx)).collect(),
                },
            }
        }
        let tabs = self
            .tabs
            .iter()
            .map(|tab| TabLayout {
                root: node(&tab.tree, cx),
                focused: tab
                    .tree
                    .leaves()
                    .iter()
                    .position(|p| *p == tab.focused)
                    .unwrap_or(0),
            })
            .collect();
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(b) => (b, false),
            WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, true),
        };
        WindowLayout {
            bounds: Some(maomi_config::WindowBounds {
                x: bounds.origin.x.as_f32(),
                y: bounds.origin.y.as_f32(),
                width: bounds.size.width.as_f32(),
                height: bounds.size.height.as_f32(),
            }),
            maximized,
            dock_open: self.dock_open,
            active_tab: self.active,
            tabs,
        }
    }

    /// A local shell pane in `cwd`; `shell` (picked from the new-tab menu) overrides the
    /// configured default.
    fn spawn_pane(
        &mut self,
        cwd: Option<PathBuf>,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Pane>> {
        let config = settings::config(cx);
        let base = settings::session_options(&config);
        let mut options = SessionOptions {
            cwd: cwd.or_else(|| base.cwd.clone()),
            shell: shell.clone().or(base.shell.clone()),
            ..base
        };
        // Agents started in this pane (Claude Code with the Maomi mod) report their state here.
        options.env.extend(agent::env(&agent::new_pane_id()));
        let session = match PtySession::spawn(options.clone()) {
            Ok(session) => session,
            Err(err) => {
                tracing::error!(%err, "failed to start the shell");
                return None;
            }
        };
        if let Some(cmd) = &self.launch.run {
            session.write(format!("{cmd}\r"));
        }
        let pane = self.add_pane(session, options, window, cx);
        pane.update(cx, |pane, _| pane.set_picked_shell(shell));
        Some(pane)
    }

    /// A pane for the saved host `host_id`; it shows "Connecting…" until the session opens.
    fn spawn_ssh_pane(
        &mut self,
        host_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Pane>> {
        let host = hosts::host(host_id, cx)?;
        let options = settings::session_options(&settings::config(cx));
        let session = ssh::placeholder(&host, options.clone())?;
        let pane = self.add_pane(session, options, window, cx);
        let terminal = pane.read(cx).terminal().clone();
        // The Hosts dot goes back to idle when a connected session ends.
        let id = host.id.clone();
        cx.subscribe(&terminal, move |_, _, event: &TerminalViewEvent, cx| {
            if matches!(event, TerminalViewEvent::Exited(_)) {
                hosts::clear_status(&id, cx);
            }
        })
        .detach();
        ssh::connect(host.id, terminal.downgrade(), window, cx);
        Some(pane)
    }

    /// Wraps a started session in a pane and subscribes to it.
    fn add_pane(
        &mut self,
        session: PtySession,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        let config = settings::config(cx);
        let pane = cx.new(|cx| Pane::new(session, options, window, cx));
        let smooth = self.launch.smooth_scroll;
        pane.read(cx).terminal().clone().update(cx, |view, cx| {
            settings::apply_to_view(view, &config, smooth, cx);
        });

        let events = cx.subscribe_in(&pane, window, |ws, pane, event: &PaneEvent, window, cx| {
            ws.on_pane_event(pane.clone(), event, window, cx)
        });
        let weak = pane.downgrade();
        let focus = cx.on_focus_in(&pane.focus_handle(cx), window, move |ws, window, cx| {
            if let Some(pane) = weak.upgrade() {
                ws.pane_focused(&pane, cx);
                ws.follow_focused_cwd(window, cx);
            }
        });
        self.pane_subscriptions
            .insert(pane.entity_id(), [events, focus]);
        pane
    }

    /// Opens `pane` as a new tab, or beside the focused pane.
    fn place_pane(
        &mut self,
        pane: Entity<Pane>,
        in_split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.focused_pane().filter(|_| in_split);
        match target.and_then(|t| self.tab_of(&t).map(|tab| (t, tab))) {
            Some((target, tab)) => {
                let mut next_id = self.next_id;
                self.tabs[tab]
                    .tree
                    .split(&target, Axis::Horizontal, pane.clone(), &mut next_id);
                self.next_id = next_id;
            }
            None => {
                self.next_id += 1;
                self.tabs.push(Tab {
                    id: self.next_id,
                    tree: PaneTree::Leaf(pane.clone()),
                    focused: pane.clone(),
                });
                self.active = self.tabs.len() - 1;
            }
        }
        self.focus_pane(&pane, window, cx);
        cx.notify();
    }

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab_in(None, None, window, cx);
    }

    /// The new-tab button and its shell menu (`shell`: None = the default shell): asks for the
    /// folder first when Settings say so. Cancelling still opens the tab, in the startup folder.
    fn new_tab_from_button(
        &mut self,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !settings::config(cx).shell.pick_folder_for_new_tab {
            self.new_tab_in(None, shell, window, cx);
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open in this folder".into()),
        });
        cx.spawn_in(window, async move |ws, cx| {
            let dir = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            _ = ws.update_in(cx, |ws, window, cx| ws.new_tab_in(dir, shell, window, cx));
        })
        .detach();
    }

    fn new_tab_in(
        &mut self,
        cwd: Option<PathBuf>,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.spawn_pane(cwd, shell, window, cx) else {
            return;
        };
        self.next_id += 1;
        self.tabs.push(Tab {
            id: self.next_id,
            tree: PaneTree::Leaf(pane.clone()),
            focused: pane.clone(),
        });
        self.active = self.tabs.len() - 1;
        self.focus_pane(&pane, window, cx);
        cx.notify();
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn focused_pane(&self) -> Option<Entity<Pane>> {
        self.active_tab().map(|t| t.focused.clone())
    }

    fn tab_of(&self, pane: &Entity<Pane>) -> Option<usize> {
        self.tabs.iter().position(|t| t.tree.contains(pane))
    }

    fn split(
        &mut self,
        target: &Entity<Pane>,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tab_of(target) else {
            return;
        };
        // New panes start where the pane they split from is, in the same shell.
        let cwd = target.read(cx).cwd().map(PathBuf::from);
        let shell = target.read(cx).picked_shell().cloned();
        let Some(pane) = self.spawn_pane(cwd, shell, window, cx) else {
            return;
        };
        let mut next_id = self.next_id;
        self.tabs[tab]
            .tree
            .split(target, axis, pane.clone(), &mut next_id);
        self.next_id = next_id;
        self.focus_pane(&pane, window, cx);
        cx.notify();
    }

    /// Moves `moving` onto `target` (beside it, or swapped for the centre zone); both must be
    /// in the same tab.
    fn move_pane(
        &mut self,
        moving: &Entity<Pane>,
        target: &Entity<Pane>,
        zone: DropZone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tab_of(target) else {
            return;
        };
        let mut next_id = self.next_id;
        if self.tabs[tab]
            .tree
            .move_leaf(moving, target, zone, &mut next_id)
        {
            self.next_id = next_id;
            self.focus_pane(moving, window, cx);
            cx.notify();
        }
    }

    /// Pushes the current config into every pane of this window.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let config = settings::config(cx);
        let smooth = self.launch.smooth_scroll;
        for tab in &self.tabs {
            for pane in tab.tree.leaves() {
                pane.read(cx).terminal().clone().update(cx, |view, cx| {
                    settings::apply_to_view(view, &config, smooth, cx);
                });
            }
        }
        cx.notify();
    }

    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_settings {
            if let Some(pane) = self.focused_pane() {
                self.focus_pane(&pane, window, cx);
            }
            return;
        }
        let page = self
            .settings_page
            .get_or_insert_with(|| cx.new(|cx| SettingsPage::new(window, cx)))
            .clone();
        self.show_settings = true;
        // Keys go to the page, not the hidden terminal.
        page.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// A rail button: leaves Settings and shows `dock`; clicking the shown dock's button again
    /// collapses it.
    fn show_dock(&mut self, dock: Dock, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_settings {
            self.toggle_settings(window, cx);
            self.dock_open = true;
        } else if self.dock == dock {
            self.dock_open = !self.dock_open;
        } else {
            self.dock_open = true;
        }
        self.dock = dock;
        if dock == Dock::Snippets && self.snippets_panel.is_none() {
            let panel = cx.new(|cx| SnippetsPanel::new(window, cx));
            let events = cx.subscribe_in(&panel, window, |ws, _, event, window, cx| {
                ws.on_snippets_event(event, window, cx)
            });
            self._observers.push(events);
            self.snippets_panel = Some(panel);
        }
        if dock == Dock::Hosts && self.hosts_panel.is_none() {
            let panel = cx.new(|cx| HostsPanel::new(window, cx));
            let events = cx.subscribe_in(&panel, window, |ws, _, event, window, cx| {
                ws.on_hosts_event(event, window, cx)
            });
            self._observers.push(events);
            self.hosts_panel = Some(panel);
        }
        if dock == Dock::Workbench && self.workbench_panel.is_none() {
            let start_dir = self.focused_cwd(cx);
            let panel = cx.new(|cx| WorkbenchPanel::new(start_dir, window, cx));
            let events = cx.subscribe_in(&panel, window, |ws, _, event, window, cx| {
                ws.on_workbench_event(event, window, cx)
            });
            self._observers.push(events);
            self.workbench_panel = Some(panel);
        }
        if dock == Dock::Workbench {
            self.follow_focused_cwd(window, cx);
        }
        cx.notify();
    }

    fn on_workbench_event(
        &mut self,
        event: &WorkbenchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorkbenchEvent::OpenBoard(plan) => self.open_board(plan.clone(), window, cx),
            WorkbenchEvent::OpenCard { plan, card } => {
                let (plan, card) = (plan.clone(), card.clone());
                self.set_board_card(Some(card.clone()), cx);
                self.card_drawer
                    .update(cx, |drawer, cx| drawer.show(plan, card, cx));
            }
            WorkbenchEvent::Collapse => {
                self.dock_open = false;
                cx.notify();
            }
        }
    }

    /// Shows a plan's board in the centre. Clicking a tab goes back to the terminals.
    fn open_board(&mut self, plan: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match &self.board_page {
            Some(page) => page.update(cx, |page, cx| page.set_plan(plan, cx)),
            None => {
                let page = cx.new(|cx| BoardPage::new(plan, cx));
                let events = cx.subscribe_in(&page, window, |ws, page, event, _, cx| match event {
                    BoardPageEvent::OpenCard(card) => {
                        let plan = page.read(cx).plan().to_owned();
                        let card = card.clone();
                        ws.set_board_card(Some(card.clone()), cx);
                        ws.card_drawer
                            .update(cx, |drawer, cx| drawer.show(plan, card, cx));
                    }
                    BoardPageEvent::CloseCard => {
                        ws.card_drawer.update(cx, |drawer, cx| drawer.close(cx));
                        ws.set_board_card(None, cx);
                    }
                });
                self._observers.push(events);
                self.board_page = Some(page);
            }
        }
        if let Some(open) = self.card_drawer.read(cx).card().map(str::to_owned) {
            self.set_board_card(Some(open), cx);
        }
        self.show_board = true;
        cx.notify();
    }

    fn focused_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.focused_pane()
            .and_then(|pane| pane.read(cx).cwd().map(Path::to_path_buf))
    }

    /// Points the Workbench dock at the focused terminal's project, when that project has a board.
    fn follow_focused_cwd(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(panel), Some(dir)) = (self.workbench_panel.clone(), self.focused_cwd(cx)) else {
            return;
        };
        panel.update(cx, |panel, cx| panel.follow_dir(dir, window, cx));
    }

    /// A Ctrl+clicked `maomi:` link. Board links open here; other valid links go to the OS (the
    /// Workbench desktop app), and so does a plan this app cannot find. An invalid link does
    /// nothing: links are untrusted input.
    fn open_deep_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(link) = maomi_workbench::parse_desktop_link(url) else {
            tracing::warn!("ignored an invalid maomi link");
            return;
        };
        let DeepLink::Plan {
            project,
            plan,
            card,
        } = link
        else {
            cx.open_url(url);
            return;
        };
        let url = url.to_owned();
        let find = cx.background_executor().spawn(async move {
            maomi_workbench::projects()
                .into_iter()
                .find(|found| found.key == project)
                .map(|found| found.board_dir.join(format!("{plan}.json")))
                .filter(|path| path.is_file())
        });
        cx.spawn_in(window, async move |ws, cx| {
            let found = find.await;
            ws.update_in(cx, |ws, window, cx| match found {
                Some(path) => ws.show_plan(path, card, window, cx),
                None => cx.open_url(&url),
            })
            .ok();
        })
        .detach();
    }

    /// Opens a plan's board in the centre and, with `card`, that card's drawer.
    fn show_plan(
        &mut self,
        plan: PathBuf,
        card: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_board(plan.clone(), window, cx);
        match card {
            Some(card) => {
                self.set_board_card(Some(card.clone()), cx);
                self.card_drawer
                    .update(cx, |drawer, cx| drawer.show(plan, card, cx));
            }
            None => {
                self.card_drawer.update(cx, |drawer, cx| drawer.close(cx));
                self.set_board_card(None, cx);
            }
        }
    }

    /// Outlines the card the drawer shows on the board.
    fn set_board_card(&mut self, card: Option<String>, cx: &mut Context<Self>) {
        if let Some(page) = &self.board_page {
            page.update(cx, |page, cx| page.set_open_card(card, cx));
        }
    }

    fn on_hosts_event(&mut self, event: &HostsEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (id, in_split) = match event {
            HostsEvent::Connect(id) => (id, false),
            HostsEvent::ConnectInSplit(id) => (id, true),
            HostsEvent::Collapse => {
                self.dock_open = false;
                cx.notify();
                return;
            }
        };
        if let Some(pane) = self.spawn_ssh_pane(id, window, cx) {
            self.place_pane(pane, in_split, window, cx);
        }
    }

    fn on_snippets_event(
        &mut self,
        event: &SnippetsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SnippetsEvent::RunInFocused(command) => {
                if let Some(pane) = self.focused_pane() {
                    pane.read(cx).terminal().read(cx).run_command(command);
                    self.focus_pane(&pane, window, cx);
                }
            }
            SnippetsEvent::RunOnTabs(command) => {
                let choices = self
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, tab)| {
                        let pane = tab.focused.read(cx);
                        let mut detail = pane.shell_name().to_owned();
                        if i == self.active {
                            detail.push_str(" \u{b7} current tab");
                        }
                        TabChoice {
                            label: pane.title().into(),
                            detail: detail.into(),
                            busy: pane.is_busy(cx),
                            checked: i == self.active,
                            terminal: pane.terminal().downgrade(),
                        }
                    })
                    .collect();
                snippets::open_tab_picker(command.clone(), choices, window, cx);
            }
            SnippetsEvent::Collapse => {
                self.dock_open = false;
                cx.notify();
            }
        }
    }

    /// Focusing a pane always shows the terminals (tab clicks, new tabs, splits).
    fn focus_pane(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = false;
        self.show_board = false;
        if let Some(tab) = self.tab_of(pane) {
            self.active = tab;
            self.tabs[tab].focused = pane.clone();
        }
        // A detached pane takes keys in its own window; here it is only a placeholder.
        if !pane.read(cx).is_detached() {
            pane.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn pane_focused(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab_of(pane)
            && self.tabs[tab].focused != *pane
        {
            self.tabs[tab].focused = pane.clone();
            cx.notify();
        }
    }

    fn on_pane_event(
        &mut self,
        pane: Entity<Pane>,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneEvent::Changed => {
                // A `cd` in the focused terminal can move the Workbench to another project.
                if self.focused_pane().as_ref() == Some(&pane) {
                    self.follow_focused_cwd(window, cx);
                }
                cx.notify()
            }
            PaneEvent::SplitRight => self.split(&pane, Axis::Horizontal, window, cx),
            PaneEvent::SplitDown => self.split(&pane, Axis::Vertical, window, cx),
            PaneEvent::PaneDropped(moving, zone) => {
                self.move_pane(moving, &pane, *zone, window, cx)
            }
            // A detached pane's own window asks and then reports `Closed`.
            PaneEvent::Close if pane.read(cx).is_detached() => {}
            PaneEvent::Close => self.close_pane(pane, false, window, cx),
            PaneEvent::Closed => self.close_pane(pane, true, window, cx),
            PaneEvent::Detach => self.detach_pane(pane, window, cx),
            PaneEvent::Reattach => self.reattach_pane(pane, window, cx),
            PaneEvent::ExitedCleanly => self.close_pane(pane, true, window, cx),
            PaneEvent::OpenDeepLink(url) => self.open_deep_link(url, window, cx),
        }
    }

    /// Closes `pane`, asking first when a command is running (unless `confirmed`).
    fn close_pane(
        &mut self,
        pane: Entity<Pane>,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !confirmed && pane.read(cx).is_busy(cx) {
            let shell = pane.read(cx).shell_name().to_owned();
            self.confirm(
                "Close this pane?",
                &format!("A command is still running in {shell}. Closing the pane ends it."),
                move |ws, window, cx| ws.close_pane(pane.clone(), true, window, cx),
                window,
                cx,
            );
            return;
        }
        let detached = pane.read(cx).detached_window();
        self.take_pane(&pane, window, cx);
        if let Some(detached) = detached {
            _ = detached.update(cx, |_, window, _| window.remove_window());
        }
    }

    /// Moves `pane` into a standalone window of its own, with its shell still running.
    /// The pane stays in its tab, which shows a "Window detached" placeholder, so it can come
    /// back with [`Self::reattach_pane`].
    fn detach_pane(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab_of(&pane) else {
            return;
        };
        super::standalone::open(pane.clone(), cx.entity().downgrade(), cx);
        // Keys go to another pane of the tab, if it has one that is still here.
        let other = self.tabs[tab]
            .tree
            .leaves()
            .into_iter()
            .find(|p| *p != pane && !p.read(cx).is_detached());
        if let Some(other) = other {
            self.focus_pane(&other, window, cx);
        }
        cx.notify();
    }

    /// Takes a detached pane back into its tab and closes its window.
    fn reattach_pane(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detached) = pane.read(cx).detached_window() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.rehome(window, cx);
            pane.set_detached(None, cx);
            pane.set_backdrop_opacity(1., cx);
            pane.set_text_outline(false, cx);
        });
        _ = detached.update(cx, |_, window, _| window.remove_window());
        window.activate_window();
        self.focus_pane(&pane, window, cx);
    }

    /// Focuses `pane`; a detached one brings its window to the front instead.
    fn reveal_pane(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_pane(pane, window, cx);
        if let Some(detached) = pane.read(cx).detached_window() {
            _ = detached.update(cx, |_, window, _| window.activate_window());
        }
    }

    /// Takes `pane` out of its tab; the tab (or this window) closes when it was the last one.
    /// The pane lives on while something else holds it.
    fn take_pane(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab_of(pane) else {
            return;
        };
        self.pane_subscriptions.remove(&pane.entity_id());
        if self.tabs[tab].tree.remove(pane) {
            let next = self.tabs[tab].tree.leaves()[0].clone();
            self.tabs[tab].focused = next.clone();
            if tab == self.active {
                self.focus_pane(&next, window, cx);
            }
        } else {
            self.remove_tab(tab, window, cx);
        }
        cx.notify();
    }

    /// Moves `pane` (dropped on a tab in the Sessions dock) beside the focused pane of tab
    /// `to`. The tab it leaves closes when it was its last pane.
    fn move_pane_to_tab(
        &mut self,
        pane: &Entity<Pane>,
        to: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.tab_of(pane) else {
            return;
        };
        if self.tabs[from].id == to || !self.tabs.iter().any(|t| t.id == to) {
            return;
        }
        if self.tabs[from].tree.remove(pane) {
            self.tabs[from].focused = self.tabs[from].tree.leaves()[0].clone();
        } else {
            // Focusing the moved pane below sets `active` again.
            self.tabs.remove(from);
        }
        let Some(target) = self.tabs.iter().position(|t| t.id == to) else {
            return;
        };
        let beside = self.tabs[target].focused.clone();
        let mut next_id = self.next_id;
        self.tabs[target]
            .tree
            .split(&beside, Axis::Horizontal, pane.clone(), &mut next_id);
        self.next_id = next_id;
        self.focus_pane(pane, window, cx);
    }

    /// Closes a whole tab, asking first when any of its panes runs a command.
    fn close_tab(
        &mut self,
        tab: usize,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(t) = self.tabs.get(tab) else { return };
        let id = t.id;
        if !confirmed && t.tree.leaves().iter().any(|p| p.read(cx).is_busy(cx)) {
            self.confirm(
                "Close this tab?",
                "A command is still running in this tab. Closing it ends the command.",
                move |ws, window, cx| {
                    if let Some(tab) = ws.tabs.iter().position(|t| t.id == id) {
                        ws.close_tab(tab, true, window, cx);
                    }
                },
                window,
                cx,
            );
            return;
        }
        for pane in t.tree.leaves() {
            self.pane_subscriptions.remove(&pane.entity_id());
            // The tab's detached panes end with it.
            if let Some(detached) = pane.read(cx).detached_window() {
                _ = detached.update(cx, |_, window, _| window.remove_window());
            }
        }
        self.remove_tab(tab, window, cx);
    }

    fn remove_tab(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(tab);
        if self.tabs.is_empty() {
            // Last tab gone: the window goes too (its panes have already been confirmed). When it
            // is the last window the user closed everything, so the next start opens fresh.
            if cx.windows().len() == 1 {
                super::save_layout(Vec::new());
            }
            self.close_confirmed = true;
            window.remove_window();
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        if tab < self.active {
            self.active -= 1;
        }
        let focused = self.tabs[self.active].focused.clone();
        self.focus_pane(&focused, window, cx);
    }

    fn activate_tab(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focused) = self.tabs.get(tab).map(|t| t.focused.clone()) {
            self.focus_pane(&focused, window, cx);
        }
    }

    fn move_focus(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        if let Some(next) = tab.tree.neighbour(&tab.focused, direction) {
            self.focus_pane(&next, window, cx);
        }
    }

    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        let next = (self.active as isize + step).rem_euclid(len) as usize;
        self.activate_tab(next, window, cx);
    }

    /// Window close button / Alt+F4: ask once when commands are running.
    fn should_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_confirmed {
            return true;
        }
        // The last window closing ends the app: remember it for the next start. Saved before the
        // busy prompt, so a cancelled close only leaves a slightly early save behind.
        if cx.windows().len() == 1 {
            super::save_layout(vec![self.snapshot(window, cx)]);
        }
        // Detached panes live on in their own windows.
        let busy = self
            .tabs
            .iter()
            .flat_map(|t| t.tree.leaves())
            .any(|p| !p.read(cx).is_detached() && p.read(cx).is_busy(cx));
        if !busy {
            return true;
        }
        self.confirm(
            "Close this window?",
            "Commands are still running in this window. Closing it ends them.",
            |ws, window, _| {
                ws.close_confirmed = true;
                window.remove_window();
            },
            window,
            cx,
        );
        false
    }

    /// Native confirmation dialog; runs `on_confirm` when the user picks Close.
    fn confirm(
        &mut self,
        message: &str,
        detail: &str,
        on_confirm: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            message,
            Some(detail),
            &["Close", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |ws, window, cx| on_confirm(ws, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn render_title_bar(
        &self,
        theme: Theme,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(i, tab)| {
            // Like Settings, a board in the centre leaves no tab active.
            let on = i == self.active && !self.show_board;
            let title: SharedString = tab.focused.read(cx).title().into();
            let id = tab.id;
            let (hover_bg, emph, x_hover) = (
                p.surface_2.to_hsla(),
                p.fg_emph.to_hsla(),
                p.surface_3.to_hsla(),
            );
            div()
                .id(("tab", id as usize))
                // Occluding hides the title bar's drag area under the tab, so Windows sends the
                // click to us instead of treating it as a window drag (HTCAPTION).
                .occlude()
                .flex()
                .items_center()
                .gap(px(7.))
                .flex_none()
                .max_w(px(210.))
                .h(px(28.))
                .pl(px(11.))
                .pr(px(6.))
                .rounded(px(8.))
                .border_1()
                .text_size(px(12.))
                .map(|d| {
                    if on {
                        d.text_color(emph)
                            .bg(p.surface.to_hsla())
                            .border_color(p.line_strong.to_hsla())
                    } else {
                        d.text_color(p.muted.to_hsla())
                            .border_color(gpui_kit::transparent_black())
                            .hover(move |s| s.bg(hover_bg).text_color(emph))
                    }
                })
                .tooltip({
                    let title = title.clone();
                    move |window, cx| Tooltip::new(title.clone()).build(window, cx)
                })
                .on_click(cx.listener(move |ws, _, window, cx| {
                    if let Some(i) = ws.tabs.iter().position(|t| t.id == id) {
                        ws.activate_tab(i, window, cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |ws, _, window, cx| {
                        if let Some(i) = ws.tabs.iter().position(|t| t.id == id) {
                            ws.close_tab(i, false, window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(title),
                )
                .child(
                    div()
                        .id(("tab-close", id as usize))
                        .occlude()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.))
                        .rounded(px(4.))
                        .text_color(p.muted.to_hsla())
                        .hover(move |s| s.bg(x_hover).text_color(emph))
                        .on_click(cx.listener(move |ws, _, window, cx| {
                            cx.stop_propagation();
                            if let Some(i) = ws.tabs.iter().position(|t| t.id == id) {
                                ws.close_tab(i, false, window, cx);
                            }
                        }))
                        .child(Icon::new(IconName::Close).size(px(12.))),
                )
        });

        let skins = [Skin::Aurora, Skin::Onyx, Skin::Mochi].map(|skin| {
            let on = theme.skin == skin;
            let emph = p.fg_emph.to_hsla();
            div()
                .id(skin.name())
                .occlude()
                .px(px(11.))
                .py(px(4.))
                .rounded(px(7.))
                .text_size(px(12.))
                .map(|d| {
                    if on {
                        d.text_color(emph).bg(p.surface.to_hsla())
                    } else {
                        d.text_color(p.muted.to_hsla())
                            .hover(move |s| s.text_color(emph))
                    }
                })
                .on_click(move |_, _, cx| {
                    let current = Theme::current(cx);
                    set_theme(cx, Theme::new(skin, current.mode));
                })
                .child(skin.name())
        });
        // Shown while a downloaded update waits; a click restarts into it.
        let update_pill = match updates::state(cx) {
            UpdateState::Ready(version) => Some(
                div()
                    .id("update-pill")
                    .occlude()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(9.))
                    .rounded(px(12.))
                    .text_size(px(12.))
                    .text_color(p.accent.to_hsla())
                    .bg(p.accent_soft.to_hsla())
                    .border_1()
                    .border_color(p.accent.to_hsla())
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        Tooltip::new(format!(
                            "v{version} is ready. Click to restart and install."
                        ))
                        .build(window, cx)
                    })
                    .on_click(|_, _, cx| updates::restart(cx))
                    .child(Icon::new(IconName::ArrowUp).size(px(13.)))
                    .child("Update"),
            ),
            _ => None,
        };
        let mode_enabled = theme.mode_toggle_enabled();
        let mode_icon = if theme.effective_mode() == Mode::Dark {
            IconName::Moon
        } else {
            IconName::Sun
        };

        TitleBar::new().child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .size_full()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .flex_none()
                        .mr(px(8.))
                        .text_size(px(12.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(p.fg_emph.to_hsla())
                        .child(div().size(px(10.)).rounded(px(3.)).bg(linear_gradient(
                            90.,
                            linear_color_stop(p.gradient[0].to_hsla(), 0.),
                            linear_color_stop(p.gradient[3].to_hsla(), 1.),
                        )))
                        .child("MaomiTerminal"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .min_w_0()
                        .overflow_hidden()
                        .children(tabs),
                )
                .child(
                    title_button("new-tab", IconName::Plus, "New tab (Ctrl+Shift+T)", p).on_click(
                        cx.listener(|ws, _, window, cx| ws.new_tab_from_button(None, window, cx)),
                    ),
                )
                .child(self.render_shell_menu(cx))
                .child(div().flex_1().min_w(px(8.)))
                .when_some(update_pill, |d, pill| d.child(pill))
                .child(
                    div()
                        .flex()
                        .p(px(3.))
                        .gap(px(2.))
                        .rounded(px(10.))
                        .bg(p.surface_2.to_hsla())
                        .border_1()
                        .border_color(p.line.to_hsla())
                        .children(skins),
                )
                .child(
                    title_button(
                        "mode",
                        mode_icon,
                        if mode_enabled {
                            "Toggle dark / light (Ctrl+Shift+L)"
                        } else {
                            "Mochi is dark only"
                        },
                        p,
                    )
                    .when(!mode_enabled, |d| d.opacity(0.4))
                    .on_click(move |_, _, cx| {
                        let theme = Theme::current(cx);
                        if theme.mode_toggle_enabled() {
                            set_theme(cx, theme.with_toggled_mode());
                        }
                    }),
                )
                .child(div().w(px(6.))),
        )
    }

    /// The ▾ beside the new-tab button: a new tab in any shell found on the machine. The list
    /// is read when the menu opens; the default shell is ticked.
    fn render_shell_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ws = cx.entity().downgrade();
        div().occlude().child(
            Button::new("new-tab-shell")
                .ghost()
                .xsmall()
                .icon(IconName::ChevronDown)
                .tooltip("New tab in another shell")
                .dropdown_menu(move |menu, _, cx| {
                    let default = settings::session_options(&settings::config(cx))
                        .shell
                        .unwrap_or_else(default_shell);
                    let mut menu = menu;
                    for shell in available_shells() {
                        let checked =
                            shell.program == default.program && shell.args == default.args;
                        let ws = ws.clone();
                        let label = shell.display_name();
                        menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                            move |_, window, cx| {
                                let shell = Some(shell.clone());
                                _ = ws
                                    .update(cx, |ws, cx| ws.new_tab_from_button(shell, window, cx));
                            },
                        ));
                    }
                    let ws = ws.clone();
                    menu.separator()
                        .item(PopupMenuItem::new("Default shell: Settings…").on_click(
                            move |_, window, cx| {
                                _ = ws.update(cx, |ws, cx| {
                                    if !ws.show_settings {
                                        ws.toggle_settings(window, cx);
                                    }
                                });
                            },
                        ))
                }),
        )
    }

    fn render_rail(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = p.accent.to_hsla();
        let rail_button =
            |id: &'static str, icon: Icon, tooltip: &'static str, on: bool, enabled: bool| {
                let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
                div()
                    .id(id)
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(40.))
                    .text_color(if on { accent } else { p.muted.to_hsla() })
                    .when(enabled && !on, |d| {
                        d.hover(move |s| s.bg(hover).text_color(emph))
                    })
                    .when(!enabled, |d| d.opacity(0.5))
                    .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
                    .when(on, |d| {
                        d.child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(8.))
                                .bottom(px(8.))
                                .w(px(3.))
                                .rounded_r(px(3.))
                                .bg(accent),
                        )
                    })
                    .child(icon.size(px(20.)))
            };
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .flex_none()
            .w(px(44.))
            .py(px(8.))
            .bg(p.bg.to_hsla())
            .border_r_1()
            .border_color(p.line.to_hsla())
            .child(
                rail_button(
                    "rail-terminals",
                    Icon::new(IconName::SquareTerminal),
                    "Terminals",
                    !self.show_settings && self.dock == Dock::Sessions,
                    true,
                )
                .on_click(
                    cx.listener(|ws, _, window, cx| ws.show_dock(Dock::Sessions, window, cx)),
                ),
            )
            .child(
                rail_button(
                    "rail-hosts",
                    Icon::new(IconName::HardDrive),
                    "Hosts",
                    !self.show_settings && self.dock == Dock::Hosts,
                    true,
                )
                .on_click(cx.listener(|ws, _, window, cx| ws.show_dock(Dock::Hosts, window, cx))),
            )
            .child(
                rail_button(
                    "rail-snippets",
                    Icon::new(gpui_kit::assets::IconName::SquareCode),
                    "Snippets",
                    !self.show_settings && self.dock == Dock::Snippets,
                    true,
                )
                .on_click(
                    cx.listener(|ws, _, window, cx| ws.show_dock(Dock::Snippets, window, cx)),
                ),
            )
            .child(
                rail_button(
                    "rail-workbench",
                    Icon::new(gpui_kit::assets::IconName::SquareKanban),
                    "Workbench boards",
                    !self.show_settings && self.dock == Dock::Workbench,
                    true,
                )
                .on_click(
                    cx.listener(|ws, _, window, cx| ws.show_dock(Dock::Workbench, window, cx)),
                ),
            )
            .child(div().flex_1())
            .child(
                rail_button(
                    "rail-settings",
                    Icon::new(IconName::Settings),
                    if cfg!(target_os = "macos") {
                        "Settings (Cmd+,)"
                    } else {
                        "Settings (Ctrl+,)"
                    },
                    self.show_settings,
                    true,
                )
                .on_click(cx.listener(|ws, _, window, cx| ws.toggle_settings(window, cx))),
            )
    }

    fn render_dock(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let several = self.tabs.len() > 1;
        let sections: Vec<AnyElement> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| self.render_tab_section(i, tab, p, cx))
            .collect();

        let section_title = |text: &'static str| {
            div()
                .text_size(px(12.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(p.muted.to_hsla())
                .child(text)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .flex_none()
            .w(px(240.))
            .px(px(10.))
            .py(px(12.))
            .overflow_hidden()
            .bg(p.bg.to_hsla())
            .border_r_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(4.))
                            .child(section_title("SESSIONS"))
                            .child(
                                title_button(
                                    "dock-collapse",
                                    IconName::PanelLeftClose,
                                    "Collapse dock",
                                    p,
                                )
                                .size(px(24.))
                                .on_click(cx.listener(
                                    |ws, _, _, cx| {
                                        ws.dock_open = false;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .mt(px(6.))
                            .children(sections),
                    )
                    .when(several, |d| {
                        d.child(
                            div()
                                .mt(px(10.))
                                .px(px(4.))
                                .text_size(px(11.))
                                .text_color(p.muted.to_hsla())
                                .child("Drag a session onto another tab to move it there."),
                        )
                    }),
            )
    }

    /// One tab's collapsible part of the Sessions dock; it takes sessions dropped from the
    /// other tabs.
    fn render_tab_section(
        &self,
        index: usize,
        tab: &Tab,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tab.id;
        let active = index == self.active;
        let collapsed = self.collapsed.contains(&id);
        let panes = tab.tree.leaves();
        let title: SharedString = tab.focused.read(cx).title().into();
        let (hover, tint, ring) = (
            p.surface_2.to_hsla(),
            p.accent_soft.to_hsla(),
            p.accent.to_hsla(),
        );
        let header = div()
            .id(("tab-section", id as usize))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(4.))
            .py(px(4.))
            .rounded(px(7.))
            .cursor_pointer()
            .text_size(px(11.5))
            .text_color(p.muted.to_hsla())
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |ws, _, _, cx| {
                if !ws.collapsed.remove(&id) {
                    ws.collapsed.insert(id);
                }
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(if active { p.fg_emph } else { p.fg }.to_hsla())
                    .child(title),
            )
            .child(div().flex_none().child(panes.len().to_string()));

        let focused = self.focused_pane();
        let rows = (!collapsed).then(|| {
            panes
                .into_iter()
                .map(|pane| {
                    let selected = focused.as_ref() == Some(&pane);
                    self.render_session_row(pane, id, selected, p, cx)
                })
                .collect::<Vec<_>>()
        });
        div()
            .id(("tab-drop", id as usize))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(9.))
            .border_1()
            .border_color(gpui_kit::transparent_black())
            .drag_over::<DraggedSession>(move |style, dragged, _, _| {
                if dragged.from_tab == id {
                    style
                } else {
                    style.bg(tint).border_color(ring)
                }
            })
            .on_drop(
                cx.listener(move |ws, dragged: &DraggedSession, window, cx| {
                    ws.move_pane_to_tab(&dragged.pane, id, window, cx)
                }),
            )
            .child(header)
            .children(rows.into_iter().flatten())
            .into_any_element()
    }

    /// A session in the dock: its status dot matches the pane header's (running colour while a
    /// command runs, green at the prompt, grey once exited).
    fn render_session_row(
        &self,
        pane: Entity<Pane>,
        tab: u64,
        selected: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let data = pane.read(cx);
        let hover = p.surface_2.to_hsla();
        let agent = data.agent_status().filter(|_| !data.has_exited());
        let (dot, detail, detail_color) = if data.has_exited() {
            (p.muted, "exited".to_owned(), p.muted)
        } else if agent.is_none() && data.is_detached() {
            (p.ok, "in its own window".to_owned(), p.muted)
        } else if let Some(agent) = &agent {
            let color = agent::color(agent, p);
            (color, agent.label(), color)
        } else if data.is_running() {
            let command = data.command().unwrap_or("running").to_owned();
            (p.now, command, p.now)
        } else {
            let cwd = data.cwd().map(display_path).unwrap_or_default();
            (p.ok, cwd, p.muted)
        };
        let name: SharedString = data.title().into();
        let shell = data.name().to_owned();
        let two_lines = data.has_program_title();
        let dragged = DraggedSession {
            pane: pane.clone(),
            from_tab: tab,
        };
        let target = pane.downgrade();
        // Blocked on the user (a permission or a question): the row pulses its colour.
        let pulse = agent.as_ref().filter(|a| a.pulses()).map(|_| dot.to_hsla());
        let id = pane.entity_id().as_u64() as usize;
        let row = div()
            .id(("session", id))
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(9.))
            .text_size(px(12.5))
            .text_color(p.fg.to_hsla())
            .when(selected, |d| d.bg(hover))
            .hover(move |s| s.bg(hover))
            .on_drag(dragged, move |_, _, _, cx| {
                let name = name.clone();
                cx.new(|_| DragPreview { name })
            })
            .on_click(cx.listener(move |ws, _, window, cx| {
                if let Some(pane) = target.upgrade() {
                    ws.reveal_pane(&pane, window, cx);
                }
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot.to_hsla()),
            )
            .child(
                div()
                    .flex()
                    // A program's title (Claude's topic) is long: the status goes under it.
                    .when(two_lines, |d| d.flex_col())
                    // The smaller detail text would otherwise sit at the top of the line.
                    .when(!two_lines, |d| d.items_center().gap(px(5.)))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().min_w_0().truncate().child(shell))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(detail_color.to_hsla())
                            .text_ellipsis()
                            .child(detail),
                    ),
            );
        match pulse {
            Some(color) => row
                .with_animation(("session-pulse", id), agent::pulse(), move |row, t| {
                    row.bg(color.opacity(0.06 + 0.2 * t))
                })
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    fn render_tree(
        &self,
        tree: &PaneTree<Entity<Pane>>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match tree {
            PaneTree::Leaf(pane) if pane.read(cx).is_detached() => {
                self.render_detached(pane, p, cx)
            }
            PaneTree::Leaf(pane) => div()
                .size_full()
                .p(px(2.))
                .child(pane.clone())
                .into_any_element(),
            PaneTree::Split { id, axis, children } => {
                let group = match axis {
                    Axis::Horizontal => h_resizable(("split", *id as usize)),
                    Axis::Vertical => v_resizable(("split", *id as usize)),
                };
                group
                    .children(
                        children
                            .iter()
                            .map(|child| resizable_panel().child(self.render_tree(child, p, cx))),
                    )
                    .into_any_element()
            }
        }
    }

    /// Where a detached pane sits in its tab: what it is and how to get it back.
    fn render_detached(
        &self,
        pane: &Entity<Pane>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = pane.entity_id().as_u64() as usize;
        let name = pane.read(cx).name().to_owned();
        let (back, show) = (pane.downgrade(), pane.downgrade());
        div()
            .size_full()
            .p(px(2.))
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.line_strong.to_hsla())
                    .bg(p.surface.to_hsla())
                    .child(
                        Icon::new(gpui_kit::assets::IconName::SquareArrowOutUpRight)
                            .size(px(22.))
                            .text_color(p.muted.to_hsla()),
                    )
                    .child(
                        div()
                            .text_color(p.fg_emph.to_hsla())
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("Window detached"),
                    )
                    .child(
                        div()
                            .max_w(px(320.))
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(p.muted.to_hsla())
                            .child(name),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .mt(px(4.))
                            .child(
                                Button::new(("bring-back", id))
                                    .primary()
                                    .small()
                                    .label("Bring back")
                                    .on_click(cx.listener(move |ws, _, window, cx| {
                                        if let Some(pane) = back.upgrade() {
                                            ws.reattach_pane(pane, window, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new(("show-window", id))
                                    .small()
                                    .label("Show window")
                                    .on_click(cx.listener(move |ws, _, window, cx| {
                                        if let Some(pane) = show.upgrade() {
                                            ws.reveal_pane(&pane, window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_status_bar(
        &self,
        theme: Theme,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let left = self
            .focused_pane()
            .map(|pane| {
                let pane = pane.read(cx);
                let size = pane.terminal().read(cx).session().size();
                format!(
                    "{} \u{b7} {}\u{d7}{}",
                    pane.shell_name(),
                    size.columns,
                    size.rows
                )
            })
            .unwrap_or_default();
        let mode = if theme.effective_mode() == Mode::Dark {
            "Dark"
        } else {
            "Light"
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .flex_none()
            .h(px(24.))
            .px(px(12.))
            .text_size(px(11.5))
            .text_color(p.muted.to_hsla())
            .bg(p.surface.to_hsla())
            .border_t_1()
            .border_color(p.line.to_hsla())
            .child(left)
            .child(format!("{} \u{b7} {mode}", theme.skin.name()))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let p = theme.palette();
        let centre = self
            .active_tab()
            .map(|tab| self.render_tree(&tab.tree, p, cx));

        div()
            .key_context(super::KEY_CONTEXT)
            .on_action(cx.listener(|ws, _: &NewTab, window, cx| ws.new_tab(window, cx)))
            .on_action(cx.listener(|ws, _: &ClosePane, window, cx| {
                if let Some(pane) = ws.focused_pane() {
                    ws.close_pane(pane, false, window, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &SplitRight, window, cx| {
                if let Some(pane) = ws.focused_pane() {
                    ws.split(&pane, Axis::Horizontal, window, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &SplitDown, window, cx| {
                if let Some(pane) = ws.focused_pane() {
                    ws.split(&pane, Axis::Vertical, window, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &FocusLeft, window, cx| {
                ws.move_focus(Direction::Left, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &FocusRight, window, cx| {
                ws.move_focus(Direction::Right, window, cx)
            }))
            .on_action(
                cx.listener(|ws, _: &FocusUp, window, cx| ws.move_focus(Direction::Up, window, cx)),
            )
            .on_action(cx.listener(|ws, _: &FocusDown, window, cx| {
                ws.move_focus(Direction::Down, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &NextTab, window, cx| ws.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|ws, _: &PrevTab, window, cx| ws.cycle_tab(-1, window, cx)))
            .on_action(
                cx.listener(|ws, _: &OpenSettings, window, cx| ws.toggle_settings(window, cx)),
            )
            .on_action(cx.listener(|ws, _: &NewWindow, _, cx| {
                let launch = LaunchOptions {
                    run: None,
                    panes: 1,
                    restore: None,
                    ..ws.launch.clone()
                };
                super::open_window(launch, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg.to_hsla())
            .text_color(p.fg.to_hsla())
            .text_size(px(13.))
            .child(self.render_title_bar(theme, p, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_rail(p, cx))
                    .map(|d| match &self.settings_page {
                        Some(page) if self.show_settings => {
                            d.child(div().flex_1().min_w_0().min_h_0().child(page.clone()))
                        }
                        _ => d
                            .when(self.dock_open, |d| match self.dock {
                                Dock::Snippets if self.snippets_panel.is_some() => {
                                    d.children(self.snippets_panel.clone())
                                }
                                Dock::Hosts if self.hosts_panel.is_some() => {
                                    d.children(self.hosts_panel.clone())
                                }
                                Dock::Workbench if self.workbench_panel.is_some() => {
                                    d.children(self.workbench_panel.clone())
                                }
                                _ => d.child(self.render_dock(p, cx)),
                            })
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .map(|d| match &self.board_page {
                                        Some(page) if self.show_board => d.child(page.clone()),
                                        _ => d.p(px(6.)).children(centre),
                                    })
                                    .when(self.card_drawer.read(cx).is_open(), |d| {
                                        d.child(self.card_drawer.clone())
                                    }),
                            ),
                    }),
            )
            .child(self.render_status_bar(theme, p, cx))
    }
}

fn title_button(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (hover, emph) = (p.surface_2.to_hsla(), p.fg_emph.to_hsla());
    // Occluding keeps clicks away from the title bar's drag area (see the tab buttons).
    div()
        .id(id)
        .occlude()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(28.))
        .rounded(px(8.))
        .text_color(p.muted.to_hsla())
        .hover(move |s| s.bg(hover).text_color(emph))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(Icon::new(icon).size(px(16.)))
}
