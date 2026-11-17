//! The Outcomes tab (owner decision 2026-11-17; `docs/specs/desktop.md` "Outcomes tab", ADR-021): outcomes the
//! orchestrator runs, three columns (list, Goal | Decisions | History, inspector). It is a surface in the
//! sense of ADR-021: it talks to the orchestrator only through `OutcomeClient` on its own worker thread (`link`),
//! renders only `synthesis_view::outcomes` models, and depends on no orchestrator crate. This is not the
//! Conductor (`docs/specs/conductor.md`), and it creates and edits nothing yet.
//!
//! Everything is on the keyboard (Tab between the columns, arrows, numbers, Enter, Esc); the mouse does the
//! same things and nothing is mouse-only.

use futures::StreamExt as _;
use gpui::{Context, FocusHandle, Focusable, ScrollHandle, Task, Window, point, px};
use synthesis_config::Env;

use crate::live::now_ms;
use link::{Link, Msg, Req};
use model::{Effect, Key, Pane, Screen, State};

pub mod link;
pub mod model;
mod render;
pub mod sample;
#[cfg(feature = "shot")]
pub mod shot;

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
}

impl OutcomesView {
    /// The live tab: it connects (and checks the setting) when it is first shown, never at launch.
    pub fn live(env: Env, cx: &mut Context<Self>) -> Self {
        let mut v = Self::with_state(State::new(Screen::Loading), cx);
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
                Msg::Screen(s) => self.state.screen = s,
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
            }
        }
        cx.notify();
    }

    /// Does what the state asked for. The sample answers locally (and sends nothing).
    fn run(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for e in effects {
            match (e, &self.link) {
                (Effect::Scroll(n), _) => {
                    let h = &self.scroll[match self.state.pane {
                        Pane::List => 0,
                        Pane::Centre => 1,
                        Pane::Inspector => 2,
                    }];
                    let max = f32::from(h.max_offset().y);
                    let y = (f32::from(h.offset().y) - n as f32 * 19.).clamp(-max, 0.);
                    h.set_offset(point(px(0.), px(y)));
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
                (Effect::Refresh | Effect::Retry, None) => {}
                (Effect::Refresh, Some(l)) => l.send(Req::Refresh),
                (Effect::Retry, Some(l)) => {
                    self.state.screen = Screen::Loading;
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

    pub(crate) fn on_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &ev.keystroke;
        let m = &k.modifiers;
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

impl Focusable for OutcomesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests;
