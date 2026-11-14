//! Provider entry normalisation. Port of Node `src/native/providercfg.js`.

use crate::env::Env;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    Anthropic,
    OpenAi,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::OpenAi => "openai",
        }
    }
}

/// How the protocol was decided (Node `typeSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeSource {
    Explicit,
    Npm,
    #[default]
    Id,
    Url,
    Key,
    Default,
}

impl TypeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TypeSource::Explicit => "explicit",
            TypeSource::Npm => "npm",
            TypeSource::Id => "id",
            TypeSource::Url => "url",
            TypeSource::Key => "key",
            TypeSource::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DetectInput<'a> {
    pub id: Option<&'a str>,
    pub explicit_type: Option<&'a str>,
    pub npm: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub credential: Option<&'a str>,
}

/// explicit type > npm > built-in id > baseURL > `sk-ant-` key > openai-compatible.
pub fn detect_protocol(i: DetectInput<'_>) -> (Protocol, TypeSource) {
    match i.explicit_type {
        Some("anthropic") => return (Protocol::Anthropic, TypeSource::Explicit),
        Some("openai") => return (Protocol::OpenAi, TypeSource::Explicit),
        _ => {}
    }
    if let Some(npm) = i.npm.filter(|n| !n.is_empty()) {
        let p = if npm == "@ai-sdk/anthropic" {
            Protocol::Anthropic
        } else {
            Protocol::OpenAi
        };
        return (p, TypeSource::Npm);
    }
    match i.id {
        Some("anthropic") => return (Protocol::Anthropic, TypeSource::Id),
        Some("openai") => return (Protocol::OpenAi, TypeSource::Id),
        _ => {}
    }
    if let Some(u) = i
        .base_url
        .filter(|u| !u.is_empty())
        .and_then(|u| url::Url::parse(u).ok())
    {
        let host = u.host_str().unwrap_or("").to_lowercase();
        if host == "api.anthropic.com"
            || host.ends_with(".anthropic.com")
            || u.path().split('/').any(|s| s == "anthropic")
        {
            return (Protocol::Anthropic, TypeSource::Url);
        }
        if host == "api.openai.com" {
            return (Protocol::OpenAi, TypeSource::Url);
        }
    }
    if i.credential.is_some_and(|c| c.starts_with("sk-ant-")) {
        return (Protocol::Anthropic, TypeSource::Key);
    }
    (Protocol::OpenAi, TypeSource::Default)
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApiKey {
    /// `apiKey: false` (no auth).
    Disabled,
    Value(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelMeta {
    pub name: Option<String>,
    pub context_window: Option<f64>,
    pub max_output: Option<f64>,
    pub vendor: Option<String>,
    pub family: Option<String>,
    pub quality: Option<Value>,
    pub quality_score: Option<Value>,
    pub quality_rank: Option<Value>,
    pub reasoning: Option<Value>,
    pub thinking: Option<Value>,
    pub capabilities: Map<String, Value>,
}

/// Normalised provider entry (Node's flat internal shape). Unset fields are `None`.
#[derive(Clone, PartialEq, Default)]
pub struct Provider {
    pub protocol: Protocol,
    pub type_source: TypeSource,
    pub npm: Option<String>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<ApiKey>,
    pub api_key_source: Option<String>,
    pub auth_token: Option<String>,
    pub auth_token_source: Option<String>,
    pub auth_header: Option<String>,
    pub headers: Option<IndexMap<String, String>>,
    pub models: Option<Vec<String>>,
    pub models_meta: Option<IndexMap<String, ModelMeta>>,
    pub discover: Option<bool>,
    pub no_auth: Option<bool>,
}

// Secrets must not reach logs: key, token and header values are redacted.
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let red = |set: bool| set.then_some("<redacted>");
        let api = self.api_key.as_ref().map(|k| match k {
            ApiKey::Disabled => "Disabled",
            ApiKey::Value(_) => "<redacted>",
        });
        let header_names = self.headers.as_ref().map(|h| h.keys().collect::<Vec<_>>());
        f.debug_struct("Provider")
            .field("protocol", &self.protocol)
            .field("type_source", &self.type_source)
            .field("npm", &self.npm)
            .field("name", &self.name)
            .field("base_url", &self.base_url.as_deref().map(redact_url))
            .field("api_key", &api)
            .field("api_key_source", &self.api_key_source)
            .field("auth_token", &red(self.auth_token.is_some()))
            .field("auth_token_source", &self.auth_token_source)
            .field("auth_header", &self.auth_header)
            .field("header_names", &header_names)
            .field("models", &self.models)
            .field("models_meta", &self.models_meta)
            .field("discover", &self.discover)
            .field("no_auth", &self.no_auth)
            .finish()
    }
}

/// Drops userinfo and query (they can carry credentials) from a URL for display.
fn redact_url(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(mut p) => {
            let _ = p.set_username("");
            let _ = p.set_password(None);
            p.set_query(None);
            p.to_string()
        }
        Err(_) => "<unparsable url>".into(),
    }
}

impl Provider {
    /// `apiKey === false || noAuth === true`
    pub fn is_no_auth(&self) -> bool {
        self.api_key == Some(ApiKey::Disabled) || self.no_auth == Some(true)
    }

    /// Node `{...base, ...later}`: every field `later` sets wins; the rest stay.
    pub(crate) fn merged_with(&self, later: &Provider) -> Provider {
        macro_rules! pick {
            ($f:ident) => {
                later.$f.clone().or_else(|| self.$f.clone())
            };
        }
        Provider {
            protocol: later.protocol,
            type_source: later.type_source,
            npm: pick!(npm),
            name: pick!(name),
            base_url: pick!(base_url),
            api_key: pick!(api_key),
            api_key_source: pick!(api_key_source),
            auth_token: pick!(auth_token),
            auth_token_source: pick!(auth_token_source),
            auth_header: pick!(auth_header),
            headers: pick!(headers),
            models: pick!(models),
            models_meta: pick!(models_meta),
            discover: pick!(discover),
            no_auth: pick!(no_auth),
        }
    }
}

/// Where `{file:}` paths resolve and which env `{env:}` reads.
#[derive(Clone, Copy)]
pub struct SubstCtx<'a> {
    pub env: &'a Env,
    /// Directory of the config file (base for relative `{file:}` refs).
    pub base_dir: &'a Path,
    /// Real user home, for `~` expansion.
    pub user_home: &'a Path,
    /// `false` for the (untrusted-origin) project layer: `{file:}` is then never read.
    pub allow_file: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Substituted {
    pub value: Option<String>,
    pub source: Option<String>,
    pub warning: Option<String>,
}

fn braced<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    let inner = raw.strip_prefix(prefix)?.strip_suffix('}')?;
    (!inner.is_empty() && !inner.contains('}')).then_some(inner)
}

/// `{env:NAME}` / `{file:path}` / plain string. Warnings name the variable or
/// path, never a value.
pub fn substitute_string(raw: &str, ctx: &SubstCtx<'_>) -> Substituted {
    if let Some(name) = braced(raw, "{env:") {
        // a key typed in place of a name must not be repeated in a diagnostic
        if crate::env::looks_like_key(name) {
            return Substituted {
                warning: Some("{env:...} holds a key, not a variable name".into()),
                ..Default::default()
            };
        }
        return match ctx.env.get(name) {
            Some(v) => Substituted {
                value: Some(v.to_string()),
                source: Some(format!("{{env:{name}}}")),
                warning: None,
            },
            None => Substituted {
                warning: Some(format!(
                    "missing env var \"{name}\" (referenced by {{env:{name}}})"
                )),
                ..Default::default()
            },
        };
    }
    if let Some(r) = braced(raw, "{file:") {
        if crate::env::looks_like_key(r) {
            return Substituted {
                warning: Some("{file:...} holds a key, not a path".into()),
                ..Default::default()
            };
        }
        if !ctx.allow_file {
            return Substituted {
                warning: Some(format!("{{file:{r}}} is not allowed in project config")),
                ..Default::default()
            };
        }
        let mut p: PathBuf = match r.strip_prefix('~') {
            Some(rest) => ctx.user_home.join(rest.trim_start_matches(['/', '\\'])),
            None => PathBuf::from(r),
        };
        if !p.is_absolute() {
            p = ctx.base_dir.join(p);
        }
        // a network path would be contacted by the read itself; the rest is judged on the real path
        if let Err(e) =
            crate::secretfile::check_credential_file(&p, ctx.base_dir, ctx.user_home, false)
        {
            return Substituted {
                warning: Some(format!("{{file:...}} refused: {e}")),
                ..Default::default()
            };
        }
        return match std::fs::read(&p) {
            Ok(b) => Substituted {
                value: Some(String::from_utf8_lossy(&b).trim().to_string()),
                source: Some(format!("{{file:{r}}}")),
                warning: None,
            },
            Err(_) => Substituted {
                warning: Some(format!(
                    "could not read file \"{}\" (referenced by {{file:{r}}})",
                    p.display()
                )),
                ..Default::default()
            },
        };
    }
    Substituted {
        value: Some(raw.to_string()),
        source: Some("inline".into()),
        warning: None,
    }
}

/// Array of ids, or OpenCode's `{id: {name, limit}}` object.
pub fn normalize_models(raw: &Value) -> (Vec<String>, IndexMap<String, ModelMeta>) {
    match raw {
        Value::Array(a) => (
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            IndexMap::new(),
        ),
        Value::Object(m) => {
            let truthy_str = |m: &Value, k: &str| {
                m.get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            };
            let truthy_num = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| *n != 0.0);
            let loose = |m: &Value, k: &str| m.get(k).filter(|v| !v.is_null()).cloned();
            let mut meta = IndexMap::new();
            for (id, m) in m {
                let limit = m.get("limit");
                meta.insert(
                    id.clone(),
                    ModelMeta {
                        name: truthy_str(m, "name"),
                        context_window: truthy_num(limit.and_then(|l| l.get("context"))),
                        max_output: truthy_num(limit.and_then(|l| l.get("output"))),
                        vendor: truthy_str(m, "vendor"),
                        family: truthy_str(m, "family"),
                        quality: loose(m, "quality"),
                        quality_score: loose(m, "qualityScore"),
                        quality_rank: loose(m, "qualityRank"),
                        reasoning: loose(m, "reasoning"),
                        thinking: loose(m, "thinking"),
                        capabilities: m
                            .get("capabilities")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default(),
                    },
                );
            }
            (m.keys().cloned().collect(), meta)
        }
        _ => (Vec::new(), IndexMap::new()),
    }
}

fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(String::from)
}

/// Resolves one credential-ish field, recording a warning on failure.
fn subst_field(
    raw: &str,
    label: &str,
    id: &str,
    ctx: &SubstCtx<'_>,
    warnings: &mut Vec<String>,
) -> Option<(String, Option<String>)> {
    let r = substitute_string(raw, ctx);
    if let Some(w) = r.warning {
        warnings.push(format!("provider \"{id}\" {label}: {w}"));
    }
    // a CR/LF or other control character would split or corrupt the HTTP header it ends up in
    if r.value
        .as_deref()
        .is_some_and(|v| v.chars().any(char::is_control))
    {
        warnings.push(format!(
            "provider \"{id}\" {label}: the value has a control character; ignored"
        ));
        return None;
    }
    r.value.map(|v| (v, r.source))
}

pub fn normalize_provider_entry(
    id: &str,
    raw: &Value,
    ctx: &SubstCtx<'_>,
    warnings: &mut Vec<String>,
) -> Provider {
    let options = raw.get("options");
    let opt = |k: &str| options.and_then(|o| o.get(k)).filter(|v| !v.is_null());
    let rawv = |k: &str| raw.get(k).filter(|v| !v.is_null());
    let no_auth =
        rawv("apiKey") == Some(&Value::Bool(false)) || rawv("noAuth") == Some(&Value::Bool(true));

    let base_url = s(rawv("baseURL").or(opt("baseURL")));
    let npm = s(rawv("npm"));
    let (protocol, type_source) = detect_protocol(DetectInput {
        id: Some(id),
        explicit_type: rawv("type").and_then(Value::as_str),
        npm: npm.as_deref(),
        base_url: base_url.as_deref(),
        credential: None,
    });
    let mut out = Provider {
        protocol,
        type_source,
        npm,
        name: s(rawv("name")).filter(|n| !n.is_empty()),
        base_url,
        auth_header: s(rawv("authHeader").or(opt("authHeader"))).filter(|h| !h.is_empty()),
        discover: rawv("discover").and_then(Value::as_bool),
        no_auth: if no_auth {
            Some(true)
        } else {
            rawv("noAuth").and_then(Value::as_bool)
        },
        ..Default::default()
    };

    if no_auth {
        out.api_key = Some(ApiKey::Disabled);
    } else {
        let api_raw = if rawv("apiKey").is_some_and(Value::is_string) {
            rawv("apiKey")
        } else {
            opt("apiKey")
        };
        match api_raw {
            Some(Value::String(r)) => {
                if let Some((v, src)) = subst_field(r, "apiKey", id, ctx, warnings) {
                    out.api_key = Some(ApiKey::Value(v));
                    out.api_key_source = src;
                }
            }
            Some(Value::Bool(false)) => out.api_key = Some(ApiKey::Disabled),
            _ => {}
        }
        if let Some(r) = rawv("authToken")
            .or(opt("authToken"))
            .and_then(Value::as_str)
            && let Some((v, src)) = subst_field(r, "authToken", id, ctx, warnings)
        {
            out.auth_token = Some(v);
            out.auth_token_source = src;
        }
    }

    let headers = rawv("headers").filter(|h| h.is_object()).or(opt("headers"));
    if let Some(h) = headers.and_then(Value::as_object) {
        let mut out_h = IndexMap::new();
        for (k, v) in h {
            if let Some(raw_v) = v.as_str() {
                let r = substitute_string(raw_v, ctx);
                if let Some(w) = r.warning {
                    warnings.push(format!("provider \"{id}\" header \"{k}\": {w}"));
                }
                if let Some(val) = r.value {
                    if val.chars().any(char::is_control) {
                        warnings.push(format!(
                            "provider \"{id}\" header \"{k}\": the value has a control character; ignored"
                        ));
                        continue;
                    }
                    out_h.insert(k.clone(), val);
                }
            }
        }
        out.headers = Some(out_h);
    }

    if let Some(m) = raw.get("models") {
        let (ids, meta) = normalize_models(m);
        out.models = Some(ids);
        if !meta.is_empty() {
            out.models_meta = Some(meta);
        }
    }
    out
}

/// `raw_providers`: the already-combined `provider` + `providers` object of one file.
pub fn normalize_providers(
    raw_providers: &Map<String, Value>,
    ctx: &SubstCtx<'_>,
) -> (IndexMap<String, Provider>, Vec<String>) {
    let mut warnings = Vec::new();
    let providers = raw_providers
        .iter()
        .map(|(id, raw)| {
            (
                id.clone(),
                normalize_provider_entry(id, raw, ctx, &mut warnings),
            )
        })
        .collect();
    (providers, warnings)
}
