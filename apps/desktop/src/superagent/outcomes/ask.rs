//! Answering one decision card: the cursor (it starts on nothing), the arming delay, the in-flight flag and the
//! notice. Shared by the Super Agent view's Decisions tab and its docked decision card, so both answer by the same rules: a
//! choice is a deliberate act (arrows, a number or a click), Enter sends it only after the arming delay and
//! never twice, and a card whose revision or brief hash changed is a new card with the cursor on nothing.

use synthesis_view::outcomes::{DecisionCard, Picker};

use super::model::Effect;
use crate::live::ARM_MS;

/// Which brief a picker belongs to: a card is the same one only while all three match.
type CardKey = (String, i64, String);

fn key_of(c: &DecisionCard) -> CardKey {
    (c.id.clone(), c.revision, c.brief_sha256.clone())
}

#[derive(Default)]
pub struct Ask {
    /// The answer cursor of the card (starts on nothing), `None` when it cannot be answered.
    pub picker: Option<Picker>,
    seen: Option<CardKey>,
    /// Enter does nothing before this (ms): a key typed ahead of the card answers nothing.
    pub armed_until: i64,
    /// An answer is on its way: no second one.
    pub sending: bool,
    /// One line: what the last answer or refresh said.
    pub notice: Option<String>,
}

impl Ask {
    /// Enter would send now (a choice, armed, nothing in flight).
    pub fn can_send(&self, now: i64) -> bool {
        !self.sending
            && now >= self.armed_until
            && self.picker.as_ref().and_then(Picker::choice).is_some()
    }

    /// Forget the card seen: the next `sync` makes a fresh picker (no "changed" notice).
    pub fn forget(&mut self) {
        self.seen = None;
    }

    /// Drop everything about the card (another outcome was selected).
    pub fn clear(&mut self) {
        self.picker = None;
        self.seen = None;
        self.notice = None;
    }

    /// A screen other than the list: an answer in flight is not resent and not waited for, and the choice
    /// made before is gone.
    pub fn link_changed(&mut self) {
        self.sending = false;
        self.seen = None;
    }

    pub fn answered(&mut self, note: String) {
        self.sending = false;
        // whatever came of it, the next answer is a new choice (and the refetch is not "a card that changed")
        self.seen = None;
        self.notice = Some(note);
    }

    /// Keeps the picker while `card` is the same brief; a new card (other id, a new revision or brief hash)
    /// gets a picker with the cursor on nothing and the arming delay again.
    pub fn sync(&mut self, card: Option<&DecisionCard>, now: i64) {
        let Some(card) = card else {
            self.picker = None;
            self.seen = None;
            return;
        };
        let key = key_of(card);
        if self.seen.as_ref() == Some(&key) && self.picker.is_some() == card.resolvable() {
            return;
        }
        if matches!(&self.seen, Some(s) if s.0 == card.id) {
            self.notice = Some(
                "This decision changed since you saw it: the new version is shown and nothing was sent. Read it again, then choose."
                    .into(),
            );
        }
        self.picker = Picker::new(card);
        self.seen = Some(key);
        self.armed_until = now + ARM_MS;
    }

    /// Moves the cursor to option `i` (a click, a number): it chooses nothing yet. Out of range: unchanged.
    pub fn choose(&mut self, i: usize) {
        if let Some(p) = self.picker.as_mut().filter(|p| i < p.options.len()) {
            p.move_to(i);
            self.notice = None;
        }
    }

    /// Up or down: from nothing, down goes to the first and up to the last; the ends do not wrap.
    pub fn step(&mut self, down: bool) {
        let Some(p) = &self.picker else { return };
        let n = p.options.len();
        let to = match (p.cursor(), down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(c), true) => (c + 1).min(n - 1),
            (Some(c), false) => c.saturating_sub(1),
        };
        self.choose(to);
    }

    /// Enter: the answer of `card`, when it may be sent; else why not (in `notice`).
    pub fn send(&mut self, outcome: &str, card: &DecisionCard, now: i64) -> Option<Effect> {
        let p = self.picker.as_ref()?;
        let Some(choice) = p.choice().map(str::to_string) else {
            self.notice = Some(
                "Nothing is chosen. Choose an answer (arrows, 1-9 or a click), then press Enter to send it."
                    .into(),
            );
            return None;
        };
        if self.sending {
            return None;
        }
        if now < self.armed_until {
            self.notice =
                Some("The decision has just appeared: look at it, then press Enter again.".into());
            return None;
        }
        self.sending = true;
        self.notice = Some("Sending the answer...".into());
        Some(Effect::Resolve {
            outcome: outcome.to_string(),
            card: Box::new(card.clone()),
            choice,
        })
    }
}
