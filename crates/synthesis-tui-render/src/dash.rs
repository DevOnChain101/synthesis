//! Dashboard frame: the header, the panels of the registry (`layout.rs`) drawn as borderless
//! sections, the chat pane, the Tabs-mode strip and the bottom bar (spec docs/specs/tui-design.md
//! §5, §6 and §14). Pure: `render_frame` takes the state, the size, the animation `frame` counter
//! and `now` (ms) and returns the painted rows. Where the rectangles go is `layout::plan`.

use crate::agent::{self, Agent, AgentOpts, EditorView};
use crate::centre::{self, Centre, ProjectData};
use crate::dialogs::mode_name;
use crate::folders::{folder_with_parent, shorten_left};
use crate::layout::{self, Mode, PanelId, Panels, Plan, View};
use crate::model::{Phase, Session};
use crate::modelname::model_label;
use crate::panels;
use crate::theme::*;
use crate::util::{basename, fmt_dur, fmt_num, pad_end, short, to_fixed, u16len};
use synthesis_tui_base::text::{strip_ansi, width};

pub use crate::layout::{MIN_COLS, MIN_ROWS, Rect};

pub(crate) type Lines = Vec<String>;

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
    /// Which panels are on (`/config`).
    pub panels: Panels,
    /// The view of Tabs mode (terminals under 100 columns).
    pub tab: View,
    /// `/config` is open: it takes the place of the chat pane.
    pub config: Option<crate::config_view::ConfigView>,
    /// Mission centre tab (chat, project, diff, terminal).
    pub centre: Centre,
    pub project: ProjectData,
}

/// What the host knows that the event stream does not say. `None` / empty means "not told": the
/// panels then show "no data yet", never a guess.
#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    /// `enforce`, `best-effort` or `off`.
    pub sandbox_mode: Option<String>,
    pub sandbox_backend: String,
    /// The memory store (instruction files) is available.
    pub memory: Option<bool>,
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
    pub host: &'a HostInfo,
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
    /// `/config`: where it is drawn and the screen row of each listed row (index into its
    /// visible rows), for clicks.
    pub config_rect: Option<Rect>,
    pub config_hits: Vec<(i64, usize)>,
    /// Tabs mode: the strip row and the columns `[x0, x1)` of each tab, for clicks.
    pub tab_row: Option<i64>,
    pub tab_hits: Vec<(i64, i64, View)>,
    /// Mission centre strip row and the columns `[x0, x1)` of each tab, for clicks.
    pub centre_row: Option<i64>,
    pub centre_hits: Vec<(i64, i64, Centre)>,
}

pub struct Frame {
    pub lines: Vec<String>,
    pub cols: usize,
    pub rows: usize,
    pub rects: Rects,
    /// The chat rows behind the frame (mouse selection / copy read them).
    pub chat_body: Option<agent::Body>,
    /// Which layout the size got (Tab cycles the views in Tabs mode).
    pub mode: Mode,
    pub tabs: Vec<View>,
    pub view: View,
}

// ---------------------------------------------------------------- helpers

pub(crate) fn col_accent() -> Rgb {
    t().accent
}
pub(crate) fn col_ok() -> Rgb {
    t().ok
}
pub(crate) fn col_warn() -> Rgb {
    t().warn
}
pub(crate) fn col_bad() -> Rgb {
    t().err
}
pub(crate) fn col_muted() -> Rgb {
    t().label
}
pub(crate) fn col_subtle() -> Rgb {
    t().muted
}
pub(crate) fn col_text() -> Rgb {
    t().body
}
pub(crate) fn col_white() -> Rgb {
    t().strong
}
pub(crate) fn col_tool() -> Rgb {
    t().info
}
pub(crate) fn col_agent() -> Rgb {
    t().purple()
}
pub(crate) fn col_skill() -> Rgb {
    t().ok
}
pub(crate) fn col_track() -> Rgb {
    t().track()
}

pub(crate) fn u(x: i64) -> usize {
    x.max(0) as usize
}

pub fn level_color(p: f64) -> Rgb {
    if p >= 85.0 {
        col_bad()
    } else if p >= 60.0 {
        col_warn()
    } else {
        col_ok()
    }
}

/// Filled `█` and empty `░` cells; `p` of `None` counts as 0. `frame` and `animate` are kept for
/// the callers: the bar no longer shimmers (spec §2, quiet motion).
pub fn bar(p: Option<f64>, w: usize, _frame: usize, _animate: bool) -> String {
    let pctv = p.filter(|v| !v.is_nan()).unwrap_or(0.0).clamp(0.0, 100.0);
    let filled = js_round(pctv / 100.0 * w as f64) as usize;
    format!(
        "{}{}",
        paint(level_color(pctv), &"█".repeat(filled.min(w))),
        paint(col_track(), &"░".repeat(w - filled.min(w)))
    )
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
/// A borderless section (spec §3 "C2"): a label row (UPPERCASE label, `right` flush right, the
/// label in the accent colour when `active`) and `h - 1` content rows. Each row has one column of
/// padding on both sides, so content may use `w - 2` cells. No frame, no heavy line.
pub(crate) fn section(
    label: &str,
    right: &str,
    lines: Lines,
    w: i64,
    h: i64,
    active: bool,
    valign: VAlign,
) -> Lines {
    if h <= 0 {
        return vec![];
    }
    let th = t();
    let label = label.to_uppercase();
    // the right-hand text is secondary: it goes when it would cut the label
    let right = if width(&label) as i64 + width(right) as i64 + 3 > w {
        ""
    } else {
        right
    };
    let rw = width(right) as i64;
    let max_label = u((w - 3 - rw - i64::from(rw > 0)).max(1));
    let lab = if width(&label) > max_label {
        fit(&label, max_label)
    } else {
        label
    };
    let gap = u(w - 2 - width(&lab) as i64 - rw).max(1);
    let head = format!(
        " {}{}{} ",
        paint(if active { th.accent } else { th.label }, &lab),
        " ".repeat(gap),
        right
    );
    let mut rows = vec![fit(&head, u(w))];
    let row_count = u(h - 1);
    let content = match valign {
        VAlign::Center => valign_lines(lines, row_count),
        VAlign::Evenly => valign_evenly(lines, row_count),
        VAlign::Top => lines,
    };
    for i in 0..row_count {
        let c = content.get(i).map_or("", String::as_str);
        rows.push(fit(&format!(" {c}"), u(w)));
    }
    rows
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
fn civil(ms: i64, tz: i32) -> Civil {
    let days = (ms + i64::from(tz) * 60_000).div_euclid(86_400_000);
    let (_, month, day) = civil_from_days(days);
    Civil {
        month,
        day,
        dow: (days + 4).rem_euclid(7) as usize,
    }
}

// ---------------------------------------------------------------- panels

pub(crate) fn open_todos(s: &DashState) -> usize {
    s.session
        .todos
        .iter()
        .filter(|t| t.status != "completed")
        .count()
}

pub(crate) fn context_size(session: &Session, tap: Option<&Tap>, agent_window: Option<f64>) -> f64 {
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

pub(crate) fn ctx_pct_of(s: &DashState) -> f64 {
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

pub(crate) fn phase_label(p: &Phase) -> (&'static str, Rgb) {
    match p {
        Phase::Tool => ("RUNNING TOOL", col_tool()),
        Phase::Agents => ("WAITING ON AGENTS", col_agent()),
        Phase::Thinking => ("THINKING", col_accent()),
        Phase::Idle => ("IDLE", col_muted()),
    }
}

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
    let inner = w - 2;
    let mut lines: Lines = vec![];
    let mut hits: Vec<Option<String>> = vec![];
    let active_at = wlist
        .iter()
        .position(|x| x.key == s.active_workspace)
        .unwrap_or(0);
    let mut capacity = (h - 1).max(1);
    let mut win = sessions_window(wlist.len(), active_at, capacity * 2);
    let ind = i64::from(win.2 > 0) + i64::from(win.3 > 0);
    if ind > 0 {
        capacity = (capacity - ind).max(0);
        win = sessions_window(wlist.len(), active_at, capacity * 2);
    }
    let (start, end, above, below) = win;
    let blink_on = (frame / 4).is_multiple_of(2);
    if above > 0 {
        lines.push(paint(col_subtle(), &format!("   ↑ {above} more")));
        hits.push(None);
    }
    for (i, ws) in wlist.iter().enumerate().take(end).skip(start) {
        let on = i == active_at;
        let icon = if ws.waiting {
            paint(col_bad(), if blink_on { "●" } else { "○" })
        } else if ws.busy {
            paint(col_ok(), spin(frame + i))
        } else {
            paint(if ws.count > 0 { col_ok() } else { col_muted() }, "●")
        };
        let count = if ws.waiting {
            format!("! {}", ws.count)
        } else {
            ws.count.to_string()
        };
        let name_w = (inner - 4 - count.len() as i64).max(4);
        let name = short(&ws.name, u(name_w));
        let title = if ws.waiting {
            paint(if blink_on { col_bad() } else { col_muted() }, &name)
        } else if on {
            paint(col_accent(), &name)
        } else {
            paint(col_text(), &name)
        };
        let left = format!(
            "{}{icon} {title}",
            if on {
                paint(col_accent(), "▌")
            } else {
                " ".into()
            }
        );
        let gap = " ".repeat(u((inner - width(&left) as i64 - count.len() as i64).max(1)));
        lines.push(format!(
            "{left}{gap}{}",
            paint(
                if ws.waiting {
                    if blink_on { col_bad() } else { col_muted() }
                } else {
                    col_muted()
                },
                &count
            )
        ));
        hits.push(Some(ws.key.clone()));
    }
    if below > 0 {
        lines.push(paint(col_subtle(), &format!("   ↓ {below} more")));
        hits.push(None);
    }
    let waiting_n = wlist.iter().filter(|x| x.waiting).count();
    let wait_tag = if waiting_n > 0 {
        paint(col_bad(), &format!(" !{waiting_n}"))
    } else {
        String::new()
    };
    let right = format!("{wait_tag}{}", paint(col_white(), &wlist.len().to_string()));
    (
        section(
            "Workspace",
            &right,
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
        "acceptEdits" => ("edits", col_agent()),
        "plan" => ("plan", col_skill()),
        "auto" => ("auto", col_warn()),
        "dontAsk" => ("dontAsk", col_tool()),
        "bypassPermissions" => ("bypass", col_bad()),
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
    let inner = w - 2;
    let mut lines: Lines = vec![];
    let mut capacity = (h - 1).max(1);
    let mut win = sessions_window(sessions.len(), index, capacity);
    let ind = i64::from(win.2 > 0) + i64::from(win.3 > 0);
    if ind > 0 {
        capacity = (capacity - ind).max(0);
        win = sessions_window(sessions.len(), index, capacity);
    }
    let (start, end, above, below) = win;
    session_hits.clear();
    if above > 0 {
        lines.push(paint(col_subtle(), &format!("   ↑ {above} more")));
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
            paint(col_bad(), if blink_on { "●" } else { "○" })
        } else if busy {
            paint(col_ok(), spin(frame + i))
        } else if live.is_some() {
            paint(col_ok(), "●")
        } else {
            paint(col_muted(), "○")
        };
        let cursor = if sel {
            paint(col_accent(), if s.workspaces.is_some() { "▌" } else { "►" })
        } else {
            " ".into()
        };
        let tag = if is_agent {
            paint(t().accent_light, " ⌘")
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
            paint(if blink_on { col_bad() } else { col_muted() }, &name)
        } else if sel {
            paint(col_accent(), &name)
        } else {
            paint(
                if live.is_some() {
                    col_text()
                } else {
                    col_muted()
                },
                &name,
            )
        };
        let left = format!(
            "{cursor}{icon} {title}{}{tag}",
            mt.map_or(String::new(), |(_, c)| paint(c, &mode_s))
        );
        let ctx_col = if waiting {
            if blink_on { col_bad() } else { col_muted() }
        } else {
            ctx_pct.map_or(col_muted(), level_color)
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
            if sel { col_text() } else { col_subtle() },
            &short(&format!("   {folder}"), u(inner)),
        ));
        session_hits.push(Some(sr.id.clone()));
        session_hits.push(Some(sr.id.clone()));
    }
    if below > 0 {
        lines.push(paint(col_subtle(), &format!("   ↓ {below} more")));
        session_hits.push(None);
    }
    if sessions.is_empty() {
        lines.push(paint(col_muted(), "no running sessions"));
        session_hits.push(None);
    }
    let waiting_n = sessions
        .iter()
        .filter(|x| x.waiting || x.live.as_ref().is_some_and(|l| l.status == "waiting"))
        .count();
    let wait_tag = if waiting_n > 0 {
        paint(col_bad(), &format!(" !{waiting_n}"))
    } else {
        String::new()
    };
    section(
        "Running sessions",
        &format!(
            "{wait_tag}{}",
            paint(col_white(), &sessions.len().to_string())
        ),
        lines,
        w,
        h,
        s.focus == Focus::Dashboard,
        VAlign::Top,
    )
}

pub(crate) fn model_of(s: &DashState) -> Option<String> {
    s.agent
        .and_then(|a| model_label(&a.model, &a.active_model, a.config.as_ref()))
}

fn session_row(k: &str, v: &str, c: Rgb, inner: i64) -> String {
    format!(
        "{}{}",
        paint(col_muted(), &pad_end(k, 7)),
        paint(c, &short(v, u((inner - 7).max(4))))
    )
}

pub(crate) fn mode_line(a: &Agent) -> (String, Rgb) {
    let hint = agent::turn_mode_hint(a);
    (
        format!(
            "{}{}",
            mode_name(Some(&a.mode)),
            hint.map_or(String::new(), |h| format!(" (turn: {h})"))
        ),
        if !a.mode.is_empty() && a.mode != "default" {
            col_warn()
        } else {
            col_text()
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
        if working { col_ok() } else { col_muted() },
        inner,
    )
}

pub(crate) fn session_info_panel(s: &DashState, w: i64, h: i64, frame: usize, now: i64) -> Lines {
    let session = s.session;
    let inner = w - 2;
    let mut lines = vec![];
    let model = model_of(s)
        .or(Some(session.model.clone()).filter(|m| !m.is_empty()))
        .unwrap_or_else(|| waiting("loading model…", frame));
    lines.push(session_row("model", &model, col_accent(), inner));
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
            col_text(),
            inner,
        ));
        if let Some(g) = &a.goal {
            lines.push(goal_row(g, inner));
        }
        if let Some(cw) = a.context_window.filter(|c| *c != 0.0) {
            lines.push(session_row(
                "window",
                &format!("{} tokens", fmt_num(Some(cw))),
                col_text(),
                inner,
            ));
        }
    }
    if !session.git_branch.is_empty() {
        lines.push(session_row(
            "branch",
            &session.git_branch,
            col_skill(),
            inner,
        ));
    }
    if !session.cwd.is_empty() {
        lines.push(session_row(
            "cwd",
            &basename(&session.cwd),
            col_text(),
            inner,
        ));
    }
    if let Some(st) = session.started_at.filter(|t| *t != 0) {
        lines.push(session_row(
            "uptime",
            &fmt_dur(Some((now - st) as f64)),
            col_text(),
            inner,
        ));
    }
    let tools: u32 = session.tool_counts.iter().map(|t| t.1).sum();
    lines.push(session_row(
        "turns",
        &format!("{} prompts · {tools} tools", session.prompts),
        col_text(),
        inner,
    ));
    if session.errors > 0 {
        lines.push(session_row(
            "errors",
            &session.errors.to_string(),
            col_bad(),
            inner,
        ));
    }
    lines.truncate(u(h - 1));
    let live = if s.agent.is_some_and(|a| a.status != "idle") {
        paint(col_ok(), "running")
    } else {
        String::new()
    };
    section("Session", &live, lines, w, h, false, VAlign::Top)
}

pub(crate) struct McpRow {
    pub name: String,
    pub transport: String,
    pub url: String,
    pub state: String,
    pub tool_count: u32,
    pub error: Option<String>,
}

pub(crate) fn mcp_rows_of(s: &DashState) -> (bool, Vec<McpRow>) {
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

/// The MCP server rows (no chrome): `(enabled count, rows)`.
pub(crate) fn mcp_lines(s: &DashState, w: i64, room: usize, frame: usize) -> (usize, Lines) {
    let (live, rows) = mcp_rows_of(s);
    let enabled: Vec<&McpRow> = rows.iter().filter(|r| r.state != "disabled").collect();
    let inner = w - 2;
    let mut lines: Lines = vec![];
    for r in &enabled {
        if lines.len() >= room {
            break;
        }
        let (dot, right, c);
        if live {
            dot = match r.state.as_str() {
                "connecting" => paint(col_warn(), spin(frame + lines.len())),
                "failed" => paint(col_bad(), "●"),
                "connected" => paint(col_ok(), "●"),
                _ => paint(col_muted(), "○"),
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
                col_bad()
            } else if r.state == "connected" {
                col_white()
            } else {
                col_text()
            };
        } else {
            let comp = s.session.components.get(&format!("mcp:{}", r.name));
            let busy = comp.is_some_and(|c| c.open > 0);
            let errored = comp.is_some_and(|c| c.error);
            let used = comp.is_some_and(|c| c.count > 0);
            dot = if busy {
                paint(col_ok(), spin(frame + lines.len()))
            } else if errored {
                paint(col_bad(), "●")
            } else {
                paint(col_ok(), "●")
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
                col_bad()
            } else if used {
                col_white()
            } else {
                col_text()
            };
        }
        let name_w = (inner - 2 - u16len(&right) as i64 - 1).max(4);
        let left = format!("{dot} {}", paint(c, &short(&r.name, u(name_w))));
        lines.push(format!(
            "{left}{}{}",
            " ".repeat(u(
                (inner - width(&left) as i64 - u16len(&right) as i64).max(1)
            )),
            paint(col_muted(), &right)
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
                paint(col_muted(), "○"),
                paint(col_muted(), &short(&r.name, u(name_w)))
            );
            lines.push(format!(
                "{left}{}{}",
                " ".repeat(u(
                    (inner - width(&left) as i64 - u16len(&right) as i64).max(1)
                )),
                paint(col_subtle(), &right)
            ));
            shown_disabled += 1;
        }
    }
    if !live && lines.len() < room {
        lines.push(paint(
            col_subtle(),
            "configured · synthesis has no MCP client yet",
        ));
    }
    if shown_disabled < disabled.len() && lines.len() < room {
        lines.push(paint(
            col_subtle(),
            &format!("+{} disabled", disabled.len() - shown_disabled),
        ));
    }
    (enabled.len(), lines)
}

#[derive(Default)]
struct Hits {
    session: Vec<Option<String>>,
}

// ---------------------------------------------------------------- agent pane in the frame

fn chat_agent<'a>(s: &DashState<'a>) -> Option<&'a Agent> {
    let a = s.agent?;
    match s.chat_agent {
        Some(i) => a.subagents.get(i).or(Some(a)),
        None => Some(a),
    }
}

/// The chat pane: a label row and the conversation behind a 2-column gutter. `docked`: a viewed
/// sub-agent's transcript is in the Sub-agents panel, so the pane does not repeat it.
pub(crate) fn agent_panel(
    s: &DashState,
    w: i64,
    h: i64,
    frame: usize,
    now: i64,
    docked: bool,
    out: &mut Rects,
    body_out: &mut Option<agent::Body>,
) -> Lines {
    let (Some(main), Some(chat)) = (s.agent, chat_agent(s)) else {
        let note = paint(col_muted(), "no conversation attached");
        return section(
            "Synthesis",
            "",
            vec![format!(" {note}")],
            w,
            h,
            false,
            VAlign::Top,
        );
    };
    let focused = s.focus == Focus::Agent;
    let subagent_view = !std::ptr::eq(chat, main);
    let mut opts = s.view.opts.clone();
    opts.focused = Some(focused);
    opts.docked = docked;
    opts.config = main.config.clone();
    opts.tz_offset_min = s.tz_offset_min;
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
        u(w - 4),
        u(h - 1),
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
            col_warn(),
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
            col_warn(),
            &format!(
                "{} {what}{}",
                glyphs().attention,
                if focused { "" } else { " · tab" }
            ),
        )
    } else if busy {
        paint(col_ok(), &format!("{} working", spin(frame)))
    } else {
        paint(col_muted(), if focused { "ready" } else { "tab to type" })
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
    let label = if subagent_view {
        format!("Synthesis @{}", chat.display_name())
    } else {
        "Synthesis".to_string()
    };
    let right = format!(
        "{}  {tag}",
        paint(col_subtle(), &format!("{where_}{sub_route}"))
    );
    let lines = res.lines.into_iter().map(|l| format!(" {l}")).collect();
    section(&label, &right, lines, w, h, focused || busy, VAlign::Top)
}

pub fn heat_level(v: Option<f64>, max: Option<f64>) -> usize {
    let (Some(v), Some(max)) = (v, max) else {
        return 0;
    };
    if v.is_nan() || max.is_nan() || v <= 0.0 || max <= 0.0 || !v.is_finite() || !max.is_finite() {
        return 0;
    }
    4.min(1 + 3.min(((v / max) * (4.0 - 1e-9)).floor() as usize))
}

// ---------------------------------------------------------------- header / footer

/// The brand mark (braille ring): left cell accent, right cell pearl. Empty in the ASCII set.
pub fn mark() -> String {
    let m: Vec<char> = glyphs().mark.chars().collect();
    match m[..] {
        [a, b] => format!(
            "{}{}",
            paint(t().accent, &a.to_string()),
            paint(t().pearl, &b.to_string())
        ),
        _ => String::new(),
    }
}

/// Wordmark by width (spec §4): spaced from 80 columns, plain from 60, none below (the mark only).
fn wordmarks(w: i64) -> Vec<&'static str> {
    if w >= 80 {
        vec!["S Y N T H E S I S", "SYNTHESIS", ""]
    } else if w >= 60 {
        vec!["SYNTHESIS", ""]
    } else {
        vec![""]
    }
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// One header chip. `drop`: the highest number goes first, 0 never (the clock). A chip with a
/// `short` form shrinks to it before it disappears (the health word keeps its dot).
struct Chip {
    drop: u8,
    left: bool,
    full: String,
    short: Option<String>,
}

fn header(s: &DashState, w: i64, _frame: usize, now: i64) -> String {
    let session = s.session;
    let (th, g) = (t(), glyphs());
    let sep = format!(" {} ", paint(th.border, g.vbar));
    let live = s.sessions.iter().filter(|x| x.live.is_some()).count();
    let busy = s
        .sessions
        .iter()
        .filter(|x| x.live.is_some() && !x.phase.is_empty() && x.phase != "idle")
        .count();
    let waiting = s.agent.map_or(0, |a| a.pending.len());
    let project = {
        let b = basename(&session.cwd);
        if b.is_empty() {
            session.project.clone()
        } else {
            b
        }
    };
    let labelled = |k: &str, v: &str| format!("{} {}", paint(th.muted, k), paint(th.body, v));
    let sid: String = s
        .agent_session_id
        .unwrap_or(&session.id)
        .chars()
        .take(8)
        .collect();
    let ctx = {
        let used = session.context_used;
        let max = context_size(
            session,
            s.usage.tap.as_ref(),
            s.agent.and_then(|a| a.context_window),
        );
        (used > 0).then(|| format!("{}/{}", fmt_num(Some(used as f64)), fmt_num(Some(max))))
    };
    let (hl, hw) = panels::overall_health(s);
    let hc = panels::level_colour(&hl);
    let health_full = format!(
        "{} {}",
        paint(hc, g.live),
        paint(hc, &format!("system {hw}"))
    );
    let c = civil(now, s.tz_offset_min);
    let clock = fmt_time(now, s.tz_offset_min);
    let mut chips: Vec<Chip> = vec![];
    let mut add = |drop: u8, left: bool, full: String, short: Option<String>| {
        if !full.is_empty() {
            chips.push(Chip {
                drop,
                left,
                full,
                short,
            });
        }
    };
    add(
        4,
        true,
        format!(
            "{} {}{}",
            if live > 0 {
                paint(th.ok, g.live)
            } else {
                paint(th.muted, g.idle)
            },
            paint(th.strong, &format!("{live} live")),
            if busy > 0 {
                paint(th.muted, &format!(" {} {busy} busy", g.dot))
            } else {
                String::new()
            }
        ),
        None,
    );
    add(
        1,
        true,
        if waiting > 0 {
            paint(th.warn, &format!("{} {waiting} waiting", g.attention))
        } else {
            String::new()
        },
        None,
    );
    add(
        2,
        true,
        if project.is_empty() {
            String::new()
        } else {
            labelled("workspace", &project)
        },
        None,
    );
    add(
        5,
        true,
        if sid.is_empty() {
            String::new()
        } else {
            labelled("session", &sid)
        },
        None,
    );
    add(
        3,
        true,
        panels::model_chip(s).map_or(String::new(), |m| labelled("model", &m)),
        None,
    );
    add(
        6,
        true,
        ctx.map_or(String::new(), |c| labelled("context", &c)),
        None,
    );
    add(
        7,
        false,
        s.usage
            .tap
            .as_ref()
            .and_then(|t| t.cost_usd)
            .map_or(String::new(), |c| {
                paint(th.muted, &format!("${}", to_fixed(c, 2)))
            }),
        None,
    );
    add(8, false, health_full, Some(paint(hc, g.live)));
    if w >= 140 {
        add(
            9,
            false,
            paint(
                th.muted,
                &format!("{} {} {:>2}", DAYS[c.dow], MONTHS[c.month], c.day),
            ),
            None,
        );
    }
    add(0, false, paint(th.body, &clock), None);
    let words = wordmarks(w);
    for (li, word) in words.iter().enumerate() {
        let is_last = li == words.len() - 1;
        let brand = {
            let m = mark();
            let word = if word.is_empty() && m.is_empty() {
                "SYNTH"
            } else {
                word
            };
            match (m.is_empty(), word.is_empty()) {
                (_, true) => format!(" {m}"),
                (true, _) => format!(" {}", paint(th.accent_light, word)),
                _ => format!(" {m}  {}", paint(th.accent_light, word)),
            }
        };
        // 0 full, 1 short form, 2 gone
        let mut state = vec![0u8; chips.len()];
        let build = |st: &[u8]| {
            let pick = |left: bool| {
                chips
                    .iter()
                    .zip(st)
                    .filter(|(c, k)| c.left == left && **k < 2)
                    .map(|(c, k)| {
                        if *k == 1 {
                            c.short.as_deref().unwrap_or(&c.full)
                        } else {
                            c.full.as_str()
                        }
                    })
                    .collect::<Vec<_>>()
            };
            let lc = pick(true);
            let left = if lc.is_empty() {
                brand.clone()
            } else {
                format!("{brand}   {}", lc.join(&sep))
            };
            let right = pick(false).join(&sep);
            let need = width(&left) as i64 + width(&right) as i64 + 2;
            (left, right, need)
        };
        let mut row = build(&state);
        while row.2 > w {
            let victim = (0..chips.len())
                .filter(|i| state[*i] < 2 && (chips[*i].drop > 0 || is_last))
                .max_by_key(|i| (chips[*i].drop, std::cmp::Reverse(*i)));
            let Some(i) = victim else { break };
            state[i] = if state[i] == 0 && chips[i].short.is_some() {
                1
            } else {
                2
            };
            row = build(&state);
        }
        if row.2 <= w || is_last {
            return fit(
                &format!(
                    "{}{}{}",
                    row.0,
                    " ".repeat(u(w - width(&row.0) as i64 - width(&row.1) as i64 - 1).max(1)),
                    row.1
                ),
                u(w),
            );
        }
    }
    fit("", u(w))
}

fn header_rule(w: i64) -> String {
    paint(t().border, &"─".repeat(u(w)))
}

/// Key hints on the left (whole hints, the later ones dropped first), `project | branch | n files
/// changed` on the right (spec §14.3), as many of its parts as fit.
fn footer(s: &DashState, w: i64) -> String {
    let agent_keys: Vec<(&str, &str)> = crate::dialogs::FOOTER_KEYS.to_vec();
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
        agent_keys
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
    let mut items: Vec<(u8, String)> = keys
        .iter()
        .enumerate()
        .map(|(i, (k, d))| {
            (
                (i as u8).saturating_add(1).min(200),
                format!("{} {}", paint(col_white(), k), paint(col_muted(), d)),
            )
        })
        .collect();
    if s.follow {
        items.push((0, paint(col_ok(), "[following newest]")));
    }
    let project = {
        let b = basename(&s.session.cwd);
        if b.is_empty() {
            s.session.project.clone()
        } else {
            b
        }
    };
    let changed = panels::counters(s.session).files_edited;
    let sep = format!(" {} ", paint(t().border, glyphs().vbar));
    // (drop priority, part): the branch and the count go first
    let mut parts: Vec<(u8, String)> = vec![
        (1, paint(col_text(), &project)),
        (
            3,
            paint(
                col_muted(),
                &format!("{} {}", glyphs().branch, s.session.git_branch),
            ),
        ),
        (
            2,
            paint(
                col_muted(),
                &format!(
                    "{changed} file{} changed",
                    if changed == 1 { "" } else { "s" }
                ),
            ),
        ),
    ];
    parts.retain(|p| {
        !strip_ansi(&p.1).trim().is_empty() && strip_ansi(&p.1).trim() != glyphs().branch
    });
    let right_for = |parts: &[(u8, String)]| {
        parts
            .iter()
            .map(|p| p.1.as_str())
            .collect::<Vec<_>>()
            .join(&sep)
    };
    let wr = w;
    // the right block gets what the hints leave: at least 20 cells for them
    while !parts.is_empty() && width(&right_for(&parts)) as i64 + 22 > wr {
        let drop = (0..parts.len()).max_by_key(|i| (parts[*i].0, *i)).unwrap();
        parts.remove(drop);
    }
    let right = right_for(&parts);
    let rw = width(&right) as i64;
    let left = fit_items(&items, &sep, u((wr - rw - 2).max(1)));
    let left = format!(" {}", left.trim_end());
    let pad = u(wr - width(&left) as i64 - rw - 1).max(1);
    fit(&format!("{left}{}{right}", " ".repeat(pad)), u(wr))
}

/// Tabs mode: `Chat · Sessions n · Agents n · Panels n`, the shown view marked with the focus bar.
/// Also the columns `[x0, x1)` each tab covers.
fn tab_strip(s: &DashState, pl: &Plan, w: i64) -> (String, Vec<(i64, i64, View)>) {
    let th = t();
    let on = &s.view.panels;
    let n_panels = [
        PanelId::Session,
        PanelId::ToolsSkills,
        PanelId::RecentActivity,
        PanelId::SessionStats,
        PanelId::Topology,
        PanelId::UsageMetrics,
        PanelId::SystemHealth,
    ]
    .iter()
    .filter(|id| on.is_on(**id))
    .count();
    let n_agents = s.agent.map_or(0, |a| a.subagents.len());
    let mut x = 1;
    let mut hits = vec![];
    let items: Vec<String> = pl
        .tabs
        .iter()
        .map(|v| {
            let n = match v {
                View::Sessions => running_sessions(s).0.len(),
                View::Agents => n_agents,
                View::Panels => n_panels,
                View::Chat => 0,
            };
            let text = if n > 0 {
                format!("{} {n}", v.label())
            } else {
                v.label().to_string()
            };
            let item = if *v == pl.view {
                format!(
                    "{}{}",
                    paint(th.pearl, glyphs().focus),
                    paint(th.strong, &text)
                )
            } else {
                format!(" {}", paint(th.label, &text))
            };
            hits.push((x, x + width(&item) as i64, *v));
            x += width(&item) as i64 + 4;
            item
        })
        .collect();
    let left = format!(" {}", items.join("    "));
    let hint = paint(th.muted, "tab switches");
    let gap = w - width(&left) as i64 - width(&hint) as i64 - 1;
    let row = if gap >= 2 {
        fit(&format!("{left}{}{hint}", " ".repeat(u(gap))), u(w))
    } else {
        fit(&left, u(w))
    };
    (row, hits)
}

// ---------------------------------------------------------------- frame

/// Painted segments by row; panels never overlap, so a row is its segments in x order with
/// blanks between.
struct Canvas {
    w: i64,
    rows: Vec<Vec<(i64, i64, String)>>,
}

/// `s` on the card surface colour: the colour is set again after every reset inside `s`.
fn on_surface(s: &str) -> String {
    let th = t();
    if !th.paint_bg {
        return s.to_string();
    }
    let b = bg(th.surface);
    format!(
        "{b}{}{RESET}",
        s.replace(RESET, &format!("{RESET}{b}"))
            .replace("\x1b[49m", &format!("\x1b[49m{b}"))
    )
}

/// The thin card border: the token `border` pulled towards the background when the theme paints
/// one, as is in the others.
fn card_border() -> Rgb {
    let th = t();
    if th.paint_bg {
        lerp(th.border, th.bg_ref, 0.5)
    } else {
        th.border
    }
}

/// The longest run of spaces of a label row becomes a rule: `LABEL        2` -> `LABEL ─────── 2`.
fn label_to_rule(row: &str) -> String {
    let b = row.as_bytes();
    let (mut best, mut i) = ((0, 0), 0);
    while i < b.len() {
        if b[i] == b' ' {
            let j = (i..b.len()).find(|k| b[*k] != b' ').unwrap_or(b.len());
            if j - i > best.1 - best.0 {
                best = (i, j);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    let (i, j) = best;
    if j - i < 3 {
        return row.to_string();
    }
    let rule = glyphs().box_h;
    let n = j - i;
    let (lead, trail) = (" ", if j == b.len() { "" } else { " " });
    let run = rule.repeat(n - lead.len() - trail.len());
    format!(
        "{}{lead}{}{trail}{}",
        &row[..i],
        paint(card_border(), &run),
        &row[j..]
    )
}

/// Wraps a panel drawn `w - 2` wide and `h - 1` tall (its label row, then the content) into a thin
/// rounded card `w` x `h`: the label row becomes the top border, a bottom border closes it.
fn card(lines: Lines, w: i64, h: i64) -> Lines {
    let g = glyphs();
    let edge = |c: &str| paint(card_border(), c);
    let mut it = lines.into_iter();
    let head = it.next().unwrap_or_default();
    let mut out = vec![format!(
        "{}{}{}",
        edge(g.box_tl),
        label_to_rule(&fit(&head, u(w - 2))),
        edge(g.box_tr)
    )];
    for l in it.take(u(h - 2)) {
        out.push(format!(
            "{}{}{}",
            edge(g.box_v),
            fit(&l, u(w - 2)),
            edge(g.box_v)
        ));
    }
    while (out.len() as i64) < h - 1 {
        out.push(format!(
            "{}{}{}",
            edge(g.box_v),
            " ".repeat(u(w - 2)),
            edge(g.box_v)
        ));
    }
    out.push(format!(
        "{}{}{}",
        edge(g.box_bl),
        edge(&g.box_h.repeat(u(w - 2))),
        edge(g.box_br)
    ));
    out.into_iter().map(|l| on_surface(&l)).collect()
}

impl Canvas {
    fn new(w: i64, h: i64) -> Self {
        Canvas {
            w,
            rows: vec![vec![]; u(h)],
        }
    }
    fn put(&mut self, y: i64, x: i64, w: i64, text: String) {
        if y >= 0
            && x + w <= self.w
            && w > 0
            && let Some(r) = self.rows.get_mut(u(y))
        {
            r.push((x, w, text));
        }
    }
    fn paste(&mut self, r: Rect, lines: Lines) {
        for (i, l) in lines.into_iter().take(u(r.h)).enumerate() {
            self.put(r.y + i as i64, r.x, r.w, fit(&l, u(r.w)));
        }
    }
    fn into_lines(self) -> Lines {
        let w = self.w;
        self.rows
            .into_iter()
            .map(|mut r| {
                r.sort_by_key(|s| s.0);
                let mut out = String::new();
                let mut x = 0;
                for (sx, sw, text) in r {
                    if sx < x {
                        continue; // overlap: the earlier segment wins
                    }
                    out.push_str(&" ".repeat(u(sx - x)));
                    out.push_str(&text);
                    x = sx + sw;
                }
                out.push_str(&" ".repeat(u(w - x)));
                out
            })
            .collect()
    }
}

pub(crate) fn running_count(s: &DashState) -> usize {
    running_sessions(s).0.len()
}

fn viewed_subagent<'a>(s: &DashState<'a>) -> Option<&'a Agent> {
    let a = s.agent?;
    s.chat_agent.and_then(|i| a.subagents.get(i))
}

/// Renders the dashboard into `rows_total` lines of exactly `cols` cells (at least 40x11). The
/// rectangles come from `layout::plan`; this draws each panel into its rectangle.
pub fn render_frame(
    s: &DashState,
    cols: usize,
    rows_total: usize,
    frame: usize,
    now: i64,
) -> Frame {
    let w = MIN_COLS.max(cols as i64);
    let h = MIN_ROWS.max(rows_total as i64);
    let pl = layout::plan(w, h, &s.view.panels, s.view.tab, &panels::demand(s));
    let mut canvas = Canvas::new(w, h);
    let mut rects = Rects::default();
    let mut hits = Hits::default();
    let mut chat_body = None;
    canvas.put(0, 0, w, header(s, w, frame, now));
    if pl.header_rows == 2 {
        canvas.put(1, 0, w, header_rule(w));
    }
    if let Some(y) = pl.tab_row {
        let (row, hits) = tab_strip(s, &pl, w);
        canvas.put(y, 0, w, row);
        rects.tab_row = Some(y);
        rects.tab_hits = hits;
    }
    if pl.mode == Mode::Columns {
        rects.sidebar_rect = Some(pl.left);
    }
    let fallback_ws = [Workspace {
        key: String::new(),
        name: basename(&s.session.cwd),
        cwd: s.session.cwd.clone(),
        count: s.sessions.len() as u32,
        waiting: false,
        busy: false,
    }];
    if let Some(r) = pl.strip {
        canvas.put(r.y, r.x, r.w, panels::status_line(s, r.w, frame, now));
    }
    for p in &pl.panels {
        let r = p.rect;
        // a panel of 4+ rows and 12+ columns is a card: drawn 2 columns and 1 row smaller, its
        // label row becomes the top border
        let carded = r.h >= 4 && r.w >= 12;
        let (cw, ch) = if carded {
            (r.w - 2, r.h - 1)
        } else {
            (r.w, r.h)
        };
        let lines = match p.id {
            PanelId::Workspace => {
                let list = s
                    .workspaces
                    .filter(|l| !l.is_empty())
                    .unwrap_or(&fallback_ws);
                let (rows, wh) = workspace_panel(s, cw, ch, frame, list);
                if s.workspaces.is_some_and(|l| !l.is_empty()) {
                    rects.workspace_rect = Some(r);
                    rects.workspace_rows = wh
                        .iter()
                        .enumerate()
                        .filter_map(|(i, k)| k.clone().map(|k| (r.y + 1 + i as i64, k)))
                        .collect();
                }
                rows
            }
            PanelId::Running => {
                let rows = sessions_panel(s, cw, ch, frame, &mut hits.session);
                rects.session_rows = hits
                    .session
                    .iter()
                    .enumerate()
                    .filter_map(|(i, id)| id.clone().map(|id| (r.y + 1 + i as i64, id)))
                    .collect();
                rows
            }
            PanelId::Session => session_info_panel(s, cw, ch, frame, now),
            PanelId::ToolsSkills => panels::tools_skills(s, cw, ch, frame),
            PanelId::RecentActivity => {
                rects.activity_rect = Some(r);
                panels::recent_activity(s, cw, ch, now, &mut rects)
            }
            PanelId::SessionStats => panels::session_stats(s, cw, ch, now),
            PanelId::AgentStatus if r.h == 1 => vec![panels::status_line(s, r.w, frame, now)],
            PanelId::AgentStatus => panels::agent_status(s, cw, ch, frame, now),
            PanelId::Context => panels::context(s, cw, ch, frame, now),
            PanelId::Topology => panels::topology(s, cw, ch, now),
            PanelId::UsageMetrics => panels::usage_metrics(s, cw, ch),
            PanelId::SystemHealth => panels::system_health(s, cw, ch),
            PanelId::Subagents => {
                rects.dock_rect = Some(r);
                panels::subagents(s, cw, ch, frame, now, &mut rects)
            }
            PanelId::Chat | PanelId::ProjectTree | PanelId::Diff | PanelId::Terminal => continue,
        };
        if carded {
            canvas.paste(r, card(lines, r.w, r.h));
        } else {
            canvas.paste(r, lines);
        }
    }
    if let (Some(c), Some(cfg)) = (pl.chat, &s.view.config) {
        let note = (pl.mode == Mode::ChatOnly).then_some("panels appear at 60 columns and wider");
        let ctx = crate::config_view::Context {
            starved: &pl.starved,
            note,
        };
        // one column of air on each side, so the frame does not touch the column rules
        let c = if c.w > 24 {
            Rect {
                x: c.x + 1,
                w: c.w - 2,
                ..c
            }
        } else {
            c
        };
        let out = crate::config_view::render(cfg, c.w, c.h, &ctx);
        canvas.paste(c, out.lines);
        rects.config_rect = Some(c);
        rects.config_hits = out.hits.into_iter().map(|(y, k)| (c.y + y, k)).collect();
    } else if let Some(c) = pl.chat {
        // Mission in columns: a one-row tab strip tops the centre; the other tabs replace the chat
        let c = if pl.mode == Mode::Columns && c.h > 4 {
            let (row, hits) = centre::strip(s.view.centre, c.w);
            canvas.put(c.y, c.x, c.w, row);
            rects.centre_row = Some(c.y);
            rects.centre_hits = hits
                .into_iter()
                .map(|(a, b, k)| (a + c.x, b + c.x, k))
                .collect();
            Rect {
                y: c.y + 1,
                h: c.h - 1,
                ..c
            }
        } else {
            c
        };
        let other = rects.centre_row.is_some() && s.view.centre != Centre::Chat;
        if other {
            let lines = match s.view.centre {
                Centre::Files => centre::project(&s.view.project, c.w, c.h),
                Centre::Diff => centre::diff(&s.view.project, c.w, c.h),
                Centre::Browser => centre::browser(s, c.w, c.h, now),
                _ => centre::terminal(s, c.w, c.h, now),
            };
            canvas.paste(c, lines);
        } else {
            let docked = pl.mode == Mode::Columns && viewed_subagent(s).is_some();
            let _cards = agent::CardsScope::new(true);
            let lines = agent_panel(s, c.w, c.h, frame, now, docked, &mut rects, &mut chat_body);
            canvas.paste(c, lines);
            rects.chat_rect = Some(c);
            // gutter of 2 columns + the label row above the conversation
            rects.caret = rects.caret.map(|(x, y)| (u(c.x) + 2 + x, u(c.y) + 1 + y));
        }
    }
    canvas.put(pl.bottom_row, 0, w, footer(s, w));
    Frame {
        lines: canvas.into_lines(),
        cols: u(w),
        rows: u(h),
        rects,
        chat_body,
        mode: pl.mode,
        tabs: pl.tabs.clone(),
        view: pl.view,
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
