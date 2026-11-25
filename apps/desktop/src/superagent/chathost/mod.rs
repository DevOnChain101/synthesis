//! The Super Agent's own chat for a session frame (backlog item B): message turns in the UI font, tool calls as
//! compact collapsible cards, fenced blocks with Copy and the two-click Run, the permission and question cards
//! above the composer, and a composer with model and mode pills.
//!
//! It draws; it does not drive. The engine, the session rules (lazy start, queue, `/commands`), the approvals and
//! the Terminal and Browser panes stay in the embedded `TuiView` that the `SessionHost` still creates per frame
//! (`Embed::headless`: never drawn, its key hook only serves those panes and the dialogs). Everything shown comes
//! from that view's `Live` (the `Convo` of `synthesis-view`, already redacted for display) and is cleaned again
//! with `sanitize_for_display` or the inert markdown of the conductor chat, so no text reaches the screen another way.

pub mod model;
#[cfg(feature = "shot")]
pub mod shot;
#[cfg(test)]
mod tests;

use std::collections::HashSet;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::*, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::input::Textarea;
use synthesis_harness::sanitize_for_display;
use synthesis_view::convo::{ApprovalState, Item};
use synthesis_view::toolview::{self, Card, Glyph, Tone};

use super::chat::md;
use super::kit::{self, ButtonKind, text};
use super::tokens::{self as t, hsla};
use crate::live::Live;
use crate::tui_view::TuiView;
use crate::tui_view::chat::{tone_bg, tone_fg};

use model::{Block, Turn};

/// Widest the conversation grows: a calm reading column, centred.
const COLUMN_W: f32 = 760.;
/// Rows of a tool body drawn when it is open (the card counts the rest).
const BODY_ROWS: usize = 120;

/// The chat of one session frame.
pub struct ChatHost {
    tui: Entity<TuiView>,
    scroll: ScrollHandle,
    dlg_scroll: ScrollHandle,
    body_scroll: ScrollHandle,
    focus: FocusHandle,
    /// Tool cards the person toggled (by tool call id): a failed call starts open, the rest closed.
    toggled: HashSet<String>,
    /// The transcript follows its end until the person scrolls up.
    stick: bool,
    /// What the last draw followed: the items and the length of the last one.
    seen: (usize, usize),
    _subs: Vec<Subscription>,
}

impl ChatHost {
    pub fn new(tui: Entity<TuiView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if let Some(p) = tui.read(cx).embed_prompt() {
            p.update(cx, |s, cx| {
                s.set_placeholder("Message Synthesis", window, cx)
            });
        }
        let subs = vec![
            // the engine, a dialog or the gate changed: draw again; a line handed back unsent returns to the box
            cx.observe_in(&tui, window, |this, tui, window, cx| {
                tui.update(cx, |t, cx| t.embed_restore_prompt(window, cx));
                let _ = this;
                cx.notify();
            }),
        ];
        Self {
            tui,
            scroll: ScrollHandle::new(),
            dlg_scroll: ScrollHandle::new(),
            body_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            toggled: HashSet::new(),
            stick: true,
            seen: (0, 0),
            _subs: subs,
        }
    }

    /// The Send button (Enter in the box does the same through the embedded view's own subscription).
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stick = true;
        self.tui.update(cx, |t, cx| t.embed_submit(window, cx));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.tui.read(cx).embed_interrupt();
        cx.notify();
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.toggled.remove(id) {
            self.toggled.insert(id.to_string());
        }
        cx.notify();
    }

    // ------------------------------------------------------------------------------------------ pieces

    fn mono(&self, cx: &App) -> SharedString {
        cx.theme().mono_font_family.clone()
    }

    fn assistant_text(
        &self,
        at: usize,
        body: &str,
        (streaming, tainted): (bool, bool),
        cx: &App,
    ) -> AnyElement {
        let id = format!("sa-asst-{at}");
        // the code of a fenced block is mono (it inherits), the prose around it keeps the UI font
        let mut prose = |i: usize, s: String| {
            div()
                .font_family(crate::brand::UI_FONT)
                .child(md(SharedString::from(format!("{id}-{i}")), &s))
                .into_any_element()
        };
        let blocks = crate::tui_view::run_block::embed_blocks(
            &self.tui,
            &id,
            body,
            (streaming, tainted),
            &mut prose,
            cx,
        );
        let inner = match blocks {
            Some(b) => div().font_family(self.mono(cx)).child(b).into_any_element(),
            None => md(SharedString::from(id.clone()), body).into_any_element(),
        };
        div()
            .debug_selector(|| "sa-chat-text".to_string())
            .min_w(px(0.))
            .text_size(px(t::T3.0))
            .line_height(px(t::T3.1))
            .text_color(hsla(t::TEXT_STRONG))
            .child(inner)
            .into_any_element()
    }

    /// A tool call: one header row; a click opens its body (what `toolview::shape` made of the call, diffs coloured).
    fn tool_card(&self, at: usize, it: &Item, cx: &Context<Self>) -> AnyElement {
        let Item::Tool { id, name, .. } = it else {
            return div().into_any_element();
        };
        thread_local! {
            static CARDS: std::cell::RefCell<toolview::CardCache> = std::cell::RefCell::default();
        }
        let shape = |open: bool| CARDS.with(|c| c.borrow_mut().get(it, open, 100));
        let Some(closed) = shape(false) else {
            return div().into_any_element();
        };
        let toggled = self.toggled.contains(id);
        let open = toggled != (closed.glyph == Glyph::Error);
        let card = if open {
            shape(true).unwrap_or(closed)
        } else {
            closed
        };
        let (mark, c) = glyph_look(card.glyph);
        let tid = id.clone();
        let mut head = div()
            .id(("sa-tool-head", at))
            .debug_selector(move || format!("sa-tool-head-{at}"))
            .flex()
            .items_center()
            .gap(px(t::S2))
            .px(px(t::S3))
            .py(px(t::S1 + 2.))
            .cursor_pointer()
            .hover(|d| d.bg(t::hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(&tid, cx)))
            .child(
                text(t::T2, t::W_MEDIUM, t::TEXT_STRONG)
                    .flex_none()
                    .child(sanitize_for_display(name)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .gap(px(t::S2))
                    .overflow_hidden()
                    .child(
                        text(t::T2, t::W_REGULAR, t::TEXT)
                            .truncate()
                            .child(card.head.clone()),
                    )
                    .when(!card.sub.is_empty(), |d| {
                        d.child(
                            text(t::T2, t::W_REGULAR, t::TEXT)
                                .truncate()
                                .opacity(0.7)
                                .child(card.sub.clone()),
                        )
                    }),
            )
            .children(card.badges.iter().map(|b| {
                div()
                    .flex_none()
                    .text_size(px(t::T2.0))
                    .text_color(tone_fg(b.tone))
                    .child(b.text.clone())
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(t::T2.0))
                    .text_color(c)
                    .child(mark),
            );
        if card.collapsible || !card.rows.is_empty() {
            head = head.child(
                text(t::T2, t::W_REGULAR, t::TEXT)
                    .flex_none()
                    .child(if open { "\u{2304}" } else { "\u{203a}" }),
            );
        }
        let body = open.then(|| self.tool_body(at, &card, cx));
        div()
            .debug_selector(move || format!("sa-tool-{at}"))
            .rounded(px(t::R_CONTROL))
            .border_1()
            .border_color(t::stroke())
            .bg(hsla(t::SURFACE))
            .overflow_hidden()
            .child(head)
            .children(body)
            .into_any_element()
    }

    fn tool_body(&self, at: usize, card: &Card, cx: &App) -> AnyElement {
        let gut = card.gutter as f32 * 7.5 + 8.;
        div()
            .id(("sa-tool-body", at))
            .debug_selector(move || format!("sa-tool-body-{at}"))
            .border_t_1()
            .border_color(t::stroke())
            .bg(t::well())
            .py(px(t::S1))
            .font_family(self.mono(cx))
            .text_size(px(12.))
            .line_height(px(18.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .children(card.rows.iter().take(BODY_ROWS).map(|r| {
                let row = div()
                    .flex()
                    .px(px(t::S3))
                    .text_color(tone_fg(r.tone))
                    .when_some(tone_bg(r.tone), |d, bg| d.bg(bg));
                let no = r.no.map(|n| n.to_string()).unwrap_or_default();
                row.when(card.gutter > 0, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .w(px(gut))
                            .text_color(tone_fg(Tone::Dim))
                            .child(no),
                    )
                })
                .child(
                    div()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .child(if r.text.is_empty() {
                            " ".into()
                        } else {
                            r.text.clone()
                        }),
                )
            }))
            .children((card.hidden > 0 || card.rows.len() > BODY_ROWS).then(|| {
                text(t::T1, t::W_REGULAR, t::TEXT)
                    .px(px(t::S3))
                    .child(format!(
                        "+{} more line{}",
                        card.hidden + card.rows.len().saturating_sub(BODY_ROWS),
                        if card.hidden + card.rows.len().saturating_sub(BODY_ROWS) == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ))
            }))
            .into_any_element()
    }

    fn quiet(&self, s: &str, warn: bool) -> AnyElement {
        text(
            t::T2,
            t::W_REGULAR,
            if warn { t::SOFT_BEIGE } else { t::TEXT },
        )
        .debug_selector(|| "sa-chat-note".to_string())
        .whitespace_normal()
        .child(s.to_string())
        .into_any_element()
    }

    fn blocks(&self, blocks: &[Block], items: &[Item], cx: &Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().gap(px(t::S2)).min_w(px(0.));
        for b in blocks {
            col = col.child(match b {
                Block::Text {
                    at,
                    text,
                    streaming,
                    tainted,
                } => self.assistant_text(*at, text, (*streaming, *tainted), cx),
                Block::Tool { at } => self.tool_card(*at, &items[*at], cx),
                Block::Thinking { text, .. } => text_el(text, t::TEXT, true),
                Block::Ask { state, prompt, .. } => {
                    let line = format!("{}: {}", model::ask_word(*state), clip(prompt));
                    self.quiet(&line, *state == ApprovalState::Pending)
                }
                Block::Note { warning, text } => self.quiet(&clip(text), *warning),
                Block::End { ok, text } => self.quiet(&clip(text), !*ok),
            });
        }
        col.into_any_element()
    }

    fn transcript(&self, live: &Live, cx: &Context<Self>) -> Vec<AnyElement> {
        let items = &live.convo().items;
        let skip = items.len().saturating_sub(model::MAX_ITEMS);
        let mut out: Vec<AnyElement> = vec![];
        if skip > 0 {
            out.push(self.quiet(&format!("{skip} earlier items are not shown here"), false));
        }
        for turn in model::turns(items, skip, live.show_thinking, live.verbose) {
            out.push(match turn {
                Turn::User { at, text } => user_bubble(at, &text),
                Turn::Assistant { mut blocks } => {
                    // `/focus`: no tool, thinking or approval rows
                    if live.focus_view {
                        blocks.retain(|b| {
                            !matches!(
                                b,
                                Block::Tool { .. } | Block::Thinking { .. } | Block::Ask { .. }
                            )
                        });
                    }
                    self.blocks(&blocks, items, cx)
                }
            });
        }
        out
    }

    /// The front dialog (permission, plan, question, trust): the shared card, answered through the embedded view.
    fn dialog(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let tui = self.tui.clone();
        let live = self.tui.read(cx).embed_live()?;
        let t1 = tui.clone();
        let t2 = tui.clone();
        let win_h = f32::from(window.viewport_size().height);
        let card = crate::widgets::dialog::dock(
            live,
            (&self.dlg_scroll, &self.body_scroll),
            &move |i| {
                let tui = t1.clone();
                Box::new(move |_, _, cx| tui.update(cx, |t, cx| t.embed_pick(i, cx)))
            },
            &move |g| {
                let tui = t2.clone();
                Box::new(move |_, _, cx| tui.update(cx, |t, cx| t.embed_toggle(g, cx)))
            },
            hsla(t::ACCENT),
            ((win_h * 0.3).clamp(120., 320.), win_h),
            (
                Box::new(move |_, _, cx| tui.update(cx, |t, cx| t.embed_engage(cx))),
                false,
            ),
        )?;
        Some(
            div()
                .debug_selector(|| "sa-chat-dialog".to_string())
                .font_family(self.mono(cx))
                .text_size(px(12.))
                .child(card)
                .into_any_element(),
        )
    }

    fn composer_el(&self, live: &Live, cx: &Context<Self>) -> AnyElement {
        let running = live.busy || live.convo().running;
        let model_name = {
            let m = sanitize_for_display(&live.view.status.model);
            if m.is_empty() { "model".to_string() } else { m }
        };
        let mode = {
            let m = sanitize_for_display(&live.view.status.mode);
            format!("{} mode", if m.is_empty() { "default" } else { &m })
        };
        let starting = live.starting;
        let input = self
            .tui
            .read(cx)
            .embed_prompt()
            .map(|p| {
                Textarea::new(&p)
                    .appearance(false)
                    .bordered(false)
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element());
        // switching the model is `/model` in the box; the mode pill cycles like Shift+Tab
        let left = vec![
            pill("sa-chat-model", &model_name).into_any_element(),
            pill("sa-chat-mode", &mode)
                .cursor_pointer()
                .hover(|d| d.bg(t::hover()))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.tui.update(cx, |t, cx| t.embed_cycle_mode(cx))
                }))
                .into_any_element(),
        ];
        let action = if running {
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
                    cx.listener(|this, _, window, cx| this.send(window, cx)),
                )),
            )
        };
        let card = composer_card(input, left, action.into_any_element());
        // the attached images (the TUI's chips); `/images clear` and the x drop them
        let chips = (!live.images.is_empty()).then(|| {
            div()
                .debug_selector(|| "sa-chat-images".to_string())
                .flex()
                .items_center()
                .gap(px(t::S2))
                .px(px(t::S1))
                .children(live.images.iter().enumerate().map(|(i, a)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(t::S1))
                        .px(px(t::S2))
                        .rounded(px(t::R_PILL))
                        .border_1()
                        .border_color(t::stroke())
                        .text_size(px(t::T2.0))
                        .text_color(hsla(t::TEXT_STRONG))
                        .child(format!("{} {:.1} MB", a.name, a.bytes as f64 / 1e6))
                        .child(
                            div()
                                .id(("sa-unattach", i))
                                .debug_selector(move || format!("sa-unattach-{i}"))
                                .cursor_pointer()
                                .text_color(hsla(t::TEXT))
                                .hover(|d| d.text_color(hsla(t::TEXT_STRONG)))
                                .child("\u{d7}")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.tui.update(cx, |t, cx| t.embed_unattach(i, cx))
                                })),
                        )
                }))
        });
        let card = div()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .children(chips)
            .child(card)
            .into_any_element();
        let Some(pop) = self.completion_el(cx) else {
            return card;
        };
        div()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .child(pop)
            .child(card)
            .into_any_element()
    }

    /// The `/command` and `@file` suggestions of the prompt (the TUI's own list, keys included), above the card.
    fn completion_el(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let tui = self.tui.read(cx);
        let (items, sel) = tui.embed_completion()?;
        let rows = items.iter().enumerate().map(|(i, (label, detail))| {
            div()
                .flex()
                .gap(px(t::S3))
                .px(px(t::S2))
                .rounded(px(t::R_PILL))
                .when(i == sel, |d| d.bg(t::hover()))
                .child(
                    div()
                        .flex_none()
                        .text_color(hsla(if i == sel { t::TEXT_STRONG } else { t::TEXT }))
                        .child(sanitize_for_display(label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(hsla(t::TEXT))
                        .child(sanitize_for_display(detail)),
                )
        });
        Some(
            div()
                .debug_selector(|| "sa-chat-completion".to_string())
                .p(px(t::S1))
                .rounded(px(t::R_PANEL))
                .bg(hsla(t::ELEVATED))
                .border_1()
                .border_color(t::stroke())
                .text_size(px(t::T2.0))
                .children(rows)
                .into_any_element(),
        )
    }
}

/// The person's line: a bubble on the right of the reading column (the session chat and the orchestrator chat).
pub(crate) fn user_bubble(at: usize, line: &str) -> AnyElement {
    div()
        .id(("sa-user", at))
        .debug_selector(|| "sa-chat-user".to_string())
        .self_end()
        .max_w(gpui::relative(0.8))
        .px(px(t::S3))
        .py(px(t::S2))
        .rounded(px(t::R_PANEL))
        .bg(hsla(t::SURFACE))
        .border_1()
        .border_color(t::stroke())
        .text_size(px(t::T3.0))
        .line_height(px(t::T3.1))
        .text_color(hsla(t::TEXT_STRONG))
        .whitespace_normal()
        .child(line.to_string())
        .into_any_element()
}

/// A small outlined pill (the model and the mode of the composer, the orchestrator's target).
pub(crate) fn pill(id: &'static str, label: &str) -> gpui::Stateful<gpui::Div> {
    text(t::T2, t::W_REGULAR, t::TEXT)
        .id(id)
        .debug_selector(move || id.to_string())
        .px(px(t::S3))
        .py(px(2.))
        .rounded(px(t::R_PILL))
        .border_1()
        .border_color(t::stroke())
        .max_w(px(260.))
        .truncate()
        .child(label.to_string())
}

/// The composer: the text box over a bar with the pills on the left and the send or stop button on the right.
pub(crate) fn composer_card(
    input: AnyElement,
    left: Vec<AnyElement>,
    action: AnyElement,
) -> AnyElement {
    div()
        .debug_selector(|| "sa-chat-composer".to_string())
        .flex()
        .flex_col()
        .gap(px(t::S2))
        .p(px(t::S3))
        .rounded(px(t::R_FLOAT))
        .bg(hsla(t::ELEVATED))
        .border_1()
        .border_color(t::stroke())
        .child(div().px(px(t::S1)).child(input))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(t::S2))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(t::S2))
                        .min_w(px(0.))
                        .children(left),
                )
                .child(action),
        )
        .into_any_element()
}

impl Focusable for ChatHost {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChatHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tui = self.tui.clone();
        // the follow rule reads the last draw's scroll position
        let (max, off) = (
            f32::from(self.scroll.max_offset().y),
            f32::from(self.scroll.offset().y),
        );
        self.stick = max + off <= 8.;
        tui.read(cx).embed_note_shape(
            -off,
            max,
            self.seen.0,
            f32::from(window.viewport_size().width),
        );
        let Some(live) = tui.read(cx).embed_live() else {
            return div().size_full().into_any_element();
        };
        let rows = self.transcript(live, cx);
        let status = model::status_line(
            live.starting,
            live.busy || live.convo().running,
            live.dialogs.len(),
            live.queue.len(),
            &live.convo().session_status,
        );
        let sig = (
            live.convo().items.len(),
            live.convo().items.last().map_or(0, |i| match i {
                Item::Assistant { text, .. } => text.len(),
                Item::Tool { output, .. } => output.len(),
                _ => 0,
            }),
        );
        if self.stick && (sig != self.seen || self.seen == (0, 0)) {
            self.scroll.scroll_to_bottom();
        }
        self.seen = sig;
        let composer = self.composer_el(live, cx);
        let dialog = self.dialog(window, cx);
        let empty = rows.is_empty();
        div()
            .id("sa-chat-host")
            .debug_selector(|| "sa-chat-host".to_string())
            .track_focus(&self.focus)
            // an image file dropped on the chat is attached by the embedded view (the TUI's own rules)
            .on_drop(cx.listener(|this, p: &gpui::ExternalPaths, _, cx| {
                this.tui.update(cx, |t, cx| t.drop_files(p.paths(), cx))
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                // Esc stops the running turn (a dialog's Esc is taken by the embedded view first)
                if ev.keystroke.key == "escape" {
                    this.stop(cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .id("sa-chat-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .mx_auto()
                            .max_w(px(COLUMN_W))
                            .flex()
                            .flex_col()
                            .gap(px(t::S5))
                            .px(px(t::S4))
                            .pt(px(t::S5))
                            .pb(px(t::S3))
                            .when(empty, |d| {
                                d.child(
                                    text(t::T3, t::W_REGULAR, t::TEXT)
                                        .debug_selector(|| "sa-chat-empty".to_string())
                                        .child("Ask for something to start. Enter sends, Shift+Enter adds a line."),
                                )
                            })
                            .children(rows),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN_W))
                    .px(px(t::S4))
                    .pb(px(t::S4))
                    .flex()
                    .flex_col()
                    .gap(px(t::S2))
                    .children(status.map(|s| {
                        text(t::T2, t::W_REGULAR, t::TEXT)
                            .debug_selector(|| "sa-chat-status".to_string())
                            .px(px(t::S1))
                            .child(s)
                    }))
                    .children(dialog)
                    .child(composer),
            )
            .into_any_element()
    }
}

fn glyph_look(g: Glyph) -> (&'static str, gpui::Hsla) {
    match g {
        Glyph::Running => (g.mark(), tone_fg(Tone::Warn)),
        Glyph::Ok => (g.mark(), tone_fg(Tone::Ok)),
        Glyph::Error => (g.mark(), tone_fg(Tone::Err)),
        Glyph::Denied | Glyph::Interrupted => (g.mark(), hsla(t::TEXT)),
    }
}

fn text_el(s: &str, c: u32, dim: bool) -> AnyElement {
    text(t::T2, t::W_REGULAR, c)
        .whitespace_normal()
        .when(dim, |d| d.opacity(0.75))
        .child(clip(s))
        .into_any_element()
}

fn clip(s: &str) -> String {
    const MAX: usize = 400;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(MAX).collect::<String>())
    }
}
