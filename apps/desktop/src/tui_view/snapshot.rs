//! The real data behind the TUI view: a [`Snapshot`] built from the live `ViewState` (docs/specs/desktop.md
//! section 2). Every number comes from `synthesis-view`; this file only picks words and formats.

use synthesis_tui_render::dialogs::mode_name;
use synthesis_view::topology::{Group, Topology, is_busy};
use synthesis_view::workspaces::{SessionRow, fold_dir};

use super::activity::{
    Activity, RunState, activity_of, run_state_of_live, run_state_of_status, status_word,
};
use super::sample::{
    ActKind, ActivityRow, AgentStatus, ChatHead, ContextInfo, Core, Metric, MonthUse, NodeState,
    Pos, Running, SessionInfo, Snapshot, StatusChip, Step, StepKind, ToolRow, ToolsInfo, Top,
    TopoNode, Usage, Workspace,
};
use crate::live::{Focus, Live};
use synthesis_view::convo::{Item, StepState};

/// `124000` -> `124k`, `1_300_000` -> `1.3M`.
pub fn tok(n: u64) -> String {
    if n >= 1_000_000 {
        let m = format!("{:.1}", n as f64 / 1e6);
        format!("{}M", m.trim_end_matches(".0"))
    } else if n >= 100_000 {
        format!("{}k", (n + 500) / 1000)
    } else if n >= 1000 {
        // 12_400 -> `12.4k`: under 100k the tenths matter
        format!(
            "{}k",
            format!("{:.1}", n as f64 / 1000.0).trim_end_matches(".0")
        )
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
        m if m < super::activity::INACTIVE_MIN => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        1440..=2879 => "yesterday".into(),
        _ => format!("{}d", m / 1440),
    }
}

/// The home folder as `~`.
/// The chat header's folder: `~/a/b` stays whole, any other path keeps its last `keep` segments behind `…/`, and the
/// result is at most `max` chars (the front is cut, on char boundaries). The panels cut by width instead.
pub fn short_path(p: &str, keep: usize, max: usize) -> String {
    let segs: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let s = if segs.len() <= keep + usize::from(segs.first() == Some(&"~")) {
        p.to_string()
    } else {
        format!("…/{}", segs[segs.len() - keep..].join("/"))
    };
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

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

/// The word beside the open session's mark.
fn activity_word(l: &Live) -> String {
    match activity_of(l) {
        Some(Activity::Approval) => "waiting approval",
        Some(_) => "running",
        None => "idle",
    }
    .to_string()
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
                title: l.session_name(),
                status: synthesis_core::SessionStatus::Idle,
                updated_at: String::new(),
                mtime: now,
                is_live: true,
            },
        );
    }
    let last_event = v.convo.times.iter().flatten().copied().max().unwrap_or(0);
    let mut out: Vec<Running> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // the open session is known first-hand, the others by their status column in the store; a stored
            // "running" or "waiting approval" counts only while a process holds the session (a dead one leaves it behind)
            let open = r.id == l.sid;
            let state = if open {
                run_state_of_live(l)
            } else {
                match run_state_of_status(r.status) {
                    RunState::Running | RunState::Approval if !r.is_live => RunState::Idle,
                    s => s,
                }
            };
            let last = if open && matches!(state, RunState::Running | RunState::Approval) {
                now
            } else if open {
                r.mtime.max(last_event)
            } else {
                r.mtime
            };
            Running {
                id: r.id.clone(),
                peer: false,
                title: if open {
                    l.session_name()
                } else {
                    r.title.clone()
                },
                folder: folder.to_string(),
                // the open session shows its git branch; a stored one only has its status
                branch: if open && !v.session.git_branch.is_empty() {
                    v.session.git_branch.clone()
                } else {
                    status_word(r.status).to_string()
                },
                state,
                status: if open {
                    activity_word(l)
                } else {
                    status_word(r.status).to_string()
                },
                age: if open && last >= now {
                    "now".into()
                } else {
                    age(now, last)
                },
                last,
                live: r.is_live,
                selected: l.focus == Focus::Running && i == l.run_sel,
            }
        })
        .collect();
    out.extend(v.running.peers.iter().map(|p| Running {
        id: p.address.clone(),
        peer: true,
        title: if p.task.is_empty() {
            format!("{} \u{b7} {}", p.session_short_id, p.agent)
        } else {
            p.task.clone()
        },
        folder: tilde(&p.workspace_label, home),
        branch: p.status.to_string(),
        age: String::new(),
        last: now,
        live: true,
        selected: false,
        state: super::activity::RunState::Running,
        status: p.status.to_string(),
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

/// Days since 1970-01-01 to (year, month 1-12, day) (proleptic Gregorian).
fn civil(z: i64) -> (i64, usize, i64) {
    let z = z + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m as usize, doy - (153 * mp + 2) / 5 + 1)
}

/// The month of the last history day (today) as dots of token intensity, like the CLI's month grid: level 1..=4
/// is the day's share of the month's busiest day, 0 a day with no tokens, `None` a day still to come.
fn month_use(days: &[synthesis_view::usage::HistoryDay], tz_min: i32) -> MonthUse {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(last) = days.last() else {
        return MonthUse::default();
    };
    let day_no = |ms: i64| (ms + i64::from(tz_min) * 60_000).div_euclid(86_400_000);
    let today = day_no(last.date_ms);
    let (y, m, d) = civil(today);
    let first = today - (d - 1);
    let len = (28..=31)
        .rev()
        .find(|n| civil(first + n - 1).1 == m)
        .unwrap_or(28);
    let tokens = |n: i64| {
        days.iter()
            .find(|x| day_no(x.date_ms) == n)
            .map_or(0., |x| x.tokens)
    };
    let (mut total, mut max) = (0., 0f64);
    for n in first..=today {
        total += tokens(n);
        max = max.max(tokens(n));
    }
    MonthUse {
        label: format!("{} {y}", MONTHS[m - 1]),
        total: tok(total as u64),
        days: (first..first + len)
            .map(|n| {
                (n <= today).then(|| match tokens(n) {
                    v if v <= 0. => 0,
                    v => 1 + (((v / max) * 3.999) as u8).min(3),
                })
            })
            .collect(),
    }
}

fn first_line(s: &str, n: usize) -> String {
    s.lines().next().unwrap_or("").chars().take(n).collect()
}

/// Recent Activity: what the person asked and what the agent answered (assistant text), strictly newest first by
/// event time. Commands, tool calls, turns, notices and errors are not listed (the chat and the Running list
/// show those); a row with no event time (a local notice) is not an event and is left out.
fn activity_rows(l: &Live, tz_min: i32) -> Vec<ActivityRow> {
    let c = &l.view.convo;
    let mut v: Vec<(i64, ActivityRow)> = vec![];
    // newest first, so the stable sort keeps the later of two events of the same instant on top
    for (i, it) in c.items.iter().enumerate().rev() {
        let Some(ts) = c.time_of(i) else { continue };
        let (kind, text) = match it {
            Item::User(t) => (ActKind::Prompt, t.clone()),
            Item::Assistant { text, .. } if !text.trim().is_empty() => {
                (ActKind::Reply, text.trim().to_string())
            }
            _ => continue,
        };
        v.push((
            ts,
            ActivityRow {
                kind,
                text: first_line(&text, 200),
                time: clock(ts, tz_min)[..5].to_string(),
                anchor: i,
                shown: super::chat::visible(it, l.show_thinking, l.verbose, l.focus_view),
            },
        ));
    }
    v.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    v.into_iter().take(200).map(|(_, r)| r).collect()
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

    let steps: Vec<Step> = v
        .convo
        .mission(now)
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
            session: l.session_name(),
            model: model.clone(),
            effort: effort.clone(),
            // `?` only when the window is truly unknown
            ctx: if window > 0 { tok(window) } else { "?".into() },
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
                closable: w.key != l.live_ws_key(),
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
                        "?".into()
                    },
                ),
                ("tools".into(), format!("{} used", tools.tools.len())),
                ("cwd".into(), tilde(&l.cwd, home)),
            ]
            .into_iter()
            .chain(
                l.engine
                    .as_ref()
                    .and_then(|e| e.worktree_info())
                    .into_iter()
                    .flat_map(|(path, branch)| {
                        [
                            ("worktree".into(), tilde(&path, home)),
                            ("branch".into(), branch),
                        ]
                    }),
            )
            .chain([(
                "uptime".into(),
                st.uptime_ms(now)
                    .map_or_else(|| "-".into(), |m| uptime(m / 1000)),
            )])
            .collect(),
        },
        tools: ToolsInfo {
            count: total.to_string(),
            rows: trows,
        },
        activity: activity_rows(l, tz_min),
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
            path: short_path(&tilde(&l.cwd, home), 2, 20),
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
            month: month_use(&v.usage.days, tz_min),
        },
        tips: vec![
            "Ask for a change in plain words; Shift+Tab cycles the permission mode.".into(),
            "Tab moves between the chat, Workspace and Running.".into(),
            "/ completes a command, @ mentions a file; Esc interrupts a running task.".into(),
            "Click a session in Running to continue it where it stopped.".into(),
        ],
        mode,
        effort,
        roster: synthesis_view::lifecycle::roster(&v.panes.subs, now),
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
            if let Some(b) = &st.budget_exceeded {
                c.push(StatusChip {
                    text: format!("budget exceeded: {b}"),
                    warn: true,
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
        pulses: v.session.pulses.clone(),
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
        let digits = l.dialogs[0].digits_pick();
        return vec![
            h("\u{2191}\u{2193}", "choose"),
            h("Enter", "confirm"),
            if digits {
                h("1-9", "pick")
            } else {
                h("type", "filter")
            },
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
    #[test]
    fn short_path_keeps_the_tail_and_never_splits_a_char() {
        use super::short_path as sp;
        assert_eq!(sp("~/Vibe/synthesis", 2, 28), "~/Vibe/synthesis");
        assert_eq!(sp("/", 2, 28), "/");
        assert_eq!(sp("/private/var/folders/x/ws", 2, 28), "…/x/ws");
        assert_eq!(sp("~/a/b/c", 2, 28), "…/b/c");
        assert_eq!(sp("/a/日本語日本語日本語日本語日本語", 1, 6), "…本語日本語");
        assert_eq!(sp("C:\\Users\\me\\code\\ws", 2, 28), "…/code/ws");
    }

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
    fn an_exceeded_budget_is_a_warning_chip() {
        let mut l = Live::new();
        l.starting = false;
        let chips = |l: &Live| build(l, 0, 0, None).status;
        assert!(chips(&l).iter().all(|c| !c.text.starts_with("budget")));
        l.view.apply(&ev(
            1,
            "2026-11-07T10:00:00Z",
            "budget.exceeded",
            json!({"budget": "turns", "limit": 5.0, "used": 5.0}),
        ));
        let c = chips(&l);
        let b = c.iter().find(|c| c.text.starts_with("budget")).unwrap();
        assert!(b.warn && b.text == "budget exceeded: turns 5 / 5", "{b:?}");
    }

    #[test]
    fn the_top_bar_names_the_session_like_its_running_row() {
        use synthesis_view::workspaces::{WorkspaceRow, fold_dir};
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/w/proj".into();
        l.sid = "s1".into();
        l.ws_key = fold_dir("/w/proj");
        l.view.workspaces.workspaces.push(WorkspaceRow {
            root: "/w/proj".into(),
            key: l.ws_key.clone(),
            label: "proj".into(),
            sessions: vec![],
            available: true,
            changes: None,
        });
        let names = |l: &Live| {
            let s = build(l, 0, 0, None);
            let r = s.running.iter().find(|r| r.live).unwrap().title.clone();
            (s.top.session, r)
        };
        let user = |l: &mut Live, n: u64, t: &str| {
            l.view.apply(&ev(
                n,
                "2026-11-07T10:00:00Z",
                "user.message",
                json!({"text": t, "images": []}),
            ))
        };
        assert_eq!(names(&l), ("(new session)".into(), "(new session)".into()));
        user(&mut l, 1, "first  prompt");
        assert_eq!(names(&l), ("first prompt".into(), "first prompt".into()));
        user(&mut l, 2, "second prompt");
        assert_eq!(names(&l), ("first prompt".into(), "first prompt".into()));
        l.renamed = Some(("s1".into(), "My name".into()));
        assert_eq!(names(&l), ("My name".into(), "My name".into()));
    }

    #[test]
    fn a_stored_running_status_counts_only_while_a_process_holds_the_session() {
        use synthesis_core::SessionStatus::*;
        use synthesis_view::workspaces::{SessionRow, WorkspaceRow, fold_dir};
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/w/proj".into();
        l.sid = "open".into();
        l.ws_key = fold_dir("/w/proj");
        let now = 10 * 3_600_000;
        let row = |id: &str, status, is_live, age_min: i64| SessionRow {
            id: id.into(),
            title: id.into(),
            status,
            updated_at: String::new(),
            mtime: now - age_min * 60_000,
            is_live,
        };
        l.view.workspaces.workspaces.push(WorkspaceRow {
            root: "/w/proj".into(),
            key: l.ws_key.clone(),
            label: "proj".into(),
            sessions: vec![
                row("held", Running, true, 1),
                row("dead", Running, false, 1),
                row("ask", WaitingApproval, true, 1),
                row("stale-ask", WaitingApproval, false, 1),
                row("bad", Failed, false, 120),
                row("blocked", Blocked, false, 5),
                row("idle", Completed, false, 45),
            ],
            available: true,
            changes: None,
        });
        let st = |s: &Snapshot, id: &str| s.running.iter().find(|r| r.id == id).unwrap().state;
        let s = build(&l, now, 0, None);
        assert_eq!(st(&s, "held"), RunState::Running);
        assert_eq!(st(&s, "dead"), RunState::Idle);
        assert_eq!(st(&s, "ask"), RunState::Approval);
        assert_eq!(st(&s, "stale-ask"), RunState::Idle);
        assert_eq!(st(&s, "bad"), RunState::Error);
        assert_eq!(st(&s, "blocked"), RunState::Error);
        assert_eq!(st(&s, "idle"), RunState::Idle);
        assert_eq!(
            s.running.iter().find(|r| r.id == "idle").unwrap().age,
            "45m"
        );
    }

    #[test]
    fn recent_activity_lists_prompts_and_answers_only_newest_first() {
        let mut l = Live::new();
        l.starting = false;
        let mut n = 0;
        let mut send = |l: &mut Live, ts: &str, ty: &str, p: serde_json::Value| {
            n += 1;
            l.view.apply(&ev(n, ts, ty, p));
        };
        let tool = |id: &str, cmd: &str| json!({"tool_call_id": id, "name": "Bash", "input": {"command": cmd}, "input_truncated": false});
        let done = |id: &str, status: &str, err: bool| json!({"tool_call_id": id, "name": "Bash", "status": status, "is_error": err, "not_run": false, "duration_ms": 3, "preview": "p", "bytes": 1, "truncated": false});
        let said =
            |t: &str| json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": t}]});
        send(
            &mut l,
            "2026-11-07T10:00:00Z",
            "user.message",
            json!({"text": "first", "images": []}),
        );
        send(
            &mut l,
            "2026-11-07T10:01:00Z",
            "tool.requested",
            tool("a", "ls"),
        );
        send(
            &mut l,
            "2026-11-07T10:01:05Z",
            "tool.completed",
            done("a", "ok", false),
        );
        send(
            &mut l,
            "2026-11-07T10:01:30Z",
            "assistant.message",
            said("listed it"),
        );
        send(
            &mut l,
            "2026-11-07T10:02:00Z",
            "user.message",
            json!({"text": "second", "images": []}),
        );
        send(
            &mut l,
            "2026-11-07T10:03:00Z",
            "tool.requested",
            tool("b", "make"),
        );
        send(
            &mut l,
            "2026-11-07T10:03:05Z",
            "tool.completed",
            done("b", "error", true),
        );
        let rows = build(&l, 0, 0, None).activity;
        // the failed command and the tool calls are not listed
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.kind, r.time.as_str(), r.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (ActKind::Prompt, "10:02", "second"),
                (ActKind::Reply, "10:01", "listed it"),
                (ActKind::Prompt, "10:00", "first"),
            ]
        );
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
        assert_eq!(
            (
                s.activity[0].kind,
                s.activity[0].text.as_str(),
                s.activity[0].anchor
            ),
            (ActKind::Prompt, "run the tests", 0)
        );
        assert_eq!(s.topology.len(), 8);
        let tools = s.topology.iter().find(|n| n.pos == Pos::Tools).unwrap();
        assert_eq!(tools.state, NodeState::Active, "{tools:?}");
        assert!(tools.lines[0].contains("Bash"));
        assert_eq!(s.usage.rows.len(), 6);
        assert_eq!(s.usage.rows[1].label, "Tool calls");
        assert!(s.roster.is_empty());
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
    fn the_month_is_one_dot_per_day_up_to_today() {
        use synthesis_view::usage::HistoryDay;
        // 2026-11-10 (day 20767), 02:00 local at +120 min: the day starts at local midnight
        let day = |n: i64, tokens: f64| HistoryDay {
            date_ms: n * 86_400_000 - 120 * 60_000,
            tokens,
            ..Default::default()
        };
        let h = [
            day(20_760, 850.),
            day(20_765, 100.),
            day(20_766, 0.),
            day(20_767, 50.),
        ];
        let m = month_use(&h, 120);
        assert_eq!((m.label.as_str(), m.total.as_str()), ("Nov 2026", "1k"));
        assert_eq!(m.days.len(), 30);
        // Nov 3 is the busiest day (4); Nov 8 has 100 and Nov 10 has 50 of its 850 (1); Nov 9 none; Nov 11 to come
        assert_eq!(
            (m.days[0], m.days[2], m.days[8]),
            (Some(0), Some(4), Some(0))
        );
        assert_eq!((m.days[7], m.days[9], m.days[10]), (Some(1), Some(1), None));
        assert_eq!(month_use(&[], 0).days.len(), 0);
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
