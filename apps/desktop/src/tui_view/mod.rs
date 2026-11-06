//! The TUI view: the classic TUI's dashboard drawn natively (docs/specs/desktop.md), 1:1 with
//! `cli.html` `#view1` in the platinum palette. Left column Workspace / Running / Session / MCP /
//! Tools & Skills / Activity; centre Status + Tokens boxes over the chat (welcome, prompt, mode line);
//! right Topology and Usage; top bar and key hint bar. It reads a `Snapshot` (see `sample`); no
//! terminal, no PTY. Panels hide with the TUI's `panels.*` keys (`layout::Hidden`).

pub mod chat;
pub mod dialog;
pub mod driver;
pub mod layout;
pub mod panels;
pub mod sample;
#[cfg(feature = "shot")]
pub mod shot;
pub mod snapshot;
pub mod topo;

use gpui::{
    Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Window, div, px,
};
use synthesis_tui_render::theme as t;

use self::layout::{
    FOOTER_H, GAP, HEADER_H, LEFT_GAP, MIN_H, MIN_W, PAD, cols, list_body_h, tiers,
};
use self::panels::{Head, dim, icon, panel, strong, text};
use self::sample::{Snapshot, sample};
use crate::{brand, theme::hsla};
use gpui::prelude::FluentBuilder as _;

/// The TUI view's font (bundled, SIL OFL).
pub const MONO: &str = "JetBrains Mono";

/// Icons the view draws (checked by a test).
pub const ICONS: [&str; 10] = [
    "folder",
    "bot",
    "square-terminal",
    "network",
    "settings-2",
    "gallery-vertical-end",
    "cpu",
    "memory-stick",
    "layout-dashboard",
    "chart-pie",
];

/// Text with letter-spacing (`em` of the size): gpui has none, so one box per character.
pub fn tracked(s: &str, size: f32, em: f32, c: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .gap(px(size * em))
        .text_size(px(size))
        .text_color(c)
        .children(s.chars().map(|ch| div().child(ch.to_string())))
}

pub struct TuiView {
    snap: Snapshot,
    /// The engine, the chat and the prompt; `None` draws the sample (layout tests, `--sample`, `--shot`).
    live: Option<driver::LiveUi>,
    hidden: layout::Hidden,
    /// Width taken by chrome beside the view (the shell's rail): breakpoints use the view's own width.
    inset: f32,
    /// False while another view of the shell is shown: the window-wide key hook then leaves the keys alone.
    pub active: bool,
}

impl TuiView {
    pub fn new(inset: f32, hidden: layout::Hidden) -> Self {
        Self {
            active: true,
            snap: sample(),
            live: None,
            hidden,
            inset,
        }
    }

    /// Before drawing: rebuild the snapshot from the live state when it changed, and keep the chat
    /// stuck to the newest row unless the person scrolled up.
    fn refresh(&mut self, window: &mut Window) {
        let Some(lu) = &mut self.live else { return };
        let _ = window;
        if lu.dirty {
            lu.dirty = false;
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

    fn top_bar(&self) -> Div {
        let top = &self.snap.top;
        let chip = |label: &str, value: String, c: Hsla| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(14.))
                .h(px(22.))
                .border_l_1()
                .border_color(hsla(t::RULE))
                .whitespace_nowrap()
                .child(div().child(label.to_string()))
                .child(div().text_color(c).child(value))
        };
        div()
            .debug_selector(|| "header".to_string())
            .h(px(HEADER_H))
            .flex_none()
            .flex()
            .items_center()
            .px(px(16.))
            .border_b_1()
            .border_color(hsla(t::RULE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(gpui::img(brand::mark()).size(px(32.)).flex_none())
                    .child(tracked("SYNTHESIS", 14., 0.32, strong())),
            )
            .child(
                div()
                    .flex()
                    .ml(px(28.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(14.))
                            .h(px(22.))
                            .text_color(hsla(t::SUCCESS))
                            .child(panels::dot(hsla(t::SUCCESS), true))
                            .child(
                                div()
                                    .text_color(strong())
                                    .child(format!("{} live", top.live)),
                            ),
                    )
                    .child(chip("workspace:", top.workspace.clone(), hsla(t::INFO)))
                    .child(chip("session:", top.session.clone(), hsla(t::ACCENT)))
                    .child(chip("model:", top.model.clone(), hsla(t::INFO)))
                    .child(chip("ctx:", top.ctx.clone(), hsla(t::SUCCESS))),
            )
            .child(div().flex_1())
            .child(chip("", top.health.clone(), hsla(t::SUCCESS)))
            .child(chip("", top.clock.clone(), text()))
    }

    fn footer(&self) -> Div {
        let hint = |(k, d): &(String, String)| {
            div()
                .flex()
                .gap(px(6.))
                .px(px(10.))
                .border_l_1()
                .border_color(hsla(t::RULE))
                .child(div().text_color(strong()).child(k.clone()))
                .child(div().child(d.clone()))
        };
        div()
            .debug_selector(|| "footer".to_string())
            .h(px(FOOTER_H))
            .flex_none()
            .flex()
            .items_center()
            .px(px(12.))
            .border_t_1()
            .border_color(hsla(t::RULE))
            .text_color(text())
            .whitespace_nowrap()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .children(self.snap.hints.iter().map(hint)),
            )
            .child(
                div()
                    .flex()
                    .child(
                        div()
                            .px(px(10.))
                            .child(format!("ctx {}", self.snap.top.ctx)),
                    )
                    .child(
                        div()
                            .px(px(10.))
                            .border_l_1()
                            .border_color(hsla(t::RULE))
                            .child(format!("{} mode", self.snap.mode)),
                    ),
            )
    }

    fn left(&self, tier: layout::Tiers, w: f32, cx: &mut Context<Self>) -> Div {
        let (s, h) = (&self.snap, &self.hidden);
        let split = h.left_is_split();
        let head = |id, title, icon| Head {
            id,
            title,
            icon,
            left: true,
            compact: false,
        };
        let min = px(tier.left_min);
        let meta = |c: Hsla, s: String| div().text_color(c).child(s).into_any_element();
        // Activity grows; with it hidden the last panel below Running takes the rest
        let grow_last = if h.activity {
            [
                (!h.tools, "tools"),
                (!h.mcp, "mcp"),
                (!h.session, "session"),
            ]
            .iter()
            .find(|(shown, _)| *shown)
            .map(|(_, k)| *k)
        } else {
            None
        };
        let ws_click = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| this.click_workspace(i, cx)))
        };
        let run_click = |i: usize| -> panels::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| this.click_running(i, cx)))
        };
        let live = self.live.as_ref();
        let focus = live.map(|l| l.live.focus);
        let list = |id, title, ic, m, body: Div, focused: bool| {
            let p = panels::panel_scroll(
                head(id, title, ic),
                Some(m),
                !split,
                (!split).then(list_body_h),
                body,
                live.map(|l| {
                    if id == "p-workspace" {
                        &l.ws_scroll
                    } else {
                        &l.run_scroll
                    }
                }),
            )
            .when(focused, |d| d.border_color(hsla(t::ACCENT)));
            if split {
                p.flex_1().min_h(px(0.))
            } else {
                p.flex_initial().min_h(min)
            }
        };
        let mut col = div()
            .debug_selector(|| "col-left".to_string())
            .w(px(cols(w).left))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(LEFT_GAP))
            .min_h(px(0.))
            .overflow_hidden()
            .child(list(
                "p-workspace",
                "Workspace",
                "folder",
                meta(text(), s.workspaces.len().to_string()),
                panels::workspace_rows(
                    &s.workspaces,
                    live.map(|_| &ws_click as &dyn Fn(usize) -> panels::OnClick),
                ),
                focus == Some(crate::live::Focus::Workspace),
            ))
            .child(list(
                "p-running",
                "Running",
                "bot",
                meta(
                    hsla(t::SUCCESS),
                    s.running.iter().filter(|r| r.live).count().to_string(),
                ),
                panels::running_rows(
                    &s.running,
                    live.map(|_| &run_click as &dyn Fn(usize) -> panels::OnClick),
                ),
                focus == Some(crate::live::Focus::Running),
            ));
        let grown = |key: &str, p: Div| {
            if grow_last == Some(key) {
                p.flex_1().min_h(px(0.))
            } else {
                p
            }
        };
        if !h.session {
            col = col.child(grown(
                "session",
                // Session always shows all its rows (`#view1 .col.l>.panel:has(#h-sess){flex:0 0 auto}`)
                panel(
                    head("p-session", "Session", "square-terminal"),
                    None,
                    true,
                    None,
                    panels::kv(&s.session, 62.),
                )
                .flex_none(),
            ));
        }
        if !h.mcp {
            col = col.child(grown(
                "mcp",
                panel(
                    head("p-mcp", "MCP", "network"),
                    Some(meta(text(), s.mcp.len().to_string())),
                    true,
                    Some(layout::whole_rows_h(72.)),
                    panels::mcp_rows(&s.mcp),
                )
                .flex_initial()
                .min_h(min),
            ));
        }
        if !h.tools {
            col = col.child(grown(
                "tools",
                panel(
                    head("p-tools", "Tools & Skills", "settings-2"),
                    None,
                    true,
                    Some(layout::body_h(4)),
                    panels::kv(&s.tools, 62.),
                )
                .flex_initial()
                .min_h(min),
            ));
        }
        if h.activity {
            return col;
        }
        col.child(
            panel(
                head("p-activity", "Activity", "gallery-vertical-end"),
                Some(meta(text(), s.activity.len().to_string())),
                false,
                None,
                panels::activity_rows(&s.activity),
            )
            .flex_1()
            .min_h(min),
        )
    }

    fn centre(&self, w: f32, cx: &mut Context<Self>) -> Div {
        let (s, h) = (&self.snap, &self.hidden);
        let head = |id, title, icon| Head {
            id,
            title,
            icon,
            left: false,
            compact: false,
        };
        let mut col = div()
            .debug_selector(|| "col-centre".to_string())
            .flex_1()
            .min_w(px(cols(w).centre_min))
            .flex()
            .flex_col()
            .gap(px(GAP))
            .min_h(px(0.));
        if h.has_cards() {
            // `.cards{grid-template-columns:minmax(0,1fr) 224px;gap:10px}`
            let mut cards = div().flex().flex_none().gap(px(GAP)).items_start();
            if !h.status {
                cards = cards.child(
                    panel(
                        head("p-status", "Status", "cpu"),
                        None,
                        true,
                        None,
                        panels::status_body(s),
                    )
                    .flex_1()
                    .min_w(px(0.)),
                );
            }
            if !h.tokens {
                let p = panel(
                    head("p-tokens", "Tokens", "memory-stick"),
                    None,
                    true,
                    None,
                    panels::tokens_body(&s.tokens),
                );
                cards = cards.child(if h.status {
                    p.flex_1().min_w(px(0.))
                } else {
                    p.w(px(224.)).flex_none()
                });
            }
            col = col.child(cards);
        }
        col.child(self.chat(cx))
    }

    fn chat(&self, cx: &mut Context<Self>) -> Div {
        let s = &self.snap;
        let (body, prompt) = if self.live.is_some() {
            (self.conversation(cx), self.live_prompt(cx))
        } else {
            let prompt = div()
                .flex_none()
                .mx(px(12.))
                .h(px(40.))
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .border_1()
                .border_color(hsla(t::RULE))
                .rounded(px(layout::PANEL_RADIUS))
                .child(div().text_color(strong()).child(">"))
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(dim())
                        .child("Ask Synthesis anything... (Ctrl+L for history)"),
                )
                .child(icon("arrow-up", 14., dim()));
            (self.welcome().into_any_element(), prompt)
        };
        let mode = div()
            .flex_none()
            .h(px(28.))
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(5.))
            .whitespace_nowrap()
            .child(
                div()
                    .text_color(hsla(t::WARN))
                    .child(format!("\u{23F5}\u{23F5} {} mode on", s.mode)),
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
            .child(div().text_color(hsla(t::INFO)).child(s.top.model.clone()))
            .child(div().text_color(dim()).child("\u{00B7} effort"))
            .child(div().text_color(hsla(t::ACCENT)).child(s.effort.clone()));
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
            .rounded(px(layout::PANEL_RADIUS))
            .child(body)
            .child(prompt)
            .child(mode)
    }

    fn right(&self, tier: layout::Tiers, w: f32) -> Div {
        let (s, h) = (&self.snap, &self.hidden);
        let right_w = cols(w).right;
        let mut col = div()
            .debug_selector(|| "col-right".to_string())
            .w(px(right_w))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(GAP))
            .min_h(px(0.))
            .overflow_hidden();
        if !h.topology {
            let head = Head {
                id: "p-topology",
                title: "Synthesis Topology",
                icon: "layout-dashboard",
                left: false,
                compact: tier.compact_head,
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
            let body = div().relative().size_full().child(topo::graph(s));
            let p = panel(head, Some(online), false, None, body);
            // `.grow` (flex 1 1 0); alone in the column it takes everything
            col = col.child(p.flex_1().min_h(px(tier.topo_min)));
        }
        if !h.usage {
            let head = Head {
                id: "p-usage",
                title: "Usage",
                icon: "chart-pie",
                left: false,
                compact: false,
            };
            let range = div()
                .px(px(8.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .rounded(px(5.))
                .bg(hsla(t::SURFACE))
                .text_color(strong())
                .child(s.usage.range.clone())
                .into_any_element();
            let inner = right_w - 2. - 24.;
            let p = panel(head, Some(range), true, None, panels::heat_body(s, inner));
            col = col.child(if h.topology {
                p.flex_1().min_h(px(tier.usage_min))
            } else {
                p.flex_initial().min_h(px(tier.usage_min))
            });
        }
        col
    }
}

impl Render for TuiView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh(window);
        let vp = window.viewport_size();
        let w = f32::from(vp.width) - self.inset;
        let h = f32::from(vp.height);
        let tier = tiers(h);
        let mut body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .gap(px(GAP))
            .p(px(PAD))
            .child(self.left(tier, w, cx))
            .child(self.centre(w, cx));
        if self.hidden.has_right() {
            body = body.child(self.right(tier, w));
        }
        let dialog = self.dialog_overlay(cx);
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
            .bg(hsla(t::BG))
            .font_family(MONO)
            .font_weight(FontWeight::NORMAL)
            .text_size(px(12.5))
            .line_height(gpui::relative(1.55))
            .text_color(text())
            .child(self.top_bar())
            .child(body)
            .child(self.footer())
            .children(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::layout::*;
    use super::*;
    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, px, size};

    const SIZES: [(f32, f32); 5] = [
        (900., 520.),
        (1000., 700.),
        (1200., 800.),
        (1400., 900.),
        (1920., 1080.),
    ];

    fn open(cx: &mut TestAppContext, w: f32, h: f32, hidden: Hidden) -> &mut VisualTestContext {
        let (_, vcx) = cx.add_window_view(|_, _| TuiView::new(0., hidden));
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
            let v = open(cx, w, h, Hidden::default());
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
    fn panel_heights_respect_the_design_minimums(cx: &mut TestAppContext) {
        for (w, h) in SIZES {
            let t = tiers(h);
            let v = open(cx, w, h, Hidden::default());
            let at = format!("{w}x{h}");
            let col = b(v, "col-left");
            let mut bottom = 0.;
            for id in ["p-workspace", "p-running", "p-mcp", "p-tools", "p-activity"] {
                let p = b(v, id);
                assert!(
                    f(p.size.height) >= t.left_min - 0.6,
                    "{at} {id} {} < {}",
                    f(p.size.height),
                    t.left_min
                );
                bottom = f(p.bottom());
            }
            assert!(
                bottom <= f(col.bottom()) + 0.6,
                "{at} left column overflows: {bottom} > {}",
                f(col.bottom())
            );
            let (tp, us) = (b(v, "p-topology"), b(v, "p-usage"));
            assert!(
                f(tp.size.height) >= t.topo_min - 0.6,
                "{at} topology {}",
                f(tp.size.height)
            );
            assert!(
                f(us.size.height) >= t.usage_min - 0.6,
                "{at} usage {}",
                f(us.size.height)
            );
            near(
                f(tp.size.height) + GAP + f(us.size.height),
                f(b(v, "col-right").size.height),
                &format!("{at} right fill"),
            );
            // Workspace and Running list at most 5 rows before they scroll
            for id in ["p-workspace-body", "p-running-body"] {
                assert!(f(b(v, id).size.height) <= list_body_h() + 0.6, "{at} {id}");
            }
        }
    }

    #[gpui::test]
    fn tall_window_lists_five_rows(cx: &mut TestAppContext) {
        let v = open(cx, 1920., 1080., Hidden::default());
        near(
            f(b(v, "p-workspace-body").size.height),
            list_body_h(),
            "workspace body",
        );
        near(
            f(b(v, "p-running-body").size.height),
            list_body_h(),
            "running body",
        );
    }

    #[gpui::test]
    fn hiding_panels_gives_their_space_away(cx: &mut TestAppContext) {
        let mut h = Hidden::default();
        for k in ["session", "mcp", "tools", "activity"] {
            h.hide(k);
        }
        let v = open(cx, 1400., 900., h);
        let (ws, run) = (b(v, "p-workspace"), b(v, "p-running"));
        near(f(ws.size.height), f(run.size.height), "50/50 split");
        near(
            f(ws.size.height) + LEFT_GAP + f(run.size.height),
            f(b(v, "col-left").size.height),
            "split fills",
        );

        let mut h = Hidden::default();
        h.hide("status");
        h.hide("tokens");
        let v = open(cx, 1400., 900., h);
        near(
            f(b(v, "p-chat").origin.y),
            HEADER_H + PAD,
            "chat takes the card rows",
        );

        let mut h = Hidden::default();
        h.hide("topology");
        h.hide("usage");
        let v = open(cx, 1400., 900., h);
        assert!(v.debug_bounds("col-right").is_none());
        near(
            f(b(v, "col-centre").bottom_right().x),
            1400. - PAD,
            "chat spans to the right edge",
        );

        let mut h = Hidden::default();
        h.hide("usage");
        let v = open(cx, 1400., 900., h);
        near(
            f(b(v, "p-topology").size.height),
            f(b(v, "col-right").size.height),
            "topology takes the column",
        );
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
}
