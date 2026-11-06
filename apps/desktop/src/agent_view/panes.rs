//! The tabs next to the chat: the files the session changed (diff), the command history, the verification
//! reports, the browser calls and the project files. All read-only, all from `synthesis_view` state
//! (`AgentPanes`, the stored reports, `project::scan`); none is a terminal.

use gpui::{
    Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use serde_json::Value;
use synthesis_tui_render::theme as t;
use synthesis_view::agent::{CheckRow, CmdSource, CmdState, VerifyRun};
use synthesis_view::project;

use super::{AgentView, Tab, with_alpha};
use crate::theme::hsla;
use crate::tui_view::panels::{dim, dot, icon, strong, text};

/// `1.2s` / `340 ms`.
pub fn fmt_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.)
    } else {
        format!("{ms} ms")
    }
}

/// The colour and glyph of a verification status.
pub fn status_look(status: &str) -> (&'static str, Hsla) {
    match status {
        "passed" => ("\u{2713}", hsla(t::SUCCESS)),
        "failed" => ("\u{2717}", hsla(t::ERROR)),
        "blocked" => ("!", hsla(t::WARN)),
        _ => ("?", dim()),
    }
}

fn state_colour(s: CmdState) -> Hsla {
    match s {
        CmdState::Running => hsla(t::WARN),
        CmdState::Ok => hsla(t::SUCCESS),
        CmdState::Failed => hsla(t::ERROR),
        CmdState::NotRun => dim(),
    }
}

fn badge(label: String, c: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .rounded(px(4.))
        .border_1()
        .border_color(with_alpha(c, 0.5))
        .text_size(px(11.))
        .text_color(c)
        .child(label)
}

fn card() -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(8.))
        .overflow_hidden()
}

fn note(s: impl Into<String>) -> Div {
    div().text_color(dim()).child(s.into())
}

/// The diff lines of a `ToolCompleted.diff`, as the design's `.dl` grid: old no, new no, sign, text.
fn diff_grid(d: &Value) -> Div {
    let mut col = div().flex().flex_col().text_size(px(12.5));
    if d.get("hidden").and_then(Value::as_bool) == Some(true) {
        return col.child(
            div()
                .px(px(12.))
                .py(px(4.))
                .text_color(dim())
                .child("the diff of a secret file is not shown (only the counts)"),
        );
    }
    let no = |l: &Value, k: &str| {
        l.get(k)
            .and_then(Value::as_u64)
            .map_or(String::new(), |n| n.to_string())
    };
    for h in d
        .get("hunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let n = |k: &str| h.get(k).and_then(Value::as_u64).unwrap_or(0);
        col = col.child(
            div()
                .px(px(12.))
                .py(px(3.))
                .border_b_1()
                .border_color(hsla(t::RULE))
                .text_color(hsla(t::INFO))
                .child(format!(
                    "@@ -{},{} +{},{} @@",
                    n("old_start"),
                    n("old_lines"),
                    n("new_start"),
                    n("new_lines")
                )),
        );
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
                _ => (text(), None),
            };
            let cell = |s: String, w: f32| {
                div()
                    .w(px(w))
                    .flex_none()
                    .pr(px(8.))
                    .text_right()
                    .text_color(dim())
                    .child(s)
            };
            let row = div()
                .flex()
                .whitespace_nowrap()
                .child(cell(no(l, "old_no"), 34.))
                .child(cell(no(l, "new_no"), 34.))
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .text_center()
                        .text_color(fg)
                        .child(if tag == " " { "" } else { tag }.to_string()),
                )
                .child(div().text_color(text()).child(tx.to_string()));
            col = col.child(match bg {
                Some(b) => row.bg(b),
                None => row,
            });
        }
    }
    if d.get("truncated").and_then(Value::as_bool) == Some(true) {
        col = col.child(
            div()
                .px(px(12.))
                .text_color(dim())
                .child("\u{2026} diff cut"),
        );
    }
    col
}

/// One line of a unified diff from `git diff`, coloured by its first character.
fn git_line(l: &str) -> Div {
    let c = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") {
        strong()
    } else if l.starts_with('+') {
        hsla(t::SUCCESS)
    } else if l.starts_with('-') {
        hsla(t::ERROR)
    } else if l.starts_with("@@") {
        hsla(t::INFO)
    } else {
        text()
    };
    div()
        .whitespace_nowrap()
        .text_color(c)
        .child(if l.is_empty() { " " } else { l }.to_string())
}

impl AgentView {
    /// The body of a tab other than chat.
    pub(super) fn pane(&self, tab: Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let inner = match tab {
            Tab::Chat => div(),
            Tab::Diff => self.diff_pane(cx),
            Tab::Commands => self.commands_pane(),
            Tab::Verify => self.verify_pane(),
            Tab::Browser => self.browser_pane(),
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

    fn diff_pane(&self, cx: &mut Context<Self>) -> Div {
        let Some(s) = self.active_session() else {
            return div();
        };
        let p = &s.live.view.panes;
        if p.files.is_empty() {
            return note(
                "No file changed in this session yet. Edits show up here with their diff.",
            );
        }
        let (a, r) = p.totals();
        let mut col = div().flex().flex_col().gap(px(10.)).child(
            div()
                .flex()
                .gap(px(8.))
                .text_color(dim())
                .child(format!("{} files changed", p.files.len()))
                .child(div().text_color(hsla(t::SUCCESS)).child(format!("+{a}")))
                .child(
                    div()
                        .text_color(hsla(t::ERROR))
                        .child(format!("\u{2212}{r}")),
                ),
        );
        for (i, f) in p.files.iter().enumerate() {
            let open = self.diff_open.contains(&f.path);
            let path = f.path.clone();
            col = col.child(
                card()
                    .child(
                        div()
                            .id(("dfile", i))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(7.))
                            .bg(with_alpha(gpui::black(), 0.16))
                            .cursor_pointer()
                            .when(open, |d| d.border_b_1().border_color(hsla(t::RULE)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.diff_open.remove(&path) {
                                    this.diff_open.insert(path.clone());
                                }
                                cx.notify();
                            }))
                            .child(icon("file", 14., hsla(t::ACCENT)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(strong())
                                    .child(f.path.clone()),
                            )
                            .child(
                                div()
                                    .text_color(hsla(t::SUCCESS))
                                    .child(format!("+{}", f.added)),
                            )
                            .child(
                                div()
                                    .text_color(hsla(t::ERROR))
                                    .child(format!("\u{2212}{}", f.removed)),
                            )
                            .child(icon(
                                if open {
                                    "chevron-down"
                                } else {
                                    "chevron-right"
                                },
                                14.,
                                dim(),
                            )),
                    )
                    .when_some(f.diff.as_ref().filter(|_| open), |d, v| {
                        d.child(diff_grid(v))
                    }),
            );
        }
        col
    }

    fn commands_pane(&self) -> Div {
        let Some(s) = self.active_session() else {
            return div();
        };
        let cmds = &s.live.view.panes.commands;
        if cmds.is_empty() {
            return note(
                "No command ran yet. This is the read-only history of what the agent ran; it is not a terminal.",
            );
        }
        let mut col = div().flex().flex_col().gap(px(8.)).child(note(format!(
            "{} command{} \u{b7} newest first \u{b7} read-only",
            cmds.len(),
            if cmds.len() == 1 { "" } else { "s" }
        )));
        for c in cmds.iter().rev() {
            let who = match (c.source, &c.agent) {
                (CmdSource::User, _) => Some("you".to_string()),
                (CmdSource::Process, _) => Some("process".to_string()),
                (_, Some(a)) => Some(format!("@{a}")),
                _ => None,
            };
            let code = match (c.state, c.exit_code) {
                (CmdState::Running, _) => badge("running".into(), hsla(t::WARN)),
                (CmdState::NotRun, _) => badge("not run".into(), dim()),
                (s, Some(n)) => badge(format!("exit {n}"), state_colour(s)),
                (s, None) => badge("failed".into(), state_colour(s)),
            };
            col = col.child(
                card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(10.))
                            .py(px(6.))
                            .child(dot(state_colour(c.state), c.state != CmdState::Running))
                            .child(div().text_color(strong()).child("$"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(strong())
                                    .child(c.command.lines().next().unwrap_or("").to_string()),
                            )
                            .children(who.map(|w| badge(w, dim())))
                            .child(code)
                            .when(c.duration_ms > 0, |d| {
                                d.child(div().text_color(dim()).child(fmt_ms(c.duration_ms)))
                            }),
                    )
                    .when(!c.tail.is_empty(), |d| {
                        d.child(
                            div()
                                .px(px(12.))
                                .pb(px(6.))
                                .flex()
                                .flex_col()
                                .border_t_1()
                                .border_color(hsla(t::RULE))
                                .pt(px(4.))
                                .text_color(dim())
                                .children(c.tail.iter().map(|l| {
                                    div()
                                        .whitespace_nowrap()
                                        .child(if l.is_empty() { " " } else { l }.to_string())
                                })),
                        )
                    }),
            );
        }
        col
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

    fn browser_pane(&self) -> Div {
        let Some(s) = self.active_session() else {
            return div();
        };
        let b = &s.live.view.panes;
        let mut col = div().flex().flex_col().gap(px(10.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .h(px(30.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .rounded(px(6.))
                .child(icon("globe", 14., hsla(t::SUCCESS)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(if b.url.is_empty() { dim() } else { strong() })
                        .child(if b.url.is_empty() {
                            "no page opened yet".to_string()
                        } else {
                            b.url.clone()
                        }),
                ),
        );
        // only a PNG inside this session's artifact directory is opened
        let shot = s.live.engine.as_ref().and_then(|e| {
            let root = e.synthesis_home.join("sessions").join(&e.sid);
            b.screenshot
                .as_deref()
                .and_then(|p| synthesis_view::agent::viewable_screenshot(&root, p))
        });
        if let Some(p) = shot {
            col = col.child(
                card().child(
                    div().p(px(6.)).child(
                        gpui::img(p)
                            .max_h(px(260.))
                            .w_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
                ),
            );
        }
        if b.browser.is_empty() {
            return col.child(note(
                "No browser call yet. Calls of the browser tools list here; a screenshot shows when the model takes one.",
            ));
        }
        col.child(
            div()
                .flex()
                .flex_col()
                .children(b.browser.iter().rev().map(|c| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(dot(state_colour(c.state), c.state != CmdState::Running))
                        .child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .text_color(strong())
                                .child(c.name.trim_start_matches("browser_").to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(c.summary.clone()),
                        )
                        .when(c.duration_ms > 0, |d| {
                            d.child(div().text_color(dim()).child(fmt_ms(c.duration_ms)))
                        })
                })),
        )
        .child(note(
            "Screenshots come from the saved artifact of browser_screenshot; there is no live page stream.",
        ))
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
