//! The modal dialogs shared by the TUI view and the Agent view: permission asks (`permission` and
//! `outside_read`), the plan and the questions. The text is what the harness sent, shown the way the TUI
//! shows it: the reason first, then every line of the tool input.

use gpui::{
    AnyElement, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use gpui_component::text::TextView;
use synthesis_tui_render::theme as t;

use crate::live::{DialogKind, Live, input_lines};
use crate::theme::hsla;
use crate::tui_view::panels::{OnClick, dim, strong, text};

/// Lines of the tool input a permission dialog shows before the count of the rest.
const INPUT_LINES: usize = 40;

/// The front dialog over the whole window, or `None`. `click(i)` is the handler of option `i`.
pub fn overlay(live: &Live, click: &dyn Fn(usize) -> OnClick) -> Option<AnyElement> {
    let d = live.dialogs.first()?;
    let queued = live.dialogs.len() - 1;
    let mut card = div()
        .debug_selector(|| "dialog".to_string())
        .w(px(680.))
        .max_w_full()
        .max_h_full()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(16.))
        .bg(hsla(t::SURFACE))
        .border_1()
        .border_color(hsla(t::ACCENT))
        .rounded(px(10.))
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(strong())
                .child(d.title()),
        );
    match &d.kind {
        DialogKind::Approval(a) => {
            if !a.reason.is_empty() {
                card = card.child(div().text_color(hsla(t::WARN)).child(a.reason.clone()));
            }
            if let Some(id) = &a.agent_id {
                card = card.child(
                    div()
                        .text_color(dim())
                        .child(format!("asked by agent {id}")),
                );
            }
            let lines = input_lines(&a.tool, &a.input, INPUT_LINES);
            card = card.child(
                div()
                    .id("dialog-input")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .bg(hsla(t::BG))
                    .children(lines.into_iter().map(|l| {
                        div().text_color(text()).child(if l.is_empty() {
                            " ".to_string()
                        } else {
                            l
                        })
                    })),
            );
        }
        DialogKind::Plan { plan, .. } => {
            card = card.child(
                div()
                    .id("dialog-plan")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .text_color(text())
                    .child(TextView::markdown("plan", plan.clone())),
            );
        }
        DialogKind::Question { questions, cur, .. } => {
            let q = &questions[*cur];
            card = card
                .child(div().text_color(dim()).child(format!(
                    "question {} of {}{}",
                    cur + 1,
                    questions.len(),
                    q.header.as_deref().map_or(String::new(), |h| format!(" \u{b7} {h}"))
                )))
                .child(div().text_color(strong()).child(q.question.clone()));
        }
    }
    card = card.children(d.options.iter().enumerate().map(|(i, (_, label))| {
        let on = i == d.sel;
        div()
            .id(("dialog-opt", i))
            .flex()
            .gap(px(10.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(6.))
            .cursor_pointer()
            .border_1()
            .border_color(if on {
                hsla(t::ACCENT)
            } else {
                gpui::transparent_black()
            })
            .when(on, |d| d.bg(hsla(t::ELEVATED)))
            .hover(|d| d.bg(hsla(t::ELEVATED)))
            .child(
                div()
                    .w(px(18.))
                    .flex_none()
                    .text_color(dim())
                    .child(format!("{}.", i + 1)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_color(if on { strong() } else { text() })
                    .child(label.clone()),
            )
            .on_click(click(i))
    }));
    card = card.child(div().text_color(dim()).child(format!(
        "\u{2191}\u{2193} or 1-9 choose \u{b7} Enter confirms \u{b7} Esc rejects{}",
        if queued > 0 {
            format!(" \u{b7} {queued} more waiting")
        } else {
            String::new()
        }
    )));
    Some(
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .bg(gpui::hsla(0., 0., 0., 0.55))
            // a click on the backdrop answers nothing, and nothing underneath gets it
            .occlude()
            .child(card)
            .into_any_element(),
    )
}
