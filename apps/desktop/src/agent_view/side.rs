//! The Agent view's side columns: Workspace, Session, Tools & Skills and Activity on the left (the
//! workspace's sessions are clickable and open as tabs), Topology, Plan and Usage on the right.

use gpui::{
    Context, Div, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div, px,
};
use synthesis_tui_render::theme as t;
use synthesis_view::convo::StepState;

use super::AgentView;
use super::layout::{GAP, LEFT_GAP, Tiers, cols};
use crate::theme::hsla;
use crate::tui_view::panels::{self, OnClick, dim, dot, icon, strong, text};
use crate::tui_view::sample::Snapshot;
use crate::tui_view::topo;
use crate::widgets::chrome::{Head, Pad, panel};

fn head(id: &'static str, title: &'static str, icon: &'static str, pad: Pad) -> Head {
    Head::new(id, title, icon, pad)
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
            .child(panels::running_body(
                &rows,
                (self.cfg.is_some()).then_some(&click as &dyn Fn(usize) -> OnClick),
                None,
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
                head("p-workspace", "Workspace", "folder", Pad::LEFT),
                None,
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
                head("p-session", "Session", "square-terminal", Pad::LEFT),
                Some(panels::session_meta(&s.session)),
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
                    .child(panels::session_body(&s.session)),
            )
            .flex_initial()
            .min_h(min),
        );
        col = col.child(
            panel(
                head("p-tools", "Tools & Skills", "wrench", Pad::LEFT).max_body(110.),
                Some(div().child(s.tools.count.clone()).into_any_element()),
                panels::tools_body(&s.tools),
            )
            .flex_initial()
            .min_h(min),
        );
        col.child(
            panel(
                head("p-activity", "Recent Activity", "activity", Pad::LEFT),
                None,
                panels::activity_body(&s.activity),
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
            .child("online")
            .into_any_element();
        col = col.child(
            panel(
                head(
                    "p-topology",
                    "Synthesis Topology",
                    "network",
                    if tier.compact_head {
                        Pad::COMPACT
                    } else {
                        Pad::NORMAL
                    },
                ),
                Some(online),
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
                head("p-plan", "Plan", "sparkles", Pad::NORMAL).max_body(112.),
                Some(
                    div()
                        .child(format!("{done}/{}", plan.len()))
                        .into_any_element(),
                ),
                body,
            )
            .flex_initial()
            .min_h(px(56.)),
        );
        let mut h = head("p-usage", "Usage & Metrics", "chart-bar", Pad::NORMAL);
        h.meta_h = 22.6;
        col.child(
            panel(
                h,
                Some(panels::usage_meta(&s.usage)),
                panels::usage_body(&s.usage),
            )
            .flex_initial()
            .min_h(px(84.)),
        )
    }
}
