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

use crate::live::{DialogKind, Live, PickerKind, input_lines};
use crate::rewind::GROUP_LINES;
use crate::theme::hsla;
use crate::tui_view::panels::{OnClick, dim, strong, text};

/// Lines of the tool input a permission dialog lists before the count of the rest (shown prominently).
const INPUT_LINES: usize = 200;

/// A mouse press on the panel.
pub type OnMouseDown =
    Box<dyn Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static>;

/// The front dialog as a panel for the chat column, docked above the prompt, or `None`. `click(i)` is the
/// handler of option `i`; `scroll` keeps the option list's position (the pickers scroll to the selected row);
/// `border` is the panel's border (the red of the approval signal, or the accent).
pub fn dock(
    live: &Live,
    scroll: &ScrollHandle,
    click: &dyn Fn(usize) -> OnClick,
    toggle: &dyn Fn(usize) -> OnClick,
    border: gpui::Hsla,
    max_input_h: f32,
    engage: OnMouseDown,
) -> Option<AnyElement> {
    let d = live.dialogs.first()?;
    let queued = live.dialogs.len() - 1;
    let mut card = div()
        .debug_selector(|| "dialog".to_string())
        .flex_none()
        .mx(px(12.))
        .mb(px(8.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(12.))
        .bg(crate::theme::solid(t::SURFACE))
        .border_1()
        .border_color(border)
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
            let all = input_lines(&a.tool, &a.input, usize::MAX);
            let hidden = all.len().saturating_sub(INPUT_LINES);
            let lines: Vec<String> = all.into_iter().take(INPUT_LINES).collect();
            // what the box can show at once: when the input is longer (or wraps), say so
            let scrolls = lines.len() as f32 * 19. > max_input_h
                || lines.iter().any(|l| l.chars().count() > 100);
            card = card.child(
                div()
                    .id("dialog-input")
                    .max_h(px(max_input_h))
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
            if scrolls {
                card = card.child(div().text_color(hsla(t::WARN)).child(
                    "\u{2195} more than fits: scroll the box to read all of it before answering",
                ));
            }
            if hidden > 0 {
                card = card.child(div().text_color(hsla(t::ERROR)).child(format!(
                    "\u{2026} {hidden} more line{} NOT shown here: this is not the whole input",
                    if hidden == 1 { "" } else { "s" }
                )));
            }
        }
        DialogKind::Plan { plan, .. } => {
            card = card.child(
                div()
                    .id("dialog-plan")
                    .max_h(px(200.))
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
        // a memory confirmation: the whole text it is about, in a scroll box like the approval's input
        DialogKind::Picker {
            kind: PickerKind::MemConfirm | PickerKind::CloseWorkspace,
            headers,
            ..
        } => {
            let lines: Vec<String> = headers
                .first()
                .map(|(_, h)| h.lines().map(String::from).collect())
                .unwrap_or_default();
            let scrolls = lines.len() as f32 * 19. > max_input_h
                || lines.iter().any(|l| l.chars().count() > 100);
            card = card.child(
                div()
                    .id("dialog-memory")
                    .max_h(px(max_input_h))
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
            if scrolls {
                card = card.child(
                    div()
                        .text_color(hsla(t::WARN))
                        .child("\u{2195} scroll the box to read all of it before answering"),
                );
            }
        }
        DialogKind::Picker { .. } => {}
        DialogKind::Rewind(r) => {
            card = card.child(div().text_color(hsla(t::WARN)).child(r.preview.warning()));
            let groups = r.preview.groups();
            card = card.child(
                div()
                    .id("dialog-rewind")
                    .max_h(px(max_input_h))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .bg(hsla(t::BG))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(groups.into_iter().enumerate().map(|(g, grp)| {
                        let open = r.expanded.contains(&g);
                        let total = grp.rows.len();
                        let cut = !open && total > GROUP_LINES;
                        let col = if grp.title.starts_with("Will be deleted") {
                            hsla(t::ERROR)
                        } else if grp.title.starts_with("Left alone") {
                            dim()
                        } else {
                            hsla(t::WARN)
                        };
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_color(col).child(grp.title))
                            .children(
                                grp.rows
                                    .into_iter()
                                    .take(if cut { GROUP_LINES } else { usize::MAX })
                                    .map(|l| div().text_color(text()).child(l)),
                            )
                            .when(total > GROUP_LINES, |d| {
                                d.child(
                                    div()
                                        .id(("rewind-more", g))
                                        .cursor_pointer()
                                        .text_color(hsla(t::ACCENT))
                                        .child(if cut {
                                            format!("Show {} more", total - GROUP_LINES)
                                        } else {
                                            "Show less".to_string()
                                        })
                                        .on_click(toggle(g)),
                                )
                            })
                    })),
            );
        }
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
        DialogKind::Picker {
            kind: PickerKind::MemConfirm | PickerKind::CloseWorkspace,
            ..
        } => &[],
        DialogKind::Picker { headers, .. } => headers,
        _ => &[],
    };
    let mut list: Vec<AnyElement> = vec![];
    let now = crate::live::now_ms();
    for (i, (key, label)) in d.options.iter().enumerate() {
        if let Some((_, h)) = headers.iter().find(|(at, _)| *at == i) {
            list.push(
                div()
                    .px(px(10.))
                    .pt(px(8.))
                    .flex()
                    .gap(px(8.))
                    .text_color(hsla(t::ACCENT))
                    .child(match h.split_once('\u{1f}') {
                        // a label and the id after it, painted apart so a name cannot pass for the id
                        Some((name, id)) => div()
                            .flex()
                            .gap(px(8.))
                            .child(name.to_string())
                            .child(div().text_color(dim()).child(id.to_string()))
                            .into_any_element(),
                        None => div().child(h.clone()).into_any_element(),
                    })
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
                .children(send_dot(live, d, key, now))
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
            .max_h(px(180.))
            .overflow_y_scroll()
            .track_scroll(scroll)
            .children(list),
    );
    let verb = match (&d.kind, typing.is_some()) {
        (_, true) => "Enter keeps the text \u{b7} Esc drops it",
        (
            DialogKind::Picker {
                kind: PickerKind::Model,
                ..
            },
            _,
        ) if d.answerable() && live.held_prompt.is_some() => {
            "Enter sends your prompt to the chosen model \u{b7} Esc returns it unsent"
        }
        (DialogKind::Picker { .. }, _) if d.answerable() => "Enter selects \u{b7} Esc closes",
        // a dialog nobody has touched ignores Enter and digits: they were probably meant for the prompt
        _ if !d.answerable() => {
            "\u{2191}/\u{2193} or click to choose, then Enter \u{b7} Esc rejects"
        }
        _ => "Enter confirms \u{b7} Esc rejects",
    };
    card = card.child(div().text_color(dim()).child(format!(
        "{}{verb}{}",
        if d.answerable() && d.digits_pick() {
            "\u{2191}\u{2193} or 1-9 choose \u{b7} "
        } else if d.answerable() {
            "\u{2191}\u{2193} choose \u{b7} type to filter \u{b7} "
        } else {
            ""
        },
        if queued > 0 {
            format!(" \u{b7} {queued} more waiting")
        } else {
            String::new()
        }
    )));
    // any press on the panel engages it: from then on Enter and digits answer
    Some(
        card.on_mouse_down(gpui::MouseButton::Left, engage)
            .into_any_element(),
    )
}

/// The activity dot of a `/send` picker row (the Running panel's rule), `None` in any other dialog.
fn send_dot(live: &Live, d: &crate::live::Dialog, key: &str, now: i64) -> Option<gpui::Div> {
    use crate::tui_view::activity::Presence;
    if !matches!(
        d.kind,
        DialogKind::Picker {
            kind: PickerKind::Send,
            ..
        }
    ) {
        return None;
    }
    let r = live
        .send_pick
        .as_ref()?
        .rows
        .iter()
        .find(|r| r.address == key)?;
    Some(
        match r.presence(now) {
            Presence::Active => crate::widgets::chrome::dot(hsla(t::SUCCESS), true),
            Presence::Recent => crate::widgets::chrome::dot(hsla(t::SUCCESS), false),
            Presence::Inactive => crate::widgets::chrome::dot(dim(), true),
        }
        .mt(px(6.)),
    )
}
