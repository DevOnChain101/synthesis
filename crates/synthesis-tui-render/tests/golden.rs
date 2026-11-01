//! Golden cases from `tests/golden/render` (captured from the Node reference): `expected.json`
//! (plain rows + hit rects), `expected.txt` (the same rows) and `expected.ansi.json` (raw truecolor
//! SGR rows plus their `sgr::encode` form). Every case must pass or be listed with a reason in
//! `golden_divergences.txt` (`render/<case>` or `render/<case>#json|txt|ansi`); no silent skips.
//!
//! `SYNTHESIS_UPDATE_GOLDEN=1` rewrites the expected files of a failing case from the actual output
//! ONLY when the difference is explained by the Nemo -> Synthesis rename (same text once the
//! product name, whitespace and box drawing are ignored, or only text dropped: the longer name
//! moves wrapping and padding and pushes header chips out). It prints every rewritten and every unexplained case; unexplained ones still fail.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use synthesis_tui_base::editor::LineEditor;
use synthesis_tui_base::sgr::{ColorMode, encode};
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{
    Agent, AgentOpts, EditorView, Pending, QHit, render_agent, todo_window,
};
use synthesis_tui_render::dash::*;
use synthesis_tui_render::model::{Session, Todo, apply_entry};
use synthesis_tui_render::picker::{Picker, PickerItem};
use synthesis_tui_render::theme::*;
use synthesis_tui_render::{dialogs, highlight};

const NOW: i64 = 1_790_942_400_000;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn divergences() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(root().join("tests/golden_divergences.txt")).unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (case, why) = l.split_once('|').expect("`case | reason`");
            assert!(!why.trim().is_empty(), "divergence without reason: {l}");
            (case.trim().to_string(), why.trim().to_string())
        })
        .collect()
}

fn read(path: PathBuf) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

// ------------------------------------------------------------ hydration

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn hydrate_session(spec: &Value) -> Session {
    let id = spec
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("session-1");
    let mut s = Session::new(id);
    if let Some(v) = spec.get("cwd").and_then(Value::as_str) {
        s.cwd = v.into();
    }
    if let Some(v) = spec.get("model").and_then(Value::as_str) {
        s.model = v.into();
    }
    if let Some(v) = spec.get("gitBranch").and_then(Value::as_str) {
        s.git_branch = v.into();
    }
    for e in spec
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        apply_entry(&mut s, e);
    }
    if let Some(set) = spec.get("set") {
        if let Some(t) = set.get("todos") {
            s.todos = serde_json::from_value::<Vec<Todo>>(t.clone()).unwrap();
        }
        if let Some(c) = set.get("contextUsed").and_then(Value::as_u64) {
            s.context_used = c;
        }
        if let Some(b) = set.get("contextDisplayReset").and_then(Value::as_bool) {
            s.context_display_reset = b;
        }
    }
    if let Some(t) = spec.get("startedAt").and_then(Value::as_i64) {
        s.started_at = Some(t);
    }
    s
}

fn hydrate_sessions(st: &Value) -> Vec<SessionRow> {
    let live_of = |l: &Value| Live {
        name: l.get("name").and_then(Value::as_str).unwrap_or("").into(),
        cwd: l.get("cwd").and_then(Value::as_str).unwrap_or("").into(),
        status: l.get("status").and_then(Value::as_str).unwrap_or("").into(),
    };
    if let Some(list) = st.get("sessions").and_then(Value::as_array) {
        return list
            .iter()
            .map(|s| SessionRow {
                id: s["id"].as_str().unwrap().into(),
                project: s
                    .get("project")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                cwd: s.get("cwd").and_then(Value::as_str).unwrap_or("").into(),
                mtime: s.get("mtime").and_then(Value::as_i64).unwrap_or(0),
                live: s.get("live").map(live_of),
                phase: s.get("phase").and_then(Value::as_str).unwrap_or("").into(),
                tokens: f(s, "tokens"),
                ctx_pct: s.get("ctxPct").and_then(Value::as_f64),
                ..Default::default()
            })
            .collect();
    }
    let n = st.get("sessions_n").and_then(Value::as_u64).unwrap_or(0);
    (0..n)
        .map(|i| SessionRow {
            id: format!("session-{i}-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            project: format!("proj-{i}"),
            cwd: format!("D:/workspace/project-{i}"),
            mtime: NOW - i as i64 * 1000,
            live: Some(Live {
                name: format!("proj-{i}"),
                cwd: format!("D:/workspace/project-{i}"),
                status: String::new(),
            }),
            phase: "idle".into(),
            tokens: 1000.0 * (i + 1) as f64,
            ctx_pct: Some(10.0),
            ..Default::default()
        })
        .collect()
}

fn parse_iso(s: &str) -> i64 {
    synthesis_tui_render::model::parse_ts(s).unwrap()
}

fn gen_history(g: &Value) -> Vec<HistoryDay> {
    let end = parse_iso(g["end"].as_str().unwrap());
    let days = g["days"].as_i64().unwrap();
    let base = g.get("base").and_then(Value::as_f64).unwrap_or(1000.0);
    (0..days)
        .rev()
        .map(|n| HistoryDay {
            date_ms: end - n * 86_400_000,
            tokens: if n % 7 == 0 { 0.0 } else { base + n as f64 },
            output: (n % 5) as f64,
            messages: 3.0,
            tools: 2.0,
            agents: if n % 3 == 0 { 1.0 } else { 0.0 },
            sessions: 1.0,
        })
        .collect()
}

fn hydrate_picker(p: &Value) -> Picker {
    let items: Vec<PickerItem> =
        serde_json::from_value(p.get("items").cloned().unwrap_or(json!([]))).unwrap();
    let mut x = Picker::new(
        p.get("id").and_then(Value::as_str).unwrap_or("p"),
        p.get("title").and_then(Value::as_str).unwrap_or(""),
        p.get("subtitle").and_then(Value::as_str).unwrap_or(""),
        items,
    );
    if let Some(set) = p.get("set") {
        if let Some(q) = set.get("query").and_then(Value::as_str) {
            x.query = q.into();
        }
        if let Some(c) = set.get("choice").and_then(Value::as_u64) {
            x.choice = c as usize;
        }
        if let Some(o) = set.get("offset").and_then(Value::as_u64) {
            x.offset = o as usize;
        }
        if let Some(l) = set.get("loading").and_then(Value::as_bool) {
            x.loading = l;
        }
        if let Some(t) = set.get("loadingText").and_then(Value::as_str) {
            x.loading_text = t.into();
        }
    }
    x
}

fn hydrate_agent(a: &Value) -> Agent {
    let mut base = json!({
        "cwd": "/work/proj", "mode": "default", "status": "idle", "effort": "auto", "model": "anthropic/fake-sonnet",
        "items": [], "turnStart": 0, "turnTokens": 0, "version": "1.0.0", "contextWindow": 200000
    });
    let obj = base.as_object_mut().unwrap();
    let mut subs = vec![];
    if let Some(o) = a.as_object() {
        for (k, v) in o {
            match k.as_str() {
                "subagents" => subs = v.as_array().cloned().unwrap_or_default(),
                "pending" => {}
                _ => {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
    let mut agent: Agent = serde_json::from_value(base).unwrap();
    agent.pending = a
        .get("pending")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .map(|p| {
                    let mut p: Pending = serde_json::from_value(p.clone()).unwrap();
                    p.hydrate();
                    p
                })
                .collect()
        })
        .unwrap_or_default();
    agent.subagents = subs
        .iter()
        .map(|s| {
            let mut m = json!({"items": [], "mode": "default", "cwd": agent.cwd});
            for (k, v) in s.as_object().unwrap() {
                m[k] = v.clone();
            }
            serde_json::from_value(m).unwrap()
        })
        .collect();
    agent
}

fn hydrate_opts(o: &Value) -> AgentOpts {
    let mut o = o.clone();
    let picker = o.as_object_mut().and_then(|m| m.remove("picker"));
    let mut opts: AgentOpts = serde_json::from_value(o).unwrap();
    opts.picker = picker.filter(|p| !p.is_null()).map(|p| hydrate_picker(&p));
    opts
}

fn hydrate_editor(e: Option<&Value>) -> LineEditor {
    let mut ed = LineEditor::new();
    if let Some(t) = e.and_then(|e| e.get("text")).and_then(Value::as_str) {
        ed.insert(t);
    }
    ed
}

struct Owned {
    session: Session,
    sessions: Vec<SessionRow>,
    history: Vec<HistoryDay>,
    usage: Usage,
    mcp: Vec<McpServer>,
    agent: Option<Agent>,
    editor: LineEditor,
    view: DashView,
    metric: String,
    index: usize,
}

fn hydrate_state(st: &Value) -> Owned {
    let usage_json = st.get("usage");
    let quota = |k: &str| {
        usage_json
            .and_then(|u| u.get(k))
            .filter(|q| !q.is_null())
            .map(|q| Quota {
                used: f(q, "used"),
                resets_at: q.get("resetsAt").and_then(Value::as_i64),
            })
    };
    let usage = Usage {
        tap: usage_json
            .and_then(|u| u.get("tap"))
            .filter(|t| !t.is_null())
            .map(|t| Tap {
                context_pct: t.get("contextPct").and_then(Value::as_f64),
                context_size: t.get("contextSize").and_then(Value::as_f64),
                cost_usd: t.get("costUsd").and_then(Value::as_f64),
                model: t.get("model").and_then(Value::as_str).map(String::from),
                ..Default::default()
            }),
        five_hour: quota("fiveHour"),
        seven_day: quota("sevenDay"),
    };
    let mcp = st
        .get("mcpServers")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .map(|m| McpServer {
                    name: m["name"].as_str().unwrap().into(),
                    transport: m.get("type").and_then(Value::as_str).unwrap_or("").into(),
                    url: m.get("url").and_then(Value::as_str).unwrap_or("").into(),
                    enabled: m.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    disabled_reason: m
                        .get("disabledReason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                })
                .collect()
        })
        .unwrap_or_default();
    let agent = st.get("agent").map(hydrate_agent);
    let view = DashView {
        opts: st.get("agentView").map(hydrate_opts).unwrap_or_default(),
        ..Default::default()
    };
    Owned {
        session: hydrate_session(st.get("session").unwrap_or(&json!({}))),
        sessions: hydrate_sessions(st),
        history: st.get("history_gen").map(gen_history).unwrap_or_default(),
        usage,
        mcp,
        editor: hydrate_editor(st.get("editor")),
        agent,
        view,
        metric: st
            .get("usageMetric")
            .and_then(Value::as_str)
            .unwrap_or("tokens")
            .into(),
        index: st.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
    }
}

impl Owned {
    fn dash<'a>(&'a self, chars: &'a [char], cur: usize) -> DashState<'a> {
        DashState {
            session: &self.session,
            sessions: &self.sessions,
            index: self.index,
            usage: &self.usage,
            follow: false,
            history: &self.history,
            usage_metric: &self.metric,
            mcp_servers: &self.mcp,
            mcp_live: None,
            workspaces: None,
            active_workspace: "",
            agent: self.agent.as_ref(),
            chat_agent: None,
            editor: self.agent.as_ref().map(|_| EditorView { chars, cur }),
            focus: Focus::Agent,
            view: &self.view,
            agent_session_id: None,
            tz_offset_min: 0,
        }
    }
}

// ------------------------------------------------------------ comparisons

type Check = (&'static str, Result<(), String>);

/// Actual output of the case being run, for `SYNTHESIS_UPDATE_GOLDEN`.
#[derive(Default)]
struct Actual {
    ansi: Vec<String>,
    json: BTreeMap<&'static str, Value>,
}

thread_local! {
    static ACTUAL: RefCell<Actual> = RefCell::new(Actual::default());
}

fn note_json(key: &'static str, v: Value) {
    ACTUAL.with(|a| {
        a.borrow_mut().json.insert(key, v);
    });
}

/// Letters and digits only, lowercase, product names removed.
fn skeleton<S: AsRef<str>>(rows: &[S]) -> String {
    let s: String = rows
        .iter()
        .flat_map(|r| strip_ansi(r.as_ref()).chars().collect::<Vec<_>>())
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    s.replace("synthesis", "").replace("nemo", "")
}

/// Cases whose rename diff was checked by hand (2026-10-30) because it ADDS text: the panel title
/// "SYNTHESIS CORE" no longer fits, flow.rs falls back to "CORE" and the model chip now fits.
const REVIEWED_RENAME_DIFFS: [&str; 1] = ["dash-short-sessions-scroll-index-39-180x30"];

/// Same text, or only whole header chips dropped for lack of room (the longer name takes it).
fn is_subsequence(got: &str, want: &str) -> bool {
    let mut w = want.chars();
    got.chars().all(|c| w.any(|x| x == c))
}

/// Rewrites `expected.{json,txt,ansi.json}` of `dir` from the recorded actual output.
fn rewrite_expected(dir: &std::path::Path, expected: &Value, a: &Actual) {
    let ansi = &a.ansi;
    let plain: Vec<String> = ansi.iter().map(|l| strip_ansi(l)).collect();
    let mut e = expected.clone();
    e["lines"] = json!(plain);
    for (k, v) in &a.json {
        e[*k] = v.clone();
    }
    let put = |name: &str, v: &Value| {
        std::fs::write(
            dir.join(name),
            format!("{}\n", serde_json::to_string_pretty(v).unwrap()),
        )
        .unwrap()
    };
    put("expected.json", &e);
    std::fs::write(dir.join("expected.txt"), format!("{}\n", plain.join("\n"))).unwrap();
    let mut x = read(dir.join("expected.ansi.json"));
    x["lines"] = json!(ansi);
    x["encoded_truecolor"] = json!(
        ansi.iter()
            .map(|l| encode(l, ColorMode::TrueColor))
            .collect::<Vec<_>>()
    );
    put("expected.ansi.json", &x);
}

fn cmp_lines(label: &str, got: &[String], want: &Value) -> Result<(), String> {
    let want: Vec<&str> = want
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    if got.len() != want.len() {
        return Err(format!(
            "{label}: {} rows, expected {}",
            got.len(),
            want.len()
        ));
    }
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        if g != w {
            return Err(format!("{label} row {i}:\n   got  {g:?}\n   want {w:?}"));
        }
    }
    Ok(())
}

fn frame_checks(
    dir: &std::path::Path,
    ansi_rows: &[String],
    extra: Vec<(&'static str, Result<(), String>)>,
    json_ok: Result<(), String>,
) -> Vec<Check> {
    let plain: Vec<String> = ansi_rows.iter().map(|l| strip_ansi(l)).collect();
    ACTUAL.with(|a| a.borrow_mut().ansi = ansi_rows.to_vec());
    let mut out: Vec<Check> = vec![("json", json_ok)];
    out.extend(extra);
    let txt = std::fs::read_to_string(dir.join("expected.txt")).unwrap();
    let want = format!("{}\n", plain.join("\n"));
    out.push((
        "txt",
        if txt == want {
            Ok(())
        } else {
            Err("expected.txt differs".to_string())
        },
    ));
    let ansi = read(dir.join("expected.ansi.json"));
    let raw = cmp_lines("ansi", ansi_rows, &ansi["lines"]);
    let enc: Vec<String> = ansi_rows
        .iter()
        .map(|l| encode(l, ColorMode::TrueColor))
        .collect();
    let enc_ok = cmp_lines("encoded_truecolor", &enc, &ansi["encoded_truecolor"]);
    out.push(("ansi", raw.and(enc_ok)));
    out
}

fn rect_json(r: &Rect) -> Value {
    json!({"x": r.x, "y": r.y, "w": r.w, "h": r.h})
}

fn rects_json(r: &Rects) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(x) = &r.chat_rect {
        m.insert("chatRect".into(), rect_json(x));
    }
    if let Some(x) = &r.sidebar_rect {
        m.insert("sidebarRect".into(), rect_json(x));
    }
    if let Some(x) = &r.dock_rect {
        m.insert("dockRect".into(), rect_json(x));
    }
    if let Some(x) = &r.activity_rect {
        m.insert("activityRect".into(), rect_json(x));
    }
    if let Some(x) = &r.workspace_rect {
        m.insert("workspaceRect".into(), rect_json(x));
    }
    m.insert(
        "sessionRows".into(),
        Value::Array(
            r.session_rows
                .iter()
                .map(|(y, id)| json!({"y": y, "id": id}))
                .collect(),
        ),
    );
    m.insert(
        "workspaceRows".into(),
        Value::Array(
            r.workspace_rows
                .iter()
                .map(|(y, key)| json!({"y": y, "key": key}))
                .collect(),
        ),
    );
    Value::Object(m)
}

fn qhit_json(h: &QHit) -> Value {
    let mut m = json!({"type": h.kind, "x0": h.x0, "x1": h.x1, "bodyRow": h.body_row, "y": h.y});
    if let Some(i) = h.index {
        m["index"] = json!(i);
    }
    if let Some(q) = h.qi {
        m["qi"] = json!(q);
    }
    m
}

fn run_case(dir: &std::path::Path, input: &Value) -> Vec<Check> {
    let expected = read(dir.join("expected.json"));
    let op = input["op"].as_str().unwrap();
    match op {
        "frame" => {
            let st = hydrate_state(&input["state"]);
            let chars = st.editor.chars.clone();
            let ds = st.dash(&chars, st.editor.cur);
            let fr = render_frame(
                &ds,
                input["cols"].as_u64().unwrap() as usize,
                input["rows"].as_u64().unwrap() as usize,
                input["frame"].as_u64().unwrap() as usize,
                input["now"].as_i64().unwrap(),
            );
            let plain: Vec<String> = fr.lines.iter().map(|l| strip_ansi(l)).collect();
            let mut rects_ok = Ok(());
            if st.agent.is_some() {
                note_json("rects", rects_json(&fr.rects));
            }
            if st.agent.is_some() && rects_json(&fr.rects) != expected["rects"] {
                rects_ok = Err(format!(
                    "rects differ:\n got  {}\n want {}",
                    rects_json(&fr.rects),
                    expected["rects"]
                ));
            }
            let json_ok = cmp_lines("lines", &plain, &expected["lines"]);
            let mut c = frame_checks(dir, &fr.lines, vec![("rects", rects_ok)], json_ok);
            let dims = if expected["cols"] == input["cols"] && expected["rows"] == input["rows"] {
                Ok(())
            } else {
                Err("cols/rows".to_string())
            };
            c.push(("json", dims));
            c
        }
        "agent" => {
            let session = hydrate_session(&{
                let mut s = input.get("session").cloned().unwrap_or(json!({}));
                if s.get("id").is_none() {
                    s["id"] = json!("session-1");
                }
                s
            });
            let agent = hydrate_agent(&input["agent"]);
            let ed = hydrate_editor(input.get("editor"));
            let opts = hydrate_opts(input.get("opts").unwrap_or(&json!({})));
            let out = render_agent(
                &agent,
                &session.todos,
                &EditorView {
                    chars: &ed.chars,
                    cur: ed.cur,
                },
                input["w"].as_u64().unwrap() as usize,
                input["h"].as_u64().unwrap() as usize,
                input["frame"].as_u64().unwrap() as usize,
                input["now"].as_i64().unwrap(),
                &opts,
            );
            let plain: Vec<String> = out.lines.iter().map(|l| strip_ansi(l)).collect();
            let mut json_ok = cmp_lines("lines", &plain, &expected["lines"]);
            if expected["maxScroll"] != json!(out.max_scroll) {
                json_ok = json_ok.and(Err(format!(
                    "maxScroll {} vs {}",
                    out.max_scroll, expected["maxScroll"]
                )));
            }
            let hits: Vec<Value> = out.question_hits.iter().map(qhit_json).collect();
            note_json("maxScroll", json!(out.max_scroll));
            if expected.get("question_hits").is_some() || !hits.is_empty() {
                note_json("question_hits", Value::Array(hits.clone()));
            }
            let want_hits = expected.get("question_hits").cloned().unwrap_or(json!([]));
            if Value::Array(hits.clone()) != want_hits {
                json_ok = json_ok.and(Err(format!(
                    "question_hits {} vs {}",
                    Value::Array(hits),
                    want_hits
                )));
            }
            frame_checks(dir, &out.lines, vec![], json_ok)
        }
        "tasks" | "usage" | "tasksUsage" => {
            let st = hydrate_state(&input["state"]);
            let ds = st.dash(&[], 0);
            let (w, h) = (input["w"].as_i64().unwrap(), input["h"].as_i64().unwrap());
            let (rows, flags) = match op {
                "tasks" => (tasks_panel(&ds, w, h, 0, VAlign::Top), None),
                "usage" => (
                    usage_panel(&ds, w, h, VAlign::Evenly, UsageView::Auto, NOW),
                    None,
                ),
                _ => {
                    let r = tasks_usage_row(&ds, w, h, 0, NOW);
                    (r.rows, Some((r.tasks, r.usage)))
                }
            };
            let plain: Vec<String> = rows.iter().map(|l| strip_ansi(l)).collect();
            let mut json_ok = cmp_lines("lines", &plain, &expected["lines"]);
            if let Some((t, u)) = flags {
                note_json("tasks", json!(t));
                note_json("usage", json!(u));
            }
            if let Some((t, u)) = flags
                && (expected["tasks"] != json!(t) || expected["usage"] != json!(u))
            {
                json_ok = json_ok.and(Err("tasks/usage flags".to_string()));
            }
            frame_checks(dir, &rows, vec![], json_ok)
        }
        _ => vec![("json", helper_case(op, input, &expected))],
    }
}

fn palette_json() -> Value {
    let rgb = |c: Rgb| json!([c[0], c[1], c[2]]);
    json!({
        "THEME": {"claude": rgb(T_CLAUDE), "shimmer": rgb(T_SHIMMER), "text": rgb(T_TEXT), "body": rgb(T_BODY), "inactive": rgb(T_INACTIVE), "subtle": rgb(T_SUBTLE),
            "border": rgb(T_BORDER), "permission": rgb(T_PERMISSION), "autoAccept": rgb(T_AUTO_ACCEPT), "planMode": rgb(T_PLAN_MODE), "mcp": rgb(T_MCP), "bash": rgb(T_BASH),
            "success": rgb(T_SUCCESS), "error": rgb(T_ERROR), "warning": rgb(T_WARNING), "track": rgb(T_TRACK)},
        "ACCENT": {"core": rgb(ACCENT_CORE), "light": rgb(ACCENT_LIGHT), "glow": rgb(ACCENT_GLOW), "deep": rgb(ACCENT_DEEP), "cyan": rgb(ACCENT_CYAN), "ink": rgb(ACCENT_INK)},
        "VIVID": {"purple": rgb(PURPLE), "blue": rgb(BLUE), "green": rgb(GREEN), "yellow": rgb(YELLOW), "orange": rgb(ORANGE), "pink": rgb(PINK), "cyan": rgb(CYAN), "teal": rgb(TEAL), "red": rgb(RED)},
        "WARM": {"claude": rgb(ACCENT_CORE), "ember": rgb(ACCENT_LIGHT), "amber": rgb(YELLOW), "gold": rgb(GREEN), "sand": rgb(CYAN), "clay": rgb(ACCENT_DEEP),
            "cream": rgb(WARM_CREAM), "dim": rgb(WARM_DIM), "deep": rgb(WARM_DEEP), "ok": rgb(GREEN), "bad": rgb(RED)},
        "HEAT": HEAT.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "SELECT_BG": rgb(SELECT_BG), "SELECT_FG": rgb(SELECT_FG),
        "BORDER_GRAD": BORDER_GRAD.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "LOGO_GRAD": LOGO_GRAD.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "AGENT_COLORS": AGENT_COLORS.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "CLAUDE_SPIN": (0..8).map(spin).collect::<Vec<_>>(),
    })
}

fn same(got: Value, want: &Value, what: &str) -> Result<(), String> {
    if &got == want {
        Ok(())
    } else {
        Err(format!("{what}:\n got  {got}\n want {want}"))
    }
}

fn helper_case(op: &str, input: &Value, expected: &Value) -> Result<(), String> {
    use synthesis_tui_render::util::{fmt_dur, fmt_num};
    let nf = |v: &Value| v.as_f64();
    let got = match op {
        "todoWindow" => json!(input["cases"].as_array().unwrap().iter().map(|c| {
            let todos: Vec<Todo> = c["todos"].as_array().unwrap().iter().map(|s| Todo { content: "x".into(), status: s.as_str().unwrap().into(), ..Default::default() }).collect();
            json!({"todos": c["todos"], "cap": c["cap"], "start": todo_window(&todos, c["cap"].as_u64().unwrap() as usize)})
        }).collect::<Vec<_>>()),
        "heatLevel" => json!(input["cases"].as_array().unwrap().iter().map(|c| json!({"v": c[0], "max": c[1], "level": heat_level(nf(&c[0]), nf(&c[1]))})).collect::<Vec<_>>()),
        "layoutFor" => json!(input["sizes"].as_array().unwrap().iter().map(|c| {
            let (w, h) = (c[0].as_i64().unwrap(), c[1].as_i64().unwrap());
            json!({"w": w, "h": h, "layout": match layout_for(w, h) { Layout::Right => "right", Layout::Bottom => "bottom", Layout::None => "none" }})
        }).collect::<Vec<_>>()),
        "fmt" => json!({
            "nums": input["nums"].as_array().unwrap().iter().map(|n| json!({"n": n, "s": fmt_num(nf(n))})).collect::<Vec<_>>(),
            "durs": input["durs"].as_array().unwrap().iter().map(|n| json!({"ms": n, "s": fmt_dur(nf(n))})).collect::<Vec<_>>(),
        }),
        "sessionsWindow" => json!(input["cases"].as_array().unwrap().iter().map(|c| {
            let (s, e, a, b) = sessions_window(c[0].as_u64().unwrap() as usize, c[1].as_u64().unwrap() as usize, c[2].as_i64().unwrap());
            json!({"count": c[0], "index": c[1], "cap": c[2], "win": {"start": s, "end": e, "above": a, "below": b}})
        }).collect::<Vec<_>>()),
        "bar" => json!(input["cases"].as_array().unwrap().iter().map(|c| {
            let a = bar(nf(&c[0]), c[1].as_u64().unwrap() as usize, 0, false);
            json!({"p": c[0], "w": c[1], "ansi": a, "plain": strip_ansi(&a)})
        }).collect::<Vec<_>>()),
        "width" => json!({
            "widths": input["strs"].as_array().unwrap().iter().map(|s| json!({"s": s, "width": synthesis_tui_base::text::width(s.as_str().unwrap()), "plain": strip_ansi(s.as_str().unwrap())})).collect::<Vec<_>>(),
            "fits": input["fits"].as_array().unwrap().iter().map(|c| {
                let o = fit(c[0].as_str().unwrap(), c[1].as_u64().unwrap() as usize);
                json!({"s": c[0], "w": c[1], "out": o, "width": synthesis_tui_base::text::width(&o)})
            }).collect::<Vec<_>>(),
        }),
        "colors" => {
            let rgb = |c: Rgb| json!([c[0], c[1], c[2]]);
            let arr = |v: &Value| -> Rgb { let a = v.as_array().unwrap(); [a[0].as_u64().unwrap() as u8, a[1].as_u64().unwrap() as u8, a[2].as_u64().unwrap() as u8] };
            json!({
                "lerp": input["lerp"].as_array().unwrap().iter().map(|c| rgb(lerp(arr(&c[0]), arr(&c[1]), c[2].as_f64().unwrap()))).collect::<Vec<_>>(),
                "cycle": input["cycle"].as_array().unwrap().iter().map(|t| rgb(cycle(&BORDER_GRAD, t.as_f64().unwrap()))).collect::<Vec<_>>(),
                "pulse": input["pulse"].as_array().unwrap().iter().map(|f| rgb(pulse([0, 0, 0], [200, 100, 50], f.as_u64().unwrap(), 20))).collect::<Vec<_>>(),
                "agent_colors": input["agentTypes"].as_array().unwrap().iter().map(|t| json!({"type": t, "rgb": rgb(agent_color(t.as_str().unwrap()))})).collect::<Vec<_>>(),
                "dots": input["dots"].as_array().unwrap().iter().map(|f| json!(dots(f.as_u64().unwrap() as usize))).collect::<Vec<_>>(),
                "waiting": input["waiting"].as_array().unwrap().iter().map(|c| json!(waiting(c[0].as_str().unwrap(), c[1].as_u64().unwrap() as usize))).collect::<Vec<_>>(),
                "palette": palette_json(),
            })
        }
        "highlight" => json!({"lines": highlight::highlight(input["code"].as_str().unwrap(), input["lang"].as_str().unwrap()).iter().map(|l| l.iter().map(|t| json!({"t": t.t, "c": [t.c.rgb[0], t.c.rgb[1], t.c.rgb[2]]})).collect::<Vec<_>>()).collect::<Vec<_>>()}),
        "langFromFile" => json!(input["files"].as_array().unwrap().iter().map(|f| json!({"file": f, "lang": highlight::lang_from_file(f.as_str())})).collect::<Vec<_>>()),
        "dialogs" => dialogs_json(),
        other => return Err(format!("unknown op {other}")),
    };
    same(got, expected, op)
}

fn dialogs_json() -> Value {
    let mut p = Picker::new(
        "p",
        "t",
        "",
        ["alpha", "beta", "gamma", "alphabet"]
            .iter()
            .map(|l| PickerItem {
                label: (*l).into(),
                detail: l.to_uppercase(),
                current: false,
            })
            .collect(),
    );
    let mut steps = vec![];
    let snap = |tag: &str, p: &Picker| json!({"tag": tag, "query": p.query, "choice": p.choice, "offset": p.offset, "items": p.filtered().iter().map(|i| i.label.clone()).collect::<Vec<_>>()});
    steps.push(snap("initial", &p));
    p.type_text("a");
    steps.push(snap("type a", &p));
    p.type_text("l");
    steps.push(snap("type l", &p));
    p.move_by(1);
    steps.push(snap("down", &p));
    p.end();
    steps.push(snap("end", &p));
    p.home();
    steps.push(snap("home", &p));
    p.backspace();
    steps.push(snap("backspace", &p));
    let mut valid: Vec<&str> = dialogs::VALID_MODES.to_vec();
    valid.sort();
    let opts = |sug: Vec<Value>| -> Value {
        json!(
            dialogs::permission_options(&sug, false)
                .into_iter()
                .map(|(k, l)| json!({"key": k, "label": l}))
                .collect::<Vec<_>>()
        )
    };
    let modes: Vec<Value> = [
        json!("default"),
        json!("auto"),
        json!("plan"),
        json!("acceptEdits"),
        json!("bypassPermissions"),
        json!("dontAsk"),
        json!("bogus"),
        json!(""),
        Value::Null,
    ]
    .iter()
    .map(|m| {
        let n = dialogs::normalize_mode(m.as_str());
        json!({"m": m, "v": n, "name": dialogs::mode_name(n)})
    })
    .collect();
    json!({
        "MODES": dialogs::MODES,
        "VALID_MODES": valid,
        "PLAN_OPTIONS": dialogs::PLAN_OPTIONS.iter().map(|(k, l, m)| { let mut o = json!({"key": k, "label": l}); if let Some(m) = m { o["mode"] = json!(m); } o }).collect::<Vec<_>>(),
        "OUTSIDE_READ_OPTIONS": dialogs::OUTSIDE_READ_OPTIONS.iter().map(|(k, l)| json!({"key": k, "label": l})).collect::<Vec<_>>(),
        "LOCAL_COMMANDS": dialogs::LOCAL_COMMANDS.iter().map(|(n, d, h)| { let mut o = json!({"name": n, "description": d}); if let Some(h) = h { o["argumentHint"] = json!(h); } o }).collect::<Vec<_>>(),
        "normalizeMode": modes,
        "permissionOptions": [
            opts(vec![]),
            opts(vec![json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "rm:*"}]})]),
            opts(vec![json!({"type": "setMode", "mode": "acceptEdits"})]),
        ],
        "questionState": [{"choice": 0, "picked": [], "other": "", "answered": false, "multi": true}],
        "picker_steps": steps,
    })
}

#[test]
fn render_golden() {
    let allowed = divergences();
    let update = std::env::var_os("SYNTHESIS_UPDATE_GOLDEN").is_some_and(|v| v == "1");
    let (mut rewritten, mut unexplained) = (Vec::<String>::new(), Vec::<String>::new());
    let base = root().join("../../tests/golden/render");
    let mut dirs: Vec<_> = std::fs::read_dir(&base)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty());
    let (mut cases, mut passed, mut diverged, mut failures) = (0, 0, 0, Vec::<String>::new());
    // per-file counters for the frame-like cases (those with expected.txt)
    let mut files: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    let mut used_allow: Vec<String> = vec![];
    for dir in &dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let id = format!("render/{name}");
        let input = read(dir.join("input.json"));
        ACTUAL.with(|a| *a.borrow_mut() = Actual::default());
        let mut checks =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(dir, &input)))
                .unwrap_or_else(|e| {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    vec![("json", Err(format!("panic: {msg}")))]
                });
        if update && checks.iter().any(|(_, r)| r.is_err()) {
            let a = ACTUAL.with(|a| std::mem::take(&mut *a.borrow_mut()));
            let want = read(dir.join("expected.json"));
            let want_rows: Vec<&str> = want["lines"]
                .as_array()
                .map(|v| v.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let got_plain: Vec<String> = a.ansi.iter().map(|l| strip_ansi(l)).collect();
            if !a.ansi.is_empty()
                && !allowed.keys().any(|k| k.starts_with(&id))
                && (REVIEWED_RENAME_DIFFS.contains(&name.as_str())
                    || is_subsequence(&skeleton(&got_plain), &skeleton(&want_rows)))
            {
                rewrite_expected(dir, &want, &a);
                rewritten.push(name.clone());
                checks = vec![("json", Ok(()))];
            } else if !allowed.keys().any(|k| k.starts_with(&id)) {
                unexplained.push(name.clone());
            }
        }
        cases += 1;
        let mut case_ok = true;
        let mut case_div = false;
        for (file, res) in &checks {
            if matches!(*file, "txt" | "ansi") {
                let e = files.entry(file).or_default();
                e.0 += 1;
                if res.is_ok() {
                    e.1 += 1;
                }
            }
            let keys = [id.clone(), format!("{id}#{file}")];
            match (res, keys.iter().find(|k| allowed.contains_key(*k))) {
                (Ok(()), _) => {}
                (Err(_), Some(k)) => {
                    used_allow.push(k.clone());
                    case_div = true;
                }
                (Err(e), None) => {
                    case_ok = false;
                    failures.push(format!("{id} [{file}]: {e}"));
                }
            }
        }
        if case_ok && case_div {
            diverged += 1;
        } else if case_ok {
            passed += 1;
        }
    }
    for k in allowed.keys() {
        if !used_allow.contains(k) {
            failures.push(format!(
                "{k}: allow-list entry that is not needed (stale) or has no case"
            ));
        }
    }
    if update {
        println!(
            "SYNTHESIS_UPDATE_GOLDEN: rewrote {} case(s) whose diff is explained by the rename:\n  {}\nnot explained, left unchanged ({}):\n  {}",
            rewritten.len(),
            rewritten.join("\n  "),
            unexplained.len(),
            unexplained.join("\n  ")
        );
    }
    let (txt, ansi) = (files["txt"], files["ansi"]);
    println!(
        "GOLDEN render: {cases} cases, {passed} passed, {diverged} divergent (listed); frame-like cases: expected.txt {}/{}, expected.ansi.json {}/{}",
        txt.1, txt.0, ansi.1, ansi.0
    );
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
