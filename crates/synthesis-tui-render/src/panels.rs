//! The panels of the registry that are not Workspace, Running, Session or Chat (those live in
//! `dash.rs`): agent status, context, tools & skills, recent activity, session stats, topology,
//! usage & metrics, system health and sub-agents. Every number comes from the session model, the
//! agent, the usage tap, the MCP status or the host; where none exists the panel says "no data yet"
//! (spec docs/specs/tui-design.md §14.4). Nothing here invents a value.

use crate::agent::{self, Agent};
use crate::dash::{
    DashState, Lines, Rects, VAlign, col_accent, col_bad, col_muted, col_ok, col_skill, col_subtle,
    col_text, col_tool, col_warn, col_white, context_size, ctx_pct_of, level_color, mcp_lines,
    mcp_rows_of, mode_line, model_of, open_todos, phase_label, section, u,
};
use crate::model::{Phase, Session, session_phase};
use crate::theme::*;
use crate::util::{basename, fmt_dur, fmt_num, pad_end, pad_start, short, to_fixed};
use synthesis_tui_base::text::width;

/// `left` flush left, `right` flush right in `inner` cells (at least one space between).
fn lr(left: &str, right: &str, inner: i64) -> String {
    let gap = u(inner - width(left) as i64 - width(right) as i64).max(1);
    format!("{left}{}{right}", " ".repeat(gap))
}

/// `label  value`: the label in the muted tone, the value flush right.
fn kv(label: &str, value: &str, inner: i64) -> String {
    lr(&paint(col_muted(), label), value, inner)
}

fn no_data() -> String {
    paint(col_subtle(), "no data yet")
}

// ---------------------------------------------------------------- counters

/// Facts counted from the session's tool calls. All of them are events that happened.
#[derive(Default, Debug, PartialEq)]
pub struct Counters {
    pub files_read: usize,
    pub files_edited: usize,
    pub commands: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub browser: usize,
    pub mcp_calls: usize,
}

impl Counters {
    /// Share of finished tool calls that did not fail; `None` before any tool call.
    pub fn success_pct(&self) -> Option<f64> {
        (self.tool_calls > 0).then(|| {
            (self.tool_calls - self.tool_errors.min(self.tool_calls)) as f64 * 100.0
                / self.tool_calls as f64
        })
    }
}

pub fn counters(session: &Session) -> Counters {
    let mut read: Vec<&str> = vec![];
    let mut edited: Vec<&str> = vec![];
    let mut c = Counters::default();
    for t in session.tools.values() {
        c.tool_calls += 1;
        c.tool_errors += usize::from(t.is_error);
        match t.name.as_str() {
            "Read" => {
                if !read.contains(&t.summary.as_str()) {
                    read.push(&t.summary)
                }
            }
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                if !edited.contains(&t.summary.as_str()) {
                    edited.push(&t.summary)
                }
            }
            "Bash" | "PowerShell" => c.commands += 1,
            n if n.to_lowercase().contains("browser") => c.browser += 1,
            n if n.starts_with("mcp__") => c.mcp_calls += 1,
            _ => {}
        }
    }
    c.files_read = read.len();
    c.files_edited = edited.len();
    c
}

fn age(ts: Option<i64>, now: i64) -> String {
    match ts.filter(|t| *t > 0) {
        Some(t) => format!("{} ago", fmt_dur(Some((now - t).max(0) as f64))),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- agent status and context

const PHASES: [&str; 4] = ["ready", "planning", "coding", "verifying"];

/// Index into `PHASES` of what the agent is doing now.
fn phase_index(s: &DashState, now: i64) -> usize {
    let ph = session_phase(s.session, now);
    match ph.phase {
        Phase::Idle => 0,
        Phase::Thinking | Phase::Agents => 1,
        Phase::Tool => {
            if ph.running.last().is_some_and(|t| t.cat == "verify") {
                3
            } else {
                2
            }
        }
    }
}

fn todo_rows(s: &DashState, rows: usize, inner: i64, frame: usize) -> Lines {
    let todos = &s.session.todos;
    let cap = rows.min(todos.len());
    let start = agent::todo_window(todos, cap);
    todos[start..start + cap]
        .iter()
        .map(|t| {
            let g = glyphs();
            let (mark, c, text) = match t.status.as_str() {
                "in_progress" => (
                    spin(frame).to_string(),
                    col_warn(),
                    if t.active_form.is_empty() {
                        &t.content
                    } else {
                        &t.active_form
                    },
                ),
                "completed" => (g.ok.to_string(), col_ok(), &t.content),
                _ => (g.idle.to_string(), col_subtle(), &t.content),
            };
            format!(
                "{} {}",
                paint(c, &mark),
                paint(
                    if t.status == "completed" {
                        col_muted()
                    } else {
                        col_text()
                    },
                    &short(text, u((inner - 2).max(4)))
                )
            )
        })
        .collect()
}

pub fn agent_status(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> Lines {
    let inner = w - 2;
    let rows = u(h - 1);
    let idx = phase_index(s, now);
    let chain: Vec<String> = PHASES
        .iter()
        .enumerate()
        .map(|(i, p)| paint(if i == idx { col_white() } else { col_subtle() }, p))
        .collect();
    let sep = paint(col_subtle(), &format!(" {} ", glyphs().dot));
    let ph = session_phase(s.session, now);
    let working = ph
        .running
        .last()
        .map(|t| format!("{} {}", t.name, t.summary))
        .or_else(|| {
            s.session
                .todos
                .iter()
                .find(|t| t.status == "in_progress")
                .map(|t| {
                    if t.active_form.is_empty() {
                        t.content.clone()
                    } else {
                        t.active_form.clone()
                    }
                })
        })
        .or_else(|| Some(s.session.last_prompt.clone()).filter(|p| !p.is_empty()));
    let next = s
        .session
        .todos
        .iter()
        .find(|t| t.status == "pending")
        .map(|t| t.content.clone());
    let two = inner >= 56 && !s.session.todos.is_empty();
    let left_w = if two { inner * 55 / 100 } else { inner };
    let mut left: Lines = vec![fit(
        &format!("{}{}{}", chain[0], sep, chain[1..].join(&sep)),
        u(left_w),
    )];
    let line = |k: &str, v: Option<String>, none: &str| {
        format!(
            "{}{}",
            paint(col_muted(), &pad_end(k, 12)),
            match v {
                Some(v) => paint(col_text(), &short(&v, u((left_w - 12).max(4)))),
                None => paint(col_subtle(), none),
            }
        )
    };
    left.push(line("Working on", working, "nothing yet"));
    left.push(line("Next", next, "no plan yet"));
    let lines = if two {
        let right = todo_rows(s, rows, inner - left_w - 2, frame);
        (0..rows.max(left.len()))
            .map(|i| {
                format!(
                    "{}  {}",
                    fit(left.get(i).map_or("", String::as_str), u(left_w)),
                    right.get(i).map_or("", String::as_str)
                )
            })
            .collect()
    } else {
        left
    };
    let (word, c) = if idx == 0 {
        ("ready", col_muted())
    } else {
        (PHASES[idx], phase_label(&ph.phase).1)
    };
    let right = if idx == 0 {
        paint(c, word)
    } else {
        format!("{} {}", paint(c, spin(frame)), paint(c, word))
    };
    section("Agent status", &right, lines, w, h, idx != 0, VAlign::Top)
}

/// Window meter as `▰▱` cells, coloured by how full it is.
fn meter(pct: f64, cells: usize) -> String {
    let g = glyphs();
    let on = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    format!(
        "{}{}",
        paint(level_color(pct), &g.meter_full.repeat(on.min(cells))),
        paint(col_subtle(), &g.meter_empty.repeat(cells - on.min(cells)))
    )
}

pub fn context(s: &DashState, w: i64, h: i64, _frame: usize, _now: i64) -> Lines {
    let inner = w - 2;
    let session = s.session;
    let tap = s.usage.tap.as_ref();
    let max = context_size(session, tap, s.agent.and_then(|a| a.context_window));
    let pct = ctx_pct_of(s);
    let t = &session.tokens;
    let n = |v: u64| fmt_num(Some(v as f64));
    let denom = t.cache_read + t.input + t.cache_create;
    let hit = (denom > 0).then(|| t.cache_read as f64 * 100.0 / denom as f64);
    let known = session.context_used > 0 || tap.is_some_and(|t| t.context_pct.is_some());
    let mut lines = vec![
        if known {
            kv(
                "tokens",
                &format!(
                    "{} / {}",
                    paint(col_white(), &fmt_num(Some(session.context_used as f64))),
                    paint(col_muted(), &fmt_num(Some(max)))
                ),
                inner,
            )
        } else {
            kv("tokens", &no_data(), inner)
        },
        meter(if known { pct } else { 0.0 }, u(inner)),
        format!(
            "{} {} · {} {}",
            paint(col_muted(), "in"),
            paint(col_text(), &n(t.input)),
            paint(col_muted(), "out"),
            paint(col_text(), &n(t.output))
        ),
    ];
    lines.push(format!(
        "{} {}{} · {}",
        paint(col_muted(), "cache"),
        paint(col_text(), &n(t.cache_read)),
        hit.map_or(String::new(), |p| paint(
            col_muted(),
            &format!(" (hit {}%)", to_fixed(p, 0))
        )),
        paint(
            col_warn(),
            &tap.and_then(|t| t.cost_usd)
                .map_or("$ –".into(), |c| format!("${}", to_fixed(c, 2)))
        )
    ));
    let right = if known {
        paint(level_color(pct), &format!("{}%", to_fixed(pct, 0)))
    } else {
        String::new()
    };
    section(
        "Context",
        &right,
        lines.into_iter().take(u(h - 1)).collect(),
        w,
        h,
        false,
        VAlign::Top,
    )
}

// ---------------------------------------------------------------- left column

pub fn tools_skills(s: &DashState, w: i64, h: i64, frame: usize) -> Lines {
    let inner = w - 2;
    let room = u(h - 1);
    let mut lines: Lines = vec![];
    let mut tools: Vec<&(String, u32)> = s.session.tool_counts.iter().collect();
    tools.sort_by_key(|e| std::cmp::Reverse(e.1));
    let max = tools.first().map_or(1, |e| e.1) as f64;
    let name_w = (inner * 35 / 100).clamp(6, 12);
    let bw = (inner - name_w - 8).max(2);
    // the MCP block is kept whole: one summary row and the servers that fit
    let (mcp_n, mcp) = mcp_lines(s, w, room.saturating_sub(2), frame);
    let mcp_rows = if mcp_n + mcp.len() == 0 {
        0
    } else {
        (1 + mcp.len()).min(room.saturating_sub(1))
    };
    let skills = if s.session.skills.is_empty() { 0 } else { 1 };
    let tool_room = room.saturating_sub(mcp_rows + skills);
    for (name, n) in tools.iter().take(tool_room) {
        let len = 1.max(js_round(*n as f64 / max * bw as f64) as usize);
        lines.push(format!(
            "{} {} {} {}",
            paint(col_ok(), glyphs().live),
            paint(col_text(), &pad_end(&short(name, u(name_w)), u(name_w))),
            paint(col_tool(), &glyphs().bar.repeat(len)),
            paint(col_muted(), &n.to_string())
        ));
    }
    if tools.is_empty() {
        lines.push(paint(col_subtle(), "no tool calls yet"));
    }
    if skills > 0 && lines.len() < room {
        lines.push(format!(
            "{} {}",
            paint(col_muted(), "skills"),
            s.session
                .skills
                .iter()
                .map(|(k, n)| paint(col_skill(), &format!("{k} ×{n}")))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if mcp_rows > 0 && lines.len() < room {
        lines.push(kv(
            "mcp",
            &paint(
                col_white(),
                &format!("{mcp_n} server{}", if mcp_n == 1 { "" } else { "s" }),
            ),
            inner,
        ));
        lines.extend(mcp.into_iter().take(room.saturating_sub(lines.len())));
    }
    let right = paint(col_white(), &s.session.tool_counts.len().to_string());
    section("Tools & skills", &right, lines, w, h, false, VAlign::Top)
}

fn feed_icon(k: &str) -> (&'static str, Rgb) {
    let g = glyphs();
    match k {
        "prompt" => (g.user, col_white()),
        "tool" => (g.tool, col_tool()),
        "agent" => (g.attention, crate::dash::col_agent()),
        "skill" => ("★", col_skill()),
        "error" => (g.fail, col_bad()),
        "hook" => ("↯", col_warn()),
        "cmd" => ("/", col_accent()),
        _ => (g.dot, col_text()),
    }
}

/// Newest event first, each with its age. `scroll` rows back from the newest.
pub fn recent_activity(s: &DashState, w: i64, h: i64, now: i64, rects: &mut Rects) -> Lines {
    let inner = w - 2;
    let rows = u(h - 1);
    let activity = &s.session.activity;
    let max_scroll = activity.len().saturating_sub(rows);
    let scroll = s.view.activity_scroll.min(max_scroll);
    let newest = activity.len() - scroll; // exclusive
    let first = newest.saturating_sub(rows);
    rects.activity_max_scroll = max_scroll;
    rects.activity_scroll = scroll;
    rects.activity_hits = (first..newest).rev().enumerate().collect();
    let mut lines: Lines = (first..newest)
        .rev()
        .map(|k| {
            let f = &activity[k];
            let (icon, c) = feed_icon(f.kind);
            let a = age(f.ts, now);
            let text_w = (inner - 2 - width(&a) as i64 - 1).max(4);
            lr(
                &format!(
                    "{} {}",
                    paint(c, icon),
                    paint(col_text(), &short(&f.text, u(text_w)))
                ),
                &paint(col_subtle(), &a),
                inner,
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push(paint(col_subtle(), "no events yet"));
    }
    let right = if activity.is_empty() {
        String::new()
    } else {
        paint(col_white(), &activity.len().to_string())
    };
    section("Recent activity", &right, lines, w, h, false, VAlign::Top)
}

pub fn session_stats(s: &DashState, w: i64, h: i64, now: i64) -> Lines {
    let inner = w - 2;
    let c = counters(s.session);
    let v = |n: usize| paint(col_white(), &n.to_string());
    let have_calls = c.tool_calls > 0;
    let lines = vec![
        kv("files read", &v(c.files_read), inner),
        kv("files edited", &v(c.files_edited), inner),
        kv("commands run", &v(c.commands), inner),
        kv(
            "tokens used",
            &paint(col_white(), &fmt_num(Some(s.session.tokens.total() as f64))),
            inner,
        ),
        kv("tool calls", &v(c.tool_calls), inner),
        kv(
            "success rate",
            &c.success_pct().map_or_else(no_data, |p| {
                paint(
                    if have_calls && p < 100.0 {
                        col_warn()
                    } else {
                        col_ok()
                    },
                    &format!("{}%", to_fixed(p, 0)),
                )
            }),
            inner,
        ),
    ];
    let up = s
        .session
        .started_at
        .filter(|t| *t > 0)
        .map_or(String::new(), |t| {
            paint(col_white(), &fmt_dur(Some((now - t).max(0) as f64)))
        });
    section("Session stats", &up, lines, w, h, false, VAlign::Top)
}

// ---------------------------------------------------------------- right column

/// One line per state of the system, from what the host and the runtime say.
#[derive(Clone, Debug, PartialEq)]
pub enum Level {
    Ok,
    Warn,
    Bad,
    Unknown,
}

pub struct HealthItem {
    pub name: &'static str,
    pub level: Level,
    pub text: String,
}

pub fn health_items(s: &DashState) -> Vec<HealthItem> {
    let core = match s.agent {
        Some(a) if !a.pending.is_empty() => (Level::Warn, "waiting for you".to_string()),
        Some(a) if a.status != "idle" => (Level::Ok, "working".to_string()),
        Some(_) => (Level::Ok, "ready".to_string()),
        None => (Level::Unknown, "no agent attached".to_string()),
    };
    let runtime = match s.host.sandbox_mode.as_deref() {
        Some("off") => (Level::Warn, "sandbox off".to_string()),
        Some(m) => (Level::Ok, format!("sandbox {m}")),
        None => (Level::Unknown, "no data yet".to_string()),
    };
    let (live, rows) = mcp_rows_of(s);
    let enabled: Vec<_> = rows.iter().filter(|r| r.state != "disabled").collect();
    let mcp = if enabled.is_empty() {
        (Level::Unknown, "none configured".to_string())
    } else if live {
        let ok = enabled.iter().filter(|r| r.state == "connected").count();
        let failed = enabled.iter().filter(|r| r.state == "failed").count();
        let level = if failed > 0 {
            Level::Bad
        } else if ok == enabled.len() {
            Level::Ok
        } else {
            Level::Warn
        };
        (level, format!("{ok}/{} online", enabled.len()))
    } else {
        (Level::Unknown, format!("{} configured", enabled.len()))
    };
    let memory = match s.host.memory {
        Some(true) => (Level::Ok, "available".to_string()),
        Some(false) => (Level::Unknown, "not available".to_string()),
        None => (Level::Unknown, "no data yet".to_string()),
    };
    [
        ("Core agent", core),
        ("Tool runtime", runtime),
        ("MCP servers", mcp),
        ("Memory store", memory),
        (
            "Browser sandbox",
            (Level::Unknown, "no data yet".to_string()),
        ),
    ]
    .into_iter()
    .map(|(name, (level, text))| HealthItem { name, level, text })
    .collect()
}

/// `(level of the whole, word)`: degraded when a server failed, attention when something needs a look.
pub fn overall_health(s: &DashState) -> (Level, &'static str) {
    let items = health_items(s);
    if items.iter().any(|i| i.level == Level::Bad) {
        (Level::Bad, "degraded")
    } else if items.iter().any(|i| i.level == Level::Warn) {
        (Level::Warn, "attention")
    } else {
        (Level::Ok, "healthy")
    }
}

pub fn level_colour(l: &Level) -> Rgb {
    match l {
        Level::Ok => col_ok(),
        Level::Warn => col_warn(),
        Level::Bad => col_bad(),
        Level::Unknown => col_subtle(),
    }
}

pub fn system_health(s: &DashState, w: i64, h: i64) -> Lines {
    let inner = w - 2;
    let g = glyphs();
    let lines: Lines = health_items(s)
        .into_iter()
        .map(|i| {
            let dot = match i.level {
                Level::Unknown => g.idle,
                _ => g.live,
            };
            lr(
                &format!(
                    "{} {}",
                    paint(level_colour(&i.level), dot),
                    paint(col_text(), i.name)
                ),
                &paint(
                    level_colour(&i.level),
                    &short(&i.text, u((inner - 18).max(6))),
                ),
                inner,
            )
        })
        .collect();
    let (lvl, word) = overall_health(s);
    section(
        "System health",
        &paint(level_colour(&lvl), word),
        lines,
        w,
        h,
        false,
        VAlign::Top,
    )
}

pub fn usage_metrics(s: &DashState, w: i64, h: i64) -> Lines {
    let inner = w - 2;
    let c = counters(s.session);
    let tap = s.usage.tap.as_ref();
    let max = context_size(s.session, tap, s.agent.and_then(|a| a.context_window));
    // (label, value shown, bar fill 0..=1)
    let mut rows: Vec<(&str, String, f64)>;
    let right;
    if s.history.is_empty() {
        let used = s.session.tokens.total() as f64;
        let peak = [
            c.tool_calls,
            c.files_edited,
            c.commands,
            c.browser,
            c.mcp_calls,
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
        .max(1) as f64;
        let cnt = |n: usize| (n.to_string(), n as f64 / peak);
        rows = vec![(
            "Tokens",
            format!("{} / {}", fmt_num(Some(used)), fmt_num(Some(max))),
            (used / max).min(1.0),
        )];
        for (label, n) in [
            ("Tool calls", c.tool_calls),
            ("Files modified", c.files_edited),
            ("Commands run", c.commands),
            ("Browser requests", c.browser),
            ("MCP calls", c.mcp_calls),
        ] {
            let (v, f) = cnt(n);
            rows.push((label, v, f));
        }
        right = paint(col_muted(), "this session");
    } else {
        // the last 7 days of the usage history: what the history holds (no files or commands)
        let week = &s.history[s.history.len().saturating_sub(7)..];
        let sum = |f: fn(&crate::dash::HistoryDay) -> f64| week.iter().map(f).sum::<f64>();
        let peak = [
            sum(|d| d.tools),
            sum(|d| d.messages),
            sum(|d| d.agents),
            sum(|d| d.sessions),
        ]
        .into_iter()
        .fold(1.0, f64::max);
        let n = |v: f64| crate::util::round_str(v);
        rows = vec![
            ("Tokens", fmt_num(Some(sum(|d| d.tokens))), 1.0),
            ("Tool calls", n(sum(|d| d.tools)), sum(|d| d.tools) / peak),
            (
                "Messages",
                n(sum(|d| d.messages)),
                sum(|d| d.messages) / peak,
            ),
            ("Agents", n(sum(|d| d.agents)), sum(|d| d.agents) / peak),
            (
                "Sessions",
                n(sum(|d| d.sessions)),
                sum(|d| d.sessions) / peak,
            ),
        ];
        right = paint(col_muted(), "7 days");
    }
    let label_w = rows.iter().map(|r| width(r.0)).max().unwrap_or(0).min(16) as i64;
    let val_w = rows.iter().map(|r| width(&r.1)).max().unwrap_or(0) as i64;
    let bar_w = (inner - label_w - val_w - 2).max(0);
    let lines: Lines = rows
        .iter()
        .map(|(label, v, f)| {
            let filled = ((f.clamp(0.0, 1.0)) * bar_w as f64).round() as usize;
            let bar = format!(
                "{}{}",
                paint(col_tool(), &"█".repeat(filled.min(u(bar_w)))),
                paint(col_track(), &"░".repeat(u(bar_w) - filled.min(u(bar_w))))
            );
            let left = paint(col_muted(), &pad_end(label, u(label_w)));
            if bar_w >= 4 {
                format!(
                    "{left} {bar} {}",
                    paint(col_white(), &pad_start(v, u(val_w)))
                )
            } else {
                lr(&left, &paint(col_white(), v), inner)
            }
        })
        .collect();
    let mut lines = lines;
    if let Some(cost) = tap.and_then(|t| t.cost_usd) {
        lines.push(kv(
            "Cost (est)",
            &paint(col_white(), &format!("${}", to_fixed(cost, 2))),
            inner,
        ));
    }
    section("Usage & metrics", &right, lines, w, h, false, VAlign::Top)
}

fn col_track() -> Rgb {
    t().track()
}

// ---------------------------------------------------------------- topology

struct Card {
    title: &'static str,
    colour: Rgb,
    rows: Vec<String>,
}

fn card_box(c: &Card, w: i64, detail: usize) -> Lines {
    let b = |s: &str| paint(c.colour, s);
    let inner = u(w - 4);
    let title = format!(" {} ", c.title.to_uppercase());
    let top = format!(
        "{}{}{}",
        b("╭─"),
        paint(c.colour, &title),
        b(&format!("{}╮", "─".repeat(u(w - 3 - width(&title) as i64))))
    );
    let mut out = vec![top];
    for i in 0..detail {
        out.push(format!(
            "{} {} {}",
            b("│"),
            fit(c.rows.get(i).map_or("", String::as_str), inner),
            b("│")
        ));
    }
    out.push(b(&format!("╰{}╯", "─".repeat(u(w - 2)))));
    out
}

fn dot(c: Rgb) -> String {
    paint(c, glyphs().live)
}

fn cards(s: &DashState, now: i64) -> Vec<Card> {
    let th = t();
    let c = counters(s.session);
    let session = s.session;
    let g = glyphs();
    let subs: &[Agent] = s.agent.map_or(&[], |a| &a.subagents);
    let val = |k: &str, v: String| format!("{} {v}", paint(col_muted(), k));
    let project = {
        let b = basename(&session.cwd);
        if b.is_empty() {
            session.project.clone()
        } else {
            b
        }
    };
    let ph = session_phase(session, now);
    let live = s.agent.is_some();
    let user = Card {
        title: "User / project",
        colour: th.teal(),
        rows: vec![
            format!(
                "{} {}",
                dot(if live { col_ok() } else { col_subtle() }),
                paint(
                    col_text(),
                    if live { "active session" } else { "no session" }
                )
            ),
            paint(col_text(), &project),
            if session.last_prompt.is_empty() {
                paint(col_subtle(), "no prompt yet")
            } else {
                paint(col_muted(), &session.last_prompt)
            },
        ],
    };
    let hooks_blocked: u32 = session
        .components
        .values()
        .filter(|x| x.kind == "hook")
        .map(|x| x.blocked)
        .sum();
    let guards = Card {
        title: "Guards",
        colour: th.orange(),
        rows: vec![
            val(
                "sandbox",
                s.host.sandbox_mode.clone().map_or_else(no_data, |m| {
                    paint(if m == "off" { col_warn() } else { col_ok() }, &m)
                }),
            ),
            val(
                "approvals",
                paint(
                    col_text(),
                    &s.agent.map_or("–".to_string(), |a| mode_line(a).0),
                ),
            ),
            val(
                "blocked",
                paint(
                    if hooks_blocked > 0 {
                        col_warn()
                    } else {
                        col_text()
                    },
                    &format!("{hooks_blocked} by hooks"),
                ),
            ),
        ],
    };
    let mut top_tools: Vec<&(String, u32)> = session.tool_counts.iter().collect();
    top_tools.sort_by_key(|e| std::cmp::Reverse(e.1));
    let skills = Card {
        title: "Skills",
        colour: th.ok,
        rows: if session.skills.is_empty() {
            vec![paint(col_subtle(), "no skills used yet")]
        } else {
            session
                .skills
                .iter()
                .map(|(k, n)| {
                    format!(
                        "{} {}",
                        dot(col_ok()),
                        paint(col_text(), &format!("{k} ×{n}"))
                    )
                })
                .collect()
        },
    };
    let sub = Card {
        title: "Sub-agents",
        colour: th.purple(),
        rows: if subs.is_empty() {
            vec![
                paint(col_text(), "no sub-agents"),
                paint(col_subtle(), "single agent mode"),
            ]
        } else {
            subs.iter()
                .map(|a| {
                    lr_plain(
                        &format!(
                            "{} {}",
                            dot(if a.status == "running" {
                                col_ok()
                            } else {
                                col_subtle()
                            }),
                            paint(col_text(), &a.display_name())
                        ),
                        &paint(col_muted(), &a.status),
                    )
                })
                .collect()
        },
    };
    let core = Card {
        title: "Synthesis core",
        colour: th.border,
        rows: vec![paint(
            col_text(),
            &format!(
                "plan {a} act {a} verify {a} learn",
                a = if g.ok == "+" { "->" } else { "→" }
            ),
        )],
    };
    let exec = Card {
        title: "Execution",
        colour: th.teal(),
        rows: vec![
            val("tool calls", paint(col_white(), &c.tool_calls.to_string())),
            val("running", paint(col_white(), &ph.running.len().to_string())),
            val(
                "errors",
                paint(
                    if c.tool_errors > 0 {
                        col_bad()
                    } else {
                        col_white()
                    },
                    &c.tool_errors.to_string(),
                ),
            ),
            val(
                "success",
                c.success_pct().map_or_else(no_data, |p| {
                    paint(col_ok(), &format!("{}%", to_fixed(p, 0)))
                }),
            ),
        ],
    };
    let hooks: Vec<_> = session
        .components
        .values()
        .filter(|x| x.kind == "hook")
        .collect();
    let hk = Card {
        title: "Hooks",
        colour: th.warn,
        rows: if hooks.is_empty() {
            vec![paint(col_subtle(), "no hook events yet")]
        } else {
            hooks
                .iter()
                .map(|x| {
                    format!(
                        "{} {}",
                        dot(if x.error { col_bad() } else { col_ok() }),
                        paint(col_text(), &format!("{} ×{}", x.name, x.count))
                    )
                })
                .collect()
        },
    };
    let (_, mcp_rows) = mcp_rows_of(s);
    let mcp = Card {
        title: "External / MCP",
        colour: th.blue(),
        rows: if mcp_rows.is_empty() {
            vec![paint(col_subtle(), "no servers")]
        } else {
            mcp_rows
                .iter()
                .map(|r| {
                    let c = match r.state.as_str() {
                        "connected" => col_ok(),
                        "failed" => col_bad(),
                        "disabled" => col_subtle(),
                        _ => col_warn(),
                    };
                    format!("{} {}", dot(c), paint(col_text(), &r.name))
                })
                .collect()
        },
    };
    let verify = session.last_verify.as_ref().map_or_else(
        || paint(col_subtle(), "none yet"),
        |v| {
            paint(
                if v.ok { col_ok() } else { col_bad() },
                &format!("{} {}", if v.ok { g.ok } else { g.fail }, v.name),
            )
        },
    );
    let memory = Card {
        title: "Memory / feedback",
        colour: th.err,
        rows: vec![
            val(
                "writes",
                paint(col_white(), &session.memory_writes.to_string()),
            ),
            val("last verify", verify),
            val(
                "store",
                s.host.memory.map_or_else(no_data, |m| {
                    paint(col_text(), if m { "available" } else { "not available" })
                }),
            ),
        ],
    };
    vec![user, guards, skills, sub, core, exec, hk, mcp, memory]
}

fn lr_plain(l: &str, r: &str) -> String {
    format!("{l}  {r}")
}

/// Two columns of cards with the core row in the middle; as many detail rows per card as the
/// height allows (0 to 4), connectors between the card rows when there is room.
pub fn topology(s: &DashState, w: i64, h: i64, now: i64) -> Lines {
    let inner = w - 2;
    let all = cards(s, now);
    // (detail rows, connector rows) by height: 12 + 4 d + 4 c
    let (d, conn) = [(4, 1), (3, 1), (2, 1), (1, 1), (0, 1), (0, 0)]
        .into_iter()
        .find(|(d, c)| 12 + 4 * d + 4 * c <= h)
        .unwrap_or((0, 0));
    let (lw, rw) = ((inner - 1) / 2, inner - 1 - (inner - 1) / 2);
    let pair = |a: &Card, b: &Card| -> Lines {
        let (x, y) = (card_box(a, lw, d as usize), card_box(b, rw, d as usize));
        x.into_iter()
            .zip(y)
            .map(|(l, r)| format!("{l} {r}"))
            .collect()
    };
    let connector = |lines: &mut Lines| {
        if conn == 1 {
            let half = u(lw / 2);
            lines.push(format!(
                "{}{}{}{}",
                " ".repeat(half),
                paint(col_subtle(), "│"),
                " ".repeat(u(lw + 1 + rw / 2 - lw / 2 - 1)),
                paint(col_subtle(), "│")
            ));
        }
    };
    let mut lines: Lines = vec![];
    lines.extend(pair(&all[0], &all[1]));
    connector(&mut lines);
    lines.extend(pair(&all[2], &all[3]));
    connector(&mut lines);
    lines.extend(card_box(&all[4], inner, 1));
    connector(&mut lines);
    lines.extend(pair(&all[5], &all[6]));
    connector(&mut lines);
    lines.extend(pair(&all[7], &all[8]));
    let active = session_phase(s.session, now).running.len();
    let right = if active > 0 {
        paint(col_ok(), &format!("{active} active"))
    } else {
        paint(col_muted(), "idle")
    };
    section("Topology", &right, lines, w, h, false, VAlign::Top)
}

// ---------------------------------------------------------------- sub-agents

pub fn subagents(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    now: i64,
    rects: &mut Rects,
) -> Lines {
    let inner = w - 2;
    let rows = u(h - 1);
    let subs: &[Agent] = s.agent.map_or(&[], |a| &a.subagents);
    if subs.is_empty() {
        let msg = paint(col_subtle(), "No sub-agents yet · /agents");
        return section("Sub-agents", "", vec![msg], w, h, false, VAlign::Top);
    }
    let active = subs.iter().filter(|a| a.status == "running").count();
    let viewed = s
        .chat_agent
        .filter(|i| *i < subs.len())
        .unwrap_or(subs.len() - 1);
    let g = glyphs();
    // at most three list rows, the viewed one always inside the window
    let list_n = subs.len().min(3).min(rows.saturating_sub(2));
    let first = viewed
        .saturating_sub(list_n.saturating_sub(1))
        .min(subs.len() - list_n);
    let mut lines: Lines = subs[first..first + list_n]
        .iter()
        .enumerate()
        .map(|(k, a)| {
            let on = first + k == viewed;
            let run = a.status == "running";
            let icon = if run {
                paint(col_ok(), spin(frame + k))
            } else {
                paint(col_subtle(), g.idle)
            };
            let el = if run && a.turn_start > 0 {
                fmt_dur(Some((now - a.turn_start).max(0) as f64))
            } else {
                String::new()
            };
            let name = a.display_name();
            let task = short(
                &a.description,
                u((inner - width(&name) as i64 - width(&el) as i64 - 6).max(0)),
            );
            lr(
                &format!(
                    "{}{} {}  {}",
                    if on {
                        paint(t().pearl, g.focus)
                    } else {
                        " ".to_string()
                    },
                    icon,
                    paint(if on { col_white() } else { col_text() }, &name),
                    paint(col_muted(), &task)
                ),
                &paint(col_subtle(), &el),
                inner,
            )
        })
        .collect();
    let body = rows.saturating_sub(lines.len() + 1);
    if body > 0 {
        lines.push(paint(col_subtle(), &g.dash.repeat(u(inner))));
        let sub = &subs[viewed];
        let (t_lines, max_scroll) = agent::transcript_rows(
            sub,
            u(inner),
            body,
            frame,
            now,
            s.view.opts.expanded,
            s.view.dock_scroll,
        );
        rects.dock_max_scroll = max_scroll;
        rects.dock_scroll = s.view.dock_scroll.min(max_scroll);
        lines.extend(t_lines);
    }
    let right = if active > 0 {
        paint(col_ok(), &format!("{active} active"))
    } else {
        paint(col_muted(), &format!("{} idle", subs.len()))
    };
    section("Sub-agents", &right, lines, w, h, active > 0, VAlign::Top)
}

/// Rows the content would like (label row included), by panel: the planner's `Demand`.
pub fn demand(s: &DashState) -> crate::layout::Demand {
    use crate::layout::PanelId as P;
    let c = counters(s.session);
    let n_ws = s.workspaces.map_or(0, |l| l.len() as i64);
    let sessions = crate::dash::running_count(s) as i64;
    let tools = s.session.tool_counts.len() as i64;
    let mcp = mcp_rows_of(s).1.len() as i64;
    let _ = c;
    crate::layout::Demand::default()
        .with(P::Workspace, 2 + n_ws.max(1))
        .with(P::Running, 2 + 2 * sessions.max(1))
        .with(
            P::ToolsSkills,
            3 + tools.max(1)
                + i64::from(!s.session.skills.is_empty())
                + if mcp > 0 { 1 + mcp } else { 0 },
        )
        .with(
            P::RecentActivity,
            2 + s.session.activity.len().max(1) as i64,
        )
        .with(P::AgentStatus, if open_todos(s) > 2 { 6 } else { 5 })
}

/// The Tabs-mode compact status line: the phase and what the agent is working on.
pub fn status_line(s: &DashState, w: i64, frame: usize, now: i64) -> String {
    let idx = phase_index(s, now);
    let ph = session_phase(s.session, now);
    let (word, c) = if idx == 0 {
        ("ready", col_muted())
    } else {
        (PHASES[idx], phase_label(&ph.phase).1)
    };
    let head = if idx == 0 {
        paint(c, word)
    } else {
        format!("{} {}", paint(c, spin(frame)), paint(c, word))
    };
    let working = ph
        .running
        .last()
        .map(|t| format!("{} {}", t.name, t.summary))
        .or_else(|| {
            s.session
                .todos
                .iter()
                .find(|t| t.status == "in_progress")
                .map(|t| {
                    if t.active_form.is_empty() {
                        t.content.clone()
                    } else {
                        t.active_form.clone()
                    }
                })
        });
    let tail = working.map_or(String::new(), |t| format!("  {}", paint(col_text(), &t)));
    fit(&format!(" {head}{tail}"), u(w))
}

pub fn model_chip(s: &DashState) -> Option<String> {
    model_of(s).or(Some(s.session.model.clone()).filter(|m| !m.is_empty()))
}
