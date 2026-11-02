//! Read-before-write guard for MCP write tools.
//!
//! Servers such as Confluence/Jira replace what they are sent. A model that updates a resource it
//! never read overwrites everything it did not know about (lost update). So a call to an MCP
//! WRITE tool is only run when this task already read the same resource through a READ tool of the
//! same server and nothing wrote to it since; otherwise the call is rejected as a normal tool
//! error that tells the model how to recover (fetch, MERGE, retry). A fetched copy that the
//! update would mostly discard, and destructive tools, force an
//! approval (`Check::Ask`) even when a rule allows the tool.
//!
//! Data used here (tool results) only ever feeds this guard's own string comparisons; it is never
//! handed to the permission engine as a grant. Annotations are hints from the server: a read-only
//! hint is taken as read (user overrides can still force a tool to write), but nothing here
//! relaxes a permission rule.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::util::lock;

/// What the guard needs to know about one MCP tool.
#[derive(Debug, Clone, Default)]
pub struct McpToolInfo {
    /// `mcp__server__tool`.
    pub wire: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
}

/// The live tool list (changes with `tools/list_changed`).
pub trait McpCatalog: Send + Sync {
    fn tools(&self) -> Vec<McpToolInfo>;
}

/// `synt.mcp.servers.<name>.write_tools | read_tools` (server tool names).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerRules {
    pub write_tools: Vec<String>,
    pub read_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardConfig {
    pub servers: BTreeMap<String, ServerRules>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Pass,
    /// Not run; the text goes to the model as a tool error.
    Reject(String),
    /// Run only after a person approved; the text is the reason shown.
    Ask(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Read,
    Write,
    Destructive,
}

struct Read {
    seq: u64,
    server: String,
    ids: Vec<String>,
    text: String,
}

struct Write {
    seq: u64,
    server: String,
    ids: Vec<String>,
}

#[derive(Default)]
struct TaskState {
    seq: u64,
    reads: Vec<Read>,
    writes: Vec<Write>,
}

pub struct McpGuard {
    cfg: GuardConfig,
    catalog: Arc<dyn McpCatalog>,
    tasks: Mutex<HashMap<String, TaskState>>,
}

const READ_VERBS: [&str; 16] = [
    "get", "read", "list", "search", "find", "fetch", "view", "query", "describe", "discover",
    "diff", "show", "lookup", "count", "check", "retrieve",
];
const WRITE_VERBS: [&str; 17] = [
    "update",
    "edit",
    "put",
    "patch",
    "replace",
    "write",
    "set",
    "modify",
    "delete",
    "transition",
    "comment",
    "destructive",
    "remove",
    "restore",
    "merge",
    "move",
    "archive",
];
const BODY_KEYS: [&str; 9] = [
    "body",
    "content",
    "description",
    "text",
    "value",
    "markdown",
    "html",
    "storage",
    "adf",
];
const MAX_READS: usize = 64;
const MAX_TASKS: usize = 16;
const SHRINK_RATIO: f64 = 0.7;
const MIN_BODY: usize = 200;
const DIFF_LINES: usize = 30;
const WRAPPER_ARGS: [&str; 5] = ["arguments", "args", "input", "params", "parameters"];
const WRAPPER_NAME: [&str; 5] = ["tool", "toolName", "tool_name", "name", "operation"];

/// `getPageContent` -> [get, page, content]; `update_page` -> [update, page].
fn words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn has_verb(ws: &[String], verbs: &[&str]) -> bool {
    ws.iter()
        .any(|w| verbs.iter().any(|v| w == v || *w == format!("{v}s")))
}

fn is_id_key(k: &str) -> bool {
    let lc = k.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "id" | "key" | "uri" | "issueidorkey" | "pageid" | "issuekey"
    ) || lc.ends_with("_id")
        || (k.ends_with("Id") && k.len() > 2)
}

fn is_version_key(k: &str) -> bool {
    let lc = k.to_ascii_lowercase();
    [
        "version", "etag", "revision", "ifmatch", "if_match", "updated",
    ]
    .iter()
    .any(|v| lc.contains(v))
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Leaf scalars below `v` (itself included).
fn leaves(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| leaves(x, out)),
        Value::Object(o) => o.values().for_each(|x| leaves(x, out)),
        other => out.extend(scalar(other)),
    }
}

/// Leaf scalars found under keys accepted by `pick`, at any depth.
fn under_keys(v: &Value, pick: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| under_keys(x, pick, out)),
        Value::Object(o) => {
            for (k, x) in o {
                if pick(k) {
                    leaves(x, out);
                } else {
                    under_keys(x, pick, out);
                }
            }
        }
        _ => {}
    }
}

/// Strings that sit under `key` anywhere, longest first candidate for a page body.
fn longest_under(v: &Value, pick: &dyn Fn(&str) -> bool, best: &mut Option<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| longest_under(x, pick, best)),
        Value::Object(o) => {
            for (k, x) in o {
                match x {
                    Value::String(s) if pick(k) => {
                        if best.as_ref().is_none_or(|b| s.len() > b.len()) {
                            *best = Some(s.clone());
                        }
                    }
                    _ => longest_under(x, pick, best),
                }
            }
        }
        _ => {}
    }
}

/// `id` occurs in `text` as a whole token (neighbours are not alphanumeric or `_`).
fn token_in(text: &str, id: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(id).any(|(i, m)| {
        !text[..i].chars().next_back().is_some_and(word)
            && !text[i + m.len()..].chars().next().is_some_and(word)
    })
}

fn mentions(text: &str, id: &str) -> bool {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => {
            let mut ids = vec![];
            under_keys(&v, &is_id_key, &mut ids);
            ids.iter().any(|i| i == id) || (id.len() >= 4 && token_in(text, id))
        }
        _ => id.len() >= 3 && token_in(text, id),
    }
}

/// The tool name and argument object a call really targets: `executeWrite`-style wrappers carry
/// the real tool and its arguments inside their input.
fn target(tool: &str, input: &Value) -> (String, Value) {
    let ws = words(tool);
    if ws.first().is_some_and(|w| w == "execute")
        && let Some(o) = input.as_object()
    {
        let inner = WRAPPER_ARGS
            .iter()
            .find_map(|k| o.get(*k).filter(|v| v.is_object()));
        let name = WRAPPER_NAME
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str));
        if let Some(a) = inner {
            return (name.unwrap_or(tool).to_string(), a.clone());
        }
    }
    (tool.to_string(), input.clone())
}

/// Values of the id-like arguments (top level of the target arguments).
fn id_values(args: &Value) -> Vec<String> {
    let mut v: Vec<String> = args
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| is_id_key(k))
        .filter_map(|(_, x)| scalar(x))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn lines(s: &str) -> Vec<&str> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
}

fn is_heading(l: &str) -> bool {
    let lc = l.to_ascii_lowercase();
    l.starts_with('#')
        || lc.starts_with("<h1")
        || lc.starts_with("<h2")
        || lc.starts_with("<h3")
        || (lc.len() > 3
            && lc.starts_with('h')
            && lc[1..].starts_with(|c: char| c.is_ascii_digit())
            && lc[2..].starts_with('.'))
}

/// Changed lines only (`-` old, `+` new), via an LCS of the two line lists; empty when too big.
fn diff(old: &[&str], new: &[&str]) -> Vec<String> {
    if old.len() * new.len() > 250_000 {
        return vec![];
    }
    let (n, m) = (old.len(), new.len());
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if old[i] == new[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, vec![]);
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            i += 1;
            j += 1;
        } else if j >= m || (i < n && t[i + 1][j] >= t[i][j + 1]) {
            out.push(format!("- {}", old[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", new[j]));
            j += 1;
        }
    }
    out
}

impl crate::harness::Harness {
    /// The MCP read-before-write guard (no-op without one).
    pub(crate) fn mcp_check(
        &self,
        task: &crate::harness::TaskRun,
        tool: &str,
        input: &Value,
    ) -> Check {
        self.tools
            .guard()
            .map_or(Check::Pass, |g| g.check(&task.task_id, tool, input))
    }

    pub(crate) fn mcp_observe(
        &self,
        task: &crate::harness::TaskRun,
        tool: &str,
        input: &Value,
        text: &str,
        is_error: bool,
    ) {
        if let Some(g) = self.tools.guard() {
            g.observe(&task.task_id, tool, input, text, is_error);
        }
    }
}

impl McpGuard {
    pub fn new(cfg: GuardConfig, catalog: Arc<dyn McpCatalog>) -> Self {
        Self {
            cfg,
            catalog,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    fn classify(&self, i: &McpToolInfo, called: &str) -> Class {
        let rules = self.cfg.servers.get(&i.server);
        let listed = |l: fn(&ServerRules) -> &Vec<String>| {
            rules.is_some_and(|r| l(r).iter().any(|t| *t == i.tool || t == called))
        };
        // restrictions win: a tool listed as write stays write even if also listed as read
        if listed(|r| &r.write_tools) {
            return Class::Write;
        }
        if listed(|r| &r.read_tools) {
            return Class::Read;
        }
        if i.read_only == Some(true) {
            return Class::Read;
        }
        if i.destructive == Some(true) {
            return Class::Destructive;
        }
        let ws = words(called);
        if ws.first().is_some_and(|w| READ_VERBS.contains(&w.as_str())) {
            return Class::Read;
        }
        let name_write = has_verb(&ws, &WRITE_VERBS);
        let wrapper_read = ws.first().is_some_and(|w| w == "execute") && has_verb(&ws, &["read"]);
        if wrapper_read {
            return Class::Read;
        }
        let head: String = i.description.chars().take(80).collect();
        if name_write || has_verb(&words(&head), &WRITE_VERBS) {
            return if ws.iter().any(|w| w == "destructive" || w == "delete") {
                Class::Destructive
            } else {
                Class::Write
            };
        }
        Class::Read
    }

    fn find(&self, wire: &str) -> Option<McpToolInfo> {
        self.catalog.tools().into_iter().find(|t| t.wire == wire)
    }

    /// The read tool to suggest: same server, read class, `get|read|fetch|view|retrieve` first.
    fn suggest_read(&self, server: &str, id_arg: &str) -> String {
        let all: Vec<McpToolInfo> = self
            .catalog
            .tools()
            .into_iter()
            .filter(|t| t.server == server && self.classify(t, &t.tool) == Class::Read)
            .collect();
        let score = |t: &McpToolInfo| {
            let ws = words(&t.tool);
            (
                ws.first().is_some_and(|w| {
                    ["get", "read", "fetch", "view", "retrieve"].contains(&w.as_str())
                }),
                t.input_schema["properties"].get(id_arg).is_some(),
                !ws.iter()
                    .any(|w| w == "search" || w == "list" || w == "diff" || w == "execute"),
            )
        };
        all.iter()
            .max_by_key(|t| score(t))
            .map_or_else(|| "a read tool of this server".into(), |t| t.wire.clone())
    }

    /// Decides one call before it is policy-checked. Destructive tools always ask, in every mode
    /// and whatever the allow rules say.
    pub fn check(&self, task: &str, wire: &str, input: &Value) -> Check {
        let Some(info) = self.find(wire) else {
            return Check::Pass;
        };
        let (inner, args) = target(&info.tool, input);
        let class = self
            .classify(&info, &inner)
            .max_class(self.classify(&info, &info.tool));
        if class == Class::Read {
            return Check::Pass;
        }
        let mut asks: Vec<String> = vec![];
        if class == Class::Destructive {
            asks.push("the MCP server marks this tool destructive".into());
        }
        let ids = id_values(&args);
        let tasks = lock(&self.tasks);
        let empty = TaskState::default();
        let st = tasks.get(task).unwrap_or(&empty);
        let id_arg = args
            .as_object()
            .into_iter()
            .flatten()
            .find(|(k, _)| is_id_key(k))
            .map_or("id", |(k, _)| k.as_str());
        let mut fetched: Vec<&str> = vec![];
        for id in &ids {
            let last_read = st
                .reads
                .iter()
                .filter(|r| {
                    r.server == info.server && (r.ids.contains(id) || mentions(&r.text, id))
                })
                .map(|r| r.seq)
                .max();
            let last_write = st
                .writes
                .iter()
                .filter(|w| w.server == info.server && w.ids.contains(id))
                .map(|w| w.seq)
                .max();
            match (last_read, last_write) {
                (Some(r), w) if w.is_none_or(|w| r > w) => {
                    fetched.extend(
                        st.reads
                            .iter()
                            .filter(|x| {
                                x.seq == r
                                    || (x.server == info.server
                                        && x.seq > w.unwrap_or(0)
                                        && mentions(&x.text, id))
                            })
                            .map(|x| x.text.as_str()),
                    );
                }
                (_, w) => {
                    let why = if w.is_some() {
                        "this task already changed it since it was last fetched"
                    } else {
                        "this task has not fetched its current content"
                    };
                    return Check::Reject(format!(
                        "Not sent: `{}` changes {id} but {why}, so your update could overwrite changes you have not seen. Fetch the current content with `{}`, MERGE the user's requested change into what you receive (keep everything the user did not ask to change, and use its current version), then call `{}` again.",
                        wire,
                        self.suggest_read(&info.server, id_arg),
                        wire
                    ));
                }
            }
        }
        if let Some(e) = stale_version(&args, &fetched) {
            return Check::Reject(format!(
                "Not sent: {e}. Use the version from the content you fetched with `{}` (many APIs want that version plus one), then call `{}` again.",
                self.suggest_read(&info.server, id_arg),
                wire
            ));
        }
        if let Some(why) = shrinkage(&args, &fetched) {
            asks.push(why);
        }
        if asks.is_empty() {
            Check::Pass
        } else {
            Check::Ask(asks.join("; "))
        }
    }

    /// Records a finished call: a successful read is freshness proof, a successful write makes
    /// earlier reads of its resource stale.
    pub fn observe(&self, task: &str, wire: &str, input: &Value, text: &str, is_error: bool) {
        if is_error {
            return;
        }
        let Some(info) = self.find(wire) else { return };
        let (inner, args) = target(&info.tool, input);
        let class = self
            .classify(&info, &inner)
            .max_class(self.classify(&info, &info.tool));
        let ids = id_values(&args);
        let mut tasks = lock(&self.tasks);
        if tasks.len() >= MAX_TASKS && !tasks.contains_key(task) {
            tasks.clear();
        }
        let st = tasks.entry(task.to_string()).or_default();
        st.seq += 1;
        let seq = st.seq;
        if class == Class::Read {
            st.reads.push(Read {
                seq,
                server: info.server,
                ids,
                text: text.to_string(),
            });
            if st.reads.len() > MAX_READS {
                st.reads.remove(0);
            }
        } else {
            st.writes.push(Write {
                seq,
                server: info.server,
                ids,
            });
        }
    }
}

impl Class {
    fn max_class(self, o: Class) -> Class {
        let rank = |c: Class| match c {
            Class::Read => 0,
            Class::Write => 1,
            Class::Destructive => 2,
        };
        if rank(o) > rank(self) { o } else { self }
    }
}

/// A version-like field of the write whose value the fetched content does not support.
fn stale_version(args: &Value, fetched: &[&str]) -> Option<String> {
    let mut given = vec![];
    under_keys(args, &is_version_key, &mut given);
    if given.is_empty() || fetched.is_empty() {
        return None;
    }
    let mut known: Vec<String> = vec![];
    for t in fetched {
        match serde_json::from_str::<Value>(t) {
            Ok(v) => under_keys(&v, &is_version_key, &mut known),
            Err(_) => known.extend(given.iter().filter(|g| token_in(t, g)).cloned()),
        }
    }
    if known.is_empty() {
        return None; // the fetched content shows no version at all: nothing to compare with
    }
    let ok = |g: &String| {
        known.contains(g)
            || g.parse::<i64>().is_ok_and(|n| {
                known
                    .iter()
                    .any(|k| k.parse::<i64>().is_ok_and(|k| k + 1 == n))
            })
    };
    let bad: Vec<&String> = given.iter().filter(|g| !ok(g)).collect();
    (!bad.is_empty()).then(|| {
        format!(
            "version {} does not appear in the content you fetched (it shows {})",
            bad[0],
            known.join(", ")
        )
    })
}

/// Why the update would discard much of the fetched content, with the lines that go.
fn shrinkage(args: &Value, fetched: &[&str]) -> Option<String> {
    let mut new = None;
    longest_under(
        args,
        &|k| BODY_KEYS.contains(&k.to_ascii_lowercase().as_str()),
        &mut new,
    );
    let new = new?;
    let mut old: Option<String> = None;
    for t in fetched {
        match serde_json::from_str::<Value>(t) {
            Ok(v) => longest_under(
                &v,
                &|k| BODY_KEYS.contains(&k.to_ascii_lowercase().as_str()),
                &mut old,
            ),
            Err(_) => old = Some((*t).to_string()),
        }
    }
    let old = old.filter(|o| o.chars().count() >= MIN_BODY)?;
    let (ol, nl) = (lines(&old), lines(&new));
    let kept: HashSet<&str> = nl.iter().copied().collect();
    let missing: Vec<&&str> = ol.iter().filter(|l| !kept.contains(**l)).collect();
    let lost_headings = missing.iter().any(|l| is_heading(l));
    let shrunk = (new.chars().count() as f64) < SHRINK_RATIO * old.chars().count() as f64;
    if !(shrunk || lost_headings) || missing.is_empty() {
        return None;
    }
    let d: Vec<String> = diff(&ol, &nl).into_iter().take(DIFF_LINES).collect();
    Some(format!(
        "this update will replace {} lines; {} lines of the current content are not present in your update{}{}",
        ol.len(),
        missing.len(),
        if lost_headings {
            " (including headings)"
        } else {
            ""
        },
        if d.is_empty() {
            String::new()
        } else {
            format!(". Changes (first {DIFF_LINES}):\n{}", d.join("\n"))
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Cat(Vec<McpToolInfo>);
    impl McpCatalog for Cat {
        fn tools(&self) -> Vec<McpToolInfo> {
            self.0.clone()
        }
    }

    fn info(server: &str, tool: &str, ro: Option<bool>, destructive: Option<bool>) -> McpToolInfo {
        McpToolInfo {
            wire: format!("mcp__{server}__{tool}"),
            server: server.into(),
            tool: tool.into(),
            description: String::new(),
            input_schema: if tool.contains("Issue") {
                json!({"properties": {"issueIdOrKey": {}}})
            } else {
                json!({"properties": {"id": {}}})
            },
            read_only: ro,
            destructive,
        }
    }

    fn guard(cfg: GuardConfig) -> McpGuard {
        McpGuard::new(
            cfg,
            Arc::new(Cat(vec![
                info("a", "getPage", None, None),
                info("a", "updatePage", None, None),
                info("a", "editIssue", Some(false), None),
                info("a", "getIssue", Some(true), None),
                info("a", "executeRead", None, None),
                info("a", "executeWrite", None, None),
                info("a", "dropAll", None, Some(true)),
                info("b", "updatePage", None, None),
                info("a", "searchPages", None, None),
                info("a", "lookup_it", Some(false), None),
            ])),
        )
    }

    const BODY: &str = "# Title\n\nline one of the page body that matters\nline two of the page body that matters\n## Contacts\nalice@example.com is the owner\nbob@example.com is the backup\nmore text here to pass two hundred characters in total for the shrink check to be active ok\n";

    fn page(version: i64) -> String {
        json!({"id": "100", "title": "T", "body": BODY, "version": {"number": version}}).to_string()
    }

    fn up(id: &str, body: &str, version: i64) -> Value {
        json!({"id": id, "title": "T", "body": body, "version": {"number": version}})
    }

    #[test]
    fn classification_follows_annotations_names_and_user_rules() {
        let g = guard(GuardConfig::default());
        let c = |t: &str| g.classify(&g.find(&format!("mcp__a__{t}")).unwrap(), t);
        assert_eq!(c("getPage"), Class::Read);
        assert_eq!(c("updatePage"), Class::Write);
        assert_eq!(c("editIssue"), Class::Write);
        assert_eq!(c("getIssue"), Class::Read, "readOnlyHint wins");
        assert_eq!(c("dropAll"), Class::Destructive);
        assert_eq!(c("searchPages"), Class::Read);
        assert_eq!(c("executeWrite"), Class::Write);
        assert_eq!(c("executeRead"), Class::Read);
        let mut cfg = GuardConfig::default();
        cfg.servers.insert(
            "a".into(),
            ServerRules {
                write_tools: vec!["getIssue".into(), "lookup_it".into()],
                read_tools: vec!["updatePage".into(), "getIssue".into()],
            },
        );
        let g = guard(cfg);
        let c = |t: &str| g.classify(&g.find(&format!("mcp__a__{t}")).unwrap(), t);
        assert_eq!(
            c("getIssue"),
            Class::Write,
            "write_tools beats read_tools and the hint"
        );
        assert_eq!(c("updatePage"), Class::Read);
    }

    #[test]
    fn write_needs_a_fetch_of_the_same_resource_first() {
        let g = guard(GuardConfig::default());
        let w = "mcp__a__updatePage";
        let Check::Reject(m) = g.check("t", w, &up("100", BODY, 4)) else {
            panic!()
        };
        assert!(m.contains("mcp__a__getPage") && m.contains("MERGE"), "{m}");
        g.observe(
            "t",
            "mcp__a__getPage",
            &json!({"id": "100"}),
            &page(3),
            false,
        );
        assert_eq!(g.check("t", w, &up("100", BODY, 4)), Check::Pass);
        // another page, another task, another server: no proof
        assert!(matches!(
            g.check("t", w, &up("200", BODY, 4)),
            Check::Reject(_)
        ));
        assert!(matches!(
            g.check("u", w, &up("100", BODY, 4)),
            Check::Reject(_)
        ));
        assert!(matches!(
            g.check("t", "mcp__b__updatePage", &up("100", BODY, 4)),
            Check::Reject(_)
        ));
        // a failed read proves nothing
        g.observe(
            "v",
            "mcp__a__getPage",
            &json!({"id": "100"}),
            &page(3),
            true,
        );
        assert!(matches!(
            g.check("v", w, &up("100", BODY, 4)),
            Check::Reject(_)
        ));
        // a successful write makes the fetched copy stale until it is read again
        g.observe("t", w, &up("100", BODY, 4), "{}", false);
        let Check::Reject(m) = g.check("t", w, &up("100", BODY, 5)) else {
            panic!()
        };
        assert!(m.contains("already changed"), "{m}");
        g.observe(
            "t",
            "mcp__a__getPage",
            &json!({"id": "100"}),
            &page(4),
            false,
        );
        assert_eq!(g.check("t", w, &up("100", BODY, 5)), Check::Pass);
    }

    #[test]
    fn a_search_that_mentions_the_key_counts_and_other_keys_do_not() {
        let g = guard(GuardConfig::default());
        g.observe(
            "t",
            "mcp__a__searchPages",
            &json!({"q": "x"}),
            &json!({"issues": [{"key": "PROJ-1", "summary": "s"}]}).to_string(),
            false,
        );
        let edit = |k: &str| json!({"issueIdOrKey": k, "fields": {"summary": "n"}});
        assert_eq!(
            g.check("t", "mcp__a__editIssue", &edit("PROJ-1")),
            Check::Pass
        );
        assert!(matches!(
            g.check("t", "mcp__a__editIssue", &edit("PROJ-12")),
            Check::Reject(_)
        ));
        assert!(matches!(
            g.check("t", "mcp__a__editIssue", &edit("PROJ-2")),
            Check::Reject(_)
        ));
    }

    #[test]
    fn the_version_must_come_from_the_fetched_content() {
        let g = guard(GuardConfig::default());
        g.observe(
            "t",
            "mcp__a__getPage",
            &json!({"id": "100"}),
            &page(3),
            false,
        );
        let w = "mcp__a__updatePage";
        assert_eq!(g.check("t", w, &up("100", BODY, 3)), Check::Pass, "same");
        assert_eq!(g.check("t", w, &up("100", BODY, 4)), Check::Pass, "next");
        let Check::Reject(m) = g.check("t", w, &up("100", BODY, 9)) else {
            panic!()
        };
        assert!(m.contains("version 9") && m.contains('3'), "{m}");
    }

    #[test]
    fn dropping_most_of_the_page_asks_with_a_diff_even_when_allowed() {
        let g = guard(GuardConfig::default());
        g.observe(
            "t",
            "mcp__a__getPage",
            &json!({"id": "100"}),
            &page(3),
            false,
        );
        let Check::Ask(r) = g.check(
            "t",
            "mcp__a__updatePage",
            &up("100", "# Title\nonly this", 4),
        ) else {
            panic!()
        };
        assert!(
            r.contains("not present in your update")
                && r.contains("- ## Contacts")
                && r.contains("headings"),
            "{r}"
        );
        // same page with one line changed is fine
        let edited = BODY.replace("line one", "line 1");
        assert_eq!(
            g.check("t", "mcp__a__updatePage", &up("100", &edited, 4)),
            Check::Pass
        );
    }

    #[test]
    fn destructive_tools_always_ask_and_wrappers_are_seen_through() {
        let g = guard(GuardConfig::default());
        assert!(matches!(
            g.check("t", "mcp__a__dropAll", &json!({})),
            Check::Ask(_)
        ));
        let w = json!({"tool": "updatePage", "arguments": {"id": "100", "body": BODY}});
        assert!(matches!(
            g.check("t", "mcp__a__executeWrite", &w),
            Check::Reject(_)
        ));
        g.observe(
            "t",
            "mcp__a__executeRead",
            &json!({"tool": "getPage", "arguments": {"id": "100"}}),
            &page(3),
            false,
        );
        assert_eq!(g.check("t", "mcp__a__executeWrite", &w), Check::Pass);
    }

    #[test]
    fn word_and_token_helpers() {
        assert_eq!(words("getPageContent"), ["get", "page", "content"]);
        assert_eq!(words("update_page-2"), ["update", "page", "2"]);
        assert!(token_in("see PROJ-1.", "PROJ-1") && !token_in("PROJ-12", "PROJ-1"));
        assert!(
            is_id_key("issueIdOrKey")
                && is_id_key("page_id")
                && is_id_key("spaceId")
                && !is_id_key("title")
        );
    }
}
