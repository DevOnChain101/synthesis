//! What waits for the person in the Super Agent view (`docs/specs/super-agent.md` section 5, moved here from the TUI
//! view in S0): the outcome decision card docked at the bottom of the centre, the list of what waits (outcome
//! decisions and sessions waiting for a tool permission, oldest first: Alt+A walks it, the title bar's badge counts it).
//! The data is this view's own (`OutcomesView::state`), a decision is answered through the same worker
//! (`OutcomesView::send_effect`), and nothing here depends on an orchestrator crate.
//!
//! The card is shown below the centre tabs while the Decisions tab is not open (that tab draws the same card in full);
//! it opens by itself and never takes the keys: the view keeps them, so plain keys act as without a card. Alt+A (or
//! a click on the card) gives the card the keys, Esc or Alt+A gives them back, and a second Esc soon after, or
//! "Later", dismisses it. Any implicit event (another tab, another outcome, a click elsewhere, the window going to the
//! background) gives the keys back at once.

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;

use super::OutcomesView;
use super::model::{CentreTab, Screen};
use crate::approvals::{self, Ages, Kind, Out, Popup, Row};
use crate::live::now_ms;
use crate::theme::hsla;
use crate::widgets::chrome::{dim, icon, strong, text, with_alpha};

/// Alt+A reached a session that waits for a tool permission: the shell opens it in the TUI view (the answer
/// is given in the session itself, never here).
pub struct OpenSession(pub String);

impl gpui::EventEmitter<OpenSession> for OutcomesView {}

/// Which brief a card is: a new id, revision or brief hash is a new card (what "later" is remembered for).
type CardKey = (String, i64, String);

fn card_key(c: &synthesis_view::outcomes::DecisionCard) -> CardKey {
    (c.id.clone(), c.revision, c.brief_sha256.clone())
}

/// A second Esc this soon after the one that gave the keys back dismisses the card...
const DOUBLE_ESC_MS: i64 = 700;
/// ...unless it comes sooner than this: that is the first Esc auto-repeating (a held key), which dismisses nothing.
const REPEAT_MS: i64 = 150;

/// The clock of the shots and tests (ms): ages are fixed so the frames do not move.
pub const SAMPLE_NOW: i64 = 1_700_000_000_000;

/// What the card shows.
pub enum Shown {
    /// The outcome was asked for and its card has not come yet.
    Loading {
        outcome: String,
        goal: String,
    },
    Card(Box<Popup>, String),
}

/// The view's side of what waits for the person.
pub struct Wire {
    ages: Ages,
    tool_ages: Ages,
    /// `(key, title)` of the sessions waiting for a tool permission: the shell pushes them (`set_waiting_tools`).
    tools: Vec<(String, String)>,
    /// A fixed clock (the shots and tests); `None`: the real one.
    pub fixed_now: Option<i64>,
    pub shown: Option<Shown>,
    /// The key of the row Alt+A went to last.
    cursor: Option<String>,
    /// The card has the keys (Alt+A or a click): only then do plain keys act on it. This is the person's intent; the
    /// effective state also needs the card's own GPUI focus (`handle`), which any click elsewhere takes away.
    pub focus: bool,
    handle: FocusHandle,
    /// The centre tab and outcome of the last look: a change of either gives the keys back to the view.
    seen: (CentreTab, Option<String>),
    /// The card the person put off ("later"): it stays closed until a new card, revision or Alt+A.
    dismissed: Option<CardKey>,
    /// When Esc last gave the keys back (a second one soon after dismisses the card).
    esc_at: i64,
}

impl Wire {
    pub fn new(cx: &mut Context<OutcomesView>) -> Self {
        Self {
            ages: Ages::default(),
            tool_ages: Ages::default(),
            tools: vec![],
            fixed_now: None,
            shown: None,
            cursor: None,
            focus: false,
            handle: cx.focus_handle(),
            seen: (CentreTab::Decisions, None),
            dismissed: None,
            esc_at: i64::MIN / 2,
        }
    }
}

impl OutcomesView {
    /// The sample data of the shots: a fixed clock, and the outcome with the decision waiting since 9 minutes.
    pub fn sample_clock(&mut self) {
        self.pop.fixed_now = Some(SAMPLE_NOW);
        self.pop.ages.seed("o_auth", SAMPLE_NOW - 9 * 60_000);
    }

    fn now(&self) -> i64 {
        self.pop.fixed_now.unwrap_or_else(now_ms)
    }

    /// The sessions waiting for a tool permission now (pushed by the shell from the TUI view).
    pub fn set_waiting_tools(&mut self, tools: Vec<(String, String)>, cx: &mut Context<Self>) {
        if self.pop.tools != tools {
            self.pop.tools = tools;
            cx.notify();
        }
    }

    /// What waits for the person: sessions waiting for a tool permission and outcome decisions, the longest waiting
    /// first. Empty unless the link is up (while outcomes are off or unsupported nothing is counted).
    pub fn approval_rows(&self) -> Vec<Row> {
        if self.state.screen != Screen::Ready {
            return vec![];
        }
        let (w, now) = (&self.pop, self.now());
        let tools = w
            .tools
            .iter()
            .map(|(key, subject)| Row {
                since: w.tool_ages.since(key).unwrap_or(now),
                key: key.clone(),
                kind: Kind::Tool,
                subject: subject.clone(),
            })
            .collect();
        approvals::merge(
            tools,
            approvals::decision_rows(&self.state.items, &w.ages, now),
        )
    }

    /// The number on the Super Agent button of the title bar.
    pub fn waiting_count(&self) -> usize {
        self.approval_rows().len()
    }

    /// The card is on screen: there is one, the screen is the list, no form or disclosure is open, and the centre
    /// is not the Decisions tab (that tab draws the card itself).
    pub(super) fn popup_visible(&self) -> bool {
        self.pop.shown.is_some()
            && self.state.screen == Screen::Ready
            && self.state.form.is_none()
            && self.state.guide.is_none()
            && self.state.centre != CentreTab::Decisions
    }

    /// Whether the keys are the card's now: it was given them and still holds the window's focus.
    pub fn popup_has_keys(&self, window: &Window) -> bool {
        self.pop.focus && self.pop.handle.is_focused(window)
    }

    /// The whole brief is shown (what Tab does in the focused card), for the shots.
    pub fn expand_decision(&mut self, cx: &mut Context<Self>) {
        if let Some(Shown::Card(p, _)) = self.pop.shown.as_mut()
            && !p.full
        {
            p.toggle_full();
        }
        cx.notify();
    }

    /// The card takes the keys (what Alt+A does), for the shots and tests of the focused state.
    pub fn focus_decision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        self.focus_popup(now, window, cx);
    }

    /// Per frame, before drawing: first-seen ages of what waits, and the card follows the fresh data (a new revision
    /// or brief hash is a new card with a notice; a card no longer pending closes it).
    pub(super) fn sync_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_stale_focus(window, cx);
        self.sync_inner();
        // the card lost the keys (dismissed, other card, outcome gone) while it still holds GPUI focus, or is gone
        // with it: the view gets the window's focus back at once, so no key is lost on the way
        if self.visible && !self.pop.focus && self.pop.handle.is_focused(window) {
            window.focus(&self.focus, cx);
        }
    }

    /// The keys are the card's only while it holds the window's own focus and nothing implicit happened since: a
    /// click elsewhere, another centre tab, another outcome. Any of those gives the keys back to the view.
    fn drop_stale_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = (self.state.centre, self.state.selected.clone());
        let moved = at != self.pop.seen;
        self.pop.seen = at;
        if self.pop.focus && (moved || !self.pop.handle.is_focused(window)) {
            self.release_popup_focus(window, cx);
        }
    }

    /// The card gives the keys up (an implicit event: a click elsewhere, a tab or outcome switch, the window going
    /// to the background): the view has them again.
    pub(super) fn release_popup_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.pop.focus || self.pop.handle.is_focused(window);
        self.pop.focus = false;
        if held && self.visible {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The window was activated or deactivated: the keys go back to the view either way, so what was typed on the
    /// way back in is never read by the card.
    pub fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_popup_focus(window, cx);
    }

    fn sync_inner(&mut self) {
        let now = self.now();
        let (st, w) = (&self.state, &mut self.pop);
        let ready = st.screen == Screen::Ready;
        let pending: Vec<&str> = if ready {
            st.items
                .iter()
                .filter(|i| i.row.needs_you.decisions_pending > 0)
                .map(|i| i.row.id.as_str())
                .collect()
        } else {
            vec![]
        };
        w.ages.observe(pending, now);
        w.tool_ages
            .observe(w.tools.iter().map(|(k, _)| k.as_str()), now);
        let goal_of = |id: &str| {
            st.items
                .iter()
                .find(|i| i.row.id == id)
                .map(|i| i.row.goal.clone())
        };
        let card = st.cards().iter().find(|c| c.resolvable()).cloned();
        let Some(shown) = w.shown.take() else {
            // a decision of the selected outcome is pending and nothing else is up: the card opens by itself, WITHOUT
            // the keys (the view keeps them); a card the person put off stays off until it changes
            let sel = st
                .selected
                .clone()
                .filter(|s| st.detail.as_ref().is_some_and(|d| &d.outcome == s));
            if let (true, Some(o), Some(c)) = (ready, sel, card)
                && w.dismissed.as_ref() != Some(&card_key(&c))
                && let Some(goal) = goal_of(&o)
            {
                w.shown = Some(Shown::Card(Box::new(Popup::new(o, c, now)), goal));
            }
            w.focus = false;
            return;
        };
        let outcome = match &shown {
            Shown::Loading { outcome, .. } => outcome.clone(),
            Shown::Card(p, _) => p.outcome.clone(),
        };
        let Some(goal) = goal_of(&outcome).filter(|_| ready) else {
            // the outcome is gone, or the link ended: nothing to answer
            w.focus = false;
            return;
        };
        let landed = st.selected.as_deref() == Some(outcome.as_str())
            && st.detail.as_ref().is_some_and(|d| d.outcome == outcome);
        w.shown = match (shown, landed) {
            (Shown::Loading { .. }, false) => Some(Shown::Loading { outcome, goal }),
            // its detail landed: the card, or nothing when nothing is pending any more
            (Shown::Loading { .. }, true) => {
                card.map(|c| Shown::Card(Box::new(Popup::new(outcome, c, now)), goal))
            }
            // the selection moved to another outcome: its card is not this one (its own opens next frame)
            (Shown::Card(..), false) => None,
            (Shown::Card(mut p, _), true) => card.map(|c| {
                // the reply (`Msg::Answered`, counted by `State::answers`) came back: whatever it said, the choice
                // is gone and the card may answer again, only after a new choice
                // (a link that ended also clears the in-flight flag: that ends the wait too)
                if p.sent_at.is_some_and(|n| st.answers > n || !st.ask.sending) {
                    p.sent_at = None;
                    p.ask.answered(st.ask.notice.clone().unwrap_or_default());
                }
                if p.card.id != c.id {
                    // a DIFFERENT card is not what the person was about to answer: the keys go back to the view
                    // (only a new revision of the same card keeps them)
                    w.focus = false;
                    *p = Popup::new(outcome, c, now);
                } else {
                    p.update(c, now);
                }
                Shown::Card(p, goal)
            }),
        };
        // nothing shown any more (answered, or nothing pending): no focus either
        if w.shown.is_none() {
            w.focus = false;
        }
    }

    /// Alt+A: the next row (the oldest first, then around) is opened.
    pub fn next_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.approval_rows();
        let Some(i) = approvals::next(&rows, self.pop.cursor.as_deref()) else {
            return;
        };
        self.pop.cursor = Some(rows[i].key.clone());
        self.open_approval(&rows[i], window, cx);
    }

    /// A row was activated (Alt+A): a session that waits is shown by the shell (in the TUI view, where it is
    /// answered); a decision shows its card.
    fn open_approval(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        match row.kind {
            Kind::Tool => cx.emit(OpenSession(row.key.clone())),
            Kind::Decision => self.open_decision(&row.key, &row.subject, window, cx),
        }
        cx.notify();
    }

    /// The card of outcome `id`. When it is not the selected one, it is selected (and the card says so) and shown when
    /// its detail lands: nothing is selected silently.
    fn open_decision(&mut self, id: &str, goal: &str, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        let st = &self.state;
        let landed = st.selected.as_deref() == Some(id)
            && st.detail.as_ref().is_some_and(|d| d.outcome == id);
        let card = st.cards().iter().find(|c| c.resolvable()).cloned();
        // asked for on purpose (Alt+A): the card takes the keys, Enter inert for ARM_MS
        self.pop.dismissed = None;
        self.pop.shown = Some(match (landed, card) {
            (true, Some(c)) => Shown::Card(Box::new(Popup::new(id.into(), c, now)), goal.into()),
            _ => {
                self.select_id(id, cx);
                Shown::Loading {
                    outcome: id.to_string(),
                    goal: goal.into(),
                }
            }
        });
        // the Decisions tab shows the card in full itself, and it is the view that has the keys there
        if self.state.centre == CentreTab::Decisions {
            self.state.pane = super::model::Pane::Centre;
            window.focus(&self.focus, cx);
        } else {
            self.focus_popup(now, window, cx);
        }
    }

    /// The keys of this feature, before the view's own: Alt+A (next approval), and the card's while it has them.
    /// The form and the disclosure own the keyboard while open (the caller checks).
    pub(super) fn popup_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.state.screen != Screen::Ready {
            return false;
        }
        // the card's keys need its own GPUI focus: a click elsewhere (or anything else that moved the window's
        // focus) since Alt+A took them back already; a tab or outcome switch did too
        self.drop_stale_focus(window, cx);
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        if m.alt && !m.control && !m.platform && !m.shift && ks.key == "a" {
            // a held key (auto-repeat) must not flip the keys back and forth
            if !ev.is_held {
                self.alt_a(window, cx);
            }
            return true;
        }
        // Shift (Shift+Enter, Shift+digit) and every other chord belong to the view, never to the card
        if m.control || m.platform || m.alt || m.shift || !self.popup_visible() {
            return false;
        }
        let (key, now) = (ks.key.as_str(), self.now());
        if self.popup_has_keys(window) && matches!(self.pop.shown, Some(Shown::Card(..))) {
            // the card has the keys: Esc gives them back (the card stays), the rest is its own
            if key == "escape" {
                self.pop.esc_at = now;
                self.release_popup_focus(window, cx);
                cx.notify();
                return true;
            }
            let out = match self.pop.shown.as_mut() {
                Some(Shown::Card(p, _)) => p.key(key, now),
                _ => Out::Pass,
            };
            if out != Out::Pass {
                self.popup_out(out, window, cx);
                return true;
            }
            return false;
        }
        // the view has the keys: nothing goes to the card. The one thing taken is a second Esc soon after the one that
        // gave the keys back: "later". A repeat of the first Esc (a held key: under REPEAT_MS) is swallowed without
        // dismissing.
        if key == "escape" {
            let gap = now - self.pop.esc_at;
            if (0..REPEAT_MS).contains(&gap) {
                return true;
            }
            if gap < DOUBLE_ESC_MS {
                self.dismiss_popup(window, cx);
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Alt+A: with a card it moves the keys into it, or back to the view when it has them (the card stays);
    /// without one it goes to the next approval, which opens a decision's card with the keys.
    fn alt_a(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        if self.popup_visible() && matches!(self.pop.shown, Some(Shown::Card(..))) {
            if self.popup_has_keys(window) {
                self.release_popup_focus(window, cx);
            } else {
                self.focus_popup(now, window, cx);
            }
            cx.notify();
        } else {
            self.pop.dismissed = None;
            self.next_approval(window, cx);
        }
    }

    /// The card takes the keys: the window's focus moves to it (so the view no longer holds it) and Enter is inert for
    /// ARM_MS from now, so a key still held cannot answer.
    pub(super) fn focus_popup(&mut self, now: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.pop.focus = true;
        window.focus(&self.pop.handle, cx);
        if let Some(Shown::Card(p, _)) = self.pop.shown.as_mut() {
            p.rearm(now);
        }
    }

    /// "Later": the card closes and stays closed until a new card or revision, or Alt+A.
    fn dismiss_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Shown::Card(p, _)) = &self.pop.shown {
            self.pop.dismissed = Some(card_key(&p.card));
        }
        self.pop.shown = None;
        self.release_popup_focus(window, cx);
    }

    fn popup_out(&mut self, out: Out, window: &mut Window, cx: &mut Context<Self>) {
        if out == Out::Later {
            self.dismiss_popup(window, cx);
            cx.notify();
            return;
        }
        if let Out::Send(e) = out {
            let (busy, answers) = (self.state.ask.sending, self.state.answers);
            let mut send = false;
            match self.pop.shown.as_mut() {
                // the Decisions tab has an answer on its way: no second one from here
                Some(Shown::Card(p, _)) if busy => {
                    p.ask.sending = false;
                    p.ask.notice = Some("An answer is already on its way.".into());
                }
                Some(Shown::Card(p, _)) => {
                    p.sent_at = Some(answers);
                    send = true;
                }
                _ => {}
            }
            if send {
                self.send_effect(e, cx);
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------------------------- drawing

    /// Tallest the card may be: about 42% of the window collapsed, 60% expanded, and never less than the answers,
    /// buttons and hint need (the brief is the part that gives way).
    fn popup_max_h(&self, expanded: bool) -> f32 {
        let h = (self.win_h - crate::ui::TITLEBAR_H - 56.).max(300.);
        (h * if expanded { 0.6 } else { 0.42 }).max(if expanded { 420. } else { 380. })
    }

    /// The decision card docked under the centre tabs: a cream border (the Synthesis accent), not the amber of a tool
    /// permission. Full brief in the server's order, nothing preselected, two steps (choose, then Enter).
    pub(super) fn decision_popup(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.popup_visible() {
            return None;
        }
        let w = &self.pop;
        let shown = w.shown.as_ref()?;
        // the keys are in the popup (a bright border) or in the prompt (a dim one): never both, always visible
        let focused = w.focus && matches!(shown, Shown::Card(..));
        let card = div()
            .debug_selector(|| "decision".to_string())
            .id("decision")
            .min_h(px(0.))
            // collapsed: about 40% of the chat column; expanded (Tab): about 60%, the brief scrolls inside and the
            // answers and buttons stay pinned. Never less than what the pinned parts need.
            .max_h(px(
                self.popup_max_h(matches!(shown, Shown::Card(p, _) if p.full))
            ))
            .overflow_hidden()
            .mx(px(12.))
            .mb(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(12.))
            .py(px(8.))
            .bg(crate::theme::solid(t::SURFACE))
            .border_2()
            .border_color(if focused {
                hsla(t::ACCENT)
            } else {
                with_alpha(hsla(t::ACCENT), 0.7)
            })
            .rounded(px(10.))
            // the card holds the window's focus while it has the keys (the prompt does not then)
            .track_focus(&w.handle)
            // a press on the card gives it the keys (Enter inert for ARM_MS from then); a press anywhere outside it
            // gives them back to the prompt (the click that goes to type there)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let now = this.now();
                    this.focus_popup(now, window, cx);
                    cx.notify();
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.release_popup_focus(window, cx);
            }));
        let head = |goal: &str, sub: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(icon("shield", 14., hsla(t::ACCENT)))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(strong())
                                .truncate()
                                .child(format!("Decision: {goal}")),
                        ),
                )
                .child(div().text_color(dim()).child(sub))
        };
        Some(
            match shown {
                Shown::Loading { goal, .. } => card
                    .child(head(goal, "Loading the decision...".into()))
                    .child(div().text_color(text()).child(
                        "This outcome is now selected. The decision appears here when it arrives.",
                    )),
                Shown::Card(p, goal) => {
                    let c = &p.card;
                    let mut card = card.child(head(
                        goal,
                        format!(
                            "{} · revision {} · {}{}",
                            c.kind,
                            c.revision,
                            c.state,
                            if c.expires_at.is_empty() {
                                String::new()
                            } else {
                                format!(" · expires {}", c.expires_at)
                            }
                        ),
                    ));
                    // Collapsed (the default, also for a new card or revision): only the first field the server lists,
                    // in two lines at most, and a visible line saying what is not shown. Expanded (Tab, or a click on
                    // that line): every field in the server's order, scrolling inside its own box so the answers and
                    // buttons below stay in sight. Nothing is ever only reachable some other way.
                    let hidden = c.rows.len().saturating_sub(1);
                    let first_cut = p.first_cut();
                    let mut brief = div()
                        .id("decision-brief")
                        .debug_selector(|| "decision-brief".to_string())
                        // collapsed it is two lines and never squeezed; expanded it is the part that gives way
                        .flex_shrink(if p.full { 1. } else { 0. })
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(5.));
                    for (i, r) in
                        c.rows
                            .iter()
                            .enumerate()
                            .take(if p.full { usize::MAX } else { 1 })
                    {
                        brief = brief.child(
                            div()
                                .debug_selector(move || format!("decision-field-{i}"))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(dim())
                                        .child(r.label.clone()),
                                )
                                .child({
                                    let b = div().text_color(text()).child(r.text.clone());
                                    if p.full { b } else { b.line_clamp(2) }
                                }),
                        );
                    }
                    card = card.child(brief);
                    if hidden > 0 || first_cut || p.full {
                        let toggle = cx.listener(|this, _, window, cx| {
                            let now = this.now();
                            this.focus_popup(now, window, cx);
                            if let Some(Shown::Card(p, _)) = this.pop.shown.as_mut() {
                                p.toggle_full();
                            }
                            cx.notify();
                        });
                        card = card.child(
                            div()
                                .id("decision-more")
                                .debug_selector(|| "decision-more".to_string())
                                .flex_none()
                                .cursor_pointer()
                                .text_color(hsla(t::ACCENT_LIGHT))
                                .on_click(toggle)
                                .child(if p.full {
                                    "Tab: show less".to_string()
                                } else if hidden > 0 {
                                    format!(
                                        "{hidden} more section{} not shown: Tab for the full brief",
                                        if hidden == 1 { "" } else { "s" }
                                    )
                                } else {
                                    "Tab: full brief".to_string()
                                }),
                        );
                    }
                    match &p.ask.picker {
                        Some(pk) => {
                            for (i, o) in pk.options.iter().enumerate() {
                                let on = pk.cursor() == Some(i);
                                card = card.child(
                                    div()
                                        .id(("decision-opt", i))
                                        .debug_selector(move || format!("decision-opt-{i}"))
                                        .flex_none()
                                        .flex()
                                        .gap(px(8.))
                                        .px(px(8.))
                                        .py(px(0.))
                                        .rounded(px(6.))
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if on {
                                            hsla(t::ACCENT)
                                        } else {
                                            hsla(t::BORDER)
                                        })
                                        .when(on, |d| d.bg(hsla(t::ELEVATED)))
                                        .hover(|d| d.bg(hsla(t::ELEVATED)))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let now = this.now();
                                            this.focus_popup(now, window, cx);
                                            if let Some(Shown::Card(p, _)) = this.pop.shown.as_mut()
                                            {
                                                p.ask.choose(i);
                                            }
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .w(px(14.))
                                                .text_color(hsla(t::ACCENT))
                                                .child(if on { "›" } else { "" }),
                                        )
                                        .child(div().text_color(dim()).child(format!("{}.", i + 1)))
                                        .child(div().text_color(strong()).child(o.label.clone())),
                                );
                            }
                        }
                        None => {
                            card = card.child(div().text_color(hsla(t::WARN)).child(format!(
                                "This decision is {}: it cannot be answered here.",
                                c.state
                            )));
                        }
                    }
                    let send = cx.listener(|this, _, window, cx| {
                        let now = this.now();
                        let out = match this.pop.shown.as_mut() {
                            Some(Shown::Card(p, _)) => p.key("enter", now),
                            _ => Out::Pass,
                        };
                        this.popup_out(out, window, cx);
                    });
                    card.child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .id("decision-send")
                                    .debug_selector(|| "decision-send".to_string())
                                    .cursor_pointer()
                                    .px(px(10.))
                                    .py(px(3.))
                                    .rounded(px(6.))
                                    .border_1()
                                    // dim while the whole brief has not been shown (the line above the answers says why)
                                    .border_color(if p.send_blocked() {
                                        hsla(t::BORDER)
                                    } else {
                                        hsla(t::ACCENT)
                                    })
                                    .text_color(if p.send_blocked() { dim() } else { strong() })
                                    .on_click(send)
                                    .child(if p.send_blocked() {
                                        "Send answer (expand first)"
                                    } else {
                                        "Send answer"
                                    }),
                            )
                            .child(
                                div()
                                    .id("decision-later")
                                    .debug_selector(|| "decision-later".to_string())
                                    .cursor_pointer()
                                    .px(px(10.))
                                    .py(px(3.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(hsla(t::BORDER))
                                    .text_color(text())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dismiss_popup(window, cx);
                                        cx.notify();
                                    }))
                                    .child("Later"),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            // one line always: a longer hint when the card has the keys must not move the
                            // answers under the pointer mid-click
                            .truncate()
                            .debug_selector(|| "decision-hint".to_string())
                            .text_color(if focused {
                                hsla(t::ACCENT_LIGHT)
                            } else {
                                dim()
                            })
                            .child(if focused {
                                "1-9 or arrows choose · Enter sends · Tab brief · Esc back"
                            } else {
                                "Alt+A to answer"
                            }),
                    )
                    .children(
                        p.ask
                            .notice
                            .clone()
                            .map(|n| div().text_color(hsla(t::ACCENT_LIGHT)).child(n)),
                    )
                }
            }
            .into_any_element(),
        )
    }
}
