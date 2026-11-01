//! Session / activity model: port of `parser.js` (`createSession`, `applyEntry`, `sessionPhase`,
//! tool classification) plus a builder from `synthesis_events` streams (the Rust UI is a client of the
//! event stream, so events are mapped onto the same transcript-shaped entries).
//!
//! Shortcut: `serde_json` keeps object keys sorted, Node keeps insertion order; this only
//! matters for `summarize_tool`'s "first string input" fallback and `serde_json::to_string`.

use crate::util::{basename, short};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::OnceLock;

pub const FEED_LIMIT: usize = 200;
pub const TIMELINE_LIMIT: usize = 120;

/// Insertion-ordered map with O(1) lookup (JS `Map`).
#[derive(Clone, Debug)]
pub struct IdxMap<V> {
    pub items: Vec<V>,
    keys: Vec<String>,
    idx: HashMap<String, usize>,
}
impl<V> Default for IdxMap<V> {
    fn default() -> Self {
        Self {
            items: vec![],
            keys: vec![],
            idx: HashMap::new(),
        }
    }
}
impl<V> IdxMap<V> {
    pub fn get(&self, k: &str) -> Option<&V> {
        self.idx.get(k).map(|&i| &self.items[i])
    }
    pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
        let i = *self.idx.get(k)?;
        Some(&mut self.items[i])
    }
    pub fn contains(&self, k: &str) -> bool {
        self.idx.contains_key(k)
    }
    pub fn insert(&mut self, k: &str, v: V) {
        if let Some(&i) = self.idx.get(k) {
            self.items[i] = v;
        } else {
            self.idx.insert(k.to_string(), self.items.len());
            self.keys.push(k.to_string());
            self.items.push(v);
        }
    }
    pub fn values(&self) -> std::slice::Iter<'_, V> {
        self.items.iter()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
}
impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_create
    }
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub is_error: bool,
    pub cat: &'static str,
    pub comp_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CurrentTool {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub start: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct AgentRec {
    pub tool_use_id: String,
    pub agent_id: Option<String>,
    pub kind: String,
    pub description: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub status: String,
    pub background: bool,
    pub tokens: Tokens,
    pub usage_by_id: HashMap<String, Tokens>,
    pub tool_count: u32,
    pub current_tool: Option<CurrentTool>,
    pub last_activity: Option<i64>,
    pub context_used: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Component {
    pub key: String,
    pub kind: String,
    pub name: String,
    pub count: u32,
    pub open: u32,
    pub first_seen: Option<i64>,
    pub last_start: Option<i64>,
    pub last_end: Option<i64>,
    pub error: bool,
    pub sub: String,
    pub event: Option<String>,
    pub blocked: u32,
    pub last_blocked_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct Todo {
    pub id: String,
    pub content: String,
    #[serde(rename = "activeForm")]
    pub active_form: String,
    pub status: String,
    #[serde(skip)]
    pub pending_tool_use_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FeedItem {
    pub ts: Option<i64>,
    pub kind: &'static str,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Verify {
    pub name: String,
    pub summary: String,
    pub ok: bool,
    pub ts: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub project: String,
    pub model: String,
    pub version: String,
    pub git_branch: String,
    pub started_at: Option<i64>,
    pub last_activity: Option<i64>,
    pub last_assistant_at: Option<i64>,
    pub last_user_at: Option<i64>,
    pub last_stop_reason: Option<String>,
    pub prompts: u32,
    pub last_prompt_at: Option<i64>,
    pub last_prompt: String,
    pub memory_writes: u32,
    pub last_verify: Option<Verify>,
    pub tokens: Tokens,
    pub usage_by_id: HashMap<String, Tokens>,
    pub context_used: u64,
    pub tools: IdxMap<Tool>,
    pub tool_counts: Vec<(String, u32)>,
    pub skills: Vec<(String, u32)>,
    pub agents: IdxMap<AgentRec>,
    pub components: IdxMap<Component>,
    pub todos: Vec<Todo>,
    pub feed: Vec<FeedItem>,
    pub activity: Vec<FeedItem>,
    pub turn_replied: bool,
    pub timeline: Vec<(i64, u64)>,
    pub errors: u32,
    /// Set after a compaction until the next request reports usage (display only).
    pub context_display_reset: bool,
    pub context_estimated: bool,
}

impl Session {
    pub fn new(id: &str) -> Self {
        Session {
            id: id.to_string(),
            ..Default::default()
        }
    }
}

fn bump(v: &mut Vec<(String, u32)>, k: &str) {
    match v.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 += 1,
        None => v.push((k.to_string(), 1)),
    }
}

pub fn parse_ts(s: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(s, &Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}
fn truthy_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub fn summarize_tool(name: &str, input: &Value) -> String {
    let s = |k: &str| str_of(input, k);
    let first = |a: &'static str, b: &'static str| -> String {
        truthy_str(input, a).unwrap_or_else(|| s(b)).to_string()
    };
    match name {
        "Bash" | "PowerShell" => short(&first("command", "description"), 80),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            basename(&first("file_path", "notebook_path"))
        }
        "Grep" | "Glob" => short(s("pattern"), 80),
        "WebSearch" => short(s("query"), 80),
        "WebFetch" => short(s("url"), 80),
        "Agent" | "Task" => short(
            &format!(
                "{}: {}",
                truthy_str(input, "subagent_type").unwrap_or("general-purpose"),
                s("description")
            ),
            80,
        ),
        "Skill" => short(
            &format!(
                "{}{}",
                s("skill"),
                truthy_str(input, "args").map_or(String::new(), |a| format!(" {a}"))
            ),
            80,
        ),
        "TodoWrite" => format!(
            "{} todos",
            input
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        ),
        "TaskCreate" => short(
            truthy_str(input, "subject")
                .or(truthy_str(input, "title"))
                .unwrap_or_else(|| s("description")),
            80,
        ),
        "TaskUpdate" => {
            let id = input
                .get("taskId")
                .or(input.get("id"))
                .filter(|v| !v.is_null())
                .map_or("?".to_string(), json_str);
            short(
                &format!(
                    "#{id} → {}",
                    truthy_str(input, "status").unwrap_or("update")
                ),
                80,
            )
        }
        "SendMessage" => short(
            &format!(
                "→ {}: {}",
                json_str(input.get("to").unwrap_or(&Value::Null)),
                truthy_str(input, "summary").unwrap_or_else(|| s("message"))
            ),
            80,
        ),
        _ => {
            let first = input
                .as_object()
                .and_then(|o| o.values().find_map(Value::as_str))
                .unwrap_or("");
            short(first, 80)
        }
    }
}

/// JS `String(v)` for the scalar values tool inputs carry.
pub fn json_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}

fn re(cell: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(p).unwrap())
}

pub fn classify_tool(name: &str, input: &Value) -> &'static str {
    static VERIFY: OnceLock<Regex> = OnceLock::new();
    static MEMORY: OnceLock<Regex> = OnceLock::new();
    let plan = [
        "Read",
        "Grep",
        "Glob",
        "LS",
        "TodoWrite",
        "TaskCreate",
        "TaskUpdate",
        "TaskList",
        "TaskGet",
        "EnterPlanMode",
        "ExitPlanMode",
        "AskUserQuestion",
    ];
    if matches!(name, "Agent" | "Task" | "Skill" | "SendMessage") {
        return "orchestrate";
    }
    if plan.contains(&name) {
        return "plan";
    }
    let path = truthy_str(input, "file_path").unwrap_or_else(|| str_of(input, "notebook_path"));
    if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
        && re(
            &MEMORY,
            r"(?i)(^|[\\/])(memory|MEMORY\.md|CLAUDE\.md|lessons\.md|todo\.md)([\\/]|$)",
        )
        .is_match(path)
    {
        return "learn";
    }
    if matches!(name, "Bash" | "PowerShell") {
        let text = format!(
            "{} {}",
            str_of(input, "description"),
            str_of(input, "command")
        );
        if re(
            &VERIFY,
            r"(?i)(?-u:\b)(test\w*|lint\w*|eslint|tsc|build|check\w*|verify|pytest|jest|vitest|mocha|go vet|cargo test|terraform plan)(?-u:\b)",
        )
        .is_match(&text)
        {
            return "verify";
        }
    }
    "execute"
}

/// `isSoftToolError`: refusals and inapplicable calls are not failures.
pub fn is_soft_tool_error(text: &str) -> bool {
    static SOFT: OnceLock<Vec<Regex>> = OnceLock::new();
    let v = SOFT.get_or_init(|| {
        [
            r"has not been read in this conversation \(or changed since\)\. Read it first\.",
            r#"^Edit: "old_string" (was not found in|matches \d+ times in) "#,
            r#"^Edit: "old_string" and "new_string" must be different\."#,
            r#"^NotebookEdit: (no cell "|"cell_id" is required for )"#,
            r"^(Read|Edit|NotebookEdit): cannot access .*ENOENT",
            r"^(Read|Edit|Write): .* is a directory\.",
            r"^The user (doesn't want to proceed|declined this read|did not answer)",
            r"^Reads outside the working directories are blocked",
            r"^Interrupted( before this tool ran)?\.",
            r"^Auto mode (could not evaluate|is paused)",
            r"^Denied by auto mode classifier",
            r"^Removing a critical path was denied",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    v.iter().any(|r| r.is_match(text))
}

fn text_of_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| str_of(b, "text"))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn is_synthetic_prompt(text: &str) -> bool {
    let t = text.trim_matches(synthesis_tui_base::text::is_js_space);
    t.is_empty()
        || [
            "<system-reminder>",
            "<local-command",
            "<command-",
            "[Request interrupted",
            "Caveat:",
        ]
        .iter()
        .any(|p| t.starts_with(p))
}

fn push_feed(s: &mut Session, ts: Option<i64>, kind: &'static str, text: String) {
    s.feed.push(FeedItem { ts, kind, text });
    if s.feed.len() > FEED_LIMIT {
        s.feed.drain(..s.feed.len() - FEED_LIMIT);
    }
}
fn push_activity(s: &mut Session, ts: Option<i64>, kind: &'static str, text: String) {
    s.activity.push(FeedItem { ts, kind, text });
    if s.activity.len() > FEED_LIMIT {
        s.activity.drain(..s.activity.len() - FEED_LIMIT);
    }
}

fn usage_of(u: &Value) -> Tokens {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Tokens {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_create: n("cache_creation_input_tokens"),
    }
}

/// Streamed messages repeat one id and usage: replace rather than add.
fn apply_usage(
    tokens: &mut Tokens,
    by_id: &mut HashMap<String, Tokens>,
    ctx: &mut u64,
    msg_id: Option<&str>,
    usage: Option<&Value>,
) {
    let (Some(id), Some(u)) = (
        msg_id.filter(|s| !s.is_empty()),
        usage.filter(|u| !u.is_null()),
    ) else {
        return;
    };
    let next = usage_of(u);
    let prev = by_id.get(id).cloned().unwrap_or_default();
    tokens.input = tokens.input + next.input - prev.input;
    tokens.output = tokens.output + next.output - prev.output;
    tokens.cache_read = tokens.cache_read + next.cache_read - prev.cache_read;
    tokens.cache_create = tokens.cache_create + next.cache_create - prev.cache_create;
    let c = next.total();
    if c > 0 {
        *ctx = c;
    }
    by_id.insert(id.to_string(), next);
}

fn touch_component(s: &mut Session, kind: &str, name: &str, ts: Option<i64>) -> String {
    let key = format!("{kind}:{name}");
    if !s.components.contains(&key) {
        s.components.insert(
            &key,
            Component {
                key: key.clone(),
                kind: kind.into(),
                name: name.into(),
                first_seen: ts,
                ..Default::default()
            },
        );
    }
    let c = s.components.get_mut(&key).unwrap();
    c.count += 1;
    c.last_start = ts;
    c.error = false;
    key
}

fn handle_hook(s: &mut Session, att: &Value, ts: Option<i64>) {
    let typ = str_of(att, "type");
    let name = truthy_str(att, "hookName")
        .or(truthy_str(att, "hookEvent"))
        .unwrap_or(typ)
        .to_string();
    let key = touch_component(s, "hook", &name, ts);
    let exit = att.get("exitCode").and_then(Value::as_i64);
    let failed = ["error", "blocking", "cancel"]
        .iter()
        .any(|w| typ.contains(w))
        || exit.is_some_and(|e| e != 0);
    let ev = truthy_str(att, "hookEvent");
    let c = s.components.get_mut(&key).unwrap();
    c.last_end = ts;
    c.event = Some(ev.map_or_else(
        || name.split(':').next().unwrap_or("").to_string(),
        String::from,
    ));
    c.error = failed;
    if failed {
        c.blocked += 1;
        c.last_blocked_at = ts;
    }
    c.sub = format!(
        "{}{}",
        ev.unwrap_or("hook"),
        exit.map_or(String::new(), |e| format!(" · exit {e}"))
    );
    if failed {
        let t = typ.strip_prefix("hook_").unwrap_or(typ);
        push_feed(s, ts, "hook", format!("Hook {name} {t}"));
    }
}

fn handle_task_tools(s: &mut Session, block: &Value) {
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    let name = str_of(block, "name");
    let st = |k: &str| truthy_str(&input, k).map(String::from);
    if name == "TodoWrite" && input.get("todos").is_some_and(Value::is_array) {
        s.todos = input["todos"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, t)| Todo {
                id: t
                    .get("id")
                    .filter(|v| !v.is_null())
                    .map_or((i + 1).to_string(), json_str),
                content: truthy_str(t, "content")
                    .or(truthy_str(t, "subject"))
                    .unwrap_or("")
                    .into(),
                active_form: str_of(t, "activeForm").into(),
                status: truthy_str(t, "status").unwrap_or("pending").into(),
                pending_tool_use_id: None,
            })
            .collect();
    } else if name == "TaskCreate" {
        let content = st("subject")
            .or(st("title"))
            .or(st("description"))
            .unwrap_or_default();
        s.todos.push(Todo {
            id: (s.todos.len() + 1).to_string(),
            content,
            active_form: st("activeForm").unwrap_or_default(),
            status: "pending".into(),
            pending_tool_use_id: Some(str_of(block, "id").to_string()),
        });
    } else if name == "TaskUpdate" {
        let id = input
            .get("taskId")
            .or(input.get("id"))
            .filter(|v| !v.is_null())
            .map_or(String::new(), json_str);
        if let Some(t) = s.todos.iter_mut().find(|t| t.id == id) {
            if let Some(v) = st("status") {
                t.status = v;
            }
            if let Some(v) = st("subject") {
                t.content = v;
            }
            if let Some(v) = st("activeForm") {
                t.active_form = v;
            }
        }
    }
}

fn handle_assistant(s: &mut Session, entry: &Value, ts: Option<i64>) {
    let msg = entry.get("message").cloned().unwrap_or(Value::Null);
    if let Some(m) = truthy_str(&msg, "model").filter(|m| *m != "<synthetic>") {
        s.model = m.into();
    }
    apply_usage(
        &mut s.tokens,
        &mut s.usage_by_id,
        &mut s.context_used,
        msg.get("id").and_then(Value::as_str),
        msg.get("usage"),
    );
    s.last_assistant_at = ts;
    if let Some(r) = truthy_str(&msg, "stop_reason") {
        s.last_stop_reason = Some(r.into());
    }
    let empty = vec![];
    for block in msg
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
        match str_of(block, "type") {
            "tool_use" => {
                let id = str_of(block, "id").to_string();
                if s.tools.contains(&id) {
                    continue;
                }
                let name = str_of(block, "name").to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                let summary = summarize_tool(&name, &input);
                let cat = classify_tool(&name, &input);
                let mut tool = Tool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: summary.clone(),
                    start: ts,
                    end: None,
                    is_error: false,
                    cat,
                    comp_key: None,
                };
                if cat == "learn" {
                    s.memory_writes += 1;
                }
                bump(&mut s.tool_counts, &name);
                if name != "Agent" && name != "Task" {
                    let (kind, cname, sub_tool) =
                        if name == "Skill" && truthy_str(&input, "skill").is_some() {
                            ("skill", str_of(&input, "skill").to_string(), String::new())
                        } else if let Some(rest) = name.strip_prefix("mcp__") {
                            let mut parts = rest.split("__");
                            let server = parts.next().unwrap_or("");
                            let tool_rest = parts.collect::<Vec<_>>().join("__");
                            (
                                "mcp",
                                if server.is_empty() {
                                    name.clone()
                                } else {
                                    server.into()
                                },
                                tool_rest,
                            )
                        } else {
                            ("tool", name.clone(), String::new())
                        };
                    let key = touch_component(s, kind, &cname, ts);
                    let c = s.components.get_mut(&key).unwrap();
                    c.open += 1;
                    c.sub = if kind == "mcp" {
                        format!("{sub_tool} {summary}")
                    } else {
                        summary.clone()
                    };
                    tool.comp_key = Some(key);
                }
                s.tools.insert(&id, tool);
                if name == "Skill" && truthy_str(&input, "skill").is_some() {
                    let sk = str_of(&input, "skill").to_string();
                    bump(&mut s.skills, &sk);
                    push_feed(s, ts, "skill", format!("Skill {sk}"));
                } else if name == "Agent" || name == "Task" {
                    let kind = truthy_str(&input, "subagent_type").unwrap_or("general-purpose");
                    let desc = short(
                        truthy_str(&input, "description")
                            .unwrap_or_else(|| str_of(&input, "prompt")),
                        60,
                    );
                    s.agents.insert(
                        &id,
                        AgentRec {
                            tool_use_id: id.clone(),
                            agent_id: None,
                            kind: kind.into(),
                            description: desc.clone(),
                            start: ts,
                            end: None,
                            status: "running".into(),
                            background: input.get("run_in_background") != Some(&json!(false)),
                            tokens: Tokens::default(),
                            usage_by_id: HashMap::new(),
                            tool_count: 0,
                            current_tool: None,
                            last_activity: ts,
                            context_used: 0,
                        },
                    );
                    push_feed(s, ts, "agent", format!("Delegate → {kind}: {desc}"));
                } else {
                    push_feed(s, ts, "tool", format!("{name} {summary}"));
                }
                handle_task_tools(s, block);
            }
            "text" => {
                let t = str_of(block, "text");
                let tt = t.trim_matches(synthesis_tui_base::text::is_js_space);
                if !tt.is_empty() && tt != "(empty)" {
                    push_feed(s, ts, "text", short(t, 120));
                    if !s.turn_replied {
                        s.turn_replied = true;
                        push_activity(s, ts, "text", short(t, 120));
                    }
                }
            }
            _ => {}
        }
    }
}

fn handle_user(s: &mut Session, entry: &Value, ts: Option<i64>) {
    let msg = entry.get("message").cloned().unwrap_or(Value::Null);
    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    s.last_user_at = ts;
    let result = entry.get("toolUseResult").cloned().unwrap_or(Value::Null);
    if let Some(arr) = content.as_array() {
        for block in arr {
            if str_of(block, "type") != "tool_result" {
                continue;
            }
            let tid = str_of(block, "tool_use_id").to_string();
            let is_err = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let bcontent = block.get("content").cloned().unwrap_or(Value::Null);
            if let Some(tool) = s.tools.get_mut(&tid)
                && tool.end.is_none()
            {
                tool.end = ts;
                tool.is_error = is_err && !is_soft_tool_error(&text_of_content(&bcontent));
                let (ck, cat, name, summary, terr) = (
                    tool.comp_key.clone(),
                    tool.cat,
                    tool.name.clone(),
                    tool.summary.clone(),
                    tool.is_error,
                );
                if let Some(c) = ck.and_then(|k| s.components.get_mut(&k)) {
                    c.open = c.open.saturating_sub(1);
                    c.last_end = ts;
                    c.error = terr;
                }
                if cat == "verify" {
                    s.last_verify = Some(Verify {
                        name: name.clone(),
                        summary,
                        ok: !terr,
                        ts,
                    });
                }
                if terr {
                    s.errors += 1;
                    let tx = text_of_content(&bcontent);
                    let shown = if tx.is_empty() {
                        "[object Object]".to_string()
                    } else {
                        tx
                    };
                    push_feed(
                        s,
                        ts,
                        "error",
                        format!("{name} failed: {}", short(&shown, 80)),
                    );
                }
            }
            if let Some(agent) = s.agents.get_mut(&tid) {
                if let Some(a) = truthy_str(&result, "agentId") {
                    agent.agent_id = Some(a.into());
                }
                if str_of(&result, "status") == "async_launched" {
                    agent.background = true;
                } else if agent.end.is_none() {
                    agent.end = ts;
                    agent.status = if is_err { "failed" } else { "done" }.into();
                    let k = agent.kind.clone();
                    push_feed(s, ts, "agent", format!("{k} finished"));
                }
            }
            if let Some(todo) = s
                .todos
                .iter_mut()
                .find(|t| t.pending_tool_use_id.as_deref() == Some(tid.as_str()))
            {
                let real = result
                    .pointer("/task/id")
                    .or(result.get("taskId"))
                    .or(result.get("id"))
                    .filter(|v| !v.is_null());
                if let Some(r) = real {
                    todo.id = json_str(r);
                }
                todo.pending_tool_use_id = None;
            }
        }
    }
    let truthy = |k: &str| entry.get(k).is_some_and(|v| v.as_bool() == Some(true));
    if !truthy("isMeta") && !truthy("isSidechain") {
        let text = text_of_content(&content);
        let has_result = content
            .as_array()
            .is_some_and(|a| a.iter().any(|b| str_of(b, "type") == "tool_result"));
        static CMD: OnceLock<Regex> = OnceLock::new();
        let cmd = re(&CMD, r"<command-name>\s*([^<]+?)\s*</command-name>").captures(&text);
        if let Some(c) = cmd {
            let name = c[1].to_string();
            let key = touch_component(s, "cmd", &name, ts);
            let comp = s.components.get_mut(&key).unwrap();
            comp.last_end = ts;
            comp.sub = "slash command".into();
            push_feed(s, ts, "cmd", format!("Command {name}"));
        } else if !has_result && !is_synthetic_prompt(&text) {
            s.prompts += 1;
            s.last_prompt_at = ts;
            s.last_prompt = short(&text, 200);
            push_feed(s, ts, "prompt", short(&text, 120));
            s.turn_replied = false;
            push_activity(s, ts, "prompt", short(&text, 120));
        }
    }
}

/// `applyEntry`: feed one transcript entry (Node JSONL shape) into the session.
pub fn apply_entry(s: &mut Session, entry: &Value) {
    if !entry.is_object() || str_of(entry, "type") == "permission-mode" {
        return;
    }
    let ts = entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .filter(|t| *t != 0);
    if let Some(t) = ts {
        if s.started_at.is_none() {
            s.started_at = Some(t);
        }
        s.last_activity = Some(s.last_activity.unwrap_or(0).max(t));
    }
    if let Some(v) = truthy_str(entry, "cwd") {
        s.cwd = v.into();
    }
    if let Some(v) = truthy_str(entry, "version") {
        s.version = v.into();
    }
    if let Some(v) = truthy_str(entry, "gitBranch") {
        s.git_branch = v.into();
    }
    let typ = str_of(entry, "type");
    if typ == "assistant" {
        handle_assistant(s, entry, ts);
    } else if typ == "user" {
        handle_user(s, entry, ts);
    } else if typ == "attachment" {
        let att = entry.get("attachment").cloned().unwrap_or(Value::Null);
        if str_of(&att, "type").starts_with("hook_") {
            handle_hook(s, &att, ts);
        }
    }
    if let (Some(t), "assistant") = (ts, typ) {
        let total = s.tokens.total();
        if s.timeline.last().is_none_or(|l| l.1 != total) {
            s.timeline.push((t, total));
        }
        if s.timeline.len() > TIMELINE_LIMIT {
            s.timeline.remove(0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Tool,
    Agents,
    Thinking,
    Idle,
}

pub struct PhaseInfo<'a> {
    pub phase: Phase,
    pub running: Vec<&'a Tool>,
    pub running_agents: Vec<&'a AgentRec>,
}

pub fn session_phase(s: &Session, now: i64) -> PhaseInfo<'_> {
    let running: Vec<&Tool> = s.tools.values().filter(|t| t.end.is_none()).collect();
    let running_agents: Vec<&AgentRec> = s
        .agents
        .values()
        .filter(|a| a.status == "running")
        .collect();
    let idle_for = now - s.last_activity.unwrap_or(0);
    let phase = if !running.is_empty() {
        Phase::Tool
    } else if !running_agents.is_empty() && idle_for < 10 * 60 * 1000 {
        Phase::Agents
    } else if s.last_stop_reason.as_deref() == Some("end_turn")
        && s.last_assistant_at.unwrap_or(0) >= s.last_user_at.unwrap_or(0)
    {
        Phase::Idle
    } else if idle_for < 5 * 60 * 1000 {
        Phase::Thinking
    } else {
        Phase::Idle
    };
    PhaseInfo {
        phase,
        running,
        running_agents,
    }
}

// ------------------------------------------------------------------ event streams

/// Maps one `synthesis_events` envelope onto transcript-shaped entries (the model is a client of the
/// event stream, not of the Node JSONL files). Events with no UI meaning give no entries.
pub fn entries_from_event(env: &synthesis_events::Envelope) -> Vec<Value> {
    use synthesis_events::Event;
    let ts = env.ts.clone();
    match &env.event {
        Event::SessionStarted {
            workspace, model, ..
        } => {
            vec![
                json!({"type":"system","timestamp":ts,"cwd":workspace.cwd,"message":{"model":model}}),
            ]
        }
        // a harness note to the model (`display_text: ""`) is not something the person said
        Event::UserMessage {
            display_text: Some(d),
            ..
        } if d.is_empty() => vec![],
        Event::UserMessage {
            text, display_text, ..
        } => vec![json!({
            "type":"user","timestamp":ts,
            "message":{"role":"user","content":display_text.clone().unwrap_or_else(|| text.clone())}
        })],
        Event::AssistantMessage {
            message_id,
            content,
            stop_reason,
            usage,
        } => {
            let mut message = json!({"content":content,"stop_reason":stop_reason});
            if let Some(id) = message_id {
                message["id"] = json!(id);
            }
            if let Some(u) = usage {
                message["usage"] = json!({
                    "input_tokens":u.input_tokens,"output_tokens":u.output_tokens,
                    "cache_read_input_tokens":u.cache_read_input_tokens,
                    "cache_creation_input_tokens":u.cache_creation_input_tokens});
            }
            vec![json!({"type":"assistant","timestamp":ts,"message":message})]
        }
        Event::ToolCompleted {
            tool_call_id,
            is_error,
            preview,
            output,
            ..
        } => vec![json!({
            "type":"user","timestamp":ts,
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":tool_call_id,"content":output.as_deref().unwrap_or(preview),"is_error":is_error}]}
        })],
        Event::HookRan {
            event,
            hook_name,
            exit_code,
            outcome,
            ..
        } => {
            let kind = if outcome == "blocked" || outcome == "error" {
                "hook_blocking_error"
            } else {
                "hook_success"
            };
            vec![
                json!({"type":"attachment","timestamp":ts,"attachment":{"type":kind,"hookName":hook_name,"hookEvent":event,"exitCode":exit_code}}),
            ]
        }
        _ => vec![],
    }
}

/// Applies one event; `todo.updated` and `model.changed` set fields directly.
pub fn apply_event(s: &mut Session, env: &synthesis_events::Envelope) {
    use synthesis_events::Event;
    match &env.event {
        Event::TodoUpdated { todos } => {
            s.todos = todos
                .iter()
                .enumerate()
                .map(|(i, t)| Todo {
                    id: t.id.clone().unwrap_or_else(|| (i + 1).to_string()),
                    content: t.content.clone(),
                    active_form: t.active_form.clone(),
                    status: t.status.clone(),
                    pending_tool_use_id: None,
                })
                .collect();
        }
        Event::ModelChanged { model, .. } => s.model = model.clone(),
        Event::SessionStarted { model, .. } => {
            s.model = model.clone();
            for e in entries_from_event(env) {
                apply_entry(s, &e);
            }
        }
        Event::ContextCompacted { .. } => s.context_display_reset = true,
        _ => {
            for e in entries_from_event(env) {
                apply_entry(s, &e);
                if str_of(&e, "type") == "assistant" {
                    s.context_display_reset = false;
                }
            }
        }
    }
    if s.id.is_empty() {
        s.id = env.session_id.clone();
    }
}

/// Builds a session from a whole event stream.
pub fn session_from_events<'a>(
    id: &str,
    events: impl IntoIterator<Item = &'a synthesis_events::Envelope>,
) -> Session {
    let mut s = Session::new(id);
    for e in events {
        apply_event(&mut s, e);
    }
    s
}
