//! Layout numbers of the Agent view: the columns, breakpoints and height tiers are the TUI view's (`cli.html`
//! `.grid1`, `tui_view::layout`), so the two views share one grid; what is here is the conversation's own
//! (`.chatstage.with-subagent`, `.inputbar`, `.modebar`).

pub use crate::tui_view::layout::{
    Cols, FOOTER_H, GAP, HEADER_H, LEFT_GAP, MIN_H, MIN_W, PAD, PANEL_RADIUS, Tiers, cols, tiers,
};

/// Height of the tab strip above the conversation (`.bottomtabs{height:32px}`).
pub const TABS_H: f32 = 32.;
/// Conversation header (`.chead`: 8px padding around one 12.5px line at 1.55).
pub const CHEAD_H: f32 = 36.;
/// The input box (`.inputbar form{height:44px}`) and its margins (`0 12px 12px`).
pub const INPUT_H: f32 = 44.;
pub const INPUT_MARGIN: f32 = 12.;
/// The mode line under it (`.modebar{height:22px}`).
pub const MODEBAR_H: f32 = 22.;
/// One row of the sub-agent switch (`.agent-option{height:20px}`).
pub const SWITCH_ROW_H: f32 = 20.;
/// Width the sub-agent pane takes at least (`minmax(220px,1fr)`); the conversation gets `2fr`.
pub const SUB_MIN_W: f32 = 220.;

/// The conversation and the sub-agent pane side by side: `minmax(0,2fr) minmax(220px,1fr)` of `w`.
pub fn split(w: f32) -> (f32, f32) {
    let sub = (w / 3.).max(SUB_MIN_W).min(w);
    (w - sub, sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_is_two_to_one_with_a_floor() {
        assert_eq!(split(900.), (600., 300.));
        let (a, b) = split(600.);
        assert_eq!((a, b), (380., 220.));
        assert_eq!(split(100.), (0., 100.));
    }
}
