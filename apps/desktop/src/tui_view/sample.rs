//! The data the TUI view draws, as plain structs shaped like the engine-side models (docs/specs/desktop.md
//! section 2: workspaces, sessions, running, usage, status, tokens, topology nodes, activity, tools),
//! plus a SAMPLE. Wiring the real view-model later means building a `Snapshot` from it; no painting
//! code changes.

#[derive(Clone, Debug)]
pub struct Workspace {
    /// What a click selects (`WorkspaceRow::key`; empty in the sample).
    pub key: String,
    pub name: String,
    pub path: String,
    pub sessions: u32,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub struct Running {
    /// What a click opens (the session id; empty in the sample).
    pub id: String,
    /// Another process's session (the peers listing): not clickable.
    pub peer: bool,
    pub title: String,
    pub branch: String,
    pub age: String,
    pub live: bool,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub struct McpRow {
    pub name: String,
    pub state: McpState,
    pub tools: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpState {
    Connected,
    Connecting,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Activity {
    pub time: String,
    pub text: String,
    /// A `❯` row: a user prompt.
    pub prompt: bool,
}

/// Topology node state; maps to the card border colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Idle,
    Active,
    Blocked,
}

/// Where a node sits around the core (the design's `data-pos`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pos {
    User,
    Hooks,
    Guards,
    Sub,
    Skills,
    Mcp,
    Tools,
    Mem,
}

#[derive(Clone, Debug)]
pub struct TopoNode {
    pub pos: Pos,
    pub title: String,
    pub lines: Vec<String>,
    pub state: NodeState,
}

#[derive(Clone, Debug)]
pub struct Core {
    pub title: String,
    pub sub: String,
    pub tag: String,
}

#[derive(Clone, Debug)]
pub struct Usage {
    /// Tokens per local day, oldest first; the last entry is today.
    pub days: Vec<f32>,
    pub range: String,
    pub rows: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Tokens {
    pub input: String,
    pub output: String,
    pub cache: String,
    pub cost: String,
    /// Context fill, 0..=100.
    pub ctx_pct: f32,
    pub ctx_label: String,
}

#[derive(Clone, Debug)]
pub struct Top {
    pub live: u32,
    pub workspace: String,
    pub session: String,
    pub model: String,
    pub ctx: String,
    pub health: String,
    pub clock: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub top: Top,
    pub workspaces: Vec<Workspace>,
    pub running: Vec<Running>,
    pub session: Vec<(String, String)>,
    pub mcp: Vec<McpRow>,
    /// "Tools & Skills": counts per kind.
    pub tools: Vec<(String, String)>,
    pub activity: Vec<Activity>,
    pub status: Vec<(String, String)>,
    pub tokens: Tokens,
    pub topology: Vec<TopoNode>,
    pub core: Core,
    pub usage: Usage,
    pub tips: Vec<String>,
    pub mode: String,
    pub effort: String,
    pub hints: Vec<(String, String)>,
}

fn s(x: &str) -> String {
    x.to_string()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(a, b)| (s(a), s(b))).collect()
}

pub fn sample() -> Snapshot {
    let ws = |n: &str, p: &str, c: u32, sel: bool| Workspace {
        key: String::new(),
        name: s(n),
        path: s(p),
        sessions: c,
        selected: sel,
    };
    let run = |t: &str, b: &str, a: &str, live: bool, sel: bool| Running {
        id: String::new(),
        peer: false,
        title: s(t),
        branch: s(b),
        age: s(a),
        live,
        selected: sel,
    };
    let act = |t: &str, x: &str, p: bool| Activity {
        time: s(t),
        text: s(x),
        prompt: p,
    };
    let node = |pos, title: &str, lines: &[&str], state| TopoNode {
        pos,
        title: s(title),
        lines: lines.iter().map(|l| s(l)).collect(),
        state,
    };
    Snapshot {
        top: Top {
            live: 2,
            workspace: s("synthesis"),
            session: s("fix flaky retry test"),
            model: s("claude-sonnet-5-5"),
            ctx: s("62% 124k/200k"),
            health: s("all systems normal"),
            clock: s("14:32:07"),
        },
        workspaces: vec![
            ws("synthesis", "~/Vibe/synthesis", 12, true),
            ws("legacy-cli", "~/Vibe/legacy-cli", 4, false),
            ws(
                "synthesis-view-model",
                "~/Vibe/synthesis-view-model",
                2,
                false,
            ),
            ws("design", "~/Vibe/synthesis-cli-ui", 3, false),
            ws("scratch", "~/tmp/scratch", 1, false),
            ws("dotfiles", "~/dotfiles", 1, false),
            ws("site", "~/Vibe/site", 6, false),
        ],
        running: vec![
            run("fix flaky retry test", "main", "now", true, true),
            run(
                "port the topology wires",
                "feat/desktop-tui-view",
                "12m",
                false,
                false,
            ),
            run(
                "review the sandbox policy",
                "feat/sandbox",
                "1h",
                false,
                false,
            ),
            run("why does resume lose the mode", "main", "3h", false, false),
            run("add heat map legend", "main", "yesterday", false, false),
            run("bump gpui pins", "chore/pins", "2d", false, false),
            run("draft the release notes", "main", "3d", false, false),
        ],
        session: pairs(&[
            ("id", "9f3c2a71"),
            ("cwd", "~/Vibe/synthesis"),
            ("branch", "main"),
            ("mode", "default"),
            ("uptime", "00:41:18"),
        ]),
        mcp: vec![
            McpRow {
                name: s("github"),
                state: McpState::Connected,
                tools: 38,
            },
            McpRow {
                name: s("playwright"),
                state: McpState::Connecting,
                tools: 0,
            },
            McpRow {
                name: s("sentry"),
                state: McpState::Failed,
                tools: 0,
            },
        ],
        tools: pairs(&[
            ("tools", "24 builtin"),
            ("skills", "7 loaded"),
            ("hooks", "3 active"),
        ]),
        activity: vec![
            act("14:31", "fix the flaky retry test", true),
            act("14:31", "Read crates/synthesis-core/src/retry.rs", false),
            act("14:31", "Grep backoff_ms", false),
            act("14:32", "Edit retry.rs (+8 -3)", false),
            act("14:32", "Bash cargo test -p synthesis-core retry", false),
            act("14:32", "tests passed (12)", false),
            act("14:30", "run the whole suite", true),
            act("14:29", "Bash cargo clippy --workspace", false),
            act("14:28", "Edit lib.rs (+2 -0)", false),
        ],
        status: pairs(&[
            ("phase", "act"),
            ("task", "fix flaky retry test"),
            ("agents", "1 running"),
            ("elapsed", "00:03:12"),
            ("verify", "pending"),
        ]),
        tokens: Tokens {
            input: s("84.2k"),
            output: s("12.6k"),
            cache: s("210k (71% hit)"),
            cost: s("$0.42"),
            ctx_pct: 62.0,
            ctx_label: s("124k/200k"),
        },
        topology: vec![
            node(
                Pos::User,
                "User",
                &["fix flaky retry test", "queued 0"],
                NodeState::Active,
            ),
            node(
                Pos::Hooks,
                "Hooks",
                &["3 active", "last: pre-commit ok"],
                NodeState::Idle,
            ),
            node(
                Pos::Guards,
                "Guards",
                &["permission: default", "sandbox on"],
                NodeState::Idle,
            ),
            node(
                Pos::Sub,
                "Sub-agents",
                &["1 running", "explorer idle"],
                NodeState::Active,
            ),
            node(
                Pos::Skills,
                "Skills",
                &["7 loaded", "tdd, review"],
                NodeState::Idle,
            ),
            node(
                Pos::Mcp,
                "MCP",
                &["2 of 3 connected", "sentry failed"],
                NodeState::Blocked,
            ),
            node(
                Pos::Tools,
                "Tools",
                &["Bash running", "24 builtin"],
                NodeState::Active,
            ),
            node(
                Pos::Mem,
                "Memory",
                &["12 episodes", "reflect off"],
                NodeState::Idle,
            ),
        ],
        core: Core {
            title: s("Synthesis"),
            sub: s("CORE"),
            tag: s("ready"),
        },
        usage: Usage {
            // 53 weeks of days; a deterministic pattern stands in for the store's history
            days: (0..371)
                .map(|i| {
                    let v = ((i * 37 + 11) % 23) as f32;
                    if i % 9 == 4 { 0.0 } else { v * 41.0 }
                })
                .collect(),
            range: s("53w"),
            rows: pairs(&[("today", "412k"), ("7 days", "2.9M"), ("sessions", "31")]),
        },
        tips: vec![
            s("Ask for a change in plain words; Shift+Tab cycles the permission mode."),
            s("Tab moves between the chat, Workspace and Running."),
            s("/model picks a model, /resume opens a stored session."),
            s("Ctrl+N opens another folder as a workspace."),
        ],
        mode: s("default"),
        effort: s("high"),
        hints: pairs(&[
            ("Tab", "focus"),
            ("\u{2191}\u{2193}", "select"),
            ("Enter", "open"),
            ("Ctrl+N", "workspace"),
            ("/help", "commands"),
            ("Ctrl+D", "quit"),
        ]),
    }
}
