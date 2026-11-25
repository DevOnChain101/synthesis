//! The New goal form of the Super Agent tab (`docs/specs/super-agent-s1.md` sections 1 and 2; mock
//! `super-agent-v3` `vNew` and the approve dialog). Two kinds: writes (goal, criteria, checks, scope, budget,
//! folder with its base commit) and read-only (goal, budget, attempts, any folder, rubric lines).
//!
//! The model is `superagent::outcomes::form`, unchanged: the text becomes a `Contract` checked by `validate_contract`, Send
//! stores a DRAFT on the server, the review shows what the server stored (version, hash, its diff), and approving is
//! a separate, explicit act on exactly that version and hash. Nothing here approves anything by itself: no field is
//! preselected, a prefill from a conductor card only fills text, and the approve button obeys the decision card's
//! rules (a fresh press, 400 ms after the dialog showed, Enter only on the button the person tabbed to).
//! The form owns its own `Link` to the orchestrator (the same worker the outcomes tab uses); drafts of the typed text
//! live in the entity, so the shell keeps one `GoalForm` per open `Frame::NewGoal`.

pub mod model;
mod review;
#[cfg(feature = "shot")]
pub mod shot;
#[cfg(test)]
mod tests;
mod view;

use std::time::Instant;

use futures::StreamExt as _;
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    ScrollHandle, Task, Window,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;

use self::model::{Field, approve_ok, blocked, field_of, why_not_send};
use super::chat::model::ARM;
use super::frames::GoalKind;
use crate::superagent::outcomes::form::{Form, FormText, Head, Stage, Submit};
use crate::superagent::outcomes::link::{self, FormMsg, Link, Msg, Req};
use crate::superagent::outcomes::model::Screen;

/// What the shell does with the frame.
pub enum GoalEvent {
    /// Cancel, Esc, "Not now": close the frame. Never sent while an approval is in flight (the frame waits for
    /// the answer), but a stored draft stays and the shell must not assume it is unapproved: show the outcome.
    Close,
    /// The server applied the approval of `outcome`: open it and close the frame.
    Approved { outcome: String, note: String },
}

impl EventEmitter<GoalEvent> for GoalForm {}

/// A request for the orchestrator. Without a link (the sample, the tests) it waits in `GoalForm::outbox`.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Head(String),
    Submit(Submit),
    Approve {
        outcome: String,
        version: u32,
        hash: String,
        key: String,
    },
}

/// The open approve dialog: when it showed (the 400 ms rule starts here).
pub(crate) struct Dialog {
    pub since: Instant,
}

pub(crate) struct Inputs {
    pub goal: Entity<TextareaState>,
    pub criteria: Entity<TextareaState>,
    pub checks: Entity<TextareaState>,
    pub scope: Entity<TextareaState>,
    pub budget: Entity<InputState>,
    pub attempts: Entity<InputState>,
    pub folder: Entity<InputState>,
}

/// The Tab stops that are not inputs.
pub(crate) struct Stops {
    pub choose: FocusHandle,
    pub send: FocusHandle,
    pub cancel: FocusHandle,
    pub review_approve: FocusHandle,
    pub review_close: FocusHandle,
    pub dlg_approve: FocusHandle,
    pub dlg_cancel: FocusHandle,
}

pub struct GoalForm {
    pub(crate) kind: GoalKind,
    pub(crate) form: Form,
    pub(crate) screen: Screen,
    /// The engine advertised `read_only_goal` and `report_review`.
    pub(crate) read_only_goal: bool,
    /// The engine runs on another machine: a local folder dialog would not name its folders.
    pub(crate) remote: bool,
    env: Option<Env>,
    link: Option<Link>,
    _pump: Option<Task<()>>,
    /// Requests made while no link exists (the sample, the tests).
    pub outbox: Vec<Out>,
    pub(crate) ui: Inputs,
    pub(crate) stops: Stops,
    pub(crate) focus: FocusHandle,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) scroll: ScrollHandle,
    /// The first field takes the keys when the form first shows.
    pub(crate) want_focus: bool,
    pub(crate) was_reviewing: bool,
}

impl GoalForm {
    /// `env`: the live app (checks the platform and the setting first, then connects); `None`: the sample, which
    /// sends nothing. `workspace` is the app's folder: the writes form starts with it (its base commit is read at
    /// once); the read-only form starts with no folder, the person names one.
    pub fn new(
        kind: GoalKind,
        env: Option<Env>,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let area = |ph: &'static str, rows: usize, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(ph)
                    .auto_grow(rows, rows)
            })
        };
        let line = |ph: &'static str, v: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(ph)
                    .default_value(v.to_string())
            })
        };
        let writes = kind == GoalKind::Writes;
        let start = if writes { workspace } else { "" };
        let folder = line("/absolute/path/of/the/folder", start, window, cx);
        // leaving the folder field reads the base commit again if the folder changed
        cx.subscribe_in(&folder, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Blur) {
                this.folder_left(cx);
            }
        })
        .detach();
        let ui = Inputs {
            goal: area(
                if writes {
                    "What should change?"
                } else {
                    "What should be checked?"
                },
                3,
                window,
                cx,
            ),
            criteria: area(
                if writes {
                    "One acceptance criterion per line"
                } else {
                    "One line per thing the report must cover (optional)"
                },
                if writes { 4 } else { 3 },
                window,
                cx,
            ),
            checks: area(
                "One verification command per line, for example: cargo test -p auth",
                3,
                window,
                cx,
            ),
            scope: area(
                "One path pattern per line, for example: src/**",
                2,
                window,
                cx,
            ),
            budget: line(
                "Most you accept to spend, in USD (for example 5)",
                "",
                window,
                cx,
            ),
            attempts: line("3", "", window, cx),
            folder,
        };
        let tab = |cx: &mut Context<Self>| cx.focus_handle().tab_stop(true);
        let stops = Stops {
            choose: tab(cx),
            send: tab(cx),
            cancel: tab(cx),
            review_approve: tab(cx),
            review_close: tab(cx),
            dlg_approve: tab(cx),
            dlg_cancel: tab(cx),
        };
        let mut form = if writes {
            Form::new_outcome(start)
        } else {
            Form::new_read_only(start)
        };
        let head = writes && !start.trim().is_empty();
        if head {
            form.resolving(start.trim());
        }
        let mut this = Self {
            kind,
            form,
            screen: Screen::Ready,
            read_only_goal: true,
            remote: false,
            env: env.clone(),
            link: None,
            _pump: None,
            outbox: vec![],
            ui,
            stops,
            focus: cx.focus_handle(),
            dialog: None,
            scroll: ScrollHandle::new(),
            want_focus: true,
            was_reviewing: false,
        };
        if head {
            this.run(Out::Head(start.trim().to_string()));
        }
        if let Some(env) = env {
            this.connect(env, cx);
        }
        this
    }

    /// The platform and the setting are checked before anything starts (no thread, socket or process is made on
    /// native Windows or with Super Agent off).
    fn connect(&mut self, env: Env, cx: &mut Context<Self>) {
        match link::gate(&env) {
            Err(s) => self.screen = s,
            Ok(home) => {
                self.screen = Screen::Loading;
                self.read_only_goal = false;
                self.remote = crate::wsl::enabled(&home).unwrap_or(true);
                let (l, mut rx) = Link::start(env);
                self.link = Some(l);
                self._pump = Some(cx.spawn(async move |this, cx| {
                    while let Some(m) = rx.next().await {
                        let mut batch = vec![m];
                        while let Ok(m) = rx.try_recv() {
                            batch.push(m);
                        }
                        if this
                            .update(cx, |v, cx| {
                                for m in batch {
                                    v.apply(m, cx)
                                }
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }));
            }
        }
        // the Head request made before the link existed goes out now
        let queued = std::mem::take(&mut self.outbox);
        if self.link.is_some() {
            for o in queued {
                self.run(o);
            }
        } else {
            self.outbox = queued;
        }
    }

    /// What the link said. A snapshot only says the link is up.
    pub(crate) fn apply(&mut self, m: Msg, cx: &mut Context<Self>) {
        match m {
            Msg::Screen(s) => self.screen = s,
            Msg::ReadOnlyGoal(on) => self.read_only_goal = on,
            Msg::Snapshot { .. } => self.screen = Screen::Ready,
            Msg::Form(m) => self.apply_form(m, cx),
            Msg::Inspector { .. }
            | Msg::Answered(_)
            | Msg::Enabled(..)
            | Msg::AttemptMessage(_)
            | Msg::Sent { .. } => {}
        }
        cx.notify();
    }

    /// What the worker said about the form. A late answer for another stage is dropped by `Form` itself.
    pub(crate) fn apply_form(&mut self, m: FormMsg, cx: &mut Context<Self>) {
        match m {
            FormMsg::Approved(note) => {
                if let Stage::Review(r) | Stage::Approving(r) = &self.form.stage {
                    let outcome = r.outcome.clone();
                    self.dialog = None;
                    cx.emit(GoalEvent::Approved { outcome, note });
                }
            }
            FormMsg::Head { ws, result } => self.form.head_known(&ws, result),
            FormMsg::Failed { text, spent } => {
                self.dialog = None;
                self.form.failed(text, spent)
            }
            FormMsg::BaseMoved { ws, commit, text } => self.form.base_moved(&ws, commit, text),
            FormMsg::Review(r) => {
                self.dialog = None;
                self.form.reviewing(*r)
            }
        }
        cx.notify();
    }

    /// Does a request now (link) or keeps it (no link).
    fn run(&mut self, o: Out) {
        let Some(l) = &self.link else {
            self.outbox.push(o);
            return;
        };
        let req = match o {
            Out::Head(ws) => return l.head(ws),
            Out::Submit(s) => Req::Submit(s),
            Out::Approve {
                outcome,
                version,
                hash,
                key,
            } => Req::Approve {
                outcome,
                version,
                hash,
                key,
            },
        };
        // a request after a failed link goes after a retry
        if self.screen != Screen::Ready {
            l.send(Req::Retry);
        }
        if !l.try_send(req) {
            // the key is not spent: the same request goes out again unchanged
            self.form.failed(
                "The link to the orchestrator has ended. Press Try again.".into(),
                false,
            );
        }
    }

    /// "Try again" of a failed link.
    pub(crate) fn retry_link(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &self.link {
            self.screen = Screen::Loading;
            l.send(Req::Retry);
        } else if let Some(env) = self.env.clone() {
            // the gate refused at the start (no link was made): check again
            self.connect(env, cx);
        }
        cx.notify();
    }

    // -------------------------------------------------------------------------------- the text

    pub(crate) fn text(&self, cx: &gpui::App) -> FormText {
        let area = |s: &Entity<TextareaState>| s.read(cx).value().to_string();
        let line = |s: &Entity<InputState>| s.read(cx).value().to_string();
        FormText {
            goal: area(&self.ui.goal),
            criteria: area(&self.ui.criteria),
            checks: area(&self.ui.checks),
            budget: line(&self.ui.budget),
            scope: area(&self.ui.scope),
            workspace: line(&self.ui.folder),
            attempts: line(&self.ui.attempts),
        }
    }

    /// Puts `v` into `f`'s input (the shots and the tests type this way).
    #[cfg(any(test, feature = "shot"))]
    pub(crate) fn set_text(
        &mut self,
        f: Field,
        v: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let v = v.to_string();
        let u = &self.ui;
        match f {
            Field::Goal => u.goal.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Criteria => u.criteria.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Checks => u.checks.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Scope => u.scope.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Budget => u.budget.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Attempts => u.attempts.update(cx, |s, cx| s.set_value(v, window, cx)),
            Field::Folder => u.folder.update(cx, |s, cx| s.set_value(v, window, cx)),
        }
    }

    /// Fills the goal (and the folder, when none is typed) from a conductor goal card. Nothing is created and
    /// nothing is approved: the person completes the form, sends it, and approves the stored contract. The text is
    /// the model's, so hidden characters are made visible before it can reach a contract. `false`: not applied
    /// (the person already typed a goal, or the form is past editing).
    pub fn prefill(
        &mut self,
        goal: &str,
        folder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.form.stage != Stage::Edit || !self.text(cx).goal.trim().is_empty() {
            return false;
        }
        let goal = sanitize_for_display(goal);
        self.ui
            .goal
            .update(cx, |s, cx| s.set_value(goal, window, cx));
        if self.text(cx).workspace.trim().is_empty() && !folder.trim().is_empty() {
            self.set_folder(&sanitize_for_display(folder.trim()), window, cx);
        }
        cx.notify();
        true
    }

    fn set_folder(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.ui
            .folder
            .update(cx, |s, cx| s.set_value(path.to_string(), window, cx));
        self.folder_left(cx);
    }

    /// The folder field was left (or filled): the base commit of a writes goal is read again if the folder changed.
    fn folder_left(&mut self, cx: &mut Context<Self>) {
        let t = self.text(cx);
        if let Some(ws) = self.form.needs_head(&t) {
            self.form.resolving(&ws);
            self.run(Out::Head(ws));
            cx.notify();
        }
    }

    /// "Choose folder...": the system dialog (one directory); a cancelled dialog changes nothing.
    pub(crate) fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the folder of the goal".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                this.update_in(cx, |t, window, cx| {
                    t.set_folder(&p.to_string_lossy(), window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    // -------------------------------------------------------------------------------- the flow

    /// Why Send does nothing now (shown next to it), or `None`.
    pub(crate) fn why_not_send(&self, cx: &gpui::App) -> Option<&'static str> {
        let goal = self.ui.goal.read(cx).value();
        why_not_send(
            self.kind,
            &self.form,
            &self.screen,
            self.read_only_goal,
            &goal,
        )
    }

    /// Send (button, Enter on it, Cmd+Enter): creates the DRAFT on the server; invalid text shows its errors inline.
    pub fn send(&mut self, cx: &mut Context<Self>) {
        if self.why_not_send(cx).is_some() {
            return;
        }
        let t = self.text(cx);
        if let Some(s) = self.form.submit(&t) {
            self.run(Out::Submit(s));
        }
        cx.notify();
    }

    /// The review's "Approve contract...": opens the dialog. Nothing is approved by this.
    pub(crate) fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(&self.form.stage, Stage::Review(r) if r.approvable()) && self.dialog.is_none() {
            self.dialog = Some(Dialog {
                since: Instant::now(),
            });
            // no button of the dialog has the focus: approving takes a Tab to it
            window.focus(&self.focus, cx);
            // the button looks armed once the 400 ms are over
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(ARM + std::time::Duration::from_millis(30))
                    .await;
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
            cx.notify();
        }
    }

    pub(crate) fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The dialog's Approve (click or Enter on it): the version and hash the server reported, never others.
    /// `key`: it came from the keyboard (then the button must hold the focus).
    pub(crate) fn approve(
        &mut self,
        key: bool,
        held: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(d) = &self.dialog else { return };
        let focused = self.stops.dlg_approve.is_focused(window);
        if !approve_ok(d.since, Instant::now(), held, key, focused) {
            return;
        }
        if let Some((outcome, version, hash, key)) = self.form.approve() {
            self.run(Out::Approve {
                outcome,
                version,
                hash,
                key,
            });
        }
        cx.notify();
    }

    /// An approval is in flight: the frame must not close.
    pub(crate) fn approving(&self) -> bool {
        matches!(self.form.stage, Stage::Approving(_))
    }

    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        // an approval in flight may still be applied: the frame stays until the server answers
        if matches!(self.form.stage, Stage::Approving(_)) {
            return;
        }
        cx.emit(GoalEvent::Close);
    }

    // -------------------------------------------------------------------------------- the keys

    /// Tab and Shift+Tab move between fields and buttons; Cmd+Enter sends; Enter acts on a focused button only and
    /// never when held; Esc closes the dialog, else the frame. True when the key was the form's.
    pub fn on_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (k, m) = (ev.keystroke.key.as_str(), &ev.keystroke.modifiers);
        if m.secondary(cx) && k == "enter" {
            if !ev.is_held && self.form.stage == Stage::Edit && self.dialog.is_none() {
                self.send(cx);
            }
            return true;
        }
        if m.control || m.alt || m.platform {
            return false;
        }
        let s = &self.stops;
        match k {
            "escape" if self.dialog.is_some() => self.close_dialog(window, cx),
            "escape" => self.close(cx),
            // inside the dialog Tab cycles its two buttons: the page behind is not reachable
            "tab" if self.dialog.is_some() => {
                let to = if s.dlg_approve.is_focused(window) {
                    &s.dlg_cancel
                } else {
                    &s.dlg_approve
                };
                window.focus(to, cx);
            }
            "tab" if m.shift => window.focus_prev(cx),
            "tab" => window.focus_next(cx),
            "enter" if ev.is_held => {}
            "enter" => {
                let at = |h: &FocusHandle| h.is_focused(window);
                if self.dialog.is_some() {
                    if at(&s.dlg_approve) {
                        self.approve(true, false, window, cx);
                    } else if at(&s.dlg_cancel) {
                        self.close_dialog(window, cx);
                    }
                } else if at(&s.send) {
                    self.send(cx);
                } else if at(&s.cancel) || at(&s.review_close) {
                    self.close(cx);
                } else if at(&s.choose) {
                    self.choose_folder(window, cx);
                } else if at(&s.review_approve) {
                    self.open_dialog(window, cx);
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    // -------------------------------------------------------------------------------- errors

    /// The inline errors of `f`, and the ones that belong to no field (shown above the form).
    pub(crate) fn errors_of(&self, f: Field) -> Vec<&str> {
        self.form
            .errors
            .iter()
            .filter(|e| field_of(e) == Some(f))
            .map(String::as_str)
            .collect()
    }

    pub(crate) fn loose_errors(&self) -> Vec<&str> {
        self.form
            .errors
            .iter()
            .filter(|e| field_of(e).is_none())
            .map(String::as_str)
            .collect()
    }

    pub(crate) fn blocked(&self) -> Option<&'static str> {
        blocked(&self.screen)
    }

    pub(crate) fn head_text(&self) -> (String, bool) {
        match &self.form.head {
            None => ("Enter the folder: its HEAD is read here.".into(), false),
            Some(Head::Resolving(_)) => ("Reading HEAD...".into(), false),
            Some(Head::Known { commit, .. }) => (commit.clone(), false),
            Some(Head::Failed { why, .. }) => (why.clone(), true),
        }
    }
}

impl Focusable for GoalForm {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}
