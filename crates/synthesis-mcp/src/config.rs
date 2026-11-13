//! MCP server config entries and env policy (port of claudecfg.js:560-607 + mcp.js:50-106).
//!
//! Only entry parsing lives here. Merging `.mcp.json` / `~/.claude.json` / `synt.json` sources and
//! the project-server approval lists (claudecfg.js `buildMcpServers`) is not ported.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::Value;
use synthesis_config::Env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
    Sse,
    Ws,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
            Self::Ws => "ws",
        }
    }
}

/// One configured server. `env` / `headers` hold secrets: they are only used to spawn/connect and
/// are redacted from `Debug`, status and error messages.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub scope: Option<String>,
    /// `None` means "infer": `url` set => http, else stdio.
    pub transport: Option<TransportKind>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    /// Per-server `tools/call` timeout (`tool_timeout_sec` | `tool_timeout_ms` | `timeout` in ms).
    pub timeout: Option<Duration>,
    /// Per-server connect timeout (`startup_timeout_sec` | `startup_timeout_ms`).
    pub startup_timeout: Option<Duration>,
    /// When set, only these server tool names are exposed.
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
}

impl McpServerConfig {
    /// Whether the model may see / call this server tool (`disabled_tools` wins).
    pub fn tool_enabled(&self, tool: &str) -> bool {
        !self.disabled_tools.iter().any(|t| t == tool)
            && self
                .enabled_tools
                .as_ref()
                .is_none_or(|e| e.iter().any(|t| t == tool))
    }

    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self::new(
            name,
            Some(TransportKind::Stdio),
            Some(command.into()),
            args,
            None,
        )
    }

    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::new(
            name,
            Some(TransportKind::Http),
            None,
            vec![],
            Some(url.into()),
        )
    }

    fn new(
        name: impl Into<String>,
        transport: Option<TransportKind>,
        command: Option<String>,
        args: Vec<String>,
        url: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            scope: None,
            transport,
            command,
            args,
            url,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            disabled: false,
            disabled_reason: None,
            timeout: None,
            startup_timeout: None,
            enabled_tools: None,
            disabled_tools: vec![],
        }
    }

    /// Transports to try in order (mcp.js:290): explicit, else url => http then sse, else stdio.
    pub fn transport_candidates(&self) -> Vec<TransportKind> {
        match (self.transport, &self.url) {
            (Some(t), _) => vec![t],
            (None, Some(_)) => vec![TransportKind::Http, TransportKind::Sse],
            (None, None) => vec![TransportKind::Stdio],
        }
    }
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("disabled", &self.disabled)
            .finish_non_exhaustive()
    }
}

fn js_string(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

fn record(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), js_string(v))).collect())
        .unwrap_or_default()
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(js_string).collect())
        .unwrap_or_default()
}

fn present<'a>(cfg: &'a Value, k: &str) -> Option<&'a Value> {
    cfg.get(k).filter(|v| !v.is_null())
}

fn non_empty_str<'a>(cfg: &'a Value, k: &str) -> Option<&'a str> {
    cfg.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Timeouts and tool filters shared by both shapes. Seconds or milliseconds, any of the Claude
/// Code / Codex spellings; non-positive or non-numeric values are ignored.
fn common(e: &mut McpServerConfig, cfg: &Value) {
    let dur = |keys: &[(&str, f64)]| {
        keys.iter().find_map(|(k, mul)| {
            let v = cfg
                .get(*k)?
                .as_f64()
                .filter(|v| *v > 0.0 && v.is_finite())?;
            Some(Duration::from_secs_f64(v * mul / 1000.0))
        })
    };
    e.timeout = dur(&[
        ("tool_timeout_sec", 1000.0),
        ("toolTimeoutSec", 1000.0),
        ("tool_timeout_ms", 1.0),
        ("timeout", 1.0),
    ]);
    e.startup_timeout = dur(&[
        ("startup_timeout_sec", 1000.0),
        ("startupTimeoutSec", 1000.0),
        ("startup_timeout_ms", 1.0),
    ]);
    let names = |k: &str| {
        cfg.get(k).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
    };
    e.enabled_tools = names("enabled_tools");
    e.disabled_tools = names("disabled_tools").unwrap_or_default();
}

/// Claude Code shape (`.mcp.json` `mcpServers.<name>`): `{type?, command, args, url, env, headers}`.
pub fn from_claude_shape(name: &str, cfg: &Value, scope: Option<&str>) -> McpServerConfig {
    let kind = match cfg.get("type").and_then(Value::as_str) {
        Some("http") => TransportKind::Http,
        Some("sse") => TransportKind::Sse,
        Some("ws" | "websocket") => TransportKind::Ws,
        _ if non_empty_str(cfg, "url").is_some() && non_empty_str(cfg, "command").is_none() => {
            TransportKind::Http
        }
        _ => TransportKind::Stdio,
    };
    let mut e = McpServerConfig::new(name, Some(kind), None, vec![], None);
    e.scope = scope.map(Into::into);
    if kind == TransportKind::Stdio {
        e.command = present(cfg, "command").map(js_string);
        e.args = strings(cfg.get("args"));
    } else {
        e.url = present(cfg, "url").map(js_string);
    }
    e.env = record(present(cfg, "env").or_else(|| cfg.get("environment")));
    e.headers = record(cfg.get("headers"));
    common(&mut e, cfg);
    e
}

/// OpenCode shape (`mcp.<name>`): `{type: "local"|"remote", command: [bin, ...args], url, enabled}`.
pub fn from_opencode_shape(name: &str, cfg: &Value, scope: Option<&str>) -> McpServerConfig {
    let local = cfg.get("type").and_then(Value::as_str) != Some("remote");
    let kind = if local {
        TransportKind::Stdio
    } else {
        TransportKind::Http
    };
    let mut e = McpServerConfig::new(name, Some(kind), None, vec![], None);
    e.scope = scope.map(Into::into);
    if local {
        match cfg.get("command").and_then(Value::as_array) {
            Some(arr) if !arr.is_empty() => {
                e.command = Some(js_string(&arr[0]));
                e.args = arr[1..].iter().map(js_string).collect();
            }
            Some(_) => {}
            None => {
                e.command = present(cfg, "command").map(js_string);
                e.args = strings(cfg.get("args"));
            }
        }
    } else {
        e.url = present(cfg, "url").map(js_string);
    }
    e.env = record(present(cfg, "env").or_else(|| cfg.get("environment")));
    e.headers = record(cfg.get("headers"));
    common(&mut e, cfg);
    if cfg.get("enabled") == Some(&Value::Bool(false)) {
        e.disabled = true;
        e.disabled_reason = Some("disabled".into());
    }
    e
}

/// Server names end up in tool names, prompts and the terminal: `^[A-Za-z0-9_.-]{1,64}$`, and never `__`: tool
/// names are `mcp__<server>__<tool>`, so with `__` inside a name `mcp__github__x__t` could be server `github`
/// (tool `x__t`) or server `github__x` (tool `t`), and a rule for one would cover the other.
pub fn valid_server_name(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && !n.contains("__")
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Servers parsed plus one error per rejected entry (the error embeds the raw name: sanitize
/// before showing it).
pub type Parsed = (Vec<McpServerConfig>, Vec<String>);

fn parse_map(map: &Value, mut one: impl FnMut(&str, &Value) -> McpServerConfig) -> Parsed {
    let (mut ok, mut bad) = (vec![], vec![]);
    for (n, c) in map
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, c)| c.is_object())
    {
        if valid_server_name(n) {
            ok.push(one(n, c));
        } else {
            bad.push(format!(
                "invalid MCP server name {n:?} (allowed: 1-64 of A-Z a-z 0-9 _ . -, no a double underscore)"
            ));
        }
    }
    (ok, bad)
}

/// Parses a `mcpServers` object (Claude shape). Non-object entries are skipped.
pub fn parse_claude_servers(map: &Value, scope: Option<&str>) -> Parsed {
    parse_map(map, |n, c| from_claude_shape(n, c, scope))
}

/// Parses a synthesis `mcp` object: OpenCode shape when `type` is local/remote, else Claude shape.
pub fn parse_synthesis_mcp_servers(map: &Value, scope: Option<&str>) -> Parsed {
    parse_map(map, |n, c| match c.get("type").and_then(Value::as_str) {
        Some("local" | "remote") => from_opencode_shape(n, c, scope),
        _ => from_claude_shape(n, c, scope),
    })
}

/// `${VAR}` / `${VAR:-default}` against `env` (the caller passes an allowlisted view, never the
/// whole process env). Unset => the default, else `Err(name)`: never a silent empty string.
pub fn expand_vars(value: &str, env: &Env) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let parsed = after.find('}').and_then(|end| {
            let body = &after[..end];
            let (name, def) = match body.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (body, None),
            };
            let mut cs = name.chars();
            let ok = cs
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
            ok.then_some((name, def, end))
        });
        match parsed {
            Some((name, def, end)) => {
                out.push_str(env.get(name).or(def).ok_or_else(|| name.to_string())?);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Base env for a stdio child: `Env::child_env` (allowlist). Node passes the whole process env
/// minus secrets; this is deliberately narrower.
pub fn stdio_base_env(env: &Env) -> BTreeMap<String, String> {
    env.child_env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The only variables `${VAR}` in a server definition of `scope` may read: `child_env`, plus the
/// user's `passthrough` names for user-owned scopes ("user", "synthesis"). Hostile repository config can
/// never reach other secrets. Empty values count as unset, so `Bearer ${TOKEN}` can never silently
/// become `Bearer `.
pub fn expansion_env(env: &Env, passthrough: &[String], scope: Option<&str>) -> Env {
    let base = env.child_env();
    let nonempty = |(_, v): &(&str, &str)| !v.is_empty();
    if !matches!(scope, Some("user" | "synthesis")) {
        // proxy URLs may carry credentials and SSL_CERT_* paths are local: a repo-chosen
        // url/header must not be able to read them (children still inherit them as env)
        return Env::from_pairs(
            base.iter()
                .filter(|(k, _)| {
                    !k.to_ascii_lowercase().contains("proxy") && !k.starts_with("SSL_CERT")
                })
                .filter(nonempty),
        );
    }
    let extra = passthrough.iter().filter_map(|n| {
        // Windows variable names are case-insensitive: `Jira_Token` in the list finds `JIRA_TOKEN`
        let found = if cfg!(windows) {
            env.iter().find(|(k, _)| k.eq_ignore_ascii_case(n))
        } else {
            Some((n.as_str(), env.get(n)?))
        };
        found.map(|(_, v)| (n.as_str(), v))
    });
    Env::from_pairs(base.iter().chain(extra).filter(nonempty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_shape_infers_type() {
        let c = from_claude_shape(
            "a",
            &json!({"command":"node","args":["x",1],"env":{"K":"v"}}),
            Some("project"),
        );
        assert_eq!(
            (c.transport, c.command.as_deref()),
            (Some(TransportKind::Stdio), Some("node"))
        );
        assert_eq!(c.args, ["x", "1"]);
        assert_eq!(c.env["K"], "v");
        let c = from_claude_shape(
            "a",
            &json!({"url":"http://h/mcp","headers":{"X":"y"}}),
            None,
        );
        assert_eq!(
            (c.transport, c.url.as_deref()),
            (Some(TransportKind::Http), Some("http://h/mcp"))
        );
        let c = from_claude_shape("a", &json!({"type":"sse","url":"http://h/sse"}), None);
        assert_eq!(c.transport, Some(TransportKind::Sse));
        // url + command and no type: stdio
        let c = from_claude_shape("a", &json!({"url":"http://h","command":"c"}), None);
        assert_eq!(c.transport, Some(TransportKind::Stdio));
    }

    #[test]
    fn opencode_shape() {
        let c = from_opencode_shape(
            "a",
            &json!({"type":"local","command":["bin","-x"],"enabled":false}),
            None,
        );
        assert_eq!(
            (c.command.as_deref(), c.args.as_slice()),
            (Some("bin"), &["-x".to_string()][..])
        );
        assert!(c.disabled);
        assert_eq!(c.disabled_reason.as_deref(), Some("disabled"));
        let c = from_opencode_shape("a", &json!({"type":"remote","url":"http://r"}), None);
        assert_eq!(c.transport, Some(TransportKind::Http));
        let (all, bad) = parse_synthesis_mcp_servers(
            &json!({"l":{"type":"local","command":["x"]},"c":{"command":"y"},"bad":3}),
            None,
        );
        assert_eq!((all.len(), bad.len()), (2, 0));
    }

    #[test]
    fn server_names_are_validated_at_parse_time() {
        let long = "a".repeat(65);
        let m = json!({"ok.name-1_":{"command":"x"},"\u{1b}]52;;AAAA\u{7}":{"command":"x"},
            "sp ace":{"command":"x"},"":{"command":"x"}, long:{"command":"x"}});
        let (ok, bad) = parse_claude_servers(&m, None);
        assert_eq!(ok.len(), 1);
        assert_eq!(bad.len(), 4);
        let (ok, bad) = parse_synthesis_mcp_servers(&m, None);
        assert_eq!((ok.len(), bad.len()), (1, 4));
    }

    #[test]
    fn transport_candidates() {
        let mut c = McpServerConfig::stdio("a", "x", vec![]);
        assert_eq!(c.transport_candidates(), [TransportKind::Stdio]);
        c.transport = None;
        c.url = Some("http://x".into());
        assert_eq!(
            c.transport_candidates(),
            [TransportKind::Http, TransportKind::Sse]
        );
    }

    #[test]
    fn expansion() {
        let env = Env::from_pairs([("A", "1"), ("EMPTY", "")]);
        let x = |v: &str| expand_vars(v, &env);
        assert_eq!(x("x${A}y").unwrap(), "x1y");
        assert_eq!(x("${NOPE:-dflt}").unwrap(), "dflt");
        assert_eq!(x("a${NOPE}").unwrap_err(), "NOPE");
        assert_eq!(x("${EMPTY:-d}").unwrap(), "");
        assert_eq!(x("${1bad} $A ${A").unwrap(), "${1bad} $A ${A");
    }

    #[test]
    fn base_env_allowlist_strips_secrets_and_unknowns() {
        let env = Env::from_pairs([
            ("PATH", "/bin"),
            ("LC_ALL", "C"),
            ("HTTPS_PROXY", "p"),
            ("GITHUB_TOKEN", "nope"),
            ("SYNT_PASSWORD", "nope"),
            ("SYNT_HOME", "/h"),
            ("SYNT_API_KEY", "secret"),
            ("SYNT_OPENAI_AUTH_TOKEN", "secret"),
            ("ANTHROPIC_API_KEY", "nope"),
            ("RANDOM", "nope"),
        ]);
        let b = stdio_base_env(&env);
        assert_eq!(
            b.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "HTTPS_PROXY",
                "LC_ALL",
                "PATH",
                "SYNT_AGENT_SESSION",
                "SYNT_HOME"
            ]
        );
    }

    #[test]
    fn debug_hides_secret_values() {
        let mut c = McpServerConfig::stdio("a", "x", vec![]);
        c.env.insert("K".into(), "sekrit".into());
        c.headers
            .insert("Authorization".into(), "Bearer sekrit".into());
        assert!(!format!("{c:?}").contains("sekrit"));
    }
}

#[cfg(test)]
mod name_tests {
    use super::valid_server_name;

    #[test]
    fn a_server_name_never_contains_a_double_underscore() {
        for ok in ["github", "my-server", "a.b", "a_b", "A1"] {
            assert!(valid_server_name(ok), "{ok}");
        }
        for bad in ["github__x", "__", "a__", "__a", "", "a b", "../x"] {
            assert!(!valid_server_name(bad), "{bad}");
        }
    }
}
