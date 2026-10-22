//! A window's contents: title bar with tabs, activity rail, sessions dock, split panes and the
//! status bar. Each window has its own workspace and tab set.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Axis, Context, Entity, EntityId, Focusable as _,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, PathPromptOptions,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, WindowBounds, div, linear_color_stop, linear_gradient, px,
};
use maomi_config::{PaneLayout, TabLayout, WindowLayout};
use maomi_term::{PtySession, SessionOptions};

use super::hosts::{self, HostsEvent, HostsPanel};
use super::pane::{Pane, PaneEvent, display_path};
use super::settings_page::SettingsPage;
use super::snippets::{self, SnippetsEvent, SnippetsPanel, TabChoice};
use super::ssh;
use super::tree::{Direction, DropZone, PaneTree};
use super::{LaunchOptions, actions::*};
use crate::updates::{self, UpdateState};
use crate::{Mode, Palette, Skin, TerminalViewEvent, Theme, set_theme, settings};

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
    launch: LaunchOptions,
    next_id: u64,
    /// The user confirmed closing the window although commands were running.
    close_confirmed: bool,
    pane_subscriptions: HashMap<EntityId, [Subscription; 2]>,
    /// The settings page replaces the dock and panes while shown; created on first open.
    show_settings: bool,
    settings_page: Option<Entity<SettingsPage>>,
    _observers: Vec<Subscription>,
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
            launch,
            next_id: 0,
            close_confirmed: false,
            pane_subscriptions: HashMap::new(),
            show_settings: false,
            settings_page: None,
            _observers: Vec::new(),
        };
        this._observers = vec![
            cx.observe_global_in::<settings::Settings>(window, |ws, _, cx| ws.apply_settings(cx)),
            cx.observe_window_appearance(window, |_, _, cx| settings::sync_system_theme(cx)),
            cx.observe_global::<updates::Updates>(|_, cx| cx.notify()),
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
            PaneLayout::Pane { cwd, drawer } => {
                let cwd = cwd.filter(|dir| dir.is_dir());
                let pane = self.spawn_pane(cwd, window, cx)?;
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

    fn spawn_pane(
        &mut self,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Pane>> {
        let config = settings::config(cx);
        let base = settings::session_options(&config);
        let options = SessionOptions {
            cwd: cwd.or_else(|| base.cwd.clone()),
            ..base
        };
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
        Some(self.add_pane(session, options, window, cx))
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
        let focus = cx.on_focus_in(&pane.focus_handle(cx), window, move |ws, _, cx| {
            if let Some(pane) = weak.upgrade() {
                ws.pane_focused(&pane, cx);
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
        self.new_tab_in(None, window, cx);
    }

    /// The new-tab button: asks for the folder first when Settings say so. Cancelling still
    /// opens the tab, in the startup folder.
    fn new_tab_from_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !settings::config(cx).shell.pick_folder_for_new_tab {
            self.new_tab(window, cx);
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
            _ = ws.update_in(cx, |ws, window, cx| ws.new_tab_in(dir, window, cx));
        })
        .detach();
    }

    fn new_tab_in(&mut self, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.spawn_pane(cwd, window, cx) else {
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
        // New panes start where the pane they split from is.
        let cwd = target.read(cx).cwd().map(PathBuf::from);
        let Some(pane) = self.spawn_pane(cwd, window, cx) else {
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
        cx.notify();
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
        if let Some(tab) = self.tab_of(pane) {
            self.active = tab;
            self.tabs[tab].focused = pane.clone();
        }
        pane.focus_handle(cx).focus(window, cx);
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
            PaneEvent::Changed => cx.notify(),
            PaneEvent::SplitRight => self.split(&pane, Axis::Horizontal, window, cx),
            PaneEvent::SplitDown => self.split(&pane, Axis::Vertical, window, cx),
            PaneEvent::PaneDropped(moving, zone) => {
                self.move_pane(moving, &pane, *zone, window, cx)
            }
            PaneEvent::Close => self.close_pane(pane, false, window, cx),
            PaneEvent::ExitedCleanly => self.close_pane(pane, true, window, cx),
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
        let Some(tab) = self.tab_of(&pane) else {
            return;
        };
        self.pane_subscriptions.remove(&pane.entity_id());
        if self.tabs[tab].tree.remove(&pane) {
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
        let busy = self
            .tabs
            .iter()
            .flat_map(|t| t.tree.leaves())
            .any(|p| p.read(cx).is_busy(cx));
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
            let on = i == self.active;
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
                        cx.listener(|ws, _, window, cx| ws.new_tab_from_button(window, cx)),
                    ),
                )
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
        let focused = self.focused_pane();
        let mut rows: Vec<AnyElement> = Vec::new();
        for tab in &self.tabs {
            for pane in tab.tree.leaves() {
                let data = pane.read(cx);
                let selected = focused.as_ref() == Some(&pane);
                let hover = p.surface_2.to_hsla();
                let dot = if data.has_exited() { p.muted } else { p.ok };
                let sub = data.cwd().map(display_path).unwrap_or_default();
                let target = pane.downgrade();
                rows.push(
                    div()
                        .id(("session", pane.entity_id().as_u64() as usize))
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
                        .on_click(cx.listener(move |ws, _, window, cx| {
                            if let Some(pane) = target.upgrade() {
                                ws.focus_pane(&pane, window, cx);
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
                                .gap(px(5.))
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(data.shell_name().to_owned())
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(p.muted.to_hsla())
                                        .text_ellipsis()
                                        .child(sub),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }

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
                            .gap(px(2.))
                            .mt(px(6.))
                            .children(rows),
                    ),
            )
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
        let centre = self.active_tab().map(|tab| render_tree(&tab.tree));

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
                            .when(self.dock_open, |d| {
                                match (self.dock, &self.snippets_panel, &self.hosts_panel) {
                                    (Dock::Snippets, Some(panel), _) => d.child(panel.clone()),
                                    (Dock::Hosts, _, Some(panel)) => d.child(panel.clone()),
                                    _ => d.child(self.render_dock(p, cx)),
                                }
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .p(px(6.))
                                    .children(centre),
                            ),
                    }),
            )
            .child(self.render_status_bar(theme, p, cx))
    }
}

fn render_tree(tree: &PaneTree<Entity<Pane>>) -> AnyElement {
    match tree {
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
                        .map(|child| resizable_panel().child(render_tree(child))),
                )
                .into_any_element()
        }
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
