//! Super Agent (docs/specs/super-agent-s1.md). `SuperAgentView` is what `View::SuperAgent` shows (phase B1): the
//! mock's chrome (rail, title-row tabs, panel) with real navigation, and placeholder frame bodies that the later
//! phases fill. Every later module is declared here as a stub so a phase adds files only inside its own directory.
//! `OutcomesView` stays the data and link owner; the old outcomes UI is not mounted (phase E deletes it).

pub mod chat;
pub mod frames;
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
use chat::{ConductorChat, OpenEnableGuide};
use frames::{Frame, Frames, GoalKind};
use home::{HomeEvent, HomeView};
use port::{Cmd, HostPort};
use session::host::SessionHost;
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
    /// What the conductor reads of the sessions, and the `s<N>` handles it names them by (never reused).
    port: Arc<HostPort>,
    handles: Vec<(String, String)>,
    /// A new session without a folder starts in a scratch folder under this home (`None`: the sample).
    synt_home: Option<PathBuf>,
    /// The enable dialog was open at the last look: its closing is when the setting may have changed.
    had_guide: bool,
    /// The New goal frame opened the contract form (its closing closes the frame).
    goal_form_opened: bool,
    goal_prefill: Option<(String, String)>,
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
        let chat = conductor.map(|(env, ws)| {
            let chat = cx.new(|cx| ConductorChat::new(env, ws, Some(port.clone()), window, cx));
            home.update(cx, |h, cx| {
                h.set_conversation(Some(chat.clone().into()), cx)
            });
            subs.push(
                cx.subscribe_in(&chat, window, |v, _, _: &OpenEnableGuide, _, cx| {
                    v.enable_guide(cx)
                }),
            );
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
            handles: Vec::new(),
            synt_home,
            had_guide: false,
            goal_form_opened: false,
            goal_prefill: None,
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
        cx.notify();
    }

    /// The shell shows another view (the TUI view, Settings over this one): no hosted chat takes a key.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            self.sync_host(cx);
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
        // the New goal frame lives while its form is open; the form's end ends it
        let o = self.outcomes.read(cx);
        if matches!(self.frames.active(), Frame::NewGoal(GoalKind::Writes))
            && o.state.form.is_none()
            && self.goal_form_opened
        {
            self.goal_form_opened = false;
            let i = self.frames.active_index();
            self.close_tab(i, cx);
            self.refocus = true;
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
        self.refresh_port(cx);
        self.start_goal_form(cx);
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

    pub(crate) fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
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
    /// chat is hosted in a session frame of this view, never in the TUI view. `Err(why)`: nothing opened, `why` is
    /// shown (the sample, the cap of open sessions, a refused folder). The frame id is the folder.
    pub fn open_session_folder(
        &mut self,
        p: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let id = p.to_string_lossy().into_owned();
        let opened = self.host.update(cx, |h, cx| h.open(&id, p, window, cx));
        if let Err(why) = opened {
            self.notice = Some(why.clone());
            cx.notify();
            return Err(why);
        }
        self.register_session(&id, p);
        self.open(Frame::Session(id.clone()), cx);
        Ok(id)
    }

    /// The rail row and the conductor's `s<N>` handle of a hosted session.
    fn register_session(&mut self, id: &str, p: &std::path::Path) {
        if !self.sessions.iter().any(|(i, _)| i == id) {
            let title = p
                .file_name()
                .map_or_else(|| id.to_string(), |n| n.to_string_lossy().into_owned());
            self.sessions.push((id.to_string(), title));
        }
        if !self.handles.iter().any(|(_, i)| i == id) {
            let n = self.handles.len() + 1;
            self.handles.push((format!("s{n}"), id.to_string()));
        }
    }

    /// What the conductor reads of the sessions of this window (live or ended), and the rail's titles.
    fn refresh_port(&mut self, cx: &mut Context<Self>) {
        let host = self.host.read(cx);
        let mut snap = port::Snapshot::default();
        for (handle, id) in &self.handles {
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
                handle: handle.clone(),
                sid: sid.unwrap_or_default(),
                title: title.unwrap_or_default(),
                state: state.into(),
                model: String::new(),
                mode: String::new(),
                cwd_label: std::path::Path::new(id)
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                spent_usd: 0.0,
                task_summary: String::new(),
                live: info.is_some(),
            });
            if let Some(r) = report {
                snap.reports.insert(handle.clone(), r);
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

    fn id_of(&self, handle: &str) -> Option<String> {
        self.handles
            .iter()
            .find(|(h, _)| h == handle)
            .map(|(_, i)| i.clone())
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
                let _ = reply.send(self.open_goal_form(Some(&draft), cx));
            }
        }
    }

    fn start_for_conductor(
        &mut self,
        req: &synthesis_app::conductor::SessionStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let folder = match &req.folder {
            Some(f) => PathBuf::from(f),
            None => {
                let home = self.synt_home.as_ref().ok_or("sessions need a live app")?;
                crate::outcomes_view::kind_picker::make_scratch(home)?
            }
        };
        let id = self.open_session_folder(&folder, window, cx)?;
        let text = req.first_message.clone();
        self.host
            .update(cx, |h, cx| h.first_message(&id, text, window, cx));
        self.handles
            .iter()
            .find(|(_, i)| *i == id)
            .map(|(h, _)| h.clone())
            .ok_or_else(|| "the session has no handle".into())
    }

    /// The New goal frame: the contract form (the old one, as a modal) opens. `draft`: what a conductor card
    /// proposed, filled in; the person still finishes the contract and approves it. A read-only goal has no form yet.
    fn open_goal_form(
        &mut self,
        draft: Option<&synthesis_app::conductor::GoalDraft>,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let kind = match draft.map(|d| d.kind.as_str()) {
            Some("read_only" | "read-only" | "readonly") => GoalKind::ReadOnly,
            _ => GoalKind::Writes,
        };
        self.goal_prefill = draft.map(|d| (d.contract.clone(), d.folder.clone()));
        self.open(Frame::NewGoal(kind), cx);
        self.goal_prefill = None;
        if self.frames.active() != &Frame::NewGoal(kind) {
            return Err(CAP_NOTE.into());
        }
        Ok(match kind {
            GoalKind::Writes => "New goal form".into(),
            GoalKind::ReadOnly => "New read-only goal (its form is not built yet)".into(),
        })
    }

    /// Opens the form of the active New goal frame once (the read-only form comes with its own phase).
    fn start_goal_form(&mut self, cx: &mut Context<Self>) {
        if self.frames.active() != &Frame::NewGoal(GoalKind::Writes) || self.goal_form_opened {
            return;
        }
        let prefill = self.goal_prefill.take();
        self.goal_form_opened = true;
        self.outcomes
            .update(cx, |o, cx| o.open_goal_form(prefill, cx));
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
            Frame::Worker(o, i) => clean(&format!("{o} {i}")),
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
        // before the Cmd/Ctrl chords: on Windows and Linux Ctrl is the secondary key, and Ctrl+Tab is not one of them
        if m.control && !m.alt && !m.platform && key == "tab" {
            self.frames.cycle(if m.shift { -1 } else { 1 });
            self.changed(cx);
            return true;
        }
        if m.secondary(cx) && !m.alt {
            match key {
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
            || self.home.focus_handle(cx).is_focused(window);
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
            f => col()
                .debug_selector(|| "sa-frame".to_string())
                .child(
                    kit::card("sa-frame-card", false)
                        .child(
                            kit::text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG)
                                .child(self.title(f, cx)),
                        )
                        .child(kit::text(t::T2, t::W_REGULAR, t::TEXT).child(match f {
                            Frame::NewGoal(GoalKind::ReadOnly) => {
                                "The read-only goal form is coming. Nothing was created."
                            }
                            _ => "This frame is filled in a later phase.",
                        })),
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
                _ => window.focus(&self.focus, cx),
            }
        }
        let modals = self.outcomes.update(cx, |o, cx| o.modals(window, cx));
        let picker = self.picker_overlay(cx);
        div()
            .id("superagent")
            .debug_selector(|| "superagent".to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| {
                if v.on_key(ev, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .relative()
            .bg(hsla(t::SURFACE))
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
                            .child(self.panel(window, cx)),
                    ),
            )
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
