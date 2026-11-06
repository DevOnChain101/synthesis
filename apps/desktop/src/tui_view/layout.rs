//! Layout numbers of the TUI view, taken from `cli.html` `#view1` (`.grid1`, the `@media` rules and the
//! `#view1 .col.l` overrides). Pure functions so the tests can compare them with the design.

/// Outer padding and the gap between columns.
pub const PAD: f32 = 10.;
pub const GAP: f32 = 10.;
/// View header (top bar) and footer (key hint bar).
pub const HEADER_H: f32 = 46.;
pub const FOOTER_H: f32 = 28.;
/// Minimum app size of the design (`#app`).
pub const MIN_W: f32 = 900.;
pub const MIN_H: f32 = 520.;
pub const PANEL_RADIUS: f32 = 8.;
/// Left column gap (`#view1 .col.l{gap:8px}`).
pub const LEFT_GAP: f32 = 8.;
/// Rows a Workspace / Running panel lists before it scrolls (docs/TUI.md), each `ROW_H` tall.
pub const LIST_ROWS: usize = 5;
pub const ROW_H: f32 = 36.;
/// Panel chrome: header (padding + one 12px line at 1.55) and body padding.
pub const HEAD_PAD: (f32, f32) = (9., 6.);
pub const HEAD_PAD_LEFT: (f32, f32) = (7., 4.);
pub const HEAD_LINE: f32 = 18.6;
pub const BODY_PAD: (f32, f32) = (2., 10.);
pub const BODY_PAD_LEFT_BOTTOM: f32 = 6.;
/// One text line of the panels (12.5px at 1.55): kv and MCP rows are this tall, so a capped body
/// ends on a whole row.
pub const KV_ROW: f32 = 19.375;

/// Body height (top padding + `rows` lines) of a panel capped at `rows` rows.
pub fn body_h(rows: usize) -> f32 {
    BODY_PAD.0 + rows as f32 * KV_ROW
}

/// The most whole rows that fit under `max` px of body.
pub fn whole_rows_h(max: f32) -> f32 {
    body_h(((max - BODY_PAD.0) / KV_ROW).floor().max(1.) as usize)
}

/// Column widths for a viewport width: `250 | minmax(320,1fr) | 500`, then 200|300|380 below 1400,
/// 190|300|300 below 1200, 180|300|260 below 1000.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cols {
    pub left: f32,
    pub centre_min: f32,
    pub right: f32,
}

pub fn cols(width: f32) -> Cols {
    let (left, centre_min, right) = if width < 1000. {
        (180., 300., 260.)
    } else if width < 1200. {
        (190., 300., 300.)
    } else if width < 1400. {
        (200., 300., 380.)
    } else {
        (250., 320., 500.)
    };
    Cols {
        left,
        centre_min,
        right,
    }
}

/// Height tiers (`@media (max-height: 749/609)`, `(min-height: 860)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tiers {
    /// Minimum height of a left column panel (84, 56 below 750, 38 below 610).
    pub left_min: f32,
    /// Minimum height of the Topology panel (230, 150 below 750) and whether its header is tightened.
    pub topo_min: f32,
    pub compact_head: bool,
    /// Minimum height of the Usage panel (84, 140 from 860).
    pub usage_min: f32,
}

pub fn tiers(height: f32) -> Tiers {
    Tiers {
        left_min: if height < 610. {
            38.
        } else if height < 750. {
            56.
        } else {
            84.
        },
        topo_min: if height < 750. { 150. } else { 230. },
        compact_head: height < 750.,
        usage_min: if height >= 860. { 140. } else { 84. },
    }
}

/// The `panels.*` settings keys (docs/TUI.md "Settings"). Workspace, Running and the chat cannot be hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub session: bool,
    pub mcp: bool,
    pub tools: bool,
    pub activity: bool,
    pub status: bool,
    pub tokens: bool,
    pub topology: bool,
    pub usage: bool,
}

impl Hidden {
    /// Hides the panel with this settings key; false for a key that is not a panel.
    pub fn hide(&mut self, key: &str) -> bool {
        let f = match key {
            "session" => &mut self.session,
            "mcp" => &mut self.mcp,
            "tools" => &mut self.tools,
            "activity" => &mut self.activity,
            "status" => &mut self.status,
            "tokens" => &mut self.tokens,
            "topology" => &mut self.topology,
            "usage" => &mut self.usage,
            _ => return false,
        };
        *f = true;
        true
    }

    /// Workspace and Running split the left column in halves when everything below them is hidden.
    pub fn left_is_split(&self) -> bool {
        self.session && self.mcp && self.tools && self.activity
    }

    /// The right column (Topology, Usage) exists at all.
    pub fn has_right(&self) -> bool {
        !(self.topology && self.usage)
    }

    pub fn has_cards(&self) -> bool {
        !(self.status && self.tokens)
    }
}

/// Height of a list body that shows `LIST_ROWS` rows (Workspace, Running).
pub fn list_body_h() -> f32 {
    LIST_ROWS as f32 * ROW_H + BODY_PAD.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_are_the_designs() {
        let w = |x| {
            let c = cols(x);
            (c.left, c.right)
        };
        assert_eq!(w(1920.), (250., 500.));
        assert_eq!(w(1400.), (250., 500.));
        assert_eq!(w(1399.), (200., 380.));
        assert_eq!(w(1200.), (200., 380.));
        assert_eq!(w(1199.), (190., 300.));
        assert_eq!(w(1000.), (190., 300.));
        assert_eq!(w(999.), (180., 260.));
        assert_eq!(w(900.), (180., 260.));
    }

    #[test]
    fn height_tiers() {
        assert_eq!(tiers(900.).left_min, 84.);
        assert_eq!(tiers(900.).usage_min, 140.);
        assert_eq!(tiers(800.).usage_min, 84.);
        assert_eq!(tiers(749.).left_min, 56.);
        assert_eq!(tiers(749.).topo_min, 150.);
        assert_eq!(tiers(609.).left_min, 38.);
        assert_eq!(tiers(750.).topo_min, 230.);
    }

    #[test]
    fn panel_keys() {
        let mut h = Hidden::default();
        assert!(!h.hide("workspace") && !h.hide("running") && !h.hide("chat"));
        for k in ["session", "mcp", "tools"] {
            assert!(h.hide(k));
        }
        assert!(!h.left_is_split());
        assert!(h.hide("activity"));
        assert!(h.left_is_split());
        h.hide("topology");
        assert!(h.has_right());
        h.hide("usage");
        assert!(!h.has_right());
    }
}
