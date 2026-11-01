//! Agent pane (port of `agentview.js`): markdown-ish rendering, tables, diffs, wrapped tool
//! output, permission / question / plan dialogs, pickers, the prompt box and the status line.
//!
//! The pane works on rows of styled [`Run`]s which are painted to SGR strings only for the
//! visible window. Per-item rows are cached (see [`Item::rev`]).

use crate::dialogs::{mode_name, permission_options};
use crate::highlight::{ADD as HL_ADD, Col, DEL as HL_DEL, col, highlight, lang_from_file};
use crate::model::{Todo, summarize_tool};
use crate::modelname::{ModelConfig, model_parts};
use crate::picker::{DEFAULT_WINDOW, Picker};
use crate::theme::*;
use crate::util::{one_line, pad_end, pad_start, to_fixed};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use synthesis_tui_base::selection::SelRow;
use synthesis_tui_base::text::{char_width, is_js_space};

mod diffview;
mod thinking;

// ---------------------------------------------------------------- colours

const C_TEXT: Col = col(T_BODY);
const C_BRIGHT: Col = col(T_TEXT);
const C_DIM: Col = col(T_INACTIVE);
const C_SUBTLE: Col = col(T_SUBTLE);
const C_ACCENT: Col = col(ACCENT_CORE);
const C_LIGHT: Col = col(ACCENT_LIGHT);
const C_CODE: Col = col([125, 211, 252]);
const C_HEADING: Col = col(GREEN);
const C_STRONG: Col = col(ACCENT_LIGHT);
const C_LINK: Col = col(ACCENT_LIGHT);
const C_OK: Col = col(GREEN);
const C_BAD: Col = col(RED);
const C_WARN: Col = col(YELLOW);
const C_BORDER: Col = col(WARM_DIM);
const C_PERM: Col = col(BLUE);
/// Same value as GREEN, a different object in Node (see `highlight::Col`).
const C_ADD: Col = Col {
    rgb: [74, 222, 128],
    tag: 1,
};
const C_BANG: Col = col(CYAN);
const C_TEAL: Col = col(TEAL);
const C_PURPLE: Col = col(PURPLE);

// ---------------------------------------------------------------- runs

#[derive(Clone, Debug)]
pub struct Run {
    pub t: String,
    pub c: Col,
    pub b: bool,
    pub i: bool,
    pub u: bool,
    pub raw: bool,
    pub w: usize,
    /// `false` for decorations (bullets, bars) that are not part of copied chat text.
    pub copy: bool,
    /// Last run of a row that `wrap` broke to fit the width: the next row continues the same line.
    pub soft: bool,
}
pub type Row = Vec<Run>;

fn run_full(t: impl Into<String>, c: Col, b: bool, i: bool, u: bool) -> Run {
    Run {
        t: t.into(),
        c,
        b: b && BOLD_ON,
        i,
        u,
        raw: false,
        w: 0,
        copy: true,
        soft: false,
    }
}
fn r(t: impl Into<String>, c: Col) -> Run {
    run_full(t, c, false, false, false)
}
fn rb(t: impl Into<String>, c: Col) -> Run {
    run_full(t, c, true, false, false)
}
fn deco(t: &str, c: Col) -> Run {
    Run {
        copy: false,
        ..r(t, c)
    }
}

fn cw(ch: char) -> usize {
    char_width(ch)
}
fn str_w(s: &str) -> usize {
    if s.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        s.len()
    } else {
        s.chars().map(cw).sum()
    }
}
fn row_w(row: &[Run]) -> usize {
    row.iter()
        .map(|r| if r.raw { r.w } else { str_w(&r.t) })
        .sum()
}

fn fmt_dur(ms: i64) -> String {
    let s = ((ms as f64 / 1000.0) + 0.5).floor().max(0.0) as i64;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}
fn fmt_tok(n: f64) -> String {
    if n >= 1000.0 {
        format!("{}k", to_fixed(n / 1000.0, 1))
    } else {
        crate::util::round_str(n)
    }
}
fn fmt_bytes(n: f64) -> String {
    if n >= 1e6 {
        format!("{} MB", to_fixed(n / 1e6, 1))
    } else {
        format!("{} KB", ((n / 1024.0 + 0.5).floor()).max(1.0))
    }
}

const TURN_VERBS: [&str; 4] = ["Working", "Thinking", "Reasoning", "Planning"];
fn turn_verb(turn_start: i64) -> &'static str {
    TURN_VERBS[(turn_start.div_euclid(1000)).rem_euclid(4) as usize]
}

// ---------------------------------------------------------------- data model

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Image {
    pub name: String,
    pub bytes: f64,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Child {
    pub name: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
struct Cache {
    w: usize,
    expanded: bool,
    fp: u64,
    rows: Rc<Vec<Row>>,
}

/// One conversation item. Rows are cached per item: a cached entry is reused while the width, the
/// expanded flag and a fingerprint of the main text fields are unchanged. **Whoever mutates
/// `input`, `images`, `children`, `data`, `diff`, `rows`, `lines` or any non-text field must bump `rev`.**
/// Items drawn with `frame`/`now` (spinners, running tools, busy notes, unfinished thinking) are
/// never cached. The clamp of tasks / bash / code blocks (see [`CLAMP_LINES`]) is a pure function
/// of those fields and the expanded flag, so it needs no state of its own in the key.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Item {
    pub kind: String,
    pub text: String,
    pub status: String,
    pub soft: bool,
    pub not_run: bool,
    pub checking: bool,
    pub busy: bool,
    pub tone: String,
    pub queued: bool,
    pub images: Vec<Image>,
    pub children: Vec<Child>,
    pub child_count: usize,
    pub input: Value,
    pub name: String,
    pub result: String,
    pub output: String,
    pub command: String,
    pub restored: bool,
    pub streaming: bool,
    pub error: bool,
    pub lines: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub title: String,
    pub data: Value,
    /// Structured diff of a file change (`tool.completed.diff`); see `diffview`.
    pub diff: Value,
    pub file: String,
    pub ok: bool,
    pub turns: u64,
    pub ms: i64,
    pub cost: Option<f64>,
    pub feed_kind: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub trigger: String,
    pub before: f64,
    pub after: f64,
    pub bootstrap: bool,
    pub rev: u64,
    #[serde(skip)]
    cache: RefCell<Option<Cache>>,
}

impl Item {
    fn animated(&self) -> bool {
        self.checking
            || self.busy
            || self.status == "running"
            || (self.kind == "thinking"
                && (self.streaming
                    || !(self.start.is_some_and(|s| s != 0) && self.end.is_some_and(|e| e != 0))))
    }
    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            &self.kind,
            &self.text,
            &self.status,
            &self.result,
            &self.output,
            &self.command,
            &self.name,
            &self.title,
        )
            .hash(&mut h);
        (
            self.soft,
            self.not_run,
            self.checking,
            self.busy,
            self.queued,
            self.restored,
            self.streaming,
            self.error,
            self.ok,
            self.is_final,
        )
            .hash(&mut h);
        (
            self.images.len(),
            self.children.len(),
            self.child_count,
            self.rev,
            self.lines.len(),
            self.rows.len(),
            self.turns,
            self.ms,
        )
            .hash(&mut h);
        h.finish()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Question {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QOption>,
}

#[derive(Clone, Debug, Default)]
pub struct QState {
    pub choice: usize,
    pub picked: BTreeSet<usize>,
    pub other: String,
    pub answered: bool,
    pub multi: bool,
    pub typing: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Opt {
    pub key: String,
    pub label: String,
    pub mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Pending {
    pub kind: String,
    pub dialog: String,
    pub tool: String,
    pub input: Value,
    /// Structured diff of the change the tool would make (see `diffview`), when known.
    pub diff: Value,
    pub suggestions: Vec<Value>,
    pub status: String,
    pub choice: usize,
    pub options: Option<Vec<Opt>>,
    pub qi: usize,
    pub questions: Vec<Question>,
    #[serde(skip)]
    pub qstate: Vec<QState>,
    pub description: String,
    pub deadline: Option<i64>,
    pub prompt: String,
    pub display_name: String,
    pub offer_auto: bool,
    /// `x` in a permission dialog: show the whole input, scrolled to row `input_scroll`.
    #[serde(skip)]
    pub view_all: bool,
    #[serde(skip)]
    pub input_scroll: usize,
    /// Rows the expanded input took at the last draw. Shared with the clones the renderer gets,
    /// so the app can limit scrolling to what was really drawn.
    #[serde(skip)]
    pub input_rows: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Tools that run a model-supplied shell command: their `command` is shown like Bash's.
/// (Mirrors `synthesis_permissions::COMMAND_TOOLS`.)
const COMMAND_TOOLS: &[&str] = &["verify_run", "terminal_spawn"];
/// Rows of a diff shown in a permission dialog before the "more" footer.
const DIALOG_DIFF_ROWS: usize = 16;
/// Characters of a generic tool input shown before the "hidden" marker.
const INPUT_SHOWN: usize = 400;
/// Rows of the expanded (`x`) input view.
pub const INPUT_VIEW_ROWS: usize = 12;

impl Pending {
    /// What the capture does to a pending request: a question gets its answer state, a
    /// permission without `options` gets the standard ones.
    pub fn hydrate(&mut self) {
        if self.dialog == "question" && !self.questions.is_empty() {
            self.qstate = self
                .questions
                .iter()
                .map(|q| QState {
                    multi: q.multi_select,
                    ..Default::default()
                })
                .collect();
        }
        if self.kind == "permission" && self.dialog != "question" && self.options.is_none() {
            self.options = Some(if self.dialog == "plan" {
                crate::dialogs::PLAN_OPTIONS
                    .iter()
                    .map(|(k, l, m)| Opt {
                        key: (*k).into(),
                        label: (*l).into(),
                        mode: m.map(String::from),
                    })
                    .collect()
            } else {
                permission_options(&self.suggestions, self.offer_auto)
                    .into_iter()
                    .map(|(key, label)| Opt {
                        key,
                        label,
                        mode: None,
                    })
                    .collect()
            });
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Goal {
    pub status: String,
    pub iterations: u32,
    pub max_iterations: u32,
    pub condition: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DefName {
    pub name: String,
}

/// The agent (or a sub-agent record) the pane draws; a duck-typed subset of the Node agent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub def: Option<DefName>,
    pub description: String,
    pub cwd: String,
    pub mode: String,
    pub turn_mode: Option<String>,
    pub status: String,
    pub effort: String,
    pub effort_label: Option<String>,
    pub model: String,
    pub active_model: String,
    pub items: Vec<Item>,
    pub turn_start: i64,
    pub turn_tokens: u64,
    pub version: String,
    pub context_window: Option<f64>,
    pub pending: Vec<Pending>,
    pub subagents: Vec<Agent>,
    pub prompt_color: Option<String>,
    pub focus_view: bool,
    pub interrupting: bool,
    /// Verb of a busy line that is not a model turn (`Compacting`); no tokens, no Esc hint.
    pub activity: Option<String>,
    pub goal: Option<Goal>,
    #[serde(skip)]
    pub config: Option<ModelConfig>,
}

impl Agent {
    /// `def.name || type`
    pub fn display_name(&self) -> String {
        self.def
            .as_ref()
            .map(|d| d.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.kind.clone())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Sug {
    pub label: String,
    pub detail: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Suggestions {
    pub items: Vec<Sug>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Search {
    pub query: String,
    #[serde(rename = "match")]
    pub matched: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Preview {
    pub id: String,
    pub title: String,
    pub live: bool,
    pub items: Vec<Item>,
}

/// Prompt editor contents: the text and the cursor (an index into `chars`).
pub struct EditorView<'a> {
    pub chars: &'a [char],
    pub cur: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentOpts {
    pub focused: Option<bool>,
    pub scroll: usize,
    pub attachments: Vec<Image>,
    pub suggestions: Option<Suggestions>,
    pub selected: usize,
    pub expanded: bool,
    pub input_mode: Option<String>,
    pub preview: Option<Preview>,
    pub search: Option<Search>,
    pub picker: Option<Picker>,
    pub bypass_confirmation: bool,
    pub chat_target: Option<String>,
    pub docked: bool,
    /// Messages typed while a task runs; sent in order when it ends (drawn above the prompt).
    pub queued: Vec<String>,
    /// Sub-agents of the session, one row each: `developer-2 (developer) running · 3 tools · 2 queued`.
    pub agent_rows: Vec<String>,
    /// Where the next line goes when it is not the main agent: `@worker · running · 2 queued`.
    pub route: Option<String>,
    #[serde(skip)]
    pub config: Option<ModelConfig>,
}

// ---------------------------------------------------------------- markdown

fn inline_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r##"(\[[^\]\n]+\]\(https?://[^)\s]+\)|\*\*[^*]+\*\*|`[^`]+`|\*[^*\s][^*]*\*|https?://[^\s<>"'`()\[\]│┃]+)"##).unwrap()
    })
}

fn inline(text: &str, base: Col) -> Vec<Run> {
    let mut out = vec![];
    let mut last = 0;
    for m in inline_re().find_iter(text) {
        if m.start() > last {
            out.push(r(&text[last..m.start()], base));
        }
        let tok = m.as_str();
        if tok.starts_with('[') {
            let close = tok.find("](").unwrap();
            out.push(run_full(&tok[1..close], C_LINK, false, false, true));
            out.push(run_full(
                format!(" ({})", &tok[close + 2..tok.len() - 1]),
                C_LINK,
                false,
                false,
                true,
            ));
        } else if tok.starts_with("http") {
            let url = tok.trim_end_matches(['.', ',', ';', ':', '!', '?']);
            out.push(run_full(url, C_LINK, false, false, true));
            if url.len() < tok.len() {
                out.push(r(&tok[url.len()..], base));
            }
        } else if tok.starts_with("**") {
            out.push(rb(&tok[2..tok.len() - 2], C_STRONG));
        } else if tok.starts_with('`') {
            out.push(r(&tok[1..tok.len() - 1], C_CODE));
        } else {
            out.push(run_full(
                &tok[1..tok.len() - 1],
                C_BRIGHT,
                false,
                true,
                false,
            ));
        }
        last = m.end();
    }
    if last < text.len() {
        out.push(r(&text[last..], base));
    }
    out
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let s = js_trim(line);
    if !s.contains('|') {
        return None;
    }
    let inner = s.strip_prefix('|').unwrap_or(s);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let cells: Vec<String> = inner.split('|').map(|c| js_trim(c).to_string()).collect();
    (cells.len() > 1).then_some(cells)
}

fn table_separator(line: &str, columns: usize) -> Option<Vec<&'static str>> {
    let cells = table_cells(line)?;
    let ok = |c: &str| {
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        c.len() >= 3 && c.bytes().all(|b| b == b'-')
    };
    if cells.len() != columns || !cells.iter().all(|c| ok(c)) {
        return None;
    }
    Some(
        cells
            .iter()
            .map(|c| {
                if c.starts_with(':') && c.ends_with(':') && c.len() > 1 {
                    "center"
                } else if c.ends_with(':') {
                    "right"
                } else {
                    "left"
                }
            })
            .collect(),
    )
}

#[derive(Clone, Debug)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<&'static str>,
    rows: Vec<Vec<String>>,
}

struct MdLine {
    runs: Vec<Run>,
    indent: usize,
    table: Option<Table>,
}

const NL: &str = r"[^\n\r\u{2028}\u{2029}]";

struct MdRes {
    fence: regex::Regex,
    head: regex::Regex,
    quote: regex::Regex,
    li: regex::Regex,
}
fn md_res() -> &'static MdRes {
    static R: std::sync::OnceLock<MdRes> = std::sync::OnceLock::new();
    R.get_or_init(|| MdRes {
        fence: regex::Regex::new(r"^\s*```\s*([^\s`]*)").unwrap(),
        head: regex::Regex::new(&format!(r"^(#{{1,6}})\s+({NL}*)$")).unwrap(),
        quote: regex::Regex::new(&format!(r"^>\s?({NL}*)$")).unwrap(),
        li: regex::Regex::new(&format!(r"^(\s*)([-*+]|\d+\.)\s+({NL}*)$")).unwrap(),
    })
}

fn is_hr(raw: &str) -> bool {
    let t = js_trim(raw);
    let mut it = t.chars();
    match it.next() {
        Some(c @ ('-' | '*' | '_')) => t.chars().count() >= 3 && t.chars().all(|x| x == c),
        _ => false,
    }
}

fn markdown(text: &str, expanded: bool) -> Vec<MdLine> {
    let res = md_res();
    let mut out: Vec<MdLine> = vec![];
    struct Code {
        lang: String,
        lines: Vec<String>,
    }
    let mut code: Option<Code> = None;
    let emit_code = |code: &mut Option<Code>, out: &mut Vec<MdLine>| {
        let Some(c) = code.take() else { return };
        let hl = highlight(&c.lines.join("\n").replace('\t', "  "), &c.lang);
        let (shown, footer) = clamp(hl, expanded, false);
        for line in &shown {
            // a plain bar: a "+" / "−" here read as diff additions
            let mut runs = vec![r("│ ", C_SUBTLE)];
            runs.extend(line.iter().map(|t| r(&t.t, t.c)));
            out.push(MdLine {
                runs,
                indent: 2,
                table: None,
            });
        }
        if let Some(f) = footer {
            out.push(MdLine {
                runs: vec![r(format!("│ {f}"), C_SUBTLE)],
                indent: 2,
                table: None,
            });
        }
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut n = 0;
    while n < lines.len() {
        let raw = lines[n];
        if let Some(f) = res.fence.captures(raw) {
            if code.is_some() {
                emit_code(&mut code, &mut out);
            } else {
                code = Some(Code {
                    lang: f[1].to_string(),
                    lines: vec![],
                });
            }
            n += 1;
            continue;
        }
        if let Some(c) = code.as_mut() {
            c.lines
                .push(raw.strip_suffix('\r').unwrap_or(raw).to_string());
            n += 1;
            continue;
        }
        let headers = table_cells(raw);
        let aligns = headers.as_ref().and_then(|h| {
            if n + 1 < lines.len() {
                table_separator(lines[n + 1], h.len())
            } else {
                None
            }
        });
        if let (Some(headers), Some(aligns)) = (headers, aligns) {
            let mut rows = vec![];
            n += 2;
            while n < lines.len() {
                match table_cells(lines[n]) {
                    Some(cells) if cells.len() == headers.len() => rows.push(cells),
                    _ => break,
                }
                n += 1;
            }
            out.push(MdLine {
                table: Some(Table {
                    headers,
                    aligns,
                    rows,
                }),
                runs: vec![],
                indent: 0,
            });
            continue;
        }
        if let Some(h) = res.head.captures(raw) {
            out.push(MdLine {
                runs: vec![rb(h[2].replace("**", ""), C_HEADING)],
                indent: 0,
                table: None,
            });
        } else if let Some(q) = res.quote.captures(raw) {
            let mut runs = vec![r("▎ ", C_SUBTLE)];
            runs.extend(inline(&q[1], C_DIM));
            out.push(MdLine {
                runs,
                indent: 2,
                table: None,
            });
        } else if let Some(li) = res.li.captures(raw) {
            let pad = " ".repeat(li[1].chars().count().min(6));
            let mark = if li[2].bytes().any(|b| b.is_ascii_digit()) {
                format!("{} ", &li[2])
            } else {
                "• ".to_string()
            };
            let indent = pad.chars().count() + mark.chars().count();
            let mut runs = vec![r(format!("{pad}{mark}"), C_DIM)];
            runs.extend(inline(&li[3], C_TEXT));
            out.push(MdLine {
                runs,
                indent,
                table: None,
            });
        } else if is_hr(raw) {
            out.push(MdLine {
                runs: vec![r("─".repeat(20), C_SUBTLE)],
                indent: 0,
                table: None,
            });
        } else {
            out.push(MdLine {
                runs: inline(raw, C_TEXT),
                indent: 0,
                table: None,
            });
        }
        n += 1;
    }
    emit_code(&mut code, &mut out);
    out
}

// ---------------------------------------------------------------- wrapping

fn tokens(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let ws = is_js_space(c);
        if prev_ws.is_some_and(|p| p != ws) {
            out.push(&s[start..i]);
            start = i;
        }
        prev_ws = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[allow(unused_assignments)]
/// Runs -> rows of at most `w` cells; `first` / `rest` are the prefix runs of the first and the
/// following rows.
fn wrap(runs: &[Run], w: usize, first: &[Run], rest: &[Run]) -> Vec<Row> {
    // prefixes (bullets, indents, prompt marks) are layout, not copied text
    let deco_all = |rs: &[Run]| -> Vec<Run> {
        rs.iter()
            .map(|x| Run {
                copy: false,
                ..x.clone()
            })
            .collect()
    };
    let (first, rest) = (&deco_all(first)[..], &deco_all(rest)[..]);
    let mut rows: Vec<Row> = vec![];
    let mut row: Row = first.to_vec();
    let mut prefix_n = first.len();
    let mut used = row_w(first);
    let mut fresh = true;
    macro_rules! new_row {
        () => {{
            rows.push(std::mem::replace(&mut row, rest.to_vec()));
            if let Some(l) = rows.last_mut().and_then(|r| r.last_mut()) {
                l.soft = true;
            }
            prefix_n = rest.len();
            used = row_w(rest);
            fresh = true;
        }};
    }
    macro_rules! add {
        ($t:expr, $r:expr) => {{
            let t: &str = $t;
            let rr: &Run = $r;
            let merge = row.len() > prefix_n
                && row.last().is_some_and(|l| {
                    !l.raw && l.c == rr.c && l.b == rr.b && l.i == rr.i && l.u == rr.u
                });
            if merge {
                row.last_mut().unwrap().t.push_str(t);
            } else {
                row.push(run_full(t, rr.c, rr.b, rr.i, rr.u));
                // run_full re-applies BOLD_ON; keep the source flag as is
                row.last_mut().unwrap().b = rr.b;
            }
            used += str_w(t);
            fresh = false;
        }};
    }
    for rr in runs {
        for tok in tokens(&rr.t) {
            let tw = str_w(tok);
            if tok.chars().all(is_js_space) {
                if fresh && !rows.is_empty() {
                    continue;
                }
                if used + tw <= w {
                    add!(tok, rr);
                }
                continue;
            }
            if used + tw > w && !fresh {
                new_row!();
            }
            if used + tw <= w {
                add!(tok, rr);
                continue;
            }
            for ch in tok.chars() {
                if used + cw(ch) > w {
                    new_row!();
                }
                let mut b = [0u8; 4];
                add!(ch.encode_utf8(&mut b), rr);
            }
        }
    }
    rows.push(row);
    rows
}

fn table_rows(table: &Table, w: usize) -> Vec<Row> {
    let n = table.headers.len();
    if w < 4 * n + 1 {
        let labelled: Vec<Vec<String>> = table
            .rows
            .iter()
            .flat_map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, cell)| vec![format!("{}: {}", table.headers[i], cell)])
            })
            .collect();
        return table_rows(
            &Table {
                headers: vec![table.headers.join(" / ")],
                aligns: vec!["left"],
                rows: labelled,
            },
            w,
        );
    }
    let all: Vec<&Vec<String>> = std::iter::once(&table.headers)
        .chain(table.rows.iter())
        .collect();
    let room = w - (3 * n + 1);
    let mut widths = vec![1usize; n];
    let wanted: Vec<usize> = (0..n)
        .map(|i| all.iter().map(|row| str_w(&row[i])).fold(3, usize::max))
        .collect();
    let mut extra = room as i64 - n as i64;
    while extra > 0 {
        let mut changed = false;
        for i in 0..n {
            if extra <= 0 {
                break;
            }
            if widths[i] < wanted[i] {
                widths[i] += 1;
                extra -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let border = |l: &str, m: &str, rr: &str| -> Row {
        vec![r(
            format!(
                "{l}{}{rr}",
                widths
                    .iter()
                    .map(|w| "─".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join(m)
            ),
            C_BORDER,
        )]
    };
    let draw = |cells: &Vec<String>| -> Vec<Row> {
        let wrapped: Vec<Vec<Row>> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| wrap(&inline(c, C_TEXT), widths[i], &[], &[]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|y| {
                let mut line = vec![r("│", C_BORDER)];
                for i in 0..n {
                    let empty: Row = vec![];
                    let cell = wrapped[i].get(y).unwrap_or(&empty);
                    let spare = widths[i].saturating_sub(row_w(cell));
                    let left = match table.aligns[i] {
                        "right" => spare,
                        "center" => spare / 2,
                        _ => 0,
                    };
                    line.push(r(format!(" {}", " ".repeat(left)), C_TEXT));
                    line.extend(cell.iter().cloned());
                    line.push(r(format!("{} ", " ".repeat(spare - left)), C_TEXT));
                    line.push(r("│", C_BORDER));
                }
                line
            })
            .collect()
    };
    let mut out = vec![border("┌", "┬", "┐")];
    out.extend(draw(&table.headers));
    out.push(border("├", "┼", "┤"));
    for row in &table.rows {
        out.extend(draw(row));
    }
    out.push(border("└", "┴", "┘"));
    out
}

pub fn paint_row(row: &[Run]) -> String {
    let mut s = String::new();
    for r in row {
        if r.raw {
            s.push_str(&r.t);
            continue;
        }
        if r.b {
            s.push_str(BOLD);
        }
        if r.i {
            s.push_str(ITALIC);
        }
        if r.u {
            s.push_str("\x1b[4m");
        }
        s.push_str(&fg(r.c.rgb));
        s.push_str(&r.t);
        s.push_str(RESET);
    }
    s
}

fn framed(rows: Vec<Row>, w: usize, c: Col, title: &str) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(4);
    let t = if title.is_empty() {
        String::new()
    } else {
        format!("─ {title} ")
    };
    let mut out = vec![vec![
        run_full(format!("╭{t}"), c, !title.is_empty(), false, false),
        r(
            format!(
                "{}╮",
                "─".repeat((w.saturating_sub(2)).saturating_sub(str_w(&t)))
            ),
            c,
        ),
    ]];
    for rw in rows {
        let pad = inner.saturating_sub(row_w(&rw));
        let mut line = vec![r("│ ", c)];
        line.extend(rw);
        line.push(r(format!("{} │", " ".repeat(pad)), c));
        out.push(line);
    }
    out.push(vec![r(format!("╰{}╯", "─".repeat(w.saturating_sub(2))), c)]);
    out
}

fn image_label(im: &Image, i: usize) -> String {
    let dims = match (im.width, im.height) {
        (Some(w), Some(h)) if w != 0.0 && h != 0.0 => format!(" {}×{}", w, h),
        _ => String::new(),
    };
    format!(
        "[Image #{}] {}{} · {}",
        i + 1,
        im.name,
        dims,
        fmt_bytes(im.bytes)
    )
}

// ---------------------------------------------------------------- items -> rows

fn tool_title(it: &Item) -> String {
    // Same name and path as the finished diff row ("Update(path)"), so the row does not change shape.
    let base = base_name(&it.name);
    if matches!(base, "Edit" | "MultiEdit" | "NotebookEdit")
        && let Some(p) = ["file_path", "notebook_path"].iter().find_map(|k| {
            it.input
                .get(k)
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
        })
    {
        return format!("{}Update({p})", it.name.strip_suffix(base).unwrap_or(""));
    }
    let s = summarize_tool(&it.name, &it.input);
    let name = if it.name.starts_with("mcp__") {
        it.name.split("__").skip(1).collect::<Vec<_>>().join(":")
    } else {
        it.name.clone()
    };
    if s.is_empty() {
        name
    } else {
        format!("{name}({s})")
    }
}

fn clip_runs(runs: Vec<Run>, w: usize) -> Row {
    let mut out: Vec<Run> = vec![];
    let mut used = 0;
    for rr in &runs {
        for ch in rr.t.chars() {
            let n = cw(ch);
            if used + n > w {
                if w > 0 {
                    while !out.is_empty() && row_w(&out) >= w {
                        out.pop();
                    }
                    out.push(r("…", rr.c));
                }
                return out;
            }
            match out.last_mut() {
                Some(l) if l.c == rr.c => l.t.push(ch),
                _ => out.push(r(ch.to_string(), rr.c)),
            }
            used += n;
        }
    }
    out
}

fn edit_lines(v: Option<&Value>) -> Vec<String> {
    let s = match v {
        None | Some(Value::Null) => return vec![],
        Some(Value::String(s)) => s.clone(),
        Some(o) => o.to_string(),
    };
    if s.is_empty() {
        return vec![];
    }
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(String::from)
        .collect()
}

fn diff_rows(
    old: Option<&Value>,
    new: Option<&Value>,
    w: usize,
    max: usize,
    lang: &str,
    mut on_more: Option<&mut dyn FnMut(usize)>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![];
    let del = edit_lines(old);
    let add = edit_lines(new);
    let old_t = highlight(&del.join("\n").replace('\t', "  "), lang);
    let new_t = highlight(&add.join("\n").replace('\t', "  "), lang);
    let line = |sign: char, c: Col, toks: Option<&Vec<crate::highlight::Tok>>| -> Row {
        let mut runs = vec![r(format!("{sign} "), c)];
        if let Some(t) = toks {
            runs.extend(t.iter().map(|t| r(&t.t, t.c)));
        }
        clip_runs(runs, w)
    };
    for i in 0..del.len().min(max) {
        rows.push(line('-', HL_DEL, old_t.get(i)));
    }
    if del.len() > max {
        let n = del.len() - max;
        match on_more.as_mut() {
            Some(f) => f(n),
            None => rows.push(vec![r(format!("  … {n} more removed"), C_SUBTLE)]),
        }
    }
    for i in 0..add.len().min(max) {
        rows.push(line('+', HL_ADD, new_t.get(i)));
    }
    if add.len() > max {
        let n = add.len() - max;
        match on_more.as_mut() {
            Some(f) => f(n),
            None => rows.push(vec![r(format!("  … {n} more added"), C_SUBTLE)]),
        }
    }
    rows
}

/// Display-limit policy: ONLY tasks, bash output and code are clamped, to this many lines (a
/// footer says how many are hidden; ctrl+o shows everything). Everything else is shown in full.
pub const CLAMP_LINES: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cat {
    Tasks,
    Bash,
    Code,
    Other,
}

/// Tool name without the `@agent:` prefix sub-agent tools carry.
fn base_name(name: &str) -> &str {
    name.strip_prefix('@')
        .and_then(|r| r.split_once(':'))
        .map_or(name, |(_, n)| n)
}

fn category(name: &str) -> Cat {
    match base_name(name) {
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" => Cat::Tasks,
        "Bash" | "BashOutput" | "PowerShell" => Cat::Bash,
        n if n.starts_with("terminal_") => Cat::Bash,
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Cat::Code,
        _ => Cat::Other,
    }
}

/// The lines of a clamped block that are shown, plus its footer. `tail` keeps the last lines
/// (running bash), otherwise the first. Blocks of at most [`CLAMP_LINES`] have no footer.
fn clamp<T>(mut v: Vec<T>, expanded: bool, tail: bool) -> (Vec<T>, Option<String>) {
    let n = v.len();
    if n <= CLAMP_LINES {
        return (v, None);
    }
    if expanded {
        return (v, Some("(ctrl+o to collapse)".into()));
    }
    let hidden = n - CLAMP_LINES;
    if tail {
        v.drain(..hidden);
    } else {
        v.truncate(CLAMP_LINES);
    }
    (
        v,
        Some(format!("… +{hidden} more lines (ctrl+o to expand)")),
    )
}

fn clip_to(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let total = str_w(s);
    for ch in s.chars() {
        if used + cw(ch) + 1 > w && total > w {
            out.push('…');
            return out;
        }
        out.push(ch);
        used += cw(ch);
    }
    out
}

fn input_edits(inp: &Value) -> Vec<&Value> {
    match inp.get("edits").filter(|e| !e.is_null()) {
        Some(Value::Array(a)) => a.iter().collect(),
        _ => vec![inp],
    }
}

/// The summary lines under a tool call and the footer of a clamped block (see [`clamp`]).
/// Only tasks / bash / code tools are clamped; every other result is shown whole.
fn tool_result_lines(it: &Item, expanded: bool, frame: usize) -> (Vec<String>, Option<String>) {
    let inp = &it.input;
    let cat = category(&it.name);
    let name = base_name(&it.name);
    let plain = |v: Vec<String>| (v, None);
    if it.checking {
        return plain(vec![waiting("checking…", frame)]);
    }
    if it.not_run {
        return plain(vec![
            "Not run · auto mode's check had no usable answer".into(),
        ]);
    }
    if it.status == "running" {
        if !it.children.is_empty() {
            let mut v: Vec<String> = it
                .children
                .iter()
                .map(|c| {
                    format!("{} {}", c.name, c.summary)
                        .trim_matches(is_js_space)
                        .to_string()
                })
                .collect();
            if it.child_count > it.children.len() {
                v.push(format!(
                    "+{} more tool uses",
                    it.child_count - it.children.len()
                ));
            }
            return plain(v);
        }
        // streaming bash output: the LAST lines
        let out = it.output.replace('\r', "");
        let out = out.trim_end_matches('\n');
        if cat == Cat::Bash && !out.is_empty() {
            let v = out.split('\n').map(|l| l.replace('\t', "  ")).collect();
            return clamp(v, expanded, true);
        }
        return plain(vec![waiting("running…", frame)]);
    }
    if it.status == "interrupted" {
        // what the command had printed stays, marked; the harness's own "Interrupted." is no output
        let text = it.result.replace('\r', "");
        let kept: Vec<String> = text
            .trim_end_matches('\n')
            .split('\n')
            .filter(|l| !(l.is_empty() || *l == "Interrupted."))
            .map(|l| l.replace('\t', "  "))
            .collect();
        let (mut v, footer) = clamp(kept, expanded, true);
        v.push("interrupted".into());
        return (v, footer);
    }
    let text = it.result.replace('\r', "");
    let text = text.trim_end_matches('\n');
    let lines: Vec<&str> = if text.is_empty() {
        vec![]
    } else {
        text.split('\n').collect()
    };
    // clamped categories keep the first lines; everything else is whole
    let result_lines = |lines: &[&str]| {
        let v: Vec<String> = lines.iter().map(|l| l.replace('\t', "  ")).collect();
        if cat == Cat::Other {
            (v, None)
        } else {
            clamp(v, expanded, false)
        }
    };
    if it.status == "error" {
        let lower = text.to_lowercase();
        if lower.contains("doesn't want to proceed") || lower.contains("rejected") {
            return plain(vec!["rejected by you".into()]);
        }
        return result_lines(&lines);
    }
    if (name == "Edit" || name == "MultiEdit") && it.status == "done" {
        let edits = input_edits(inp);
        let add: usize = edits
            .iter()
            .map(|e| edit_lines(e.get("new_string")).len())
            .sum();
        let del: usize = edits
            .iter()
            .map(|e| edit_lines(e.get("old_string")).len())
            .sum();
        return plain(vec![format!("updated · +{add} -{del} lines")]);
    }
    if name == "Write" {
        let c = inp.get("content").and_then(Value::as_str).unwrap_or("");
        return plain(vec![format!("wrote {} lines", c.split('\n').count())]);
    }
    if name == "TodoWrite"
        && let Some(todos) = inp.get("todos").and_then(Value::as_array)
    {
        let v = todos
            .iter()
            .map(|t| {
                let m = match t.get("status").and_then(Value::as_str) {
                    Some("completed") => '☒',
                    Some("in_progress") => '■',
                    _ => '☐',
                };
                format!(
                    "{m} {}",
                    t.get("content").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        return clamp(v, expanded, false);
    }
    if name == "Read" {
        return plain(vec![
            diffview::read_summary(it).unwrap_or_else(|| format!("read {} lines", lines.len())),
        ]);
    }
    if lines.is_empty() {
        return plain(vec!["done".into()]);
    }
    result_lines(&lines)
}

/// File content / diff body of a Read, Write, Edit or NotebookEdit row: the first
/// [`CLAMP_LINES`] lines (all after ctrl+o) and the clamp footer.
fn code_body(it: &Item, w: usize, expanded: bool) -> (Vec<Row>, Option<String>) {
    if it.status != "done" {
        return (vec![], None);
    }
    let inp = &it.input;
    let lang = lang_from_file(inp.get("file_path").and_then(Value::as_str)).unwrap_or("");
    let text_rows = |code: &str, lang: &str| -> Vec<Row> {
        highlight(&code.replace('\t', "  "), lang)
            .into_iter()
            .map(|l| l.iter().map(|t| r(&t.t, t.c)).collect())
            .collect()
    };
    let rows: Vec<Row> = match base_name(&it.name) {
        "Edit" | "MultiEdit" => input_edits(inp)
            .into_iter()
            .flat_map(|e| {
                diff_rows(
                    e.get("old_string"),
                    e.get("new_string"),
                    w.saturating_sub(5).max(10),
                    usize::MAX,
                    lang,
                    None,
                )
            })
            .collect(),
        "Write" => inp
            .get("content")
            .and_then(Value::as_str)
            .map(|c| text_rows(c, lang))
            .unwrap_or_default(),
        "NotebookEdit" => inp
            .get("new_source")
            .and_then(Value::as_str)
            .map(|c| text_rows(c, ""))
            .unwrap_or_default(),
        "Read" => it
            .result
            .replace('\r', "")
            .trim_end_matches('\n')
            .split('\n')
            .filter(|_| !it.result.is_empty())
            .map(|l| vec![r(l.replace('\t', "  "), C_DIM)])
            .collect(),
        _ => vec![],
    };
    clamp(rows, expanded, false)
}

fn history_icon(k: &str) -> &'static str {
    match k {
        "prompt" => ">",
        "tool" | "agent" | "text" => "●",
        "skill" => "★",
        "error" => "✗",
        "hook" => "↯",
        "cmd" => "/",
        _ => "·",
    }
}

fn handoff_title(ln: &str) -> Option<(usize, &'static str)> {
    // /^((issue(?: \/ objective)?|done|open (?:work|tasks)(?: \/ open decisions)?|open decisions)\s*:)/i
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = R.get_or_init(|| {
        regex::Regex::new(r"(?i)^((issue(?: / objective)?|done|open (?:work|tasks)(?: / open decisions)?|open decisions)\s*:)").unwrap()
    });
    let c = re.captures(ln)?;
    let name = c[2].to_lowercase();
    Some((
        c[1].len(),
        if name.starts_with("done") {
            "ok"
        } else if name.starts_with("open") {
            "warn"
        } else {
            "accent"
        },
    ))
}

const CTX_COLORS: [Rgb; 8] = [BLUE, PURPLE, TEAL, YELLOW, PINK, CYAN, GREEN, RED];

fn context_rows(d: &Value, w: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![];
    let cats: Vec<&Value> = d
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|c| c.get("tokens").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
                .collect()
        })
        .unwrap_or_default();
    let tok = |c: &Value| c.get("tokens").and_then(Value::as_f64).unwrap_or(0.0);
    let known_max = d
        .get("maxTokens")
        .and_then(Value::as_f64)
        .filter(|m| m.is_finite() && *m > 0.0);
    let sum: f64 = cats.iter().map(|c| tok(c)).sum();
    let max = known_max.unwrap_or(if sum == 0.0 { 1.0 } else { sum });
    let used = d
        .get("totalTokens")
        .and_then(Value::as_f64)
        .or(d.get("total").and_then(Value::as_f64))
        .unwrap_or_else(|| {
            cats.iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) != Some("free"))
                .map(|c| tok(c))
                .sum()
        });
    let pct = known_max.map(|km| {
        d.get("percentage")
            .and_then(Value::as_f64)
            .unwrap_or(used / km * 100.0)
            .min(100.0)
    });
    let remaining = known_max.map(|km| {
        d.get("remainingTokens")
            .and_then(Value::as_f64)
            .unwrap_or(max - used)
            .max(0.0)
            + 0.0 * km
    });
    let overflow = known_max.map(|km| {
        d.get("overflowTokens")
            .and_then(Value::as_f64)
            .unwrap_or(used - km)
            .max(0.0)
    });
    let head = match known_max {
        Some(km) => format!(
            "  {} / {} tokens ({}%) · {} remaining",
            fmt_tok(used),
            fmt_tok(km),
            to_fixed(pct.unwrap(), 1),
            fmt_tok(remaining.unwrap())
        ),
        None => format!("  {} tokens (window unknown)", fmt_tok(used)),
    };
    rows.push(vec![rb("Context usage", C_BRIGHT), r(head, C_DIM)]);
    if let Some(o) = overflow.filter(|o| *o > 0.0) {
        rows.push(vec![r(
            format!("  {} estimated overflow", fmt_tok(o)),
            C_WARN,
        )]);
    }
    let bar_w = (w.saturating_sub(4)).clamp(10, 60);
    let mut segs = vec![];
    let mut filled = 0usize;
    for (i, c) in cats.iter().enumerate() {
        let n = js_round(tok(c) / max * bar_w as f64) as usize;
        let free = c.get("kind").and_then(Value::as_str) == Some("free");
        let colr = if free { T_TRACK } else { CTX_COLORS[i % 8] };
        if n > 0 {
            segs.push(r(
                if free { "░" } else { "█" }.repeat(n.min(bar_w.saturating_sub(filled))),
                col(colr),
            ));
        }
        filled += n;
    }
    let mut bar = vec![r("  ", C_TEXT)];
    bar.extend(segs);
    rows.push(bar);
    for (i, c) in cats.iter().enumerate() {
        let free = c.get("kind").and_then(Value::as_str) == Some("free");
        let colr = if free { T_SUBTLE } else { CTX_COLORS[i % 8] };
        let share = to_fixed((tok(c) / max * 100.0).min(100.0), 1);
        rows.push(vec![
            r("  ", C_TEXT),
            r(if free { "░ " } else { "█ " }, col(colr)),
            r(
                pad_end(c.get("name").and_then(Value::as_str).unwrap_or(""), 18),
                C_TEXT,
            ),
            r(
                format!(
                    "{}  {}%",
                    pad_start(&fmt_tok(tok(c)), 7),
                    pad_start(&share, 5)
                ),
                C_DIM,
            ),
        ]);
    }
    if let Some(s) = d
        .get("autocompactSource")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        rows.push(vec![r(format!("  auto-compact: {s}"), C_SUBTLE)]);
    }
    rows
}

fn item_rows(it: &Item, w: usize, frame: usize, now: i64, expanded: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![];
    match it.kind.as_str() {
        "user" => {
            rows.push(vec![]);
            if !it.text.is_empty() {
                let norm = it.text.replace("\r\n", "\n").replace('\r', "\n");
                let lines: Vec<&str> = norm.split('\n').collect();
                for (i, line) in lines.iter().enumerate() {
                    let mut runs = vec![r(*line, C_BRIGHT)];
                    if it.queued && i == lines.len() - 1 {
                        runs.push(r("  (queued)", C_SUBTLE));
                    }
                    rows.extend(wrap(
                        &runs,
                        w,
                        &[rb(if i > 0 { "  " } else { "> " }, C_LIGHT)],
                        &[r("  ", C_TEXT)],
                    ));
                }
            } else {
                rows.push(vec![
                    rb("> ", C_LIGHT),
                    r(if it.queued { "(queued)" } else { "" }, C_SUBTLE),
                ]);
            }
            for (i, im) in it.images.iter().enumerate() {
                rows.extend(wrap(
                    &[r(image_label(im, i), C_DIM)],
                    w,
                    &[r("  ⎿ ", C_SUBTLE)],
                    &[r("    ", C_TEXT)],
                ));
            }
        }
        "history" => rows.extend(wrap(
            &[r(&it.text, C_SUBTLE)],
            w,
            &[r(format!("{} ", history_icon(&it.feed_kind)), C_SUBTLE)],
            &[r("  ", C_TEXT)],
        )),
        "text" => {
            rows.push(vec![]);
            // no blank rows at either end: the one blank between blocks is the separator above
            let body = it.text.trim_start_matches(['\n', '\r']).trim_end();
            for (i, ln) in markdown(body, expanded).iter().enumerate() {
                let bar = || deco("▌ ", C_OK);
                let lead = if it.is_final {
                    vec![bar()]
                } else if i == 0 {
                    vec![deco("● ", C_BRIGHT)]
                } else {
                    vec![deco("  ", C_TEXT)]
                };
                let rest_pad = if it.is_final {
                    let mut v = vec![bar()];
                    if ln.indent > 0 {
                        v.push(deco(&" ".repeat(ln.indent), C_TEXT));
                    }
                    v
                } else {
                    vec![deco(&" ".repeat(2 + ln.indent), C_TEXT)]
                };
                if let Some(t) = &ln.table {
                    let grid = table_rows(t, w.saturating_sub(row_w(&lead)).max(4));
                    for (n, row) in grid.into_iter().enumerate() {
                        let mut line = if n > 0 {
                            rest_pad.clone()
                        } else {
                            lead.clone()
                        };
                        line.extend(row);
                        rows.push(line);
                    }
                } else if !ln.runs.is_empty() {
                    rows.extend(wrap(&ln.runs, w, &lead, &rest_pad));
                } else {
                    rows.push(lead);
                }
            }
            if it.streaming {
                rows.last_mut().unwrap().push(r(" ▍", C_LIGHT));
            }
        }
        "thinking" => rows.extend(thinking::rows(it, w, frame, now)),
        "tool" => {
            let st = it.status.as_str();
            let (dot, dcol) = match st {
                "running" => (spin(frame), C_LIGHT),
                "error" => ("●", if it.soft { C_BRIGHT } else { C_BAD }),
                "interrupted" => ("●", C_WARN),
                _ => ("●", C_OK),
            };
            rows.push(vec![]);
            let dv = diffview::view(it, w, expanded);
            let title = match &dv {
                Some(v) => {
                    let sub = it.name.strip_suffix(base_name(&it.name)).unwrap_or("");
                    format!("{sub}{}({})", v.name, v.path)
                }
                None => tool_title(it),
            };
            let (name, args) = match title.find('(').filter(|p| *p > 0) {
                Some(p) => (&title[..p], &title[p..]),
                None => (title.as_str(), ""),
            };
            rows.extend(wrap(
                &[rb(name, C_BRIGHT), r(args, C_DIM)],
                w,
                &[rb(format!("{dot} "), dcol)],
                &[r("  ", C_TEXT)],
            ));
            let (result, footer) = match &dv {
                Some(v) => (vec![v.summary.clone()], None),
                None => tool_result_lines(it, expanded, frame),
            };
            for (i, ln) in result.iter().enumerate() {
                let c = if it.not_run || it.checking {
                    C_DIM
                } else if st == "error" {
                    if it.soft { C_BRIGHT } else { C_BAD }
                } else if st == "interrupted" {
                    C_WARN
                } else {
                    C_DIM
                };
                rows.extend(wrap(
                    &[r(ln, c)],
                    w,
                    &[r(if i == 0 { "  ⎿  " } else { "     " }, C_SUBTLE)],
                    &[r("     ", C_TEXT)],
                ));
            }
            if let Some(f) = footer {
                rows.push(vec![r(format!("     {f}"), C_SUBTLE)]);
            }
            let (body, footer) = match dv {
                Some(v) => (v.body, v.footer),
                None => code_body(it, w, expanded),
            };
            for rw in body {
                let mut line = vec![r("     ", C_TEXT)];
                line.extend(rw);
                rows.push(line);
            }
            if let Some(f) = footer {
                rows.push(vec![r(format!("     {f}"), C_SUBTLE)]);
            }
        }
        "note" => {
            let c = match it.tone.as_str() {
                "error" => C_BAD,
                "warn" => C_WARN,
                "goal" => C_ACCENT,
                _ => C_DIM,
            };
            let text = if it.busy {
                waiting(&it.text, frame)
            } else {
                it.text.clone()
            };
            let lead = if it.busy {
                format!("{} ", spin(frame))
            } else {
                "※ ".to_string()
            };
            rows.extend(wrap(
                &[r(text, if it.busy { C_LIGHT } else { c })],
                w,
                &[r(lead, if it.busy { C_LIGHT } else { c })],
                &[r("  ", C_TEXT)],
            ));
        }
        "handoff" => {
            rows.push(vec![]);
            let mark = vec![rb(if it.bootstrap { "↳ " } else { "⇄ " }, C_ACCENT)];
            let text = it.text.replace('\r', "").replace('\t', "  ");
            for (i, ln) in text.split('\n').enumerate() {
                let runs = match handoff_title(ln) {
                    Some((n, kind)) => {
                        let c = match kind {
                            "ok" => C_OK,
                            "warn" => C_WARN,
                            _ => C_ACCENT,
                        };
                        vec![rb(&ln[..n], c), r(&ln[n..], C_TEXT)]
                    }
                    None => vec![r(ln, C_TEXT)],
                };
                let two = [r("  ", C_TEXT)];
                rows.extend(wrap(&runs, w, if i == 0 { &mark } else { &two[..] }, &two));
            }
        }
        "compact" => {
            let text = if it.trigger == "auto" {
                format!(
                    "context compacted automatically ({} → {} est.)",
                    fmt_tok(it.before),
                    fmt_tok(it.after)
                )
            } else {
                format!(
                    "conversation compacted ({} → {} est.)",
                    fmt_tok(it.before),
                    fmt_tok(it.after)
                )
            };
            rows.extend(wrap(
                &[r(text, C_DIM)],
                w,
                &[r("※ ", C_DIM)],
                &[r("  ", C_TEXT)],
            ));
        }
        "command-output" => {
            let c = if it.error { C_BAD } else { C_DIM };
            for (i, ln) in it.text.split('\n').enumerate() {
                rows.extend(wrap(
                    &[r(ln, c)],
                    w,
                    &[r(if i == 0 { "  ⎿  " } else { "     " }, C_SUBTLE)],
                    &[r("     ", C_TEXT)],
                ));
            }
        }
        "help" => {
            rows.push(vec![]);
            rows.push(vec![rb("Commands & keys", C_BRIGHT)]);
            for ln in &it.lines {
                rows.extend(wrap(
                    &[r(ln, C_DIM)],
                    w,
                    &[r("  ", C_TEXT)],
                    &[r("    ", C_TEXT)],
                ));
            }
        }
        "bash" => {
            rows.push(vec![]);
            let dot = if it.status == "running" {
                spin(frame)
            } else {
                "!"
            };
            let c = if it.status == "error" {
                C_BAD
            } else if it.status == "running" {
                C_LIGHT
            } else {
                C_BANG
            };
            rows.extend(wrap(
                &[r(&it.command, C_BRIGHT)],
                w,
                &[rb(format!("{dot} "), c)],
                &[r("  ", C_TEXT)],
            ));
            let out = it.output.replace('\r', "");
            let out = out.trim_end_matches('\n');
            let lines: Vec<String> = if out.is_empty() {
                vec![if it.status == "running" {
                    waiting("running…", frame)
                } else {
                    "(no output)".into()
                }]
            } else {
                out.split('\n').map(String::from).collect()
            };
            let (shown, footer) = if it.status == "running" && !out.is_empty() {
                clamp(lines, expanded, true) // streaming: the last lines
            } else {
                clamp(lines, expanded, false)
            };
            for (i, ln) in shown.iter().enumerate() {
                rows.extend(wrap(
                    &[r(
                        ln.replace('\t', "  "),
                        if it.status == "error" { C_BAD } else { C_DIM },
                    )],
                    w,
                    &[r(if i == 0 { "  ⎿  " } else { "     " }, C_SUBTLE)],
                    &[r("     ", C_TEXT)],
                ));
            }
            if let Some(f) = footer {
                rows.push(vec![r(format!("     {f}"), C_SUBTLE)]);
            }
            if it.status != "running" && !it.restored {
                rows.push(vec![r(
                    "     output will be sent to Synthesis with your next message",
                    C_SUBTLE,
                )]);
            }
        }
        "memory" => {
            rows.push(vec![]);
            rows.extend(wrap(
                &[r(&it.text, C_BRIGHT)],
                w,
                &[rb("# ", C_PURPLE)],
                &[r("  ", C_TEXT)],
            ));
            rows.push(vec![r(format!("  ⎿  saved to {}", it.file), C_SUBTLE)]);
        }
        "info" => {
            rows.push(vec![]);
            rows.push(vec![rb(&it.title, C_BRIGHT)]);
            let key_w = (it
                .rows
                .iter()
                .map(|rw| str_w(rw.first().map_or("", String::as_str)))
                .max()
                .unwrap_or(0)
                + 2)
            .min(20);
            for rw in &it.rows {
                let k = rw.first().map_or("", String::as_str);
                let v = rw.get(1).map_or("", String::as_str);
                rows.extend(wrap(
                    &[r(v, C_TEXT)],
                    w,
                    &[r("  ", C_TEXT), r(pad_end(k, key_w), C_DIM)],
                    &[r(" ".repeat(key_w + 2), C_TEXT)],
                ));
            }
        }
        "context" => {
            rows.push(vec![]);
            rows.extend(context_rows(&it.data, w));
        }
        "result" => {
            rows.push(vec![]);
            if !it.ok {
                rows.extend(wrap(
                    &[r(format!("error: {}", it.text), C_BAD)],
                    w,
                    &[rb("✗ ", C_BAD)],
                    &[r("  ", C_TEXT)],
                ));
            } else {
                let mut bits = vec![];
                if it.turns > 0 {
                    bits.push(format!("{} turns", it.turns));
                }
                if it.ms != 0 {
                    bits.push(fmt_dur(it.ms));
                }
                if let Some(c) = it.cost {
                    bits.push(format!("${}", to_fixed(c, 4)));
                }
                let label = if it.text.starts_with("✓ goal met") {
                    it.text.chars().skip(2).collect::<String>()
                } else {
                    "Done".to_string()
                };
                let mut runs = vec![rb(label, C_OK)];
                if !bits.is_empty() {
                    runs.push(r(format!(" · {}", bits.join(" · ")), C_BRIGHT));
                }
                rows.extend(wrap(&runs, w, &[rb("✓ ", C_OK)], &[r("  ", C_TEXT)]));
            }
        }
        _ => {}
    }
    rows
}

/// A note has no separator row of its own (consecutive notes stay together); one blank row
/// goes between it and any other kind before it, so every block change has exactly one.
fn note_gap(prev: Option<&str>, it: &Item) -> Option<Rc<Vec<Row>>> {
    (it.kind == "note" && prev.is_some_and(|k| k != "note")).then(|| Rc::new(vec![vec![]]))
}

fn cached_item_rows(it: &Item, w: usize, frame: usize, now: i64, expanded: bool) -> Rc<Vec<Row>> {
    if it.animated() {
        return Rc::new(item_rows(it, w, frame, now, expanded));
    }
    let fp = it.fingerprint();
    if let Some(c) = it.cache.borrow().as_ref()
        && c.w == w
        && c.expanded == expanded
        && c.fp == fp
    {
        return c.rows.clone();
    }
    let rows = Rc::new(item_rows(it, w, frame, now, expanded));
    *it.cache.borrow_mut() = Some(Cache {
        w,
        expanded,
        fp,
        rows: rows.clone(),
    });
    rows
}

// ---------------------------------------------------------------- window

pub struct Windowed<'a> {
    pub view: Vec<&'a Row>,
    pub max_scroll: usize,
    pub off: usize,
    pub skip: usize,
    pub total: usize,
}

/// The bottom-aligned window of `h` rows, `scroll` rows up from the end. Leading blank rows are
/// dropped.
fn window_rows(parts: &[Rc<Vec<Row>>], scroll: usize, h: usize) -> Windowed<'_> {
    let all: usize = parts.iter().map(|p| p.len()).sum();
    let mut skip = 0;
    'scan: for p in parts {
        for rw in p.iter() {
            if !rw.is_empty() {
                break 'scan;
            }
            skip += 1;
        }
    }
    let total = all - skip;
    let max_scroll = total.saturating_sub(h);
    let off = scroll.min(max_scroll);
    let end = total - off;
    let a = end.saturating_sub(h) + skip;
    let b = end + skip;
    let mut view = vec![];
    let mut pos = 0;
    for p in parts {
        let next = pos + p.len();
        if next > a && pos < b {
            for i in (a.max(pos) - pos)..(b.min(next) - pos) {
                view.push(&p[i]);
            }
        }
        if next >= b {
            break;
        }
        pos = next;
    }
    Windowed {
        view,
        max_scroll,
        off,
        skip,
        total,
    }
}

/// `s` cut from the left (`…/repo`) to fit `w` columns; a path is most telling at its end.
fn tail_fit(s: &str, w: usize) -> String {
    if str_w(s) <= w {
        return s.to_string();
    }
    let mut out: Vec<char> = vec![];
    let mut used = 1;
    for ch in s.chars().rev() {
        if used + cw(ch) > w {
            break;
        }
        used += cw(ch);
        out.push(ch);
    }
    out.push('…');
    out.reverse();
    out.into_iter().collect()
}

fn welcome_rows(agent: &Agent, w: usize) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(10);
    let line = |runs: Vec<Run>| -> Row {
        let rw = wrap(&runs, inner, &[], &[]).into_iter().next().unwrap();
        let pad = inner.saturating_sub(row_w(&rw));
        let mut l = vec![r("│ ", C_BORDER)];
        l.extend(rw);
        l.push(r(format!("{} │", " ".repeat(pad)), C_BORDER));
        l
    };
    let mut v = vec![
        vec![r(format!("╭{}╮", "─".repeat(inner + 2)), C_BORDER)],
        line(vec![
            rb("✻ ", C_ACCENT),
            rb("Hello there! I'm Synthesis.", C_BRIGHT),
            r(
                if agent.version.is_empty() {
                    String::new()
                } else {
                    format!("  · {}", agent.version)
                },
                C_SUBTLE,
            ),
        ]),
        line(vec![]),
        line(vec![r(
            "/help for commands · alt+v pastes an image",
            C_SUBTLE,
        )]),
        line(vec![
            r("cwd: ", C_SUBTLE),
            r(tail_fit(&agent.cwd, inner.saturating_sub(5)), C_DIM),
        ]),
        vec![r(format!("╰{}╯", "─".repeat(inner + 2)), C_BORDER)],
        vec![],
        vec![r(" Tips for getting started:", C_DIM)],
    ];
    let tips = [
        "Ask Synthesis to explain, fix or build something in this folder",
        "Type / for commands and skills, @ to mention a file",
        "Drop or paste an image path to attach a screenshot",
    ];
    for (i, t) in tips.iter().enumerate() {
        v.extend(wrap(
            &[r(*t, C_SUBTLE)],
            w.saturating_sub(5),
            &[r(format!("  {}. ", i + 1), C_TEXT)],
            &[r("     ", C_TEXT)],
        ));
    }
    v
}

/// The sub-agents the selector lists: the running ones, plus the one being viewed.
pub fn visible_subagents<'a>(subs: &'a [Agent], chat_target: Option<&str>) -> Vec<&'a Agent> {
    subs.iter()
        .filter(|s| s.status == "running" || chat_target.is_some_and(|t| s.id == t))
        .collect()
}

fn subagent_rows(subs: &[Agent], target: Option<&str>, docked: bool) -> Vec<Row> {
    let mut rows = vec![vec![
        rb("Conversations  ", C_BRIGHT),
        r("↑/↓ switch · Enter view · @name message", C_SUBTLE),
    ]];
    let mut entries: Vec<(Option<&str>, String, &str, String)> =
        vec![(None, "main agent".into(), "main", String::new())];
    for s in visible_subagents(subs, target) {
        entries.push((
            Some(s.id.as_str()),
            format!("@{}", s.display_name()),
            s.status.as_str(),
            s.description.clone(),
        ));
    }
    for (id, label, status, desc) in entries {
        let selected = target == id;
        let st = if status == "running" {
            "running"
        } else if status == "main" {
            "ready"
        } else {
            "idle"
        };
        rows.push(vec![
            rb(
                if selected { "▶ " } else { "  " },
                if selected { C_ACCENT } else { C_SUBTLE },
            ),
            run_full(
                format!("{label} · {st}"),
                if selected { C_BRIGHT } else { C_TEXT },
                selected,
                false,
                false,
            ),
            r(
                if selected && id.is_some() && docked {
                    "  → shown beside the topology"
                } else {
                    ""
                },
                C_ACCENT,
            ),
            r(
                if desc.is_empty() {
                    String::new()
                } else {
                    format!("  {}", one_line(&desc))
                },
                C_SUBTLE,
            ),
        ]);
    }
    rows
}

// ---------------------------------------------------------------- dialogs

#[derive(Clone, Debug, PartialEq)]
pub struct QHit {
    pub kind: &'static str,
    pub index: Option<usize>,
    pub qi: Option<usize>,
    pub x0: usize,
    pub x1: usize,
    pub body_row: usize,
    pub y: i64,
}

fn perm_title(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "Bash" => "Bash command",
        "PowerShell" => "PowerShell command",
        "Edit" | "MultiEdit" => "Edit file",
        "Write" => "Write file",
        "NotebookEdit" => "Edit notebook",
        "Read" => "Read file",
        "WebFetch" => "Fetch",
        "WebSearch" => "Web search",
        _ => return None,
    })
}

fn question_rows(item: &Pending, w: usize) -> (Vec<Row>, Vec<QHit>) {
    let inner = w.saturating_sub(4).max(10);
    let mut body: Vec<Row> = vec![];
    let mut hits: Vec<QHit> = vec![];
    let many = item.questions.len() > 1;
    let on_submit = many && item.qi >= item.questions.len();
    let hit = |kind, index, qi, x0, x1, body_row| QHit {
        kind,
        index,
        qi,
        x0,
        x1,
        body_row,
        y: 0,
    };
    if many {
        let mut tabs = vec![];
        let mut x = 2;
        for (i, q) in item.questions.iter().enumerate() {
            let done = item.qstate.get(i).is_some_and(|s| !s.picked.is_empty());
            let label = format!(
                "{} {}",
                if done { "☒" } else { "☐" },
                if q.header.is_empty() {
                    format!("Q{}", i + 1)
                } else {
                    q.header.clone()
                }
            );
            let text = format!(" {label} ");
            tabs.push(run_full(
                &text,
                if i == item.qi { C_PERM } else { C_DIM },
                i == item.qi,
                false,
                false,
            ));
            hits.push(hit("tab", Some(i), None, x, x + str_w(&text), 0));
            x += str_w(&text);
        }
        let submit = " ✓ Submit ";
        tabs.push(run_full(
            submit,
            if on_submit { C_OK } else { C_DIM },
            on_submit,
            false,
            false,
        ));
        hits.push(hit("submit-tab", None, None, x, x + str_w(submit), 0));
        let mut row = vec![r("← ", C_SUBTLE)];
        row.extend(tabs);
        row.push(r(" →", C_SUBTLE));
        body.push(row);
        body.push(vec![]);
    }
    if on_submit {
        body.push(vec![rb("Review your answers", C_BRIGHT)]);
        for (i, q) in item.questions.iter().enumerate() {
            let st = &item.qstate[i];
            let labels: Vec<String> = st
                .picked
                .iter()
                .map(|&k| {
                    if k == q.options.len() {
                        if st.other.is_empty() {
                            "(empty)".to_string()
                        } else {
                            st.other.clone()
                        }
                    } else {
                        q.options[k].label.clone()
                    }
                })
                .collect();
            let joined = labels.join(", ");
            let shown = if joined.is_empty() {
                "(no answer)".to_string()
            } else {
                joined
            };
            body.extend(wrap(
                &[
                    r(format!("{} ", q.question), C_DIM),
                    r("→ ", C_SUBTLE),
                    run_full(
                        shown,
                        if labels.is_empty() { C_WARN } else { C_BRIGHT },
                        true,
                        false,
                        false,
                    ),
                ],
                inner,
                &[r("  ", C_TEXT)],
                &[r("  ", C_TEXT)],
            ));
        }
        body.push(vec![]);
        let submit_row = body.len();
        body.push(vec![
            rb("❯ ", C_PERM),
            rb("Submit answers", C_PERM),
            r("   esc to cancel", C_SUBTLE),
        ]);
        hits.push(hit("submit", None, None, 0, inner, submit_row));
        return (framed(body, w, C_PERM, "Synthesis has questions"), hits);
    }
    let q = &item.questions[item.qi];
    let st = &item.qstate[item.qi];
    body.extend(wrap(&[rb(&q.question, C_BRIGHT)], inner, &[], &[]));
    if q.multi_select {
        body.push(vec![r(
            "  (select all that apply · space to toggle; click choices to toggle)",
            C_SUBTLE,
        )]);
    }
    body.push(vec![]);
    let mut options: Vec<QOption> = q.options.clone();
    options.push(QOption {
        label: "Other".into(),
        description: "type your own answer".into(),
    });
    for (k, o) in options.iter().enumerate() {
        let option_row = body.len();
        let sel = k == st.choice;
        let picked = st.picked.contains(&k);
        let bx = if q.multi_select {
            if picked { "[✓] " } else { "[ ] " }
        } else if picked {
            "● "
        } else {
            ""
        };
        let is_other = k == q.options.len();
        let mut label = o.label.clone();
        if is_other && (!st.other.is_empty() || (sel && st.typing)) {
            label = format!(
                "Other: {}{}",
                st.other,
                if sel && st.typing { "▍" } else { "" }
            );
        }
        body.extend(wrap(
            &[run_full(
                format!("{}. {bx}{label}", k + 1),
                if sel { C_PERM } else { C_TEXT },
                sel,
                false,
                false,
            )],
            inner,
            &[rb(if sel { "❯ " } else { "  " }, C_PERM)],
            &[r("     ", C_TEXT)],
        ));
        hits.push(hit("option", Some(k), Some(item.qi), 0, inner, option_row));
        if !o.description.is_empty() && !(is_other && !st.other.is_empty()) {
            body.extend(wrap(
                &[r(&o.description, C_SUBTLE)],
                inner,
                &[r("     ", C_TEXT)],
                &[r("     ", C_TEXT)],
            ));
        }
    }
    body.push(vec![]);
    let keys = if q.multi_select {
        "space toggle · enter next"
    } else {
        "enter select"
    };
    body.push(vec![r(
        format!(
            "↑/↓ move · {keys}{} · esc cancel",
            if many {
                " · ←/→ question · tab next tab"
            } else {
                ""
            }
        ),
        C_SUBTLE,
    )]);
    let title = if q.header.is_empty() {
        "Synthesis has a question"
    } else {
        &q.header
    };
    (framed(body, w, C_PERM, title), hits)
}

fn plan_rows(item: &Pending, w: usize) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(10);
    let mut body: Vec<Row> = vec![];
    let plan = item.input.get("plan").and_then(Value::as_str).unwrap_or("");
    let md = markdown(plan, false);
    let max_rows = 14;
    for ln in &md {
        if body.len() >= max_rows {
            break;
        }
        let runs = if ln.runs.is_empty() {
            vec![r("", C_TEXT)]
        } else {
            ln.runs.clone()
        };
        body.extend(wrap(
            &runs,
            inner,
            &[r(" ".repeat(ln.indent), C_TEXT)],
            &[r(" ".repeat(ln.indent), C_TEXT)],
        ));
    }
    if body.len() >= max_rows {
        body.truncate(max_rows);
        body.push(vec![r(
            format!("… plan continues ({} lines)", md.len()),
            C_SUBTLE,
        )]);
    }
    body.push(vec![]);
    body.push(vec![r("Would you like to proceed?", C_BRIGHT)]);
    for (i, o) in item.options.iter().flatten().enumerate() {
        let sel = i == item.choice;
        body.extend(wrap(
            &[run_full(
                format!("{}. {}", i + 1, o.label),
                if sel { C_PERM } else { C_TEXT },
                sel,
                false,
                false,
            )],
            inner,
            &[rb(if sel { "❯ " } else { "  " }, C_PERM)],
            &[r("     ", C_TEXT)],
        ));
    }
    framed(body, w, C_TEAL, "Ready to code? · here is Synthesis’s plan")
}

/// A tool input as JSON: shown up to [`INPUT_SHOWN`] chars followed by an explicit
/// `[N more chars hidden]` marker (never a silent cut), or whole and scrollable after `x`.
fn input_json_rows(body: &mut Vec<Row>, item: &Pending, input: &Value, inner: usize) {
    let json = input.to_string();
    let n = json.chars().count();
    let mut line = |text: String, c: Col| body.extend(wrap(&[r(text, c)], inner, &[], &[]));
    if item.view_all {
        let rows = wrap(&[r(&json, C_DIM)], inner, &[], &[]);
        let total = rows.len();
        item.input_rows
            .store(total, std::sync::atomic::Ordering::Relaxed);
        let start = item.input_scroll.min(total.saturating_sub(INPUT_VIEW_ROWS));
        let end = (start + INPUT_VIEW_ROWS).min(total);
        let scroll = if total > INPUT_VIEW_ROWS {
            " · PgUp/PgDn scroll"
        } else {
            ""
        };
        body.extend(rows.into_iter().skip(start).take(INPUT_VIEW_ROWS));
        body.extend(wrap(
            &[r(
                format!(
                    "[rows {}-{end} of {total}{scroll} · x to collapse]",
                    start + 1
                ),
                C_WARN,
            )],
            inner,
            &[],
            &[],
        ));
    } else if n > INPUT_SHOWN {
        line(json.chars().take(INPUT_SHOWN).collect(), C_DIM);
        line(
            format!(
                "[{} more chars hidden — press x to view all]",
                n - INPUT_SHOWN
            ),
            C_WARN,
        );
    } else {
        line(json, C_DIM);
    }
}

fn permission_rows(item: &Pending, w: usize, more: usize, now: i64) -> (Vec<Row>, Vec<QHit>) {
    if item.dialog == "question" {
        return question_rows(item, w);
    }
    if item.dialog == "plan" {
        return (plan_rows(item, w), vec![]);
    }
    let inner = w.saturating_sub(4).max(10);
    let inp = &item.input;
    let s = |k: &str| inp.get(k).and_then(Value::as_str).unwrap_or("");
    let mut body: Vec<Row> = vec![];
    let add = |body: &mut Vec<Row>, runs: Vec<Run>, first: &[Run]| {
        body.extend(wrap(&runs, inner, first, first))
    };
    let tool = item.tool.as_str();
    if tool == "Bash" || tool == "PowerShell" || COMMAND_TOOLS.contains(&tool) {
        for ln in s("command").split('\n').take(8) {
            add(&mut body, vec![r(ln, C_BRIGHT)], &[r("  ", C_TEXT)]);
        }
        if !s("description").is_empty() {
            add(
                &mut body,
                vec![r(s("description"), C_DIM)],
                &[r("  ", C_TEXT)],
            );
        }
        // cwd / env / ... of terminal_spawn and verify_run are as decisive as the command
        if COMMAND_TOOLS.contains(&tool)
            && let Some(m) = inp.as_object()
        {
            let rest: serde_json::Map<String, Value> = m
                .iter()
                .filter(|(k, _)| *k != "command")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                input_json_rows(&mut body, item, &Value::Object(rest), inner);
            }
        }
    } else if let Some(rows) = (tool == "Edit" || tool == "MultiEdit" || tool == "Write")
        .then(|| diffview::dialog_rows(&item.diff, inner, DIALOG_DIFF_ROWS))
        .flatten()
    {
        add(&mut body, vec![rb(s("file_path"), C_BRIGHT)], &[]);
        body.extend(rows);
    } else if tool == "Edit" || tool == "MultiEdit" {
        add(&mut body, vec![rb(s("file_path"), C_BRIGHT)], &[]);
        let lang = lang_from_file(inp.get("file_path").and_then(Value::as_str)).unwrap_or("");
        for e in input_edits(inp).into_iter().take(3) {
            body.extend(diff_rows(
                e.get("old_string"),
                e.get("new_string"),
                inner,
                6,
                lang,
                None,
            ));
        }
    } else if tool == "Write" {
        add(&mut body, vec![rb(s("file_path"), C_BRIGHT)], &[]);
        let lines: Vec<&str> = s("content").split('\n').collect();
        for ln in lines.iter().take(6) {
            body.push(vec![r(clip_to(&format!("+ {ln}"), inner), C_ADD)]);
        }
        if lines.len() > 6 {
            body.push(vec![r(
                format!("  … {} more lines", lines.len() - 6),
                C_SUBTLE,
            )]);
        }
    } else if tool == "WebFetch" {
        add(&mut body, vec![r(s("url"), C_BRIGHT)], &[]);
        if !s("prompt").is_empty() {
            add(&mut body, vec![r(s("prompt"), C_DIM)], &[]);
        }
    } else {
        input_json_rows(&mut body, item, inp, inner);
    }
    let shown_path = if !s("file_path").is_empty() {
        s("file_path")
    } else {
        s("url")
    };
    let desc = if !item.description.is_empty() && !shown_path.ends_with(&item.description) {
        item.description.as_str()
    } else {
        ""
    };
    if !desc.is_empty() && tool != "Bash" && tool != "PowerShell" {
        add(&mut body, vec![r(desc, C_SUBTLE)], &[]);
    }
    if let Some(dl) = item.deadline {
        let left = ((dl - now) as f64 / 1000.0).ceil().max(0.0) as i64;
        body.push(vec![]);
        add(
            &mut body,
            vec![r(
                format!(
                    "Critical path removal · denied automatically in {}:{:02} · press any key to keep waiting",
                    left / 60,
                    left % 60
                ),
                C_WARN,
            )],
            &[],
        );
    }
    if !item.prompt.is_empty() {
        add(&mut body, vec![r(&item.prompt, C_WARN)], &[]);
    }
    body.push(vec![]);
    body.push(vec![r("Do you want to proceed?", C_BRIGHT)]);
    for (i, o) in item.options.iter().flatten().enumerate() {
        let sel = i == item.choice;
        body.extend(wrap(
            &[run_full(
                format!("{}. {}", i + 1, o.label),
                if sel { C_PERM } else { C_TEXT },
                sel,
                false,
                false,
            )],
            inner,
            &[rb(if sel { "❯ " } else { "  " }, C_PERM)],
            &[r("     ", C_TEXT)],
        ));
    }
    if more > 0 {
        body.push(vec![r(
            format!(
                "+{more} more request{} waiting",
                if more > 1 { "s" } else { "" }
            ),
            C_SUBTLE,
        )]);
    }
    let title = perm_title(tool).map(String::from).unwrap_or_else(|| {
        if tool.starts_with("mcp__") {
            format!(
                "Tool use · {}",
                tool.split("__").skip(1).collect::<Vec<_>>().join(":")
            )
        } else if !item.display_name.is_empty() {
            item.display_name.clone()
        } else {
            tool.to_string()
        }
    });
    (framed(body, w, C_PERM, &title), vec![])
}

// ---------------------------------------------------------------- prompt box

const INPUT_MAX_ROWS: usize = 6;

struct Scan {
    w: usize,
    cur: usize,
    want: Option<(usize, usize)>,
    rows: Vec<Vec<char>>,
    row: usize,
    col: usize,
    pos: Option<(usize, usize)>,
    out: Option<Vec<char>>,
}
impl Scan {
    fn start(&mut self) {
        if let Some(o) = self.out.take() {
            self.rows.push(o);
        }
        self.out = match self.want {
            Some((f, t)) if self.row >= f && self.row < t => Some(vec![]),
            _ => None,
        };
    }
    fn next(&mut self) {
        self.row += 1;
        self.col = 0;
        self.start();
    }
    fn mark(&mut self, i: usize) {
        if i == self.cur {
            if self.col >= self.w {
                self.next();
            }
            self.pos = Some((self.row, self.col));
        }
    }
    fn put(&mut self, ch: char) {
        if let Some(o) = self.out.as_mut() {
            o.push(ch);
        }
        self.col += cw(ch);
    }
}

fn scan_input(
    chars: &[char],
    cur: usize,
    w: usize,
    want: Option<(usize, usize)>,
) -> (Vec<Vec<char>>, (usize, usize), usize) {
    let mut s = Scan {
        w,
        cur,
        want,
        rows: vec![],
        row: 0,
        col: 0,
        pos: None,
        out: None,
    };
    s.start();
    let n = chars.len();
    let width_of = |a: usize, b: usize| chars[a..b].iter().map(|c| cw(*c)).sum::<usize>();
    let mut i = 0;
    while i < n {
        s.mark(i);
        if chars[i] == '\n' {
            s.next();
            i += 1;
            continue;
        }
        let space = chars[i] == ' ';
        let mut end = i;
        while end < n && chars[end] != '\n' && (chars[end] == ' ') == space {
            end += 1;
        }
        if space {
            let mut word_end = end;
            while word_end < n && chars[word_end] != '\n' && chars[word_end] != ' ' {
                word_end += 1;
            }
            let sw = width_of(i, end);
            let ww = width_of(end, word_end);
            if end < n && s.col > 0 && s.col + sw + ww > w && ww <= w {
                s.next();
                i = end;
                continue;
            }
            while i < end {
                s.mark(i);
                if s.col + cw(chars[i]) > w {
                    s.next();
                }
                s.put(chars[i]);
                i += 1;
            }
            continue;
        }
        let ww = width_of(i, end);
        if s.col > 0 && s.col + ww > w {
            s.next();
        }
        while i < end {
            s.mark(i);
            if s.col + cw(chars[i]) > w {
                s.next();
            }
            s.put(chars[i]);
            i += 1;
        }
    }
    s.mark(n);
    if let Some(o) = s.out.take() {
        s.rows.push(o);
    }
    (s.rows, s.pos.unwrap_or((0, 0)), s.row + 1)
}

/// Editor text -> only the display rows around the cursor.
pub fn layout_input(
    chars: &[char],
    cur: usize,
    w: usize,
) -> (Vec<Vec<char>>, (usize, usize), usize) {
    let (_, pos, total) = scan_input(chars, cur, w, None);
    let top = (pos.0 + 1)
        .saturating_sub(INPUT_MAX_ROWS)
        .min(total.saturating_sub(INPUT_MAX_ROWS));
    let (rows, _, _) = scan_input(chars, cur, w, Some((top, top + INPUT_MAX_ROWS)));
    (rows, pos, top)
}

struct InputMode {
    lead: &'static str,
    col: Col,
    lead_col: Option<Col>,
    hint: &'static str,
    label: &'static str,
}
fn input_mode(m: Option<&str>) -> Option<InputMode> {
    match m {
        Some("bash") => Some(InputMode {
            lead: "! ",
            col: C_BANG,
            lead_col: Some(C_BANG),
            hint: "run a shell command in this folder · backspace to leave bash mode",
            label: "! bash mode",
        }),
        Some("memory") => Some(InputMode {
            lead: "# ",
            col: C_PURPLE,
            lead_col: None,
            hint: "remember this in ./AGENTS.md · backspace to leave memory mode",
            label: "# memory mode",
        }),
        _ => None,
    }
}

fn vivid(name: &str) -> Option<Rgb> {
    Some(match name {
        "purple" => PURPLE,
        "blue" => BLUE,
        "green" => GREEN,
        "yellow" => YELLOW,
        "orange" => ORANGE,
        "pink" => PINK,
        "cyan" => CYAN,
        "teal" => TEAL,
        "red" => RED,
        _ => return None,
    })
}

fn input_rows(
    ed: &EditorView,
    w: usize,
    focused: bool,
    attachments: &[Image],
    mode: Option<&str>,
    prompt_color: Option<&str>,
    route_hint: Option<&str>,
) -> (Vec<Row>, Option<(usize, usize)>) {
    let inner = w.saturating_sub(6).max(4);
    let (shown, pos, top) = layout_input(ed.chars, ed.cur, inner);
    let im = input_mode(mode);
    let border = if let Some(im) = &im {
        im.col
    } else if let Some(c) = prompt_color.and_then(vivid) {
        col(c)
    } else if focused {
        col(ACCENT_CORE)
    } else {
        C_BORDER
    };
    let mut out: Vec<Row> = vec![vec![r(format!("╭{}╮", "─".repeat(w - 2)), border)]];
    for (i, a) in attachments.iter().enumerate() {
        let label = clip_to(&format!("▣ {}", image_label(a, i)), inner);
        out.push(vec![
            r("│ ", border),
            r("  ", C_TEXT),
            r(&label, C_LIGHT),
            r(
                format!("{} │", " ".repeat(inner.saturating_sub(str_w(&label)))),
                border,
            ),
        ]);
    }
    let raw = |t: String, w: usize| Run {
        raw: true,
        w,
        ..r(t, C_TEXT)
    };
    let mut caret = None;
    for (ri, chars) in shown.iter().enumerate() {
        let rr = ri + top;
        if focused && pos.0 == rr {
            // (x, y) inside the input rows: border + space + lead = 4 cells, then the chars before the caret
            caret = Some((
                4 + chars.iter().take(pos.1).map(|c| cw(*c)).sum::<usize>(),
                out.len(),
            ));
        }
        let lead = if rr == 0 {
            match &im {
                Some(im) => rb(im.lead, im.lead_col.unwrap_or(im.col)),
                None => rb("> ", if focused { C_LIGHT } else { C_SUBTLE }),
            }
        } else {
            r("  ", C_TEXT)
        };
        let mut cells: Vec<Run> = vec![];
        let mut used = 0;
        for (c, ch) in chars.iter().enumerate() {
            let is_cur = focused && pos.0 == rr && pos.1 == c;
            if is_cur {
                cells.push(raw(
                    format!("{}{}{ch}{RESET}", bg(ACCENT_LIGHT), fg(ACCENT_INK)),
                    cw(*ch),
                ));
            } else {
                cells.push(r(ch.to_string(), C_BRIGHT));
            }
            used += cw(*ch);
        }
        if focused && pos.0 == rr && pos.1 >= chars.len() {
            cells.push(raw(format!("{} {RESET}", bg(ACCENT_LIGHT)), 1));
            used += 1;
        }
        if ed.chars.is_empty() && rr == 0 {
            let mut hint = route_hint.map(String::from).unwrap_or_else(|| {
                if focused {
                    if !attachments.is_empty() {
                        "add a message, or press enter to send the image".into()
                    } else {
                        let keys = "Enter send · Shift+Enter newline · / commands · @ files · ! shell · Shift+Tab mode";
                        // The example prompt only when every key still fits next to it.
                        let full = format!("Try \"explain this repo\" · {keys}");
                        if str_w(&full) <= inner.saturating_sub(2) { full } else { keys.into() }
                    }
                } else {
                    "press tab to talk to the agent".into()
                }
            });
            if let Some(im) = &im
                && focused
            {
                hint = im.hint.to_string();
            }
            // Whole " · " items only; a single item longer than the room is cut.
            let room = inner.saturating_sub(used);
            let mut parts: Vec<&str> = hint.split(" · ").collect();
            while parts.len() > 1 && str_w(&parts.join(" · ")) > room {
                parts.pop();
            }
            let h: String = parts.join(" · ").chars().take(room).collect();
            used += str_w(&h);
            cells.push(r(h, C_SUBTLE));
        }
        let mut line = vec![r("│ ", border), lead];
        line.extend(cells);
        line.push(r(
            format!("{} │", " ".repeat(inner.saturating_sub(used))),
            border,
        ));
        out.push(line);
    }
    out.push(vec![r(format!("╰{}╯", "─".repeat(w - 2)), border)]);
    (out, caret)
}

const SUGGEST_ROWS: usize = 8;

fn suggestion_rows(sug: Option<&Suggestions>, selected: usize, w: usize) -> Vec<Row> {
    let Some(sug) = sug else { return vec![] };
    let total = sug.items.len();
    let start = (selected as i64 - (SUGGEST_ROWS / 2) as i64)
        .min(total as i64 - SUGGEST_ROWS as i64)
        .max(0) as usize;
    let shown: Vec<&Sug> = sug.items.iter().skip(start).take(SUGGEST_ROWS).collect();
    let label_w = (shown.iter().map(|s| str_w(&s.label)).max().unwrap_or(0) + 2).min(28);
    let mut rows: Vec<Row> = shown
        .iter()
        .enumerate()
        .map(|(j, s)| {
            let sel = start + j == selected;
            let label = clip_to(&s.label, label_w);
            let detail = clip_to(&s.detail, w.saturating_sub(label_w + 4));
            vec![
                rb(if sel { "❯ " } else { "  " }, C_LIGHT),
                run_full(
                    format!(
                        "{label}{}",
                        " ".repeat(label_w.saturating_sub(str_w(&label)))
                    ),
                    if sel { C_LIGHT } else { C_TEXT },
                    sel,
                    false,
                    false,
                ),
                r(detail, if sel { C_DIM } else { C_SUBTLE }),
            ]
        })
        .collect();
    if total > SUGGEST_ROWS {
        rows.push(vec![r(
            format!("  {}/{}", total.min(selected + 1), total),
            C_SUBTLE,
        )]);
    }
    rows
}

/// Which `cap` todos to show when the list does not fit: creation order is kept, the window
/// starts at the first open task and always includes the first in-progress one.
pub fn todo_window(todos: &[Todo], cap: usize) -> usize {
    if cap >= todos.len() {
        return 0;
    }
    let open = todos.iter().position(|t| t.status != "completed");
    let doing = todos.iter().position(|t| t.status == "in_progress");
    let mut start = open.unwrap_or(todos.len() - cap);
    if doing.is_some_and(|d| d >= start + cap) {
        start = doing.unwrap();
    }
    start.min(todos.len() - cap)
}

fn todo_rows(todos: &[Todo], w: usize, expanded: bool) -> Vec<Row> {
    if todos.is_empty() {
        return vec![];
    }
    let cap = if expanded {
        todos.len()
    } else {
        CLAMP_LINES.min(todos.len())
    };
    let start = todo_window(todos, cap);
    let shown = &todos[start..(start + cap).min(todos.len())];
    let mut rows: Vec<Row> = vec![];
    for (i, t) in shown.iter().enumerate() {
        let (mark, c) = match t.status.as_str() {
            "completed" => ("☒ ", C_SUBTLE),
            "in_progress" => ("■ ", C_LIGHT),
            _ => ("☐ ", C_DIM),
        };
        let text = if t.status == "in_progress" && !t.active_form.is_empty() {
            &t.active_form
        } else {
            &t.content
        };
        rows.extend(wrap(
            &[run_full(text, c, t.status == "in_progress", false, false)],
            w,
            &[
                r(if i == 0 { "  ⎿ " } else { "    " }, C_SUBTLE),
                r(mark, c),
            ],
            &[r("      ", C_TEXT)],
        ));
    }
    if todos.len() > CLAMP_LINES {
        rows.push(vec![r(
            if expanded {
                "    (ctrl+o to collapse)".to_string()
            } else {
                format!(
                    "    … +{} more lines (ctrl+o to expand)",
                    todos.len() - shown.len()
                )
            },
            C_SUBTLE,
        )]);
    }
    rows
}

fn preview_banner(p: &Preview, w: usize) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(10);
    let short_id: String = p.id.chars().take(8).collect();
    let mut body = wrap(
        &[
            r("Viewing ", C_DIM),
            rb(
                if p.title.is_empty() {
                    short_id.clone()
                } else {
                    p.title.clone()
                },
                C_BRIGHT,
            ),
            r(
                format!(
                    "  {short_id}{}",
                    if p.live {
                        " · running in another terminal"
                    } else {
                        ""
                    }
                ),
                C_SUBTLE,
            ),
        ],
        inner,
        &[],
        &[],
    );
    body.extend(wrap(
        &[
            rb("enter", C_LIGHT),
            r(" continue this session here   ", C_DIM),
            rb("esc", C_LIGHT),
            r(" back to your conversation   ", C_DIM),
            rb("ctrl+↑/↓", C_LIGHT),
            r(" other sessions", C_DIM),
        ],
        inner,
        &[],
        &[],
    ));
    framed(body, w, C_WARN, "Session preview")
}

fn search_rows(s: &Search, w: usize) -> Vec<Row> {
    let m = s.matched.clone().unwrap_or_default();
    framed(
        vec![
            vec![
                r("search: ", C_DIM),
                rb(&s.query, C_BRIGHT),
                r("▍", C_LIGHT),
            ],
            vec![r(
                if m.is_empty() {
                    "no match".to_string()
                } else {
                    format!("› {}", one_line(&m))
                },
                if m.is_empty() { C_SUBTLE } else { C_TEXT },
            )],
        ],
        w,
        C_LIGHT,
        "reverse-i-search · enter use · esc cancel · ctrl+r older",
    )
}

fn bypass_rows(w: usize) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(10);
    let mut body = wrap(
        &[r(
            "Bypass permissions skips normal tool permission prompts. Explicit deny rules and protected-path safeguards still apply.",
            C_WARN,
        )],
        inner,
        &[],
        &[],
    );
    body.push(vec![]);
    body.extend(wrap(
        &[
            rb("enter", C_LIGHT),
            r(" enable bypass permissions   ", C_DIM),
            rb("esc", C_LIGHT),
            r(" keep auto mode", C_DIM),
        ],
        inner,
        &[],
        &[],
    ));
    framed(body, w, C_WARN, "Warning: bypass permissions")
}

fn picker_rows(p: &Picker, w: usize, h: usize, frame: usize) -> Vec<Row> {
    let inner = w.saturating_sub(4).max(10);
    let mut body: Vec<Row> = vec![];
    if !p.subtitle.is_empty() {
        body.extend(wrap(&[r(&p.subtitle, C_SUBTLE)], inner, &[], &[]));
    }
    if p.loading {
        body.push(vec![
            rb(format!("{} ", spin(frame)), C_LIGHT),
            r(
                waiting(
                    if p.loading_text.is_empty() {
                        "loading…"
                    } else {
                        &p.loading_text
                    },
                    frame,
                ),
                C_BRIGHT,
            ),
        ]);
        return framed(body, w, C_PERM, &p.title);
    }
    let fixed = 6 + usize::from(!p.subtitle.is_empty());
    let max_window = DEFAULT_WINDOW.min(h.saturating_sub(fixed)).max(3);
    let mut p = p.clone();
    p.set_window(max_window);
    let items = p.filtered();
    let choice = p.choice.min(items.len().saturating_sub(1));
    let window = if p.window_size == 0 {
        DEFAULT_WINDOW
    } else {
        p.window_size
    };
    let offset = p.offset.min(items.len().saturating_sub(window));
    let visible: Vec<&&crate::picker::PickerItem> =
        items.iter().skip(offset).take(window).collect();
    let ordinal_w = items.len().max(1).to_string().len();
    let prefix_w = ordinal_w + 4;
    let combined =
        (inner.saturating_sub(2)).min((prefix_w + 10).max((inner as f64 * 0.55).floor() as usize));
    let label_w = visible
        .iter()
        .map(|it| str_w(&it.label))
        .max()
        .unwrap_or(0)
        .min(combined.saturating_sub(prefix_w + 2));
    let detail_col = combined.min(prefix_w + label_w + 2);
    if items.is_empty() {
        body.push(vec![r("no matches", C_SUBTLE)]);
    }
    if offset > 0 {
        body.extend(wrap(
            &[r(format!("↑ {offset} more"), C_SUBTLE)],
            inner,
            &[],
            &[],
        ));
    }
    for (i, it) in visible.iter().enumerate() {
        let idx = offset + i;
        let sel = idx == choice;
        let mark = if it.current { "✓ " } else { "  " };
        let label = format!(
            "{}. {mark}{}",
            pad_start(&(idx + 1).to_string(), ordinal_w),
            clip_to(&it.label, label_w)
        );
        let c = if sel { C_PERM } else { C_TEXT };
        let runs = vec![
            run_full(&label, c, sel, false, false),
            r(" ".repeat(detail_col.saturating_sub(str_w(&label))), c),
            r(&it.detail, if sel { C_DIM } else { C_SUBTLE }),
        ];
        body.extend(wrap(
            &runs,
            inner,
            &[rb(if sel { "❯ " } else { "  " }, C_PERM)],
            &[r("     ", C_TEXT)],
        ));
    }
    let remaining = items.len() as i64 - offset as i64 - visible.len() as i64;
    if remaining > 0 {
        body.extend(wrap(
            &[r(format!("↓ {remaining} more"), C_SUBTLE)],
            inner,
            &[],
            &[],
        ));
    }
    body.push(vec![]);
    // a yes/no style picker has nothing to filter
    let filterable = !p.no_filter;
    if filterable {
        body.push(vec![
            r("filter: ", C_DIM),
            rb(&p.query, C_BRIGHT),
            r("▍", C_LIGHT),
        ]);
    }
    body.extend(wrap(
        &[r(
            if filterable {
                "type to filter · ↑/↓ choose · pgup/pgdn page · home/end · enter select · esc cancel"
            } else {
                "↑/↓ choose · enter select · esc cancel"
            },
            C_SUBTLE,
        )],
        inner,
        &[],
        &[],
    ));
    framed(body, w, C_PERM, &p.title)
}

// ---------------------------------------------------------------- render_agent

pub struct Body {
    pub parts: Vec<Rc<Vec<Row>>>,
    pub skip: usize,
    pub total: usize,
    pub h: usize,
}

pub struct AgentOut {
    pub lines: Vec<String>,
    pub max_scroll: usize,
    pub body: Body,
    /// Hit boxes of the pending question dialog, in agent-pane content coordinates.
    pub question_hits: Vec<QHit>,
    /// Prompt caret `(x, y)` in `lines`: where the hardware cursor goes (IME candidate window).
    pub caret: Option<(usize, usize)>,
}

fn mode_label(mode: &str) -> Option<(&'static str, Col)> {
    Some(match mode {
        "default" => ("⏸ manual mode on", C_DIM),
        "acceptEdits" => ("⏵⏵ accept edits on", C_PURPLE),
        "plan" => ("⏸ plan mode on", C_TEAL),
        "auto" => ("⏵⏵ auto mode on", col(YELLOW)),
        "bypassPermissions" => ("⏵⏵ bypass permissions on", col(RED)),
        "dontAsk" => ("⏵⏵ don’t ask on", col(RED)),
        _ => return None,
    })
}

/// Display name of the mode the running turn was sent with, when it differs from the current one.
pub fn turn_mode_hint(a: &Agent) -> Option<String> {
    match &a.turn_mode {
        Some(tm)
            if !a.status.is_empty() && a.status != "idle" && !tm.is_empty() && *tm != a.mode =>
        {
            Some(mode_name(Some(tm)))
        }
        _ => None,
    }
}

pub fn render_agent(
    agent: &Agent,
    todos: &[Todo],
    editor: &EditorView,
    w: usize,
    h: usize,
    frame: usize,
    now: i64,
    opts: &AgentOpts,
) -> AgentOut {
    let content_w = w.max(10);
    let focused = opts.focused.unwrap_or(true);
    let pending = if opts.preview.is_none() {
        agent.pending.first()
    } else {
        None
    };

    let mut question_hits_rel: Vec<QHit> = vec![];
    let mut caret_rel: Option<(usize, usize)> = None;
    let bottom: Vec<Row> = if let Some(p) = &opts.preview {
        preview_banner(p, content_w)
    } else if let Some(p) = pending {
        let (rows, hits) = permission_rows(p, content_w, agent.pending.len() - 1, now);
        question_hits_rel = hits;
        rows
    } else if opts.bypass_confirmation {
        bypass_rows(content_w)
    } else if let Some(p) = &opts.picker {
        picker_rows(p, content_w, h, frame)
    } else if let Some(s) = &opts.search {
        search_rows(s, content_w)
    } else {
        let target = if opts.docked {
            None
        } else {
            opts.chat_target.as_deref().and_then(|t| {
                visible_subagents(&agent.subagents, Some(t))
                    .into_iter()
                    .find(|s| s.id == t)
            })
        };
        let route_hint = match (&target, &opts.chat_target) {
            (Some(t), _) => Some(format!("Type @{} to message this agent", t.display_name())),
            (None, Some(_)) if !opts.docked => {
                Some("Type @agent-name to message an agent".to_string())
            }
            _ => None,
        };
        let (mut rows, caret) = input_rows(
            editor,
            content_w,
            focused,
            &opts.attachments,
            opts.input_mode.as_deref(),
            agent.prompt_color.as_deref(),
            route_hint.as_deref(),
        );
        caret_rel = caret;
        rows.extend(suggestion_rows(
            opts.suggestions.as_ref(),
            opts.selected,
            content_w,
        ));
        rows
    };

    // status line under the bottom area
    let mode = mode_label(&agent.mode);
    let im = input_mode(opts.input_mode.as_deref());
    let mut left: Vec<Run> = if let (Some(im), None) = (&im, &opts.preview) {
        vec![
            rb(im.label, im.col),
            r(" · enter to run · esc to cancel", C_SUBTLE),
        ]
    } else if let Some((text, c)) = mode {
        vec![rb(text, c), r(" (shift+tab to cycle)", C_SUBTLE)]
    } else {
        vec![r("? for shortcuts · shift+tab: mode", C_SUBTLE)]
    };
    let effort_text = format!(
        "effort: {}",
        agent
            .effort_label
            .clone()
            .filter(|e| !e.is_empty())
            .or(Some(agent.effort.clone()).filter(|e| !e.is_empty()))
            .unwrap_or_else(|| "auto".into())
    );
    let cfg = opts.config.as_ref().or(agent.config.as_ref());
    let model_name = model_parts(&agent.model, &agent.active_model, cfg)
        .map(|p| p.id)
        .unwrap_or_default();
    let right_for = |name: &str| {
        if name.is_empty() {
            effort_text.clone()
        } else {
            format!("{name} · {effort_text}")
        }
    };
    let right = right_for(&model_name);
    let running_mode = if im.is_none() && mode.is_some() {
        turn_mode_hint(agent)
    } else {
        None
    };
    if let Some(rm) = running_mode {
        let extra = format!(" · this turn: {rm}");
        if row_w(&left) + str_w(&extra) + str_w(&right) + 2 <= content_w {
            left.push(r(extra, C_SUBTLE));
        }
    }
    let mut shown = right.clone();
    let mut left_runs = left.clone();
    if row_w(&left) + str_w(&right) + 2 > content_w {
        let room_for = |runs: &[Run]| {
            content_w as i64 - row_w(runs) as i64 - 2 - str_w(&format!(" · {effort_text}")) as i64
        };
        let mut room = room_for(&left);
        if room < 4 && mode.is_some() && im.is_none() {
            left_runs = left[..1].to_vec();
            room = room_for(&left_runs);
        }
        // Never cut a word: priority mode > model > effort; whole items only (QA B-27).
        let free = content_w as i64 - row_w(&left_runs) as i64 - 2;
        shown = if !model_name.is_empty() && str_w(&model_name) as i64 <= room {
            right_for(&model_name)
        } else if !model_name.is_empty() && str_w(&model_name) as i64 <= free {
            model_name.clone()
        } else {
            effort_text.clone()
        };
    }
    let status: Vec<Run> = if row_w(&left_runs) + str_w(&shown) + 2 <= content_w {
        let mut s = left_runs.clone();
        s.push(r(
            " ".repeat(content_w - row_w(&left_runs) - str_w(&shown)),
            C_TEXT,
        ));
        s.push(r(&shown, C_SUBTLE));
        s
    } else {
        left.clone()
    };

    let mut working: Vec<Row> = vec![];
    if opts.preview.is_some() {
        // nothing: the preview shows another session
    } else if agent.status != "idle" && pending.is_none() {
        let secs = fmt_dur(now - agent.turn_start);
        let glyph = spin(frame);
        let shimmer = col(lerp(
            ACCENT_CORE,
            ACCENT_GLOW,
            ((frame as f64 / 3.0).sin() + 1.0) / 2.0,
        ));
        let thinking_now = agent
            .items
            .last()
            .is_some_and(|l| l.kind == "thinking" && l.streaming);
        let verb = if let Some(a) = &agent.activity {
            a.as_str()
        } else if agent.interrupting {
            "Interrupting"
        } else if agent.status == "starting" {
            "Starting"
        } else {
            turn_verb(agent.turn_start)
        };
        let mut bits = vec![secs];
        if agent.activity.is_none() {
            bits.push(format!("↓ {} tokens", fmt_tok(agent.turn_tokens as f64)));
            if thinking_now {
                bits.push("thinking".into());
            }
            bits.push("esc to interrupt".into());
        }
        working.push(vec![
            rb(format!("{glyph} "), shimmer),
            rb(format!("{} ", waiting(verb, frame)), shimmer),
            r(format!("({})", bits.join(" · ")), C_DIM),
        ]);
    } else if let Some(p) = pending {
        let what = match p.dialog.as_str() {
            "question" => "Synthesis is asking you",
            "plan" => "Synthesis finished planning",
            _ => "Synthesis needs your permission",
        };
        working.push(vec![
            rb("◆ ", C_PERM),
            rb(what, C_PERM),
            r("  ↑/↓ choose · enter confirm · esc reject", C_SUBTLE),
        ]);
    }
    if opts.preview.is_none() && todos.iter().any(|t| t.status != "completed") {
        working.extend(todo_rows(todos, content_w, opts.expanded));
    }
    for row in opts.agent_rows.iter().take(6) {
        working.push(vec![
            rb("◆ ", C_ACCENT),
            r(clip_to(row, content_w.saturating_sub(4)), C_DIM),
        ]);
    }
    if let Some(t) = &opts.route {
        working.push(vec![
            r("to ", C_SUBTLE),
            rb(clip_to(t, content_w.saturating_sub(4)), C_ACCENT),
        ]);
    }
    for q in opts.queued.iter().take(3) {
        let one_line = q.replace('\n', " ");
        working.push(vec![
            r("⧗ queued: ", C_WARN),
            r(clip_to(&one_line, content_w.saturating_sub(12)), C_DIM),
        ]);
    }
    if opts.queued.len() > 3 {
        working.push(vec![r(
            format!(
                "⧗ +{} more queued · esc cancels, ↑ edits",
                opts.queued.len() - 3
            ),
            C_DIM,
        )]);
    }

    let selector: Vec<Row> = if opts.preview.is_some()
        || visible_subagents(&agent.subagents, opts.chat_target.as_deref()).is_empty()
    {
        vec![]
    } else {
        subagent_rows(&agent.subagents, opts.chat_target.as_deref(), opts.docked)
    };
    let reserved = selector.len() + bottom.len() + 1 + working.len();
    let msg_h = h.saturating_sub(reserved);
    let mut parts: Vec<Rc<Vec<Row>>> = vec![];
    let items: &[Item] = opts.preview.as_ref().map_or(&agent.items, |p| &p.items);
    if opts.preview.is_some() && items.is_empty() {
        parts.push(Rc::new(vec![vec![r(
            "  (no messages in this session yet)",
            C_SUBTLE,
        )]]));
    } else if items.is_empty() {
        parts.push(Rc::new(welcome_rows(agent, content_w)));
    } else {
        let mut prev: Option<&str> = None;
        for it in items {
            let focus_kinds = ["user", "text", "result", "note", "help", "info", "compact"];
            if !agent.focus_view
                || opts.preview.is_some()
                || focus_kinds.contains(&it.kind.as_str())
            {
                parts.extend(note_gap(prev, it));
                prev = Some(&it.kind);
                parts.push(cached_item_rows(it, content_w, frame, now, opts.expanded));
            }
        }
    }
    let win = window_rows(&parts, opts.scroll, msg_h);
    let (max_scroll, off, skip, total) = (win.max_scroll, win.off, win.skip, win.total);
    let mut lines: Vec<String> = vec![];
    for _ in win.view.len()..msg_h {
        lines.push(String::new());
    }
    for rw in &win.view {
        lines.push(paint_row(rw));
    }
    if off > 0 && !lines.is_empty() {
        let last = lines.len() - 1;
        lines[last] = paint_row(&[r(format!("↓ {off} more rows · pgdn"), C_WARN)]);
    }
    for rw in &working {
        lines.push(paint_row(rw));
    }
    let bottom_start = lines.len();
    for rw in &bottom {
        lines.push(paint_row(rw));
    }
    for rw in &selector {
        lines.push(paint_row(rw));
    }
    lines.push(paint_row(&status));
    let line_offset = lines.len().saturating_sub(h);
    let question_hits = if pending.is_some_and(|p| p.dialog == "question") {
        question_hits_rel
            .into_iter()
            .map(|mut q| {
                q.x0 += 4;
                q.x1 += 4;
                q.y = (bottom_start + 1 + q.body_row) as i64 - line_offset as i64;
                q
            })
            .collect()
    } else {
        vec![]
    };
    drop(win);
    let start = lines.len().saturating_sub(h);
    let caret = caret_rel.and_then(|(x, y)| (bottom_start + y).checked_sub(start).map(|y| (x, y)));
    AgentOut {
        caret,
        lines: lines.split_off(start),
        max_scroll,
        body: Body {
            parts,
            skip,
            total,
            h: msg_h,
        },
        question_hits,
    }
}

/// Read-only tail of a conversation, `h` painted rows: a sub-agent transcript docked beside the
/// topology. Returns `(lines, max_scroll)`.
pub fn transcript_rows(
    agent: &Agent,
    w: usize,
    h: usize,
    frame: usize,
    now: i64,
    expanded: bool,
    scroll: usize,
) -> (Vec<String>, usize) {
    let mut parts: Vec<Rc<Vec<Row>>> = vec![];
    for (i, it) in agent.items.iter().enumerate() {
        parts.extend(note_gap(
            i.checked_sub(1).map(|p| agent.items[p].kind.as_str()),
            it,
        ));
        parts.push(cached_item_rows(it, w.max(10), frame, now, expanded));
    }
    let win = window_rows(&parts, scroll, h);
    let mut lines: Vec<String> = vec![String::new(); h.saturating_sub(win.view.len())];
    lines.extend(win.view.iter().map(|rw| paint_row(rw)));
    if win.off > 0 && !lines.is_empty() {
        let last = lines.len() - 1;
        lines[last] = paint_row(&[r(format!("↓ {} more rows · scroll", win.off), C_WARN)]);
    }
    (lines, win.max_scroll)
}

/// Plain text of the chat body rows `from..=to` (counted from the top of the conversation): the
/// same rows as the screen without colours; decorations count as blanks and `lead` says how many
/// leading cells are only decoration.
pub fn body_sel_rows(body: &Body, from: usize, to: usize) -> Vec<SelRow> {
    let mut out = vec![];
    let mut k = -(body.skip as i64);
    for p in &body.parts {
        if k + p.len() as i64 <= from as i64 {
            k += p.len() as i64;
            continue;
        }
        for rw in p.iter() {
            if k >= from as i64 && k <= to as i64 {
                let width = |x: &Run| if x.raw { x.w } else { str_w(&x.t) };
                let lead = rw
                    .iter()
                    .filter(|x| x.raw || !x.t.is_empty())
                    .take_while(|x| !x.copy)
                    .map(width)
                    .sum::<usize>();
                let copy: Row = rw
                    .iter()
                    .map(|x| {
                        if x.copy {
                            x.clone()
                        } else {
                            Run {
                                t: " ".repeat(width(x)),
                                ..x.clone()
                            }
                        }
                    })
                    .collect();
                out.push(SelRow {
                    text: synthesis_tui_base::text::strip_ansi(&paint_row(&copy)),
                    soft: rw.last().is_some_and(|x| x.soft),
                    lead: lead as i64,
                });
            }
            k += 1;
        }
        if k > to as i64 {
            break;
        }
    }
    out
}
