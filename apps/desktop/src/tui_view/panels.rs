//! Panel building blocks and the panel bodies of the TUI view (`cli.html` `.panel`, `.rows`, `.kv`).

use gpui::{
    AnyElement, App, ClickEvent, Div, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
    svg,
};
use synthesis_tui_render::theme as t;

use super::layout::{
    BODY_PAD, BODY_PAD_LEFT_BOTTOM, HEAD_LINE, HEAD_PAD, HEAD_PAD_LEFT, KV_ROW, PANEL_RADIUS, ROW_H,
};
use super::sample::{Activity, McpRow, McpState, Running, Snapshot, Tokens, Workspace};
use super::tracked;
use crate::theme::hsla;

pub fn text() -> Hsla {
    hsla(t::TEXT)
}
pub fn strong() -> Hsla {
    hsla(t::TEXT_STRONG)
}
pub fn dim() -> Hsla {
    hsla(t::MUTED)
}

pub fn icon(name: &str, size: f32, c: Hsla) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .flex_none()
        .text_color(c)
}

#[derive(Clone, Copy)]
pub struct Head {
    pub id: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    /// Left column panels use the tighter header (`#view1 .col.l .panel>header`).
    pub left: bool,
    /// Short windows tighten the Topology header (`max-height: 749px`).
    pub compact: bool,
}

/// A panel: bordered, rounded, header (icon + uppercase tracked title + optional meta) and a body.
/// `fit` bodies size to their content (capped by `max_h`); the others fill the panel.
pub fn panel(
    h: Head,
    meta: Option<AnyElement>,
    fit: bool,
    max_h: Option<f32>,
    body: impl IntoElement,
) -> Div {
    let (pt, pb) = if h.left { HEAD_PAD_LEFT } else { HEAD_PAD };
    let (pt, pb, size, px_) = if h.compact {
        (4., 2., 11., 10.)
    } else {
        (pt, pb, 12., 12.)
    };
    let bottom = if h.left {
        BODY_PAD_LEFT_BOTTOM
    } else {
        BODY_PAD.1
    };
    let mut b = div()
        .id(h.id)
        .debug_selector(|| format!("{}-body", h.id))
        .min_h(px(0.))
        .px(px(12.))
        .pt(px(BODY_PAD.0))
        .overflow_y_scroll();
    b = if fit { b.flex_initial() } else { b.flex_1() };
    if let Some(m) = max_h {
        b = b.max_h(px(m));
    }
    let mut head = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .px(px(px_))
        .pt(px(pt))
        .pb(px(pb))
        .h(px(pt + pb + HEAD_LINE))
        .child(icon(h.icon, 14., hsla(t::ACCENT)))
        .child(tracked(&h.title.to_uppercase(), size, 0.16, strong()));
    if let Some(m) = meta {
        head = head.child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(text())
                .child(m),
        );
    }
    div()
        .debug_selector(|| h.id.to_string())
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(hsla(t::SURFACE))
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(PANEL_RADIUS))
        .child(head)
        .child(b.child(body))
        // bottom padding outside the scroll area, or the next row shows through it
        .child(div().h(px(bottom)).flex_none())
}

pub fn dot(c: Hsla, filled: bool) -> Div {
    let d = div().size(px(7.)).flex_none().rounded_full().mt(px(1.));
    if filled {
        d.bg(c)
    } else {
        d.border_1().border_color(c)
    }
}

/// `.kv`: dim label, strong value.
pub fn kv(rows: &[(String, String)], label_w: f32) -> Div {
    div().flex().flex_col().children(rows.iter().map(|(k, v)| {
        div()
            .flex()
            .h(px(KV_ROW))
            .flex_none()
            .gap(px(12.))
            .child(
                div()
                    .w(px(label_w))
                    .flex_none()
                    .text_color(dim())
                    .child(k.clone()),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(strong())
                    .child(v.clone()),
            )
    }))
}

fn row_box(selected: bool) -> Div {
    let d = div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .rounded(px(5.))
        .border_1()
        .border_color(gpui::transparent_black());
    if selected {
        // gpui has no per-side border colour: the accent edge is a bar
        d.bg(hsla(t::ELEVATED))
            .border_color(hsla(t::BORDER))
            .text_color(strong())
            .child(div().w(px(3.)).h_full().flex_none().bg(hsla(t::ACCENT)))
    } else {
        d.text_color(text())
    }
}

fn two_lines(top: String, sub: String, selected: bool) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .flex_1()
        .child(div().truncate().line_height(px(16.)).child(top))
        .child(
            div()
                .truncate()
                .line_height(px(15.))
                .text_size(px(11.5))
                .text_color(if selected { text() } else { dim() })
                .child(sub),
        )
}

/// A click on row `i`.
pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub fn workspace_rows(ws: &[Workspace], on_click: Option<&dyn Fn(usize) -> OnClick>) -> Div {
    div()
        .flex()
        .flex_col()
        .children(ws.iter().enumerate().map(|(i, w)| {
            row_box(w.selected)
                .id(("ws-row", i))
                .when_some(on_click, |d, f| d.cursor_pointer().on_click(f(i)))
                .child(icon("folder", 14., hsla(t::ACCENT)))
                .child(two_lines(w.name.clone(), w.path.clone(), w.selected))
                .child(
                    div()
                        .flex_none()
                        .text_color(dim())
                        .child(w.sessions.to_string()),
                )
        }))
}

pub fn running_rows(rs: &[Running], on_click: Option<&dyn Fn(usize) -> OnClick>) -> Div {
    div()
        .flex()
        .flex_col()
        .children(rs.iter().enumerate().map(|(i, r)| {
            row_box(r.selected)
                .id(("run-row", i))
                .when_some(on_click.filter(|_| !r.peer), |d, f| {
                    d.cursor_pointer().on_click(f(i))
                })
                .child(dot(hsla(t::SUCCESS), r.live).when(!r.live, |d| d.border_color(dim())))
                .child(two_lines(r.title.clone(), r.branch.clone(), r.selected))
                .child(div().flex_none().text_color(dim()).child(r.age.clone()))
        }))
}

pub fn mcp_rows(rows: &[McpRow]) -> Div {
    div().flex().flex_col().children(rows.iter().map(|m| {
        let c = match m.state {
            McpState::Connected => hsla(t::SUCCESS),
            McpState::Connecting => hsla(t::WARN),
            McpState::Failed => hsla(t::ERROR),
        };
        div()
            .flex()
            .h(px(KV_ROW))
            .flex_none()
            .items_center()
            .gap(px(8.))
            .child(dot(c, m.state == McpState::Connected))
            .child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .flex_1()
                    .child(m.name.clone()),
            )
            .child(div().flex_none().text_color(dim()).child(match m.state {
                McpState::Connected => format!("{} tools", m.tools),
                McpState::Connecting => "connecting".to_string(),
                McpState::Failed => "failed".to_string(),
            }))
    }))
}

pub fn activity_rows(rows: &[Activity]) -> Div {
    div().flex().flex_col().children(rows.iter().map(|a| {
        div()
            .flex()
            .gap(px(8.))
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(dim())
                    .child(a.time.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(10.))
                    .text_color(hsla(t::ACCENT))
                    .child(if a.prompt { "\u{276F}" } else { "" }),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .truncate()
                    .text_color(if a.prompt { strong() } else { text() })
                    .child(a.text.clone()),
            )
    }))
}

pub fn status_body(s: &Snapshot) -> Div {
    kv(&s.status, 58.)
}

pub fn tokens_body(k: &Tokens) -> Div {
    let rows = vec![
        ("input".to_string(), k.input.clone()),
        ("output".to_string(), k.output.clone()),
        ("cache".to_string(), k.cache.clone()),
        ("cost".to_string(), k.cost.clone()),
    ];
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(kv(&rows, 58.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().w(px(58.)).flex_none().text_color(dim()).child("ctx"))
                .child(
                    div()
                        .flex_1()
                        .h(px(5.))
                        .rounded(px(3.))
                        .bg(hsla(t::BORDER))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(k.ctx_pct.clamp(0., 100.) / 100.))
                                .rounded(px(3.))
                                .bg(hsla(t::ACCENT)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(strong())
                        .child(format!("{:.0}%", k.ctx_pct)),
                ),
        )
}

/// Heat level 0..=4 of a day relative to the busiest one shown.
pub fn level(v: f32, max: f32) -> usize {
    if v <= 0. || max <= 0. {
        0
    } else {
        ((v / max * 4.).ceil() as usize).clamp(1, 4)
    }
}

/// Weeks (columns) of 11px cells with 3px gaps that fit in `w` px.
pub fn heat_weeks(w: f32) -> usize {
    (((w + 3.) / 14.).floor() as usize).max(1)
}

/// The levels of every cell of a `weeks`-wide grid (column-major, 7 rows): the newest days at the
/// end, empty cells before a short history, so the grid is always drawn in full.
pub fn heat_levels(days: &[f32], weeks: usize) -> Vec<usize> {
    let n = (weeks * 7).min(days.len());
    let shown = &days[days.len() - n..];
    let max = shown.iter().cloned().fold(0., f32::max);
    let mut v = vec![0; weeks * 7 - n];
    v.extend(shown.iter().map(|d| level(*d, max)));
    v
}

pub fn heat_body(s: &Snapshot, inner_w: f32) -> Div {
    let weeks = heat_weeks(inner_w);
    let levels = heat_levels(&s.usage.days, weeks);
    let cell = |lvl: usize| {
        div()
            .size(px(11.))
            .flex_none()
            .rounded(px(2.))
            .bg(hsla(t::HEAT[lvl]))
    };
    let cols = (0..weeks).map(|c| {
        div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .children((0..7).map(|r| cell(levels[c * 7 + r])))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(div().flex().gap(px(3.)).children(cols))
        .child(
            div()
                .flex()
                .gap(px(12.))
                .children(s.usage.rows.iter().map(|(k, v)| {
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(div().text_color(dim()).child(k.clone()))
                        .child(div().text_color(strong()).child(v.clone()))
                })),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(3.))
                .text_size(px(11.))
                .text_color(dim())
                .child("less")
                .children((0..5).map(cell))
                .child("more"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_levels_and_weeks() {
        assert_eq!(level(0., 100.), 0);
        assert_eq!(level(1., 100.), 1);
        assert_eq!(level(100., 100.), 4);
        assert_eq!(level(50., 100.), 2);
        assert_eq!(level(5., 0.), 0);
        // 500px right column: 500 - 2 border - 24 padding = 474 wide
        assert_eq!(heat_weeks(474.), 34);
        assert_eq!(heat_weeks(1.), 1);
        // a sparse history still fills the grid: empty cells first, the days at the end
        let v = heat_levels(&[0., 5.], 53);
        assert_eq!(
            (
                v.len(),
                v[..v.len() - 1].iter().sum::<usize>(),
                v[v.len() - 1]
            ),
            (371, 0, 4)
        );
        assert_eq!(heat_levels(&[], 4), vec![0; 28]);
    }
}
