//! The Outcomes tab's state and its key handling, with no window in it: what is shown comes from the
//! `synthesis_view::outcomes` models the link fetched, and a key or click only moves a cursor or asks for an
//! `Effect`. Nothing here computes a state, a verdict or a brief (ADR-021).
//!
//! A decision is answered in two steps by two deliberate acts: the person moves the cursor to an answer
//! (arrows, a number or a click; it starts on nothing), then presses Enter or "Send answer". Enter does
//! nothing while the card is new (the arming delay of the approval dock) and nothing is sent for a card
//! that changed in between: a new revision or brief hash is a new card with the cursor on nothing again.

use synthesis_view::outcomes::{
    ActivityTab, BudgetView, CriteriaView, DecisionCard, InspectorContent, InspectorTab,
    OutcomeRow, Picker,
};

use crate::live::ARM_MS;

/// What the Off screen says (the setting, then the explicit enable step; this window never enables).
pub const OFF_LINES: [&str; 4] = [
    "Outcomes are off. Nothing here runs, and nothing is started or migrated for you.",
    "1. Set synt.orchestrator.enabled to true in your user settings ($SYNT_HOME/synt.json, or your organisation's managed settings).",
    "2. Run `synt outcome enable` once in a terminal: it shows the integrity notice, backs the session store up and migrates it.",
    "Then press r here. This window does not turn outcomes on.",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    List,
    Centre,
    Inspector,
}

impl Pane {
    fn step(self, back: bool) -> Self {
        const ALL: [Pane; 3] = [Pane::List, Pane::Centre, Pane::Inspector];
        let i = ALL.iter().position(|p| *p == self).unwrap_or(0);
        ALL[(i + if back { 2 } else { 1 }) % 3]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CentreTab {
    Goal,
    Decisions,
    History,
}

impl CentreTab {
    pub const ALL: [CentreTab; 3] = [Self::Goal, Self::Decisions, Self::History];
    pub fn label(self) -> &'static str {
        match self {
            Self::Goal => "Goal",
            Self::Decisions => "Decisions",
            Self::History => "History",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Loading,
    /// `synt.orchestrator.enabled` is false.
    Off,
    /// On, but the store is not migrated (or the owner says it is not ready).
    NotReady(String),
    /// Native Windows: attempts need an OS sandbox (`WSL2_REFUSAL`).
    Windows,
    /// The owner cannot be reached, or the link failed; the text is the error.
    Unreachable(String),
    Ready,
}

/// One row of the list: the server's row and its budget (unknown when the server did not say).
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub row: OutcomeRow,
    pub budget: BudgetView,
}

/// What the centre tabs show for the selected outcome. A part the server refused is its error text.
#[derive(Clone, Debug, PartialEq)]
pub struct Detail {
    pub outcome: String,
    pub cards: Result<Vec<DecisionCard>, String>,
    pub contract: Result<CriteriaView, String>,
    /// `InspectorContent::Activity` of every event read, or `Unavailable`.
    pub history: InspectorContent,
}

/// Which brief a picker belongs to: a card is the same one only while all three match.
type CardKey = (String, i64, String);

fn key_of(c: &DecisionCard) -> CardKey {
    (c.id.clone(), c.revision, c.brief_sha256.clone())
}

pub struct State {
    pub screen: Screen,
    pub items: Vec<Item>,
    pub selected: Option<String>,
    pub detail: Option<Detail>,
    pub inspector: Option<InspectorContent>,
    pub pane: Pane,
    pub centre: CentreTab,
    pub itab: InspectorTab,
    /// Index of the expanded decision card.
    pub active: usize,
    /// The answer cursor of the expanded card (starts on nothing), `None` when it cannot be answered.
    pub picker: Option<Picker>,
    seen: Option<CardKey>,
    /// Enter and "Send answer" do nothing before this (ms): a key typed ahead of the card answers nothing.
    pub armed_until: i64,
    /// An answer is on its way: no second one.
    pub sending: bool,
    /// One line for the footer: what the last answer or refresh said.
    pub notice: Option<String>,
}

/// A key, with the modifiers that matter (any other modifier means it is not ours).
pub struct Key<'a> {
    pub name: &'a str,
    pub shift: bool,
    pub other_mod: bool,
}

/// What the view must do for the state; the view turns these into link requests.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Refresh,
    /// Run the connection check again (the Off, Not ready and Unreachable screens).
    Retry,
    Select(String),
    Inspect(InspectorTab),
    Resolve {
        outcome: String,
        card: Box<DecisionCard>,
        choice: String,
    },
    /// Scroll the focused pane by this many lines (negative: up).
    Scroll(i32),
}

impl State {
    pub fn new(screen: Screen) -> Self {
        Self {
            screen,
            items: vec![],
            selected: None,
            detail: None,
            inspector: None,
            pane: Pane::List,
            centre: CentreTab::Decisions,
            itab: InspectorTab::Overview,
            active: 0,
            picker: None,
            seen: None,
            armed_until: 0,
            sending: false,
            notice: None,
        }
    }

    pub fn cards(&self) -> &[DecisionCard] {
        match self.detail.as_ref().map(|d| &d.cards) {
            Some(Ok(c)) => c,
            _ => &[],
        }
    }

    pub fn active_card(&self) -> Option<&DecisionCard> {
        self.cards().get(self.active)
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.items
            .iter()
            .find(|i| Some(&i.row.id) == self.selected.as_ref())
    }

    /// Enter and "Send answer" would send now (a choice, armed, nothing in flight).
    pub fn can_send(&self, now: i64) -> bool {
        !self.sending
            && now >= self.armed_until
            && self.picker.as_ref().and_then(Picker::choice).is_some()
    }

    // ------------------------------------------------------------------------- incoming data

    /// A fresh list (and the detail and inspector of the selected outcome). Detail and inspector of
    /// another outcome than the selected one are dropped: they arrived late.
    pub fn apply_snapshot(
        &mut self,
        items: Vec<Item>,
        selected: Option<String>,
        detail: Option<Detail>,
        inspector: Option<InspectorContent>,
        now: i64,
    ) {
        self.screen = Screen::Ready;
        self.items = items;
        // the person's selection stands while the outcome exists; else the server's pick
        if !self
            .items
            .iter()
            .any(|i| Some(&i.row.id) == self.selected.as_ref())
        {
            self.selected = selected;
        }
        if detail.as_ref().map(|d| &d.outcome) == self.selected.as_ref() {
            self.detail = detail;
            // a snapshot made for another tab than the one open now is late
            self.inspector = inspector.filter(|c| c.tab() == self.itab);
        }
        self.sync_picker(now);
    }

    /// The inspector tab of the selected outcome (it was asked for).
    pub fn apply_inspector(&mut self, outcome: &str, c: InspectorContent) {
        if self.selected.as_deref() == Some(outcome) && c.tab() == self.itab {
            self.inspector = Some(c);
        }
    }

    pub fn answered(&mut self, note: String) {
        self.sending = false;
        // whatever came of it, the next answer is a new choice (and the refetch is not "a card that changed")
        self.seen = None;
        self.notice = Some(note);
    }

    /// Keeps the picker for the expanded card while the card is the same brief; a new card (other id, a new
    /// revision or brief hash) gets a picker with the cursor on nothing and the arming delay again.
    fn sync_picker(&mut self, now: i64) {
        let len = self.cards().len();
        if self.active >= len {
            self.active = 0;
        }
        let Some(card) = self.active_card().cloned() else {
            self.picker = None;
            self.seen = None;
            return;
        };
        let key = key_of(&card);
        if self.seen.as_ref() == Some(&key) && self.picker.is_some() == card.resolvable() {
            return;
        }
        if matches!(&self.seen, Some(s) if s.0 == card.id) {
            self.notice = Some(
                "This decision changed since you saw it: the new version is shown and nothing was sent. Read it again, then choose."
                    .into(),
            );
        }
        self.picker = Picker::new(&card);
        self.seen = Some(key);
        self.armed_until = now + ARM_MS;
    }

    /// Expand card `i` (a click, `[` or `]`).
    pub fn show_card(&mut self, i: usize, now: i64) {
        if i < self.cards().len() && i != self.active {
            self.active = i;
            self.seen = None;
            self.notice = None;
            self.sync_picker(now);
        }
    }

    // ------------------------------------------------------------------------------ input

    /// Moves the answer cursor to option `i` (a click, a number): it chooses nothing yet. Out of range: unchanged.
    pub fn choose(&mut self, i: usize) {
        if let Some(p) = self.picker.as_mut().filter(|p| i < p.options.len()) {
            p.move_to(i);
            self.notice = None;
        }
    }

    pub fn select(&mut self, i: usize) -> Vec<Effect> {
        let Some(id) = self.items.get(i).map(|i| i.row.id.clone()) else {
            return vec![];
        };
        if self.selected.as_ref() == Some(&id) {
            return vec![];
        }
        self.selected = Some(id.clone());
        self.detail = None;
        self.inspector = None;
        self.active = 0;
        self.picker = None;
        self.seen = None;
        self.notice = None;
        vec![Effect::Select(id)]
    }

    pub fn set_itab(&mut self, t: InspectorTab) -> Vec<Effect> {
        if self.itab == t {
            return vec![];
        }
        self.itab = t;
        self.inspector = None;
        vec![Effect::Inspect(t)]
    }

    /// Enter or "Send answer": the answer of the expanded card, when it may be sent; else why not.
    pub fn send(&mut self, now: i64) -> Vec<Effect> {
        let (Some(card), Some(p), Some(outcome)) = (
            self.active_card(),
            self.picker.as_ref(),
            self.selected.as_ref(),
        ) else {
            return vec![];
        };
        let Some(choice) = p.choice().map(str::to_string) else {
            self.notice = Some(
                "Nothing is chosen. Choose an answer (arrows, 1-9 or a click), then press Enter to send it."
                    .into(),
            );
            return vec![];
        };
        if self.sending {
            return vec![];
        }
        if now < self.armed_until {
            self.notice =
                Some("The decision has just appeared: look at it, then press Enter again.".into());
            return vec![];
        }
        let out = Effect::Resolve {
            outcome: outcome.clone(),
            card: Box::new(card.clone()),
            choice,
        };
        self.sending = true;
        self.notice = Some("Sending the answer...".into());
        vec![out]
    }

    /// A key. `None`: not ours (the window may use it).
    pub fn key(&mut self, k: &Key, now: i64) -> Option<Vec<Effect>> {
        if k.other_mod {
            return None;
        }
        if self.screen != Screen::Ready {
            return match (&self.screen, k.name) {
                (Screen::Loading | Screen::Windows, _) => None,
                (_, "r" | "enter") => Some(vec![Effect::Retry]),
                _ => None,
            };
        }
        match k.name {
            "tab" => {
                self.pane = self.pane.step(k.shift);
                return Some(vec![]);
            }
            "r" if !k.shift => return Some(vec![Effect::Refresh]),
            _ => {}
        }
        let digit = k.name.parse::<usize>().ok().filter(|n| (1..=9).contains(n));
        Some(match self.pane {
            Pane::List => self.list_key(k),
            Pane::Centre => match (self.centre, k.name) {
                (_, "left" | "right") => {
                    let all = CentreTab::ALL;
                    let i = all.iter().position(|t| *t == self.centre).unwrap_or(0);
                    self.centre = all[if k.name == "left" {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(2)
                    }];
                    vec![]
                }
                (CentreTab::Decisions, "up" | "down") => {
                    if let Some(p) = &self.picker {
                        let n = p.options.len();
                        let to = match (p.cursor(), k.name) {
                            (None, "down") => 0,
                            (None, _) => n - 1,
                            (Some(c), "down") => (c + 1).min(n - 1),
                            (Some(c), _) => c.saturating_sub(1),
                        };
                        self.choose(to);
                    }
                    vec![]
                }
                (CentreTab::Decisions, "enter") => self.send(now),
                (CentreTab::Decisions, "escape")
                    if self.picker.as_ref().is_some_and(|p| p.cursor().is_some()) =>
                {
                    // clears the choice: a new picker, the cursor on nothing
                    self.seen = None;
                    self.sync_picker(now);
                    vec![]
                }
                (CentreTab::Decisions, "[" | "]") => {
                    let n = self.cards().len();
                    if n > 0 {
                        let to = if k.name == "[" {
                            (self.active + n - 1) % n
                        } else {
                            (self.active + 1) % n
                        };
                        self.show_card(to, now);
                    }
                    vec![]
                }
                (CentreTab::Decisions, _) if digit.is_some() => {
                    self.choose(digit.unwrap_or(1) - 1);
                    vec![]
                }
                (_, "escape") => {
                    self.pane = Pane::List;
                    vec![]
                }
                (_, "up") => vec![Effect::Scroll(-1)],
                (_, "down") => vec![Effect::Scroll(1)],
                (_, "pageup") => vec![Effect::Scroll(-12)],
                (_, "pagedown") => vec![Effect::Scroll(12)],
                _ => return None,
            },
            Pane::Inspector => {
                let all = InspectorTab::ALL;
                let i = all.iter().position(|t| *t == self.itab).unwrap_or(0);
                match (k.name, digit) {
                    ("left", _) => self.set_itab(all[i.saturating_sub(1)]),
                    ("right", _) => self.set_itab(all[(i + 1).min(all.len() - 1)]),
                    (_, Some(n)) if n <= all.len() => self.set_itab(all[n - 1]),
                    ("escape", _) => {
                        self.pane = Pane::List;
                        vec![]
                    }
                    ("up", _) => vec![Effect::Scroll(-1)],
                    ("down", _) => vec![Effect::Scroll(1)],
                    ("pageup", _) => vec![Effect::Scroll(-12)],
                    ("pagedown", _) => vec![Effect::Scroll(12)],
                    _ => return None,
                }
            }
        })
    }

    fn list_key(&mut self, k: &Key) -> Vec<Effect> {
        let at = self
            .items
            .iter()
            .position(|i| Some(&i.row.id) == self.selected.as_ref());
        match k.name {
            "up" => self.select(at.map_or(0, |i| i.saturating_sub(1))),
            "down" => self.select(at.map_or(0, |i| i + 1)),
            "enter" | "right" => {
                self.pane = Pane::Centre;
                vec![]
            }
            _ => vec![],
        }
    }
}

/// The activity lines of `c`, if it is the Activity content.
pub fn lines_of(c: &InspectorContent) -> Option<&ActivityTab> {
    match c {
        InspectorContent::Activity(a) => Some(a),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
