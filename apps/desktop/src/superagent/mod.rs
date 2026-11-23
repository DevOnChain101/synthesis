//! Super Agent (docs/specs/super-agent-s1.md). `SuperAgentView` is what `View::SuperAgent` shows (phase B1): the
//! mock's chrome (rail, title-row tabs, panel) with real navigation, and placeholder frame bodies that the later
//! phases fill. Every later module is declared here as a stub so a phase adds files only inside its own directory.
//! `OutcomesView` stays the data and link owner; the old outcomes UI is not mounted (phase E deletes it).

pub mod chat;
pub mod frames;
pub mod goal;
pub mod home;
pub mod kit;
pub mod material;
mod picker;
pub mod port;
pub mod rail;
pub mod session;
pub mod sidebar;
pub mod tabstrip;
pub mod tokens;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollHandle, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*,
    px,
};

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui::Subscription;

use crate::outcomes_view::OutcomesView;
use crate::outcomes_view::popup::OpenSession;
use crate::ui::{TITLEBAR_H, Tip, View};
use crate::widgets::chrome::icon;
use chat::{ConductorChat, OpenEnableGuide, OpenSettings};
use frames::{Frame, Frames, GoalKind};
use goal::{GoalEvent, GoalForm};
use home::{HomeEvent, HomeView};
use port::{Cmd, HostPort};
use session::host::SessionHost;
use session::outcome::{OutcomeEvent, OutcomeFrame};
use session::worker::{WorkerEvent, WorkerFrame};
use synthesis_app::conductor::SessionRow;
use tokens::{self as t, hsla};

/// The rail and the title row start at the window's top: the traffic lights sit in this 48 px row (the mock's grid).
pub const TITLE_ROW_H: f32 = 48.;
/// Below this width the rail is icon-only unless the person toggled it.
const MINI_BELOW: f32 = 900.;
pub const CAP_NOTE: &str = "At most 6 session tabs can be open. Close one first.";

/// The person asked for another view (the mode buttons, the gear): the shell switches.
pub struct SwitchView(pub View);

impl EventEmitter<SwitchView> for SuperAgentView {}

/// A session that waits for a tool permission is answered in the TUI view, in that session (the shell opens it).
impl EventEmitter<OpenSession> for SuperAgentView {}

pub struct SuperAgentView {
    outcomes: Entity<OutcomesView>,
    pub(crate) frames: Frames,
    focus: FocusHandle,
    /// The rail's one tab stop; the row under `rail_cursor` has the ring while it holds the focus.
    rail_focus: FocusHandle,
    rail_cursor: usize,
    /// `Some`: the person toggled the rail (collapsed or not); `None`: it follows the window width.
    rail_user: Option<bool>,
    /// Sessions opened in this window (id, title), oldest first; closing the tab keeps the rail row.
    pub(crate) sessions: Vec<(String, String)>,
    /// Why the last action did not go on; cleared by the next one.
    notice: Option<String>,
    /// The mode button counts what waits for the person only in a live app (never the sample).
    pub(crate) count_waiting: bool,
    tab_scroll: ScrollHandle,
    /// The home frame's body (B2); the conversation of the orchestrator chat (B3) sits in its slot.
    home: Entity<HomeView>,
    /// Hosts the embedded chats of the session frames (C1).
    pub(crate) host: Entity<SessionHost>,
    /// `None` for the sample: no conductor runs.
    chat: Option<Entity<ConductorChat>>,
    /// The Super Agent view is the one on screen (not the TUI view, not Settings over it).
    visible: bool,
    /// The next draw gives the keys to the front frame (a frame came forward).
    refocus: bool,
    /// The folder of each session (by frame id); a folder already hosted is not started twice.
    port: Arc<HostPort>,
    folders: Vec<(String, PathBuf)>,
    /// The next session frame id is `s<N>`: the id is the conductor's handle too, minted per open and never reused,
    /// so a stale card can never reach another session.
    next_session: usize,
    /// A new session without a folder starts in a scratch folder under this home (`None`: the sample).
    synt_home: Option<PathBuf>,
    /// The enable dialog was open at the last look: its closing is when the setting may have changed.
    had_guide: bool,
    /// The live app's environment and folder (`None`: the sample): what a new goal form starts with.
    conductor: Option<(synthesis_config::Env, PathBuf)>,
    /// One form per open `Frame::NewGoal`; dropping it (with its frame) ends its link.
    goals: Vec<(GoalKind, Entity<GoalForm>, Subscription)>,
    /// One per open `Frame::Outcome` and `Frame::Worker` (C3); a worker's draft goes to `frames` when its tab closes.
    outcome_frames: Vec<(String, Entity<OutcomeFrame>, Subscription)>,
    worker_frames: Vec<((String, String), Entity<WorkerFrame>, Subscription)>,
    /// The right sidebar (C2): open state, segment and width are the window's.
    pub(crate) side: sidebar::Side,
    /// The sidebar's grip holds the keys (Left and Right resize); the pointer is dragging it.
    grip_focus: FocusHandle,
    grip_drag: bool,
    _subs: Vec<Subscription>,
}

impl SuperAgentView {
    /// `conductor`: the environment and workspace of the live app (`None`: the sample, which runs no conductor).
    pub fn new(
        outcomes: Entity<OutcomesView>,
        host: Entity<SessionHost>,
        conductor: Option<(synthesis_config::Env, PathBuf)>,
        count_waiting: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subs = vec![cx.observe_in(&outcomes, window, |v, _, window, cx| {
            v.outcomes_changed(window, cx)
        })];
        subs.push(cx.observe(&host, |v, _, cx| {
            v.refresh_port(cx);
            cx.notify();
        }));
        let home = cx.new(|cx| HomeView::new(outcomes.clone(), cx));
        subs.push(cx.subscribe(&home, |v, _, e: &HomeEvent, cx| match e {
            HomeEvent::OpenOutcome(id) => v.open(Frame::Outcome(id.clone()), cx),
            HomeEvent::OpenSession(id) => cx.emit(OpenSession(id.clone())),
        }));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let port = Arc::new(HostPort::new(tx));
        let synt_home = conductor
            .as_ref()
            .and_then(|(env, _)| synthesis_app::homes(env).ok())
            .map(|h| h.synthesis);
        let chat = conductor.clone().map(|(env, ws)| {
            let chat = cx.new(|cx| ConductorChat::new(env, ws, Some(port.clone()), window, cx));
            home.update(cx, |h, cx| {
                h.set_conversation(Some(chat.clone().into()), cx)
            });
            subs.push(
                cx.subscribe_in(&chat, window, |v, _, _: &OpenEnableGuide, _, cx| {
                    v.enable_guide(cx)
                }),
            );
            subs.push(cx.subscribe(&chat, |_, _, _: &OpenSettings, cx| {
                cx.emit(SwitchView(View::Settings))
            }));
            chat
        });
        // the conductor's effects on sessions run here, on the UI thread
        cx.spawn_in(window, async move |this, cx| {
            while let Some(c) = rx.next().await {
                if this
                    .update_in(cx, |v, window, cx| v.run_cmd(c, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
        Self {
            home,
            host,
            chat,
            visible: false,
            refocus: false,
            port: port.clone(),
            folders: Vec::new(),
            next_session: 1,
            synt_home,
            had_guide: false,
            conductor,
            goals: Vec::new(),
            outcome_frames: Vec::new(),
            worker_frames: Vec::new(),
            side: sidebar::Side::default(),
            grip_focus: cx.focus_handle(),
            grip_drag: false,
            _subs: subs,
            outcomes,
            frames: Frames::default(),
            focus: cx.focus_handle(),
            rail_focus: cx.focus_handle().tab_stop(true),
            rail_cursor: 1,
            rail_user: None,
            sessions: Vec::new(),
            notice: None,
            count_waiting,
            tab_scroll: ScrollHandle::new(),
        }
    }

    /// The view was shown: the keys come here (the home, or the front session, takes them at the next draw).
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.visible = true;
        self.refocus = true;
        self.sync_host(cx);
        self.sync_side(cx);
        cx.notify();
    }

    /// The shell shows another view (the TUI view, Settings over this one): no hosted chat takes a key.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            self.sync_host(cx);
            self.sync_side(cx);
            cx.notify();
        }
    }

    /// What waits for the person: decisions and sessions that wait for a tool permission, and the questions in the
    /// embedded chats (the rail, the mode button).
    pub(crate) fn waiting(&self, cx: &App) -> usize {
        if self.count_waiting {
            self.outcomes.read(cx).waiting_count() + self.host.read(cx).needs_total(cx)
        } else {
            0
        }
    }

    fn sync_host(&mut self, cx: &mut Context<Self>) {
        let (frames, visible) = (&self.frames, self.visible);
        self.host.update(cx, |h, cx| h.sync(frames, visible, cx));
    }

    /// The outcomes view changed: the enable dialog just closed is when the setting may be on now.
    fn outcomes_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let guide = self.outcomes.read(cx).state.guide.is_some();
        if std::mem::replace(&mut self.had_guide, guide)
            && !guide
            && let Some(c) = &self.chat
        {
            c.update(cx, |c, cx| c.recheck(cx));
        }
        cx.notify();
    }

    /// The chat's "Turn on Super Agent": the existing enable dialog opens over this view.
    fn enable_guide(&mut self, cx: &mut Context<Self>) {
        self.outcomes.update(cx, |o, _| o.state.open_guide());
        if self.outcomes.read(cx).state.guide.is_none() {
            self.notice =
                Some("Turn Super Agent on in Settings (synt.orchestrator.enabled).".into());
        }
        cx.notify();
    }

    fn collapsed(&self, window: &Window) -> bool {
        self.rail_user
            .unwrap_or(f32::from(window.viewport_size().width) < MINI_BELOW)
    }

    // ------------------------------------------------------------------------------------------ frames

    /// The frames changed: the active tab is brought into view, the hosted chats follow the tabs.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.refocus = true;
        self.tab_scroll.scroll_to_item(self.frames.active_index());
        self.sync_host(cx);
        self.sync_side(cx);
        self.refresh_port(cx);
        let open = self.frames.tabs().to_vec();
        self.goals
            .retain(|(k, ..)| open.contains(&Frame::NewGoal(*k)));
        self.outcome_frames
            .retain(|(o, ..)| open.contains(&Frame::Outcome(o.clone())));
        // a closed worker frame keeps what was typed (not what was sent) with its frame key
        let gone: Vec<_> = self
            .worker_frames
            .iter()
            .filter(|((o, i), ..)| !open.contains(&Frame::Worker(o.clone(), i.clone())))
            .map(|((o, i), e, _)| (Frame::Worker(o.clone(), i.clone()), e.read(cx).draft(cx)))
            .collect();
        for (f, draft) in gone {
            self.frames.set_draft(f, draft);
        }
        self.worker_frames
            .retain(|((o, i), ..)| open.contains(&Frame::Worker(o.clone(), i.clone())));
        // the outcome or worker frame in front reads the selected outcome: select it
        if let Frame::Outcome(id) | Frame::Worker(id, _) = self.frames.active().clone() {
            self.outcomes.update(cx, |o, cx| o.select_id(&id, cx));
        }
        cx.notify();
    }

    pub fn open(&mut self, f: Frame, cx: &mut Context<Self>) {
        let r = self.frames.open(f);
        self.changed(cx);
        if r.is_err() {
            self.notice = Some(CAP_NOTE.into());
        }
    }

    pub(crate) fn focus_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        self.frames.focus(i);
        self.changed(cx);
    }

    /// A goal form with an approval in flight stays: the server may still apply it (then the outcome opens).
    pub(crate) fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(Frame::NewGoal(k)) = self.frames.tabs().get(i)
            && self
                .goals
                .iter()
                .any(|(g, f, _)| g == k && f.read(cx).approving())
        {
            self.notice =
                Some("An approval is being sent. The form stays until the server answers.".into());
            return cx.notify();
        }
        self.drop_tab(i, cx);
    }

    fn drop_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        self.frames.close(i);
        self.changed(cx);
    }

    pub(crate) fn reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.frames.reorder(from, to);
        self.changed(cx);
    }

    fn alive(&self, cx: &App) -> impl Fn(&Frame) -> bool + use<> {
        let hosted = self.host.read(cx).ids();
        let items: Vec<String> = self
            .outcomes
            .read(cx)
            .state
            .items
            .iter()
            .map(|i| i.row.id.clone())
            .collect();
        move |f| match f {
            Frame::Outcome(id) | Frame::Worker(id, _) => items.contains(id),
            // a closed session is not a place to go back to
            Frame::Session(id) => hosted.contains(id),
            _ => true,
        }
    }

    pub(crate) fn can_nav(&self, back: bool, cx: &App) -> bool {
        let alive = self.alive(cx);
        if back {
            self.frames.can_back(alive)
        } else {
            self.frames.can_forward(alive)
        }
    }

    pub fn nav(&mut self, back: bool, cx: &mut Context<Self>) {
        let alive = self.alive(cx);
        let _ = if back {
            self.frames.back(alive)
        } else {
            self.frames.forward(alive)
        };
        self.changed(cx);
    }

    /// A new session started in folder `p` (a folder the person chose, a scratch folder, or the conductor's): its
    /// chat is hosted in a session frame of this view, never in the TUI view. The frame id is a fresh `s<N>`, never
    /// derived from the folder. A folder that already has an open session shows that tab (`Ok((id, false))`).
    /// `Err(why)`: nothing opened, `why` is shown (the sample, the cap of open sessions, a refused folder).
    pub fn open_session_folder(
        &mut self,
        p: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(String, bool), String> {
        let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let hosted = self.host.read(cx).ids();
        if let Some((id, _)) = self
            .folders
            .iter()
            .find(|(i, f)| *f == canon && hosted.contains(i))
        {
            let id = id.clone();
            self.open(Frame::Session(id.clone()), cx);
            return Ok((id, false));
        }
        let id = format!("s{}", self.next_session);
        let opened = self.host.update(cx, |h, cx| h.open(&id, p, window, cx));
        if let Err(why) = opened {
            self.notice = Some(why.clone());
            cx.notify();
            return Err(why);
        }
        self.register_session(&id, &canon);
        self.open(Frame::Session(id.clone()), cx);
        Ok((id, true))
    }

    /// The rail row of a hosted session; its id is the conductor's handle.
    fn register_session(&mut self, id: &str, folder: &std::path::Path) {
        self.next_session += 1;
        let title = folder
            .file_name()
            .map_or_else(|| id.to_string(), |n| n.to_string_lossy().into_owned());
        self.sessions.push((id.to_string(), title));
        self.folders.push((id.to_string(), folder.to_path_buf()));
    }

    /// What the conductor reads of the sessions of this window (live or ended), and the rail's titles.
    fn refresh_port(&mut self, cx: &mut Context<Self>) {
        let host = self.host.read(cx);
        let mut snap = port::Snapshot::default();
        for (id, _) in &self.sessions {
            let info = host.info(id, cx);
            let (sid, report) = host.read(id, cx).unzip();
            let title = info.as_ref().map(|i| i.title.clone()).or_else(|| {
                self.sessions
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, t)| t.clone())
            });
            let state = match &info {
                None => "ended",
                Some(i) if i.needs > 0 => "needs_you",
                Some(i) if i.busy => "running",
                Some(_) => "idle",
            };
            snap.rows.push(SessionRow {
                handle: id.clone(),
                sid: sid.unwrap_or_default(),
                title: title.unwrap_or_default(),
                state: state.into(),
                model: "unknown".into(),
                mode: "unknown".into(),
                cwd_label: self
                    .folders
                    .iter()
                    .find(|(i, _)| i == id)
                    .and_then(|(_, f)| f.file_name())
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                // not reported for window sessions: say so instead of a misleading zero
                spent_usd: 0.0,
                task_summary: "cost, model and mode are not reported for this session".into(),
                live: info.is_some(),
            });
            if let Some(r) = report {
                snap.reports.insert(id.clone(), r);
            }
        }
        if let Ok(mut s) = self.port.shared.lock() {
            *s = snap;
        }
        // the rail keeps the first thing the person asked as the row's title, also after the tab closed
        for (id, title) in &mut self.sessions {
            if let Some(i) = self.host.read(cx).info(id, cx) {
                *title = i.title;
            }
        }
    }

    /// The frame id behind a handle: only a session of this window (never reused, so a stale handle finds none).
    fn id_of(&self, handle: &str) -> Option<String> {
        self.sessions
            .iter()
            .any(|(i, _)| i == handle)
            .then(|| handle.to_string())
    }

    /// An effect the conductor's card asked for (the person confirmed it; the proof came with it).
    fn run_cmd(&mut self, c: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        match c {
            Cmd::Start(req, reply) => {
                let _ = reply.send(self.start_for_conductor(&req, window, cx));
            }
            Cmd::Send(handle, text, approval, reply) => {
                let r = match self.id_of(&handle) {
                    Some(id) => self.host.read(cx).send(&id, text, approval, cx),
                    None => Err("no such session".into()),
                };
                let _ = reply.send(r);
            }
            Cmd::Interrupt(handle, reply) => {
                let r = match self.id_of(&handle) {
                    Some(id) => self.host.read(cx).interrupt(&id, cx),
                    None => Err("no such session".into()),
                };
                let _ = reply.send(r);
            }
            Cmd::Close(handle, reply) => {
                let at = self.id_of(&handle).and_then(|id| {
                    self.frames
                        .tabs()
                        .iter()
                        .position(|f| matches!(f, Frame::Session(i) if *i == id))
                });
                let r = match at {
                    Some(i) => {
                        self.close_tab(i, cx);
                        Ok(())
                    }
                    None => Err("that session is not open".into()),
                };
                let _ = reply.send(r);
            }
            Cmd::Goal(draft, reply) => {
                let _ = reply.send(self.open_goal_form(&draft, window, cx));
            }
        }
    }

    fn start_for_conductor(
        &mut self,
        req: &synthesis_app::conductor::SessionStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        if req.model.is_some() {
            return Err("choosing a model for a started session is not supported here".into());
        }
        let folder = match &req.folder {
            Some(f) => PathBuf::from(f),
            None => {
                let home = self.synt_home.as_ref().ok_or("sessions need a live app")?;
                crate::outcomes_view::kind_picker::make_scratch(home)?
            }
        };
        let (id, fresh) = self.open_session_folder(&folder, window, cx)?;
        if !fresh {
            // never type the card's line into a session the person is already using
            return Err("a session is already open in that folder".into());
        }
        let text = req.first_message.clone();
        self.host
            .update(cx, |h, cx| h.first_message(&id, text, window, cx));
        Ok(id)
    }

    /// The form of `kind` (made on first use, with the live environment or as the sample); `start` is the folder
    /// of a writes form made for a conductor card.
    fn goal_form(
        &mut self,
        kind: GoalKind,
        start: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<GoalForm> {
        if let Some((_, f, _)) = self.goals.iter().find(|(k, ..)| *k == kind) {
            return f.clone();
        }
        let (env, ws) = match &self.conductor {
            Some((e, w)) => (Some(e.clone()), w.to_string_lossy().into_owned()),
            None => (None, String::new()),
        };
        let ws = start.map_or(ws, str::to_string);
        let form = cx.new(|cx| GoalForm::new(kind, env, &ws, window, cx));
        let sub = cx.subscribe(&form, move |v, _, e: &GoalEvent, cx| {
            let at = v
                .frames
                .tabs()
                .iter()
                .position(|f| *f == Frame::NewGoal(kind));
            match (e, at) {
                (GoalEvent::Close, Some(i)) => v.close_tab(i, cx),
                (GoalEvent::Approved { outcome, .. }, at) => {
                    if let Some(i) = at {
                        v.drop_tab(i, cx);
                    }
                    v.open(Frame::Outcome(outcome.clone()), cx);
                }
                _ => {}
            }
        });
        self.goals.push((kind, form.clone(), sub));
        form
    }

    /// The entity of an outcome frame (made on first use).
    fn outcome_frame(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<OutcomeFrame> {
        if let Some((_, e, _)) = self.outcome_frames.iter().find(|(o, ..)| o == id) {
            return e.clone();
        }
        let (outcomes, oid) = (self.outcomes.clone(), id.to_string());
        let e = cx.new(|cx| OutcomeFrame::new(outcomes, oid, cx));
        let at = id.to_string();
        let sub = cx.subscribe(&e, move |v, _, ev: &OutcomeEvent, cx| match ev {
            OutcomeEvent::OpenWorker(item) => v.open(Frame::Worker(at.clone(), item.clone()), cx),
            OutcomeEvent::AnswerOnHome => {
                v.frames.focus(0);
                v.changed(cx);
                v.home.update(cx, |h, cx| h.review(&at, cx));
            }
        });
        self.outcome_frames.push((id.to_string(), e.clone(), sub));
        e
    }

    /// The entity of a worker frame (made on first use, with the draft its closed tab kept).
    fn worker_frame(
        &mut self,
        o: &str,
        i: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<WorkerFrame> {
        if let Some((_, e, _)) = self
            .worker_frames
            .iter()
            .find(|((a, b), ..)| a == o && b == i)
        {
            return e.clone();
        }
        let draft = self
            .frames
            .draft(&Frame::Worker(o.into(), i.into()))
            .to_string();
        let (outcomes, oid, iid) = (self.outcomes.clone(), o.to_string(), i.to_string());
        let e = cx.new(|cx| WorkerFrame::new(outcomes, oid, iid, &draft, window, cx));
        let at = o.to_string();
        let sub = cx.subscribe_in(
            &e,
            window,
            move |v, _, ev: &WorkerEvent, window, cx| match ev {
                WorkerEvent::OpenOutcome => v.open(Frame::Outcome(at.clone()), cx),
                WorkerEvent::Blur => window.focus(&v.focus, cx),
            },
        );
        self.worker_frames
            .push(((o.to_string(), i.to_string()), e.clone(), sub));
        e
    }

    /// A conductor goal card: the New goal frame opens (or comes forward) and its text is filled in. The person
    /// still finishes the contract, sends it and approves the stored version; nothing is approved here.
    fn open_goal_form(
        &mut self,
        draft: &synthesis_app::conductor::GoalDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let kind = match draft.kind.as_str() {
            "read_only" | "read-only" | "readonly" => GoalKind::ReadOnly,
            _ => GoalKind::Writes,
        };
        self.open(Frame::NewGoal(kind), cx);
        if self.frames.active() != &Frame::NewGoal(kind) {
            return Err(CAP_NOTE.into());
        }
        let folder = synthesis_harness::sanitize_for_display(draft.folder.trim());
        let start = (kind == GoalKind::Writes && !folder.is_empty()).then_some(folder.as_str());
        let form = self.goal_form(kind, start, window, cx);
        let filled = form.update(cx, |f, cx| f.prefill(&draft.contract, &folder, window, cx));
        Ok(match (filled, kind) {
            (false, _) => "the form already has text, nothing was overwritten".into(),
            (true, GoalKind::Writes) => "New goal form".into(),
            (true, GoalKind::ReadOnly) => "New read-only goal form".into(),
        })
    }

    fn toggle_rail(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.rail_user = Some(!self.collapsed(window));
        cx.notify();
    }

    pub(crate) fn title(&self, f: &Frame, cx: &App) -> String {
        let clean = |s: &str| {
            synthesis_harness::sanitize_for_display(&s.chars().take(200).collect::<String>())
        };
        match f {
            Frame::Home => "Super-Agent".into(),
            Frame::Outcome(id) => self
                .outcomes
                .read(cx)
                .state
                .items
                .iter()
                .find(|i| i.row.id == *id)
                .map_or_else(|| clean(id), |i| clean(&i.row.goal)),
            Frame::Worker(o, i) => {
                let st = &self.outcomes.read(cx).state;
                match session::worker::build(st, o, i, session::worker::is_windows()) {
                    session::worker::Body::Ready(v) => clean(&v.title),
                    _ => clean(&format!("{o} {i}")),
                }
            }
            Frame::Session(id) => self
                .host
                .read(cx)
                .info(id, cx)
                .map(|i| i.title)
                .or_else(|| {
                    self.sessions
                        .iter()
                        .find(|(i, _)| i == id)
                        .map(|(_, t)| t.clone())
                })
                .map_or_else(|| "Session".into(), |t| clean(&t)),
            Frame::NewGoal(GoalKind::Writes) => "New goal".into(),
            Frame::NewGoal(GoalKind::ReadOnly) => "New read-only goal".into(),
            Frame::Picker => "New".into(),
        }
    }

    // ------------------------------------------------------------------------------------------ keys

    /// Shortcuts of the mock (DESIGN-NOTES 10.5). Cmd+, and Cmd+1/2 belong to the shell. Bare keys act only while the
    /// view or the rail holds the focus, so they never steal typing from a chat hosted inside.
    pub(crate) fn on_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (k, m) = (&ev.keystroke, &ev.keystroke.modifiers);
        let key = k.key.as_str();
        if self.picker_open(cx) {
            return self.picker_key(ev, window, cx);
        }
        let o = self.outcomes.read(cx);
        if o.state.form.is_some() || o.state.guide.is_some() {
            // the contract form and the enable dialog are modal: their keys are the outcomes view's
            return self.outcomes.update(cx, |o, cx| o.on_key(ev, window, cx));
        }
        let bare = !(m.control || m.alt || m.platform || m.shift);
        if bare && self.grip_focus.is_focused(window) && matches!(key, "left" | "right") {
            self.resize_side(
                if key == "left" {
                    sidebar::STEP
                } else {
                    -sidebar::STEP
                },
                window,
                cx,
            );
            return true;
        }
        // before the Cmd/Ctrl chords: on Windows and Linux Ctrl is the secondary key, and Ctrl+Tab is not one of them
        if m.control && !m.alt && !m.platform && key == "tab" {
            self.frames.cycle(if m.shift { -1 } else { 1 });
            self.changed(cx);
            return true;
        }
        if m.control && !m.alt && !m.platform && !m.shift && key == "`" {
            self.toggle_side(sidebar::Segment::Terminal, cx);
            return true;
        }
        if m.secondary(cx) && !m.alt {
            match key {
                "i" => self.toggle_side(sidebar::Segment::Details, cx),
                "t" => self.new_picker(window, cx),
                "w" => self.close_tab(self.frames.active_index(), cx),
                "b" => self.toggle_rail(window, cx),
                _ => return false,
            }
            return true;
        }
        if m.alt && !m.control && !m.platform && !m.shift && matches!(key, "left" | "right") {
            self.nav(key == "left", cx);
            return true;
        }
        if m.alt && !m.control && !m.platform && !m.shift && key == "a" {
            // the next thing that waits for the person: the home shows it
            self.frames.focus(0);
            self.changed(cx);
            self.home.update(cx, |h, cx| h.next_decision(cx));
            return true;
        }
        let mine = self.focus.is_focused(window)
            || self.rail_focus.is_focused(window)
            || self.home.focus_handle(cx).is_focused(window)
            || self
                .outcome_frames
                .iter()
                .any(|(_, e, _)| e.read(cx).focus.is_focused(window));
        if !bare || !mine {
            return false;
        }
        match key {
            "n" => self.new_picker(window, cx),
            "escape" => self.nav(true, cx),
            _ if self.rail_focus.is_focused(window) => {
                return self.rail_key(key, ev.is_held, window, cx);
            }
            _ => return false,
        }
        true
    }
}

impl Focusable for SuperAgentView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// An icon button of the chrome: a round plate, 40 % when disabled, a tooltip that says what and why.
pub(super) fn ib(
    sel: &'static str,
    glyph: &'static str,
    tip: String,
    (w, h): (f32, f32),
    (enabled, pressed): (bool, bool),
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let fg = if pressed { t::TEXT_STRONG } else { t::TEXT };
    div()
        .id(sel)
        .debug_selector(move || sel.to_string())
        .flex_none()
        .w(px(w))
        .h(px(h))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(t::R_PILL))
        .text_color(hsla(fg))
        .when(pressed, |d| d.bg(hsla(t::ELEVATED)).shadow(kit::raised()))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
                .on_click(on_click)
        })
        .when(!enabled, |d| d.opacity(0.4))
        .tooltip(move |_, cx| -> AnyView { cx.new(|_| Tip(tip.clone())).into() })
        .child(icon(glyph, 16., hsla(fg)))
}

impl SuperAgentView {
    /// The body of a frame: the home (B2) with the orchestrator chat in its slot (B3), a hosted session chat (C1),
    /// or a placeholder for the frames of the later phases.
    fn frame_body(&mut self, f: &Frame, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let col = || {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(t::S1))
                .px(px(t::S5))
        };
        match f {
            Frame::Home => div()
                .size_full()
                .child(self.home.clone())
                .into_any_element(),
            Frame::Session(id) => {
                let body = self.host.update(cx, |h, cx| h.render_frame(id, window, cx));
                if let Some(b) = body {
                    return div()
                        .debug_selector(|| "sa-session".to_string())
                        .size_full()
                        .child(b)
                        .into_any_element();
                }
                col()
                    .debug_selector(|| "sa-frame".to_string())
                    .child(kit::text(t::T3, t::W_REGULAR, t::TEXT).child("This session has ended."))
                    .into_any_element()
            }
            Frame::Outcome(id) => div()
                .size_full()
                .child(self.outcome_frame(id, cx))
                .into_any_element(),
            Frame::Worker(o, i) => div()
                .size_full()
                .child(self.worker_frame(o, i, window, cx))
                .into_any_element(),
            Frame::NewGoal(k) => {
                let form = self.goal_form(*k, None, window, cx);
                div()
                    .debug_selector(|| "sa-new-goal".to_string())
                    .size_full()
                    .child(form)
                    .into_any_element()
            }
            f @ Frame::Picker => col()
                .debug_selector(|| "sa-frame".to_string())
                .child(
                    kit::card("sa-frame-card", false)
                        .child(
                            kit::text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG)
                                .child(self.title(f, cx)),
                        )
                        .child(
                            kit::text(t::T2, t::W_REGULAR, t::TEXT)
                                .child("This frame is filled in a later phase."),
                        ),
                )
                .into_any_element(),
        }
    }

    fn panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.notice.clone();
        let active = self.frames.active().clone();
        let body = self.frame_body(&active, window, cx);
        div()
            .debug_selector(|| "sa-panel".to_string())
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(t::R_PANEL))
            .bg(hsla(t::BG))
            .border_1()
            .border_color(t::stroke())
            .children(notice.map(|n| {
                kit::text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                    .debug_selector(|| "sa-notice".to_string())
                    .px(px(t::S5))
                    .py(px(t::S2))
                    .bg(hsla(t::ELEVATED))
                    .child(n)
            }))
            .child(div().flex_1().min_h(px(0.)).child(body))
    }
}

impl Render for SuperAgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.collapsed(window);
        if std::mem::take(&mut self.refocus) && self.visible {
            match self.frames.active() {
                Frame::Home => self.home.update(cx, |h, cx| h.shown(window, cx)),
                // a hosted chat takes its own focus; the others keep the keys on this view
                Frame::Session(_) => {}
                Frame::Outcome(id) => {
                    let f = self.outcome_frame(&id.clone(), cx).read(cx).focus.clone();
                    window.focus(&f, cx);
                }
                Frame::Worker(o, i) => {
                    let (o, i) = (o.clone(), i.clone());
                    let f = self
                        .worker_frame(&o, &i, window, cx)
                        .read(cx)
                        .focus_target(cx);
                    window.focus(&f, cx);
                }
                // a form already used keeps its own focus; a new one takes its first field when it first draws
                Frame::NewGoal(k) if self.goals.iter().any(|(g, ..)| g == k) => {
                    let f = self
                        .goals
                        .iter()
                        .find(|(g, ..)| g == k)
                        .map(|g| g.1.clone());
                    if let Some(f) = f {
                        window.focus(&f.read(cx).focus.clone(), cx);
                    }
                }
                _ => window.focus(&self.focus, cx),
            }
        }
        self.close_side_if_empty(cx);
        let modals = self.outcomes.update(cx, |o, cx| o.modals(window, cx));
        let picker = self.picker_overlay(cx);
        let side = self.sidebar(window, cx);
        let overlay = f32::from(window.viewport_size().width) < sidebar::OVERLAY_BELOW;
        let (side_row, side_over) = if overlay { (None, side) } else { (side, None) };
        div()
            .id("superagent")
            .debug_selector(|| "superagent".to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| {
                if v.on_key(ev, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, window, cx| {
                if v.grip_drag {
                    v.drag_side(e.position.x.into(), window, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|v, _, _, _| v.grip_drag = false),
            )
            .size_full()
            .flex()
            .relative()
            .bg(t::surface_fill())
            .text_color(hsla(t::TEXT_STRONG))
            .child(self.rail(collapsed, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.title_row(window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .pr(px(t::S2))
                            .pb(px(t::S2))
                            .child(self.panel(window, cx))
                            .children(side_row),
                    ),
            )
            .children(side_over)
            .children(modals)
            .children(picker)
    }
}

/// The traffic lights of the mac window sit in the first row of the rail: its centre line.
pub fn traffic_light_y(super_agent: bool) -> f32 {
    (if super_agent { TITLE_ROW_H } else { TITLEBAR_H } - 12.) / 2.
}

#[cfg(test)]
mod tests;
