//! The conversation of the current session (`cli.html` `.msg`, `.tool`, `.subagent-view`): messages with
//! the USER / SYNTHESIS avatars, tool cards that open on click, the inline approval rows, and the
//! transcript of the picked sub-agent beside it (`.chatstage.with-subagent`).

use gpui::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use gpui_component::text::TextView;
use synthesis_tui_render::theme as t;
use synthesis_view::convo::{ApprovalState, Item, ToolState};

use super::layout::{GAP, PAD, split};
use super::{AgentView, hhmmss, with_alpha};
use crate::theme::hsla;
use crate::tui_view::chat::{COLLAPSED_LINES, diff_rows, visible};
use crate::tui_view::panels::{dim, dot, icon, strong, text};

/// The icon of a tool card (`cli.html` `toolHTML`: read, search, run, edit ...).
pub fn tool_icon(name: &str) -> &'static str {
    match name {
        "Read" => "book-open",
        "Grep" | "Glob" | "WebSearch" | "WebFetch" => "search",
        "Bash" | "BashOutput" | "KillShell" => "square-terminal",
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "file-text",
        "Task" => "bot",
        n if n.starts_with("browser_") => "globe",
        _ => "settings-2",
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

/// `.msg .av`: the 32px avatar.
fn avatar(user: bool) -> Div {
    let d = div()
        .size(px(32.))
        .flex_none()
        .rounded(px(7.))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden();
    if user {
        d.bg(hsla(t::INFO))
            .border_color(hsla(t::INFO))
            .child(icon("user", 16., hsla(t::BG)))
    } else {
        d.bg(gpui::black())
            .border_color(gpui::black())
            .child(gpui::img(crate::brand::mark()).size(px(26.)))
    }
}

/// `.msg`: avatar, the speaker, the content.
fn msg(user: bool, content: AnyElement) -> Div {
    div()
        .flex()
        .gap(px(10.))
        .px(px(10.))
        .py(px(8.))
        .child(avatar(user))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(hsla(if user { t::INFO } else { t::ACCENT }))
                        .child(if user { "USER" } else { "SYNTHESIS" }),
                )
                .child(content),
        )
}

impl AgentView {
    /// The conversation, with the picked sub-agent's transcript beside it.
    pub(super) fn chat_stage(&self, w: f32, cx: &mut Context<Self>) -> Div {
        let c = super::layout::cols(w);
        // the centre column minus the panel border
        let inner = w - 2. * PAD - 2. * GAP - c.left - c.right - 2.;
        let sub = self
            .active_session()
            .and_then(|s| s.sub.as_deref())
            .and_then(|id| self.sub_pane(id, split(inner).1));
        div()
            .debug_selector(|| "tab-body".to_string())
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
    }

    fn conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.active_session() else {
            return div().into_any_element();
        };
        let l = &s.live;
        let c = &l.view.convo;
        let rows: Vec<AnyElement> = c
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| visible(it, l.show_thinking, l.verbose, l.focus_view))
            .map(|(i, it)| self.item(i, it, cx))
            .collect();
        if rows.is_empty() && !c.running {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .child(gpui::img(crate::brand::mark()).size(px(56.)))
                .child(crate::tui_view::tracked("SYNTHESIS", 14., 0.32, strong()))
                .child(div().text_color(dim()).child(if l.starting {
                    "starting the session..."
                } else {
                    "ask for a change and the work shows up here"
                }))
                .into_any_element();
        }
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .children(rows);
        if let Some(a) = c.activity().filter(|_| l.dialogs.is_empty()) {
            body = body.child(
                div()
                    .ml(px(44.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(dim())
                    .child(dot(hsla(t::ACCENT), true))
                    .child(a),
            );
        }
        let scroll = div()
            .id("chat-scroll")
            .debug_selector(|| "chat-scroll".to_string())
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&s.scroll)
            .child(body);
        let mut wrap = div().relative().flex_1().min_h(px(0.)).child(scroll);
        if !s.follow {
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
                        if let Some(s) = this.active_mut() {
                            s.follow = true;
                        }
                        cx.notify();
                    })),
            );
        }
        wrap.into_any_element()
    }

    fn item(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let under = |d: Div| d.ml(px(44.));
        match it {
            Item::User(s) => msg(
                true,
                div()
                    .min_w(px(0.))
                    .text_color(strong())
                    .child(s.clone())
                    .into_any_element(),
            )
            .into_any_element(),
            Item::Assistant { text: s, .. } => msg(
                false,
                div()
                    .min_w(px(0.))
                    .text_color(strong())
                    .child(TextView::markdown(("a-msg", idx), s.clone()))
                    .into_any_element(),
            )
            .into_any_element(),
            Item::Thinking(s) => under(
                div()
                    .pl(px(10.))
                    .border_l_2()
                    .border_color(hsla(t::BORDER))
                    .text_color(dim())
                    .italic()
                    .child(s.clone()),
            )
            .into_any_element(),
            Item::Tool { .. } => self.tool_card(idx, it, cx),
            Item::Verification { status, summary } => {
                let ok = status == "passed";
                under(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_color(if ok { hsla(t::SUCCESS) } else { hsla(t::WARN) })
                                .child(format!(
                                    "{} verification {status}",
                                    if ok { "\u{2713}" } else { "!" }
                                )),
                        )
                        .children(summary.lines().map(|s| line_box(s, dim()))),
                )
                .into_any_element()
            }
            Item::Approval { prompt, state, .. } => {
                let (word, c) = match state {
                    ApprovalState::Pending => ("waiting for you", hsla(t::WARN)),
                    ApprovalState::Allowed => ("allowed", hsla(t::SUCCESS)),
                    ApprovalState::Denied => ("denied", hsla(t::ERROR)),
                };
                under(
                    div()
                        .flex()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if *state == ApprovalState::Pending {
                            hsla(t::WARN)
                        } else {
                            hsla(t::BORDER)
                        })
                        .child(div().text_color(c).child(format!("permission {word}")))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(dim())
                                .child(prompt.clone()),
                        ),
                )
                .into_any_element()
            }
            Item::Notice { warning, text: s } => {
                let c = if *warning { hsla(t::WARN) } else { dim() };
                under(
                    div()
                        .flex()
                        .flex_col()
                        .children(s.lines().map(|l| line_box(l, c))),
                )
                .into_any_element()
            }
            Item::Agent { kind, running, .. } => under(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(dim())
                    .child(dot(hsla(t::ACCENT), *running))
                    .child(format!(
                        "{kind} agent {}",
                        if *running {
                            "running in the background"
                        } else {
                            "finished"
                        }
                    )),
            )
            .into_any_element(),
            Item::TaskEnd { ok, text: s } => under(
                div()
                    .text_color(if *ok {
                        hsla(t::SUCCESS)
                    } else {
                        hsla(t::ERROR)
                    })
                    .child(format!("{} {s}", if *ok { "\u{2713}" } else { "\u{2717}" })),
            )
            .into_any_element(),
        }
    }

    /// `.tool`: a bordered card, icon + name + argument + state, the output below.
    fn tool_card(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.active_session() else {
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
        let open = s.live.expand_all || s.live.expanded.contains(id);
        let body = if output.is_empty() { preview } else { output };
        let diff_lines = diff.as_ref().map(diff_rows).unwrap_or_default();
        let lines: Vec<(String, gpui::Hsla, Option<gpui::Hsla>)> = if diff_lines.is_empty() {
            body.lines()
                .map(|l| (l.to_string(), text(), None))
                .collect()
        } else {
            diff_lines
        };
        let total = lines.len();
        let cut = !open && total > COLLAPSED_LINES;
        let tid = id.clone();
        let toggle = cx.listener(move |this, _, _, cx| {
            if let Some(s) = this.active_mut() {
                if !s.live.expanded.remove(&tid) {
                    s.live.expanded.insert(tid.clone());
                }
                s.dirty = true;
            }
            cx.notify();
        });
        let mut card = div()
            .ml(px(44.))
            .flex()
            .flex_col()
            .border_1()
            .border_color(hsla(t::BORDER))
            .rounded(px(6.))
            .bg(with_alpha(hsla(t::BG), 0.6))
            .overflow_hidden()
            .child(
                div()
                    .id(("tool-head", idx))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(6.))
                    .cursor_pointer()
                    .on_click(toggle)
                    .child(icon(tool_icon(name), 16., hsla(t::ACCENT)))
                    .child(
                        div()
                            .text_color(strong())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(text())
                            .child(summary.clone()),
                    )
                    .when(*duration_ms > 0, |d| {
                        d.child(div().text_color(dim()).child(format!("{duration_ms} ms")))
                    })
                    .child(div().text_color(c).child(mark)),
            );
        if total > 0 {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(12.))
                    .pb(px(8.))
                    .children(
                        lines
                            .into_iter()
                            .take(if cut { COLLAPSED_LINES } else { usize::MAX })
                            .map(|(l, fg, bg)| {
                                let d = line_box(&l, fg);
                                match bg {
                                    Some(b) => d.bg(b),
                                    None => d,
                                }
                            }),
                    )
                    .when(cut, |d| {
                        d.child(div().text_color(hsla(t::ACCENT)).child(format!(
                            "\u{2026} {} more line{} (click, or Ctrl+O for all)",
                            total - COLLAPSED_LINES,
                            if total - COLLAPSED_LINES == 1 {
                                ""
                            } else {
                                "s"
                            }
                        )))
                    }),
            );
        }
        card.into_any_element()
    }

    /// `.subagent-view`: the transcript of one sub-agent, its model and its state.
    fn sub_pane(&self, id: &str, w: f32) -> Option<Div> {
        let s = self.active_session()?;
        let a = s.live.view.panes.sub(id)?;
        let tz = self.cfg.as_ref().map_or(0, |c| c.tz_min);
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
                .bg(with_alpha(hsla(t::BG), 0.72))
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
                                .child(dot(hsla(t::ACCENT), live))
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
                        .track_scroll(&s.sub_scroll)
                        .px(px(10.))
                        .py(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(9.))
                        .children(a.entries.iter().map(|e| {
                            let c = if e.failed {
                                hsla(t::ERROR)
                            } else if e.kind == synthesis_view::agent::EntryKind::Tool {
                                text()
                            } else {
                                strong()
                            };
                            div()
                                .pl(px(8.))
                                .border_l_2()
                                .border_color(hsla(t::ACCENT))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(dim())
                                        .child(e.ts.map_or(String::new(), |ms| hhmmss(ms, tz))),
                                )
                                .child(line_box(
                                    &if e.kind == synthesis_view::agent::EntryKind::Tool {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_icons_follow_the_kind_of_work() {
        assert_eq!(tool_icon("Read"), "book-open");
        assert_eq!(tool_icon("Grep"), "search");
        assert_eq!(tool_icon("Bash"), "square-terminal");
        assert_eq!(tool_icon("Edit"), "file-text");
        assert_eq!(tool_icon("browser_click"), "globe");
        assert_eq!(tool_icon("Mystery"), "settings-2");
    }
}
