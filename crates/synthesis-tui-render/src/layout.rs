//! Panel registry and the pure layout planner (docs/specs/tui-design.md §6 and §14.4/§14.5).
//!
//! `plan` maps (terminal size, which panels are on, the Tabs-mode view, what each panel would like)
//! to rectangles. No I/O, no state: a resize back to an earlier size gives the earlier plan.
//! Owner rules: Workspace, Running sessions and Chat are locked; the Sub-agents panel is locked and
//! always in the right sidebar; every other panel can be hidden; the panels that stay visible take
//! the rows the hidden ones freed (no holes); with no optional left panel the left column is split
//! equally between Workspace and Running.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[cfg(test)]
impl Rect {
    fn right(&self) -> i64 {
        self.x + self.w
    }
    fn bottom(&self) -> i64 {
        self.y + self.h
    }
}

/// Smallest frame (columns x rows of `render_frame`; the app adds the status line, so the smallest
/// terminal is 40x12).
pub const MIN_COLS: i64 = 40;
pub const MIN_ROWS: i64 = 11;
/// Columns mode from this width; Tabs mode from `CHAT_ONLY_BELOW`.
pub const COLUMNS_FROM: i64 = 100;
pub const CHAT_ONLY_BELOW: i64 = 60;
/// The chat column never gets narrower than this in Columns mode.
const CHAT_MIN_W: i64 = 48;
/// Right column that fits the topology cards (46 columns of panel plus the rule), and the chat width
/// that must remain for the column to grow to it.
const TOPOLOGY_R: i64 = 47;
const TOPOLOGY_CHAT_MIN_W: i64 = 70;
/// The chat pane needs at least this many rows (label row included).
const CHAT_MIN_H: i64 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelId {
    Workspace,
    Running,
    Session,
    ToolsSkills,
    RecentActivity,
    SessionStats,
    Chat,
    AgentStatus,
    Context,
    ProjectTree,
    Diff,
    Terminal,
    Topology,
    UsageMetrics,
    SystemHealth,
    Subagents,
}

pub const PANEL_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    /// The cards above the chat (agent status, context).
    CentreTop,
    Chat,
    /// A tab of the centre strip (content arrives in P3; the planner does not place it).
    CentreTab,
}

pub struct PanelSpec {
    pub id: PanelId,
    /// Settings key under `synthesis.tui.panels` and the id the docs use.
    pub key: &'static str,
    pub title: &'static str,
    /// One line for `/config`.
    pub desc: &'static str,
    pub side: Side,
    pub default_on: bool,
    /// `Some(reason)`: never hidden; the reason is what `/config` shows.
    pub locked: Option<&'static str>,
    pub min_h: i64,
    pub ideal_h: i64,
    pub max_h: i64,
    /// Narrowest column the panel is drawn in; on a narrower one it is "starved".
    pub min_w: i64,
    /// 0: fixed height (grows to its ideal); n: shares spare rows by weight n, beyond its ideal.
    pub flex: i64,
    /// Order in which an optional panel is dropped when its column is too short: 1 goes first.
    pub drop: u8,
    /// Where the content comes from (documentation; the renderer matches on `id`).
    pub source: &'static str,
}

const INF: i64 = i64::MAX / 4;

macro_rules! spec {
    ($id:ident, $key:expr, $title:expr, $desc:expr, $side:ident, $on:expr, $locked:expr,
     $min:expr, $ideal:expr, $max:expr, $minw:expr, $flex:expr, $drop:expr, $src:expr) => {
        PanelSpec {
            id: PanelId::$id,
            key: $key,
            title: $title,
            desc: $desc,
            side: Side::$side,
            default_on: $on,
            locked: $locked,
            min_h: $min,
            ideal_h: $ideal,
            max_h: $max,
            min_w: $minw,
            flex: $flex,
            drop: $drop,
            source: $src,
        }
    };
}

/// The registry, indexed by `PanelId as usize`.
pub static REGISTRY: [PanelSpec; PANEL_COUNT] = [
    spec!(
        Workspace,
        "workspace",
        "Workspace",
        "workspaces and their session counts",
        Left,
        true,
        Some("Workspace is always visible: it switches workspaces."),
        2,
        5,
        8,
        0,
        0,
        0,
        "workspace_panel"
    ),
    spec!(
        Running,
        "running",
        "Running sessions",
        "sessions of the workspace",
        Left,
        true,
        Some("Running sessions are always visible."),
        3,
        7,
        INF,
        0,
        1,
        0,
        "sessions_panel"
    ),
    spec!(
        Session,
        "session",
        "Session",
        "model, mode, effort, branch, cwd",
        Left,
        true,
        None,
        4,
        9,
        9,
        0,
        0,
        4,
        "session_info_panel"
    ),
    spec!(
        ToolsSkills,
        "tools_skills",
        "Tools & skills",
        "tools used, skills, MCP servers",
        Left,
        true,
        None,
        3,
        8,
        12,
        0,
        0,
        3,
        "tools_panel + mcp rows"
    ),
    spec!(
        RecentActivity,
        "recent_activity",
        "Recent activity",
        "the latest events with their age",
        Left,
        true,
        None,
        4,
        8,
        INF,
        0,
        1,
        2,
        "feed_panel"
    ),
    spec!(
        SessionStats,
        "session_stats",
        "Session stats",
        "files, commands, tokens, tool calls, success rate",
        Left,
        true,
        None,
        4,
        7,
        7,
        0,
        0,
        1,
        "Session counters"
    ),
    spec!(
        Chat,
        "chat",
        "Chat",
        "the conversation and the prompt",
        Chat,
        true,
        Some("Chat is always visible."),
        CHAT_MIN_H,
        INF,
        INF,
        0,
        1,
        0,
        "agent_panel"
    ),
    spec!(
        AgentStatus,
        "agent_status",
        "Agent status",
        "phase, working on, next, plan",
        CentreTop,
        true,
        None,
        4,
        5,
        5,
        0,
        0,
        6,
        "status_panel + todos"
    ),
    spec!(
        Context,
        "context",
        "Context",
        "window meter, input, output, cache",
        CentreTop,
        true,
        None,
        4,
        5,
        5,
        0,
        0,
        5,
        "context meter + token_panel"
    ),
    spec!(
        ProjectTree,
        "project_tree",
        "Project tree",
        "folder tree with git status (tab arrives in P3)",
        CentreTab,
        false,
        None,
        4,
        10,
        INF,
        0,
        1,
        0,
        "folders.rs + git status"
    ),
    spec!(
        Diff,
        "diff",
        "Diff",
        "the session's file changes (tab arrives in P3)",
        CentreTab,
        false,
        None,
        4,
        10,
        INF,
        0,
        1,
        0,
        "agent/diffview.rs"
    ),
    spec!(
        Terminal,
        "terminal",
        "Terminal",
        "bash tool output, read only (tab arrives in P3)",
        CentreTab,
        false,
        None,
        4,
        10,
        INF,
        0,
        1,
        0,
        "bash tool outputs"
    ),
    spec!(
        Topology,
        "topology",
        "Topology",
        "cards: guards, skills, sub-agents, execution, hooks, MCP, memory",
        Right,
        true,
        None,
        12,
        32,
        32,
        46,
        0,
        1,
        "cards from session data"
    ),
    spec!(
        UsageMetrics,
        "usage_metrics",
        "Usage & metrics",
        "bars of the session counters",
        Right,
        true,
        None,
        4,
        9,
        9,
        0,
        0,
        2,
        "Session counters + usage"
    ),
    spec!(
        SystemHealth,
        "system_health",
        "System health",
        "core agent, tool runtime, MCP, memory, browser",
        Right,
        true,
        None,
        4,
        8,
        8,
        0,
        0,
        3,
        "host and runtime state"
    ),
    spec!(
        Subagents,
        "subagents",
        "Sub-agents",
        "sub-agent list and transcript",
        Right,
        true,
        Some("Sub-agents always live in the right sidebar."),
        6,
        10,
        INF,
        0,
        2,
        0,
        "agent.subagents + transcript_rows"
    ),
];

pub fn spec(id: PanelId) -> &'static PanelSpec {
    &REGISTRY[id as usize]
}

impl PanelId {
    pub fn key(self) -> &'static str {
        spec(self).key
    }
    pub fn parse(key: &str) -> Option<PanelId> {
        REGISTRY.iter().find(|s| s.key == key).map(|s| s.id)
    }
}

/// Which panels are on. Locked panels are always on, whatever the mask says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panels(u32);

impl Default for Panels {
    fn default() -> Self {
        Panels(
            REGISTRY
                .iter()
                .filter(|s| s.default_on)
                .fold(0, |m, s| m | 1 << s.id as u32),
        )
    }
}

impl Panels {
    pub fn is_on(&self, id: PanelId) -> bool {
        spec(id).locked.is_some() || self.0 & (1 << id as u32) != 0
    }
    /// Locked panels ignore the request.
    pub fn set(&mut self, id: PanelId, on: bool) {
        if spec(id).locked.is_none() {
            if on {
                self.0 |= 1 << id as u32;
            } else {
                self.0 &= !(1 << id as u32);
            }
        }
    }
    /// Every hideable panel off (tests).
    pub fn none() -> Panels {
        let mut p = Panels::default();
        for s in REGISTRY.iter() {
            p.set(s.id, false);
        }
        p
    }
    pub fn all() -> Panels {
        let mut p = Panels::default();
        for s in REGISTRY.iter() {
            p.set(s.id, true);
        }
        p
    }
    /// Mask over the hideable panels, bit i = the i-th hideable panel (tests sweep all of them).
    pub fn from_mask(mask: u32) -> Panels {
        let mut p = Panels::none();
        for (i, s) in REGISTRY.iter().filter(|s| s.locked.is_none()).enumerate() {
            p.set(s.id, mask >> i & 1 == 1);
        }
        p
    }
}

/// How many panels can be hidden.
pub fn hideable_count() -> usize {
    REGISTRY.iter().filter(|s| s.locked.is_none()).count()
}

/// What the content would like, by panel (rows including the label row). The default is the
/// registry's ideal; the renderer overrides the panels whose size follows their content.
#[derive(Clone, Copy, Debug)]
pub struct Demand([i64; PANEL_COUNT]);

impl Default for Demand {
    fn default() -> Self {
        let mut d = [0; PANEL_COUNT];
        for s in REGISTRY.iter() {
            d[s.id as usize] = s.ideal_h;
        }
        Demand(d)
    }
}

impl Demand {
    pub fn with(mut self, id: PanelId, rows: i64) -> Self {
        self.0[id as usize] = rows;
        self
    }
    pub fn get(&self, id: PanelId) -> i64 {
        self.0[id as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Columns,
    Tabs,
    ChatOnly,
    TooSmall,
}

/// The views of Tabs mode (Tab cycles them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Chat,
    Sessions,
    Agents,
    Panels,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Chat => "Chat",
            View::Sessions => "Sessions",
            View::Agents => "Agents",
            View::Panels => "Panels",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub id: PanelId,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub w: i64,
    pub h: i64,
    pub mode: Mode,
    /// The view actually shown (Tabs mode): `Panels` falls back to `Chat` when no panel is on.
    pub view: View,
    pub header_rows: i64,
    /// Tabs mode: the row of the tab strip, and the views it offers.
    pub tab_row: Option<i64>,
    pub tabs: Vec<View>,
    /// First row of the bottom bar.
    pub bottom_row: i64,
    /// Everything between the header and the bottom bar.
    pub body: Rect,
    /// Columns mode: the sidebars (rule column included) and the chat column.
    pub left: Rect,
    pub right: Rect,
    pub chat: Option<Rect>,
    /// Every placed panel except the chat.
    pub panels: Vec<Placed>,
    /// On, but with no room in this size (shown as "no room at this size" in `/config`).
    pub starved: Vec<PanelId>,
}

impl Plan {
    pub fn rect(&self, id: PanelId) -> Option<Rect> {
        if id == PanelId::Chat {
            return self.chat;
        }
        self.panels.iter().find(|p| p.id == id).map(|p| p.rect)
    }
}

pub fn clamp(v: i64, lo: i64, hi: i64) -> i64 {
    v.max(lo).min(hi)
}

/// Column widths of Columns mode, each including its 1-column rule.
pub fn column_widths(w: i64) -> (i64, i64) {
    let mut l = clamp(w * 16 / 100, 22, 34);
    let mut r = clamp(w * 28 / 100, 30, 56);
    if w - l - r < CHAT_MIN_W {
        r = (w - l - CHAT_MIN_W).max(30);
        l = (w - r - CHAT_MIN_W).max(22);
    } else if r < TOPOLOGY_R && w - l - TOPOLOGY_R >= TOPOLOGY_CHAT_MIN_W {
        r = TOPOLOGY_R; // wide enough for the topology cards while the chat keeps 70 columns
    }
    (l, r)
}

/// Gives `spare` rows to `items` (index into `h`, weight, cap), one row at a time to the item with
/// the least rows per weight; ties go to the earlier one. Returns what is left.
fn share(h: &mut [i64], items: &[(usize, i64, i64)], mut spare: i64) -> i64 {
    let mut given = vec![0i64; items.len()];
    while spare > 0 {
        let pick = items
            .iter()
            .enumerate()
            .filter(|(_, (i, _, cap))| h[*i] < *cap)
            .min_by_key(|(k, (_, wt, _))| (given[*k] * 1000 / wt.max(&1), *k))
            .map(|(k, _)| k);
        let Some(k) = pick else { break };
        h[items[k].0] += 1;
        given[k] += 1;
        spare -= 1;
    }
    spare
}

struct Fit {
    /// (panel, height), top to bottom.
    rows: Vec<(PanelId, i64)>,
    gap: i64,
    starved: Vec<PanelId>,
}

/// Stacks the panels of `ids` that are on into a column `col_w` wide and `height` rows tall.
fn fit(ids: &[PanelId], col_w: i64, height: i64, on: &Panels, demand: &Demand) -> Fit {
    let mut starved = vec![];
    let mut p: Vec<PanelId> = vec![];
    for &id in ids.iter().filter(|id| on.is_on(**id)) {
        if spec(id).min_w > col_w {
            starved.push(id);
        } else {
            p.push(id);
        }
    }
    let min_sum = |p: &[PanelId]| p.iter().map(|id| spec(*id).min_h).sum::<i64>();
    let gap_for = |p: &[PanelId]| {
        if min_sum(p) + (p.len() as i64 - 1).max(0) <= height {
            1
        } else {
            0
        }
    };
    // gaps go first, then the optional panel with the lowest drop number
    loop {
        if min_sum(&p) + gap_for(&p) * (p.len() as i64 - 1).max(0) <= height {
            break;
        }
        let victim = p
            .iter()
            .enumerate()
            .filter(|(_, id)| spec(**id).locked.is_none())
            .min_by_key(|(_, id)| spec(**id).drop)
            .map(|(i, _)| i);
        match victim {
            Some(i) => starved.push(p.remove(i)),
            None => break,
        }
    }
    let n = p.len() as i64;
    if p.is_empty() {
        return Fit {
            rows: vec![],
            gap: 0,
            starved,
        };
    }
    let gap = gap_for(&p);
    if min_sum(&p) + gap * (n - 1) > height {
        // only locked panels and a very short column: equal split, the rest to the flex panel
        let mut h = vec![height / n; p.len()];
        let at = p
            .iter()
            .position(|id| spec(*id).flex > 0)
            .unwrap_or(p.len() - 1);
        h[at] += height - h.iter().sum::<i64>();
        return Fit {
            rows: p.into_iter().zip(h).collect(),
            gap: 0,
            starved,
        };
    }
    if p == [PanelId::Workspace, PanelId::Running] {
        let ws = (height - gap) / 2;
        return Fit {
            rows: vec![
                (PanelId::Workspace, ws),
                (PanelId::Running, height - gap - ws),
            ],
            gap,
            starved,
        };
    }
    let mut h: Vec<i64> = p.iter().map(|id| spec(*id).min_h).collect();
    let mut spare = height - h.iter().sum::<i64>() - gap * (n - 1);
    let ideal = |id: PanelId| {
        let s = spec(id);
        demand.get(id).clamp(s.min_h, s.max_h.max(s.min_h))
    };
    // 1 locked panels reach their ideal, 2 fixed optional panels (registry order), 3 flex optional
    // panels by weight, 4 what is left goes to the flex panels beyond their ideal, then to the last
    for stage in 1..=3 {
        let items: Vec<(usize, i64, i64)> = p
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                let s = spec(**id);
                match stage {
                    1 => s.locked.is_some(),
                    2 => s.locked.is_none() && s.flex == 0,
                    _ => s.locked.is_none() && s.flex > 0,
                }
            })
            .map(|(i, id)| (i, spec(*id).flex.max(1), ideal(*id)))
            .collect();
        if stage == 2 {
            for (i, _, cap) in items {
                let t = spare.min(cap - h[i]).max(0);
                h[i] += t;
                spare -= t;
            }
        } else {
            spare = share(&mut h, &items, spare);
        }
    }
    // 4a optional flex panels take what is left up to their max
    let opt_flex: Vec<(usize, i64, i64)> = p
        .iter()
        .enumerate()
        .filter(|(_, id)| spec(**id).locked.is_none() && spec(**id).flex > 0)
        .map(|(i, id)| (i, spec(*id).flex, spec(*id).max_h))
        .collect();
    spare = share(&mut h, &opt_flex, spare);
    // 4b then the locked ones; Workspace and Running stop at half the column, where the owner's
    // equal split starts, so hiding a panel never shortens either of them
    let both = p.contains(&PanelId::Workspace) && p.contains(&PanelId::Running);
    let half = (height - gap * (n - 1).min(1)) / 2;
    let locked: Vec<(usize, i64, i64)> = p
        .iter()
        .enumerate()
        .filter_map(|(i, id)| match id {
            PanelId::Workspace if both => Some((i, 1, half)),
            PanelId::Running if both => Some((i, 1, height - half)),
            _ if spec(*id).locked.is_some() && spec(*id).flex > 0 => {
                Some((i, spec(*id).flex, spec(*id).max_h))
            }
            _ => None,
        })
        .collect();
    spare = share(&mut h, &locked, spare);
    if spare > 0 {
        let last = h.len() - 1;
        h[last] += spare;
    }
    Fit {
        rows: p.into_iter().zip(h).collect(),
        gap,
        starved,
    }
}

fn place(f: &Fit, x: i64, y0: i64, w: i64, out: &mut Vec<Placed>) {
    let mut y = y0;
    for (id, h) in &f.rows {
        out.push(Placed {
            id: *id,
            rect: Rect { x, y, w, h: *h },
        });
        y += h + f.gap;
    }
}

const LEFT_OPTIONAL: [PanelId; 6] = [
    PanelId::Workspace,
    PanelId::Running,
    PanelId::Session,
    PanelId::ToolsSkills,
    PanelId::RecentActivity,
    PanelId::SessionStats,
];
const RIGHT: [PanelId; 4] = [
    PanelId::Topology,
    PanelId::UsageMetrics,
    PanelId::SystemHealth,
    PanelId::Subagents,
];
const CARDS: [PanelId; 2] = [PanelId::AgentStatus, PanelId::Context];

/// The optional panels the Panels view stacks: the left ones, then the right ones.
fn panels_view_ids() -> Vec<PanelId> {
    LEFT_OPTIONAL[2..]
        .iter()
        .chain(&RIGHT[..3])
        .copied()
        .collect()
}

/// Cards above the chat in a centre column of `w` x `h` at (`x`, `y`): returns the placed cards
/// and the chat rectangle.
fn centre(
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    on: &Panels,
    out: &mut Vec<Placed>,
    starved: &mut Vec<PanelId>,
) -> Rect {
    let want: Vec<PanelId> = CARDS.iter().copied().filter(|id| on.is_on(*id)).collect();
    let top_h = if h >= 30 {
        5
    } else if h >= 20 {
        4
    } else {
        0
    };
    if want.is_empty() || top_h == 0 || top_h + 1 + CHAT_MIN_H > h {
        starved.extend(want);
        return Rect { x, y, w, h };
    }
    // side by side; below 60 columns only the more important card (higher drop number) stays
    let mut shown = want.clone();
    if w < 60 && shown.len() == 2 {
        starved.push(PanelId::Context);
        shown.retain(|i| *i != PanelId::Context);
    }
    let gap = 2;
    if shown.len() == 2 {
        let cw = clamp(w * 38 / 100, 24, 40);
        let aw = w - cw - gap;
        out.push(Placed {
            id: PanelId::AgentStatus,
            rect: Rect {
                x,
                y,
                w: aw,
                h: top_h,
            },
        });
        out.push(Placed {
            id: PanelId::Context,
            rect: Rect {
                x: x + aw + gap,
                y,
                w: cw,
                h: top_h,
            },
        });
    } else {
        out.push(Placed {
            id: shown[0],
            rect: Rect { x, y, w, h: top_h },
        });
    }
    Rect {
        x,
        y: y + top_h + 1,
        w,
        h: h - top_h - 1,
    }
}

/// The layout of a `w` x `h` frame. `view` only matters in Tabs mode.
pub fn plan(w: i64, h: i64, on: &Panels, view: View, demand: &Demand) -> Plan {
    let mut pl = Plan {
        w,
        h,
        mode: Mode::TooSmall,
        view: View::Chat,
        header_rows: 0,
        tab_row: None,
        tabs: vec![],
        bottom_row: h - 1,
        body: Rect::default(),
        left: Rect::default(),
        right: Rect::default(),
        chat: None,
        panels: vec![],
        starved: vec![],
    };
    if w < MIN_COLS || h < MIN_ROWS {
        return pl;
    }
    let header = 1 + i64::from(h >= 30);
    pl.header_rows = header;
    pl.mode = if w >= COLUMNS_FROM {
        Mode::Columns
    } else if w >= CHAT_ONLY_BELOW {
        Mode::Tabs
    } else {
        Mode::ChatOnly
    };
    let tab = i64::from(pl.mode == Mode::Tabs);
    let body_y = header + tab;
    let body_h = h - header - 1 - tab;
    pl.body = Rect {
        x: 0,
        y: body_y,
        w,
        h: body_h,
    };
    let mut starved: Vec<PanelId> = vec![];
    match pl.mode {
        Mode::Columns => {
            let (l, r) = column_widths(w);
            pl.left = Rect {
                x: 0,
                y: body_y,
                w: l,
                h: body_h,
            };
            pl.right = Rect {
                x: w - r,
                y: body_y,
                w: r,
                h: body_h,
            };
            let left = fit(&LEFT_OPTIONAL, l - 1, body_h, on, demand);
            place(&left, 0, body_y, l - 1, &mut pl.panels);
            starved.extend(left.starved);
            let right = fit(&RIGHT, r - 1, body_h, on, demand);
            place(&right, w - r + 1, body_y, r - 1, &mut pl.panels);
            starved.extend(right.starved);
            pl.chat = Some(centre(
                l,
                body_y,
                w - l - r,
                body_h,
                on,
                &mut pl.panels,
                &mut starved,
            ));
        }
        Mode::Tabs => {
            let ids = panels_view_ids();
            let any_panel = ids.iter().any(|id| on.is_on(*id));
            pl.tabs = vec![View::Chat, View::Sessions, View::Agents];
            if any_panel {
                pl.tabs.push(View::Panels);
            }
            pl.view = if pl.tabs.contains(&view) {
                view
            } else {
                View::Chat
            };
            pl.tab_row = Some(header);
            match pl.view {
                View::Chat => {
                    // the compact status line (1 row) instead of the card row
                    let mut chat = pl.body;
                    if on.is_on(PanelId::AgentStatus) && body_h >= 14 {
                        pl.panels.push(Placed {
                            id: PanelId::AgentStatus,
                            rect: Rect {
                                x: 0,
                                y: body_y,
                                w,
                                h: 1,
                            },
                        });
                        chat = Rect {
                            x: 0,
                            y: body_y + 1,
                            w,
                            h: body_h - 1,
                        };
                    } else if on.is_on(PanelId::AgentStatus) {
                        starved.push(PanelId::AgentStatus);
                    }
                    if on.is_on(PanelId::Context) {
                        starved.push(PanelId::Context); // its numbers are in the header chip
                    }
                    pl.chat = Some(chat);
                }
                View::Sessions => {
                    let f = fit(&LEFT_OPTIONAL[..2], w, body_h, &Panels::none(), demand);
                    place(&f, 0, body_y, w, &mut pl.panels);
                }
                View::Agents => pl.panels.push(Placed {
                    id: PanelId::Subagents,
                    rect: pl.body,
                }),
                View::Panels => {
                    let f = fit(&ids, w, body_h, on, demand);
                    place(&f, 0, body_y, w, &mut pl.panels);
                    starved.extend(f.starved);
                }
            }
        }
        Mode::ChatOnly => pl.chat = Some(pl.body),
        Mode::TooSmall => {}
    }
    // everything that is on and not drawn: the other views' panels are not starved, only the ones
    // this size cannot show
    starved.sort();
    starved.dedup();
    pl.starved = starved;
    pl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masks() -> Vec<Panels> {
        let mut v = vec![Panels::default(), Panels::none(), Panels::all()];
        // each hideable panel hidden alone, and only that one on
        for s in REGISTRY.iter().filter(|s| s.locked.is_none()) {
            let mut p = Panels::all();
            p.set(s.id, false);
            v.push(p);
            let mut p = Panels::none();
            p.set(s.id, true);
            v.push(p);
        }
        v
    }

    /// No overlap, inside the body, tiles every column with at most gap rows between panels.
    fn check(pl: &Plan, tag: &str) {
        if pl.mode == Mode::TooSmall {
            return;
        }
        let mut all: Vec<(PanelId, Rect)> = pl.panels.iter().map(|p| (p.id, p.rect)).collect();
        if let Some(c) = pl.chat {
            all.push((PanelId::Chat, c));
        }
        for (id, r) in &all {
            assert!(r.w > 0 && r.h > 0, "{tag}: {id:?} empty {r:?}");
            assert!(
                r.x >= 0 && r.y >= pl.body.y && r.right() <= pl.w && r.bottom() <= pl.body.bottom(),
                "{tag}: {id:?} outside the body {r:?} {:?}",
                pl.body
            );
        }
        for (i, (a, ra)) in all.iter().enumerate() {
            for (b, rb) in &all[i + 1..] {
                let apart = ra.right() <= rb.x
                    || rb.right() <= ra.x
                    || ra.bottom() <= rb.y
                    || rb.bottom() <= ra.y;
                assert!(apart, "{tag}: {a:?} {ra:?} overlaps {b:?} {rb:?}");
            }
        }
        // sidebars and the full-width views: panels sharing an x stack from the top of the body to its
        // bottom with at most one gap row between them
        let is_card = |id: PanelId| matches!(id, PanelId::AgentStatus | PanelId::Context);
        let mut xs: Vec<i64> = pl
            .panels
            .iter()
            .filter(|p| !is_card(p.id))
            .map(|p| p.rect.x)
            .collect();
        xs.sort();
        xs.dedup();
        for x in xs {
            let mut col: Vec<&Placed> = pl
                .panels
                .iter()
                .filter(|p| p.rect.x == x && !is_card(p.id))
                .collect();
            col.sort_by_key(|p| p.rect.y);
            assert_eq!(col[0].rect.y, pl.body.y, "{tag}: column {x} starts late");
            for pair in col.windows(2) {
                let gap = pair[1].rect.y - pair[0].rect.bottom();
                assert!(
                    (0..=1).contains(&gap),
                    "{tag}: hole of {gap} rows after {:?}",
                    pair[0].id
                );
            }
            let last = col.last().unwrap();
            assert_eq!(
                last.rect.bottom(),
                pl.body.bottom(),
                "{tag}: column {x} ends early: {:?}",
                last.id
            );
        }
        // the chat is the rest of the centre column, under the cards
        if let Some(c) = pl.chat {
            assert_eq!(c.bottom(), pl.body.bottom(), "{tag}: chat ends early");
            let cards: Vec<&Placed> = pl.panels.iter().filter(|p| is_card(p.id)).collect();
            if cards.is_empty() {
                assert_eq!(c.y, pl.body.y, "{tag}: hole above the chat");
            } else {
                assert_eq!(cards[0].rect.y, pl.body.y, "{tag}: cards start late");
                let want = cards[0].rect.bottom() + i64::from(pl.mode == Mode::Columns);
                assert_eq!(c.y, want, "{tag}: gap above the chat");
            }
            if pl.mode == Mode::Columns {
                assert!(c.w >= CHAT_MIN_W, "{tag}: chat {} wide", c.w);
            }
        }
    }

    #[test]
    fn plans_tile_without_overlap_or_holes_over_every_size() {
        let ms = masks();
        for w in 40..=260 {
            for h in 11..=80 {
                for (i, m) in ms.iter().enumerate() {
                    for view in [View::Chat, View::Sessions, View::Agents, View::Panels] {
                        let pl = plan(w, h, m, view, &Demand::default());
                        check(&pl, &format!("{w}x{h} mask {i} {view:?}"));
                    }
                }
            }
        }
    }

    #[test]
    fn every_mask_at_a_few_sizes() {
        for &(w, h) in &[
            (100, 16),
            (120, 36),
            (200, 50),
            (80, 24),
            (176, 30),
            (140, 22),
        ] {
            for mask in 0..1u32 << hideable_count() {
                let pl = plan(
                    w,
                    h,
                    &Panels::from_mask(mask),
                    View::Chat,
                    &Demand::default(),
                );
                check(&pl, &format!("{w}x{h} mask {mask:b}"));
                // 4: hiding one more optional panel never shortens a locked panel unless a panel
                // that was dropped for lack of room comes back
                for s in REGISTRY.iter().filter(|s| s.locked.is_none()) {
                    let mut less = Panels::from_mask(mask);
                    if !less.is_on(s.id) {
                        continue;
                    }
                    less.set(s.id, false);
                    let q = plan(w, h, &less, View::Chat, &Demand::default());
                    let placed = |p: &Plan| p.panels.iter().map(|x| x.id).collect::<Vec<_>>();
                    let (a, b) = (placed(&pl), placed(&q));
                    if pl.mode == Mode::Columns && b.iter().all(|id| a.contains(id)) {
                        for id in [PanelId::Workspace, PanelId::Running, PanelId::Subagents] {
                            assert!(
                                q.rect(id).unwrap().h >= pl.rect(id).unwrap().h,
                                "{w}x{h} mask {mask:b}: hiding {:?} shortened {id:?}",
                                s.id
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn locked_panels_are_always_present_and_subagents_sit_on_the_right() {
        let ms = masks();
        for w in 100..=260 {
            for h in 11..=80 {
                for m in &ms {
                    let pl = plan(w, h, m, View::Chat, &Demand::default());
                    for (id, min) in [
                        (PanelId::Workspace, 1),
                        (PanelId::Running, 1),
                        (PanelId::Subagents, 1),
                    ] {
                        let r = pl
                            .rect(id)
                            .unwrap_or_else(|| panic!("{w}x{h}: {id:?} missing"));
                        assert!(r.h >= min);
                    }
                    assert!(pl.chat.is_some());
                    let sa = pl.rect(PanelId::Subagents).unwrap();
                    assert_eq!(
                        sa.x,
                        pl.right.x + 1,
                        "{w}x{h}: sub-agents not in the right sidebar"
                    );
                    // locked panels get their minimum whenever the column can give it
                    let body = pl.body.h;
                    if body > 2 + 3 {
                        assert!(pl.rect(PanelId::Workspace).unwrap().h >= 2, "{w}x{h}");
                        assert!(pl.rect(PanelId::Running).unwrap().h >= 3, "{w}x{h}");
                    }
                    if body >= 6 {
                        assert!(sa.h >= 6, "{w}x{h}: sub-agents {}", sa.h);
                    }
                }
            }
        }
        // Tabs and chat-only modes keep the locked panels reachable
        for w in 40..100 {
            for h in 11..=40 {
                let pl = plan(w, h, &Panels::none(), View::Agents, &Demand::default());
                if pl.mode == Mode::Tabs {
                    assert_eq!(pl.rect(PanelId::Subagents), Some(pl.body));
                    let s = plan(w, h, &Panels::none(), View::Sessions, &Demand::default());
                    assert!(
                        s.rect(PanelId::Workspace).is_some() && s.rect(PanelId::Running).is_some()
                    );
                }
                if pl.mode == Mode::ChatOnly {
                    assert_eq!(pl.chat, Some(pl.body));
                    assert!(pl.panels.is_empty());
                }
            }
        }
    }

    #[test]
    fn equal_split_when_no_optional_left_panel_is_on() {
        for w in 100..=260 {
            for h in 11..=80 {
                // optional right panels do not matter
                for m in [Panels::none(), Panels::default()] {
                    let mut m = m;
                    for id in [
                        PanelId::Session,
                        PanelId::ToolsSkills,
                        PanelId::RecentActivity,
                        PanelId::SessionStats,
                    ] {
                        m.set(id, false);
                    }
                    let pl = plan(w, h, &m, View::Chat, &Demand::default());
                    let (ws, run) = (
                        pl.rect(PanelId::Workspace).unwrap(),
                        pl.rect(PanelId::Running).unwrap(),
                    );
                    let gap = run.y - ws.bottom();
                    assert_eq!(ws.h + gap + run.h, pl.body.h, "{w}x{h}");
                    assert!(
                        run.h - ws.h <= 1 && run.h >= ws.h,
                        "{w}x{h}: {} vs {}",
                        ws.h,
                        run.h
                    );
                    assert_eq!(ws.h, (pl.body.h - gap) / 2);
                }
            }
        }
    }

    #[test]
    fn subagents_take_the_whole_right_column_when_alone() {
        for w in 100..=260 {
            for h in 11..=80 {
                let pl = plan(w, h, &Panels::none(), View::Chat, &Demand::default());
                let sa = pl.rect(PanelId::Subagents).unwrap();
                assert_eq!((sa.y, sa.h), (pl.body.y, pl.body.h), "{w}x{h}");
            }
        }
    }

    #[test]
    fn plans_are_deterministic_and_a_resize_back_restores_the_plan() {
        let m = Panels::default();
        let d = Demand::default();
        let a = plan(120, 36, &m, View::Chat, &d);
        let _ = plan(80, 24, &m, View::Chat, &d);
        let _ = plan(200, 50, &m, View::Panels, &d);
        assert_eq!(a, plan(120, 36, &m, View::Chat, &d));
    }

    #[test]
    fn worked_examples() {
        let d = Demand::default();
        // everything hidden: ws and running split the 120x36 body (33 rows) 16 + gap + 16
        let pl = plan(120, 36, &Panels::none(), View::Chat, &d);
        assert_eq!(pl.body.h, 33);
        let h = |p: &Plan, id| p.rect(id).unwrap().h;
        assert_eq!(
            (h(&pl, PanelId::Workspace), h(&pl, PanelId::Running)),
            (16, 16)
        );
        assert_eq!(h(&pl, PanelId::Subagents), 33);
        // 100x16: body 14, default left: the optional panels that do not fit are dropped, never a locked one
        let pl = plan(100, 16, &Panels::default(), View::Chat, &d);
        assert_eq!(pl.body.h, 14);
        assert!(pl.rect(PanelId::Workspace).is_some() && pl.rect(PanelId::Running).is_some());
        // narrow modes
        assert_eq!(
            plan(99, 24, &Panels::default(), View::Chat, &d).mode,
            Mode::Tabs
        );
        assert_eq!(
            plan(60, 24, &Panels::default(), View::Chat, &d).mode,
            Mode::Tabs
        );
        assert_eq!(
            plan(59, 24, &Panels::default(), View::Chat, &d).mode,
            Mode::ChatOnly
        );
        assert_eq!(
            plan(39, 24, &Panels::default(), View::Chat, &d).mode,
            Mode::TooSmall
        );
        assert_eq!(
            plan(40, 10, &Panels::default(), View::Chat, &d).mode,
            Mode::TooSmall
        );
        // the topology needs a wide sidebar: starved at 120, placed at 200
        assert!(
            plan(120, 50, &Panels::default(), View::Chat, &d)
                .starved
                .contains(&PanelId::Topology)
        );
        assert!(
            plan(200, 50, &Panels::default(), View::Chat, &d)
                .rect(PanelId::Topology)
                .is_some()
        );
        // Panels tab disappears when no optional panel is on
        assert!(
            !plan(80, 24, &Panels::none(), View::Panels, &d)
                .tabs
                .contains(&View::Panels)
        );
        assert!(
            plan(80, 24, &Panels::default(), View::Panels, &d)
                .tabs
                .contains(&View::Panels)
        );
    }

    #[test]
    fn locked_panels_ignore_set_and_the_registry_is_indexed_by_id() {
        let mut p = Panels::default();
        for id in [
            PanelId::Workspace,
            PanelId::Running,
            PanelId::Chat,
            PanelId::Subagents,
        ] {
            p.set(id, false);
            assert!(p.is_on(id));
        }
        for (i, s) in REGISTRY.iter().enumerate() {
            assert_eq!(s.id as usize, i);
            assert_eq!(PanelId::parse(s.key), Some(s.id));
        }
    }
}
