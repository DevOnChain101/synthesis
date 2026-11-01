//! Topology / flow canvas: port of `topology.js` (node states, z-ordered character canvas, wire
//! glyphs, conveyor animation) and `flow.js` (phase bar, cards, ring / stack / fan / list layouts).

use crate::model::{AgentRec, Component, Phase, Session, Tool, session_phase};
use crate::theme::*;
use crate::util::{basename, one_line, to_fixed};
use std::collections::BTreeSet;

const DISPATCH_MS: i64 = 1800;
const RETURN_HOLD_MS: i64 = 6000;
const COMP_DISPATCH_MS: i64 = 600;
const COMP_HOLD_MS: i64 = 2500;
const CONVEYOR_MS: i64 = 50;
const SLATE: Rgb = [176, 190, 210];
const DIM: Rgb = WARM_DIM;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum St {
    Dispatch,
    Working,
    Returning,
    Failed,
    Done,
}
pub fn is_active(s: St) -> bool {
    matches!(s, St::Dispatch | St::Working | St::Returning)
}
pub fn is_busy(s: St) -> bool {
    matches!(s, St::Dispatch | St::Working)
}

fn agent_state(a: &AgentRec, now: i64) -> St {
    if a.status == "running" {
        return if now - a.start.unwrap_or(0) < DISPATCH_MS {
            St::Dispatch
        } else {
            St::Working
        };
    }
    if let Some(e) = a.end.filter(|e| *e != 0)
        && now - e < RETURN_HOLD_MS
    {
        return St::Returning;
    }
    if a.status == "failed" {
        St::Failed
    } else {
        St::Done
    }
}

fn component_state(c: &Component, now: i64) -> St {
    if c.open > 0 {
        return if now - c.last_start.unwrap_or(0) < COMP_DISPATCH_MS {
            St::Dispatch
        } else {
            St::Working
        };
    }
    if let Some(e) = c.last_end.filter(|e| *e != 0)
        && now - e < COMP_HOLD_MS
    {
        return St::Returning;
    }
    if c.error { St::Failed } else { St::Done }
}

pub enum NodeRef<'a> {
    Agent(&'a AgentRec),
    Comp(&'a Component),
}

pub struct Node<'a> {
    pub id: String,
    pub kind: String,
    pub r: NodeRef<'a>,
    pub st: St,
    pub last: i64,
    pub col: Rgb,
}

fn kind_icon(k: &str) -> &'static str {
    match k {
        "agent" => "●",
        "skill" => "★",
        "mcp" => "⬡",
        "hook" => "↯",
        "cmd" => "/",
        _ => "▣",
    }
}
fn kind_col(k: &str) -> Rgb {
    match k {
        "agent" => T_AUTO_ACCEPT,
        "skill" => T_PLAN_MODE,
        "mcp" => T_MCP,
        "hook" => T_WARNING,
        "cmd" => T_BASH,
        _ => T_PERMISSION,
    }
}

/// All agents and components (every tool, as the dashboard asks for).
pub fn collect_nodes(s: &Session, now: i64) -> Vec<Node<'_>> {
    let mut nodes = vec![];
    for a in s.agents.values() {
        let last = a
            .start
            .unwrap_or(0)
            .max(a.end.unwrap_or(0))
            .max(a.last_activity.unwrap_or(0));
        nodes.push(Node {
            id: format!("agent:{}", a.tool_use_id),
            kind: "agent".into(),
            st: agent_state(a, now),
            last,
            col: agent_color(&a.kind),
            r: NodeRef::Agent(a),
        });
    }
    for c in s.components.values() {
        nodes.push(Node {
            id: c.key.clone(),
            kind: c.kind.clone(),
            st: component_state(c, now),
            last: c.last_start.unwrap_or(0).max(c.last_end.unwrap_or(0)),
            col: kind_col(&c.kind),
            r: NodeRef::Comp(c),
        });
    }
    nodes
}

// ---------------------------------------------------------------- canvas

#[derive(Clone)]
struct Cell {
    ch: char,
    col: Option<Rgb>,
    z: i32,
}

pub struct Canvas {
    w: i64,
    h: i64,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas {
            w: w as i64,
            h: h as i64,
            cells: vec![
                Cell {
                    ch: ' ',
                    col: None,
                    z: 0
                };
                w * h
            ],
        }
    }
    fn idx(&self, x: i64, y: i64) -> Option<usize> {
        (x >= 0 && y >= 0 && x < self.w && y < self.h).then(|| (y * self.w + x) as usize)
    }
    fn set(&mut self, x: i64, y: i64, ch: char, col: Option<Rgb>, z: i32) {
        if let Some(i) = self.idx(x, y)
            && z >= self.cells[i].z
        {
            self.cells[i] = Cell { ch, col, z };
        }
    }
    fn tint(&mut self, x: i64, y: i64, col: Rgb, z: i32) {
        if let Some(i) = self.idx(x, y)
            && z >= self.cells[i].z
        {
            self.cells[i].col = Some(col);
            self.cells[i].z = z;
        }
    }
    fn text(&mut self, x: i64, y: i64, s: &str, col: Rgb, z: i32) {
        for (i, ch) in s.chars().enumerate() {
            self.set(x + i as i64, y, ch, Some(col), z);
        }
    }
    fn rows(&self) -> Vec<String> {
        (0..self.h)
            .map(|y| {
                let mut s = String::new();
                for x in 0..self.w {
                    let c = &self.cells[(y * self.w + x) as usize];
                    match c.col {
                        None => s.push(c.ch),
                        Some(rgb) => {
                            s.push_str(&fg(rgb));
                            s.push(c.ch);
                            s.push_str(RESET);
                        }
                    }
                }
                s
            })
            .collect()
    }
}

const U: u8 = 1;
const D: u8 = 2;
const L: u8 = 4;
const R: u8 = 8;

fn glyph(bits: u8, heavy: bool) -> char {
    let (l, h) = match bits {
        x if x == L | R || x == L || x == R => ('─', '━'),
        x if x == U | D || x == U || x == D => ('│', '┃'),
        x if x == D | R => ('┌', '┏'),
        x if x == D | L => ('┐', '┓'),
        x if x == U | R => ('└', '┗'),
        x if x == U | L => ('┘', '┛'),
        x if x == U | D | R => ('├', '┣'),
        x if x == U | D | L => ('┤', '┫'),
        x if x == L | R | D => ('┬', '┳'),
        x if x == L | R | U => ('┴', '┻'),
        _ => ('┼', '╋'),
    };
    if heavy { h } else { l }
}

type Pt = (i64, i64);

fn dir_to(a: Pt, b: Pt) -> u8 {
    if b.0 > a.0 {
        R
    } else if b.0 < a.0 {
        L
    } else if b.1 > a.1 {
        D
    } else {
        U
    }
}

fn run(mut pts: Vec<Pt>, x: i64, y: i64) -> Vec<Pt> {
    let (mut cx, mut cy) = *pts.last().unwrap();
    while cx != x || cy != y {
        if cx != x {
            cx += (x - cx).signum();
        } else {
            cy += (y - cy).signum();
        }
        pts.push((cx, cy));
    }
    pts
}

fn draw_box(cv: &mut Canvas, x: i64, y: i64, w: i64, h: i64, col: Rgb, heavy: bool) {
    let (tl, tr, bl, br, hh, v) = if heavy {
        ('┏', '┓', '┗', '┛', '━', '┃')
    } else {
        ('╭', '╮', '╰', '╯', '─', '│')
    };
    for i in 1..w - 1 {
        cv.set(x + i, y, hh, Some(col), 5);
        cv.set(x + i, y + h - 1, hh, Some(col), 5);
    }
    for j in 1..h - 1 {
        cv.set(x, y + j, v, Some(col), 5);
        cv.set(x + w - 1, y + j, v, Some(col), 5);
        for i in 1..w - 1 {
            cv.set(x + i, y + j, ' ', None, 5);
        }
    }
    cv.set(x, y, tl, Some(col), 5);
    cv.set(x + w - 1, y, tr, Some(col), 5);
    cv.set(x, y + h - 1, bl, Some(col), 5);
    cv.set(x + w - 1, y + h - 1, br, Some(col), 5);
}

fn conveyor(cv: &mut Canvas, path: &[Pt], col: Rgb, reverse: bool, now: i64) {
    let len = path.len() as i64;
    let step = now.div_euclid(CONVEYOR_MS);
    for (k, &(x, y)) in path.iter().enumerate() {
        let pos = if reverse {
            len - 1 - k as i64
        } else {
            k as i64
        };
        let ph = ((pos - step) % 6 + 6) % 6;
        if cv.idx(x, y).is_none() {
            continue;
        }
        if ph == 0 {
            cv.set(x, y, '●', Some(WARM_CREAM), 3);
        } else {
            cv.tint(x, y, col, 3);
        }
    }
}

fn blink_on(now: i64) -> bool {
    now.div_euclid(420) % 2 == 0
}

// ---------------------------------------------------------------- flow

struct Phase_ {
    key: &'static str,
    label: &'static str,
    short: &'static str,
    desc: &'static str,
    col: Rgb,
}
const PHASES: [Phase_; 6] = [
    Phase_ {
        key: "plan",
        label: "Plan",
        short: "Plan",
        desc: "understand & plan",
        col: BLUE,
    },
    Phase_ {
        key: "guard",
        label: "Guard",
        short: "Guard",
        desc: "check policies & safety",
        col: GREEN,
    },
    Phase_ {
        key: "orchestrate",
        label: "Orchestrate",
        short: "Orch",
        desc: "select skills / sub-agents",
        col: CYAN,
    },
    Phase_ {
        key: "execute",
        label: "Execute",
        short: "Exec",
        desc: "run tools & code",
        col: TEAL,
    },
    Phase_ {
        key: "verify",
        label: "Verify",
        short: "Verify",
        desc: "check, test, review",
        col: YELLOW,
    },
    Phase_ {
        key: "learn",
        label: "Learn",
        short: "Learn",
        desc: "update memory",
        col: PURPLE,
    },
];

const RING: [(&str, &str); 8] = [
    ("tl", "user"),
    ("t", "guards"),
    ("tr", "skills"),
    ("l", "memory"),
    ("r", "agents"),
    ("bl", "mcp"),
    ("b", "exec"),
    ("br", "hooks"),
];
const FAN: [&str; 8] = [
    "user", "guards", "agents", "skills", "hooks", "exec", "mcp", "memory",
];
const PHASE_H: i64 = 3;
const MIN_CARD_H: i64 = 3;
const RING_MIN_W: i64 = 66;
const RING_WIDE_W: i64 = 88;
const RING_MIN_H: i64 = 2 + 1 + 3 + 1 + 2;
const FAN_MIN_H: i64 = 3 + FAN.len() as i64;
const PROMPT_FLASH_MS: i64 = 5000;
const HOOK_FLASH_MS: i64 = 1200;
const BLOCK_ALERT_MS: i64 = 10_000;
const CARD_MAX_LINES: usize = 4;

fn len(s: &str) -> i64 {
    s.chars().count() as i64
}
fn clip(s: &str, n: i64) -> String {
    if n <= 0 {
        return String::new();
    }
    if len(s) <= n {
        return s.to_string();
    }
    let mut o: String = s.chars().take((n - 1).max(0) as usize).collect();
    o.push('…');
    o
}

fn fmt_num(n: f64) -> String {
    if n >= 1e6 {
        format!("{}M", to_fixed(n / 1e6, 1))
    } else if n >= 1e3 {
        format!("{}k", to_fixed(n / 1e3, 1))
    } else {
        crate::util::round_str(n)
    }
}

fn ago(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", (ms as f64 / 1000.0).floor().max(0.0) as i64)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}h", ms / 3_600_000)
    }
}

fn is_guard(c: &Component) -> bool {
    let s = c
        .event
        .as_deref()
        .filter(|e| !e.is_empty())
        .unwrap_or(&c.name);
    ["PreToolUse", "UserPromptSubmit", "PermissionRequest"]
        .iter()
        .any(|p| {
            s.strip_prefix(p).is_some_and(|rest| {
                !rest.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
            })
        })
}

struct PhaseState<'a> {
    current: Option<String>,
    seen: BTreeSet<String>,
    current_tool: Option<&'a Tool>,
}

fn phase_state(s: &Session, now: i64) -> PhaseState<'_> {
    let since = s.last_prompt_at.unwrap_or(0);
    let mut seen = BTreeSet::new();
    let mut current_tool: Option<&Tool> = None;
    for t in s.tools.values() {
        if t.start.unwrap_or(0) >= since {
            seen.insert(t.cat.to_string());
        }
        if t.end.is_none()
            && current_tool.is_none_or(|c| t.start.unwrap_or(0) > c.start.unwrap_or(0))
        {
            current_tool = Some(t);
        }
    }
    let mut last_guard = 0;
    for c in s.components.values() {
        let Some(le) = c.last_end.filter(|e| *e != 0) else {
            continue;
        };
        if c.kind != "hook" || !is_guard(c) {
            continue;
        }
        if le >= since {
            seen.insert("guard".into());
        }
        last_guard = last_guard.max(le);
    }
    let agents_busy = s.agents.values().any(|a| a.status == "running");
    let current = if last_guard != 0 && now - last_guard < 900 {
        Some("guard".to_string())
    } else if let Some(t) = current_tool {
        Some(t.cat.to_string())
    } else if agents_busy {
        Some("orchestrate".into())
    } else if session_phase(s, now).phase == Phase::Thinking {
        Some("plan".into())
    } else {
        None
    };
    if let Some(c) = &current {
        seen.remove(c);
    }
    PhaseState {
        current,
        seen,
        current_tool,
    }
}

type Seg = (String, Rgb, bool);
#[derive(Clone, Default)]
struct Line {
    left: Vec<Seg>,
    right: Option<Seg>,
}
fn seg(t: impl Into<String>, c: Rgb) -> Seg {
    (t.into(), c, false)
}
fn segb(t: impl Into<String>, c: Rgb) -> Seg {
    (t.into(), c, true)
}

#[derive(Clone, Default)]
struct Card {
    title: &'static str,
    short: &'static str,
    col: Rgb,
    lines: Vec<Line>,
    busy: bool,
    returning: bool,
    error: bool,
    badge: String,
}

fn card_meta(id: &str) -> (&'static str, &'static str, Rgb) {
    match id {
        "user" => ("USER · PROJECT", "USER", SLATE),
        "core" => ("SYNTHESIS CORE", "CORE", T_CLAUDE),
        "guards" => ("GUARDS", "GUARDS", RED),
        "skills" => ("SKILLS", "SKILLS", kind_col("skill")),
        "agents" => ("SUB-AGENTS", "AGENTS", kind_col("agent")),
        "hooks" => ("HOOKS", "HOOKS", kind_col("hook")),
        "mcp" => ("EXTERNAL · MCP", "MCP", kind_col("mcp")),
        "exec" => ("EXECUTION", "EXEC", kind_col("tool")),
        _ => ("MEMORY · FEEDBACK", "MEMORY", PINK),
    }
}

fn node_line(n: &Node, frame: usize, now: i64, i: usize, name: &str) -> Line {
    let busy = is_busy(n.st);
    let active = is_active(n.st);
    let mut g = kind_icon(&n.kind);
    let mut gcol = lerp(n.col, DIM, 0.45);
    let comp_err = matches!(n.r, NodeRef::Comp(c) if c.error);
    if busy {
        g = spin(frame + i);
        gcol = n.col;
    } else if n.st == St::Returning {
        g = "«";
        gcol = if comp_err { RED } else { GREEN };
    } else if n.st == St::Failed {
        g = "✗";
        gcol = RED;
    }
    let mut left = vec![
        segb(format!("{g} "), gcol),
        (
            name.to_string(),
            if active { WARM_CREAM } else { T_BODY },
            active,
        ),
    ];
    if let NodeRef::Agent(a) = &n.r
        && !a.description.is_empty()
    {
        left.push(seg(format!(" · {}", one_line(&a.description)), T_SUBTLE));
    }
    let right = if busy {
        let detail = match &n.r {
            NodeRef::Agent(a) => a.current_tool.as_ref().map_or("working…".to_string(), |t| {
                format!("{} {}", t.name, t.summary)
            }),
            NodeRef::Comp(c) => {
                if c.sub.is_empty() {
                    "running".into()
                } else {
                    c.sub.clone()
                }
            }
        };
        seg(one_line(&detail), n.col)
    } else if n.st == St::Returning {
        let t = match &n.r {
            NodeRef::Agent(a) => format!("« {} tok", fmt_num(a.tokens.total() as f64)),
            NodeRef::Comp(_) => "« returned".into(),
        };
        seg(t, if comp_err { RED } else { GREEN })
    } else if n.st == St::Failed {
        seg("failed", RED)
    } else {
        match &n.r {
            NodeRef::Agent(a) => seg(
                format!(
                    "{} · {} tok",
                    ago(a.end.filter(|e| *e != 0).unwrap_or(now)
                        - a.start.filter(|e| *e != 0).unwrap_or(now)),
                    fmt_num(a.tokens.total() as f64)
                ),
                T_SUBTLE,
            ),
            NodeRef::Comp(c) => seg(
                format!(
                    "×{} · {}",
                    c.count,
                    ago(now - if n.last != 0 { n.last } else { now })
                ),
                T_SUBTLE,
            ),
        }
    };
    Line {
        left,
        right: Some(right),
    }
}

fn rank<'a, 'b>(nodes: &'b [&'b Node<'a>]) -> Vec<&'b Node<'a>> {
    let w = |n: &Node| {
        if is_busy(n.st) {
            0
        } else if is_active(n.st) {
            1
        } else {
            2
        }
    };
    let mut v: Vec<&Node> = nodes.to_vec();
    v.sort_by(|a, b| {
        w(a).cmp(&w(b))
            .then(b.last.cmp(&a.last))
            .then(a.id.cmp(&b.id))
    });
    v
}

fn default_name(n: &Node) -> String {
    match &n.r {
        NodeRef::Agent(a) => a.kind.clone(),
        NodeRef::Comp(c) => c.name.clone(),
    }
}

fn list_card(
    nodes: &[&Node],
    frame: usize,
    now: i64,
    empty: &str,
    name_of: &dyn Fn(&Node) -> String,
) -> Card {
    let lines = if nodes.is_empty() {
        vec![Line {
            left: vec![seg(empty, T_SUBTLE)],
            right: None,
        }]
    } else {
        rank(nodes)
            .iter()
            .enumerate()
            .map(|(i, n)| node_line(n, frame, now, i, &name_of(n)))
            .collect()
    };
    let busy_n = nodes.iter().filter(|n| is_busy(n.st)).count();
    Card {
        lines,
        busy: busy_n > 0,
        returning: nodes.iter().any(|n| n.st == St::Returning),
        badge: if nodes.is_empty() {
            String::new()
        } else if busy_n > 0 {
            format!("{busy_n} active / {}", nodes.len())
        } else {
            nodes.len().to_string()
        },
        ..Default::default()
    }
}

fn ctx_bar(pct: f64, n: usize) -> String {
    let filled = js_round(pct.clamp(0.0, 100.0) / 100.0 * n as f64) as usize;
    format!("{}{}", "▰".repeat(filled), "▱".repeat(n - filled))
}

#[derive(Clone, Copy, Default)]
pub struct FlowOpts {
    pub context_size: Option<f64>,
    pub ctx_pct: Option<f64>,
}

fn level_col(pct: f64) -> Rgb {
    if pct >= 85.0 {
        RED
    } else if pct >= 60.0 {
        YELLOW
    } else {
        GREEN
    }
}

fn build_cards(
    s: &Session,
    nodes: &[Node],
    ps: &PhaseState,
    frame: usize,
    now: i64,
    opts: FlowOpts,
) -> Vec<(&'static str, Card)> {
    let by_kind = |kinds: &[&str]| -> Vec<&Node> {
        nodes
            .iter()
            .filter(|n| kinds.contains(&n.kind.as_str()))
            .collect()
    };
    let hooks = by_kind(&["hook"]);
    let guards: Vec<&Node> = hooks
        .iter()
        .copied()
        .filter(|n| is_guard(comp(n)))
        .collect();
    let events: Vec<&Node> = hooks
        .iter()
        .copied()
        .filter(|n| !is_guard(comp(n)))
        .collect();
    let todos = &s.todos;
    let done = todos.iter().filter(|t| t.status == "completed").count();
    let doing = todos.iter().find(|t| t.status == "in_progress");
    let ctx_max = opts.context_size.filter(|c| *c != 0.0).unwrap_or(200_000.0);
    let ctx_pct = opts
        .ctx_pct
        .unwrap_or(s.context_used as f64 / ctx_max * 100.0)
        .min(100.0);
    let stage = |k: &str| ps.current.as_deref() == Some(k);
    let stage_glyph = |on: bool, i: usize| {
        if on {
            spin(frame + i).to_string()
        } else {
            "·".to_string()
        }
    };
    let mut cards: Vec<(&'static str, Card)> = vec![];

    let prompted = s
        .last_prompt_at
        .is_some_and(|p| p != 0 && now - p < PROMPT_FLASH_MS);
    let lp = one_line(&s.last_prompt);
    let proj = [basename(&s.cwd), s.project.clone(), "?".to_string()]
        .into_iter()
        .find(|p| !p.is_empty())
        .unwrap();
    cards.push((
        "user",
        Card {
            busy: prompted,
            lines: vec![
                Line {
                    left: vec![
                        segb("❯ ", SLATE),
                        seg(
                            if lp.is_empty() {
                                "waiting for a prompt…".to_string()
                            } else {
                                lp.clone()
                            },
                            if lp.is_empty() { T_SUBTLE } else { WARM_CREAM },
                        ),
                    ],
                    right: None,
                },
                Line {
                    left: vec![seg(format!("prompts {}", s.prompts), T_INACTIVE)],
                    right: Some(seg(
                        s.last_prompt_at
                            .filter(|p| *p != 0)
                            .map_or(String::new(), |p| format!("last {} ago", ago(now - p))),
                        T_SUBTLE,
                    )),
                },
                Line {
                    left: vec![seg("project ", T_SUBTLE), seg(proj.clone(), T_BODY)],
                    right: Some(seg(
                        if s.git_branch.is_empty() {
                            String::new()
                        } else {
                            format!("git:{}", s.git_branch)
                        },
                        T_SUBTLE,
                    )),
                },
            ],
            ..Default::default()
        },
    ));

    let agents_busy = by_kind(&["agent"]).iter().filter(|n| is_busy(n.st)).count();
    let skills_busy = by_kind(&["skill", "cmd"])
        .iter()
        .filter(|n| is_busy(n.st))
        .count();
    let intent_on =
        stage("plan") && (todos.is_empty() || now - s.last_prompt_at.unwrap_or(0) < 15_000);
    let plan_on = stage("plan") && !intent_on;
    let orch_on = stage("orchestrate") || agents_busy > 0;
    let row = |on: bool, i: usize, label: &str, text: String, right: String, col: Rgb| Line {
        left: vec![
            segb(
                format!("{} ", stage_glyph(on, i)),
                if on { col } else { DIM },
            ),
            (
                crate::util::pad_end(label, 13),
                if on { WARM_CREAM } else { T_BODY },
                on,
            ),
            seg(text, T_SUBTLE),
        ],
        right: Some(seg(right, if on { col } else { T_SUBTLE })),
    };
    cards.push((
        "core",
        Card {
            busy: ps.current.is_some(),
            badge: s
                .model
                .strip_prefix("claude-")
                .unwrap_or(&s.model)
                .to_string(),
            lines: vec![
                row(
                    intent_on,
                    0,
                    "Intent",
                    "parse request".into(),
                    if intent_on {
                        "reading…".into()
                    } else if s.prompts > 0 {
                        "understood".into()
                    } else {
                        String::new()
                    },
                    BLUE,
                ),
                row(
                    plan_on,
                    1,
                    "Plan",
                    doing.map_or("decompose".to_string(), |d| {
                        one_line(if d.active_form.is_empty() {
                            &d.content
                        } else {
                            &d.active_form
                        })
                    }),
                    if todos.is_empty() {
                        String::new()
                    } else {
                        format!("{done}/{} tasks", todos.len())
                    },
                    BLUE,
                ),
                row(
                    orch_on,
                    2,
                    "Orchestrator",
                    "route work".into(),
                    if agents_busy > 0 || skills_busy > 0 {
                        format!("{agents_busy} agents · {skills_busy} skills")
                    } else {
                        "idle".into()
                    },
                    CYAN,
                ),
                Line {
                    left: vec![
                        seg(
                            format!("ctx {}% ", to_fixed(ctx_pct, 0)),
                            level_or_inactive(ctx_pct),
                        ),
                        seg(
                            format!("· {} tok", fmt_num(s.tokens.total() as f64)),
                            T_SUBTLE,
                        ),
                    ],
                    right: None,
                },
            ],
            ..Default::default()
        },
    ));

    let blocked: u32 = guards.iter().map(|n| comp(n).blocked).sum();
    let mut lb: Vec<&Node> = guards
        .iter()
        .copied()
        .filter(|n| comp(n).last_blocked_at.is_some_and(|b| b != 0))
        .collect();
    lb.sort_by_key(|n| std::cmp::Reverse(comp(n).last_blocked_at));
    let last_block = lb.first().copied();
    let alert =
        last_block.is_some_and(|n| now - comp(n).last_blocked_at.unwrap_or(0) < BLOCK_ALERT_MS);
    let g = list_card(&guards, frame, now, "no guard hooks fired", &|n| {
        comp(n)
            .name
            .strip_prefix("PreToolUse:")
            .map_or(comp(n).name.clone(), |r| format!("pre:{r}"))
    });
    let summary = Line {
        left: vec![seg(format!("{} guards", guards.len()), T_INACTIVE)],
        right: Some(if blocked > 0 {
            segb(format!("{blocked} blocked"), RED)
        } else if !guards.is_empty() {
            seg("all approved", GREEN)
        } else {
            seg("", T_SUBTLE)
        }),
    };
    let mut glines = vec![];
    if alert {
        glines.push(Line {
            left: vec![
                segb("✗ STOP ", RED),
                seg(
                    format!("{} blocked a call", comp(last_block.unwrap()).name),
                    RED,
                ),
            ],
            right: None,
        });
    }
    glines.push(summary);
    if !guards.is_empty() {
        glines.extend(g.lines.clone());
    }
    cards.push((
        "guards",
        Card {
            busy: guards.iter().any(|n| {
                comp(n)
                    .last_end
                    .is_some_and(|e| e != 0 && now - e < HOOK_FLASH_MS)
            }),
            returning: false,
            error: alert,
            lines: glines,
            badge: g.badge,
            ..Default::default()
        },
    ));

    cards.push((
        "skills",
        list_card(
            &by_kind(&["skill", "cmd"]),
            frame,
            now,
            "no skills used yet",
            &default_name,
        ),
    ));
    cards.push((
        "agents",
        list_card(
            &by_kind(&["agent"]),
            frame,
            now,
            "no sub-agents yet",
            &default_name,
        ),
    ));
    let mut ev = list_card(&events, frame, now, "no event hooks fired", &default_name);
    ev.busy = events.iter().any(|n| {
        comp(n)
            .last_end
            .is_some_and(|e| e != 0 && now - e < HOOK_FLASH_MS)
    });
    cards.push(("hooks", ev));
    cards.push((
        "mcp",
        list_card(
            &by_kind(&["mcp"]),
            frame,
            now,
            "no MCP servers called",
            &default_name,
        ),
    ));

    let tools = by_kind(&["tool"]);
    let calls: u32 = tools.iter().map(|n| comp(n).count).sum();
    let mut ex = list_card(&tools, frame, now, "no tool calls yet", &default_name);
    let mut elines = vec![Line {
        left: vec![seg(format!("{calls} calls"), T_INACTIVE)],
        right: Some(if s.errors > 0 {
            seg(format!("{} errors", s.errors), RED)
        } else {
            seg("no errors", T_SUBTLE)
        }),
    }];
    if !tools.is_empty() {
        elines.extend(ex.lines.clone());
    }
    ex.lines = elines;
    cards.push(("exec", ex));

    let learning = ps
        .current_tool
        .is_some_and(|t| t.cat == "verify" || t.cat == "learn");
    let v = &s.last_verify;
    cards.push((
        "memory",
        Card {
            busy: learning,
            lines: vec![
                Line {
                    left: vec![
                        seg("context ", T_SUBTLE),
                        seg(ctx_bar(ctx_pct, 10), level_col(ctx_pct)),
                    ],
                    right: Some(seg(format!("{}%", to_fixed(ctx_pct, 0)), T_INACTIVE)),
                },
                match v {
                    Some(v) => Line {
                        left: vec![
                            seg("verify  ", T_SUBTLE),
                            segb(
                                if v.ok { "✓ " } else { "✗ " },
                                if v.ok { GREEN } else { RED },
                            ),
                            seg(
                                one_line(if v.summary.is_empty() {
                                    &v.name
                                } else {
                                    &v.summary
                                }),
                                T_BODY,
                            ),
                        ],
                        right: Some(seg(ago(now - v.ts.unwrap_or(0)), T_SUBTLE)),
                    },
                    None => Line {
                        left: vec![
                            seg("verify  ", T_SUBTLE),
                            seg("no test / build run yet", T_SUBTLE),
                        ],
                        right: None,
                    },
                },
                Line {
                    left: vec![
                        seg("memory  ", T_SUBTLE),
                        seg(format!("{} writes", s.memory_writes), T_BODY),
                    ],
                    right: None,
                },
                Line {
                    left: vec![
                        seg("tasks   ", T_SUBTLE),
                        seg(
                            if todos.is_empty() {
                                "none".to_string()
                            } else {
                                format!("{done}/{} done", todos.len())
                            },
                            T_BODY,
                        ),
                    ],
                    right: None,
                },
            ],
            ..Default::default()
        },
    ));
    for (id, c) in cards.iter_mut() {
        let (title, short, col) = card_meta(id);
        c.title = title;
        c.short = short;
        c.col = col;
    }
    cards
}

fn comp<'a>(n: &Node<'a>) -> &'a Component {
    match &n.r {
        NodeRef::Comp(c) => c,
        NodeRef::Agent(_) => unreachable!("hook nodes are components"),
    }
}

fn level_or_inactive(p: f64) -> Rgb {
    if p >= 85.0 {
        RED
    } else if p >= 60.0 {
        YELLOW
    } else {
        T_INACTIVE
    }
}

// ---------------------------------------------------------------- layout

#[derive(Clone, Debug)]
struct Pos {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    compact: bool,
}
#[derive(Clone, Debug)]
struct Port {
    x: i64,
    y: i64,
    side: &'static str,
}
#[derive(Clone, Debug)]
struct Arrow {
    x: i64,
    y: i64,
    ch: char,
}
#[derive(Clone, Debug)]
struct Link {
    id: &'static str,
    path: Vec<Pt>,
    start_bits: u8,
    end_bits: u8,
    port: Port,
    arrow: Arrow,
}
struct Layout {
    ph: i64,
    pos: Vec<(&'static str, Pos)>,
    links: Vec<Link>,
}
impl Layout {
    fn get(&self, id: &str) -> Option<&Pos> {
        self.pos.iter().find(|(i, _)| *i == id).map(|(_, p)| p)
    }
}

fn desired_h(c: &Card) -> i64 {
    2 + c.lines.len().min(CARD_MAX_LINES) as i64
}

fn distribute(
    avail: i64,
    mins: &[i64],
    wants: &[i64],
    gap_count: usize,
    gap_min: i64,
    gap_pref: i64,
    gap_max: i64,
) -> (Vec<i64>, Vec<i64>) {
    let mut heights = mins.to_vec();
    let mut gaps = vec![gap_min; gap_count];
    let mut spare = avail - heights.iter().sum::<i64>() - gap_min * gap_count as i64;
    let grow_gaps = |gaps: &mut Vec<i64>, spare: &mut i64, to: i64| {
        let mut grew = true;
        while *spare > 0 && grew {
            grew = false;
            for g in gaps.iter_mut() {
                if *spare <= 0 {
                    break;
                }
                if *g < to {
                    *g += 1;
                    *spare -= 1;
                    grew = true;
                }
            }
        }
    };
    grow_gaps(&mut gaps, &mut spare, gap_pref);
    let mut grew = true;
    while spare > 0 && grew {
        grew = false;
        for i in 0..heights.len() {
            if spare <= 0 {
                break;
            }
            if heights[i] < wants[i] {
                heights[i] += 1;
                spare -= 1;
                grew = true;
            }
        }
    }
    grow_gaps(&mut gaps, &mut spare, gap_max);
    (heights, gaps)
}

fn card_of<'a>(cards: &'a [(&'static str, Card)], id: &str) -> &'a Card {
    &cards.iter().find(|(i, _)| *i == id).unwrap().1
}

fn layout_ring(w: i64, h: i64, cards: &[(&'static str, Card)], ph: i64) -> Layout {
    let gap_x = ((w as f64 * 0.03).floor() as i64).clamp(3, 4);
    let col_w = (w - 2 * gap_x) / 3;
    let xs = [0, col_w + gap_x, 2 * (col_w + gap_x)];
    let widths = [col_w, col_w, w - xs[2]];
    let at = |slot: &str| {
        RING.iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, id)| *id)
            .unwrap()
    };
    let grid = [["tl", "t", "tr"], ["l", "core", "r"], ["bl", "b", "br"]];
    let id_of = |slot: &str| if slot == "core" { "core" } else { at(slot) };
    let avail = h - ph;
    let mins = [2, 3, 2];
    let wants: Vec<i64> = (0..3)
        .map(|r| {
            grid[r]
                .iter()
                .map(|s| desired_h(card_of(cards, id_of(s))))
                .fold(mins[r], i64::max)
        })
        .collect();
    let (heights, gaps) = distribute(avail, &mins, &wants, 2, 1, 1, 2);
    let mut pos: Vec<(&'static str, Pos)> = vec![];
    let mut y = ph;
    for r in 0..3 {
        for c in 0..3 {
            let id = id_of(grid[r][c]);
            pos.push((
                id,
                Pos {
                    x: xs[c],
                    y,
                    w: widths[c],
                    h: heights[r],
                    compact: false,
                },
            ));
        }
        y += heights[r] + gaps.get(r).copied().unwrap_or(0);
    }
    let get = |id: &str| pos.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    let core = get("core");
    let ym = core.y + core.h / 2;
    let top = core.y - 1;
    let bottom = core.y + core.h;
    let tx0 = core.x + core.w - 6;
    let side_ports = core.h >= 5;
    let mut links = vec![];
    for (slot, id) in RING {
        let p = get(id);
        let mk = |path, sb, eb, port, arrow| Link {
            id,
            path,
            start_bits: sb,
            end_bits: eb,
            port,
            arrow,
        };
        match slot {
            "t" => links.push(mk(
                run(vec![(tx0, top)], tx0, p.y + p.h),
                D,
                U,
                Port {
                    x: tx0,
                    y: core.y,
                    side: "top",
                },
                Arrow {
                    x: tx0,
                    y: p.y + p.h - 1,
                    ch: '▲',
                },
            )),
            "b" => links.push(mk(
                run(vec![(tx0, bottom)], tx0, p.y - 1),
                U,
                D,
                Port {
                    x: tx0,
                    y: bottom - 1,
                    side: "bottom",
                },
                Arrow {
                    x: tx0,
                    y: p.y,
                    ch: '▼',
                },
            )),
            "l" => links.push(mk(
                run(vec![(core.x - 1, ym)], p.x + p.w, ym),
                R,
                L,
                Port {
                    x: core.x,
                    y: ym,
                    side: "left",
                },
                Arrow {
                    x: p.x + p.w - 1,
                    y: ym,
                    ch: '◄',
                },
            )),
            "r" => links.push(mk(
                run(vec![(core.x + core.w, ym)], p.x - 1, ym),
                L,
                R,
                Port {
                    x: core.x + core.w - 1,
                    y: ym,
                    side: "right",
                },
                Arrow {
                    x: p.x,
                    y: ym,
                    ch: '►',
                },
            )),
            _ => {
                let up = slot.starts_with('t');
                let left_side = slot.ends_with('l');
                let tx = if left_side { p.x + p.w - 4 } else { p.x + 3 };
                let gy = if up {
                    p.y + p.h + (core.y - (p.y + p.h)).div_euclid(2)
                } else {
                    bottom + (p.y - bottom - 1).div_euclid(2)
                };
                let end = if up { p.y + p.h } else { p.y - 1 };
                let arrow = Arrow {
                    x: tx,
                    y: if up { p.y + p.h - 1 } else { p.y },
                    ch: if up { '▲' } else { '▼' },
                };
                if side_ports {
                    let py = if up { core.y + 1 } else { core.y + core.h - 2 };
                    let vx = if left_side {
                        core.x - 2
                    } else {
                        core.x + core.w + 1
                    };
                    let start = if left_side {
                        core.x - 1
                    } else {
                        core.x + core.w
                    };
                    let path = run(
                        run(run(run(vec![(start, py)], vx, py), vx, gy), tx, gy),
                        tx,
                        end,
                    );
                    let port = Port {
                        x: if left_side {
                            core.x
                        } else {
                            core.x + core.w - 1
                        },
                        y: py,
                        side: if left_side { "left" } else { "right" },
                    };
                    links.push(mk(
                        path,
                        if left_side { R } else { L },
                        if up { U } else { D },
                        port,
                        arrow,
                    ));
                } else {
                    let px = if left_side {
                        core.x + 2
                    } else {
                        core.x + core.w - 3
                    };
                    let path = run(
                        run(
                            run(vec![(px, if up { top } else { bottom })], px, gy),
                            tx,
                            gy,
                        ),
                        tx,
                        end,
                    );
                    links.push(mk(
                        path,
                        if up { D } else { U },
                        if up { U } else { D },
                        Port {
                            x: px,
                            y: if up { core.y } else { bottom - 1 },
                            side: if up { "top" } else { "bottom" },
                        },
                        arrow,
                    ));
                }
            }
        }
    }
    Layout { ph, pos, links }
}

fn stack_rows(cols: usize) -> (Vec<Vec<&'static str>>, Vec<Vec<&'static str>>) {
    if cols == 2 {
        (
            vec![vec!["agents", "skills"], vec!["user", "guards"]],
            vec![vec!["exec", "hooks"], vec!["mcp", "memory"]],
        )
    } else {
        (
            vec![vec!["agents"], vec!["skills"], vec!["guards"], vec!["user"]],
            vec![vec!["exec"], vec!["hooks"], vec!["mcp"], vec!["memory"]],
        )
    }
}

struct SRow {
    ids: Vec<&'static str>,
    side: &'static str,
    k: usize,
    core: bool,
}

fn layout_stack(w: i64, h: i64, cards: &[(&'static str, Card)], ph: i64) -> Option<Layout> {
    let cols: usize = if w >= 56 { 2 } else { 1 };
    let (above, below) = stack_rows(cols);
    let depth_of = |n: usize| -> i64 {
        if cols == 2 {
            n.saturating_sub(1) as i64
        } else {
            (n.saturating_sub(1) as i64 + 1) / 2
        }
    };
    let depth = depth_of(above.len()).max(depth_of(below.len()));
    let core_min = MIN_CARD_H.max(2 * depth + 2);
    let row_count = (above.len() + below.len() + 1) as i64;
    let need = core_min + (row_count - 1) * (MIN_CARD_H + 1);
    let avail = h - ph;
    if need > avail {
        return None;
    }
    let lanes = depth;
    let margin = if lanes > 0 { lanes + 1 } else { 0 };
    let area_w = w - 2 * margin;
    let gap_x = if cols == 2 { 2 } else { 0 };
    let col_w = (area_w - gap_x * (cols as i64 - 1)) / cols as i64;
    let mut order: Vec<SRow> = above
        .iter()
        .enumerate()
        .map(|(k, r)| SRow {
            ids: r.clone(),
            side: "above",
            k,
            core: false,
        })
        .rev()
        .collect();
    order.push(SRow {
        ids: vec!["core"],
        side: "",
        k: 0,
        core: true,
    });
    order.extend(below.iter().enumerate().map(|(k, r)| SRow {
        ids: r.clone(),
        side: "below",
        k,
        core: false,
    }));
    let mins: Vec<i64> = order
        .iter()
        .map(|r| if r.core { core_min } else { MIN_CARD_H })
        .collect();
    let wants: Vec<i64> = order
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r.ids
                .iter()
                .map(|id| desired_h(card_of(cards, id)))
                .fold(mins[i], i64::max)
        })
        .collect();
    let (heights, gaps) = distribute(avail, &mins, &wants, order.len() - 1, 1, 1, 2);
    let mut pos: Vec<(&'static str, Pos)> = vec![];
    let mut y = ph;
    for (i, r) in order.iter().enumerate() {
        for (c, id) in r.ids.iter().enumerate() {
            let x = if r.core {
                margin
            } else {
                margin + c as i64 * (col_w + gap_x)
            };
            let cw = if r.core {
                area_w
            } else if c == r.ids.len() - 1 {
                margin + area_w - x
            } else {
                col_w
            };
            pos.push((
                id,
                Pos {
                    x,
                    y,
                    w: cw,
                    h: heights[i],
                    compact: false,
                },
            ));
        }
        y += heights[i] + gaps.get(i).copied().unwrap_or(0);
    }
    let get = |id: &str| pos.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    let core = get("core");
    let mut links = vec![];
    for r in &order {
        if r.core {
            continue;
        }
        for (c, id) in r.ids.iter().enumerate() {
            let p = get(id);
            let up = r.side == "above";
            if r.k == 0 {
                let x = p.x + p.w - if c == r.ids.len() - 1 { 6 } else { 4 };
                if up {
                    links.push(Link {
                        id,
                        path: run(vec![(x, core.y - 1)], x, p.y + p.h),
                        start_bits: D,
                        end_bits: U,
                        port: Port {
                            x,
                            y: core.y,
                            side: "top",
                        },
                        arrow: Arrow {
                            x,
                            y: p.y + p.h - 1,
                            ch: '▲',
                        },
                    });
                } else {
                    links.push(Link {
                        id,
                        path: run(vec![(x, core.y + core.h)], x, p.y - 1),
                        start_bits: U,
                        end_bits: D,
                        port: Port {
                            x,
                            y: core.y + core.h - 1,
                            side: "bottom",
                        },
                        arrow: Arrow {
                            x, y: p.y, ch: '▼'
                        },
                    });
                }
                continue;
            }
            let left_side = if cols == 2 { c == 0 } else { r.k % 2 == 1 };
            let d = if cols == 2 {
                r.k as i64
            } else {
                (r.k as i64 + 1) / 2
            };
            let py = if up {
                core.y + d
            } else {
                core.y + core.h - 1 - d
            };
            let ty = p.y + 1;
            if left_side {
                let lx = lanes - d;
                let path = run(
                    run(run(vec![(core.x - 1, py)], lx, py), lx, ty),
                    p.x - 1,
                    ty,
                );
                links.push(Link {
                    id,
                    path,
                    start_bits: R,
                    end_bits: R,
                    port: Port {
                        x: core.x,
                        y: py,
                        side: "left",
                    },
                    arrow: Arrow {
                        x: p.x,
                        y: ty,
                        ch: '►',
                    },
                });
            } else {
                let rx = w - 1 - lanes + d;
                let path = run(
                    run(run(vec![(core.x + core.w, py)], rx, py), rx, ty),
                    p.x + p.w,
                    ty,
                );
                links.push(Link {
                    id,
                    path,
                    start_bits: L,
                    end_bits: L,
                    port: Port {
                        x: core.x + core.w - 1,
                        y: py,
                        side: "right",
                    },
                    arrow: Arrow {
                        x: p.x + p.w - 1,
                        y: ty,
                        ch: '◄',
                    },
                });
            }
        }
    }
    Some(Layout { ph, pos, links })
}

fn layout_fan(w: i64, h: i64, cards: &[(&'static str, Card)], ph: i64) -> Layout {
    let n = FAN.len() as i64;
    let core_h = (h - ph - n).clamp(3, desired_h(card_of(cards, "core")).max(3));
    let mut pos = vec![(
        "core",
        Pos {
            x: 0,
            y: ph,
            w,
            h: core_h,
            compact: false,
        },
    )];
    let mut links = vec![];
    let bottom = ph + core_h;
    for (i, id) in FAN.iter().enumerate() {
        let y = bottom + i as i64;
        let lx = n - i as i64;
        pos.push((
            id,
            Pos {
                x: n + 2,
                y,
                w: w - n - 2,
                h: 1,
                compact: true,
            },
        ));
        let path = run(run(vec![(lx, bottom)], lx, y), n, y);
        links.push(Link {
            id,
            path,
            start_bits: U,
            end_bits: R,
            port: Port {
                x: lx,
                y: bottom - 1,
                side: "bottom",
            },
            arrow: Arrow {
                x: n + 1,
                y,
                ch: '►',
            },
        });
    }
    Layout { ph, pos, links }
}

fn layout_list(w: i64, h: i64) -> Layout {
    let ids: Vec<&'static str> = std::iter::once("core").chain(FAN.iter().copied()).collect();
    let ph = 1;
    let cols = if h - ph >= ids.len() as i64 { 1 } else { 2 };
    let per_col = (ids.len() as i64 + cols - 1) / cols;
    let col_w = (w - (cols - 1) * 2) / cols;
    let pos = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let i = i as i64;
            (
                *id,
                Pos {
                    x: (i / per_col) * (col_w + 2),
                    y: ph + i % per_col,
                    w: col_w,
                    h: 1,
                    compact: true,
                },
            )
        })
        .collect();
    Layout {
        ph,
        pos,
        links: vec![],
    }
}

fn layout_cards(w: i64, h: i64, cards: &[(&'static str, Card)]) -> Layout {
    let order: [&str; 2] = if w >= RING_WIDE_W {
        ["ring", "stack"]
    } else {
        ["stack", "ring"]
    };
    for ph in [PHASE_H, 2, 1] {
        for mode in order {
            if mode == "ring" && w >= RING_MIN_W && h - ph >= RING_MIN_H {
                return layout_ring(w, h, cards, ph);
            }
            if mode == "stack"
                && let Some(l) = layout_stack(w, h, cards, ph)
            {
                return l;
            }
        }
    }
    for ph in [PHASE_H, 2, 1] {
        if h - ph >= FAN_MIN_H {
            return layout_fan(w, h, cards, ph);
        }
    }
    layout_list(w, h)
}

fn offset_layout(lay: &mut Layout, dx: i64, dy: i64) {
    if dx == 0 && dy == 0 {
        return;
    }
    for (_, p) in lay.pos.iter_mut() {
        p.x += dx;
        p.y += dy;
    }
    for lk in lay.links.iter_mut() {
        for pt in lk.path.iter_mut() {
            pt.0 += dx;
            pt.1 += dy;
        }
        lk.port.x += dx;
        lk.port.y += dy;
        lk.arrow.x += dx;
        lk.arrow.y += dy;
    }
}

fn center_layout(lay: &mut Layout, w: i64, h: i64) {
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
    for (_, p) in &lay.pos {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x + p.w - 1);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y + p.h - 1);
    }
    for lk in &lay.links {
        for &(x, y) in &lk.path {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    if min_x == i64::MAX {
        return;
    }
    let bbox_w = max_x - min_x + 1;
    let bbox_h = max_y - min_y + 1;
    let target_x = (w - bbox_w).div_euclid(2);
    let target_y = lay.ph + (h - lay.ph - bbox_h).div_euclid(2);
    let clamp = |v: i64, lo: i64, hi: i64| v.max(lo).min(hi);
    let dx = clamp(target_x - min_x, -min_x, w - 1 - max_x);
    let dy = clamp(target_y - min_y, lay.ph - min_y, h - 1 - max_y);
    offset_layout(lay, dx, dy);
}

/// Natural height of the diagram at width `w` (capped at `max_h`).
pub fn natural_flow_height(
    s: &Session,
    w: i64,
    max_h: i64,
    frame: usize,
    now: i64,
    opts: FlowOpts,
) -> i64 {
    if w < 24 || max_h < 6 {
        return max_h;
    }
    let nodes = collect_nodes(s, now);
    let ps = phase_state(s, now);
    let cards = build_cards(s, &nodes, &ps, frame, now, opts);
    let lay = layout_cards(w, max_h, &cards);
    let mut max_y = i64::MIN;
    for (_, p) in &lay.pos {
        max_y = max_y.max(p.y + p.h - 1);
    }
    for lk in &lay.links {
        for &(_, y) in &lk.path {
            max_y = max_y.max(y);
        }
    }
    if max_y == i64::MIN {
        max_h
    } else {
        max_h.min(max_y + 1)
    }
}

// ---------------------------------------------------------------- drawing

fn draw_line(cv: &mut Canvas, x: i64, y: i64, w: i64, ln: &Line) {
    let mut right_text = ln
        .right
        .as_ref()
        .filter(|r| !r.0.is_empty())
        .map_or(String::new(), |r| {
            clip(&r.0, 4.max((w as f64 * 0.55).floor() as i64))
        });
    let mut room = if right_text.is_empty() {
        w
    } else {
        w - len(&right_text) - 1
    };
    if !right_text.is_empty() && room < 6 {
        right_text.clear();
        room = w;
    }
    let mut cx = x;
    for (text, col, _) in &ln.left {
        if room <= 0 {
            break;
        }
        let t = clip(text, room);
        cv.text(cx, y, &t, *col, 6);
        cx += len(&t);
        room -= len(&t);
    }
    if !right_text.is_empty() {
        cv.text(
            x + w - len(&right_text),
            y,
            &right_text,
            ln.right.as_ref().unwrap().1,
            6,
        );
    }
}

fn card_border(c: &Card, now: i64) -> Rgb {
    if c.error {
        return if blink_on(now) {
            RED
        } else {
            lerp(RED, DIM, 0.4)
        };
    }
    if c.busy {
        return if blink_on(now) { WARM_CREAM } else { c.col };
    }
    if c.returning {
        return GREEN;
    }
    lerp(c.col, DIM, 0.6)
}

fn title_slot(p: &Pos, reserved: &[i64]) -> (i64, i64) {
    let lo = p.x + 2;
    let hi = p.x + p.w - 3;
    let mut cuts: Vec<i64> = reserved
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|x| *x >= lo - 1 && *x <= hi + 1)
        .collect();
    cuts.sort();
    let mut best = (lo, 0);
    let mut from = lo;
    cuts.push(hi + 2);
    for x in cuts {
        let to = x - 2;
        if to - from + 1 > best.1 {
            best = (from, to - from + 1);
        }
        from = x + 2;
    }
    best
}

fn draw_compact(cv: &mut Canvas, card: &Card, p: &Pos, now: i64) {
    let active = card.busy || card.returning || card.error;
    let col = if active {
        card_border(card, now)
    } else {
        lerp(card.col, DIM, 0.2)
    };
    let label = if len(card.title) <= (p.w as f64 * 0.4).floor() as i64 {
        card.title
    } else {
        card.short
    };
    let title = clip(label, 6.max((p.w as f64 * 0.4).floor() as i64));
    cv.text(p.x, p.y, &title, col, 6);
    let mut x = p.x + len(&title) + 1;
    if !card.badge.is_empty() {
        let b = clip(&card.badge, 0.max(p.w - (x - p.x) - 8));
        cv.text(x, p.y, &b, T_SUBTLE, 6);
        x += len(&b) + 1;
    }
    let first = card
        .lines
        .iter()
        .find(|l| !l.left.is_empty())
        .or(card.lines.first());
    if let Some(f) = first
        && x < p.x + p.w - 4
    {
        draw_line(cv, x + 1, p.y, p.x + p.w - x - 1, f);
    }
}

fn draw_card(cv: &mut Canvas, card: &Card, p: &Pos, now: i64, reserved: &[i64]) {
    if p.compact {
        return draw_compact(cv, card, p, now);
    }
    let active = card.busy || card.returning || card.error;
    draw_box(cv, p.x, p.y, p.w, p.h, card_border(card, now), active);
    let slot = title_slot(p, reserved);
    let tx = if slot.0 == p.x + 2 && reserved.is_empty() {
        p.x + 4
    } else {
        slot.0
    };
    let room = slot.0 + slot.1 - tx;
    let label = if len(&format!(" {} ", card.title)) <= room {
        card.title
    } else {
        card.short
    };
    let title = clip(&format!(" {label} "), room);
    cv.text(
        tx,
        p.y,
        &title,
        if active {
            card.col
        } else {
            lerp(card.col, DIM, 0.2)
        },
        6,
    );
    let badge_room = room - len(&title);
    if !card.badge.is_empty() && badge_room > 5 {
        cv.text(
            tx + len(&title),
            p.y,
            &clip(&format!(" {} ", card.badge), badge_room),
            T_SUBTLE,
            6,
        );
    }
    let rows = p.h - 2;
    if rows <= 0 {
        return;
    }
    let mut lines = card.lines.clone();
    if lines.len() as i64 > rows {
        let keep = (rows - 1).max(0) as usize;
        let more = card.lines.len() - keep;
        lines.truncate(keep);
        lines.push(Line {
            left: vec![seg(format!("+{more} more"), T_SUBTLE)],
            right: None,
        });
    }
    for (i, ln) in lines.iter().enumerate() {
        draw_line(cv, p.x + 2, p.y + 1 + i as i64, p.w - 4, ln);
    }
}

fn draw_phases(cv: &mut Canvas, w: i64, ps: &PhaseState, frame: usize, detail: &str, ph: i64) {
    let glyph_of = |p: &Phase_, i: usize| -> &str {
        if ps.current.as_deref() == Some(p.key) {
            spin(frame + i)
        } else if ps.seen.contains(p.key) {
            "✓"
        } else {
            "○"
        }
    };
    type TextFn = fn(&Phase_, usize) -> String;
    let variants: [(TextFn, &str); 3] = [
        (|p, i| format!("{} {}", i + 1, p.label), " ━► "),
        (|p, i| format!("{} {}", i + 1, p.short), " ► "),
        (|_, i| format!("{}", i + 1), "›"),
    ];
    let total = |v: &(TextFn, &str)| -> i64 {
        PHASES
            .iter()
            .enumerate()
            .map(|(i, p)| 2 + len(&(v.0)(p, i)))
            .sum::<i64>()
            + len(v.1) * (PHASES.len() as i64 - 1)
    };
    let v = variants
        .iter()
        .find(|x| total(x) <= w)
        .unwrap_or(&variants[2]);
    let mut x = 0;
    for (i, p) in PHASES.iter().enumerate() {
        let cur = ps.current.as_deref() == Some(p.key);
        let seen = ps.seen.contains(p.key);
        let col = if cur {
            p.col
        } else if seen {
            lerp(p.col, DIM, 0.25)
        } else {
            T_SUBTLE
        };
        cv.text(x, 0, &format!("{} ", glyph_of(p, i)), col, 6);
        let t = (v.0)(p, i);
        cv.text(x + 2, 0, &t, if cur { WARM_CREAM } else { col }, 6);
        x += 2 + len(&t);
        if i < PHASES.len() - 1 {
            cv.text(
                x,
                0,
                v.1,
                if seen || cur {
                    lerp(p.col, DIM, 0.35)
                } else {
                    DIM
                },
                6,
            );
            x += len(v.1);
        }
    }
    let cur = PHASES.iter().find(|p| ps.current.as_deref() == Some(p.key));
    let (caption, col) = match cur {
        Some(c) => (
            format!(
                "▸ {}: {}{}",
                c.label,
                c.desc,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            ),
            c.col,
        ),
        None => (
            if ps.seen.is_empty() {
                "idle · waiting for a prompt".to_string()
            } else {
                "✓ turn complete · waiting for the next prompt".to_string()
            },
            T_SUBTLE,
        ),
    };
    if ph >= 2 {
        cv.text(0, 1, &clip(&caption, w), col, 6);
    }
}

#[derive(PartialEq, Clone, Copy)]
enum LinkState {
    Error,
    Busy,
    Returning,
    Idle,
}

fn port_glyphs(side: &str) -> (char, char) {
    match side {
        "top" => ('┻', '▼'),
        "bottom" => ('┳', '▲'),
        "left" => ('┫', '►'),
        _ => ('┣', '◄'),
    }
}

/// Renders the diagram into `h` rows of exactly `w` cells.
pub fn render_flow(
    s: &Session,
    w: usize,
    h: usize,
    frame: usize,
    now: i64,
    opts: FlowOpts,
) -> Vec<String> {
    let mut cv = Canvas::new(w, h);
    if w < 24 || h < 6 {
        cv.text(
            0,
            0,
            &clip("enlarge the window to show the topology", w as i64),
            T_SUBTLE,
            6,
        );
        return cv.rows();
    }
    let (wi, hi) = (w as i64, h as i64);
    let nodes = collect_nodes(s, now);
    let ps = phase_state(s, now);
    let cards = build_cards(s, &nodes, &ps, frame, now, opts);
    let mut lay = layout_cards(wi, hi, &cards);
    center_layout(&mut lay, wi, hi);
    let detail = if let Some(t) = ps.current_tool {
        one_line(&format!("{} {}", t.name, t.summary))
    } else {
        nodes
            .iter()
            .filter(|n| n.kind == "agent" && is_busy(n.st))
            .map(|n| default_name(n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    draw_phases(&mut cv, wi, &ps, frame, &detail, lay.ph);

    let state_of = |c: &Card| {
        if c.error {
            LinkState::Error
        } else if c.busy {
            LinkState::Busy
        } else if c.returning {
            LinkState::Returning
        } else {
            LinkState::Idle
        }
    };
    let tone = |st: LinkState, col: Rgb| match st {
        LinkState::Idle => DIM,
        LinkState::Error => RED,
        LinkState::Returning => GREEN,
        LinkState::Busy => col,
    };
    struct Wire {
        bits: u8,
        col: Rgb,
        rank: i32,
    }
    let mut wires: Vec<((i64, i64), Wire)> = vec![];
    let mut widx: std::collections::HashMap<(i64, i64), usize> = Default::default();
    for lk in &lay.links {
        let card = card_of(&cards, lk.id);
        let st = state_of(card);
        let rank = if st == LinkState::Idle { 0 } else { 2 };
        let n = lk.path.len();
        for (k, &pt) in lk.path.iter().enumerate() {
            let i = *widx.entry(pt).or_insert_with(|| {
                wires.push((
                    pt,
                    Wire {
                        bits: 0,
                        col: DIM,
                        rank: -1,
                    },
                ));
                wires.len() - 1
            });
            let cur = &mut wires[i].1;
            if k > 0 {
                cur.bits |= dir_to(pt, lk.path[k - 1]);
            }
            if k < n - 1 {
                cur.bits |= dir_to(pt, lk.path[k + 1]);
            }
            if k == 0 {
                cur.bits |= lk.start_bits;
            }
            if k == n - 1 {
                cur.bits |= lk.end_bits;
            }
            if rank > cur.rank {
                cur.col = tone(st, card.col);
                cur.rank = rank;
            }
        }
    }
    for ((x, y), c) in &wires {
        cv.set(*x, *y, glyph(c.bits, c.rank == 2), Some(c.col), 1);
    }
    for lk in &lay.links {
        let card = card_of(&cards, lk.id);
        match state_of(card) {
            LinkState::Busy => conveyor(&mut cv, &lk.path, card.col, lk.id == "user", now),
            LinkState::Returning => conveyor(&mut cv, &lk.path, GREEN, true, now),
            LinkState::Error => conveyor(&mut cv, &lk.path, RED, true, now),
            LinkState::Idle => {}
        }
    }
    let mut reserved: std::collections::HashMap<&str, Vec<i64>> = Default::default();
    let mut reserve = |id: &'static str, x: i64, y: i64| {
        if lay.get(id).is_some_and(|p| p.y == y) {
            reserved.entry(id).or_default().push(x);
        }
    };
    for lk in &lay.links {
        reserve("core", lk.port.x, lk.port.y);
        reserve(lk.id, lk.arrow.x, lk.arrow.y);
    }
    for (id, p) in &lay.pos {
        let r = reserved.get(id).cloned().unwrap_or_default();
        draw_card(&mut cv, card_of(&cards, id), p, now, &r);
    }
    for lk in &lay.links {
        let card = card_of(&cards, lk.id);
        let st = state_of(card);
        let on = st != LinkState::Idle;
        let col = tone(st, card.col);
        let inbound = lk.id == "user";
        let (out, inn) = port_glyphs(lk.port.side);
        cv.set(
            lk.port.x,
            lk.port.y,
            if inbound { inn } else { out },
            Some(if on {
                col
            } else {
                card_border(card_of(&cards, "core"), now)
            }),
            7,
        );
        if !inbound {
            cv.set(
                lk.arrow.x,
                lk.arrow.y,
                lk.arrow.ch,
                Some(if on { col } else { DIM }),
                7,
            );
        }
    }
    cv.rows()
}

/// Node count and how many are busy (topology panel title).
pub fn node_counts(s: &Session, now: i64) -> (usize, usize) {
    let nodes = collect_nodes(s, now);
    (nodes.iter().filter(|n| is_busy(n.st)).count(), nodes.len())
}
