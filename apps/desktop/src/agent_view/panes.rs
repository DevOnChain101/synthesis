//! The tabs next to the chat that only the Agent view has (verification reports, the project files); the
//! diff, command history and browser panes are shared (`widgets::panes`). All read-only, all from
//! `synthesis_view` state (`AgentPanes`, the stored reports, `project::scan`); none is a terminal.

use gpui::{
    Context, Div, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use synthesis_tui_render::theme as t;
use synthesis_view::agent::{CheckRow, VerifyRun};
use synthesis_view::project;

use super::{AgentView, Tab};
use crate::theme::hsla;
use crate::tui_view::panels::{dim, icon, strong, text, with_alpha};
use crate::widgets::panes::{card, fmt_ms, git_line, note, status_look};

impl AgentView {
    fn diff_tab(&self, cx: &mut Context<Self>) -> Div {
        let Some(s) = self.active_session() else {
            return div();
        };
        let toggle = |path: String| -> crate::widgets::chrome::OnClick {
            Box::new(cx.listener(move |this, _, _, cx| {
                if !this.diff_open.remove(&path) {
                    this.diff_open.insert(path.clone());
                }
                cx.notify();
            }))
        };
        crate::widgets::panes::diff(&s.live, &self.diff_open, &toggle)
    }

    /// The body of a tab other than chat.
    pub(super) fn pane(&self, tab: Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let inner = match tab {
            Tab::Chat => div(),
            Tab::Diff => self.diff_tab(cx),
            Tab::Commands => self
                .active_session()
                .map_or_else(div, |s| crate::widgets::panes::commands(&s.live)),
            Tab::Verify => self.verify_pane(),
            Tab::Browser => self
                .active_session()
                .map_or_else(div, |s| crate::widgets::panes::browser(&s.live)),
            Tab::Files => self.files_pane(cx),
        };
        div()
            .id(tab.label())
            .debug_selector(|| "tab-body".to_string())
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(px(14.))
            .py(px(10.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(inner)
            .into_any_element()
    }

    fn check_row(c: &CheckRow) -> Div {
        let (mark, col) = status_look(&c.status);
        let mut right = vec![];
        if let Some((p, f, k)) = c.tests {
            right.push(format!("{p} passed \u{b7} {f} failed \u{b7} {k} skipped"));
        }
        if let Some(n) = c.exit_code {
            right.push(format!("exit {n}"));
        }
        if c.duration_ms > 0 {
            right.push(fmt_ms(c.duration_ms));
        }
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().w(px(12.)).flex_none().text_color(col).child(mark))
                    .child(
                        div()
                            .w(px(92.))
                            .flex_none()
                            .text_color(strong())
                            .child(c.kind.replace('_', " ")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(if c.command.is_empty() {
                                "(no command)".to_string()
                            } else {
                                c.command.clone()
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(dim())
                            .child(right.join(" \u{b7} ")),
                    ),
            )
            .when(!c.summary.trim().is_empty(), |d| {
                d.child(
                    div()
                        .pl(px(114.))
                        .text_color(dim())
                        .child(c.summary.lines().next().unwrap_or("").to_string()),
                )
            })
            .when(!c.blocked_reason.is_empty(), |d| {
                d.child(
                    div()
                        .pl(px(114.))
                        .text_color(hsla(t::WARN))
                        .child(format!("blocked: {}", c.blocked_reason)),
                )
            })
    }

    fn verify_card(v: &VerifyRun) -> Div {
        let (mark, col) = status_look(&v.status);
        let mut body = div().flex().flex_col().gap(px(4.)).px(px(12.)).py(px(8.));
        if v.loaded {
            body = body.children(v.checks.iter().map(Self::check_row));
            if v.checks.is_empty() {
                body = body.child(note("the report lists no check"));
            }
            if !v.limitations.is_empty() {
                body =
                    body.child(
                        div()
                            .mt(px(4.))
                            .flex()
                            .flex_col()
                            .child(div().text_color(strong()).child("limitations"))
                            .children(v.limitations.iter().map(|l| {
                                div().text_color(hsla(t::WARN)).child(format!("\u{b7} {l}"))
                            })),
                    );
            }
            if !v.changed_files.is_empty() {
                body = body.child(
                    div()
                        .text_color(dim())
                        .child(format!("changed: {}", v.changed_files.join(", "))),
                );
            }
        } else {
            body = body.child(note("reading the stored report..."));
        }
        card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(7.))
                    .bg(with_alpha(gpui::black(), 0.16))
                    .border_b_1()
                    .border_color(hsla(t::RULE))
                    .child(
                        div()
                            .text_color(col)
                            .font_weight(FontWeight::BOLD)
                            .child(mark),
                    )
                    .child(
                        div()
                            .text_color(col)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(v.status.clone()),
                    )
                    .child(
                        div()
                            .text_color(dim())
                            .child(format!("attempt {}", v.attempt)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(dim())
                            .child(v.summary.clone()),
                    ),
            )
            .child(body)
    }

    fn verify_pane(&self) -> Div {
        let Some(s) = self.active_session() else {
            return div();
        };
        let reports = &s.live.view.panes.verify;
        if reports.is_empty() {
            return note(
                "No verification ran yet. After a change the agent's checks and their evidence show up here.",
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(reports.iter().rev().map(Self::verify_card))
    }

    fn files_pane(&self, cx: &mut Context<Self>) -> Div {
        let Some(d) = self.tree.data.as_ref() else {
            return note(if self.tree.loading {
                "reading the project..."
            } else {
                "The project tree loads when this tab opens."
            });
        };
        if let Some(n) = &d.note {
            return note(n.clone());
        }
        // rows are depth-first: keep the path of the open directories to name each file
        let mut stack: Vec<String> = vec![];
        let mut rows = div().flex().flex_col();
        for (i, r) in d.rows.iter().enumerate() {
            stack.truncate(usize::from(r.depth));
            let path = stack
                .iter()
                .map(String::as_str)
                .chain([r.name.as_str()])
                .collect::<Vec<_>>()
                .join("/");
            if r.dir {
                stack.push(r.name.clone());
            }
            let sel = self.tree.sel.as_deref() == Some(path.as_str());
            let st = r.status.map(|c| {
                (
                    c.to_string(),
                    hsla(match c {
                        'A' | '?' => t::SUCCESS,
                        'D' => t::ERROR,
                        _ => t::WARN,
                    }),
                )
            });
            let click_path = path.clone();
            rows = rows.child(
                div()
                    .id(("tree-row", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(21.))
                    .pl(px(f32::from(r.depth) * 14.))
                    .when(sel, |d| d.bg(hsla(t::ELEVATED)))
                    .when(!r.dir && r.status.is_some(), |d| {
                        d.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tree.sel = Some(click_path.clone());
                                cx.notify();
                            }))
                    })
                    .child(icon(
                        if r.dir { "folder" } else { "file" },
                        14.,
                        hsla(if r.dir { t::ACCENT } else { t::MUTED }),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(if r.dir { strong() } else { text() })
                            .child(r.name.clone()),
                    )
                    .children(st.map(|(l, c)| div().text_color(c).child(l))),
            );
        }
        let mut col = div().flex().flex_col().gap(px(10.)).child(
            div()
                .id("tree-scroll")
                .max_h(px(240.))
                .overflow_y_scroll()
                .child(rows),
        );
        if d.rows.is_empty() {
            col = col.child(note("no files"));
        }
        if d.truncated {
            col = col.child(note("\u{2026} cut at the display limit"));
        }
        if let Some(sel) = &self.tree.sel {
            let lines = project::diff_lines(d, sel);
            col = col.child(
                card()
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(6.))
                            .border_b_1()
                            .border_color(hsla(t::RULE))
                            .text_color(strong())
                            .child(format!("git diff \u{b7} {sel}")),
                    )
                    .child(
                        div()
                            .id("git-diff")
                            .max_h(px(300.))
                            .overflow_scroll()
                            .px(px(12.))
                            .py(px(4.))
                            .children(lines.iter().map(|l| git_line(l))),
                    ),
            );
        }
        col
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_a_stopwatch() {
        assert_eq!(fmt_ms(340), "340 ms");
        assert_eq!(fmt_ms(1200), "1.2s");
        assert_eq!(fmt_ms(61_000), "1m01s");
    }

    #[test]
    fn verification_statuses_have_distinct_looks() {
        let marks: Vec<_> = ["passed", "failed", "blocked", "unverified"]
            .iter()
            .map(|s| status_look(s).0)
            .collect();
        assert_eq!(marks, ["\u{2713}", "\u{2717}", "!", "?"]);
    }
}
