//! Closed `config.json/v1` validator. Port of Node `src/native/configschema.js`;
//! messages are byte-for-byte the Node text (golden-tested).

use serde_json::Value;
use std::fmt::Write as _;

pub const SCHEMA_V1: &str = "config.json/v1";
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 12;
const MAX_COLLECTION_ITEMS: usize = 4096;
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const TOP_LEVEL: [&str; 14] = [
    "$schema",
    "provider",
    "providers",
    "model",
    "smallModel",
    "aliases",
    "maxTokens",
    "profiles",
    "mcp",
    "mcpServers",
    "projects",
    "claude",
    "fallback",
    "fallbackReprobeMinutes",
];
const PROVIDER_FIELDS: [&str; 12] = [
    "npm",
    "name",
    "type",
    "baseURL",
    "apiKey",
    "authToken",
    "headers",
    "authHeader",
    "models",
    "discover",
    "noAuth",
    "options",
];

/// Escapes control characters and caps at 500 UTF-16 units (Node `sanitizeDiagnostic`).
pub fn sanitize_diagnostic(value: &str) -> String {
    let mut text = String::new();
    for c in value.chars() {
        if matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}') {
            let _ = write!(text, "\\x{:02x}", c as u32);
        } else {
            text.push(c);
        }
    }
    if text.encode_utf16().count() <= 500 {
        return text;
    }
    let (mut out, mut units) = (String::new(), 0);
    for c in text.chars() {
        units += c.len_utf16();
        if units > 500 {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// Node `ConfigValidationError` (code `SYNT_INVALID_CONFIG`). Never carries file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: String,
    pub problems: Vec<String>,
}

impl ConfigError {
    pub const CODE: &'static str = "SYNT_INVALID_CONFIG";

    pub fn new(file: &str, problems: impl IntoIterator<Item = String>) -> Self {
        Self {
            file: sanitize_diagnostic(file),
            problems: problems
                .into_iter()
                .take(MAX_DIAGNOSTICS)
                .map(|p| sanitize_diagnostic(&p))
                .collect(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid config {}: {}",
            self.file,
            self.problems.join("; ")
        )
    }
}

impl std::error::Error for ConfigError {}

type P = Vec<String>;

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
    }
}

fn finite(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Validates a parsed config file. `file` is only used in the error text.
pub fn validate_versioned_config(raw: &Value, file: &str) -> Result<(), ConfigError> {
    let err = |m: String| ConfigError::new(file, [m]);
    if !raw.is_object() {
        return Err(err(format!("$ must be an object, got {}", type_name(raw))));
    }
    match raw.get("$schema") {
        None => {
            return Err(err(format!(
                "missing required $schema (use \"{SCHEMA_V1}\")"
            )));
        }
        Some(Value::String(s)) if s == SCHEMA_V1 => {}
        Some(Value::String(s)) => {
            return Err(err(format!(
                "unsupported $schema \"{s}\" (supported: {SCHEMA_V1})"
            )));
        }
        Some(_) => return Err(err("$schema must be a string".into())),
    }
    let mut p = P::new();
    validate_v1(raw, &mut p);
    if p.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::new(file, p))
    }
}

fn allowed_keys(v: &Value, allowed: &[&str], at: &str, p: &mut P) -> bool {
    let Some(m) = v.as_object() else {
        p.push(format!("{at} must be an object, got {}", type_name(v)));
        return false;
    };
    if m.len() > MAX_COLLECTION_ITEMS {
        p.push(format!(
            "{at} has too many entries (maximum {MAX_COLLECTION_ITEMS})"
        ));
        return false;
    }
    for key in m.keys() {
        if !allowed.contains(&key.as_str()) {
            p.push(format!("{at}.{key} is not allowed by {SCHEMA_V1}"));
        }
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
    true
}

fn string(v: Option<&Value>, at: &str, p: &mut P) {
    if !matches!(v, Some(Value::String(s)) if !s.is_empty()) {
        p.push(format!("{at} must be a non-empty string"));
    }
}

fn boolean(v: Option<&Value>, at: &str, p: &mut P) {
    if !matches!(v, Some(Value::Bool(_))) {
        p.push(format!("{at} must be a boolean"));
    }
}

/// Returns the entries when `v` is an object within the size limit, else records a problem.
fn entries<'a>(v: &'a Value, at: &str, p: &mut P) -> Option<&'a serde_json::Map<String, Value>> {
    let Some(m) = v.as_object() else {
        p.push(format!("{at} must be an object"));
        return None;
    };
    if m.len() > MAX_COLLECTION_ITEMS {
        p.push(format!(
            "{at} has too many entries (maximum {MAX_COLLECTION_ITEMS})"
        ));
        return None;
    }
    Some(m)
}

fn string_record(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (k, item) in m {
        if !item.is_string() {
            p.push(format!("{at}.{k} must be a string"));
        }
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn non_empty_string_record(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (k, item) in m {
        string(Some(item), &format!("{at}.{k}"), p);
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn string_array(v: &Value, at: &str, p: &mut P) {
    let Some(a) = v.as_array() else {
        p.push(format!("{at} must be an array"));
        return;
    };
    if a.len() > MAX_COLLECTION_ITEMS {
        p.push(format!(
            "{at} has too many items (maximum {MAX_COLLECTION_ITEMS})"
        ));
        return;
    }
    for (i, item) in a.iter().enumerate() {
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
        string(Some(item), &format!("{at}[{i}]"), p);
    }
}

fn validate_capabilities(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (name, setting) in m {
        if name == "effort_tiers" {
            let levels: Option<Vec<&str>> = setting.as_array().and_then(|a| {
                a.iter()
                    .map(|l| l.as_str().filter(|s| EFFORT_LEVELS.contains(s)))
                    .collect()
            });
            match levels {
                None => p.push(format!(
                    "{at}.effort_tiers must contain only low, medium, high, xhigh, or max"
                )),
                Some(l) => {
                    let mut seen = l.clone();
                    seen.sort_unstable();
                    seen.dedup();
                    if seen.len() != l.len() {
                        p.push(format!("{at}.effort_tiers must not contain duplicates"));
                    }
                }
            }
        } else if !setting.is_boolean() {
            p.push(format!("{at}.{name} must be a boolean"));
        }
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn validate_model(v: &Value, at: &str, p: &mut P) {
    const FIELDS: [&str; 10] = [
        "name",
        "limit",
        "vendor",
        "family",
        "quality",
        "qualityScore",
        "qualityRank",
        "reasoning",
        "thinking",
        "capabilities",
    ];
    if !allowed_keys(v, &FIELDS, at, p) {
        return;
    }
    for key in ["name", "vendor", "family"] {
        if let Some(x) = v.get(key) {
            string(Some(x), &format!("{at}.{key}"), p);
        }
    }
    for key in ["qualityScore", "qualityRank"] {
        if let Some(x) = v.get(key)
            && finite(Some(x)).is_none()
        {
            p.push(format!("{at}.{key} must be a number"));
        }
    }
    for key in ["quality", "reasoning", "thinking"] {
        if let Some(x) = v.get(key)
            && !(x.is_string() || x.is_boolean() || x.is_array())
        {
            p.push(format!("{at}.{key} must be a string, boolean, or array"));
        }
    }
    if let Some(limit) = v.get("limit") {
        let lat = format!("{at}.limit");
        if allowed_keys(limit, &["context", "output"], &lat, p) {
            for key in ["context", "output"] {
                if let Some(x) = limit.get(key)
                    && !finite(Some(x)).is_some_and(|f| f > 0.0)
                {
                    p.push(format!("{at}.limit.{key} must be a positive number"));
                }
            }
        }
    }
    if let Some(c) = v.get("capabilities") {
        validate_capabilities(c, &format!("{at}.capabilities"), p);
    }
}

fn validate_models(v: &Value, at: &str, p: &mut P) {
    if v.is_array() {
        return string_array(v, at, p);
    }
    let Some(m) = v.as_object() else {
        p.push(format!("{at} must be an array or object"));
        return;
    };
    if m.len() > MAX_COLLECTION_ITEMS {
        p.push(format!(
            "{at} has too many entries (maximum {MAX_COLLECTION_ITEMS})"
        ));
        return;
    }
    for (id, model) in m {
        if id.is_empty() {
            p.push(format!("{at} contains an empty model id"));
        }
        validate_model(model, &format!("{at}.{id}"), p);
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn api_key_ok(v: &Value) -> bool {
    matches!(v, Value::Bool(false)) || matches!(v, Value::String(s) if !s.is_empty())
}

fn one_of(v: &Value, set: &[&str]) -> bool {
    v.as_str().is_some_and(|s| set.contains(&s))
}

fn validate_provider(v: &Value, at: &str, p: &mut P) {
    if !allowed_keys(v, &PROVIDER_FIELDS, at, p) {
        return;
    }
    for key in ["npm", "name", "baseURL", "authToken"] {
        if let Some(x) = v.get(key) {
            string(Some(x), &format!("{at}.{key}"), p);
        }
    }
    if v.get("type")
        .is_some_and(|t| !one_of(t, &["anthropic", "openai"]))
    {
        p.push(format!("{at}.type must be anthropic or openai"));
    }
    if v.get("apiKey").is_some_and(|k| !api_key_ok(k)) {
        p.push(format!("{at}.apiKey must be a non-empty string or false"));
    }
    if v.get("authHeader")
        .is_some_and(|h| !one_of(h, &["bearer", "x-api-key"]))
    {
        p.push(format!("{at}.authHeader must be bearer or x-api-key"));
    }
    if let Some(h) = v.get("headers") {
        string_record(h, &format!("{at}.headers"), p);
    }
    if let Some(d) = v.get("discover") {
        boolean(Some(d), &format!("{at}.discover"), p);
    }
    if let Some(d) = v.get("noAuth") {
        boolean(Some(d), &format!("{at}.noAuth"), p);
    }
    if let Some(m) = v.get("models") {
        validate_models(m, &format!("{at}.models"), p);
    }
    if let Some(o) = v.get("options") {
        let oat = format!("{at}.options");
        let fields = ["baseURL", "apiKey", "authToken", "headers", "authHeader"];
        if allowed_keys(o, &fields, &oat, p) {
            for key in ["baseURL", "authToken"] {
                if let Some(x) = o.get(key) {
                    string(Some(x), &format!("{oat}.{key}"), p);
                }
            }
            if o.get("apiKey").is_some_and(|k| !api_key_ok(k)) {
                p.push(format!("{oat}.apiKey must be a non-empty string or false"));
            }
            if let Some(h) = o.get("headers") {
                string_record(h, &format!("{oat}.headers"), p);
            }
            if o.get("authHeader")
                .is_some_and(|h| !one_of(h, &["bearer", "x-api-key"]))
            {
                p.push(format!("{oat}.authHeader must be bearer or x-api-key"));
            }
        }
    }
}

fn validate_providers(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (id, provider) in m {
        if id.is_empty() {
            p.push(format!("{at} contains an empty provider id"));
        }
        validate_provider(provider, &format!("{at}.{id}"), p);
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn validate_profiles(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (name, profile) in m {
        let pat = format!("{at}.{name}");
        // Node quirk kept: a non-object, non-array profile is skipped without a diagnostic.
        let candidates = if profile.is_array() {
            Some(profile)
        } else if profile.is_object() {
            if !allowed_keys(profile, &["candidates"], &pat, p) {
                continue;
            }
            profile.get("candidates")
        } else {
            continue;
        };
        let Some(list) = candidates.and_then(Value::as_array) else {
            p.push(format!("{pat}.candidates must be an array"));
            continue;
        };
        if list.len() > MAX_COLLECTION_ITEMS {
            p.push(format!(
                "{pat}.candidates has too many items (maximum {MAX_COLLECTION_ITEMS})"
            ));
            continue;
        }
        for (i, c) in list.iter().enumerate() {
            if p.len() >= MAX_DIAGNOSTICS {
                break;
            }
            let cat = format!("{pat}.candidates[{i}]");
            if c.is_string() {
                string(Some(c), &cat, p);
            } else if allowed_keys(c, &["model"], &cat, p) {
                string(c.get("model"), &format!("{cat}.model"), p);
            }
        }
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn validate_mcp_server(v: &Value, at: &str, p: &mut P) {
    let fields = [
        "type",
        "command",
        "args",
        "url",
        "enabled",
        "env",
        "environment",
        "headers",
    ];
    if !allowed_keys(v, &fields, at, p) {
        return;
    }
    if v.get("type")
        .is_some_and(|t| !one_of(t, &["local", "remote", "stdio", "http", "sse"]))
    {
        p.push(format!("{at}.type is invalid"));
    }
    if let Some(c) = v.get("command") {
        if !c.is_string() && !c.is_array() {
            p.push(format!("{at}.command must be a string or array"));
        }
        if c.is_array() {
            string_array(c, &format!("{at}.command"), p);
        }
    }
    if let Some(a) = v.get("args") {
        string_array(a, &format!("{at}.args"), p);
    }
    if let Some(u) = v.get("url") {
        string(Some(u), &format!("{at}.url"), p);
    }
    if let Some(e) = v.get("enabled") {
        boolean(Some(e), &format!("{at}.enabled"), p);
    }
    for key in ["env", "environment", "headers"] {
        if let Some(x) = v.get(key) {
            string_record(x, &format!("{at}.{key}"), p);
        }
    }
}

fn validate_mcp_map(v: &Value, at: &str, p: &mut P) {
    let Some(m) = entries(v, at, p) else { return };
    for (name, server) in m {
        validate_mcp_server(server, &format!("{at}.{name}"), p);
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn validate_projects(v: &Value, at: &str, p: &mut P) {
    const FIELDS: [&str; 5] = [
        "mcpServers",
        "enabledMcpjsonServers",
        "disabledMcpjsonServers",
        "enableAllProjectMcpServers",
        "disabledMcpServers",
    ];
    let Some(m) = entries(v, at, p) else { return };
    for (project, entry) in m {
        let pat = format!("{at}.{project}");
        if !allowed_keys(entry, &FIELDS, &pat, p) {
            continue;
        }
        if let Some(s) = entry.get("mcpServers") {
            validate_mcp_map(s, &format!("{pat}.mcpServers"), p);
        }
        for key in [
            "enabledMcpjsonServers",
            "disabledMcpjsonServers",
            "disabledMcpServers",
        ] {
            if let Some(x) = entry.get(key) {
                string_array(x, &format!("{pat}.{key}"), p);
            }
        }
        if let Some(x) = entry.get("enableAllProjectMcpServers") {
            boolean(Some(x), &format!("{pat}.enableAllProjectMcpServers"), p);
        }
        if p.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

fn validate_v1(raw: &Value, p: &mut P) {
    allowed_keys(raw, &TOP_LEVEL, "$", p);
    for key in ["model", "smallModel"] {
        if let Some(x) = raw.get(key) {
            string(Some(x), &format!("$.{key}"), p);
        }
    }
    if let Some(x) = raw.get("maxTokens")
        && !finite(Some(x)).is_some_and(|f| f > 0.0)
    {
        p.push("$.maxTokens must be a positive number".into());
    }
    if let Some(a) = raw.get("aliases") {
        non_empty_string_record(a, "$.aliases", p);
    }
    for key in ["provider", "providers"] {
        if let Some(x) = raw.get(key) {
            validate_providers(x, &format!("$.{key}"), p);
        }
    }
    if let Some(x) = raw.get("fallback") {
        match x.as_array() {
            Some(a)
                if a.len() <= 4
                    && a.iter()
                        .all(|m| m.as_str().is_some_and(|m| m.contains('/'))) => {}
            _ => {
                p.push("$.fallback must be an array of at most 4 \"provider/model\" strings".into())
            }
        }
    }
    if let Some(x) = raw.get("fallbackReprobeMinutes")
        && !x.as_u64().is_some_and(|n| (1..=1440).contains(&n))
    {
        p.push("$.fallbackReprobeMinutes must be a whole number of minutes, 1 to 1440".into());
    }
    if let Some(x) = raw.get("profiles") {
        validate_profiles(x, "$.profiles", p);
    }
    for key in ["mcp", "mcpServers"] {
        if let Some(x) = raw.get(key) {
            validate_mcp_map(x, &format!("$.{key}"), p);
        }
    }
    if let Some(x) = raw.get("projects") {
        validate_projects(x, "$.projects", p);
    }
    if let Some(c) = raw.get("claude")
        && allowed_keys(c, &["readUserConfig"], "$.claude", p)
        && let Some(r) = c.get("readUserConfig")
    {
        boolean(Some(r), "$.claude.readUserConfig", p);
    }
}
