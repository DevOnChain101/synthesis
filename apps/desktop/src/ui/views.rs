//! The Mode 2 views: the two left sidebars, the session view (conversation, composer, file/diff
//! viewer, terminal panel, status line), Mode 1 around a terminal, and the Activity, Tools,
//! Topology and Inbox pages. Look: `design/03-designs/desktop` tokens (`theme`). Logic lives in
//! `convo`, `sessions`, `project`, `activity`, `topology` and `engine`; this file only draws.

use std::path::PathBuf;

use gpui::AnimationExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledText, canvas, div, img, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use synthesis_permissions::Mode as Perm;

use super::{
    ActiveNav, Bottom, INSPECTOR_W, Menu, Nav, READING_W, Shell, SideTab, TOP_SAFE, Tab, choice,
    drag, empty, icon, kbd, label, spaced,
};
use crate::activity::{self, Filter};
use crate::convo::{ApprovalState, Item, StepState, ToolState};
use crate::engine::Choice;
use crate::sessions::{self, Status};
use crate::syntax;
use crate::theme::{self, hsla};
use crate::topology::{self, Pos, Rect};

const DOT_NEEDS_YOU: u32 = theme::ERROR;
const DOT_RUNNING: u32 = theme::PENDING;
const DOT_DONE: u32 = theme::SUCCESS;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn dot(s: Status) -> gpui::Div {
    div()
        .size(px(7.))
        .flex_shrink_0()
        .rounded_full()
        .bg(hsla(match s {
            Status::NeedsYou => DOT_NEEDS_YOU,
            Status::Running => DOT_RUNNING,
            Status::Done => DOT_DONE,
        }))
}

fn sid(prefix: &str, i: impl std::fmt::Display) -> SharedString {
    SharedString::from(format!("{prefix}-{i}"))
}

fn rule() -> gpui::Div {
    div().h(px(1.)).my(px(8.)).bg(hsla(0x30333A))
}

fn nav_row(
    id: impl Into<gpui::ElementId>,
    icon_name: &str,
    text: String,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .gap(px(12.))
        .p(px(10.))
        .my(px(2.))
        .rounded(px(9.))
        .text_size(px(13.))
        .text_color(hsla(if active { 0xFFFFFF } else { 0xAAB0BA }))
        .child(icon(icon_name, 16., 0xB6BDC6))
        .child(div().flex_1().child(text))
        .cursor_pointer()
        .hover(|d| d.bg(hsla(0x1A1F26)))
        .when_active(active)
}

fn perm_label(m: Perm) -> &'static str {
    match m {
        Perm::Default => "Ask",
        Perm::AcceptEdits => "Accept edits",
        Perm::Plan => "Plan",
        Perm::Auto => "Auto",
        Perm::DontAsk => "Don't ask",
        Perm::BypassPermissions => "Bypass",
    }
}

const PERMS: [Perm; 5] = [
    Perm::Default,
    Perm::AcceptEdits,
    Perm::Plan,
    Perm::Auto,
    Perm::DontAsk,
];

impl Shell {
    /// Sidebar 1, `Workspace`: workspaces from the session store, Inbox, search, Configuration.
    pub(super) fn workspace_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.cwd.display().to_string();
        let list = sessions::workspaces(&self.saved, &current);
        let pending = self.pending_total();
        let rows = list.into_iter().enumerate().map(|(i, w)| {
            let active = w.cwd == current;
            let stat = (active && self.proj.stat != (0, 0)).then(|| {
                h_flex()
                    .gap(px(5.))
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(hsla(theme::SUCCESS))
                            .child(format!("+{}", self.proj.stat.0)),
                    )
                    .child(
                        div()
                            .text_color(hsla(theme::ERROR))
                            .child(format!("−{}", self.proj.stat.1)),
                    )
            });
            let path = PathBuf::from(&w.cwd);
            nav_row(sid("ws", i), "folder", w.name, active)
                .children(stat)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_workspace(path.clone(), cx)),
                )
        });
        let hint = |t: String| {
            div()
                .text_size(px(11.))
                .text_color(hsla(theme::MUTED))
                .child(t)
        };
        v_flex()
            .w(px(210.))
            .flex_shrink_0()
            .h_full()
            .justify_between()
            .px(px(13.))
            .pb(px(13.))
            .bg(gpui::linear_gradient(
                160.,
                gpui::linear_color_stop(hsla(0x151A20), 0.),
                gpui::linear_color_stop(hsla(theme::SIDEBAR), 1.),
            ))
            .border_r_1()
            .border_color(hsla(0x2C3139))
            .child(
                v_flex()
                    .child(drag("drag-rail").h(px(TOP_SAFE)).w_full())
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .m(px(6.))
                            .mb(px(18.))
                            .child(img(crate::brand::mark()).size(px(30.)).flex_shrink_0())
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_semibold()
                                    .child(spaced("SYNTHESIS")),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("new-task")
                            .gap(px(10.))
                            .px(px(12.))
                            .py(px(9.))
                            .rounded(px(10.))
                            .border_1()
                            .border_color(hsla(theme::INPUT_BORDER))
                            .bg(hsla(0x1B2027))
                            .text_size(px(13.))
                            .cursor_pointer()
                            .child(icon("plus", 16., 0xD4D9DF))
                            .child(div().flex_1().child("New Task"))
                            .child(hint(kbd("N")))
                            .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                    )
                    .child(rule())
                    .child(
                        nav_row("search", "search", "Search".into(), false)
                            .child(hint(kbd("K")))
                            .on_click(cx.listener(|this, _, w, cx| this.open_palette(w, cx))),
                    )
                    .child(
                        nav_row(
                            "inbox",
                            "inbox",
                            if pending > 0 {
                                format!("Inbox  ({pending})")
                            } else {
                                "Inbox".into()
                            },
                            self.nav == Nav::Inbox,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav = Nav::Inbox;
                            cx.notify();
                        })),
                    )
                    .child(rule())
                    .child(
                        h_flex()
                            .justify_between()
                            .px(px(10.))
                            .child(label("Workspace"))
                            .child(icon("plus", 14., theme::MUTED)),
                    )
                    .children(rows),
            )
            .child(
                v_flex()
                    .child(
                        nav_row(
                            "config",
                            "settings",
                            "Configuration".into(),
                            self.nav == Nav::Configuration,
                        )
                        .child(hint(kbd(",")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav = Nav::Configuration;
                            cx.notify();
                        })),
                    )
                    .child(
                        v_flex()
                            .mt(px(8.))
                            .gap(px(4.))
                            .p(px(10.))
                            .rounded(px(11.))
                            .border_1()
                            .border_color(hsla(0x2C3139))
                            .bg(hsla(0x151A20))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .text_size(px(12.))
                                    .child(icon("folder-open", 15., 0xB6BDC6))
                                    .child(div().flex_1().child(self.project_name()))
                                    .child(icon("chevron-down", 14., theme::MUTED)),
                            )
                            .when(!self.branch.is_empty(), |d| {
                                d.child(
                                    h_flex()
                                        .gap(px(8.))
                                        .text_size(px(11.))
                                        .text_color(hsla(theme::MUTED))
                                        .child(icon("git-branch", 14., theme::MUTED))
                                        .child(self.branch.clone()),
                                )
                            }),
                    )
                    .child(div().px(px(10.)).pt(px(8.)).child(hint(format!(
                        "v{} · {}",
                        env!("CARGO_PKG_VERSION"),
                        match (self.settings.mode, self.settings.plain_terminal) {
                            (crate::settings::Mode::Gpui, _) => "GPUI",
                            (crate::settings::Mode::Tui, false) => "TUI",
                            (crate::settings::Mode::Tui, true) => "TUI · plain terminal",
                        }
                    )))),
            )
            .into_any_element()
    }

    pub(super) fn new_session(&mut self, cx: &mut Context<Self>) {
        self.tabs[0] = Tab::new_live();
        self.active = 0;
        self.nav = Nav::Session;
        self.start_engine(cx);
        cx.notify();
    }

    /// Sidebar 2, `Sessions`, for the selected workspace: tabs Sessions | Explorer | Changes.
    pub(super) fn sessions_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.side_tab {
            SideTab::Sessions => self.sessions_list(cx),
            SideTab::Explorer => self.explorer(cx),
            SideTab::Changes => self.changes(cx),
        };
        let tab = |id: &'static str, text: &'static str, t: SideTab| {
            let on = self.side_tab == t;
            div()
                .id(id)
                .flex_1()
                .py(px(6.))
                .text_center()
                .rounded(px(7.))
                .text_size(px(12.))
                .text_color(hsla(if on { 0xFFFFFF } else { theme::MUTED }))
                .cursor_pointer()
                .when(on, |d| d.bg(hsla(theme::NAV_ACTIVE)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.side_tab = t;
                    cx.notify();
                }))
                .child(text)
        };
        let page = |id: &'static str, ic: &'static str, text: &'static str, n: Nav| {
            div()
                .id(id)
                .text_size(px(12.))
                .text_color(hsla(if self.nav == n {
                    0xFFFFFF
                } else {
                    theme::MUTED
                }))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav = n;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(icon(ic, 13., theme::MUTED))
                        .child(text),
                )
        };
        v_flex()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .gap(px(10.))
            .px(px(13.))
            .pb(px(13.))
            .bg(hsla(theme::SURFACE))
            .child(drag("drag-side").h(px(TOP_SAFE - 10.)).w_full())
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_semibold()
                            .child(self.project_name()),
                    )
                    .child(
                        div()
                            .id("side-plus")
                            .cursor_pointer()
                            .child(icon("plus", 16., theme::MUTED))
                            .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                    ),
            )
            .child(
                h_flex()
                    .p(px(3.))
                    .rounded(px(9.))
                    .bg(hsla(theme::BG))
                    .child(tab("t-sessions", "Sessions", SideTab::Sessions))
                    .child(tab("t-explorer", "Explorer", SideTab::Explorer))
                    .child(tab("t-changes", "Changes", SideTab::Changes)),
            )
            .child(
                h_flex()
                    .gap(px(12.))
                    .child(page("p-activity", "activity", "Activity", Nav::Activity))
                    .child(page("p-tools", "wrench", "Tools", Nav::Tools))
                    .child(page("p-topology", "network", "Topology", Nav::Topology)),
            )
            .child(body)
            .into_any_element()
    }

    fn sessions_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let q = self.search.read(cx).value().to_lowercase();
        let cwd = self.cwd.display().to_string();
        let live = &self.tabs[0];
        let live_row = (live.title != "New session" || self.engine.is_some()).then(|| {
            let st = sessions::status_of(Some(&live.convo));
            session_row(
                "s-live".into(),
                live.title.clone(),
                "now".into(),
                st,
                self.active == 0,
                [
                    ("cpu", self.model.clone().unwrap_or_default()),
                    ("git-branch", self.branch.clone()),
                ]
                .into_iter()
                .filter(|(_, t)| !t.is_empty())
                .collect(),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.active = 0;
                this.nav = Nav::Session;
                cx.notify();
            }))
        });
        let mut groups = vec![];
        let live_sid = self.engine.as_ref().map(|e| e.sid.clone());
        for (g, rows) in sessions::grouped(&self.saved, &cwd, now_ms()) {
            let rows: Vec<_> = rows
                .into_iter()
                .filter(|s| Some(&s.id) != live_sid.as_ref())
                .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let items = rows.into_iter().map(|s| {
                let id = s.id.clone();
                let open = self.tabs.iter().position(|t| t.id == s.id) == Some(self.active);
                session_row(
                    sid("s", &s.id),
                    if s.title.is_empty() {
                        s.id.chars().take(8).collect()
                    } else {
                        s.title.clone()
                    },
                    sessions::age_text(now_ms() - s.mtime),
                    Status::Done,
                    open,
                    vec![],
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_saved(&id, cx)))
            });
            groups.push(
                v_flex()
                    .gap(px(2.))
                    .child(label(g))
                    .children(items)
                    .into_any_element(),
            );
        }
        let none = groups.is_empty() && live_row.is_none();
        v_flex()
            .id("sessions-scroll")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .gap(px(8.))
            .child(
                div()
                    .rounded(px(9.))
                    .border_1()
                    .border_color(hsla(0x30333A))
                    .px(px(8.))
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(icon("search", 14., theme::MUTED))
                            .child(Input::new(&self.search).appearance(false)),
                    ),
            )
            .children(live_row)
            .children(groups)
            .when(none, |d| d.child(empty("No sessions yet.")))
            .child(
                Button::new("new-session")
                    .icon(icon("plus", 14., theme::MUTED))
                    .label("New session")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
            )
            .into_any_element()
    }

    fn explorer(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(n) = &self.proj.note {
            return empty(n).into_any_element();
        }
        let rows = self.proj.tree.iter().enumerate().map(|(i, r)| {
            let path = r.path.clone();
            h_flex()
                .id(sid("f", i))
                .gap(px(6.))
                .pl(px(6. + 12. * r.depth as f32))
                .py(px(2.))
                .text_size(px(12.))
                .text_color(hsla(if r.dir { theme::MUTED } else { theme::SILVER }))
                .when(!r.dir, |d| {
                    d.cursor_pointer().on_click(
                        cx.listener(move |this, _, _, cx| this.open_file(&path, false, cx)),
                    )
                })
                .child(icon(
                    if r.dir { "folder" } else { "file" },
                    13.,
                    theme::MUTED,
                ))
                .child(r.name.clone())
        });
        v_flex()
            .id("explorer-scroll")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .children(rows)
            .into_any_element()
    }

    fn changes(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(n) = &self.proj.note {
            return empty(n).into_any_element();
        }
        if self.proj.changes.is_empty() {
            return empty("No uncommitted changes.").into_any_element();
        }
        let rows = self.proj.changes.iter().enumerate().map(|(i, c)| {
            let path = c.path.clone();
            let colour = match c.letter {
                'D' => theme::ERROR,
                'M' | 'R' => theme::PENDING,
                _ => theme::SUCCESS,
            };
            h_flex()
                .id(sid("c", i))
                .gap(px(8.))
                .py(px(3.))
                .px(px(6.))
                .text_size(px(12.))
                .cursor_pointer()
                .child(div().text_color(hsla(colour)).child(c.letter.to_string()))
                .child(div().text_color(hsla(theme::SILVER)).child(c.path.clone()))
                .on_click(cx.listener(move |this, _, _, cx| this.open_file(&path, true, cx)))
        });
        v_flex()
            .id("changes-scroll")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .children(rows)
            .into_any_element()
    }

    // ------------------------------------------------------------------------------ session

    fn tab_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = self.tabs.iter().enumerate().map(|(i, t)| {
            let on = i == self.active;
            let st = sessions::status_of(t.live.then_some(&t.convo));
            h_flex()
                .id(sid("tab", i))
                .gap(px(8.))
                .px(px(12.))
                .py(px(6.))
                .rounded(px(8.))
                .text_size(px(12.))
                .text_color(hsla(if on { 0xFFFFFF } else { theme::MUTED }))
                .cursor_pointer()
                .when(on, |d| d.bg(hsla(0x1A1F26)))
                .hover(|d| d.text_color(hsla(0xFFFFFF)))
                .child(dot(st))
                .child(t.title.clone())
                .when(!t.live, |d| {
                    d.child(
                        div()
                            .id(sid("tabx", i))
                            .child(icon("close", 12., theme::MUTED))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tabs.remove(i);
                                this.active = this.active.min(this.tabs.len() - 1);
                                cx.notify();
                            })),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = i;
                    cx.notify();
                }))
        });
        h_flex()
            .h(px(TOP_SAFE))
            .flex_shrink_0()
            .px(px(16.))
            .gap(px(4.))
            .children(tabs)
            .child(drag("drag-tabs").flex_1().h_full())
            .into_any_element()
    }

    /// One conversation item. Approvals never focus Allow: Deny comes first.
    fn item(&self, i: usize, it: &Item, interactive: bool, cx: &mut Context<Self>) -> AnyElement {
        match it {
            Item::User(t) => div()
                .px(px(16.))
                .py(px(12.))
                .rounded(px(14.))
                .bg(hsla(0x1A1F26))
                .text_size(px(13.))
                .line_height(px(20.))
                .child(t.clone())
                .into_any_element(),
            Item::Assistant { text, streaming } => div()
                .text_size(px(14.))
                .line_height(px(22.))
                .text_color(hsla(theme::TEXT))
                .child(format!("{text}{}", if *streaming { " ▍" } else { "" }))
                .into_any_element(),
            Item::Tool {
                id,
                name,
                summary,
                state,
                preview,
                file,
            } => self.tool_item(i, id, name, summary, *state, preview, file.as_deref(), cx),
            Item::Verification { status, summary } => {
                let ok = status == "passed";
                let (ic, c) = if ok {
                    ("circle-check", theme::SUCCESS)
                } else {
                    ("circle-x", theme::ERROR)
                };
                v_flex()
                    .gap(px(8.))
                    .p(px(16.))
                    .rounded(px(14.))
                    .bg(hsla(0x14181D))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .child(icon(ic, 16., c))
                            .child(div().font_semibold().child("Verification"))
                            .child(div().text_color(hsla(c)).child(status.clone())),
                    )
                    .children(summary.lines().filter(|l| !l.trim().is_empty()).map(|l| {
                        h_flex()
                            .gap(px(8.))
                            .pl(px(24.))
                            .text_size(px(12.))
                            .text_color(hsla(theme::SILVER))
                            .child(icon(
                                "check",
                                12.,
                                if ok { theme::SUCCESS } else { theme::MUTED },
                            ))
                            .child(l.trim().to_string())
                    }))
                    .into_any_element()
            }
            Item::Approval {
                id, prompt, state, ..
            } => {
                let body = match state {
                    ApprovalState::Pending if interactive => {
                        let btn = |tag: &'static str,
                                   text: &'static str,
                                   c: Choice,
                                   primary: bool| {
                            let id = id.clone();
                            let b = Button::new(sid(tag, format!("{i}-{id}")))
                                .label(text)
                                .small();
                            let b = if primary { b.primary() } else { b.outline() };
                            b.on_click(cx.listener(move |this, _, _, cx| this.answer(&id, c, cx)))
                        };
                        h_flex()
                            .gap(px(8.))
                            .child(btn("deny", "Deny", Choice::Deny, false))
                            .child(btn("once", "Allow once", Choice::Once, false))
                            .child(btn("always", "Always for project", Choice::Always, false))
                            .into_any_element()
                    }
                    ApprovalState::Pending => div()
                        .text_color(hsla(theme::PENDING))
                        .child("waiting for an answer")
                        .into_any_element(),
                    ApprovalState::Allowed => div()
                        .text_color(hsla(theme::SUCCESS))
                        .child("Allowed")
                        .into_any_element(),
                    ApprovalState::Denied => div()
                        .text_color(hsla(theme::ERROR))
                        .child("Denied")
                        .into_any_element(),
                };
                v_flex()
                    .gap(px(8.))
                    .p(px(16.))
                    .rounded(px(14.))
                    .border_1()
                    .border_color(hsla(if *state == ApprovalState::Pending {
                        theme::PEARL
                    } else {
                        0x1B1F26
                    }))
                    .bg(hsla(0x1B1F26))
                    .text_size(px(12.))
                    .child(label("Needs your approval"))
                    .child(
                        div()
                            .font_family(self.font())
                            .text_size(px(12.))
                            .child(prompt.clone()),
                    )
                    .child(body)
                    .into_any_element()
            }
            Item::Notice { warning, text } => div()
                .text_size(px(12.))
                .text_color(hsla(if *warning {
                    theme::PENDING
                } else {
                    theme::MUTED
                }))
                .child(text.clone())
                .into_any_element(),
        }
    }

    /// Tool calls as evidence: a diff card per changed file, a terminal snippet for Bash, a browser
    /// card for browser tools; every other tool is one quiet line.
    #[allow(clippy::too_many_arguments)]
    fn tool_item(
        &self,
        i: usize,
        id: &str,
        name: &str,
        summary: &str,
        state: ToolState,
        preview: &str,
        file: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state_icon, c) = match state {
            ToolState::Running => ("loader-circle", theme::PENDING),
            ToolState::Ok => ("circle-check", theme::SUCCESS),
            ToolState::Failed => ("circle-x", theme::ERROR),
            ToolState::NotRun => ("circle", theme::MUTED),
        };
        let lower = name.to_lowercase();
        let ev = if let Some(f) = file {
            Evidence::Diff(f.to_string())
        } else if name == "Bash" {
            Evidence::Terminal
        } else if lower.starts_with("browser") {
            Evidence::Browser
        } else {
            return h_flex()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(hsla(theme::MUTED))
                .child(icon(state_icon, 13., c))
                .child(name.to_string())
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(summary.to_string()),
                )
                .into_any_element();
        };
        let open = !self.collapsed.contains(id);
        let mono = self.font();
        let diff = file.and_then(|f| self.file_diffs.get(f));
        let (kind_icon, title) = match &ev {
            Evidence::Diff(f) => (
                "file",
                f.rsplit(['/', '\\']).next().unwrap_or(f).to_string(),
            ),
            Evidence::Terminal => ("square-terminal", summary.to_string()),
            Evidence::Browser => ("globe", summary.to_string()),
        };
        let body: Option<AnyElement> = match &ev {
            Evidence::Diff(_) => diff.filter(|d| !d.2.is_empty()).map(|(_, _, lines)| {
                v_flex()
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(hsla(0x0F1216))
                    .font_family(mono.clone())
                    .text_size(px(11.))
                    .line_height(px(17.))
                    .children(lines.iter().take(14).map(|l| {
                        let c = if l.starts_with('+') && !l.starts_with("+++") {
                            theme::SUCCESS
                        } else if l.starts_with('-') && !l.starts_with("---") {
                            theme::ERROR
                        } else if l.starts_with("@@") {
                            theme::ACCENT
                        } else {
                            theme::MUTED
                        };
                        div()
                            .text_color(hsla(c))
                            .whitespace_nowrap()
                            .child(l.clone())
                    }))
                    .into_any_element()
            }),
            Evidence::Terminal => (!preview.is_empty()).then(|| {
                div()
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(hsla(0x0F1216))
                    .font_family(mono.clone())
                    .text_size(px(11.))
                    .line_height(px(17.))
                    .text_color(hsla(theme::SILVER))
                    .child(preview.lines().take(8).collect::<Vec<_>>().join("\n"))
                    .into_any_element()
            }),
            Evidence::Browser => (!preview.is_empty()).then(|| {
                div()
                    .px(px(4.))
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(hsla(theme::SILVER))
                    .child(preview.lines().take(4).collect::<Vec<_>>().join("\n"))
                    .into_any_element()
            }),
        };
        let toggle_id = id.to_string();
        let ev2 = ev.clone();
        v_flex()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(14.))
            .bg(hsla(0x14181D))
            .child(
                h_flex()
                    .id(sid("ev", i))
                    .gap(px(8.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .child(icon(state_icon, 14., c))
                    .child(icon(kind_icon, 14., 0x6B727C))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(mono)
                            .child(title),
                    )
                    .children(diff.map(|(a, r, _)| {
                        h_flex()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .child(
                                div()
                                    .text_color(hsla(theme::SUCCESS))
                                    .child(format!("+{a}")),
                            )
                            .child(
                                div()
                                    .text_color(hsla(theme::ERROR))
                                    .child(format!("\u{2212}{r}")),
                            )
                    }))
                    .child(
                        div()
                            .id(sid("ins", i))
                            .child(icon("panel-right-open", 15., 0x6B727C))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.inspect(&ev2, cx);
                            })),
                    )
                    .child(icon(
                        if open { "chevron-up" } else { "chevron-down" },
                        14.,
                        0x6B727C,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&toggle_id) {
                            this.collapsed.insert(toggle_id.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(open, |d| d.children(body))
            .into_any_element()
    }

    /// Opens the Inspector slide-over on the evidence of a card.
    fn inspect(&mut self, ev: &Evidence, cx: &mut Context<Self>) {
        match ev {
            Evidence::Diff(f) => self.open_file(f, true, cx),
            Evidence::Terminal => {
                self.bottom = Bottom::Terminal;
                self.inspector_open = true;
            }
            Evidence::Browser => {
                self.bottom = Bottom::Browser;
                self.inspector_open = true;
            }
        }
        cx.notify();
    }

    /// The mission at the top of a session: the agent's plan with progress.
    fn mission_card(&self) -> Option<AnyElement> {
        let c = &self.tabs[self.active].convo;
        if c.plan.is_empty() {
            return None;
        }
        let done = c.plan.iter().filter(|s| s.state == StepState::Done).count();
        let steps = c.plan.iter().map(|s| {
            let (ic, col) = match s.state {
                StepState::Done => ("circle-check", theme::SUCCESS),
                StepState::Active => ("loader-circle", theme::PENDING),
                StepState::Todo => ("circle", theme::MUTED),
            };
            h_flex()
                .gap(px(8.))
                .text_size(px(13.))
                .text_color(hsla(if s.state == StepState::Todo {
                    theme::MUTED
                } else {
                    theme::TEXT
                }))
                .child(icon(ic, 15., col))
                .child(s.text.clone())
        });
        Some(
            v_flex()
                .gap(px(8.))
                .p(px(16.))
                .rounded(px(14.))
                .bg(hsla(0x14181D))
                .child(
                    h_flex().justify_between().child(label("Mission")).child(
                        div()
                            .text_size(px(11.))
                            .text_color(hsla(theme::MUTED))
                            .child(format!("{done} of {}", c.plan.len())),
                    ),
                )
                .children(steps)
                .into_any_element(),
        )
    }

    fn items(&self, tab: usize, interactive: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let items = self.tabs[tab].convo.items.clone();
        items
            .iter()
            .enumerate()
            // the plan is the mission card, not a row
            .filter(|(_, it)| !matches!(it, Item::Tool { name, .. } if name == "TodoWrite"))
            .map(|(i, it)| self.item(i, it, interactive, cx))
            .collect()
    }

    fn chip(
        &self,
        id: &'static str,
        text: String,
        menu: Menu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .px(px(10.))
            .py(px(4.))
            .rounded(px(7.))
            .border_1()
            .border_color(hsla(theme::INPUT_BORDER))
            .text_size(px(11.))
            .text_color(hsla(theme::SILVER))
            .cursor_pointer()
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(text)
                    .child(icon("chevron-down", 12., theme::MUTED)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.menu = if this.menu == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                cx.notify();
            }))
            .into_any_element()
    }

    fn menu_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.menu?;
        let opt = |id: SharedString, text: String, on: bool| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(5.))
                .rounded(px(6.))
                .text_size(px(12.))
                .cursor_pointer()
                .text_color(hsla(if on { 0xFFFFFF } else { theme::SILVER }))
                .when(on, |d| d.bg(hsla(theme::NAV_ACTIVE)))
                .child(text)
        };
        let items: Vec<AnyElement> = match m {
            Menu::Model => self
                .engine
                .as_ref()
                .map(|e| e.models.clone())
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let n = name.clone();
                    opt(
                        sid("mo", i),
                        name.clone(),
                        self.model.as_deref() == Some(&name),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_model(n.clone(), cx)))
                    .into_any_element()
                })
                .collect(),
            Menu::Effort => std::iter::once(None)
                .chain(
                    self.engine
                        .as_ref()
                        .map(|e| e.effort_tiers.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .map(Some),
                )
                .enumerate()
                .map(|(i, e)| {
                    let on = self.effort == e;
                    let text = e.clone().unwrap_or_else(|| "default".into());
                    opt(sid("ef", i), text, on)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.effort = e.clone();
                            this.menu = None;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
            Menu::Perm => PERMS
                .into_iter()
                .enumerate()
                .map(|(i, p)| {
                    opt(sid("pm", i), perm_label(p).into(), self.perm == p)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_perm(p);
                            this.menu = None;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
        };
        Some(
            v_flex()
                .p(px(4.))
                .rounded(px(9.))
                .border_1()
                .border_color(hsla(theme::LINE))
                .bg(hsla(theme::PANEL))
                .children(items)
                .into_any_element(),
        )
    }

    pub(super) fn set_perm(&mut self, p: Perm) {
        self.perm = p;
        if let Some(e) = &self.engine {
            e.set_mode(p);
        }
    }

    /// The composer. On the landing it also carries the attach chips (visual only: attachments are
    /// not built yet) and the larger hero look.
    fn composer_box(&self, landing: bool, cx: &mut Context<Self>) -> AnyElement {
        let running = self.tabs[0].convo.running;
        let model = self.model.clone().unwrap_or_else(|| "model".into());
        let has_effort = self
            .engine
            .as_ref()
            .is_some_and(|e| !e.effort_tiers.is_empty());
        let attach = |t: &'static str| {
            h_flex()
                .gap(px(5.))
                .px(px(9.))
                .py(px(5.))
                .rounded(px(8.))
                .bg(hsla(0x1E242B))
                .text_size(px(11.))
                .text_color(hsla(theme::SILVER))
                .child(icon("plus", 12., theme::MUTED))
                .child(t)
        };
        let auto = self.perm == Perm::Auto;
        v_flex()
            .gap(px(8.))
            .m(px(12.))
            .children(self.menu_list(cx))
            .child(
                v_flex()
                    .p(px(if landing { 16. } else { 12. }))
                    .gap(px(if landing { 22. } else { 8. }))
                    .border_1()
                    .border_color(hsla(theme::INPUT_BORDER))
                    .rounded(px(11.))
                    .bg(hsla(0x232831))
                    .child(Input::new(&self.composer).appearance(false))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .when(landing, |d| {
                                d.child(attach("Codebase"))
                                    .child(attach("Image"))
                                    .child(attach("Document"))
                            })
                            .when(!landing, |d| d.child(icon("plus", 14., theme::MUTED)))
                            .child(self.chip("chip-model", model, Menu::Model, cx))
                            .when(has_effort, |d| {
                                d.child(self.chip(
                                    "chip-effort",
                                    self.effort.clone().unwrap_or_else(|| "effort".into()),
                                    Menu::Effort,
                                    cx,
                                ))
                            })
                            .child(self.chip(
                                "chip-perm",
                                perm_label(self.perm).into(),
                                Menu::Perm,
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(
                                h_flex()
                                    .id("auto")
                                    .gap(px(5.))
                                    .text_size(px(12.))
                                    .text_color(hsla(if auto {
                                        theme::PEARL
                                    } else {
                                        theme::MUTED
                                    }))
                                    .cursor_pointer()
                                    .child(icon(
                                        "zap",
                                        14.,
                                        if auto { theme::PEARL } else { theme::MUTED },
                                    ))
                                    .child("Auto")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_perm(if auto {
                                            Perm::Default
                                        } else {
                                            Perm::Auto
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(if running {
                                Button::new("stop")
                                    .label("Stop")
                                    .outline()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(e) = &this.engine {
                                            e.cancel();
                                        }
                                        cx.notify();
                                    }))
                                    .into_any_element()
                            } else {
                                div()
                                    .id("send")
                                    .p(px(7.))
                                    .rounded(px(9.))
                                    .bg(hsla(0x363C45))
                                    .cursor_pointer()
                                    .child(icon("arrow-up", 16., theme::TEXT))
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.send_composer(w, cx)),
                                    )
                                    .into_any_element()
                            }),
                    ),
            )
            .into_any_element()
    }

    fn engine_note(&self) -> Option<AnyElement> {
        use super::EngineState::{Failed, Idle, Starting};
        let (text, colour) = match &self.engine_state {
            Starting => ("Starting the engine…".to_string(), theme::MUTED),
            Failed(e) => (
                format!("The engine did not start: {e}. Connect a provider in Configuration."),
                theme::ERROR,
            ),
            Idle => (
                "The engine starts after you connect a provider in Configuration.".into(),
                theme::MUTED,
            ),
            _ => return None,
        };
        Some(
            div()
                .text_size(px(12.))
                .text_color(hsla(colour))
                .child(text)
                .into_any_element(),
        )
    }

    fn conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[self.active];
        let live = tab.live;
        let items = self.items(self.active, live, cx);
        if live && (tab.convo.running || tab.convo.needs_you() > 0) {
            self.convo_scroll.scroll_to_bottom();
        }
        if self.is_landing() {
            return self.landing(cx);
        }
        let activity = tab.convo.activity().filter(|_| live);
        let composer = live.then(|| self.composer_box(false, cx));
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .children(self.mission_card().map(|m| {
                div()
                    .flex_shrink_0()
                    .w_full()
                    .max_w(px(READING_W))
                    .px(px(24.))
                    .pt(px(8.))
                    .child(m)
            }))
            .child(
                div()
                    .id("convo-scroll")
                    .flex()
                    .flex_col()
                    .items_center()
                    .overflow_y_scroll()
                    .track_scroll(&self.convo_scroll)
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(READING_W))
                            .gap(px(16.))
                            .px(px(24.))
                            .py(px(24.))
                            .children(items)
                            .when(!live, |d| {
                                d.child(empty("Read-only replay of a stored session."))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(READING_W))
                    .px(px(12.))
                    .children(activity.map(|a| {
                        h_flex()
                            .gap(px(8.))
                            .px(px(12.))
                            .text_size(px(12.))
                            .text_color(hsla(theme::MUTED))
                            .child(icon("loader-circle", 13., theme::PENDING))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(a),
                            )
                    }))
                    .children(composer),
            )
            .into_any_element()
    }

    /// The first screen of a session: greeting over a soft hero gradient, the composer and four
    /// starting points (docs/specs/desktop.md section 8, `desktop-preview.png`).
    fn landing(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.composer_box(true, cx);
        let card = |i: usize, text: &'static str| {
            div()
                .id(sid("sugg", i))
                .flex_1()
                .min_w_0()
                .p(px(14.))
                .rounded(px(11.))
                .border_1()
                .border_color(hsla(0x2D333B))
                .bg(hsla(0x14181D))
                .text_size(px(12.))
                .line_height(px(17.))
                .text_color(hsla(theme::SILVER))
                .cursor_pointer()
                .child(text)
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.composer.update(cx, |c, cx| c.set_value(text, w, cx));
                }))
        };
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .m(px(12.))
            .rounded(px(16.))
            .border_1()
            .border_color(hsla(theme::LINE))
            .overflow_hidden()
            .bg(gpui::linear_gradient(
                215.,
                gpui::linear_color_stop(hsla(0x2A2826), 0.),
                gpui::linear_color_stop(hsla(0x0E1115), 0.55),
            ))
            .child(
                img(crate::brand::mark())
                    .absolute()
                    .top(px(-40.))
                    .right(px(-30.))
                    .size(px(300.))
                    .opacity(0.22),
            )
            .child(
                v_flex().size_full().items_center().justify_center().child(
                    v_flex()
                        .w_full()
                        .max_w(px(680.))
                        .px(px(24.))
                        .gap(px(18.))
                        .child(
                            v_flex()
                                .text_size(px(28.))
                                .line_height(px(36.))
                                .child(div().child("Welcome back."))
                                .child(
                                    div()
                                        .text_color(hsla(theme::SILVER))
                                        .child("What shall we build today?"),
                                ),
                        )
                        .children(self.engine_note())
                        .child(div().mx(px(-12.)).child(composer))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .items_stretch()
                                .child(card(0, "Implement feature with tests"))
                                .child(card(1, "Refactor this module"))
                                .child(card(2, "Investigate and fix an issue"))
                                .child(card(3, "Add monitoring and alerts")),
                        ),
                ),
            )
            .into_any_element()
    }

    fn viewer_body(&self) -> AnyElement {
        let mono = self.font();
        match &self.viewer {
            None => empty("Open a file or a change to see it here.").into_any_element(),
            Some(v) => {
                let lines = v.lines.iter().enumerate().map(|(i, l)| {
                    let c = if !v.diff {
                        theme::SILVER
                    } else if l.starts_with('+') && !l.starts_with("+++") {
                        theme::SUCCESS
                    } else if l.starts_with('-') && !l.starts_with("---") {
                        theme::ERROR
                    } else if l.starts_with("@@") {
                        theme::ACCENT
                    } else {
                        theme::MUTED
                    };
                    h_flex()
                        .gap(px(10.))
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .when(!v.diff, |d| {
                            d.child(
                                div()
                                    .w(px(28.))
                                    .flex_shrink_0()
                                    .text_color(hsla(theme::MUTED))
                                    .child(format!("{}", i + 1)),
                            )
                        })
                        .child(
                            div()
                                .text_color(hsla(c))
                                .whitespace_nowrap()
                                .child(if v.diff {
                                    StyledText::new(l.clone())
                                } else {
                                    StyledText::new(l.clone()).with_highlights(
                                        syntax::tokens(l).into_iter().map(|(r, k)| {
                                            let c = match k {
                                                syntax::Kind::Keyword => theme::SYNTAX_KEYWORD,
                                                syntax::Kind::Str => theme::SUCCESS,
                                                syntax::Kind::Number => theme::PENDING,
                                                syntax::Kind::Comment => theme::SYNTAX_COMMENT,
                                            };
                                            (
                                                r,
                                                HighlightStyle {
                                                    color: Some(hsla(c)),
                                                    ..Default::default()
                                                },
                                            )
                                        }),
                                    )
                                }),
                        )
                });
                v_flex()
                    .id("viewer-scroll")
                    .overflow_scroll()
                    .flex_1()
                    .min_h_0()
                    .p(px(12.))
                    .child(
                        div()
                            .pb(px(8.))
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .text_color(hsla(0xFFFFFF))
                            .child(v.title.clone()),
                    )
                    .children(lines)
                    .into_any_element()
            }
        }
    }

    /// The Inspector: a slide-over on the right, on demand (Esc closes), for the full diff, file,
    /// terminal and browser evidence.
    fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = |id: &'static str, text: &'static str, b: Bottom| {
            let on = self.bottom == b;
            div()
                .id(id)
                .cursor_pointer()
                .px(px(10.))
                .py(px(4.))
                .rounded(px(8.))
                .text_size(px(12.))
                .text_color(hsla(if on { 0xFFFFFF } else { theme::MUTED }))
                .when(on, |d| d.bg(hsla(0x1E242B)))
                .hover(|d| d.text_color(hsla(0xFFFFFF)))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.bottom = b;
                    cx.notify();
                }))
        };
        let body = match (self.bottom, &self.bottom_term) {
            (Bottom::Code, _) => self.viewer_body(),
            (Bottom::Terminal, Some(t)) => div()
                .size_full()
                .p(px(10.))
                .child(t.clone())
                .into_any_element(),
            (Bottom::Terminal, None) => {
                empty("No shell: this build has no PTY for this platform.").into_any_element()
            }
            (Bottom::Browser, _) => {
                empty("No browser session. The browser runtime is not wired into the window yet.")
                    .into_any_element()
            }
            (Bottom::Verification, _) => {
                let n = self.tabs[0]
                    .events
                    .iter()
                    .filter(|e| activity::type_of(e).as_deref() == Some("verification.completed"))
                    .count();
                empty(&format!("{n} verification reports in this session.")).into_any_element()
            }
        };
        v_flex()
            .id("inspector")
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(INSPECTOR_W))
            .bg(hsla(0x12161B))
            .border_l_1()
            .border_color(hsla(0x20252C))
            .shadow(vec![gpui::BoxShadow {
                color: hsla(0x000000).opacity(0.45),
                offset: gpui::point(px(-8.), px(0.)),
                blur_radius: px(32.),
                spread_radius: px(0.),
                inset: false,
            }])
            .child(
                h_flex()
                    .gap(px(4.))
                    .px(px(16.))
                    .pt(px(if cfg!(target_os = "macos") { 14. } else { 40. }))
                    .pb(px(12.))
                    .child(tab("b-code", "Code", Bottom::Code))
                    .child(tab("b-term", "Terminal", Bottom::Terminal))
                    .child(tab("b-browser", "Browser", Bottom::Browser))
                    .child(tab("b-verify", "Verification", Bottom::Verification))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("inspector-close")
                            .p(px(6.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .hover(|d| d.bg(hsla(0x1E242B)))
                            .child(icon("close", 14., theme::MUTED))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.inspector_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().px(px(8.)).child(body))
            .with_animation(
                "inspector-in",
                gpui::Animation::new(std::time::Duration::from_millis(160))
                    .with_easing(gpui::ease_out_quint()),
                |d, t| d.right(px(-INSPECTOR_W * (1. - t))).opacity(t),
            )
            .into_any_element()
    }

    fn status_line(&self) -> AnyElement {
        let t = &self.tabs[0];
        let sandbox = self
            .engine
            .as_ref()
            .map_or(String::new(), |e| format!("sandbox {}", e.sandbox.mode));
        let left: Vec<String> = [
            self.model.clone().unwrap_or_default(),
            perm_label(self.perm).to_string(),
            sandbox,
        ]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
        let needs = self.pending_total();
        let right = format!("${:.2}   ·   {} tokens", t.convo.cost_usd, t.convo.tokens);
        h_flex()
            .px(px(24.))
            .py(px(8.))
            .justify_between()
            .text_size(px(11.))
            .text_color(hsla(0x6B727C))
            .child(h_flex().gap(px(6.)).child(left.join("  ·  ")).when(
                !self.branch.is_empty(),
                |d| {
                    d.child(icon("git-branch", 12., 0x6B727C))
                        .child(self.branch.clone())
                },
            ))
            .child(
                h_flex().gap(px(12.)).child(right).child(
                    div()
                        .text_color(hsla(if needs > 0 { theme::PENDING } else { 0x6B727C }))
                        .child(format!("needs you: {needs}")),
                ),
            )
            .into_any_element()
    }

    /// The first screen: a live session with nothing said yet and no file opened.
    fn is_landing(&self) -> bool {
        let t = &self.tabs[self.active];
        t.live && t.convo.items.is_empty() && self.viewer.is_none()
    }

    pub(super) fn session_view(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .relative()
            .child(self.tab_strip(cx))
            .child(self.conversation(cx))
            .child(self.status_line())
            .when(self.inspector_open, |d| d.child(self.inspector(cx)))
            .into_any_element()
    }

    // ---------------------------------------------------------------------------------- Mode 1

    /// Mode 1: the TUI with the real engine, or a plain shell with a native chat bar under it.
    pub(super) fn mode1(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let plain = self.settings.plain_terminal;
        if !plain || self.agent_on {
            self.ensure_agent_term(cx);
            let t = self.agent_term.clone();
            return v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().children(t))
                .children(plain.then(|| self.chat_bar(cx)))
                .into_any_element();
        }
        self.ensure_shell_term(cx);
        if self.engine.is_none() {
            self.start_engine(cx);
        }
        let shell = self.shell_term.clone();
        let sheet = (self.sheet && !self.tabs[0].convo.items.is_empty()).then(|| {
            let n = self.tabs[0].convo.items.len();
            let items: Vec<AnyElement> = self
                .items(0, true, cx)
                .into_iter()
                .skip(n.saturating_sub(6))
                .collect();
            self.convo_scroll.scroll_to_bottom();
            v_flex()
                .h(px(300.))
                .flex_shrink_0()
                .gap(px(8.))
                .p(px(12.))
                .border_t_1()
                .border_color(hsla(theme::LINE))
                .bg(hsla(theme::PANEL))
                .child(
                    h_flex().justify_between().child(label("Synthesis")).child(
                        div()
                            .id("sheet-hide")
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(hsla(theme::MUTED))
                            .child("Hide")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sheet = false;
                                cx.notify();
                            })),
                    ),
                )
                .child(
                    v_flex()
                        .id("sheet-scroll")
                        .overflow_y_scroll()
                        .track_scroll(&self.convo_scroll)
                        .flex_1()
                        .min_h_0()
                        .gap(px(8.))
                        .children(items),
                )
        });
        v_flex()
            .size_full()
            .child(div().flex_1().min_h_0().children(shell))
            .children(sheet)
            .child(self.chat_bar(cx))
            .into_any_element()
    }

    fn chat_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap(px(10.))
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(hsla(theme::LINE))
            .bg(hsla(theme::SURFACE))
            .child(div().text_color(hsla(theme::MUTED)).child(">"))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.composer).appearance(false)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(hsla(theme::MUTED))
                    .child(format!("workspace: {}", self.project_name())),
            )
            .child(
                Button::new("agent-toggle")
                    .label(if self.agent_on {
                        "shell ⇄"
                    } else {
                        "agent ⇄"
                    })
                    .outline()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.agent_on = !this.agent_on;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    // ----------------------------------------------------------------------------------- pages

    fn page_frame(title: &str) -> gpui::Div {
        v_flex()
            .size_full()
            .p(px(24.))
            .gap(px(12.))
            .child(div().text_size(px(22.)).child(title.to_string()))
    }

    pub(super) fn activity_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let chips = Filter::ALL.into_iter().map(|f| {
            choice(f.label(), f.label(), self.filter == f).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.filter = f;
                    cx.notify();
                },
            ))
        });
        let rows = activity::rows(&self.tabs[self.active].events, self.filter);
        let none = rows.is_empty();
        let rows = rows.into_iter().take(300).enumerate().map(|(i, r)| {
            h_flex()
                .id(sid("act", i))
                .gap(px(14.))
                .py(px(6.))
                .border_b_1()
                .border_color(hsla(0x23272E))
                .text_size(px(12.))
                .child(
                    div()
                        .w(px(64.))
                        .text_color(hsla(theme::MUTED))
                        .child(r.time),
                )
                .child(div().w(px(96.)).child(r.who))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(hsla(theme::SILVER))
                        .child(r.text),
                )
        });
        Self::page_frame("Activity")
            .child(h_flex().gap(px(8.)).children(chips))
            .child(
                v_flex()
                    .id("activity-scroll")
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .children(rows)
                    .when(none, |d| {
                        d.child(empty("No events yet. Start a task to see them here."))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn tools_page(&self) -> AnyElement {
        let events = &self.tabs[0].events;
        let used = self.used_tools();
        let hooks = events
            .iter()
            .filter(|e| activity::type_of(e).as_deref() == Some("hook.ran"))
            .count();
        let mcp = self.engine.as_ref().and_then(|e| e.mcp().cloned());
        let mcp_rows: Vec<String> = mcp.map_or_else(Vec::new, |m| {
            m.status()
                .into_iter()
                .map(|s| {
                    format!(
                        "{} · {} · {} · {} tools",
                        s.name,
                        format!("{:?}", s.state).to_lowercase(),
                        s.transport,
                        s.tool_count
                    )
                })
                .collect()
        });
        let section = |title: &str, rows: Vec<String>, none: &str| {
            let empty_text = none.to_string();
            v_flex()
                .gap(px(4.))
                .child(label(title.to_string()))
                .children(rows.iter().map(|r| {
                    div()
                        .text_size(px(12.))
                        .text_color(hsla(theme::SILVER))
                        .child(r.clone())
                }))
                .when(rows.is_empty(), |d| d.child(empty(&empty_text)))
        };
        Self::page_frame("Tools")
            .child(section(
                "MCP servers",
                mcp_rows,
                "No MCP servers are configured for this session.",
            ))
            .child(section(
                "Tools used in this session",
                used,
                "No tool has run yet.",
            ))
            .child(section(
                "Hooks",
                if hooks > 0 {
                    vec![format!("{hooks} hook runs in this session")]
                } else {
                    vec![]
                },
                "No hook has run in this session. Listing configured hooks is not built yet.",
            ))
            .child(section(
                "Skills",
                vec![],
                "Listing skills is not built yet.",
            ))
            .into_any_element()
    }

    fn used_tools(&self) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        for it in &self.tabs[0].convo.items {
            if let Item::Tool { name, .. } = it
                && !v.contains(name)
            {
                v.push(name.clone());
            }
        }
        v
    }

    fn topology_snapshot(&self) -> topology::Snapshot {
        let t = &self.tabs[0];
        let ty = |e: &synthesis_events::Envelope| activity::type_of(e).unwrap_or_default();
        let mut subagents: Vec<String> = vec![];
        for e in &t.events {
            if ty(e).starts_with("subagent.")
                && let Some(a) = &e.agent_id
                && !subagents.contains(a)
            {
                subagents.push(a.clone());
            }
        }
        let hooks = t.events.iter().filter(|e| ty(e) == "hook.ran").count();
        topology::Snapshot {
            project: self.project_name(),
            session: t
                .events
                .first()
                .map_or_else(String::new, |e| e.session_id.chars().take(12).collect()),
            prompt: t
                .convo
                .items
                .iter()
                .rev()
                .find_map(|i| {
                    if let Item::User(u) = i {
                        Some(u.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default(),
            sandbox: self
                .engine
                .as_ref()
                .map_or("none".into(), |e| e.sandbox.mode.clone()),
            network: self
                .engine
                .as_ref()
                .map_or("none".into(), |e| e.sandbox.network.clone()),
            mode: perm_label(self.perm).to_lowercase(),
            hooks: if hooks > 0 {
                vec![format!("{hooks} runs")]
            } else {
                vec![]
            },
            subagents,
            mcp: self
                .engine
                .as_ref()
                .and_then(|e| e.mcp().cloned())
                .map_or_else(Vec::new, |m| {
                    m.status().into_iter().map(|s| s.name).collect()
                }),
            tools: self.used_tools(),
            memory: self.engine.as_ref().map(|e| e.memory_on),
        }
    }

    pub(super) fn topology_page(&self) -> AnyElement {
        const GAP_X: f32 = 40.0;
        const GAP_Y: f32 = 48.0;
        const H_OUT: f32 = 100.0;
        const H_CORE: f32 = 150.0;
        let cards = topology::cards(&self.topology_snapshot());
        let card = |pos: Pos| {
            let c = cards.iter().find(|c| c.pos == pos).expect("card");
            v_flex()
                .flex_1()
                .min_w_0()
                .h(px(H_OUT))
                .p(px(12.))
                .gap(px(4.))
                .rounded(px(12.))
                .border_1()
                .border_color(hsla(c.color).opacity(0.6))
                .bg(hsla(0x15191E))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_semibold()
                        .text_color(hsla(c.color))
                        .child(c.title),
                )
                .children(c.lines.iter().map(|l| {
                    div()
                        .text_size(px(12.))
                        .text_color(hsla(theme::SILVER))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(l.clone())
                }))
        };
        let core = v_flex()
            .flex_1()
            .min_w_0()
            .h(px(H_CORE))
            .items_center()
            .justify_center()
            .gap(px(2.))
            .rounded(px(16.))
            .border_1()
            .border_color(hsla(0xB2A7A0))
            .bg(gpui::linear_gradient(
                135.,
                gpui::linear_color_stop(hsla(0x363338), 0.),
                gpui::linear_color_stop(hsla(0x1C2027), 1.),
            ))
            .shadow(vec![gpui::BoxShadow {
                color: hsla(0xB1A6A0).opacity(0.10),
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(32.),
                spread_radius: px(2.),
                inset: false,
            }])
            .child(img(crate::brand::mark()).size(px(44.)))
            .child(div().text_size(px(13.)).font_semibold().child("Synthesis"))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(hsla(theme::SILVER))
                    .child("Core"),
            )
            .child(
                div()
                    .mt(px(6.))
                    .text_size(px(11.))
                    .text_color(hsla(theme::MUTED))
                    .child("Reason • Act • Verify"),
            );
        let links = canvas(
            |_, _, _| {},
            move |b, _, window, _| {
                let w = f32::from(b.size.width);
                let cw = (w - 2.0 * GAP_X) / 3.0;
                let col = |i: usize| i as f32 * (cw + GAP_X);
                let y1 = H_OUT + GAP_Y;
                let y2 = y1 + H_CORE + GAP_Y;
                let at = |c: usize, y: f32, h: f32| Rect {
                    l: col(c),
                    t: y,
                    r: col(c) + cw,
                    b: y + h,
                };
                let rect = |p: Pos| match p {
                    Pos::User => at(0, 0.0, H_OUT),
                    Pos::Hooks => at(1, 0.0, H_OUT),
                    Pos::Guards => at(2, 0.0, H_OUT),
                    Pos::Sub => at(0, y1, H_CORE),
                    Pos::Skills => at(2, y1, H_CORE),
                    Pos::Mcp => at(0, y2, H_OUT),
                    Pos::Tools => at(1, y2, H_OUT),
                    Pos::Mem => at(2, y2, H_OUT),
                };
                let origin = b.origin;
                let pt = |p: (f32, f32)| gpui::point(origin.x + px(p.0), origin.y + px(p.1));
                for line in topology::links(rect, at(1, y1, H_CORE)) {
                    let mut pb = gpui::PathBuilder::stroke(px(1.25));
                    pb.move_to(pt(line[0]));
                    for p in &line[1..] {
                        pb.line_to(pt(*p));
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, hsla(0xD9D9E0).opacity(0.48));
                    }
                    for p in [line[0], line[line.len() - 1]] {
                        let c = pt(p);
                        window.paint_quad(
                            gpui::fill(
                                gpui::Bounds::new(
                                    gpui::point(c.x - px(2.5), c.y - px(2.5)),
                                    gpui::size(px(5.), px(5.)),
                                ),
                                hsla(0xF7F8FA),
                            )
                            .corner_radii(px(2.5)),
                        );
                    }
                }
            },
        )
        .absolute()
        .size_full();
        let row = |a: Pos, b: Pos, c: Pos| {
            h_flex()
                .gap(px(GAP_X))
                .child(card(a))
                .child(card(b))
                .child(card(c))
        };
        Self::page_frame("Agent Topology")
            .child(
                v_flex()
                    .relative()
                    .gap(px(GAP_Y))
                    .child(links)
                    .child(row(Pos::User, Pos::Hooks, Pos::Guards))
                    .child(
                        h_flex()
                            .gap(px(GAP_X))
                            .child(card(Pos::Sub))
                            .child(core)
                            .child(card(Pos::Skills)),
                    )
                    .child(row(Pos::Mcp, Pos::Tools, Pos::Mem)),
            )
            .into_any_element()
    }

    pub(super) fn inbox_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut pending = vec![];
        for (ti, t) in self.tabs.iter().enumerate() {
            for (i, it) in t.convo.items.iter().enumerate() {
                if matches!(
                    it,
                    Item::Approval {
                        state: ApprovalState::Pending,
                        ..
                    }
                ) && t.live
                {
                    pending.push(self.item(ti * 1000 + i, it, true, cx));
                }
            }
        }
        let t = &self.tabs[0];
        let done = !t.convo.running
            && t.convo
                .items
                .iter()
                .any(|i| matches!(i, Item::Assistant { .. }));
        let none = pending.is_empty() && !done;
        Self::page_frame("Inbox")
            .children(pending)
            .when(done, |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(hsla(theme::SUCCESS))
                        .child(format!("Task finished: {}", t.title)),
                )
            })
            .when(none, |d| d.child(empty("Nothing needs you.")))
            .into_any_element()
    }
}

fn session_row(
    id: SharedString,
    title: String,
    age: String,
    st: Status,
    on: bool,
    chips: Vec<(&'static str, String)>,
) -> gpui::Stateful<gpui::Div> {
    let chips = chips
        .into_iter()
        .map(|(ic, t)| {
            h_flex()
                .gap(px(4.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(6.))
                .bg(hsla(0x1E242B))
                .text_size(px(10.))
                .text_color(hsla(theme::SILVER))
                .child(icon(ic, 11., theme::SUCCESS))
                .child(t)
        })
        .collect::<Vec<_>>();
    let has_chips = !chips.is_empty();
    v_flex()
        .id(id)
        .gap(px(5.))
        .px(px(8.))
        .py(px(7.))
        .rounded(px(8.))
        .text_size(px(12.))
        .cursor_pointer()
        .hover(|d| d.bg(hsla(0x1A1F26)))
        .when(on, |d| d.bg(hsla(theme::NAV_ACTIVE)))
        .child(
            h_flex()
                .gap(px(8.))
                .child(dot(st))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(hsla(theme::SILVER))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(hsla(theme::MUTED))
                        .child(age),
                ),
        )
        .when(has_chips, |d| {
            d.child(h_flex().gap(px(6.)).pl(px(15.)).children(chips))
        })
}

/// What a tool card is evidence of (and what the Inspector opens on).
#[derive(Clone)]
enum Evidence {
    Diff(String),
    Terminal,
    Browser,
}
