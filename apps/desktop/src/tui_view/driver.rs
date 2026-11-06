//! Wires the live TUI view to the engine: starting and relaunching it off the UI thread, pumping its
//! messages into [`Live`] (batched, at most one redraw per 16 ms), the keyboard (the classic TUI's keys),
//! the prompt box with its completion popup, and the sidebar clicks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AppContext as _, Context, Entity, Keystroke, ScrollHandle, Subscription, Task, WeakEntity,
    Window,
};
use gpui_component::input::{InputEvent, TextareaState};
use synthesis_config::Env;
use synthesis_tui_base::complete::{self, StdFs};
use synthesis_tui_base::editor::LineEditor;
use synthesis_view::workspaces::{LoadOpts, WorkspaceList, fold_dir};

use super::TuiView;
use crate::engine::{self, Args, Engine, StartOpts, UiMsg};
use crate::live::{Focus, Live, SWITCH_ARM_MS, Submitted, now_ms};

/// What the live view needs from `main`: the environment snapshot and the flags.
pub struct LiveCfg {
    pub env: Env,
    pub args: Args,
    /// Canonical start folder.
    pub project: PathBuf,
    /// The project root `--trust-project` was granted for.
    pub granted: PathBuf,
    pub synthesis_home: PathBuf,
    /// The user's home, for `~` in the panels.
    pub home: Option<String>,
    pub tz_min: i32,
    /// `synt.tui` of the user settings (panels, thinking, verbose, expanded output).
    pub tui: synthesis_settings::TuiSettings,
}

/// Where a relaunch goes.
pub enum Target {
    /// A stored session, continued in place.
    Resume(String),
}

/// Completion popup state: what the token under the cursor offers.
pub struct Comp {
    pub items: Vec<(String, String)>,
    pub sel: usize,
    token: complete::Token,
    inserts: Vec<complete::Item>,
}

/// Prompt history navigation: how far back, and what was being typed.
pub struct HistNav {
    pub back: usize,
    pub draft: String,
}

pub struct LiveUi {
    pub live: Live,
    pub cfg: Arc<LiveCfg>,
    pub prompt: Entity<TextareaState>,
    pub scroll: ScrollHandle,
    /// The chat sticks to the newest row until the person scrolls up.
    pub follow: bool,
    /// The scroll offset after the last frame.
    pub last_y: gpui::Pixels,
    pub dirty: bool,
    pub comp: Option<Comp>,
    pub hist: Option<HistNav>,
    /// Ctrl+R: the needle typed so far and the entry shown.
    pub search: Option<(String, usize)>,
    tick_task: Option<Task<()>>,
    pump_task: Option<Task<()>>,
    engine_task: Option<Task<()>>,
    store_task: Option<Task<()>>,
    peer_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

fn text_of(s: &Entity<TextareaState>, cx: &gpui::App) -> String {
    s.read(cx).value().to_string()
}

impl TuiView {
    /// A view on the real engine; call [`TuiView::start`] to launch it.
    pub fn live(
        window: &mut Window,
        cx: &mut Context<Self>,
        cfg: LiveCfg,
        hidden: super::layout::Hidden,
        inset: f32,
    ) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(
                    "Ask Synthesis anything...  (/ commands, @ files, Shift+Enter for a new line)",
                )
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let mut subs = vec![];
        subs.push(cx.subscribe_in(
            &prompt,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } => this.submit_prompt(window, cx),
                InputEvent::Change => this.on_prompt_changed(cx),
                _ => {}
            },
        ));
        let mut live = Live::new();
        live.show_thinking = cfg.tui.show_thinking;
        live.verbose = cfg.tui.verbose;
        live.expand_all = cfg.tui.diffs_expanded;
        let mut hidden = hidden;
        for (shown, (key, _)) in cfg.tui.panels.iter().zip(synthesis_settings::TUI_PANELS) {
            if !shown {
                hidden.hide(key);
            }
        }
        let mut me = Self::new(inset, hidden);
        me.live = Some(LiveUi {
            live,
            cfg: Arc::new(cfg),
            prompt,
            scroll: ScrollHandle::new(),
            follow: true,
            last_y: gpui::px(0.),
            dirty: true,
            comp: None,
            hist: None,
            search: None,
            tick_task: None,
            pump_task: None,
            engine_task: None,
            store_task: None,
            peer_task: None,
            _subs: subs,
        });
        me
    }

    /// Launches the engine, the clock and the keyboard.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.prompt.update(cx, |s, cx| s.focus(window, cx));
        let weak = cx.weak_entity();
        let key_sub = cx.intercept_keystrokes(move |ev, window, cx| {
            let ks = ev.keystroke.clone();
            let consumed = weak
                .update(cx, |t, cx| t.on_key(&ks, window, cx))
                .unwrap_or(false);
            if consumed {
                cx.stop_propagation();
            }
        });
        lu.add_sub(key_sub);
        let quit_sub = cx.on_app_quit(|this, _| {
            this.shutdown_engine();
            async {}
        });
        lu.add_sub(quit_sub);
        let cfg = lu.cfg.clone();
        // the clock and the polled panels
        let tick = cx.spawn(async move |this, cx| {
            let mut n = 0u64;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                n += 1;
                if this.update(cx, |t, cx| t.tick(n, cx)).is_err() {
                    break;
                }
            }
        });
        let resume_args = cfg.args.common.resume.is_some() || cfg.args.common.continue_session;
        let first = {
            let c = cfg.clone();
            (c.project.clone(), resume_args)
        };
        lu.tick_task = Some(tick);
        self.launch(first.0, None, vec![], cx);
    }

    /// Builds the engine on a worker thread (closing `prior` first), then installs it.
    /// `resume`: a session to continue; with `None` the `--resume`/`--continue` flags apply once.
    fn launch(
        &mut self,
        project: PathBuf,
        prior: Option<Arc<Engine>>,
        notes: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.launch_with(project, prior, None, notes, cx)
    }

    fn launch_with(
        &mut self,
        project: PathBuf,
        prior: Option<Arc<Engine>>,
        target: Option<Target>,
        mut notes: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(lu) = &mut self.live else { return };
        lu.live.starting = true;
        lu.live.engine = None;
        lu.dirty = true;
        let cfg = lu.cfg.clone();
        let first_start = prior.is_none() && target.is_none();
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let prior_sid = prior.as_ref().map(|p| p.sid.clone());
            let mut project = project;
            if let Some(p) = prior {
                engine::close_shared(p);
            }
            let home = cfg.synthesis_home.clone();
            let mut resume = None;
            if first_start {
                let keep = cfg.args.common.resume.clone().unwrap_or_default();
                notes.extend(engine::housekeeping(&home, &keep));
            }
            let want = match &target {
                Some(Target::Resume(id)) => Some(Ok(id.clone())),
                None if first_start => {
                    match engine::requested_resume(&cfg.args.common, &project, &home) {
                        Ok(r) => {
                            resume = r;
                            None
                        }
                        Err(e) => Some(Err(e)),
                    }
                }
                None => None,
            };
            match want {
                Some(Ok(id)) => match engine::load_resume_in_place(&home, &id) {
                    Ok(r) => resume = Some(r),
                    Err(e) => {
                        notes.push(if e.starts_with("cannot resume") {
                            e
                        } else {
                            format!("/resume: {e}")
                        });
                        // the session we came from continues
                        resume = prior_sid.and_then(|s| synthesis_tui::load_resume(&home, &s).ok());
                    }
                },
                Some(Err(e)) => {
                    let _ = tx.send(Err(e));
                    return;
                }
                None => {}
            }
            if let Some(r) = &resume {
                project = PathBuf::from(&r.record.cwd);
            }
            // the replay of the stored conversation, before the new engine adds its own events
            let mut replay = synthesis_view::state::ViewState::default();
            if let Some(r) = &resume {
                for e in &r.events {
                    replay.apply(e);
                }
                if replay.session.cwd.is_empty() {
                    replay.session.cwd = r.record.cwd.clone();
                }
            }
            let (common, note) = engine::common_for(&cfg.args.common, &cfg.granted, &project);
            notes.extend(note);
            let had_resume = resume.is_some();
            let r = Engine::start_with(
                StartOpts {
                    project,
                    common,
                    resume,
                    notes,
                },
                &cfg.env,
            );
            let _ = tx.send(r.map(|(e, rx)| (e, rx, had_resume.then_some(replay))));
        });
        let task = cx.spawn(async move |this, cx| {
            let r = rx
                .await
                .unwrap_or_else(|_| Err("the engine thread ended unexpectedly".into()));
            this.update(cx, |t, cx| t.engine_ready(r, cx)).ok();
        });
        if let Some(lu) = &mut self.live {
            lu.engine_task = Some(task);
        }
        cx.notify();
    }

    fn engine_ready(
        &mut self,
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
        let Some(lu) = &mut self.live else { return };
        lu.live.starting = false;
        lu.dirty = true;
        match r {
            Err(e) => {
                lu.live.note(startup_failure(&e), true);
            }
            Ok((engine, rx, replay)) => {
                let l = &mut lu.live;
                if let Some(v) = replay {
                    // the stored conversation and panels; the store panels reload below
                    l.view.convo = v.convo;
                    l.view.session = v.session;
                    l.view.status = v.status;
                } else {
                    l.view.convo = Default::default();
                    l.view.session = synthesis_view::model::Session::new(&engine.sid);
                    l.view.status = Default::default();
                }
                if let Some(w) = engine.context_window {
                    l.view.status.context.set_window(w);
                }
                for n in &engine.notices {
                    l.note(n.clone(), false);
                }
                l.sid = engine.sid.clone();
                l.cwd = engine.cwd.clone();
                l.trusted = engine.trusted;
                l.commands = engine.commands.clone();
                l.mcp = engine.mcp_status();
                l.queue.clear();
                l.dialogs.clear();
                l.busy = false;
                l.engine = Some(Arc::new(engine));
                lu.follow = true;
                let pump = cx.spawn(async move |this, cx| pump(this, rx, cx).await);
                lu.pump_task = Some(pump);
                self.refresh_store(cx);
            }
        }
        cx.notify();
    }

    fn apply_batch(&mut self, batch: Vec<UiMsg>, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let now = now_ms();
        let mut ended = false;
        for m in batch {
            ended |= lu.live.on_msg(m, now);
        }
        lu.dirty = true;
        if ended {
            self.after_task(cx);
        }
        cx.notify();
    }

    /// A task ended: reload the store panels (the usage, the session list) and start what was queued.
    fn after_task(&mut self, cx: &mut Context<Self>) {
        self.refresh_store(cx);
        self.send_queued(cx);
    }

    fn send_queued(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        if let Some(t) = lu.live.next_queued() {
            self.send_text(t, cx);
        }
    }

    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.follow = true;
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        if let Err(why) = e.send(text) {
            lu.live.busy = false;
            lu.live.note(why, true);
        }
        lu.dirty = true;
        cx.notify();
    }

    /// Reads the workspace list, the usage history and the peers off the UI thread.
    pub fn refresh_store(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let cfg = lu.cfg.clone();
        let root = lu.live.engine.as_ref().map(|e| e.root.clone());
        let job = cx.background_spawn(async move {
            let r =
                synthesis_session::Reader::open(&cfg.synthesis_home).map_err(|e| e.to_string())?;
            let home = cfg.synthesis_home.clone();
            let live = move |id: &str| !synthesis_app::peers::session_lock_free(&home, id);
            let (granted, trust) = (cfg.granted.clone(), cfg.args.common.trust_project);
            // the git scan runs only where `--trust-project` was granted
            let trusted = move |root: &str| trust && engine::root_of(Path::new(root)) == granted;
            let opts = LoadOpts {
                is_live: Some(&live),
                trusted: Some(&trusted),
                env: Some(&cfg.env),
            };
            let ws = WorkspaceList::load(&r, root.as_deref().map(Path::new), &opts)?;
            let usage = synthesis_view::usage::load_from(&r, now_ms(), cfg.tz_min);
            Ok::<_, String>((ws, usage))
        });
        let t = cx.spawn(async move |this, cx| {
            let r = job.await;
            this.update(cx, |t, cx| t.store_loaded(r, cx)).ok();
        });
        lu.store_task = Some(t);
    }

    fn store_loaded(
        &mut self,
        r: Result<(WorkspaceList, Vec<synthesis_view::usage::HistoryDay>), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(lu) = &mut self.live else { return };
        match r {
            Ok((ws, days)) => {
                let l = &mut lu.live;
                l.view.workspaces = ws;
                l.view.usage = synthesis_view::usage::UsageHistory { days };
                let rows = l.peer_rows.clone();
                l.view.set_peers(&rows);
                // the selection follows the open session's folder until the person picks another
                if l.view
                    .workspaces
                    .workspaces
                    .iter()
                    .all(|w| w.key != l.ws_key)
                {
                    l.ws_key = fold_dir(l.engine.as_ref().map_or(&l.cwd, |e| &e.root));
                }
            }
            Err(e) => lu
                .live
                .note(format!("cannot read the session store: {e}"), true),
        }
        lu.dirty = true;
        cx.notify();
    }

    /// Once a second: the clock, the MCP states, the peers every fifth time.
    fn tick(&mut self, n: u64, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.dirty = true;
        if let Some(e) = lu.live.engine.clone() {
            lu.live.mcp = e.mcp_status();
            if n % 5 == 0 {
                let job = cx.background_spawn(async move { e.peer_rows() });
                let t = cx.spawn(async move |this, cx| {
                    let rows = job.await;
                    this.update(cx, |t, cx| {
                        if let Some(lu) = &mut t.live {
                            lu.live.peer_rows = rows.unwrap_or_default();
                            let rows = lu.live.peer_rows.clone();
                            lu.live.view.set_peers(&rows);
                            lu.dirty = true;
                        }
                        cx.notify();
                    })
                    .ok();
                });
                lu.peer_task = Some(t);
            }
        }
        cx.notify();
    }

    /// Ends the session on quit (blocking: the process is going away).
    fn shutdown_engine(&mut self) {
        if let Some(lu) = &mut self.live
            && let Some(e) = lu.live.engine.take()
        {
            engine::close_shared(e);
        }
    }

    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.shutdown_engine();
        cx.quit();
    }

    // ---- the prompt ----

    pub(super) fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let text = text_of(&lu.prompt, cx);
        lu.comp = None;
        lu.hist = None;
        lu.search = None;
        match lu.live.submit(&text) {
            Submitted::Quit => return self.quit(cx),
            Submitted::Send(t) => {
                self.set_prompt("", window, cx);
                self.send_text(t, cx);
                return;
            }
            Submitted::Nothing => {}
        }
        if !text.trim().is_empty() {
            self.set_prompt("", window, cx);
        }
        let Some(lu) = &mut self.live else { return };
        lu.follow = true;
        lu.dirty = true;
        cx.notify();
    }

    fn set_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.prompt.update(cx, |s, cx| {
            s.set_value(text.to_string(), window, cx);
            // the cursor goes to the end (set_value leaves it where it was)
            let end = s.value().len();
            s.set_selected_range(end..end, cx);
        });
        self.on_prompt_changed(cx);
    }

    /// The text changed (typing, paste, history, completion): refresh the popup.
    fn on_prompt_changed(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let (text, cursor) = {
            let s = lu.prompt.read(cx);
            (s.value().to_string(), s.cursor())
        };
        lu.comp = suggest(&text, cursor, &lu.live, lu.cfg.project.as_path());
        lu.dirty = true;
        cx.notify();
    }

    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let Some(c) = lu.comp.take() else { return };
        let (text, cursor) = {
            let s = lu.prompt.read(cx);
            (s.value().to_string(), s.cursor())
        };
        let mut ed = editor_at(&text, cursor);
        complete::apply(&mut ed, &c.token, &c.inserts[c.sel]);
        let new = ed.text();
        let byte = ed.chars[..ed.cur]
            .iter()
            .map(|c| c.len_utf8())
            .sum::<usize>();
        lu.prompt.update(cx, |s, cx| {
            s.set_value(new, window, cx);
            s.set_selected_range(byte..byte, cx);
        });
        self.on_prompt_changed(cx);
    }

    // ---- keys ----

    /// Handles a keystroke before the focused input sees it. True: consumed.
    fn on_key(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(lu) = &mut self.live else {
            return false;
        };
        let now = now_ms();
        let key = ks.key.as_str();
        let (ctrl, shift) = (ks.modifiers.control, ks.modifiers.shift);
        let cmd = ks.modifiers.platform || ks.modifiers.alt;
        let digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit() && key != "0";
        // a dialog owns the keyboard
        if !lu.live.dialogs.is_empty() {
            let mut out = None;
            match key {
                "up" => lu.live.move_sel(false),
                "down" => lu.live.move_sel(true),
                "tab" => lu.live.move_sel(!shift),
                "enter" => {
                    let sel = lu.live.dialogs[0].sel;
                    out = lu.live.pick(sel, now);
                }
                "escape" => out = lu.live.reject(),
                "c" if ctrl => out = lu.live.reject(),
                _ if digit && !ctrl && !cmd => {
                    out = lu.live.pick(usize::from(key.as_bytes()[0] - b'1'), now);
                }
                _ => {}
            }
            if let Some(k) = out {
                lu.live.answer(&k, now);
            }
            lu.dirty = true;
            cx.notify();
            return true;
        }
        // Ctrl+R: history search
        if let Some((needle, at)) = lu.search.clone() {
            match key {
                "escape" => lu.search = None,
                "enter" => {
                    lu.search = None;
                }
                "r" if ctrl => {
                    let next = lu.live.history_search(&needle, at + 1).unwrap_or(at);
                    lu.search = Some((needle, next));
                }
                "backspace" => {
                    let mut n = needle;
                    n.pop();
                    let at = lu.live.history_search(&n, 0).unwrap_or(0);
                    lu.search = Some((n, at));
                }
                _ if !ctrl && !cmd && ks.key_char.is_some() => {
                    let n = format!("{needle}{}", ks.key_char.as_deref().unwrap_or(""));
                    match lu.live.history_search(&n, 0) {
                        Some(at) => lu.search = Some((n, at)),
                        None => lu.search = Some((n, at)),
                    }
                }
                _ => return false,
            }
            self.show_search_hit(window, cx);
            return true;
        }
        match (key, ctrl) {
            ("c", true) => {
                if lu.live.ctrl_c(now) {
                    self.quit(cx);
                } else {
                    lu.live.note("press Ctrl+C again to quit", false);
                    lu.dirty = true;
                    cx.notify();
                }
                return true;
            }
            ("d", true) if text_of(&lu.prompt, cx).is_empty() => {
                self.quit(cx);
                return true;
            }
            ("o", true) => {
                lu.live.expand_all = !lu.live.expand_all;
                lu.dirty = true;
                cx.notify();
                return true;
            }
            ("r", true) => {
                lu.search = Some((String::new(), 0));
                self.show_search_hit(window, cx);
                return true;
            }
            ("pageup", _) | ("pagedown", _) => {
                let h = f32::from(lu.scroll.bounds().size.height);
                let mut o = lu.scroll.offset();
                let step = gpui::px((h - 40.).max(40.));
                o.y = if key == "pageup" {
                    (o.y + step).min(gpui::px(0.))
                } else {
                    (o.y - step).max(-lu.scroll.max_offset().y)
                };
                lu.scroll.set_offset(o);
                lu.follow = o.y <= -lu.scroll.max_offset().y + gpui::px(1.);
                cx.notify();
                return true;
            }
            ("tab", _) if shift => {
                lu.live.cycle_mode();
                lu.dirty = true;
                cx.notify();
                return true;
            }
            _ => {}
        }
        if lu.live.focus != Focus::Chat {
            return self.sidebar_key(key, ks, window, cx);
        }
        // the completion popup
        if let Some(c) = &mut lu.comp {
            match key {
                "up" => {
                    c.sel = (c.sel + c.items.len() - 1) % c.items.len();
                    cx.notify();
                    return true;
                }
                "down" => {
                    c.sel = (c.sel + 1) % c.items.len();
                    cx.notify();
                    return true;
                }
                "tab" => {
                    self.accept_completion(window, cx);
                    return true;
                }
                "escape" => {
                    lu.comp = None;
                    cx.notify();
                    return true;
                }
                _ => {}
            }
        }
        let (text, cursor) = {
            let s = lu.prompt.read(cx);
            (s.value().to_string(), s.cursor())
        };
        match key {
            "escape" => {
                if lu.live.interrupt() {
                    lu.dirty = true;
                    cx.notify();
                    return true;
                }
                false
            }
            "tab" if text.is_empty() => {
                lu.live.focus = lu.live.focus.next();
                lu.dirty = true;
                cx.notify();
                true
            }
            "up" if !text[..cursor.min(text.len())].contains('\n') => {
                self.history_step(true, window, cx)
            }
            "down" if lu.hist.is_some() && !text[cursor.min(text.len())..].contains('\n') => {
                self.history_step(false, window, cx)
            }
            _ => false,
        }
    }

    fn sidebar_key(
        &mut self,
        key: &str,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(lu) = &mut self.live else {
            return false;
        };
        let n = match lu.live.focus {
            Focus::Workspace => lu.live.view.workspaces.workspaces.len(),
            _ => self.snap.running.len(),
        };
        match key {
            "tab" => lu.live.focus = lu.live.focus.next(),
            "escape" => lu.live.focus = Focus::Chat,
            "up" | "down" | "pageup" | "pagedown" | "home" | "end" => {
                let down = matches!(key, "down" | "pagedown" | "end");
                let step = if key.starts_with("page") { 5 } else { 1 };
                let cur = match lu.live.focus {
                    Focus::Workspace => lu
                        .live
                        .view
                        .workspaces
                        .workspaces
                        .iter()
                        .position(|w| w.key == lu.live.ws_key)
                        .unwrap_or(0),
                    _ => lu.live.run_sel,
                };
                let next = match key {
                    "home" => 0,
                    "end" => n.saturating_sub(1),
                    _ if down => (cur + step).min(n.saturating_sub(1)),
                    _ => cur.saturating_sub(step),
                };
                self.select_row(next, cx);
            }
            "enter" => match lu.live.focus {
                Focus::Workspace => {
                    // its sessions get the keys
                    lu.live.focus = Focus::Running;
                    lu.live.run_sel = 0;
                }
                _ => {
                    let sel = lu.live.run_sel;
                    self.open_row(sel, cx);
                }
            },
            // typing goes back to the chat (the character is inserted there)
            _ if ks.key_char.is_some() && !ks.modifiers.control && !ks.modifiers.platform => {
                lu.live.focus = Focus::Chat;
                lu.dirty = true;
                cx.notify();
                return false;
            }
            _ => return false,
        }
        let Some(lu) = &mut self.live else {
            return true;
        };
        lu.prompt.update(cx, |s, cx| s.focus(window, cx));
        lu.dirty = true;
        cx.notify();
        true
    }

    fn select_row(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        match lu.live.focus {
            Focus::Workspace => {
                if let Some(w) = lu.live.view.workspaces.workspaces.get(i) {
                    lu.live.ws_key = w.key.clone();
                    lu.live.run_sel = 0;
                }
            }
            _ => lu.live.run_sel = i,
        }
        lu.dirty = true;
        cx.notify();
    }

    /// Click on a Workspace row: select it; Running lists its sessions.
    pub fn click_workspace(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.live.focus = Focus::Workspace;
        self.select_row(i, cx);
    }

    /// Click on a Running row: open that session (the open one: nothing happens).
    pub fn click_running(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.live.focus = Focus::Running;
        lu.live.run_sel = i;
        self.open_row(i, cx);
    }

    fn open_row(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(row) = self.snap.running.get(i).cloned() else {
            return;
        };
        let Some(lu) = &mut self.live else { return };
        if row.peer || row.id.is_empty() || row.id == lu.live.sid {
            lu.dirty = true;
            cx.notify();
            return;
        }
        self.open_session(&row.id, cx);
    }

    /// Resume a stored session: the engine restarts on it (like the TUI's relaunch). While a task
    /// runs, the second request within 5 s cancels it and switches.
    pub fn open_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let now = now_ms();
        if lu.live.busy {
            let armed = lu
                .live
                .switch_armed
                .as_ref()
                .is_some_and(|(a, t)| a == id && now < *t);
            if !armed {
                lu.live.switch_armed = Some((id.to_string(), now + SWITCH_ARM_MS));
                lu.live.note(
                    "a task is running: pick the session again within 5 s to cancel it and switch",
                    false,
                );
                lu.dirty = true;
                cx.notify();
                return;
            }
            if let Some(e) = &lu.live.engine {
                e.cancel();
            }
            lu.live.note("task cancelled: switching session", false);
        }
        lu.live.switch_armed = None;
        let prior = lu.live.engine.take();
        let project = lu.cfg.project.clone();
        lu.live.queue.clear();
        lu.live.dialogs.clear();
        lu.live.busy = false;
        self.launch_with(
            project,
            prior,
            Some(Target::Resume(id.to_string())),
            vec![],
            cx,
        );
    }

    // ---- history ----

    fn history_step(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(lu) = &mut self.live else {
            return false;
        };
        let cur = lu.hist.as_ref().map(|h| h.back);
        let text = text_of(&lu.prompt, cx);
        let next = match (cur, older) {
            (None, true) => Some(0),
            (Some(b), true) => Some(b + 1),
            (Some(0), false) => None,
            (Some(b), false) => Some(b - 1),
            (None, false) => return false,
        };
        let shown = match next {
            Some(b) => match lu.live.history_at(b) {
                Some(h) => h.to_string(),
                None => return true, // past the oldest
            },
            None => lu.hist.take().map_or(String::new(), |h| h.draft),
        };
        if let Some(b) = next {
            let draft = lu.hist.take().map_or(text, |h| h.draft);
            lu.hist = Some(HistNav { back: b, draft });
        }
        self.set_prompt(&shown, window, cx);
        true
    }

    fn show_search_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let hit = lu
            .search
            .as_ref()
            .and_then(|(n, at)| {
                lu.live
                    .history_at(*at)
                    .filter(|h| n.is_empty() || h.to_lowercase().contains(&n.to_lowercase()))
            })
            .map(str::to_string);
        if let Some(h) = hit {
            self.set_prompt(&h, window, cx);
        }
        if let Some(lu) = &mut self.live {
            lu.dirty = true;
        }
        cx.notify();
    }
}

impl LiveUi {
    fn add_sub(&mut self, s: Subscription) {
        self._subs.push(s);
    }
}

/// The message pump: everything that arrived is applied in one go, then at most one redraw per 16 ms.
async fn pump(
    this: WeakEntity<TuiView>,
    mut rx: futures::channel::mpsc::UnboundedReceiver<UiMsg>,
    cx: &mut gpui::AsyncApp,
) {
    while let Some(m) = rx.next().await {
        let mut batch = vec![m];
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        if this.update(cx, |t, cx| t.apply_batch(batch, cx)).is_err() {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
}

/// Why the engine did not start, for the chat. A missing model gets the setup card of `synt`.
fn startup_failure(e: &str) -> String {
    if e.starts_with("no model configured") {
        [
            "Synthesis needs a model and an API key before it can run a task.".to_string(),
            format!("why: {e}"),
            String::new(),
            "1. Keep the key in an environment variable you manage (never in a repository):".into(),
            "    export MY_PROVIDER_KEY=...".into(),
            "2. Point your user config at it, in $SYNT_HOME/synt.json (~/.synt by default):".into(),
            r#"    {"$schema": "config.json/v1", "model": "anthropic/<model>","#.into(),
            r#"     "providers": {"anthropic": {"apiKey": "{env:MY_PROVIDER_KEY}"}}}"#.into(),
            "3. `synt models` lists what is configured, `synt doctor` checks the installation;"
                .into(),
            "   or start the app once with `--model provider/model`.".into(),
        ]
        .join("\n")
    } else {
        format!("cannot start the engine: {e}")
    }
}

/// A `LineEditor` holding `text` with the cursor at byte offset `cursor`.
fn editor_at(text: &str, cursor: usize) -> LineEditor {
    let mut ed = LineEditor::new();
    ed.chars = text.chars().collect();
    let cur = text
        .char_indices()
        .take_while(|(i, _)| *i < cursor.min(text.len()))
        .count();
    ed.cur = cur;
    ed
}

/// The completion for the token under the cursor: `/` commands and `@` files.
pub fn suggest(text: &str, cursor: usize, live: &Live, cwd: &Path) -> Option<Comp> {
    let ed = editor_at(text, cursor);
    let cwd_now = live
        .engine
        .as_ref()
        .map_or_else(|| cwd.to_path_buf(), |e| PathBuf::from(&e.cwd));
    let mut o = complete::Options::new(&cwd_now);
    o.commands = &live.commands;
    o.limit = 8;
    let s = complete::suggest(&ed, &o, &StdFs)?;
    let now = now_ms();
    let items = s
        .items
        .iter()
        .map(|i| (i.label.clone(), i.detail(&StdFs, now)))
        .collect();
    Some(Comp {
        items,
        sel: 0,
        token: s.token,
        inserts: s.items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slash_offers_commands_and_the_cursor_is_in_chars() {
        let mut l = Live::new();
        l.commands = synthesis_tui::local_commands();
        let c = suggest("/mo", 3, &l, Path::new("/")).expect("commands");
        assert!(c.items.iter().any(|(label, _)| label == "/model"));
        assert!(suggest("plain words", 5, &l, Path::new("/")).is_none());
        // multi-byte text before the token: the byte cursor maps to the right char
        let ed = editor_at("é /mo", 6);
        assert_eq!(ed.cur, 5);
        // a slash only starts a command at the start of the message
        assert!(suggest("é /mo", 6, &l, Path::new("/")).is_none());
    }

    #[test]
    fn at_offers_files_of_the_folder() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("notes.txt"), "x").unwrap();
        let root = synthesis_core::canonicalize(d.path()).unwrap();
        let l = Live::new();
        let c = suggest("look at @no", 11, &l, &root).expect("files");
        assert!(
            c.items.iter().any(|(label, _)| label.contains("notes.txt")),
            "{:?}",
            c.items
        );
    }
}
