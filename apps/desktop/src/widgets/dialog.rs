//! The modal dialogs shared by the TUI view and the Super Agent view: permission asks (`permission` and
//! `outside_read`), the plan, the questions (with the free-text "Other" answer) and the pickers. The text
//! is what the harness sent, shown the way the CLI shows it: the reason first, then every line of the
//! tool input.

use gpui::{
    AnyElement, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use gpui_component::text::TextView;
use synthesis_cli_ui_render::theme as t;

use crate::live::{DialogKind, Live, PickerKind, input_lines};
use crate::rewind::GROUP_LINES;
use crate::theme::hsla;
use crate::tui_view::panels::{OnClick, dim, strong, text};

/// Lines of the tool input a permission dialog lists before the count of the rest (shown prominently).
const INPUT_LINES: usize = 200;

/// A mouse press on the panel.
pub type OnMouseDown =
    Box<dyn Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static>;

/// Whether a scroll box (as last drawn) has more below its visible part.
fn more_below(h: &ScrollHandle) -> bool {
    f32::from(h.max_offset().y) + f32::from(h.offset().y) > 1.
}

/// The line under a scroll box that has more below, so a cut text is never taken for the whole of it.
fn scroll_cue(h: &ScrollHandle, what: &str) -> Option<AnyElement> {
    more_below(h).then(|| {
        div()
            .text_color(hsla(t::WARN))
            .child(format!("\u{2193} more {what}: scroll"))
            .into_any_element()
    })
}

/// One text line of the TUI view: the Trust text box is cut at a multiple of it, never inside a line.
const LH: f32 = crate::tui_view::layout::LINE;

/// Height of the front part of a scroll box's children (rows) that fits `budget` whole: the lowest row edge not
/// below it, at least the first row. `budget` itself while the rows are not measured yet (the first frame).
fn whole_rows(h: &ScrollHandle, budget: f32) -> f32 {
    let top = f32::from(h.bounds().origin.y) - f32::from(h.offset().y);
    let edges: Vec<f32> = (0..)
        .map_while(|i| h.bounds_for_item(i))
        .map(|b| f32::from(b.bottom()) - top)
        .collect();
    let first = edges.first().copied().unwrap_or(budget);
    edges
        .into_iter()
        .filter(|e| *e <= budget + 0.5)
        .fold(first, f32::max)
}

/// The front dialog as a panel for the chat column, docked above the prompt, or `None`. `click(i)` is the
/// handler of option `i`; the scroll handles are (`scroll`, `body_scroll`): `scroll` keeps the option list's position (the pickers scroll to the selected row);
/// `body_scroll` is the one text box above the options (input, plan, memory, trust, rewind, clean);
/// `border` is the panel's border (the red of the approval signal, or the accent). `win_h` sizes the Trust
/// dialog's two boxes, which are cut between lines and rows, never inside one.
pub fn dock(
    live: &Live,
    (scroll, body_scroll): (&ScrollHandle, &ScrollHandle),
    click: &dyn Fn(usize) -> OnClick,
    toggle: &dyn Fn(usize) -> OnClick,
    border: gpui::Hsla,
    (max_input_h, win_h): (f32, f32),
    engage: OnMouseDown,
) -> Option<AnyElement> {
    let d = live.dialogs.first()?;
    let queued = live.dialogs.len() - 1;
    let mut card = div()
        .debug_selector(|| "dialog".to_string())
        .id("dialog")
        // short window: the text boxes and the option list give way and scroll, and the card itself scrolls as the
        // last resort, so the prompt below stays on screen (a card that cannot shrink pushed it out)
        .min_h(px(0.))
        .overflow_y_scroll()
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
                    .track_scroll(body_scroll)
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
            if scrolls || more_below(body_scroll) {
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
                    .track_scroll(body_scroll)
                    .text_color(text())
                    .child(TextView::markdown("plan", plan.clone())),
            );
            card = card.children(scroll_cue(body_scroll, "of the plan"));
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
                    .track_scroll(body_scroll)
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
            if scrolls || more_below(body_scroll) {
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
                    .track_scroll(body_scroll)
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
            card = card.children(scroll_cue(body_scroll, "lines"));
        }
        DialogKind::Clean(c) => {
            let lines = c.lines();
            card = card.child(
                div()
                    .id("dialog-clean")
                    .max_h(px(max_input_h))
                    .overflow_y_scroll()
                    .track_scroll(body_scroll)
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .bg(hsla(t::BG))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(lines.into_iter().enumerate().map(|(i, l)| {
                        let col = match i {
                            1 => hsla(t::ERROR),
                            3 => dim(),
                            _ => text(),
                        };
                        div().text_color(col).child(l)
                    })),
            );
            card = card.children(scroll_cue(body_scroll, "lines"));
            if c.needs_word() {
                let ok = c.word_ok();
                card = card.child(
                    div()
                        .id("clean-word")
                        .flex()
                        .gap(px(8.))
                        .p(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if ok { hsla(t::ERROR) } else { hsla(t::ACCENT) })
                        .bg(hsla(t::BG))
                        .child(div().text_color(dim()).child(if d.answerable() {
                            "Type delete (or clean) to enable the delete row:"
                        } else {
                            "Press Tab or click here first, then type delete (or clean):"
                        }))
                        .child(
                            div()
                                .text_color(strong())
                                .child(format!("{}\u{258F}", c.typed)),
                        ),
                );
            }
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
    let mut list_cap = None;
    let headers: &[(usize, String)] = match &d.kind {
        DialogKind::Picker {
            kind: PickerKind::MemConfirm | PickerKind::CloseWorkspace,
            ..
        } => &[],
        // the Trust paragraphs wrap in a capped scroll box of their own, so the options below stay fully visible
        DialogKind::Picker {
            kind: PickerKind::Trust,
            headers,
            ..
        } => {
            // what the card leaves its two boxes: the chat column keeps about 310px of the window, the card's own
            // padding, title, hint and gaps ~155px; the text gets the larger half, at least three lines
            let room = (win_h - 310. - 155.).max(3. * LH + 63.);
            let text_cap = ((room * 0.55 / LH).floor() * LH).max(3. * LH);
            let shown = f32::from(body_scroll.bounds().size.height);
            let used = if shown > 0. {
                shown.min(text_cap)
            } else {
                text_cap
            };
            list_cap = Some(whole_rows(scroll, (room - used).max(0.)));
            card = card.child(
                div()
                    .id("dialog-trust")
                    .debug_selector(|| "dialog-trust".to_string())
                    .max_h(px(text_cap))
                    .overflow_y_scroll()
                    .track_scroll(body_scroll)
                    .flex()
                    .flex_col()
                    .gap(px(LH))
                    .line_height(px(LH))
                    .text_color(hsla(t::ACCENT))
                    .children(headers.iter().map(|(_, h)| div().child(h.clone()))),
            );
            card = card.children(scroll_cue(body_scroll, "text"));
            &[]
        }
        DialogKind::Picker { headers, .. } => headers,
        _ => &[],
    };
    let trust = matches!(
        d.kind,
        DialogKind::Picker {
            kind: PickerKind::Trust,
            ..
        }
    );
    let mut list: Vec<AnyElement> = vec![];
    let now = crate::live::now_ms();
    for (i, (key, label)) in d.options.iter().enumerate() {
        for (_, h) in headers.iter().filter(|(at, _)| *at == i) {
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
            .when_some(list_cap, |d2, h| d2.max_h(px(h)))
            .when(!trust, |d2| {
                d2.max_h(px(
                    if matches!(
                        d.kind,
                        DialogKind::Picker {
                            kind: PickerKind::Clean,
                            ..
                        }
                    ) {
                        300.
                    } else {
                        180.
                    },
                ))
            })
            .overflow_y_scroll()
            .track_scroll(scroll)
            .children(list),
    );
    card = card.children(scroll_cue(scroll, "options"));
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
