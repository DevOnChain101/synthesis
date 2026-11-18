//! The TUI view's dialog hooks: the card itself is `widgets::dialog`, shared with the Super Agent view.

use gpui::{AnyElement, Context};

use super::TuiView;
use crate::live::now_ms;

impl TuiView {
    /// The front dialog as a panel above the prompt, or `None`. Only where it is drawn changed from the
    /// modal: the keys, the arming delay and the answers are `Live`'s and `on_key`'s as before.
    pub(super) fn dialog_dock(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let lu = self.live.as_ref()?;
        let border = self
            .attention_colour()
            .unwrap_or_else(|| crate::theme::hsla(synthesis_cli_ui_render::theme::ACCENT));
        // Enter is armed: nothing in the prompt, the first frame drawn (`refresh`) and the arming delay over
        let enter_ready = lu.live.dialogs.first().is_some_and(|d| {
            !self.auto_ready.is_empty()
                && self.auto_ready == d.ready_id()
                && now_ms() >= d.armed_until
                && lu.prompt.read(cx).value().is_empty()
        });
        crate::widgets::dialog::dock(
            &lu.live,
            (&lu.dlg_scroll, &lu.body_scroll),
            &|i| Box::new(cx.listener(move |this, _, _, cx| this.click_option(i, cx))),
            &|g| Box::new(cx.listener(move |this, _, _, cx| this.rewind_toggle(g, cx))),
            border,
            ((self.win_h * 0.4).clamp(150., 420.), self.win_h),
            // a press on the panel engages it: Enter and digits answer from then on
            (
                Box::new(cx.listener(|this, _, _, cx| {
                    if let Some(lu) = &mut this.live
                        && let Some(d) = lu.live.dialogs.first_mut()
                    {
                        d.engaged = true;
                        lu.dirty = true;
                    }
                    cx.notify();
                })),
                enter_ready,
            ),
        )
    }

    /// Whether a dialog waits for the person (and whether it blinks, per `cfg.tui.motion`).
    pub fn attention(&self) -> Option<bool> {
        let lu = self.live.as_ref()?;
        lu.live
            .dialogs
            .iter()
            .any(|d| d.needs_person())
            .then_some(lu.cfg.tui.motion)
    }

    /// The red of the "needs your approval" signal right now, while one is awaited.
    pub fn attention_colour(&self) -> Option<gpui::Hsla> {
        self.attention()
            .map(|motion| crate::widgets::chrome::attention_red(motion, now_ms()))
    }

    /// Sessions waiting for a tool permission (peers left out) as `(key, title)`: only the Super Agent badge and
    /// its Alt+A read this, the TUI view draws nothing of it. The key is the session id, or `#index` without one.
    pub fn waiting_tools(&self) -> Vec<(String, String)> {
        self.snap
            .running
            .iter()
            .enumerate()
            .filter(|(_, r)| r.state == super::activity::RunState::Approval && !r.peer)
            .map(|(i, r)| {
                let key = if r.id.is_empty() {
                    format!("#{i}")
                } else {
                    r.id.clone()
                };
                (key, synthesis_harness::sanitize_for_display(&r.title))
            })
            .collect()
    }

    /// A click on option `i` of the front dialog.
    pub(super) fn click_option(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let now = now_ms();
        let action = lu.live.pick(i, now).and_then(|k| lu.live.answer(&k, now));
        lu.dirty = true;
        cx.notify();
        if let Some(a) = action {
            self.run_action(a, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TuiView, activity::RunState, layout::Hidden};

    #[test]
    fn waiting_tools_are_the_sessions_that_wait_and_show_bidi_controls_escaped() {
        let mut t = TuiView::new(0., Hidden::default());
        assert!(
            t.snap.running.iter().all(|r| r.state != RunState::Approval),
            "the sample has none waiting"
        );
        assert!(t.waiting_tools().is_empty());
        let bidi = "\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
        t.snap.running[0].state = RunState::Approval;
        t.snap.running[0].peer = false;
        t.snap.running[0].title = format!("a{bidi}b");
        let rows = t.waiting_tools();
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].1.chars().any(|c| bidi.contains(c)),
            "raw bidi control in {:?}",
            rows[0].1
        );
        assert!(rows[0].1.contains("\\u{202e}"));
        // a peer's session is not this window's to answer
        t.snap.running[0].peer = true;
        assert!(t.waiting_tools().is_empty());
    }
}
