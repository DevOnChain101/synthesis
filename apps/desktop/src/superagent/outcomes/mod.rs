//! The data and link owner of the Super Agent view (owner decisions 2026-11-17/18, 2026-11-25; `docs/specs/desktop.md`
//! "Super Agent tab", `docs/specs/super-agent.md`, ADR-021). The module (formerly `outcomes_view`) keeps "outcomes"
//! (the thing is the outcome; only the tab is called Super Agent). The old three-column outcomes UI is deleted: the
//! Super Agent frames in `superagent/` draw everything, and this module holds the state (`model`), the worker thread
//! (`link`, the only way to the orchestrator: a surface in the sense of ADR-021, depending on no orchestrator crate),
//! the contract form model (`form`), the decision card rules (`ask`, `popup`) and the enable dialog (`guide_view`).
//! This is not the Conductor (`docs/specs/conductor.md`).

use futures::StreamExt as _;
use gpui::{
    AnyElement, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement as _,
    ParentElement as _, ScrollHandle, Styled as _, Task, Window, div, point, px,
};
use std::ops::Not as _;
use synthesis_config::Env;

use crate::live::now_ms;
use link::{Link, Msg, Req};
use model::{Effect, Key, Pane, Screen, State};

pub mod ask;
pub mod form;
mod guide_view;
pub mod history;
pub mod kind_picker;
pub mod link;
pub mod model;
pub mod popup;
pub(crate) mod render;
pub mod sample;
#[cfg(feature = "shot")]
pub mod shot;

/// The notice while an answer of the decision card is on its way.
pub const SENDING: &str = "Sending the answer...";

pub struct OutcomesView {
    pub state: State,
    focus: FocusHandle,
    /// `None` for the sample (nothing is sent) and until the tab is first opened.
    link: Option<Link>,
    /// What the link is started with; `None`: the sample.
    env: Option<Env>,
    started: bool,
    pump: Option<Task<()>>,
    scroll: [ScrollHandle; 3],
    /// What waits for the person: the decision card docked under the centre tabs, Alt+A, the title bar's badge.
    pub(crate) pop: popup::Wire,
    /// The list column is shown (the title bar's rail button hides it); kept for this window only.
    pub(crate) rail_open: bool,
    /// The shell shows this view (or Settings over it). While not, it never moves the window's focus.
    visible: bool,
    /// Back and forward through the outcomes shown (the title bar's buttons, Alt+Left / Alt+Right).
    hist: history::History,
}

/// The picker made a folder for a new session: the shell opens it as a session tab of the Super Agent view.
pub struct OpenFolder(pub std::path::PathBuf);

impl gpui::EventEmitter<OpenFolder> for OutcomesView {}

/// What became of a worker message (`OutcomesView::send_message`): the key is the one it was sent with.
pub struct MessageSent {
    pub key: String,
    pub result: Result<link::Delivery, String>,
}

impl gpui::EventEmitter<MessageSent> for OutcomesView {}

impl OutcomesView {
    /// The live tab: it connects (and checks the setting) when it is first shown, never at launch.
    pub fn live(env: Env, workspace: String, cx: &mut Context<Self>) -> Self {
        let mut st = State::new(Screen::Loading);
        st.workspace = workspace;
        let mut v = Self::with_state(st, cx);
        v.env = Some(env);
        v
    }

    /// `--sample`: the sample outcomes, no link.
    pub fn sample(cx: &mut Context<Self>) -> Self {
        Self::with_state(sample::state(), cx)
    }

    pub fn with_state(state: State, cx: &mut Context<Self>) -> Self {
        // what is selected at the start is the first place of the history
        let mut hist = history::History::default();
        if let Some(id) = &state.selected {
            hist.visit(history::Place::Outcome(id.clone()));
        }
        Self {
            state,
            focus: cx.focus_handle(),
            link: None,
            env: None,
            started: false,
            pump: None,
            scroll: Default::default(),
            pop: popup::Wire::new(cx),
            rail_open: true,
            visible: true,
            hist,
        }
    }

    /// The frames' tests give the view a link whose requests they can read.
    #[cfg(test)]
    pub(crate) fn set_link(&mut self, l: Link) {
        self.link = Some(l);
    }

    /// The tab was opened: the keys go to it, and it connects the first time (else it looks again).
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        match (&self.link, self.started) {
            (None, false) => self.start(cx),
            (Some(l), _) if self.state.screen == Screen::Ready => l.send(Req::Refresh),
            _ => {}
        }
    }

    /// The title bar's badge needs the list without this tab being opened: it connects now. Only called after
    /// `approvals::gate` said outcomes are on and supported.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if !self.started {
            self.start(cx);
        }
    }

    /// Selects outcome `id` (Alt+A, back and forward); the tab shows it from then on.
    pub fn select_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.state.items.iter().position(|i| i.row.id == id) {
            let e = self.state.select(i);
            self.run(e, cx);
        }
    }

    /// The person's line to the worker of `item`, typed in its frame and sent on Enter there. `MessageSent`
    /// follows with the same `key`. Only the worker frame calls this (a source test keeps it so); the text is
    /// never logged, and the sample sends nothing.
    pub(crate) fn send_message(
        &mut self,
        outcome: &str,
        item: &str,
        text: String,
        key: String,
        cx: &mut Context<Self>,
    ) {
        let failed = match &self.link {
            None => Some("Sample data: nothing was sent.".to_string()),
            Some(l) => l
                .try_send(Req::Message {
                    outcome: outcome.into(),
                    item: item.into(),
                    text,
                    key: key.clone(),
                })
                .not()
                .then(|| {
                    "The link to the orchestrator has ended. Press r on the home to check again."
                        .to_string()
                }),
        };
        if let Some(e) = failed {
            cx.emit(MessageSent {
                key,
                result: Err(e),
            });
        }
    }

    /// Sends an effect the docked decision card made (the same worker path as the Decisions tab's own answer).
    pub fn send_effect(&mut self, e: Effect, cx: &mut Context<Self>) {
        // the reply (`Msg::Answered`) replaces this line: that is how the card sees the answer came back; in
        // flight for the Decisions tab too: its own Enter sends nothing until the reply (`State::answered`) comes back
        self.state.ask.sending = true;
        self.state.ask.notice = Some(SENDING.into());
        self.run(vec![e], cx);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.started = true;
        let Some(env) = self.env.clone() else { return };
        let (link, mut rx) = Link::start(env);
        self.link = Some(link);
        self.pump = Some(cx.spawn(async move |this, cx| {
            while let Some(m) = rx.next().await {
                let mut batch = vec![m];
                while let Ok(m) = rx.try_recv() {
                    batch.push(m);
                }
                if this.update(cx, |v, cx| v.apply(batch, cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn apply(&mut self, batch: Vec<Msg>, cx: &mut Context<Self>) {
        let now = now_ms();
        for m in batch {
            match m {
                Msg::Screen(s) => {
                    // a failed or restarted link: what that engine advertised no longer holds
                    self.state.read_only_goal = false;
                    self.state.attempt_message = false;
                    self.state.queue = None;
                    self.state.set_screen(s);
                }
                Msg::ReadOnlyGoal(on) => self.state.read_only_goal = on,
                Msg::AttemptMessage(on) => self.state.attempt_message = on,
                Msg::Sent { key, result } => cx.emit(MessageSent { key, result }),
                Msg::Queue(q) => self.state.queue = q.map(|q| *q),
                Msg::Snapshot {
                    items,
                    selected,
                    detail,
                    inspector,
                } => self
                    .state
                    .apply_snapshot(items, selected, detail.map(|d| *d), inspector, now),
                Msg::Inspector { outcome, content } => {
                    self.state.apply_inspector(&outcome, content)
                }
                Msg::Answered(note) => self.state.answered(note),
                Msg::Form(m) => self.state.apply_form(m),
                Msg::Enabled(n, r) => self.state.enabled(n, r),
            }
        }
        self.note_selection();
        cx.notify();
    }

    /// Does what the state asked for. The sample answers locally (and sends nothing).
    fn run(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for e in effects {
            if e == Effect::EnableSetting {
                self.enable_setting();
                continue;
            }
            match (e, &self.link) {
                (Effect::Scroll(n), _) => scroll(
                    &self.scroll[match self.state.pane {
                        Pane::List => 0,
                        Pane::Centre => 1,
                        Pane::Inspector => 2,
                    }],
                    n,
                ),
                // the sample answers locally and sends nothing
                (Effect::Head(ws), None) => {
                    if let Some(f) = self.state.form.as_mut() {
                        f.head_known(&ws, Ok(sample::COMMIT.into()));
                    }
                }
                (Effect::Submit(_) | Effect::Approve { .. }, None) => {
                    if let Some(f) = self.state.form.as_mut() {
                        f.failed("Sample data: nothing was sent.".into(), true);
                    }
                }
                (Effect::Review(_), None) => {
                    if let Some(f) = self.state.form.as_mut() {
                        f.reviewing(sample::review(false));
                    }
                }
                (Effect::Head(ws), Some(l)) => l.head(ws),
                (Effect::Review(id), Some(l)) => {
                    self.reconnect_first(l);
                    l.send(Req::Review(id));
                }
                (Effect::Submit(s), Some(l)) => {
                    self.reconnect_first(l);
                    l.send(Req::Submit(s));
                }
                (
                    Effect::Approve {
                        outcome,
                        version,
                        hash,
                        key,
                    },
                    Some(l),
                ) => {
                    self.reconnect_first(l);
                    l.send(Req::Approve {
                        outcome,
                        version,
                        hash,
                        key,
                    });
                }
                (Effect::Resolve { .. }, None) => {
                    self.state.answered("Sample data: nothing was sent.".into());
                }
                (Effect::Select(id), None) => {
                    let tab = self.state.itab;
                    sample::show(&mut self.state, &id, tab);
                }
                (Effect::Inspect(t), None) => {
                    if let Some(item) = self.state.selected_item().cloned() {
                        self.state.inspector = Some(sample::inspector(t, &item));
                    }
                }
                (Effect::Migrate(_), None) => self
                    .state
                    .guide_failed("Sample data: nothing was sent.".into()),
                (Effect::Migrate(n), Some(l)) => {
                    if !l.try_send(Req::Enable(n)) {
                        self.state.guide_failed(
                            "The link to the orchestrator has ended. Close this window and press r."
                                .into(),
                        );
                    }
                }
                (Effect::NewSession, _) => self.new_session(cx),
                (Effect::EnableSetting, _) => {}
                (Effect::Refresh | Effect::Retry, None) => {}
                (Effect::Refresh, Some(l)) => l.send(Req::Refresh),
                (Effect::Retry, Some(l)) => {
                    self.state.set_screen(Screen::Loading);
                    l.send(Req::Retry);
                }
                (Effect::Select(id), Some(l)) => l.send(Req::Select(id)),
                (Effect::Inspect(t), Some(l)) => l.send(Req::Inspect(t)),
                (
                    Effect::Resolve {
                        outcome,
                        card,
                        choice,
                    },
                    Some(l),
                ) => l.send(Req::Resolve {
                    outcome,
                    card,
                    choice,
                }),
            }
        }
        self.note_selection();
        cx.notify();
    }

    /// The outcome now selected is a place of the history (a click, the arrows, Alt+A, the server's first pick).
    fn note_selection(&mut self) {
        if let Some(id) = &self.state.selected {
            self.hist.visit(history::Place::Outcome(id.clone()));
        }
    }

    /// The shell shows or hides this view. Hiding drops the card's keys without touching the window's focus.
    pub fn set_visible(&mut self, v: bool) {
        self.visible = v;
        if !v {
            self.pop.focus = false;
        }
    }

    /// A form or the disclosure is open: history keys and buttons do nothing.
    fn modal(&self) -> bool {
        self.state.form.is_some() || self.state.guide.is_some() || self.state.picker.is_some()
    }

    pub fn can_back(&self) -> bool {
        !self.modal() && self.hist.can_back(|p| place_alive(&self.state.items, p))
    }

    pub fn can_forward(&self) -> bool {
        !self.modal() && self.hist.can_forward(|p| place_alive(&self.state.items, p))
    }

    /// Title-bar back and forward (also Alt+Left / Alt+Right): moves through the outcomes shown.
    pub fn nav(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        let items = &self.state.items;
        let alive = |p: &history::Place| place_alive(items, p);
        let to = if back {
            self.hist.back(alive)
        } else {
            self.hist.forward(alive)
        };
        if let Some(history::Place::Outcome(id)) = to {
            self.select_id(&id, cx);
        }
        cx.notify();
    }

    /// The title bar's rail button: hides or shows the list column.
    pub fn toggle_rail(&mut self, cx: &mut Context<Self>) {
        self.rail_open = !self.rail_open;
        cx.notify();
    }

    /// The selected outcome's goal, for the breadcrumb (`None` when the list is not shown or nothing is selected).
    pub fn breadcrumb_goal(&self) -> Option<&str> {
        (self.state.screen == Screen::Ready)
            .then(|| self.state.selected_item().map(|i| i.row.goal.as_str()))
            .flatten()
    }

    /// The person confirmed the disclosure of the Off screen: the user settings file gets
    /// `synt.orchestrator.enabled = true` (the Settings rows' writer: atomic, other keys kept, a managed value
    /// refuses), then the link checks again. Nothing is remembered about the confirmation.
    fn enable_setting(&mut self) {
        let Some(env) = &self.env else {
            self.state
                .guide_failed("Sample data: nothing was written.".into());
            return;
        };
        match crate::settings_view::model::write_orchestrator(env, true) {
            Ok(()) => {
                self.state.guide_done();
                self.state.set_screen(Screen::Loading);
                if let Some(l) = &self.link {
                    l.send(Req::Retry);
                }
            }
            Err(e) => self.state.guide_failed(e),
        }
    }

    /// The enable dialog as a modal over the Super Agent view; its keys come back here.
    pub(crate) fn modals(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.guide_overlay(cx)
            .map(|guide| {
                div()
                    .id("out-modals")
                    .absolute()
                    .inset_0()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, w, cx| {
                        if this.on_key(ev, w, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(guide)
                    .into_any_element()
            })
            .into_iter()
            .collect()
    }

    /// "New session": with no folder named, a scratch folder under the synt home. For a remote engine (WSL) a local
    /// scratch folder is no use: the person is asked for a folder.
    pub(crate) fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.state.remote {
            return self.session_in_folder(cx);
        }
        let made = match &self.env {
            Some(env) => {
                synthesis_app::homes(env).and_then(|h| kind_picker::make_scratch(&h.synthesis))
            }
            None => Err("Sample data: nothing was created.".to_string()),
        };
        match made {
            Ok(p) => cx.emit(OpenFolder(p)),
            Err(e) => {
                self.state.picker = Some(kind_picker::Picker {
                    cursor: Some(0),
                    notice: Some(e),
                })
            }
        }
    }

    /// The system folder dialog; the chosen folder gets the new session.
    pub(crate) fn session_in_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Folder for the new session".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                this.update(cx, |o, cx| {
                    o.state.picker = None;
                    cx.emit(OpenFolder(p));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// A request after a failed link goes after a retry: the worker that ended waits for one, and the
    /// request stays queued behind it (the form's text and key are kept either way).
    fn reconnect_first(&self, l: &Link) {
        if self.state.screen != Screen::Ready {
            l.send(Req::Retry);
        }
    }

    pub(crate) fn on_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        // a held key (auto-repeat) never acts on the disclosure: one deliberate press does
        if ev.is_held && self.state.guide.is_some() {
            return true;
        }
        if self.state.guide.is_none() {
            if m.alt
                && !m.control
                && !m.platform
                && !m.shift
                && matches!(k.key.as_str(), "left" | "right")
            {
                self.nav(k.key == "left", cx);
                return true;
            }
            if self.popup_key(ev, window, cx) {
                return true;
            }
        }
        let key = Key {
            name: &k.key,
            shift: m.shift,
            other_mod: m.control || m.alt || m.platform,
        };
        match self.state.key(&key, now_ms()) {
            Some(effects) => {
                self.run(effects, cx);
                true
            }
            None => false,
        }
    }
}

/// Places back and forward may go to: an outcome that is gone is skipped; the chat (S1) and the sessions (S2) are
/// places of the model that nothing shows yet.
fn place_alive(items: &[model::Item], p: &history::Place) -> bool {
    matches!(p, history::Place::Outcome(id) if items.iter().any(|i| &i.row.id == id))
}

/// Scrolls a pane by `n` lines (negative: up).
fn scroll(h: &ScrollHandle, n: i32) {
    let max = f32::from(h.max_offset().y);
    let y = (f32::from(h.offset().y) - n as f32 * 19.).clamp(-max, 0.);
    h.set_offset(point(px(0.), px(y)));
}

impl Focusable for OutcomesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}
