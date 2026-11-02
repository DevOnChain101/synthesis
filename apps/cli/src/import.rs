//! `synt import claude`: COPY/CONVERT a Claude Code setup into Synthesis-owned files.
//!
//! Claude files are only read here, never at runtime (docs/IMPORT.md). Every path derives from
//! the injected `Env` (HOME / SYNTHESIS_HOME) or from `--from` / `--project-dir`; nothing else is
//! touched. Everything is computed in memory first (a malformed source or target aborts before
//! the first write), then written atomically with a timestamped backup of each changed file.
//! Credentials are never copied: literal secrets in MCP entries become `${VAR}` references,
//! secret-shaped or foreign env names are skipped, `bypassPermissions`/`auto` never reach a
//! settings file. Output holds names and counts only, never values.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};
use synthesis_config::{Env, is_secret_shaped};
use synthesis_settings::is_secret_env_key;

const MAX_TEXT: u64 = 1 << 20;
/// `~/.claude.json` also holds caches and history: allow it to be large.
const MAX_CLAUDE_JSON: u64 = 64 << 20;
const MAX_FILES_PER_TREE: usize = 500;
const HEADER_PREFIX: &str = "<!-- imported from ";
const HOOK_EVENTS: [&str; 9] = [
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "SubagentStop",
    "SessionStart",
    "SessionEnd",
    "Notification",
    "PreCompact",
];
/// Tool names Synthesis's toolbox knows (rules may also name `mcp__*` / `browser_*`).
const SYNTHESIS_TOOLS: [&str; 16] = [
    "Bash",
    "BashOutput",
    "KillShell",
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "Task",
    "Skill",
    "NotebookEdit",
    "ExitPlanMode",
    "AskUserQuestion",
];
const RULE_ONLY_TOOLS: [&str; 3] = ["MultiEdit", "LS", "Agent"];
const BROAD_TOOLS: [&str; 6] = [
    "Bash",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "WebFetch",
];
/// Default modes a settings file may carry (`bypassPermissions` and `auto` never).
const SAFE_MODES: [&str; 5] = ["default", "manual", "acceptEdits", "plan", "dontAsk"];
const PROJECT_MODES: [&str; 4] = ["default", "manual", "plan", "dontAsk"];
/// Handled (or deliberately ignored) top-level settings keys; the rest are listed by name.
const KNOWN_KEYS: [&str; 5] = ["permissions", "hooks", "env", "model", "statusLine"];

#[derive(Subcommand)]
pub enum ImportCmd {
    /// Import a Claude Code setup (dry run unless --apply).
    Claude(Box<ClaudeArgs>),
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ScopeSel {
    User,
    Project,
    All,
}

#[derive(Args)]
pub struct ClaudeArgs {
    #[arg(long, value_enum, default_value = "all")]
    scope: ScopeSel,
    /// Project to import into `<DIR>/.synthesis` (needed for the project scope).
    #[arg(long)]
    project_dir: Option<PathBuf>,
    /// Claude config directory (default: $HOME/.claude; `.claude.json` is read from its parent).
    #[arg(long)]
    from: Option<PathBuf>,
    /// Print the plan only (the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Write the files (after a backup of each changed file).
    #[arg(long)]
    apply: bool,
    /// Comma list: instructions,permissions,hooks,env,agents,skills,commands,mcp,memory
    /// (default: all but memory).
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Merge into existing Synthesis files, keeping their values (default).
    #[arg(long, conflicts_with_all = ["overwrite", "skip_existing"])]
    merge: bool,
    /// Replace what the import manages; backups are still made.
    #[arg(long, conflicts_with = "skip_existing")]
    overwrite: bool,
    /// Never touch a file that already exists.
    #[arg(long)]
    skip_existing: bool,
    /// Import allow rules such as bare `Bash` or `Bash(*)`.
    #[arg(long)]
    accept_broad_allow: bool,
    /// Write user-scope hooks (they run shell commands; review the printed list first).
    #[arg(long)]
    accept_hooks: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Instructions,
    Permissions,
    Hooks,
    Env,
    Agents,
    Skills,
    Commands,
    Mcp,
    Memory,
}

impl Kind {
    const ALL: [Kind; 9] = [
        Kind::Instructions,
        Kind::Permissions,
        Kind::Hooks,
        Kind::Env,
        Kind::Agents,
        Kind::Skills,
        Kind::Commands,
        Kind::Mcp,
        Kind::Memory,
    ];
    fn name(self) -> &'static str {
        match self {
            Kind::Instructions => "instructions",
            Kind::Permissions => "permissions",
            Kind::Hooks => "hooks",
            Kind::Env => "env",
            Kind::Agents => "agents",
            Kind::Skills => "skills",
            Kind::Commands => "commands",
            Kind::Mcp => "mcp",
            Kind::Memory => "memory",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Merge,
    Overwrite,
    SkipExisting,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Create,
    Update,
    Unchanged,
    Skip,
}

impl Act {
    fn word(self) -> &'static str {
        match self {
            Act::Create => "create",
            Act::Update => "update",
            Act::Unchanged => "unchanged",
            Act::Skip => "skip",
        }
    }
}

struct Item {
    kind: Kind,
    source: String,
    target: String,
    act: Act,
    notes: Vec<String>,
}

struct J {
    orig: Option<Value>,
    cur: Value,
}

struct F {
    orig: Option<Vec<u8>>,
    new: Vec<u8>,
}

/// `<root>/imports/claude.json`: what a previous import wrote (also the "ours" test for merge).
struct Manifest {
    prev: Map<String, Value>,
    new: Map<String, Value>,
}

struct Imp {
    mode: Mode,
    cats: BTreeSet<Kind>,
    broad_ok: bool,
    hooks_ok: bool,
    date: String,
    items: Vec<Item>,
    todo: Vec<String>,
    json: BTreeMap<PathBuf, J>,
    files: BTreeMap<PathBuf, F>,
    manifests: BTreeMap<PathBuf, Manifest>,
}

/// Where one scope's files go.
struct Target {
    /// `$SYNTHESIS_HOME` or `<project>/.synthesis`.
    root: PathBuf,
    user: bool,
}

// ---------------------------------------------------------------------------------------------
// small helpers

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// `Ok(None)`: absent. Regular files only (symlinks followed), size capped.
fn read_bytes(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !meta.is_file() || meta.len() > cap {
        return Err(format!(
            "{}: not a regular file or larger than {cap} bytes",
            path.display()
        ));
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    read_bytes(path, MAX_TEXT)?
        .map(|b| String::from_utf8(b).map_err(|_| format!("{}: not valid UTF-8", path.display())))
        .transpose()
}

/// A JSON object file; malformed or not an object is an error naming the file (no content).
fn read_obj(path: &Path, cap: u64) -> Result<Option<Value>, String> {
    let Some(b) = read_bytes(path, cap)? else {
        return Ok(None);
    };
    match serde_json::from_slice::<Value>(&b) {
        Ok(v @ Value::Object(_)) => Ok(Some(v)),
        Ok(_) => Err(format!(
            "{}: the top level must be a JSON object",
            path.display()
        )),
        Err(e) => Err(format!("malformed JSON in {}: {e}", path.display())),
    }
}

/// Regular files under `dir` (sorted, symlinks skipped), relative to `dir`.
fn walk(dir: &Path, depth: usize, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let Ok(t) = e.file_type() else { continue };
        let r = rel.join(e.file_name());
        if t.is_file() {
            out.push(r);
        } else if t.is_dir() && depth < 4 {
            walk(&e.path(), depth + 1, &r, out);
        }
        if out.len() >= MAX_FILES_PER_TREE {
            return;
        }
    }
}

fn list(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    walk(dir, 0, Path::new(""), &mut v);
    v
}

/// (year, month, day, seconds of day) of a unix time, UTC.
fn civil(secs: u64) -> (i64, u32, u32, u64) {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d, rem)
}

fn stamps() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d, r) = civil(secs);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!(
            "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
            r / 3600,
            r % 3600 / 60,
            r % 60
        ),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let e = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(e)?;
    let tmp = dir.join(format!(
        ".{}.synthesis-import-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f")
    ));
    {
        use std::io::Write;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            // settings/synthesis.json may hold env or headers: owner-only, like every file we write
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp).map_err(e)?;
        f.write_all(bytes).and_then(|()| f.sync_all()).map_err(e)?;
    }
    std::fs::rename(&tmp, path).map_err(|x| {
        let _ = std::fs::remove_file(&tmp);
        e(x)
    })
}

fn backup(path: &Path, stamp: &str) -> Result<(), String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let mut dst = path.with_file_name(format!("{name}.bak-{stamp}"));
    let mut n = 1;
    while dst.exists() {
        dst = path.with_file_name(format!("{name}.bak-{stamp}-{n}"));
        n += 1;
    }
    std::fs::copy(path, &dst)
        .map(|_| ())
        .map_err(|e| format!("backup of {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// secrets

/// Header/env/flag name that carries a credential.
fn key_secret(k: &str) -> bool {
    let n = k.to_ascii_uppercase().replace(['-', '.'], "_");
    is_secret_shaped(&n) || n.contains("APIKEY") || n.contains("AUTHORIZATION")
}

/// A value that looks like a literal credential (known prefix or long random string).
fn looks_secret(v: &str) -> bool {
    let v = v.trim();
    let v = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
        .unwrap_or(v);
    if v.contains("${") || v.is_empty() {
        return false;
    }
    const PREFIXES: [&str; 17] = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "AKIA",
        "ASIA",
        "AIza",
        "glpat-",
        "npm_",
        "eyJ",
    ];
    if PREFIXES.iter().any(|p| v.starts_with(p)) {
        return true;
    }
    v.len() >= 24
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-+=.".contains(c))
        && v.chars().any(|c| c.is_ascii_digit())
        && v.chars().any(|c| c.is_ascii_alphabetic())
        && !v.starts_with(['.', '~'])
}

fn var_part(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        };
        if c != '_' || !o.ends_with('_') {
            o.push(c);
        }
    }
    o.trim_matches('_').to_string()
}

/// `${VAR}` names in `v`.
fn refs(v: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        let Some(end) = rest[i..].find('}') else {
            break;
        };
        let n = &rest[i + 2..i + end];
        out.push(n.split(":-").next().unwrap_or(n).to_string());
        rest = &rest[i + end + 1..];
    }
    out
}

/// Fits a Claude MCP entry into Synthesis's closed `config.json/v1` server schema (a key outside it
/// makes the whole file invalid): unknown keys are dropped by name, `disabled` becomes `enabled`.
fn conform_server(cfg: &Value) -> (Value, Vec<String>) {
    const FIELDS: [&str; 8] = [
        "type",
        "command",
        "args",
        "url",
        "enabled",
        "env",
        "environment",
        "headers",
    ];
    let mut notes = vec![];
    let Some(o) = cfg.as_object() else {
        return (cfg.clone(), notes);
    };
    let mut out = Map::new();
    let mut dropped = vec![];
    for (k, v) in o {
        match k.as_str() {
            "disabled" => {
                out.insert("enabled".into(), json!(v.as_bool().is_some_and(|d| !d)));
            }
            "type" => match v.as_str() {
                Some(t) if ["local", "remote", "stdio", "http", "sse"].contains(&t) => {
                    out.insert(k.clone(), v.clone());
                }
                _ if o.contains_key("url") => {
                    notes.push(format!(
                        "transport type {} is unknown to Synthesis; treated as http",
                        clean(&v.to_string())
                    ));
                    out.insert(k.clone(), json!("http"));
                }
                _ => dropped.push(k.as_str()),
            },
            "env" | "environment" | "headers" => {
                let m: Map<String, Value> = v
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(a, b)| match b {
                        Value::String(_) => Some((a.clone(), b.clone())),
                        Value::Number(_) | Value::Bool(_) => {
                            Some((a.clone(), json!(b.to_string())))
                        }
                        _ => None,
                    })
                    .collect();
                out.insert(k.clone(), Value::Object(m));
            }
            f if FIELDS.contains(&f) => {
                out.insert(k.clone(), v.clone());
            }
            _ => dropped.push(k.as_str()),
        }
    }
    if dropped.contains(&"oauth") || o.get("type").and_then(Value::as_str) == Some("oauth") {
        notes.push("needs OAuth; imported as-is, the login is left to the provider package".into());
    }
    if !dropped.is_empty() {
        notes.push(format!(
            "keys outside Synthesis's MCP schema dropped: {}",
            clean(&dropped.join(", "))
        ));
    }
    (Value::Object(out), notes)
}

/// Replaces literal secrets in one MCP entry by `${SERVER_...}` references. Returns the new
/// entry, the variables introduced and warnings. Never returns a secret value.
fn redact_server(name: &str, cfg: &Value) -> (Value, BTreeSet<String>, Vec<String>) {
    let mut vars = BTreeSet::new();
    let mut warns = vec![];
    let srv = var_part(name);
    let mut var_for = |key: &str| {
        let k = var_part(key);
        let k = if k == "AUTHORIZATION" {
            "TOKEN".to_string()
        } else {
            k.strip_prefix("X_").unwrap_or(&k).to_string()
        };
        let v = if k.starts_with(&srv) {
            k
        } else {
            format!("{srv}_{k}")
        };
        vars.insert(v.clone());
        v
    };
    let mut out = cfg.clone();
    let Some(o) = out.as_object_mut() else {
        return (out, vars, warns);
    };
    for field in ["env", "headers"] {
        let Some(m) = o.get_mut(field).and_then(Value::as_object_mut) else {
            continue;
        };
        for (k, v) in m.iter_mut() {
            let Some(s) = v.as_str() else { continue };
            if s.is_empty() || s.contains("${") {
                continue;
            }
            if key_secret(k) || looks_secret(s) {
                let var = var_for(k);
                let scheme = s.split_once(' ').filter(|(w, _)| {
                    ["bearer", "basic", "token"].contains(&w.to_ascii_lowercase().as_str())
                });
                *v = json!(match scheme {
                    Some((w, _)) => format!("{w} ${{{var}}}"),
                    None => format!("${{{var}}}"),
                });
            }
        }
    }
    if let Some(args) = o.get_mut("args").and_then(Value::as_array_mut) {
        let mut prev_flag: Option<String> = None;
        for a in args.iter_mut() {
            let Some(s) = a.as_str().map(String::from) else {
                continue;
            };
            let mut next_flag = None;
            if s.contains("${") {
            } else if let Some((flag, val)) = s.split_once('=').filter(|(f, _)| f.starts_with('-'))
            {
                if (key_secret(flag) || looks_secret(val)) && !val.is_empty() {
                    let var = var_for(flag.trim_start_matches('-'));
                    *a = json!(format!("{flag}=${{{var}}}"));
                }
            } else if prev_flag.is_some() || (looks_secret(&s) && !s.starts_with('-')) {
                let var = var_for(prev_flag.as_deref().unwrap_or("ARG_SECRET"));
                *a = json!(format!("${{{var}}}"));
            } else if s.starts_with('-') && key_secret(&s) {
                next_flag = Some(s.trim_start_matches('-').to_string());
            }
            prev_flag = next_flag;
        }
    }
    if let Some(u) = o.get("url").and_then(Value::as_str).map(String::from) {
        let (nu, changed) = redact_url(&u, &mut var_for);
        if changed {
            o.insert("url".into(), json!(nu));
        }
    }
    if o.get("type").and_then(Value::as_str) == Some("remote") {
        warns.push(
            "remote server: if it needs OAuth, the login is left to the provider package".into(),
        );
    }
    (out, vars, warns)
}

fn redact_url(u: &str, var_for: &mut impl FnMut(&str) -> String) -> (String, bool) {
    let mut changed = false;
    let mut s = u.to_string();
    if let Some(i) = s.find("://") {
        let rest = &s[i + 3..];
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if let Some(at) = rest[..auth_end].rfind('@') {
            let ui = &rest[..at];
            if !ui.contains("${") {
                let user = ui.split(':').next().unwrap_or("");
                let new = if ui.contains(':') {
                    format!("{user}:${{{}}}", var_for("PASSWORD"))
                } else {
                    format!("${{{}}}", var_for("TOKEN"))
                };
                s = format!("{}://{}{}", &s[..i], new, &rest[at..]);
                changed = true;
            }
        }
    }
    if let Some((base, q)) = s.split_once('?') {
        let (q, frag) = q.split_once('#').map_or((q, None), |(a, b)| (a, Some(b)));
        let parts: Vec<String> = q
            .split('&')
            .map(|p| match p.split_once('=') {
                Some((k, v)) if !v.contains("${") && (key_secret(k) || looks_secret(v)) => {
                    changed = true;
                    format!("{k}=${{{}}}", var_for(k))
                }
                _ => p.to_string(),
            })
            .collect();
        s = format!(
            "{base}?{}{}",
            parts.join("&"),
            frag.map(|f| format!("#{f}")).unwrap_or_default()
        );
    }
    (s, changed)
}

/// Does a (Claude-shape) server entry still hold a literal secret?
fn has_literal_secret(cfg: &Value) -> bool {
    let (red, vars, _) = redact_server("x", cfg);
    !vars.is_empty() && red != *cfg
}

// ---------------------------------------------------------------------------------------------
// tool names, rules, frontmatter

/// Claude tool name to Synthesis's tool name (`None`: Synthesis has no such tool).
fn map_tool(name: &str) -> Option<&'static str> {
    SYNTHESIS_TOOLS
        .iter()
        .find(|t| **t == name)
        .copied()
        .or(match name {
            "Agent" => Some("Task"),
            "MultiEdit" => Some("Edit"),
            "LS" => Some("Glob"),
            "KillBash" => Some("KillShell"),
            _ => None,
        })
}

/// One `tools` item: `Name` or `Name(spec)`. `None`: dropped.
fn map_tool_item(item: &str) -> Option<String> {
    let (n, spec) = item.split_at(item.find('(').unwrap_or(item.len()));
    if n == "*" || n.starts_with("mcp__") || n.starts_with("browser_") {
        return Some(item.to_string());
    }
    map_tool(n).map(|m| format!("{m}{spec}"))
}

fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (vec![], String::new(), 0i32);
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .map(|x| x.trim().trim_matches(['"', '\'']).to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn translate_tools(v: &str, dropped: &mut Vec<String>) -> String {
    let (inner, bracket) = match v.trim().strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        Some(i) => (i, true),
        None => (v.trim(), false),
    };
    let mut seen: Vec<String> = vec![];
    for it in split_top(inner) {
        match map_tool_item(&it) {
            Some(m) if !seen.contains(&m) => seen.push(m),
            Some(_) => {}
            None => dropped.push(it),
        }
    }
    let j = seen.join(", ");
    if bracket { format!("[{j}]") } else { j }
}

/// Rewrites the tool lists of a `---` frontmatter and reports fields Synthesis does not use.
fn translate_fm(
    text: &str,
    tool_keys: &[&str],
    used: &[&str],
    what: &str,
    notes: &mut Vec<String>,
) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return text.to_string();
    }
    let Some(end) = lines[1..].iter().position(|l| l.trim_end() == "---") else {
        return text.to_string();
    };
    let end = end + 1; // index of the closing line
    let (mut out, mut cur_tools) = (String::from(lines[0]), false);
    let (mut unused, mut dropped) = (vec![], vec![]);
    for l in &lines[1..end] {
        let body = l.trim_end_matches(['\r', '\n']);
        let eol = &l[body.len()..];
        let top = !body.starts_with([' ', '\t', '-']) && body.contains(':');
        if top {
            let (k, v) = body.split_once(':').unwrap_or((body, ""));
            let k = k.trim();
            cur_tools = tool_keys.contains(&k);
            if !used.contains(&k) && !cur_tools {
                unused.push(k.to_string());
            }
            if cur_tools && !v.trim().is_empty() {
                out += &format!("{k}: {}{eol}", translate_tools(v, &mut dropped));
                continue;
            }
        } else if cur_tools && body.trim_start().starts_with("- ") {
            let item = body.trim_start()[2..].trim().trim_matches(['"', '\'']);
            match map_tool_item(item) {
                Some(m) => out += &format!("  - {m}{eol}"),
                None => dropped.push(item.to_string()),
            }
            continue;
        }
        out += l;
    }
    for l in &lines[end..] {
        out += l;
    }
    if !dropped.is_empty() {
        notes.push(format!(
            "{what}: tools with no Synthesis equivalent dropped: {}",
            clean(&dropped.join(", "))
        ));
    }
    if !unused.is_empty() {
        notes.push(format!(
            "{what}: fields Synthesis does not use (kept in the file): {}",
            clean(&unused.join(", "))
        ));
    }
    out
}

/// Body without frontmatter, and whether the frontmatter scoped the file to paths.
fn strip_fm(text: &str) -> (&str, bool) {
    let mut pos = 0;
    let mut first = true;
    let mut has_paths = false;
    for l in text.split_inclusive('\n') {
        pos += l.len();
        if first {
            if l.trim_end() != "---" {
                return (text, false);
            }
            first = false;
        } else if l.trim_end() == "---" {
            return (text[pos..].trim_start_matches('\n'), has_paths);
        } else {
            has_paths |= l.starts_with("paths:") || l.starts_with("globs:");
        }
    }
    (text, false)
}

/// Why a Claude permission rule cannot be carried over (`None`: copied verbatim; Synthesis's rule
/// engine is a port of Claude's, the `//`, `~/` and `/` path forms mean the same).
fn bad_rule(r: &str) -> Option<String> {
    if r.is_empty() {
        return Some("empty rule".into());
    }
    let tool = &r[..r.find('(').unwrap_or(r.len())];
    if r.contains('(') && (!r.ends_with(')') || r.ends_with("()")) {
        return Some("malformed rule".into());
    }
    let known = SYNTHESIS_TOOLS.contains(&tool)
        || RULE_ONLY_TOOLS.contains(&tool)
        || tool.starts_with("mcp__")
        || tool.starts_with("browser_")
        || tool == "*";
    (!known).then(|| format!("tool \"{}\" does not exist in Synthesis", clean(tool)))
}

fn is_broad(r: &str) -> bool {
    let (tool, spec) = r.split_at(r.find('(').unwrap_or(r.len()));
    r == "*" || (BROAD_TOOLS.contains(&tool) && ["", "(*)", "(**)", "(:*)"].contains(&spec))
}

/// Masks words that look like credentials (hook commands are printed).
fn mask_cmd(c: &str) -> String {
    clean(
        &c.split(' ')
            .map(|w| {
                let v = w.split_once('=').map_or(w, |(_, v)| v);
                if looks_secret(v) { "****" } else { w }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Drops the `<!-- imported from ... -->` line (its date changes on every run).
fn strip_header(b: &[u8]) -> &[u8] {
    if b.starts_with(HEADER_PREFIX.as_bytes()) {
        let i = b
            .iter()
            .position(|c| *c == b'\n')
            .map_or(b.len(), |i| i + 1);
        return &b[i..];
    }
    b
}

fn trim_nl(b: &[u8]) -> &[u8] {
    let s = b.iter().position(|c| *c != b'\n').unwrap_or(b.len());
    &b[s..]
}

fn merge_act(a: Act, b: Act) -> Act {
    match (a, b) {
        (Act::Unchanged, x) | (x, Act::Unchanged) => x,
        (Act::Skip, x) | (x, Act::Skip) => x,
        (Act::Create, Act::Create) => Act::Create,
        _ => Act::Update,
    }
}

// ---------------------------------------------------------------------------------------------
// planner

impl Imp {
    fn note(&mut self, kind: Kind, source: &str, target: &str, act: Act, notes: Vec<String>) {
        self.items.push(Item {
            kind,
            source: clean(source),
            target: clean(target),
            act,
            notes,
        });
    }

    fn manifest(&mut self, root: &Path) -> &mut Manifest {
        self.manifests.entry(root.to_path_buf()).or_insert_with(|| {
            let prev = read_obj(&root.join("imports").join("claude.json"), MAX_TEXT)
                .ok()
                .flatten()
                .and_then(|v| v.get("files").and_then(Value::as_object).cloned())
                .unwrap_or_default();
            Manifest {
                prev,
                new: Map::new(),
            }
        })
    }

    fn record(&mut self, root: &Path, rel: &str, kind: Kind, source: &str, count: Option<usize>) {
        let mut v = json!({"kind": kind.name(), "source": source});
        if let Some(c) = count {
            v["count"] = json!(c);
        }
        self.manifest(root).new.insert(rel.to_string(), v);
    }

    fn rel(root: &Path, p: &Path) -> String {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Plans one copied/converted file; `root` owns the manifest that tracks it.
    fn put_file(
        &mut self,
        kind: Kind,
        source: &Path,
        target: &Path,
        root: &Path,
        content: Vec<u8>,
        mut notes: Vec<String>,
    ) {
        let src = source.to_string_lossy().to_string();
        let tgt = target.to_string_lossy().to_string();
        let existing = match read_bytes(target, 8 * MAX_TEXT) {
            Ok(e) => e,
            Err(e) => {
                notes.push(e);
                self.note(kind, &src, &tgt, Act::Skip, notes);
                return;
            }
        };
        let rel = Self::rel(root, target);
        let ours = self.manifest(root).prev.contains_key(&rel);
        let act = match &existing {
            None => Act::Create,
            Some(e) if trim_nl(strip_header(e)) == trim_nl(strip_header(&content)) => {
                Act::Unchanged
            }
            Some(_) => match self.mode {
                Mode::Overwrite => Act::Update,
                Mode::SkipExisting => Act::Skip,
                Mode::Merge if ours => Act::Update,
                Mode::Merge => {
                    notes.push("exists and was not written by an import; kept (--overwrite replaces it, a backup is made)".into());
                    Act::Skip
                }
            },
        };
        if matches!(act, Act::Create | Act::Update) {
            self.files.insert(
                target.to_path_buf(),
                F {
                    orig: existing,
                    new: content,
                },
            );
        }
        if act != Act::Skip {
            self.record(root, &rel, kind, &src, None);
        }
        self.note(kind, &src, &tgt, act, notes);
    }

    /// Edits the node at `ptr` of the JSON file `path`; returns what happened to that node.
    fn jedit(
        &mut self,
        path: &Path,
        ptr: &[&str],
        f: impl FnOnce(&mut Value),
    ) -> Result<Act, String> {
        if !self.json.contains_key(path) {
            let orig = read_obj(path, MAX_TEXT)?;
            self.json.insert(
                path.to_path_buf(),
                J {
                    cur: orig.clone().unwrap_or_else(|| json!({})),
                    orig,
                },
            );
        }
        let j = self.json.get_mut(path).expect("inserted above");
        if self.mode == Mode::SkipExisting && j.orig.is_some() {
            return Ok(Act::Skip);
        }
        let mut node = &mut j.cur;
        let (last, parents) = ptr.split_last().expect("non-empty pointer");
        for k in parents {
            node = node
                .as_object_mut()
                .ok_or(format!("{}: {k} is not an object", path.display()))?
                .entry(*k)
                .or_insert_with(|| json!({}));
        }
        let m = node.as_object_mut().ok_or(format!(
            "{}: not an object at {}",
            path.display(),
            ptr.join(".")
        ))?;
        let before = m.get(*last).cloned();
        let slot = m.entry(*last).or_insert(Value::Null);
        f(slot);
        let after = if slot.is_null() {
            m.remove(*last);
            None
        } else {
            Some(slot.clone())
        };
        Ok(match (before, after) {
            (b, a) if b == a => Act::Unchanged,
            (None, _) => Act::Create,
            _ => Act::Update,
        })
    }

    // --- instructions -------------------------------------------------------------------------

    /// Relative `@imports` become copies under `<root>/imports/` so the Claude tree is not read.
    fn rewrite_imports(
        &mut self,
        text: &str,
        base: &Path,
        t: &Target,
        notes: &mut Vec<String>,
    ) -> String {
        let mut fence = false;
        let mut out = vec![];
        for line in text.lines() {
            let tr = line.trim_start();
            if tr.starts_with("```") || tr.starts_with("~~~") {
                fence = !fence;
            }
            if fence || !line.contains('@') {
                out.push(line.to_string());
                continue;
            }
            let mut new = String::new();
            let mut prev_ws = true;
            let mut rest = line;
            while let Some(c) = rest.chars().next() {
                if c == '@' && prev_ws {
                    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    let tok = &rest[1..end];
                    let relative =
                        !tok.is_empty() && !tok.starts_with(['/', '~']) && !tok.contains(':');
                    let src = base.join(tok);
                    if relative && src.is_file() {
                        if let Ok(Some(b)) = read_bytes(&src, MAX_TEXT) {
                            let name = format!("imports/{}.md", var_part(tok).to_lowercase());
                            self.put_file(
                                Kind::Instructions,
                                &src,
                                &t.root.join(&name),
                                &t.root.clone(),
                                b,
                                vec![],
                            );
                            new += &format!("@{name}");
                            notes.push("an @import was copied under imports/; its own nested imports are not followed".into());
                        } else {
                            new += &rest[..end];
                        }
                    } else {
                        if tok.starts_with("~/") && tok.contains(".claude/") {
                            notes.push("an @import points into ~/.claude and would be read at runtime; copy that file and edit the line".into());
                        }
                        new += &rest[..end];
                    }
                    rest = &rest[end..];
                    prev_ws = false;
                    continue;
                }
                prev_ws = c.is_whitespace();
                new.push(c);
                rest = &rest[c.len_utf8()..];
            }
            out.push(new);
        }
        let mut s = out.join("\n");
        if text.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    /// `project_dir`: `Some` for the project scope. `claude_dir`: `~/.claude` or `<p>/.claude`.
    fn instructions(
        &mut self,
        project_dir: Option<&Path>,
        claude_dir: &Path,
        t: &Target,
    ) -> Result<(), String> {
        let mut cands = vec![claude_dir.join("CLAUDE.md")];
        if let Some(p) = project_dir {
            cands.insert(0, p.join("CLAUDE.md"));
        }
        let mut base: Option<(PathBuf, String)> = None;
        for c in cands {
            if let Some(s) = read_text(&c)?.filter(|s| !s.trim().is_empty()) {
                base = Some((c, s));
                break;
            }
        }
        let rules_dir = claude_dir.join("rules");
        let rule_files: Vec<PathBuf> = list(&rules_dir)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        let target = project_dir.map_or(t.root.join("SYNTHESIS.md"), |p| p.join("SYNTHESIS.md"));
        let mut skip_note = None;
        if let Some(p) = project_dir {
            // Synthesis reads the project's CLAUDE.md and CLAUDE.local.md by itself; only
            // .claude/rules need a Synthesis-owned file (SYNTHESIS.md wins over CLAUDE.md, so it carries
            // the base text too).
            let winner = ["SYNTHESIS.md", "AGENTS.md"].into_iter().find(|n| {
                read_text(&p.join(n))
                    .ok()
                    .flatten()
                    .is_some_and(|s| !s.trim().is_empty() && !s.starts_with(HEADER_PREFIX))
            });
            if let Some((b, _)) = &base
                && rule_files.is_empty()
            {
                {
                    let n = winner
                        .map(|w| {
                            vec![format!(
                                "{w} exists and wins: this CLAUDE.md is not loaded by Synthesis"
                            )]
                        })
                        .unwrap_or_default();
                    self.note(
                        Kind::Instructions,
                        &b.to_string_lossy(),
                        "(loaded by Synthesis as is)",
                        Act::Unchanged,
                        n,
                    );
                }
            }
            let local = p.join("CLAUDE.local.md");
            if local.is_file() {
                self.note(
                    Kind::Instructions,
                    &local.to_string_lossy(),
                    "(loaded by Synthesis as is)",
                    Act::Unchanged,
                    vec![],
                );
            }
            if rule_files.is_empty() {
                return Ok(());
            }
            if let Some(w) = winner {
                skip_note = Some(format!(
                    "{w} already exists; add `@.synthesis/rules/<file>` lines to it by hand"
                ));
            }
        } else if base.is_none() && rule_files.is_empty() {
            return Ok(());
        }
        let mut notes = vec![];
        let mut body = String::new();
        if let Some((p, s)) = &base {
            let dir = p.parent().unwrap_or(claude_dir).to_path_buf();
            body += &self.rewrite_imports(s, &dir, t, &mut notes);
        }
        let mut imports = String::new();
        for r in &rule_files {
            let Some(text) = read_text(&rules_dir.join(r))? else {
                continue;
            };
            let (stripped, has_paths) = strip_fm(&text);
            if has_paths {
                notes.push(format!(
                    "rule {} is path-scoped in Claude; Synthesis loads it for every task",
                    clean(&r.to_string_lossy())
                ));
            }
            let rel = r.to_string_lossy().replace('\\', "/");
            self.put_file(
                Kind::Instructions,
                &rules_dir.join(r),
                &t.root.join("rules").join(r),
                &t.root.clone(),
                stripped.as_bytes().to_vec(),
                vec![],
            );
            imports += &if project_dir.is_some() {
                format!("@.synthesis/rules/{rel}\n")
            } else {
                format!("@rules/{rel}\n")
            };
        }
        if !imports.is_empty() {
            body += &format!("\n\n{imports}");
        }
        let src = base.as_ref().map_or(rules_dir.clone(), |b| b.0.clone());
        if body.len() > 32 * 1024 {
            notes.push(
                "larger than Synthesis's 32 KiB instruction cap: the tail is cut when loaded"
                    .into(),
            );
        }
        if let Some(n) = skip_note {
            notes.push(n);
            self.note(
                Kind::Instructions,
                &src.to_string_lossy(),
                &target.to_string_lossy(),
                Act::Skip,
                notes,
            );
            return Ok(());
        }
        let header = format!(
            "{HEADER_PREFIX}{} on {} by synt import claude -->\n",
            clean(&src.to_string_lossy()),
            self.date
        );
        let root = t.root.clone();
        self.put_file(
            Kind::Instructions,
            &src,
            &target,
            &root,
            format!("{header}{body}").into_bytes(),
            notes,
        );
        Ok(())
    }

    // --- settings -----------------------------------------------------------------------------

    fn settings(&mut self, src: &Path, dst: &Path, t: &Target, local: bool) -> Result<(), String> {
        let Some(raw) = read_obj(src, MAX_TEXT)? else {
            return Ok(());
        };
        let s = src.to_string_lossy().to_string();
        let rel = Self::rel(&t.root, dst);
        let overwrite = self.mode == Mode::Overwrite;
        let cx = Cx {
            s: &s,
            dst,
            t,
            rel: &rel,
        };
        if self.cats.contains(&Kind::Permissions) {
            if let Some(p) = raw.get("permissions").and_then(Value::as_object) {
                self.permissions(p, &cx)?;
            }
            if local {
                self.gitignore_local(dst)?;
            }
        }
        if self.cats.contains(&Kind::Hooks)
            && let Some(h) = raw.get("hooks").and_then(Value::as_object)
        {
            self.hooks(h, &cx, overwrite)?;
        }
        if self.cats.contains(&Kind::Env)
            && let Some(e) = raw.get("env").and_then(Value::as_object)
        {
            self.env(e, &cx, overwrite)?;
        }
        let mut notes = vec![];
        if let Some(m) = raw.get("model").and_then(Value::as_str) {
            notes.push(format!("model \"{}\" noted, not imported: use SYNTHESIS_MODEL or `model` in synthesis.json (Synthesis aliases opus|sonnet|haiku|fable exist)", clean(m)));
        }
        if raw.get("statusLine").is_some() {
            notes.push("statusLine not imported (Synthesis has its own status line)".into());
        }
        let ignored: Vec<&str> = raw
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .map(String::as_str)
            .filter(|k| !KNOWN_KEYS.contains(k))
            .collect();
        if !ignored.is_empty() {
            notes.push(format!(
                "other settings keys not imported (no Synthesis equivalent, or credential-related): {}",
                clean(&ignored.join(", "))
            ));
        }
        if !notes.is_empty() {
            self.note(
                Kind::Permissions,
                &s,
                &dst.to_string_lossy(),
                Act::Skip,
                notes,
            );
        }
        Ok(())
    }

    /// `.synthesis/.gitignore` lists settings.local.json (as Synthesis's own "allow always" does).
    fn gitignore_local(&mut self, dst: &Path) -> Result<(), String> {
        let gi = dst.with_file_name(".gitignore");
        let cur = read_text(&gi)?.unwrap_or_default();
        if !cur.lines().any(|l| l.trim() == "settings.local.json") {
            let sep = if cur.is_empty() || cur.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let orig = read_bytes(&gi, MAX_TEXT)?;
            self.files.insert(
                gi,
                F {
                    orig,
                    new: format!("{cur}{sep}settings.local.json\n").into_bytes(),
                },
            );
        }
        Ok(())
    }

    fn permissions(&mut self, p: &Map<String, Value>, cx: &Cx) -> Result<(), String> {
        let (s, dst, t) = (cx.s, cx.dst, cx.t);
        let mut notes = vec![];
        let mut lists: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for key in ["allow", "ask", "deny"] {
            let rules = p.get(key).and_then(Value::as_array).into_iter().flatten();
            for r in rules.filter_map(Value::as_str).map(str::trim) {
                if let Some(why) = bad_rule(r) {
                    notes.push(format!(
                        "{key} rule dropped ({why}){}",
                        if key == "deny" {
                            ": THIS RESTRICTION IS NOT CARRIED OVER"
                        } else {
                            ""
                        }
                    ));
                } else if key == "allow" && is_broad(r) && !self.broad_ok {
                    notes.push(format!(
                        "broad allow rule \"{}\" NOT imported (needs --accept-broad-allow)",
                        clean(r)
                    ));
                } else {
                    if key == "allow" && is_broad(r) {
                        notes.push(format!(
                            "broad allow rule \"{}\" imported on request",
                            clean(r)
                        ));
                    }
                    lists.entry(key).or_default().push(r.to_string());
                }
            }
        }
        let dirs: Vec<String> = p
            .get("additionalDirectories")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !dirs.is_empty() {
            lists.insert("additionalDirectories", dirs);
        }
        let mut mode = None;
        if let Some(m) = p.get("defaultMode").and_then(Value::as_str) {
            let ok = if t.user {
                SAFE_MODES.contains(&m)
            } else {
                PROJECT_MODES.contains(&m)
            };
            if ok {
                mode = Some(m.to_string());
            } else if m == "bypassPermissions" || m == "auto" {
                notes.push(format!("defaultMode \"{m}\" NOT imported (mapped to \"default\"); Synthesis never takes it from settings"));
            } else {
                notes.push(format!(
                    "defaultMode \"{}\" is not allowed here, left unset",
                    clean(m)
                ));
            }
        }
        if !t.user && lists.contains_key("allow") {
            notes.push(
                "project allow rules only apply when the project is trusted (--trust-project)"
                    .into(),
            );
        }
        let overwrite = self.mode == Mode::Overwrite;
        let (mut act, mut count) = (Act::Unchanged, 0);
        for (k, v) in &lists {
            count += v.len();
            let a = self.jedit(dst, &["permissions", k], |n| {
                let mut cur: Vec<Value> = if overwrite {
                    vec![]
                } else {
                    n.as_array().cloned().unwrap_or_default()
                };
                for r in v {
                    if !cur.iter().any(|x| x.as_str() == Some(r)) {
                        cur.push(json!(r));
                    }
                }
                *n = Value::Array(cur);
            })?;
            act = merge_act(act, a);
        }
        if let Some(m) = mode.filter(|m| m != "default") {
            let a = self.jedit(dst, &["permissions", "defaultMode"], |n| {
                if overwrite || n.is_null() {
                    *n = json!(m);
                }
            })?;
            act = merge_act(act, a);
            count += 1;
        }
        if count > 0 || !notes.is_empty() {
            if count > 0 && act != Act::Skip {
                self.record(
                    &t.root.clone(),
                    &format!("{}#permissions", cx.rel),
                    Kind::Permissions,
                    s,
                    Some(count),
                );
            }
            self.note(
                Kind::Permissions,
                s,
                &dst.to_string_lossy(),
                if count == 0 { Act::Skip } else { act },
                notes,
            );
        }
        Ok(())
    }

    fn hooks(&mut self, h: &Map<String, Value>, cx: &Cx, overwrite: bool) -> Result<(), String> {
        let (s, dst, t) = (cx.s, cx.dst, cx.t);
        let d = dst.to_string_lossy().to_string();
        let mut notes = vec![];
        let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut cmds = vec![];
        for (ev, gs) in h {
            if !HOOK_EVENTS.contains(&ev.as_str()) {
                notes.push(format!(
                    "hook event {} is not supported by Synthesis, skipped",
                    clean(ev)
                ));
                continue;
            }
            for g in gs.as_array().into_iter().flatten() {
                let mut keep = vec![];
                for c in g
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let cmd = c.get("command").and_then(Value::as_str).unwrap_or("");
                    if c.get("type").and_then(Value::as_str) != Some("command") || cmd.is_empty() {
                        notes.push(format!(
                            "a {} hook that is not a shell command was skipped",
                            clean(ev)
                        ));
                        continue;
                    }
                    let mut e = Map::new();
                    e.insert("type".into(), json!("command"));
                    e.insert("command".into(), json!(cmd));
                    if let Some(to) = c.get("timeout").filter(|x| x.is_u64()) {
                        e.insert("timeout".into(), to.clone());
                    }
                    cmds.push(format!("{ev}: {}", mask_cmd(cmd)));
                    keep.push(Value::Object(e));
                }
                if keep.is_empty() {
                    continue;
                }
                let mut ng = Map::new();
                if let Some(m) = g.get("matcher").and_then(Value::as_str) {
                    ng.insert("matcher".into(), json!(m));
                }
                ng.insert("hooks".into(), Value::Array(keep));
                groups
                    .entry(ev.clone())
                    .or_default()
                    .push(Value::Object(ng));
            }
        }
        if cmds.is_empty() {
            if !notes.is_empty() {
                self.note(Kind::Hooks, s, &d, Act::Skip, notes);
            }
            return Ok(());
        }
        notes.push(format!(
            "HOOKS {} command(s), they run with your shell permissions:",
            cmds.len()
        ));
        notes.extend(cmds.iter().map(|c| format!("  {c}")));
        if t.user && !self.hooks_ok {
            notes.push(
                "NOT imported: review the commands above, then re-run with --accept-hooks".into(),
            );
            self.todo.push(
                "Hooks were not imported: review them, then re-run with --accept-hooks.".into(),
            );
            self.note(Kind::Hooks, s, &d, Act::Skip, notes);
            return Ok(());
        }
        notes.push(if t.user {
            "imported on request; unreviewed until you read them".into()
        } else {
            "project hooks stay inert until the project is trusted (--trust-project)".into()
        });
        self.todo
            .push(format!("Review the imported hooks in {}.", clean(&d)));
        let n = cmds.len();
        let act = self.jedit(dst, &["hooks"], |node| {
            let mut cur = if overwrite {
                Map::new()
            } else {
                node.as_object().cloned().unwrap_or_default()
            };
            for (ev, add) in groups {
                let arr = cur.entry(ev).or_insert_with(|| json!([]));
                if let Some(a) = arr.as_array_mut() {
                    for g in add {
                        if !a.contains(&g) {
                            a.push(g);
                        }
                    }
                }
            }
            *node = Value::Object(cur);
        })?;
        if act != Act::Skip {
            self.record(
                &t.root.clone(),
                &format!("{}#hooks", cx.rel),
                Kind::Hooks,
                s,
                Some(n),
            );
        }
        self.note(Kind::Hooks, s, &d, act, notes);
        Ok(())
    }

    fn env(&mut self, e: &Map<String, Value>, cx: &Cx, overwrite: bool) -> Result<(), String> {
        let (s, dst, t) = (cx.s, cx.dst, cx.t);
        let mut keep = Map::new();
        let mut skipped = vec![];
        for (k, v) in e {
            let foreign = ["ANTHROPIC_", "OPENAI_", "CLAUDE_", "CODEX_"]
                .iter()
                .any(|p| k.starts_with(p));
            let scalar = matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_));
            if foreign
                || is_secret_env_key(k)
                || key_secret(k)
                || !scalar
                || k.contains(['=', '\0'])
                || v.as_str().is_some_and(looks_secret)
            {
                skipped.push(k.as_str());
            } else {
                keep.insert(k.clone(), v.clone());
            }
        }
        let mut notes = vec![];
        if !skipped.is_empty() {
            notes.push(format!(
                "{} variable(s) skipped (secret-shaped, foreign prefix or not a scalar): {}",
                skipped.len(),
                clean(&skipped.join(", "))
            ));
        }
        let n = keep.len();
        let act = if keep.is_empty() {
            Act::Skip
        } else {
            self.jedit(dst, &["env"], |node| {
                let mut cur = if overwrite {
                    Map::new()
                } else {
                    node.as_object().cloned().unwrap_or_default()
                };
                for (k, v) in keep {
                    cur.entry(k).or_insert(v);
                }
                *node = Value::Object(cur);
            })?
        };
        if act != Act::Skip {
            self.record(
                &t.root.clone(),
                &format!("{}#env", cx.rel),
                Kind::Env,
                s,
                Some(n),
            );
        }
        if n > 0 || !notes.is_empty() {
            self.note(Kind::Env, s, &dst.to_string_lossy(), act, notes);
        }
        Ok(())
    }

    // --- agents / skills / commands / memory ----------------------------------------------------

    fn agents(&mut self, from: &Path, t: &Target) -> Result<(), String> {
        for f in list(from) {
            if f.components().count() != 1 || f.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Some(text) = read_text(&from.join(&f))? else {
                continue;
            };
            let mut notes = vec![];
            let what = clean(&f.to_string_lossy());
            let out = translate_fm(
                &text,
                &["tools"],
                &["name", "description"],
                &what,
                &mut notes,
            );
            self.put_file(
                Kind::Agents,
                &from.join(&f),
                &t.root.join("agents").join(&f),
                &t.root.clone(),
                out.into_bytes(),
                notes,
            );
        }
        Ok(())
    }

    fn commands(&mut self, from: &Path, t: &Target) -> Result<(), String> {
        let mut first = true;
        for f in list(from) {
            if f.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Some(text) = read_text(&from.join(&f))? else {
                continue;
            };
            let mut notes = vec![];
            if std::mem::take(&mut first) {
                notes.push(
                    "Synthesis does not run custom slash commands yet; files are staged in commands/"
                        .into(),
                );
            }
            let what = clean(&f.to_string_lossy());
            let out = translate_fm(
                &text,
                &["allowed-tools"],
                &["description", "argument-hint", "model"],
                &what,
                &mut notes,
            );
            self.put_file(
                Kind::Commands,
                &from.join(&f),
                &t.root.join("commands").join(&f),
                &t.root.clone(),
                out.into_bytes(),
                notes,
            );
        }
        Ok(())
    }

    fn skills(&mut self, from: &Path, t: &Target) -> Result<(), String> {
        let Ok(rd) = std::fs::read_dir(from) else {
            return Ok(());
        };
        let mut dirs: Vec<_> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name())
            .collect();
        dirs.sort();
        for d in dirs {
            let dir = from.join(&d);
            if !dir.join("SKILL.md").is_file() {
                continue;
            }
            for f in list(&dir) {
                let Some(bytes) = read_bytes(&dir.join(&f), 8 * MAX_TEXT)? else {
                    continue;
                };
                let mut notes = vec![];
                let content = if f == Path::new("SKILL.md") {
                    let text = String::from_utf8(bytes)
                        .map_err(|_| format!("{}: not valid UTF-8", dir.join(&f).display()))?;
                    let what = format!("skill {}", clean(&d.to_string_lossy()));
                    translate_fm(
                        &text,
                        &["allowed-tools"],
                        &["name", "description", "license", "argument-hint"],
                        &what,
                        &mut notes,
                    )
                    .into_bytes()
                } else {
                    bytes
                };
                self.put_file(
                    Kind::Skills,
                    &dir.join(&f),
                    &t.root.join("skills").join(&d).join(&f),
                    &t.root.clone(),
                    content,
                    notes,
                );
            }
        }
        Ok(())
    }

    fn memory(
        &mut self,
        from: &Path,
        project_dir: &Path,
        synthesis_home: &Path,
    ) -> Result<(), String> {
        let key = synthesis_session::node::sanitize_path(&project_dir.to_string_lossy());
        let src = from.join("projects").join(&key).join("memory");
        let mut first = true;
        for f in list(&src) {
            if f.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Some(b) = read_bytes(&src.join(&f), MAX_TEXT)? else {
                continue;
            };
            let notes = if std::mem::take(&mut first) {
                vec!["loaded at start as low-priority, size-capped, untrusted notes".into()]
            } else {
                vec![]
            };
            let dst = synthesis_home
                .join("projects")
                .join(synthesis_app::ctx::auto_memory_key(project_dir))
                .join("memory")
                .join(&f);
            self.put_file(Kind::Memory, &src.join(&f), &dst, synthesis_home, b, notes);
        }
        Ok(())
    }

    // --- MCP ------------------------------------------------------------------------------------

    /// `servers`: Claude-shape map. `ptr`: where it goes in synthesis.json (user-owned file).
    fn mcp_servers(
        &mut self,
        servers: &Value,
        src: &str,
        synthesis_json: &Path,
        ptr: &[&str],
        root: &Path,
    ) -> Result<(), String> {
        let Some(map) = servers.as_object() else {
            return Ok(());
        };
        let overwrite = self.mode == Mode::Overwrite;
        for (name, cfg) in map {
            let label = format!("{src} [{}]", clean(name));
            if !cfg.is_object() || name.is_empty() {
                self.note(
                    Kind::Mcp,
                    &label,
                    "-",
                    Act::Skip,
                    vec!["not a server object, skipped".into()],
                );
                continue;
            }
            let (fit, mut notes) = conform_server(cfg);
            let (red, vars, more) = redact_server(name, &fit);
            notes.extend(more);
            for v in &vars {
                notes.push(format!("literal secret replaced by ${{{v}}}"));
                self.todo.push(format!("export {v} (replaces a literal secret of MCP server \"{}\") and list it in synthesis.mcp.env_passthrough of the user settings.json", clean(name)));
            }
            if let Some(h) = server_hash(name, &red, "user") {
                notes.push(format!("definition hash {h}"));
            }
            for v in refs(&red.to_string())
                .into_iter()
                .filter(|v| !vars.contains(v))
            {
                let line = format!(
                    "export {v} and list it in synthesis.mcp.env_passthrough of the user settings.json (MCP servers only see allowlisted variables)"
                );
                if !self.todo.contains(&line) {
                    self.todo.push(line);
                }
            }
            let mut p: Vec<&str> = ptr.to_vec();
            p.push(name);
            let new = red.clone();
            let mut kept_existing = false;
            let act = self.jedit(synthesis_json, &p, |n| {
                if n.is_null() || overwrite {
                    *n = new;
                } else if *n != new {
                    kept_existing = true;
                }
            })?;
            if kept_existing {
                notes.push("a different server of this name exists; kept (use --overwrite)".into());
            }
            if matches!(act, Act::Create | Act::Update) {
                self.jedit(synthesis_json, &["$schema"], |n| {
                    if n.is_null() {
                        *n = json!("config.json/v1");
                    }
                })?;
            }
            if act != Act::Skip && !kept_existing {
                self.record(
                    root,
                    &format!("synthesis.json#mcp/{}/{}", ptr.len(), clean(name)),
                    Kind::Mcp,
                    src,
                    None,
                );
            }
            let tgt = format!("{} [{}]", synthesis_json.display(), ptr.join("."));
            self.note(
                Kind::Mcp,
                &label,
                &tgt,
                if kept_existing { Act::Skip } else { act },
                notes,
            );
        }
        Ok(())
    }

    /// Project `.mcp.json` is read by Synthesis itself (hash approval); only report and warn.
    fn mcp_project_file(&mut self, p: &Path) -> Result<(), String> {
        let Some(v) = read_obj(p, MAX_TEXT)? else {
            return Ok(());
        };
        let Some(map) = v.get("mcpServers").and_then(Value::as_object) else {
            return Ok(());
        };
        for (name, cfg) in map {
            let mut notes = vec![];
            if has_literal_secret(cfg) {
                notes.push("holds a literal secret: edit .mcp.json to use ${VAR} (the import does not modify it)".into());
            }
            if let Some(h) = server_hash(name, cfg, "project") {
                self.todo.push(format!("approve project MCP server \"{}\" after review: --trust-project --approve-mcp {}@{h}", clean(name), clean(name)));
            }
            self.note(
                Kind::Mcp,
                &format!("{} [{}]", p.display(), clean(name)),
                "(read by Synthesis as is; needs approval)",
                Act::Unchanged,
                notes,
            );
        }
        Ok(())
    }

    // --- apply -----------------------------------------------------------------------------------

    fn finish_manifests(&mut self, from: &Path) {
        let roots: Vec<PathBuf> = self.manifests.keys().cloned().collect();
        for root in roots {
            let m = &self.manifests[&root];
            let mut files = m.prev.clone();
            files.extend(m.new.clone());
            if files == m.prev {
                continue;
            }
            let path = root.join("imports").join("claude.json");
            let doc = json!({"version": 1, "imported_at": self.date, "from": from.to_string_lossy(), "files": files});
            let orig = std::fs::read(&path).ok();
            let new = (serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n").into_bytes();
            self.files.insert(path, F { orig, new });
        }
    }

    /// (path, bytes, existed) of every file whose content changes.
    fn changes(&self) -> Vec<(PathBuf, Vec<u8>, bool)> {
        let mut v = vec![];
        for (p, j) in &self.json {
            if Some(&j.cur) != j.orig.as_ref() {
                let b =
                    (serde_json::to_string_pretty(&j.cur).unwrap_or_default() + "\n").into_bytes();
                v.push((p.clone(), b, j.orig.is_some()));
            }
        }
        for (p, f) in &self.files {
            if f.orig.as_deref() != Some(&f.new[..]) {
                v.push((p.clone(), f.new.clone(), f.orig.is_some()));
            }
        }
        v
    }
}

struct Cx<'a> {
    s: &'a str,
    dst: &'a Path,
    t: &'a Target,
    rel: &'a str,
}

fn server_hash(name: &str, cfg: &Value, scope: &str) -> Option<String> {
    synthesis_mcp::parse_claude_servers(&json!({ name: cfg.clone() }), Some(scope))
        .0
        .first()
        .map(|s| synthesis_app::mcp::definition_hash(s)[..24].to_string())
}

// ---------------------------------------------------------------------------------------------
// entry point

pub fn run_cmd(env: &Env, cmd: ImportCmd) -> Result<u8, String> {
    match cmd {
        ImportCmd::Claude(a) => claude(env, &a),
    }
}

fn claude(env: &Env, a: &ClaudeArgs) -> Result<u8, String> {
    let h = synthesis_app::homes(env)?;
    let from = match &a.from {
        Some(f) => f.clone(),
        None if h.user.as_os_str().is_empty() => {
            return Err("HOME is not set: pass --from DIR".into());
        }
        None => h.user.join(".claude"),
    };
    if !from.is_dir() {
        return Err(format!(
            "{} is not a directory (use --from)",
            from.display()
        ));
    }
    let project = match (&a.project_dir, a.scope) {
        (_, ScopeSel::User) => None,
        (Some(p), _) => Some(
            synthesis_core::canonicalize(p)
                .map_err(|e| format!("--project-dir {}: {e}", p.display()))?,
        ),
        (None, ScopeSel::Project) => return Err("--scope project needs --project-dir DIR".into()),
        (None, ScopeSel::All) => None,
    };
    let mut cats: BTreeSet<Kind> = Kind::ALL
        .iter()
        .copied()
        .filter(|k| *k != Kind::Memory)
        .collect();
    if !a.only.is_empty() {
        cats.clear();
        for o in a.only.iter().map(|s| s.trim()) {
            let k = Kind::ALL.iter().find(|k| k.name() == o).ok_or_else(|| {
                format!("--only: unknown category \"{}\" (instructions,permissions,hooks,env,agents,skills,commands,mcp,memory)", clean(o))
            })?;
            cats.insert(*k);
        }
    }
    let mode = if a.overwrite {
        Mode::Overwrite
    } else if a.skip_existing {
        Mode::SkipExisting
    } else {
        Mode::Merge
    };
    let (date, stamp) = stamps();
    let mut imp = Imp {
        mode,
        cats,
        broad_ok: a.accept_broad_allow,
        hooks_ok: a.accept_hooks,
        date,
        items: vec![],
        todo: vec![],
        json: BTreeMap::new(),
        files: BTreeMap::new(),
        manifests: BTreeMap::new(),
    };
    let claude_json = from
        .parent()
        .map(|p| p.join(".claude.json"))
        .unwrap_or_default();
    let cj = read_obj(&claude_json, MAX_CLAUDE_JSON)?;
    let synthesis_json = h.synthesis.join("synthesis.json");
    let user_t = Target {
        root: h.synthesis.clone(),
        user: true,
    };

    if a.scope != ScopeSel::Project {
        if imp.cats.contains(&Kind::Instructions) {
            imp.instructions(None, &from, &user_t)?;
        }
        imp.settings(
            &from.join("settings.json"),
            &h.synthesis.join("settings.json"),
            &user_t,
            false,
        )?;
        if imp.cats.contains(&Kind::Agents) {
            imp.agents(&from.join("agents"), &user_t)?;
        }
        if imp.cats.contains(&Kind::Skills) {
            imp.skills(&from.join("skills"), &user_t)?;
        }
        if imp.cats.contains(&Kind::Commands) {
            imp.commands(&from.join("commands"), &user_t)?;
        }
        if imp.cats.contains(&Kind::Mcp)
            && let Some(m) = cj.as_ref().and_then(|c| c.get("mcpServers"))
        {
            imp.mcp_servers(
                m,
                &claude_json.to_string_lossy(),
                &synthesis_json,
                &["mcpServers"],
                &h.synthesis,
            )?;
        }
    }
    let mut project_note = false;
    if let Some(p) = &project {
        let t = Target {
            root: p.join(".synthesis"),
            user: false,
        };
        let pc = p.join(".claude");
        if imp.cats.contains(&Kind::Instructions) {
            imp.instructions(Some(p), &pc, &t)?;
        }
        imp.settings(
            &pc.join("settings.json"),
            &t.root.join("settings.json"),
            &t,
            false,
        )?;
        imp.settings(
            &pc.join("settings.local.json"),
            &t.root.join("settings.local.json"),
            &t,
            true,
        )?;
        if imp.cats.contains(&Kind::Agents) {
            imp.agents(&pc.join("agents"), &t)?;
        }
        if imp.cats.contains(&Kind::Skills) {
            imp.skills(&pc.join("skills"), &t)?;
        }
        if imp.cats.contains(&Kind::Commands) {
            imp.commands(&pc.join("commands"), &t)?;
        }
        if imp.cats.contains(&Kind::Mcp) {
            let local = cj
                .as_ref()
                .and_then(|c| c.get("projects"))
                .and_then(Value::as_object)
                .and_then(|m| {
                    m.iter().find(|(k, _)| {
                        Path::new(k) == p.as_path()
                            || synthesis_core::canonicalize(k).is_ok_and(|c| c == *p)
                    })
                })
                .and_then(|(k, v)| v.get("mcpServers").map(|s| (k.clone(), s.clone())));
            if let Some((k, s)) = local {
                let src = format!("{} [projects]", claude_json.display());
                imp.mcp_servers(
                    &s,
                    &src,
                    &synthesis_json,
                    &["projects", &k, "mcpServers"],
                    &h.synthesis,
                )?;
            }
            imp.mcp_project_file(&p.join(".mcp.json"))?;
        }
        if imp.cats.contains(&Kind::Memory) {
            imp.memory(&from, p, &h.synthesis)?;
        }
    } else if a.scope == ScopeSel::All {
        project_note = true;
    }

    imp.finish_manifests(&from);
    let changes = imp.changes();

    out!(
        "synt import claude: {} from {}",
        if a.apply { "APPLY" } else { "DRY RUN" },
        clean(&from.to_string_lossy())
    );
    if project_note {
        out!("note: no --project-dir given, project scope skipped");
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut warnings = 0;
    for it in &imp.items {
        out!(
            "[{}] {} -> {} : {}",
            it.kind.name(),
            it.source,
            it.target,
            it.act.word()
        );
        for n in &it.notes {
            if n.starts_with("  ") || n.starts_with("HOOKS ") {
                out!("    {n}");
            } else {
                out!("    ! {n}");
                warnings += 1;
            }
        }
        *counts.entry(it.act.word()).or_default() += 1;
    }
    let c = |k: &str| counts.get(k).copied().unwrap_or(0);
    out!(
        "summary: {} imported ({} new, {} updated), {} unchanged, {} skipped, {} warning(s), {} file(s) {}",
        c("create") + c("update"),
        c("create"),
        c("update"),
        c("unchanged"),
        c("skip"),
        warnings,
        changes.len(),
        if a.apply {
            "written"
        } else {
            "would be written"
        }
    );
    if a.apply {
        for (path, bytes, existed) in &changes {
            if *existed {
                backup(path, &stamp)?;
            }
            write_atomic(path, bytes)?;
        }
    } else if !changes.is_empty() {
        out!(
            "dry run: nothing written; re-run with --apply (each changed file gets a *.bak-{stamp} backup)"
        );
    }
    if !imp.todo.is_empty() {
        out!("manual steps:");
        let mut seen = BTreeSet::new();
        for t in imp.todo.iter().filter(|t| seen.insert((*t).clone())) {
            out!("  - {}", clean(t));
        }
    }
    Ok(0)
}

/// `synt doctor`: what imports left behind (counts and paths only).
pub fn doctor_report(synthesis_home: &Path, project_root: &Path) {
    let mut any = false;
    for (label, root) in [
        ("user", synthesis_home.to_path_buf()),
        ("project", project_root.join(".synthesis")),
    ] {
        let Ok(Some(v)) = read_obj(&root.join("imports").join("claude.json"), MAX_TEXT) else {
            continue;
        };
        any = true;
        let mut by: BTreeMap<String, usize> = BTreeMap::new();
        for e in v
            .get("files")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
        {
            let n = e["count"].as_u64().map_or(1, |c| c as usize);
            *by.entry(e["kind"].as_str().unwrap_or("?").to_string())
                .or_default() += n;
        }
        let list: Vec<String> = by.iter().map(|(k, n)| format!("{k} {n}")).collect();
        out!(
            "import (claude, {label}): {} on {}: {}",
            root.display(),
            clean(v["imported_at"].as_str().unwrap_or("?")),
            list.join(", ")
        );
    }
    let proj = [
        "SYNTHESIS.md",
        "AGENTS.md",
        "CLAUDE.md",
        ".claude/CLAUDE.md",
    ]
    .iter()
    .map(|n| project_root.join(n))
    .find(|p| std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty()));
    out!(
        "instruction files: user {}, project {}{}",
        if synthesis_home.join("SYNTHESIS.md").is_file() {
            "SYNTHESIS.md"
        } else {
            "none"
        },
        proj.map_or("none".to_string(), |p| clean(&p.display().to_string())),
        if any { "" } else { " (no import recorded)" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_become_references() {
        let cfg = json!({
            "type": "http", "url": "https://h.example/mcp?api_key=abc123&x=1",
            "headers": {"Authorization": "Bearer abcdefghij1234567890abcd", "X-Ok": "1"},
            "env": {"GITHUB_TOKEN": "ghp_aaaaaaaaaaaaaaaaaaaa", "KEEP": "${KEEP}", "MODE": "fast"},
            "args": ["--token", "s3cr3tvalue", "--api-key=zzz", "pkg@1.2.3"]
        });
        let (r, vars, _) = redact_server("my-srv", &cfg);
        let s = r.to_string();
        for lit in [
            "abcdefghij1234567890abcd",
            "ghp_aaaa",
            "abc123",
            "s3cr3tvalue",
            "zzz",
        ] {
            assert!(!s.contains(lit), "{lit} leaked: {s}");
        }
        assert_eq!(r["headers"]["Authorization"], "Bearer ${MY_SRV_TOKEN}");
        assert_eq!(r["env"]["KEEP"], "${KEEP}");
        assert_eq!(r["env"]["MODE"], "fast");
        assert_eq!(r["headers"]["X-Ok"], "1");
        assert_eq!(r["args"][3], "pkg@1.2.3");
        assert!(vars.contains("MY_SRV_TOKEN") && vars.contains("MY_SRV_GITHUB_TOKEN"));
    }

    #[test]
    fn rules_and_tools() {
        assert!(bad_rule("Bash(npm test:*)").is_none());
        assert!(bad_rule("Read(//etc/**)").is_none());
        assert!(bad_rule("mcp__srv__tool").is_none());
        assert!(bad_rule("PowerShell(x)").is_some());
        assert!(bad_rule("Bash(x").is_some());
        assert!(is_broad("Bash") && is_broad("Bash(*)"));
        assert!(!is_broad("Bash(ls:*)") && !is_broad("Read"));
        let mut d = vec![];
        assert_eq!(
            translate_tools("Bash, PowerShell, MultiEdit, Edit, Agent", &mut d),
            "Bash, Edit, Task"
        );
        assert_eq!(d, ["PowerShell"]);
    }

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0));
        assert_eq!(civil(1_761_696_000).0, 2025);
    }
}
