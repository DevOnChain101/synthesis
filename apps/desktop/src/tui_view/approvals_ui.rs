//! Outcomes in the TUI view (`docs/specs/desktop.md` "TUI view: outcomes"): the "outcomes" tab of the centre strip,
//! the Approvals panel on top of the right column, and the outcome decision popup above the prompt. Everything is
//! inert while `TuiView::outcomes` is `None` (`approvals::gate` said off or unsupported when the shell started):
//! no panel, tab, popup, worker thread or poll. The data is the Outcomes tab's own (`OutcomesView::state`, read
//! only here), a decision is answered through the same worker (`OutcomesView::send_effect`), and an outcome is
//! opened in the Outcomes view, never inspected here. This file depends on no orchestrator crate.

use gpui::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;

use super::activity::RunState;
use super::panels::{OnClick, dim, icon, strong, text, with_alpha};
use super::{ChatTab, TuiView};
use crate::approvals::{self, Ages, Kind, Out, Popup, Row};
use crate::live::now_ms;
use crate::outcomes_view::model::Screen;
use crate::outcomes_view::{OutcomesView, render};
use crate::theme::hsla;
use crate::widgets::chrome::{Head, Pad, panel};

/// A click on an outcome of the tab asks the shell to open it in the Outcomes view (Cmd/Ctrl+2).
pub struct OpenOutcome(pub String);

impl gpui::EventEmitter<OpenOutcome> for TuiView {}

/// Which brief a card is: a new id, revision or brief hash is a new card (what "later" is remembered for).
type CardKey = (String, i64, String);

fn card_key(c: &synthesis_view::outcomes::DecisionCard) -> CardKey {
    (c.id.clone(), c.revision, c.brief_sha256.clone())
}

/// A second Esc this soon after the one that gave the keys back dismisses the popup...
const DOUBLE_ESC_MS: i64 = 700;
/// ...unless it comes sooner than this: that is the first Esc auto-repeating (a held key), which dismisses nothing.
const REPEAT_MS: i64 = 150;

/// Rows the Approvals panel shows before it scrolls.
const PANEL_ROWS: f32 = 5.;

/// The sample states of the shots (`--outcomes`): one approval, three (mixed), the decision popup, the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutShot {
    Approvals1,
    Approvals3,
    /// The popup open by itself, the prompt keeping the keys.
    Decision,
    /// The popup with the keys (after Alt+A).
    DecisionFocused,
    /// The popup with the keys and the whole brief (after Alt+A, Tab).
    DecisionExpanded,
    Tab,
}

impl OutShot {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "approvals-1" => Self::Approvals1,
            "approvals-3" => Self::Approvals3,
            "decision" => Self::Decision,
            "decision-focus" => Self::DecisionFocused,
            "decision-expanded" => Self::DecisionExpanded,
            "tab" => Self::Tab,
            _ => return None,
        })
    }
}

/// What the popup shows.
pub enum Shown {
    /// The outcome was asked for and its card has not come yet.
    Loading {
        outcome: String,
        goal: String,
    },
    Card(Box<Popup>, String),
}

/// The TUI view's side of the Outcomes link.
pub struct Wire {
    pub view: Entity<OutcomesView>,
    ages: Ages,
    tool_ages: Ages,
    /// Rows the sample adds (the shots); empty in a live view.
    pub extra_tools: Vec<Row>,
    /// A fixed clock (the shots); `None`: the real one.
    pub fixed_now: Option<i64>,
    pub shown: Option<Shown>,
    /// The key of the row Alt+A went to last.
    cursor: Option<String>,
    /// The row of the outcomes tab Up/Down moved to.
    list_sel: usize,
    /// The popup has the keys (Alt+A or a click): only then do plain keys act on it. Otherwise the prompt and
    /// the engine get every key, as without outcomes. This is the person's intent; the effective state also needs
    /// the popup's own GPUI focus (`handle`, see `popup_focused`), which any click elsewhere, a focus move to the
    /// prompt or another view takes away.
    pub focus: bool,
    /// The popup's focus handle: the keys are the popup's only while it holds the window's focus.
    handle: gpui::FocusHandle,
    /// The centre tab and session of the last look: a change of either gives the keys back to the prompt.
    tab_seen: ChatTab,
    sid_seen: String,
    /// The card the person put off ("later"): it stays closed until a new card, revision or Alt+A.
    dismissed: Option<CardKey>,
    /// When Esc last gave the keys back (a second one soon after dismisses the popup).
    esc_at: i64,
    _sub: gpui::Subscription,
}

impl Wire {
    pub fn seed_tool(&mut self, row: Row) {
        self.tool_ages.seed(&row.key, row.since);
        self.extra_tools.push(row);
    }

    pub fn seed_outcome(&mut self, id: &str, since: i64) {
        self.ages.seed(id, since);
    }
}

impl TuiView {
    /// Connects the view to the Outcomes link. Called once by the shell after `approvals::gate` said yes (and the
    /// link was started); a second call changes nothing.
    pub fn set_outcomes(&mut self, view: Entity<OutcomesView>, cx: &mut Context<Self>) {
        if self.outcomes.is_some() {
            return;
        }
        let sub = cx.observe(&view, |_, _, cx| cx.notify());
        self.outcomes = Some(Wire {
            view,
            ages: Ages::default(),
            tool_ages: Ages::default(),
            extra_tools: vec![],
            fixed_now: None,
            shown: None,
            cursor: None,
            list_sel: 0,
            focus: false,
            handle: cx.focus_handle(),
            tab_seen: self.ts.tab,
            sid_seen: self
                .live
                .as_ref()
                .map(|l| l.live.sid.clone())
                .unwrap_or_default(),
            dismissed: None,
            esc_at: i64::MIN / 2,
            _sub: sub,
        });
        cx.notify();
    }

    /// The sample states of the shots and design tests (`--outcomes NAME`): the Outcomes sample behind a fixed clock.
    pub fn seed_sample(
        &mut self,
        what: OutShot,
        view: Entity<OutcomesView>,
        cx: &mut Context<Self>,
    ) {
        use super::sample::{OUTCOMES_NOW as NOW, approval_tools};
        self.set_outcomes(view, cx);
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        w.fixed_now = Some(NOW);
        w.seed_outcome("o_auth", NOW - 9 * 60_000);
        // the popup opens by itself on the selected outcome's pending card: the states that show something else
        // have it put off
        if !matches!(
            what,
            OutShot::Decision | OutShot::DecisionFocused | OutShot::DecisionExpanded
        ) {
            let st = &w.view.read(cx).state;
            w.dismissed = st.cards().iter().find(|c| c.resolvable()).map(card_key);
        }
        match what {
            OutShot::Approvals1 => {}
            OutShot::Approvals3 => approval_tools().into_iter().for_each(|r| w.seed_tool(r)),
            OutShot::Decision => {}
            // the window's own focus is needed for this one: `focus_decision`, once the first frame is drawn
            OutShot::DecisionFocused | OutShot::DecisionExpanded => {}
            OutShot::Tab => self.ts.tab = ChatTab::Outcomes,
        }
    }

    /// Whether the keys are the popup's now: it was given them and still holds the window's focus.
    pub fn popup_has_keys(&self, window: &Window) -> bool {
        self.outcomes
            .as_ref()
            .is_some_and(|w| w.focus && w.handle.is_focused(window))
    }

    /// The whole brief is shown (what Tab does in the focused popup), for the shots.
    pub fn expand_decision(&mut self, cx: &mut Context<Self>) {
        if let Some(Shown::Card(p, _)) = self.outcomes.as_mut().and_then(|w| w.shown.as_mut()) {
            p.full = true;
        }
        cx.notify();
    }

    /// The popup takes the keys (what Alt+A does), for the shots and tests of the focused state.
    pub fn focus_decision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        self.focus_popup(now, window, cx);
    }

    fn now(&self) -> i64 {
        self.outcomes
            .as_ref()
            .and_then(|w| w.fixed_now)
            .unwrap_or_else(now_ms)
    }

    /// The tabs of the strip: the five of the design, and "outcomes" while outcomes are on.
    pub(super) fn tab_list(&self) -> Vec<ChatTab> {
        let mut v = ChatTab::ALL.to_vec();
        if self.outcomes.is_some() {
            v.push(ChatTab::Outcomes);
        }
        v
    }

    /// The count on the tab: outcomes that need the person.
    pub(super) fn outcomes_badge(&self, cx: &Context<Self>) -> Option<String> {
        let st = &self.outcomes.as_ref()?.view.read(cx).state;
        let n = st.items.iter().filter(|i| i.row.needs_you.any()).count();
        (st.screen == Screen::Ready && n > 0).then(|| n.to_string())
    }

    /// Per frame, before drawing: first-seen ages of what waits, and the popup follows the fresh card (a new
    /// revision or brief hash is a new card with a notice; a card no longer pending closes it).
    pub(super) fn sync_outcomes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.outcomes.is_none() {
            return;
        }
        self.drop_stale_focus(window, cx);
        self.sync_inner(cx);
        // the popup lost the keys (dialog, dismissed, other card, outcome gone) while it still holds GPUI focus, or
        // is gone with it: the prompt gets the window's focus back at once, so no key is lost on the way
        if self
            .outcomes
            .as_ref()
            .is_some_and(|w| !w.focus && w.handle.is_focused(window))
        {
            self.focus_prompt(window, cx);
        }
    }

    /// The keys are the popup's only while it holds the window's own focus and nothing implicit happened since: the
    /// prompt (a click, Ctrl+L, ...) or anything else taking the focus, another centre tab, another session. Any of
    /// those gives the keys back to the prompt.
    fn drop_stale_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sid = self
            .live
            .as_ref()
            .map(|l| l.live.sid.clone())
            .unwrap_or_default();
        let tab = self.ts.tab;
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let moved = (tab, &sid) != (w.tab_seen, &w.sid_seen);
        (w.tab_seen, w.sid_seen) = (tab, sid);
        if w.focus && (moved || !w.handle.is_focused(window)) {
            self.release_popup_focus(window, cx);
        }
    }

    /// The popup gives the keys up (an implicit event: a click elsewhere, a tab or session switch, the window
    /// going to the background): the prompt has them again.
    pub(super) fn release_popup_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let held = w.focus || w.handle.is_focused(window);
        w.focus = false;
        if held {
            self.focus_prompt(window, cx);
            cx.notify();
        }
    }

    /// The window was activated or deactivated: the keys go back to the prompt either way, so what was typed on
    /// the way back in is never read by the popup (called from `observe_window_activation`).
    pub fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_popup_focus(window, cx);
    }

    fn sync_inner(&mut self, cx: &Context<Self>) {
        let tools = self.tool_keys();
        let now = self.now();
        let dialogs = self.dialogs_open();
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        // a tool dialog on screen owns the keyboard: the popup loses the focus and its Enter stays inert for
        // ARM_MS after the dialog closes
        if dialogs {
            w.focus = false;
            if let Some(Shown::Card(p, _)) = w.shown.as_mut() {
                p.rearm(now);
            }
        }
        let st = &w.view.read(cx).state;
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
        w.tool_ages.observe(
            tools
                .iter()
                .map(String::as_str)
                .chain(w.extra_tools.iter().map(|r| r.key.as_str())),
            now,
        );
        let goal_of = |id: &str| {
            st.items
                .iter()
                .find(|i| i.row.id == id)
                .map(|i| i.row.goal.clone())
        };
        let card = st.cards().iter().find(|c| c.resolvable()).cloned();
        let Some(shown) = w.shown.take() else {
            // a decision of the selected outcome is pending and nothing else is up: the popup opens by itself, on
            // the card, WITHOUT the keys (the prompt keeps them); a card the person put off stays off until it changes
            let sel = st
                .selected
                .clone()
                .filter(|s| st.detail.as_ref().is_some_and(|d| &d.outcome == s));
            if let (true, Some(o), Some(c)) = (ready && !dialogs, sel, card)
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
            (Shown::Card(p, _), false) => Some(Shown::Card(p, goal)),
            (Shown::Card(mut p, _), true) => card.map(|c| {
                // the reply (`Msg::Answered`, counted by `State::answers`) came back: whatever it said, the choice
                // is gone and the popup may answer again, only after a new choice
                // (a link that ended also clears the tab's in-flight flag: that ends the wait too)
                if p.sent_at.is_some_and(|n| st.answers > n || !st.ask.sending) {
                    p.sent_at = None;
                    p.ask.answered(st.ask.notice.clone().unwrap_or_default());
                }
                if p.card.id != c.id {
                    // a DIFFERENT card is not what the person was about to answer: the keys go back to the prompt
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

    /// A tool-permission (or other) dialog of the open session is up: it owns the keyboard.
    fn dialogs_open(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|l| !l.live.dialogs.is_empty())
    }

    /// Sessions waiting for a tool permission (the open one, by its dialog, and the others by their status).
    fn tool_keys(&self) -> Vec<String> {
        self.tool_rows_raw().into_iter().map(|(k, _)| k).collect()
    }

    fn tool_rows_raw(&self) -> Vec<(String, String)> {
        self.snap
            .running
            .iter()
            .enumerate()
            .filter(|(_, r)| r.state == RunState::Approval && !r.peer)
            .map(|(i, r)| {
                let key = if r.id.is_empty() {
                    format!("#{i}")
                } else {
                    r.id.clone()
                };
                (key, synthesis_harness::sanitize_for_display(&r.title))
            })
            .collect()
    }

    /// The Approvals list: tool asks and outcome decisions, the longest waiting first.
    pub fn approval_rows(&self, cx: &Context<Self>) -> Vec<Row> {
        let Some(w) = &self.outcomes else {
            return vec![];
        };
        let now = self.now();
        let mut tools: Vec<Row> = self
            .tool_rows_raw()
            .into_iter()
            .map(|(key, subject)| Row {
                since: w.tool_ages.since(&key).unwrap_or(now),
                key,
                kind: Kind::Tool,
                subject,
            })
            .collect();
        tools.extend(w.extra_tools.iter().cloned());
        let st = &w.view.read(cx).state;
        let decisions = if st.screen == Screen::Ready {
            approvals::decision_rows(&st.items, &w.ages, now)
        } else {
            vec![]
        };
        approvals::merge(tools, decisions)
    }

    /// Alt+A: the next row (the oldest first, then around) is opened.
    pub fn next_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.approval_rows(cx);
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let Some(i) = approvals::next(&rows, w.cursor.as_deref()) else {
            return;
        };
        w.cursor = Some(rows[i].key.clone());
        self.open_approval(&rows[i], window, cx);
    }

    /// A row was activated (a click, or Alt+A): a session is opened; a decision shows its popup.
    fn open_approval(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        match row.kind {
            Kind::Tool => {
                if !row.key.starts_with('#')
                    && self.live.as_ref().is_some_and(|l| l.live.sid != row.key)
                {
                    self.open_session(&row.key.clone(), cx);
                }
            }
            Kind::Decision => self.open_decision(&row.key, &row.subject, window, cx),
        }
        cx.notify();
    }

    /// The popup of outcome `id`. When it is not the one the Outcomes tab has selected, it is selected there (and
    /// the popup says so) and the card is shown when its detail lands: nothing is selected silently.
    fn open_decision(&mut self, id: &str, goal: &str, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let (landed, card) = {
            let st = &w.view.read(cx).state;
            (
                st.selected.as_deref() == Some(id)
                    && st.detail.as_ref().is_some_and(|d| d.outcome == id),
                st.cards().iter().find(|c| c.resolvable()).cloned(),
            )
        };
        // asked for on purpose (a click on the row, Alt+A): the popup takes the keys, Enter inert for ARM_MS
        w.dismissed = None;
        w.shown = Some(match (landed, card) {
            (true, Some(c)) => Shown::Card(Box::new(Popup::new(id.into(), c, now)), goal.into()),
            _ => {
                let id = id.to_string();
                w.view.update(cx, |v, cx| v.select_id(&id, cx));
                Shown::Loading {
                    outcome: id,
                    goal: goal.into(),
                }
            }
        });
        self.focus_popup(now, window, cx);
    }

    /// The keys of this feature, before the engine's own: Alt+0 (the tab), Alt+A (next approval), and the popup's
    /// while it is open. Tool-permission dialogs keep the keyboard: while one is up only the Alt keys act.
    pub(super) fn outcome_key(
        &mut self,
        ks: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.outcomes.is_none() || !self.shown {
            return false;
        }
        // the popup's keys need its own GPUI focus: a click in the prompt (or anything else that moved the window's
        // focus) since Alt+A took them back already; a tab or session switch did too
        self.drop_stale_focus(window, cx);
        let m = &ks.modifiers;
        if m.alt && !m.control && !m.platform && !m.shift {
            match ks.key.as_str() {
                "0" => {
                    self.select_tab(ChatTab::Outcomes, cx);
                    self.release_popup_focus(window, cx);
                    return true;
                }
                "a" => {
                    self.alt_a(window, cx);
                    return true;
                }
                _ => {}
            }
        }
        // Shift (Shift+Enter, Shift+digit = "@" "#", Shift+Tab) and every other chord belong to the prompt and the
        // engine, never to the popup
        if m.control || m.platform || m.alt || m.shift {
            return false;
        }
        let now = self.now();
        if self.dialogs_open() {
            // the dialog owns the keyboard (the sync also drops the focus); the popup's Enter stays inert until
            // ARM_MS after it closes
            if let Some(w) = self.outcomes.as_mut()
                && let Some(Shown::Card(p, _)) = w.shown.as_mut()
            {
                p.rearm(now);
            }
            self.release_popup_focus(window, cx);
            return false;
        }
        // the completion popup and the history search own Tab, Esc and the arrows while they are open
        if self
            .live
            .as_ref()
            .is_some_and(|l| l.comp.is_some() || l.search.is_some())
        {
            return false;
        }
        let key = ks.key.as_str();
        let Some(w) = self.outcomes.as_mut() else {
            return false;
        };
        let focused =
            w.focus && w.handle.is_focused(window) && matches!(w.shown, Some(Shown::Card(..)));
        if focused {
            // the popup has the keys: Esc gives them back (the popup stays), the rest is its own
            if key == "escape" {
                w.esc_at = now;
                self.release_popup_focus(window, cx);
                cx.notify();
                return true;
            }
            let out = match w.shown.as_mut() {
                Some(Shown::Card(p, _)) => p.key(key, now),
                _ => Out::Pass,
            };
            if out != Out::Pass {
                self.popup_out(out, window, cx);
                return true;
            }
            return false;
        }
        // the prompt has the keys: nothing goes to the popup. The one thing taken is a second Esc soon after the
        // one that gave the keys back: "later". A repeat of the first Esc (a held key: under REPEAT_MS) is
        // swallowed without dismissing; it never interrupts a task either.
        if key == "escape" && w.shown.is_some() {
            let gap = now - w.esc_at;
            if (0..REPEAT_MS).contains(&gap) {
                return true;
            }
            if gap < DOUBLE_ESC_MS {
                self.dismiss_popup(window, cx);
                cx.notify();
                return true;
            }
        }
        // the outcome list of the tab, while the prompt is empty
        if self.ts.tab == ChatTab::Outcomes
            && matches!(key, "up" | "down" | "enter")
            && self
                .live
                .as_ref()
                .is_none_or(|l| l.prompt.read(cx).value().is_empty())
        {
            return self.list_key(key, cx);
        }
        false
    }

    /// Alt+A: with a popup it moves the keys into it, or back to the prompt when it has them (the popup stays);
    /// without one it goes to the next approval, which opens a decision's popup with the keys.
    fn alt_a(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now();
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        if matches!(w.shown, Some(Shown::Card(..))) {
            if w.focus && w.handle.is_focused(window) {
                self.release_popup_focus(window, cx);
            } else {
                self.focus_popup(now, window, cx);
            }
            cx.notify();
        } else {
            w.dismissed = None;
            self.next_approval(window, cx);
        }
    }

    /// The popup takes the keys: the window's focus moves to it (so the prompt no longer holds it) and Enter is
    /// inert for ARM_MS from now, so a key still held from typing cannot answer.
    pub(super) fn focus_popup(&mut self, now: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        w.focus = true;
        window.focus(&w.handle, cx);
        if let Some(Shown::Card(p, _)) = w.shown.as_mut() {
            p.rearm(now);
        }
    }

    /// "Later": the popup closes and stays closed until a new card or revision, or Alt+A.
    fn dismiss_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(w) = self.outcomes.as_mut() {
            if let Some(Shown::Card(p, _)) = &w.shown {
                w.dismissed = Some(card_key(&p.card));
            }
            w.shown = None;
        }
        self.release_popup_focus(window, cx);
    }

    fn popup_out(&mut self, out: Out, window: &mut Window, cx: &mut Context<Self>) {
        if out == Out::Later {
            self.dismiss_popup(window, cx);
            cx.notify();
            return;
        }
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        match out {
            Out::Send(e) => {
                let busy = w.view.read(cx).state.ask.sending;
                match w.shown.as_mut() {
                    // the Outcomes tab has an answer on its way: no second one from here
                    Some(Shown::Card(p, _)) if busy => {
                        p.ask.sending = false;
                        p.ask.notice = Some("An answer is already on its way.".into());
                    }
                    Some(Shown::Card(p, _)) => {
                        p.sent_at = Some(w.view.read(cx).state.answers);
                        w.view.update(cx, |v, cx| v.send_effect(e, cx));
                    }
                    _ => {}
                }
            }
            Out::Later | Out::Handled | Out::Pass => {}
        }
        cx.notify();
    }

    fn list_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(w) = self.outcomes.as_mut() else {
            return false;
        };
        let items = &w.view.read(cx).state.items;
        let n = items.len();
        if n == 0 {
            return false;
        }
        w.list_sel = w.list_sel.min(n - 1);
        match key {
            "up" => w.list_sel = w.list_sel.saturating_sub(1),
            "down" => w.list_sel = (w.list_sel + 1).min(n - 1),
            _ => {
                let id = items[w.list_sel].row.id.clone();
                cx.emit(OpenOutcome(id));
            }
        }
        cx.notify();
        true
    }

    // ------------------------------------------------------------------------------------- drawing

    /// The Approvals panel, or `None`: outcomes off, the panel switched off, or nothing waits.
    pub(super) fn approvals_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.outcomes.is_none() || self.hidden.approvals {
            return None;
        }
        let rows = self.approval_rows(cx);
        if rows.is_empty() {
            return None;
        }
        let now = self.now();
        let n = rows.len();
        let lines = rows.into_iter().enumerate().map(|(i, r)| {
            let (label, colour) = match r.kind {
                Kind::Tool => (r.kind.label(), hsla(t::WARN)),
                Kind::Decision => (r.kind.label(), hsla(t::ACCENT)),
            };
            let row = r.clone();
            div()
                .id(("appr-row", i))
                .debug_selector(move || format!("appr-row-{i}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(1.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(with_alpha(gpui::white(), 0.05)))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.open_approval(&row, window, cx)),
                )
                .child(
                    div()
                        .w(px(62.))
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(colour)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(text())
                        .child(r.subject),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(dim())
                        .child(approvals::age_label(now - r.since)),
                )
        });
        let meta = div()
            .text_color(hsla(t::ACCENT))
            .child(format!("{n}"))
            .into_any_element();
        Some(
            panel(
                Head::new("p-approvals", "Approvals", "shield", Pad::CARD)
                    .max_body(PANEL_ROWS * 21.4 + 8.),
                Some(meta),
                div().flex().flex_col().children(lines),
            )
            .flex_none()
            .into_any_element(),
        )
    }

    /// The outcomes tab: the list, read-only. A click (or Enter) opens the outcome in the Outcomes view.
    pub(super) fn outcomes_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = div()
            .debug_selector(|| "tab-pane".to_string())
            .id("outcomes-pane")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py(px(10.))
            .flex()
            .flex_col()
            .gap(px(6.));
        let Some(w) = &self.outcomes else {
            return body.into_any_element();
        };
        let (st, sel) = (&w.view.read(cx).state, w.list_sel);
        if st.screen != Screen::Ready {
            return body
                .child(crate::widgets::panes::note(
                    "Outcomes are not connected. Open the Outcomes view (Cmd/Ctrl+2) to see why.",
                ))
                .into_any_element();
        }
        if st.items.is_empty() {
            return body
                .child(crate::widgets::panes::note("No outcomes yet."))
                .into_any_element();
        }
        let rows = st.items.iter().enumerate().map(|(i, it)| {
            let id = it.row.id.clone();
            div()
                .id(("out-tab-row", i))
                .debug_selector(move || format!("out-tab-row-{i}"))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .border_1()
                .border_color(if i == sel {
                    hsla(t::ACCENT)
                } else {
                    hsla(t::BORDER)
                })
                .hover(|d| d.bg(hsla(t::ELEVATED)))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenOutcome(id.clone()))))
                .child(
                    div()
                        .text_color(strong())
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(it.row.goal.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .flex_wrap()
                        .child(
                            div()
                                .text_color(render::state_colour(&it.row.state))
                                .child(it.row.state.clone()),
                        )
                        .children(render::needs_badges(&it.row.needs_you))
                        .child(
                            div()
                                .ml_auto()
                                .text_size(px(11.))
                                .text_color(dim())
                                .child(render::budget_line(&it.budget)),
                        ),
                )
        });
        body.children(rows).into_any_element()
    }

    /// Tallest the popup may be: about 40% of the chat column collapsed, 60% expanded, and never less than the
    /// answers, buttons and hint need (the brief is the part that gives way).
    fn popup_max_h(&self, expanded: bool) -> f32 {
        use super::layout::{FOOTER_H, HEADER_H, PAD};
        let chat_h = (self.win_h - self.top - HEADER_H - FOOTER_H - 2. * PAD).max(300.);
        (chat_h * if expanded { 0.6 } else { 0.42 }).max(if expanded { 360. } else { 330. })
    }

    /// The decision popup above the prompt: a cream border (the Synthesis accent), not the amber of a tool
    /// permission. Full brief in the server's order, nothing preselected, two steps (choose, then Enter).
    pub(super) fn decision_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let w = self.outcomes.as_ref()?;
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
                with_alpha(hsla(t::ACCENT), 0.35)
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
                        "This outcome is now selected in the Outcomes tab too. The decision appears here when it arrives.",
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
                    let first_cut = c.rows.first().is_some_and(|r| {
                        r.text.lines().count() > 2 || r.text.chars().count() > 150
                    });
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
                    for (i, r) in c
                        .rows
                        .iter()
                        .enumerate()
                        .take(if p.full { usize::MAX } else { 1 })
                    {
                        brief = brief.child(
                            div()
                                .debug_selector(move || format!("decision-field-{i}"))
                                .flex()
                                .flex_col()
                                .child(div().text_size(px(11.)).text_color(dim()).child(r.label.clone()))
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
                            if let Some(Shown::Card(p, _)) =
                                this.outcomes.as_mut().and_then(|w| w.shown.as_mut())
                            {
                                p.full = !p.full;
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
                                            if let Some(Shown::Card(p, _)) =
                                                this.outcomes.as_mut().and_then(|w| w.shown.as_mut())
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
                    let send: OnClick = Box::new(cx.listener(|this, _, window, cx| {
                        let now = this.now();
                        let out = match this.outcomes.as_mut().and_then(|w| w.shown.as_mut()) {
                            Some(Shown::Card(p, _)) => p.key("enter", now),
                            _ => Out::Pass,
                        };
                        this.popup_out(out, window, cx);
                    }));
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
                                    .border_color(hsla(t::ACCENT))
                                    .text_color(strong())
                                    .on_click(move |e, w, cx| send(e, w, cx))
                                    .child("Send answer"),
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
                            .debug_selector(|| "decision-hint".to_string())
                            .text_color(if focused {
                                hsla(t::ACCENT_LIGHT)
                            } else {
                                dim()
                            })
                            .child(if focused {
                                "approval focused: Esc back to input · 1-9 or arrows choose · Enter sends · Tab full brief"
                            } else {
                                "Alt+A to answer (your typing is untouched)"
                            }),
                    )
                    .children(p.ask.notice.clone().map(|n| {
                        div().text_color(hsla(t::ACCENT_LIGHT)).child(n)
                    }))
                }
            }
            .into_any_element(),
        )
    }
}
