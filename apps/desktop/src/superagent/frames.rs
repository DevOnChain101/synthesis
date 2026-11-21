//! The open frames of the Super Agent view: tabs, the active one, and a browser-like history over frames
//! (`docs/specs/super-agent-s1.md` section 5.1; DESIGN-NOTES section 10.1). Pure state, no window: the view turns a
//! `Frame` into a body. Supersedes `outcomes_view/history.rs` (which stays until phase E deletes the old UI).

use std::collections::HashMap;

/// The two goal forms (the kind picker's goal rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GoalKind {
    Writes,
    ReadOnly,
}

/// One distinct view. `Picker` is the kind-picker dialog: it is never a tab and never in the history.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Frame {
    Home,
    Outcome(String),
    /// A worker item of an outcome.
    Worker(String, String),
    Session(String),
    NewGoal(GoalKind),
    Picker,
}

/// At most this many session frames are open at once (owner decision 2026-11-21).
pub const MAX_SESSIONS: usize = 6;
/// The most places the history keeps: the oldest go first.
const MAX_HISTORY: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub struct CapReached;

pub struct Frames {
    /// `tabs[0]` is always `Home`: pinned, not closable, not movable.
    tabs: Vec<Frame>,
    active: usize,
    hist: Vec<Frame>,
    at: usize,
    drafts: HashMap<Frame, String>,
}

impl Default for Frames {
    fn default() -> Self {
        Self {
            tabs: vec![Frame::Home],
            active: 0,
            hist: vec![Frame::Home],
            at: 0,
            drafts: HashMap::new(),
        }
    }
}

impl Frames {
    pub fn tabs(&self) -> &[Frame] {
        &self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> &Frame {
        &self.tabs[self.active]
    }

    pub fn session_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|f| matches!(f, Frame::Session(_)))
            .count()
    }

    fn at_cap(&self, f: &Frame) -> bool {
        matches!(f, Frame::Session(_)) && self.session_count() >= MAX_SESSIONS
    }

    /// `f` was shown: the history gets it (the same place twice is one; what was ahead is dropped).
    fn visit(&mut self, f: Frame) {
        if self.hist.get(self.at) == Some(&f) {
            return;
        }
        self.hist.truncate(self.at + 1);
        self.hist.push(f);
        if self.hist.len() > MAX_HISTORY {
            self.hist.remove(0);
        }
        self.at = self.hist.len() - 1;
    }

    /// Makes `f` the active tab, opening it right after the active one when it has none.
    fn show(&mut self, f: Frame) -> Result<(), CapReached> {
        match self.tabs.iter().position(|t| *t == f) {
            Some(i) => self.active = i,
            None if self.at_cap(&f) => return Err(CapReached),
            None => {
                self.active += 1;
                self.tabs.insert(self.active, f);
            }
        }
        Ok(())
    }

    /// Opens or focuses `f` and records it. `Picker` is not a frame of the strip: nothing happens.
    pub fn open(&mut self, f: Frame) -> Result<(), CapReached> {
        if f == Frame::Picker {
            return Ok(());
        }
        self.show(f.clone())?;
        self.visit(f);
        Ok(())
    }

    /// Focuses tab `i`.
    pub fn focus(&mut self, i: usize) {
        if let Some(f) = self.tabs.get(i).cloned() {
            self.active = i;
            self.visit(f);
        }
    }

    /// Ctrl+Tab (`step` 1) and Ctrl+Shift+Tab (`step` -1): wraps around.
    pub fn cycle(&mut self, step: isize) {
        let n = self.tabs.len() as isize;
        self.focus((self.active as isize + step).rem_euclid(n) as usize);
    }

    /// Closes tab `i` (never the home): the right neighbour is shown, else the left. Its history entries stay.
    pub fn close(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.tabs.len() {
            return false;
        }
        self.tabs.remove(i);
        if i < self.active {
            self.active -= 1;
        } else if i == self.active {
            self.active = i.min(self.tabs.len() - 1);
            let f = self.tabs[self.active].clone();
            self.visit(f);
        }
        true
    }

    /// Drag-reorder: tab `from` goes to position `to`. The home neither moves nor can be passed.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        let n = self.tabs.len();
        if from == 0 || to == 0 || from >= n || to >= n || from == to {
            return false;
        }
        let shown = self.tabs[self.active].clone();
        let f = self.tabs.remove(from);
        self.tabs.insert(to, f);
        self.active = self.tabs.iter().position(|t| *t == shown).unwrap_or(0);
        true
    }

    /// Can history step to `f`: it is alive, and it has a tab or one may be opened.
    fn reachable(&self, f: &Frame, alive: &impl Fn(&Frame) -> bool) -> bool {
        alive(f) && (self.tabs.contains(f) || !self.at_cap(f))
    }

    fn before(&self, alive: &impl Fn(&Frame) -> bool) -> Option<usize> {
        (0..self.at)
            .rev()
            .find(|i| self.reachable(&self.hist[*i], alive))
    }

    fn after(&self, alive: &impl Fn(&Frame) -> bool) -> Option<usize> {
        (self.at + 1..self.hist.len()).find(|i| self.reachable(&self.hist[*i], alive))
    }

    pub fn can_back(&self, alive: impl Fn(&Frame) -> bool) -> bool {
        self.before(&alive).is_some()
    }

    pub fn can_forward(&self, alive: impl Fn(&Frame) -> bool) -> bool {
        self.after(&alive).is_some()
    }

    fn go(&mut self, to: Option<usize>) -> bool {
        let Some(i) = to else { return false };
        // a closed tab is reopened after the active one; `reachable` checked the cap
        let shown = self.show(self.hist[i].clone());
        if shown.is_ok() {
            self.at = i;
        }
        shown.is_ok()
    }

    /// Back to the nearest place `alive` accepts, without recording a visit. False when there is none.
    pub fn back(&mut self, alive: impl Fn(&Frame) -> bool) -> bool {
        let to = self.before(&alive);
        self.go(to)
    }

    pub fn forward(&mut self, alive: impl Fn(&Frame) -> bool) -> bool {
        let to = self.after(&alive);
        self.go(to)
    }

    pub fn draft(&self, f: &Frame) -> &str {
        self.drafts.get(f).map_or("", String::as_str)
    }

    pub fn set_draft(&mut self, f: Frame, text: String) {
        if text.is_empty() {
            self.drafts.remove(&f);
        } else {
            self.drafts.insert(f, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(id: &str) -> Frame {
        Frame::Outcome(id.into())
    }
    fn s(id: &str) -> Frame {
        Frame::Session(id.into())
    }
    fn any(_: &Frame) -> bool {
        true
    }

    #[test]
    fn it_starts_on_the_pinned_home_and_opening_twice_focuses() {
        let mut f = Frames::default();
        assert_eq!((f.tabs(), f.active_index()), (&[Frame::Home][..], 0));
        f.open(o("a")).unwrap();
        f.open(s("1")).unwrap();
        f.open(o("a")).unwrap();
        assert_eq!(
            f.tabs(),
            &[Frame::Home, o("a"), s("1")][..],
            "a new tab opens after the active one"
        );
        assert_eq!(f.active(), &o("a"));
        assert_eq!(f.tabs().iter().filter(|t| **t == o("a")).count(), 1);
        f.open(Frame::Picker).unwrap();
        assert_eq!(f.tabs().len(), 3, "the picker is no tab");
    }

    #[test]
    fn closing_shows_the_right_neighbour_else_the_left_and_never_the_home() {
        let mut f = Frames::default();
        for id in ["a", "b", "c"] {
            f.open(o(id)).unwrap();
        }
        // tabs: home, a, b, c ; active c
        assert!(!f.close(0), "home is pinned");
        f.focus(2);
        assert!(f.close(2));
        assert_eq!(f.active(), &o("c"), "the right neighbour");
        assert!(f.close(2));
        assert_eq!(f.active(), &o("a"), "no right neighbour: the left");
        assert!(f.close(1));
        assert_eq!(f.active(), &Frame::Home, "the last tab closed returns home");
        assert!(!f.close(5));
        // closing a tab before the active one keeps the active frame
        for id in ["x", "y"] {
            f.open(o(id)).unwrap();
        }
        assert!(f.close(1));
        assert_eq!(f.active(), &o("y"));
    }

    #[test]
    fn reorder_keeps_home_first_and_the_active_frame() {
        let mut f = Frames::default();
        for id in ["a", "b", "c"] {
            f.open(o(id)).unwrap();
        }
        assert!(
            !f.reorder(0, 2) && !f.reorder(2, 0),
            "home neither moves nor is passed"
        );
        assert!(f.reorder(3, 1));
        assert_eq!(f.tabs(), &[Frame::Home, o("c"), o("a"), o("b")][..]);
        assert_eq!(f.active(), &o("c"), "the active frame follows its tab");
        assert!(!f.reorder(1, 1) && !f.reorder(1, 9));
    }

    #[test]
    fn at_most_six_session_frames() {
        let mut f = Frames::default();
        for i in 0..MAX_SESSIONS {
            f.open(s(&i.to_string())).unwrap();
        }
        assert_eq!(f.open(s("7")), Err(CapReached));
        assert_eq!(f.session_count(), MAX_SESSIONS);
        f.open(o("a")).unwrap();
        f.open(s("3")).unwrap();
        assert_eq!(f.active(), &s("3"), "an open one still focuses at the cap");
        f.close(1);
        f.open(s("7")).unwrap();
    }

    #[test]
    fn it_cycles_tabs_with_wraparound() {
        let mut f = Frames::default();
        f.open(o("a")).unwrap();
        f.cycle(1);
        assert_eq!(f.active(), &Frame::Home);
        f.cycle(-1);
        assert_eq!(f.active(), &o("a"));
    }

    #[test]
    fn it_walks_back_and_forward_and_a_new_visit_drops_what_was_ahead() {
        let mut f = Frames::default();
        assert!(!f.can_back(any) && !f.can_forward(any));
        f.open(o("a")).unwrap();
        f.open(o("b")).unwrap();
        f.open(o("b")).unwrap();
        assert!(f.back(any));
        assert_eq!(f.active(), &o("a"));
        assert!(f.back(any));
        assert_eq!(f.active(), &Frame::Home);
        assert!(!f.back(any) && f.can_forward(any));
        assert!(f.forward(any));
        f.open(s("1")).unwrap();
        assert!(!f.can_forward(any), "ahead was dropped");
        assert!(f.back(any));
        assert_eq!(f.active(), &o("a"));
        assert!(f.forward(any));
        assert_eq!(f.active(), &s("1"));
    }

    #[test]
    fn history_skips_what_is_gone_and_reopens_a_closed_tab() {
        let mut f = Frames::default();
        for id in ["a", "gone", "b"] {
            f.open(o(id)).unwrap();
        }
        let alive = |x: &Frame| *x != o("gone");
        assert!(f.back(alive));
        assert_eq!(f.active(), &o("a"), "skips the one that is gone");
        assert!(f.forward(alive));
        assert_eq!(f.active(), &o("b"));
        assert!(!f.can_back(|x: &Frame| *x == o("b")), "nothing else alive");
        // a closed tab comes back after the active one
        let i = f.tabs().iter().position(|t| *t == o("a")).unwrap();
        f.close(i);
        assert!(f.back(alive));
        assert_eq!(f.active(), &o("a"));
        assert!(f.tabs().contains(&o("a")));
    }

    #[test]
    fn history_does_not_reopen_a_session_past_the_cap() {
        let mut f = Frames::default();
        f.open(s("old")).unwrap();
        f.close(1);
        for i in 0..MAX_SESSIONS {
            f.open(s(&i.to_string())).unwrap();
        }
        f.open(o("a")).unwrap();
        // back goes to the nearest reachable place: the session that is open, never the closed `old` past the cap
        while f.back(any) {
            assert_ne!(f.active(), &s("old"));
        }
        assert_eq!(f.session_count(), MAX_SESSIONS);
    }

    #[test]
    fn it_keeps_at_most_max_places() {
        let mut f = Frames::default();
        for i in 0..MAX_HISTORY + 20 {
            f.open(o(&i.to_string())).unwrap();
        }
        assert_eq!(f.hist.len(), MAX_HISTORY);
        assert_eq!(f.hist.last(), Some(&o(&(MAX_HISTORY + 19).to_string())));
    }

    #[test]
    fn drafts_are_kept_per_frame() {
        let mut f = Frames::default();
        f.set_draft(s("1"), "hello".into());
        f.set_draft(o("a"), "x".into());
        assert_eq!((f.draft(&s("1")), f.draft(&s("2"))), ("hello", ""));
        f.set_draft(s("1"), String::new());
        assert_eq!(f.draft(&s("1")), "");
    }
}
