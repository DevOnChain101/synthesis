//! The centre of the TUI view when it is live: the conversation drawn from `Convo` with the TUI's "quiet
//! chat" rules (docs/TUI.md "Events": what a person needs, nothing else), the prompt box with its
//! popups, and the "jump to latest" button.

use gpui::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, list, prelude::*, px,
};
use gpui_component::input::Textarea;
use gpui_component::text::TextView;
use serde_json::Value;
use synthesis_tui_render::theme as t;
use synthesis_view::convo::{ApprovalState, Item, StepState, ToolState};

use super::panels::{dim, icon, strong, text};
use super::topo::with_alpha;
use super::tracked;
use super::{TuiView, layout};
use crate::theme::hsla;

/// Lines of a tool's output shown before "ctrl+o" (the TUI's default).
pub const COLLAPSED_LINES: usize = 6;

/// Whether a row of the quiet chat is shown. `verbose` is the TUI's switch for the internal rows
/// (`Item::Internal`: memory, peer and hook bookkeeping) and widens what approvals show.
pub fn visible(it: &Item, show_thinking: bool, verbose: bool, focus: bool) -> bool {
    // `/focus`: prompts, answers, results and notices only (the TUI's focus kinds)
    if focus
        && !matches!(
            it,
            Item::User(_) | Item::Assistant { .. } | Item::TaskEnd { .. } | Item::Notice { .. }
        )
    {
        return false;
    }
    match it {
        Item::Thinking(t) => show_thinking && !t.trim().is_empty(),
        // a granted permission is bookkeeping; waiting and denied ones are what a person needs
        Item::Approval { state, .. } => verbose || *state != ApprovalState::Allowed,
        Item::Internal(_) => verbose,
        Item::Agent { .. } => true,
        Item::Assistant { text, streaming } => *streaming || !text.trim().is_empty(),
        _ => true,
    }
}

/// Left edge of what sits under a speaker's text: the row's 10px padding, the 34px avatar column, the 10px gap.
const INDENT: f32 = 54.;

/// `.msg`: a 32px avatar (radius 7) and, beside it, the tracked who label over the body (`cli.html`).
fn msg(avatar: Div, who: &str, who_colour: gpui::Hsla, time: &str, body: Div) -> Div {
    div()
        .flex()
        .gap(px(10.))
        .px(px(10.))
        .py(px(8.))
        .child(
            div()
                .size(px(32.))
                .flex_none()
                .rounded(px(7.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .overflow_hidden()
                .child(avatar),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(10.))
                        .child(
                            tracked(who, 12.5, 0.06, who_colour).font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(div().text_color(dim()).child(time.to_string())),
                )
                .child(body),
        )
}

/// The title of a tool card (`cli.html`: "Reading file", "Searching", "Running", "Applying patch").
fn tool_title(name: &str) -> String {
    match name {
        "Read" => "Reading file".into(),
        "Grep" | "Glob" | "WebSearch" | "WebFetch" => "Searching".into(),
        "Bash" | "BashOutput" | "KillShell" => "Running".into(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "Applying patch".into(),
        "Task" => "Agent".into(),
        n if n.starts_with("browser_") => format!("Browser {}", n.trim_start_matches("browser_")),
        n => n.to_string(),
    }
}

/// The icon and title colour of a tool card (`.tool .th`): by what the tool does.
fn tool_look(name: &str) -> (&'static str, gpui::Hsla) {
    match name {
        "Read" => ("book-open", hsla(t::INFO)),
        "Grep" | "Glob" => ("search", hsla(t::ACCENT_LIGHT)),
        "Bash" => ("square-terminal", hsla(t::WARN)),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ("replace", hsla(t::DIFF_REMOVE)),
        _ => ("wrench", hsla(t::SUCCESS)),
    }
}

fn line_box(s: &str, c: gpui::Hsla) -> Div {
    div()
        .min_w(px(0.))
        .whitespace_normal()
        .text_color(c)
        .child(if s.is_empty() {
            " ".to_string()
        } else {
            s.to_string()
        })
}

impl TuiView {
    /// The conversation, with the picked sub-agent's transcript beside it (`.chatstage.with-subagent`: the
    /// conversation takes 2fr, the sub-agent 1fr, at least 220px).
    pub(super) fn chat_stage(&self, cx: &mut Context<Self>) -> AnyElement {
        let inner = self.centre_w - 2.;
        let sub_w = (inner / 3.).max(220.).min(inner);
        let sub = self
            .ts
            .sub
            .as_deref()
            .and_then(|id| self.sub_pane(id, sub_w));
        if sub.is_none() {
            return self.conversation(cx);
        }
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.conversation(cx)),
            )
            .children(sub)
            .into_any_element()
    }

    /// `.subagent-view`: the transcript of one sub-agent, its model and its state.
    fn sub_pane(&self, id: &str, w: f32) -> Option<Div> {
        let lu = self.live.as_ref()?;
        let a = lu.live.view.panes.sub(id)?;
        let tz = lu.cfg.tz_min;
        let live = a.live();
        Some(
            div()
                .debug_selector(|| "subagent".to_string())
                .w(px(w))
                .flex_none()
                .min_h(px(0.))
                .my(px(10.))
                .mr(px(12.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .border_1()
                .border_color(hsla(t::ACCENT))
                .rounded(px(7.))
                .bg(super::with_alpha_black(0.72))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_between()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(8.))
                        .border_b_1()
                        .border_color(hsla(t::RULE))
                        .whitespace_nowrap()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_color(hsla(t::ACCENT))
                                .font_weight(FontWeight::BOLD)
                                .child(super::panels::dot(hsla(t::ACCENT), live))
                                .child(format!("@{}", a.name)),
                        )
                        .child(
                            div().min_w(px(0.)).truncate().text_color(text()).child(
                                div()
                                    .flex()
                                    .gap(px(5.))
                                    .child(div().text_color(hsla(t::INFO)).child(
                                        if a.model.is_empty() {
                                            a.kind.clone()
                                        } else {
                                            a.model.clone()
                                        },
                                    ))
                                    .child("*")
                                    .child(
                                        div().text_color(hsla(t::ACCENT)).child(a.status.clone()),
                                    ),
                            ),
                        ),
                )
                .child(
                    div()
                        .id("subagent-log")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.sub_scroll)
                        .px(px(10.))
                        .py(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(9.))
                        .children(a.entries.iter().map(|e| {
                            let tool = e.kind == synthesis_view::agent::EntryKind::Tool;
                            let c = if e.failed {
                                hsla(t::ERROR)
                            } else if tool {
                                text()
                            } else {
                                strong()
                            };
                            div()
                                .pl(px(8.))
                                .border_l_2()
                                .border_color(hsla(t::ACCENT))
                                .child(div().text_size(px(11.)).text_color(dim()).child(
                                    e.ts.map_or(String::new(), |ms| super::snapshot::clock(ms, tz)),
                                ))
                                .child(line_box(
                                    &if tool {
                                        format!("\u{25B8} {}", e.text)
                                    } else {
                                        e.text.clone()
                                    },
                                    c,
                                ))
                        }))
                        .when(a.entries.is_empty(), |d| {
                            d.child(div().text_color(dim()).child("nothing yet"))
                        }),
                ),
        )
    }

    /// The scrolling conversation (or the welcome screen when there is nothing yet). A virtual
    /// list: only the rows near the viewport are built, so a 2000-row session costs what a short
    /// one does.
    pub(super) fn conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        if lu.rows.is_empty() {
            return self.welcome().into_any_element();
        }
        let rows = list(
            lu.list.clone(),
            cx.processor(|this: &mut Self, ix: usize, _window, cx| this.row(ix, cx)),
        )
        .size_full()
        .px(px(12.))
        .pb(px(10.));
        // the padding is outside the list: a row scrolled to the top edge stops below it
        let mut wrap = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .pt(px(10.))
            .debug_selector(|| "chat-scroll".to_string())
            .child(rows);
        if !lu.follow {
            wrap = wrap.child(
                div()
                    .id("jump-latest")
                    .absolute()
                    .bottom(px(10.))
                    .right(px(24.))
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(14.))
                    .border_1()
                    .border_color(hsla(t::ACCENT))
                    .bg(hsla(t::ELEVATED))
                    .text_color(strong())
                    .cursor_pointer()
                    .child("\u{2193} Jump to latest")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(lu) = &mut this.live {
                            lu.stick();
                        }
                        cx.notify();
                    })),
            );
        }
        wrap.into_any_element()
    }

    /// Row `ix` of the chat list, with the gap below it.
    fn row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        let c = &lu.live.view.convo;
        // everything but the messages sits under the speaker's text (avatar column + gap)
        let under = |d: AnyElement| div().ml(px(INDENT)).child(d).into_any_element();
        let el = match lu.rows.get(ix) {
            Some(super::driver::Row::Item(i)) => match c.items.get(*i) {
                Some(it @ (Item::User(_) | Item::Assistant { .. })) => self.item(*i, it, cx),
                Some(it) => under(self.item(*i, it, cx)),
                None => div().into_any_element(),
            },
            Some(super::driver::Row::Plan) => under(self.plan_card().into_any_element()),
            Some(super::driver::Row::Activity) => div()
                .ml(px(INDENT))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(dim())
                .child(super::panels::dot(hsla(t::ACCENT), true))
                .children(c.activity())
                .into_any_element(),
            None => div().into_any_element(),
        };
        div().pb(px(10.)).child(el).into_any_element()
    }

    fn plan_card(&self) -> Div {
        let Some(lu) = &self.live else { return div() };
        let steps = &lu.live.view.convo.plan;
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(10.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .rounded(px(layout::PANEL_RADIUS))
            .child(
                div()
                    .text_color(strong())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Plan"),
            )
            .children(steps.iter().map(|s| {
                let (mark, c) = match s.state {
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
                            .text_color(if s.state == StepState::Done {
                                dim()
                            } else {
                                text()
                            })
                            .child(s.text.clone()),
                    )
            }))
    }

    fn item(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        // `HH:MM` of the event that made the row
        let time = lu.live.view.convo.time_of(idx).map_or(String::new(), |ms| {
            super::snapshot::clock(ms, lu.cfg.tz_min)[..5].to_string()
        });
        match it {
            Item::User(s) => msg(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(hsla(t::INFO))
                    .child(icon("user", 14., hsla(t::BG))),
                "USER",
                hsla(t::INFO),
                &time,
                div().text_color(strong()).child(s.clone()),
            )
            .into_any_element(),
            Item::Assistant { text: s, .. } => msg(
                div().size_full().bg(gpui::black()).child(
                    gpui::img(crate::brand::mark())
                        .size_full()
                        .object_fit(gpui::ObjectFit::Cover),
                ),
                "SYNTHESIS",
                hsla(t::ACCENT),
                &time,
                div()
                    .min_w(px(0.))
                    .text_color(strong())
                    .child(TextView::markdown(("assistant", idx), s.clone())),
            )
            .into_any_element(),
            Item::Thinking(s) => div()
                .pl(px(10.))
                .border_l_2()
                .border_color(hsla(t::BORDER))
                .text_color(dim())
                .italic()
                .child(s.clone())
                .into_any_element(),
            Item::Tool { .. } => self.tool_card(idx, it, cx),
            Item::Verification { status, summary } => {
                let ok = status == "passed";
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_color(if ok { hsla(t::SUCCESS) } else { hsla(t::WARN) })
                            .child(format!(
                                "{} verification {status}",
                                if ok { "\u{2713}" } else { "!" }
                            )),
                    )
                    .children(summary.lines().map(|s| line_box(s, dim())))
                    .into_any_element()
            }
            Item::Approval { prompt, state, .. } => {
                let (word, c) = match state {
                    ApprovalState::Pending => ("waiting for you", hsla(t::WARN)),
                    ApprovalState::Allowed => ("allowed", hsla(t::SUCCESS)),
                    ApprovalState::Denied => ("denied", hsla(t::ERROR)),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().text_color(c).child(format!("permission {word}")))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_color(dim())
                            .child(prompt.clone()),
                    )
                    .into_any_element()
            }
            Item::Notice { warning, text: s } => div()
                .flex()
                .flex_col()
                .text_color(if *warning { hsla(t::WARN) } else { dim() })
                .children(
                    s.lines()
                        .map(|l| line_box(l, if *warning { hsla(t::WARN) } else { dim() })),
                )
                .into_any_element(),
            Item::Agent { kind, running, .. } => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(dim())
                .child(super::panels::dot(hsla(t::ACCENT), *running))
                .child(format!(
                    "{kind} agent {}",
                    if *running {
                        "running in the background"
                    } else {
                        "finished"
                    }
                ))
                .into_any_element(),
            Item::Internal(s) => div().text_color(dim()).child(s.clone()).into_any_element(),
            Item::TaskEnd {
                ok,
                text: s,
                word,
                checks,
            } => {
                // unverified / blocked / cancelled are a warning, failed is an error, never green
                let (mark, c) = match (*ok, word.as_str()) {
                    (true, _) => ("\u{2713}", hsla(t::SUCCESS)),
                    (_, "unverified" | "blocked" | "cancelled") => ("!", hsla(t::WARN)),
                    _ => ("\u{2717}", hsla(t::ERROR)),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(c).child(format!("{mark} {s}")))
                    .children(checks.lines().map(|l| line_box(l, dim())))
                    .into_any_element()
            }
        }
    }

    fn tool_card(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        let Item::Tool {
            id,
            name,
            summary,
            state,
            preview,
            output,
            duration_ms,
            diff,
            ..
        } = it
        else {
            return div().into_any_element();
        };
        let (mark, c) = match state {
            ToolState::Running => ("\u{25CF}", hsla(t::WARN)),
            ToolState::Ok => ("\u{2713}", hsla(t::SUCCESS)),
            ToolState::Failed => ("\u{2717}", hsla(t::ERROR)),
            ToolState::NotRun => ("\u{2013}", dim()),
        };
        let open = lu.live.expand_all || lu.live.expanded.contains(id);
        let body = if output.is_empty() { preview } else { output };
        let diff_lines = diff.as_ref().map(diff_rows).unwrap_or_default();
        let shown_diff = !diff_lines.is_empty();
        let (ic, kind_col) = tool_look(name);
        // the numbers on the right of the header: lines read, results, +/- of a patch, else the duration
        let n_lines = body.lines().count();
        let count = |k: &str| {
            diff.as_ref()
                .and_then(|d| d.get(k))
                .and_then(serde_json::Value::as_u64)
        };
        let meta: Vec<(String, gpui::Hsla)> =
            match (name.as_str(), count("added"), count("removed")) {
                ("Edit" | "Write" | "MultiEdit" | "NotebookEdit", Some(a), Some(r)) => {
                    vec![
                        (format!("+{a}"), hsla(t::SUCCESS)),
                        (format!("\u{2212}{r}"), hsla(t::ERROR)),
                    ]
                }
                ("Read", _, _) if n_lines > 0 => vec![(format!("{n_lines} lines"), dim())],
                ("Grep" | "Glob", _, _) if n_lines > 0 => {
                    vec![(format!("{n_lines} results"), dim())]
                }
                _ if *duration_ms > 0 => vec![(crate::widgets::panes::fmt_ms(*duration_ms), dim())],
                _ => vec![],
            };
        let tid0 = id.clone();
        let mut card = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(hsla(t::BORDER))
            .rounded(px(6.))
            .bg(super::with_alpha_black(0.6))
            .overflow_hidden()
            .child(
                div()
                    .id(("tool-head", idx))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .hover(|d| d.bg(super::with_alpha_white(0.03)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(lu) = &mut this.live {
                            let e = &mut lu.live.expanded;
                            if !e.remove(&tid0) {
                                e.insert(tid0.clone());
                            }
                        }
                        cx.notify();
                    }))
                    .child(icon(ic, 16., kind_col))
                    .child(
                        div()
                            .flex_none()
                            .text_color(kind_col)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tool_title(name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(text())
                            .child(summary.clone()),
                    )
                    .children(
                        meta.into_iter()
                            .map(|(m, c)| div().flex_none().text_color(c).child(m)),
                    )
                    .child(div().flex_none().text_color(c).child(mark)),
            );
        let mut lines: Vec<(String, gpui::Hsla, Option<gpui::Hsla>)> = vec![];
        if shown_diff {
            lines.extend(diff_lines);
        } else {
            lines.extend(body.lines().map(|l| (l.to_string(), text(), None)));
        }
        if !lines.is_empty() {
            let total = lines.len();
            let cut = !open && total > COLLAPSED_LINES;
            let more = total - COLLAPSED_LINES.min(total);
            let tid = id.clone();
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(10.))
                    .py(px(4.))
                    .children(
                        lines
                            .into_iter()
                            .take(if cut { COLLAPSED_LINES } else { usize::MAX })
                            .map(|(s, fg, bg)| {
                                let d = line_box(&s, fg);
                                match bg {
                                    Some(b) => d.bg(b),
                                    None => d,
                                }
                            }),
                    )
                    .when(cut || (open && total > COLLAPSED_LINES), |d| {
                        d.child(
                            div()
                                .id(("tool-more", idx))
                                .cursor_pointer()
                                .text_color(hsla(t::ACCENT))
                                .child(if cut {
                                    format!(
                                        "\u{2026} {more} more line{} (click or Ctrl+O)",
                                        if more == 1 { "" } else { "s" }
                                    )
                                } else {
                                    "show less".to_string()
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(lu) = &mut this.live {
                                        let e = &mut lu.live.expanded;
                                        if !e.remove(&tid) {
                                            e.insert(tid.clone());
                                        }
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        card.into_any_element()
    }

    pub(super) fn welcome(&self) -> Div {
        let s = &self.snap;
        div()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(gpui::img(crate::brand::mark()).size(px(72.)))
            .child(super::tracked("SYNTHESIS", 16., 0.32, strong()))
            .child(div().text_color(dim()).child("autonomous coding agent"))
            .child(
                div()
                    .mt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .w_full()
                    .max_w(px(560.))
                    .px(px(16.))
                    .children(s.tips.iter().map(|tip| {
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().text_color(hsla(t::ACCENT)).child("\u{00B7}"))
                            .child(div().child(tip.clone()))
                    })),
            )
    }

    /// The live prompt: the text box, the queue count, the search line and the completion popup.
    pub(super) fn live_prompt(&self, cx: &mut Context<Self>) -> Div {
        let Some(lu) = &self.live else { return div() };
        let l = &lu.live;
        let mut col = div().flex().flex_col().flex_none();
        if let Some(c) = &lu.comp {
            col = col.child(
                div()
                    .debug_selector(|| "completion".to_string())
                    .mx(px(12.))
                    .mb(px(4.))
                    .p(px(4.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .rounded(px(layout::PANEL_RADIUS))
                    .bg(hsla(t::ELEVATED))
                    .children(c.items.iter().enumerate().map(|(i, (label, detail))| {
                        div()
                            .flex()
                            .gap(px(12.))
                            .px(px(8.))
                            .rounded(px(4.))
                            .when(i == c.sel, |d| d.bg(hsla(t::BORDER)))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(if i == c.sel { strong() } else { text() })
                                    .child(label.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(dim())
                                    .child(detail.clone()),
                            )
                    })),
            );
        }
        if let Some((needle, _)) = &lu.search {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .text_color(dim())
                    .child(format!(
                        "(history search) `{needle}`  Ctrl+R older, Enter keeps, Esc leaves"
                    )),
            );
        }
        if !l.images.is_empty() {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .flex()
                    .gap(px(6.))
                    .children(l.images.iter().map(|a| {
                        div()
                            .px(px(8.))
                            .rounded(px(10.))
                            .border_1()
                            .border_color(hsla(t::ACCENT))
                            .text_color(strong())
                            .child(format!("{} {:.1} MB", a.name, a.bytes as f64 / 1e6))
                    }))
                    .child(div().text_color(dim()).child("/images clear drops them")),
            );
        }
        if !l.queue.is_empty() {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .text_color(dim())
                    .child(format!(
                        "{} queued: starts when the task ends (Esc drops it)",
                        l.queue.len()
                    )),
            );
        }
        let busy = l.busy || l.starting;
        col.child(input_box(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(Textarea::new(&lu.prompt).appearance(false).bordered(false)),
            l.focus == crate::live::Focus::Chat,
            busy,
            Some(Box::new(cx.listener(|this, _, window, cx| {
                this.submit_prompt(window, cx)
            }))),
        ))
    }
}

/// `.inputbar`: 46px box (border + 44px form), margin 0 12px 12px, the `>` prompt mark, `content`, and the
/// 28px send button. The border takes the accent while the prompt has the keys.
pub fn input_box(
    content: impl IntoElement,
    focused: bool,
    busy: bool,
    send: Option<super::panels::OnClick>,
) -> Div {
    div()
        .debug_selector(|| "prompt".to_string())
        .flex_none()
        .mx(px(12.))
        .mb(px(12.))
        .min_h(px(46.))
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .border_1()
        .border_color(hsla(if focused { t::ACCENT } else { t::BORDER }))
        .rounded(px(layout::PANEL_RADIUS))
        .bg(hsla(t::SURFACE))
        .child(
            div()
                .text_color(strong())
                .child(if busy { "\u{2026}" } else { ">" }),
        )
        .child(content)
        .child(
            div()
                .id("send")
                .debug_selector(|| "send".to_string())
                .size(px(28.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .cursor_pointer()
                .when_some(send, |d, f| d.on_click(f))
                .child(icon("send", 14., hsla(t::ACCENT))),
        )
}

/// Diff rows (text, colour, background) of a `ToolCompleted.diff`. A secret file's diff carries
/// counts only (`hidden`): then there is nothing to draw.
pub fn diff_rows(d: &Value) -> Vec<(String, gpui::Hsla, Option<gpui::Hsla>)> {
    if d.get("hidden").and_then(Value::as_bool) == Some(true) {
        return vec![];
    }
    let mut out = vec![];
    let n = |k: &str| d.get(k).and_then(Value::as_u64).unwrap_or(0);
    if let Some(p) = d.get("path").and_then(Value::as_str) {
        out.push((
            format!("{p}  +{} -{}", n("added"), n("removed")),
            strong(),
            None,
        ));
    }
    for h in d
        .get("hunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for l in h
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let tag = l.get("tag").and_then(Value::as_str).unwrap_or(" ");
            let tx = l.get("text").and_then(Value::as_str).unwrap_or("");
            let (fg, bg) = match tag {
                "+" => (hsla(t::SUCCESS), Some(with_alpha(hsla(t::SUCCESS), 0.10))),
                "-" => (hsla(t::ERROR), Some(with_alpha(hsla(t::ERROR), 0.10))),
                _ => (dim(), None),
            };
            out.push((format!("{tag} {tx}"), fg, bg));
        }
    }
    if d.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push(("\u{2026} diff cut".into(), dim(), None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quiet_chat_hides_what_a_person_does_not_need() {
        let thinking = Item::Thinking("hm".into());
        assert!(!visible(&thinking, false, false, false));
        assert!(visible(&thinking, true, false, false));
        let ap = |state| Item::Approval {
            id: "a".into(),
            tool_call_id: "t".into(),
            prompt: "p".into(),
            state,
        };
        assert!(!visible(&ap(ApprovalState::Allowed), false, false, false));
        assert!(visible(&ap(ApprovalState::Allowed), false, true, false));
        assert!(visible(&ap(ApprovalState::Pending), false, false, false));
        assert!(visible(&ap(ApprovalState::Denied), false, false, false));
        assert!(!visible(
            &Item::Assistant {
                text: " ".into(),
                streaming: false
            },
            false,
            false,
            false
        ));
        // /focus keeps the conversation and drops the work
        assert!(visible(&Item::User("x".into()), false, false, true));
        assert!(!visible(&Item::Thinking("hm".into()), true, false, true));
        // internal rows: verbose only, and never in /focus
        let internal = Item::Internal("memory: 1 note(s) retrieved (12 tokens)".into());
        assert!(!visible(&internal, false, false, false));
        assert!(visible(&internal, false, true, false));
        assert!(!visible(&internal, false, true, true));
        // what needs the person stays in the quiet chat and in /focus
        let warn = Item::Notice {
            warning: true,
            text: "budget exceeded: turns 5 / 5".into(),
        };
        assert!(visible(&warn, false, false, false) && visible(&warn, false, false, true));
    }

    #[test]
    fn a_hidden_diff_draws_nothing() {
        let d = serde_json::json!({"path": ".env", "added": 1, "removed": 0, "hunks": [], "hidden": true});
        assert!(diff_rows(&d).is_empty());
        let d = serde_json::json!({"path": "a.rs", "added": 1, "removed": 1, "truncated": false,
            "hunks": [{"lines": [{"tag": "-", "text": "x"}, {"tag": "+", "text": "y"}, {"tag": " ", "text": "z"}]}]});
        let r = diff_rows(&d);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].0, "a.rs  +1 -1");
        assert!(r[1].2.is_some() && r[2].2.is_some() && r[3].2.is_none());
    }

    /// The row list of one conversation under the quiet-chat and /focus switches.
    #[test]
    fn the_row_list_of_a_conversation_with_every_new_row_kind() {
        use synthesis_view::convo::Convo;
        let ev = |t: &str, p: serde_json::Value| -> synthesis_events::Envelope {
            serde_json::from_value(
                serde_json::json!({"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z",
                "session_id":"s","task_id":null,"turn_id":null,"type":t,"payload":p}),
            )
            .unwrap()
        };
        let addr = serde_json::json!({"workspace_key":"k","workspace_label":"api","session_id":"s2","session_name":"backend","instance":"i"});
        let mut c = Convo::default();
        for e in [
            ev(
                "user.message",
                serde_json::json!({"text":"ship it","images":[]}),
            ),
            ev(
                "memory.retrieved",
                serde_json::json!({"ids":["m"],"tokens":9,"candidates":1}),
            ),
            ev(
                "hook.ran",
                serde_json::json!({"event":"Stop","hook_name":"lint","exit_code":0,"outcome":"success","duration_ms":1}),
            ),
            ev(
                "hook.ran",
                serde_json::json!({"event":"Stop","hook_name":"lint","exit_code":1,"outcome":"error","duration_ms":1}),
            ),
            ev(
                "browser.login_required",
                serde_json::json!({"origin":"https://x.test","profile":"p"}),
            ),
            ev(
                "peer.delegation.accepted",
                serde_json::json!({"delegation_id":"d","request_id":"r","from":addr,"mode":"plan","cap":"plan"}),
            ),
            ev(
                "peer.reply",
                serde_json::json!({"request_id":"r","status":"answered","text":"42","bytes":2,"duration_ms":1}),
            ),
            ev(
                "budget.exceeded",
                serde_json::json!({"budget":"turns","limit":5.0,"used":5.0}),
            ),
            ev(
                "verification.completed",
                serde_json::json!({"report_id":"r","status":"unverified","attempt":1,"summary":"cargo test: not run"}),
            ),
            ev(
                "task.ended",
                serde_json::json!({"status":"unverified","reason":"no_check_ran","usage":{"input_tokens":0,"output_tokens":0},"changed_files":[],"artifacts_root":"/x"}),
            ),
        ] {
            c.apply(&e);
        }
        let rows = |verbose, focus| -> Vec<String> {
            c.items
                .iter()
                .filter(|i| visible(i, false, verbose, focus))
                .map(|i| match i {
                    Item::User(s) => format!("user: {s}"),
                    Item::Notice { warning, text } => {
                        format!("{}: {text}", if *warning { "warn" } else { "note" })
                    }
                    Item::Internal(t) => format!("internal: {t}"),
                    Item::TaskEnd { text, checks, .. } => format!("end: {text} [{checks}]"),
                    o => format!("{o:?}"),
                })
                .collect()
        };
        let quiet = [
            "user: ship it",
            "warn: hook lint (Stop): error",
            "warn: log in to https://x.test in the browser window, then confirm",
            "note: working for peer backend (plan mode)",
            "note: peer reply: answered  42",
            "warn: budget exceeded: turns 5 / 5",
            "end: task unverified: no_check_ran [cargo test: not run]",
        ];
        assert_eq!(rows(false, false), quiet);
        let verbose = rows(true, false);
        assert_eq!(verbose.len(), quiet.len() + 2);
        assert_eq!(
            verbose[1],
            "internal: memory: 1 note(s) retrieved (9 tokens)"
        );
        assert_eq!(verbose[2], "internal: hook lint (Stop): success");
        // /focus: the conversation and what needs the person; the same with verbose
        assert_eq!(rows(false, true), quiet);
        assert_eq!(rows(true, true), quiet);
    }
}
