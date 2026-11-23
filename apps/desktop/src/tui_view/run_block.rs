//! The fenced blocks of an assistant message in the chat: the TUI view's side of `widgets::codeblock`. Copy is
//! the widget's own; Run in terminal is decided here: a real click through `codeblock::Gate` (armed after the
//! block is drawn whole, a second click for a multi-line or risky command), and only then the command goes to
//! the person's shell. The terminal's output never goes back to the engine.

use gpui::{AnyElement, ClickEvent, Context, IntoElement as _};
use gpui_component::text::TextView;
use synthesis_view::codeblock::{self as cb, Need, Press};

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

    /// An assistant message: its prose as markdown and each fenced block as its own section. `None` when the text
    /// has no fenced block (it keeps its plain markdown).
    pub(super) fn assistant_blocks(
        &self,
        idx: usize,
        text: &str,
        streaming: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let segs = cb::split(text);
        if !ui::has_blocks(&segs) {
            return None;
        }
        let now = self.gate_ms();
        let blocked = self.run_blocked();
        let mut prose = |i: usize, s: String| {
            TextView::markdown(gpui::SharedString::from(format!("assistant-{idx}-{i}")), s)
                .style(chat::table_head_style())
                .selectable(true)
                .into_any_element()
        };
        let mut run = |_: usize, b: &cb::Block, cmd: &str| {
            let k = cb::key(cmd);
            self.gate.borrow_mut().drawn(k, now, b.closed && !streaming);
            if let Some(why) = blocked {
                return Some(RunUi {
                    label: format!("Run unavailable: {why}").into(),
                    warn: false,
                    on_click: Box::new(|_, _, _| {}),
                });
            }
            let confirming = self.gate.borrow().confirming(k, now);
            let need = Need::of(cmd).filter(|_| confirming);
            let cmd = cmd.to_string();
            Some(RunUi {
                label: need.map_or("Run in terminal".into(), |n| n.label().into()),
                warn: need.is_some(),
                on_click: Box::new(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.run_command(&cmd, streaming, ev, cx)
                })),
            })
        };
        Some(
            ui::message(&format!("assistant-{idx}"), &segs, &mut prose, &mut run)
                .into_any_element(),
        )
    }

    /// A click on Run of a command block (the gate decides; see `codeblock::Gate`).
    fn run_command(&mut self, cmd: &str, streaming: bool, ev: &ClickEvent, cx: &mut Context<Self>) {
        let ctx = cb::Ctx {
            // a key press on a focused button, a double click and a touch long press are not a decision
            mouse: matches!(ev, ClickEvent::Mouse(_))
                && ev.standard_click()
                && ev.click_count() == 1,
            streaming,
            // blocks are drawn for assistant messages only, never for a tool result or a quoted file
            tainted: false,
            runnable: self.run_blocked().is_none(),
        };
        let now = self.gate_ms();
        let press = self.gate.borrow_mut().press(cmd, ctx, now);
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
