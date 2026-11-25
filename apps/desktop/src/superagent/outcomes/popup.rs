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

use gpui::{Context, FocusHandle, Window};

use super::OutcomesView;
use super::model::{CentreTab, Screen};
use crate::approvals::{self, Ages, Kind, Out, Popup, Row};
use crate::live::now_ms;

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
    /// The same for the permission asks of the orchestrator chat (`set_chat_asks`).
    chat_asks: Vec<(String, String)>,
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
            chat_asks: vec![],
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

    /// The permission asks waiting in the orchestrator chat (pushed by the Super Agent view).
    pub fn set_chat_asks(&mut self, asks: Vec<(String, String)>, cx: &mut Context<Self>) {
        if self.pop.chat_asks != asks {
            self.pop.chat_asks = asks;
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
            .chain(&w.chat_asks)
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
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tui_view_has_no_answer_path_for_a_decision_any_more() {
        // the grep test of S0: nothing of the decision card, the approvals list or the answer worker is in the TUI view
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui_view");
        let banned = [
            "send_effect",
            "Effect::Resolve",
            "outcomes_view",
            "superagent::outcomes",
            "crate::approvals",
            "OpenOutcome",
            "ChatTab::Outcomes",
        ];
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                for b in banned {
                    assert!(!text.contains(b), "{} mentions {b}", p.display());
                }
            }
        }
    }
}
