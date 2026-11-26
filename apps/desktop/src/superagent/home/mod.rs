//! The Super Agent home, the command centre (spec `super-agent-s1.md` 5.5, mock `super-agent-v3`): one scroll column
//! with no title. Needs you (rows, the decision card in place of a row on Review), Running, Finished, then a slot for
//! the Super-Agent conversation (phase B3 builds it, the lead wires it). Loading is skeleton rows, an error is a
//! banner with Retry (R), nothing at all is the three kinds of "What should we work on?".
//!
//! The data is `OutcomesView`'s (read through `data`), an answer goes through `OutcomesView::send_effect`. The
//! recommended one-tap, batch apply and Undo are gated off (`data::Gates::SHIPPED`): the row has the slot, nothing
//! fakes them until approvals-concept A1/A4 exist.

pub mod card;
pub mod data;
pub mod finished;
pub mod needs;
pub mod running;
#[cfg(feature = "shot")]
pub mod shot;
#[cfg(test)]
mod tests;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollHandle, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*,
    px,
};

use super::kit::{self, ButtonKind, Status};
use super::tokens::{self as t, hsla};
use crate::approvals::Popup;
use crate::live::ARM_MS;
use crate::superagent::outcomes::OutcomesView;
use crate::superagent::outcomes::kind_picker::{ROWS, refusal};
use crate::superagent::outcomes::model::Screen;
use crate::widgets::chrome::OnClick;
use data::{Gates, Model, Target};

/// A row was opened: the shell shows the frame.
pub enum HomeEvent {
    OpenOutcome(String),
    /// A session waits for a tool permission: it is answered in the session itself.
    OpenSession(String),
}

impl EventEmitter<HomeEvent> for HomeView {}

/// More waiting decisions than this are behind "Show N more" (not while a card is open).
const FIRST: usize = 5;

/// The decision opened in place of its row. The card exists once the outcome's detail has landed.
pub(crate) struct Open {
    pub outcome: String,
    /// The row's own decision; `None` (an older orchestrator): the outcome's first open one.
    pub decision: Option<String>,
    pub popup: Option<Popup>,
    /// When the card got the keys: they act 400 ms later (the engagement guard).
    pub since: i64,
}

pub struct HomeView {
    pub(crate) outcomes: Entity<OutcomesView>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    pub(crate) cursor: Option<usize>,
    pub(crate) open: Option<Open>,
    show_all: bool,
    /// The "Lapsed" and "Answered, recorded" sections are collapsed until asked for.
    show_lapsed: bool,
    show_answered: bool,
    /// The row Alt+A went to last.
    alt_a: Option<String>,
    gates: Gates,
    /// The Super-Agent conversation (phase B3); the lead sets it.
    conversation: Option<AnyView>,
}

/// What a plain key does on the home while no card is open (a pure map, so it is tested).
#[derive(Debug, PartialEq, Eq)]
pub enum Act {
    Down,
    Up,
    Open,
    Retry,
    Kind(usize),
}

pub fn act(key: &str, error: bool, empty: bool) -> Option<Act> {
    match key {
        "j" | "down" => Some(Act::Down),
        "k" | "up" => Some(Act::Up),
        "enter" => Some(Act::Open),
        "r" if error => Some(Act::Retry),
        "1" | "2" | "3" if empty => Some(Act::Kind(key.parse::<usize>().unwrap_or(1) - 1)),
        _ => None,
    }
}

impl HomeView {
    pub fn new(outcomes: Entity<OutcomesView>, cx: &mut Context<Self>) -> Self {
        cx.observe(&outcomes, |_, _, cx| cx.notify()).detach();
        Self {
            outcomes,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            cursor: None,
            open: None,
            show_all: false,
            show_lapsed: false,
            show_answered: false,
            alt_a: None,
            gates: Gates::SHIPPED,
            conversation: None,
        }
    }

    pub fn set_conversation(&mut self, c: Option<AnyView>, cx: &mut Context<Self>) {
        self.conversation = c;
        cx.notify();
    }

    /// The home was shown: the keys come here.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    pub(crate) fn now(&self, cx: &App) -> i64 {
        data::now(self.outcomes.read(cx))
    }

    pub(crate) fn model(&self, cx: &App) -> Model {
        data::model(self.outcomes.read(cx))
    }

    // ------------------------------------------------------------------------------------------ actions

    /// Review: the decision card opens in place of the row once its brief has been read; it has the keys from now.
    pub fn review(&mut self, key: &str, cx: &mut Context<Self>) {
        self.review_decision(key, None, cx);
    }

    fn review_decision(&mut self, key: &str, decision: Option<String>, cx: &mut Context<Self>) {
        let now = self.now(cx);
        self.open = Some(Open {
            outcome: key.into(),
            decision,
            popup: None,
            since: now,
        });
        self.outcomes.update(cx, |o, cx| data::review(o, key, cx));
        cx.notify();
    }

    /// Alt+A: the next waiting thing (the oldest first, then around). A decision opens inline, a session is shown.
    pub fn next_decision(&mut self, cx: &mut Context<Self>) {
        let m = self.model(cx);
        let keys: Vec<_> = m.needs.iter().map(|r| r.id().to_string()).collect();
        let i = match self
            .alt_a
            .as_ref()
            .and_then(|c| keys.iter().position(|k| k == c))
        {
            Some(i) => (i + 1) % keys.len(),
            None if keys.is_empty() => return,
            None => 0,
        };
        self.alt_a = Some(keys[i].clone());
        self.cursor = Some(i);
        self.activate(Target::Need(i), cx);
    }

    fn activate(&mut self, at: Target, cx: &mut Context<Self>) {
        let m = self.model(cx);
        match at {
            Target::Need(i) => match m.needs.get(i) {
                Some(r) if r.kind == crate::approvals::Kind::Tool => {
                    cx.emit(HomeEvent::OpenSession(r.key.clone()))
                }
                // a contract is read and approved in the outcome's own frame
                Some(r) if r.kind == crate::approvals::Kind::Contract => {
                    cx.emit(HomeEvent::OpenOutcome(r.key.clone()))
                }
                Some(r) => self.review_decision(&r.key, r.decision.clone(), cx),
                None => {}
            },
            Target::Run(i) => {
                if let Some(r) = m.running.get(i) {
                    cx.emit(HomeEvent::OpenOutcome(r.id.clone()))
                }
            }
            Target::Fin(i) => {
                if let Some(r) = m.finished.get(i) {
                    cx.emit(HomeEvent::OpenOutcome(r.id.clone()))
                }
            }
        }
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.outcomes.update(cx, data::retry);
    }

    /// An inline kind of the empty state: the kind picker opens with that row chosen (Enter starts it).
    fn kind(&mut self, i: usize, cx: &mut Context<Self>) {
        let st = &self.outcomes.read(cx).state;
        if refusal(ROWS[i], &st.screen, st.read_only_goal).is_some() {
            return;
        }
        self.outcomes.update(cx, |o, _| {
            o.state.open_picker();
            o.state.pick(i);
        });
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let n = self.model(cx).flat().len();
        if n == 0 {
            return;
        }
        self.cursor = Some(match (self.cursor, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(c), true) => (c + 1).min(n - 1),
            (Some(c), false) => c.saturating_sub(1),
        });
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ the open card

    /// Per frame: the card follows the fresh data. It is made when the outcome's detail has landed; it closes when
    /// the outcome is gone, the link is down, or nothing is pending any more (answered).
    fn sync(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let st = &self.outcomes.read(cx).state;
        if st.screen != Screen::Ready || !st.items.iter().any(|i| i.row.id == open.outcome) {
            self.open = None;
            return;
        }
        let landed = st.selected.as_deref() == Some(open.outcome.as_str())
            && st
                .detail
                .as_ref()
                .is_some_and(|d| d.outcome == open.outcome);
        if !landed {
            return;
        }
        let wanted = |id: &str| open.decision.as_deref().is_none_or(|d| d == id);
        let found = st.cards().iter().find(|c| c.resolvable() && wanted(&c.id));
        let Some(c) = found.cloned() else {
            self.open = None;
            return;
        };
        open.popup = Some(match open.popup.take() {
            None => {
                open.since = now;
                Popup::new(open.outcome.clone(), c, now)
            }
            Some(mut p) => {
                // the reply came back (or the link ended): whatever it said, the choice is gone
                if p.sent_at.is_some_and(|n| st.answers > n || !st.ask.sending) {
                    p.sent_at = None;
                    p.ask.answered(st.ask.notice.clone().unwrap_or_default());
                }
                if p.card.id == c.id {
                    p.update(c, now);
                    p
                } else {
                    open.since = now;
                    Popup::new(open.outcome.clone(), c, now)
                }
            }
        });
    }

    /// Enter and the Send button: the answer, when the popup's rules allow it.
    pub(crate) fn send(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let answers = self.outcomes.read(cx).state.answers;
        let Some(p) = self.open.as_mut().and_then(|o| o.popup.as_mut()) else {
            return;
        };
        if let crate::approvals::Out::Send(e) = p.key("enter", now) {
            p.sent_at = Some(answers);
            self.outcomes.update(cx, |o, cx| data::answer(o, e, cx));
        }
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ keys

    /// Alt+A, J/K, Enter, Esc, R and the digits. Plain keys act on the card while one is open (after its guard).
    pub fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let (k, m) = (&ev.keystroke, &ev.keystroke.modifiers);
        let key = k.key.as_str();
        if self.outcomes.read(cx).state.picker.is_some() {
            return false;
        }
        if m.alt && !m.control && !m.platform && !m.shift && key == "a" {
            self.next_decision(cx);
            return true;
        }
        if m.control || m.alt || m.platform || m.shift {
            return false;
        }
        if self.open.is_some() {
            return self.card_key(key, ev.is_held, cx);
        }
        let model = self.model(cx);
        match act(key, model.retry, model.is_empty()) {
            Some(Act::Down) => self.step(true, cx),
            Some(Act::Up) => self.step(false, cx),
            Some(Act::Open) if !ev.is_held => {
                if let Some(at) = self.cursor.and_then(|c| model.flat().get(c).copied()) {
                    self.activate(at, cx);
                }
            }
            Some(Act::Retry) => self.retry(cx),
            Some(Act::Kind(i)) => self.kind(i, cx),
            Some(Act::Open) | None => return false,
        }
        true
    }

    fn card_key(&mut self, key: &str, held: bool, cx: &mut Context<Self>) -> bool {
        let now = self.now(cx);
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if key == "escape" {
            self.open = None;
            cx.notify();
            return true;
        }
        // the engagement guard: a key typed ahead of the card (or still held) answers nothing
        if now < open.since + ARM_MS || (held && key == "enter") {
            return true;
        }
        let Some(p) = open.popup.as_mut() else {
            return true;
        };
        if key == "enter" {
            self.send(cx);
            return true;
        }
        let handled = !matches!(p.key(key, now), crate::approvals::Out::Pass);
        cx.notify();
        handled
    }

    // ------------------------------------------------------------------------------------------ drawing

    fn banner(&self, m: &Model, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = m.banner.clone()?;
        Some(
            div()
                .debug_selector(|| "home-banner".to_string())
                .flex()
                .items_center()
                .gap(px(t::S3))
                .px(px(t::S5))
                .py(px(t::S2))
                .bg(hsla(t::ELEVATED))
                .border_b_1()
                .border_color(t::stroke())
                .child(
                    kit::text(t::T4, t::W_REGULAR, Status::Failed.colour())
                        .child(Status::Failed.glyph_and_word().0),
                )
                .child(
                    kit::text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .child(text),
                )
                .children(m.retry.then(|| {
                    kit::button(
                        "home-retry",
                        "Retry R",
                        ButtonKind::Secondary,
                        false,
                        Some(Box::new(cx.listener(|v, _, _, cx| v.retry(cx)))),
                    )
                }))
                .into_any_element(),
        )
    }

    fn skeleton(&self) -> AnyElement {
        let bar = |w: f32, h: f32| {
            div()
                .h(px(h))
                .w(px(w))
                .rounded(px(t::R_PILL))
                .bg(hsla(t::ELEVATED))
        };
        let sec = || {
            div()
                .flex()
                .flex_col()
                .gap(px(t::S1))
                .child(bar(96., 16.).mx(px(t::S4)).mb(px(t::S2)))
                .children((0..2).map(|_| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(t::S4))
                        .px(px(t::S4))
                        .py(px(t::S2))
                        .min_h(px(52.))
                        .child(bar(16., 16.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(t::S2))
                                .child(bar(240., 12.))
                                .child(bar(150., 10.)),
                        )
                }))
        };
        div()
            .debug_selector(|| "home-skeleton".to_string())
            .flex()
            .flex_col()
            .gap(px(t::S5))
            .child(sec())
            .child(sec())
            .into_any_element()
    }

    /// "What should we work on?" and the three kinds, nothing preselected.
    fn invite(&self, cx: &mut Context<Self>) -> AnyElement {
        let st = &self.outcomes.read(cx).state;
        let rows: Vec<_> = ROWS
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let off = refusal(*k, &st.screen, st.read_only_goal);
                let mut r = div()
                    .id(("home-kind", i))
                    .debug_selector(move || format!("home-kind-{i}"))
                    .flex()
                    .items_center()
                    .gap(px(t::S4))
                    .px(px(t::S4))
                    .py(px(t::S3))
                    .rounded(px(t::R_CONTROL))
                    .child(
                        div()
                            .w(px(24.))
                            .flex()
                            .flex_none()
                            .child(kit::keycap("home-kind-key", (i + 1).to_string())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                kit::text(
                                    t::T3,
                                    t::W_MEDIUM,
                                    if off.is_some() {
                                        t::TEXT
                                    } else {
                                        t::TEXT_STRONG
                                    },
                                )
                                .child(k.label()),
                            )
                            .child(
                                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                                    .whitespace_normal()
                                    .child(off.unwrap_or(k.blurb())),
                            ),
                    );
                if off.is_none() {
                    r = r
                        .cursor_pointer()
                        .hover(|d| d.bg(hsla(t::SURFACE)))
                        .on_click(cx.listener(move |v, _, _, cx| v.kind(i, cx)));
                }
                r
            })
            .collect();
        div()
            .debug_selector(|| "home-empty".to_string())
            .w_full()
            .max_w(px(480.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .child(kit::text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG).child("What should we work on?"))
            .child(
                kit::text(t::T3, t::W_REGULAR, t::TEXT)
                    .whitespace_normal()
                    .pb(px(t::S4))
                    .child("Start with a chat, or give Super-Agent a goal. Anything that needs a decision will wait for you on the home screen."),
            )
            .children(rows)
            .into_any_element()
    }
}

impl Focusable for HomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A section: the header (title, count) over its rows.
pub(in crate::superagent) fn section(
    id: &'static str,
    title: &'static str,
    count: usize,
    rows: Vec<AnyElement>,
) -> Div {
    div()
        .debug_selector(move || format!("{id}-sec"))
        .flex()
        .flex_col()
        .gap(px(t::S1))
        .child(kit::section_header(id, title, Some(count)))
        .children(rows)
}

/// What one list row shows: the glyph, the title, plain meta parts then beige notes, trailing controls.
pub(in crate::superagent) struct RowSpec {
    pub status: Status,
    pub title: String,
    pub meta: Vec<String>,
    pub notes: Vec<String>,
    pub trailing: Vec<AnyElement>,
    pub current: bool,
}

/// min h 52, p 8 x 16, r 8: glyph, title 13/500 over meta 12, trailing. `on_click` is the title area's (the buttons
/// have their own), so a click never fires two handlers.
pub(in crate::superagent) fn row(
    id: (&'static str, usize),
    s: RowSpec,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let (name, i) = id;
    let mut main = div()
        .id((name, i + 1000))
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(
            kit::text(t::T3, t::W_MEDIUM, t::TEXT_STRONG)
                .truncate()
                .child(s.title),
        )
        .child(
            div()
                .flex()
                .gap_x(px(t::S4))
                .overflow_hidden()
                .children(
                    s.meta
                        .into_iter()
                        .map(|m| kit::text(t::T2, t::W_REGULAR, t::TEXT).child(m)),
                )
                .children(
                    s.notes
                        .into_iter()
                        .map(|m| kit::text(t::T2, t::W_REGULAR, t::SOFT_BEIGE).child(m)),
                ),
        );
    if let Some(f) = on_click {
        main = main
            .cursor_pointer()
            .on_click(move |e: &ClickEvent, w, cx| f(e, w, cx));
    }
    div()
        .id(id)
        .debug_selector(move || format!("{name}-{i}"))
        .flex()
        .items_center()
        .gap(px(t::S4))
        .min_h(px(52.))
        .px(px(t::S4))
        .py(px(t::S2))
        .rounded(px(t::R_CONTROL))
        .when(s.current, |d| d.bg(hsla(t::SURFACE)))
        .hover(|d| d.bg(hsla(t::SURFACE)))
        .child(
            kit::text(t::T4, t::W_REGULAR, s.status.colour())
                .size(px(16.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .child(s.status.glyph_and_word().0),
        )
        .child(main)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(t::S2))
                .children(s.trailing),
        )
}

/// The conversation slot: B3 builds the chat, the lead hands it in. The home body ends with it.
pub fn render(
    home: &mut HomeView,
    conversation: Option<AnyElement>,
    window: &mut Window,
    cx: &mut Context<HomeView>,
) -> AnyElement {
    let _ = window;
    home.sync(cx);
    let m = home.model(cx);
    let centred = m.is_empty();
    let (conversation_centred, conversation) = if centred {
        (conversation, None)
    } else {
        (None, conversation)
    };
    let body: Vec<AnyElement> = if m.loading {
        vec![home.skeleton()]
    } else if centred {
        vec![]
    } else {
        vec![home.needs(&m, cx)]
            .into_iter()
            .chain(home.logs(&m, cx))
            .chain([home.running(&m, cx), home.finished(&m, cx)])
            .collect()
    };
    div()
        .id("sa-home")
        .debug_selector(|| "sa-home".to_string())
        .track_focus(&home.focus)
        .on_key_down(cx.listener(|v, ev: &KeyDownEvent, _, cx| {
            if v.on_key(ev, cx) {
                cx.stop_propagation();
            }
        }))
        .size_full()
        .flex()
        .flex_col()
        .children(home.banner(&m, cx))
        // nothing at all: the invitation sits in the middle, a little above it (the mock's `.col.mid`)
        .children(centred.then(|| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(t::S5))
                .pb(px(96.))
                .child(home.invite(cx))
        }))
        // the chat is there from the first launch too: with nothing yet, it sits under the invitation
        .children(centred.then(|| {
            div()
                .debug_selector(|| "home-conversation".to_string())
                .w_full()
                .max_w(px(728.))
                .mx_auto()
                .px(px(t::S5))
                .pb(px(t::S6))
                .children(conversation_centred)
        }))
        .children((!centred).then(|| {
            div()
                .id("home-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&home.scroll)
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
                        .children(body)
                        // the conversation slot (phase B3): the orchestrator chat with its proposal cards
                        .child(
                            div()
                                .debug_selector(|| "home-conversation".to_string())
                                .children(conversation),
                        ),
                )
        }))
        .into_any_element()
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = self.conversation.clone().map(IntoElement::into_any_element);
        render(self, c, window, cx)
    }
}
