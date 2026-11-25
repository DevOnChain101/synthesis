//! The fenced blocks of an assistant message in the chat: the TUI view's side of `widgets::codeblock`. Copy is
//! the widget's own; Run in terminal is decided here: a real click through `codeblock::Gate` (armed after the
//! block is drawn whole, a second click for a multi-line or risky command), and only then the command goes to
//! the person's shell. The terminal's output never goes back to the engine.

use std::hash::{Hash as _, Hasher as _};

use gpui::{AnyElement, ClickEvent, Context, IntoElement as _};
use synthesis_view::codeblock::{self as cb, Need, Press};
use synthesis_view::convo::Item;

use super::{ChatTab, TuiView, WantTerminal, chat};
use crate::widgets::codeblock::{self as ui, RunUi};

impl TuiView {
    fn gate_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Why Run cannot open a shell here (`None`: it can).
    fn run_blocked(&self) -> Option<&'static str> {
        let Some(e) = self.live.as_ref().and_then(|l| l.live.engine.as_ref()) else {
            return Some("session starting");
        };
        (e.is_remote() && !(e.remote_distro.is_some() && e.remote_cwd.is_some()))
            .then_some("no shell for this engine")
    }

    /// Nothing streams anywhere in the chat and no turn runs.
    fn chat_quiet(&self) -> bool {
        self.live.as_ref().is_some_and(|l| {
            let c = &l.live.view.convo;
            !c.running
                && !c.items.iter().any(|i| {
                    matches!(
                        i,
                        Item::Assistant {
                            streaming: true,
                            ..
                        }
                    )
                })
        })
    }

    /// The transcript was drawn with this scroll offset, height, row count and width (see `Gate::layout`).
    pub(super) fn note_chat_shape(&self, at: f32, max: f32, rows: usize) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            at.round() as i64,
            max.round() as i64,
            rows,
            self.centre_w.round() as i64,
        )
            .hash(&mut h);
        self.gate.borrow_mut().layout(h.finish(), self.gate_ms());
    }

    /// An assistant message: its prose as markdown and each fenced block as its own section. `None` when the text
    /// has no fenced block (it keeps its plain markdown). `tainted`: the message follows web, browser or MCP
    /// content, so every command asks twice.
    pub(super) fn assistant_blocks(
        &self,
        idx: usize,
        text: &str,
        streaming: bool,
        tainted: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let segs = cb::split(text);
        if !ui::has_blocks(&segs) {
            return None;
        }
        let now = self.gate_ms();
        let blocked = self.run_blocked();
        let bracketed = self.term.as_ref().and_then(|p| p.read(cx).bracketed());
        let mut prose = |i: usize, s: String| {
            chat::AssistantMd::new(gpui::SharedString::from(format!("assistant-{idx}-{i}")), s)
                .into_any_element()
        };
        let mut run = |_: usize, b: &cb::Block, cmd: &str| {
            let k = cb::key(cmd);
            self.gate.borrow_mut().drawn(k, now, b.closed && !streaming);
            let off = |why: &str| RunUi {
                label: format!("Run unavailable: {why}").into(),
                warn: false,
                on_click: Box::new(|_, _, _| {}),
                on_hover: None,
            };
            if let Some(why) = blocked {
                return Some(off(why));
            }
            // a many-line command goes as one bracketed paste; a shell that does not take it gets Copy only
            if cmd.contains('\n') && bracketed == Some(false) {
                return Some(off("this shell has no bracketed paste"));
            }
            let need = self
                .gate
                .borrow()
                .confirming(k, now)
                .then_some(Need::of(cmd, tainted));
            let (cmd, block) = (cmd.to_string(), b.clone());
            let this = cx.entity();
            Some(RunUi {
                label: need.map_or("Run in terminal".into(), |n| n.label().into()),
                warn: need.is_some_and(Need::warn),
                on_click: Box::new(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.run_command(&block, &cmd, (streaming, tainted, true), ev, cx)
                })),
                on_hover: Some(Box::new(move |on, _, cx| {
                    this.update(cx, |t, _| {
                        let now = t.gate_ms();
                        t.gate.borrow_mut().hovered(k, *on, now)
                    })
                })),
            })
        };
        Some(
            ui::message(
                &format!("assistant-{idx}"),
                &segs,
                tainted,
                &mut prose,
                &mut run,
            )
            .into_any_element(),
        )
    }

    /// A click on Run of a command block (the gate decides; see `codeblock::Gate`).
    fn run_command(
        &mut self,
        block: &cb::Block,
        cmd: &str,
        (streaming, tainted, owner_quiet): (bool, bool, bool),
        ev: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        let ctx = cb::Ctx {
            // a key press on a focused button, a double click and a touch long press are not a decision
            mouse: matches!(ev, ClickEvent::Mouse(_))
                && ev.standard_click()
                && ev.click_count() == 1,
            streaming,
            quiet: owner_quiet && self.chat_quiet(),
            tainted,
            runnable: self.run_blocked().is_none(),
        };
        let now = self.gate_ms();
        let press = self.gate.borrow_mut().press(block, cmd, ctx, now);
        match press {
            Press::Run => self.send_to_terminal(cmd.to_string(), cx),
            Press::Confirm(_) => cx.notify(),
            Press::Ignored => {}
        }
    }

    /// Shows the Terminal (the tab here, the sidebar pane when embedded) and types `cmd` into the current shell.
    fn send_to_terminal(&mut self, cmd: String, cx: &mut Context<Self>) {
        if self.embed.is_some() {
            self.embed_side(Some(ChatTab::Terminal), cx);
            cx.emit(WantTerminal);
        } else {
            self.select_tab(ChatTab::Terminal, cx);
        }
        let pane = self.ensure_term(cx);
        if !pane.read(cx).tried || pane.read(cx).tabs().is_empty() {
            self.term_new(cx);
        }
        pane.update(cx, |p, cx| p.run(cmd, cx));
    }

    /// The host's transcript was drawn with this scroll offset, height, row count and width (see `Gate::layout`).
    pub(crate) fn embed_note_shape(&self, at: f32, max: f32, rows: usize, width: f32) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            at.round() as i64,
            max.round() as i64,
            rows,
            width.round() as i64,
        )
            .hash(&mut h);
        self.gate.borrow_mut().layout(h.finish(), self.gate_ms());
    }
}

/// An assistant message of the Super Agent's own chat: the same fenced sections and the same gate as
/// [`TuiView::assistant_blocks`], with the click handlers bound to the embedded view `this` (which owns the shell
/// and the gate). `prose` draws a stretch of prose. `None` when the text has no fenced block.
pub(crate) fn embed_blocks(
    this: &gpui::Entity<TuiView>,
    id: &str,
    text: &str,
    flags: (bool, bool),
    prose: &mut dyn FnMut(usize, String) -> AnyElement,
    cx: &gpui::App,
) -> Option<AnyElement> {
    embed_blocks_in(true, this, id, text, flags, prose, cx)
}

/// [`embed_blocks`] for an owner whose own chat must be still too: `quiet` false (its turn runs, its reply
/// streams) keeps every click from counting, whatever the session's chat does.
pub(crate) fn embed_blocks_in(
    quiet: bool,
    this: &gpui::Entity<TuiView>,
    id: &str,
    text: &str,
    (streaming, tainted): (bool, bool),
    prose: &mut dyn FnMut(usize, String) -> AnyElement,
    cx: &gpui::App,
) -> Option<AnyElement> {
    let segs = cb::split(text);
    if !ui::has_blocks(&segs) {
        return None;
    }
    let (now, blocked, bracketed) = {
        let t = this.read(cx);
        (
            t.gate_ms(),
            t.run_blocked(),
            t.term.as_ref().and_then(|p| p.read(cx).bracketed()),
        )
    };
    let mut run = |_: usize, b: &cb::Block, cmd: &str| {
        let k = cb::key(cmd);
        let t = this.read(cx);
        t.gate.borrow_mut().drawn(k, now, b.closed && !streaming);
        let off = |why: &str| RunUi {
            label: format!("Run unavailable: {why}").into(),
            warn: false,
            on_click: Box::new(|_, _, _| {}),
            on_hover: None,
        };
        if let Some(why) = blocked {
            return Some(off(why));
        }
        if cmd.contains('\n') && bracketed == Some(false) {
            return Some(off("this shell has no bracketed paste"));
        }
        let need = t
            .gate
            .borrow()
            .confirming(k, now)
            .then_some(Need::of(cmd, tainted));
        let (cmd, block) = (cmd.to_string(), b.clone());
        let (click, hover) = (this.clone(), this.clone());
        Some(RunUi {
            label: need.map_or("Run in terminal".into(), |n| n.label().into()),
            warn: need.is_some_and(Need::warn),
            on_click: Box::new(move |ev, _, cx| {
                click.update(cx, |t, cx| {
                    t.run_command(&block, &cmd, (streaming, tainted, quiet), ev, cx)
                })
            }),
            on_hover: Some(Box::new(move |on, _, cx| {
                hover.update(cx, |t, _| {
                    let now = t.gate_ms();
                    t.gate.borrow_mut().hovered(k, *on, now)
                })
            })),
        })
    };
    Some(ui::message(id, &segs, tainted, prose, &mut run).into_any_element())
}

#[cfg(test)]
mod tests {
    /// Run reaches a shell from one place only (the click handler through the gate), and nothing here hands
    /// terminal output or the command to the engine.
    #[test]
    fn a_command_reaches_the_shell_only_through_the_click_and_never_the_engine() {
        let me = include_str!("run_block.rs");
        let code = &me[..me.find("#[cfg(test)]").unwrap()];
        assert_eq!(code.matches("self.send_to_terminal(").count(), 1);
        assert_eq!(code.matches("p.run(cmd, cx)").count(), 1);
        assert!(code.contains("gate.borrow_mut().press("));
        for banned in [
            "UiMsg",
            "submit",
            "harness",
            "read_from_clipboard",
            "engine.send",
        ] {
            assert!(!code.contains(banned), "{banned}");
        }
        for (name, src) in [
            ("chat.rs", include_str!("chat.rs")),
            ("driver.rs", include_str!("driver.rs")),
            ("tabs.rs", include_str!("tabs.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("term_pane.rs", include_str!("term_pane.rs")),
        ] {
            assert!(
                !src.contains("send_to_terminal(") && !src.contains(".run_command("),
                "{name}"
            );
        }
    }
}
