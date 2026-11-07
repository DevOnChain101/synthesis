//! Read-only panes the TUI view shows under its tabs: the diff of the files the session changed, the command
//! history (the "terminal" tab: it is not a terminal) and the browser calls. They read `Live`
//! (`synthesis_view::agent::AgentPanes`) and own no state; the caller keeps what is open or selected.

use std::collections::HashSet;

use gpui::{
    Div, Hsla, InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::*, px,
};
use serde_json::Value;
use synthesis_tui_render::theme as t;
use synthesis_view::agent::{CmdSource, CmdState};

use crate::live::Live;
use crate::theme::hsla;
use crate::widgets::chrome::{OnClick, dim, dot, icon, strong, text, with_alpha};

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

pub fn state_colour(s: CmdState) -> Hsla {
    match s {
        CmdState::Running => hsla(t::WARN),
        CmdState::Ok => hsla(t::SUCCESS),
        CmdState::Failed => hsla(t::ERROR),
        CmdState::NotRun => dim(),
    }
}

pub fn badge(label: String, c: Hsla) -> Div {
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

pub fn card() -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(8.))
        .overflow_hidden()
}

pub fn note(s: impl Into<String>) -> Div {
    div().text_color(dim()).child(s.into())
}

/// The diff lines of a `ToolCompleted.diff`, as the design's `.dl` grid: old no, new no, sign, text.
pub fn diff_grid(d: &Value) -> Div {
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
                .child(div().text_color(text()).child(super::syntax::styled(tx)));
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
pub fn git_line(l: &str) -> Div {
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

/// `path` relative to the session's folder when it lies inside it.
pub fn rel(cwd: &str, path: &str) -> String {
    let dir = cwd.trim_end_matches(['/', '\\']);
    path.strip_prefix(dir)
        .and_then(|r| r.strip_prefix(['/', '\\']))
        .unwrap_or(path)
        .to_string()
}

/// The files the session changed, each with its diff when open (`open` holds the paths); `toggle(path)` is
/// the click handler of a file header. Shared by the TUI view's diff tab and the Agent view's.
pub fn diff(live: &Live, open: &HashSet<String>, toggle: &dyn Fn(String) -> OnClick) -> Div {
    let p = &live.view.panes;
    if p.files.is_empty() {
        return note("No file changed in this session yet. Edits show up here with their diff.");
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
        let open = open.contains(&f.path);
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
                        .on_click(toggle(path))
                        .child(icon("file", 14., hsla(t::ACCENT)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(strong())
                                .child(rel(&live.cwd, &f.path)),
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

/// The read-only history of the commands the agent ran (newest first); it is not a terminal.
pub fn commands(live: &Live) -> Div {
    let cmds = &live.view.panes.commands;
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

/// The screenshot to draw, or why not (shown in place of the image).
fn viewable(root: Option<&std::path::Path>, path: &str) -> Result<std::path::PathBuf, String> {
    let root = root.ok_or("no session folder")?;
    if let Some(p) = synthesis_view::agent::viewable_screenshot(root, path) {
        return Ok(p);
    }
    Err(match std::fs::metadata(path) {
        Err(_) => "the file is missing".into(),
        Ok(m) if m.len() > synthesis_view::agent::MAX_SHOT_BYTES => {
            format!(
                "the file is larger than {} MiB",
                synthesis_view::agent::MAX_SHOT_BYTES >> 20
            )
        }
        Ok(_) => "the file is not a PNG inside the session's artifact folder".into(),
    })
}

/// The browser tab: the page the agent is on, the selected screenshot (the latest by default), the history of
/// saved frames (`browser.frame`, newest first; `pick(i)` selects the frame `i` of `AgentPanes::frames`) and the
/// list of browser calls with their state and duration. There is no live page stream: screenshots are the files
/// `browser_screenshot` saved in the session's artifact directory, opened only through `viewable_screenshot`.
pub fn browser(
    live: &Live,
    sel: Option<usize>,
    tz_min: i32,
    home: Option<&std::path::Path>,
    pick: &dyn Fn(usize) -> OnClick,
) -> Div {
    let b = &live.view.panes;
    // the page the agent navigated to, else the page of the latest saved frame
    let page = if b.url.is_empty() {
        b.frames.last().map_or(String::new(), |f| f.url.clone())
    } else {
        b.url.clone()
    };
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
                    .text_color(if page.is_empty() { dim() } else { strong() })
                    .child(if page.is_empty() {
                        "no page opened yet".to_string()
                    } else {
                        page.clone()
                    }),
            ),
    );
    // the session id outlives the engine (`Live::sid`): the image must not vanish while the engine restarts
    let root = home.map(|h| h.join("sessions").join(&live.sid));
    let shown = sel
        .filter(|i| *i < b.frames.len())
        .or(b.frames.len().checked_sub(1));
    let frame_path = shown
        .and_then(|i| b.frames.get(i))
        .map(|f| f.path.as_str())
        .or(b.screenshot.as_deref());
    col = col.child(
        card().child(
            div().p(px(6.)).child(match frame_path {
                None => note("No screenshot saved yet."),
                Some(p) => match viewable(root.as_deref(), p) {
                    // fit inside the panel width and 300 px of height, aspect ratio kept
                    Ok(p) => div().h(px(300.)).child(
                        gpui::img(p)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
                    Err(why) => note(format!("Screenshot not shown: {why}")),
                },
            }),
        ),
    );
    if !b.frames.is_empty() {
        col = col.child(
            div()
                .flex()
                .flex_col()
                .child(div().mb(px(4.)).text_color(dim()).child(format!(
                    "{} saved frame{}",
                    b.frames.len(),
                    if b.frames.len() == 1 { "" } else { "s" }
                )))
                .children(b.frames.iter().enumerate().rev().map(|(i, f)| {
                    let on = shown == Some(i);
                    div()
                        .id(("frame", i))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(8.))
                        .h(px(21.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(hsla(t::ELEVATED)).text_color(strong()))
                        .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.05)))
                        .on_click(pick(i))
                        .child(div().w(px(64.)).flex_none().text_color(dim()).child(
                            f.ts.map_or(String::new(), |ms| {
                                crate::tui_view::snapshot::clock(ms, tz_min)
                            }),
                        ))
                        .child(div().flex_1().min_w(px(0.)).truncate().child(f.url.clone()))
                        .child(div().flex_none().text_color(dim()).child(format!(
                            "{}x{} \u{b7} {} KB",
                            f.w,
                            f.h,
                            f.bytes.div_ceil(1024)
                        )))
                })),
        );
    }
    if b.browser.is_empty() {
        return col.child(note(
            "No browser call yet. The agent's browser tools list here; a screenshot shows when it takes one.",
        ));
    }
    col.child(
        div()
            .flex()
            .flex_col()
            .child(div().mb(px(4.)).text_color(dim()).child(format!(
                "{} call{}",
                b.browser.len(),
                if b.browser.len() == 1 { "" } else { "s" }
            )))
            .children(b.browser.iter().rev().map(|c| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(21.))
                    .child(dot(state_colour(c.state), c.state != CmdState::Running))
                    .child(
                        div()
                            .w(px(130.))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_inside_the_folder_is_shown_relative() {
        assert_eq!(rel("/w/proj", "/w/proj/src/a.rs"), "src/a.rs");
        assert_eq!(rel("/w/proj/", "/w/proj/a.rs"), "a.rs");
        assert_eq!(rel("/w/proj", "/w/other/a.rs"), "/w/other/a.rs");
        assert_eq!(rel("/w/proj", "/w/project/a.rs"), "/w/project/a.rs");
    }
}
