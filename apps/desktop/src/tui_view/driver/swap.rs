//! Moving a session's `LiveUi` between views (mode switch, "option H"): the session keeps running, only the
//! window that shows it changes. `Shell::go` moves sessions with `take_live`/`put_live` (through the Super Agent
//! host's `take`/`adopt`); `swap_live` and `where_is` are the same moves for two views and a lookup, test-only
//! (`cfg(test)`).
//!
//! Invariants: a session is in exactly one place; a swap never drops a `LiveUi` (its engine, session lock,
//! pending approvals and event stream stay as they are); no task is duplicated or left behind.
//!
//! Who owns which task (`LiveUi` field or kind), and what a move does with it:
//!
//! | task                                 | spawned in                | what it touches        | take_live        | adopt_live                |
//! |--------------------------------------|---------------------------|------------------------|------------------|---------------------------|
//! | `pump_task` (engine events)          | `engine_ready`            | the owner view, via `Owner` | moves along | `Owner` re-pointed        |
//! | `engine_task` (engine being built)   | `launch_with`             | the owner view, via `Owner` | moves along (never dropped: `EngineRx` closes the engine on drop) | `Owner` re-pointed |
//! | `bg` (detached work, `spawn_owned`)  | actions, previews, clean, rewind | the owner view, via `Owner` | moves along | `Owner` re-pointed |
//! | `tick_task` (clock, MCP, peers)      | `start_at`                | `self.embed` of its view | dropped        | spawned again             |
//! | `store_task`, `branch_task`, `peer_task` | `refresh_store`, `tick` | re-read results       | dropped          | `refresh_store` again     |
//! | `_subs`: prompt box, key interceptor, quit hook, release hook | `live`, `start_at`, `start_in` | the view | dropped | made again for the new view (release only when embedded) |
//!
//! Not moved (state of the view and its panes, guarded by their own epoch checks): the terminal and browser
//! panes, the Files/Diff/Scm reads, the folder picker, the "copied" flash, `topo_tick`.

use std::cell::RefCell;
use std::rc::Rc;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use gpui::{App, Entity};
use gpui::{AsyncApp, Context, WeakEntity, Window};

use super::{LiveUi, TuiView};

/// The view a session's tasks report to. Shared by the session's tasks and re-pointed by `adopt_live`, so a task
/// started in one view lands in whichever view shows the session now. Foreground only (gpui's single thread).
#[derive(Clone)]
pub(in crate::tui_view) struct Owner(Rc<RefCell<WeakEntity<TuiView>>>);

impl Owner {
    pub(in crate::tui_view) fn new(v: WeakEntity<TuiView>) -> Self {
        Self(Rc::new(RefCell::new(v)))
    }

    fn set(&self, v: WeakEntity<TuiView>) {
        *self.0.borrow_mut() = v;
    }

    /// `WeakEntity::update` on the current owner (`Err` when that view is gone).
    pub(in crate::tui_view) fn update<R>(
        &self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut TuiView, &mut Context<TuiView>) -> R,
    ) -> gpui::Result<R> {
        let w = self.0.borrow().clone();
        w.update(cx, f)
    }
}

impl LiveUi {
    /// Runs `f` for this session: it reports to the session's current view, and it ends (is cancelled) with the
    /// session, not with the view it started in.
    pub(in crate::tui_view) fn spawn_owned(
        &mut self,
        cx: &mut Context<TuiView>,
        f: impl AsyncFnOnce(Owner, &mut AsyncApp) + 'static,
    ) {
        let owner = self.owner.clone();
        self.bg.retain(|t| !t.is_ready());
        self.bg
            .push(cx.spawn(async move |_, cx| f(owner, cx).await));
    }

    /// An engine, or one being built: a session a mode switch must keep alive (a preview or a fixture has none).
    pub(crate) fn carries_session(&self) -> bool {
        (self.live.engine.is_some() || self.live.starting) && !self.fixture
    }

    /// Tasks and subscriptions that are still running (tests: nothing leaks or doubles over a move). The store,
    /// branch and peer reads are one-shot and a move starts one on purpose: not counted.
    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize) {
        let run = |t: &Option<gpui::Task<()>>| t.as_ref().is_some_and(|t| !t.is_ready()) as usize;
        let tasks = run(&self.tick_task)
            + run(&self.pump_task)
            + run(&self.engine_task)
            + self.bg.iter().filter(|t| !t.is_ready()).count();
        (tasks, self._subs.len())
    }
}

/// Where a session is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(test)]
pub enum Place {
    /// The TUI view.
    Tui,
    /// The Super Agent frame with this id.
    Frame(String),
}

impl TuiView {
    /// Takes the session out of this view (the view is left with none): its view-bound tasks and subscriptions
    /// end, the rest travels with the returned `LiveUi`. Put it back with `put_live`, never drop it.
    #[must_use = "dropping a LiveUi closes its engine"]
    pub(crate) fn take_live(&mut self, cx: &mut Context<Self>) -> Option<LiveUi> {
        let mut lu = self.live.take()?;
        lu._subs.clear();
        (lu.tick_task, lu.store_task, lu.branch_task, lu.peer_task) = (None, None, None, None);
        cx.notify();
        Some(lu)
    }

    /// Shows `lu` in this view. A session already here is taken out first and returned (the caller places it:
    /// nothing is ever dropped by a put).
    #[must_use = "the displaced session must be put somewhere"]
    pub(crate) fn put_live(
        &mut self,
        lu: LiveUi,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<LiveUi> {
        let old = self.take_live(cx);
        self.adopt_live(lu, window, cx);
        old
    }

    /// Binds a session to this (empty) view: its tasks report here, its subscriptions and clock are made again.
    fn adopt_live(&mut self, mut lu: LiveUi, window: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.live.is_none());
        lu.owner.set(cx.weak_entity());
        lu._subs.push(Self::prompt_sub(&lu.prompt, window, cx));
        if lu.started {
            lu._subs.extend(Self::key_and_quit_subs(cx));
            if self.embed.is_some() {
                lu._subs
                    .push(cx.on_release(|this, cx| this.close_embedded(cx)));
            }
            lu.tick_task = Some(Self::spawn_tick(cx));
        }
        // an embedded frame is one session in its folder: the folder follows the session it holds now
        if let Some(em) = &mut self.embed
            && !lu.live.cwd.is_empty()
        {
            em.folder = Some(std::path::PathBuf::from(&lu.live.cwd));
        }
        lu.dirty = true;
        self.live = Some(lu);
        self.refresh_store(cx);
        cx.notify();
    }

    /// A session worth keeping alive across a mode switch: it has an engine, or one is being built (a view that
    /// shows only a preview or the sample has nothing to carry).
    pub(crate) fn has_session(&self) -> bool {
        self.live.as_ref().is_some_and(LiveUi::carries_session)
    }

    /// `stamp` of the last task the person (or the orchestrator) gave this session, or it answered (0: never).
    pub(crate) fn activity(&self) -> u64 {
        self.live.as_ref().map_or(0, |l| l.activity.get())
    }

    /// An embedded frame view that holds no session yet: it takes the one a mode switch brings (`put_live`),
    /// in `folder`, and starts nothing.
    pub(crate) fn embed_empty(&mut self, folder: std::path::PathBuf) {
        self.embed = Some(super::super::Embed {
            headless: true,
            folder: Some(folder),
            ..Default::default()
        });
        // keys only after the owner says this frame is on screen
        (self.active, self.shown) = (false, false);
    }

    /// Mode switch to the Super Agent: this view's session is handed over (`None`: it has none to carry) and the
    /// view keeps an empty one, so the TUI draws as before its first start.
    pub(crate) fn hand_over_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<LiveUi> {
        if !self.has_session() {
            return None;
        }
        let mut blank = Self::fresh_live_ui(window, cx, self.live_cfg()?);
        blank.live.starting = false;
        let lu = self.take_live(cx)?;
        let displaced = self.put_live(blank, window, cx);
        debug_assert!(displaced.is_none());
        Some(lu)
    }

    /// The hidden sessions of the Super Agent, for the Running panel.
    pub(crate) fn set_hosted(&mut self, rows: Vec<crate::live::HostedRow>, cx: &mut Context<Self>) {
        if let Some(lu) = &mut self.live
            && lu.live.hosted != rows
        {
            lu.live.hosted = rows;
            lu.dirty = true;
            cx.notify();
        }
    }

    /// `(session id, engine address, (running tasks, subscriptions))` of the session shown here (tests).
    #[cfg(test)]
    pub(crate) fn session_probe(&self) -> Option<(String, usize, (usize, usize))> {
        let lu = self.live.as_ref()?;
        let e = lu.live.engine.as_ref()?;
        Some((lu.live.sid.clone(), Arc::as_ptr(e) as usize, lu.counts()))
    }

    /// The hidden sessions this view lists (tests).
    #[cfg(test)]
    pub(crate) fn live_hosted(&self) -> Vec<crate::live::HostedRow> {
        self.live
            .as_ref()
            .map(|l| l.live.hosted.clone())
            .unwrap_or_default()
    }

    /// The person answered every question of this session (tests).
    #[cfg(test)]
    pub(crate) fn test_clear_questions(&mut self, cx: &mut Context<Self>) {
        if let Some(lu) = &mut self.live {
            lu.live.dialogs.clear();
        }
        cx.notify();
    }

    /// The store id of the session shown here (empty until its engine reported one).
    #[cfg(test)]
    fn shown_sid(&self) -> Option<&str> {
        self.live
            .as_ref()
            .map(|l| l.live.sid.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// Ends a session nobody can take (a hand-over that found no host): closed through the usual off-thread path,
/// never dropped with its engine open.
pub(crate) fn close_live(mut lu: LiveUi) {
    if let Some(e) = lu.live.engine.take() {
        drop(super::close_off_thread(e));
    }
}

/// Exchanges the sessions of two views (either may be empty: then it is a move). Neither session is dropped.
#[cfg(test)]
pub(crate) fn swap_live(
    a: &Entity<TuiView>,
    b: &Entity<TuiView>,
    window: &mut Window,
    cx: &mut App,
) {
    if a == b {
        return;
    }
    let la = a.update(cx, |t, cx| t.take_live(cx));
    let lb = b.update(cx, |t, cx| t.take_live(cx));
    for (view, lu) in [(a, lb), (b, la)] {
        if let Some(lu) = lu {
            let left = view.update(cx, |t, cx| t.put_live(lu, window, cx));
            debug_assert!(left.is_none(), "both views were just emptied");
        }
    }
}

/// Where the session `sid` is shown, among the TUI view and the frame views (`None`: nowhere).
#[cfg(test)]
pub(crate) fn where_is(sid: &str, places: &[(Place, Entity<TuiView>)], cx: &App) -> Option<Place> {
    places
        .iter()
        .find(|(_, v)| v.read(cx).shown_sid() == Some(sid))
        .map(|(p, _)| p.clone())
}

#[cfg(test)]
mod tests;
