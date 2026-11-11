//! The panel bodies of the TUI view, 1:1 with `cli.html` (`.rows`, `.ws-item`, `.run-item`, `.kv`, `.chk`,
//! `.bars`): Workspace, Running, Session, Tools & Skills, Recent Activity (prompts and answers), Agent Status, Context
//! and Usage & Metrics. The panel chrome itself is `widgets::chrome`.

use gpui::{
    Div, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;

use super::activity::{Presence, RunState, presence};
use super::layout::{MONTH_GAP, MonthMode, YEAR_LABEL_W, month_h, year_fit};
use super::sample::{
    ActKind, ActivityRow, AgentStatus, ContextInfo, DayDot, MonthUse, Running, SessionInfo,
    StepKind, Tone, ToolsInfo, Usage, Workspace,
};
use crate::theme::hsla;
pub use crate::widgets::chrome::{OnClick, dim, dot, icon, strong, text, with_alpha};

/// Height of one Workspace row (`li` 1px padding + a two-line `.ws-item`: 2 + 2 + 16.25 + 14.95 + 2).
pub const WS_ROW_H: f32 = 37.19;
/// Height of one Running row (`li` padding + `.run-item`: 10 + 2 + 19.375 + 2 + 17.825 + 2).
pub const RUN_ROW_H: f32 = 53.19;

/// A `.rows li` with the design's 1px vertical padding.
fn li() -> Div {
    div().py(px(1.))
}

/// The `.ws-item`/`.run-item` box: 1px transparent border, radius 5; selected: elevated, a rule border and
/// the accent bar on the left edge (gpui has no per-side border colour, so the bar is a child).
/// The pointer-hover fill of a clickable row (the picker options use `ELEVATED`; rows on a panel stay lighter).
fn row_hover() -> gpui::Hsla {
    with_alpha(gpui::white(), 0.05)
}

fn item_box(selected: bool, pad: (f32, f32, f32, f32)) -> Div {
    let d = div()
        .relative()
        .flex()
        .w_full()
        .gap(px(8.))
        .pt(px(pad.0))
        .pr(px(pad.1))
        .pb(px(pad.2))
        .pl(px(pad.3))
        .rounded(px(5.))
        .border_1()
        .border_color(gpui::transparent_black());
    if selected {
        d.bg(hsla(t::ELEVATED))
            .border_color(hsla(t::BORDER))
            .text_color(strong())
            .child(
                div()
                    .absolute()
                    .left(px(-1.))
                    .top(px(-1.))
                    .bottom(px(-1.))
                    .w(px(3.))
                    .rounded_l(px(5.))
                    .bg(hsla(t::ACCENT)),
            )
    } else {
        d.text_color(text())
    }
}

pub fn workspace_body(
    ws: &[Workspace],
    attn: Option<gpui::Hsla>,
    on_click: Option<&dyn Fn(usize) -> OnClick>,
    on_close: Option<&dyn Fn(usize) -> OnClick>,
    on_new: Option<OnClick>,
    new_selected: bool,
) -> Div {
    let mut col = div().flex().flex_col();
    for (i, w) in ws.iter().enumerate() {
        col = col.child(
            li().child(
                item_box(w.selected, (1., 8., 1., 8.))
                    .when_some(attn.filter(|_| w.selected), |d, c| d.border_color(c))
                    .id(("ws-row", i))
                    .items_center()
                    .when_some(on_click, |d, f| {
                        d.cursor_pointer()
                            .when(!w.selected, |d| d.hover(|d| d.bg(row_hover())))
                            .on_click(f(i))
                    })
                    .child(icon(
                        "folder",
                        14.,
                        if w.selected { strong() } else { text() },
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.))
                            .when(w.closable && on_close.is_some(), |d| d.flex_1())
                            .child(
                                div()
                                    .truncate()
                                    .line_height(px(16.25))
                                    .child(w.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .line_height(px(14.95))
                                    .text_size(px(11.5))
                                    .text_color(if w.selected { text() } else { dim() })
                                    .child(w.path.clone()),
                            ),
                    )
                    // the open folder has none; the others close all their sessions (after a confirmation)
                    .when_some(on_close.filter(|_| w.closable), |d, f| {
                        d.child(
                            div()
                                .id(("ws-close", i))
                                .flex_none()
                                .cursor_pointer()
                                .rounded(px(4.))
                                .p(px(2.))
                                .hover(|h| h.bg(hsla(t::BORDER)))
                                .on_click(f(i))
                                .child(icon("close", 12., dim())),
                        )
                    }),
            ),
        );
    }
    col.child(
        li().child(
            item_box(new_selected, (1., 8., 1., 8.))
                .id("ws-new")
                .items_center()
                .text_color(dim())
                .when_some(on_new, |d, f| {
                    d.cursor_pointer()
                        .hover(|d| d.text_color(strong()))
                        .on_click(f)
                })
                .child(icon("plus", 14., dim()))
                .child("new workspace..."),
        ),
    )
}

/// The mark at the left of a Running row: a spinner while the session works (a steady glyph without motion), a
/// yellow "!" that blinks while it waits for approval, a red dot after a failure, else the design's dot.
fn run_mark(r: &Running, attn: Option<gpui::Hsla>, motion: bool, now: i64) -> Div {
    let glyph = |c: gpui::Hsla, s: &'static str| {
        div()
            .w(px(7.))
            .mt(px(1.))
            .flex_none()
            .text_color(c)
            .child(s)
    };
    match r.state {
        RunState::Running => glyph(
            hsla(t::SUCCESS),
            if motion {
                super::chat::spinner_glyph(now)
            } else {
                "*"
            },
        ),
        RunState::Approval => glyph(
            crate::widgets::chrome::blink(hsla(t::WARN), motion, now),
            "!",
        ),
        RunState::Error => dot(hsla(t::ERROR), true).mt(px(5.)),
        RunState::Idle => match (attn.filter(|_| r.selected), presence(now, r.last)) {
            (Some(c), _) => dot(c, true),
            (None, Presence::Active) => dot(hsla(t::SUCCESS), true),
            // a ring: green border, transparent inside
            (None, Presence::Recent) => dot(hsla(t::SUCCESS), false),
            (None, Presence::Inactive) => dot(dim(), true),
        }
        .mt(px(5.)),
    }
}

pub fn running_body(
    rs: &[Running],
    attn: Option<gpui::Hsla>,
    motion: bool,
    now: i64,
    on_click: Option<&dyn Fn(usize) -> OnClick>,
    on_new: Option<OnClick>,
    new_selected: bool,
) -> Div {
    let waiting = crate::widgets::chrome::blink(hsla(t::WARN), motion, now);
    let mut col = div().flex().flex_col();
    for (i, r) in rs.iter().enumerate() {
        // inactive for over an hour: gray text as well as the gray dot
        let stale = r.state == RunState::Idle && presence(now, r.last) == Presence::Inactive;
        col = col.child(
            li().child(
                item_box(r.selected, (5., 7., 5., 6.))
                    .when_some(attn.filter(|_| r.selected), |d, c| d.border_color(c))
                    .id(("run-row", i))
                    .when_some(on_click, |d, f| {
                        d.cursor_pointer()
                            .when(!r.selected, |d| d.hover(|d| d.bg(row_hover())))
                            .on_click(f(i))
                    })
                    .child(run_mark(r, attn, motion, now))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .min_w(px(0.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_color(if stale { dim() } else { strong() })
                                            .child(r.title.clone()),
                                    )
                                    .child(
                                        div().flex_none().text_color(dim()).child(r.age.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .min_w(px(0.))
                                    .text_size(px(11.5))
                                    .text_color(if r.selected { text() } else { dim() })
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(if stale { dim() } else { hsla(t::INFO) })
                                            .child(r.folder.clone()),
                                    )
                                    // waiting for approval: the yellow status word (blinking unless motion
                                    // is off) takes the place of the branch
                                    .when(r.state == RunState::Approval, |d| {
                                        d.child(div().text_color(waiting).child(r.status.clone()))
                                    })
                                    .when(
                                        !r.branch.is_empty() && r.state != RunState::Approval,
                                        |d| {
                                            d.child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(3.))
                                                    .min_w(px(0.))
                                                    .overflow_hidden()
                                                    .child(icon("git-branch", 12., dim()))
                                                    .child(
                                                        div().truncate().child(r.branch.clone()),
                                                    ),
                                            )
                                        },
                                    ),
                            ),
                    ),
            ),
        );
    }
    col.child(
        li().child(
            div()
                .id("run-new")
                .relative()
                .flex()
                .w_full()
                .items_center()
                .gap(px(7.))
                .px(px(4.))
                .py(px(1.))
                .border_1()
                .border_color(gpui::transparent_black())
                .rounded(px(4.))
                .text_color(dim())
                // selected with the keys: lit like a session row (the accent bar is a child, as in `item_box`)
                .when(new_selected, |d| {
                    d.bg(hsla(t::ELEVATED))
                        .border_color(hsla(t::BORDER))
                        .text_color(strong())
                        .child(
                            div()
                                .absolute()
                                .left(px(-1.))
                                .top(px(-1.))
                                .bottom(px(-1.))
                                .w(px(3.))
                                .rounded_l(px(5.))
                                .bg(hsla(t::ACCENT)),
                        )
                })
                .when_some(on_new, |d, f| {
                    d.cursor_pointer()
                        .hover(|d| d.text_color(strong()))
                        .on_click(f)
                })
                .child(icon("plus", 14., dim()))
                .child("new session..."),
        ),
    )
}

/// The colour of a Session / Context value by what it means: names, numbers, paths; "n/a", "?" and "-" are dim.
fn value_colour(key: &str, v: &str) -> gpui::Hsla {
    use crate::theme::value;
    if matches!(v, "n/a" | "?" | "-" | "no model") {
        return dim();
    }
    hsla(match key {
        "mode" => value::MODE,
        "model" => value::MODEL,
        "cwd" | "worktree" | "branch" => value::PATH,
        _ => value::NUMBER,
    })
}

/// `.kv`: dim label column, value coloured by meaning; rows `row_gap` apart.
pub fn kv(rows: &[(String, String)], label_w: f32, col_gap: f32, row_gap: f32) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(row_gap))
        .children(rows.iter().map(|(k, v)| {
            div()
                .flex()
                .gap(px(col_gap))
                .child(
                    div()
                        .w(px(label_w))
                        .flex_none()
                        .text_color(dim())
                        .child(k.clone()),
                )
                .child(
                    // one line: a path keeps its tail ("…/folders/ws"), any other value its head; the full value is the tooltip
                    div()
                        .id(gpui::SharedString::from(format!("kv-{k}")))
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .when(matches!(k.as_str(), "cwd" | "worktree"), |d| {
                            d.text_ellipsis_start()
                        })
                        .when(!matches!(k.as_str(), "cwd" | "worktree"), |d| {
                            d.text_ellipsis()
                        })
                        .text_color(value_colour(k, v))
                        .child(v.clone())
                        .tooltip({
                            let v = v.clone();
                            move |_, cx| cx.new(|_| crate::ui::Tip(v.clone())).into()
                        }),
                )
        }))
}

pub fn session_meta(s: &SessionInfo) -> gpui::AnyElement {
    let c = hsla(crate::theme::status_colour(&s.status));
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_color(c)
        .child(dot(c, true))
        .child(s.status.clone())
        .into_any_element()
}

pub fn session_body(s: &SessionInfo) -> Div {
    kv(&s.rows, 62., 12., 3.)
}

pub fn tools_body(k: &ToolsInfo) -> Div {
    div().flex().flex_col().children(k.rows.iter().map(|r| {
        li().flex()
            .items_center()
            .gap(px(8.))
            .child(dot(hsla(t::SUCCESS), true))
            .child(div().child(r.name.clone()))
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .text_color(if r.plain { dim() } else { hsla(t::SUCCESS) })
                    .child(r.value.clone()),
            )
    }))
}

/// `.circ`: the 11px ring of a list row or a checklist step.
fn circ(kind: Option<StepKind>) -> Div {
    let d = div().size(px(11.)).flex_none().rounded_full().border_1();
    match kind {
        Some(StepKind::Done) => d
            .border_color(hsla(t::SUCCESS))
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(px(6.)).rounded_full().bg(hsla(t::SUCCESS))),
        Some(StepKind::Now) => d.border_color(hsla(t::ACCENT)),
        _ => d.border_color(dim()),
    }
}

/// Recent Activity: `time  mark  text` per entry, one line each. Prompts are strong text behind a blue mark, the
/// agent's answers plain text behind an accent mark; an entry the chat no longer shows is dimmed and not clickable.
pub fn activity_body(rows: &[ActivityRow], on_click: Option<&dyn Fn(usize) -> OnClick>) -> Div {
    if rows.is_empty() {
        return div().text_color(dim()).child("No activity yet");
    }
    div()
        .flex()
        .flex_col()
        .children(rows.iter().enumerate().map(|(i, a)| {
            let (mark, fg) = match a.kind {
                ActKind::Prompt => (hsla(t::INFO), strong()),
                ActKind::Reply => (hsla(t::ACCENT), text()),
            };
            let fg = if a.shown { fg } else { dim() };
            li().child(
                div()
                    .id(("act-row", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(3.))
                    .text_color(fg)
                    .when(a.shown, |d| {
                        d.when_some(on_click, |d, f| {
                            d.cursor_pointer()
                                .hover(|d| d.bg(with_alpha(gpui::white(), 0.05)))
                                .on_click(f(i))
                        })
                    })
                    .child(div().flex_none().text_color(dim()).child(a.time.clone()))
                    .child(dot(
                        if a.shown { mark } else { dim() },
                        a.kind == ActKind::Prompt,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(a.text.clone()),
                    ),
            )
        }))
}

pub fn agent_body(a: &AgentStatus, steps_shown: usize) -> Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .mb(px(4.))
                .pb(px(3.))
                .border_b_1()
                .border_color(hsla(t::RULE))
                .child(
                    div()
                        .truncate()
                        .text_color(strong())
                        .child(a.working.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_color(dim())
                        .child(format!("next: {}", a.next)),
                ),
        )
        .children(a.steps.iter().take(steps_shown).map(|s| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(circ(Some(s.kind)))
                .child(
                    div()
                        .flex()
                        .gap(px(7.5))
                        .min_w(px(0.))
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(if s.kind == StepKind::Todo {
                                    dim()
                                } else {
                                    strong()
                                })
                                .child(s.text.clone()),
                        )
                        .when(!s.note.is_empty(), |d| {
                            d.child(div().flex_none().text_color(dim()).child(s.note.clone()))
                        }),
                )
        }))
}

/// The 5px progress bar (`.bar`): a faint track and an accent fill.
pub fn bar(pct: f32) -> Div {
    div()
        .h(px(5.))
        .rounded(px(3.))
        .overflow_hidden()
        .bg(with_alpha(gpui::white(), 0.08))
        .child(
            div()
                .h_full()
                .w(gpui::relative(pct.clamp(0., 100.) / 100.))
                .rounded(px(3.))
                .bg(hsla(t::ACCENT)),
        )
}

/// `n` rows of the Context kv list beyond the tokens line are dropped when the row is tight (`r-tight`).
pub fn context_body(c: &ContextInfo, rows_shown: usize) -> Div {
    let rows: Vec<(String, String)> = [
        ("model", c.model.clone()),
        ("turns", c.turns.clone()),
        ("input", c.input.clone()),
        ("files", c.files.clone()),
        ("output", c.output.clone()),
        ("cache", format!("{} ({} hit)", c.cache, c.hit)),
    ]
    .into_iter()
    .take(rows_shown)
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .gap(px(10.))
                .child(
                    div()
                        .w(px(48.))
                        .flex_none()
                        .text_color(dim())
                        .child("tokens"),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(if c.max == "?" {
                            dim()
                        } else {
                            hsla(crate::theme::value::NUMBER)
                        })
                        .child(format!("{} / {}", c.used, c.max)),
                )
                .child(
                    div()
                        .ml_auto()
                        .flex_none()
                        .text_color(if c.max == "?" {
                            dim()
                        } else {
                            hsla(crate::theme::ctx_level(c.pct))
                        })
                        .child(format!("{:.0}%", c.pct)),
                ),
        )
        .child(div().my(px(4.)).mb(px(6.)).child(bar(c.pct)))
        .child(kv(&rows, 62., 10., 1.))
}

/// The period select of Usage & Metrics: a click toggles 7 and 30 days.
pub fn usage_meta(u: &Usage, on_click: Option<OnClick>) -> gpui::AnyElement {
    div()
        .id("usage-range")
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(8.))
        .py(px(1.))
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(5.))
        .bg(hsla(t::SURFACE))
        .text_color(strong())
        .when_some(on_click, |d, f| {
            d.cursor_pointer()
                .hover(|d| d.border_color(hsla(t::ACCENT)))
                .on_click(f)
        })
        .child(u.range.clone())
        .child(icon("chevron-down", 14., strong()))
        .into_any_element()
}

pub fn usage_body(u: &Usage) -> Div {
    div().flex().flex_col().children(u.rows.iter().map(|r| {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .py(px(3.))
            .child(
                div()
                    .w(px(118.))
                    .flex_none()
                    .whitespace_nowrap()
                    .child(r.label.clone()),
            )
            .child(div().flex_1().min_w(px(0.)).child(bar(r.pct)))
            .child(
                div()
                    .w(px(92.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .text_color(strong())
                    .child(r.value.clone()),
            )
    }))
}

/// Width of one character of the panel text (12.5px monospace), for fitting the CLI's titles.
const CH: f32 = 7.5;
/// Pitch of a day cell of the month grid and its dot.
const CELL: f32 = 12.;
const DOT: f32 = 8.;

fn heat(level: u8) -> gpui::Hsla {
    match level {
        0 => with_alpha(gpui::white(), 0.16),
        l => hsla(t::HEAT[usize::from(l).min(4)]),
    }
}

fn day_dot(d: Option<DayDot>) -> Div {
    let cell = div()
        .size(px(DOT))
        .flex_none()
        .mr(px(CELL - DOT))
        .flex()
        .items_center()
        .justify_center();
    match d {
        None => cell,
        Some(d) if d.future => cell.child(
            div()
                .size(px(3.))
                .rounded_full()
                .bg(with_alpha(gpui::white(), 0.18)),
        ),
        Some(d) => cell.child(div().size(px(DOT)).rounded(px(2.)).bg(heat(d.level))),
    }
}

/// The CLI's `Less ... More` legend (`dash.rs` `legend`).
fn legend() -> Div {
    div()
        .h(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.))
        .text_size(px(11.))
        .text_color(dim())
        .child("Less")
        .children((0..5).map(|l| div().size(px(DOT)).rounded(px(2.)).bg(heat(l))))
        .child("More")
}

/// The CLI's Usage month section, added under the metric rows (see `layout::month_mode`): the title that fits
/// `body_w` (the CLI's fallback list), then the calendar grid (weeks as columns, M / W / F row labels) with the
/// summary rows beside it when there is room, or one strip of the days so far; the legend under both. Its height is
/// exactly `layout::month_h(mode)`.
pub fn month_section(m: &MonthUse, mode: MonthMode, body_w: f32) -> Div {
    let mut d = div()
        .debug_selector(|| "usage-month".to_string())
        .h(px(month_h(mode, m, body_w)))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.))
        .mt(px(MONTH_GAP))
        .pt(px(5.))
        .border_t_1()
        .border_color(hsla(t::RULE))
        .overflow_hidden();
    if let Some(n) = &m.note {
        return d.child(div().text_color(dim()).child(n.clone()));
    }
    let year = mode != MonthMode::Strip && !m.year.is_empty();
    let titles = if year { &m.year_titles } else { &m.titles };
    let title = titles
        .iter()
        .find(|t| t.chars().count() as f32 * CH <= body_w)
        .unwrap_or(&titles[2]);
    d = d.child(
        div()
            .h(px(16.))
            .truncate()
            .text_color(dim())
            .child(title.clone()),
    );
    if !year {
        let n = ((body_w / CELL) as usize).max(1);
        let all = m.strip();
        let from = all.len().saturating_sub(n);
        return d
            .child(div().flex().children(all[from..].iter().map(|l| {
                day_dot(Some(DayDot {
                    level: *l,
                    future: false,
                }))
            })))
            .child(legend());
    }
    // the CLI's year grid: weeks as columns sized to the width (the oldest weeks drop when it is too narrow),
    // month labels above, M / W / F at the left
    let (weeks, pitch) = year_fit(body_w);
    let skip = m.year.len() - weeks;
    let dot = pitch - if pitch >= 9. { 2. } else { 1. };
    let mut last_x = f32::MIN;
    let marks = m.year_marks.iter().filter_map(|(c, name)| {
        let x = YEAR_LABEL_W + c.checked_sub(skip)? as f32 * pitch;
        // a label is three 10 px letters (about 18 px) wide; never over the previous one or past the edge
        (x - last_x >= 20. && x + 18. <= body_w).then(|| {
            last_x = x;
            div().absolute().left(px(x)).child(*name)
        })
    });
    d = d
        .child(
            div()
                .relative()
                .h(px(12.))
                .flex_none()
                .text_size(px(10.))
                .line_height(px(12.))
                .text_color(dim())
                .whitespace_nowrap()
                .children(marks),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .mt(px(-2.))
                .children((0..7).map(|r| {
                    div()
                        .h(px(pitch))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .w(px(YEAR_LABEL_W))
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(dim())
                                .child(["", "M", "", "W", "", "F", ""][r]),
                        )
                        .children(m.year[skip..].iter().map(|w| {
                            let cell = div().size(px(pitch)).flex_none().flex().items_center();
                            match w[r] {
                                Some(x) => cell.child(
                                    div()
                                        .size(px(dot))
                                        .rounded(px(if dot >= 6. { 2. } else { 1. }))
                                        .bg(heat(x.level)),
                                ),
                                None => cell,
                            }
                        }))
                })),
        )
        .child(legend());
    if mode == MonthMode::Full {
        let rows = |r: &[(&'static str, String, String, Tone)]| {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .children(r.iter().map(|(k, v, sub, tone)| {
                    div()
                        .h(px(16.))
                        .line_height(px(14.))
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .child(div().w(px(52.)).flex_none().text_color(dim()).child(*k))
                        .child(
                            div()
                                .text_color(match tone {
                                    Tone::Plain => text(),
                                    Tone::Accent => hsla(t::ACCENT),
                                    Tone::Ok => hsla(t::SUCCESS),
                                })
                                .child(v.clone()),
                        )
                        .child(div().text_color(dim()).child(sub.clone()))
                }))
        };
        let (a, b) = m.summary.split_at(m.summary.len().div_ceil(2));
        d = d.child(
            div()
                .flex()
                .gap(px(12.))
                .mt(px(0.))
                .child(rows(a))
                .child(rows(b)),
        );
    }
    d
}

/// The scroll offset (px from the top) that brings the row `[top, top + row)` into a view of
/// `view` px currently showing from `cur`, or `None` when it is already whole in view.
pub fn reveal_offset(cur: f32, top: f32, row: f32, view: f32) -> Option<f32> {
    if top < cur {
        Some(top)
    } else if top + row > cur + view {
        Some((top + row - view).max(0.))
    } else {
        None
    }
}

/// Keeps list row `i` (rows of `row_h`) in view of `h`.
pub fn reveal_row(h: &gpui::ScrollHandle, i: usize, row_h: f32) {
    let (cur, view) = (-f32::from(h.offset().y), f32::from(h.bounds().size.height));
    let top = 2. + i as f32 * row_h;
    if let Some(next) = reveal_offset(cur, top, row_h, view) {
        h.set_offset(gpui::point(h.offset().x, px(-next)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_below_the_view_scrolls_up_to_it() {
        assert_eq!(reveal_offset(0., 150., 36., 100.), Some(86.));
        assert_eq!(reveal_offset(100., 36., 36., 100.), Some(36.));
        assert_eq!(reveal_offset(0., 36., 36., 100.), None);
    }
}
