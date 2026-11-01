//! Layered config + model/alias resolution. Port of Node `src/native/config.js`.
//!
//! Precedence (low to high): built-in defaults < `<synthesis_home>/synthesis.json` <
//! `state.json` model < `<cwd>/.synthesis/config.json` < `SYNTHESIS_*` env. Only `SYNTHESIS_*`
//! names are read for layering.

use crate::env::Env;
use crate::provider::{
    DetectInput, Protocol, Provider, SubstCtx, TypeSource, detect_protocol, normalize_providers,
};
use crate::schema::{ConfigError, MAX_CONFIG_BYTES, validate_versioned_config};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Explicit inputs; nothing is taken from the process.
#[derive(Clone, Copy)]
pub struct LoadContext<'a> {
    pub cwd: &'a Path,
    /// `$SYNTHESIS_HOME` (see [`default_synthesis_home`]).
    pub synthesis_home: &'a Path,
    /// Real user home, only for `~` expansion in `{file:}`.
    pub user_home: &'a Path,
    pub env: &'a Env,
}

/// `$SYNTHESIS_HOME` or `<user_home>/.synthesis`.
pub fn default_synthesis_home(env: &Env, user_home: &Path) -> PathBuf {
    env.get_nonempty("SYNTHESIS_HOME")
        .map_or_else(|| user_home.join(".synthesis"), PathBuf::from)
}

/// Trust in `<cwd>/.synthesis/config.json`, which comes from the repository and may be hostile.
/// `Untrusted` (the default) keeps only the safe subset: `$schema`, `model`, `smallModel`,
/// `aliases`, `maxTokens`, `profiles`, and providers' `npm`/`name`/`type`/`models`/`discover`/
/// `noAuth`. Provider connection and credential fields (`baseURL`, `apiKey`, `authToken`,
/// `authHeader`, `headers`, `options`) and `mcp`, `mcpServers`, `projects`, `claude` are dropped
/// with a warning that names the key, never a value. `Trusted` must be an explicit decision by
/// the caller for this project; `{file:}` stays disabled in the project layer either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectTrust {
    #[default]
    Untrusted,
    Trusted,
}

const PROJECT_DENIED_PROVIDER_KEYS: [&str; 6] = [
    "baseURL",
    "apiKey",
    "authToken",
    "authHeader",
    "headers",
    "options",
];
const PROJECT_DENIED_TOP_KEYS: [&str; 4] = ["mcp", "mcpServers", "projects", "claude"];

fn strip_untrusted(raw: &mut Value, warnings: &mut Vec<String>) {
    let Some(top) = raw.as_object_mut() else {
        return;
    };
    for k in PROJECT_DENIED_TOP_KEYS {
        if top.remove(k).is_some() {
            warnings.push(format!(
                "project config: ignored \"{k}\" (untrusted project)"
            ));
        }
    }
    for group in ["providers", "provider"] {
        let Some(m) = top.get_mut(group).and_then(Value::as_object_mut) else {
            continue;
        };
        for (id, entry) in m {
            let Some(e) = entry.as_object_mut() else {
                continue;
            };
            for k in PROJECT_DENIED_PROVIDER_KEYS {
                if e.remove(k).is_some() {
                    warnings.push(format!(
                        "project config: ignored {group}.\"{id}\".{k} (untrusted project)"
                    ));
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Alias name to target; `None` = resolved at runtime from discovery.
    pub aliases: IndexMap<String, Option<String>>,
    pub providers: IndexMap<String, Provider>,
    pub max_tokens: f64,
    /// `$schema` of the last loaded file (Node keeps it in the merged object).
    pub schema: Option<String>,
    pub model: Option<String>,
    pub small_model: Option<String>,
    /// Deep-merged passthrough keys: `profiles`, `mcp`, `mcpServers`, `projects`, `claude`.
    pub extra: Map<String, Value>,
    pub model_explicit: bool,
    pub explicit_aliases: BTreeSet<String>,
    pub warnings: Vec<String>,
}

const ALIAS_NAMES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

impl Default for Config {
    /// Built-in defaults: `anthropic` + `openai`, no model, null alias targets.
    fn default() -> Self {
        let builtin = |protocol| Provider {
            protocol,
            type_source: TypeSource::Id,
            models: Some(Vec::new()),
            ..Default::default()
        };
        let mut anthropic = builtin(Protocol::Anthropic);
        anthropic.headers = Some(IndexMap::new());
        Config {
            aliases: ALIAS_NAMES.iter().map(|a| (a.to_string(), None)).collect(),
            providers: IndexMap::from([
                ("anthropic".to_string(), anthropic),
                ("openai".to_string(), builtin(Protocol::OpenAi)),
            ]),
            max_tokens: 32000.0,
            schema: None,
            model: None,
            small_model: None,
            extra: Map::from_iter([("profiles".to_string(), Value::Object(Map::new()))]),
            model_explicit: false,
            explicit_aliases: BTreeSet::new(),
            warnings: Vec::new(),
        }
    }
}

/// `state.json` (`{model?, effort?, autocompact?}`); effort is read as is, outside layering.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub autocompact: Option<Value>,
}

/// Missing, unreadable or malformed state.json yields the empty state (like Node).
pub fn load_state(synthesis_home: &Path) -> State {
    let v: Value = std::fs::read(synthesis_home.join("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    State {
        model: text("model"),
        effort: text("effort"),
        autocompact: v.get("autocompact").cloned(),
    }
}

fn deep_merge(base: &mut Value, extra: &Value) {
    let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (k, v) in e {
        match b.get_mut(k) {
            Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
            _ => {
                b.insert(k.clone(), v.clone());
            }
        }
    }
}

fn read_json(file: &Path) -> Result<Option<Value>, ConfigError> {
    let name = file.display().to_string();
    let too_large = |m: String| ConfigError::new(&name, [m]);
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(too_large(format!("cannot read file: {e}"))),
    };
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(too_large(format!(
            "file is too large ({} bytes; maximum {MAX_CONFIG_BYTES})",
            meta.len()
        )));
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(too_large(format!("cannot read file: {e}"))),
    };
    let text = String::from_utf8_lossy(&bytes);
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(too_large(format!(
            "file is too large (maximum {MAX_CONFIG_BYTES} bytes)"
        )));
    }
    // Never forward the parser's message: it can quote file contents (inline credentials).
    // A UTF-8 BOM (Windows PowerShell 5.1 `Set-Content -Encoding UTF8`, older Notepad) is not part of the JSON.
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map(Some)
        .map_err(|_| too_large("malformed JSON".into()))
}

struct FileConfig {
    schema: Option<String>,
    model: Option<String>,
    small_model: Option<String>,
    max_tokens: Option<f64>,
    aliases: IndexMap<String, String>,
    providers: IndexMap<String, Provider>,
    extra: Map<String, Value>,
    warnings: Vec<String>,
}

/// `None` = user layer; `Some(trust)` = project layer.
fn load_config_file(
    file: &Path,
    ctx: &LoadContext<'_>,
    project: Option<ProjectTrust>,
) -> Result<Option<FileConfig>, ConfigError> {
    let Some(raw) = read_json(file)? else {
        return Ok(None);
    };
    // Node `if (!raw) return null`: JSON null/false/0/"" is treated as an absent file.
    let falsy = match &raw {
        Value::Null | Value::Bool(false) => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        _ => false,
    };
    if falsy {
        return Ok(None);
    }
    validate_versioned_config(&raw, &file.display().to_string())?;
    let mut raw = raw;
    let mut trust_warnings = Vec::new();
    if project == Some(ProjectTrust::Untrusted) {
        strip_untrusted(&mut raw, &mut trust_warnings);
    }
    // `provider` wins over `providers` on an id collision.
    let mut raw_providers = raw
        .get("providers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(p) = raw.get("provider").and_then(Value::as_object) {
        raw_providers.extend(p.clone());
    }
    let base_dir = file.parent().unwrap_or(Path::new(""));
    let (providers, warnings) = normalize_providers(
        &raw_providers,
        &SubstCtx {
            env: ctx.env,
            base_dir,
            user_home: ctx.user_home,
            allow_file: project.is_none(),
        },
    );
    let warnings = [trust_warnings, warnings].concat();
    let text = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    let extra = ["profiles", "mcp", "mcpServers", "projects", "claude"]
        .iter()
        .filter_map(|k| raw.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    Ok(Some(FileConfig {
        schema: text("$schema"),
        model: text("model"),
        small_model: text("smallModel"),
        max_tokens: raw.get("maxTokens").and_then(Value::as_f64),
        aliases: raw
            .get("aliases")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        providers,
        extra,
        warnings,
    }))
}

impl Config {
    fn merge(&mut self, f: FileConfig) -> bool {
        let explicit_model = f.model.is_some();
        if f.schema.is_some() {
            self.schema = f.schema;
        }
        if f.model.is_some() {
            self.model = f.model;
        }
        if f.small_model.is_some() {
            self.small_model = f.small_model;
        }
        if let Some(t) = f.max_tokens {
            self.max_tokens = t;
        }
        for (k, v) in f.aliases {
            self.explicit_aliases.insert(k.clone());
            self.aliases.insert(k, Some(v));
        }
        for (id, p) in f.providers {
            let merged = match self.providers.get(&id) {
                Some(base) => base.merged_with(&p),
                None => p,
            };
            self.providers.insert(id, merged);
        }
        for (k, v) in f.extra {
            match self.extra.get_mut(&k) {
                Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, &v),
                _ => {
                    self.extra.insert(k, v);
                }
            }
        }
        self.warnings.extend(f.warnings);
        explicit_model
    }
}

/// Case-insensitive match of the target id against configured providers.
fn canonical_provider_id(providers: &IndexMap<String, Provider>, id: &str) -> String {
    if providers.contains_key(id) {
        return id.to_string();
    }
    let lower = id.to_lowercase();
    providers
        .keys()
        .find(|k| k.to_lowercase() == lower)
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

/// `SYNTHESIS_BASE_URL` / `SYNTHESIS_CUSTOM_HEADERS` onto the `SYNTHESIS_PROVIDER` entry (default `anthropic`).
pub fn apply_flat_provider_env(providers: &mut IndexMap<String, Provider>, env: &Env) {
    let id = canonical_provider_id(
        providers,
        env.get_nonempty("SYNTHESIS_PROVIDER")
            .unwrap_or("anthropic"),
    );
    let base_url = env.get_nonempty("SYNTHESIS_BASE_URL");
    let header_text = env.get_nonempty("SYNTHESIS_CUSTOM_HEADERS");
    let has_target = base_url.is_some()
        || header_text.is_some()
        || env.get_nonempty("SYNTHESIS_AUTH_TOKEN").is_some()
        || env.get_nonempty("SYNTHESIS_API_KEY").is_some();
    if !has_target {
        return;
    }
    if !providers.contains_key(&id) {
        let Some(url) = base_url else { return };
        let (protocol, type_source) = detect_protocol(DetectInput {
            id: Some(&id),
            base_url: Some(url),
            credential: env
                .get_nonempty("SYNTHESIS_API_KEY")
                .or(env.get_nonempty("SYNTHESIS_AUTH_TOKEN")),
            ..Default::default()
        });
        providers.insert(
            id.clone(),
            Provider {
                protocol,
                type_source,
                models: Some(Vec::new()),
                ..Default::default()
            },
        );
    }
    let p = providers.get_mut(&id).expect("inserted above");
    if let Some(u) = base_url {
        p.base_url = Some(u.to_string());
    }
    if let Some(t) = header_text {
        p.headers
            .get_or_insert_with(IndexMap::new)
            .extend(parse_header_lines(t));
    }
}

/// `Name: value` lines; later lines for the same name win.
pub fn parse_header_lines(text: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        if let Some((name, value)) = line.split_once(':')
            && !name.is_empty()
        {
            out.insert(name.trim().to_string(), value.trim().to_string());
        }
    }
    out
}

/// Moves the model's provider to `SYNTHESIS_PROVIDER` when flat env vars are in play.
pub fn apply_env_default_model(
    model: Option<&str>,
    env: &Env,
    providers: &IndexMap<String, Provider>,
) -> Option<String> {
    let model = model.filter(|m| !m.is_empty())?;
    let id = canonical_provider_id(
        providers,
        env.get_nonempty("SYNTHESIS_PROVIDER")
            .unwrap_or("anthropic"),
    );
    let has_target = [
        "SYNTHESIS_BASE_URL",
        "SYNTHESIS_AUTH_TOKEN",
        "SYNTHESIS_API_KEY",
    ]
    .iter()
    .any(|k| env.get_nonempty(k).is_some());
    if !has_target {
        return Some(model.to_string());
    }
    let bare = model.split_once('/').map_or(model, |(_, b)| b);
    Some(format!("{id}/{bare}"))
}

/// `SYNTHESIS_DEFAULT_<NAME>_MODEL` -> lowercase `<name>`.
fn alias_env_name(key: &str) -> Option<String> {
    let mid = key
        .strip_prefix("SYNTHESIS_DEFAULT_")?
        .strip_suffix("_MODEL")?;
    (!mid.is_empty()).then(|| mid.to_lowercase())
}

/// Loads with the project layer untrusted (see [`ProjectTrust`]).
pub fn load_config(ctx: &LoadContext<'_>) -> Result<Config, ConfigError> {
    load_config_with_trust(ctx, ProjectTrust::Untrusted)
}

pub fn load_config_with_trust(
    ctx: &LoadContext<'_>,
    trust: ProjectTrust,
) -> Result<Config, ConfigError> {
    let env = ctx.env;
    let mut config = Config::default();
    let mut model_explicit = false;

    if let Some(f) = load_config_file(&ctx.synthesis_home.join("synthesis.json"), ctx, None)? {
        model_explicit |= config.merge(f);
    }
    if let Some(m) = load_state(ctx.synthesis_home).model {
        config.model = Some(m);
        model_explicit = false;
    }
    if let Some(f) = load_config_file(
        &ctx.cwd.join(".synthesis").join("config.json"),
        ctx,
        Some(trust),
    )? {
        model_explicit |= config.merge(f);
    }

    apply_flat_provider_env(&mut config.providers, env);
    for (key, value) in env.iter() {
        if let Some(name) = alias_env_name(key) {
            config.explicit_aliases.insert(name.clone());
            config.aliases.insert(name, Some(value.to_string()));
        }
    }

    let env_model = env.get_nonempty("SYNTHESIS_MODEL");
    if env_model.is_none() {
        config.model = apply_env_default_model(config.model.as_deref(), env, &config.providers);
    }
    if let Some(m) = env_model {
        config.model = Some(m.to_string());
        model_explicit = true;
    }
    if let Some(m) = env.get_nonempty("SYNTHESIS_SMALL_MODEL") {
        config.small_model = Some(m.to_string());
    }
    config.model_explicit = model_explicit;
    Ok(config)
}

// --- model families and defaults ------------------------------------------

const FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `/claude-(opus|sonnet|haiku|fable)\b/`
pub fn model_family(id_or_alias: &str) -> Option<&'static str> {
    id_or_alias.match_indices("claude-").find_map(|(i, m)| {
        let rest = &id_or_alias[i + m.len()..];
        FAMILIES.iter().copied().find(|f| {
            rest.starts_with(f) && !rest.as_bytes().get(f.len()).is_some_and(|c| is_word(*c))
        })
    })
}

/// Digits after `claude-<family>-` (date-like 8-digit segments dropped); `[]` if none.
fn version_of(id: &str, family: &str) -> Vec<f64> {
    let needle = format!("claude-{family}");
    let Some(i) = id.find(&needle) else {
        return Vec::new();
    };
    let Some(rest) = id[i + needle.len()..].strip_prefix('-') else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for seg in rest.split('-') {
        let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        let full = digits.len() == seg.len();
        if digits.len() != 8 || !full {
            parts.push(digits.parse::<f64>().unwrap_or(0.0));
        }
        if !full {
            break;
        }
    }
    parts
}

fn compare_versions(a: &[f64], b: &[f64]) -> std::cmp::Ordering {
    for i in 0..a.len().max(b.len()) {
        let d = b.get(i).copied().unwrap_or(0.0) - a.get(i).copied().unwrap_or(0.0);
        if d != 0.0 {
            return d.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal);
        }
    }
    std::cmp::Ordering::Equal
}

/// Best discovered id for a family: unprefixed ids first, then highest version.
pub fn best_model_for_family<S: AsRef<str>>(
    discovered: &[S],
    family: Option<&str>,
) -> Option<String> {
    let family = family?;
    let mut c: Vec<&str> = discovered
        .iter()
        .map(AsRef::as_ref)
        .filter(|id| model_family(id) == Some(family))
        .collect();
    c.sort_by(|a, b| {
        a.contains('/')
            .cmp(&b.contains('/'))
            .then_with(|| compare_versions(&version_of(a, family), &version_of(b, family)))
    });
    c.first().map(|s| s.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swap {
    pub provider_id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDefaults {
    pub model: Option<String>,
    pub aliases: IndexMap<String, Option<String>>,
    pub notes: Vec<Swap>,
    pub warnings: Vec<Unavailable>,
}

/// Resolves `config.model` / aliases against discovered ids per provider. Explicit
/// targets are never swapped (a warning is recorded instead).
pub fn resolve_model_defaults(
    config: &Config,
    discovered: &IndexMap<String, Vec<String>>,
    model_explicit: bool,
    explicit_aliases: &BTreeSet<String>,
) -> ResolvedDefaults {
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let mut try_fallback =
        |target: Option<&str>, hint: Option<&str>, explicit: bool| -> Option<String> {
            let target = target.filter(|t| !t.is_empty())?;
            let parsed = parse_model(Some(target), config).ok();
            let (Some(pid), bare) = parsed.map(|p| (p.provider_id, p.model))? else {
                return Some(target.to_string());
            };
            let ids = match discovered.get(&pid) {
                Some(ids) if !ids.is_empty() && !ids.contains(&bare) => ids,
                _ => return Some(target.to_string()),
            };
            if explicit {
                warnings.push(Unavailable {
                    provider_id: pid,
                    model: bare,
                });
                return Some(target.to_string());
            }
            let family = model_family(&bare).or(hint);
            match best_model_for_family(ids, family) {
                Some(fb) => {
                    let out = format!("{pid}/{fb}");
                    notes.push(Swap {
                        provider_id: pid,
                        from: bare,
                        to: fb,
                    });
                    Some(out)
                }
                None => Some(target.to_string()),
            }
        };
    let fill_null = |hint: &str| {
        discovered.iter().find_map(|(pid, ids)| {
            best_model_for_family(ids, Some(hint)).map(|f| format!("{pid}/{f}"))
        })
    };

    let model = try_fallback(config.model.as_deref(), None, model_explicit);
    let mut aliases = IndexMap::new();
    for (name, target) in &config.aliases {
        let explicit = explicit_aliases.contains(name);
        let resolved = if target.as_deref().is_none_or(str::is_empty) && !explicit {
            fill_null(name)
        } else {
            try_fallback(target.as_deref(), Some(name), explicit)
        };
        aliases.insert(name.clone(), resolved);
    }
    ResolvedDefaults {
        model,
        aliases,
        notes,
        warnings,
    }
}

pub fn resolve_alias(name: &str, config: &Config) -> String {
    config
        .aliases
        .get(name)
        .and_then(|t| t.as_deref())
        .filter(|t| !t.is_empty())
        .unwrap_or(name)
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModel {
    /// `None` only when nothing is configured at all.
    pub provider_id: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoModelConfigured;

impl std::fmt::Display for NoModelConfigured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no model configured")
    }
}
impl std::error::Error for NoModelConfigured {}

/// `provider/model`, an alias, or a bare model (first provider listing it, else the
/// default model's provider, else the first provider).
pub fn parse_model(s: Option<&str>, config: &Config) -> Result<ParsedModel, NoModelConfigured> {
    let s = s
        .filter(|s| !s.is_empty())
        .or(config.model.as_deref().filter(|m| !m.is_empty()))
        .ok_or(NoModelConfigured)?;
    let resolved = resolve_alias(s, config);
    if let Some((p, m)) = resolved.split_once('/') {
        return Ok(ParsedModel {
            provider_id: Some(p.to_string()),
            model: m.to_string(),
        });
    }
    for (id, p) in &config.providers {
        if p.models.as_ref().is_some_and(|m| m.contains(&resolved)) {
            return Ok(ParsedModel {
                provider_id: Some(id.clone()),
                model: resolved,
            });
        }
    }
    let def = config
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(|m| resolve_alias(m, config));
    let provider_id = match def.as_deref().and_then(|d| d.split_once('/')) {
        Some((p, _)) => Some(p.to_string()),
        None => config.providers.keys().next().cloned(),
    };
    Ok(ParsedModel {
        provider_id,
        model: resolved,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    pub value: String,
    pub provider: String,
    pub model: String,
}

/// provider/model pairs from each provider's list plus alias / default targets.
pub fn list_models(config: &Config, provider_ids: Option<&[String]>) -> Vec<ListedModel> {
    let mut out: Vec<ListedModel> = Vec::new();
    let mut add = |id: &str, model: &str| {
        if provider_ids.is_some_and(|ids| !ids.iter().any(|i| i == id)) {
            return;
        }
        let value = format!("{id}/{model}");
        if !out.iter().any(|m| m.value == value) {
            out.push(ListedModel {
                value,
                provider: id.to_string(),
                model: model.to_string(),
            });
        }
    };
    for (id, p) in &config.providers {
        for m in p.models.iter().flatten() {
            add(id, m);
        }
    }
    let extras = [config.model.as_deref(), config.small_model.as_deref()]
        .into_iter()
        .chain(config.aliases.values().map(Option::as_deref))
        .flatten()
        .filter(|s| !s.is_empty());
    for s in extras {
        if let Ok(ParsedModel {
            provider_id: Some(pid),
            model,
        }) = parse_model(Some(s), config)
            && config.providers.contains_key(&pid)
        {
            add(&pid, &model);
        }
    }
    out
}

/// State's saved model if usable and still offered, else the best sonnet of the first
/// usable provider with ids, else that provider's first id.
pub fn pick_default_model(
    config: &Config,
    usable_provider_ids: &[String],
    discovered: &IndexMap<String, Vec<String>>,
    saved_model: Option<&str>,
) -> Option<String> {
    if usable_provider_ids.is_empty() {
        return None;
    }
    if let Some(saved) = saved_model.filter(|s| !s.is_empty())
        && let Ok(ParsedModel {
            provider_id: Some(pid),
            model,
        }) = parse_model(Some(saved), config)
        && usable_provider_ids.contains(&pid)
        && discovered.get(&pid).is_none_or(|ids| ids.contains(&model))
    {
        return Some(saved.to_string());
    }
    for id in usable_provider_ids {
        let Some(ids) = discovered.get(id).filter(|ids| !ids.is_empty()) else {
            continue;
        };
        let best = best_model_for_family(ids, Some("sonnet")).unwrap_or_else(|| ids[0].clone());
        return Some(format!("{id}/{best}"));
    }
    None
}

/// `name` from config, else `Anthropic`/`OpenAI` for built-ins, else the id.
pub fn provider_display_name(config: &Config, provider_id: &str) -> String {
    config
        .providers
        .get(provider_id)
        .and_then(|p| p.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| match provider_id {
            "anthropic" => "Anthropic".into(),
            "openai" => "OpenAI".into(),
            other => other.to_string(),
        })
}
