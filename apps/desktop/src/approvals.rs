//! What waits for the person in the TUI view, without a window: the Approvals list (tool-permission asks of the
//! sessions and outcome decisions, oldest first), the decision popup's keys, and the gate that keeps all of it
//! inert while outcomes are off or unsupported. Nothing here talks to the orchestrator: a decision is answered
//! by an `Effect::Resolve` that the Outcomes link sends (the same worker, the same revision and brief hash).
//!
//! Age is the time this window first saw the item (the server sends no creation time): it is a lower bound of
//! the real wait.

use std::collections::HashMap;

use synthesis_config::Env;
use synthesis_view::outcomes::DecisionCard;

use crate::live::ARM_MS;
use crate::outcomes_view::ask::Ask;
use crate::outcomes_view::link;
use crate::outcomes_view::model::{Effect, Item, Screen};

/// `Ok` when the platform supports outcomes and `synt.orchestrator.enabled` is true; `Err` is why not. Checked
/// before any panel, tab, popup, worker thread or poll exists: while it is `Err` the TUI view draws nothing of this.
pub fn gate(env: &Env) -> Result<(), Screen> {
    link::gate(env).map(|_| ())
}

/// When each pending item was first seen (ms), by key.
#[derive(Default)]
pub struct Ages(HashMap<String, i64>);

impl Ages {
    /// The keys pending now: new ones start at `now`, ones no longer pending are forgotten.
    pub fn observe<'a>(&mut self, keys: impl IntoIterator<Item = &'a str>, now: i64) {
        let live: Vec<&str> = keys.into_iter().collect();
        self.0.retain(|k, _| live.contains(&k.as_str()));
        for k in live {
            self.0.entry(k.to_string()).or_insert(now);
        }
    }

    /// Sets a first-seen time (the sample data, so a shot shows fixed ages).
    pub fn seed(&mut self, key: &str, since: i64) {
        self.0.insert(key.to_string(), since);
    }

    pub fn since(&self, key: &str) -> Option<i64> {
        self.0.get(key).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A tool-permission ask of a session.
    Tool,
    /// An outcome decision.
    Decision,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tool => "Tool",
            Self::Decision => "Decision",
        }
    }
}

/// One row of the Approvals panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Dialog id (a tool ask) or outcome id (a decision): what Alt+A and a click act on.
    pub key: String,
    pub kind: Kind,
    pub subject: String,
    /// When it was first seen (ms).
    pub since: i64,
}

/// One row per outcome that has decisions pending (the list only says how many; the cards are read when opened).
pub fn decision_rows(items: &[Item], ages: &Ages, now: i64) -> Vec<Row> {
    items
        .iter()
        .filter(|i| i.row.needs_you.decisions_pending > 0)
        .map(|i| Row {
            key: i.row.id.clone(),
            kind: Kind::Decision,
            subject: i.row.goal.clone(),
            since: ages.since(&i.row.id).unwrap_or(now),
        })
        .collect()
}

/// Both kinds in one list, the longest waiting first (ties keep the order given: tools, then decisions).
pub fn merge(tools: Vec<Row>, decisions: Vec<Row>) -> Vec<Row> {
    let mut all: Vec<Row> = tools.into_iter().chain(decisions).collect();
    all.sort_by_key(|r| r.since);
    all
}

/// The row after `current` (wrapping), or the oldest when `current` is not in the list. Alt+A.
pub fn next(rows: &[Row], current: Option<&str>) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    Some(
        match current.and_then(|c| rows.iter().position(|r| r.key == c)) {
            Some(i) => (i + 1) % rows.len(),
            None => 0,
        },
    )
}

/// "12s", "3m", "2h", "1d".
pub fn age_label(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// What a key did to the popup.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// Not the popup's key: the window may use it.
    Pass,
    Handled,
    /// Send this answer (the Outcomes link does).
    Send(Effect),
    /// Esc: answer later; the popup closes and the item stays in Approvals.
    Later,
}

/// The decision popup over the prompt: one card, two steps (choose, then Enter), Tab for the whole brief.
pub struct Popup {
    pub outcome: String,
    pub card: DecisionCard,
    pub ask: Ask,
    /// Tab: the full brief instead of the first lines.
    pub full: bool,
    /// `State::answers` when this popup sent an answer: it is waiting for that count to grow (the reply).
    pub sent_at: Option<u64>,
}

impl Popup {
    /// A card shown for the first time: nothing chosen, Enter inert for the arming delay.
    pub fn new(outcome: String, card: DecisionCard, now: i64) -> Self {
        let mut ask = Ask::default();
        ask.sync(Some(&card), now);
        Self {
            outcome,
            card,
            ask,
            full: false,
            sent_at: None,
        }
    }

    /// Enter is inert for `ARM_MS` from `now` (a tool dialog is on screen, or has just closed).
    pub fn rearm(&mut self, now: i64) {
        self.ask.armed_until = self.ask.armed_until.max(now + ARM_MS);
    }

    /// A fresh read of the card. The same revision and brief hash changes nothing; another one is a new card:
    /// nothing chosen, armed again, the brief collapsed, and a notice that it changed.
    pub fn update(&mut self, card: DecisionCard, now: i64) {
        let same = (&card.id, card.revision, &card.brief_sha256)
            == (&self.card.id, self.card.revision, &self.card.brief_sha256);
        if !same {
            self.full = false;
        }
        self.ask.sync(Some(&card), now);
        self.card = card;
    }

    /// `held`: the key is an auto-repeat (a held Enter never answers).
    pub fn key(&mut self, name: &str, held: bool, now: i64) -> Out {
        match name {
            "up" | "down" => self.ask.step(name == "down"),
            "tab" => self.full = !self.full,
            "escape" => return Out::Later,
            "enter" if held => {}
            "enter" => {
                return match self.ask.send(&self.outcome, &self.card, now) {
                    Some(e) => Out::Send(e),
                    None => Out::Handled,
                };
            }
            d => match d.parse::<usize>() {
                Ok(n @ 1..=9) => self.ask.choose(n - 1),
                _ => return Out::Pass,
            },
        }
        Out::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::ARM_MS;
    use crate::outcomes_view::sample;

    fn card() -> DecisionCard {
        sample::state().cards()[0].clone()
    }

    fn row(key: &str, kind: Kind, since: i64) -> Row {
        Row {
            key: key.into(),
            kind,
            subject: key.into(),
            since,
        }
    }

    #[test]
    fn ages_start_at_first_sight_and_forget_what_is_gone() {
        let mut a = Ages::default();
        a.observe(["x", "y"], 100);
        a.observe(["x", "z"], 500);
        assert_eq!(a.since("x"), Some(100), "first sight stands");
        assert_eq!(a.since("z"), Some(500));
        assert_eq!(a.since("y"), None, "no longer pending");
    }

    #[test]
    fn rows_of_both_kinds_are_oldest_first_and_alt_a_cycles() {
        let all = merge(
            vec![row("t2", Kind::Tool, 300), row("t1", Kind::Tool, 100)],
            vec![row("o1", Kind::Decision, 200)],
        );
        let keys: Vec<_> = all.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["t1", "o1", "t2"]);
        assert_eq!(next(&all, None), Some(0));
        assert_eq!(next(&all, Some("t1")), Some(1));
        assert_eq!(next(&all, Some("t2")), Some(0), "wraps");
        assert_eq!(next(&all, Some("gone")), Some(0));
        assert_eq!(next(&[], None), None);
    }

    #[test]
    fn an_outcome_with_decisions_pending_is_one_row_aged_from_first_sight() {
        let s = sample::state();
        let pending: Vec<_> = s
            .items
            .iter()
            .filter(|i| i.row.needs_you.decisions_pending > 0)
            .map(|i| i.row.id.as_str())
            .collect();
        assert!(!pending.is_empty(), "the sample has one");
        let mut ages = Ages::default();
        ages.observe(pending.iter().copied(), 1000);
        let rows = decision_rows(&s.items, &ages, 9000);
        assert_eq!(rows.len(), pending.len());
        assert!(
            rows.iter()
                .all(|r| r.kind == Kind::Decision && r.since == 1000)
        );
    }

    #[test]
    fn ages_read_as_one_short_unit() {
        assert_eq!(age_label(12_000), "12s");
        assert_eq!(age_label(180_000), "3m");
        assert_eq!(age_label(7_300_000), "2h");
        assert_eq!(age_label(200_000_000), "2d");
        assert_eq!(age_label(-5), "0s");
    }

    #[test]
    fn the_popup_chooses_then_sends_and_enter_is_inert_at_first() {
        let mut p = Popup::new("o".into(), card(), 0);
        assert_eq!(
            p.ask.picker.as_ref().unwrap().cursor(),
            None,
            "no preselection"
        );
        // Enter with nothing chosen sends nothing
        assert_eq!(p.key("enter", false, 10_000), Out::Handled);
        assert!(
            p.ask
                .notice
                .as_deref()
                .unwrap()
                .contains("Nothing is chosen")
        );
        // step 1: the arrow (or a number) moves the cursor and sends nothing
        assert_eq!(p.key("down", false, 0), Out::Handled);
        assert!(p.ask.picker.as_ref().unwrap().choice().is_some());
        // step 2 inside the arming delay: nothing is sent
        assert_eq!(p.key("enter", false, ARM_MS - 1), Out::Handled);
        assert!(!p.ask.sending);
        // a held Enter never answers, even armed
        assert_eq!(p.key("enter", true, 10_000), Out::Handled);
        assert!(!p.ask.sending);
        // a deliberate Enter sends the card's own revision and hash, once
        let Out::Send(Effect::Resolve { card: c, .. }) = p.key("enter", false, 10_000) else {
            panic!("not sent")
        };
        assert_eq!(
            (c.revision, c.brief_sha256.clone()),
            (p.card.revision, p.card.brief_sha256.clone())
        );
        assert_eq!(
            p.key("enter", false, 20_000),
            Out::Handled,
            "no second answer in flight"
        );
    }

    #[test]
    fn digits_choose_and_other_keys_are_not_the_popups() {
        let mut p = Popup::new("o".into(), card(), 0);
        assert_eq!(p.key("2", false, 0), Out::Handled);
        assert_eq!(p.ask.picker.as_ref().unwrap().cursor(), Some(1));
        assert_eq!(p.key("a", false, 0), Out::Pass);
        assert_eq!(p.key("0", false, 0), Out::Pass);
    }

    #[test]
    fn esc_is_later_and_tab_is_the_full_brief() {
        let mut p = Popup::new("o".into(), card(), 0);
        assert!(!p.full);
        assert_eq!(p.key("tab", false, 0), Out::Handled);
        assert!(p.full);
        assert_eq!(p.key("escape", false, 0), Out::Later);
        assert!(!p.ask.sending);
    }

    #[test]
    fn a_new_revision_is_a_new_card_with_a_notice_and_nothing_chosen() {
        let mut p = Popup::new("o".into(), card(), 0);
        p.key("1", false, 0);
        p.key("tab", false, 0);
        // the same card again (a refresh) changes nothing
        p.update(card(), 5000);
        assert!(p.ask.picker.as_ref().unwrap().choice().is_some() && p.full);
        let mut c = card();
        c.revision += 1;
        p.update(c, 6000);
        assert_eq!(p.ask.picker.as_ref().unwrap().cursor(), None);
        assert!(!p.full);
        assert_eq!(p.ask.armed_until, 6000 + ARM_MS);
        assert!(
            p.ask
                .notice
                .as_deref()
                .unwrap()
                .contains("changed since you saw it")
        );
        assert_eq!(p.key("enter", false, 6000), Out::Handled);
        assert!(!p.ask.sending, "nothing was sent for the old choice");
    }

    #[test]
    fn a_brief_that_names_a_preferred_option_gets_no_card_to_answer() {
        let v = serde_json::json!({
            "id": "d1", "kind": "k", "state": "pending", "revision": 1,
            "brief_sha256": "a".repeat(64),
            "brief": {"alternatives": [{"id": "a1", "label": "A", "recommended": true}]}
        });
        assert!(
            DecisionCard::from_json(&v).is_err(),
            "fails closed: no popup can be made"
        );
    }

    #[test]
    fn nothing_starts_while_outcomes_are_off_or_unsupported() {
        let home = std::env::temp_dir().join("synt-approvals-gate-test");
        let env = Env::from_pairs([
            ("HOME", home.display().to_string()),
            ("USERPROFILE", home.display().to_string()),
            ("SYNT_HOME", home.join(".synt").display().to_string()),
        ]);
        assert!(matches!(gate(&env), Err(Screen::Off | Screen::Windows)));
    }
}
