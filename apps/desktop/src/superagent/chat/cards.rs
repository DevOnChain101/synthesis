//! A proposal card (super-agent-s1.md 4.3, 4.8 T3/T12; mock `.card` in a conversation). It is drawn from a
//! `ProposalCard` and nothing else: every payload field, the taint reason, the reasons a card is forced, the
//! hash prefix. No button is focused or preselected. The click handlers are the caller's.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Div, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Styled as _, Window, div, px,
};
use gpui_component::input::{Textarea, TextareaState};
use synthesis_view::conductor::CardButton;

use super::model::Shown;
use crate::superagent::kit::{self, ButtonKind, text};
use crate::superagent::tokens as t;

pub type Act = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// What the person can press. `edit`: the open text box of this card (the "Edit in form" path).
pub struct Acts {
    pub confirm: Act,
    pub edit: Act,
    pub cancel: Act,
}

fn state_word(s: &str) -> &'static str {
    match s {
        "accepted" => "Done",
        "edited" => "Done, with your edit",
        "cancelled" => "Cancelled",
        "stale" => "No longer valid. Nothing was sent. Ask again for a fresh card.",
        "expired" => "Expired. Nothing was sent.",
        "failed" => "Failed. Nothing was changed, or only part of it.",
        _ => "",
    }
}

fn line(label: String, value: String) -> Div {
    div()
        .flex()
        .gap(px(t::S5))
        .child(
            text(t::T2, t::W_REGULAR, t::TEXT)
                .flex_none()
                .min_w(px(96.))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(t::T2.0))
                .line_height(px(t::T2.1))
                .text_color(t::hsla(t::TEXT_STRONG))
                .child(value),
        )
}

pub fn render(
    i: usize,
    s: &Shown,
    editor: Option<&Entity<TextareaState>>,
    acts: &Acts,
) -> AnyElement {
    let c = &s.card;
    let open = !c.buttons.is_empty();
    let warn = c.taint.is_some() || !c.forced.is_empty();
    let mut card = kit::card("sa-card", warn).child(
        text(
            t::T2,
            t::W_MEDIUM,
            if warn { t::SOFT_BEIGE } else { t::TEXT },
        )
        .child(if open {
            format!(
                "Proposed: {}. Nothing happens until you press {}.",
                c.title, c.confirm_label
            )
        } else {
            format!("Proposed: {}", c.title)
        }),
    );
    for why in c.taint.iter().chain(c.forced.iter()) {
        card = card.child(
            text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                .whitespace_normal()
                .child(format!("Check this one: {why}")),
        );
    }
    let rows = div().flex().flex_col().gap(px(t::S2)).children(
        c.fields
            .iter()
            .filter(|(k, _)| editor.is_none() || !is_text(k))
            .map(|(k, v)| line(k.replace('_', " "), v.clone())),
    );
    card = card.child(rows);
    if let Some(e) = editor {
        card = card.child(
            div()
                .rounded(px(t::R_CONTROL))
                .bg(t::well())
                .p(px(t::S3))
                .child(Textarea::new(e).appearance(false).bordered(false)),
        );
    }
    card =
        card.child(text(t::T1, t::W_REGULAR, t::TEXT).child(format!("#{} ({})", c.sha8, c.state)));
    if !open {
        return wrap(
            i,
            card.child(text(t::T2, t::W_MEDIUM, t::TEXT).child(state_word(&c.state))),
        );
    }
    if let Some(n) = &s.note {
        card = card.child(
            text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                .whitespace_normal()
                .child(n.clone()),
        );
    }
    let mut row = div().flex().gap(px(t::S2));
    for b in &c.buttons {
        // nothing is focused or preselected: the primary look marks the kind of action, not a default
        row = row.child(match b {
            CardButton::Confirm => kit::button(
                "sa-card-confirm",
                if editor.is_some() {
                    format!("{} edited", c.confirm_label)
                } else {
                    c.confirm_label.to_string()
                },
                ButtonKind::Primary,
                false,
                Some(forward(&acts.confirm)),
            ),
            CardButton::Edit => kit::button(
                "sa-card-edit",
                if editor.is_some() {
                    "Reset edit"
                } else {
                    "Edit in form"
                },
                ButtonKind::Secondary,
                false,
                Some(forward(&acts.edit)),
            ),
            CardButton::Cancel => kit::button(
                "sa-card-cancel",
                "Cancel",
                ButtonKind::Ghost,
                false,
                Some(forward(&acts.cancel)),
            ),
        });
    }
    wrap(i, card.child(row))
}

/// Each card has its own id, so the buttons of two cards never share state.
fn wrap(i: usize, card: Div) -> AnyElement {
    div().id(("sa-card-wrap", i)).child(card).into_any_element()
}

fn forward(a: &Act) -> crate::widgets::chrome::OnClick {
    let a = a.clone();
    Box::new(move |e, w, cx| a(e, w, cx))
}

/// The free-text payload fields the edit box replaces.
pub fn is_text(key: &str) -> bool {
    matches!(key, "first_message" | "text" | "contract")
}
