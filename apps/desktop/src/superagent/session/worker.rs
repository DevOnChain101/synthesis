//! The Worker frame (phase C3; `super-agent-s1.md` 4.6, 5.1; mock `super-agent-v3` `vSession` with a worker): one work
//! item of an outcome, its attempts, the person's messages to it with how each stands, the recent events that name it
//! (masked), and the composer.
//!
//! **The composer is the one place a worker message is sent from.** The person types, and Enter in the focused
//! composer sends (`submit` is private and has one caller: the `PressEnter` of the composer, which gpui raises only for
//! a real Enter key action, never for IME composition or `set_value`). There is no Send button and no other path (a
//! source test keeps `send_message` to this file). It sends the exact text the person typed, once: a second Enter
//! while one is in flight does nothing (the in-flight key), and the box keeps its text until the owner says what
//! became of it. Per owner default NQ1 there is no card: the person's own line to a worker is not an ask. The owner
//! applies it as `user:superagent` (the surface principal); nothing here builds a `PersonClick`, and message text is
//! never logged or echoed except through the redactor.

use std::cell::Cell;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use synthesis_view::outcomes::{InspectorContent, MessageRow};

use super::outcome::{AttemptRow, item_finished, item_status, word};
use crate::live::{ARM_MS, now_ms};
use crate::superagent::outcomes::link::{Delivery, UNSETTLED};
use crate::superagent::outcomes::model::{Screen, State};
use crate::superagent::outcomes::{MessageSent, OutcomesView};
use crate::superagent::home::{RowSpec, row, section};
use crate::superagent::kit::{self, ButtonKind, Status};
use crate::superagent::tokens::{self as t, hsla};

/// Lines of recent events shown, and the width each is cut to.
const LOG_LINES: usize = 30;
const LOG_CHARS: usize = 300;
pub const WINDOWS_WHY: &str =
    "Worker messages are not available on native Windows. Open the folder in WSL2.";
pub const FINISHED_WHY: &str = "This work is finished. Create a new goal to continue.";
pub const HINT: &str =
    "Enter sends. Shift+Enter adds a line. Guidance only: it cannot widen the contract.";
pub const LOST_LINK: &str =
    "The link was lost before the answer came. Read the messages above before sending it again.";

pub enum WorkerEvent {
    /// "In <outcome>": the outcome frame.
    OpenOutcome,
    /// Escape in the composer: the keys go back to the frame.
    Blur,
}

impl EventEmitter<WorkerEvent> for WorkerFrame {}

/// Whether the composer may send, and if not, why in words.
#[derive(Clone, Debug, PartialEq)]
pub enum Gate {
    /// `chip`: `w3 · attempt a7 (live)` or `w3 · next attempt (queued)`.
    Open {
        chip: String,
    },
    Closed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MsgRow {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub title: String,
    pub status: Status,
    pub state: String,
    pub goal: String,
    pub attempts: Vec<AttemptRow>,
    pub messages: Vec<MsgRow>,
    pub log: Vec<String>,
    pub gate: Gate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Loading,
    Gone(String),
    Ready(Box<View>),
}

fn mask(s: &str) -> String {
    synthesis_view::toolview::redact_text(s)
}

/// Native Windows, where the owner refuses worker messages. Tests run as "not Windows" on every OS unless one
/// forces it, so the open-composer paths are exercised everywhere.
pub fn is_windows() -> bool {
    #[cfg(test)]
    {
        FORCE_WINDOWS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        cfg!(windows)
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_WINDOWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Why the composer is off, or the chip when it is on. `windows` is `cfg!(windows)` (a parameter so both are tested).
fn gate(
    st: &State,
    key: &str,
    item_state: &str,
    newest: Option<(&str, &str)>,
    windows: bool,
) -> Gate {
    if windows {
        return Gate::Closed(WINDOWS_WHY.into());
    }
    if st.screen != Screen::Ready {
        return Gate::Closed(
            "Super Agent is not running, so nothing can be sent. Open the home and press r.".into(),
        );
    }
    if !st.attempt_message {
        return Gate::Closed(
            "This engine does not accept worker messages. Update synt and start it again.".into(),
        );
    }
    let outcome_done = st
        .selected_item()
        .is_some_and(|i| matches!(i.row.state.as_str(), "completed" | "failed" | "cancelled"));
    if item_finished(item_state) || outcome_done {
        return Gate::Closed(FINISHED_WHY.into());
    }
    Gate::Open {
        chip: match newest {
            Some((id, "running")) => {
                format!(
                    "{key} \u{b7} attempt {} (live)",
                    id.chars().take(8).collect::<String>()
                )
            }
            _ => format!("{key} \u{b7} next attempt (queued)"),
        },
    }
}

/// The frame of work item `item` of outcome `id`.
pub fn build(st: &State, id: &str, item: &str, windows: bool) -> Body {
    let Some(o) = st.items.iter().find(|i| i.row.id == id) else {
        return match &st.screen {
            Screen::Loading => Body::Loading,
            Screen::Ready => Body::Gone("This outcome is not in the list. It may be gone.".into()),
            _ => Body::Gone(
                "Super Agent is not reachable, so this work item cannot be shown.".into(),
            ),
        };
    };
    let Some(d) = st
        .detail
        .as_ref()
        .filter(|d| d.outcome == id && st.selected.as_deref() == Some(id))
    else {
        return Body::Loading;
    };
    let w = match &d.workers {
        InspectorContent::Workers(w) => w,
        InspectorContent::Unavailable { reason, .. } => {
            return Body::Gone(format!("The work items could not be read: {reason}"));
        }
        _ => return Body::Gone("The work items were not read.".into()),
    };
    let Some(wi) = w.plan.items.iter().find(|i| i.id == item) else {
        return Body::Gone("This work item is not in the plan.".into());
    };
    let mut mine: Vec<_> = w
        .attempts
        .iter()
        .filter(|a| a.work_item_id == wi.id)
        .collect();
    mine.sort_by_key(|a| a.ordinal);
    let newest = mine.last().map(|a| (a.id.as_str(), a.state.as_str()));
    let ids: Vec<&str> = mine
        .iter()
        .flat_map(|a| [a.id.as_str(), a.session_id.as_str()])
        .filter(|s| !s.is_empty())
        .chain([wi.id.as_str()])
        .collect();
    let attempts = mine
        .iter()
        .map(|a| AttemptRow {
            status: item_status(&a.state),
            title: format!(
                "Attempt {}",
                a.ordinal.map_or_else(|| "?".into(), |o| o.to_string())
            ),
            meta: [
                Some(word(&a.state)),
                (!a.started_at.is_empty()).then(|| format!("started {}", a.started_at)),
                (!a.ended_at.is_empty()).then(|| format!("ended {}", a.ended_at)),
            ]
            .into_iter()
            .flatten()
            .collect(),
        })
        .collect();
    let messages = d
        .messages
        .iter()
        .filter(|m| mine_msg(m, &wi.id, &mine))
        .map(|m| MsgRow {
            label: m.label(),
            text: mask(&m.text),
        })
        .collect();
    let log = match &d.history {
        InspectorContent::Activity(a) => {
            let lines: Vec<&String> = a
                .lines
                .iter()
                .filter(|l| ids.iter().any(|i| l.contains(i)))
                .collect();
            lines[lines.len().saturating_sub(LOG_LINES)..]
                .iter()
                .map(|l| mask(l).chars().take(LOG_CHARS).collect())
                .collect()
        }
        _ => vec![],
    };
    Body::Ready(Box::new(View {
        title: format!("{} {}", wi.key, word(&wi.kind)),
        status: item_status(&wi.state),
        state: word(&wi.state),
        goal: o.row.goal.clone(),
        attempts,
        messages,
        log,
        gate: gate(st, &wi.key, &wi.state, newest, windows),
    }))
}

/// A live message names its attempt (short id), a queued one its work item.
fn mine_msg(
    m: &MessageRow,
    item: &str,
    attempts: &[&synthesis_view::outcomes::AttemptView],
) -> bool {
    m.work_item.as_deref() == Some(item)
        || m.attempt
            .as_deref()
            .is_some_and(|a| attempts.iter().any(|x| x.id.starts_with(a)))
}

/// What the person is told of a delivery.
pub fn delivered_words(d: &Delivery) -> String {
    match d {
        Delivery::Live(Some(a)) => {
            format!("Sent to attempt {a} (live). It takes it at its next step.")
        }
        Delivery::Live(None) => {
            "Sent to the running attempt (live). It takes it at its next step.".into()
        }
        Delivery::Queued => "Queued for the next attempt. It reads it in its first prompt.".into(),
    }
}

pub struct WorkerFrame {
    outcomes: Entity<OutcomesView>,
    outcome: String,
    item: String,
    composer: Entity<TextareaState>,
    /// The idempotency key of the message in flight: a second Enter does nothing until the answer.
    pending: Option<String>,
    /// The key and text of a send whose fate is unknown (lost link, not confirmed): the same text goes out with the
    /// same key until the owner applies or refuses it, so a resend can never be a second message.
    keep: Option<(String, String)>,
    /// What became of the last send: the words, and whether it went.
    result: Option<Result<String, String>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    /// Enter does nothing until this time (ms): a held Enter that opened the frame must not send a kept draft.
    arm_until: Cell<i64>,
}

impl WorkerFrame {
    pub fn new(
        outcomes: Entity<OutcomesView>,
        outcome: String,
        item: String,
        draft: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message this worker")
                .auto_grow(1, 6)
                .submit_on_enter(true)
        });
        if !draft.is_empty() {
            composer.update(cx, |c, cx| c.set_value(draft.to_string(), window, cx));
        }
        // the only caller of `submit`: a real Enter in the focused composer
        cx.subscribe_in(&composer, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(
                ev,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.submit(window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&outcomes, window, |this, _, e: &MessageSent, window, cx| {
            this.sent(e, window, cx)
        })
        .detach();
        cx.observe_in(&outcomes, window, |this, _, window, cx| {
            this.sync(window, cx)
        })
        .detach();
        let focus = cx.focus_handle();
        Self {
            outcomes,
            outcome,
            item,
            composer,
            pending: None,
            keep: None,
            result: None,
            scroll: ScrollHandle::new(),
            focus,
            arm_until: Cell::new(now_ms() + ARM_MS),
        }
    }

    fn body(&self, cx: &App) -> Body {
        build(
            &self.outcomes.read(cx).state,
            &self.outcome,
            &self.item,
            is_windows(),
        )
    }

    fn gate(&self, cx: &App) -> Gate {
        match self.body(cx) {
            Body::Ready(v) => v.gate,
            Body::Loading => Gate::Closed("Loading the work item...".into()),
            Body::Gone(why) => Gate::Closed(why),
        }
    }

    /// The text typed so far (kept per frame by the shell when the tab closes).
    pub fn draft(&self, cx: &App) -> String {
        self.composer.read(cx).value().to_string()
    }

    /// A link that ended while a message was in flight cannot say what became of it.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() && self.outcomes.read(cx).state.screen != Screen::Ready {
            self.pending = None;
            self.result = Some(Err(LOST_LINK.into()));
        }
        let _ = window;
        cx.notify();
    }

    /// Where the shell puts the keys when this frame comes forward: the composer when it takes text.
    pub fn focus_target(&self, cx: &App) -> FocusHandle {
        self.arm_until.set(now_ms() + ARM_MS);
        if matches!(self.gate(cx), Gate::Open { .. }) {
            self.composer.read(cx).focus_handle(cx)
        } else {
            self.focus.clone()
        }
    }

    /// Enter in the focused composer.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        if let Gate::Closed(why) = self.gate(cx) {
            self.result = Some(Err(why));
            cx.notify();
            return;
        }
        if now_ms() < self.arm_until.get() {
            self.result = Some(Err("Look at the message first, then press Enter.".into()));
            cx.notify();
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        // the client and the owner check again; this says it before anything is sent
        if let Err(why) = synthesis_session::outcome::check_message_text(&text) {
            self.result = Some(Err(format!("Not sent: {why}.")));
            cx.notify();
            return;
        }
        let key = match self.keep.take() {
            Some((k, t)) if t == text => k,
            _ => uuid::Uuid::new_v4().to_string(),
        };
        self.keep = Some((key.clone(), text.clone()));
        self.pending = Some(key.clone());
        self.result = None;
        let (o, i) = (self.outcome.clone(), self.item.clone());
        self.outcomes
            .update(cx, |v, cx| v.send_message(&o, &i, text, key, cx));
        self.sync(window, cx);
    }

    fn sent(&mut self, e: &MessageSent, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.as_deref() != Some(e.key.as_str()) {
            return;
        }
        self.pending = None;
        if !matches!(&e.result, Err(w) if w.starts_with(UNSETTLED)) {
            self.keep = None;
        }
        self.result = Some(match &e.result {
            Ok(d) => {
                self.composer
                    .update(cx, |c, cx| c.set_value(String::new(), window, cx));
                Ok(delivered_words(d))
            }
            // the text stays in the box
            Err(why) => Err(format!("Not sent: {why}")),
        });
        self.sync(window, cx);
    }

    fn note(text: &str) -> AnyElement {
        kit::text(t::T3, t::W_REGULAR, t::TEXT)
            .whitespace_normal()
            .px(px(t::S4))
            .child(text.to_string())
            .into_any_element()
    }

    fn body_rows(&self, v: &View, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![
            div()
                .debug_selector(|| "wk-head".to_string())
                .flex()
                .flex_col()
                .gap(px(t::S2))
                .px(px(t::S4))
                .child(
                    kit::text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG)
                        .whitespace_normal()
                        .child(v.title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_x(px(t::S4))
                        .child(kit::status_glyph("wk-status", v.status))
                        .child(kit::text(t::T2, t::W_REGULAR, t::TEXT).child(v.state.clone()))
                        .child(kit::button(
                            "wk-outcome",
                            format!("In {}", v.goal.chars().take(60).collect::<String>()),
                            ButtonKind::Ghost,
                            false,
                            Some(Box::new(
                                cx.listener(|_, _, _, cx| cx.emit(WorkerEvent::OpenOutcome)),
                            )),
                        )),
                )
                .into_any_element(),
        ];
        let attempts = if v.attempts.is_empty() {
            vec![Self::note("No attempt has started yet.")]
        } else {
            v.attempts
                .iter()
                .enumerate()
                .map(|(n, a)| {
                    row(
                        ("wk-attempt", n),
                        RowSpec {
                            status: a.status,
                            title: a.title.clone(),
                            meta: a.meta.clone(),
                            notes: vec![],
                            trailing: vec![],
                            current: false,
                        },
                        None,
                    )
                    .into_any_element()
                })
                .collect()
        };
        out.push(section("wk-attempts", "Attempts", v.attempts.len(), attempts).into_any_element());
        let msgs = if v.messages.is_empty() {
            vec![Self::note("You have not sent this worker anything.")]
        } else {
            v.messages
                .iter()
                .map(|m| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(t::S1))
                        .px(px(t::S4))
                        .child(kit::text(t::T2, t::W_MEDIUM, t::TEXT).child(m.label.clone()))
                        .child(
                            kit::text(t::T3, t::W_REGULAR, t::TEXT_STRONG)
                                .whitespace_normal()
                                .child(m.text.clone()),
                        )
                        .into_any_element()
                })
                .collect()
        };
        out.push(
            section("wk-messages", "Your messages", v.messages.len(), msgs).into_any_element(),
        );
        let log = if v.log.is_empty() {
            vec![Self::note("No events name this work item yet.")]
        } else {
            vec![
                div()
                    .debug_selector(|| "wk-log".to_string())
                    .flex()
                    .flex_col()
                    .gap(px(t::S1))
                    .px(px(t::S4))
                    .children(v.log.iter().map(|l| {
                        kit::text(t::T1, t::W_REGULAR, t::TEXT)
                            .font_family(crate::brand::MONO_FONT)
                            .whitespace_normal()
                            .child(l.clone())
                    }))
                    .into_any_element(),
            ]
        };
        out.push(section("wk-events", "Recent events", v.log.len(), log).into_any_element());
        out
    }

    fn composer_el(&self, gate: &Gate, cx: &App) -> AnyElement {
        let (chip, off) = match gate {
            Gate::Open { chip } => (chip.clone(), None),
            Gate::Closed(why) => ("Message".to_string(), Some(why.clone())),
        };
        let hint = match (&self.result, &off, self.pending.is_some()) {
            (_, _, true) => ("Sending...".to_string(), t::TEXT),
            (_, Some(why), _) => (why.clone(), t::SOFT_BEIGE),
            (Some(Ok(w)), ..) => (w.clone(), t::TEXT_STRONG),
            (Some(Err(w)), ..) => (w.clone(), t::SOFT_BEIGE),
            (None, None, _) => (HINT.to_string(), t::TEXT),
        };
        let _ = cx;
        div()
            .flex()
            .flex_col()
            .gap(px(t::S2))
            .w_full()
            .max_w(px(728.))
            .mx_auto()
            .px(px(t::S5))
            .pb(px(t::S4))
            .child(
                div()
                    .debug_selector(|| "wk-composer".to_string())
                    .flex()
                    .items_center()
                    .gap(px(t::S2))
                    .p(px(t::S2))
                    .rounded(px(t::R_FLOAT))
                    .bg(hsla(t::ELEVATED))
                    .when(off.is_some(), |d| d.opacity(0.6))
                    .child(kit::composer_chip("wk-chip", chip))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Textarea::new(&self.composer)
                                .appearance(false)
                                .bordered(false),
                        ),
                    ),
            )
            .child(
                kit::text(t::T2, t::W_REGULAR, hint.1)
                    .debug_selector(|| "wk-hint".to_string())
                    .whitespace_normal()
                    .px(px(t::S2))
                    .child(hint.0),
            )
            .into_any_element()
    }
}

impl Focusable for WorkerFrame {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for WorkerFrame {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, gate) = match self.body(cx) {
            Body::Loading => (
                vec![Self::note("Loading the work item...")],
                Gate::Closed("Loading the work item...".into()),
            ),
            Body::Gone(why) => (vec![Self::note(&why)], Gate::Closed(why)),
            Body::Ready(v) => (self.body_rows(&v, cx), v.gate),
        };
        // the box takes text only while the gate is open and nothing is in flight (a no-op when unchanged)
        let off = !matches!(gate, Gate::Open { .. }) || self.pending.is_some();
        self.composer.update(cx, |c, cx| c.set_disabled(off, cx));
        div()
            .id("sa-worker")
            .debug_selector(|| "sa-worker".to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let (k, m) = (&ev.keystroke, &ev.keystroke.modifiers);
                let plain = !(m.control || m.alt || m.platform || m.shift);
                if k.key == "escape" && plain {
                    cx.emit(WorkerEvent::Blur);
                    cx.stop_propagation();
                } else if k.key == "enter"
                    && plain
                    && this.composer.read(cx).focus_handle(cx).is_focused(window)
                {
                    // the box's own Enter has run (it raised `PressEnter`): stop here, so no newline is typed after
                    // it and the text is exactly what the person wrote
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("wk-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(728.))
                            .mx_auto()
                            .px(px(t::S5))
                            .py(px(t::S6))
                            .flex()
                            .flex_col()
                            .gap(px(t::S5))
                            .children(rows),
                    ),
            )
            .child(self.composer_el(&gate, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::outcomes::sample;
    use serde_json::json;

    /// The sample with the engine advertising `attempt_message`.
    fn st() -> State {
        let mut s = sample::state();
        s.attempt_message = true;
        s
    }

    fn view(st: &State, item: &str, windows: bool) -> View {
        match build(st, "o_auth", item, windows) {
            Body::Ready(v) => *v,
            other => panic!("not ready: {other:?}"),
        }
    }

    #[test]
    fn a_running_attempt_makes_the_chip_live_and_no_attempt_makes_it_queued() {
        let s = st();
        assert_eq!(
            view(&s, "wi3", false).gate,
            Gate::Open {
                chip: "w3 \u{b7} attempt at4 (live)".into()
            }
        );
        assert_eq!(
            view(&s, "wi4", false).gate,
            Gate::Open {
                chip: "w4 \u{b7} next attempt (queued)".into()
            }
        );
        // waiting for a decision between attempts: queued, not live
        assert_eq!(
            view(&s, "wi5", false).gate,
            Gate::Open {
                chip: "w5 \u{b7} next attempt (queued)".into()
            }
        );
    }

    #[test]
    fn the_composer_is_off_with_a_reason_on_windows_without_the_feature_when_finished_and_when_the_engine_is_down()
     {
        let s = st();
        assert_eq!(view(&s, "wi3", true).gate, Gate::Closed(WINDOWS_WHY.into()));
        let mut off = st();
        off.attempt_message = false;
        assert!(
            matches!(view(&off, "wi3", false).gate, Gate::Closed(w) if w.contains("does not accept worker messages"))
        );
        assert_eq!(
            view(&s, "wi1", false).gate,
            Gate::Closed(FINISHED_WHY.into())
        );
        let mut down = st();
        down.screen = Screen::Unreachable("gone".into());
        assert!(
            matches!(view(&down, "wi3", false).gate, Gate::Closed(w) if w.contains("not running"))
        );
        let mut done = st();
        done.items[0].row.state = "completed".into();
        assert_eq!(
            view(&done, "wi3", false).gate,
            Gate::Closed(FINISHED_WHY.into())
        );
    }

    #[test]
    fn messages_of_this_item_only_each_with_its_delivery_label_and_masked_text() {
        let mut s = st();
        let secret = format!("{}={}", "API_KEY", "x".repeat(12));
        let row = |seq, state, attempt: Option<&str>, item: Option<&str>, text: &str| MessageRow {
            seq,
            state,
            attempt: attempt.map(Into::into),
            work_item: item.map(Into::into),
            text: text.into(),
        };
        use synthesis_view::outcomes::MessageState::*;
        s.detail.as_mut().unwrap().messages = vec![
            row(5, Delivered, Some("at4"), None, &format!("use {secret}")),
            row(6, Queued, None, Some("wi4"), "for w4"),
            row(7, Queued, None, Some("wi3"), "for w3 later"),
            row(8, Live, Some("at1"), None, "to w1"),
        ];
        let v = view(&s, "wi3", false);
        let labels: Vec<_> = v.messages.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["delivered to at4", "queued for the next attempt"]);
        assert!(
            v.messages[0].text.contains("[redacted]"),
            "{:?}",
            v.messages[0]
        );
        assert!(!v.messages[0].text.contains(&"x".repeat(12)));
        assert_eq!(view(&s, "wi4", false).messages.len(), 1);
    }

    #[test]
    fn the_event_tail_names_this_item_and_is_masked_and_cut() {
        let mut s = st();
        let secret = format!("{}={}", "TOKEN", "y".repeat(12));
        let evs: Vec<_> = (1..=60)
            .map(|i| json!({"seq": i, "type": "attempt.note", "payload": {"attempt": "at4", "n": i, "note": format!("{secret} {}", "z".repeat(400))}}))
            .chain([json!({"seq": 61, "type": "attempt.note", "payload": {"attempt": "at1"}})])
            .collect();
        s.detail.as_mut().unwrap().history = InspectorContent::activity(&evs, 61);
        let v = view(&s, "wi3", false);
        assert_eq!(v.log.len(), LOG_LINES, "the newest ones");
        assert!(v.log.iter().all(|l| l.chars().count() <= LOG_CHARS));
        assert!(v.log.iter().all(|l| !l.contains(&"y".repeat(12))));
        assert!(v.log.last().unwrap().starts_with("60 "));
        assert!(view(&s, "wi5", false).log.is_empty());
    }

    #[test]
    fn loading_and_a_missing_item_are_said_not_drawn_empty() {
        let s = st();
        assert_eq!(build(&s, "o_ci", "wi1", false), Body::Loading);
        assert!(
            matches!(build(&s, "o_auth", "nope", false), Body::Gone(m) if m.contains("not in the plan"))
        );
        assert!(matches!(build(&s, "zzz", "wi1", false), Body::Gone(_)));
    }

    #[test]
    fn delivery_words_say_live_or_queued() {
        assert!(
            delivered_words(&Delivery::Live(Some("at4".into())))
                .starts_with("Sent to attempt at4 (live)")
        );
        assert!(delivered_words(&Delivery::Queued).starts_with("Queued for the next attempt"));
    }

    // ------------------------------------------------------------------ the composer, with a real window

    use crate::superagent::outcomes::link::Req;
    use gpui::{TestAppContext, VisualTestContext, px, size};
    use std::sync::mpsc::Receiver;

    struct Rig {
        outcomes: Entity<OutcomesView>,
        frame: Entity<WorkerFrame>,
        rx: Receiver<Req>,
    }

    fn mount(cx: &mut TestAppContext, state: State) -> (Rig, &mut VisualTestContext) {
        mount_item(cx, state, "wi3")
    }

    fn mount_item<'a>(
        cx: &'a mut TestAppContext,
        state: State,
        item: &str,
    ) -> (Rig, &'a mut VisualTestContext) {
        cx.update(gpui_component::init);
        let (link, rx) = crate::superagent::outcomes::link::Link::fake();
        let outcomes = cx.new(|cx| {
            let mut o = OutcomesView::with_state(state, cx);
            o.set_link(link);
            o
        });
        let (o2, item) = (outcomes.clone(), item.to_string());
        let (frame, v) = cx.add_window_view(|window, cx| {
            WorkerFrame::new(o2, "o_auth".into(), item, "", window, cx)
        });
        v.simulate_resize(size(px(900.), px(900.)));
        v.run_until_parked();
        (
            Rig {
                outcomes,
                frame,
                rx,
            },
            v,
        )
    }

    fn type_text(r: &Rig, v: &mut VisualTestContext, text: &str) {
        let f = r.frame.read_with(v, |f, cx| f.focus_target(cx));
        let c = r.frame.read_with(v, |f, _| f.composer.clone());
        v.update(|w, cx| {
            f.focus(w, cx);
            c.update(cx, |c, cx| c.set_value(text.to_string(), w, cx));
        });
        v.run_until_parked();
        r.frame.read_with(v, |f, _| f.arm_until.set(0));
    }

    fn text_of(r: &Rig, v: &mut VisualTestContext) -> String {
        r.frame.read_with(v, |f, cx| f.draft(cx))
    }

    fn reqs(r: &Rig) -> Vec<(String, String, String, String)> {
        r.rx.try_iter()
            .map(|q| match q {
                Req::Message {
                    outcome,
                    item,
                    text,
                    key,
                } => (outcome, item, text, key),
                _ => panic!("another request"),
            })
            .collect()
    }

    fn answer(r: &Rig, v: &mut VisualTestContext, key: &str, result: Result<Delivery, String>) {
        let key = key.to_string();
        r.outcomes
            .update(v, |_, cx| cx.emit(MessageSent { key, result }));
        v.run_until_parked();
    }

    fn result(r: &Rig, v: &mut VisualTestContext) -> Option<Result<String, String>> {
        r.frame.read_with(v, |f, _| f.result.clone())
    }

    #[gpui::test]
    fn enter_in_the_focused_composer_sends_the_exact_text_once_and_a_second_enter_waits(
        cx: &mut TestAppContext,
    ) {
        let (r, v) = mount(cx, st());
        let text = "  use the REST shape\nkeep the old API  ";
        type_text(&r, v, text);
        assert!(reqs(&r).is_empty(), "typing and set_value send nothing");
        v.simulate_keystrokes("enter");
        v.run_until_parked();
        let sent = reqs(&r);
        assert_eq!(sent.len(), 1);
        let (o, i, t, key) = sent[0].clone();
        assert_eq!(
            (o.as_str(), i.as_str(), t.as_str()),
            ("o_auth", "wi3", text)
        );
        // the answer has not come: Enter again sends nothing, the text stays
        v.simulate_keystrokes("enter");
        v.run_until_parked();
        assert!(reqs(&r).is_empty(), "double submit");
        assert_eq!(text_of(&r, v), text);
        // delivered live: the box empties and the words say where it went
        answer(&r, v, &key, Ok(Delivery::Live(Some("at4".into()))));
        assert_eq!(text_of(&r, v), "");
        assert!(
            matches!(result(&r, v), Some(Ok(w)) if w.starts_with("Sent to attempt at4 (live)"))
        );
        // the next message is a new command with its own key
        type_text(&r, v, "second");
        v.simulate_keystrokes("enter");
        let again = reqs(&r);
        assert_eq!(again.len(), 1);
        assert_ne!(again[0].3, key);
    }

    #[gpui::test]
    fn an_enter_that_arrives_with_the_frame_sends_nothing(cx: &mut TestAppContext) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "kept draft");
        // the frame just came forward (a held Enter opened it): the guard time has not passed
        r.frame
            .read_with(v, |f, _| f.arm_until.set(now_ms() + ARM_MS));
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty());
        assert!(matches!(result(&r, v), Some(Err(w)) if w.contains("Look at the message first")));
        assert_eq!(text_of(&r, v), "kept draft");
        r.frame.read_with(v, |f, _| f.arm_until.set(0));
        v.simulate_keystrokes("enter");
        assert_eq!(reqs(&r).len(), 1, "one Enter after the guard sends it");
    }

    #[gpui::test]
    fn a_resend_after_a_lost_link_or_no_confirmation_reuses_the_key_until_the_owner_answers(
        cx: &mut TestAppContext,
    ) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "hello");
        v.simulate_keystrokes("enter");
        let k1 = reqs(&r).remove(0).3;
        // not confirmed: the same text goes out again with the same key
        answer(&r, v, &k1, Err(format!("{UNSETTLED} yet: x")));
        v.simulate_keystrokes("enter");
        assert_eq!(reqs(&r).remove(0).3, k1, "same text, same key");
        // an edit is another message
        answer(&r, v, &k1, Err(format!("{UNSETTLED} yet: x")));
        type_text(&r, v, "hello there");
        v.simulate_keystrokes("enter");
        let k2 = reqs(&r).remove(0).3;
        assert_ne!(k2, k1);
        // a lost link keeps the key too
        r.outcomes.update(v, |o, cx| {
            o.state.screen = Screen::Unreachable("closed".into());
            cx.notify();
        });
        v.run_until_parked();
        r.outcomes.update(v, |o, cx| {
            o.state.screen = Screen::Ready;
            cx.notify();
        });
        v.run_until_parked();
        v.simulate_keystrokes("enter");
        assert_eq!(reqs(&r).remove(0).3, k2, "after a lost link");
        // a refusal ends it: the next send is a new command
        answer(
            &r,
            v,
            &k2,
            Err("at most 20 messages to one outcome in an hour".into()),
        );
        v.simulate_keystrokes("enter");
        assert_ne!(reqs(&r).remove(0).3, k2);
    }

    #[gpui::test]
    fn on_native_windows_the_composer_is_closed_with_the_wsl_words_and_sends_nothing(
        cx: &mut TestAppContext,
    ) {
        FORCE_WINDOWS.with(|c| c.set(true));
        let (r, v) = mount(cx, st());
        type_text(&r, v, "hello");
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty());
        let g = r.frame.read_with(v, |f, cx| f.gate(cx));
        assert_eq!(g, Gate::Closed(WINDOWS_WHY.into()));
        FORCE_WINDOWS.with(|c| c.set(false));
    }

    #[gpui::test]
    fn queued_and_denied_are_told_and_a_refusal_keeps_the_text(cx: &mut TestAppContext) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "hold on");
        v.simulate_keystrokes("enter");
        let key = reqs(&r).remove(0).3;
        answer(&r, v, &key, Ok(Delivery::Queued));
        assert!(
            matches!(result(&r, v), Some(Ok(w)) if w.starts_with("Queued for the next attempt"))
        );
        type_text(&r, v, "again");
        v.simulate_keystrokes("enter");
        let key = reqs(&r).remove(0).3;
        let why = "at most 5 messages to one attempt in 10 minutes; wait, or the worker is flooded";
        answer(&r, v, &key, Err(why.into()));
        assert_eq!(result(&r, v), Some(Err(format!("Not sent: {why}"))));
        assert_eq!(text_of(&r, v), "again", "a refusal keeps the text");
        // an answer for another key (an older message) changes nothing
        answer(&r, v, "someone-elses", Ok(Delivery::Queued));
        assert_eq!(text_of(&r, v), "again");
    }

    #[gpui::test]
    fn hidden_characters_and_more_than_4_kib_are_refused_in_words_before_anything_is_sent(
        cx: &mut TestAppContext,
    ) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "a\u{202e}b");
        v.simulate_keystrokes("enter");
        assert!(matches!(result(&r, v), Some(Err(w)) if w.contains("hidden, bidi or control")));
        type_text(&r, v, &"x".repeat(4097));
        v.simulate_keystrokes("enter");
        assert!(matches!(result(&r, v), Some(Err(w)) if w.contains("longer than 4096")));
        type_text(&r, v, "   ");
        v.simulate_keystrokes("enter");
        assert!(matches!(result(&r, v), Some(Err(w)) if w.contains("empty")));
        assert!(reqs(&r).is_empty());
    }

    #[gpui::test]
    fn enter_outside_the_composer_sends_nothing_and_shift_enter_is_a_new_line(
        cx: &mut TestAppContext,
    ) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "draft");
        // the keys are on the frame, not in the box
        let h = r.frame.read_with(v, |f, _| f.focus.clone());
        v.update(|w, cx| h.focus(w, cx));
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty(), "Enter outside the composer");
        type_text(&r, v, "draft");
        v.simulate_keystrokes("shift-enter");
        assert!(reqs(&r).is_empty(), "Shift+Enter is a new line");
        assert!(text_of(&r, v).contains('\n'), "a line was added");
    }

    #[gpui::test]
    fn a_composer_that_is_off_sends_nothing_and_says_why(cx: &mut TestAppContext) {
        let mut off = st();
        off.attempt_message = false;
        let (r, v) = mount(cx, off);
        type_text(&r, v, "hello");
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty());
        let g = r.frame.read_with(v, |f, cx| f.gate(cx));
        assert!(matches!(g, Gate::Closed(w) if w.contains("does not accept worker messages")));
    }

    #[gpui::test]
    fn a_finished_item_takes_no_message(cx: &mut TestAppContext) {
        let (r, v) = mount_item(cx, st(), "wi1");
        type_text(&r, v, "hello");
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty());
        let g = r.frame.read_with(v, |f, cx| f.gate(cx));
        assert_eq!(g, Gate::Closed(FINISHED_WHY.into()), "said in the hint");
    }

    #[gpui::test]
    fn a_link_lost_with_a_message_in_flight_says_it_cannot_tell_and_blocks_a_resend(
        cx: &mut TestAppContext,
    ) {
        let (r, v) = mount(cx, st());
        type_text(&r, v, "hello");
        v.simulate_keystrokes("enter");
        assert_eq!(reqs(&r).len(), 1);
        r.outcomes.update(v, |o, cx| {
            o.state.screen = Screen::Unreachable("closed".into());
            cx.notify();
        });
        v.run_until_parked();
        assert_eq!(result(&r, v), Some(Err(LOST_LINK.into())));
        v.simulate_keystrokes("enter");
        assert!(reqs(&r).is_empty(), "the engine is not running");
        assert_eq!(text_of(&r, v), "hello");
    }

    /// `send_message` has one caller: this file's composer. Anything else in the app that could reach the link with a
    /// worker message would be a path that is not the person's Enter.
    #[test]
    fn only_the_worker_composer_calls_send_message() {
        fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        for d in ["src", "tests", "examples"] {
            walk(&root.join(d), &mut files);
        }
        let mut callers = vec![];
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let n = code.matches("send_message(").count()
                + code.matches("send_worker_message(").count()
                + code.matches("send_worker_card_message(").count()
                + code.matches("Req::Message").count();
            if n > 0 {
                callers.push(
                    f.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        callers.sort();
        // the definition and its request type (outcomes_view), the worker that serves it, this composer
        assert_eq!(
            callers,
            [
                "src/superagent/chat/glue.rs",
                "src/superagent/outcomes/link.rs",
                "src/superagent/outcomes/link/tests.rs",
                "src/superagent/outcomes/mod.rs",
                "src/superagent/session/worker.rs",
            ]
        );
        let w = std::fs::read_to_string(root.join("src/superagent/session/worker.rs")).unwrap();
        assert!(w.contains("v.send_message(&o, &i, text, key, cx)"));
        // the conductor card path: one call, inside the clicked-card effects
        let g = std::fs::read_to_string(root.join("src/superagent/chat/glue.rs")).unwrap();
        assert_eq!(g.matches("send_worker_card_message(").count(), 1);
        assert!(g.find("send_worker_card_message(") > g.find("impl Effects for DesktopEffects"));
    }
}
