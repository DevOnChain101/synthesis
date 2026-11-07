//! The Agent view: the session as a working surface (docs/specs/desktop.md), 1:1 with `cli.html` `#view2` in
//! the platinum palette. Left column Workspace / Session / Tools & Skills / Activity; centre the
//! conversation of the current session (tool cards, inline approvals, a sub-agent beside it) with tabs for
//! the changed files, the command history, verification, the browser and the project files; right the
//! topology, the plan and the usage. Several sessions of the workspace can be open (a tab each, an
//! engine each). Everything reads `Live` / `synthesis_view` state; there is no terminal and no PTY.

pub mod chat;
pub mod layout;
pub mod panes;
pub mod sample;
pub mod session;
#[cfg(feature = "shot")]
pub mod shot;
pub mod side;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext as _, Context, Div, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    Window, div, prelude::*, px,
};
use gpui_component::input::{InputEvent, TextareaState};
use synthesis_tui_render::theme as t;

use self::layout::{
    CHEAD_H, GAP, INPUT_H, INPUT_MARGIN, MIN_H, MIN_W, MODEBAR_H, PAD, PANEL_RADIUS, SWITCH_ROW_H,
    cols, tiers,
};
pub use self::session::{AgentSession, MAX_SESSIONS, Tree};
use crate::live::Live;
use crate::theme::hsla;
use crate::tui_view::MONO;
use crate::tui_view::driver::LiveCfg;
use crate::tui_view::panels::{dim, dot, icon, strong, text};
use crate::ui;

/// Icons the view draws (checked by a test).
pub const ICONS: [&str; 30] = [
    "layout-dashboard",
    "folder",
    "gallery-vertical-end",
    "cpu",
    "chart-pie",
    "calendar",
    "settings-2",
    "play",
    "network",
    "star",
    "file-text",
    "replace",
    "square-terminal",
    "circle-check",
    "globe",
    "folder-open",
    "file",
    "user",
    "bot",
    "plus",
    "close",
    "chevron-down",
    "chevron-right",
    "arrow-up",
    "check",
    "book-open",
    "search",
    "circle-x",
    "circle-alert",
    "info",
];

/// The panes under the conversation header (`.bottomtabs`: chat, diff, terminal, browser, file explorer;
/// the terminal is the read-only command history here, and verification is added).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Chat,
    Diff,
    Commands,
    Verify,
    Browser,
    Files,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Chat,
        Tab::Diff,
        Tab::Commands,
        Tab::Verify,
        Tab::Browser,
        Tab::Files,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Chat => "chat",
            Tab::Diff => "diff",
            Tab::Commands => "terminal",
            Tab::Verify => "verify",
            Tab::Browser => "browser",
            Tab::Files => "file explorer",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Tab::Chat => "file-text",
            Tab::Diff => "replace",
            Tab::Commands => "square-terminal",
            Tab::Verify => "circle-check",
            Tab::Browser => "globe",
            Tab::Files => "folder-open",
        }
    }

    pub fn needs_tree(self) -> bool {
        self == Tab::Files
    }
}

/// `HH:MM:SS` of epoch milliseconds, `tz_min` minutes east of UTC.
pub fn hhmmss(ms: i64, tz_min: i32) -> String {
    let s = (ms / 1000 + i64::from(tz_min) * 60).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub(crate) fn with_alpha(c: Hsla, a: f32) -> Hsla {
    Hsla { a, ..c }
}

pub struct AgentView {
    pub(crate) sessions: Vec<AgentSession>,
    pub(crate) active: usize,
    pub(crate) tab: Tab,
    /// `None` draws the sessions it was given (layout tests, `--sample`, `--shot`); no engine starts.
    pub(crate) cfg: Option<Arc<LiveCfg>>,
    /// Width taken by chrome beside the view (the shell's rail).
    inset: f32,
    /// The shell shows this view: only then the keyboard is ours.
    pub(crate) shown: bool,
    pub(crate) prompt: Entity<TextareaState>,
    pub(crate) tree: Tree,
    pub(crate) next_uid: u64,
    /// Scroll position of the dialog's option list (pickers).
    pub(crate) dlg_scroll: gpui::ScrollHandle,
    /// The view is narrower than 1000px: tabs drop their icons.
    narrow: bool,
    /// Width of the conversation column (the tab strip wraps when it is narrower).
    tabs_w: f32,
    /// Files of the diff tab that are expanded (the newest one when none was picked).
    pub(crate) diff_open: HashSet<String>,
    _tick: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl AgentView {
    fn base(
        window: &mut Window,
        cx: &mut Context<Self>,
        cfg: Option<LiveCfg>,
        lives: Vec<Live>,
        inset: f32,
    ) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask Synthesis anything...  (Shift+Enter for a new line)")
                .auto_grow(1, 6)
                .submit_on_enter(true)
        });
        let sub = cx.subscribe_in(&prompt, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter {
                secondary: false,
                shift: false,
            } = ev
            {
                this.submit_prompt(window, cx);
            }
        });
        let sessions: Vec<AgentSession> = lives
            .into_iter()
            .enumerate()
            .map(|(i, l)| AgentSession::new(i as u64 + 1, l))
            .collect();
        Self {
            narrow: false,
            tabs_w: 600.,
            dlg_scroll: gpui::ScrollHandle::new(),
            next_uid: sessions.len() as u64,
            sessions,
            active: 0,
            tab: Tab::Chat,
            cfg: cfg.map(Arc::new),
            inset,
            shown: true,
            prompt,
            tree: Tree::default(),
            diff_open: HashSet::new(),
            _tick: None,
            _subs: vec![sub],
        }
    }

    /// A view that draws the given sessions and starts nothing.
    pub fn with_sessions(
        window: &mut Window,
        cx: &mut Context<Self>,
        lives: Vec<Live>,
        inset: f32,
    ) -> Self {
        Self::base(window, cx, None, lives, inset)
    }

    /// A view on real engines: the first session starts when the shell shows the view.
    pub fn live(window: &mut Window, cx: &mut Context<Self>, cfg: LiveCfg, inset: f32) -> Self {
        let mut me = Self::base(window, cx, Some(cfg), vec![], inset);
        me.shown = false;
        let weak = cx.weak_entity();
        let key = cx.intercept_keystrokes(move |ev, _window, cx| {
            let ks = ev.keystroke.clone();
            let consumed = weak.update(cx, |t, cx| t.on_key(&ks, cx)).unwrap_or(false);
            if consumed {
                cx.stop_propagation();
            }
        });
        let quit = cx.on_app_quit(|this, _| {
            this.shutdown();
            async {}
        });
        me._subs.extend([key, quit]);
        me._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |t, cx| t.tick(cx)).is_err() {
                    break;
                }
            }
        }));
        me
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == Tab::Diff
            && self.diff_open.is_empty()
            && let Some(f) = self
                .active_session()
                .and_then(|s| s.live.view.panes.files.last())
        {
            self.diff_open.insert(f.path.clone());
        }
        if tab.needs_tree() {
            self.request_scan(cx);
        }
        cx.notify();
    }

    /// Opens the transcript of the sub-agent with this name or id beside the conversation.
    pub fn pick_sub(&mut self, name: &str) {
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.sub = s
                .live
                .view
                .panes
                .subs
                .iter()
                .find(|a| a.name == name || a.id == name)
                .map(|a| a.id.clone());
        }
    }

    fn top_bar(&self) -> Div {
        let Some(top) = self
            .active_session()
            .and_then(|s| s.snap.as_ref())
            .map(|n| &n.top)
        else {
            return crate::widgets::chrome::top_bar(&[], &[]);
        };
        let mut top = top.clone();
        top.live = self.sessions.len() as u32;
        let (left, right) = crate::tui_view::top_chips(&top);
        crate::widgets::chrome::top_bar(&left, &right)
    }

    fn footer(&self) -> Div {
        use crate::widgets::chrome::{Chip, footer};
        let s = self.active_session();
        let (files, added, removed) = s.map_or((0, 0, 0), |s| {
            let p = &s.live.view.panes;
            let (a, r) = p.totals();
            (p.files.len(), a, r)
        });
        let (ws, branch) = s.map_or_else(Default::default, |s| {
            (
                s.snap
                    .as_ref()
                    .map_or(String::new(), |n| n.top.workspace.clone()),
                s.live.view.session.git_branch.clone(),
            )
        });
        let hint = |k: &str, d: &str| (k.to_string(), d.to_string());
        let hints = [
            hint("Enter", "send"),
            hint("Shift+Enter", "newline"),
            hint("Esc", "interrupt"),
            hint("Shift+Tab", "mode"),
            hint("Ctrl+O", "expand output"),
            hint("PgUp/PgDn", "scroll"),
        ];
        let mut chips = vec![Chip::new("", ws, text())];
        if !branch.is_empty() {
            chips.push(Chip::new("", format!("\u{2387} {branch}"), text()));
        }
        let changed = format!(
            "{files} file{} changed{}",
            if files == 1 { "" } else { "s" },
            if files > 0 {
                format!(" +{added} \u{2212}{removed}")
            } else {
                String::new()
            }
        );
        chips.push(
            Chip::new("", changed, strong()).dot_colour(hsla(if files > 0 {
                t::WARN
            } else {
                t::SUCCESS
            })),
        );
        footer(&hints, &chips)
    }

    /// The header of the conversation panel (`.chead`).
    fn chead(&self) -> Div {
        let Some(s) = self.active_session() else {
            return div().h(px(CHEAD_H));
        };
        let snap = s.snap.as_ref();
        let c = &s.live.view.convo;
        let status = if s.live.starting {
            "starting"
        } else if !s.live.dialogs.is_empty() {
            "needs you"
        } else if s.live.busy || c.running {
            "working"
        } else {
            "ready"
        };
        let prompt = c
            .items
            .iter()
            .rev()
            .find_map(|i| match i {
                synthesis_view::convo::Item::User(t) => Some(t.lines().next().unwrap_or("")),
                _ => None,
            })
            .map_or_else(|| "No prompt yet".to_string(), str::to_string);
        let pct = snap.map_or(0., |n| n.context.pct);
        let tail = snap.map_or(String::new(), |n| {
            format!("{} | {}", n.head.path, n.head.stats)
        });
        crate::widgets::chrome::chat_head(status, hsla(t::SUCCESS), &prompt, pct, &tail)
    }

    /// One tab per open session, and "+" for another.
    fn session_tabs(&self, cx: &mut Context<Self>) -> Div {
        let mut row = div()
            .debug_selector(|| "session-tabs".to_string())
            .h(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .border_b_1()
            .border_color(hsla(t::RULE))
            .whitespace_nowrap()
            .overflow_hidden();
        for (i, s) in self.sessions.iter().enumerate() {
            let on = i == self.active;
            let state = if !s.live.dialogs.is_empty() {
                hsla(t::WARN)
            } else if s.live.busy || s.live.starting {
                hsla(t::ACCENT)
            } else {
                hsla(t::SUCCESS)
            };
            row = row.child(
                div()
                    .id(("stab", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .h(px(22.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(if on {
                        hsla(t::BORDER)
                    } else {
                        gpui::transparent_black()
                    })
                    .when(on, |d| d.bg(hsla(t::ELEVATED)))
                    .text_color(if on { strong() } else { text() })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(i, cx)))
                    .child(dot(state, true))
                    .child(div().max_w(px(150.)).truncate().child(s.title()))
                    .when(self.cfg.is_some(), |d| {
                        d.child(
                            div()
                                .id(("stab-x", i))
                                .child(icon("close", 12., dim()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(i, cx)
                                })),
                        )
                    }),
            );
        }
        if self.cfg.is_some() && self.sessions.len() < MAX_SESSIONS {
            row = row.child(
                div()
                    .id("stab-new")
                    .flex()
                    .items_center()
                    .px(px(6.))
                    .h(px(22.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.open(None, cx)))
                    .child(icon("plus", 14., dim())),
            );
        }
        row
    }

    /// `.bottomtabs` (the shared `chrome::tabs`).
    fn tab_strip(&self, cx: &mut Context<Self>) -> Div {
        let count = |tab: Tab| -> Option<String> {
            let p = &self.active_session()?.live.view.panes;
            match tab {
                Tab::Diff if !p.files.is_empty() => Some(p.files.len().to_string()),
                Tab::Commands if !p.commands.is_empty() => Some(p.commands.len().to_string()),
                Tab::Verify if !p.verify.is_empty() => Some(p.verify.len().to_string()),
                Tab::Browser if !p.browser.is_empty() => Some(p.browser.len().to_string()),
                _ => None,
            }
        };
        let items: Vec<crate::widgets::chrome::Tab> = Tab::ALL
            .iter()
            .map(|t| crate::widgets::chrome::Tab {
                label: t.label(),
                icon: t.icon(),
                badge: count(*t),
            })
            .collect();
        let click = |i: usize| -> crate::widgets::chrome::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| this.set_tab(Tab::ALL[i], cx)))
        };
        let on = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        crate::widgets::chrome::tabs(&items, on, !self.narrow, self.tabs_w, &click)
    }

    /// `.agent-switch`: main and the sub-agents; picking one opens its transcript beside the chat.
    fn agent_switch(&self, cx: &mut Context<Self>) -> Option<Div> {
        let s = self.active_session()?;
        let subs = &s.live.view.panes.subs;
        if subs.is_empty() {
            return None;
        }
        let row = |id: Option<String>, label: String, extra: String, on: bool, n: usize| {
            div()
                .id(("agent-opt", n))
                .h(px(SWITCH_ROW_H))
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_color(if on { strong() } else { text() })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = Tab::Chat;
                    if let Some(s) = this.active_mut() {
                        s.sub = id.clone();
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(8.))
                        .text_color(hsla(t::ACCENT))
                        .font_weight(FontWeight::BOLD)
                        .child(if on { "*" } else { "" }),
                )
                .child(label)
                .child(div().text_color(dim()).child(extra))
        };
        let mut col = div()
            .debug_selector(|| "agent-switch".to_string())
            .flex_none()
            .flex()
            .flex_col()
            .items_start()
            .mx(px(14.))
            .mb(px(6.))
            .child(row(None, "main".into(), String::new(), s.sub.is_none(), 0));
        for (i, a) in subs.iter().enumerate() {
            col = col.child(row(
                Some(a.id.clone()),
                a.name.clone(),
                a.status.clone(),
                s.sub.as_deref() == Some(a.id.as_str()),
                i + 1,
            ));
        }
        Some(col)
    }

    /// `.inputbar`
    fn input_bar(&self, cx: &mut Context<Self>) -> Div {
        let busy = self
            .active_session()
            .is_some_and(|s| s.live.busy || s.live.starting);
        let queued = self.active_session().map_or(0, |s| s.live.queue.len());
        div()
            .flex_none()
            .flex()
            .flex_col()
            .when(queued > 0, |d| {
                d.child(
                    div()
                        .mx(px(INPUT_MARGIN))
                        .mb(px(2.))
                        .text_color(dim())
                        .child(format!(
                            "{queued} queued: starts when the task ends (Esc drops it)"
                        )),
                )
            })
            .child(
                div()
                    .debug_selector(|| "prompt".to_string())
                    .mx(px(INPUT_MARGIN))
                    .mb(px(INPUT_MARGIN))
                    .min_h(px(INPUT_H))
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(10.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .rounded(px(PANEL_RADIUS))
                    .bg(with_alpha(hsla(t::SURFACE), 0.9))
                    .child(div().mt(px(1.)).text_color(strong()).child(if busy {
                        "\u{2026}"
                    } else {
                        ">"
                    }))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            gpui_component::input::Textarea::new(&self.prompt)
                                .appearance(false)
                                .bordered(false),
                        ),
                    )
                    .child(
                        div()
                            .id("send")
                            .mt(px(2.))
                            .cursor_pointer()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_prompt(window, cx)),
                            )
                            .child(icon("arrow-up", 14., dim())),
                    ),
            )
    }

    /// `.modebar`
    fn mode_bar(&self) -> Div {
        let (mode, model, effort) = self
            .active_session()
            .and_then(|s| s.snap.as_ref())
            .map_or_else(Default::default, |n| {
                (n.mode.clone(), n.top.model.clone(), n.effort.clone())
            });
        div()
            .flex_none()
            .h(px(MODEBAR_H))
            .mx(px(14.))
            .px(px(12.))
            .mb(px(8.))
            .flex()
            .items_center()
            .gap(px(5.))
            .whitespace_nowrap()
            .text_size(px(11.5))
            .child(
                div()
                    .text_color(hsla(t::WARN))
                    .child(format!("\u{23F5}\u{23F5} {mode} mode on")),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(dim())
                    .child("(shift+tab to cycle)"),
            )
            .child(div().flex_1())
            .child(div().text_color(dim()).child("model"))
            .child(div().text_color(hsla(t::INFO)).child(model))
            .child(div().text_color(dim()).child("\u{00B7} effort"))
            .child(div().text_color(hsla(t::ACCENT)).child(effort))
    }

    fn centre(&self, w: f32, cx: &mut Context<Self>) -> Div {
        let body = match self.tab {
            Tab::Chat => self.chat_stage(w, cx).into_any_element(),
            other => self.pane(other, cx),
        };
        let c = cols(w);
        div()
            .debug_selector(|| "col-centre".to_string())
            .flex_1()
            .min_w(px(c.centre_min))
            .flex()
            .flex_col()
            .min_h(px(0.))
            .child(
                div()
                    .debug_selector(|| "p-chat".to_string())
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(hsla(t::SURFACE))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .rounded(px(PANEL_RADIUS))
                    .child(self.chead())
                    .child(self.session_tabs(cx))
                    .child(self.tab_strip(cx))
                    .child(body)
                    .child(self.input_bar(cx))
                    .children(self.agent_switch(cx))
                    .child(self.mode_bar()),
            )
    }
}

impl Render for AgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh();
        let vp = window.viewport_size();
        let w = f32::from(vp.width) - self.inset;
        let h = f32::from(vp.height);
        let tier = tiers(h);
        self.narrow = w < 1000.;
        let c = cols(w);
        self.tabs_w = w - 2. * PAD - 2. * GAP - c.left - c.right - 2.;
        let body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .gap(px(GAP))
            .p(px(PAD))
            .child(self.left(tier, w, cx))
            .child(self.centre(w, cx))
            .child(self.right(tier, w));
        let dialog = self.active_session().and_then(|s| {
            crate::widgets::dialog::overlay(&s.live, &self.dlg_scroll, &|i| {
                Box::new(cx.listener(move |this, _, _, cx| this.click_option(i, cx)))
            })
        });
        div()
            .debug_selector(|| "view".to_string())
            .relative()
            .size_full()
            .min_w(px(MIN_W))
            .min_h(px(MIN_H))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(hsla(t::BG))
            .font_family(MONO)
            .font_weight(FontWeight::MEDIUM)
            .text_size(px(12.5))
            .line_height(gpui::relative(1.55))
            .text_color(text())
            .child(self.top_bar())
            .child(body)
            .child(self.footer())
            .children(dialog)
    }
}

/// The shell's rail width, re-exported so the tests build the view the way the shell does.
pub const RAIL_W: f32 = ui::RAIL_W;

#[cfg(test)]
mod tests {
    use super::layout::*;
    use super::*;
    use crate::brand;
    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, px, size};

    const SIZES: [(f32, f32); 5] = [
        (900., 520.),
        (1000., 700.),
        (1200., 800.),
        (1400., 900.),
        (1920., 1080.),
    ];

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
    }

    fn open(cx: &mut TestAppContext, w: f32, h: f32, lives: Vec<Live>) -> &mut VisualTestContext {
        init(cx);
        let (_, vcx) =
            cx.add_window_view(|window, cx| AgentView::with_sessions(window, cx, lives, 0.));
        vcx.simulate_resize(size(px(w), px(h)));
        vcx.run_until_parked();
        vcx
    }

    fn b(cx: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("no element {sel}"))
    }
    fn f(p: Pixels) -> f32 {
        f32::from(p)
    }
    fn near(a: f32, e: f32, what: &str) {
        assert!((a - e).abs() < 0.6, "{what}: {a} != {e}");
    }

    #[gpui::test]
    fn columns_and_bands_match_the_design(cx: &mut TestAppContext) {
        for (w, h) in SIZES {
            let c = cols(w);
            let v = open(cx, w, h, vec![sample::live()]);
            let at = format!("{w}x{h}");
            near(
                f(b(v, "header").size.height),
                HEADER_H,
                &format!("{at} header"),
            );
            near(
                f(b(v, "footer").size.height),
                FOOTER_H,
                &format!("{at} footer"),
            );
            near(
                f(b(v, "footer").origin.y),
                h - FOOTER_H,
                &format!("{at} footer y"),
            );
            let (l, m, r) = (b(v, "col-left"), b(v, "col-centre"), b(v, "col-right"));
            near(f(l.size.width), c.left, &format!("{at} left"));
            near(f(r.size.width), c.right, &format!("{at} right"));
            near(
                f(m.size.width),
                w - 2. * PAD - 2. * GAP - c.left - c.right,
                &format!("{at} centre"),
            );
            assert!(f(m.size.width) >= c.centre_min, "{at} centre min");
            near(f(l.origin.x), PAD, &format!("{at} left x"));
            near(f(m.origin.x), PAD + c.left + GAP, &format!("{at} centre x"));
            near(f(r.origin.x), w - PAD - c.right, &format!("{at} right x"));
            for col in [l, m, r] {
                near(f(col.origin.y), HEADER_H + PAD, &format!("{at} col y"));
                near(
                    f(col.size.height),
                    h - HEADER_H - FOOTER_H - 2. * PAD,
                    &format!("{at} col h"),
                );
            }
        }
    }

    #[gpui::test]
    fn the_conversation_panel_has_the_designs_bands(cx: &mut TestAppContext) {
        for (w, h) in SIZES {
            let v = open(cx, w, h, vec![sample::live()]);
            let at = format!("{w}x{h}");
            let chat = b(v, "p-chat");
            near(
                f(b(v, "chead").size.height),
                CHEAD_H,
                &format!("{at} chead"),
            );
            near(f(b(v, "tabs").size.height), TABS_H, &format!("{at} tabs"));
            assert!(
                f(b(v, "prompt").size.height) >= INPUT_H - 0.6,
                "{at} prompt"
            );
            // the prompt and the mode line sit at the bottom of the panel, inside it
            assert!(
                f(b(v, "prompt").bottom()) <= f(chat.bottom()),
                "{at} prompt inside"
            );
            assert!(
                f(b(v, "chat-scroll").size.height) > 40.,
                "{at} the conversation keeps room: {}",
                f(b(v, "chat-scroll").size.height)
            );
            // the sub-agent switch lists main and the two sub-agents of the sample
            near(
                f(b(v, "agent-switch").size.height),
                3. * SWITCH_ROW_H,
                &format!("{at} switch"),
            );
        }
    }

    #[gpui::test]
    fn side_panels_respect_the_height_tiers_and_fill_their_columns(cx: &mut TestAppContext) {
        for (w, h) in SIZES {
            let t = tiers(h);
            let v = open(cx, w, h, vec![sample::live()]);
            let at = format!("{w}x{h}");
            let col = b(v, "col-left");
            let mut bottom = 0.;
            for id in ["p-workspace", "p-session", "p-tools", "p-activity"] {
                let p = b(v, id);
                assert!(
                    f(p.size.height) >= t.left_min - 0.6,
                    "{at} {id} {} < {}",
                    f(p.size.height),
                    t.left_min
                );
                bottom = f(p.bottom());
            }
            assert!(bottom <= f(col.bottom()) + 0.6, "{at} left overflows");
            let (tp, us, pl) = (b(v, "p-topology"), b(v, "p-usage"), b(v, "p-plan"));
            assert!(
                f(tp.size.height) >= t.topo_min - 0.6,
                "{at} topology {}",
                f(tp.size.height)
            );
            near(
                f(tp.size.height) + f(pl.size.height) + f(us.size.height) + 2. * GAP,
                f(b(v, "col-right").size.height),
                &format!("{at} right fills"),
            );
        }
    }

    #[gpui::test]
    fn a_waiting_approval_opens_the_shared_dialog_over_the_view(cx: &mut TestAppContext) {
        let v = open(cx, 1400., 900., vec![sample::live_with_dialog()]);
        let d = b(v, "dialog");
        assert!(f(d.size.width) <= 680.6);
        let view = b(v, "view");
        assert!(d.origin.x >= view.origin.x && f(d.bottom()) <= f(view.bottom()));
        let v = open(cx, 1400., 900., vec![sample::live()]);
        assert!(v.debug_bounds("dialog").is_none());
    }

    #[gpui::test]
    fn every_tab_draws_inside_the_conversation_panel(cx: &mut TestAppContext) {
        init(cx);
        for tab in Tab::ALL {
            let (view, vcx) = cx.add_window_view(|window, cx| {
                AgentView::with_sessions(window, cx, vec![sample::live()], 0.)
            });
            vcx.simulate_resize(size(px(1200.), px(800.)));
            view.update(vcx, |v, cx| v.set_tab(tab, cx));
            vcx.run_until_parked();
            let chat = b(vcx, "p-chat");
            let body = b(vcx, "tab-body");
            assert!(
                f(body.size.height) > 60. && f(body.bottom()) <= f(chat.bottom()) + 0.6,
                "{tab:?}: {body:?} in {chat:?}"
            );
        }
    }

    #[test]
    fn the_clock_shows_local_time() {
        assert_eq!(hhmmss(0, 0), "00:00:00");
        assert_eq!(hhmmss(3_723_000, 0), "01:02:03");
        assert_eq!(hhmmss(0, -60), "23:00:00");
        assert_eq!(hhmmss(0, 420), "07:00:00");
    }

    #[test]
    fn every_icon_the_view_draws_loads() {
        use gpui::AssetSource as _;
        for name in ICONS {
            assert!(
                brand::Assets
                    .load(&format!("icons/{name}.svg"))
                    .unwrap()
                    .is_some(),
                "{name}"
            );
        }
    }
}
