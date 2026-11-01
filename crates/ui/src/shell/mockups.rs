//! Mockup Studio: a project's design mockups (`.maomi/mockups`) as a centre page, opened from
//! the Workbench dock. Left: the design system and the pages grouped by their home plan. Right:
//! the chosen page with its linked cards and kept versions. Preview writes the sandboxed preview
//! shell to the cache folder and opens it in the default browser; review happens in the card
//! drawer of the linked card.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, EventEmitter, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};
use maomi_workbench::{
    Column, MockupIndex, MockupPage, MockupStatus, MockupTarget, Project, mockups_dir,
    preview_html, read_mockups,
};

use super::board::{ago, chip, muted_text};
use super::workbench::{WorkbenchStore, column_color, column_label, load_board};
use crate::{Palette, Theme};

/// What the page asks the workspace to do.
pub enum MockupsEvent {
    /// Show a linked card in the card drawer (where Approve / Send back live).
    OpenCard { plan: PathBuf, card: String },
}

/// The pages list's status filter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Review,
    Draft,
    Approved,
    SentBack,
}

impl Filter {
    const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Review,
        Filter::Draft,
        Filter::Approved,
        Filter::SentBack,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Review => "Review",
            Filter::Draft => "Draft",
            Filter::Approved => "Approved",
            Filter::SentBack => "Sent back",
        }
    }

    fn keeps(self, status: MockupStatus) -> bool {
        match self {
            Filter::All => true,
            Filter::Review => status == MockupStatus::Review,
            Filter::Draft => status == MockupStatus::Draft,
            Filter::Approved => status == MockupStatus::Approved,
            Filter::SentBack => status == MockupStatus::SentBack,
        }
    }
}

pub struct MockupsPage {
    project: Project,
    /// None while the first read runs.
    index: Option<MockupIndex>,
    selected: Option<String>,
    /// The version chosen in the detail; None = latest.
    version: Option<u32>,
    filter: Filter,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MockupsEvent> for MockupsPage {}

impl MockupsPage {
    pub fn new(project: Project, select: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            project,
            index: None,
            selected: select,
            version: None,
            filter: Filter::All,
            // Plan and card titles come from the board, which redraws when a session writes it.
            _subscriptions: vec![cx.observe_global::<WorkbenchStore>(|_, cx| cx.notify())],
        };
        page.reload(cx);
        page
    }

    /// Shows another project (or the same one again, re-read) and optionally selects a page.
    pub fn set_project(
        &mut self,
        project: Project,
        select: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if project.key != self.project.key {
            self.index = None;
            self.filter = Filter::All;
        }
        self.project = project;
        if select.is_some() {
            self.selected = select;
            self.version = None;
        }
        self.reload(cx);
    }

    fn dir(&self) -> Option<PathBuf> {
        self.project.path.as_deref().map(mockups_dir)
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        load_board(self.project.board_dir.clone(), cx);
        let Some(dir) = self.dir() else {
            self.index = Some(MockupIndex::default());
            cx.notify();
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { read_mockups(&dir) });
        cx.spawn(async move |page, cx| {
            let index = read.await;
            _ = page.update(cx, |page, cx| {
                if page.selected.is_none() {
                    page.selected = index.pages.first().map(|p| p.id.clone());
                }
                page.index = Some(index);
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes the preview shell to the cache folder and opens it in the default browser.
    fn preview(
        &mut self,
        target: MockupTarget,
        title: String,
        latest: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.dir() else {
            return;
        };
        let name = match &target {
            MockupTarget::Page { id, version } => {
                format!(
                    "{}-{id}-v{}.html",
                    self.project.key,
                    version.or(latest).unwrap_or(0)
                )
            }
            MockupTarget::Theme => format!("{}-system-theme.html", self.project.key),
            MockupTarget::Components => format!("{}-system-components.html", self.project.key),
        };
        let write = cx.background_executor().spawn(async move {
            let html = preview_html(&dir, &target, &title, latest).map_err(|e| e.to_string())?;
            let out = maomi_config::mockup_preview_dir().map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            let file = out.join(sanitize(&name));
            std::fs::write(&file, html).map_err(|e| e.to_string())?;
            Ok::<_, String>(file)
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = write.await;
            _ = cx.update(|window, cx| match result {
                Ok(file) => cx.open_with_system(&file),
                Err(err) => window.push_notification(
                    Notification::error(format!("Could not open the preview: {err}")),
                    cx,
                ),
            });
        })
        .detach();
    }

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.version = None;
        cx.notify();
    }

    fn plan_path(&self, plan: &str) -> PathBuf {
        self.project.board_dir.join(format!("{plan}.json"))
    }

    fn plan_title(&self, plan: &str, cx: &Context<Self>) -> String {
        cx.global::<WorkbenchStore>()
            .plan(&self.plan_path(plan))
            .map(|(_, entry)| entry.plan.title.clone())
            .unwrap_or_else(|| plan.to_owned())
    }
}

impl MockupsPage {
    fn render_header(
        &self,
        index: &MockupIndex,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = |status| {
            index
                .pages
                .iter()
                .filter(|page| page.status == status)
                .count()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(20.))
            .pt(px(16.))
            .pb(px(12.))
            .border_b_1()
            .border_color(p.line.to_hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child("Mockups"),
                    )
                    .child(
                        div()
                            .text_color(p.muted.to_hsla())
                            .child(self.project.name.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("mk-refresh")
                            .small()
                            .label("Refresh")
                            .on_click(cx.listener(|page, _, _, cx| page.reload(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(chip(
                        plural(index.pages.len(), "page"),
                        p.muted.to_hsla(),
                        p,
                    ))
                    .child(chip(
                        format!("{} in review", count(MockupStatus::Review)),
                        status_color(MockupStatus::Review, p),
                        p,
                    ))
                    .child(chip(
                        format!("{} approved", count(MockupStatus::Approved)),
                        status_color(MockupStatus::Approved, p),
                        p,
                    ))
                    .child(chip(
                        format!("{} draft", count(MockupStatus::Draft)),
                        status_color(MockupStatus::Draft, p),
                        p,
                    ))
                    .when(count(MockupStatus::SentBack) > 0, |d| {
                        d.child(chip(
                            format!("{} sent back", count(MockupStatus::SentBack)),
                            status_color(MockupStatus::SentBack, p),
                            p,
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_system(
        &self,
        index: &MockupIndex,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(system) = &index.system else {
            return card(p)
                .child(card_title("Design system", p))
                .child(muted_text(
                    "No design system yet. An agent creates it with mockup_init.",
                    p,
                ))
                .into_any_element();
        };
        let (label, color) = if system.approved {
            ("Approved", p.ok.to_hsla())
        } else {
            ("Not approved", p.warn.to_hsla())
        };
        card(p)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(card_title(&format!("Design system v{}", system.version), p))
                    .child(div().flex_1())
                    .child(chip(label.into(), color, p)),
            )
            .child(small_text(
                format!(
                    "{} \u{b7} {}",
                    plural(system.components.len(), "component"),
                    system.themes.join(" \u{b7} ")
                ),
                p,
            ))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        Button::new("mk-theme")
                            .small()
                            .label("Theme & contrast")
                            .on_click(cx.listener(|page, _, window, cx| {
                                page.preview(MockupTarget::Theme, "Theme".into(), None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("mk-components")
                            .small()
                            .label("Components")
                            .on_click(cx.listener(|page, _, window, cx| {
                                page.preview(
                                    MockupTarget::Components,
                                    "Components".into(),
                                    None,
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_pages(&self, index: &MockupIndex, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        // Groups by home plan, in the order the newest page of each plan appears.
        let mut groups: Vec<(Option<String>, Vec<&MockupPage>)> = Vec::new();
        for page in index
            .pages
            .iter()
            .filter(|page| self.filter.keeps(page.status))
        {
            let home = page.home_plan().map(str::to_owned);
            match groups.iter_mut().find(|(plan, _)| *plan == home) {
                Some((_, pages)) => pages.push(page),
                None => groups.push((home, vec![page])),
            }
        }
        let filters = Filter::ALL.map(|filter| {
            let on = self.filter == filter;
            let (accent, hover) = (p.accent_soft.to_hsla(), p.surface_3.to_hsla());
            div()
                .id(SharedString::from(format!("mk-filter-{}", filter.label())))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(6.))
                .text_size(px(11.5))
                .cursor_pointer()
                .text_color(if on {
                    p.fg_emph.to_hsla()
                } else {
                    p.muted.to_hsla()
                })
                .when(on, |d| d.bg(accent))
                .when(!on, |d| d.hover(move |s| s.bg(hover)))
                .child(filter.label())
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.filter = filter;
                    cx.notify();
                }))
        });
        let mut list = div().flex().flex_col().gap(px(10.));
        if groups.is_empty() {
            list = list.child(muted_text(
                if index.pages.is_empty() {
                    "No mockups yet. Agents save them with mockup_save."
                } else {
                    "No pages with this status."
                },
                p,
            ));
        }
        for (plan, pages) in groups {
            let heading = match &plan {
                Some(plan) => self.plan_title(plan, cx),
                None => "No plan".into(),
            };
            let rows: Vec<AnyElement> = pages
                .into_iter()
                .map(|page| self.render_page_row(page, p, cx))
                .collect();
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .px(px(6.))
                            .pb(px(2.))
                            .text_size(px(11.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.muted.to_hsla())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(heading),
                    )
                    .children(rows),
            );
        }
        card(p)
            .child(card_title("Pages", p))
            .child(div().flex().flex_wrap().gap(px(2.)).children(filters))
            .child(list)
            .into_any_element()
    }

    fn render_page_row(
        &self,
        page: &MockupPage,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on = self.selected.as_deref() == Some(page.id.as_str());
        let (accent, hover) = (p.accent_soft.to_hsla(), p.surface_3.to_hsla());
        let id = page.id.clone();
        div()
            .id(SharedString::from(format!("mk-page-{}", page.id)))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(5.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(on, |d| d.bg(accent))
            .when(!on, |d| d.hover(move |s| s.bg(hover)))
            .when(page.archived, |d| d.opacity(0.6))
            .child(super::board::dot(status_color(page.status, p), 7.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(if on {
                        p.fg_emph.to_hsla()
                    } else {
                        p.fg.to_hsla()
                    })
                    .child(page.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(p.muted.to_hsla())
                    .child(format!("v{}", page.version)),
            )
            .on_click(cx.listener(move |page, _, _, cx| page.select(id.clone(), cx)))
            .into_any_element()
    }

    fn render_detail(&self, page: &MockupPage, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let version = self.version.unwrap_or(page.version);
        let latest = page.version;
        let links: Vec<AnyElement> = page
            .plans
            .iter()
            .enumerate()
            .map(|(i, link)| self.render_link(i, &link.plan, link.milestone.as_deref(), p, cx))
            .collect();
        let versions: Vec<AnyElement> = page
            .versions
            .iter()
            .rev()
            .map(|v| {
                let on = v.v == version;
                let n = v.v;
                let (accent, hover) = (p.accent_soft.to_hsla(), p.surface_3.to_hsla());
                let mut label = format!("v{n}");
                if n == latest {
                    label.push_str(" (latest)");
                }
                if page.approved_version == Some(n) {
                    label.push_str(" \u{2705} approved");
                }
                div()
                    .id(SharedString::from(format!("mk-version-{n}")))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(accent))
                    .when(!on, |d| d.hover(move |s| s.bg(hover)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(p.fg_emph.to_hsla())
                                    .child(label),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.muted.to_hsla())
                                    .child(ago(v.at as i64)),
                            ),
                    )
                    .when_some(v.note.clone(), |d, note| d.child(small_text(note, p)))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.version = Some(n);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let (id, title) = (page.id.clone(), page.title.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(16.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.fg_emph.to_hsla())
                            .child(page.title.clone()),
                    )
                    .child(chip(
                        page.status.label().into(),
                        status_color(page.status, p),
                        p,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("mk-preview")
                            .primary()
                            .small()
                            .label(format!("Preview v{version}"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.preview(
                                    MockupTarget::Page {
                                        id: id.clone(),
                                        version: Some(version),
                                    },
                                    format!("{title} \u{b7} v{version}"),
                                    Some(latest),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .child(small_text(
                format!(
                    "{} \u{b7} updated {} \u{b7} opens in your browser, sandboxed with no network",
                    page.id,
                    ago(page.updated_at as i64)
                ),
                p,
            ))
            .child(card(p).child(card_title("Cards", p)).map(|d| {
                if links.is_empty() {
                    d.child(muted_text("Not linked to a plan.", p))
                } else {
                    d.children(links)
                }
            }))
            .child(card(p).child(card_title("Versions", p)).children(versions))
            .into_any_element()
    }

    /// A plan (and card) using the page. A card waiting in Review gets a Review button that opens
    /// the card drawer with its Approve / Send back box.
    fn render_link(
        &self,
        i: usize,
        plan: &str,
        card: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = self.plan_path(plan);
        let store = cx.global::<WorkbenchStore>();
        let found = store.plan(&path).map(|(_, entry)| entry.plan.clone());
        let plan_title = found
            .as_ref()
            .map(|plan| plan.title.clone())
            .unwrap_or_else(|| plan.to_owned());
        let card_info = card.and_then(|id| {
            found
                .as_ref()?
                .cards
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.id.clone(), c.title.clone(), c.column.clone()))
        });
        let row = div().flex().items_center().gap(px(8.)).min_w_0().child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(small_text(plan_title, p))
                .when_some(card_info.clone(), |d, (_, title, _)| {
                    d.child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(p.fg_emph.to_hsla())
                            .child(title),
                    )
                }),
        );
        let Some((card_id, _, column)) = card_info else {
            return row.into_any_element();
        };
        let review = column == Column::Review;
        row.child(chip(
            column_label(&column).to_owned(),
            column_color(&column, p),
            p,
        ))
        .child(
            Button::new(SharedString::from(format!("mk-card-{i}")))
                .small()
                .when(review, |b| b.primary())
                .label(if review { "Review" } else { "Open card" })
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(MockupsEvent::OpenCard {
                        plan: path.clone(),
                        card: card_id.clone(),
                    })
                })),
        )
        .into_any_element()
    }
}

impl Render for MockupsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Theme::current(cx).palette();
        let page = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(p.bg.to_hsla())
            .text_color(p.fg.to_hsla());
        let Some(index) = self.index.clone() else {
            return page.child(centre_note("Loading\u{2026}", p));
        };
        if self.project.path.is_none() {
            return page.child(centre_note(
                "This project has no folder, so it has no mockups.",
                p,
            ));
        }
        let selected = self
            .selected
            .as_deref()
            .and_then(|id| index.pages.iter().find(|page| page.id == id));
        let detail = match selected {
            Some(page) => self.render_detail(page, p, cx),
            None => muted_text("Pick a page on the left.", p),
        };
        page.child(self.render_header(&index, p, cx)).child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .id("mk-left")
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap(px(12.))
                        .w(px(330.))
                        .h_full()
                        .p(px(14.))
                        .overflow_y_scroll()
                        .border_r_1()
                        .border_color(p.line.to_hsla())
                        .child(self.render_system(&index, p, cx))
                        .child(self.render_pages(&index, p, cx)),
                )
                .child(
                    div()
                        .id("mk-detail")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .p(px(18.))
                        .overflow_y_scroll()
                        .child(detail),
                ),
        )
    }
}

fn status_color(status: MockupStatus, p: &Palette) -> Hsla {
    match status {
        MockupStatus::Review => p.now,
        MockupStatus::Approved => p.ok,
        MockupStatus::SentBack => p.warn,
        MockupStatus::Draft | MockupStatus::Other => p.muted,
    }
    .to_hsla()
}

fn card(p: &Palette) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .p(px(12.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.line.to_hsla())
        .bg(p.surface.to_hsla())
}

fn card_title(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(p.fg_emph.to_hsla())
        .child(text.to_owned())
}

fn small_text(text: String, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(11.5))
        .text_color(p.muted.to_hsla())
        .child(text)
}

fn centre_note(text: &'static str, p: &Palette) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(p.muted.to_hsla())
        .child(text)
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// A file name from a project key and page id: anything but letters, digits, `-`, `_` and `.`
/// becomes `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
