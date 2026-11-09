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
/// the click handler of a file header and `jump(call_id)` the one of its "show in chat" button (the tool row
/// of the latest edit). Shared by the TUI view's diff tab and the Agent view's.
pub fn diff(
    live: &Live,
    open: &HashSet<String>,
    toggle: &dyn Fn(String) -> OnClick,
    jump: &dyn Fn(String) -> OnClick,
) -> Div {
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
            .child(format!(
                "{} file{} changed",
                p.files.len(),
                if p.files.len() == 1 { "" } else { "s" }
            ))
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
        let kind = f
            .diff
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(Value::as_str)
            .filter(|k| matches!(*k, "create" | "delete"))
            .map(|k| if k == "create" { "new" } else { "deleted" });
        col = col.child(
            card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(7.))
                        .bg(with_alpha(gpui::black(), 0.16))
                        .when(open, |d| d.border_b_1().border_color(hsla(t::RULE)))
                        .child(
                            div()
                                .id(("dfile", i))
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
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
                                .children(kind.map(|k| badge(k.into(), dim())))
                                .when(f.edits > 1, |d| {
                                    d.child(
                                        div().text_color(dim()).child(format!("{} edits", f.edits)),
                                    )
                                })
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
                        .when(!f.call_id.is_empty(), |d| {
                            d.child(
                                div()
                                    .id(("djump", i))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .px(px(6.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .text_color(dim())
                                    .hover(|d| {
                                        d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.08))
                                            .text_color(strong())
                                    })
                                    .on_click(jump(f.call_id.clone()))
                                    .child(icon("message-square", 13., dim()))
                                    .child("chat"),
                            )
                        }),
                )
                .when_some(f.diff.as_ref().filter(|_| open), |d, v| {
                    d.child(diff_grid(v))
                }),
        );
    }
    col
}

/// Lines of a command's output shown before "show more".
pub const CLAMP_LINES: usize = 5;

/// What the terminal tab keeps of the person's clicks: commands expanded to their whole tail, and background
/// processes the person already stopped (the harness learns of an exit only when asked again).
pub struct TermView<'a> {
    pub open: &'a HashSet<String>,
    pub killed: &'a HashSet<String>,
    pub toggle: &'a dyn Fn(String) -> OnClick,
    pub kill: &'a dyn Fn(String) -> OnClick,
}

/// A background process the harness runs for the agent and the person may stop: only a runtime id (`proc_N`);
/// a foreground command ends with the task (Esc).
pub fn killable(c: &synthesis_view::agent::CommandRun) -> bool {
    c.source == CmdSource::Process && c.state == CmdState::Running && c.id.starts_with("proc_")
}

/// The commands the agent ran, in a terminal's order (oldest first, the newest at the bottom), each as `$ command`
/// with the end of its output (5 lines, then "show more"), state, exit code and duration. A running background
/// process gets a Kill button. It is a view, not a terminal: nothing is typed here.
pub fn commands(live: &Live, v: &TermView) -> Div {
    let cmds = &live.view.panes.commands;
    if cmds.is_empty() {
        return note(
            "No command ran yet. This shows what the agent runs, in order; it is not a terminal.",
        );
    }
    let mut col = div().flex().flex_col().gap(px(8.)).child(note(format!(
        "{} command{} \u{b7} newest last \u{b7} read-only, not a terminal",
        cmds.len(),
        if cmds.len() == 1 { "" } else { "s" }
    )));
    for (i, c) in cmds.iter().enumerate() {
        let killed = v.killed.contains(&c.id);
        let who = match (c.source, &c.agent) {
            (CmdSource::User, _) => Some("you".to_string()),
            (CmdSource::Process, _) => Some("background".to_string()),
            (_, Some(a)) => Some(format!("@{a}")),
            _ => None,
        };
        let code = match (c.state, c.exit_code) {
            _ if killed => badge("stopped by you".into(), hsla(t::WARN)),
            (CmdState::Running, _) => badge("running".into(), hsla(t::WARN)),
            (CmdState::NotRun, _) => badge("not run".into(), dim()),
            (s, Some(n)) => badge(format!("exit {n}"), state_colour(s)),
            (s, None) => badge("failed".into(), state_colour(s)),
        };
        let open = v.open.contains(&c.id);
        let shown = if open || c.tail.len() <= CLAMP_LINES {
            &c.tail[..]
        } else {
            &c.tail[c.tail.len() - CLAMP_LINES..]
        };
        let hidden = c.tail.len().saturating_sub(CLAMP_LINES);
        col = col.child(
            card()
                .bg(with_alpha(gpui::black(), 0.22))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(6.))
                        .child(dot(
                            state_colour(c.state),
                            c.state != CmdState::Running || killed,
                        ))
                        .child(div().text_color(hsla(t::ACCENT)).child("$"))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(strong())
                                .child(synthesis_harness::sanitize_for_display(
                                    c.command.lines().next().unwrap_or(""),
                                )),
                        )
                        .children(who.map(|w| badge(w, dim())))
                        .child(code)
                        .when(c.duration_ms > 0, |d| {
                            d.child(div().text_color(dim()).child(fmt_ms(c.duration_ms)))
                        })
                        .when(killable(c) && !killed, |d| {
                            d.child(
                                div()
                                    .id(("kill", i))
                                    .flex_none()
                                    .px(px(8.))
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(hsla(t::ERROR))
                                    .text_size(px(11.))
                                    .text_color(hsla(t::ERROR))
                                    .cursor_pointer()
                                    .hover(|d| d.bg(with_alpha(hsla(t::ERROR), 0.15)))
                                    .on_click((v.kill)(c.id.clone()))
                                    .child("Kill"),
                            )
                        }),
                )
                .when(!shown.is_empty(), |d| {
                    d.child(
                        div()
                            .px(px(12.))
                            .pb(px(6.))
                            .flex()
                            .flex_col()
                            .border_t_1()
                            .border_color(hsla(t::RULE))
                            .pt(px(4.))
                            .text_color(text())
                            .children(shown.iter().map(|l| {
                                // output is untrusted: control characters are made visible, not sent to the text system
                                let l = synthesis_harness::sanitize_for_display(l);
                                div().whitespace_nowrap().child(if l.is_empty() {
                                    " ".to_string()
                                } else {
                                    l
                                })
                            }))
                            .when(hidden > 0, |d| {
                                d.child(
                                    div()
                                        .id(("more", i))
                                        .pt(px(3.))
                                        .cursor_pointer()
                                        .text_color(hsla(t::INFO))
                                        .on_click((v.toggle)(c.id.clone()))
                                        .child(if open {
                                            "show less".to_string()
                                        } else {
                                            format!(
                                                "\u{2026} {hidden} more line{} \u{b7} show",
                                                if hidden == 1 { "" } else { "s" }
                                            )
                                        }),
                                )
                            }),
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

/// The agent cursor stays fully visible this long after the last browser action, then fades out over
/// [`POINTER_FADE_MS`].
pub const POINTER_HOLD_MS: i64 = 3000;
pub const POINTER_FADE_MS: i64 = 2000;

/// Opacity of the agent cursor `age_ms` after the action (1 while held, 0 once faded).
pub fn pointer_alpha(age_ms: i64) -> f32 {
    if age_ms <= POINTER_HOLD_MS {
        1.0
    } else {
        (1.0 - (age_ms - POINTER_HOLD_MS) as f32 / POINTER_FADE_MS as f32).clamp(0.0, 1.0)
    }
}

/// Where the cursor goes on a screenshot of `shot_w` x `shot_h`, as fractions of the image: the action's point in
/// its viewport scaled over. `None` without a point, and when the image is not the viewport (a full-page shot has
/// another aspect ratio), since the point would land on the wrong place.
pub fn cursor_on(
    p: &synthesis_view::agent::Pointer,
    shot_w: u32,
    shot_h: u32,
) -> Option<(f32, f32)> {
    let (fx, fy) = p.frac()?;
    let (vw, vh) = (p.w? as f32, p.h? as f32);
    if shot_w == 0 || shot_h == 0 {
        return None;
    }
    let (a, b) = (vw / vh, shot_w as f32 / shot_h as f32);
    ((a - b).abs() / b <= 0.02).then_some((fx, fy))
}

/// The agent's cursor over a screenshot: a dot with a ring that pulses (still when `motion` is off), an arrow and
/// the action's name, drawn at `(fx, fy)` of its parent at opacity `alpha`.
fn cursor_overlay(fx: f32, fy: f32, alpha: f32, now: i64, motion: bool, label: &str) -> Div {
    let acc = hsla(t::ACCENT_LIGHT);
    let phase = if motion {
        (now.rem_euclid(1200)) as f32 / 1200.0
    } else {
        0.0
    };
    let ring = 18.0 + 22.0 * phase;
    let ring_a = if motion { 0.9 * (1.0 - phase) } else { 0.7 };
    div()
        .absolute()
        .left(gpui::relative(fx))
        .top(gpui::relative(fy))
        .size(px(0.))
        .child(
            div()
                .absolute()
                .ml(px(-ring / 2.))
                .mt(px(-ring / 2.))
                .size(px(ring))
                .rounded_full()
                .border_2()
                .border_color(with_alpha(acc, ring_a * alpha)),
        )
        .child(
            div()
                .absolute()
                .ml(px(-4.))
                .mt(px(-4.))
                .size(px(8.))
                .rounded_full()
                .bg(with_alpha(acc, alpha)),
        )
        .child(div().absolute().ml(px(2.)).mt(px(2.)).child(icon(
            "mouse-pointer",
            18.,
            with_alpha(acc, alpha),
        )))
        .child(
            div()
                .absolute()
                .ml(px(18.))
                .mt(px(14.))
                .px(px(6.))
                .whitespace_nowrap()
                .rounded(px(4.))
                .bg(with_alpha(gpui::black(), 0.7 * alpha))
                .text_size(px(11.))
                .text_color(with_alpha(acc, alpha))
                .child(label.to_string()),
        )
}

/// The browser tab: the page the agent is on (URL and title), the selected screenshot (the latest by default)
/// with the agent's cursor on it, the history of saved frames (`browser.frame`, newest first; `pick(i)` selects
/// the frame `i` of `AgentPanes::frames`) and the list of browser calls with their state and duration.
///
/// The screenshot is a still, NOT a live stream: it is the file `browser_screenshot` saved in the session's
/// artifact directory, opened only through `viewable_screenshot`. The cursor is where the latest
/// `browser.action` landed, scaled onto that image; it appears when the action is recent, fades after
/// [`POINTER_HOLD_MS`] + [`POINTER_FADE_MS`], and can lag or differ from the page, which may have changed since.
/// `now` is epoch ms; `motion` false keeps the ring still.
pub fn browser(
    live: &Live,
    sel: Option<usize>,
    tz_min: i32,
    home: Option<&std::path::Path>,
    pick: &dyn Fn(usize) -> OnClick,
    now: i64,
    motion: bool,
) -> Div {
    let b = &live.view.panes;
    // the page the agent navigated to, else the page of the latest saved frame
    let page = if b.url.is_empty() {
        b.frames.last().map_or(String::new(), |f| f.url.clone())
    } else {
        b.url.clone()
    };
    let pointer = b
        .pointer
        .as_ref()
        .and_then(|p| Some((p, pointer_alpha(now - p.ts?))))
        .filter(|(_, a)| *a > 0.0);
    let driving = pointer.is_some() || b.browser.iter().any(|c| c.state == CmdState::Running);
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
            )
            .when(driving, |d| {
                d.child(badge("agent controlling".into(), hsla(t::ACCENT_LIGHT)))
            }),
    );
    if !b.title.is_empty() {
        col = col.child(
            div()
                .truncate()
                .text_color(dim())
                .child(synthesis_harness::sanitize_for_display(&b.title)),
        );
    }
    // the session id outlives the engine (`Live::sid`): the image must not vanish while the engine restarts
    let root = home.map(|h| h.join("sessions").join(&live.sid));
    let shown = sel
        .filter(|i| *i < b.frames.len())
        .or(b.frames.len().checked_sub(1));
    let frame = shown.and_then(|i| b.frames.get(i));
    let frame_path = frame.map(|f| f.path.as_str()).or(b.screenshot.as_deref());
    let mut hint = None;
    let stage = match frame_path {
        None => note("No screenshot saved yet."),
        Some(p) => match viewable(root.as_deref(), p) {
            Ok(img) => {
                // the stage has the screenshot's own aspect ratio, so a fraction of it is a fraction of the
                // image; capped at 420 px of height
                let (fw, fh) = frame.map_or((0, 0), |f| (f.w, f.h));
                let ratio = if fw > 0 && fh > 0 {
                    fw as f32 / fh as f32
                } else {
                    1.6
                };
                let mut st = div()
                    .relative()
                    .w_full()
                    .max_w(px(420. * ratio))
                    .aspect_ratio(ratio)
                    .child(
                        gpui::img(img)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                if let Some((p, alpha)) = pointer {
                    match cursor_on(p, fw, fh) {
                        Some((fx, fy)) => {
                            st = st.child(cursor_overlay(fx, fy, alpha, now, motion, &p.action));
                        }
                        None if p.x.is_some() => {
                            hint = Some(
                                "The agent cursor is hidden: this screenshot is not the size of the viewport.",
                            );
                        }
                        None => {}
                    }
                }
                st
            }
            Err(why) => note(format!("Screenshot not shown: {why}")),
        },
    };
    col = col.child(card().child(div().p(px(6.)).child(stage)));
    col = col.child(note(format!(
        "A still image from the last browser_screenshot, not a live view. The cursor marks where the agent last acted and can lag the page.{}",
        hint.map_or(String::new(), |h| format!(" {h}"))
    )));
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

    fn ptr(x: i32, y: i32, w: u32, h: u32) -> synthesis_view::agent::Pointer {
        synthesis_view::agent::Pointer {
            action: "click".into(),
            x: Some(x),
            y: Some(y),
            w: Some(w),
            h: Some(h),
            target: None,
            ts: Some(0),
        }
    }

    #[test]
    fn the_cursor_scales_onto_a_screenshot_of_the_viewport_only() {
        // the same aspect at twice the pixels (a 2x display): same fractions
        assert_eq!(
            cursor_on(&ptr(640, 200, 1280, 800), 2560, 1600),
            Some((0.5, 0.25))
        );
        assert_eq!(
            cursor_on(&ptr(0, 800, 1280, 800), 1280, 800),
            Some((0.0, 1.0))
        );
        // a full-page shot (another aspect) or an unreadable one: no cursor rather than a wrong place
        assert_eq!(cursor_on(&ptr(640, 200, 1280, 800), 1280, 4000), None);
        assert_eq!(cursor_on(&ptr(640, 200, 1280, 800), 0, 0), None);
    }

    #[test]
    fn the_cursor_holds_then_fades_out() {
        assert_eq!(pointer_alpha(0), 1.0);
        assert_eq!(pointer_alpha(POINTER_HOLD_MS), 1.0);
        let mid = pointer_alpha(POINTER_HOLD_MS + POINTER_FADE_MS / 2);
        assert!((mid - 0.5).abs() < 1e-6);
        assert_eq!(pointer_alpha(POINTER_HOLD_MS + POINTER_FADE_MS), 0.0);
        assert_eq!(pointer_alpha(1_000_000), 0.0);
    }

    #[test]
    fn a_path_inside_the_folder_is_shown_relative() {
        assert_eq!(rel("/w/proj", "/w/proj/src/a.rs"), "src/a.rs");
        assert_eq!(rel("/w/proj/", "/w/proj/a.rs"), "a.rs");
        assert_eq!(rel("/w/proj", "/w/other/a.rs"), "/w/other/a.rs");
        assert_eq!(rel("/w/proj", "/w/project/a.rs"), "/w/project/a.rs");
    }
}
