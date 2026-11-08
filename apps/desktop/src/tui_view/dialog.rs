//! The TUI view's dialog hooks: the card itself is `widgets::dialog`, shared with the Agent view.

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
            .unwrap_or_else(|| crate::theme::hsla(synthesis_tui_render::theme::ACCENT));
        crate::widgets::dialog::dock(
            &lu.live,
            &lu.dlg_scroll,
            &|i| Box::new(cx.listener(move |this, _, _, cx| this.click_option(i, cx))),
            border,
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
