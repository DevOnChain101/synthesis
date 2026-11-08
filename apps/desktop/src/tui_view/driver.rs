//! Wires the live TUI view to the engine: starting and relaunching it off the UI thread, pumping its
//! messages into [`Live`] (batched, at most one redraw per 16 ms), the keyboard (the classic TUI's keys),
//! the prompt box with its completion popup, and the sidebar clicks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AppContext as _, Context, Entity, FollowMode, Keystroke, ListAlignment, ListState,
    ScrollHandle, Subscription, Task, WeakEntity, Window,
};
use gpui_component::input::{InputEvent, TextareaState};
use synthesis_config::Env;
use synthesis_tui_base::complete::{self, StdFs};
use synthesis_tui_base::editor::LineEditor;
use synthesis_view::workspaces::{LoadOpts, WorkspaceList, fold_dir};

use super::{TuiView, panels};
use crate::engine::{self, Args, Engine, StartOpts, UiMsg};
use crate::live::{Action, Focus, Live, SWITCH_ARM_MS, Submitted, now_ms};

/// What the live view needs from `main`: the environment snapshot and the flags.
#[derive(Clone)]
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
    /// The same session on this `provider/model`.
    Model(String),
    /// A fresh session in this folder.
    New(PathBuf),
    /// Hide the session we came from and open a fresh one in its folder.
    Close,
}

/// `/config`: asks the shell to show the Settings view.
pub struct OpenSettings;

impl gpui::EventEmitter<OpenSettings> for TuiView {}

/// Rows at the end of the chat measured again after every change.
const TAIL_REMEASURE: usize = 12;

/// How long the row a click in Recent Activity jumped to stays highlighted.
pub const FLASH_MS: i64 = 1500;

/// One row of the chat list: an item of the conversation, the plan card or the working line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Item(usize),
    Plan,
    Activity,
}

/// What the worker thread hands back.
pub struct Launched {
    pub engine: Engine,
    pub rx: futures::channel::mpsc::UnboundedReceiver<UiMsg>,
    /// The stored conversation of a resumed session.
    pub replay: Option<synthesis_view::state::ViewState>,
    /// The model the session runs on when the person chose one (kept for later relaunches).
    pub model: Option<String>,
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
    /// The chat is a virtual list: only the rows near the viewport are built.
    pub list: ListState,
    pub rows: Vec<Row>,
    /// The next sync starts the list over (another session, `/clear`).
    pub list_reset: bool,
    /// The chat sticks to the newest row until the person scrolls up (read off the list).
    pub follow: bool,
    /// Scroll handles of the Workspace and Running bodies and of a picker dialog.
    pub ws_scroll: ScrollHandle,
    pub run_scroll: ScrollHandle,
    /// The chat item a click in Recent Activity jumped to, and when its highlight ends (epoch ms).
    pub flash: Option<(usize, i64)>,
    pub dlg_scroll: ScrollHandle,
    /// The model chosen with `/model`, applied to every relaunch until a resume.
    pub model_override: Option<String>,
    pub dirty: bool,
    pub comp: Option<Comp>,
    pub hist: Option<HistNav>,
    /// Ctrl+R: the needle typed so far and the entry shown.
    pub search: Option<(String, usize)>,
    /// The placeholder shows the busy hint (Enter queue, send now) rather than the idle one.
    busy_hint: bool,
    tick_task: Option<Task<()>>,
    pump_task: Option<Task<()>>,
    engine_task: Option<Task<()>>,
    /// `set_live` fed the conversation: no engine is launched.
    fixture: bool,
    store_task: Option<Task<()>>,
    peer_task: Option<Task<()>>,
    /// Blink state of the prompt's block caret.
    pub blink: super::chat::Blink,
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
                .placeholder("Ask Synthesis anything... (Ctrl+R for history)")
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
                } => this.submit_prompt(false, window, cx),
                InputEvent::PressEnter {
                    secondary: true,
                    shift: false,
                } => this.submit_prompt(true, window, cx),
                InputEvent::Change => this.on_prompt_changed(cx),
                _ => {}
            },
        ));
        let mut live = Live::new();
        live.show_thinking = cfg.tui.show_thinking;
        live.verbose = cfg.tui.verbose;
        // not `cfg.tui.diffs_expanded`: with it every card started whole (the owner's settings.json has it on); the
        // desktop view always starts clamped to 5 lines and Ctrl+O still expands them all
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
            list: {
                let l = ListState::new(0, ListAlignment::Top, gpui::px(600.));
                l.set_follow_mode(FollowMode::Tail);
                l
            },
            rows: vec![],
            list_reset: true,
            follow: true,
            ws_scroll: ScrollHandle::new(),
            run_scroll: ScrollHandle::new(),
            flash: None,
            dlg_scroll: ScrollHandle::new(),
            model_override: None,
            dirty: true,
            comp: None,
            hist: None,
            search: None,
            tick_task: None,
            pump_task: None,
            engine_task: None,
            fixture: false,
            store_task: None,
            peer_task: None,
            busy_hint: false,
            blink: Default::default(),
            _subs: subs,
        });
        me
    }

    /// Replaces the live state wholesale (offscreen shots and tests feed a conversation this way).
    pub fn set_live(&mut self, live: Live) {
        if let Some(lu) = &mut self.live {
            lu.live = live;
            lu.dirty = true;
            // the conversation is a fixture: `start` must not launch an engine over it (it has no model)
            lu.fixture = true;
        }
    }

    /// Puts the keyboard focus back in the prompt (the shell shows this view again).
    pub fn focus_prompt(&self, window: &mut Window, cx: &mut gpui::App) {
        if let Some(lu) = &self.live {
            lu.prompt.update(cx, |s, cx| s.focus(window, cx));
        }
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
    /// `target`: where it goes; with `None` the `--resume`/`--continue` flags apply once.
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
        notes: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(lu) = &mut self.live else { return };
        if lu.fixture {
            return;
        }
        // the old conversation stays on screen until the new engine replaces it: no blank frame
        lu.live.starting = true;
        lu.live.engine = None;
        lu.dirty = true;
        let (cfg, model) = (lu.cfg.clone(), lu.model_override.clone());
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(build(&cfg, project, prior, target, model, notes));
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

    fn engine_ready(&mut self, r: Result<Launched, String>, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        lu.live.starting = false;
        lu.dirty = true;
        match r {
            Err(e) => {
                lu.live.note(startup_failure(&e), true);
                if e.starts_with("no model configured") {
                    lu.live.open_setup(&e, now_ms());
                }
            }
            Ok(l) => {
                lu.model_override = l.model;
                lu.live.attach(Arc::new(l.engine), l.replay);
                lu.list_reset = true;
                lu.stick();
                let rx = l.rx;
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
        // the model reads a `!cmd`'s output before anything queued starts
        let Some(lu) = &mut self.live else { return };
        if let Some(e) = lu.live.engine.clone()
            && e.take_followup()
        {
            lu.live.busy = true;
            if let Err(why) = e.send_followup() {
                lu.live.busy = false;
                lu.live.note(why, true);
            }
            return;
        }
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
        lu.stick();
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        let started = match text.strip_prefix('!') {
            // `!cmd` / `!!cmd` (record only), as in the TUI
            Some(rest) => {
                let (record_only, cmd) = match rest.strip_prefix('!') {
                    Some(c) => (true, c),
                    None => (false, rest),
                };
                e.user_command(cmd.trim().to_string(), record_only)
            }
            None => e.send_images(text, lu.live.take_images()),
        };
        if let Err(why) = started {
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
        let job = cx.background_spawn(async move { read_store(&cfg, root.as_deref()) });
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
            if n.is_multiple_of(5) {
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
            // nothing of the window is left to show it in
            if let Some(m) = engine::close_shared(e) {
                eprintln!("synthesis-desktop: {m}");
            }
        }
    }

    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.shutdown_engine();
        cx.quit();
    }

    // ---- the prompt ----

    pub(super) fn submit_prompt(&mut self, now: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let mut text = text_of(&lu.prompt, cx);
        // Enter on a typed prefix (`/mo`) completes the highlighted command first
        if let Some(run) = lu
            .comp
            .as_ref()
            .and_then(|c| enter_completes(&text, c, &lu.live.commands))
        {
            self.accept_completion(window, cx);
            if !run {
                return;
            }
            let Some(lu) = &mut self.live else { return };
            text = text_of(&lu.prompt, cx);
        }
        let Some(lu) = &mut self.live else { return };
        lu.comp = None;
        lu.hist = None;
        lu.search = None;
        match lu.live.submit_as(&text, now) {
            Submitted::Held => {
                lu.dirty = true;
                return cx.notify();
            }
            Submitted::Inject { to, text } => {
                self.set_prompt("", window, cx);
                return self.inject(to, text, cx);
            }
            Submitted::Quit => return self.quit(cx),
            Submitted::Send(t) => {
                self.set_prompt("", window, cx);
                self.send_text(t, cx);
                return;
            }
            Submitted::Do(a) => {
                self.set_prompt("", window, cx);
                return self.run_action(a, cx);
            }
            Submitted::Nothing => {}
        }
        if !text.trim().is_empty() {
            self.set_prompt("", window, cx);
        }
        let Some(lu) = &mut self.live else { return };
        lu.stick();
        lu.dirty = true;
        cx.notify();
    }

    /// Send now: the harness takes the line at the next safe point (`UiMsg::SendFailed` if not).
    pub(super) fn inject(&mut self, to: String, text: String, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        // refused before it was sent (engine closed): the line goes back to the queue, never lost
        if let Some(Err(why)) = lu
            .live
            .engine
            .clone()
            .map(|e| e.send_now(to.clone(), text.clone()))
        {
            let why = why.to_string();
            lu.live
                .on_msg(UiMsg::SendFailed { to, text, why }, now_ms());
        }
        lu.stick();
        lu.dirty = true;
        cx.notify();
    }

    /// The queued row's "Send now" and remove controls.
    pub(super) fn queue_row(&mut self, i: usize, send_now: bool, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        if !send_now {
            lu.live.queue.remove(i);
            lu.dirty = true;
            return cx.notify();
        }
        match lu.live.send_queued_now(i) {
            Submitted::Inject { to, text } => self.inject(to, text, cx),
            _ => {
                lu.dirty = true;
                cx.notify();
            }
        }
    }

    /// The prompt's hint follows the task: while one runs, Enter queues and Cmd/Ctrl+Enter sends now.
    pub(super) fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let busy = lu.live.busy;
        if busy == lu.busy_hint {
            return;
        }
        lu.busy_hint = busy;
        let mod_key = if cfg!(target_os = "macos") {
            "\u{2318}"
        } else {
            "Ctrl+"
        };
        let hint = if busy {
            format!("Enter queue \u{b7} {mod_key}\u{21b5} send now \u{b7} Ctrl+T target")
        } else {
            "Ask Synthesis anything... (Ctrl+R for history)".into()
        };
        lu.prompt
            .update(cx, |s, cx| s.set_placeholder(hint, window, cx));
    }

    pub(super) fn set_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn on_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.active {
            return false;
        }
        let Some(lu) = &mut self.live else {
            return false;
        };
        let now = now_ms();
        let key = ks.key.as_str();
        let (ctrl, shift) = (ks.modifiers.control, ks.modifiers.shift);
        let cmd = ks.modifiers.platform || ks.modifiers.alt;
        let digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit() && key != "0";
        // a dialog owns the keyboard, but only while the TUI view is the one on screen: under Settings the keys
        // are Settings', and must never answer the hidden approval
        if !lu.live.dialogs.is_empty() && !self.shown {
            return false;
        }
        if !lu.live.dialogs.is_empty() {
            let mut out = None;
            if lu.live.typing_other() {
                // the free-text "Other" answer of a question
                match key {
                    "enter" if enter_answers(ctrl, cmd, true) => lu.live.commit_other(),
                    "escape" => {
                        lu.live.reject();
                    }
                    "backspace" => lu.live.backspace_other(),
                    "v" if ctrl || ks.modifiers.platform => {
                        let t = cx.read_from_clipboard().and_then(|c| c.text());
                        let t = t.unwrap_or_default();
                        lu.live.type_other(t.lines().next().unwrap_or(""));
                    }
                    _ if !ctrl && !cmd && ks.key_char.is_some() => {
                        lu.live.type_other(ks.key_char.as_deref().unwrap_or(""));
                    }
                    _ => {}
                }
                lu.dirty = true;
                cx.notify();
                return true;
            }
            // the model picker filters as you type (model ids hold digits, so they are text there)
            if !ctrl && !cmd && (key == "backspace" || ks.key_char.is_some()) {
                let typed = ks.key_char.as_deref().filter(|_| key != "backspace");
                if lu.live.model_filter_key(typed, key == "backspace") {
                    lu.dirty = true;
                    cx.notify();
                    return true;
                }
            }
            match key {
                "up" => lu.live.move_sel(false),
                "down" => lu.live.move_sel(true),
                "tab" => lu.live.move_sel(!shift),
                // Enter and digits answer only a dialog the person has engaged (arrows, Tab, a click); before
                // that they are swallowed (they were meant for the prompt) and the footer says how to choose
                "enter" if enter_answers(ctrl, cmd, lu.live.dialogs[0].answerable()) => {
                    let sel = lu.live.dialogs[0].sel;
                    out = lu.live.pick(sel, now);
                }
                "escape" => out = lu.live.reject(),
                "c" if ctrl => out = lu.live.reject(),
                _ if digit && !ctrl && !cmd && lu.live.dialogs[0].answerable() => {
                    out = lu.live.pick(usize::from(key.as_bytes()[0] - b'1'), now);
                }
                _ => {}
            }
            let action = out.and_then(|k| lu.live.answer(&k, now));
            if let Some(ix) = lu.live.dialogs.first().and_then(|d| d.scroll_child()) {
                lu.dlg_scroll.scroll_to_item(ix);
            }
            lu.dirty = true;
            cx.notify();
            if let Some(a) = action {
                self.run_action(a, cx);
            }
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
            // Ctrl+1..5: the centre tabs (chat, diff, terminal, browser, file explorer)
            (k @ ("1" | "2" | "3" | "4" | "5"), true) if !ks.modifiers.shift => {
                self.ts.tab = super::ChatTab::ALL[usize::from(k.as_bytes()[0] - b'1')];
                cx.notify();
                return true;
            }
            ("n", true) => {
                self.pick_folder(cx);
                return true;
            }
            ("t", true) => {
                lu.live.cycle_target();
                lu.dirty = true;
                cx.notify();
                return true;
            }
            ("v", _) if ctrl || ks.modifiers.platform || ks.modifiers.alt => {
                // an image on the clipboard first (Alt+V: only an image); text pastes as usual
                return self.paste_image(ks.modifiers.alt, cx);
            }
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
            ("pageup", _) | ("pagedown", _) if self.ts.tab != super::ChatTab::Chat => {
                // the pane under another tab scrolls by a page
                let h = f32::from(self.pane_scroll.bounds().size.height);
                let step = (h - 40.).max(40.);
                let max = f32::from(self.pane_scroll.max_offset().y);
                let cur = -f32::from(self.pane_scroll.offset().y);
                let to = (cur + if key == "pageup" { -step } else { step }).clamp(0., max);
                self.pane_scroll
                    .set_offset(gpui::point(self.pane_scroll.offset().x, gpui::px(-to)));
                cx.notify();
                return true;
            }
            ("pageup", _) | ("pagedown", _) => {
                let h = lu.list.viewport_bounds().size.height;
                let step = (h - gpui::px(40.)).max(gpui::px(40.));
                lu.list
                    .scroll_by(if key == "pageup" { -step } else { step });
                if lu.list.is_scrolled_to_end() != Some(false) {
                    lu.stick();
                }
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
        // the file explorer is walked with Up/Down while the prompt is empty
        if self.ts.tab == super::ChatTab::Files && text.is_empty() && matches!(key, "up" | "down") {
            return self.step_tree(key == "down", cx);
        }
        match key {
            "escape" => {
                if lu.live.interrupt() {
                    lu.dirty = true;
                    cx.notify();
                    return true;
                }
                // out of another tab, back to the chat
                if self.ts.tab != super::ChatTab::Chat {
                    self.ts.tab = super::ChatTab::Chat;
                    cx.notify();
                    return true;
                }
                false
            }
            // Tab on `/open <path>` completes folder names
            "tab" if text.starts_with("/open ") && cursor >= text.len() => {
                let home = synthesis_app::homes(&lu.cfg.env).ok().map(|h| h.user);
                let arg = &text["/open ".len()..];
                if let Some(new) =
                    crate::folder::complete(arg, home.as_deref(), Path::new(&lu.live.cwd))
                {
                    let t = format!("/open {new}");
                    let end = t.len();
                    lu.prompt.update(cx, |s, cx| {
                        s.set_value(t, window, cx);
                        s.set_selected_range(end..end, cx);
                    });
                    self.on_prompt_changed(cx);
                }
                true
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
        if lu.live.focus == Focus::Workspace {
            panels::reveal_row(&lu.ws_scroll, i, panels::WS_ROW_H);
        } else {
            panels::reveal_row(&lu.run_scroll, i, panels::RUN_ROW_H);
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

    /// A click on entry `i` of Recent Activity: the chat scrolls to its row (a collapsed tool card opens) and the
    /// row flashes. An entry whose row is gone does nothing.
    pub fn click_activity(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(a) = self.snap.activity.get(i).filter(|a| a.shown) else {
            return;
        };
        let anchor = a.anchor;
        let Some(lu) = &mut self.live else { return };
        let Some(pos) = lu.rows.iter().position(|r| *r == Row::Item(anchor)) else {
            return;
        };
        if let Some(synthesis_view::convo::Item::Tool { id, .. }) =
            lu.live.view.convo.items.get(anchor)
        {
            lu.live.expanded.insert(id.clone());
        }
        self.ts.tab = super::ChatTab::Chat;
        lu.follow = false;
        lu.list.set_follow_mode(FollowMode::Normal);
        lu.list.remeasure_items(pos..pos + 1);
        lu.list.scroll_to(gpui::ListOffset {
            item_ix: pos,
            offset_in_item: gpui::px(0.),
        });
        lu.flash = Some((anchor, crate::live::now_ms() + FLASH_MS));
        lu.dirty = true;
        cx.notify();
    }

    /// Click on a Running row: open that session (the open one: nothing happens); another
    /// process's session starts a `/send` to it in the prompt.
    pub fn click_running(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self
            .snap
            .running
            .get(i)
            .filter(|r| r.peer && !r.id.is_empty())
        {
            let text = format!("/send {} ", row.id);
            if let Some(lu) = &mut self.live {
                lu.live.focus = Focus::Chat;
                // a draft being typed is never overwritten
                if !text_of(&lu.prompt, cx).trim().is_empty() {
                    lu.live.note(
                        "finish or clear the message first, then pick the session again",
                        false,
                    );
                    lu.dirty = true;
                    cx.notify();
                    return;
                }
            }
            self.set_prompt(&text, window, cx);
            if let Some(lu) = &self.live {
                lu.prompt.update(cx, |s, cx| s.focus(window, cx));
            }
            return;
        }
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
        self.switch(Target::Resume(id.to_string()), id.to_string(), cx);
    }

    /// Every relaunch (resume, `/new`, `/close`, `/model`, Ctrl+N, the Running panel) goes through
    /// here: the engine restarts without the window going blank. `key` names the request, so the
    /// repeat within [`SWITCH_ARM_MS`] that cancels a running task is for the same one.
    fn switch(&mut self, target: Target, key: String, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let now = now_ms();
        if lu.live.busy {
            let armed = lu
                .live
                .switch_armed
                .as_ref()
                .is_some_and(|(a, t)| *a == key && now < *t);
            if !armed {
                lu.live.switch_armed = Some((key, now + SWITCH_ARM_MS));
                lu.live.note(
                    "a task is running: repeat it within 5 s to cancel the task and switch",
                    false,
                );
                lu.dirty = true;
                cx.notify();
                return;
            }
            if let Some(e) = &lu.live.engine {
                e.cancel();
            }
            lu.live.note("task cancelled: switching", false);
        }
        lu.live.switch_armed = None;
        let project = match &target {
            Target::New(p) => p.clone(),
            Target::Close => lu
                .live
                .engine
                .as_ref()
                .map_or_else(|| lu.cfg.project.clone(), |e| PathBuf::from(&e.cwd)),
            _ => lu.cfg.project.clone(),
        };
        let prior = lu.live.engine.take();
        lu.live.queue.clear();
        lu.live.dialogs.clear();
        lu.live.busy = false;
        self.launch_with(project, prior, Some(target), vec![], cx);
    }

    /// Runs `f` on the live state, then redraws and scrolls to the new row.
    fn live_do(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Live)) {
        if let Some(lu) = &mut self.live {
            f(&mut lu.live);
            lu.stick();
            lu.dirty = true;
        }
        cx.notify();
    }

    /// What a command or picker asked the window to do.
    pub(super) fn run_action(&mut self, a: Action, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let busy = lu.live.busy;
        match a {
            Action::Clear => {
                if let Some(e) = &lu.live.engine {
                    e.clear_history();
                }
                lu.live.clear_conversation();
                lu.list_reset = true;
                lu.stick();
            }
            Action::Compact(i) => {
                let r = lu.live.engine.as_ref().map(|e| e.compact(i));
                match r {
                    Some(Ok(true)) => lu.live.note(
                        "compaction requested: it runs before the next model request",
                        false,
                    ),
                    Some(Ok(false)) => {
                        lu.live.compacting = true;
                        lu.live.note("compacting the conversation...", false)
                    }
                    Some(Err(e)) => lu.live.note(format!("/compact: {e}"), true),
                    None => {}
                }
            }
            Action::Model(_) if busy => lu.live.note(
                "/model: a task is running \u{b7} esc cancels it first",
                true,
            ),
            Action::Model(m) => return self.switch(Target::Model(m), "model".into(), cx),
            Action::Resume(id) => return self.open_session(&id, cx),
            Action::RefreshModels => {
                let (env, project) = (lu.cfg.env.clone(), lu.cfg.project.clone());
                let job =
                    cx.background_spawn(async move { crate::models::refresh(&env, &project) });
                cx.spawn(async move |this, cx| {
                    let (rows, errors) = job.await;
                    this.update(cx, |t, cx| {
                        t.live_do(cx, |l| l.models_refreshed(rows, errors))
                    })
                    .ok();
                })
                .detach();
            }
            Action::Open(typed) => {
                let l = &lu.live;
                let home = synthesis_app::homes(&lu.cfg.env).ok().map(|h| h.user);
                let cwd = PathBuf::from(&l.cwd);
                let open: Vec<PathBuf> = l
                    .view
                    .workspaces
                    .workspaces
                    .iter()
                    .map(|w| PathBuf::from(&w.root))
                    .collect();
                // the disk is touched off the UI thread, with a time limit
                let job = cx.background_spawn(async move {
                    crate::folder::resolve_bounded(
                        typed,
                        home,
                        cwd,
                        open,
                        std::time::Duration::from_secs(5),
                    )
                });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| match r {
                        Ok(p) => t.open_folder(&p, cx),
                        Err(e) => t.live_do(cx, |l| l.note(e, true)),
                    })
                    .ok();
                })
                .detach();
                return;
            }
            Action::New => {
                let l = &lu.live;
                let root = l
                    .view
                    .workspaces
                    .workspaces
                    .iter()
                    .find(|w| w.key == l.ws_key)
                    .map(|w| (w.root.clone(), w.available))
                    .or_else(|| l.engine.as_ref().map(|e| (e.root.clone(), true)));
                match root {
                    Some((r, true)) => {
                        return self.switch(Target::New(PathBuf::from(r)), "new".into(), cx);
                    }
                    Some((r, false)) => lu
                        .live
                        .note(format!("/new: the folder {r} no longer exists"), true),
                    None => lu.live.note("/new: the session is still starting", true),
                }
            }
            Action::Close => return self.switch(Target::Close, "close".into(), cx),
            Action::OpenSettings => cx.emit(OpenSettings),
            Action::Quit => return self.quit(cx),
            Action::Recheck => {
                let project = lu.cfg.project.clone();
                return self.launch(project, None, vec![], cx);
            }
            Action::ShowFiles => return self.show_tab(super::ChatTab::Files, None, cx),
            Action::ShowDiff(f) => return self.show_tab(super::ChatTab::Diff, f, cx),
            Action::Send { to, text } => {
                let Some(e) = lu.live.engine.clone() else {
                    return;
                };
                let job = cx.background_spawn(async move { e.peer_ask(&to, &text) });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| t.live_do(cx, |l| l.send_done(r)))
                        .ok();
                })
                .detach();
            }
            Action::Peers => {
                let Some(e) = lu.live.engine.clone() else {
                    return;
                };
                let job = cx.background_spawn(async move { e.peer_rows() });
                cx.spawn(async move |this, cx| {
                    let rows = job.await;
                    this.update(cx, |t, cx| t.live_do(cx, |l| l.peers_done(rows)))
                        .ok();
                })
                .detach();
            }
        }
        lu.dirty = true;
        cx.notify();
    }

    /// "new workspace..." of the Workspace panel.
    pub fn new_workspace(&mut self, cx: &mut Context<Self>) {
        self.pick_folder(cx);
    }

    /// "new session..." of the Running panel: a fresh session in the selected workspace.
    pub fn new_session(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_action(Action::New, cx);
    }

    /// Ctrl+N: the system folder picker; the folder gets a fresh session.
    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open a folder as a new workspace".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                this.update(cx, |t, cx| t.open_folder(&p, cx)).ok();
            }
        })
        .detach();
    }

    /// A folder chosen as a new workspace: it must be a readable directory.
    pub fn open_folder(&mut self, p: &Path, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        // the system picker can return a network path too: not probed here either
        if crate::folder::is_unc(&p.to_string_lossy()) {
            lu.live.note(crate::folder::UNC_REFUSED, true);
            lu.dirty = true;
            cx.notify();
            return;
        }
        match synthesis_core::canonicalize(p) {
            Ok(c) if c.is_dir() => {
                let key = format!("folder:{}", c.display());
                self.switch(Target::New(c), key, cx)
            }
            _ => {
                lu.live
                    .note(format!("no such folder: {}", p.display()), true);
                lu.dirty = true;
                cx.notify();
            }
        }
    }

    // ---- images ----

    /// Cmd/Ctrl+V and Alt+V: an image on the clipboard is attached. Returns true when it was
    /// (Alt+V: always, with a note when there is none); Cmd/Ctrl+V falls back to the text paste.
    fn paste_image(&mut self, image_only: bool, cx: &mut Context<Self>) -> bool {
        let Some(lu) = &mut self.live else {
            return false;
        };
        let img = cx.read_from_clipboard().and_then(|c| {
            c.into_entries().find_map(|e| match e {
                gpui::ClipboardEntry::Image(i) => Some(i),
                _ => None,
            })
        });
        let Some(img) = img else {
            if image_only {
                lu.live.note(
                    "the clipboard holds no image \u{b7} Cmd/Ctrl+V pastes text",
                    true,
                );
                lu.dirty = true;
                cx.notify();
            }
            return image_only;
        };
        let r = match img.format {
            gpui::ImageFormat::Png => attach_png(&mut lu.live, img.bytes),
            gpui::ImageFormat::Jpeg => attach_jpeg(&mut lu.live, img.bytes),
            f => Err(format!("a {f:?} image is not supported yet (PNG or JPEG)")),
        };
        lu.report_image(r);
        cx.notify();
        true
    }

    /// Files dropped on the window: images are attached, anything else is said.
    pub fn drop_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        for p in paths {
            // the TUI's `@file` rule: inside the workspace, no secret file, links resolved
            let r = e
                .image_path(p)
                .and_then(|p| read_image_file(&p))
                .and_then(|(png, bytes)| {
                    if png {
                        attach_png(&mut lu.live, bytes)
                    } else {
                        attach_jpeg(&mut lu.live, bytes)
                    }
                });
            lu.report_image(r.map_err(|e| format!("{}: {e}", p.display())));
        }
        cx.notify();
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
    /// Follow the newest row again (a sent message, a new session, "jump to latest").
    pub fn stick(&mut self) {
        self.follow = true;
        self.list.set_follow_mode(FollowMode::Tail);
    }

    /// Rows of the chat now: what the quiet chat shows, then the plan and the working line.
    fn rows_now(&self) -> Vec<Row> {
        let l = &self.live;
        let c = &l.view.convo;
        let mut rows: Vec<Row> = c
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| super::chat::visible(it, l.show_thinking, l.verbose, l.focus_view))
            .map(|(i, _)| Row::Item(i))
            .collect();
        if !c.plan.is_empty() {
            rows.push(Row::Plan);
        }
        // every busy state has its line: thinking, a tool, a sub-agent, verifying, compacting, an approval
        if super::activity::activity_of(l).is_some() {
            rows.push(Row::Activity);
        }
        rows
    }

    /// Tells the list what changed: rows added or dropped at the end, and the last rows (a streaming
    /// answer, a tool that just finished) measured again. Shortcut: a row older than
    /// `TAIL_REMEASURE` rows that changes (a slow parallel tool) keeps its old height until the
    /// next `remeasure`.
    pub fn sync_rows(&mut self) {
        let rows = self.rows_now();
        let (old, new) = (self.list.item_count(), rows.len());
        if std::mem::take(&mut self.list_reset) {
            self.list.reset(new);
            self.list.set_follow_mode(FollowMode::Tail);
        } else if new > old {
            self.list.splice(old..old, new - old);
        } else if new < old {
            self.list.splice(new..old, 0);
        }
        self.list
            .remeasure_items(new.saturating_sub(TAIL_REMEASURE)..new);
        self.rows = rows;
        self.follow = self.list.is_following_tail();
    }

    /// The outcome of an attach, as a line in the chat.
    fn report_image(&mut self, r: Result<String, String>) {
        match r {
            Ok(m) => self.live.note(m, false),
            Err(e) => self.live.note(format!("image: {e}"), true),
        }
        self.dirty = true;
    }

    fn add_sub(&mut self, s: Subscription) {
        self._subs.push(s);
    }
}

/// The TUI's checks for a pasted PNG (magic, 4 MB, 8000x8000) and the per-message limits.
fn attach_png(l: &mut Live, bytes: Vec<u8>) -> Result<String, String> {
    let img = synthesis_tui_base::imgclip::validate_png(bytes).map_err(|e| e.to_string())?;
    l.attach_image("image/png", &img.png, Some((img.width, img.height)))
}

/// JPEG: magic bytes and the same 4 MB cap (no decoder in the tree, so no size check in pixels).
fn attach_jpeg(l: &mut Live, bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() > synthesis_tui_base::imgclip::MAX_PNG {
        return Err("the image is larger than 4 MB".into());
    }
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("not a JPEG".into());
    }
    l.attach_image("image/jpeg", &bytes, None)
}

/// A dropped file: (is PNG, bytes). Only `.png`, `.jpg` and `.jpeg`, read up to the cap.
pub fn read_image_file(p: &Path) -> Result<(bool, Vec<u8>), String> {
    use std::io::Read as _;
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let png = match ext.as_str() {
        "png" => true,
        "jpg" | "jpeg" => false,
        _ => return Err("not an image (png, jpg)".into()),
    };
    let cap = synthesis_tui_base::imgclip::MAX_PNG as u64;
    // `p` is the resolved path from `Engine::image_path`; a link swapped in since then is refused
    // by the open itself (O_NOFOLLOW / FILE_FLAG_OPEN_REPARSE_POINT), and the opened handle is
    // checked to be a regular file within the cap.
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(&mut o, 0x0020_0000);
    let f = o.open(p).map_err(|e| e.to_string())?;
    let md = f.metadata().map_err(|e| e.to_string())?;
    if !md.is_file() || md.len() > cap {
        return Err("not a regular image file within the 4 MB cap".into());
    }
    let mut v = vec![];
    f.take(cap + 1)
        .read_to_end(&mut v)
        .map_err(|e| e.to_string())?;
    Ok((png, v))
}

/// The worker side of a (re)launch: ends `prior`, then starts the engine on `target`. A model
/// that cannot be built falls back to the session on the model it had.
pub fn build(
    cfg: &LiveCfg,
    project: PathBuf,
    prior: Option<Arc<Engine>>,
    target: Option<Target>,
    model: Option<String>,
    mut notes: Vec<String>,
) -> Result<Launched, String> {
    let first_start = prior.is_none() && target.is_none();
    let prior_sid = prior.as_ref().map(|p| p.sid.clone());
    if let Some(p) = prior {
        // a worktree session's end report (path, branch, changes, cleanup) is a notice of the next chat
        notes.extend(engine::close_shared(p));
    }
    let home = cfg.synthesis_home.clone();
    let mut model = model;
    let prior_model = model.clone();
    let mut project = project;
    let mut resume = None;
    if first_start {
        let keep = cfg.args.common.resume.clone().unwrap_or_default();
        notes.extend(engine::housekeeping(&home, &keep));
    }
    let back = |home: &Path, sid: Option<String>| {
        sid.and_then(|s| synthesis_tui::load_resume(home, &s).ok())
    };
    let want = match &target {
        Some(Target::Resume(id)) => {
            // the session picked continues on its own stored model
            model = None;
            Some(Ok(id.clone()))
        }
        Some(Target::Model(m)) => {
            model = Some(m.clone());
            prior_sid.clone().map(Ok)
        }
        Some(Target::New(p)) => {
            project.clone_from(p);
            None
        }
        Some(Target::Close) => {
            // hidden, not deleted; a fresh session opens in the same folder
            let closed = prior_sid.as_deref().map(|sid| {
                synthesis_session::Store::open(&home, synthesis_session::StoreOptions::default())
                    .and_then(|s| s.close_session(sid))
            });
            if let Some(Err(e)) = closed {
                notes.push(format!("/close: could not close the session ({e})"));
                prior_sid.clone().map(Ok)
            } else {
                None
            }
        }
        None if first_start => match engine::requested_resume(&cfg.args.common, &project, &home) {
            Ok(r) => {
                resume = r;
                None
            }
            Err(e) => Some(Err(e)),
        },
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
                resume = back(&home, prior_sid.clone());
                model = prior_model.clone();
            }
        },
        Some(Err(e)) => return Err(e),
        None => {}
    }
    let attempt = |project: PathBuf,
                   resume: Option<synthesis_tui::Resume>,
                   model: Option<String>,
                   notes: Vec<String>| {
        let project = resume
            .as_ref()
            .map_or(project, |r| PathBuf::from(&r.record.cwd));
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
        let (mut common, note) = engine::common_for(&cfg.args.common, &cfg.granted, &project);
        if model.is_some() {
            common.model.clone_from(&model);
        }
        let mut notes = notes;
        notes.extend(note);
        let had_resume = resume.is_some();
        Engine::start_with(
            StartOpts {
                project,
                common,
                resume,
                notes,
            },
            &cfg.env,
        )
        .map(|(engine, rx)| Launched {
            engine,
            rx,
            replay: had_resume.then_some(replay),
            model,
        })
    };
    match attempt(project.clone(), resume, model, notes.clone()) {
        Err(e) if matches!(target, Some(Target::Model(_))) => {
            notes.push(format!(
                "/model: could not switch ({e}); staying on the previous model"
            ));
            attempt(project, back(&home, prior_sid), prior_model, notes)
        }
        r => r,
    }
}

/// The workspace list (with the trust-gated git scan), the sessions a process holds, and the usage
/// history. Blocking (reads the store, runs git): off the UI thread.
pub fn read_store(
    cfg: &LiveCfg,
    root: Option<&str>,
) -> Result<(WorkspaceList, Vec<synthesis_view::usage::HistoryDay>), String> {
    let r = synthesis_session::Reader::open(&cfg.synthesis_home).map_err(|e| e.to_string())?;
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
    let ws = WorkspaceList::load(&r, root.map(Path::new), &opts)?;
    let usage = synthesis_view::usage::load_from(&r, now_ms(), cfg.tz_min);
    Ok((ws, usage))
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
pub fn startup_failure(e: &str) -> String {
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

/// Plain Enter answers an engaged dialog; Cmd/Ctrl+Enter (send now) never does.
fn enter_answers(ctrl: bool, cmd: bool, answerable: bool) -> bool {
    answerable && !ctrl && !cmd
}

/// Enter with the popup open on a slash prefix: `Some(run)` completes the highlighted command (and
/// runs it when `run`, i.e. it takes no argument). `None`: the text already names a command.
fn enter_completes(
    text: &str,
    c: &Comp,
    cmds: &[synthesis_tui_base::complete::Command],
) -> Option<bool> {
    if c.token.kind != complete::TokenKind::Command {
        return None;
    }
    let typed = text.trim().trim_start_matches('/');
    if cmds
        .iter()
        .any(|x| x.name == typed || x.aliases.iter().any(|a| a == typed))
    {
        return None;
    }
    let name = c.inserts.get(c.sel)?.insert.trim().trim_start_matches('/');
    Some(
        cmds.iter()
            .any(|x| x.name == name && x.argument_hint.is_empty()),
    )
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
    fn cmd_or_ctrl_enter_never_answers_a_dialog() {
        assert!(enter_answers(false, false, true));
        assert!(!enter_answers(true, false, true));
        assert!(!enter_answers(false, true, true));
        assert!(!enter_answers(false, false, false));
    }

    #[test]
    fn enter_on_a_prefix_completes_and_runs_only_without_args() {
        let mut l = Live::new();
        l.commands = synthesis_tui::local_commands();
        let c = suggest("/mo", 3, &l, Path::new("/")).unwrap();
        assert!(enter_completes("/mo", &c, &l.commands).is_some());
        let c = suggest("/model", 6, &l, Path::new("/")).unwrap();
        assert_eq!(enter_completes("/model", &c, &l.commands), None);
        let c = suggest("/hel", 4, &l, Path::new("/")).unwrap();
        assert_eq!(enter_completes("/hel", &c, &l.commands), Some(true));
    }

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
