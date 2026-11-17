//! The Outcomes tab (owner decision 2026-11-17; `docs/specs/desktop.md` "Outcomes tab", ADR-021): outcomes the
//! orchestrator runs, three columns (list, Goal | Decisions | History, inspector). It is a surface in the
//! sense of ADR-021: it talks to the orchestrator only through `OutcomeClient` on its own worker thread (`link`),
//! renders only `synthesis_view::outcomes` models, and depends on no orchestrator crate. This is not the
//! Conductor (`docs/specs/conductor.md`). It also creates an outcome and revises a contract (`form`,
//! `form_view`); a created or revised contract is approved only by a separate, explicit act after the
//! server's own diff.
//!
//! Everything is on the keyboard (Tab between the columns, arrows, numbers, Enter, Esc); the mouse does the
//! same things and nothing is mouse-only.

use futures::StreamExt as _;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, ScrollHandle, Task, Window, point, px,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};
use synthesis_config::Env;

use crate::live::now_ms;
use form::{FormText, Stage};
use link::{Link, Msg, Req};
use model::{Effect, Key, Pane, Screen, State};

pub mod ask;
pub mod form;
mod form_view;
mod guide_view;
pub mod link;
pub mod model;
pub(crate) mod render;
pub mod sample;
#[cfg(feature = "shot")]
pub mod shot;

/// The notice while an answer of the TUI view's popup is on its way.
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
    /// The inputs of the open form (made when it opens, dropped when it closes; the text lives in them).
    form_ui: Option<FormUi>,
    form_scroll: ScrollHandle,
    /// The form was on its review stage at the last frame (the keys go back to the window when it starts).
    was_reviewing: bool,
    /// The review has been drawn at least once: before that nothing can approve it (a held Enter that sent
    /// the form must not land on the button in the frame the review appears).
    review_armed: bool,
}

/// The text inputs and the two buttons' focus of the form. Lists are one entry per line.
struct FormUi {
    goal: Entity<TextareaState>,
    criteria: Entity<TextareaState>,
    checks: Entity<TextareaState>,
    scope: Entity<TextareaState>,
    budget: Entity<InputState>,
    workspace: Entity<InputState>,
    confirm: FocusHandle,
    cancel: FocusHandle,
}

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
        Self {
            state,
            focus: cx.focus_handle(),
            link: None,
            env: None,
            started: false,
            pump: None,
            scroll: Default::default(),
            form_ui: None,
            form_scroll: ScrollHandle::new(),
            was_reviewing: false,
            review_armed: false,
        }
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

    /// The TUI view's Approvals and tab need the list without this tab being opened: it connects now. Only called
    /// after `approvals::gate` said outcomes are on and supported.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if !self.started {
            self.start(cx);
        }
    }

    /// Selects outcome `id` (what the TUI view's rows open); the tab shows it from then on.
    pub fn select_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.state.items.iter().position(|i| i.row.id == id) {
            let e = self.state.select(i);
            self.run(e, cx);
        }
    }

    /// Sends an effect the TUI view's decision popup made (the same worker path as this tab's own answer).
    pub fn send_effect(&mut self, e: Effect, cx: &mut Context<Self>) {
        // the reply (`Msg::Answered`) replaces this line: that is how the popup sees the answer came back
        // in flight for the tab too: its own Enter sends nothing until the reply (`State::answered`) comes back
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
                Msg::Screen(s) => self.state.set_screen(s),
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
                Msg::Enabled(Ok(())) => {
                    self.state.guide_done();
                    self.state.set_screen(Screen::Loading);
                }
                Msg::Enabled(Err(e)) => self.state.guide_failed(e),
            }
        }
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
                (Effect::Migrate, None) => self
                    .state
                    .guide_failed("Sample data: nothing was sent.".into()),
                (Effect::Migrate, Some(l)) => {
                    if !l.try_send(Req::Enable) {
                        self.state.guide_failed(
                            "The link to the orchestrator has ended. Close this window and press r."
                                .into(),
                        );
                    }
                }
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
        cx.notify();
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
        if self.state.form.is_some() {
            return self.form_key(ev, window, cx);
        }
        let k = &ev.keystroke;
        let m = &k.modifiers;
        // a held key (auto-repeat) never acts on the disclosure: one deliberate press does
        if ev.is_held && self.state.guide.is_some() {
            return true;
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

/// Scrolls a pane by `n` lines (negative: up).
fn scroll(h: &ScrollHandle, n: i32) {
    let max = f32::from(h.max_offset().y);
    let y = (f32::from(h.offset().y) - n as f32 * 19.).clamp(-max, 0.);
    h.set_offset(point(px(0.), px(y)));
}

// ------------------------------------------------------------------------------------------ the form

impl FormUi {
    fn new(init: &FormText, window: &mut Window, cx: &mut Context<OutcomesView>) -> Self {
        let area = |ph: &'static str,
                    rows: usize,
                    v: &str,
                    window: &mut Window,
                    cx: &mut Context<OutcomesView>| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(ph)
                    .auto_grow(rows, rows)
                    .default_value(v.to_string())
            })
        };
        let line =
            |ph: &'static str, v: &str, window: &mut Window, cx: &mut Context<OutcomesView>| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(ph)
                        .default_value(v.to_string())
                })
            };
        let workspace = line(
            "/absolute/path/of/the/repository",
            &init.workspace,
            window,
            cx,
        );
        // leaving the folder field reads the base commit again if the folder changed
        cx.subscribe_in(&workspace, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Blur) {
                this.workspace_left(cx);
            }
        })
        .detach();
        Self {
            goal: area(
                "What must be true when this is done?",
                3,
                &init.goal,
                window,
                cx,
            ),
            criteria: area(
                "One acceptance criterion per line",
                4,
                &init.criteria,
                window,
                cx,
            ),
            checks: area(
                "One verification command per line, for example: cargo test -p auth",
                3,
                &init.checks,
                window,
                cx,
            ),
            scope: area(
                "One path pattern per line, for example: src/**",
                2,
                &init.scope,
                window,
                cx,
            ),
            budget: line(
                "Most you accept to spend, in USD (for example 5)",
                &init.budget,
                window,
                cx,
            ),
            workspace,
            confirm: cx.focus_handle().tab_stop(true),
            cancel: cx.focus_handle().tab_stop(true),
        }
    }
}

impl OutcomesView {
    /// What is typed now.
    fn form_text(&self, cx: &gpui::App) -> FormText {
        let Some(ui) = &self.form_ui else {
            return FormText::default();
        };
        let area = |s: &Entity<TextareaState>| s.read(cx).value().to_string();
        let line = |s: &Entity<InputState>| s.read(cx).value().to_string();
        FormText {
            goal: area(&ui.goal),
            criteria: area(&ui.criteria),
            checks: area(&ui.checks),
            budget: line(&ui.budget),
            scope: area(&ui.scope),
            workspace: line(&ui.workspace),
        }
    }

    fn workspace_left(&mut self, cx: &mut Context<Self>) {
        let t = self.form_text(cx);
        let e = self.state.workspace_left(&t);
        self.run(e, cx);
    }

    /// Enter on the confirm button, or a click on it: sends the edit stage, approves the review stage.
    pub(super) fn confirm(&mut self, cx: &mut Context<Self>) {
        let e = match self.state.form.as_ref().map(|f| &f.stage) {
            Some(Stage::Edit) => {
                let t = self.form_text(cx);
                self.state.submit(&t)
            }
            Some(Stage::Review(_)) if self.review_armed => self.state.approve(),
            _ => vec![],
        };
        self.run(e, cx);
    }

    /// Closes the form without sending anything (a request already on its way is not recalled).
    pub(super) fn close_form(&mut self, cx: &mut Context<Self>) {
        self.state.close_form();
        self.form_ui = None;
        cx.notify();
    }

    /// The keys of the open form. Tab and Shift+Tab move between the fields and the buttons, Enter does
    /// something only on a button (in a field it is a new line or nothing), Esc closes without sending.
    fn form_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return false;
        }
        let reviewing = self.state.form_review().is_some();
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.close_form(cx);
                window.focus(&self.focus, cx);
            }
            "tab" => {
                if m.shift {
                    window.focus_prev(cx)
                } else {
                    window.focus_next(cx)
                }
            }
            // a held key (auto-repeat) never acts: one deliberate press does
            "enter" if ev.is_held => {}
            "enter" => {
                let (confirm, cancel) = self
                    .form_ui
                    .as_ref()
                    .map(|u| (u.confirm.is_focused(window), u.cancel.is_focused(window)))
                    .unwrap_or_default();
                if confirm {
                    self.confirm(cx);
                } else if cancel {
                    self.close_form(cx);
                    window.focus(&self.focus, cx);
                }
            }
            // the review is read with the keys when no field holds them
            k @ ("up" | "down" | "pageup" | "pagedown") if reviewing => {
                scroll(
                    &self.form_scroll,
                    match k {
                        "up" => -1,
                        "down" => 1,
                        "pageup" => -12,
                        _ => 12,
                    },
                );
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    /// Makes the inputs when a form has opened and drops them when it closed (called while rendering).
    fn sync_form_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.state.form.as_ref(), self.form_ui.is_some()) {
            (Some(f), false) => {
                let ui = FormUi::new(&f.initial, window, cx);
                let first = ui.goal.read(cx).focus_handle(cx);
                let root = self.focus.clone();
                let edit = matches!(f.stage, Stage::Edit);
                self.form_ui = Some(ui);
                // never focus an answer: the first field takes the keys; a review starts on the window
                window.defer(cx, move |window, cx| {
                    window.focus(if edit { &first } else { &root }, cx)
                });
            }
            (None, true) => {
                self.form_ui = None;
                let root = self.focus.clone();
                window.defer(cx, move |window, cx| window.focus(&root, cx));
            }
            _ => {}
        }
        // the review starts with no button focused: approving takes a Tab to it, then Enter, never the
        // Enter that sent the form
        let reviewing = self.state.form_review().is_some();
        self.review_armed = reviewing && self.was_reviewing;
        if reviewing && !self.was_reviewing {
            let root = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&root, cx));
        }
        self.was_reviewing = reviewing;
    }
}

impl Focusable for OutcomesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests;
