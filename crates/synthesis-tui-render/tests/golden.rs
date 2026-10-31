//! Golden cases from `tests/golden/render` (captured from the Node reference): `expected.json`
//! (plain rows + hit rects), `expected.txt` (the same rows) and `expected.ansi.json` (raw truecolor
//! SGR rows plus their `sgr::encode` form). Every case must pass or be listed with a reason in
//! `golden_divergences.txt` (`render/<case>` or `render/<case>#json|txt|ansi`); no silent skips.
//!
//! `SYNTHESIS_UPDATE_GOLDEN=redesign-p1` is the gate of phase P1 of docs/specs/tui-design.md (theme
//! tokens, header): it rewrites only colour (`expected.ansi.json`, the colour keys of two helper cases)
//! the header rows of `frame` cases and the welcome greeting rows, REFUSES any other change (the case then keeps failing),
//! marks every rewritten case `reviewed_by = "pending-owner-review (P1 2026-10-30)"` and prints a
//! summary. Cases listed in `golden_divergences.txt` differ from the oracle in text already and are
//! skipped (their rebaseline belongs to the phase that changes that text).
//!
//! `SYNTHESIS_UPDATE_GOLDEN=redesign-p2` is the gate of phase P2 (layout engine, panel registry): it
//! rewrites only `frame` cases and fills the files of a case directory that has none yet, after
//! checking that the new rows are a frame of the new layout (size, header, locked panels from 100
//! columns). Every other op is refused. It stamps `reviewed_by = "pending-owner-review (P2 2026-10-30)"`.
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
use synthesis_tui_render::centre::{Centre, ProjectData, TreeRow};
use synthesis_tui_render::dash::*;
use synthesis_tui_render::layout::{PanelId, Panels, View};
use synthesis_tui_render::model::{Session, Todo, apply_entry};
use synthesis_tui_render::picker::{Picker, PickerItem};
use synthesis_tui_render::theme::*;
use synthesis_tui_render::{dialogs, highlight};

const NOW: i64 = 1_790_942_400_000;

/// The stops of the former `BORDER_GRAD`: `cycle` is a generic function and its golden keeps them.
const GRADIENT_STOPS: [Rgb; 5] = [
    [79, 70, 229],
    [129, 140, 248],
    [103, 232, 249],
    [129, 140, 248],
    [79, 70, 229],
];

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
    host: HostInfo,
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
        panels: hydrate_panels(st.get("panels")),
        tab: match st.get("tab").and_then(Value::as_str) {
            Some("sessions") => View::Sessions,
            Some("agents") => View::Agents,
            Some("panels") => View::Panels,
            _ => View::Chat,
        },
        ..Default::default()
    };
    let host = HostInfo {
        sandbox_mode: st
            .pointer("/host/sandbox")
            .and_then(Value::as_str)
            .map(String::from),
        sandbox_backend: String::new(),
        memory: st.pointer("/host/memory").and_then(Value::as_bool),
        peers: st
            .pointer("/host/peers")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
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
        host,
    }
}

/// `"panels"` of a case state: `"all"`, `"none"` (every hideable panel off) or `{"off": [ids]}`;
/// absent = the registry defaults.
fn hydrate_panels(v: Option<&Value>) -> Panels {
    match v {
        Some(Value::String(m)) if m == "all" => Panels::all(),
        Some(Value::String(m)) if m == "none" => Panels::none(),
        Some(o) if o.get("off").is_some() => {
            let mut p = Panels::default();
            for id in o["off"].as_array().unwrap() {
                p.set(PanelId::parse(id.as_str().unwrap()).unwrap(), false);
            }
            p
        }
        _ => Panels::default(),
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
            host: &self.host,
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
        _ => vec![("json", helper_case(op, input, &expected))],
    }
}

/// The Classic theme in the shape of the Node palette (`THEME`, `ACCENT`, ...): Classic is that palette,
/// token for token. The few stops no renderer reads any more (glow, deep, the logo gradient) are literals.
fn palette_json() -> Value {
    let rgb = |c: Rgb| json!([c[0], c[1], c[2]]);
    let t = &CLASSIC;
    let x = t.extra.unwrap();
    let (glow, deep, cyan) = ([199, 210, 254], [79, 70, 229], [103, 232, 249]);
    json!({
        "THEME": {"claude": rgb(t.accent), "shimmer": rgb(t.accent_light), "text": rgb(t.strong), "body": rgb(t.body), "inactive": rgb(t.label), "subtle": rgb(t.muted),
            "border": rgb(t.border), "permission": rgb(t.info), "autoAccept": rgb(t.purple()), "planMode": rgb(t.ok), "mcp": rgb(t.blue()), "bash": rgb(t.pink()),
            "success": rgb(t.ok), "error": rgb(t.err), "warning": rgb(t.warn), "track": rgb(t.track())},
        "ACCENT": {"core": rgb(t.accent), "light": rgb(t.accent_light), "glow": rgb(glow), "deep": rgb(deep), "cyan": rgb(cyan), "ink": rgb(t.ink)},
        "VIVID": {"purple": rgb(x.purple), "blue": rgb(x.blue), "green": rgb(t.ok), "yellow": rgb(t.warn), "orange": rgb(x.orange), "pink": rgb(x.pink), "cyan": rgb(t.info), "teal": rgb(x.teal), "red": rgb(t.err)},
        "WARM": {"claude": rgb(t.accent), "ember": rgb(t.accent_light), "amber": rgb(t.warn), "gold": rgb(t.ok), "sand": rgb(t.info), "clay": rgb(deep),
            "cream": rgb(x.cream), "dim": rgb(t.border), "deep": rgb(x.track), "ok": rgb(t.ok), "bad": rgb(t.err)},
        "HEAT": t.heat.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "SELECT_BG": rgb(t.elevated), "SELECT_FG": rgb(t.strong),
        "BORDER_GRAD": GRADIENT_STOPS.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
        "LOGO_GRAD": [rgb(t.accent_light), rgb(cyan), rgb([196, 181, 253]), rgb(t.accent_light)],
        "AGENT_COLORS": x.agents.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
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
                "cycle": input["cycle"].as_array().unwrap().iter().map(|t| rgb(cycle(&GRADIENT_STOPS, t.as_f64().unwrap()))).collect::<Vec<_>>(),
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
    note_json("helper", got.clone());
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

const P1_STAMP: &str = "pending-owner-review (P1 2026-10-30)";
const P2_STAMP: &str = "pending-owner-review (P2 2026-10-30)";
const NEW_STAMP: &str = "pending-owner-review (new TUI 2026-10-31)";

/// What phase P2 (layout engine, panel registry) may rewrite: `frame` cases only, whose new rows
/// must still be a frame of the layout: the right size, the header, and (from 100 columns) the
/// locked panels. Everything else (agent pane, pickers, dialogs, helpers) stays as it is, so a
/// change there still fails.
fn p2_check_frame(input: &Value, a: &Actual) -> Result<(), String> {
    if input["op"] != "frame" || a.ansi.is_empty() {
        return Err("not a frame case: outside the P2 scope".into());
    }
    let (cols, rows) = (
        input["cols"].as_u64().unwrap() as usize,
        input["rows"].as_u64().unwrap() as usize,
    );
    let (cols, rows) = (cols.max(40), rows.max(11));
    let plain: Vec<String> = a.ansi.iter().map(|l| strip_ansi(l)).collect();
    if plain.len() != rows {
        return Err(format!("{} rows, expected {rows}", plain.len()));
    }
    if let Some(r) = plain
        .iter()
        .position(|l| synthesis_tui_base::text::width(l) != cols)
    {
        return Err(format!("row {r} is not {cols} cells wide"));
    }
    if !plain[0].contains("SYNTH") && !plain[0].contains("⢎⡱") {
        return Err("no header".into());
    }
    if cols >= 100 {
        let all = plain.join("\n");
        for label in ["WORKSPACE", "RUNNING SESSIONS", "SUB-AGENTS"] {
            if !all.contains(label) {
                return Err(format!("locked panel {label} missing"));
            }
        }
    }
    Ok(())
}

/// Files of a case that does not exist yet, so the gate can fill them from the renderer.
fn p2_new_case(dir: &std::path::Path, input: &Value) {
    let put = |name: &str, v: &Value| {
        std::fs::write(
            dir.join(name),
            format!("{}\n", serde_json::to_string_pretty(v).unwrap()),
        )
        .unwrap()
    };
    put(
        "expected.json",
        &json!({"cols": input["cols"], "rows": input["rows"], "lines": []}),
    );
    std::fs::write(dir.join("expected.txt"), "\n").unwrap();
    put(
        "expected.ansi.json",
        &json!({"color_mode": "truecolor", "lines": [], "encoded_truecolor": []}),
    );
    put(
        "meta.json",
        &json!({"captured_at": "<TS>", "source": "Rust renderer (redesign P2): no Node oracle", "reviewed_by": P2_STAMP}),
    );
}

/// Helper cases whose `expected.json` may change in P1, with the keys (at any depth) that may.
const P1_HELPER_KEYS: [(&str, &[&str]); 2] = [
    ("theme-color-fns", &["palette", "agent_colors"]),
    ("bar-widths", &["ansi"]),
];

fn without_keys(v: &Value, keys: &[&str]) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !keys.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), without_keys(v, keys)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| without_keys(v, keys)).collect()),
        o => o.clone(),
    }
}

/// The rows P1 may change in a frame-like case: the header (first row, and the rule under it when it
/// is a heavy `━` line) and the welcome greeting (the old "Hello there! I'm ..." row and the blank row
/// under it, which now carries the prompt line).
fn p1_rows(op: &str, old: &[&str]) -> Vec<usize> {
    let mut rows = vec![];
    if op == "frame" {
        rows.push(0);
        if old
            .get(1)
            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c == '━'))
        {
            rows.push(1);
        }
    }
    for (i, r) in old.iter().enumerate() {
        if r.contains("Hello there! I'm") {
            rows.extend([i, i + 1]);
        }
    }
    rows
}

/// Rewrites only the rows P1 may change, for a case that is listed in `golden_divergences.txt`
/// (its other rows differ from the oracle for reasons of their own and stay as captured).
fn p1_sync_rows(
    dir: &std::path::Path,
    op: &str,
    expected: &Value,
    a: &Actual,
) -> Result<Vec<usize>, String> {
    let old: Vec<&str> = expected["lines"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if old.len() != a.ansi.len() {
        return Err(format!("{} rows, expected {}", a.ansi.len(), old.len()));
    }
    let rows: Vec<usize> = p1_rows(op, &old)
        .into_iter()
        .filter(|i| *i < old.len() && strip_ansi(&a.ansi[*i]) != old[*i])
        .collect();
    if rows.is_empty() {
        return Ok(rows);
    }
    let put = |name: &str, v: &Value| {
        std::fs::write(
            dir.join(name),
            format!("{}\n", serde_json::to_string_pretty(v).unwrap()),
        )
        .unwrap()
    };
    let mut e = expected.clone();
    let mut plain: Vec<String> = old.iter().map(|l| l.to_string()).collect();
    let mut x = read(dir.join("expected.ansi.json"));
    for i in &rows {
        plain[*i] = strip_ansi(&a.ansi[*i]);
        x["lines"][*i] = json!(a.ansi[*i]);
        x["encoded_truecolor"][*i] = json!(encode(&a.ansi[*i], ColorMode::TrueColor));
    }
    e["lines"] = json!(plain);
    put("expected.json", &e);
    std::fs::write(dir.join("expected.txt"), format!("{}\n", plain.join("\n"))).unwrap();
    put("expected.ansi.json", &x);
    Ok(rows)
}

/// What P1 may rewrite in a frame-like case: Ok(header rows that changed) or the reason it refuses.
fn p1_check_frame(op: &str, expected: &Value, a: &Actual) -> Result<Vec<usize>, String> {
    let old: Vec<&str> = expected["lines"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let new: Vec<String> = a.ansi.iter().map(|l| strip_ansi(l)).collect();
    if old.len() != new.len() {
        return Err(format!("{} rows, expected {}", new.len(), old.len()));
    }
    let header = p1_rows(op, &old);
    let mut changed = vec![];
    for (i, (o, n)) in old.iter().zip(&new).enumerate() {
        if o != n {
            if !header.contains(&i) {
                return Err(format!(
                    "row {i} changed outside the header: {o:?} -> {n:?}"
                ));
            }
            changed.push(i);
        }
    }
    for (k, v) in &a.json {
        if expected.get(*k).unwrap_or(&Value::Null) != v
            && !(v.is_array() && v.as_array().unwrap().is_empty() && expected.get(*k).is_none())
        {
            return Err(format!("{k} changed (layout or hit boxes)"));
        }
    }
    Ok(changed)
}

fn p1_check_helper(name: &str, expected: &Value, a: &Actual) -> Result<(), String> {
    let Some((_, keys)) = P1_HELPER_KEYS.iter().find(|(n, _)| *n == name) else {
        return Err("not a colour-bearing helper case".into());
    };
    let got = a.json.get("helper").ok_or("no actual output recorded")?;
    if without_keys(got, keys) == without_keys(expected, keys) {
        Ok(())
    } else {
        Err(format!("differs outside the colour keys {keys:?}"))
    }
}

fn stamp_pending(dir: &std::path::Path, stamp: &str) {
    let mut m = read(dir.join("meta.json"));
    m["reviewed_by"] = json!(stamp);
    std::fs::write(
        dir.join("meta.json"),
        format!("{}\n", serde_json::to_string_pretty(&m).unwrap()),
    )
    .unwrap();
}

#[test]
fn render_golden() {
    let allowed = divergences();
    let mode = std::env::var("SYNTHESIS_UPDATE_GOLDEN").unwrap_or_default();
    let update = mode == "1";
    // `redesign-new-tui` (the new TUI of docs/specs/tui-design.md §16) is both gates in one run:
    // `frame` cases may change layout and colour (the P2 check), every other op may change colour
    // only (the P1 check). Stamp: NEW_STAMP.
    // `redesign-html` is the same run, and additionally accepts TEXT changes of any frame-like case
    // (the owner's HTML design replaced wording and layout; the Node oracle no longer applies).
    let html = mode == "redesign-html";
    let nt = mode == "redesign-new-tui" || html;
    let p1 = mode == "redesign-p1" || nt;
    let p2 = mode == "redesign-p2" || nt;
    let stamp1 = if nt { NEW_STAMP } else { P1_STAMP };
    let (mut p2_done, mut p2_new, mut p2_refused) = (
        Vec::<String>::new(),
        Vec::<String>::new(),
        Vec::<(String, String)>::new(),
    );
    let (mut rewritten, mut unexplained) = (Vec::<String>::new(), Vec::<String>::new());
    // P1 summary: (case, what changed), refusals with the reason, divergent cases left alone
    let (mut p1_done, mut p1_refused, mut p1_skipped) = (
        Vec::<(String, String)>::new(),
        Vec::<(String, String)>::new(),
        Vec::<String>::new(),
    );
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
        let is_new = p2 && !dir.join("expected.json").exists();
        if is_new {
            p2_new_case(dir, &input);
        }
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
        if p2 && checks.iter().any(|(_, r)| r.is_err()) {
            let a = ACTUAL.with(|a| std::mem::take(&mut *a.borrow_mut()));
            match p2_check_frame(&input, &a) {
                Ok(()) => {
                    rewrite_expected(dir, &read(dir.join("expected.json")), &a);
                    let mut m = read(dir.join("meta.json"));
                    m["reviewed_by"] = json!(if nt { NEW_STAMP } else { P2_STAMP });
                    std::fs::write(
                        dir.join("meta.json"),
                        format!("{}\n", serde_json::to_string_pretty(&m).unwrap()),
                    )
                    .unwrap();
                    if is_new {
                        p2_new.push(name.clone())
                    } else {
                        p2_done.push(name.clone())
                    }
                    checks = vec![("json", Ok(()))];
                }
                // the new-TUI gate leaves a non-frame case to its colour check below
                Err(_) if nt && !is_new => ACTUAL.with(|x| *x.borrow_mut() = a),
                Err(why) => {
                    if is_new {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    p2_refused.push((name.clone(), why))
                }
            }
        }
        if p1 && checks.iter().any(|(_, r)| r.is_err()) {
            if allowed.keys().any(|k| k.starts_with(&id)) {
                let a = ACTUAL.with(|a| std::mem::take(&mut *a.borrow_mut()));
                let want = read(dir.join("expected.json"));
                match (!a.ansi.is_empty())
                    .then(|| p1_sync_rows(dir, input["op"].as_str().unwrap(), &want, &a))
                {
                    Some(Ok(rows)) if !rows.is_empty() => {
                        stamp_pending(dir, stamp1);
                        p1_done.push((name.clone(), format!("divergent case, rows {rows:?} only")));
                    }
                    _ => p1_skipped.push(name.clone()),
                }
            } else {
                let a = ACTUAL.with(|a| std::mem::take(&mut *a.borrow_mut()));
                let want = read(dir.join("expected.json"));
                let verdict = if a.ansi.is_empty() {
                    p1_check_helper(&name, &want, &a).map(|()| {
                        let mut v = a.json["helper"].clone();
                        if let (Some(o), Some(n)) = (want.as_object(), v.as_object_mut()) {
                            for (k, x) in o {
                                n.entry(k.clone()).or_insert_with(|| x.clone());
                            }
                        }
                        std::fs::write(
                            dir.join("expected.json"),
                            format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
                        )
                        .unwrap();
                        "colour keys".to_string()
                    })
                } else {
                    p1_check_frame(input["op"].as_str().unwrap(), &want, &a).map(|hdr| {
                        rewrite_expected(dir, &want, &a);
                        if hdr.is_empty() {
                            "colour".to_string()
                        } else {
                            format!("colour + header rows {hdr:?}")
                        }
                    })
                };
                let verdict = match verdict {
                    Err(_) if html && !a.ansi.is_empty() => {
                        rewrite_expected(dir, &want, &a);
                        Ok("text (HTML design)".to_string())
                    }
                    v => v,
                };
                match verdict {
                    Ok(what) => {
                        stamp_pending(dir, stamp1);
                        p1_done.push((name.clone(), what));
                        checks = vec![("json", Ok(()))];
                    }
                    Err(why) => p1_refused.push((name.clone(), why)),
                }
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
    if p2 && !nt {
        println!(
            "SYNTHESIS_UPDATE_GOLDEN=redesign-p2: rewrote {} case(s), created {} new case(s), marked reviewed_by = {P2_STAMP:?}\nREFUSED {} (outside the P2 scope or not a layout frame):\n  {}\nrewritten:\n  {}\nnew:\n  {}",
            p2_done.len(),
            p2_new.len(),
            p2_refused.len(),
            p2_refused
                .iter()
                .map(|(c, w)| format!("{c}: {w}"))
                .collect::<Vec<_>>()
                .join("\n  "),
            p2_done.join("\n  "),
            p2_new.join("\n  ")
        );
    }
    if nt {
        println!(
            "SYNTHESIS_UPDATE_GOLDEN=redesign-new-tui: layout frames rewritten {} (new {}), colour rewrites {}, marked reviewed_by = {NEW_STAMP:?}\nREFUSED by the colour check (text changed outside a frame case):\n  {}",
            p2_done.len(),
            p2_new.len(),
            p1_done.len(),
            p1_refused
                .iter()
                .map(|(c, w)| format!("{c}: {w}"))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    } else if p1 {
        let n = |w: &str| p1_done.iter().filter(|(_, x)| x.starts_with(w)).count();
        println!(
            "SYNTHESIS_UPDATE_GOLDEN=redesign-p1: rewrote {} case(s) ({} colour only, {} colour + header rows, {} helper colour keys), marked reviewed_by = {P1_STAMP:?}\nREFUSED {} case(s) (outside the P1 scope, left unchanged, still failing):\n  {}\nskipped {} case(s) listed in golden_divergences.txt (text differs from the oracle already):\n  {}\nrewritten:\n  {}",
            p1_done.len(),
            p1_done.iter().filter(|(_, x)| x == "colour").count(),
            n("colour +"),
            n("colour keys"),
            p1_refused.len(),
            p1_refused
                .iter()
                .map(|(c, w)| format!("{c}: {w}"))
                .collect::<Vec<_>>()
                .join("\n  "),
            p1_skipped.len(),
            p1_skipped.join("\n  "),
            p1_done
                .iter()
                .map(|(c, w)| format!("{c} [{w}]"))
                .collect::<Vec<_>>()
                .join("\n  ")
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

/// A full frame under NO_COLOR and 16 colours carries no colour code the terminal could not draw.
#[test]
fn mono_and_16_colour_frames_use_only_their_codes() {
    use synthesis_tui_base::sgr::ColorMode as M;
    let input = read(root().join("../../tests/golden/render/dash-busy-tools-120x40/input.json"));
    for (theme, mode) in [(ThemeId::Mono, M::Mono), (ThemeId::Ansi16, M::Ansi16)] {
        let _scope = ConfigScope::new(RenderConfig {
            theme,
            ascii: false,
        });
        let st = hydrate_state(&input["state"]);
        let chars = st.editor.chars.clone();
        let ds = st.dash(&chars, st.editor.cur);
        let fr = render_frame(&ds, 120, 40, 3, input["now"].as_i64().unwrap());
        for line in &fr.lines {
            let out = encode(line, mode);
            for seq in out.split('\x1b').skip(1) {
                let Some(params) = seq.strip_prefix('[').and_then(|s| s.split('m').next()) else {
                    continue;
                };
                for p in params.split(';').filter(|p| !p.is_empty()) {
                    let n: u32 = p.parse().unwrap_or(0);
                    let colour =
                        (30..=38).contains(&n) || (40..=48).contains(&n) || (90..=107).contains(&n);
                    match mode {
                        M::Mono => assert!(!colour, "{theme:?}: colour code {n} in {out:?}"),
                        _ => assert!(n != 38 && n != 48, "{theme:?}: {out:?}"),
                    }
                }
            }
        }
    }
}

/// Frames in the non-default themes: `tests/golden/render-themes/<theme>__<case>/expected.ansi.json`
/// holds the raw rows of the input of `render/<case>` drawn in `<theme>` (Classic, the default, is
/// covered by the `render` goldens). `SYNTHESIS_UPDATE_GOLDEN=themes` rewrites them; review the diff.
#[test]
fn theme_frames_golden() {
    let base = root().join("../../tests/golden");
    let update = std::env::var("SYNTHESIS_UPDATE_GOLDEN").is_ok_and(|v| v == "themes");
    let mut dirs: Vec<_> = std::fs::read_dir(base.join("render-themes"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty());
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let (theme, case) = name.split_once("__").expect("<theme>__<case>");
        let theme = ThemeId::parse(theme).unwrap_or_else(|| panic!("{name}: unknown theme"));
        let input = read(base.join("render").join(case).join("input.json"));
        let _scope = ConfigScope::new(RenderConfig {
            theme,
            ascii: false,
        });
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
        if update {
            let v =
                json!({"theme": name.split_once("__").unwrap().0, "case": case, "lines": fr.lines});
            std::fs::write(
                dir.join("expected.ansi.json"),
                format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
            )
            .unwrap();
        }
        let want = read(dir.join("expected.ansi.json"));
        assert_eq!(json!(fr.lines), want["lines"], "{name}");
    }
}

/// Visual loop (not a check): with `SYNTHESIS_FRAMES_DIR=<dir>` writes the scripted frame of the
/// golden `layout-100x30-all` data at the five reference sizes, both screens, themes synthesis and
/// system, as `<theme>-<screen>-<cols>x<rows>.ansi` (raw SGR rows). `tools/frames-to-png.mjs` turns
/// them into PNGs.
#[test]
fn write_visual_frames() {
    let Ok(dir) = std::env::var("SYNTHESIS_FRAMES_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = read(root().join("../../tests/golden/render/layout-100x30-all/input.json"));
    for (name, theme) in [
        ("synthesis", ThemeId::Synthesis),
        ("system", ThemeId::System),
    ] {
        {
            for (cols, rows) in [(80, 24), (100, 30), (120, 36), (160, 45), (200, 50)] {
                let _scope = ConfigScope::new(RenderConfig {
                    theme,
                    ascii: false,
                });
                let mut st = hydrate_state(&input["state"]);
                st.view.panels = Panels::all();
                let chars = st.editor.chars.clone();
                let ds = st.dash(&chars, st.editor.cur);
                let fr = render_frame(&ds, cols, rows, 0, input["now"].as_i64().unwrap());
                let lines = paint_background(fr.lines);
                std::fs::write(
                    dir.join(format!("{name}-{}-{cols}x{rows}.ansi", "view")),
                    lines.join("\n") + "\n",
                )
                .unwrap();
            }
        }
    }
}

/// The centre tabs (diff, terminal, browser, file explorer) with a small working-tree fixture, as
/// `synthesis-<tab>-160x45.ansi` for `tools/frames-to-png.mjs`.
#[test]
fn write_visual_tab_frames() {
    let Ok(dir) = std::env::var("SYNTHESIS_FRAMES_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = read(root().join("../../tests/golden/render/layout-100x30-all/input.json"));
    let diff = "diff --git a/src/lib/auth.ts b/src/lib/auth.ts\n--- a/src/lib/auth.ts\n+++ b/src/lib/auth.ts\n@@ -12,3 +12,4 @@ export async function createSession(user: User) {\n   const sessionId = generateSessionId()\n-  const session = await db.session.create({ data: { id: sessionId } })\n+  const session = await db.session.create({\n+    data: { id: sessionId, userId: user.id },\n+  })\n   return session\ndiff --git a/src/lib/session.ts b/src/lib/session.ts\n--- a/src/lib/session.ts\n+++ b/src/lib/session.ts\n@@ -3,3 +3,3 @@\n export const SESSION_DURATION = 1000 * 60\n-  return Math.random().toString(36)\n+  return crypto.randomUUID()\n }";
    let row = |depth, name: &str, dir, status| TreeRow {
        depth,
        name: name.into(),
        dir,
        status,
    };
    for (name, centre) in [
        ("diff", Centre::Diff),
        ("terminal", Centre::Terminal),
        ("browser", Centre::Browser),
        ("files", Centre::Files),
    ] {
        let _scope = ConfigScope::new(RenderConfig {
            theme: ThemeId::Synthesis,
            ascii: false,
        });
        let mut st = hydrate_state(&input["state"]);
        st.view.panels = Panels::all();
        st.view.centre = centre;
        st.view.project = ProjectData {
            loaded: true,
            rows: vec![
                row(0, "src", true, None),
                row(1, "lib", true, None),
                row(2, "auth.ts", false, Some('M')),
                row(2, "session.ts", false, Some('M')),
                row(0, "tests", true, None),
                row(1, "auth.test.ts", false, Some('A')),
                row(0, "README.md", false, None),
            ],
            diff: diff.split('\n').map(String::from).collect(),
            ..Default::default()
        };
        let chars = st.editor.chars.clone();
        let ds = st.dash(&chars, st.editor.cur);
        let fr = render_frame(&ds, 160, 45, 0, input["now"].as_i64().unwrap());
        std::fs::write(
            dir.join(format!("synthesis-{name}-160x45.ansi")),
            paint_background(fr.lines).join("\n") + "\n",
        )
        .unwrap();
    }
}

/// The `system` theme paints nothing: not one `48;` SGR (truecolor or 256-colour) in any frame, in
/// either screen, at any size, with every panel on or off. Positive control: the `synthesis` theme
/// paints the token background `#0B0C0F` and the card surface `#15171B`.
#[test]
fn system_theme_emits_no_background_and_synthesis_paints_the_tokens() {
    let input = read(root().join("../../tests/golden/render/layout-100x30-all/input.json"));
    let frames = |theme: ThemeId, mode: ColorMode| -> Vec<String> {
        let mut all = vec![];
        {
            for (cols, rows) in [
                (40, 12),
                (59, 20),
                (80, 24),
                (100, 30),
                (160, 45),
                (200, 50),
            ] {
                for panels in [Panels::all(), Panels::none(), Panels::default()] {
                    let _scope = ConfigScope::new(RenderConfig {
                        theme,
                        ascii: false,
                    });
                    let mut st = hydrate_state(&input["state"]);
                    st.view.panels = panels;
                    let chars = st.editor.chars.clone();
                    let ds = st.dash(&chars, st.editor.cur);
                    let fr = render_frame(&ds, cols, rows, 3, input["now"].as_i64().unwrap());
                    all.extend(paint_background(fr.lines).iter().map(|l| encode(l, mode)));
                }
            }
        }
        all
    };
    for mode in [ColorMode::TrueColor, ColorMode::Ansi256] {
        let sys = frames(ThemeId::System, mode).join("\n");
        assert!(
            !sets_background(&sys),
            "system emitted a background ({mode:?})"
        );
        assert!(sys.contains("38;"), "system must still colour text");
    }
    let syn = frames(ThemeId::Synthesis, ColorMode::TrueColor).join("\n");
    assert!(sets_background(&syn));
    assert!(
        syn.contains(";48;2;11;12;15") || syn.contains("[48;2;11;12;15"),
        "app background missing"
    );
    assert!(syn.contains("[48;2;21;23;27"), "card surface missing");
}

/// Does any SGR sequence of `s` select a background colour (`40`-`47`, `48;..`, `100`-`107`)? Reads the
/// parameters one by one, skipping the colour operands of `38;2;r;g;b` and `38;5;n`.
fn sets_background(s: &str) -> bool {
    let mut rest = s;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find(|c: char| !(c.is_ascii_digit() || c == ';')) else {
            break;
        };
        if rest.as_bytes()[end] == b'm' {
            let p: Vec<u32> = rest[..end]
                .split(';')
                .map(|x| x.parse().unwrap_or(0))
                .collect();
            let mut k = 0;
            while k < p.len() {
                match p[k] {
                    40..=48 | 100..=107 => return true,
                    38 => k += if p.get(k + 1) == Some(&2) { 4 } else { 2 },
                    _ => {}
                }
                k += 1;
            }
        }
        rest = &rest[end..];
    }
    false
}

/// Every theme, in Unicode and ASCII, on both screens: each row is exactly `cols` cells wide and the
/// frame has `rows` rows (the card chrome never breaks the grid); ASCII cards use `+ - |`.
#[test]
fn every_theme_and_glyph_set_keeps_the_grid() {
    use synthesis_tui_base::text::width;
    let input = read(root().join("../../tests/golden/render/layout-100x30-all/input.json"));
    for theme in [
        ThemeId::Synthesis,
        ThemeId::System,
        ThemeId::Classic,
        ThemeId::Mono,
        ThemeId::Ansi16,
    ] {
        for ascii in [false, true] {
            {
                for (cols, rows) in [(40, 12), (60, 20), (80, 24), (100, 30), (200, 50)] {
                    let _scope = ConfigScope::new(RenderConfig { theme, ascii });
                    let mut st = hydrate_state(&input["state"]);
                    st.view.panels = Panels::all();
                    let chars = st.editor.chars.clone();
                    let ds = st.dash(&chars, st.editor.cur);
                    let fr = render_frame(&ds, cols, rows, 0, input["now"].as_i64().unwrap());
                    assert_eq!(fr.lines.len(), rows);
                    for (i, l) in fr.lines.iter().enumerate() {
                        assert_eq!(
                            width(l),
                            cols,
                            "{theme:?} ascii={ascii} {cols}x{rows} row {i}"
                        );
                    }
                    if ascii && cols >= 100 {
                        let all = strip_ansi(&fr.lines.join("\n"));
                        assert!(all.contains("+-"), "{theme:?}"); // (the chat's prompt box keeps its Unicode corners)
                    }
                }
            }
        }
    }
}
