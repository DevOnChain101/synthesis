//! What the Super Agent's `ChatHost` takes from an embedded view that it does not draw: the engine's state
//! (`Live`, the same one the TUI view draws, so approvals, asks, masking and the session rules are not
//! re-implemented), a line sent through the same rules as the TUI prompt, a click on a dialog option and the mode.

use gpui::{Context, Window};
use gpui_component::input::TextareaState;

use super::TuiView;
use crate::live::{Action, Live};

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

    /// A key as the window-wide hook delivers it (tests).
    #[cfg(test)]
    pub(crate) fn test_key(
        &mut self,
        k: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let ks = gpui::Keystroke::parse(k).unwrap();
        self.on_key(&ks, window, cx)
    }

    /// The Files pane as the tests see it: selected row, text shown, whether text is selected, composer focused.
    #[cfg(test)]
    pub(crate) fn test_files(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> (Option<String>, Option<String>, bool, bool) {
        (
            self.ts.tree_sel.clone(),
            self.shown_lines().map(|l| l.join("\n")),
            self.has_file_sel(),
            self.live
                .as_ref()
                .is_some_and(|l| gpui::Focusable::focus_handle(&l.prompt, cx).is_focused(window)),
        )
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

    /// The completion popup of the prompt (`/command`, `@file`): the rows (label, detail) and the highlighted one.
    pub(crate) fn embed_completion(&self) -> Option<(&[(String, String)], usize)> {
        let c = self.live.as_ref()?.comp.as_ref()?;
        Some((&c.items, c.sel))
    }

    /// `/focus` typed in this frame: the state it asked for, once (the owner applies it to the whole window).
    pub(crate) fn embed_take_focus_req(&mut self) -> Option<bool> {
        self.live.as_mut()?.live.focus_req.take()
    }
}

/// What a Super Agent frame says for a command whose action it cannot carry out (`None`: it runs). No wildcard:
/// a new action must be classified here.
pub(crate) fn embed_note(a: &Action) -> Option<&'static str> {
    use Action::*;
    match a {
        OpenSettings => Some(
            "/config: open Settings from the app's Settings entry (not available inside a session frame)",
        ),
        Panels(_) => {
            Some("/panels: the dashboard panels belong to the TUI view; a session frame has none")
        }
        Model(_)
        | RefreshModels
        | Open(_)
        | SetContext(_)
        | Resume(_)
        | TrustRevoked(_)
        | AskTrust
        | New
        | CloseWorkspace(_)
        | Close
        | Clear
        | Compact(_)
        | ShowFiles
        | Reveal(_)
        | ShowDiff(_)
        | TrustFolder(_)
        | Recheck
        | Quit
        | Peers
        | Doctor
        | Send { .. }
        | Rewind
        | RewindPreview(_)
        | RewindApply(_)
        | Clean
        | CleanApply { .. } => None,
    }
}
