//! What the Super Agent's `ChatHost` takes from an embedded view that it does not draw: the engine's state
//! (`Live`, the same one the TUI view draws, so approvals, asks, masking and the session rules are not
//! re-implemented), a line sent through the same rules as the TUI prompt, a click on a dialog option and the mode.

use gpui::{Context, Window};
use gpui_component::input::TextareaState;

use super::TuiView;
use crate::live::Live;

impl TuiView {
    /// The engine state the host draws from.
    pub(crate) fn embed_live(&self) -> Option<&Live> {
        self.live.as_ref().map(|l| &l.live)
    }

    /// The prompt box: the host draws it as its composer, so Enter, focus, paste and the send rules (held while the
    /// engine starts, queued while a task runs, `/commands`) are the TUI prompt's own, and every refocus of the
    /// view's own code lands in the box the person sees.
    pub(crate) fn embed_prompt(&self) -> Option<gpui::Entity<TextareaState>> {
        self.live.as_ref().map(|l| l.prompt.clone())
    }

    /// The Send button: what Enter does.
    pub(crate) fn embed_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_prompt(false, window, cx);
    }

    /// A line the engine handed back unsent (a refused start): it returns to the box, as the TUI view's draw does.
    pub(crate) fn embed_restore_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self
            .live
            .as_mut()
            .and_then(|l| l.live.returned_prompt.take())
        {
            self.set_prompt(&t, window, cx);
        }
    }

    /// A click on option `i` of the front dialog.
    pub(crate) fn embed_pick(&mut self, i: usize, cx: &mut Context<Self>) {
        self.click_option(i, cx);
    }

    /// A press on the dialog panel engages it (Enter and digits answer from then on).
    pub(crate) fn embed_engage(&mut self, cx: &mut Context<Self>) {
        if let Some(lu) = &mut self.live
            && let Some(d) = lu.live.dialogs.first_mut()
        {
            d.engaged = true;
            lu.dirty = true;
        }
        cx.notify();
    }

    /// The permission mode pill: the next mode (the bypass step asks for its confirmation as a dialog).
    pub(crate) fn embed_cycle_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(lu) = &mut self.live {
            lu.live.cycle_mode();
            lu.dirty = true;
        }
        cx.notify();
    }

    /// A toggle row of the `/rewind` confirmation.
    pub(crate) fn embed_toggle(&mut self, g: usize, cx: &mut Context<Self>) {
        self.rewind_toggle(g, cx);
    }
}
