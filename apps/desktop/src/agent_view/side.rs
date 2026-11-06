//! The Agent view's side columns: Workspace, Session, Tools & Skills and Activity on the left (the
//! workspace's sessions are clickable and open as tabs), Topology, Plan and Usage on the right.

use gpui::{
    Context, Div, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div, px,
};
use synthesis_tui_render::theme as t;
use synthesis_view::convo::StepState;

use super::layout::{GAP, LEFT_GAP, Tiers, cols};
use super::{AgentView, with_alpha};
use crate::theme::hsla;
use crate::tui_view::layout::list_body_h;
use crate::tui_view::panels::{
    Head, OnClick, activity_rows, dim, dot, icon, kv, mcp_rows, panel, running_rows, strong, text,
};
use crate::tui_view::sample::Snapshot;
use crate::tui_view::topo;

fn head(
    id: &'static str,
    title: &'static str,
    icon: &'static str,
    left: bool,
    compact: bool,
) -> Head {
    Head {
        id,
        title,
        icon,
        left,
        compact,
    }
}

impl AgentView {
    fn snap(&self) -> Option<&Snapshot> {
        self.active_session().and_then(|s| s.snap.as_ref())
    }

    pub(super) fn left(&self, tier: Tiers, w: f32, cx: &mut Context<Self>) -> Div {
        let min = px(tier.left_min);
        let mut col = div()
            .debug_selector(|| "col-left".to_string())
            .w(px(cols(w).left))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(LEFT_GAP))
            .min_h(px(0.))
            .overflow_hidden();
        let Some(s) = self.snap() else {
            return col;
        };
        // the workspace: its name and folder, its sessions (the open ones marked), its top-level files
        let ws = s
            .workspaces
            .iter()
            .find(|w| w.selected)
            .or_else(|| s.workspaces.first());
        let (name, path) = ws.map_or_else(
            || (s.top.workspace.clone(), String::new()),
            |w| (w.name.clone(), w.path.clone()),
        );
        let open: Vec<&str> = self
            .sessions
            .iter()
            .map(|x| x.live.sid.as_str())
            .filter(|id| !id.is_empty())
            .collect();
        let mut rows = s.running.clone();
        for r in &mut rows {
            r.selected = !r.id.is_empty() && open.contains(&r.id.as_str());
        }
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let click = |i: usize| -> OnClick {
            let id = ids[i].clone();
            Box::new(cx.listener(move |this, _, _, cx| {
                if !id.is_empty() {
                    this.open(Some(id.clone()), cx);
                }
            }))
        };
        let files = self
            .tree
            .data
            .as_ref()
            .filter(|d| d.note.is_none())
            .map(|d| {
                d.rows
                    .iter()
                    .filter(|r| r.depth == 0)
                    .take(6)
                    .map(|r| (r.dir, r.name.clone()))
                    .collect::<Vec<_>>()
            });
        let ws_body = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon("folder", 14., hsla(t::INFO)))
                    .child(div().text_color(hsla(t::INFO)).child(name)),
            )
            .child(
                div()
                    .pl(px(20.))
                    .mb(px(4.))
                    .text_color(dim())
                    .truncate()
                    .child(path),
            )
            .child(running_rows(
                &rows,
                (self.cfg.is_some()).then_some(&click as &dyn Fn(usize) -> OnClick),
            ))
            .children(files.map(|f| {
                div()
                    .mt(px(4.))
                    .flex()
                    .flex_col()
                    .children(f.into_iter().map(|(dir, n)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().w(px(10.)).text_color(dim()).child(if dir {
                                "\u{203A}"
                            } else {
                                ""
                            }))
                            .child(icon(
                                if dir { "folder" } else { "file" },
                                14.,
                                hsla(t::ACCENT),
                            ))
                            .child(
                                div()
                                    .truncate()
                                    .text_color(if dir { strong() } else { text() })
                                    .child(n),
                            )
                    }))
            }));
        col = col.child(
            panel(
                head("p-workspace", "Workspace", "folder", true, false),
                None,
                true,
                Some(list_body_h() + 60.),
                ws_body,
            )
            .flex_initial()
            .min_h(min),
        );
        let title = self
            .active_session()
            .map_or_else(String::new, |s| s.title());
        col = col.child(
            panel(
                head("p-session", "Session", "gallery-vertical-end", true, false),
                Some(dot(hsla(t::SUCCESS), true).into_any_element()),
                true,
                None,
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .mb(px(4.))
                            .child(icon("file", 14., hsla(t::INFO)))
                            .child(div().text_color(hsla(t::INFO)).truncate().child(title))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(dim())
                                    .child(format!("#{}", self.active + 1)),
                            ),
                    )
                    .child(kv(&s.session, 62.)),
            )
            .flex_initial()
            .min_h(min),
        );
        col = col.child(
            panel(
                head("p-tools", "Tools & Skills", "settings-2", true, false),
                None,
                true,
                Some(110.),
                div().child(kv(&s.tools, 62.)).child(mcp_rows(&s.mcp)),
            )
            .flex_initial()
            .min_h(min),
        );
        col.child(
            panel(
                head("p-activity", "Activity", "play", true, false),
                Some(div().child(s.activity.len().to_string()).into_any_element()),
                false,
                None,
                activity_rows(&s.activity),
            )
            .flex_1()
            .min_h(min),
        )
    }

    pub(super) fn right(&self, tier: Tiers, w: f32) -> Div {
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
        let Some(s) = self.snap() else {
            return col;
        };
        let online = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .text_color(hsla(t::SUCCESS))
            .child(dot(hsla(t::SUCCESS), true))
            .child(s.core.tag.clone())
            .into_any_element();
        col = col.child(
            panel(
                head(
                    "p-topology",
                    "Capability Topology",
                    "layout-dashboard",
                    false,
                    tier.compact_head,
                ),
                Some(online),
                false,
                None,
                div().relative().size_full().child(topo::graph(s)),
            )
            .flex_1()
            .min_h(px(tier.topo_min)),
        );
        // the agent's todo list, latest version
        let plan = self
            .active_session()
            .map(|a| a.live.view.convo.plan.clone())
            .unwrap_or_default();
        let done = plan.iter().filter(|p| p.state == StepState::Done).count();
        let body = if plan.is_empty() {
            div().text_color(dim()).child("no plan yet")
        } else {
            div().flex().flex_col().children(plan.iter().map(|p| {
                let (mark, c) = match p.state {
                    StepState::Done => ("\u{2611}", hsla(t::SUCCESS)),
                    StepState::Active => ("\u{25D0}", hsla(t::ACCENT)),
                    StepState::Todo => ("\u{2610}", dim()),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().text_color(c).child(mark))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(match p.state {
                                StepState::Done => dim(),
                                StepState::Active => strong(),
                                StepState::Todo => text(),
                            })
                            .child(p.text.clone()),
                    )
            }))
        };
        col = col.child(
            panel(
                head("p-plan", "Plan", "check", false, false),
                Some(
                    div()
                        .child(format!("{done}/{}", plan.len()))
                        .into_any_element(),
                ),
                true,
                Some(112.),
                body,
            )
            .flex_initial()
            .min_h(px(56.)),
        );
        // `.usage2`: tokens with a bar, sessions, runtime, cost
        let row = |k: &str, v: String| {
            div()
                .flex()
                .gap(px(10.))
                .child(div().w(px(70.)).flex_none().child(k.to_string()))
                .child(div().text_color(strong()).child(v))
        };
        let runtime = s
            .session
            .iter()
            .find(|(k, _)| k == "uptime")
            .map_or("-".to_string(), |(_, v)| v.clone());
        let pct = s.tokens.ctx_pct.clamp(0., 100.);
        let usage = div()
            .flex()
            .flex_col()
            .child(
                div().flex().justify_between().child("tokens").child(
                    div()
                        .text_color(strong())
                        .child(format!("{} ({:.0}%)", s.tokens.ctx_label, pct)),
                ),
            )
            .child(
                div()
                    .my(px(6.))
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(with_alpha(hsla(t::TEXT_STRONG), 0.12))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(pct / 100.))
                            .rounded(px(2.))
                            .bg(hsla(t::SUCCESS)),
                    ),
            )
            .child(row("sessions", self.sessions.len().to_string()))
            .child(row("runtime", runtime))
            .child(row("cost", s.tokens.cost.clone()));
        col.child(
            panel(
                head("p-usage", "Usage", "chart-pie", false, false),
                None,
                true,
                None,
                usage,
            )
            .flex_initial()
            .min_h(px(84.)),
        )
    }
}
