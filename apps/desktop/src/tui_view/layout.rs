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
/// Gap between the Agent Status and Context cards (`.status-context{gap:8px}`).
pub const CARD_GAP: f32 = 8.;
/// One text line of the panels (12.5px at 1.55).
pub const LINE: f32 = 19.375;
/// One line of the compact Agent Status / Context bodies (`line-height:1.35`).
pub const LINE_TIGHT: f32 = 16.875;

/// Owner rule (2026-11-07), a deliberate deviation from the proportions of `cli.html`: the chat column is at least
/// 55% of the window width at every width. Left column: 250 from 1400, else 220. Right column:
/// clamp(300, 24% of the width, 420). The right column is inline from 1280 and a slide-over drawer below it; the left
/// column is inline from 1000 and a drawer below it. The chat takes the rest (79% at 3440), and the share of
/// the smallest inline layout is 55.7% (1280: 713 of 1280). Hidden panels give their space to the chat.
pub const RIGHT_INLINE_MIN: f32 = 1280.;
pub const LEFT_INLINE_MIN: f32 = 1000.;

/// Column widths for a viewport width. `left`/`right` are the widths of the column inline and of its drawer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cols {
    pub left: f32,
    pub right: f32,
    pub left_inline: bool,
    pub right_inline: bool,
}

pub fn cols(width: f32) -> Cols {
    Cols {
        left: if width < 1400. { 220. } else { 250. },
        right: (width * 0.24).clamp(300., 420.),
        left_inline: width >= LEFT_INLINE_MIN,
        right_inline: width >= RIGHT_INLINE_MIN,
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
    /// Minimum height of the Usage & Metrics panel and the Agent Status / Context row.
    pub usage_min: f32,
    /// Minimum height of the User Prompt panel (140 from 860, else `left_min`).
    pub history_min: f32,
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
        usage_min: 84.,
        history_min: if height >= 860. {
            140.
        } else if height < 610. {
            38.
        } else if height < 750. {
            56.
        } else {
            84.
        },
    }
}

/// The `synt.tui.panels.*` settings keys (docs/TUI.md "Settings"), mapped onto the panels of `cli.html`:
/// `session` = Session, `tools` = Tools & Skills, `activity` = Recent Activity and User Prompt,
/// `status` = Agent Status, `tokens` = Context, `topology` = Synthesis Topology, `usage` = Usage & Metrics.
/// `mcp` has no panel of its own any more (MCP servers are the Topology MCP card and the Tools & Skills
/// `mcp` row); the key is accepted and ignored so no user's settings.json breaks.
/// Workspace, Running and the chat cannot be hidden.
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
        self.session && self.tools && self.activity
    }

    /// The right column (Topology, Agent Status / Context, Usage & Metrics) exists at all.
    pub fn has_right(&self) -> bool {
        !(self.topology && self.usage && !self.has_cards())
    }

    /// The Agent Status / Context row exists.
    pub fn has_cards(&self) -> bool {
        !(self.status && self.tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_is_the_majority_at_every_width() {
        for w in (900..=3440).step_by(10).map(|w| w as f32) {
            let c = cols(w);
            let side = |inline: bool, x: f32| if inline { x + GAP } else { 0. };
            let centre = w - 2. * PAD - side(c.left_inline, c.left) - side(c.right_inline, c.right);
            assert!(centre / w >= 0.55, "{w}: {}", centre / w);
        }
        assert_eq!((cols(1400.).left, cols(1399.).left), (250., 220.));
        assert_eq!((cols(1400.).right, cols(1920.).right), (336., 420.));
        assert_eq!((cols(3440.).right, cols(1250.).right), (420., 300.));
        assert!(cols(1280.).right_inline && !cols(1279.).right_inline);
        assert!(cols(1000.).left_inline && !cols(999.).left_inline);
    }

    #[test]
    fn height_tiers() {
        assert_eq!(tiers(900.).left_min, 84.);
        assert_eq!(tiers(900.).history_min, 140.);
        assert_eq!(tiers(800.).history_min, 84.);
        assert_eq!(tiers(700.).history_min, 56.);
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
        h.hide("usage");
        assert!(h.has_right(), "the cards remain");
        h.hide("status");
        h.hide("tokens");
        assert!(!h.has_right());
    }
}
