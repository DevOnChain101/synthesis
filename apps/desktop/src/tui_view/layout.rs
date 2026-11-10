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

/// Left and right gutter of every centre tab body (chat, diff, terminal, browser, file explorer): one container
/// in `TuiView::chat` pads them all, so no tab draws up to the frame's border.
pub const TAB_PAD_X: f32 = 16.;

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
    /// Minimum height of the Recent Activity panel (140 from 860, else `left_min`).
    pub activity_min: f32,
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
        activity_min: if height >= 860. {
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
/// `session` = Session, `tools` = Tools & Skills, `activity` = Recent Activity (prompts and answers in one
/// panel), `tokens` = Context, `topology` = Synthesis Topology, `usage` = Usage & Metrics, `status` = Mission.
/// Never hideable (owner decision 2026-11-09): Workspace, Running, the chat (with the sub-agent transcript beside
/// it) and the sub-agents (the Topology Subagents card, or a compact Subagents block in its place while Topology
/// is hidden). So the right column always exists. `status` = Mission (the agent's task list): it shows only while
/// the agent has tasks, and `/panels status off` (or `mission off`) hides it for good.
/// `mcp` has no panel of its own any more (MCP servers are the Topology MCP card and the Tools & Skills
/// `mcp` row); the key is accepted and ignored so no user's settings.json breaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub session: bool,
    pub mcp: bool,
    pub tools: bool,
    pub activity: bool,
    pub tokens: bool,
    pub topology: bool,
    pub usage: bool,
    /// `status`: the Mission panel (it also needs a task list to show).
    pub status: bool,
}

/// The panels `/panels` and Settings can switch: (settings key, name shown).
pub const HIDEABLE: [(&str, &str); 7] = [
    ("session", "Session"),
    ("tools", "Tools & Skills"),
    ("activity", "Recent Activity"),
    ("tokens", "Context"),
    ("topology", "Synthesis Topology"),
    ("usage", "Usage & Metrics"),
    ("status", "Mission"),
];

/// The panels that stay when everything else is hidden, with the reason (docs/TUI.md).
pub const ALWAYS: [(&str, &str); 4] = [
    ("Workspace", "always shown"),
    ("Running", "always shown"),
    ("Chat", "always shown"),
    (
        "Subagents",
        "shown in Topology, or as a compact block when Topology is hidden",
    ),
];

impl Hidden {
    fn flag(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "session" => &mut self.session,
            "mcp" => &mut self.mcp,
            "tools" => &mut self.tools,
            "activity" => &mut self.activity,
            "tokens" => &mut self.tokens,
            "topology" => &mut self.topology,
            "usage" => &mut self.usage,
            "status" | "mission" => &mut self.status,
            _ => return None,
        })
    }

    /// Hides or shows the panel with this settings key; false for a key that cannot be switched.
    pub fn set_hidden(&mut self, key: &str, hidden: bool) -> bool {
        self.flag(key).map(|f| *f = hidden).is_some()
    }

    /// Hides the panel with this settings key; false for a key that is not a hideable panel.
    pub fn hide(&mut self, key: &str) -> bool {
        self.set_hidden(key, true)
    }

    pub fn is_hidden(&self, key: &str) -> bool {
        let mut c = *self;
        c.flag(key).is_some_and(|f| *f)
    }

    /// Workspace and Running split the left column in halves when everything below them is hidden.
    pub fn left_is_split(&self) -> bool {
        self.session && self.tools && self.activity
    }
}

/// What `/panels` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelsCmd {
    List,
    HideAll,
    ShowAll,
    Set(&'static str, bool),
}

/// `/panels [hide-all | show-all | <name> on|off]`. Names are the settings keys, or the panel's name without
/// spaces ("recent-activity" is not needed: `activity`).
pub fn parse_panels(arg: &str) -> Result<PanelsCmd, String> {
    let w: Vec<&str> = arg.split_whitespace().collect();
    match w.as_slice() {
        [] | ["list"] => Ok(PanelsCmd::List),
        ["hide-all"] => Ok(PanelsCmd::HideAll),
        ["show-all"] => Ok(PanelsCmd::ShowAll),
        [name, state @ ("on" | "off")] => {
            let key = HIDEABLE.iter().find(|(k, _)| k == name).map(|(k, _)| *k);
            match (key, *name) {
                (Some(k), _) => Ok(PanelsCmd::Set(k, *state == "off")),
                (None, "workspace" | "running" | "chat" | "subagents") => {
                    Err(format!("{name} is always shown"))
                }
                (None, _) => Err(format!("{name} is not a panel ({})", keys())),
            }
        }
        _ => Err("usage: /panels [hide-all | show-all | <name> on|off]".into()),
    }
}

fn keys() -> String {
    HIDEABLE
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(", ")
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
        assert_eq!(tiers(900.).activity_min, 140.);
        assert_eq!(tiers(800.).activity_min, 84.);
        assert_eq!(tiers(700.).activity_min, 56.);
        assert_eq!(tiers(749.).left_min, 56.);
        assert_eq!(tiers(749.).topo_min, 150.);
        assert_eq!(tiers(609.).left_min, 38.);
        assert_eq!(tiers(750.).topo_min, 230.);
    }

    #[test]
    fn panel_keys() {
        let mut h = Hidden::default();
        assert!(!h.hide("workspace") && !h.hide("running") && !h.hide("chat") && !h.hide("nope"));
        assert!(h.hide("mission") && h.is_hidden("status"));
        assert!(h.set_hidden("status", false));
        for k in ["session", "mcp", "tools"] {
            assert!(h.hide(k));
        }
        assert!(!h.left_is_split());
        assert!(h.hide("activity") && h.is_hidden("activity"));
        assert!(h.left_is_split());
        assert!(h.set_hidden("activity", false) && !h.is_hidden("activity"));
    }

    #[test]
    fn panels_command_parsing() {
        use PanelsCmd::*;
        assert_eq!(parse_panels(""), Ok(List));
        assert_eq!(parse_panels(" hide-all "), Ok(HideAll));
        assert_eq!(parse_panels("show-all"), Ok(ShowAll));
        assert_eq!(parse_panels("usage off"), Ok(Set("usage", true)));
        assert_eq!(parse_panels("tokens  on"), Ok(Set("tokens", false)));
        assert_eq!(parse_panels("status off"), Ok(Set("status", true)));
        assert!(
            parse_panels("running off")
                .unwrap_err()
                .contains("always shown")
        );
        assert!(parse_panels("nope on").unwrap_err().contains("not a panel"));
        assert!(parse_panels("usage maybe").is_err() && parse_panels("hide-all x").is_err());
    }
}
