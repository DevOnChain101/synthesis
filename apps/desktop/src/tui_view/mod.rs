//! The TUI view: the classic TUI's dashboard drawn natively (docs/specs/desktop.md), rebuilt 1:1 from
//! `cli.html` in the platinum palette. Top bar; left column Workspace / Running / Session / Tools & Skills /
//! Recent Activity (prompts and agent activity in one list); centre panel (header, tabs chat | diff | terminal | browser | file
//! explorer, conversation, prompt, transcript switch, mode bar); right column Synthesis Topology, Agent
//! Status + Context, Usage & Metrics; key hint footer. It reads a `Snapshot` (see `sample`); no terminal, no
//! PTY. Panels hide with the TUI's `panels.*` keys (`layout::Hidden`).

pub mod activity;
pub mod chat;
mod clean;
#[cfg(test)]
mod design_tests;
pub mod dialog;
pub mod driver;
pub mod layout;
pub mod panels;
mod quote;
mod rewind;
pub mod sample;
#[cfg(feature = "shot")]
pub mod shot;
pub mod snapshot;
pub mod tabs;
pub mod topo;

use gpui::{
    Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use synthesis_tui_render::theme as t;

use self::layout::{CARD_GAP, FOOTER_H, GAP, HEADER_H, LEFT_GAP, MIN_H, MIN_W, PAD, cols, tiers};
use self::panels::{dim, icon, strong, text};
use self::sample::{Snapshot, sample};
use crate::theme::hsla;
use crate::widgets::chrome::{self, Chip, Head, Pad, panel, panel_scroll};
use gpui::prelude::FluentBuilder as _;

pub use crate::widgets::chrome::tracked;

/// Black at `a` alpha (the tool card's `rgba(9,10,13,.6)`).
pub(crate) fn with_alpha_black(a: f32) -> Hsla {
    chrome::black_a(a)
}

pub(crate) fn with_alpha_white(a: f32) -> Hsla {
    chrome::with_alpha(gpui::white(), a)
}

/// The TUI view's font: `cli.html` `--mono` (bundled Fira Code, SIL OFL), at its body weight 500.
/// Tools & Skills body: five 21.4px rows (`li` 1+19.4+1) and the 2px top and 6px bottom padding; the rest scrolls.
const TOOLS_MAX_H: f32 = 5. * 21.4 + 8.;
pub const MONO: &str = crate::brand::MONO_FONT;

/// Icons the view draws (checked by a test).
pub const ICONS: [&str; 27] = [
    "book-open",
    "search",
    "replace",
    "folder",
    "plus",
    "git-branch",
    "chevron-down",
    "chevrons-right",
    "activity",
    "square-terminal",
    "wrench",
    "clock",
    "network",
    "sparkles",
    "cpu",
    "chart-bar",
    "message-square",
    "globe",
    "send",
    "user",
    "webhook",
    "shield",
    "users",
    "layers",
    "link",
    "database",
    "arrow-up",
];

/// The tabs of the centre panel (`STATE.bottomTabs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ChatTab {
    #[default]
    Chat,
    Diff,
    Terminal,
    Browser,
    Files,
}

fn t_mode_name(m: synthesis_permissions::Mode) -> String {
    synthesis_tui_render::dialogs::mode_name(Some(m.as_str()))
}

impl ChatTab {
    pub const ALL: [ChatTab; 5] = [
        ChatTab::Chat,
        ChatTab::Diff,
        ChatTab::Terminal,
        ChatTab::Browser,
        ChatTab::Files,
    ];

    fn tab(self) -> chrome::Tab {
        match self {
            ChatTab::Chat => chrome::Tab {
                label: "chat",
                icon: "message-square",
                badge: None,
            },
            ChatTab::Diff => chrome::Tab {
                label: "diff",
                icon: "git-branch",
                badge: None,
            },
            ChatTab::Terminal => chrome::Tab {
                label: "terminal",
                icon: "square-terminal",
                badge: None,
            },
            ChatTab::Browser => chrome::Tab {
                label: "browser",
                icon: "globe",
                badge: None,
            },
            ChatTab::Files => chrome::Tab {
                label: "file explorer",
                icon: "folder",
                badge: None,
            },
        }
    }
}

pub struct TuiView {
    snap: Snapshot,
    /// The Topology activity of this frame (set by `render`, read by `right`) and the time it was taken at.
    topo_acts: std::collections::BTreeMap<synthesis_view::pulse::Node, synthesis_view::pulse::Act>,
    topo_now: i64,
    /// `--shot --active tools,mcp`: these nodes are held open at this time (ms) instead of the live clock.
    pub(crate) topo_force: Option<(Vec<synthesis_view::pulse::Node>, i64)>,
    /// The pending repaint of the animation (about 30 per second, only while a node is active).
    topo_tick: Option<gpui::Task<()>>,
    /// The timer of the next frame of a spinner or a blink (see `chrome::next_frame`).
    anim_tick: Option<gpui::Task<()>>,
    /// The engine, the chat and the prompt; `None` draws the sample (layout tests, `--sample`, `--shot`).
    live: Option<driver::LiveUi>,
    hidden: layout::Hidden,
    /// Width taken by chrome beside the view (the shell's rail): breakpoints use the view's own width.
    inset: f32,
    /// Height taken above the view by the window's title bar; the height tiers use the view's own height.
    top: f32,
    /// The tabs of the session shown (the pane under the centre header and what it has open), and those of the
    /// sessions left, kept by session id.
    pub(crate) ts: tabs::TabState,
    tab_sid: String,
    tabs_saved: std::collections::HashMap<String, tabs::TabState>,
    /// The project tree of the file explorer tab.
    tree: tabs::Tree,
    pane_scroll: gpui::ScrollHandle,
    sub_scroll: gpui::ScrollHandle,
    /// Width of the centre column, from the last render (the tab strip wraps when it is narrower).
    centre_w: f32,
    /// False while another view of the shell is shown: the window-wide key hook then leaves the keys alone.
    pub active: bool,
    /// True while the TUI view itself is on screen (not under Settings): the only time a dialog gets keys.
    pub shown: bool,
    /// Height of the window from the last render (the dialog panel's input box takes up to 40% of it).
    win_h: f32,
    /// Scroll handles of the Tools & Skills and Recent Activity bodies (the fade shows when one is cut).
    tools_scroll: gpui::ScrollHandle,
    act_scroll: gpui::ScrollHandle,
    /// Height of the Topology panel from the last frame, and what the Usage month section took from it then:
    /// together they say how much the column can give the section now (`layout::month_mode`).
    topo_h: std::rc::Rc<std::cell::Cell<f32>>,
    month_used: std::cell::Cell<f32>,
    /// The slide-over column that is open (only a collapsed column has one; closed by default).
    pub(crate) drawer: Option<Side>,
    /// Side-panel sections folded to their header (by panel id), kept for the session.
    folded: std::collections::HashSet<&'static str>,
}

/// A side column of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// The top bar's chips: live count, workspace, session, model, effort, context on the left; health and the
/// clock on the right (shared with the Agent view).
pub fn top_chips(top: &sample::Top) -> (Vec<Chip>, Vec<Chip>) {
    (
        vec![
            Chip::new("", format!("{} live", top.live), hsla(t::SUCCESS)).dotted(),
            Chip::new("workspace:", top.workspace.clone(), hsla(t::INFO)),
            Chip::new("session:", top.session.clone(), hsla(t::ACCENT)),
            Chip::new("model:", top.model.clone(), hsla(t::INFO)),
            Chip::new("effort:", top.effort.clone(), hsla(t::ACCENT_LIGHT)),
            Chip::new("context:", top.ctx.clone(), hsla(t::SUCCESS)),
        ],
        vec![
            Chip::new("", top.health.clone(), hsla(t::SUCCESS)).dotted(),
            Chip::new("", top.clock.clone(), text()),
        ],
    )
}

impl TuiView {
    pub fn new(inset: f32, hidden: layout::Hidden) -> Self {
        Self {
            active: true,
            shown: true,
            win_h: 900.,
            tools_scroll: gpui::ScrollHandle::new(),
            act_scroll: gpui::ScrollHandle::new(),
            topo_h: Default::default(),
            month_used: Default::default(),
            snap: sample(),
            topo_acts: Default::default(),
            topo_now: 0,
            topo_force: None,
            topo_tick: None,
            anim_tick: None,
            live: None,
            hidden,
            inset,
            top: 0.,
            ts: Default::default(),
            tab_sid: String::new(),
            tabs_saved: Default::default(),
            tree: Default::default(),
            pane_scroll: gpui::ScrollHandle::new(),
            sub_scroll: gpui::ScrollHandle::new(),
            centre_w: 600.,
            drawer: None,
            folded: Default::default(),
        }
    }

    /// Opens the drawer of `side`, or closes it when it is the open one.
    pub fn toggle_drawer(&mut self, side: Side, cx: &mut Context<Self>) {
        // not while a dialog is pending: the panel above the prompt must stay in reach
        if self
            .live
            .as_ref()
            .is_some_and(|l| !l.live.dialogs.is_empty())
        {
            return;
        }
        self.drawer = if self.drawer == Some(side) {
            None
        } else {
            Some(side)
        };
        cx.notify();
    }

    /// The title bar above the view (see `ui::TITLEBAR_H`).
    pub fn set_top_inset(&mut self, h: f32) {
        self.top = h;
    }

    /// Before drawing: rebuild the snapshot from the live state when it changed, and keep the chat
    /// stuck to the newest row unless the person scrolled up.
    fn refresh(&mut self, window: &mut Window) {
        let Some(lu) = &mut self.live else { return };
        let _ = window;
        // the tabs belong to the session: leaving one keeps them, coming back finds them
        if lu.live.sid != self.tab_sid && !lu.live.sid.is_empty() {
            let prev = std::mem::replace(&mut self.tab_sid, lu.live.sid.clone());
            // the first session keeps what was set before it existed (a shot's tab); a later one swaps
            if !prev.is_empty() {
                let old = std::mem::take(&mut self.ts);
                self.tabs_saved.insert(prev, old);
                self.ts = self.tabs_saved.remove(&self.tab_sid).unwrap_or_default();
                self.tree = Default::default();
            }
        }
        if lu.dirty {
            lu.dirty = false;
            let _t = crate::trace::span(&lu.cfg.env, "frame: snapshot + rows (UI thread)");
            let _perf = crate::perf::timed(false);
            self.snap = snapshot::build(
                &lu.live,
                crate::live::now_ms(),
                lu.cfg.tz_min,
                lu.cfg.home.as_deref(),
            );
            lu.sync_rows();
        }
        lu.follow = lu.list.is_following_tail();
    }

    /// The top bar; a collapsed side column gets a button at the right end that opens its drawer.
    fn top_bar(&self, w: f32, left_btn: bool, right_btn: bool, cx: &mut Context<Self>) -> Div {
        let (left, right) = top_chips(&self.snap.top);
        // what the row holds besides the left chips: padding, brand mark and name, the gap after them, the right
        // chips, the drawer buttons and a little slack
        let fixed = chrome::top_fixed(usize::from(left_btn) + usize::from(right_btn));
        let left = chrome::fit_chips(
            left,
            w - fixed - right.iter().map(chrome::chip_w).sum::<f32>(),
        );
        let btn = |side: Side, id: &'static str, glyph: &'static str, cx: &mut Context<Self>| {
            div()
                .id(id)
                .debug_selector(|| id.to_string())
                .w(px(26.))
                .h(px(22.))
                .ml(px(6.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(self.drawer == Some(side), |d| d.bg(chrome::black_a(0.25)))
                .hover(|d| d.bg(chrome::black_a(0.25)))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_drawer(side, cx)))
                .child(icon(glyph, 14., text()))
        };
        chrome::top_bar(&left, &right)
            .when(left_btn, |d| {
                d.child(btn(Side::Left, "drawer-left", "folder", cx))
            })
            .when(right_btn, |d| {
                d.child(btn(Side::Right, "drawer-right", "network", cx))
            })
    }

    fn footer(&self) -> Div {
        let chips: Vec<Chip> = self
            .snap
            .status
            .iter()
            .map(|c| {
                if c.dot {
                    Chip::new("", c.text.clone(), strong()).dot_colour(hsla(t::WARN))
                } else {
                    Chip::new("", c.text.clone(), text())
                }
            })
            .collect();
        chrome::footer(&self.snap.hints, &chips)
    }

    fn is_folded(&self, id: &str) -> bool {
        self.folded.contains(id)
    }

    /// The click on a section's title: folds it, or opens it again.
    fn fold_cb(&self, id: &'static str, cx: &Context<Self>) -> panels::OnClick {
        Box::new(cx.listener(move |this, _, _, cx| {
            if !this.folded.remove(id) {
                this.folded.insert(id);
            }
            cx.notify();
        }))
    }

    /// A folded section is just its header; an open one gets its usual flex.
    fn fit(&self, p: Div, id: &str, open: impl FnOnce(Div) -> Div) -> Div {
        if self.is_folded(id) {
            p.flex_none().min_h(px(0.))
        } else {
            open(p)
        }
    }

    fn left(&self, tier: layout::Tiers, w: f32, cx: &mut Context<Self>) -> Div {
        let (s, h) = (&self.snap, &self.hidden);
        let split = h.left_is_split();
        let min = px(tier.left_min);
        let live = self.live.as_ref();
        let focus = live.map(|l| l.live.focus);
        let ws_click = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| this.click_workspace(i, cx)))
        };
        let ws_close = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| {
                // the button sits inside the row: the row's own click must not follow
                cx.stop_propagation();
                this.click_close_workspace(i, cx)
            }))
        };
        let run_click = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, window, cx| this.click_running(i, window, cx)))
        };
        let new_ws: Option<panels::OnClick> = live.map(|_| -> panels::OnClick {
            Box::new(cx.listener(|this, _, _, cx| this.new_workspace(cx)))
        });
        let new_run: Option<panels::OnClick> = live.map(|_| -> panels::OnClick {
            Box::new(cx.listener(|this, _, window, cx| {
                this.new_session(window, cx);
            }))
        });
        let meta = |c: Hsla, v: String| div().text_color(c).child(v).into_any_element();
        // the session that waits for approval is marked red in both lists
        let attn = self.attention_colour();
        let pick = |p: Div, focused: bool| p.when(focused, |d| d.border_color(hsla(t::ACCENT)));
        let ws = pick(
            panel_scroll(
                Head::new("p-workspace", "Workspace", "folder", Pad::LEFT).fold(
                    self.is_folded("p-workspace"),
                    self.fold_cb("p-workspace", cx),
                ),
                Some(meta(text(), s.workspaces.len().to_string())),
                panels::workspace_body(
                    &s.workspaces,
                    attn,
                    live.map(|_| &ws_click as &dyn Fn(usize) -> panels::OnClick),
                    live.map(|_| &ws_close as &dyn Fn(usize) -> panels::OnClick),
                    new_ws,
                ),
                live.map(|l| &l.ws_scroll),
            ),
            focus == Some(crate::live::Focus::Workspace),
        );
        let running_n = s.running.iter().filter(|r| r.live).count();
        let run = pick(
            panel_scroll(
                Head::new("p-running", "Running", "activity", Pad::LEFT)
                    .fold(self.is_folded("p-running"), self.fold_cb("p-running", cx)),
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_color(hsla(t::SUCCESS))
                        .child(panels::dot(hsla(t::SUCCESS), true))
                        .child(running_n.to_string())
                        .into_any_element(),
                ),
                panels::running_body(
                    &s.running,
                    attn,
                    self.live.as_ref().is_none_or(|l| l.cfg.tui.motion),
                    crate::live::now_ms(),
                    live.map(|_| &run_click as &dyn Fn(usize) -> panels::OnClick),
                    new_run,
                ),
                live.map(|l| &l.run_scroll),
            ),
            focus == Some(crate::live::Focus::Running),
        );
        let mut col = div()
            .debug_selector(|| "col-left".to_string())
            .w(px(cols(w).left))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(LEFT_GAP))
            .min_h(px(0.))
            .overflow_hidden();
        // `#view1 .col.l>.panel{flex:0 1 auto;min-height:84px}`; Workspace and Running split the column
        // in halves when everything below them is hidden
        col = col.child(self.fit(ws, "p-workspace", |p| {
            if split {
                p.flex_1().min_h(px(0.))
            } else {
                p.flex_initial().min_h(min)
            }
        }));
        col = col.child(self.fit(run, "p-running", |p| {
            if split {
                p.flex_1().min_h(px(0.))
            } else {
                p.flex_initial().min_h(min)
            }
        }));
        if !h.session {
            // Session always shows all its rows (`:has(#h-sess){flex:0 0 auto;min-height:0}`)
            col = col.child(
                panel(
                    Head::new("p-session", "Session", "square-terminal", Pad::LEFT)
                        .fold(self.is_folded("p-session"), self.fold_cb("p-session", cx)),
                    Some(panels::session_meta(&s.session)),
                    panels::session_body(&s.session),
                )
                .flex_none(),
            );
        }
        if !h.tools {
            let p = panel_scroll(
                Head::new("p-tools", "Tools & Skills", "wrench", Pad::LEFT)
                    .max_body(TOOLS_MAX_H)
                    .fold(self.is_folded("p-tools"), self.fold_cb("p-tools", cx)),
                Some(meta(text(), s.tools.count.clone())),
                panels::tools_body(&s.tools),
                Some(&self.tools_scroll),
            );
            col = col.child(self.fit(p, "p-tools", |p| p.flex_initial().min_h(min)));
        }
        if !h.activity {
            // one `.grow` panel (flex 1 1 0) for the prompts and the agent's activity together
            let act_click = |i: usize| -> panels::OnClick {
                Box::new(cx.listener(move |this, _, _, cx| this.click_activity(i, cx)))
            };
            let p = panel_scroll(
                Head::new("p-activity", "Recent Activity", "activity", Pad::LEFT)
                    .fold(self.is_folded("p-activity"), self.fold_cb("p-activity", cx)),
                Some(meta(text(), s.activity.len().to_string())),
                panels::activity_body(
                    &s.activity,
                    live.map(|_| &act_click as &dyn Fn(usize) -> panels::OnClick),
                ),
                Some(&self.act_scroll),
            );
            // as tall at least as the design's two panels it replaces (Recent Activity and User Prompt)
            col = col.child(self.fit(p, "p-activity", |p| {
                p.flex_1()
                    .min_h(px(tier.left_min + LEFT_GAP + tier.activity_min))
            }));
        }
        col
    }

    fn centre(&self, cx: &mut Context<Self>) -> Div {
        div()
            .debug_selector(|| "col-centre".to_string())
            .flex_1()
            .flex()
            .flex_col()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(self.chat(cx))
    }

    fn chat_head(&self) -> Div {
        let h = &self.snap.head;
        chrome::chat_head(
            &h.status,
            hsla(t::SUCCESS),
            &h.prompt,
            h.ctx_pct,
            &chrome::header_tail(&h.path, &h.stats, &h.status, self.centre_w),
            self.centre_w,
            self.live.as_ref().is_some_and(|l| l.cfg.tui.motion),
        )
    }

    /// `.agent-switch`: shown only while the session has sub-agents. `* main`, then a row per sub-agent, CLI style:
    /// `name - state` at the left, `online - tokens` at the far right; the chosen one is marked with `*`, and
    /// picking one opens its transcript beside the chat. Rows come and go with `synthesis_view::lifecycle`.
    fn agent_switch(&self, cx: &mut Context<Self>) -> Div {
        use synthesis_view::lifecycle::online;
        if self.snap.roster.is_empty() {
            return div();
        }
        // (id, label, state, stats)
        let mut rows: Vec<(Option<String>, String, String, String)> =
            vec![(None, "main".into(), String::new(), String::new())];
        rows.extend(self.snap.roster.iter().map(|r| {
            (
                Some(r.id.clone()),
                r.name.clone(),
                format!(" - {}", r.state),
                format!(
                    "{} - {} tokens",
                    online(r.online_ms),
                    snapshot::tok(r.tokens)
                ),
            )
        }));
        div()
            .debug_selector(|| "agent-switch".to_string())
            .flex_none()
            .mx(px(14.))
            .mt(px(-4.))
            .mb(px(8.))
            .p(px(4.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(i, (id, name, state, stats))| {
                        let on = self.ts.sub == id;
                        let pick = id.clone();
                        div()
                            .id(("agent-opt", i))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(20.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .whitespace_nowrap()
                            .text_color(if on { strong() } else { text() })
                            .hover(|d| d.text_color(strong()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ts.sub = pick.clone();
                                this.ts.tab = ChatTab::Chat;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(8.))
                                    .flex_none()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(hsla(t::ACCENT))
                                    .child(if on { "*" } else { "" }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .child(format!("{name}{state}")),
                            )
                            .child(div().flex_none().text_color(dim()).child(stats))
                    }),
            )
    }

    fn mode_bar(&self) -> Div {
        let s = &self.snap;
        // the mode the engine runs (read back, not the snapshot's copy); no engine: the sample's
        let engine_mode = self
            .live
            .as_ref()
            .and_then(|l| l.live.engine.as_ref())
            .map(|e| e.mode());
        let (mode_col, mode_icon, mode_label) = match engine_mode {
            Some(m) => {
                let (c, i) = crate::theme::mode_look(m);
                (hsla(c), i, t_mode_name(m))
            }
            None => (hsla(t::WARN), "chevrons-right", s.mode.clone()),
        };
        div()
            .debug_selector(|| "modebar".to_string())
            .flex_none()
            .mx(px(14.))
            .mt(px(-6.))
            .mb(px(10.))
            .px(px(12.))
            .h(px(22.))
            .flex()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(11.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .min_w(px(0.))
                    .text_color(mode_col)
                    .child(icon(mode_icon, 14., mode_col))
                    .child(div().flex_none().child(format!("{mode_label} mode on")))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(dim())
                            .child("(shift+tab to cycle)"),
                    ),
            )
            .child(div().flex_1().min_w(px(6.)))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(4.))
                    .child(div().text_color(dim()).child("model"))
                    .child(div().text_color(hsla(t::INFO)).child(s.top.model.clone())),
            )
            .child(div().text_color(dim()).child("\u{00B7}"))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(4.))
                    .child(div().text_color(dim()).child("effort"))
                    .child(
                        div()
                            .text_color(hsla(t::ACCENT_LIGHT))
                            .child(s.effort.clone()),
                    ),
            )
    }

    fn chat(&self, cx: &mut Context<Self>) -> Div {
        let s = &self.snap;
        let tabs: Vec<chrome::Tab> = ChatTab::ALL
            .iter()
            .map(|t| chrome::Tab {
                badge: self.badge(*t),
                ..t.tab()
            })
            .collect();
        let tab_click = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| {
                this.select_tab(ChatTab::ALL[i], cx);
            }))
        };
        let strip = chrome::tabs(
            &tabs,
            ChatTab::ALL
                .iter()
                .position(|t| *t == self.ts.tab)
                .unwrap_or(0),
            true,
            self.centre_w - 2.,
            &tab_click,
        );
        let body = if self.ts.tab != ChatTab::Chat {
            self.pane(cx)
        } else if self.live.is_some() {
            self.chat_stage(cx)
        } else {
            self.welcome().into_any_element()
        };
        let body = div()
            .debug_selector(|| "tab-body".to_string())
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .px(px(layout::TAB_PAD_X))
            .child(body);
        let dock = self.dialog_dock(cx);
        let prompt = if self.live.is_some() {
            self.live_prompt(cx)
        } else {
            self.input_bar(cx)
        };
        let _ = s;
        div()
            .debug_selector(|| "p-chat".to_string())
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(crate::theme::fill(t::SURFACE))
            .border_1()
            // one focus frame around the whole chat panel (the border is always there, only its colour
            // changes); a waiting approval's red wins over it
            .border_color(self.attention_colour().unwrap_or_else(|| {
                hsla(
                    if self
                        .live
                        .as_ref()
                        .is_some_and(|l| l.live.focus == crate::live::Focus::Chat)
                    {
                        t::ACCENT
                    } else {
                        t::BORDER
                    },
                )
            }))
            .rounded(px(layout::PANEL_RADIUS))
            .child(self.chat_head())
            .child(strip)
            .child(body)
            .children(dock)
            .child(prompt)
            .children(self.quote_chips(cx))
            .child(self.agent_switch(cx))
            .child(self.mode_bar())
    }

    /// The input bar of a view without an engine (sample, `--shot`) or under another tab: placeholder,
    /// send button. The live one is `live_prompt`.
    fn input_bar(&self, _cx: &mut Context<Self>) -> Div {
        chat::input_box(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(dim())
                .child("Ask Synthesis anything... (Ctrl+R for history)"),
            false,
            false,
            None,
        )
    }

    fn right(&self, tier: layout::Tiers, w: f32, h: f32, cx: &mut Context<Self>) -> Div {
        let (s, hid) = (&self.snap, &self.hidden);
        // the column's own height decides what gives way (`fitRight`): Usage goes under 500, the cards
        // are compacted under 400
        let col_h = h - HEADER_H - FOOTER_H - 2. * PAD;
        let (nousage, tight) = (col_h < 500., col_h < 400.);
        let mut col = div()
            .debug_selector(|| "col-right".to_string())
            .w(px(cols(w).right))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(GAP))
            .min_h(px(0.))
            .overflow_hidden();
        if !hid.topology {
            let pad = if tier.compact_head {
                Pad::COMPACT
            } else {
                Pad::NORMAL
            };
            let online = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(hsla(t::SUCCESS))
                .child(panels::dot(hsla(t::SUCCESS), true))
                .child("online")
                .into_any_element();
            // the canvas fills the body; its own size picks the mode (A, B or T)
            let body = div().relative().size_full().child(topo::graph(
                s,
                &self.topo_acts,
                self.topo_now,
                self.live.as_ref().is_none_or(|l| l.cfg.tui.motion),
            ));
            let p = panel(
                Head::new("p-topology", "Synthesis Topology", "network", pad)
                    .fold(self.is_folded("p-topology"), self.fold_cb("p-topology", cx)),
                Some(online),
                body,
            );
            // the last frame's height, for the Usage month section's slack
            let cell = self.topo_h.clone();
            let p = p.relative().child(
                gpui::canvas(
                    move |b, _, _| cell.set(f32::from(b.size.height)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
            // `.grow` (flex 1 1 0)
            col = col.child(self.fit(p, "p-topology", |p| p.flex_1().min_h(px(tier.topo_min))));
        } else if let Some(n) = s.topology.iter().find(|n| n.pos == sample::Pos::Sub) {
            // Topology is hidden: the sub-agents stay visible as a compact block (never hideable)
            let body = div().flex().flex_col().children(n.lines.iter().map(|l| {
                div()
                    .py(px(1.))
                    .truncate()
                    .text_color(text())
                    .child(l.clone())
            }));
            col = col.child(
                panel(
                    Head::new("p-subagents", "Subagents", "users", Pad::CARD),
                    None,
                    body,
                )
                .flex_none(),
            );
        }
        // Mission shows only while the agent has a task list (and `status` is not switched off)
        let mission = !hid.status && !s.agent.steps.is_empty();
        if mission || !hid.tokens {
            let card_min = if tight { 84. } else { 110. };
            let mut row = div()
                .debug_selector(|| "status-context".to_string())
                .flex()
                .gap(px(CARD_GAP))
                .flex_initial()
                .min_h(px(card_min));
            if mission {
                row = row.child(
                    panel(
                        Head::new("p-agent", "Mission", "sparkles", Pad::CARD)
                            .fold(self.is_folded("p-agent"), self.fold_cb("p-agent", cx)),
                        None,
                        div()
                            .line_height(px(layout::LINE_TIGHT))
                            .child(panels::agent_body(&s.agent, if tight { 3 } else { 8 })),
                    )
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    // folded: just the header, not as tall as its neighbour
                    .when(self.is_folded("p-agent"), |p| p.self_start()),
                );
            }
            if !hid.tokens {
                row = row.child(
                    panel(
                        Head::new("p-context", "Context", "cpu", Pad::CARD)
                            .fold(self.is_folded("p-context"), self.fold_cb("p-context", cx)),
                        None,
                        div()
                            .line_height(px(layout::LINE_TIGHT))
                            .child(panels::context_body(&s.context, if tight { 4 } else { 6 })),
                    )
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .when(self.is_folded("p-context"), |p| p.self_start()),
                );
            }
            col = col.child(row);
        }
        if !hid.usage && !nousage {
            let mut head = Head::new("p-usage", "Usage & Metrics", "chart-bar", Pad::NORMAL)
                .fold(self.is_folded("p-usage"), self.fold_cb("p-usage", cx));
            head.meta_h = 22.6;
            let p = panel(
                head,
                Some(panels::usage_meta(
                    &s.usage,
                    self.live.as_ref().map(|_| -> panels::OnClick {
                        Box::new(cx.listener(|this, _, _, cx| {
                            if let Some(lu) = &mut this.live {
                                lu.live.usage_days = if lu.live.usage_days == 7 { 30 } else { 7 };
                                lu.dirty = true;
                            }
                            cx.notify();
                        }))
                    }),
                )),
                {
                    // the CLI's month section goes under the bars only into space the topology panel can spare
                    let slack = if hid.topology || self.is_folded("p-topology") {
                        0.
                    } else {
                        self.topo_h.get() + self.month_used.get() - tier.topo_min
                    };
                    let mode = if self.is_folded("p-usage") {
                        layout::MonthMode::Hidden
                    } else {
                        layout::month_mode(slack, &s.usage.month)
                    };
                    self.month_used.set(match mode {
                        layout::MonthMode::Hidden => 0.,
                        m => layout::month_h(m, &s.usage.month) + layout::MONTH_GAP,
                    });
                    let body_w = cols(w).right - 2. * 12. - 2.;
                    panels::usage_body(&s.usage).when(mode != layout::MonthMode::Hidden, |d| {
                        d.child(panels::month_section(&s.usage.month, mode, body_w))
                    })
                },
            );
            col = col.child(self.fit(p, "p-usage", |p| p.flex_initial().min_h(px(tier.usage_min))));
        }
        col
    }
}

impl TuiView {
    /// Takes the Topology activity of this frame, and while a node is active schedules the next repaint:
    /// about 30 frames a second with motion, 4 a second without (only to catch the end of the hold).
    /// Idle means no timer and no repaint.
    fn topo_frame(&mut self, cx: &mut Context<Self>) {
        use synthesis_view::pulse::Pulses;
        let (pulses, now) = match &self.topo_force {
            Some((nodes, at)) => (Pulses::forced(nodes), *at),
            None => (self.snap.pulses.clone(), crate::live::now_ms()),
        };
        self.topo_now = now;
        self.topo_acts = pulses.acts(now);
        if self.topo_acts.is_empty() || self.topo_force.is_some() {
            self.topo_tick = None;
            return;
        }
        if self.topo_tick.is_some() {
            return;
        }
        let motion = self.live.as_ref().is_none_or(|l| l.cfg.tui.motion);
        let wait = std::time::Duration::from_millis(if motion { 33 } else { 250 });
        self.topo_tick = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |t, cx| {
                t.topo_tick = None;
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for TuiView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _perf = crate::perf::timed(true);
        // a prompt handed back unsent (Esc in the model picker) returns to the input box
        if let Some(t) = self
            .live
            .as_mut()
            .and_then(|lu| lu.live.returned_prompt.take())
        {
            cx.defer_in(window, move |this, window, cx| {
                this.set_prompt(&t, window, cx)
            });
        }
        // the refresh spinner of the model picker turns every frame
        if let Some(lu) = &mut self.live
            && lu.cfg.tui.motion
            && lu.live.spin_refresh(crate::live::now_ms())
        {
            lu.dirty = true;
            chrome::next_frame(|t| &mut t.anim_tick, self, cx);
        }
        // a blinking red is repainted every frame while an approval waits
        let motion = self.live.as_ref().is_some_and(|l| l.cfg.tui.motion);
        // a spinner or a blink in the Running list is repainted every frame too
        if self.attention() == Some(true)
            || self.pointer_animating()
            || self
                .live
                .as_ref()
                .and_then(|l| l.flash)
                .is_some_and(|(_, until)| crate::live::now_ms() < until)
            || (motion
                && self.snap.running.iter().any(|r| {
                    matches!(
                        r.state,
                        activity::RunState::Running | activity::RunState::Approval
                    )
                }))
        {
            chrome::next_frame(|t| &mut t.anim_tick, self, cx);
        }
        self.refresh(window);
        self.topo_frame(cx);
        self.sync_placeholder(window, cx);
        self.tabs_tick(cx);
        let vp = window.viewport_size();
        let w = f32::from(vp.width) - self.inset;
        let h = f32::from(vp.height) - self.top;
        self.win_h = f32::from(vp.height);
        // a dialog waits: no slide-over column covering or dimming it
        if self
            .live
            .as_ref()
            .is_some_and(|l| !l.live.dialogs.is_empty())
        {
            self.drawer = None;
        }
        let tier = tiers(h);
        let c = cols(w);
        let (right_in, right_btn) = (c.right_inline, !c.right_inline);
        let left_in = c.left_inline;
        let side = |on: bool, x: f32| if on { x + GAP } else { 0. };
        self.centre_w = w - 2. * PAD - side(left_in, c.left) - side(right_in, c.right);
        // a collapsed column that was open when the window grew has no drawer any more
        let drawer = self.drawer.filter(|s| match s {
            Side::Left => !left_in,
            Side::Right => right_btn,
        });
        let mut body = div()
            .debug_selector(|| "grid".to_string())
            .flex_1()
            .min_h(px(0.))
            .flex()
            .gap(px(GAP))
            .p(px(PAD))
            .when(left_in, |d| d.child(self.left(tier, w, cx)))
            .child(self.centre(cx));
        if right_in {
            body = body.child(self.right(tier, w, h, cx));
        }
        let drawer = drawer.map(|side| {
            let col = match side {
                Side::Left => self.left(tier, w, cx),
                Side::Right => self.right(tier, w, h, cx),
            };
            // between the header and the footer; a click on the dimmed rest closes it
            div()
                .debug_selector(|| "drawer".to_string())
                .absolute()
                .top(px(HEADER_H))
                .bottom(px(FOOTER_H))
                .left_0()
                .right_0()
                .flex()
                .when(side == Side::Right, |d| d.justify_end())
                .bg(chrome::black_a(0.5))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.drawer = None;
                        cx.notify();
                    }),
                )
                .child(div().occlude().flex().p(px(PAD)).bg(hsla(t::BG)).child(col))
        });
        div()
            .debug_selector(|| "view".to_string())
            // an image file dropped anywhere on the window is attached
            .on_drop(
                cx.listener(|this, p: &gpui::ExternalPaths, _, cx| this.drop_files(p.paths(), cx)),
            )
            .relative()
            .size_full()
            .min_w(px(MIN_W))
            .min_h(px(MIN_H))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(crate::theme::fill(t::BG))
            .font_family(MONO)
            .font_weight(FontWeight::MEDIUM)
            .text_size(px(12.5))
            .line_height(gpui::relative(1.55))
            .text_color(text())
            .child(self.top_bar(w, !left_in, right_btn, cx))
            .child(body)
            .child(self.footer())
            .children(drawer)
            .children(self.quote_menu(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::layout::*;
    use super::*;
    use crate::brand;
    use gpui::{TestAppContext, px, size};

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

    /// A throwaway live view (no engine): the conversation is whatever the test pushes.
    fn live_cfg() -> driver::LiveCfg {
        use clap::Parser as _;
        let dir = std::env::temp_dir();
        driver::LiveCfg {
            env: synthesis_config::Env::from_pairs([("HOME", "/nonexistent")]),
            args: crate::engine::Args::try_parse_from(["synthesis-desktop"]).unwrap(),
            project: dir.clone(),
            granted: dir.clone(),
            synthesis_home: dir,
            home: None,
            tz_min: 0,
            tui: Default::default(),
        }
    }

    #[gpui::test]
    fn the_tabs_belong_to_the_session(cx: &mut TestAppContext) {
        use crate::live::Live;
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let live = |sid: &str| {
            let mut l = Live::new();
            l.starting = false;
            l.sid = sid.into();
            l
        };
        let draw = |vcx: &mut gpui::VisualTestContext| {
            vcx.simulate_resize(size(px(1400.), px(900.)));
            vcx.run_until_parked();
        };
        view.update(vcx, |t, _| t.set_live(live("a")));
        draw(vcx);
        view.update(vcx, |t, cx| t.show_tab(ChatTab::Diff, None, cx));
        assert_eq!(view.read_with(vcx, |t, _| t.ts.tab), ChatTab::Diff);
        // another session starts on the chat, the first one keeps its diff tab
        view.update(vcx, |t, _| t.set_live(live("b")));
        draw(vcx);
        assert_eq!(view.read_with(vcx, |t, _| t.ts.tab), ChatTab::Chat);
        view.update(vcx, |t, _| t.set_live(live("a")));
        draw(vcx);
        assert_eq!(view.read_with(vcx, |t, _| t.ts.tab), ChatTab::Diff);
    }

    /// Opening Files in an untrusted folder reads nothing from it: no scan starts, the tree holds only the note.
    #[gpui::test]
    fn the_files_tab_does_no_scan_in_an_untrusted_folder(cx: &mut TestAppContext) {
        use crate::live::Live;
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/nonexistent/untrusted-project".into();
        assert!(!l.trusted);
        view.update(vcx, |t, _| t.set_live(l));
        view.update(vcx, |t, cx| {
            t.select_tab(ChatTab::Files, cx);
            t.tabs_tick(cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |t, _| {
            assert!(!t.tree.loading, "no background scan was started");
            let note = t.tree.data.as_ref().and_then(|d| d.note.clone());
            assert_eq!(note.as_deref(), Some(tabs::UNTRUSTED));
        });
    }

    #[gpui::test]
    fn a_long_chat_builds_only_the_rows_near_the_viewport(cx: &mut TestAppContext) {
        use crate::live::Live;
        use synthesis_view::convo::Item;
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.view.convo.items = (0..2000)
            .map(|i| Item::Notice {
                warning: false,
                text: format!("row {i}"),
            })
            .collect();
        view.update(vcx, |t, _| t.set_live(l));
        vcx.simulate_resize(size(px(1400.), px(900.)));
        vcx.run_until_parked();
        let list = view.read_with(vcx, |t, _| t.live.as_ref().unwrap().list.clone());
        assert_eq!(list.item_count(), 2000);
        // stuck to the newest row: the tail is built, the head is not
        assert!(
            list.bounds_for_item(1999).is_some(),
            "the newest row is drawn"
        );
        assert!(
            list.bounds_for_item(0).is_none(),
            "the oldest row is not built"
        );
        assert!(list.bounds_for_item(1000).is_none());
    }

    /// Every kind of long output goes through the real row builder and ends clipped, with a "Show more".
    #[gpui::test]
    fn long_tool_output_is_clipped_to_five_lines(cx: &mut TestAppContext) {
        use crate::live::Live;
        use synthesis_view::convo::{Item, ToolState};
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let numbered = |n: usize| {
            (1..=n)
                .map(|i| format!("{i:>4}  line {i}\n"))
                .collect::<String>()
        };
        let tool = |id: &str, name: &str, output: String| Item::Tool {
            id: id.into(),
            name: name.into(),
            summary: "x".into(),
            state: ToolState::Ok,
            preview: String::new(),
            output,
            duration_ms: 1,
            file: None,
            diff: None,
            input: Box::new(serde_json::Value::Null),
            status: None,
            exit_code: None,
        };
        let mut l = Live::new();
        l.starting = false;
        l.view.convo.items = vec![
            tool("read", "Read", numbered(72)),
            tool("bash", "Bash", numbered(40)),
            // one very long line is the rows it fills on screen
            tool("long", "Bash", "x".repeat(6000)),
            tool("short", "Bash", numbered(3)),
            // last, and short enough that its header stays on screen once opened
            tool("grep", "Grep", numbered(30)),
        ];
        view.update(vcx, |t, _| t.set_live(l));
        vcx.simulate_resize(size(px(1400.), px(1400.)));
        vcx.run_until_parked();
        for id in ["more-read", "more-grep", "more-bash", "more-long"] {
            assert!(vcx.debug_bounds(id).is_some(), "{id}: no Show more");
        }
        assert!(vcx.debug_bounds("more-short").is_none());
        // one click on a header opens the card whole, the next clips it again
        let head = vcx.debug_bounds("tool-head-grep").expect("header drawn");
        let at = gpui::point(head.left() + px(60.), head.center().y);
        let open = |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |t, _| {
                t.live.as_ref().unwrap().live.expanded.contains("grep")
            })
        };
        vcx.simulate_click(at, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(open(&view, vcx), "first click expands");
        // the list measures the taller card, then the header is looked up where it now is
        vcx.simulate_resize(size(px(1400.), px(1401.)));
        vcx.run_until_parked();
        let head = vcx.debug_bounds("tool-head-grep").unwrap();
        let at = gpui::point(head.left() + px(60.), head.center().y);
        vcx.simulate_click(at, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(!open(&view, vcx), "second click collapses");
    }

    /// A click on a section's title folds it to its header, a second click opens it again.
    #[gpui::test]
    fn a_click_on_a_panel_title_folds_it(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (_view, vcx) = cx.add_window_view(|_, _| TuiView::new(0., Hidden::default()));
        vcx.simulate_resize(size(px(1400.), px(900.)));
        vcx.run_until_parked();
        for id in ["p-session", "p-tools", "p-topology", "p-usage"] {
            let head_id = match id {
                "p-session" => "p-session-head",
                "p-tools" => "p-tools-head",
                "p-topology" => "p-topology-head",
                _ => "p-usage-head",
            };
            let head = vcx.debug_bounds(head_id);
            let body = match id {
                "p-session" => "p-session-body",
                "p-tools" => "p-tools-body",
                "p-topology" => "p-topology-body",
                _ => "p-usage-body",
            };
            let head = head.unwrap_or_else(|| panic!("{id}: no header"));
            assert!(vcx.debug_bounds(body).is_some(), "{id}: body before");
            let at = gpui::point(head.left() + px(40.), head.center().y);
            vcx.simulate_click(at, gpui::Modifiers::default());
            vcx.run_until_parked();
            assert!(vcx.debug_bounds(body).is_none(), "{id}: still open");
            // the column re-flows: look the header up again
            let head = vcx.debug_bounds(head_id).unwrap();
            let at = gpui::point(head.left() + px(40.), head.center().y);
            vcx.simulate_click(at, gpui::Modifiers::default());
            vcx.run_until_parked();
            assert!(vcx.debug_bounds(body).is_some(), "{id}: not reopened");
        }
    }

    /// A dialog is a panel in the chat column right above the prompt (not a window-wide modal), and while it
    /// waits the view reports the red "needs you" signal.
    #[gpui::test]
    fn a_dialog_docks_above_the_prompt(cx: &mut TestAppContext) {
        use crate::live::{Dialog, DialogKind, Live};
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.dialogs.push(Dialog {
            kind: DialogKind::Plan {
                id: "p".into(),
                plan: "do it".into(),
            },
            options: vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
            sel: 1,
            armed_until: 0,
            engaged: false,
        });
        view.update(vcx, |t, _| t.set_live(l));
        vcx.simulate_resize(size(px(1400.), px(900.)));
        vcx.run_until_parked();
        let (dlg, prompt, chat) = (
            vcx.debug_bounds("dialog").expect("dialog drawn"),
            vcx.debug_bounds("prompt").expect("prompt drawn"),
            vcx.debug_bounds("p-chat").expect("chat drawn"),
        );
        assert!(dlg.bottom() <= prompt.top(), "above the prompt");
        assert!(
            dlg.left() >= chat.left() && dlg.right() <= chat.right(),
            "inside the chat column"
        );
        assert!(
            dlg.size.width < px(1400.) - px(400.),
            "not a window-wide modal"
        );
        assert!(view.read_with(vcx, |t, _| t.attention()).is_some());
    }

    /// The keys of a pending approval: ignored under Settings, and Enter or a digit answers only after the
    /// person engaged the dialog (arrow, Tab or a click); Esc rejects at once.
    #[gpui::test]
    fn dialog_keys_need_the_tui_view_and_an_engaged_dialog(cx: &mut TestAppContext) {
        use crate::live::{Dialog, DialogKind, Live};
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.dialogs.push(Dialog {
            kind: DialogKind::Plan {
                id: "p".into(),
                plan: "do it".into(),
            },
            options: vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
        view.update(vcx, |t, _| t.set_live(l));
        let pending = |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |t, _| t.live.as_ref().unwrap().live.dialogs.len())
        };
        let press =
            |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext, k: &'static str| {
                let ks = gpui::Keystroke::parse(k).unwrap();
                view.update_in(vcx, |t, window, cx| t.on_key(&ks, window, cx))
            };
        // under Settings: not consumed, nothing answered
        view.update(vcx, |t, _| t.shown = false);
        assert!(!press(&view, vcx, "enter"));
        assert!(!press(&view, vcx, "1"));
        assert_eq!(pending(&view, vcx), 1);
        // on screen but untouched: swallowed, nothing answered
        view.update(vcx, |t, _| t.shown = true);
        assert!(press(&view, vcx, "enter"));
        assert!(press(&view, vcx, "1"));
        assert_eq!(pending(&view, vcx), 1);
        // no slide-over column while a dialog waits
        view.update(vcx, |t, cx| t.toggle_drawer(Side::Left, cx));
        assert!(view.read_with(vcx, |t, _| t.drawer.is_none()));
        // engaged by an arrow key: Enter answers
        press(&view, vcx, "down");
        press(&view, vcx, "enter");
        assert_eq!(pending(&view, vcx), 0);
    }

    /// The send picker is a dialog like the others: under Settings no key reaches it (no typing into its
    /// filter, no Enter), on screen Enter picks at once (the person opened it), and Esc sends nothing.
    #[gpui::test]
    fn the_send_picker_ignores_keys_while_hidden(cx: &mut TestAppContext) {
        use crate::live::Live;
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.commands.push(crate::live::send_command());
        l.set_peer_rows(
            vec![serde_json::json!({"address": "ws:aaaa/sess:11111111-x/agent:main",
                "workspace": {"label": "api"}, "session": {"short_id": "11111111", "name": "backend"},
                "agent": {"instance": "main", "status": "idle"}})],
            0,
        );
        l.submit("/send");
        l.dialogs[0].armed_until = 0;
        view.update(vcx, |t, _| t.set_live(l));
        let state = |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |t, _| {
                let l = &t.live.as_ref().unwrap().live;
                (
                    l.dialogs.len(),
                    l.send_to.is_some(),
                    l.send_pick.as_ref().map(|p| p.filter.clone()),
                )
            })
        };
        let press =
            |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext, k: &'static str| {
                let ks = gpui::Keystroke::parse(k).unwrap();
                view.update_in(vcx, |t, window, cx| t.on_key(&ks, window, cx))
            };
        view.update(vcx, |t, _| t.shown = false);
        for k in ["enter", "a", "escape"] {
            assert!(!press(&view, vcx, k));
        }
        assert_eq!(state(&view, vcx), (1, false, Some(String::new())));
        view.update(vcx, |t, _| t.shown = true);
        // untouched: an Enter typed ahead picks nothing
        press(&view, vcx, "enter");
        assert_eq!(state(&view, vcx), (1, false, Some(String::new())));
        press(&view, vcx, "down");
        press(&view, vcx, "enter");
        assert_eq!(
            state(&view, vcx),
            (0, true, None),
            "picked: the chip is set"
        );
    }

    /// The close-workspace confirmation is a dialog like the others: under Settings no key reaches it
    /// (Enter, Down, Esc change nothing), and on screen an Enter typed ahead picks nothing.
    #[gpui::test]
    fn the_close_workspace_dialog_ignores_keys_while_hidden(cx: &mut TestAppContext) {
        use crate::live::Live;
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.view.workspaces.workspaces = vec![synthesis_view::workspaces::WorkspaceRow {
            root: "/w/other".into(),
            key: "other".into(),
            label: "other".into(),
            sessions: vec![synthesis_view::workspaces::SessionRow {
                id: "a".into(),
                title: "t".into(),
                status: synthesis_core::SessionStatus::Idle,
                updated_at: String::new(),
                mtime: 0,
                is_live: false,
            }],
            available: true,
            changes: None,
        }];
        l.ask_close_workspace(Some("other"), 0);
        l.dialogs[0].armed_until = 0;
        view.update(vcx, |t, _| t.set_live(l));
        let state = |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |t, _| {
                let d = &t.live.as_ref().unwrap().live.dialogs;
                (d.len(), d.first().map(|d| (d.sel, d.engaged)))
            })
        };
        let press =
            |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext, k: &'static str| {
                let ks = gpui::Keystroke::parse(k).unwrap();
                view.update_in(vcx, |t, window, cx| t.on_key(&ks, window, cx))
            };
        view.update(vcx, |t, _| t.shown = false);
        for k in ["enter", "down", "1", "2", "escape"] {
            assert!(!press(&view, vcx, k));
        }
        assert_eq!(state(&view, vcx), (1, Some((0, false))), "untouched");
        view.update(vcx, |t, _| t.shown = true);
        press(&view, vcx, "enter");
        assert_eq!(
            state(&view, vcx),
            (1, Some((0, false))),
            "typed ahead picks nothing"
        );
        press(&view, vcx, "escape");
        assert_eq!(state(&view, vcx).0, 0, "Esc cancels");
    }

    /// A click on a Running row selects it and hands the keys to the chat (the prompt holds the focus), even
    /// when the Workspace/Running panel had them; the order of the rows does not change.
    #[gpui::test]
    fn clicking_a_running_row_focuses_the_chat_and_keeps_the_order(cx: &mut TestAppContext) {
        use crate::live::{Focus, Live};
        use gpui::Focusable as _;
        use synthesis_view::workspaces::{SessionRow, WorkspaceRow, fold_dir};
        cx.update(gpui_component::init);
        let (view, vcx) = cx.add_window_view(|window, cx| {
            TuiView::live(window, cx, live_cfg(), Hidden::default(), 0.)
        });
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/w/proj".into();
        l.sid = "a".into();
        l.ws_key = fold_dir("/w/proj");
        let row = |id: &str| SessionRow {
            id: id.into(),
            title: id.into(),
            status: synthesis_core::SessionStatus::Idle,
            updated_at: String::new(),
            mtime: 0,
            is_live: false,
        };
        l.view.workspaces.workspaces = vec![WorkspaceRow {
            root: "/w/proj".into(),
            key: l.ws_key.clone(),
            label: "proj".into(),
            sessions: vec![row("a"), row("b"), row("c")],
            available: true,
            changes: None,
        }];
        l.focus = Focus::Running;
        view.update(vcx, |t, _| t.set_live(l));
        vcx.simulate_resize(size(px(1400.), px(900.)));
        vcx.run_until_parked();
        let ids = |view: &gpui::Entity<TuiView>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |t, _| {
                t.snap
                    .running
                    .iter()
                    .map(|r| r.id.clone())
                    .collect::<Vec<_>>()
            })
        };
        let before = ids(&view, vcx);
        assert_eq!(before, ["a", "b", "c"]);
        view.update_in(vcx, |t, window, cx| t.click_running(1, window, cx));
        vcx.run_until_parked();
        let (focus, sel, focused) = view.update_in(vcx, |t, window, cx| {
            let lu = t.live.as_ref().unwrap();
            (
                lu.live.focus,
                lu.live.run_sel,
                lu.prompt.focus_handle(cx).is_focused(window),
            )
        });
        assert_eq!((focus, sel, focused), (Focus::Chat, 1, true));
        assert_eq!(ids(&view, vcx), before, "the click moves no row");
    }
}
