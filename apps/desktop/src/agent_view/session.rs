//! The Agent view's sessions: one engine, one `Live` and one message pump per open session (several of
//! the workspace can be open at once, each a tab), the prompt, the keyboard and the store readers
//! (workspace list, verification reports, project tree). The rules of a session are the TUI view's
//! (`live.rs`); this file only wires them to gpui.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AppContext as _, Context, Keystroke, Pixels, ScrollHandle, Task, WeakEntity, Window, px,
};
use synthesis_view::agent as panes;
use synthesis_view::project::{self, ProjectData};

use super::AgentView;
use crate::engine::{self, Engine, StartOpts, UiMsg};
use crate::live::{Live, Submitted, now_ms};
use crate::tui_view::driver::{read_store, startup_failure};
use crate::tui_view::sample::Snapshot;
use crate::tui_view::snapshot;

/// Sessions open at once: each holds an engine (a runtime, its MCP servers and a model client).
pub const MAX_SESSIONS: usize = 4;

pub struct AgentSession {
    pub uid: u64,
    pub live: Live,
    pub snap: Option<Snapshot>,
    pub dirty: bool,
    pub scroll: ScrollHandle,
    /// The conversation sticks to the newest row until the person scrolls up.
    pub follow: bool,
    pub last_y: Pixels,
    pub sub_scroll: ScrollHandle,
    sub_seen: usize,
    /// The sub-agent shown beside the conversation; `None` is `main`.
    pub sub: Option<String>,
    loading_reports: bool,
    pump: Option<Task<()>>,
    engine_task: Option<Task<()>>,
    report_task: Option<Task<()>>,
}

impl AgentSession {
    pub fn new(uid: u64, live: Live) -> Self {
        Self {
            uid,
            live,
            snap: None,
            dirty: true,
            scroll: ScrollHandle::new(),
            follow: true,
            last_y: px(0.),
            sub_scroll: ScrollHandle::new(),
            sub_seen: 0,
            sub: None,
            loading_reports: false,
            pump: None,
            engine_task: None,
            report_task: None,
        }
    }

    /// The tab label: the first thing asked, else "new session".
    pub fn title(&self) -> String {
        self.live
            .view
            .convo
            .items
            .iter()
            .find_map(|i| match i {
                synthesis_view::convo::Item::User(t) => Some(t.lines().next().unwrap_or("")),
                _ => None,
            })
            .map_or_else(
                || "new session".to_string(),
                |t| t.chars().take(22).collect(),
            )
    }
}

/// The project tree of the active session's folder (trust-gated), for the file explorer.
#[derive(Default)]
pub struct Tree {
    pub data: Option<ProjectData>,
    pub for_cwd: String,
    pub loading: bool,
    /// The file whose git diff is shown.
    pub sel: Option<String>,
}

pub const UNTRUSTED: &str =
    "files are listed in a trusted folder only: start with --trust-project (git runs nowhere else)";

impl AgentView {
    pub(super) fn index_of(&self, uid: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.uid == uid)
    }

    pub fn active_session(&self) -> Option<&AgentSession> {
        self.sessions.get(self.active)
    }

    pub(super) fn active_mut(&mut self) -> Option<&mut AgentSession> {
        self.sessions.get_mut(self.active)
    }

    /// The view is shown (or hidden): the first time starts the first session.
    pub fn set_shown(&mut self, shown: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.shown = shown;
        if shown {
            if self.cfg.is_some() && self.sessions.is_empty() {
                self.open(None, cx);
            }
            self.prompt.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Opens a session in a new tab: a new one, or the stored `resume` (focused if already open).
    pub fn open(&mut self, resume: Option<String>, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        if let Some(id) = &resume
            && let Some(i) = self.sessions.iter().position(|s| &s.live.sid == id)
        {
            self.active = i;
            return cx.notify();
        }
        if self.sessions.len() >= MAX_SESSIONS {
            if let Some(s) = self.active_mut() {
                s.live.note(
                    format!("{MAX_SESSIONS} sessions are open; close a tab first"),
                    true,
                );
                s.dirty = true;
            }
            return cx.notify();
        }
        self.next_uid += 1;
        let uid = self.next_uid;
        self.sessions.push(AgentSession::new(uid, Live::new()));
        self.active = self.sessions.len() - 1;
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let home = cfg.synthesis_home.clone();
            let mut notes = vec![];
            let mut project: PathBuf = cfg.project.clone();
            let mut resume_data = None;
            let mut replay = None;
            if let Some(id) = &resume {
                match engine::load_resume_in_place(&home, id) {
                    Ok(r) => {
                        project = PathBuf::from(&r.record.cwd);
                        let mut v = synthesis_view::state::ViewState::default();
                        for e in &r.events {
                            v.apply(e);
                        }
                        if v.session.cwd.is_empty() {
                            v.session.cwd = r.record.cwd.clone();
                        }
                        replay = Some(v);
                        resume_data = Some(r);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
            let (common, note) = engine::common_for(&cfg.args.common, &cfg.granted, &project);
            notes.extend(note);
            let r = Engine::start_with(
                StartOpts {
                    project,
                    common,
                    resume: resume_data,
                    notes,
                },
                &cfg.env,
            );
            let _ = tx.send(r.map(|(e, rx)| (e, rx, replay)));
        });
        let task = cx.spawn(async move |this, cx| {
            let r = rx
                .await
                .unwrap_or_else(|_| Err("the engine thread ended unexpectedly".into()));
            this.update(cx, |t, cx| t.engine_ready(uid, r, cx)).ok();
        });
        if let Some(s) = self.sessions.last_mut() {
            s.engine_task = Some(task);
        }
        cx.notify();
    }

    #[allow(clippy::type_complexity)]
    fn engine_ready(
        &mut self,
        uid: u64,
        r: Result<
            (
                Engine,
                futures::channel::mpsc::UnboundedReceiver<UiMsg>,
                Option<synthesis_view::state::ViewState>,
            ),
            String,
        >,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.index_of(uid) else {
            // the tab was closed while the engine started
            if let Ok((e, _, _)) = r {
                std::thread::spawn(move || e.close());
            }
            return;
        };
        let s = &mut self.sessions[i];
        s.live.starting = false;
        s.dirty = true;
        match r {
            Err(e) => s.live.note(startup_failure(&e), true),
            Ok((engine, rx, replay)) => {
                s.live.attach(Arc::new(engine), replay);
                s.follow = true;
                s.pump = Some(cx.spawn(async move |this, cx| pump(this, uid, rx, cx).await));
                self.refresh_store(uid, cx);
                self.load_reports(uid, cx);
            }
        }
        cx.notify();
    }

    fn apply_batch(&mut self, uid: u64, batch: Vec<UiMsg>, cx: &mut Context<Self>) {
        let Some(i) = self.index_of(uid) else { return };
        let s = &mut self.sessions[i];
        let now = now_ms();
        let mut ended = false;
        for m in batch {
            ended |= s.live.on_msg(m, now);
        }
        s.dirty = true;
        if !s.live.view.panes.unloaded_reports().is_empty() {
            self.load_reports(uid, cx);
        }
        if ended {
            self.after_task(uid, cx);
        }
        cx.notify();
    }

    /// A task ended: reload the store (workspace list, reports, project tree) and start what was queued.
    fn after_task(&mut self, uid: u64, cx: &mut Context<Self>) {
        self.refresh_store(uid, cx);
        self.load_reports(uid, cx);
        if self.index_of(uid) == Some(self.active) {
            self.request_scan(cx);
        }
        let Some(i) = self.index_of(uid) else { return };
        if let Some(t) = self.sessions[i].live.next_queued() {
            self.send_text(uid, t, cx);
        }
    }

    fn send_text(&mut self, uid: u64, text: String, cx: &mut Context<Self>) {
        let Some(i) = self.index_of(uid) else { return };
        let s = &mut self.sessions[i];
        s.follow = true;
        let Some(e) = s.live.engine.clone() else {
            return;
        };
        if let Err(why) = e.send(text) {
            s.live.busy = false;
            s.live.note(why, true);
        }
        s.dirty = true;
        cx.notify();
    }

    /// The workspace list, the usage history and the sessions, read off the UI thread.
    pub fn refresh_store(&mut self, uid: u64, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        let Some(i) = self.index_of(uid) else { return };
        let root = self.sessions[i]
            .live
            .engine
            .as_ref()
            .map(|e| e.root.clone());
        let job = cx.background_spawn(async move { read_store(&cfg, root.as_deref()) });
        cx.spawn(async move |this, cx| {
            let r = job.await;
            this.update(cx, |t, cx| t.store_loaded(uid, r, cx)).ok();
        })
        .detach();
    }

    #[allow(clippy::type_complexity)]
    fn store_loaded(
        &mut self,
        uid: u64,
        r: Result<
            (
                synthesis_view::workspaces::WorkspaceList,
                Vec<synthesis_view::usage::HistoryDay>,
            ),
            String,
        >,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.index_of(uid) else { return };
        let l = &mut self.sessions[i].live;
        match r {
            Ok((ws, days)) => {
                l.view.workspaces = ws;
                l.view.usage = synthesis_view::usage::UsageHistory { days };
                let rows = l.peer_rows.clone();
                l.view.set_peers(&rows);
                if l.view
                    .workspaces
                    .workspaces
                    .iter()
                    .all(|w| w.key != l.ws_key)
                {
                    l.ws_key = synthesis_view::workspaces::fold_dir(
                        l.engine.as_ref().map_or(&l.cwd, |e| &e.root),
                    );
                }
            }
            Err(e) => l.note(format!("cannot read the session store: {e}"), true),
        }
        self.sessions[i].dirty = true;
        cx.notify();
    }

    /// Reads the checks behind the `verification.completed` events from the store.
    fn load_reports(&mut self, uid: u64, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        let Some(i) = self.index_of(uid) else { return };
        let s = &mut self.sessions[i];
        let todo = s.live.view.panes.unloaded_reports();
        if todo.is_empty() || s.loading_reports {
            return;
        }
        s.loading_reports = true;
        let job = cx.background_spawn(async move {
            let r = synthesis_session::Reader::open(&cfg.synthesis_home).ok()?;
            Some(
                todo.into_iter()
                    .filter_map(|(id, task, attempt)| {
                        panes::read_report(&r, &task, &id, attempt).map(|v| (id, v))
                    })
                    .collect::<Vec<_>>(),
            )
        });
        s.report_task = Some(cx.spawn(async move |this, cx| {
            let r = job.await;
            this.update(cx, |t, cx| {
                if let Some(i) = t.index_of(uid) {
                    let s = &mut t.sessions[i];
                    s.loading_reports = false;
                    for (id, v) in r.into_iter().flatten() {
                        s.live.view.panes.set_report(&id, &v);
                    }
                    s.dirty = true;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Scans the active session's folder for the file explorer. Only a trusted folder is scanned
    /// (a repository's git config can name programs); otherwise the tree says why it is empty.
    pub fn request_scan(&mut self, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        let Some(s) = self.sessions.get(self.active) else {
            return;
        };
        let cwd = s.live.cwd.clone();
        if cwd.is_empty() || self.tree.loading {
            return;
        }
        if !s.live.trusted {
            self.tree.data = Some(ProjectData {
                loaded: true,
                note: Some(UNTRUSTED.into()),
                ..Default::default()
            });
            self.tree.for_cwd = cwd;
            return;
        }
        self.tree.loading = true;
        let dir = PathBuf::from(&cwd);
        let job = cx.background_spawn(async move { project::scan(&dir, &cfg.env) });
        cx.spawn(async move |this, cx| {
            let d = job.await;
            this.update(cx, |t, cx| {
                t.tree.loading = false;
                t.tree.data = Some(d);
                t.tree.for_cwd = cwd;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Closes a tab: its engine ends on a worker thread (it blocks).
    pub fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.sessions.len() {
            return;
        }
        let s = self.sessions.remove(i);
        if let Some(e) = s.live.engine {
            std::thread::spawn(move || engine::close_shared(e));
        }
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        } else if self.active > i {
            self.active -= 1;
        }
        self.tree = Tree::default();
        cx.notify();
    }

    pub fn select_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.sessions.len() && i != self.active {
            self.active = i;
            self.tree = Tree::default();
            if self.tab.needs_tree() {
                self.request_scan(cx);
            }
            cx.notify();
        }
    }

    /// Ends every engine on quit (blocking: the process is going away).
    pub(super) fn shutdown(&mut self) {
        for s in &mut self.sessions {
            if let Some(e) = s.live.engine.take() {
                engine::close_shared(e);
            }
        }
    }

    /// Once a second: the clock and the MCP states.
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        for s in &mut self.sessions {
            if let Some(e) = s.live.engine.clone() {
                s.live.mcp = e.mcp_status();
            }
            s.dirty = true;
        }
        cx.notify();
    }

    // ---- the prompt ----

    pub(super) fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().to_string();
        let Some(s) = self.sessions.get_mut(self.active) else {
            return;
        };
        let uid = s.uid;
        match s.live.submit(&text) {
            // the desktop window is closed with the window, not with /exit
            Submitted::Quit => {
                s.live.note("close the window to quit", false);
            }
            Submitted::Send(t) => {
                self.set_prompt("", window, cx);
                return self.send_text(uid, t, cx);
            }
            Submitted::Nothing => {}
        }
        if !text.trim().is_empty() {
            self.set_prompt("", window, cx);
        }
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.follow = true;
            s.dirty = true;
        }
        cx.notify();
    }

    fn set_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt.update(cx, |s, cx| {
            s.set_value(text.to_string(), window, cx);
            let end = s.value().len();
            s.set_selected_range(end..end, cx);
        });
    }

    /// A click on option `i` of the front dialog of the active session.
    pub(super) fn click_option(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(s) = self.sessions.get_mut(self.active) else {
            return;
        };
        let now = now_ms();
        if let Some(k) = s.live.pick(i, now) {
            s.live.answer(&k, now);
        }
        s.dirty = true;
        cx.notify();
    }

    // ---- keys ----

    /// Handles a keystroke before the focused input sees it. True: consumed.
    pub(super) fn on_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> bool {
        if !self.shown {
            return false;
        }
        let Some(s) = self.sessions.get_mut(self.active) else {
            return false;
        };
        let now = now_ms();
        let key = ks.key.as_str();
        let (ctrl, shift) = (ks.modifiers.control, ks.modifiers.shift);
        let cmd = ks.modifiers.platform || ks.modifiers.alt;
        let digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit() && key != "0";
        // a dialog owns the keyboard
        if !s.live.dialogs.is_empty() {
            let mut out = None;
            match key {
                "up" => s.live.move_sel(false),
                "down" => s.live.move_sel(true),
                "tab" => s.live.move_sel(!shift),
                "enter" => {
                    let sel = s.live.dialogs[0].sel;
                    out = s.live.pick(sel, now);
                }
                "escape" => out = s.live.reject(),
                "c" if ctrl => out = s.live.reject(),
                _ if digit && !ctrl && !cmd => {
                    out = s.live.pick(usize::from(key.as_bytes()[0] - b'1'), now);
                }
                _ => {}
            }
            if let Some(k) = out {
                s.live.answer(&k, now);
            }
            s.dirty = true;
            cx.notify();
            return true;
        }
        match (key, ctrl) {
            ("c", true) | ("escape", _) => {
                let did = s.live.interrupt();
                s.dirty = true;
                cx.notify();
                return did;
            }
            ("o", true) => {
                s.live.expand_all = !s.live.expand_all;
                s.dirty = true;
                cx.notify();
                return true;
            }
            ("pageup", _) | ("pagedown", _) => {
                let h = f32::from(s.scroll.bounds().size.height);
                let mut o = s.scroll.offset();
                let step = px((h - 40.).max(40.));
                o.y = if key == "pageup" {
                    (o.y + step).min(px(0.))
                } else {
                    (o.y - step).max(-s.scroll.max_offset().y)
                };
                s.scroll.set_offset(o);
                s.follow = o.y <= -s.scroll.max_offset().y + px(1.);
                cx.notify();
                return true;
            }
            ("tab", _) if shift => {
                s.live.cycle_mode();
                s.dirty = true;
                cx.notify();
                return true;
            }
            _ => {}
        }
        false
    }

    /// Before drawing: rebuild the snapshot of the active session when it changed, and keep the
    /// conversation on the newest row unless the person scrolled up.
    pub(super) fn refresh(&mut self) {
        let tz = self.cfg.as_ref().map_or(0, |c| c.tz_min);
        let home = self.cfg.as_ref().and_then(|c| c.home.clone());
        for s in &mut self.sessions {
            if s.dirty || s.snap.is_none() {
                s.dirty = false;
                s.snap = Some(snapshot::build(&s.live, now_ms(), tz, home.as_deref()));
            }
        }
        let Some(s) = self.sessions.get_mut(self.active) else {
            return;
        };
        let (o, mx) = (s.scroll.offset().y, s.scroll.max_offset().y);
        if s.follow {
            if o > s.last_y + px(1.) {
                s.follow = false;
            }
        } else if mx > px(0.) && o <= -mx + px(2.) {
            s.follow = true;
        }
        s.last_y = o;
        if s.follow {
            s.scroll.scroll_to_bottom();
        }
        // the picked sub-agent's log follows its newest entry
        let n = s
            .sub
            .as_deref()
            .and_then(|id| s.live.view.panes.sub(id))
            .map_or(0, |a| a.entries.len());
        if n != s.sub_seen {
            s.sub_seen = n;
            s.sub_scroll.scroll_to_bottom();
        }
    }
}

/// The message pump of one session: everything that arrived is applied in one go, then at most one
/// redraw per 16 ms.
async fn pump(
    this: WeakEntity<AgentView>,
    uid: u64,
    mut rx: futures::channel::mpsc::UnboundedReceiver<UiMsg>,
    cx: &mut gpui::AsyncApp,
) {
    while let Some(m) = rx.next().await {
        let mut batch = vec![m];
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        if this
            .update(cx, |t, cx| t.apply_batch(uid, batch, cx))
            .is_err()
        {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
}
