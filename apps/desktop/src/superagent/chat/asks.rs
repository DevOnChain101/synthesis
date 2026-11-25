//! Permission asks raised in the orchestrator chat. ONE queue holds them in arrival order; the modal (the front
//! ask only), the cards in the chat (all of them) and the Needs-you rows of the home are three views of it, so an
//! answer from any of them leaves the queue for all of them, and `take` hands each ask out once. The semantics
//! are the TUI's (`widgets::dialog`): Allow once, Always (when the harness offered a rule), Deny; an Allow waits
//! [`ARM`] after the ask became the front one; Deny is always possible. Nothing here answers by itself.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, ClickEvent, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Styled as _, div, px,
};
use serde_json::Value;
use synthesis_harness::sanitize_for_display;

use crate::engine::{AskApproval, Choice};
use crate::live::{input_lines, suggestion_view};
use crate::superagent::kit::{self, ButtonKind, text};
use crate::superagent::tokens as t;
use crate::widgets::chrome::OnClick;
use synthesis_cli_ui_render::dialogs::permission_options;

/// The same arming delay as the TUI's dialogs.
pub const ARM: Duration = Duration::from_millis(crate::live::ARM_MS as u64);
/// Lines of the tool input a card lists (the modal lists [`MODAL_LINES`]); the count of the rest is said.
const CARD_LINES: usize = 6;
const MODAL_LINES: usize = 60;
/// Prefix of the Needs-you key of an ask of this chat (never a session id).
pub const KEY: &str = "ask:";

pub struct Ask {
    pub id: String,
    pub tool: String,
    pub reason: String,
    pub lines: Vec<String>,
    /// What can be pressed, in order; Deny is always last.
    pub options: Vec<(Choice, String)>,
}

#[derive(Default)]
pub struct Queue {
    q: Vec<Ask>,
    /// When the front ask became the front one.
    front_since: Option<Instant>,
}

impl Queue {
    pub fn push(&mut self, a: &AskApproval, now: Instant) {
        if self.q.iter().any(|x| x.id == a.id) {
            return;
        }
        let views: Vec<Value> = a.suggestions.iter().map(suggestion_view).collect();
        let options = permission_options(&views, false)
            .into_iter()
            .map(|(k, label)| match k.as_str() {
                "yes" => (Choice::Once, "Allow once".to_string()),
                "always" => (Choice::Always, label),
                _ => (Choice::Deny, "Deny".to_string()),
            })
            .collect();
        self.q.push(Ask {
            id: a.id.clone(),
            tool: sanitize_for_display(&a.tool),
            reason: sanitize_for_display(&a.reason),
            lines: input_lines(&a.tool, &a.input, usize::MAX),
            options,
        });
        self.front_since.get_or_insert(now);
    }

    pub fn all(&self) -> &[Ask] {
        &self.q
    }

    pub fn front(&self) -> Option<&Ask> {
        self.q.first()
    }

    /// `choice` for ask `id`. `Some(choice)` once: the ask leaves the queue and the caller answers the engine.
    /// `None`: no such ask (already answered), or an Allow that is not the front ask or not armed yet (it stays).
    pub fn take(&mut self, id: &str, choice: Choice, now: Instant) -> Option<Choice> {
        let i = self.q.iter().position(|a| a.id == id)?;
        if choice != Choice::Deny {
            let armed = self
                .front_since
                .is_some_and(|s| now.duration_since(s) >= ARM);
            if i != 0 || !armed || !self.q[0].options.iter().any(|(c, _)| *c == choice) {
                return None;
            }
        }
        self.q.remove(i);
        if i == 0 {
            self.front_since = (!self.q.is_empty()).then_some(now);
        }
        Some(choice)
    }

    /// The ask is gone on the engine's side (cancelled, expired): it just leaves.
    pub fn withdraw(&mut self, id: &str, now: Instant) {
        if let Some(i) = self.q.iter().position(|a| a.id == id) {
            self.q.remove(i);
            if i == 0 {
                self.front_since = (!self.q.is_empty()).then_some(now);
            }
        }
    }

    /// Everything still waiting leaves, to be denied (the chat is closing).
    pub fn drain(&mut self) -> Vec<String> {
        self.front_since = None;
        self.q.drain(..).map(|a| a.id).collect()
    }

    pub fn armed(&self, now: Instant) -> bool {
        self.front_since
            .is_some_and(|s| now.duration_since(s) >= ARM)
    }

    /// `(Needs-you key, title)` of every ask, oldest first.
    pub fn waiting(&self) -> Vec<(String, String)> {
        self.q
            .iter()
            .map(|a| {
                (
                    format!("{KEY}{}", a.id),
                    format!("Super-Agent: {} needs permission", a.tool),
                )
            })
            .collect()
    }
}

/// One ask as a panel: what is asked, the tool input, and a button per option. `ids` are the three button ids
/// (Allow once, Always, Deny); `allow` says whether an Allow can be pressed now (front and armed).
pub fn body(
    a: &Ask,
    more: usize,
    allow: bool,
    ids: [&'static str; 3],
    mut on: impl FnMut(Choice) -> OnClick,
) -> Div {
    let mut c = div().flex().flex_col().gap(px(t::S3));
    c = c.child(
        text(t::T3, t::W_MEDIUM, t::TEXT_STRONG)
            .whitespace_normal()
            .child(format!("Super-Agent asks to use {}", a.tool)),
    );
    if !a.reason.is_empty() {
        c = c.child(
            text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                .whitespace_normal()
                .child(a.reason.clone()),
        );
    }
    let n = a.lines.len();
    let hidden = n.saturating_sub(more);
    c = c.child(
        div()
            .flex()
            .flex_col()
            .p(px(t::S3))
            .rounded(px(t::R_CONTROL))
            .bg(t::well())
            .children(a.lines.iter().take(more).map(|l| {
                text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                    .whitespace_normal()
                    .child(if l.is_empty() {
                        " ".to_string()
                    } else {
                        l.clone()
                    })
            })),
    );
    if hidden > 0 {
        c = c.child(text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE).child(format!(
            "... {hidden} more lines NOT shown here: this is not the whole input"
        )));
    }
    let buttons = a.options.iter().zip(ids).map(|((ch, label), id)| {
        let kind = match ch {
            Choice::Deny => ButtonKind::Secondary,
            _ if allow => ButtonKind::Primary,
            _ => ButtonKind::Disabled,
        };
        kit::button(id, label.clone(), kind, false, Some(on(*ch))).into_any_element()
    });
    c.child(div().flex().flex_wrap().gap(px(t::S2)).children(buttons))
}

/// The card in the chat's response area.
pub fn card(a: &Ask, allow: bool, on: impl FnMut(Choice) -> OnClick) -> AnyElement {
    kit::card("sa-ask-card", true)
        .child(body(
            a,
            CARD_LINES,
            allow,
            ["sa-ask-once", "sa-ask-always", "sa-ask-deny"],
            on,
        ))
        .into_any_element()
}

/// The modal over the window for the front ask. `queued`: how many wait behind it.
pub fn modal(a: &Ask, queued: usize, allow: bool, on: impl FnMut(Choice) -> OnClick) -> AnyElement {
    let mut panel = kit::card("sa-ask-modal-card", true)
        .w(px(520.))
        .max_w_full()
        .child(body(
            a,
            MODAL_LINES,
            allow,
            ["sa-ask-m-once", "sa-ask-m-always", "sa-ask-m-deny"],
            on,
        ));
    if queued > 0 {
        panel = panel.child(
            text(t::T1, t::W_REGULAR, t::TEXT)
                .child(format!("{queued} more waiting behind this one")),
        );
    }
    div()
        .id("sa-ask-modal")
        .debug_selector(|| "sa-ask-modal".to_string())
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .p(px(t::S5))
        .bg(t::scrim())
        .child(panel)
        .into_any_element()
}

/// A click that is not a real one (a key press on a focused button) never allows.
pub fn real(ev: &ClickEvent) -> bool {
    matches!(ev, ClickEvent::Mouse(_)) && ev.standard_click() && ev.click_count() == 1
}
