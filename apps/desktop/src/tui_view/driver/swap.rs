//! Moving a session's `LiveUi` between views (mode switch, "option H"): the session keeps running, only the
//! window that shows it changes. Slices 1 and 2 of the plan; nothing calls these outside tests until the switch
//! is wired (`Shell::go`), hence the `allow(dead_code)` on the entry points.
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

use gpui::{App, AsyncApp, Context, Entity, WeakEntity, Window};

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

    /// Tasks and subscriptions that are still running (tests: nothing leaks or doubles over a move). The store,
    /// branch and peer reads are one-shot and a move starts one on purpose: not counted.
    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let run = |t: &Option<gpui::Task<()>>| t.as_ref().is_some_and(|t| !t.is_ready()) as usize;
        let tasks = run(&self.tick_task)
            + run(&self.pump_task)
            + run(&self.engine_task)
            + self.bg.iter().filter(|t| !t.is_ready()).count();
        (tasks, self._subs.len())
    }
}

/// Where a session is shown.
#[allow(dead_code)] // built by the tests until slice 3 wires the switch
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// The TUI view.
    Tui,
    /// The Super Agent frame with this id.
    Frame(String),
}

impl TuiView {
    /// Takes the session out of this view (the view is left with none): its view-bound tasks and subscriptions
    /// end, the rest travels with the returned `LiveUi`. Put it back with `put_live`, never drop it.
    #[allow(dead_code)] // slice 3 wires the switch; until then tests only
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
    #[allow(dead_code)] // slice 3 wires the switch; until then tests only
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
        lu.dirty = true;
        self.live = Some(lu);
        self.refresh_store(cx);
        cx.notify();
    }

    /// The store id of the session shown here (empty until its engine reported one).
    fn shown_sid(&self) -> Option<&str> {
        self.live
            .as_ref()
            .map(|l| l.live.sid.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// Exchanges the sessions of two views (either may be empty: then it is a move). Neither session is dropped.
#[allow(dead_code)] // slice 3 wires the switch; until then tests only
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
#[allow(dead_code)] // slice 3 wires the switch; until then tests only
pub(crate) fn where_is(sid: &str, places: &[(Place, Entity<TuiView>)], cx: &App) -> Option<Place> {
    places
        .iter()
        .find(|(_, v)| v.read(cx).shown_sid() == Some(sid))
        .map(|(p, _)| p.clone())
}

#[cfg(test)]
mod tests;
