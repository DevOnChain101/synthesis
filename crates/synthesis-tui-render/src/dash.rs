//! Dashboard frame (port of `render.js`): sidebar, status / tokens / tasks / usage panels, the
//! topology column, the agent pane and the three layouts. Pure: `render_frame` takes the state, the
//! size, the animation `frame` counter and `now` (ms) and returns the painted rows.

use crate::agent::{self, Agent, AgentOpts, EditorView, todo_window};
use crate::dialogs::mode_name;
use crate::flow::{FlowOpts, natural_flow_height, node_counts, render_flow};
use crate::folders::{folder_with_parent, shorten_left};
use crate::model::{Phase, Session, session_phase};
use crate::modelname::{model_label, model_parts};
use crate::theme::*;
use crate::util::{
    basename, clamp_int, fmt_dur, fmt_num, pad_end, pad_start, short, to_fixed, u16len,
};
use synthesis_tui_base::text::{strip_ansi, width};

type Lines = Vec<String>;

// ---------------------------------------------------------------- state

#[derive(Clone, Debug, Default)]
pub struct Live {
    pub name: String,
    pub cwd: String,
    pub status: String,
}

#[derive(Clone, Debug, Default)]
pub struct SessionRow {
    pub id: String,
    pub project: String,
    pub cwd: String,
    pub mtime: i64,
    pub live: Option<Live>,
    pub phase: String,
    pub tokens: f64,
    pub ctx_pct: Option<f64>,
    pub waiting: bool,
    pub mode: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryDay {
    /// Instant (ms) inside the day; the calendar day is taken in the state's UTC offset.
    pub date_ms: i64,
    pub tokens: f64,
    pub output: f64,
    pub messages: f64,
    pub tools: f64,
    pub agents: f64,
    pub sessions: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Tap {
    pub context_pct: Option<f64>,
    pub context_size: Option<f64>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub lines_added: Option<f64>,
    pub lines_removed: Option<f64>,
}
#[derive(Clone, Debug, Default)]
pub struct Quota {
    pub used: f64,
    pub resets_at: Option<i64>,
}
#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub tap: Option<Tap>,
    pub five_hour: Option<Quota>,
    pub seven_day: Option<Quota>,
}

#[derive(Clone, Debug, Default)]
pub struct McpServer {
    pub name: String,
    pub transport: String,
    pub url: String,
    pub enabled: bool,
    pub disabled_reason: String,
}
/// Live MCP client status (connecting / connected / failed / closed).
#[derive(Clone, Debug, Default)]
pub struct McpLive {
    pub name: String,
    pub state: String,
    pub tool_count: u32,
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub key: String,
    pub name: String,
    pub cwd: String,
    pub count: u32,
    pub waiting: bool,
    pub busy: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Focus {
    #[default]
    Agent,
    Dashboard,
    Workspace,
}

#[derive(Clone, Debug, Default)]
pub struct DashView {
    pub opts: AgentOpts,
    pub dock_scroll: usize,
    pub activity_scroll: usize,
}

pub struct DashState<'a> {
    pub session: &'a Session,
    pub sessions: &'a [SessionRow],
    pub index: usize,
    pub usage: &'a Usage,
    pub follow: bool,
    pub history: &'a [HistoryDay],
    pub usage_metric: &'a str,
    pub mcp_servers: &'a [McpServer],
    pub mcp_live: Option<&'a [McpLive]>,
    pub workspaces: Option<&'a [Workspace]>,
    pub active_workspace: &'a str,
    pub agent: Option<&'a Agent>,
    /// Index into `agent.subagents` of the conversation the chat pane shows (`None`: the main agent).
    pub chat_agent: Option<usize>,
    pub editor: Option<EditorView<'a>>,
    pub focus: Focus,
    pub view: &'a DashView,
    pub agent_session_id: Option<&'a str>,
    /// Local time zone offset east of UTC, minutes (the renderer never reads the environment).
    pub tz_offset_min: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Rects {
    pub chat_rect: Option<Rect>,
    pub sidebar_rect: Option<Rect>,
    pub dock_rect: Option<Rect>,
    pub activity_rect: Option<Rect>,
    pub workspace_rect: Option<Rect>,
    pub session_rows: Vec<(i64, String)>,
    pub workspace_rows: Vec<(i64, String)>,
    pub activity_hits: Vec<(usize, usize)>,
    pub activity_max_scroll: usize,
    pub activity_scroll: usize,
    pub dock_max_scroll: usize,
    pub dock_scroll: usize,
    pub max_scroll: usize,
    /// Prompt caret in screen cells `(x, y)` (the app parks the hardware cursor there).
    pub caret: Option<(usize, usize)>,
}

pub struct Frame {
    pub lines: Vec<String>,
    pub cols: usize,
    pub rows: usize,
    pub rects: Rects,
    /// The chat rows behind the frame (mouse selection / copy read them).
    pub chat_body: Option<agent::Body>,
}

// ---------------------------------------------------------------- helpers

const COL_BORDER: Rgb = T_BORDER;
const COL_ACCENT: Rgb = T_CLAUDE;
const COL_OK: Rgb = T_SUCCESS;
const COL_WARN: Rgb = T_WARNING;
const COL_BAD: Rgb = T_ERROR;
const COL_MUTED: Rgb = T_INACTIVE;
const COL_SUBTLE: Rgb = T_SUBTLE;
const COL_TEXT: Rgb = T_BODY;
const COL_WHITE: Rgb = T_TEXT;
const COL_TOOL: Rgb = T_PERMISSION;
const COL_AGENT: Rgb = T_AUTO_ACCEPT;
const COL_SKILL: Rgb = T_PLAN_MODE;
const COL_TRACK: Rgb = T_TRACK;
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn u(x: i64) -> usize {
    x.max(0) as usize
}

pub fn level_color(p: f64) -> Rgb {
    if p >= 85.0 {
        COL_BAD
    } else if p >= 60.0 {
        COL_WARN
    } else {
        COL_OK
    }
}

/// Bar with a shimmer that sweeps across the filled part. `p` of `None` counts as 0.
pub fn bar(p: Option<f64>, w: usize, frame: usize, animate: bool) -> String {
    let pctv = p.filter(|v| !v.is_nan()).unwrap_or(0.0).clamp(0.0, 100.0);
    let filled = js_round(pctv / 100.0 * w as f64) as i64;
    let c = level_color(pctv);
    let shimmer: i64 = if animate && filled > 0 {
        (frame as i64) % (filled + 6)
    } else {
        -99
    };
    let mut out = String::new();
    for i in 0..w as i64 {
        if i < filled {
            if (i - shimmer).abs() <= 1 {
                out.push_str(&paint(lerp(c, COL_WHITE, 0.6), "█"));
            } else {
                out.push_str(&paint(c, "█"));
            }
        } else {
            out.push_str(&paint(COL_TRACK, "░"));
        }
    }
    out
}

fn sparkline(points: &[(i64, u64)], w: usize) -> String {
    if points.len() < 2 {
        return paint(COL_MUTED, &"·".repeat(w));
    }
    let deltas: Vec<u64> = points
        .windows(2)
        .map(|p| p[1].1.saturating_sub(p[0].1))
        .collect();
    let tail = &deltas[deltas.len().saturating_sub(w)..];
    let max = tail.iter().copied().max().unwrap_or(0).max(1) as f64;
    let s: String = tail
        .iter()
        .map(|d| SPARK[7.min((*d as f64 / max * 7.99).floor() as usize)])
        .collect();
    paint(T_CLAUDE, &pad_start(&s, w))
}

fn valign_lines(lines: Lines, rows: usize) -> Lines {
    if lines.len() >= rows {
        return lines.into_iter().take(rows).collect();
    }
    let spare = rows - lines.len();
    let mut out = vec![String::new(); spare / 2];
    out.extend(lines);
    out.extend(vec![String::new(); spare.div_ceil(2)]);
    out
}

fn valign_evenly(lines: Lines, rows: usize) -> Lines {
    if lines.len() >= rows {
        return lines.into_iter().take(rows).collect();
    }
    let mut groups: Vec<Lines> = vec![vec![]];
    for ln in &lines {
        if ln.is_empty() {
            if !groups.last().unwrap().is_empty() {
                groups.push(vec![]);
            }
        } else {
            groups.last_mut().unwrap().push(ln.clone());
        }
    }
    let real: Vec<&Lines> = groups.iter().filter(|g| !g.is_empty()).collect();
    if real.len() <= 1 {
        return valign_lines(lines, rows);
    }
    let content_rows: usize = real.iter().map(|g| g.len()).sum();
    let gap_count = real.len() + 1;
    let spare = rows - content_rows;
    let base = spare / gap_count;
    let extra = spare - base * gap_count;
    let mut out: Lines = vec![];
    for (i, g) in real.iter().enumerate() {
        let gap = base + usize::from(i < extra);
        out.extend(vec![String::new(); gap]);
        out.extend(g.iter().cloned());
    }
    out.extend(vec![String::new(); rows.saturating_sub(out.len())]);
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    #[default]
    Top,
    Center,
    Evenly,
}

fn boxed(title: &str, lines: Lines, w: i64, h: i64, active: bool, valign: VAlign) -> Lines {
    let inner = w - 2;
    let bc = if active { COL_ACCENT } else { COL_BORDER };
    let mut t = if title.is_empty() {
        String::new()
    } else {
        format!(" ◆ {title} ")
    };
    if width(&t) as i64 > w - 3 {
        t = format!("{} ", fit(&t, u(w - 4)));
    }
    let tw = width(&t) as i64;
    let top = format!(
        "{}{}{}",
        paint(bc, "┏━"),
        paint(
            if active { COL_ACCENT } else { COL_MUTED },
            &format!("{t}{RESET}")
        ),
        paint(bc, &format!("{}┓", "━".repeat(u(w - 3 - tw))))
    );
    let mut rows = vec![top];
    let row_count = u(h - 2);
    let content = match valign {
        VAlign::Center => valign_lines(lines, row_count),
        VAlign::Evenly => valign_evenly(lines, row_count),
        VAlign::Top => lines,
    };
    for i in 0..row_count {
        let lch = if i == 0 || i as i64 == h - 3 {
            "┃"
        } else {
            "│"
        };
        let c = content.get(i).map_or("", String::as_str);
        rows.push(format!(
            "{}{}{}",
            paint(bc, lch),
            fit(&format!(" {c}"), u(inner)),
            paint(bc, lch)
        ));
    }
    rows.push(paint(bc, &format!("┗{}┛", "━".repeat(u(inner)))));
    rows
}

fn hstack(cols: Vec<Lines>) -> Lines {
    let h = cols.iter().map(Vec::len).max().unwrap_or(0);
    (0..h)
        .map(|i| {
            cols.iter()
                .map(|c| c.get(i).map_or("", String::as_str))
                .collect::<String>()
        })
        .collect()
}

fn fmt_time(ts: i64, tz: i32) -> String {
    if ts == 0 {
        return "--:--:--".into();
    }
    let s = (ts + i64::from(tz) * 60_000)
        .div_euclid(1000)
        .rem_euclid(86400);
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

struct Civil {
    year: i64,
    month: usize,
    day: i64,
    dow: usize,
}

fn civil_from_days(z: i64) -> (i64, usize, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, (m - 1) as usize, d)
}
fn days_from_civil(y: i64, m0: usize, d: i64) -> i64 {
    let m = m0 as i64 + 1;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
fn civil(ms: i64, tz: i32) -> Civil {
    let days = (ms + i64::from(tz) * 60_000).div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        dow: (days + 4).rem_euclid(7) as usize,
    }
}
fn days_in_month(y: i64, m0: usize) -> i64 {
    days_from_civil(if m0 == 11 { y + 1 } else { y }, (m0 + 1) % 12, 1) - days_from_civil(y, m0, 1)
}

// ---------------------------------------------------------------- panels

fn open_todos(s: &DashState) -> usize {
    s.session
        .todos
        .iter()
        .filter(|t| t.status != "completed")
        .count()
}

fn context_size(session: &Session, tap: Option<&Tap>, agent_window: Option<f64>) -> f64 {
    if let Some(c) = tap.and_then(|t| t.context_size).filter(|c| *c != 0.0) {
        return c;
    }
    if let Some(c) = agent_window.filter(|c| *c != 0.0) {
        return c;
    }
    if session.model.to_lowercase().contains("1m") {
        1_000_000.0
    } else {
        200_000.0
    }
}

fn ctx_pct_of(s: &DashState) -> f64 {
    let tap = s.usage.tap.as_ref();
    if s.session.context_display_reset {
        return 0.0;
    }
    let from_tap = if s.session.context_estimated {
        None
    } else {
        tap.and_then(|t| t.context_pct)
    };
    from_tap
        .unwrap_or(
            s.session.context_used as f64
                / context_size(s.session, tap, s.agent.and_then(|a| a.context_window))
                * 100.0,
        )
        .min(100.0)
}

fn thinking_wave(frame: usize, w: usize) -> String {
    let mut s = String::new();
    for i in 0..w {
        let v = ((i + frame) as f64 / 2.2).sin() * 0.5 + 0.5;
        s.push_str(&paint(
            lerp(COL_SUBTLE, COL_ACCENT, v),
            &SPARK[(v * 7.99).floor() as usize].to_string(),
        ));
    }
    s
}

fn phase_label(p: &Phase) -> (&'static str, Rgb) {
    match p {
        Phase::Tool => ("RUNNING TOOL", COL_TOOL),
        Phase::Agents => ("WAITING ON AGENTS", COL_AGENT),
        Phase::Thinking => ("THINKING", COL_ACCENT),
        Phase::Idle => ("IDLE", COL_MUTED),
    }
}

fn status_panel(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> Lines {
    let session = s.session;
    let ph = session_phase(session, now);
    let (text, lcol) = phase_label(&ph.phase);
    let idle = ph.phase == Phase::Idle;
    let spin_s = if idle {
        paint(COL_MUTED, "●")
    } else {
        paint(lcol, spin(frame))
    };
    let mut lines = vec![format!(
        "{spin_s} {}  {}",
        paint(lcol, &format!("{text}{RESET}")),
        paint(
            COL_MUTED,
            &format!(
                "last activity {} ago",
                fmt_dur(Some(
                    (now - session.last_activity.filter(|a| *a != 0).unwrap_or(now)) as f64
                ))
            )
        )
    )];
    if let Some(t) = ph.running.last() {
        let more = if ph.running.len() > 1 {
            paint(COL_MUTED, &format!(" +{}", ph.running.len() - 1))
        } else {
            String::new()
        };
        lines.push(format!(
            "  {} {} {} {}{more}",
            paint(COL_TOOL, spin(frame + 3)),
            paint(COL_TOOL, &t.name),
            paint(COL_TEXT, &t.summary),
            paint(
                COL_MUTED,
                &fmt_dur(Some((now - t.start.unwrap_or(0)) as f64))
            )
        ));
    } else if ph.phase == Phase::Thinking {
        lines.push(format!("  {}", thinking_wave(frame, u((w - 6).min(32)))));
    } else {
        lines.push(paint(COL_MUTED, "  waiting for the next prompt…"));
    }
    let tap = s.usage.tap.as_ref();
    let ctx_max = context_size(session, tap, s.agent.and_then(|a| a.context_window));
    let ctx_pct = ctx_pct_of(s);
    let bw = u((w - 32).max(8));
    let nums = paint(
        COL_MUTED,
        &format!(
            " {}/{}",
            fmt_num(Some(session.context_used as f64)),
            fmt_num(Some(ctx_max))
        ),
    );
    lines.push(format!(
        "{} {} {}{nums}",
        paint(COL_MUTED, "Context "),
        bar(Some(ctx_pct), bw, frame, !idle),
        paint(
            level_color(ctx_pct),
            &format!("{}%", pad_start(&to_fixed(ctx_pct, 0), 3))
        )
    ));
    let quota = |name: &str, win: &Quota| {
        format!(
            "{} {} {}{}",
            paint(COL_MUTED, &pad_end(name, 8)),
            bar(Some(win.used), bw, frame, false),
            paint(
                level_color(win.used),
                &format!("{}%", pad_start(&to_fixed(win.used, 0), 3))
            ),
            paint(
                COL_MUTED,
                &win.resets_at.map_or(String::new(), |r| format!(
                    " ↻ {}",
                    fmt_dur(Some((r - now) as f64))
                ))
            )
        )
    };
    if let Some(q) = &s.usage.five_hour {
        lines.push(quota("5h quota", q));
    }
    if let Some(q) = &s.usage.seven_day {
        lines.push(quota("7d quota", q));
    }
    boxed("Status", lines, w, h, !idle, VAlign::Evenly)
}

fn token_panel(s: &DashState, w: i64, h: i64) -> Lines {
    let session = s.session;
    let t = &session.tokens;
    let agent_total: u64 = session.agents.values().map(|a| a.tokens.total()).sum();
    let total = t.total();
    let denom = t.cache_read + t.input + t.cache_create;
    let hit = if denom > 0 {
        t.cache_read as f64 / denom as f64 * 100.0
    } else {
        0.0
    };
    let tap = s.usage.tap.as_ref();
    let half = u(16.max((w - 4) / 2));
    let cell = |k: &str, v: &str, c: Rgb, bold: bool| {
        fit(
            &format!(
                "{}{}",
                paint(COL_MUTED, &pad_end(k, 7)),
                paint(c, &format!("{v}{}", if bold { RESET } else { "" }))
            ),
            half,
        )
    };
    let n = |v: u64| fmt_num(Some(v as f64));
    let lines = vec![
        cell("total", &n(total), COL_ACCENT, true)
            + &cell("agents", &n(agent_total), COL_AGENT, false),
        cell("input", &n(t.input), COL_TEXT, false)
            + &cell("output", &n(t.output), COL_ACCENT, false),
        cell("cache", &n(t.cache_read), COL_OK, false)
            + &cell("hit", &format!("{}%", to_fixed(hit, 1)), COL_OK, false),
        cell(
            "cost",
            &tap.and_then(|t| t.cost_usd)
                .map_or("-".into(), |c| format!("${}", to_fixed(c, 2))),
            COL_WARN,
            false,
        ) + &cell(
            "lines",
            &tap.and_then(|t| {
                t.lines_added
                    .map(|a| format!("+{} -{}", a, t.lines_removed.unwrap_or(0.0)))
            })
            .unwrap_or("-".into()),
            COL_TEXT,
            false,
        ),
        sparkline(&session.timeline, u(w - 4)),
    ];
    boxed("Tokens", lines, w, h, false, VAlign::Evenly)
}

pub fn tasks_panel(s: &DashState, w: i64, h: i64, frame: usize, valign: VAlign) -> Lines {
    let todos = &s.session.todos;
    let mut lines: Lines = vec![];
    if todos.is_empty() {
        lines.push(paint(COL_MUTED, "no tasks tracked"));
    }
    let done = todos.iter().filter(|t| t.status == "completed").count();
    if !todos.is_empty() {
        let p = done as f64 / todos.len() as f64 * 100.0;
        lines.push(format!(
            "{} {}",
            bar(Some(p), u((w - 16).max(8)), frame, false),
            paint(COL_TEXT, &format!("{done}/{}", todos.len()))
        ));
    }
    let room = u(h - 2 - lines.len() as i64);
    let cap = if todos.len() > room {
        room.saturating_sub(1)
    } else {
        todos.len()
    };
    let start = todo_window(todos, cap);
    let shown = &todos[start..(start + cap).min(todos.len())];
    let num_w = todos.len().to_string().len();
    for (i, t) in shown.iter().enumerate() {
        let num = paint(COL_SUBTLE, &pad_start(&(start + i + 1).to_string(), num_w));
        match t.status.as_str() {
            "in_progress" => lines.push(format!(
                "{num} {} {}",
                paint(COL_WARN, spin(frame)),
                paint(
                    COL_WARN,
                    if t.active_form.is_empty() {
                        &t.content
                    } else {
                        &t.active_form
                    }
                )
            )),
            "completed" => lines.push(format!(
                "{num} {} {}",
                paint(COL_OK, "✓"),
                paint(COL_MUTED, &t.content)
            )),
            _ => lines.push(format!(
                "{num} {} {}",
                paint(COL_MUTED, "○"),
                paint(COL_TEXT, &t.content)
            )),
        }
    }
    if shown.len() < todos.len() && room > 0 {
        lines.push(paint(
            COL_SUBTLE,
            &format!("… +{} more", todos.len() - shown.len()),
        ));
    }
    let open = todos.len() - done;
    let title = if todos.is_empty() {
        "Tasks".to_string()
    } else {
        format!(
            "Tasks {}",
            paint(
                COL_MUTED,
                &format!(
                    "· {}",
                    if open > 0 {
                        format!("{open} open")
                    } else {
                        "all done".into()
                    }
                )
            )
        )
    };
    boxed(
        &title,
        lines,
        w,
        h,
        todos.iter().any(|t| t.status == "in_progress"),
        valign,
    )
}

fn tools_panel(s: &DashState, w: i64, h: i64) -> Lines {
    let mut entries: Vec<&(String, u32)> = s.session.tool_counts.iter().collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    let max = entries.first().map_or(1, |e| e.1) as f64;
    let mut lines: Lines = vec![];
    let name_w = clamp_int((w as f64 * 0.35).floor() as i64, 6, 12);
    let bw = (w - name_w - 9).max(2);
    for (name, n) in &entries {
        if lines.len() as i64 >= h - 4 {
            break;
        }
        let len = 1.max(js_round(*n as f64 / max * bw as f64) as usize);
        lines.push(format!(
            "{} {} {}",
            paint(COL_TOOL, &pad_end(&short(name, u(name_w)), u(name_w))),
            paint(COL_TOOL, &"▇".repeat(len)),
            paint(COL_MUTED, &n.to_string())
        ));
    }
    if entries.is_empty() {
        lines.push(paint(COL_MUTED, "no tool calls yet"));
    }
    if !s.session.skills.is_empty() {
        lines.push(paint(COL_MUTED, "skills:"));
        lines.push(
            s.session
                .skills
                .iter()
                .map(|(k, n)| paint(COL_SKILL, &format!("{k}×{n}")))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    boxed("Tools & Skills", lines, w, h, false, VAlign::Evenly)
}

fn feed_icon(k: &str) -> (&'static str, Rgb) {
    match k {
        "prompt" => ("❯", COL_WHITE),
        "tool" => ("▣", COL_TOOL),
        "agent" => ("◆", COL_AGENT),
        "skill" => ("★", COL_SKILL),
        "error" => ("✗", COL_BAD),
        "hook" => ("↯", COL_WARN),
        "cmd" => ("/", COL_ACCENT),
        _ => ("✎", COL_TEXT),
    }
}

fn feed_panel(s: &DashState, w: i64, h: i64, now: i64, rects: &mut Rects) -> Lines {
    let rows = u(h - 2);
    let activity = &s.session.activity;
    let max_scroll = activity.len().saturating_sub(rows);
    let scroll = s.view.activity_scroll.min(max_scroll);
    let start = activity.len().saturating_sub(rows + scroll);
    let feed = &activity[start..(start + rows).min(activity.len())];
    rects.activity_max_scroll = max_scroll;
    rects.activity_scroll = scroll;
    rects.activity_hits = feed
        .iter()
        .enumerate()
        .map(|(i, _)| (i, start + i))
        .collect();
    let lines = feed
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (icon, c) = feed_icon(f.kind);
            let fresh = i == feed.len() - 1 && now - f.ts.unwrap_or(0) < 2500;
            let text = if fresh {
                paint(COL_WHITE, &f.text)
            } else {
                paint(c, &f.text)
            };
            format!(
                "{} {} {text}",
                paint(COL_MUTED, &fmt_time(f.ts.unwrap_or(0), s.tz_offset_min)),
                paint(c, icon)
            )
        })
        .collect();
    boxed("Activity", lines, w, h, false, VAlign::Top)
}

/// Visible window `[start, end)` of the session list and the "more" counts; the selected row
/// counts as 2 (name + folder).
pub fn sessions_window(count: usize, index: usize, capacity: i64) -> (usize, usize, usize, usize) {
    if count == 0 {
        return (0, 0, 0, 0);
    }
    let (mut start, mut end) = (index, index + 1);
    let mut used = 2i64;
    let mut grow_back = true;
    while used < capacity && (start > 0 || end < count) {
        if grow_back && start > 0 {
            if used + 2 > capacity {
                break;
            }
            start -= 1;
            used += 2;
        } else if end < count {
            if used + 2 > capacity {
                break;
            }
            end += 1;
            used += 2;
        } else if start > 0 {
            if used + 2 > capacity {
                break;
            }
            start -= 1;
            used += 2;
        } else {
            break;
        }
        grow_back = !grow_back;
    }
    (start, end, start, count - end)
}

pub fn workspace_key(cwd: &str) -> String {
    // shortcut: no path.resolve (the renderer has no cwd); paths are expected absolute
    cwd.to_lowercase()
}
fn session_cwd(s: &SessionRow) -> &str {
    if !s.cwd.is_empty() {
        &s.cwd
    } else {
        s.live.as_ref().map_or("", |l| l.cwd.as_str())
    }
}

fn running_sessions<'a>(s: &'a DashState) -> (Vec<&'a SessionRow>, usize) {
    if s.workspaces.is_none() {
        return (s.sessions.iter().collect(), s.index);
    }
    let list: Vec<&SessionRow> = s
        .sessions
        .iter()
        .filter(|x| workspace_key(session_cwd(x)) == s.active_workspace)
        .collect();
    let sel = s.sessions.get(s.index);
    let idx = sel
        .and_then(|sel| list.iter().position(|x| std::ptr::eq(*x, sel)))
        .unwrap_or(0);
    (list, idx)
}

fn workspace_panel(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    wlist: &[Workspace],
) -> (Lines, Vec<Option<String>>) {
    let inner = w - 3;
    let mut lines: Lines = vec![];
    let mut hits: Vec<Option<String>> = vec![];
    let active_at = wlist
        .iter()
        .position(|x| x.key == s.active_workspace)
        .unwrap_or(0);
    let mut capacity = (h - 2).max(1);
    let mut win = sessions_window(wlist.len(), active_at, capacity * 2);
    let ind = i64::from(win.2 > 0) + i64::from(win.3 > 0);
    if ind > 0 {
        capacity = (capacity - ind).max(0);
        win = sessions_window(wlist.len(), active_at, capacity * 2);
    }
    let (start, end, above, below) = win;
    let blink_on = (frame / 4).is_multiple_of(2);
    if above > 0 {
        lines.push(paint(COL_SUBTLE, &format!("   ↑ {above} more")));
        hits.push(None);
    }
    for (i, ws) in wlist.iter().enumerate().take(end).skip(start) {
        let on = i == active_at;
        let icon = if ws.waiting {
            paint(COL_BAD, if blink_on { "●" } else { "○" })
        } else if ws.busy {
            paint(COL_OK, spin(frame + i))
        } else {
            paint(if ws.count > 0 { COL_OK } else { COL_MUTED }, "●")
        };
        let count = if ws.waiting {
            format!("! {}", ws.count)
        } else {
            ws.count.to_string()
        };
        let name_w = (inner - 4 - count.len() as i64).max(4);
        let name = short(&ws.name, u(name_w));
        let title = if ws.waiting {
            paint(if blink_on { COL_BAD } else { COL_MUTED }, &name)
        } else if on {
            paint(COL_ACCENT, &name)
        } else {
            paint(COL_TEXT, &name)
        };
        let left = format!(
            "{}{icon} {title}",
            if on {
                paint(COL_ACCENT, "▌")
            } else {
                " ".into()
            }
        );
        let gap = " ".repeat(u((inner - width(&left) as i64 - count.len() as i64).max(1)));
        lines.push(format!(
            "{left}{gap}{}",
            paint(
                if ws.waiting {
                    if blink_on { COL_BAD } else { COL_MUTED }
                } else {
                    COL_MUTED
                },
                &count
            )
        ));
        hits.push(Some(ws.key.clone()));
    }
    if below > 0 {
        lines.push(paint(COL_SUBTLE, &format!("   ↓ {below} more")));
        hits.push(None);
    }
    let waiting_n = wlist.iter().filter(|x| x.waiting).count();
    let wait_tag = if waiting_n > 0 {
        paint(COL_BAD, &format!(" !{waiting_n}"))
    } else {
        String::new()
    };
    let title = format!(
        "Workspace {}{wait_tag}",
        paint(COL_OK, &wlist.len().to_string())
    );
    (
        boxed(
            &title,
            lines,
            w,
            h,
            s.focus == Focus::Workspace,
            VAlign::Top,
        ),
        hits,
    )
}

fn mode_tag(m: &str) -> Option<(&'static str, Rgb)> {
    Some(match m {
        "acceptEdits" => ("edits", COL_AGENT),
        "plan" => ("plan", COL_SKILL),
        "auto" => ("auto", COL_WARN),
        "dontAsk" => ("dontAsk", COL_TOOL),
        "bypassPermissions" => ("bypass", COL_BAD),
        _ => return None,
    })
}

fn sessions_panel(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    session_hits: &mut Vec<Option<String>>,
) -> Lines {
    let (sessions, index) = running_sessions(s);
    let inner = w - 3;
    let mut lines: Lines = vec![];
    let mut capacity = (h - 2).max(1);
    let mut win = sessions_window(sessions.len(), index, capacity);
    let ind = i64::from(win.2 > 0) + i64::from(win.3 > 0);
    if ind > 0 {
        capacity = (capacity - ind).max(0);
        win = sessions_window(sessions.len(), index, capacity);
    }
    let (start, end, above, below) = win;
    session_hits.clear();
    if above > 0 {
        lines.push(paint(COL_SUBTLE, &format!("   ↑ {above} more")));
        session_hits.push(None);
    }
    for (i, sr) in sessions.iter().enumerate().take(end).skip(start) {
        let sel = i == index;
        let live = sr.live.as_ref();
        let busy = live.is_some() && !sr.phase.is_empty() && sr.phase != "idle";
        let is_agent = s.agent_session_id == Some(sr.id.as_str());
        let waiting = sr.waiting || live.is_some_and(|l| l.status == "waiting");
        let blink_on = (frame / 4).is_multiple_of(2);
        let icon = if waiting {
            paint(COL_BAD, if blink_on { "●" } else { "○" })
        } else if busy {
            paint(COL_OK, spin(frame + i))
        } else if live.is_some() {
            paint(COL_OK, "●")
        } else {
            paint(COL_MUTED, "○")
        };
        let cursor = if sel {
            paint(COL_ACCENT, if s.workspaces.is_some() { "▌" } else { "►" })
        } else {
            " ".into()
        };
        let tag = if is_agent {
            paint(ACCENT_LIGHT, " ⌘")
        } else {
            String::new()
        };
        let ctx_pct = sr.ctx_pct.map(|p| p.min(100.0));
        let ctx = if waiting {
            "!".to_string()
        } else {
            ctx_pct.map_or(String::new(), |p| format!("{}%", to_fixed(p, 0)))
        };
        let mt = sr.mode.as_deref().and_then(mode_tag);
        let mode_s = mt.map_or(String::new(), |(l, _)| format!(" {l}"));
        let name_w =
            (inner - 3 - width(&tag) as i64 - mode_s.len() as i64 - ctx.len() as i64 - 1).max(4);
        let raw_name = match live {
            Some(l) if !l.name.is_empty() && l.name != "agent" => l.name.clone(),
            _ => {
                let b = basename(&sr.cwd);
                if b.is_empty() { sr.project.clone() } else { b }
            }
        };
        let name = short(&raw_name, u(name_w));
        let title = if waiting {
            paint(if blink_on { COL_BAD } else { COL_MUTED }, &name)
        } else if sel {
            paint(COL_ACCENT, &name)
        } else {
            paint(if live.is_some() { COL_TEXT } else { COL_MUTED }, &name)
        };
        let left = format!(
            "{cursor}{icon} {title}{}{tag}",
            mt.map_or(String::new(), |(_, c)| paint(c, &mode_s))
        );
        let ctx_col = if waiting {
            if blink_on { COL_BAD } else { COL_MUTED }
        } else {
            ctx_pct.map_or(COL_MUTED, level_color)
        };
        lines.push(format!(
            "{left}{}{}",
            " ".repeat(u((inner - width(&left) as i64 - ctx.len() as i64).max(1))),
            paint(ctx_col, &ctx)
        ));
        let dir = session_cwd(sr);
        let folder = if dir.is_empty() {
            sr.project.clone()
        } else {
            folder_with_parent(dir, u((inner - 3).max(8)))
        };
        lines.push(paint(
            if sel { COL_TEXT } else { COL_SUBTLE },
            &short(&format!("   {folder}"), u(inner)),
        ));
        session_hits.push(Some(sr.id.clone()));
        session_hits.push(Some(sr.id.clone()));
    }
    if below > 0 {
        lines.push(paint(COL_SUBTLE, &format!("   ↓ {below} more")));
        session_hits.push(None);
    }
    if sessions.is_empty() {
        lines.push(paint(COL_MUTED, "no running sessions"));
        session_hits.push(None);
    }
    let waiting_n = sessions
        .iter()
        .filter(|x| x.waiting || x.live.as_ref().is_some_and(|l| l.status == "waiting"))
        .count();
    let wait_tag = if waiting_n > 0 {
        paint(COL_BAD, &format!(" !{waiting_n}"))
    } else {
        String::new()
    };
    let ws = s
        .workspaces
        .and_then(|l| l.iter().find(|x| x.key == s.active_workspace));
    let where_ = ws.map_or(String::new(), |w0| {
        paint(
            COL_MUTED,
            &format!("· {} ", short(&w0.name, u((w - 20).max(4)))),
        )
    });
    boxed(
        &format!(
            "Running {where_}{}{wait_tag}",
            paint(COL_OK, &sessions.len().to_string())
        ),
        lines,
        w,
        h,
        s.focus == Focus::Dashboard,
        VAlign::Top,
    )
}

fn model_of(s: &DashState) -> Option<String> {
    s.agent
        .and_then(|a| model_label(&a.model, &a.active_model, a.config.as_ref()))
}

fn session_row(k: &str, v: &str, c: Rgb, inner: i64) -> String {
    format!(
        "{}{}",
        paint(COL_MUTED, &pad_end(k, 7)),
        paint(c, &short(v, u((inner - 7).max(4))))
    )
}

fn mode_line(a: &Agent) -> (String, Rgb) {
    let hint = agent::turn_mode_hint(a);
    (
        format!(
            "{}{}",
            mode_name(Some(&a.mode)),
            hint.map_or(String::new(), |h| format!(" (turn: {h})"))
        ),
        if !a.mode.is_empty() && a.mode != "default" {
            COL_WARN
        } else {
            COL_TEXT
        },
    )
}

fn goal_row(g: &agent::Goal, inner: i64) -> String {
    let working = g.status == "working";
    session_row(
        "goal",
        &format!(
            "{}{}",
            if working {
                format!("{}/{} · ", g.iterations, g.max_iterations)
            } else {
                format!("{} · ", g.status)
            },
            g.condition
        ),
        if working { COL_OK } else { COL_MUTED },
        inner,
    )
}

fn session_info_panel(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> Lines {
    let session = s.session;
    let inner = w - 3;
    let mut lines = vec![];
    let model = model_of(s)
        .or(Some(session.model.clone()).filter(|m| !m.is_empty()))
        .unwrap_or_else(|| waiting("loading model…", frame));
    lines.push(session_row("model", &model, COL_ACCENT, inner));
    if let Some(a) = s.agent {
        let (m, c) = mode_line(a);
        lines.push(session_row("mode", &m, c, inner));
        lines.push(session_row(
            "effort",
            a.effort_label
                .as_deref()
                .filter(|e| !e.is_empty())
                .unwrap_or(if a.effort.is_empty() {
                    "auto"
                } else {
                    &a.effort
                }),
            COL_TEXT,
            inner,
        ));
        if let Some(g) = &a.goal {
            lines.push(goal_row(g, inner));
        }
        if let Some(cw) = a.context_window.filter(|c| *c != 0.0) {
            lines.push(session_row(
                "window",
                &format!("{} tokens", fmt_num(Some(cw))),
                COL_TEXT,
                inner,
            ));
        }
    }
    if !session.git_branch.is_empty() {
        lines.push(session_row("branch", &session.git_branch, COL_SKILL, inner));
    }
    if !session.cwd.is_empty() {
        lines.push(session_row("cwd", &basename(&session.cwd), COL_TEXT, inner));
    }
    if let Some(st) = session.started_at.filter(|t| *t != 0) {
        lines.push(session_row(
            "uptime",
            &fmt_dur(Some((now - st) as f64)),
            COL_TEXT,
            inner,
        ));
    }
    let tools: u32 = session.tool_counts.iter().map(|t| t.1).sum();
    lines.push(session_row(
        "turns",
        &format!("{} prompts · {tools} tools", session.prompts),
        COL_TEXT,
        inner,
    ));
    if session.errors > 0 {
        lines.push(session_row(
            "errors",
            &session.errors.to_string(),
            COL_BAD,
            inner,
        ));
    }
    lines.truncate(u(h - 2));
    boxed("Session", lines, w, h, false, VAlign::Evenly)
}

fn session_info_essential(s: &DashState, w: i64, h: i64, frame: usize) -> Lines {
    let inner = w - 3;
    let session = s.session;
    let model = model_of(s)
        .or(Some(session.model.clone()).filter(|m| !m.is_empty()))
        .unwrap_or_else(|| waiting("loading model…", frame));
    let mut lines = vec![session_row("model", &model, COL_ACCENT, inner)];
    if let Some(a) = s.agent {
        let (m, c) = mode_line(a);
        lines.push(session_row("mode", &m, c, inner));
        lines.push(session_row(
            "effort",
            a.effort_label
                .as_deref()
                .filter(|e| !e.is_empty())
                .unwrap_or(if a.effort.is_empty() {
                    "auto"
                } else {
                    &a.effort
                }),
            COL_TEXT,
            inner,
        ));
        if let Some(g) = &a.goal {
            lines.push(goal_row(g, inner));
        }
    }
    boxed("Session", lines, w, h, false, VAlign::Evenly)
}

struct McpRow {
    name: String,
    transport: String,
    url: String,
    state: String,
    tool_count: u32,
    error: Option<String>,
}

fn mcp_rows_of(s: &DashState) -> (bool, Vec<McpRow>) {
    if let Some(live) = s.mcp_live {
        return (
            true,
            live.iter()
                .map(|l| McpRow {
                    name: l.name.clone(),
                    transport: String::new(),
                    url: String::new(),
                    state: l.state.clone(),
                    tool_count: l.tool_count,
                    error: Some(l.error.clone()).filter(|e| !e.is_empty()),
                })
                .collect(),
        );
    }
    (
        false,
        s.mcp_servers
            .iter()
            .map(|m| McpRow {
                name: m.name.clone(),
                transport: m.transport.clone(),
                url: m.url.clone(),
                state: if m.enabled {
                    "configured".into()
                } else {
                    "disabled".into()
                },
                tool_count: 0,
                error: if m.enabled {
                    None
                } else {
                    Some(if m.disabled_reason.is_empty() {
                        "disabled".into()
                    } else {
                        m.disabled_reason.clone()
                    })
                },
            })
            .collect(),
    )
}

fn url_host(url: &str) -> String {
    // new URL(url).host, or short(url, 20) when it does not parse
    match url.split_once("://") {
        Some((scheme, rest)) if !scheme.is_empty() && !rest.is_empty() => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = host.rsplit('@').next().unwrap_or(host);
            if host.is_empty() {
                short(url, 20)
            } else {
                host.to_lowercase()
            }
        }
        _ => short(url, 20),
    }
}

fn mcp_panel(s: &DashState, w: i64, h: i64, frame: usize) -> Lines {
    let (live, rows) = mcp_rows_of(s);
    let enabled: Vec<&McpRow> = rows.iter().filter(|r| r.state != "disabled").collect();
    let inner = w - 3;
    let mut lines: Lines = vec![];
    let room = u(h - 2);
    for r in &enabled {
        if lines.len() >= room {
            break;
        }
        let (dot, right, c);
        if live {
            dot = match r.state.as_str() {
                "connecting" => paint(COL_WARN, spin(frame + lines.len())),
                "failed" => paint(COL_BAD, "●"),
                "connected" => paint(COL_OK, "●"),
                _ => paint(COL_MUTED, "○"),
            };
            right = match r.state.as_str() {
                "connected" => format!("{} tools", r.tool_count),
                "failed" => short(
                    r.error.as_deref().unwrap_or("failed"),
                    6.max((inner as f64 * 0.45).floor() as usize),
                ),
                o => o.to_string(),
            };
            c = if r.state == "failed" {
                COL_BAD
            } else if r.state == "connected" {
                COL_WHITE
            } else {
                COL_TEXT
            };
        } else {
            let comp = s.session.components.get(&format!("mcp:{}", r.name));
            let busy = comp.is_some_and(|c| c.open > 0);
            let errored = comp.is_some_and(|c| c.error);
            let used = comp.is_some_and(|c| c.count > 0);
            dot = if busy {
                paint(COL_OK, spin(frame + lines.len()))
            } else if errored {
                paint(COL_BAD, "●")
            } else {
                paint(COL_OK, "●")
            };
            let mut rt = if r.transport == "stdio" {
                "stdio".to_string()
            } else if !r.url.is_empty() {
                url_host(&r.url)
            } else {
                r.transport.clone()
            };
            if used {
                rt = format!("{}× {rt}", comp.unwrap().count).trim().to_string();
            }
            right = rt;
            c = if errored {
                COL_BAD
            } else if used {
                COL_WHITE
            } else {
                COL_TEXT
            };
        }
        let name_w = (inner - 2 - u16len(&right) as i64 - 1).max(4);
        let left = format!("{dot} {}", paint(c, &short(&r.name, u(name_w))));
        lines.push(format!(
            "{left}{}{}",
            " ".repeat(u(
                (inner - width(&left) as i64 - u16len(&right) as i64).max(1)
            )),
            paint(COL_MUTED, &right)
        ));
    }
    let disabled: Vec<&McpRow> = rows.iter().filter(|r| r.state == "disabled").collect();
    let mut shown_disabled = 0;
    if !live {
        for r in &disabled {
            if lines.len() >= room {
                break;
            }
            let reason = r.error.clone().unwrap_or_else(|| "disabled".into());
            let right = short(&reason, 4.max((inner as f64 * 0.4).floor() as usize));
            let name_w = (inner - 2 - u16len(&right) as i64 - 1).max(4);
            let left = format!(
                "{} {}",
                paint(COL_MUTED, "○"),
                paint(COL_MUTED, &short(&r.name, u(name_w)))
            );
            lines.push(format!(
                "{left}{}{}",
                " ".repeat(u(
                    (inner - width(&left) as i64 - u16len(&right) as i64).max(1)
                )),
                paint(COL_SUBTLE, &right)
            ));
            shown_disabled += 1;
        }
    }
    if !live && lines.len() < room {
        lines.push(paint(
            COL_SUBTLE,
            "configured · synthesis has no MCP client yet",
        ));
    }
    if shown_disabled < disabled.len() && lines.len() < room {
        lines.push(paint(
            COL_SUBTLE,
            &format!("+{} disabled", disabled.len() - shown_disabled),
        ));
    }
    boxed(
        &format!("MCP {} enabled", paint(COL_OK, &enabled.len().to_string())),
        lines,
        w,
        h,
        false,
        VAlign::Evenly,
    )
}

fn mcp_summary_line(s: &DashState, w: i64) -> Lines {
    let (_, rows) = mcp_rows_of(s);
    let enabled: Vec<&McpRow> = rows.iter().filter(|r| r.state != "disabled").collect();
    let names = enabled
        .iter()
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let line = format!(
        "{} {} {}",
        paint(COL_OK, &enabled.len().to_string()),
        paint(COL_MUTED, "enabled:"),
        paint(COL_TEXT, &names)
    );
    vec![fit(&format!("{} {line}", paint(T_CLAUDE, "MCP")), u(w))]
}

#[derive(Default)]
struct Hits {
    ws: Vec<Option<String>>,
    ws_h: i64,
    session: Vec<Option<String>>,
}

fn side_column(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    now: i64,
    max_sessions_h: i64,
    rects: &mut Rects,
    hits: &mut Hits,
) -> Lines {
    let n = running_sessions(s).0.len() as i64;
    let ws_h = match s.workspaces {
        Some(l) if !l.is_empty() => (l.len() as i64 + 2).min(6),
        _ => 0,
    };
    let cap = max_sessions_h - ws_h;
    let sessions_h = clamp_int(2 * n + 3, 5, cap.max(5));
    let mut ws_rows: Lines = vec![];
    if ws_h > 0 {
        let (rows, wh) = workspace_panel(s, w, ws_h, frame, s.workspaces.unwrap());
        ws_rows = rows;
        hits.ws = wh;
        hits.ws_h = ws_h;
    }
    if h - ws_h - sessions_h < 10 {
        let mut out = ws_rows;
        out.extend(sessions_panel(s, w, h - ws_h, frame, &mut hits.session));
        return out;
    }
    let mut out = ws_rows;
    out.extend(sessions_panel(s, w, sessions_h, frame, &mut hits.session));
    let budget = h - ws_h - sessions_h;
    let mcp_rows = mcp_rows_of(s).1;
    let tool_count = s.session.tool_counts.len() as i64;
    let skill_count = s.session.skills.len() as i64;
    let tools_want = 4.max(tool_count + if skill_count > 0 { 2 } else { 0 } + 4);
    let activity_want = if s.agent.is_some() { 4 } else { 0 };
    let info_full_h = 11;
    let info_ess_h = 5;
    let mcp_full_h = if mcp_rows.is_empty() {
        0
    } else {
        clamp_int(mcp_rows.len() as i64 + 3, 4, 8)
    };
    let mut show_activity = s.agent.is_some();
    let mut show_tools = true;
    let mut info_full = true;
    let mut mcp_mode = if mcp_rows.is_empty() { 0 } else { 3 }; // 3 full, 2 summary, 0 hidden
    let total_want = |act: bool, tools: bool, info_full: bool, mcp_mode: i32| {
        (if info_full {
            info_full_h.min(budget)
        } else {
            info_ess_h
        }) + match mcp_mode {
            3 => mcp_full_h,
            2 => 1,
            _ => 0,
        } + if tools { tools_want } else { 0 }
            + if act { activity_want } else { 0 }
    };
    while total_want(show_activity, show_tools, info_full, mcp_mode) > budget {
        if show_activity {
            show_activity = false;
        } else if show_tools {
            show_tools = false;
        } else if info_full {
            info_full = false;
        } else if mcp_mode == 3 {
            mcp_mode = 2;
        } else if mcp_mode == 2 {
            mcp_mode = 0;
        } else {
            break;
        }
    }
    let info_h = if info_full {
        info_full_h.min(budget)
    } else {
        info_ess_h
    };
    out.extend(if info_full {
        session_info_panel(s, w, info_h, frame, now)
    } else {
        session_info_essential(s, w, info_h, frame)
    });
    let mut left = budget - info_h;
    if mcp_mode == 3 && left >= mcp_full_h {
        out.extend(mcp_panel(s, w, mcp_full_h, frame));
        left -= mcp_full_h;
    } else if mcp_mode == 2 && left >= 1 {
        out.extend(mcp_summary_line(s, w));
        left -= 1;
    }
    if show_tools && left >= 6 {
        let tools_h = if s.agent.is_some() && left >= 14 {
            tools_want.min((left as f64 * 0.45).floor() as i64)
        } else {
            left
        };
        out.extend(tools_panel(s, w, tools_h.max(4)));
        left -= tools_h.max(4);
    }
    if show_activity && left >= 4 {
        rects.activity_rect = Some(Rect {
            x: 0,
            y: out.len() as i64,
            w,
            h: left,
        });
        out.extend(feed_panel(s, w, left, now, rects));
    } else if left > 0 {
        out.extend(vec![" ".repeat(u(w)); u(left)]);
    }
    out
}

// ---------------------------------------------------------------- agent pane in the frame

fn chat_agent<'a>(s: &DashState<'a>) -> Option<&'a Agent> {
    let a = s.agent?;
    match s.chat_agent {
        Some(i) => a.subagents.get(i).or(Some(a)),
        None => Some(a),
    }
}

fn agent_panel(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    now: i64,
    docked: bool,
    out: &mut Rects,
    body_out: &mut Option<agent::Body>,
) -> Lines {
    let main = s.agent.unwrap();
    let chat = chat_agent(s).unwrap();
    let focused = s.focus == Focus::Agent;
    let subagent_view = !std::ptr::eq(chat, main);
    let mut opts = s.view.opts.clone();
    opts.focused = Some(focused);
    opts.docked = docked;
    opts.config = main.config.clone();
    let empty: [char; 0] = [];
    let editor = s.editor.as_ref().map_or(
        EditorView {
            chars: &empty,
            cur: 0,
        },
        |e| EditorView {
            chars: e.chars,
            cur: e.cur,
        },
    );
    let res = agent::render_agent(
        chat,
        &s.session.todos,
        &editor,
        u(w - 3),
        u(h - 2),
        frame,
        now,
        &opts,
    );
    out.max_scroll = res.max_scroll;
    out.caret = res.caret; // agent-line coordinates until render_frame offsets them
    *body_out = Some(res.body);
    let busy = chat.status != "idle";
    let asking = !chat.pending.is_empty();
    let where_ = shorten_left(&chat.cwd, 12.max(u(w - 40)));
    let tag = if let Some(p) = &s.view.opts.preview {
        paint(
            COL_WARN,
            &format!(
                "viewing {} · enter continue · esc back",
                p.id.chars().take(8).collect::<String>()
            ),
        )
    } else if asking {
        let what = match chat.pending[0].dialog.as_str() {
            "question" => "question",
            "plan" => "plan ready",
            _ => "permission needed",
        };
        paint(
            COL_WARN,
            &format!("◆ {what}{}", if focused { "" } else { " · tab" }),
        )
    } else if busy {
        paint(COL_OK, &format!("{} working", spin(frame)))
    } else {
        paint(COL_MUTED, if focused { "ready" } else { "tab to type" })
    };
    let sub_route = if subagent_view {
        format!(
            " · {} · effort {}",
            model_label(&chat.model, &chat.active_model, main.config.as_ref())
                .unwrap_or_else(|| "model unknown".into()),
            if chat.effort.is_empty() {
                "auto"
            } else {
                &chat.effort
            }
        )
    } else {
        String::new()
    };
    let title = format!(
        "SYNTHESIS{} {}  {tag}",
        if subagent_view {
            format!(" @{}", chat.display_name())
        } else {
            String::new()
        },
        paint(COL_MUTED, &format!("· {where_}{sub_route}"))
    );
    let lines = res.lines;
    boxed(&title, lines, w, h, focused || busy, VAlign::Top)
}

// ---------------------------------------------------------------- usage

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const USAGE_LABEL_W: usize = 4;
const USAGE_SUMMARY_W: usize = 20;
const USAGE_MAX_H: i64 = 2 + 1 + 7 + 1;
const USAGE_WEEKS: i64 = 53;
const USAGE_YEAR_H: i64 = 2 + 15;
const USAGE_MIN_H: i64 = 4;

pub fn heat_level(v: Option<f64>, max: Option<f64>) -> usize {
    let (Some(v), Some(max)) = (v, max) else {
        return 0;
    };
    if v.is_nan() || max.is_nan() || v <= 0.0 || max <= 0.0 || !v.is_finite() || !max.is_finite() {
        return 0;
    }
    4.min(1 + 3.min(((v / max) * (4.0 - 1e-9)).floor() as usize))
}

#[derive(Clone, Copy)]
struct Metric {
    label: &'static str,
    key: usize,
    is_tokens: bool,
}
const METRICS: [(&str, Metric); 4] = [
    (
        "tokens",
        Metric {
            label: "tokens",
            key: 0,
            is_tokens: true,
        },
    ),
    (
        "messages",
        Metric {
            label: "messages",
            key: 1,
            is_tokens: false,
        },
    ),
    (
        "tools",
        Metric {
            label: "tool calls",
            key: 2,
            is_tokens: false,
        },
    ),
    (
        "agents",
        Metric {
            label: "agents",
            key: 3,
            is_tokens: false,
        },
    ),
];
fn metric_of(name: &str) -> (&'static str, Metric) {
    METRICS
        .iter()
        .find(|(k, _)| *k == name)
        .copied()
        .unwrap_or(METRICS[0])
}
fn val_of(m: &Metric, d: &HistoryDay) -> f64 {
    match m.key {
        0 => d.tokens,
        1 => d.messages,
        2 => d.tools,
        _ => d.agents,
    }
}
fn m_fmt(m: &Metric, v: f64) -> String {
    if m.is_tokens {
        fmt_num(Some(v))
    } else {
        crate::util::round_str(v)
    }
}

#[derive(Clone)]
struct DayCell<'a> {
    day: i64,
    month: usize,
    future: bool,
    entry: Option<&'a HistoryDay>,
}

struct MonthGrid<'a> {
    weeks: Vec<[Option<DayCell<'a>>; 7]>,
    year: i64,
    month: usize,
    today_ms: i64,
}

fn usage_month_grid<'a>(history: &'a [HistoryDay], tz: i32, now: i64) -> MonthGrid<'a> {
    let today_ms = history.last().map_or(now, |l| l.date_ms);
    let t = civil(today_ms, tz);
    let dim = days_in_month(t.year, t.month);
    let mut weeks = vec![];
    let mut week: [Option<DayCell>; 7] = Default::default();
    for day in 1..=dim {
        let days = days_from_civil(t.year, t.month, day);
        let start_ms = days * 86_400_000 - i64::from(tz) * 60_000;
        let dow = (days + 4).rem_euclid(7) as usize;
        let entry = history.iter().find(|d| {
            let c = civil(d.date_ms, tz);
            c.year == t.year && c.month == t.month && c.day == day
        });
        week[dow] = Some(DayCell {
            day,
            month: t.month,
            future: start_ms > today_ms,
            entry,
        });
        if dow == 6 || day == dim {
            weeks.push(std::mem::take(&mut week));
        }
    }
    MonthGrid {
        weeks,
        year: t.year,
        month: t.month,
        today_ms,
    }
}

fn usage_panel_size(s: &DashState, with_summary: bool, now: i64) -> (i64, i64) {
    let grid = usage_month_grid(s.history, s.tz_offset_min, now);
    let grid_w = (USAGE_LABEL_W + grid.weeks.len().max(1) * 2 - 1) as i64;
    let inner = grid_w
        + if with_summary {
            3 + USAGE_SUMMARY_W as i64
        } else {
            0
        };
    (
        inner + 3,
        if open_todos(s) > 0 {
            USAGE_MAX_H
        } else {
            USAGE_YEAR_H
        },
    )
}

fn legend() -> String {
    format!(
        "{} {} {}",
        paint(COL_SUBTLE, "Less"),
        HEAT.iter()
            .map(|c| paint(*c, "■"))
            .collect::<Vec<_>>()
            .join(" "),
        paint(COL_SUBTLE, "More")
    )
}

fn spread_columns(cols: Vec<Lines>, inner: usize, min_gap: usize) -> Lines {
    let ws: Vec<usize> = cols
        .iter()
        .map(|c| c.iter().map(|l| width(l)).max().unwrap_or(0))
        .collect();
    let h = cols.iter().map(Vec::len).max().unwrap_or(0);
    let n = cols.len();
    let spare = inner.saturating_sub(ws.iter().sum::<usize>() + min_gap * (n - 1));
    let share = spare / (n + 1);
    let gap = min_gap + share;
    let left = share + (spare - share * (n + 1)) / 2;
    (0..h)
        .map(|r| {
            let mut line = " ".repeat(left);
            for (i, c) in cols.iter().enumerate() {
                let top = (h - c.len()) / 2;
                let cell = if r >= top {
                    c.get(r - top).map_or("", String::as_str)
                } else {
                    ""
                };
                line.push_str(&fit(cell, ws[i]));
                if i < n - 1 {
                    line.push_str(&" ".repeat(gap));
                }
            }
            line
        })
        .collect()
}

fn usage_summary(
    history: &[HistoryDay],
    grid: &MonthGrid,
    m: &Metric,
    tz: i32,
) -> Vec<(&'static str, String)> {
    let val = |d: &HistoryDay| val_of(m, d);
    let month_days: Vec<&DayCell> = grid
        .weeks
        .iter()
        .flatten()
        .flatten()
        .filter(|d| !d.future)
        .collect();
    let active = month_days
        .iter()
        .filter(|d| d.entry.is_some_and(|e| val(e) > 0.0))
        .count();
    let mut peak_val = -1.0;
    let mut peak_day: Option<&DayCell> = None;
    for d in &month_days {
        let v = d.entry.map_or(0.0, val);
        if v > peak_val {
            peak_val = v;
            peak_day = Some(d);
        }
    }
    let today_entry = history.last();
    let mut streak = 0;
    let mut i = history.len() as i64
        - 1
        - if today_entry.is_some_and(|t| val(t) > 0.0) {
            0
        } else {
            1
        };
    while i >= 0 && val(&history[i as usize]) > 0.0 {
        streak += 1;
        i -= 1;
    }
    let sum = |n: usize| {
        history[history.len().saturating_sub(n)..]
            .iter()
            .map(val)
            .sum::<f64>()
    };
    let month_total: f64 = month_days.iter().map(|d| d.entry.map_or(0.0, val)).sum();
    let when = peak_day.map_or(String::new(), |d| {
        let _ = tz;
        format!(" {} {}", d.day, MONTHS[d.month])
    });
    vec![
        (
            "today",
            paint(COL_ACCENT, &m_fmt(m, today_entry.map_or(0.0, val))),
        ),
        ("7 days", paint(COL_TEXT, &m_fmt(m, sum(7)))),
        ("month", paint(COL_TEXT, &m_fmt(m, month_total))),
        (
            "best",
            format!(
                "{}{}",
                paint(COL_TEXT, &m_fmt(m, peak_val.max(0.0))),
                paint(COL_SUBTLE, &when)
            ),
        ),
        (
            "active",
            format!(
                "{}{}",
                paint(COL_TEXT, &active.to_string()),
                paint(COL_SUBTLE, &format!("/{} days", month_days.len()))
            ),
        ),
        (
            "streak",
            format!(
                "{}{}",
                paint(
                    if streak > 0 { COL_OK } else { COL_TEXT },
                    &streak.to_string()
                ),
                paint(COL_SUBTLE, if streak == 1 { " day" } else { " days" })
            ),
        ),
    ]
}

fn row_label(r: usize) -> &'static str {
    ["", "M", "", "W", "", "F", ""][r]
}

fn usage_month_panel(s: &DashState, w: i64, h: i64, valign: VAlign, now: i64) -> Lines {
    let history = s.history;
    let (_, m) = metric_of(s.usage_metric);
    let inner = w - 3;
    if history.is_empty() {
        return boxed(
            "Usage",
            vec![paint(COL_MUTED, "collecting history…")],
            w,
            h,
            false,
            valign,
        );
    }
    let val = |d: &HistoryDay| val_of(&m, d);
    let grid = usage_month_grid(history, s.tz_offset_min, now);
    let month_days: Vec<&DayCell> = grid
        .weeks
        .iter()
        .flatten()
        .flatten()
        .filter(|d| !d.future)
        .collect();
    let month_total: f64 = month_days.iter().map(|d| d.entry.map_or(0.0, val)).sum();
    let titles = [
        format!("· {} · {} this month", m.label, m_fmt(&m, month_total)),
        format!("· {} · {}", m.label, m_fmt(&m, month_total)),
        format!("· {}", m_fmt(&m, month_total)),
    ]
    .map(|t| format!("Usage {}", paint(COL_MUTED, &t)));
    let title = titles
        .iter()
        .find(|t| width(t) as i64 <= w - 7)
        .cloned()
        .unwrap_or_else(|| "Usage".into());
    let leg = legend();
    let weeks = grid.weeks.len();
    let grid_w = USAGE_LABEL_W + weeks * 2 - 1;
    let max = month_days
        .iter()
        .map(|d| d.entry.map_or(0.0, val))
        .fold(1.0, f64::max);
    if h - 2 < 9 {
        let n = month_days.len().min(u((inner + 1) / 2));
        let strip = month_days[month_days.len() - n..]
            .iter()
            .map(|d| {
                paint(
                    HEAT[heat_level(Some(d.entry.map_or(0.0, val)), Some(max))],
                    "■",
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut rows = vec![strip];
        if h - 2 >= 2 {
            rows.push(format!(
                "{}{leg}",
                " ".repeat(u(inner.min(n as i64 * 2 - 1) - width(&leg) as i64))
            ));
        }
        return boxed(&title, rows, w, h, false, valign);
    }
    let mut grid_rows = vec![paint(
        COL_MUTED,
        &format!("{} {}", MONTHS[grid.month], grid.year),
    )];
    for r in 0..7 {
        let mut cells = vec![];
        for c in 0..weeks {
            match &grid.weeks[c][r] {
                None => cells.push(" ".to_string()),
                Some(d) if d.future => cells.push(paint(HEAT[0], "·")),
                Some(d) => {
                    let lvl = heat_level(Some(d.entry.map_or(0.0, val)), Some(max));
                    cells.push(paint(HEAT[lvl], "■"));
                }
            }
        }
        grid_rows.push(format!(
            "{}{}",
            paint(COL_SUBTLE, &pad_end(row_label(r), USAGE_LABEL_W)),
            cells.join(" ")
        ));
    }
    let side: Lines = usage_summary(history, &grid, &m, s.tz_offset_min)
        .into_iter()
        .map(|(k, v)| format!("{}{v}", paint(COL_MUTED, &pad_end(k, 8))))
        .collect();
    let side_w = side.iter().map(|x| width(x)).max().unwrap_or(0) as i64;
    let mut lines = if inner - grid_w as i64 - 3 >= side_w {
        spread_columns(vec![grid_rows, side], u(inner), 3)
    } else {
        spread_columns(vec![grid_rows], u(inner), 3)
    };
    lines.push(format!(
        "{}{leg}",
        " ".repeat((u(inner)).saturating_sub(width(&leg)) / 2)
    ));
    let _ = grid.today_ms;
    boxed(&title, lines, w, h, false, VAlign::Center)
}

fn usage_year_stats(
    history: &[HistoryDay],
    m: &Metric,
    metric_name: &str,
    tz: i32,
) -> Vec<(String, String)> {
    let val = |d: &HistoryDay| val_of(m, d);
    let today = history.last().unwrap();
    let sum = |arr: &[HistoryDay]| arr.iter().map(val).sum::<f64>();
    let last = |n: usize| &history[history.len().saturating_sub(n)..];
    let year = last(365.min(history.len()));
    let active: Vec<&HistoryDay> = year.iter().filter(|d| val(d) > 0.0).collect();
    let mut best = today;
    for d in year {
        if val(d) > val(best) {
            best = d;
        }
    }
    let mut week_best: f64 = 0.0;
    for i in 0..=year.len().saturating_sub(7) {
        if i + 7 <= year.len() {
            week_best = week_best.max(sum(&year[i..i + 7]));
        }
    }
    let this_week = sum(last(7));
    let prev_week =
        sum(&history[history.len().saturating_sub(14)..history.len().saturating_sub(7)]);
    let change = if prev_week != 0.0 {
        Some(js_round((this_week - prev_week) / prev_week * 100.0) as i64)
    } else {
        None
    };
    let mut streak = 0;
    let mut i = history.len() as i64 - 1 - if val(today) > 0.0 { 0 } else { 1 };
    while i >= 0 && val(&history[i as usize]) > 0.0 {
        streak += 1;
        i -= 1;
    }
    let (mut longest, mut run) = (0, 0);
    for d in year {
        run = if val(d) > 0.0 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let mut by_dow = [0.0f64; 7];
    for d in year {
        by_dow[civil(d.date_ms, tz).dow] += val(d);
    }
    let mx = by_dow.iter().copied().fold(f64::MIN, f64::max);
    let top_dow = by_dow.iter().position(|v| *v == mx).unwrap_or(0);
    let bc = civil(best.date_ms, tz);
    let when = format!(" {} {}", bc.day, MONTHS[bc.month]);
    let sessions: f64 = last(30).iter().map(|d| d.sessions).sum();
    let t = |x: &str| paint(COL_TEXT, x);
    let sb = |x: &str| paint(COL_SUBTLE, x);
    let mut rows: Vec<(String, String)> = vec![
        ("today".into(), paint(COL_ACCENT, &m_fmt(m, val(today)))),
        (
            "7 days".into(),
            format!(
                "{}{}",
                t(&m_fmt(m, this_week)),
                change.map_or(String::new(), |c| paint(
                    if c >= 0 { COL_OK } else { COL_WARN },
                    &format!(" {}{}%", if c >= 0 { "▲" } else { "▼" }, c.abs())
                ))
            ),
        ),
        ("30 days".into(), t(&m_fmt(m, sum(last(30))))),
        ("year".into(), t(&m_fmt(m, sum(year)))),
        (
            "avg/day".into(),
            format!(
                "{}{}",
                t(&m_fmt(
                    m,
                    js_round(sum(year) / (active.len().max(1) as f64))
                )),
                sb(" active days")
            ),
        ),
        (
            "best".into(),
            format!("{}{}", t(&m_fmt(m, val(best))), sb(&when)),
        ),
        ("best wk".into(), t(&m_fmt(m, week_best))),
        (
            "busiest".into(),
            t([
                "Sundays",
                "Mondays",
                "Tuesdays",
                "Wednesdays",
                "Thursdays",
                "Fridays",
                "Saturdays",
            ][top_dow]),
        ),
        (
            "active".into(),
            format!(
                "{}{}",
                t(&active.len().to_string()),
                sb(&format!("/{} days", year.len()))
            ),
        ),
        (
            "streak".into(),
            format!(
                "{}{}",
                paint(
                    if streak > 0 { COL_OK } else { COL_TEXT },
                    &streak.to_string()
                ),
                sb(&format!(" now · {longest} best"))
            ),
        ),
        (
            "sessions".into(),
            format!(
                "{}{}",
                t(&crate::util::round_str(sessions)),
                sb(" in 30 days")
            ),
        ),
    ];
    for (k, mm) in METRICS.iter() {
        if *k == metric_name {
            continue;
        }
        let total: f64 = last(30).iter().map(|d| val_of(mm, d)).sum();
        let label: String = mm
            .label
            .split(' ')
            .next()
            .unwrap()
            .chars()
            .take(8)
            .collect();
        rows.push((label, format!("{}{}", t(&m_fmt(mm, total)), sb(" 30d"))));
    }
    rows
}

fn usage_year_content(s: &DashState, inner: i64, max_rows: usize) -> (String, Lines) {
    let history = s.history;
    let (mname, m) = metric_of(s.usage_metric);
    let val = |d: &HistoryDay| val_of(&m, d);
    let sum = |n: usize| {
        history[history.len().saturating_sub(n)..]
            .iter()
            .map(val)
            .sum::<f64>()
    };
    let title = format!(
        "Usage {}",
        paint(
            COL_MUTED,
            &format!(
                "· {} · {} 7d · {} year · u: metric",
                m.label,
                m_fmt(&m, sum(7)),
                m_fmt(&m, sum(365))
            )
        )
    );
    let leg = legend();
    let stats: Lines = usage_year_stats(history, &m, mname, s.tz_offset_min)
        .into_iter()
        .map(|(k, v)| format!("{}{v}", paint(COL_MUTED, &pad_end(&k, 9))))
        .collect();
    let stat_w = 32.min(stats.iter().map(|x| width(x)).max().unwrap_or(0)) as i64;
    let fit_weeks = |reserve: i64| {
        clamp_int(
            (inner - reserve - USAGE_LABEL_W as i64 + 1).div_euclid(2),
            1,
            USAGE_WEEKS,
        )
    };
    let col_w = stat_w + 3;
    let mut weeks = fit_weeks(0);
    let mut stat_cols = 2.min((inner - (USAGE_LABEL_W as i64 + weeks * 2 - 1)).div_euclid(col_w));
    if stat_cols < 1 && fit_weeks(col_w) >= 13 {
        weeks = fit_weeks(col_w);
        stat_cols = 1;
    }
    let stat_cols = stat_cols.max(0) as usize;
    let grid_w = USAGE_LABEL_W + weeks as usize * 2 - 1;
    let today = history.last().unwrap();
    let today_dow = civil(today.date_ms, s.tz_offset_min).dow as i64;
    let cell = |c: i64, r: i64| -> Option<&HistoryDay> {
        let ago = (weeks - 1 - c) * 7 + (today_dow - r);
        if ago < 0 {
            return None;
        }
        let idx = history.len() as i64 - 1 - ago;
        if idx < 0 {
            None
        } else {
            history.get(idx as usize)
        }
    };
    let mut max = 1.0f64;
    for c in 0..weeks {
        for r in 0..7 {
            if let Some(d) = cell(c, r) {
                max = max.max(val(d));
            }
        }
    }
    let mut months = vec![' '; grid_w];
    let mut last_x: i64 = -10;
    for c in 0..weeks {
        let mut mo: Option<usize> = None;
        for r in 0..7 {
            if let Some(d) = cell(c, r) {
                let cv = civil(d.date_ms, s.tz_offset_min);
                if cv.day == 1 {
                    mo = Some(cv.month);
                }
            }
        }
        let x = USAGE_LABEL_W as i64 + c * 2;
        let Some(mo) = mo else { continue };
        if x + 3 > grid_w as i64 || x - last_x < 4 {
            continue;
        }
        for (k, ch) in MONTHS[mo].chars().enumerate() {
            months[x as usize + k] = ch;
        }
        last_x = x;
    }
    let mut grid = vec![paint(COL_MUTED, &months.iter().collect::<String>())];
    for r in 0..7 {
        let cells: Vec<String> = (0..weeks)
            .map(|c| match cell(c, r) {
                None => " ".to_string(),
                Some(d) => paint(HEAT[heat_level(Some(val(d)), Some(max))], "■"),
            })
            .collect();
        grid.push(format!(
            "{}{}",
            paint(COL_SUBTLE, &pad_end(row_label(r as usize), USAGE_LABEL_W)),
            cells.join(" ")
        ));
    }
    grid.push(format!(
        "{}{leg}",
        " ".repeat(grid_w.saturating_sub(width(&leg)))
    ));
    if stat_cols == 0 {
        let pick = |k: &str| {
            stats
                .iter()
                .find(|x| strip_ansi(x).starts_with(k))
                .cloned()
                .unwrap_or_default()
        };
        let below: Lines = ["today", "7 days", "30 days", "streak", "best"]
            .iter()
            .map(|k| pick(k))
            .filter(|x| !x.is_empty())
            .collect();
        let mut col: Lines = grid;
        col.extend(below);
        col.truncate(max_rows);
        return (title, spread_columns(vec![col], u(inner), 3));
    }
    let per = max_rows.min(stats.len().div_ceil(stat_cols));
    let mut columns = vec![grid];
    for i in 0..stat_cols {
        columns.push(
            stats
                .iter()
                .skip(i * per)
                .take(per)
                .map(|x| fit(x, u(stat_w)))
                .collect(),
        );
    }
    (title, spread_columns(columns, u(inner), 3))
}

fn usage_year_height(s: &DashState, w: i64) -> i64 {
    if s.history.is_empty() {
        return USAGE_MIN_H;
    }
    2 + usage_year_content(s, w - 3, u(USAGE_YEAR_H - 2)).1.len() as i64
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UsageView {
    Auto,
    Year,
    Month,
}

pub fn usage_panel(
    s: &DashState,
    w: i64,
    h: i64,
    valign: VAlign,
    view: UsageView,
    now: i64,
) -> Lines {
    let view = match view {
        UsageView::Auto => {
            if open_todos(s) > 0 {
                UsageView::Month
            } else {
                UsageView::Year
            }
        }
        v => v,
    };
    if view == UsageView::Year && !s.history.is_empty() {
        if h - 2 < 9 {
            return usage_month_panel(s, w, h, valign, now);
        }
        let (title, lines) = usage_year_content(s, w - 3, u(h - 2));
        return boxed(&title, lines, w, h, false, VAlign::Center);
    }
    usage_month_panel(s, w, h, valign, now)
}

// ---------------------------------------------------------------- header / footer

fn logo_pill(text: &str, frame: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len() as i64;
    let sweep = (frame as i64 % (n + 40)) - 4;
    let bg_at = |i: i64| {
        let base = cycle(
            &LOGO_GRAD,
            i as f64 / n as f64 / 2.0 + ((frame / 5) as f64) / 32.0,
        );
        let d = (i - sweep).abs();
        if d < 3 {
            lerp(base, ACCENT_GLOW, (3 - d) as f64 / 3.0 * 0.55)
        } else {
            base
        }
    };
    let mut s = paint(bg_at(0), "▐");
    for (i, ch) in chars.iter().enumerate() {
        s.push_str(&format!(
            "{}{}{BOLD}{ch}{RESET}",
            bg(bg_at(i as i64)),
            fg(ACCENT_INK)
        ));
    }
    s + &paint(bg_at(n - 1), "▌")
}

fn mini_bar(pct: f64, n: usize) -> String {
    let p = pct.clamp(0.0, 100.0);
    let filled = js_round(p / 100.0 * n as f64) as usize;
    format!(
        "{}{}",
        paint(level_color(p), &"▰".repeat(filled)),
        paint(COL_TRACK, &"▱".repeat(n - filled))
    )
}

fn header(s: &DashState, w: i64, frame: usize, now: i64) -> String {
    let session = s.session;
    let sep = paint(COL_BORDER, " │ ");
    let live = s.sessions.iter().filter(|x| x.live.is_some()).count();
    let busy = s
        .sessions
        .iter()
        .filter(|x| x.live.is_some() && !x.phase.is_empty() && x.phase != "idle")
        .count();
    let tap = s.usage.tap.as_ref();
    let model = model_of(s)
        .or(tap.and_then(|t| t.model.clone()).filter(|m| !m.is_empty()))
        .or(Some(session.model.clone()).filter(|m| !m.is_empty()))
        .unwrap_or_default();
    let project = {
        let b = basename(&session.cwd);
        if b.is_empty() {
            session.project.clone()
        } else {
            b
        }
    };
    let ctx_pct = ctx_pct_of(s);
    let logos = [
        " ✻ Synthesis  ·  MISSION CONTROL ",
        " ✻ Synthesis · MISSION CONTROL ",
        " ✻ Synthesis ",
        " ✻ ",
    ];
    let pulse_s = if busy > 0 {
        paint(COL_OK, spin(frame))
    } else {
        paint(COL_MUTED, "●")
    };
    // (priority, text)
    let left_chips: Vec<(u8, String)> = [
        (
            1,
            format!(
                "{pulse_s} {}{}",
                paint(COL_TEXT, &format!("{live} live")),
                if busy > 0 {
                    paint(COL_MUTED, &format!(" · {busy} busy"))
                } else {
                    String::new()
                }
            ),
        ),
        (
            2,
            if model.is_empty() {
                String::new()
            } else {
                paint(ACCENT_LIGHT, &format!("◆ {model}"))
            },
        ),
        (
            4,
            if project.is_empty() {
                String::new()
            } else {
                format!("{}{}", paint(COL_MUTED, "⌂ "), paint(COL_TEXT, &project))
            },
        ),
        (
            5,
            if session.git_branch.is_empty() {
                String::new()
            } else {
                paint(COL_MUTED, &format!("⎇ {}", session.git_branch))
            },
        ),
    ]
    .into_iter()
    .filter(|c| !c.1.is_empty())
    .collect();
    let right_chips: Vec<(u8, String)> = [
        (1, paint(COL_WHITE, &fmt_time(now, s.tz_offset_min))),
        (
            3,
            format!(
                "{}{} {}",
                paint(COL_MUTED, "ctx "),
                mini_bar(ctx_pct, 6),
                paint(level_color(ctx_pct), &format!("{}%", to_fixed(ctx_pct, 0)))
            ),
        ),
        (
            4,
            s.usage.five_hour.as_ref().map_or(String::new(), |q| {
                format!(
                    "{}{}",
                    paint(COL_MUTED, "5h "),
                    paint(level_color(q.used), &format!("{}%", to_fixed(q.used, 0)))
                )
            }),
        ),
        (
            4,
            tap.and_then(|t| t.cost_usd).map_or(String::new(), |c| {
                paint(COL_WARN, &format!("${}", to_fixed(c, 2)))
            }),
        ),
        (
            6,
            paint(
                COL_MUTED,
                &format!(
                    "{} · up {}",
                    session.id.chars().take(8).collect::<String>(),
                    fmt_dur(Some(
                        (now - session.started_at.filter(|t| *t != 0).unwrap_or(now)) as f64
                    ))
                ),
            ),
        ),
    ]
    .into_iter()
    .filter(|c| !c.1.is_empty())
    .collect();
    // chips by (side, index); dropped by priority, highest p first (stable for equal p: left before right)
    let mut all: Vec<(u8, bool, usize)> = left_chips
        .iter()
        .enumerate()
        .map(|(i, c)| (c.0, true, i))
        .chain(right_chips.iter().enumerate().map(|(i, c)| (c.0, false, i)))
        .collect();
    all.sort_by_key(|c| std::cmp::Reverse(c.0));
    for (li, logo) in logos.iter().enumerate() {
        let is_last = li == logos.len() - 1;
        let pill = logo_pill(logo, frame);
        let mut keep_l = vec![true; left_chips.len()];
        let mut keep_r = vec![true; right_chips.len()];
        let build = |kl: &[bool], kr: &[bool]| {
            let mut parts = vec![pill.clone()];
            parts.extend(
                left_chips
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| kl[*i])
                    .map(|(_, c)| c.1.clone()),
            );
            let left = parts.join(&sep);
            let right = right_chips
                .iter()
                .enumerate()
                .filter(|(i, _)| kr[*i])
                .map(|(_, c)| c.1.clone())
                .rev()
                .collect::<Vec<_>>()
                .join(&sep);
            let need = width(&left) as i64 + width(&right) as i64 + 2;
            (left, right, need)
        };
        let mut row = build(&keep_l, &keep_r);
        for (p, is_left, idx) in &all {
            if row.2 <= w {
                break;
            }
            if *p > 1 || is_last {
                if *is_left {
                    keep_l[*idx] = false
                } else {
                    keep_r[*idx] = false
                }
            }
            row = build(&keep_l, &keep_r);
        }
        if row.2 <= w || is_last {
            return fit(
                &format!(
                    "{}{}{}",
                    row.0,
                    " ".repeat(u(w - width(&row.0) as i64 - width(&row.1) as i64).max(1)),
                    row.1
                ),
                u(w),
            );
        }
    }
    fit("", u(w))
}

fn header_rule(w: i64) -> String {
    paint(lerp(ACCENT_CORE, COL_TRACK, 0.55), &"━".repeat(u(w)))
}

fn topology_panel(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> Lines {
    let tap = s.usage.tap.as_ref();
    let opts = FlowOpts {
        context_size: tap.and_then(|t| t.context_size),
        ctx_pct: if s.session.context_display_reset {
            Some(0.0)
        } else if !s.session.context_estimated {
            tap.and_then(|t| t.context_pct)
        } else {
            None
        },
    };
    let rows = render_flow(s.session, u(w - 3), u(h - 2), frame, now, opts);
    let (active, total) = node_counts(s.session, now);
    let count = if active > 0 {
        paint(PURPLE, &format!("{active} active"))
    } else {
        paint(COL_MUTED, &format!("{total} nodes"))
    };
    boxed(
        &format!("TOPOLOGY {count}"),
        rows,
        w,
        h,
        active > 0,
        VAlign::Top,
    )
}

fn footer(s: &DashState, w: i64) -> String {
    let agent_keys: &[(&str, &str)] = &[
        (
            "tab",
            "workspace selector (when multiple) → sessions → prompt",
        ),
        ("enter", "send"),
        ("esc", "interrupt"),
        ("⇧tab", "mode"),
        ("alt+v", "paste image"),
        ("!", "shell"),
        ("ctrl+o", "expand"),
        ("ctrl+r", "search history"),
        ("ctrl+l", "redraw"),
        ("ctrl+↑/↓", "browse sessions"),
        ("ctrl+n", "new workspace"),
        ("/new", "new session"),
        ("ctrl+pgup/pgdn", "switch session"),
        ("alt+↑/↓", "switch workspace"),
        ("pgup/pgdn", "scroll"),
        ("ctrl+c ×2", "quit"),
    ];
    let mut dash_keys: Vec<(&str, &str)> = vec![("↑/↓", "select session")];
    if s.workspaces.is_some() {
        dash_keys.push(("←/→", "switch workspace"));
    }
    dash_keys.extend([
        ("a", "follow newest"),
        ("u", "usage metric"),
        ("r", "reload"),
    ]);
    if s.agent.is_some() {
        dash_keys.push(("tab", "agent"));
    }
    dash_keys.push(("q", "quit"));
    let ws_keys: &[(&str, &str)] = &[
        ("↑/↓", "switch workspace"),
        ("g/G", "first/last"),
        ("enter/→/tab", "sessions"),
        ("esc", "agent"),
        ("q", "quit"),
    ];
    let perm_keys: &[(&str, &str)] = &[
        ("↑/↓ 1-3", "choose"),
        ("enter", "confirm"),
        ("esc", "reject"),
        ("tab", "dashboard"),
    ];
    let mut keys: Vec<(&str, &str)> = if s.agent.is_some() && s.focus == Focus::Agent {
        agent_keys.to_vec()
    } else if s.focus == Focus::Workspace {
        ws_keys.to_vec()
    } else {
        dash_keys
    };
    if s.agent
        .is_some_and(|a| s.focus == Focus::Agent && !a.pending.is_empty())
    {
        keys = perm_keys.to_vec();
    }
    // Whole hints only, later ones dropped first (QA B-27); "[following newest]" is kept last.
    let mut items: Vec<(u8, String)> = keys
        .iter()
        .enumerate()
        .map(|(i, (k, d))| {
            (
                (i as u8).saturating_add(1).min(200),
                format!("{} {}", paint(COL_ACCENT, k), paint(COL_MUTED, d)),
            )
        })
        .collect();
    if s.follow {
        items.push((0, paint(COL_OK, "[following newest]")));
    }
    fit_items(&items, "   ", u(w))
}

// ---------------------------------------------------------------- layout

pub const MIN_COLS: i64 = 60;
pub const MIN_ROWS: i64 = 16;
const TOPO_RIGHT_MIN_W: i64 = 140;
const TOPO_RIGHT_MIN_H: i64 = 24;
const TOPO_BOTTOM_MIN_H: i64 = 40;
const DETAIL_MIN_H: i64 = 16;
const STATUS_H: i64 = 7;
const AGENT_MIN_H: i64 = 14;
const TOPO_MIN_H: i64 = 16;
const TOPO_AGENT_MAX_H: i64 = 32;
const DOCK_MIN_H: i64 = 10;
const DOCK_MIN_W: i64 = 40;
const TASKS_MIN_W: i64 = 22;
const USAGE_LEGEND_W: i64 = 19;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Right,
    Bottom,
    None,
}

pub fn layout_for(w: i64, h: i64) -> Layout {
    let landscape = w as f64 >= h as f64 * 2.4;
    let fits_right = w >= TOPO_RIGHT_MIN_W && h >= TOPO_RIGHT_MIN_H;
    let fits_bottom = h >= TOPO_BOTTOM_MIN_H;
    if fits_right && (landscape || !fits_bottom) {
        Layout::Right
    } else if fits_bottom {
        Layout::Bottom
    } else {
        Layout::None
    }
}

struct Ctx<'r> {
    rects: &'r mut Rects,
    hits: Hits,
    local_rect: Option<Rect>,
    body: Option<agent::Body>,
    sub_docked: bool,
}

pub struct TasksUsage {
    pub rows: Lines,
    pub usage: bool,
    pub tasks: bool,
}

pub fn tasks_usage_row(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> TasksUsage {
    if open_todos(s) == 0 {
        return TasksUsage {
            rows: usage_panel(s, w, h, VAlign::Evenly, UsageView::Year, now),
            usage: true,
            tasks: false,
        };
    }
    let half = w / 2;
    if half >= TASKS_MIN_W && w - half >= usage_panel_size(s, false, now).0.max(USAGE_LEGEND_W + 3)
    {
        return TasksUsage {
            rows: hstack(vec![
                tasks_panel(s, half, h, frame, VAlign::Evenly),
                usage_panel(s, w - half, h, VAlign::Evenly, UsageView::Auto, now),
            ]),
            usage: true,
            tasks: true,
        };
    }
    let usage_w = w.min(usage_panel_size(s, true, now).0);
    if w - usage_w >= TASKS_MIN_W {
        let tasks_w = w - usage_w;
        return TasksUsage {
            rows: hstack(vec![
                tasks_panel(s, tasks_w, h, frame, VAlign::Evenly),
                usage_panel(s, usage_w, h, VAlign::Evenly, UsageView::Auto, now),
            ]),
            usage: true,
            tasks: true,
        };
    }
    if open_todos(s) > 0 {
        return TasksUsage {
            rows: tasks_panel(s, w, h, frame, VAlign::Evenly),
            usage: false,
            tasks: true,
        };
    }
    TasksUsage {
        rows: usage_panel(s, w, h, VAlign::Evenly, UsageView::Auto, now),
        usage: true,
        tasks: false,
    }
}

struct Topo {
    rows: Lines,
    usage: bool,
    tasks: bool,
    docked: bool,
    dock_offset: Option<(i64, i64)>,
}

fn viewed_subagent<'a>(s: &DashState<'a>) -> Option<&'a Agent> {
    let a = s.agent?;
    s.chat_agent.and_then(|i| a.subagents.get(i))
}

fn topology_column(s: &DashState, w: i64, h: i64, frame: usize, now: i64, cx: &mut Ctx) -> Topo {
    let min_below = USAGE_MIN_H;
    if h - TOPO_MIN_H < min_below {
        return Topo {
            rows: topology_panel(s, w, h, frame, now),
            usage: false,
            tasks: false,
            docked: false,
            dock_offset: None,
        };
    }
    let topo_max_h = if s.agent.is_some() {
        (h - min_below).min(TOPO_AGENT_MAX_H)
    } else {
        h - min_below
    };
    let nat_h = natural_flow_height(
        s.session,
        w - 3,
        topo_max_h - 2,
        frame,
        now,
        FlowOpts::default(),
    ) + 2;
    let topo_h = clamp_int(nat_h, TOPO_MIN_H, topo_max_h);
    let mut below_h = h - topo_h;
    let todo_count = s.session.todos.len() as i64;
    below_h = below_h.min(if open_todos(s) == 0 {
        usage_year_height(s, w)
    } else {
        USAGE_MAX_H.max(todo_count + 4)
    });
    if s.agent.is_some() {
        below_h = below_h.min(USAGE_MAX_H.max(if open_todos(s) > 0 { todo_count + 4 } else { 0 }));
    }
    let below = tasks_usage_row(s, w, below_h, frame, now);
    let sub = viewed_subagent(s);
    let spare = h - below_h - topo_h;
    if let Some(sub) = sub.filter(|_| w >= DOCK_MIN_W && spare >= DOCK_MIN_H) {
        let status = if sub.status == "running" {
            "running"
        } else {
            "idle"
        };
        let (lines, max_scroll) = agent::transcript_rows(
            sub,
            u(w - 3),
            u(spare - 2),
            frame,
            now,
            s.view.opts.expanded,
            s.view.dock_scroll,
        );
        cx.rects.dock_max_scroll = max_scroll;
        cx.rects.dock_scroll = s.view.dock_scroll.min(max_scroll);
        let model = model_parts(
            &sub.model,
            &sub.active_model,
            s.agent.and_then(|a| a.config.as_ref()),
        );
        let route = format!(
            "{} · effort: {}",
            model.map_or("model unknown".into(), |m| m.id),
            if sub.effort.is_empty() {
                "auto"
            } else {
                &sub.effort
            }
        );
        let dock = boxed(
            &format!(
                "@{} {}",
                sub.display_name(),
                paint(COL_MUTED, &format!("· {status} · {route}"))
            ),
            lines,
            w,
            spare,
            frame_active(sub),
            VAlign::Top,
        );
        let mut rows = topology_panel(s, w, topo_h, frame, now);
        rows.extend(dock);
        rows.extend(below.rows);
        return Topo {
            rows,
            usage: below.usage,
            tasks: below.tasks,
            docked: true,
            dock_offset: Some((topo_h, spare)),
        };
    }
    let mut rows = topology_panel(s, w, h - below_h, frame, now);
    rows.extend(below.rows);
    Topo {
        rows,
        usage: below.usage,
        tasks: below.tasks,
        docked: false,
        dock_offset: None,
    }
}

fn frame_active(sub: &Agent) -> bool {
    sub.status == "running"
}

fn agent_detail(
    s: &DashState,
    d: i64,
    h: i64,
    frame: usize,
    now: i64,
    with_usage: bool,
    with_tasks: bool,
    cx: &mut Ctx,
) -> Lines {
    if h < STATUS_H + AGENT_MIN_H {
        cx.local_rect = Some(Rect {
            x: 0,
            y: 0,
            w: d,
            h,
        });
        return agent_panel(s, d, h, frame, now, cx.sub_docked, cx.rects, &mut cx.body);
    }
    let wide = d >= 96;
    let todos_len = if with_tasks {
        s.session.todos.len() as i64
    } else {
        0
    };
    let top = if wide && (!with_tasks || open_todos(s) == 0) {
        let status_w = (d as f64 * 0.6).floor() as i64;
        hstack(vec![
            status_panel(s, status_w, STATUS_H, frame, now),
            token_panel(s, d - status_w, STATUS_H),
        ])
    } else if wide {
        let tasks_w = (d as f64 * 0.3).floor() as i64;
        let rest_w = d - tasks_w;
        let status_w = (rest_w as f64 * 0.65).floor() as i64;
        hstack(vec![
            status_panel(s, status_w, STATUS_H, frame, now),
            token_panel(s, rest_w - status_w, STATUS_H),
            tasks_panel(s, tasks_w, STATUS_H, frame, VAlign::Evenly),
        ])
    } else {
        status_panel(s, d, STATUS_H, frame, now)
    };
    let usage_h = if open_todos(s) > 0 {
        USAGE_MAX_H
    } else {
        USAGE_YEAR_H
    };
    let mid = if with_usage && h - STATUS_H - usage_h >= 24 {
        usage_panel(s, d, usage_h, VAlign::Evenly, UsageView::Auto, now)
    } else {
        vec![]
    };
    let mut tasks_block: Lines = vec![];
    if !wide && open_todos(s) > 0 {
        let avail = h - STATUS_H - mid.len() as i64 - AGENT_MIN_H;
        let tasks_h = (todos_len + 3).min(8).min(avail.max(0));
        if tasks_h >= 4 {
            tasks_block = tasks_panel(s, d, tasks_h, frame, VAlign::Evenly);
        }
    }
    let agent_h = h - STATUS_H - mid.len() as i64 - tasks_block.len() as i64;
    cx.local_rect = Some(Rect {
        x: 0,
        y: (top.len() + mid.len() + tasks_block.len()) as i64,
        w: d,
        h: agent_h,
    });
    let mut out = top;
    out.extend(mid);
    out.extend(tasks_block);
    out.extend(agent_panel(
        s,
        d,
        agent_h,
        frame,
        now,
        cx.sub_docked,
        cx.rects,
        &mut cx.body,
    ));
    out
}

fn detail_panels(
    s: &DashState,
    d: i64,
    h: i64,
    frame: usize,
    now: i64,
    with_usage: bool,
    with_tasks: bool,
    cx: &mut Ctx,
) -> Lines {
    if s.agent.is_some() {
        return agent_detail(s, d, h, frame, now, with_usage, with_tasks, cx);
    }
    let top_h = 12.min(h);
    if h < top_h + 5 {
        return status_panel(s, d, h, frame, now);
    }
    let wide = d >= 90;
    let status_w = if wide {
        (d as f64 * 0.58).floor() as i64
    } else {
        d
    };
    let top = if wide {
        hstack(vec![
            status_panel(s, status_w, top_h, frame, now),
            token_panel(s, d - status_w, top_h),
        ])
    } else {
        status_panel(s, d, top_h, frame, now)
    };
    let mut mid: Lines = vec![];
    if with_usage {
        let usage_h = clamp_int(
            h - top_h - 8,
            0,
            if open_todos(s) > 0 {
                USAGE_MAX_H
            } else {
                USAGE_YEAR_H
            },
        );
        if usage_h >= USAGE_MIN_H {
            mid = if with_tasks {
                tasks_usage_row(s, d, usage_h, frame, now).rows
            } else {
                usage_panel(s, d, usage_h, VAlign::Evenly, UsageView::Auto, now)
            };
        }
    } else if with_tasks {
        let tasks_h = clamp_int(
            (2 + 1.max(s.session.todos.len() as i64 + 1)).min(h - top_h - 8),
            0,
            12,
        );
        if tasks_h >= 4 {
            mid = tasks_panel(s, d, tasks_h, frame, VAlign::Evenly);
        }
    }
    let feed_h = h - top_h - mid.len() as i64;
    let mut out = top;
    out.extend(mid);
    out.extend(feed_panel(s, d, feed_h, now, cx.rects));
    out
}

/// Renders the dashboard into `rows` lines of exactly `cols` cells (at least 60x16).
pub fn render_frame(
    s: &DashState,
    cols: usize,
    rows_total: usize,
    frame: usize,
    now: i64,
) -> Frame {
    let w = MIN_COLS.max(cols as i64);
    let h = MIN_ROWS.max(rows_total as i64);
    let rule = h >= 30;
    let body_h = h - 2 - i64::from(rule);
    let header_rows = 1 + i64::from(rule);
    let layout = layout_for(w, h);
    // Narrow terminals: chat only; the sidebar would leave ~38 columns for the conversation (QA B-25).
    let side_w = if s.agent.is_some() && w < 80 {
        0
    } else if s.agent.is_some() {
        clamp_int((w as f64 * 0.14).floor() as i64, 20, 40)
    } else {
        clamp_int((w as f64 * 0.17).floor() as i64, 22, 40)
    };
    let mut rects = Rects::default();
    let mut cx = Ctx {
        rects: &mut rects,
        hits: Hits::default(),
        local_rect: None,
        body: None,
        sub_docked: false,
    };

    let body: Lines;
    let sidebar_rect: Rect;
    // the sidebar is built before the detail column, as in Node, so the hit lists are filled in order
    match layout {
        Layout::Right => {
            let topo_w = if s.agent.is_some() {
                clamp_int((w as f64 * 0.26).floor() as i64, 46, 72)
            } else {
                clamp_int((w as f64 * 0.45).floor() as i64, 60, 130)
            };
            let d = w - side_w - topo_w;
            let topo = topology_column(s, topo_w, body_h, frame, now, &mut cx);
            if let (Some((dy, dh)), true) = (topo.dock_offset, topo.docked) {
                cx.rects.dock_rect = Some(Rect {
                    x: side_w + d,
                    y: header_rows + dy,
                    w: topo_w,
                    h: dh,
                });
            }
            cx.sub_docked = topo.docked;
            let side = {
                let mut hits = std::mem::take(&mut cx.hits);
                let r = if side_w == 0 {
                    vec![]
                } else {
                    side_column(s, side_w, body_h, frame, now, h / 2, cx.rects, &mut hits)
                };
                cx.hits = hits;
                r
            };
            let detail = detail_panels(s, d, body_h, frame, now, !topo.usage, !topo.tasks, &mut cx);
            body = hstack(vec![side, detail, topo.rows]);
            sidebar_rect = Rect {
                x: 0,
                y: header_rows,
                w: side_w,
                h: body_h,
            };
        }
        Layout::Bottom => {
            let lower_share = if s.agent.is_some() { 0.3 } else { 0.6 };
            let min_lower = TOPO_MIN_H;
            let max_lower = body_h
                - if s.agent.is_some() {
                    STATUS_H + AGENT_MIN_H
                } else {
                    DETAIL_MIN_H
                };
            let lower_h = clamp_int(
                js_round(body_h as f64 * lower_share) as i64,
                min_lower,
                max_lower,
            );
            let upper_h = body_h - lower_h;
            let topo = topology_column(s, w, lower_h, frame, now, &mut cx);
            if let (Some((dy, dh)), true) = (topo.dock_offset, topo.docked) {
                cx.rects.dock_rect = Some(Rect {
                    x: 0,
                    y: header_rows + upper_h + dy,
                    w,
                    h: dh,
                });
            }
            cx.sub_docked = topo.docked;
            let side = {
                let mut hits = std::mem::take(&mut cx.hits);
                let r = if side_w == 0 {
                    vec![]
                } else {
                    side_column(s, side_w, upper_h, frame, now, h / 2, cx.rects, &mut hits)
                };
                cx.hits = hits;
                r
            };
            let detail = detail_panels(
                s,
                w - side_w,
                upper_h,
                frame,
                now,
                !topo.usage,
                !topo.tasks,
                &mut cx,
            );
            let mut b = hstack(vec![side, detail]);
            b.extend(topo.rows);
            body = b;
            sidebar_rect = Rect {
                x: 0,
                y: header_rows,
                w: side_w,
                h: upper_h,
            };
        }
        Layout::None => {
            let side = {
                let mut hits = std::mem::take(&mut cx.hits);
                let r = if side_w == 0 {
                    vec![]
                } else {
                    side_column(s, side_w, body_h, frame, now, h / 2, cx.rects, &mut hits)
                };
                cx.hits = hits;
                r
            };
            let detail = detail_panels(s, w - side_w, body_h, frame, now, true, true, &mut cx);
            body = hstack(vec![side, detail]);
            sidebar_rect = Rect {
                x: 0,
                y: header_rows,
                w: side_w,
                h: body_h,
            };
        }
    }
    let local = cx.local_rect;
    let hits = std::mem::take(&mut cx.hits);
    let chat_body = cx.body.take();
    let r = cx.rects;
    if let Some(l) = local {
        r.chat_rect = Some(Rect {
            x: side_w + l.x,
            y: header_rows + l.y,
            w: l.w,
            h: l.h,
        });
    }
    // box border + space = 2 cells left, 1 row above the agent lines
    r.caret = r
        .caret
        .zip(local)
        .map(|((x, y), l)| (u(side_w + l.x) + 2 + x, u(header_rows + l.y) + 1 + y));
    r.sidebar_rect = Some(sidebar_rect);
    if let Some(a) = r.activity_rect.as_mut() {
        a.y += header_rows;
    }
    r.workspace_rows = if s.workspaces.is_some() {
        hits.ws
            .iter()
            .enumerate()
            .filter_map(|(i, k)| k.clone().map(|k| (header_rows + 1 + i as i64, k)))
            .collect()
    } else {
        vec![]
    };
    r.workspace_rect = if s.workspaces.is_some_and(|l| !l.is_empty()) && hits.ws_h > 0 {
        Some(Rect {
            x: 0,
            y: header_rows,
            w: side_w,
            h: hits.ws_h,
        })
    } else {
        None
    };
    r.session_rows = hits
        .session
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            id.clone()
                .map(|id| (header_rows + hits.ws_h + 1 + i as i64, id))
        })
        .collect();

    let mut out = vec![header(s, w, frame, now)];
    if rule {
        out.push(header_rule(w));
    }
    out.extend(body);
    out.push(footer(s, w));
    out.truncate(u(h));
    let lines: Vec<String> = out.iter().map(|l| fit(l, u(w))).collect();
    Frame {
        lines,
        cols: u(w),
        rows: u(h),
        rects,
        chat_body,
    }
}

/// One styled terminal cell of a painted row.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// The grapheme cluster drawn in the cell (wide characters take the following cell too).
    pub text: String,
    pub style: synthesis_tui_base::sgr::SgrState,
}

/// Parses a painted row (SGR strings, as in [`Frame::lines`]) into styled grapheme cells.
pub fn parse_cells(line: &str) -> Vec<Cell> {
    use synthesis_tui_base::sgr::{SgrState, apply_sgr};
    use unicode_segmentation::UnicodeSegmentation;
    let mut st = SgrState::default();
    let mut out = vec![];
    let mut text = String::new();
    let flush = |text: &mut String, st: &SgrState, out: &mut Vec<Cell>| {
        for g in text.graphemes(true) {
            out.push(Cell {
                text: g.to_string(),
                style: *st,
            });
        }
        text.clear();
    };
    let b = line.as_bytes();
    let mut i = 0;
    while i < line.len() {
        if let Some(n) = synthesis_tui_base::text::csi_len(line, i) {
            if b[i + n - 1] == b'm' {
                flush(&mut text, &st, &mut out);
                apply_sgr(&mut st, &line[i + 2..i + n - 1]);
            }
            i += n;
            continue;
        }
        let ch = line[i..].chars().next().unwrap();
        text.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut text, &st, &mut out);
    out
}

impl Frame {
    /// The frame as rows of styled cells.
    pub fn cells(&self) -> Vec<Vec<Cell>> {
        self.lines.iter().map(|l| parse_cells(l)).collect()
    }
}
