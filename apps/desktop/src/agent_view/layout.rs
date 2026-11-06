//! Layout numbers of the Agent view, from `cli.html` `#view2` (`.grid2`, `.chatstage.with-subagent`,
//! `.bottomtabs`, `.inputbar`, `.modebar`). The design gives `260px | 1fr | 430px` and no breakpoints; the
//! narrower tiers below extend it the way the TUI view's tiers do (`tui_view::layout`), so the three
//! columns still fit the design's 900x520 minimum. The tokens shared with the TUI view are re-used.

pub use crate::tui_view::layout::{
    FOOTER_H, GAP, HEADER_H, LEFT_GAP, MIN_H, MIN_W, PAD, PANEL_RADIUS, Tiers, tiers,
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cols {
    pub left: f32,
    pub centre_min: f32,
    pub right: f32,
}

/// `260 | 1fr | 430`, then 230|320|360 below 1400, 210|300|320 below 1200, 180|300|260 below 1000.
pub fn cols(width: f32) -> Cols {
    let (left, centre_min, right) = if width < 1000. {
        (180., 300., 260.)
    } else if width < 1200. {
        (210., 300., 320.)
    } else if width < 1400. {
        (230., 320., 360.)
    } else {
        (260., 360., 430.)
    };
    Cols {
        left,
        centre_min,
        right,
    }
}

/// The conversation and the sub-agent pane side by side: `minmax(0,2fr) minmax(220px,1fr)` of `w`.
pub fn split(w: f32) -> (f32, f32) {
    let sub = (w / 3.).max(SUB_MIN_W).min(w);
    (w - sub, sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wide_tier_is_the_designs_grid() {
        let c = cols(1920.);
        assert_eq!((c.left, c.right), (260., 430.));
        assert_eq!(cols(1400.), cols(2400.));
    }

    #[test]
    fn every_tier_leaves_the_centre_its_minimum_at_the_minimum_width() {
        for w in [900., 999., 1000., 1199., 1200., 1399., 1400., 1920.] {
            let c = cols(w);
            let centre = w - 2. * PAD - 2. * GAP - c.left - c.right;
            assert!(centre >= c.centre_min, "{w}: {centre} < {}", c.centre_min);
        }
    }

    #[test]
    fn the_split_is_two_to_one_with_a_floor() {
        assert_eq!(split(900.), (600., 300.));
        let (a, b) = split(600.);
        assert_eq!((a, b), (380., 220.));
        assert_eq!(split(100.), (0., 100.));
    }
}
