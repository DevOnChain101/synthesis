//! The real data behind the TUI view: a [`Snapshot`] built from the live `ViewState` (docs/specs/desktop.md
//! section 2). Every number comes from `synthesis-view`; this file only picks words and formats.

use synthesis_tui_render::dialogs::mode_name;
use synthesis_view::activity::ActivityKind;
use synthesis_view::topology::{Group, Topology, is_busy};
use synthesis_view::workspaces::{SessionRow, fold_dir};

use super::sample::{
    Activity, Core, McpRow, McpState, NodeState, Pos, Running, Snapshot, Tokens, Top, TopoNode,
    Usage, Workspace,
};
use crate::live::{Focus, Live};

/// `124000` -> `124k`, `1_300_000` -> `1.3M`.
pub fn tok(n: u64) -> String {
    if n >= 1_000_000 {
        let m = format!("{:.1}", n as f64 / 1e6);
        format!("{}M", m.trim_end_matches(".0"))
    } else if n >= 1000 {
        format!("{}k", (n + 500) / 1000)
    } else {
        n.to_string()
    }
}

/// `3725` seconds -> `01:02:05`.
pub fn hms(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Local `HH:MM:SS` of an epoch ms.
pub fn clock(ms: i64, tz_min: i32) -> String {
    hms((ms / 1000 + i64::from(tz_min) * 60).rem_euclid(86_400))
}

/// `now`, `12m`, `3h`, `yesterday`, `5d`.
pub fn age(now: i64, then: i64) -> String {
    if then <= 0 {
        return String::new();
    }
    let m = ((now - then) / 60_000).max(0);
    match m {
        0 => "now".into(),
        1..=59 => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        1440..=2879 => "yesterday".into(),
        _ => format!("{}d", m / 1440),
    }
}

/// The home folder as `~`.
pub fn tilde(p: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()) {
        Some(h) if p == h => "~".into(),
        Some(h)
            if p.strip_prefix(h)
                .is_some_and(|r| r.starts_with(['/', '\\'])) =>
        {
            format!("~{}", &p[h.len()..])
        }
        _ => p.to_string(),
    }
}

fn pct_of(used: u64, window: u64) -> f32 {
    if window == 0 {
        0.
    } else {
        (used as f64 * 100. / window as f64) as f32
    }
}

fn state_of(g: &Group) -> NodeState {
    if g.alert || g.items.iter().any(|i| i.error && is_busy(i.state)) {
        NodeState::Blocked
    } else if g.busy > 0 {
        NodeState::Active
    } else {
        NodeState::Idle
    }
}

fn names(g: &Group, n: usize) -> String {
    let v: Vec<&str> = g.items.iter().take(n).map(|i| i.name.as_str()).collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn node(pos: Pos, title: &str, lines: Vec<String>, state: NodeState) -> TopoNode {
    TopoNode {
        pos,
        title: title.into(),
        lines,
        state,
    }
}

fn topology(l: &Live, t: &Topology) -> (Vec<TopoNode>, Core) {
    let v = &l.view;
    let mcp_ok = l.mcp.iter().filter(|m| m.state == "connected").count();
    let one_line = |s: &str| {
        s.lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>()
    };
    let nodes = vec![
        node(
            Pos::User,
            "User",
            vec![
                if t.user.last_prompt.is_empty() {
                    "no prompt yet".into()
                } else {
                    one_line(&t.user.last_prompt)
                },
                format!(
                    "{} prompt{}",
                    t.user.prompts,
                    if t.user.prompts == 1 { "" } else { "s" }
                ),
            ],
            if t.user.busy {
                NodeState::Active
            } else {
                NodeState::Idle
            },
        ),
        node(
            Pos::Hooks,
            "Hooks",
            vec![
                format!("{} seen", t.hooks.items.len()),
                format!("{} calls", t.hooks.calls),
            ],
            state_of(&t.hooks),
        ),
        node(
            Pos::Guards,
            "Guards",
            vec![
                format!("permission: {}", mode_name(Some(&v.status.mode))),
                if t.guards.blocked > 0 {
                    format!("{} blocked", t.guards.blocked)
                } else {
                    "nothing blocked".into()
                },
            ],
            state_of(&t.guards),
        ),
        node(
            Pos::Sub,
            "Sub-agents",
            vec![format!("{} running", t.agents.busy), names(&t.agents, 2)],
            state_of(&t.agents),
        ),
        node(
            Pos::Skills,
            "Skills",
            vec![
                format!("{} used", t.skills.items.len()),
                names(&t.skills, 2),
            ],
            state_of(&t.skills),
        ),
        node(
            Pos::Mcp,
            "MCP",
            vec![
                format!("{mcp_ok} of {} connected", l.mcp.len()),
                names(&t.mcp, 2),
            ],
            if l.mcp.iter().any(|m| m.state == "failed") || t.mcp.alert {
                NodeState::Blocked
            } else {
                state_of(&t.mcp)
            },
        ),
        node(
            Pos::Tools,
            "Tools",
            vec![
                t.exec
                    .items
                    .iter()
                    .find(|i| is_busy(i.state))
                    .map_or_else(|| "idle".into(), |i| format!("{} running", i.name)),
                format!("{} calls", t.exec.calls),
            ],
            state_of(&t.exec),
        ),
        node(
            Pos::Mem,
            "Memory",
            vec![
                format!("{} writes", t.memory.memory_writes),
                t.memory
                    .verify
                    .as_ref()
                    .map_or_else(|| "no verify yet".into(), |x| one_line(&x.summary)),
            ],
            if t.memory.busy {
                NodeState::Active
            } else {
                NodeState::Idle
            },
        ),
    ];
    let tag = match (t.stage, l.busy) {
        (Some(s), _) => s.label().to_lowercase(),
        (None, true) => "working".into(),
        (None, false) if l.starting => "starting".into(),
        (None, false) => "ready".into(),
    };
    (
        nodes,
        Core {
            title: "Synthesis".into(),
            sub: "CORE".into(),
            tag,
        },
    )
}

fn session_rows(l: &Live, now: i64, home: Option<&str>) -> Vec<Running> {
    let v = &l.view;
    let Some(ws) = v.workspaces.workspaces.iter().find(|w| w.key == l.ws_key) else {
        return vec![];
    };
    let mut rows: Vec<SessionRow> = ws.sessions.clone();
    // the open session always shows, even before it has a message
    let current = fold_dir(&v.session.cwd) == ws.key || fold_dir(&l.cwd) == ws.key;
    if current && !l.sid.is_empty() && !rows.iter().any(|r| r.id == l.sid) {
        rows.insert(
            0,
            SessionRow {
                id: l.sid.clone(),
                title: "(new session)".into(),
                status: synthesis_core::SessionStatus::Idle,
                updated_at: String::new(),
                mtime: now,
                is_live: true,
            },
        );
    }
    let mut out: Vec<Running> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| Running {
            id: r.id.clone(),
            peer: false,
            title: r.title.clone(),
            branch: format!("{:?}", r.status).to_lowercase(),
            age: if r.is_live {
                "now".into()
            } else {
                age(now, r.mtime)
            },
            live: r.is_live,
            selected: l.focus == Focus::Running && i == l.run_sel,
        })
        .collect();
    out.extend(v.running.peers.iter().map(|p| Running {
        id: String::new(),
        peer: true,
        title: if p.task.is_empty() {
            format!("{} \u{b7} {}", p.session_short_id, p.agent)
        } else {
            p.task.clone()
        },
        branch: format!("{} \u{b7} {}", tilde(&p.workspace_label, home), p.status),
        age: String::new(),
        live: true,
        selected: false,
    }));
    out
}

/// What the view paints, from the live state. `now` is epoch ms, `tz_min` minutes east of UTC.
pub fn build(l: &Live, now: i64, tz_min: i32, home: Option<&str>) -> Snapshot {
    let v = &l.view;
    let st = &v.status;
    let topo = v.topology(now);
    let (used, window) = st.context.numbers();
    let ctx = st.context.label(now).map_or_else(
        || "n/a".to_string(),
        |(s, _)| s.trim_start_matches("ctx ").to_string(),
    );
    let tools = v.tools_skills();
    let running: String = tools
        .running
        .iter()
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
        .chars()
        .take(40)
        .collect();
    let running = if running.is_empty() {
        "none".into()
    } else {
        running
    };
    let mode = mode_name(Some(&st.mode));
    let cur_ws = v
        .workspaces
        .workspaces
        .iter()
        .find(|w| w.key == fold_dir(&l.cwd))
        .map_or_else(
            || {
                std::path::Path::new(&l.cwd)
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            },
            |w| w.label.clone(),
        );
    let prompt = if topo.user.last_prompt.is_empty() {
        "(new session)".to_string()
    } else {
        topo.user
            .last_prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect()
    };
    let needs = l.dialogs.len();
    let live_n = v.running.live.len().max(usize::from(l.engine.is_some()));
    let health = if l.starting {
        "starting".to_string()
    } else if needs > 0 {
        format!("{needs} need{} you", if needs == 1 { "s" } else { "" })
    } else if topo.errors > 0 {
        format!(
            "{} error{}",
            topo.errors,
            if topo.errors == 1 { "" } else { "s" }
        )
    } else {
        "all systems normal".to_string()
    };
    let (nodes, core) = topology(l, &topo);
    let days: Vec<f32> = v.usage.days.iter().map(|d| d.tokens as f32).collect();
    let n_sessions: usize = v
        .workspaces
        .workspaces
        .iter()
        .map(|w| w.sessions.len())
        .sum();
    let sandbox = l
        .engine
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |e| e.sandbox.mode.clone());

    Snapshot {
        top: Top {
            live: live_n as u32,
            workspace: cur_ws,
            session: prompt,
            model: if st.model.is_empty() {
                "no model".into()
            } else {
                st.model.clone()
            },
            ctx: ctx.clone(),
            health,
            clock: clock(now, tz_min),
        },
        workspaces: v
            .workspaces
            .workspaces
            .iter()
            .map(|w| Workspace {
                key: w.key.clone(),
                name: w.label.clone(),
                path: tilde(&w.root, home),
                sessions: w.session_count() as u32,
                selected: w.key == l.ws_key,
            })
            .collect(),
        running: session_rows(l, now, home),
        session: vec![
            ("id".into(), l.sid.chars().take(8).collect()),
            ("cwd".into(), tilde(&l.cwd, home)),
            ("mode".into(), mode.clone()),
            ("sandbox".into(), sandbox),
            (
                "uptime".into(),
                st.uptime_ms(now)
                    .map_or_else(|| "-".into(), |m| hms(m / 1000)),
            ),
        ],
        mcp: l
            .mcp
            .iter()
            .map(|m| McpRow {
                name: m.name.clone(),
                state: match m.state.as_str() {
                    "connected" => McpState::Connected,
                    "connecting" => McpState::Connecting,
                    _ => McpState::Failed,
                },
                tools: m.tools as u32,
            })
            .collect(),
        tools: vec![
            (
                "tools".into(),
                format!("{} calls", tools.tools.iter().map(|t| t.1).sum::<u32>()),
            ),
            (
                "skills".into(),
                format!("{} used", tools.skills.iter().map(|t| t.1).sum::<u32>()),
            ),
            ("running".into(), running),
            ("errors".into(), tools.errors.to_string()),
        ],
        // newest first, like the sample: the log keeps tools and hooks, the panel shows them all
        activity: v
            .activity()
            .log
            .iter()
            .rev()
            .take(200)
            .map(|r| Activity {
                time: r.ts.map_or_else(String::new, |t| {
                    let s = clock(t, tz_min);
                    s[..5].to_string()
                }),
                text: r.text.lines().next().unwrap_or("").to_string(),
                prompt: r.kind == ActivityKind::Prompt,
            })
            .collect(),
        status: vec![
            (
                "phase".into(),
                topo.stage.map_or_else(
                    || {
                        if l.busy {
                            "working".into()
                        } else {
                            "idle".into()
                        }
                    },
                    |s| s.label().to_lowercase(),
                ),
            ),
            (
                "task".into(),
                if topo.user.last_prompt.is_empty() {
                    "none".into()
                } else {
                    topo.user
                        .last_prompt
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect()
                },
            ),
            (
                "agents".into(),
                if topo.agents.busy > 0 {
                    format!("{} running", topo.agents.busy)
                } else {
                    "none".into()
                },
            ),
            ("turns".into(), st.turns.to_string()),
            (
                "verify".into(),
                topo.memory.verify.as_ref().map_or_else(
                    || "none".into(),
                    |x| {
                        if x.ok {
                            "passed".into()
                        } else {
                            "failed".into()
                        }
                    },
                ),
            ),
        ],
        tokens: Tokens {
            input: tok(st.input_tokens),
            output: tok(st.output_tokens),
            cache: match st.cache_hit() {
                Some(h) => format!(
                    "{} ({:.0}% hit)",
                    tok(st.cache_read + st.cache_create),
                    h * 100.
                ),
                None => "-".into(),
            },
            cost: st
                .cost_usd
                .map_or_else(|| "n/a".into(), |c| format!("${c:.2}")),
            ctx_pct: pct_of(used, window),
            ctx_label: if window > 0 {
                format!("{}/{}", tok(used), tok(window))
            } else {
                format!("{}/?", tok(used))
            },
        },
        topology: nodes,
        core,
        usage: Usage {
            range: format!("{}w", days.len().div_ceil(7)),
            rows: vec![
                ("today".into(), tok(v.usage.tokens_last(1) as u64)),
                ("7 days".into(), tok(v.usage.tokens_last(7) as u64)),
                ("sessions".into(), n_sessions.to_string()),
            ],
            days,
        },
        tips: vec![
            "Ask for a change in plain words; Shift+Tab cycles the permission mode.".into(),
            "Tab moves between the chat, Workspace and Running.".into(),
            "/ completes a command, @ mentions a file; Esc interrupts a running task.".into(),
            "Click a session in Running to continue it where it stopped.".into(),
        ],
        mode,
        effort: st.effort.clone().unwrap_or_else(|| "auto".into()),
        hints: hints(l),
    }
}

/// The key hint bar for what has the keys now.
fn hints(l: &Live) -> Vec<(String, String)> {
    let h = |k: &str, d: &str| (k.to_string(), d.to_string());
    if !l.dialogs.is_empty() {
        return vec![
            h("\u{2191}\u{2193}", "choose"),
            h("Enter", "confirm"),
            h("1-9", "pick"),
            h("Esc", "reject"),
        ];
    }
    match l.focus {
        Focus::Chat => vec![
            h("Enter", "send"),
            h("Esc", "interrupt"),
            h("Tab", "focus"),
            h("Ctrl+O", "expand"),
            h("/", "commands"),
            h("Ctrl+C\u{d7}2", "quit"),
        ],
        Focus::Workspace | Focus::Running => vec![
            h("\u{2191}\u{2193}", "select"),
            h("Enter", "open"),
            h("Tab", "next"),
            h("Esc", "chat"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synthesis_events::Envelope;
    use synthesis_view::model::parse_ts;

    fn ev(seq: u64, ts: &str, ty: &str, payload: serde_json::Value) -> Envelope {
        serde_json::from_value(json!({"v": 1, "seq": seq, "ts": ts, "session_id": "s1",
            "task_id": null, "turn_id": null, "type": ty, "payload": payload}))
        .unwrap()
    }

    #[test]
    fn formats() {
        assert_eq!(tok(124_000), "124k");
        assert_eq!(tok(1_300_000), "1.3M");
        assert_eq!(hms(3725), "01:02:05");
        assert_eq!(clock(3_600_000 * 5, -90), "03:30:00");
        assert_eq!(age(10 * 60_000, 0), "");
        assert_eq!(age(10 * 60_000, 60_000), "9m");
        assert_eq!(age(3 * 3_600_000 + 60_000, 60_000), "3h");
        assert_eq!(tilde("/Users/a/x", Some("/Users/a")), "~/x");
        assert_eq!(tilde("/Users/ab", Some("/Users/a")), "/Users/ab");
    }

    #[test]
    fn a_live_stream_fills_the_panels() {
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/w/proj".into();
        l.sid = "s1abcdef012".into();
        l.view.apply(&ev(
            1,
            "2026-11-07T10:00:00Z",
            "user.message",
            json!({"text": "run the tests", "images": []}),
        ));
        l.view.apply(&ev(
            2,
            "2026-11-07T10:00:01Z",
            "assistant.message",
            json!({"stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test"}}]}),
        ));
        l.view.apply(&ev(
            3,
            "2026-11-07T10:00:02Z",
            "context.usage",
            json!({"used_tokens": 124000, "window_tokens": 200000, "percent": 62.0, "stale": false, "source": "provider"}),
        ));
        let s = build(&l, parse_ts("2026-11-07T10:00:03Z").unwrap(), 0, None);
        assert_eq!(s.top.session, "run the tests");
        assert_eq!(s.top.ctx, "62% 124k/200k");
        assert!((s.tokens.ctx_pct - 62.0).abs() < 0.01);
        assert_eq!(s.tokens.ctx_label, "124k/200k");
        assert!(
            s.activity
                .iter()
                .any(|a| a.prompt && a.text == "run the tests")
        );
        assert!(
            s.activity
                .iter()
                .any(|a| !a.prompt && a.text.contains("cargo test"))
        );
        assert_eq!(s.topology.len(), 8);
        let tools = s.topology.iter().find(|n| n.pos == Pos::Tools).unwrap();
        assert_eq!(tools.state, NodeState::Active, "{tools:?}");
        assert!(tools.lines[0].contains("Bash"));
        assert_eq!(s.usage.days.len(), 0, "no store loaded yet");
        assert_eq!(s.session[0].1, "s1abcdef");
    }

    #[test]
    fn health_chip_words() {
        let mut l = Live::new();
        l.starting = false;
        assert_eq!(build(&l, 0, 0, None).top.health, "all systems normal");
        l.starting = true;
        assert_eq!(build(&l, 0, 0, None).top.health, "starting");
    }
}
