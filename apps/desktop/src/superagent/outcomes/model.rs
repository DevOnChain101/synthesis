//! The Super Agent view's state and its key handling, with no window in it: what is shown comes from the
//! `synthesis_view::outcomes` models the link fetched, and a key or click only moves a cursor or asks for an
//! `Effect`. Nothing here computes a state, a verdict or a brief (ADR-021).
//!
//! A decision is answered in two steps by two deliberate acts: the person moves the cursor to an answer
//! (arrows, a number or a click; it starts on nothing), then presses Enter or "Send answer". Enter does
//! nothing while the card is new (the arming delay of the approval dock) and nothing is sent for a card
//! that changed in between: a new revision or brief hash is a new card with the cursor on nothing again.

use synthesis_core::outcome::Contract;
use synthesis_view::outcomes::{
    ActivityTab, BudgetView, CriteriaView, DecisionCard, InspectorContent, InspectorTab,
    MessageRow, OutcomeRow, Queue, Record,
};

use super::ask::Ask;
use super::form::{Form, FormText, Review, Stage, Submit};
use super::link::FormMsg;

/// What the Off screen says above its button. It is opt-in and nothing happens before two explicit confirms.
pub const OFF_LINES: [&str; 3] = [
    "Super Agent is off. Nothing here runs, and nothing is started or migrated until you confirm it.",
    "Experimental: Super Agent uses the stub planner, and its workers run with your configured provider and cost tokens.",
    "Turning it on takes two steps, each with its own confirmation: the setting synt.orchestrator.enabled in your user settings, then the one-time migration of the session store (a backup is made first).",
];

/// What the not-migrated screen says above its button.
pub const MIGRATE_LINES: [&str; 2] = [
    "The setting is on, but the session store is not migrated yet. The migration is its own step: it makes a backup first, then moves the store to schema v5.",
    "Releases without outcomes refuse a migrated store.",
];

/// The step of the guided enable flow a disclosure is open for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Write `synt.orchestrator.enabled` to the user settings.
    Setting,
    /// The explicit store migration (`OutcomeClient::enable`).
    Migrate,
}

/// A button of the disclosure; the keys move between them, and nothing starts focused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Btn {
    Cancel,
    Confirm,
}

/// The open disclosure of the guided flow. Consent is never kept: closing it, or a step that ends, drops it.
#[derive(Clone, Debug, PartialEq)]
pub struct Guide {
    pub step: Step,
    pub focus: Option<Btn>,
    /// The confirmed step is on its way (a migration waits for the orchestrator): no key or click acts.
    pub busy: bool,
    pub error: Option<String>,
    /// The number of the confirm this dialog sent (0: none yet).
    pub seq: u64,
}

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

/// The `approvals.queue` read and the newest recorded answers, as `(outcome, record)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueueData {
    pub queue: Queue,
    pub answered: Vec<(String, Record)>,
}

/// What the centre tabs show for the selected outcome. A part the server refused is its error text.
#[derive(Clone, Debug, PartialEq)]
pub struct Detail {
    pub outcome: String,
    pub cards: Result<Vec<DecisionCard>, String>,
    pub contract: Result<CriteriaView, String>,
    /// The current contract as the core reads it (what "Revise contract" starts from); `None` when the
    /// server's contract is not one this build can read in full.
    pub current: Option<Contract>,
    /// `InspectorContent::Activity` of every event read, or `Unavailable`.
    pub history: InspectorContent,
    /// `InspectorContent::Workers` (the plan with each item's attempts), or `Unavailable` with the reason.
    pub workers: InspectorContent,
    /// The person's lines to the workers of this outcome, from its events and the workers' acks.
    pub messages: Vec<MessageRow>,
}

pub struct State {
    pub screen: Screen,
    /// The engine advertised `read_only_goal` and `report_review` (`orchestrator.status` features).
    pub read_only_goal: bool,
    /// The engine advertised `attempt_message` (the person may message a worker).
    pub attempt_message: bool,
    /// `None`: the engine has no `approvals_queue` (or the read failed): one row per outcome, as before.
    pub queue: Option<QueueData>,
    pub items: Vec<Item>,
    pub selected: Option<String>,
    pub detail: Option<Detail>,
    pub inspector: Option<InspectorContent>,
    pub pane: Pane,
    pub centre: CentreTab,
    pub itab: InspectorTab,
    /// Index of the expanded decision card.
    pub active: usize,
    /// The answer cursor, arming delay, in-flight flag and footer notice of the expanded card.
    pub ask: Ask,
    /// Replies to answers so far (`answered` calls): the TUI view's popup waits for this to grow.
    pub answers: u64,
    /// The folder a new outcome works in by default (the app's workspace).
    pub workspace: String,
    /// Folders the app already knows (the TUI view's workspaces, newest first): what the form's picker lists.
    pub known: Vec<String>,
    /// The engine is on another machine: a local folder dialog would name a path it cannot use.
    pub remote: bool,
    /// The picker's list is open, with the cursor on an item (`ws_items().len()` is "Choose folder...").
    pub ws_menu: Option<usize>,
    /// The open kind picker (`+ new`, `n`).
    pub picker: Option<super::kind_picker::Picker>,
    /// The open "New outcome" or "Revise contract" form.
    pub form: Option<Form>,
    /// The open disclosure of the Off / not-migrated screens.
    pub guide: Option<Guide>,
    /// Why this build cannot turn Super Agent on (the enable gate is closed): the guide then shows it instead of the confirm.
    pub gate_refusal: Option<String>,
    /// Migrations the person has confirmed so far (the tag of the next one).
    pub migrations: u64,
    /// What the screen says about a migration whose dialog is gone (a late reply).
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
    /// A new session: the view makes its folder (a scratch one, or the person's choice) and the shell opens it.
    NewSession,
    /// Read `HEAD` of this folder (the form's base commit).
    Head(String),
    Submit(Submit),
    Approve {
        outcome: String,
        version: u32,
        hash: String,
        key: String,
    },
    /// Read the stored, unapproved contract of this outcome for review.
    Review(String),
    /// The person confirmed the disclosure: write `synt.orchestrator.enabled` (user settings).
    EnableSetting,
    /// The person confirmed the disclosure: migrate the store (`OutcomeClient::enable`); the number tags the
    /// request, so a reply of an older confirm is never taken for the open dialog's.
    Migrate(u64),
}

impl State {
    pub fn new(screen: Screen) -> Self {
        Self {
            screen,
            read_only_goal: false,
            attempt_message: false,
            queue: None,
            items: vec![],
            selected: None,
            detail: None,
            inspector: None,
            pane: Pane::List,
            centre: CentreTab::Decisions,
            itab: InspectorTab::Overview,
            active: 0,
            ask: Ask::default(),
            answers: 0,
            workspace: String::new(),
            known: vec![],
            remote: false,
            ws_menu: None,
            picker: None,
            form: None,
            guide: None,
            gate_refusal: synthesis_app::outcome_policy::gate_refusal().map(String::from),
            migrations: 0,
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
        self.ask.can_send(now)
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
        self.notice = None;
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

    /// A screen other than the list (the link ended, a retry, a recheck). An answer that was in flight is
    /// not resent and not waited for: the send flag goes, and the person looks at the fresh card first.
    pub fn set_screen(&mut self, screen: Screen) {
        // the choice made before is gone: after a reconnect the card is a fresh one
        self.ask.link_changed();
        // a disclosure belongs to the screen that opened it
        let open = matches!(
            (self.guide.as_ref().map(|g| g.step), &screen),
            (Some(Step::Setting), Screen::Off) | (Some(Step::Migrate), Screen::NotReady(_))
        );
        if !open {
            self.guide = None;
        }
        self.screen = screen;
    }

    pub fn answered(&mut self, note: String) {
        self.answers += 1;
        self.ask.answered(note);
    }

    /// Keeps the picker for the expanded card while the card is the same brief; a new card (other id, a new
    /// revision or brief hash) gets a picker with the cursor on nothing and the arming delay again.
    fn sync_picker(&mut self, now: i64) {
        let len = self.cards().len();
        if self.active >= len {
            self.active = 0;
        }
        let card = self.cards().get(self.active).cloned();
        self.ask.sync(card.as_ref(), now);
    }

    /// Expand card `i` (a click, `[` or `]`).
    pub fn show_card(&mut self, i: usize, now: i64) {
        if i < self.cards().len() && i != self.active {
            self.active = i;
            self.ask.forget();
            self.ask.notice = None;
            self.sync_picker(now);
        }
    }

    // ------------------------------------------------------------------------------ the form

    /// "New outcome": the form with the app's workspace; its base commit is read at once.
    pub fn open_new(&mut self) -> Vec<Effect> {
        self.ws_menu = None;
        let mut f = Form::new_outcome(&self.workspace);
        let ws = self.workspace.trim().to_string();
        let eff = (!ws.is_empty()).then(|| {
            f.resolving(&ws);
            Effect::Head(ws)
        });
        self.form = Some(f);
        eff.into_iter().collect()
    }

    /// "Revise contract" of the selected outcome, from its current contract and the revision shown now.
    pub fn open_revise(&mut self) -> Vec<Effect> {
        let cur = self
            .detail
            .as_ref()
            .filter(|d| Some(&d.outcome) == self.selected.as_ref());
        let (Some(item), Some(prev)) = (self.selected_item(), cur.and_then(|d| d.current.clone()))
        else {
            self.ask.notice = Some(
                "The contract is not loaded, or is not one this window can read in full, so it cannot be revised here."
                    .into(),
            );
            return vec![];
        };
        self.form = Some(Form::revise(&item.row.id.clone(), item.row.revision, prev));
        vec![]
    }

    /// "Review and approve": reads the stored contract of an outcome whose contract is not approved yet.
    pub fn review_draft(&mut self) -> Vec<Effect> {
        let waiting = matches!(
            self.detail.as_ref().map(|d| &d.contract),
            Some(Ok(c)) if c.approved == Some(false)
        );
        match (&self.selected, waiting) {
            (Some(id), true) => {
                self.form = Some(Form::reading(id));
                vec![Effect::Review(id.clone())]
            }
            _ => {
                self.ask.notice =
                    Some("The contract of this outcome is not waiting for approval.".into());
                vec![]
            }
        }
    }

    pub fn close_form(&mut self) {
        self.ws_menu = None;
        self.form = None;
    }

    /// The picker's folders: the app's workspace (the default) first, then the other known ones, no repeats.
    pub fn ws_items(&self) -> Vec<String> {
        let mut v: Vec<String> = Some(self.workspace.trim())
            .filter(|w| !w.is_empty())
            .map(String::from)
            .into_iter()
            .collect();
        for k in &self.known {
            if !v.contains(k) {
                v.push(k.clone());
            }
        }
        v
    }

    /// Items the open list has: the folders, then "Choose folder..." (not for a remote engine).
    pub fn ws_len(&self) -> usize {
        self.ws_items().len() + usize::from(!self.remote)
    }

    /// A folder was picked (from the list or the native dialog): the list closes and the base commit of the new
    /// folder is read; the owner process still validates the path when the form is sent.
    pub fn pick_workspace(&mut self, path: &str) -> Vec<Effect> {
        self.ws_menu = None;
        self.workspace_left(&FormText {
            workspace: path.to_string(),
            ..Default::default()
        })
    }

    /// The workspace field was left: the base commit is read again when the folder changed.
    pub fn workspace_left(&mut self, t: &FormText) -> Vec<Effect> {
        let Some(f) = self.form.as_mut() else {
            return vec![];
        };
        match f.needs_head(t) {
            Some(ws) => {
                f.resolving(&ws);
                vec![Effect::Head(ws)]
            }
            None => vec![],
        }
    }

    /// Confirm on the edit stage: the request, or the errors inline.
    pub fn submit(&mut self, t: &FormText) -> Vec<Effect> {
        if let Some(o) = self.form.as_mut().and_then(Form::retry_read) {
            return vec![Effect::Review(o)];
        }
        match self.form.as_mut().and_then(|f| f.submit(t)) {
            Some(s) => vec![Effect::Submit(s)],
            None => vec![],
        }
    }

    /// Confirm on the review stage: approves the version and hash the server reported (never others).
    pub fn approve(&mut self) -> Vec<Effect> {
        match self.form.as_mut().and_then(Form::approve) {
            Some((outcome, version, hash, key)) => vec![Effect::Approve {
                outcome,
                version,
                hash,
                key,
            }],
            None => vec![],
        }
    }

    /// What the worker said about the form. A reply for a form that was closed is dropped (the list refresh
    /// that follows a created outcome still shows it).
    pub fn apply_form(&mut self, m: FormMsg) {
        match m {
            FormMsg::Approved(note) => {
                self.form = None;
                self.ask.notice = Some(note);
            }
            m => {
                let Some(f) = self.form.as_mut() else { return };
                match m {
                    FormMsg::Head { ws, result } => f.head_known(&ws, result),
                    FormMsg::Failed { text, spent } => f.failed(text, spent),
                    FormMsg::BaseMoved { ws, commit, text } => f.base_moved(&ws, commit, text),
                    FormMsg::Review(r) => f.reviewing(*r),
                    FormMsg::Approved(_) => {}
                }
            }
        }
    }

    pub fn form_review(&self) -> Option<&Review> {
        match self.form.as_ref().map(|f| &f.stage) {
            Some(Stage::Review(r) | Stage::Approving(r)) => Some(r),
            _ => None,
        }
    }

    // ------------------------------------------------------------------------------ input

    /// Moves the answer cursor to option `i` (a click, a number): it chooses nothing yet. Out of range: unchanged.
    pub fn choose(&mut self, i: usize) {
        self.ask.choose(i);
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
        self.ask.clear();
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
        let (Some(card), Some(outcome)) = (
            self.cards().get(self.active).cloned(),
            self.selected.clone(),
        ) else {
            return vec![];
        };
        self.ask.send(&outcome, &card, now).into_iter().collect()
    }

    // ------------------------------------------------------------------------- the guided enable flow

    /// The screen's button: opens the disclosure for the step the screen is at. Nothing is focused, nothing
    /// is written or sent.
    pub fn open_guide(&mut self) {
        let step = match self.screen {
            Screen::Off => Step::Setting,
            Screen::NotReady(_) => Step::Migrate,
            _ => return,
        };
        self.guide = Some(Guide {
            step,
            focus: None,
            busy: false,
            // a closed gate: the dialog says why, and has no confirm
            error: self.gate_refusal.clone(),
            seq: 0,
        });
        self.notice = None;
    }

    /// Esc, Cancel: closes the dialog, also while a confirmed step is on its way (that request is not recalled;
    /// its reply is still applied). So a lost reply can never keep the person in the dialog.
    pub fn close_guide(&mut self) {
        self.guide = None;
    }

    /// The Confirm button (a click, or Enter with the button focused): the step's effect, once.
    pub fn confirm_guide(&mut self) -> Vec<Effect> {
        match self.guide.as_mut() {
            Some(g) if !g.busy && self.gate_refusal.is_none() => {
                g.busy = true;
                g.error = None;
                vec![match g.step {
                    Step::Setting => Effect::EnableSetting,
                    Step::Migrate => {
                        self.migrations += 1;
                        g.seq = self.migrations;
                        Effect::Migrate(g.seq)
                    }
                }]
            }
            _ => vec![],
        }
    }

    /// The reply of confirm `seq`. For the open dialog it ends or fails the step; with no dialog (closed
    /// meanwhile) or a newer one open, it only says on the screen what happened, never in that dialog.
    pub fn enabled(&mut self, seq: u64, r: Result<(), String>) {
        let live = self
            .guide
            .as_ref()
            .is_some_and(|g| g.step == Step::Migrate && g.seq == seq);
        match (live, r) {
            (true, Ok(())) => {
                self.guide = None;
                self.set_screen(Screen::Loading);
            }
            (true, Err(e)) => self.guide_failed(e),
            (false, Ok(())) => {
                self.notice = Some("Migration was started and completed; checking...".into());
                self.set_screen(Screen::Loading);
            }
            (false, Err(e)) => {
                self.notice = Some(format!("Migration was started and reported: {e}"))
            }
        }
    }

    /// The step failed: the dialog stays open with the (sanitised when drawn) reason.
    pub fn guide_failed(&mut self, why: String) {
        if let Some(g) = self.guide.as_mut() {
            g.busy = false;
            g.error = Some(why);
        }
    }

    /// The step is done: the disclosure is gone, and the link looks again.
    pub fn guide_done(&mut self) {
        self.guide = None;
    }

    /// Keys while the disclosure is open: all of them are its own. Tab moves between Cancel and Confirm (the
    /// first Tab lands on Cancel), Enter acts on the focused button only, Esc cancels.
    fn guide_key(&mut self, name: &str) -> Vec<Effect> {
        let Some(g) = self.guide.as_mut() else {
            return vec![];
        };
        if g.busy {
            // only Esc acts while a step is on its way
            if name == "escape" {
                self.guide = None;
            }
            return vec![];
        }
        match (name, g.focus) {
            ("escape", _) => self.guide = None,
            ("tab", Some(Btn::Cancel)) => g.focus = Some(Btn::Confirm),
            ("tab", _) => g.focus = Some(Btn::Cancel),
            ("enter", Some(Btn::Confirm)) => return self.confirm_guide(),
            ("enter", Some(Btn::Cancel)) => self.guide = None,
            _ => {}
        }
        vec![]
    }

    /// A key. `None`: not ours (the window may use it).
    pub fn key(&mut self, k: &Key, now: i64) -> Option<Vec<Effect>> {
        if k.other_mod || self.form.is_some() {
            return None;
        }
        if self.guide.is_some() {
            return Some(self.guide_key(k.name));
        }
        if self.picker.is_some() {
            return Some(self.picker_key(k.name));
        }
        if self.screen != Screen::Ready {
            return match (&self.screen, k.name) {
                // a session needs no orchestrator: the picker also opens on native Windows (goals are refused there)
                (Screen::Windows, "n") if !k.shift => {
                    self.open_picker();
                    Some(vec![])
                }
                (Screen::Loading | Screen::Windows, _) => None,
                (Screen::Off | Screen::NotReady(_), "enter") => {
                    self.open_guide();
                    Some(vec![])
                }
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
            "n" if !k.shift => {
                self.open_picker();
                return Some(vec![]);
            }
            "v" if !k.shift => return Some(self.open_revise()),
            "a" if !k.shift => return Some(self.review_draft()),
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
                    self.ask.step(k.name == "down");
                    vec![]
                }
                (CentreTab::Decisions, "enter") => self.send(now),
                (CentreTab::Decisions, "escape")
                    if self
                        .ask
                        .picker
                        .as_ref()
                        .is_some_and(|p| p.cursor().is_some()) =>
                {
                    // clears the choice: a new picker, the cursor on nothing
                    self.ask.forget();
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
