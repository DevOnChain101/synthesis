//! The data the TUI view draws, as plain structs shaped like the engine-side models (docs/specs/desktop.md
//! section 2), plus a SAMPLE that is the design's own sample data (`cli.html` `STATE`). `snapshot::build`
//! fills a `Snapshot` from the live `ViewState`; no painting code reads anything else.

#[derive(Clone, Debug)]
pub struct Workspace {
    /// What a click selects (`WorkspaceRow::key`; empty in the sample).
    pub key: String,
    pub name: String,
    pub path: String,
    pub sessions: u32,
    pub selected: bool,
    /// Another folder than the open one: the row has a close button (the design has none).
    pub closable: bool,
}

#[derive(Clone, Debug)]
pub struct Running {
    /// What a click opens (the session id; empty in the sample).
    pub id: String,
    /// Another process's session (the peers listing): a click starts `/send <address> `, `id` is the address.
    pub peer: bool,
    pub title: String,
    /// The folder the session runs in (blue in the design).
    pub folder: String,
    pub branch: String,
    pub age: String,
    /// Epoch ms of its last activity; the dot's colour follows the clock from it.
    pub last: i64,
    pub live: bool,
    pub selected: bool,
    /// The mark of the row (spinner, yellow "!", ...) and the status word that goes with it.
    pub state: super::activity::RunState,
    pub status: String,
}

/// The Session panel: the status pill and the key/value rows (uptime last).
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub status: String,
    pub rows: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct ToolRow {
    pub name: String,
    pub value: String,
    /// A plain value (not the green "on").
    pub plain: bool,
}

#[derive(Clone, Debug)]
pub struct ToolsInfo {
    pub count: String,
    pub rows: Vec<ToolRow>,
}

/// What a row of Recent Activity is, for its mark and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActKind {
    /// What the person typed.
    Prompt,
    /// A tool or command run.
    Tool,
    /// A sub-agent.
    Agent,
    /// A permission asked.
    Approval,
    /// A failed tool or task.
    Error,
}

/// One entry of Recent Activity: a prompt or something the agent did, at `time`.
#[derive(Clone, Debug)]
pub struct ActivityRow {
    pub kind: ActKind,
    pub text: String,
    /// `HH:MM` of the event.
    pub time: String,
    /// The chat item (`convo.items`) this entry points at.
    pub anchor: usize,
    /// The chat shows that row; false when it is gone (compacted away, hidden by `/focus`): dimmed, not clickable.
    pub shown: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Done,
    Now,
    Todo,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub text: String,
    pub kind: StepKind,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct AgentStatus {
    pub working: String,
    pub next: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug)]
pub struct ContextInfo {
    pub used: String,
    pub max: String,
    /// 0..=100.
    pub pct: f32,
    pub model: String,
    pub turns: String,
    pub input: String,
    pub files: String,
    pub output: String,
    pub cache: String,
    pub hit: String,
}

/// The centre panel header (`#chead`).
#[derive(Clone, Debug)]
pub struct ChatHead {
    pub status: String,
    pub prompt: String,
    pub ctx_pct: f32,
    pub path: String,
    /// `12 files | 3.2k LOC`.
    pub stats: String,
}

/// A row of Usage & Metrics.
#[derive(Clone, Debug)]
pub struct Metric {
    pub label: String,
    pub value: String,
    /// Bar fill, 0..=100.
    pub pct: f32,
}

#[derive(Clone, Debug)]
pub struct Usage {
    pub range: String,
    pub rows: Vec<Metric>,
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
pub struct Top {
    pub live: u32,
    pub workspace: String,
    pub session: String,
    pub model: String,
    pub effort: String,
    pub ctx: String,
    pub health: String,
    /// `Sat Nov 07  07:54:38`.
    pub clock: String,
}

/// A chip of the footer's right side.
#[derive(Clone, Debug)]
pub struct StatusChip {
    pub text: String,
    /// Dot colour: warn for pending changes.
    pub warn: bool,
    pub dot: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub top: Top,
    pub workspaces: Vec<Workspace>,
    pub running: Vec<Running>,
    pub session: SessionInfo,
    pub tools: ToolsInfo,
    pub activity: Vec<ActivityRow>,
    pub agent: AgentStatus,
    pub context: ContextInfo,
    pub head: ChatHead,
    pub topology: Vec<TopoNode>,
    pub core: Core,
    pub usage: Usage,
    pub tips: Vec<String>,
    pub mode: String,
    pub effort: String,
    /// Sub-agents of the roster under the prompt (empty: the roster is not shown).
    pub roster: Vec<synthesis_view::lifecycle::RosterRow>,
    pub hints: Vec<(String, String)>,
    pub status: Vec<StatusChip>,
    /// Live activity of the Topology nodes (flows and glow).
    pub pulses: synthesis_view::pulse::Pulses,
}

fn s(x: &str) -> String {
    x.to_string()
}

/// The design's sample (`cli.html` `STATE`), for the layout tests, `--sample` and `--shot`.
pub fn sample() -> Snapshot {
    let ws = |n: &str, p: &str, c: u32, sel: bool| Workspace {
        key: String::new(),
        name: s(n),
        path: s(p),
        sessions: c,
        selected: sel,
        closable: false,
    };
    let run = |t: &str, f: &str, b: &str, a: &str, sel: bool| Running {
        id: String::new(),
        peer: false,
        title: s(t),
        folder: s(f),
        branch: s(b),
        age: s(a),
        last: i64::MAX,
        live: true,
        selected: sel,
        state: Default::default(),
        status: String::new(),
    };
    let node = |pos, title: &str, lines: &[&str], state| TopoNode {
        pos,
        title: s(title),
        lines: lines.iter().map(|l| s(l)).collect(),
        state,
    };
    let tool = |n: &str, v: &str, plain: bool| ToolRow {
        name: s(n),
        value: s(v),
        plain,
    };
    let step = |t: &str, kind, note: &str| Step {
        text: s(t),
        kind,
        note: s(note),
    };
    let metric = |l: &str, v: &str, pct: f32| Metric {
        label: s(l),
        value: s(v),
        pct,
    };
    Snapshot {
        top: Top {
            live: 1,
            workspace: s("synthesis"),
            session: s("feature/auth-flow"),
            model: s("claude-3.5-sonnet"),
            effort: s("high"),
            ctx: s("128k"),
            health: s("system healthy"),
            clock: s("Sat Nov 07\u{a0}\u{a0}07:54:38"),
        },
        workspaces: vec![
            ws("synthesis", "~/dev/synthesis", 3, true),
            ws("platform", "~/dev/platform", 2, false),
            ws("web-app", "~/dev/web-app", 1, false),
            ws("infra", "~/dev/infra", 0, false),
        ],
        running: vec![
            run(
                "Investigate why login redirects loop in auth.ts and fix it.",
                "auth-service",
                "feature/auth-flow",
                "42m",
                true,
            ),
            run(
                "Fix the redirect loop after sign-in on staging.",
                "synthesis",
                "fix/login-redirect",
                "18m",
                false,
            ),
            run(
                "Upgrade the runtime to Node 22 and fix breakages.",
                "platform",
                "chore/runtime",
                "9m",
                false,
            ),
        ],
        session: SessionInfo {
            status: s("running"),
            rows: [
                ("mode", "agent"),
                ("model", "claude-3.5-sonnet"),
                ("context", "128,000 tokens"),
                ("tools", "12 enabled"),
                ("cwd", "~/dev/synthesis"),
                ("uptime", "42m 20s"),
            ]
            .iter()
            .map(|(a, b)| (s(a), s(b)))
            .collect(),
        },
        tools: ToolsInfo {
            count: s("12"),
            rows: vec![
                tool("file system", "on", false),
                tool("shell", "on", false),
                tool("browser", "on", false),
                tool("search", "on", false),
                tool("git", "on", false),
                tool("mcp", "2 servers", true),
                tool("memory", "on", false),
            ],
        },
        activity: [
            (ActKind::Tool, "Edited auth.ts", "23:02"),
            (ActKind::Tool, "Ran tests", "23:00"),
            (ActKind::Tool, "Searched codebase", "22:58"),
            (ActKind::Tool, "Read architecture.md", "22:55"),
            (
                ActKind::Prompt,
                "Investigate why login redirects loop in auth.ts and fix it.",
                "22:54",
            ),
        ]
        .iter()
        .enumerate()
        .map(|(i, (kind, t, time))| ActivityRow {
            kind: *kind,
            text: s(t),
            time: s(time),
            anchor: i,
            shown: true,
        })
        .collect(),
        agent: AgentStatus {
            working: s("Implementing secure authentication flow with session management"),
            next: s("Run tests and verify edge cases"),
            steps: vec![
                step("Plan updated", StepKind::Done, "2m ago"),
                step("Implementing changes...", StepKind::Now, ""),
                step("Running tests...", StepKind::Todo, ""),
                step("Verifying results...", StepKind::Todo, ""),
                step("Updating documentation...", StepKind::Todo, ""),
            ],
        },
        context: ContextInfo {
            used: s("48.2k"),
            max: s("128k"),
            pct: 38.,
            model: s("claude-sonnet-5.5"),
            turns: s("24"),
            input: s("42.1k"),
            files: s("12"),
            output: s("6.1k"),
            cache: s("12.3k"),
            hit: s("92%"),
        },
        head: ChatHead {
            status: s("ready"),
            prompt: s("Investigate why login redirects loop in auth.ts and fix it."),
            ctx_pct: 38.,
            path: s("~/dev/auth-service"),
            stats: s("12 files | 3.2k LOC"),
        },
        topology: vec![
            node(
                Pos::User,
                "User & Prompt",
                &["hunghq", "auth-flow", "fix redirect loop"],
                NodeState::Active,
            ),
            node(
                Pos::Hooks,
                "Hooks",
                &["pre-tool-use", "post-edit", "stop"],
                NodeState::Idle,
            ),
            node(
                Pos::Guards,
                "Guards",
                &[
                    "sandbox \u{b7} workspace",
                    "network \u{b7} proxy",
                    "approval \u{b7} granular",
                ],
                NodeState::Idle,
            ),
            node(
                Pos::Sub,
                "Subagents",
                &[
                    "developer \u{b7} active",
                    "tester \u{b7} idle",
                    "explorer \u{b7} ready",
                ],
                NodeState::Active,
            ),
            node(
                Pos::Skills,
                "Skills",
                &["ponytail", "graphify", "openai-docs"],
                NodeState::Idle,
            ),
            node(
                Pos::Mcp,
                "MCP",
                &["filesystem", "browser", "codex_apps"],
                NodeState::Idle,
            ),
            node(
                Pos::Tools,
                "Tools",
                &["exec_command", "apply_patch", "view_image"],
                NodeState::Active,
            ),
            node(
                Pos::Mem,
                "Memory",
                &["session.md", "project facts", "8 memories"],
                NodeState::Idle,
            ),
        ],
        core: Core {
            title: s("SYNTHESIS"),
            sub: s("CORE"),
            tag: s("Reason \u{2022} Act \u{2022} Verify"),
        },
        usage: Usage {
            range: s("7 days"),
            rows: vec![
                metric("Tokens", "48.2k / 128k", 38.),
                metric("Tool calls", "37", 24.),
                metric("Files modified", "6", 12.),
                metric("Commands run", "14", 22.),
                metric("Browser requests", "3", 9.),
                metric("MCP calls", "12", 20.),
            ],
        },
        tips: vec![
            s("Ask for a change in plain words; Shift+Tab cycles the permission mode."),
            s("Tab moves between the chat, Workspace and Running."),
            s("/model picks a model, /resume opens a stored session."),
            s("Ctrl+N opens another folder as a workspace."),
        ],
        mode: s("auto"),
        effort: s("high"),
        roster: [("developer", "idle", 240_000, 900_000)]
            .map(
                |(n, st, online_ms, tokens)| synthesis_view::lifecycle::RosterRow {
                    id: s(n),
                    name: s(n),
                    state: s(st),
                    online_ms,
                    tokens,
                },
            )
            .into(),
        hints: [
            ("\u{2318}/Ctrl+L", "focus input"),
            ("Esc", "leave input"),
            ("", "/tools /memory /shell /browser /diff"),
            ("Enter", "send"),
        ]
        .iter()
        .map(|(a, b)| (s(a), s(b)))
        .collect(),
        status: vec![
            StatusChip {
                text: s("synthesis"),
                warn: false,
                dot: false,
            },
            StatusChip {
                text: s("\u{2387} feature/auth-flow"),
                warn: false,
                dot: false,
            },
            StatusChip {
                text: s("6 files changed"),
                warn: true,
                dot: true,
            },
        ],
        pulses: Default::default(),
    }
}
