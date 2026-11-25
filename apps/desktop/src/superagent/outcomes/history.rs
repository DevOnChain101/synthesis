//! Back and forward through what the Super Agent view has shown (`docs/specs/super-agent.md` section 2.0), like a
//! browser: visiting something new drops what was ahead. No window in here: the view turns a `Place` into a selection.
//!
//! In S0 only outcome selections are recorded; the orchestrator chat (S1) and the session chats (S2) are places
//! of the model that nothing visits yet.

/// Something the centre can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// The orchestrator chat.
    Chat,
    /// An outcome, by id.
    Outcome(String),
    /// A session, by id.
    Session(String),
}

/// The most places kept: the oldest go first.
const MAX: usize = 100;

#[derive(Default)]
pub struct History {
    places: Vec<Place>,
    /// Index of the current place (meaningless while `places` is empty).
    at: usize,
}

impl History {
    pub fn current(&self) -> Option<&Place> {
        self.places.get(self.at)
    }

    /// `p` was shown. Showing the current place again changes nothing; anything ahead of it is dropped.
    pub fn visit(&mut self, p: Place) {
        if self.current() == Some(&p) {
            return;
        }
        if !self.places.is_empty() {
            self.places.truncate(self.at + 1);
        }
        self.places.push(p);
        if self.places.len() > MAX {
            self.places.remove(0);
        }
        self.at = self.places.len() - 1;
    }

    /// The nearest place before the current one that `ok` accepts (one that is gone is skipped).
    fn before(&self, ok: &impl Fn(&Place) -> bool) -> Option<usize> {
        (0..self.at.min(self.places.len()))
            .rev()
            .find(|i| ok(&self.places[*i]))
    }

    /// The nearest place after the current one that `ok` accepts.
    fn after(&self, ok: &impl Fn(&Place) -> bool) -> Option<usize> {
        (self.at + 1..self.places.len()).find(|i| ok(&self.places[*i]))
    }

    pub fn can_back(&self, ok: impl Fn(&Place) -> bool) -> bool {
        self.before(&ok).is_some()
    }

    pub fn can_forward(&self, ok: impl Fn(&Place) -> bool) -> bool {
        self.after(&ok).is_some()
    }

    /// Moves back to the nearest place `ok` accepts and returns it; `None` (nothing moves) when there is none.
    pub fn back(&mut self, ok: impl Fn(&Place) -> bool) -> Option<Place> {
        self.at = self.before(&ok)?;
        self.current().cloned()
    }

    pub fn forward(&mut self, ok: impl Fn(&Place) -> bool) -> Option<Place> {
        self.at = self.after(&ok)?;
        self.current().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(id: &str) -> Place {
        Place::Outcome(id.into())
    }

    #[test]
    fn it_walks_back_and_forward_and_a_new_visit_drops_what_was_ahead() {
        let any = |_: &Place| true;
        let mut h = History::default();
        assert!(
            !h.can_back(any) && !h.can_forward(any),
            "empty: nothing to walk"
        );
        h.visit(Place::Chat);
        h.visit(o("a"));
        h.visit(o("b"));
        h.visit(o("b"));
        assert_eq!(h.current(), Some(&o("b")), "the same place twice is one");
        assert_eq!(h.back(any), Some(o("a")));
        assert_eq!(h.back(any), Some(Place::Chat));
        assert!(!h.can_back(any) && h.can_forward(any));
        assert_eq!(h.back(any), None, "at the start nothing moves");
        assert_eq!(h.forward(any), Some(o("a")));
        h.visit(Place::Session("s1".into()));
        assert!(!h.can_forward(any), "ahead was dropped");
        assert_eq!(h.back(any), Some(o("a")));
        assert_eq!(h.forward(any), Some(Place::Session("s1".into())));
    }

    #[test]
    fn a_place_that_is_gone_is_skipped_and_enables_nothing_when_all_are_gone() {
        let mut h = History::default();
        for p in [o("a"), o("gone"), o("b")] {
            h.visit(p);
        }
        let alive = |p: &Place| *p != o("gone");
        assert!(h.can_back(alive));
        assert_eq!(h.back(alive), Some(o("a")), "skips the one that is gone");
        assert!(h.can_forward(alive));
        assert_eq!(h.forward(alive), Some(o("b")));
        assert!(!h.can_forward(alive));
        assert!(
            !h.can_back(|p: &Place| *p == o("b")),
            "nothing else is alive"
        );
        assert_eq!(h.back(|p: &Place| *p == o("b")), None);
        assert_eq!(h.current(), Some(&o("b")), "and nothing moved");
    }

    #[test]
    fn it_keeps_at_most_max_places() {
        let mut h = History::default();
        for i in 0..MAX + 20 {
            h.visit(o(&i.to_string()));
        }
        assert_eq!(h.places.len(), MAX);
        assert_eq!(h.current(), Some(&o(&(MAX + 19).to_string())));
        assert_eq!(h.places[0], o("20"));
    }
}
