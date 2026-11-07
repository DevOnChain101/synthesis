//! The panel bodies of the TUI view, 1:1 with `cli.html` (`.rows`, `.ws-item`, `.run-item`, `.kv`, `.chk`,
//! `.bars`): Workspace, Running, Session, Tools & Skills, Recent Activity, User Prompt, Agent Status, Context
//! and Usage & Metrics. The panel chrome itself is `widgets::chrome`.

use gpui::{
    Div, InteractiveElement as _, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::*, px,
};
use synthesis_tui_render::theme as t;

use super::sample::{
    ActivityRow, AgentStatus, ContextInfo, PromptRow, Running, SessionInfo, StepKind, ToolsInfo,
    Usage, Workspace,
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
    on_click: Option<&dyn Fn(usize) -> OnClick>,
    on_new: Option<OnClick>,
) -> Div {
    let mut col = div().flex().flex_col();
    for (i, w) in ws.iter().enumerate() {
        col = col.child(
            li().child(
                item_box(w.selected, (1., 8., 1., 8.))
                    .id(("ws-row", i))
                    .items_center()
                    .when_some(on_click, |d, f| d.cursor_pointer().on_click(f(i)))
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
                    ),
            ),
        );
    }
    col.child(
        li().child(
            item_box(false, (1., 8., 1., 8.))
                .id("ws-new")
                .items_center()
                .text_color(dim())
                .when_some(on_new, |d, f| d.cursor_pointer().on_click(f))
                .child(icon("plus", 14., dim()))
                .child("new workspace..."),
        ),
    )
}

pub fn running_body(
    rs: &[Running],
    on_click: Option<&dyn Fn(usize) -> OnClick>,
    on_new: Option<OnClick>,
) -> Div {
    let mut col = div().flex().flex_col();
    for (i, r) in rs.iter().enumerate() {
        col = col.child(
            li().child(
                item_box(r.selected, (5., 7., 5., 6.))
                    .id(("run-row", i))
                    .when_some(on_click.filter(|_| !r.peer), |d, f| {
                        d.cursor_pointer().on_click(f(i))
                    })
                    .child(
                        dot(hsla(t::SUCCESS), r.live)
                            .mt(px(5.))
                            .when(!r.live, |d| d.border_color(dim())),
                    )
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
                                            .text_color(strong())
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
                                            .text_color(hsla(t::INFO))
                                            .child(r.folder.clone()),
                                    )
                                    .when(!r.branch.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(3.))
                                                .min_w(px(0.))
                                                .overflow_hidden()
                                                .child(icon("git-branch", 12., dim()))
                                                .child(div().truncate().child(r.branch.clone())),
                                        )
                                    }),
                            ),
                    ),
            ),
        );
    }
    col.child(
        li().child(
            div()
                .id("run-new")
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
                .when_some(on_new, |d, f| d.cursor_pointer().on_click(f))
                .child(icon("plus", 14., dim()))
                .child("new session..."),
        ),
    )
}

/// `.kv`: dim label column, strong value; rows `row_gap` apart.
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
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(strong())
                        .child(v.clone()),
                )
        }))
}

pub fn session_meta(s: &SessionInfo) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_color(hsla(t::SUCCESS))
        .child(dot(hsla(t::SUCCESS), true))
        .child(s.status.clone())
        .into_any_element()
}

pub fn session_body(s: &SessionInfo) -> Div {
    kv(&s.rows, 62., 12., 3.)
}

pub fn tools_body(k: &ToolsInfo) -> Div {
    div()
        .flex()
        .flex_col()
        .children(k.rows.iter().map(|r| {
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
        .child(
            li().flex()
                .items_center()
                .gap(px(8.))
                .text_color(dim())
                .child(div().w(px(7.)).flex_none())
                .child(format!("+ {} more...", k.more)),
        )
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

pub fn activity_body(rows: &[ActivityRow]) -> Div {
    div().flex().flex_col().children(rows.iter().map(|a| {
        li().flex()
            .items_center()
            .gap(px(8.))
            .child(circ(None))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .child(a.text.clone()),
            )
            .child(div().flex_none().text_color(dim()).child(a.ago.clone()))
    }))
}

pub fn prompts_body(rows: &[PromptRow]) -> Div {
    if rows.is_empty() {
        return div().text_color(dim()).child("No messages yet");
    }
    div().flex().flex_col().children(rows.iter().map(|p| {
        li().flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .w(px(64.))
                    .flex_none()
                    .text_color(dim())
                    .child(p.time.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .child(p.text.clone()),
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
                        .text_color(hsla(t::SUCCESS))
                        .child(format!("{} / {}", c.used, c.max)),
                )
                .child(
                    div()
                        .ml_auto()
                        .text_color(strong())
                        .child(format!("{:.0}%", c.pct)),
                ),
        )
        .child(div().my(px(4.)).mb(px(6.)).child(bar(c.pct)))
        .child(kv(&rows, 62., 10., 1.))
}

pub fn usage_meta(u: &Usage) -> gpui::AnyElement {
    div()
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
