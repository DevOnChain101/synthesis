//! The TUI view's dialog hooks: the card itself is `widgets::dialog`, shared with the Agent view.

use gpui::{AnyElement, Context};

use super::TuiView;
use crate::live::now_ms;

impl TuiView {
    /// The front dialog over the whole window, or `None`.
    pub(super) fn dialog_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let lu = self.live.as_ref()?;
        crate::widgets::dialog::overlay(&lu.live, &lu.dlg_scroll, &|i| {
            Box::new(cx.listener(move |this, _, _, cx| this.click_option(i, cx)))
        })
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
