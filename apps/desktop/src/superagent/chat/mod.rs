//! The orchestrator chat with proposal cards and its composer (phase B3; super-agent-s1.md 4, 5.5).
//!
//! `ConductorChat` is the conversation of the Super Agent home. The person's line starts a conductor turn
//! through the host (`person_turn`); the model can only read digests and store proposals; a proposal becomes an
//! effect only through `ProposalExecutor`, whose one key is the click built in [`ConductorChat::confirm_clicked`]
//! and nowhere else. Cards are drawn from the host's `Proposal` structs ([`cards`]), never from model text, so a
//! forged "card" in a reply is just words in a message.

pub mod cards;
pub mod glue;
pub mod model;
#[cfg(feature = "shot")]
pub mod shot;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Window, div, prelude::*, px,
};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::text::TextView;
use synthesis_app::conductor::{Payload, PersonClick};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;
use synthesis_view::conductor::CardButton;
use synthesis_view::convo::{Convo, Item};

use self::cards::Acts;
use self::glue::{Live, NoPort, WindowPort};
use self::model::{Gate, Shown};
use super::kit::{self, ButtonKind, text};
use super::tokens::{self as t, hsla};
use crate::engine::{Choice, UiMsg};
use crate::outcomes_view::model::OFF_LINES;

/// How often open cards are checked for expiry.
const TICK: Duration = Duration::from_secs(5);
/// A tool row is cut here (model-written text never takes the width of the chat).
const ROW_MAX: usize = 160;
const COMMANDS_REFUSED: &str = "Commands are not available in this chat.";

/// The person asked to turn Super Agent on (the Off explanation's button): the shell opens its enable guide.
pub struct OpenEnableGuide;

impl EventEmitter<OpenEnableGuide> for ConductorChat {}

/// An open edit box. `original` is the proposal's own text (the box shows `shown`, which is cleaned for display):
/// a box left untouched is no edit, so the original bytes go on, never the cleaned ones.
struct Edit {
    ent: Entity<TextareaState>,
    original: String,
    shown: String,
}

/// What an edit sends: `None` when the box still holds the text as shown (run the proposal as it is).
fn edit_text(original: &str, shown: &str, value: &str) -> Option<String> {
    (value != shown && value != original).then(|| value.to_string())
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Starting,
    Ready,
    Failed(String),
}

pub struct ConductorChat {
    env: Env,
    workspace: PathBuf,
    port: Arc<dyn WindowPort>,
    gate: Gate,
    phase: Phase,
    live: Option<Live>,
    convo: Convo,
    /// What the person typed, in order: the n-th `Item::User` of the engine is the n-th line (the engine's own
    /// text also carries host notes, which are not the person's words).
    lines: Vec<String>,
    cards: Vec<Shown>,
    /// Open edit boxes ("Edit in form"), by proposal id.
    editors: HashMap<String, Edit>,
    /// Proposals whose click is being run: a second click is ignored.
    busy: HashSet<String>,
    /// Tab stops of each card's Confirm, Edit and Cancel (nothing is focused until the person tabs to one).
    focus: HashMap<String, [FocusHandle; 3]>,
    composer: Entity<TextareaState>,
    /// Why the last send did not go (a turn is running, a command, the engine failed).
    notice: Option<String>,
    notes: Vec<String>,
}

impl ConductorChat {
    pub fn new(
        env: Env,
        workspace: PathBuf,
        port: Option<Arc<dyn WindowPort>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message Super-Agent")
                .auto_grow(1, 6)
                .submit_on_enter(true)
        });
        cx.subscribe_in(&composer, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(
                ev,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.submit(window, cx);
            }
        })
        .detach();
        // cards expire on the host's clock: look again now and then (the entity ends this loop when it goes)
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |c, cx| c.tick(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
        Self {
            gate: model::gate(&env),
            env,
            workspace,
            port: port.unwrap_or_else(|| Arc::new(NoPort)),
            phase: Phase::Idle,
            live: None,
            convo: Convo::default(),
            lines: vec![],
            cards: vec![],
            editors: HashMap::new(),
            busy: HashSet::new(),
            focus: HashMap::new(),
            composer,
            notice: None,
            notes: vec![],
        }
    }

    /// News for the conductor's next turn (a session finished, needs the person, ...): never starts a turn.
    /// `session_text`: it carries text from a session, which taints the next proposal.
    pub fn push_news(&self, text: &str, session_text: bool) {
        if let Some(l) = &self.live {
            l.host.push_news(text, session_text);
        }
    }

    /// The setting changed (the person turned Super Agent on): check again.
    pub fn recheck(&mut self, cx: &mut Context<Self>) {
        self.gate = model::gate(&self.env);
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ sending

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.gate != Gate::Ready || self.phase == Phase::Starting {
            return;
        }
        let line = self.composer.read(cx).value().trim().to_string();
        if line.is_empty() {
            return;
        }
        self.notice = None;
        if line.starts_with('/') {
            self.notice = Some(COMMANDS_REFUSED.into());
        } else if self.live.is_none() {
            self.start(window, cx);
        } else {
            self.send(&line, window, cx);
        }
        cx.notify();
    }

    fn send(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(l) = &self.live else { return };
        if l.running() {
            self.notice = Some("Wait for the reply, or press Stop.".into());
            return;
        }
        // the host builds the turn: its pending notes, fenced, then the person's line
        let turn = l.host.person_turn(line);
        let sent = l
            .engine()
            .map_or(Err("the engine is gone"), |e| e.send(turn));
        match sent {
            Ok(()) => {
                self.lines.push(line.to_string());
                self.composer
                    .update(cx, |s, cx| s.set_value(String::new(), window, cx));
            }
            Err(e) => self.notice = Some(e.to_string()),
        }
    }

    /// First use: the engine starts on a thread of its own; the draft stays in the box until it is up.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.phase = Phase::Starting;
        let (tx, rx) = futures::channel::oneshot::channel();
        let (env, ws, port) = (self.env.clone(), self.workspace.clone(), self.port.clone());
        std::thread::spawn(move || {
            // nobody waiting any more: the engine is dropped here, and `Live` closes it
            let _ = tx.send(glue::start(&env, &ws, port));
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(r) = rx.await else { return };
            this.update_in(cx, |c, window, cx| c.started(r, window, cx))
                .ok();
        })
        .detach();
    }

    fn started(
        &mut self,
        r: Result<glue::Started, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match r {
            Err(e) => self.phase = Phase::Failed(sanitize_for_display(&e)),
            Ok(s) => {
                self.notes = s.notes.iter().map(|n| sanitize_for_display(n)).collect();
                self.live = Some(s.live);
                self.phase = Phase::Ready;
                let mut rx = s.rx;
                cx.spawn(async move |this, cx| {
                    while let Some(m) = rx.next().await {
                        if this.update(cx, |c, cx| c.on_msg(m, cx)).is_err() {
                            return;
                        }
                    }
                })
                .detach();
                let line = self.composer.read(cx).value().trim().to_string();
                if line.starts_with('/') {
                    self.notice = Some(COMMANDS_REFUSED.into());
                } else if !line.is_empty() {
                    self.send(&line, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn on_msg(&mut self, m: UiMsg, cx: &mut Context<Self>) {
        match m {
            UiMsg::Event(e) => self.convo.apply(&e),
            UiMsg::TaskDone(Err(e)) => self.notice = Some(sanitize_for_display(&e)),
            // the conductor has no tool that asks; a hook that does is refused, never left waiting
            UiMsg::Approval(a) => {
                if let Some(e) = self.live.as_ref().and_then(Live::engine) {
                    e.answer(&a.id, Choice::Deny);
                }
                self.notice = Some("A permission request in this chat was denied.".into());
            }
            UiMsg::Input(a) => {
                if let Some(e) = self.live.as_ref().and_then(Live::engine) {
                    e.answer_input(&a.id, None);
                }
            }
            _ => {}
        }
        self.refresh_cards();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.live.as_ref().and_then(Live::engine) {
            e.cancel();
        }
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ cards

    /// Reads the host's proposals again; the card on screen is always what this last read said.
    fn refresh_cards(&mut self) {
        let Some(l) = &self.live else { return };
        let ps: Vec<_> = l
            .host
            .proposals()
            .iter()
            .filter_map(|p| serde_json::to_value(p).ok())
            .collect();
        self.cards = model::snapshot(&ps, &self.cards, self.convo.items.len(), Instant::now());
        let open: HashSet<_> = self
            .cards
            .iter()
            .filter(|s| !s.card.buttons.is_empty())
            .map(|s| s.card.id.clone())
            .collect();
        self.editors.retain(|id, _| open.contains(id));
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.cards.iter().any(|s| !s.card.buttons.is_empty()) {
            self.refresh_cards();
            cx.notify();
        }
    }

    fn card(&mut self, id: &str) -> Option<&mut Shown> {
        self.cards.iter_mut().find(|s| s.card.id == id)
    }

    /// The Confirm button was pressed. THE only place that builds the person's click, and it builds it only for
    /// an open card that has been showing these bytes for [`model::ARM`], whatever input pressed the button.
    /// The hash passed on is the one the card displayed: if the proposal changed since, the host says `stale`.
    fn confirm_clicked(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(exec) = self.live.as_ref().map(|l| l.exec.clone()) else {
            return;
        };
        let edited = self
            .editors
            .get(id)
            .and_then(|e| edit_text(&e.original, &e.shown, &e.ent.read(cx).value()));
        let busy = self.busy.contains(id);
        let Some(s) = self.card(id) else { return };
        if busy || !s.card.buttons.contains(&CardButton::Confirm) || !s.armed(Instant::now()) {
            return;
        }
        s.note = None;
        let sha = s.sha256.clone();
        let id = id.to_string();
        self.busy.insert(id.clone());
        let click = PersonClick::from_card_click();
        cx.spawn(async move |this, cx| {
            let r = match edited {
                Some(t) => exec.execute_edited(&id, &sha, t, click).await,
                None => exec.execute(&id, &sha, click).await,
            };
            this.update(cx, |c, cx| c.decided(&id, r, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn decided(
        &mut self,
        id: &str,
        r: Result<synthesis_app::conductor::State, String>,
        cx: &mut Context<Self>,
    ) {
        self.busy.remove(id);
        // a stale or expired proposal is read again: the card on screen shows what is true now
        self.refresh_cards();
        let detail = self
            .live
            .as_ref()
            .and_then(|l| l.host.proposal(id))
            .and_then(|p| p.detail);
        let note = match r {
            Err(e) => Some(sanitize_for_display(&e)),
            Ok(synthesis_app::conductor::State::Failed) => detail.map(|d| sanitize_for_display(&d)),
            Ok(_) => None,
        };
        if let Some(s) = self.card(id) {
            s.note = note;
        }
        cx.notify();
    }

    fn cancel_clicked(&mut self, id: &str, cx: &mut Context<Self>) {
        let r = self.live.as_ref().map(|l| l.host.cancel(id));
        self.refresh_cards();
        if let (Some(Err(e)), Some(s)) = (r, self.card(id)) {
            s.note = Some(sanitize_for_display(&e));
        }
        cx.notify();
    }

    /// "Edit in form": the text field of the card opens as a box holding exactly the text the card showed.
    fn edit_clicked(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.editors.remove(id).is_some() {
            cx.notify();
            return;
        }
        let Some(value) = self.cards.iter().find(|s| s.card.id == id).and_then(|s| {
            s.card
                .fields
                .iter()
                .find(|(k, _)| cards::is_text(k))
                .map(|(_, v)| v.clone())
        }) else {
            return;
        };
        let original = self
            .live
            .as_ref()
            .and_then(|l| l.host.proposal(id))
            .and_then(|p| match p.payload {
                Payload::CreateSession {
                    first_message: t, ..
                }
                | Payload::SendMessage { text: t, .. }
                | Payload::CreateGoal { contract: t, .. } => Some(t),
                _ => None,
            })
            .unwrap_or_else(|| value.clone());
        let shown = value.clone();
        let ent = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .default_value(value)
        });
        self.editors.insert(
            id.to_string(),
            Edit {
                ent,
                original,
                shown,
            },
        );
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ drawing

    fn transcript(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![];
        let mut people = self.lines.iter();
        let n = self.convo.items.len();
        for idx in 0..=n {
            for s in &self.cards {
                if s.anchor.min(n) == idx {
                    out.push(self.card_el(s, cx));
                }
            }
            let Some(it) = self.convo.items.get(idx) else {
                continue;
            };
            match it {
                Item::User(_) => {
                    if let Some(l) = people.next() {
                        out.push(message("You", user_bubble(l)));
                    }
                }
                Item::Assistant { text: s, .. } => out.push(message(
                    "Super-Agent",
                    div()
                        .text_size(px(t::T3.0))
                        .line_height(px(t::T3.1))
                        .text_color(hsla(t::TEXT_STRONG))
                        .child(md_blocks(idx, s)),
                )),
                Item::Tool { name, summary, .. } => out.push(tool_row(name, summary)),
                Item::TaskEnd {
                    ok: false, text: s, ..
                } => out.push(note_row(&clip(&sanitize_for_display(s)))),
                _ => {}
            }
        }
        out
    }

    fn card_el(&self, s: &Shown, cx: &mut Context<Self>) -> AnyElement {
        let id = s.card.id.clone();
        let (a, b, c) = (id.clone(), id.clone(), id);
        let acts = Acts {
            confirm: Rc::new(
                cx.listener(move |this, _: &ClickEvent, _, cx| this.confirm_clicked(&a, cx)),
            ),
            edit: Rc::new(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.edit_clicked(&b, window, cx)
            })),
            cancel: Rc::new(
                cx.listener(move |this, _: &ClickEvent, _, cx| this.cancel_clicked(&c, cx)),
            ),
        };
        cards::render(
            s,
            self.editors.get(&s.card.id).map(|e| &e.ent),
            &acts,
            &self.focus[&s.card.id],
        )
    }

    fn composer_el(&self, cx: &mut Context<Self>) -> AnyElement {
        let starting = self.phase == Phase::Starting;
        let running = self.live.as_ref().is_some_and(Live::running);
        let hint = match &self.phase {
            Phase::Starting => Some("Starting the orchestrator chat...".to_string()),
            Phase::Failed(e) => Some(format!("Could not start: {e}")),
            _ => self.notice.clone(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(t::S2))
            .child(
                div()
                    .debug_selector(|| "sa-chat-composer".to_string())
                    .flex()
                    .items_center()
                    .gap(px(t::S2))
                    .p(px(t::S2))
                    .rounded(px(t::R_FLOAT))
                    .bg(hsla(t::ELEVATED))
                    .child(kit::composer_chip("sa-chat-chip", "Super-Agent"))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Textarea::new(&self.composer)
                                .appearance(false)
                                .bordered(false),
                        ),
                    )
                    .child(if running {
                        kit::button(
                            "sa-chat-stop",
                            "Stop",
                            ButtonKind::Secondary,
                            false,
                            Some(Box::new(cx.listener(|this, _, _, cx| this.stop(cx)))),
                        )
                    } else {
                        kit::button(
                            "sa-chat-send",
                            "Send",
                            if starting {
                                ButtonKind::Disabled
                            } else {
                                ButtonKind::Primary
                            },
                            false,
                            Some(Box::new(
                                cx.listener(|this, _, window, cx| this.submit(window, cx)),
                            )),
                        )
                    }),
            )
            .children(hint.map(|h| {
                text(t::T2, t::W_REGULAR, t::TEXT)
                    .debug_selector(|| "sa-chat-hint".to_string())
                    .whitespace_normal()
                    .px(px(t::S2))
                    .child(clip(&h))
            }))
            .into_any_element()
    }

    /// The setting is off, or the chat cannot run here: say so and why, and offer nothing that would start it.
    fn unavailable(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut c = kit::card("sa-chat-off", false);
        match &self.gate {
            Gate::Off => {
                for l in OFF_LINES {
                    c = c.child(
                        text(t::T3, t::W_REGULAR, t::TEXT)
                            .whitespace_normal()
                            .child(l),
                    );
                }
                c = c.child(
                    kit::button(
                        "sa-chat-enable",
                        "Turn on Super Agent...",
                        ButtonKind::Secondary,
                        false,
                        Some(Box::new(
                            cx.listener(|_, _, _, cx| cx.emit(OpenEnableGuide)),
                        )),
                    )
                    .self_start(),
                );
            }
            Gate::Unavailable(why) => {
                c = c.child(
                    text(t::T3, t::W_REGULAR, t::TEXT)
                        .whitespace_normal()
                        .child(clip(&sanitize_for_display(why))),
                );
            }
            Gate::Ready => {}
        }
        c.into_any_element()
    }
}

impl Render for ConductorChat {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for s in &self.cards {
            self.focus
                .entry(s.card.id.clone())
                .or_insert_with(|| std::array::from_fn(|_| cx.focus_handle().tab_stop(true)));
        }
        let body = if self.gate == Gate::Ready {
            let rows = self.transcript(cx);
            div()
                .flex()
                .flex_col()
                .gap(px(t::S4))
                .px(px(t::S4))
                .children(rows)
                .children(self.notes.iter().map(|n| note_row(&clip(n))))
                .child(self.composer_el(cx))
                .into_any_element()
        } else {
            div()
                .px(px(t::S4))
                .child(self.unavailable(cx))
                .into_any_element()
        };
        div()
            .debug_selector(|| "sa-chat".to_string())
            .flex()
            .flex_col()
            .child(kit::section_header("sa-chat-head", "Conversation", None))
            .child(body)
    }
}

/// The conversation of the home: mount the returned element in the slot of `home::render`.
pub fn conversation(chat: &Entity<ConductorChat>) -> AnyElement {
    chat.clone().into_any_element()
}

// ------------------------------------------------------------------------------------------ small pieces

/// Model text as markdown, inert: a link is shown but never opened (the model's text is untrusted, and a link
/// could pose as a card button), and no image is loaded from a path or a URL. Hidden characters are made visible.
fn md(idx: usize, text: &str) -> TextView {
    TextView::markdown(("sa-chat-md", idx), clean_md(text))
        .selectable(true)
        .on_link_click(|_, _, _, _| {})
        .image_source(|_| gpui::ImageSource::Custom(Arc::new(|_, _| None)))
}

/// The reply with each fenced block as its own section (Copy only: this chat has no terminal); the prose stays
/// the inert `md`. A reply without a fence is the plain `md`.
fn md_blocks(idx: usize, text: &str) -> AnyElement {
    let segs = synthesis_view::codeblock::split(text);
    if !crate::widgets::codeblock::has_blocks(&segs) {
        return md(idx, text).into_any_element();
    }
    crate::widgets::codeblock::message(
        &format!("sa-chat-{idx}"),
        &segs,
        &mut |i, p| md(idx * 1024 + i, &p).into_any_element(),
        &mut |_, _, _| None,
    )
    .into_any_element()
}

/// Bidi, zero-width and control characters (but not newline or tab) become visible `\u{..}` escapes.
fn clean_md(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\t' => c.to_string(),
            c if c.is_control()
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}') =>
            {
                format!("\\u{{{:04x}}}", c as u32)
            }
            c => c.to_string(),
        })
        .collect()
}

fn clip(s: &str) -> String {
    if s.chars().count() <= ROW_MAX {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(ROW_MAX).collect::<String>())
    }
}

fn message(who: &'static str, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(t::S1))
        .min_w(px(0.))
        .child(text(t::T2, t::W_MEDIUM, t::TEXT).child(who))
        .child(body)
        .into_any_element()
}

fn user_bubble(line: &str) -> impl IntoElement {
    div()
        .self_start()
        .px(px(t::S4))
        .py(px(t::S2))
        .rounded(px(t::R_CONTROL))
        .bg(hsla(t::SURFACE))
        .text_size(px(t::T3.0))
        .line_height(px(t::T3.1))
        .text_color(hsla(t::TEXT_STRONG))
        .child(line.to_string())
}

/// Tool activity as a quiet row: plain, no button, and nothing that looks like a card. The model wrote part of
/// the text, so it is cleaned and cut.
fn tool_row(name: &str, summary: &str) -> AnyElement {
    let what = name.trim_start_matches("conductor_").replace('_', " ");
    let line = clip(&sanitize_for_display(&format!("{what} {summary}")));
    text(t::T2, t::W_REGULAR, t::TEXT)
        .debug_selector(|| "sa-chat-tool".to_string())
        .whitespace_normal()
        .child(line)
        .into_any_element()
}

fn note_row(s: &str) -> AnyElement {
    text(t::T2, t::W_REGULAR, t::TEXT)
        .self_start()
        .px(px(t::S4))
        .py(px(t::S2))
        .rounded(px(t::R_CONTROL))
        .bg(hsla(t::SURFACE))
        .whitespace_normal()
        .child(s.to_string())
        .into_any_element()
}
