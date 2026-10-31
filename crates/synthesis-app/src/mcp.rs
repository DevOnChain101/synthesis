//! MCP servers for a run: which configs to load, which may start, and the tool adapter.
//!
//! Sources (later wins, Node `buildMcpServers` order minus Claude Code's files, which Rust does
//! not read): user `mcpServers` of synthesis.json, project `.mcp.json` (gated), user `projects.<path>`
//! local scope, user `mcp`, project `.synthesis/config.json` `mcp`/`mcpServers` (gated).
//! SECURITY: a project-scope server is repository content that would run a command. It starts
//! only with `--trust-project` AND an explicit approval (`--approve-mcp NAME`, or the USER's
//! settings `enabledMcpjsonServers` / `enableAllProjectMcpServers`; a project settings file
//! cannot approve itself) and never when the user settings disable it. Everything else is
//! reported as skipped and never spawned. Servers are not OS-sandboxed.
//! APPROVAL BINDING: an approval is tied to a hash of the definition (name, transport, command,
//! args, url, env, headers; see [`definition_hash`]). `--approve-mcp NAME` approves the definition
//! as it is NOW for this run only and prints it with its hash. A persisted approval must be
//! `NAME@HASH` (CLI or user `enabledMcpjsonServers`); if `.mcp.json` changes, the hash no longer
//! matches and the server is not started. A bare `NAME` in settings and
//! `enableAllProjectMcpServers` no longer approve project servers (they would survive an edit).
//! `${VAR}` in env/url/headers reads `Env::child_env` plus the names in the USER's
//! `synthesis.mcp.env_passthrough` (user settings.json only), and the latter only for user-owned
//! scopes; see `synthesis_mcp::McpOptions::env_passthrough`.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synthesis_config::{Config, Env, is_secret_shaped};
use synthesis_core::ToolContext;
use synthesis_harness::sanitize_for_display as san;
use synthesis_harness::{
    GuardConfig, McpCatalog, McpGuard, McpToolInfo, ServerRules, ToolBox, ToolSource,
};
use synthesis_mcp::{
    McpContent, McpManager, McpOptions, McpServerConfig, McpToolResult, ServerState, expand_vars,
    expansion_env, parse_claude_servers, parse_synthesis_mcp_servers,
};
use synthesis_settings::{ProjectTrust, Settings};
use synthesis_tools::{
    Block, BoxFuture, Content, ImageSource, SideEffectClass, Tool, ToolOutput, ToolRuntime,
    ToolSpec,
};

#[derive(Default)]
pub struct Plan {
    pub servers: Vec<McpServerConfig>,
    /// `(server, reason)`: configured but not started. Raw text from repository files: sanitize
    /// before printing.
    pub skipped: Vec<(String, String)>,
    /// One line per project server approved for this run, with its exact definition. Raw too.
    pub notices: Vec<String>,
    /// `synthesis.mcp.env_passthrough` of the user settings file (see [`read_passthrough`]).
    pub env_passthrough: Vec<String>,
    /// `synthesis.mcp.servers.<name>.write_tools|read_tools` (see [`read_guard`]).
    pub guard: GuardConfig,
    /// Where oversized MCP results are saved in full.
    pub artifact_dir: Option<std::path::PathBuf>,
}

/// Hex prefix length shown and the shortest prefix accepted in `NAME@HASH`.
const HASH_SHOWN: usize = 24;
const HASH_MIN: usize = HASH_SHOWN;

/// SHA-256 over the whole definition (not the scope), so any edit changes it.
pub fn definition_hash(s: &McpServerConfig) -> String {
    let def = json!([
        s.name,
        s.transport.map(|t| t.as_str()),
        s.command,
        s.args,
        s.url,
        s.env,
        s.headers
    ]);
    Sha256::digest(def.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `scheme://host/path` with userinfo and query/fragment replaced by `…`.
fn safe_url(u: &str) -> String {
    let Some((scheme, rest)) = u.split_once("://") else {
        return "…".into();
    };
    let (auth, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = auth
        .rsplit_once('@')
        .map_or(auth.to_string(), |(_, h)| format!("…@{h}"));
    let (path, more) = tail.split_at(tail.find(['?', '#']).unwrap_or(tail.len()));
    format!(
        "{scheme}://{host}{path}{}",
        if more.is_empty() { "" } else { "…" }
    )
}

/// Args as shown for approval: `${VAR}` references stay; a value given to a secret-named flag
/// (`--api-key=v`, `--api-key v`) becomes `…`.
fn mask_args(args: &[String]) -> Vec<String> {
    let mut after_secret_flag = false;
    args.iter()
        .map(|a| {
            let masked = if a.contains("${") {
                a.clone()
            } else if after_secret_flag {
                "…".into()
            } else if let Some((k, _)) = a.split_once('=').filter(|(k, _)| secret_like(k)) {
                format!("{k}=…")
            } else {
                a.clone()
            };
            after_secret_flag = a.starts_with('-') && !a.contains('=') && secret_like(a);
            masked
        })
        .collect()
}

/// What approving this server means, exactly: command, args, url and the NAMES of env/headers.
pub fn describe(s: &McpServerConfig) -> String {
    let mut t = format!("{:?}", s.transport.map_or("?", |t| t.as_str()));
    if let Some(c) = &s.command {
        t += &format!(" command={c:?} args={:?}", mask_args(&s.args));
    }
    if let Some(u) = &s.url {
        t += &format!(" url={:?}", safe_url(u));
    }
    t += &format!(
        " env_keys={:?} headers={:?} hash={}",
        s.env.keys().collect::<Vec<_>>(),
        s.headers
            .iter()
            .map(|(k, v)| format!("{k}: {}…", v.chars().take(2).collect::<String>()))
            .collect::<Vec<_>>(),
        &definition_hash(s)[..HASH_SHOWN]
    );
    t
}

/// `${NAME}` / `${NAME:-default}` references in `v`: `(name, has_default)`.
fn refs(v: &str) -> Vec<(String, bool)> {
    let mut out = vec![];
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else { break };
        let (n, d) = after[..end]
            .split_once(":-")
            .map_or((&after[..end], false), |(n, _)| (n, true));
        out.push((n.to_string(), d));
        rest = &after[end + 1..];
    }
    out
}

/// Which credentials `synth run` will apply to `s`, WITHOUT any value: names, whether each is
/// resolvable right now (`${VAR}` against the same allowlist the manager uses), lengths only for
/// names that are not secret-shaped, the scheme of `Authorization`.
pub fn credentials(s: &McpServerConfig, env: &Env, passthrough: &[String]) -> Vec<String> {
    let xenv = expansion_env(env, passthrough, s.scope.as_deref());
    let user_owned = matches!(s.scope.as_deref(), Some("user" | "synthesis"));
    let state = |v: &str, show_len: bool| match expand_vars(v, &xenv) {
        Ok(x) if x.is_empty() => "EMPTY".to_string(),
        Ok(x) if show_len => format!("set({} chars)", x.chars().count()),
        Ok(_) => "set".to_string(),
        Err(n) => format!("NOT AVAILABLE ({n})"),
    };
    let mut lines = vec![];
    if !s.env.is_empty() {
        let v: Vec<String> = s
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", state(v, !is_secret_shaped(k))))
            .collect();
        lines.push(format!("env: {}", v.join(", ")));
    }
    if !s.headers.is_empty() {
        let v: Vec<String> = s
            .headers
            .iter()
            .map(|(k, v)| {
                let st = state(v, false);
                if !k.eq_ignore_ascii_case("authorization") || st != "set" {
                    return format!("{k}={st}");
                }
                let scheme = expand_vars(v, &xenv).ok().and_then(|x| {
                    let w = x.split_whitespace().next()?.to_string();
                    (x.split_whitespace().count() > 1 && w.chars().all(|c| c.is_ascii_alphabetic()))
                        .then_some(w)
                });
                format!("{k}={} ****(set)", scheme.unwrap_or_default()).replace("= ****", "=****")
            })
            .collect();
        lines.push(format!("headers: {}", v.join(", ")));
    }
    let mut seen = std::collections::BTreeSet::new();
    let all = s
        .env
        .values()
        .chain(s.headers.values())
        .chain(s.url.iter())
        .chain(s.args.iter())
        .chain(s.command.iter());
    for (n, has_default) in all.flat_map(|v| refs(v)) {
        if !seen.insert(n.clone()) {
            continue;
        }
        lines.push(if xenv.get(&n).is_some() {
            format!("env passthrough: {n} -> available")
        } else if has_default {
            format!("env passthrough: {n} -> unset, the default is used")
        } else if user_owned {
            format!("env passthrough: {n} -> NOT AVAILABLE (export it and add it to synthesis.mcp.env_passthrough in the user settings.json)")
        } else {
            format!(
                "env passthrough: {n} -> NOT AVAILABLE ({} servers can only read the child-env allowlist; move the server to the user synthesis.json)",
                s.scope.as_deref().unwrap_or("project")
            )
        });
    }
    if s.command.is_some() {
        let home = env.child_env();
        let v: Vec<String> = ["HOME", "USERPROFILE", "APPDATA"]
            .iter()
            .map(|k| {
                format!(
                    "{k}={}",
                    if home.get(k).is_some() {
                        "set"
                    } else {
                        "unset"
                    }
                )
            })
            .collect();
        lines.push(format!(
            "inherited profile dirs (mcp-remote keeps ~/.mcp-auth there): {}",
            v.join(", ")
        ));
    }
    lines
}

/// Keys under which servers report who did something.
const WHO_KEYS: [&str; 9] = [
    "author",
    "principal",
    "user",
    "username",
    "displayname",
    "createdby",
    "lastmodifiedby",
    "modifiedby",
    "by",
];

/// True when the result says the actor is `anonymous` (structured content, or JSON text).
pub fn reports_anonymous(r: &McpToolResult) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Object(o) => o.iter().any(|(k, v)| {
                (WHO_KEYS.contains(&k.to_ascii_lowercase().as_str())
                    && v.as_str()
                        .is_some_and(|s| s.trim().eq_ignore_ascii_case("anonymous")))
                    || walk(v)
            }),
            Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    r.structured.as_ref().is_some_and(walk)
        || serde_json::from_str::<Value>(&r.text()).is_ok_and(|v| walk(&v))
}

/// Masks token-looking words (long opaque strings) so `synth mcp test` never prints a credential.
fn mask_tokens(text: &str) -> String {
    let tokeny =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=' | '+' | '/');
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let opaque = word.len() >= 40
            || (word.len() >= 24
                && ["ATATT", "sk-", "ghp_", "xox", "eyJ"]
                    .iter()
                    .any(|p| word.starts_with(p)));
        out.push_str(if opaque { "****" } else { word });
        word.clear();
    };
    for c in text.chars() {
        if tokeny(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

enum Appr {
    No,
    Yes,
    Stale(&'static str),
}

/// `entry` is `NAME` (only when `bare_ok`: a per-run CLI flag) or `NAME@HASHPREFIX`.
fn check(entry: &str, name: &str, hash: &str, bare_ok: bool) -> Appr {
    match entry.split_once('@') {
        None if entry == name && bare_ok => Appr::Yes,
        None if entry == name => {
            Appr::Stale("a bare name in settings is not bound to a definition")
        }
        Some((n, h)) if n == name => {
            if h.len() >= HASH_MIN && hash.starts_with(&h.to_ascii_lowercase()) {
                Appr::Yes
            } else {
                Appr::Stale("definition does not match the approved hash (changed since approval?)")
            }
        }
        _ => Appr::No,
    }
}

/// `synthesis.mcp.env_passthrough` from the USER settings file only (a repository must not widen it).
pub fn read_passthrough(user_settings: &Path) -> Vec<String> {
    read_obj(user_settings)
        .and_then(|v| v.pointer("/synthesis/mcp/env_passthrough").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn read_obj(path: &Path) -> Option<Value> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() || m.len() > 1 << 20 {
        return None;
    }
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut s)
        .ok()?;
    serde_json::from_str(&s).ok().filter(Value::is_object)
}

fn put(all: &mut Vec<McpServerConfig>, add: Vec<McpServerConfig>) {
    for s in add {
        all.retain(|x| x.name != s.name);
        all.push(s);
    }
}

/// `synthesis.mcp.servers.<name>.{write_tools,read_tools}` for the read-before-write guard. The USER
/// file may list both; a project file (pass `None` when the project is not trusted) may only ADD
/// `write_tools`, since a repository must not be able to switch the guard off for a tool.
pub fn read_guard(user_settings: &Path, project_settings: Option<&Path>) -> GuardConfig {
    let mut cfg = GuardConfig::default();
    for (path, user) in [(Some(user_settings), true), (project_settings, false)] {
        let Some(servers) = path
            .and_then(read_obj)
            .and_then(|v| v.pointer("/synthesis/mcp/servers").cloned())
            .and_then(|v| v.as_object().cloned())
        else {
            continue;
        };
        for (name, rules) in servers {
            let list = |k: &str| -> Vec<String> {
                rules
                    .get(k)
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default()
            };
            let r: &mut ServerRules = cfg.servers.entry(name).or_default();
            r.write_tools.extend(list("write_tools"));
            if user {
                r.read_tools.extend(list("read_tools"));
            }
        }
    }
    cfg
}

/// `user_cfg` must be loaded with `ProjectTrust::Untrusted` (no project layer in it).
pub fn plan(
    user_cfg: &Config,
    settings: &Settings,
    user_settings: &Settings,
    cwd: &Path,
    trust: ProjectTrust,
    approve: &[String],
) -> Plan {
    let x = &user_cfg.extra;
    let mut all: Vec<McpServerConfig> = vec![];
    let mut project: Vec<McpServerConfig> = vec![];

    let mut skipped = vec![];
    let mut add = |dst: &mut Vec<McpServerConfig>, (ok, bad): synthesis_mcp::Parsed| {
        put(dst, ok);
        skipped.extend(bad.into_iter().map(|e| ("(invalid name)".to_string(), e)));
    };
    if let Some(m) = x.get("mcpServers") {
        add(&mut all, parse_claude_servers(m, Some("user")));
    }
    let root = &settings.project_root;
    if let Some(j) = read_obj(&root.join(".mcp.json")) {
        add(
            &mut project,
            parse_claude_servers(&j["mcpServers"], Some("project")),
        );
    }
    if let Some(local) = x
        .get("projects")
        .and_then(Value::as_object)
        .and_then(|p| {
            p.iter()
                .find(|(k, _)| Path::new(k) == cwd || Path::new(k) == root)
        })
        .and_then(|(_, e)| e.get("mcpServers"))
    {
        add(&mut all, parse_claude_servers(local, Some("local")));
    }
    if let Some(m) = x.get("mcp") {
        add(&mut all, parse_synthesis_mcp_servers(m, Some("synthesis")));
    }
    if let Some(c) = read_obj(&root.join(".synthesis").join("config.json")) {
        for k in ["mcp", "mcpServers"] {
            if let Some(m) = c.get(k) {
                add(
                    &mut project,
                    parse_synthesis_mcp_servers(m, Some("project")),
                );
            }
        }
    }

    let mut notices = vec![];
    let ua = &user_settings.mcp_approval;
    let disabled = &settings.mcp_approval.disabled_mcpjson_servers;
    let mut started = vec![];
    for s in project {
        let hash = definition_hash(&s);
        let appr = approve
            .iter()
            .map(|e| check(e, &s.name, &hash, true))
            .chain(
                ua.enabled_mcpjson_servers
                    .iter()
                    .map(|e| check(e, &s.name, &hash, false)),
            )
            .fold(Appr::No, |a, b| match (a, b) {
                (Appr::Yes, _) | (_, Appr::Yes) => Appr::Yes,
                (Appr::No, b) => b,
                (a, _) => a,
            });
        let why = if disabled.contains(&s.name) {
            Some("disabled in settings".to_string())
        } else if trust != ProjectTrust::Trusted {
            Some(format!(
                "project not trusted (needs --trust-project and approval); definition: {}",
                describe(&s)
            ))
        } else {
            match appr {
                Appr::Yes => None,
                Appr::No => Some(format!(
                    "not approved: review it, then pass --approve-mcp {}@{} (or list that in your user settings enabledMcpjsonServers); definition: {}",
                    s.name,
                    &hash[..HASH_SHOWN],
                    describe(&s)
                )),
                Appr::Stale(w) => Some(format!(
                    "approval not applied: {w}; to approve the current definition pass --approve-mcp {}@{}; definition: {}",
                    s.name,
                    &hash[..HASH_SHOWN],
                    describe(&s)
                )),
            }
        };
        match why {
            Some(w) => skipped.push((s.name, w)),
            None => {
                notices.push(format!(
                    "MCP server \"{}\" ({}) approved: {}",
                    s.name,
                    s.scope.as_deref().unwrap_or("?"),
                    describe(&s)
                ));
                started.push(s);
            }
        }
    }
    // Project entries sit between user and local scope in Node; a user-owned entry of the same
    // name must win over an approved project one, so project ones are merged first.
    let mut servers = vec![];
    put(&mut servers, started);
    put(&mut servers, all);
    Plan {
        servers,
        skipped,
        notices,
        env_passthrough: vec![],
        guard: GuardConfig::default(),
        artifact_dir: None,
    }
}

/// What `start` hands the harness: the live tool source and the read-before-write guard.
pub struct Attach {
    source: Arc<dyn ToolSource>,
    guard: Arc<McpGuard>,
}

impl Attach {
    pub fn apply(self, toolbox: ToolBox) -> ToolBox {
        toolbox.with_source(self.source).with_guard(self.guard)
    }
}

/// Adapters for the manager's current tools; rebuilt only when the manager's tool set changed
/// (`tools/list_changed`, reconnect, loss), so the model sees new and removed tools mid-session.
struct Live {
    mgr: Arc<McpManager>,
    cache: Mutex<(u64, Vec<Arc<dyn Tool>>)>,
}

impl ToolSource for Live {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let v = self.mgr.tools_version();
        let mut c = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if c.0 != v || c.1.is_empty() {
            c.1 = adapters(&self.mgr);
            c.0 = v;
        }
        c.1.clone()
    }
}

impl McpCatalog for Live {
    fn tools(&self) -> Vec<McpToolInfo> {
        self.mgr
            .tools()
            .into_iter()
            .map(|t| McpToolInfo {
                wire: t.name,
                server: t.server,
                tool: t.tool,
                description: t.description,
                input_schema: t.input_schema,
                read_only: t.annotations.read_only,
                destructive: t.annotations.destructive,
            })
            .collect()
    }
}

fn adapters(mgr: &Arc<McpManager>) -> Vec<Arc<dyn Tool>> {
    mgr.tools()
        .into_iter()
        .map(|t| -> Arc<dyn Tool> {
            Arc::new(McpTool {
                spec: ToolSpec {
                    name: t.name,
                    description: t.description,
                    input_schema: t.input_schema,
                    side_effect: SideEffectClass::External,
                    runtime: ToolRuntime::Mcp { server: t.server },
                },
                mgr: mgr.clone(),
            })
        })
        .collect()
}

/// Starts the servers and returns what the harness needs plus value-free warnings.
pub async fn start(plan: &Plan, env: &Env, cwd: &Path) -> (Arc<McpManager>, Attach, Vec<String>) {
    let mgr = Arc::new(McpManager::new(
        plan.servers.clone(),
        McpOptions {
            cwd: cwd.into(),
            env: env.clone(),
            env_passthrough: plan.env_passthrough.clone(),
            artifact_dir: plan.artifact_dir.clone(),
            ..McpOptions::default()
        },
    ));
    mgr.start().await;
    let warnings = mgr
        .status()
        .into_iter()
        .filter(|s| s.state == ServerState::Failed)
        .map(|s| {
            format!(
                "MCP server \"{}\" failed to start: {}",
                san(&s.name),
                san(&s.error.unwrap_or_default())
            )
        })
        .collect();
    let live = Arc::new(Live {
        mgr: mgr.clone(),
        cache: Mutex::new((0, vec![])),
    });
    let guard = Arc::new(McpGuard::new(plan.guard.clone(), live.clone()));
    let attach = Attach {
        source: live,
        guard,
    };
    (mgr, attach, warnings)
}

// ---- `synth mcp list|get|add|remove` ----------------------------------------------------------
// Edits only the user's own config files: `<SYNTHESIS_HOME>/synthesis.json` (scope user) or
// `<cwd>/.mcp.json` (scope project; still needs --approve-mcp to start). No credential is ever
// written: secret-looking env/header values must be `${VAR}` references, and OAuth tokens are not
// stored anywhere (use `npx mcp-remote <url>` as a stdio server, or a Bearer header from `${VAR}`).

#[derive(clap::Subcommand)]
pub enum McpCmd {
    /// Servers that `synth run` would see (nothing is started).
    List(crate::CwdArg),
    /// One server's definition (header and env VALUES are never shown).
    Get {
        name: String,
        #[command(flatten)]
        cwd: crate::CwdArg,
    },
    /// Connect to one server, list its tools and, if it has a read-only identity tool (`whoami`,
    /// `me`, `get_current_user`, `atlassianUserInfo`, `getAccessibleAtlassianResources`), call it
    /// and print who the server thinks you are. Run it before the first write.
    Test(TestArgs),
    /// Add a server: `synth mcp add NAME --url URL` or `synth mcp add NAME -- COMMAND ARGS...`.
    Add(AddArgs),
    Remove {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: ScopeArg,
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
    },
}

#[derive(clap::Args)]
pub struct TestArgs {
    name: String,
    #[command(flatten)]
    cwd: crate::CwdArg,
    /// Needed to test a server defined in the project's `.mcp.json`.
    #[arg(long)]
    trust_project: bool,
    /// `NAME@HASH` of a project-scope server (see `synth mcp get`).
    #[arg(long, value_name = "NAME")]
    approve_mcp: Vec<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ScopeArg {
    User,
    Project,
}

#[derive(clap::Args)]
pub struct AddArgs {
    name: String,
    #[arg(long, value_enum, default_value = "user")]
    scope: ScopeArg,
    #[arg(long)]
    url: Option<String>,
    /// `http` (streamable, default) or `sse` (legacy).
    #[arg(long, default_value = "http")]
    transport: String,
    /// `KEY=VALUE`; secret-looking keys need a `${VAR}` value.
    #[arg(long)]
    env: Vec<String>,
    /// `Name: value`; Authorization and similar need a `${VAR}` value.
    #[arg(long)]
    header: Vec<String>,
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,
    #[arg(last = true)]
    command: Vec<String>,
}

fn secret_like(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "auth",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|w| k.contains(w))
}

/// `${VAR}` references, optionally behind `Bearer` / `Basic`; no literal secret text.
fn only_refs(v: &str) -> bool {
    let mut rest = v.trim();
    let mut refs = 0;
    while let Some(i) = rest.find("${") {
        let Some(end) = rest[i..].find('}') else {
            return false;
        };
        let scheme = rest[..i].trim();
        if !scheme.is_empty()
            && !scheme.eq_ignore_ascii_case("bearer")
            && !scheme.eq_ignore_ascii_case("basic")
        {
            return false;
        }
        refs += 1;
        rest = rest[i + end + 1..].trim_start();
    }
    refs > 0 && rest.is_empty()
}

fn kv(s: &str, sep: char) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once(sep)
        .ok_or(format!("expected KEY{sep}VALUE, got {s:?}"))?;
    let (k, v) = (k.trim(), v.trim());
    if secret_like(k) && !only_refs(v) {
        return Err(format!(
            "{k}: looks like a credential; synthesis never writes credentials into config files. Put it in an environment variable and pass {k}{sep}${{VAR}}"
        ));
    }
    Ok((k.to_string(), v.to_string()))
}

/// The JSON entry for `a` (Claude shape).
fn entry(a: &AddArgs) -> Result<Value, String> {
    let mut e = serde_json::Map::new();
    match (&a.url, a.command.split_first()) {
        (Some(u), None) => {
            e.insert("type".into(), json!(a.transport));
            e.insert("url".into(), json!(u));
        }
        (None, Some((c, rest))) => {
            e.insert("command".into(), json!(c));
            e.insert("args".into(), json!(rest));
        }
        _ => return Err("give either --url URL or a command after `--`".into()),
    }
    if !matches!(a.transport.as_str(), "http" | "sse") {
        return Err("--transport must be http or sse".into());
    }
    let env: Result<serde_json::Map<String, Value>, String> = a
        .env
        .iter()
        .map(|s| kv(s, '=').map(|(k, v)| (k, json!(v))))
        .collect();
    let headers: Result<serde_json::Map<String, Value>, String> = a
        .header
        .iter()
        .map(|s| kv(s, ':').map(|(k, v)| (k, json!(v))))
        .collect();
    let (env, headers) = (env?, headers?);
    if !env.is_empty() {
        e.insert("env".into(), env.into());
    }
    if !headers.is_empty() {
        e.insert("headers".into(), headers.into());
    }
    Ok(e.into())
}

/// Sets (`Some`) or removes (`None`) `mcpServers.<name>` in the JSON object file, keeping every
/// other key. Returns whether anything changed.
fn edit_file(path: &Path, name: &str, value: Option<Value>) -> Result<bool, String> {
    let mut root = match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str::<Value>(&t)
            .ok()
            .filter(Value::is_object)
            .ok_or(format!(
                "{} is not a JSON object; fix it by hand first",
                path.display()
            ))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let servers = root
        .as_object_mut()
        .map(|o| o.entry("mcpServers").or_insert_with(|| json!({})))
        .and_then(Value::as_object_mut)
        .ok_or(format!("{}: mcpServers is not an object", path.display()))?;
    let changed = match value {
        Some(v) => {
            servers.insert(name.to_string(), v);
            true
        }
        None => servers.remove(name).is_some(),
    };
    if changed {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(&root).unwrap_or_default() + "\n",
        )
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(changed)
}

fn scope_file(scope: ScopeArg, synthesis_home: &Path, cwd: &Path) -> std::path::PathBuf {
    match scope {
        ScopeArg::User => synthesis_home.join("synthesis.json"),
        ScopeArg::Project => cwd.join(".mcp.json"),
    }
}

fn show(env: &Env, h: &crate::Homes, cwd: Option<&Path>, only: Option<&str>) -> Result<u8, String> {
    let pass = read_passthrough(&h.synthesis.join("settings.json"));
    use synthesis_settings::{LoadContext, load_settings};
    let cfg = crate::load_config(env, h, cwd, ProjectTrust::Untrusted)?;
    let none = h.synthesis.join("no-project");
    let cwd = cwd.unwrap_or(&none);
    let st = load_settings(
        &LoadContext {
            cwd,
            synthesis_home: &h.synthesis,
            user_home: &h.user,
            env,
        },
        ProjectTrust::Untrusted,
    );
    let plan = plan(&cfg, &st, &st, cwd, ProjectTrust::Untrusted, &[]);
    let mut found = false;
    for s in plan
        .servers
        .iter()
        .filter(|s| only.is_none_or(|n| n == s.name))
    {
        found = true;
        out!(
            "{} [{}] {}",
            san(&s.name),
            s.scope.as_deref().unwrap_or("?"),
            san(&describe(s))
        );
        if only.is_some() {
            for l in credentials(s, env, &pass) {
                out!("  {}", san(&l));
            }
        }
    }
    for (n, why) in plan
        .skipped
        .iter()
        .filter(|(n, _)| only.is_none_or(|o| o == n))
    {
        found = true;
        out!("{}: not started, {}", san(n), san(why));
    }
    if only.is_some() && !found {
        return Err("no such server".into());
    }
    Ok(0)
}

/// Names of tools that answer "who am I", tried in this order.
const WHOAMI: [&str; 5] = [
    "whoami",
    "me",
    "get_current_user",
    "atlassianUserInfo",
    "getAccessibleAtlassianResources",
];

fn test_server(env: &Env, h: &crate::Homes, a: &TestArgs) -> Result<u8, String> {
    use synthesis_settings::{LoadContext, load_settings};
    // the server process runs here, so it must exist: default to the current directory
    let cwd = a.cwd.cwd.as_deref().unwrap_or(Path::new("."));
    let trust = if a.trust_project {
        ProjectTrust::Trusted
    } else {
        ProjectTrust::Untrusted
    };
    let lc = LoadContext {
        cwd,
        synthesis_home: &h.synthesis,
        user_home: &h.user,
        env,
    };
    let cfg = crate::load_config(env, h, Some(cwd), ProjectTrust::Untrusted)?;
    let settings = load_settings(&lc, trust);
    let user_settings = load_settings(&lc, ProjectTrust::Untrusted);
    let mut p = plan(&cfg, &settings, &user_settings, cwd, trust, &a.approve_mcp);
    p.env_passthrough = read_passthrough(&h.synthesis.join("settings.json"));
    p.servers.retain(|s| s.name == a.name);
    let Some(s) = p.servers.first().cloned() else {
        let why = p
            .skipped
            .iter()
            .find(|(n, _)| *n == a.name)
            .map_or("no such server".to_string(), |(_, w)| {
                format!("not started: {w}")
            });
        return Err(san(&why));
    };
    out!("{} [{}]", san(&s.name), s.scope.as_deref().unwrap_or("?"));
    for l in credentials(&s, env, &p.env_passthrough) {
        out!("  {}", san(&l));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let (mgr, _attach, _) = start(&p, env, cwd).await;
        let st = mgr.status().into_iter().next().ok_or("no status")?;
        if st.state != ServerState::Connected {
            out!("  connect: FAILED, {}", san(&st.error.unwrap_or_default()));
            mgr.close().await;
            return Ok(1);
        }
        let tools = mgr.tools();
        out!(
            "  connected via {}; {} tool(s): {}",
            st.transport,
            tools.len(),
            san(&tools.iter().map(|t| t.tool.as_str()).take(30).collect::<Vec<_>>().join(", "))
        );
        let who = WHOAMI.iter().find_map(|w| {
            tools
                .iter()
                .find(|t| t.tool.eq_ignore_ascii_case(w) && !t.synthetic)
        });
        let mut code = 0;
        match who {
            None => out!("  identity: no whoami-style tool; cannot verify who writes will be attributed to"),
            Some(t)
                if t.annotations.read_only == Some(false)
                    || t.annotations.destructive == Some(true)
                    || t.input_schema["required"].as_array().is_some_and(|r| !r.is_empty()) =>
            {
                out!("  identity: {} needs arguments or is not read-only; not called", san(&t.tool));
            }
            Some(t) => {
                let r = mgr
                    .call_tool(&t.server, &t.tool, json!({}), &tokio_util::sync::CancellationToken::new())
                    .await
                    .map_err(|_| "cancelled".to_string())?;
                let text = san(&mask_tokens(&r.text()));
                let text: String = text.chars().take(800).collect();
                out!("  identity ({}): {}{text}", san(&t.tool), if r.is_error { "ERROR " } else { "" });
                if reports_anonymous(&r) {
                    out!("  WARNING: the server reports an anonymous principal; the credentials configured for it were not applied. Writes would be attributed to 'anonymous'.");
                    code = 1;
                }
            }
        }
        mgr.close().await;
        Ok::<u8, String>(code)
    })
}

pub fn run_cmd(env: &Env, cmd: McpCmd) -> Result<u8, String> {
    let h = crate::homes(env)?;
    // `.` is the process directory; project scope writes `./.mcp.json`
    let cwd_of = |c: &Option<std::path::PathBuf>| -> Result<std::path::PathBuf, String> {
        Ok(c.clone().unwrap_or_else(|| ".".into()))
    };
    match cmd {
        McpCmd::List(a) => show(env, &h, a.cwd.as_deref(), None),
        McpCmd::Get { name, cwd } => show(env, &h, cwd.cwd.as_deref(), Some(&name)),
        McpCmd::Test(a) => test_server(env, &h, &a),
        McpCmd::Add(a) => {
            let cwd = cwd_of(&a.cwd)?;
            if !synthesis_mcp::valid_server_name(&a.name) {
                return Err("invalid server name (allowed: 1-64 of A-Z a-z 0-9 _ . -)".into());
            }
            let f = scope_file(a.scope, &h.synthesis, &cwd);
            edit_file(&f, &a.name, Some(entry(&a)?))?;
            out!("added {} to {}", a.name, f.display());
            if matches!(a.scope, ScopeArg::Project) {
                out!(
                    "a project server starts only with --trust-project and --approve-mcp NAME@HASH (see `synth mcp get`)"
                );
            }
            Ok(0)
        }
        McpCmd::Remove { name, scope, cwd } => {
            let f = scope_file(scope, &h.synthesis, &cwd_of(&cwd)?);
            if edit_file(&f, &name, None)? {
                out!("removed {name} from {}", f.display());
                Ok(0)
            } else {
                Err(format!("no server {name:?} in {}", f.display()))
            }
        }
    }
}

/// One MCP tool. The permission engine sees the wire name `mcp__server__tool`; the harness
/// labels every tool result as untrusted data before the model reads it.
struct McpTool {
    spec: ToolSpec,
    mgr: Arc<McpManager>,
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, input: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let r = match self
                .mgr
                .call_wire(&self.spec.name, input, &ctx.cancel)
                .await
            {
                Ok(r) => r,
                Err(_) => return ToolOutput::soft_error("MCP call cancelled"),
            };
            // A write that comes back attributed to `anonymous` means the configured credentials
            // did not reach the server: say so next to the result.
            let server = match &self.spec.runtime {
                ToolRuntime::Mcp { server } => san(server),
                _ => "?".into(),
            };
            let warn = (!r.is_error && reports_anonymous(&r)).then(|| {
                format!(
                    "\n[synth warning: MCP server \"{server}\" reports an anonymous principal. Its configured credentials were probably not applied, so changes are attributed to 'anonymous'. Tell the user; they can check with `synth mcp test {server}`.]"
                )
            });
            let content = if r.content.iter().all(|c| matches!(c, McpContent::Text(_))) {
                Content::Text(r.text() + warn.as_deref().unwrap_or(""))
            } else {
                Content::Blocks(
                    r.content
                        .iter()
                        .map(|c| match c {
                            McpContent::Text(text) => Block::Text { text: text.clone() },
                            McpContent::Image { media_type, data } => Block::Image {
                                source: ImageSource::Base64 {
                                    media_type: media_type.clone(),
                                    data: data.clone(),
                                },
                            },
                        })
                        .chain(warn.map(|text| Block::Text { text }))
                        .collect(),
                )
            };
            let mut out = ToolOutput::text("");
            out.content = content;
            out.is_error = r.is_error;
            // structuredContent verbatim for hosts and hooks; the model reads the text form
            out.with_meta(json!({"mcp": true, "structuredContent": r.structured}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_config::Config;

    fn cfg(extra: Value) -> Config {
        Config {
            extra: extra.as_object().unwrap().clone(),
            ..Config::default()
        }
    }

    #[test]
    fn project_servers_need_trust_and_user_approval() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".mcp.json"),
            json!({"mcpServers":{"repo":{"command":"x"},"other":{"command":"y"}}}).to_string(),
        )
        .unwrap();
        let user = cfg(json!({"mcpServers":{"mine":{"command":"z"}}}));
        let s = Settings {
            project_root: d.path().to_path_buf(),
            ..Settings::default()
        };
        let names = |p: &Plan| p.servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>();

        let p = plan(
            &user,
            &s,
            &s,
            d.path(),
            ProjectTrust::Untrusted,
            &["repo".into()],
        );
        assert_eq!(names(&p), ["mine"]);
        assert_eq!(p.skipped.len(), 2);
        assert!(p.skipped[0].1.contains("not trusted"));

        let p = plan(&user, &s, &s, d.path(), ProjectTrust::Trusted, &[]);
        assert_eq!(names(&p), ["mine"], "trusted but not approved");
        assert!(p.skipped[0].1.contains("not approved"));

        let p = plan(
            &user,
            &s,
            &s,
            d.path(),
            ProjectTrust::Trusted,
            &["repo".into()],
        );
        assert_eq!(names(&p), ["repo", "mine"]);

        // a project settings file cannot approve itself: only the user-settings view counts
        let mut proj_only = s.clone();
        proj_only.mcp_approval.enable_all_project_mcp_servers = true;
        let p = plan(&user, &proj_only, &s, d.path(), ProjectTrust::Trusted, &[]);
        assert_eq!(names(&p), ["mine"]);
        // enableAll in the user settings no longer approves (it would survive a definition edit)
        let p = plan(&user, &s, &proj_only, d.path(), ProjectTrust::Trusted, &[]);
        assert_eq!(names(&p), ["mine"]);

        let mut off = s.clone();
        off.mcp_approval.disabled_mcpjson_servers = vec!["repo".into()];
        let p = plan(
            &user,
            &off,
            &s,
            d.path(),
            ProjectTrust::Trusted,
            &["repo".into()],
        );
        assert_eq!(names(&p), ["mine"]);
    }

    fn repo(mcp_json: Value) -> (tempfile::TempDir, Settings) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".mcp.json"), mcp_json.to_string()).unwrap();
        let s = Settings {
            project_root: d.path().to_path_buf(),
            ..Settings::default()
        };
        (d, s)
    }

    fn started(p: &Plan) -> Vec<&str> {
        p.servers.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn approval_is_bound_to_the_definition_hash() {
        let (d, s) =
            repo(json!({"mcpServers":{"repo":{"command":"x","args":["a"],"env":{"K":"v"}}}}));
        let user = cfg(json!({}));
        let run =
            |ua: &Settings, ap: &[String]| plan(&user, &s, ua, d.path(), ProjectTrust::Trusted, ap);
        let first = run(&s, &[]);
        let why = &first.skipped[0].1;
        let hash = definition_hash(&the_server(&d));
        // the skip reason shows the exact command, args and env KEYS (never values) and the hash
        assert!(
            why.contains("command=\"x\"") && why.contains("env_keys=[\"K\"]"),
            "{why}"
        );
        assert!(
            why.contains(&hash[..HASH_SHOWN]) && !why.contains("\"v\""),
            "{why}"
        );

        let good = format!("repo@{}", &hash[..HASH_SHOWN]);
        let p = run(&s, std::slice::from_ref(&good));
        assert_eq!(started(&p), ["repo"]);
        assert!(p.notices[0].contains("command=\"x\""), "{:?}", p.notices);
        // persisted in the USER settings: only NAME@HASH counts, a bare name does not
        let mut ua = s.clone();
        ua.mcp_approval.enabled_mcpjson_servers = vec![good];
        assert_eq!(started(&run(&ua, &[])), ["repo"]);
        ua.mcp_approval.enabled_mcpjson_servers = vec!["repo".into()];
        assert!(started(&run(&ua, &[])).is_empty());

        // .mcp.json edited after the approval: not started
        let mut ua = s.clone();
        ua.mcp_approval.enabled_mcpjson_servers = vec![format!("repo@{}", &hash[..HASH_SHOWN])];
        for edit in [
            json!({"command":"evil","args":["a"],"env":{"K":"v"}}),
            json!({"command":"x","args":["a","b"],"env":{"K":"v"}}),
            json!({"command":"x","args":["a"],"env":{"K":"w"}}),
            json!({"command":"x","args":["a"],"env":{"K":"v"},"headers":{"H":"1"}}),
        ] {
            std::fs::write(
                d.path().join(".mcp.json"),
                json!({"mcpServers":{"repo":edit}}).to_string(),
            )
            .unwrap();
            let p = run(&ua, &[]);
            assert!(started(&p).is_empty(), "{edit}");
            assert!(p.skipped[0].1.contains("does not match"), "{:?}", p.skipped);
            let p = run(&s, &[format!("repo@{}", &hash[..HASH_SHOWN])]);
            assert!(started(&p).is_empty(), "{edit}");
        }
        // a hash prefix shorter than the shown one never matches
        for n in [4, 16, HASH_SHOWN - 1] {
            assert!(
                started(&run(&s, &[format!("repo@{}", &hash[..n])])).is_empty(),
                "{n}"
            );
        }
    }

    #[test]
    fn credentials_report_names_and_state_never_values() {
        let mut c = McpServerConfig::stdio("a", "npx", vec!["${ARG_VAR}".into()]);
        c.scope = Some("user".into());
        c.env
            .insert("JIRA_USERNAME".into(), "ana@example.com".into());
        c.env.insert("JIRA_API_TOKEN".into(), "${ATL_TOKEN}".into());
        c.env
            .insert("CONFLUENCE_TOKEN".into(), "${CONFLUENCE_TOKEN}".into());
        c.headers
            .insert("Authorization".into(), "Basic ${ATL_TOKEN}".into());
        c.headers.insert("X-Api-Key".into(), "literal-key-9".into());
        let env = Env::from_pairs([("ATL_TOKEN", "SEKRIT-VALUE-1"), ("HOME", "/h")]);
        let text = credentials(&c, &env, &["ATL_TOKEN".into()]).join("\n");
        assert!(
            !text.contains("SEKRIT") && !text.contains("literal-key"),
            "{text}"
        );
        assert!(text.contains("JIRA_USERNAME=set(15 chars)"), "{text}");
        assert!(text.contains("JIRA_API_TOKEN=set,"), "{text}");
        assert!(
            text.contains("CONFLUENCE_TOKEN=NOT AVAILABLE (CONFLUENCE_TOKEN)"),
            "{text}"
        );
        assert!(text.contains("Authorization=Basic ****(set)"), "{text}");
        assert!(text.contains("X-Api-Key=set"), "{text}");
        assert!(
            text.contains("CONFLUENCE_TOKEN -> NOT AVAILABLE (export it and add it to synthesis.mcp.env_passthrough"),
            "{text}"
        );
        assert!(text.contains("ATL_TOKEN -> available"), "{text}");
        assert!(
            text.contains("HOME=set, USERPROFILE=unset, APPDATA=unset"),
            "{text}"
        );
        // project scope cannot use the passthrough list at all
        c.scope = Some("project".into());
        let text = credentials(&c, &env, &["ATL_TOKEN".into()]).join("\n");
        assert!(
            text.contains("ATL_TOKEN -> NOT AVAILABLE (project servers"),
            "{text}"
        );
    }

    #[test]
    fn a_user_entry_replaces_the_whole_project_entry_of_the_same_name() {
        let (d, s) = repo(json!({"mcpServers":{"atl":{"command":"proj","env":{
            "JIRA_USERNAME":"project-user","JIRA_API_TOKEN":"p"}}}}));
        let user =
            cfg(json!({"mcpServers":{"atl":{"command":"mine","env":{"JIRA_USERNAME":"ana"}}}}));
        let hash = definition_hash(&the_server(&d));
        let ap = [format!("atl@{}", &hash[..HASH_SHOWN])];
        let p = plan(&user, &s, &s, d.path(), ProjectTrust::Trusted, &ap);
        assert_eq!(p.servers.len(), 1);
        let e = &p.servers[0];
        // credentials come from the winning entry only, never merged with the project's
        assert_eq!(e.command.as_deref(), Some("mine"));
        assert_eq!(e.env.keys().collect::<Vec<_>>(), ["JIRA_USERNAME"]);
        assert_eq!(e.env["JIRA_USERNAME"], "ana");
    }

    #[test]
    fn anonymous_detection_and_token_masking() {
        let r = |v: Value| McpToolResult {
            content: vec![McpContent::Text(v.to_string())],
            is_error: false,
            structured: None,
        };
        assert!(reports_anonymous(&r(json!({"author": "anonymous"}))));
        assert!(reports_anonymous(&r(
            json!({"version": {"by": {"displayName": "Anonymous"}}})
        )));
        assert!(!reports_anonymous(&r(
            json!({"author": "ana@example.com", "body": "anonymous"})
        )));
        let t = format!("{{\"user\":\"ana\",\"t\":\"{}\"}}", "A1b2".repeat(12));
        let m = mask_tokens(&t);
        assert!(
            m.contains("\"user\":\"ana\"") && m.contains("\"t\":\"****\""),
            "{m}"
        );
        assert_eq!(
            mask_tokens("ana@example.com 557058:abc"),
            "ana@example.com 557058:abc"
        );
    }

    #[test]
    fn describe_masks_header_values() {
        let mut c = McpServerConfig::http("h", "http://example.test/");
        c.headers
            .insert("Authorization".into(), "Bearer SECRET".into());
        let d = describe(&c);
        assert!(
            d.contains("Authorization: Be…") && !d.contains("SECRET") && !d.contains("Bearer"),
            "{d}"
        );
    }

    #[test]
    fn describe_hides_url_secrets_and_secret_args() {
        let mut c =
            McpServerConfig::http("h", "https://user:pw@example.test/mcp/v1?token=SECRET#frag");
        let d = describe(&c);
        assert!(
            d.contains("https://…@example.test/mcp/v1…")
                && !d.contains("SECRET")
                && !d.contains("pw"),
            "{d}"
        );
        c.command = Some("x".into());
        c.args = [
            "--api-key",
            "AAA",
            "--token=BBB",
            "--url=${U}",
            "--port",
            "80",
        ]
        .map(String::from)
        .to_vec();
        let d = describe(&c);
        assert!(!d.contains("AAA") && !d.contains("BBB"), "{d}");
        assert!(
            d.contains("--token=…") && d.contains("${U}") && d.contains("\"80\""),
            "{d}"
        );
    }

    pub(super) fn the_server(d: &tempfile::TempDir) -> McpServerConfig {
        let j = read_obj(&d.path().join(".mcp.json")).unwrap();
        parse_claude_servers(&j["mcpServers"], Some("project"))
            .0
            .remove(0)
    }

    #[test]
    fn hostile_server_names_are_rejected_not_started() {
        let (d, s) = repo(
            json!({"mcpServers":{"a\u{1b}]52;;QQ==\u{7}":{"command":"x"},"ok":{"command":"y"}}}),
        );
        let user = cfg(json!({}));
        let p = plan(
            &user,
            &s,
            &s,
            d.path(),
            ProjectTrust::Trusted,
            &["ok".into(), "a\u{1b}]52;;QQ==\u{7}".into()],
        );
        assert_eq!(started(&p), ["ok"]);
        assert!(
            p.skipped
                .iter()
                .any(|(_, w)| w.contains("invalid MCP server name")),
            "{:?}",
            p.skipped
        );
        // what the CLI prints is free of escape sequences
        let line = format!("{}: {}", p.skipped[0].0, san(&p.skipped[0].1));
        assert!(!line.contains(['\x1b', '\x07']), "{line:?}");
    }

    #[test]
    fn passthrough_comes_from_user_settings_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(
            &f,
            json!({"synthesis":{"mcp":{"env_passthrough":["A","B"]}}}).to_string(),
        )
        .unwrap();
        assert_eq!(read_passthrough(&f), ["A", "B"]);
        assert!(read_passthrough(&d.path().join("none.json")).is_empty());
    }

    #[test]
    fn guard_rules_user_may_add_both_project_only_write_tools() {
        let d = tempfile::tempdir().unwrap();
        let (u, p) = (d.path().join("u.json"), d.path().join("p.json"));
        std::fs::write(
            &u,
            json!({"synthesis":{"mcp":{"servers":{"a":{"write_tools":["w1"],"read_tools":["r1"]}}}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            &p,
            json!({"synthesis":{"mcp":{"servers":{"a":{"write_tools":["w2"],"read_tools":["r2"]},"b":{"write_tools":["x"]}}}}}).to_string(),
        )
        .unwrap();
        let g = read_guard(&u, Some(&p));
        assert_eq!(g.servers["a"].write_tools, ["w1", "w2"]);
        assert_eq!(
            g.servers["a"].read_tools,
            ["r1"],
            "a repository cannot mark tools read-only"
        );
        assert_eq!(g.servers["b"].write_tools, ["x"]);
        assert!(
            !read_guard(&u, None).servers.contains_key("b"),
            "untrusted project ignored"
        );
    }

    fn add(name: &str, extra: impl FnOnce(&mut AddArgs)) -> AddArgs {
        let mut a = AddArgs {
            name: name.into(),
            scope: ScopeArg::User,
            url: None,
            transport: "http".into(),
            env: vec![],
            header: vec![],
            cwd: None,
            command: vec![],
        };
        extra(&mut a);
        a
    }

    #[test]
    fn add_and_remove_keep_other_keys_and_refuse_literal_credentials() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("synthesis.json");
        std::fs::write(
            &f,
            json!({"model": "m", "mcpServers": {"old": {"command": "x"}}}).to_string(),
        )
        .unwrap();
        let e = entry(&add("s", |a| {
            a.command = vec!["npx".into(), "mcp-remote".into(), "https://h/sse".into()];
            a.env = vec!["REGION=eu".into(), "API_TOKEN=${ATL_TOKEN}".into()];
        }))
        .unwrap();
        assert!(edit_file(&f, "s", Some(e)).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["model"], "m");
        assert_eq!(v["mcpServers"]["old"]["command"], "x");
        assert_eq!(v["mcpServers"]["s"]["args"][1], "https://h/sse");
        assert_eq!(v["mcpServers"]["s"]["env"]["API_TOKEN"], "${ATL_TOKEN}");
        assert!(edit_file(&f, "s", None).unwrap() && !edit_file(&f, "s", None).unwrap());

        let literal = entry(&add("s", |a| {
            a.url = Some("https://h/mcp".into());
            a.header = vec!["Authorization: Bearer abc".into()];
        }));
        assert!(literal.unwrap_err().contains("never writes credentials"));
        let ok = entry(&add("s", |a| {
            a.url = Some("https://h/mcp".into());
            a.header = vec!["Authorization: Bearer ${ATL_TOKEN}".into()];
        }));
        assert_eq!(
            ok.unwrap()["headers"]["Authorization"],
            "Bearer ${ATL_TOKEN}"
        );
        let basic = entry(&add("s", |a| {
            a.url = Some("https://h/mcp".into());
            a.header = vec!["Authorization: Basic ${ATL_BASIC}".into()];
        }));
        assert!(
            basic.is_ok(),
            "Atlassian API-token style Basic auth via ${{VAR}}"
        );
        assert!(entry(&add("s", |_| {})).is_err());
    }
}
