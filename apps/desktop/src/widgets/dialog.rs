//! The modal dialogs shared by the TUI view and the Agent view: permission asks (`permission` and
//! `outside_read`), the plan, the questions (with the free-text "Other" answer) and the pickers. The text
//! is what the harness sent, shown the way the TUI shows it: the reason first, then every line of the
//! tool input.

use gpui::{
    AnyElement, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use gpui_component::text::TextView;
use synthesis_tui_render::theme as t;

use crate::live::{DialogKind, Live, input_lines};
use crate::theme::hsla;
use crate::tui_view::panels::{OnClick, dim, strong, text};

/// Lines of the tool input a permission dialog shows before the count of the rest.
const INPUT_LINES: usize = 40;

/// The front dialog over the whole window, or `None`. `click(i)` is the handler of option `i`;
/// `scroll` keeps the option list's position (the pickers scroll to the selected row).
pub fn overlay(
    live: &Live,
    scroll: &ScrollHandle,
    click: &dyn Fn(usize) -> OnClick,
) -> Option<AnyElement> {
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
        DialogKind::Picker { .. } => {}
    }
    let typing = match &d.kind {
        DialogKind::Question { typing, .. } => typing.clone(),
        _ => None,
    };
    if let Some(t) = &typing {
        card = card.child(
            div()
                .flex()
                .gap(px(8.))
                .p(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(hsla(t::ACCENT))
                .bg(hsla(t::BG))
                .child(div().text_color(dim()).child("Other:"))
                .child(div().text_color(strong()).child(format!("{t}\u{258F}"))),
        );
    }
    let headers: &[(usize, String)] = match &d.kind {
        DialogKind::Picker { headers, .. } => headers,
        _ => &[],
    };
    let mut list: Vec<AnyElement> = vec![];
    for (i, (_, label)) in d.options.iter().enumerate() {
        if let Some((_, h)) = headers.iter().find(|(at, _)| *at == i) {
            list.push(
                div()
                    .px(px(10.))
                    .pt(px(8.))
                    .text_color(hsla(t::ACCENT))
                    .child(h.clone())
                    .into_any_element(),
            );
        }
        let on = i == d.sel;
        list.push(
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
                        .child(if i < 9 {
                            format!("{}.", i + 1)
                        } else {
                            String::new()
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(if on { strong() } else { text() })
                        .child(label.clone()),
                )
                .on_click(click(i))
                .into_any_element(),
        );
    }
    card = card.child(
        div()
            .id("dialog-list")
            .debug_selector(|| "dialog-list".to_string())
            .flex()
            .flex_col()
            .max_h(px(420.))
            .overflow_y_scroll()
            .track_scroll(scroll)
            .children(list),
    );
    let verb = match (&d.kind, typing.is_some()) {
        (_, true) => "Enter keeps the text \u{b7} Esc drops it",
        (DialogKind::Picker { .. }, _) => "Enter selects \u{b7} Esc closes",
        _ => "Enter confirms \u{b7} Esc rejects",
    };
    card = card.child(div().text_color(dim()).child(format!(
        "\u{2191}\u{2193} or 1-9 choose \u{b7} {verb}{}",
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
