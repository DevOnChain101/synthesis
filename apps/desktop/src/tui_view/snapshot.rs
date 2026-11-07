//! The real data behind the TUI view: a [`Snapshot`] built from the live `ViewState` (docs/specs/desktop.md
//! section 2). Every number comes from `synthesis-view`; this file only picks words and formats.

use synthesis_tui_render::dialogs::mode_name;
use synthesis_view::activity::ActivityKind;
use synthesis_view::topology::{Group, Topology, is_busy};
use synthesis_view::workspaces::{SessionRow, fold_dir};

use super::sample::{
    ActivityRow, AgentStatus, ChatHead, ContextInfo, Core, Metric, NodeState, Pos, PromptRow,
    Running, SessionInfo, Snapshot, StatusChip, Step, StepKind, ToolRow, ToolsInfo, Top, TopoNode,
    Usage, Workspace,
};
use crate::live::{Focus, Live};
use synthesis_view::convo::StepState;

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
            "User & Prompt",
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
            "Subagents",
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
    (
        nodes,
        Core {
            title: "SYNTHESIS".into(),
            sub: "CORE".into(),
            tag: "Reason \u{2022} Act \u{2022} Verify".into(),
        },
    )
}

fn session_rows(l: &Live, now: i64, home: Option<&str>, folder: &str) -> Vec<Running> {
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
            folder: folder.to_string(),
            // the open session shows its git branch; a stored one only has its status
            branch: if r.is_live && !v.session.git_branch.is_empty() {
                v.session.git_branch.clone()
            } else {
                format!("{:?}", r.status).to_lowercase()
            },
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
        folder: tilde(&p.workspace_label, home),
        branch: p.status.to_string(),
        age: String::new(),
        live: true,
        selected: false,
    }));
    out
}

/// `3725` seconds -> `1h 02m 05s`, `2540` -> `42m 20s`.
pub fn uptime(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// `2m ago`, `3h ago`, `now`.
pub fn ago(now: i64, then: i64) -> String {
    match age(now, then).as_str() {
        "" => String::new(),
        a @ ("now" | "yesterday") => a.to_string(),
        a => format!("{a} ago"),
    }
}

/// `124000` -> `124,000`.
fn grouped(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn first_line(s: &str, n: usize) -> String {
    s.lines().next().unwrap_or("").chars().take(n).collect()
}

/// What the view paints, from the live state. `now` is epoch ms, `tz_min` minutes east of UTC.
pub fn build(l: &Live, now: i64, tz_min: i32, home: Option<&str>) -> Snapshot {
    let v = &l.view;
    let st = &v.status;
    let topo = v.topology(now);
    let (used, window) = st.context.numbers();
    let ctx_pct = pct_of(used, window);
    let max = if window > 0 { tok(window) } else { "?".into() };
    let tools = v.tools_skills();
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
        first_line(&topo.user.last_prompt, 40)
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
        "system healthy".to_string()
    };
    let (nodes, core) = topology(l, &topo);
    let model = if st.model.is_empty() {
        "no model".to_string()
    } else {
        st.model.clone()
    };
    let effort = st.effort.clone().unwrap_or_else(|| "auto".into());
    let (n_files, (added, removed)) = (v.panes.files.len(), v.panes.totals());
    let branch = v.session.git_branch.clone();
    let log = v.activity().log;

    // Tools & Skills: what the session used, then the fixed capabilities
    let mcp_up = l.mcp.iter().filter(|m| m.state == "connected").count();
    let mut trows: Vec<ToolRow> = vec![
        ToolRow {
            name: "mcp".into(),
            value: format!("{mcp_up} server{}", if mcp_up == 1 { "" } else { "s" }),
            plain: true,
        },
        ToolRow {
            name: "memory".into(),
            value: if l.engine.as_ref().is_some_and(|e| e.memory_on) {
                "on"
            } else {
                "off"
            }
            .into(),
            plain: false,
        },
        ToolRow {
            name: "skills".into(),
            value: format!("{} used", tools.skills.len()),
            plain: true,
        },
    ];
    trows.extend(tools.tools.iter().map(|(n, c)| ToolRow {
        name: n.to_lowercase(),
        value: format!("{c} calls"),
        plain: false,
    }));
    let total = trows.len();
    trows.truncate(7);

    let steps: Vec<Step> = v
        .convo
        .plan
        .iter()
        .map(|p| Step {
            text: p.text.clone(),
            kind: match p.state {
                StepState::Done => StepKind::Done,
                StepState::Active => StepKind::Now,
                StepState::Todo => StepKind::Todo,
            },
            note: String::new(),
        })
        .collect();
    let next = steps
        .iter()
        .find(|s| s.kind == StepKind::Todo)
        .map_or_else(|| "nothing queued".to_string(), |s| s.text.clone());

    // Usage & Metrics: the last 7 (or 30) days; a bar is that period's share of the last 28 (or 90) days
    let days = if l.usage_days == 30 { 30 } else { 7 };
    let (w7, w30) = (
        v.usage.metrics(days),
        v.usage.metrics(days * if days == 7 { 4 } else { 3 }),
    );
    let metric = |label: &str, a: f64, b: f64, tokens: bool| Metric {
        label: label.into(),
        value: if tokens {
            tok(a as u64)
        } else {
            (a as u64).to_string()
        },
        pct: if b > 0. { (a / b * 100.) as f32 } else { 0. },
    };
    let n_sessions: usize = v
        .workspaces
        .workspaces
        .iter()
        .map(|w| w.sessions.len())
        .sum();
    let _ = n_sessions;

    Snapshot {
        top: Top {
            live: live_n as u32,
            workspace: cur_ws.clone(),
            session: prompt,
            model: model.clone(),
            effort: effort.clone(),
            ctx: if window > 0 {
                tok(window)
            } else {
                "n/a".into()
            },
            health,
            clock: clock_text(now, tz_min),
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
        running: session_rows(l, now, home, &cur_ws),
        session: SessionInfo {
            status: if l.starting {
                "starting".into()
            } else if l.busy {
                "running".into()
            } else {
                "idle".into()
            },
            rows: vec![
                ("mode".into(), mode.clone()),
                ("model".into(), model.clone()),
                (
                    "context".into(),
                    if window > 0 {
                        format!("{} tokens", grouped(window))
                    } else {
                        "n/a".into()
                    },
                ),
                ("tools".into(), format!("{} used", tools.tools.len())),
                ("cwd".into(), tilde(&l.cwd, home)),
                (
                    "uptime".into(),
                    st.uptime_ms(now)
                        .map_or_else(|| "-".into(), |m| uptime(m / 1000)),
                ),
            ],
        },
        tools: ToolsInfo {
            count: total.to_string(),
            more: total.saturating_sub(7) as u32,
            rows: trows,
        },
        // newest first; prompts have their own panel
        activity: log
            .iter()
            .rev()
            .filter(|r| r.kind != ActivityKind::Prompt)
            .take(200)
            .map(|r| ActivityRow {
                text: first_line(&r.text, 200),
                ago: r.ts.map_or_else(String::new, |t| ago(now, t)),
            })
            .collect(),
        prompts: log
            .iter()
            .filter(|r| r.kind == ActivityKind::Prompt)
            .take(200)
            .map(|r| PromptRow {
                time: r.ts.map_or_else(String::new, |t| clock(t, tz_min)),
                text: first_line(&r.text, 200),
            })
            .collect(),
        agent: AgentStatus {
            working: if topo.user.last_prompt.is_empty() {
                "idle".into()
            } else {
                first_line(&topo.user.last_prompt, 120)
            },
            next,
            steps,
        },
        context: ContextInfo {
            used: tok(used),
            max,
            pct: ctx_pct,
            model: model.clone(),
            turns: st.turns.to_string(),
            input: tok(st.input_tokens),
            files: n_files.to_string(),
            output: tok(st.output_tokens),
            cache: tok(st.cache_read + st.cache_create),
            hit: st
                .cache_hit()
                .map_or_else(|| "-".into(), |h| format!("{:.0}%", h * 100.)),
        },
        head: ChatHead {
            status: if l.starting {
                "starting".into()
            } else if l.busy {
                "working".into()
            } else {
                "ready".into()
            },
            prompt: first_line(&topo.user.last_prompt, 200),
            ctx_pct,
            path: tilde(&l.cwd, home),
            stats: format!("{n_files} files | {} LOC", tok(added + removed)),
        },
        topology: nodes,
        core,
        usage: Usage {
            range: format!("{days} days"),
            rows: vec![
                metric("Tokens", w7.tokens, w30.tokens, true),
                metric("Tool calls", w7.tool_calls, w30.tool_calls, false),
                metric("Files modified", w7.files, w30.files, false),
                metric("Commands run", w7.commands, w30.commands, false),
                metric("Browser requests", w7.browser, w30.browser, false),
                metric("MCP calls", w7.mcp, w30.mcp, false),
            ],
        },
        tips: vec![
            "Ask for a change in plain words; Shift+Tab cycles the permission mode.".into(),
            "Tab moves between the chat, Workspace and Running.".into(),
            "/ completes a command, @ mentions a file; Esc interrupts a running task.".into(),
            "Click a session in Running to continue it where it stopped.".into(),
        ],
        mode,
        effort,
        agents: std::iter::once("main".to_string())
            .chain(v.convo.agents.iter().map(|a| a.0.clone()))
            .collect(),
        hints: hints(l),
        status: {
            let mut c = vec![StatusChip {
                text: cur_ws,
                warn: false,
                dot: false,
            }];
            if !branch.is_empty() {
                c.push(StatusChip {
                    text: format!("\u{2387} {branch}"),
                    warn: false,
                    dot: false,
                });
            }
            c.push(StatusChip {
                text: format!("{n_files} files changed"),
                warn: n_files > 0,
                dot: true,
            });
            c
        },
    }
}

/// `Sat Nov 07  07:54:38`, the top bar's clock.
fn clock_text(ms: i64, tz_min: i32) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = ms / 1000 + i64::from(tz_min) * 60;
    let days = local.div_euclid(86_400);
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let _ = era;
    format!(
        "{} {} {:02}\u{a0}\u{a0}{}",
        DAYS[days.rem_euclid(7) as usize],
        MONS[(m - 1) as usize],
        d,
        clock(ms, tz_min)
    )
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
        assert_eq!(s.top.ctx, "200k");
        assert!((s.context.pct - 62.0).abs() < 0.01);
        assert_eq!(
            (s.context.used.as_str(), s.context.max.as_str()),
            ("124k", "200k")
        );
        assert_eq!(
            s.session.rows[2],
            ("context".to_string(), "200,000 tokens".to_string())
        );
        assert!(
            s.prompts
                .iter()
                .any(|p| p.text == "run the tests" && p.time == "10:00:00")
        );
        assert!(s.activity.iter().any(|a| a.text.contains("cargo test")));
        assert!(s.activity.iter().all(|a| a.text != "run the tests"));
        assert_eq!(s.topology.len(), 8);
        let tools = s.topology.iter().find(|n| n.pos == Pos::Tools).unwrap();
        assert_eq!(tools.state, NodeState::Active, "{tools:?}");
        assert!(tools.lines[0].contains("Bash"));
        assert_eq!(s.usage.rows.len(), 6);
        assert_eq!(s.usage.rows[1].label, "Tool calls");
        assert_eq!(s.agents, ["main"]);
        assert_eq!(s.head.stats, "0 files | 0 LOC");
    }

    #[test]
    fn clock_and_uptime_text() {
        // 2026-11-07 is a Saturday
        let ms = parse_ts("2026-11-07T07:54:38Z").unwrap();
        assert_eq!(clock_text(ms, 0), "Sat Nov 07\u{a0}\u{a0}07:54:38");
        assert_eq!(uptime(2540), "42m 20s");
        assert_eq!(uptime(3725), "1h 02m 05s");
        assert_eq!(grouped(128_000), "128,000");
        assert_eq!(ago(10 * 60_000 + 60_000, 60_000), "10m ago");
    }

    #[test]
    fn health_chip_words() {
        let mut l = Live::new();
        l.starting = false;
        assert_eq!(build(&l, 0, 0, None).top.health, "system healthy");
        l.starting = true;
        assert_eq!(build(&l, 0, 0, None).top.health, "starting");
    }
}
