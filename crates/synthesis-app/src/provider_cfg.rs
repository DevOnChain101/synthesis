//! Custom providers of the USER `synt.json` for a settings UI: validate a form, write
//! `providers.<id>` atomically (other keys, their order and the entry's unmanaged fields stay),
//! list and remove. ADR-011: a credential is only ever a `{env:NAME}` / `{file:path}` reference;
//! input that looks like a key is refused and nothing here reads, returns or logs a key value.

use std::path::Path;

use serde_json::{Map, Value, json};
use synthesis_config::{
    EFFORT_LEVELS, Protocol, SCHEMA_V1, looks_like_key, validate_versioned_config,
};

use crate::models::parse_tokens;
use synthesis_harness::sanitize_for_display as san;

/// Where the form's references are checked: the synt home (`SYNT_HOME`) and the real user home (`~`).
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub home: &'a Path,
    pub user_home: &'a Path,
}

/// What the form holds; every field is the text as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub id: String,
    pub protocol: Protocol,
    pub base_url: String,
    /// An environment variable name (`MY_KEY`) or a file path; empty = no key.
    pub api_key: String,
    /// One `Name: VARIABLE-or-path` per line.
    pub headers: String,
    /// One `id | display name | context | max output | effort,levels` per line (all but the id optional).
    pub models: String,
    /// The [`Confirm::token`] the person accepted for this exact form (kept nowhere else).
    pub confirmed: String,
}

/// A configured provider as the list shows it (no secret; a URL without userinfo or query).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub protocol: &'static str,
    pub base_url: String,
    /// `{env:NAME}` / `{file:path}`, `none`, or `literal in file (hidden)`.
    pub key: String,
    pub models: Vec<String>,
}

/// What a click-through must show before the provider receives a credential.
#[derive(Clone, Debug, PartialEq)]
pub struct Confirm {
    /// Changes whenever a reference or the destination host changes.
    pub token: String,
    pub text: String,
}

const MAX_HEADERS: usize = 8;
const MAX_MODELS: usize = 64;
/// Biggest key file a test reads.
const MAX_KEY_FILE: u64 = 4096;
/// The ids the loader and `SYNT_API_KEY` treat as the vendors'.
const BUILTIN_IDS: [&str; 2] = ["anthropic", "openai"];

pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 40
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(
            "provider name: 1-40 of a-z 0-9 - _ . , starting with a letter or digit".into(),
        );
    }
    if BUILTIN_IDS.contains(&id) {
        return Err(
            "provider name: anthropic and openai are the built-in providers; pick another name"
                .into(),
        );
    }
    Ok(())
}

/// https, or http for a loopback host only; no userinfo, query or fragment (they can carry a key). The
/// stored form is the parsed one (`Url::as_str`: lower-case scheme and host).
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    let u = url::Url::parse(raw.trim()).map_err(|_| "base URL: not a valid URL".to_string())?;
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(a)) => a.is_loopback(),
        Some(url::Host::Ipv6(a)) => a.is_loopback(),
        None => return Err("base URL: no host".into()),
    };
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => return Err("base URL: https is required (http only for localhost)".into()),
        _ => return Err("base URL: must start with https://".into()),
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("base URL: no credentials in the URL; use an environment variable".into());
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err("base URL: no query or fragment (they can carry a key)".into());
    }
    Ok(u.as_str().to_string())
}

/// A note (not a refusal) for a host that is a private, link-local or metadata address: the key and the
/// conversation go there. Domain names are not resolved here.
pub fn url_warning(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw.trim()).ok()?;
    let internal = match u.host()? {
        url::Host::Ipv4(a) => !a.is_loopback() && synthesis_platform::net::blocked_v4(a),
        url::Host::Ipv6(a) => !a.is_loopback() && synthesis_platform::net::blocked_v6(a),
        url::Host::Domain(_) => false,
    };
    internal.then(|| "This address is private, link-local or a metadata service: the key and your conversations would be sent to a machine on your network.".into())
}

const KEY_HELP: &str = "give the NAME of an environment variable that holds the key (for example MY_KEY) or a file path";

/// A reference as parsed (names and paths already checked).
#[derive(Clone, Debug, PartialEq)]
enum Ref {
    Env(String),
    File(String),
}

impl Ref {
    fn render(&self) -> String {
        match self {
            Ref::Env(n) => format!("{{env:{n}}}"),
            Ref::File(p) => format!("{{file:{p}}}"),
        }
    }
    /// Needs the person's click-through: any file, any variable that is not the product's own.
    fn needs_confirm(&self) -> bool {
        match self {
            Ref::Env(n) => !n.starts_with("SYNT_"),
            Ref::File(_) => true,
        }
    }
}

/// `UPPER_SNAKE`: at least one `_`, at most 64 characters, no segment over 24, at most a third digits. A
/// token pasted by mistake rarely has this shape.
fn env_name_shape(n: &str) -> bool {
    n.len() <= 64
        && n.contains('_')
        && n.starts_with(|c: char| c.is_ascii_uppercase())
        && n.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && n.split('_').all(|seg| seg.len() <= 24)
        && n.bytes().filter(u8::is_ascii_digit).count() * 3 <= n.len()
}

/// Names that hold another tool's or a cloud's credentials: never sent to a model provider.
fn env_denied(n: &str) -> bool {
    const PREFIXES: [&str; 22] = [
        "AWS_",
        "GITHUB_",
        "GH_",
        "GITLAB_",
        "NPM_",
        "NODE_AUTH",
        "SSH_",
        "AZURE_",
        "GCP_",
        "GOOGLE_",
        "GCLOUD_",
        "KUBE",
        "DOCKER_",
        "SLACK_",
        "STRIPE_",
        "CI_",
        "TF_",
        "HEROKU_",
        "DIGITALOCEAN_",
        "PG",
        "MYSQL_",
        "VAULT_",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || n == "DATABASE_URL"
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn under(path: &str, dir: &str) -> bool {
    let d = dir.trim_end_matches('/');
    path == d || path.strip_prefix(d).is_some_and(|r| r.starts_with('/'))
}

/// A `{file:}` path a key may be read from: absolute, `~/` or `./` (relative to the synt home), no `..`, no
/// UNC or device path, nothing that is another tool's secret store or a system or state path; under the
/// synt home only `keys/`. When the file exists (canonicalised first) it must be a regular file of at most
/// 4 KB. Never touches a network path.
fn check_file(path: &str, ctx: &Ctx<'_>) -> Result<std::path::PathBuf, String> {
    let bad = |why: &str| Err(format!("file path: {why}"));
    let p = path.replace('\\', "/");
    if p.starts_with("//")
        || p.starts_with("/dev/")
        || p.starts_with("/proc/")
        || p.starts_with("/sys/")
    {
        return bad("network and device paths are refused");
    }
    if p.split('/').any(|c| c == "..") {
        return bad("no .. in the path");
    }
    let full = if let Some(r) = p.strip_prefix("~/") {
        ctx.user_home.join(r)
    } else if let Some(r) = p.strip_prefix("./") {
        ctx.home.join(r)
    } else if std::path::Path::new(path).is_absolute() || p.as_bytes().get(1) == Some(&b':') {
        std::path::PathBuf::from(path)
    } else {
        return bad("use an absolute path, ~/... or ./... (relative to the synt home)");
    };
    let real = std::fs::canonicalize(&full).ok();
    let home = std::fs::canonicalize(ctx.home).unwrap_or_else(|_| ctx.home.to_path_buf());
    let uh = std::fs::canonicalize(ctx.user_home).unwrap_or_else(|_| ctx.user_home.to_path_buf());
    for cand in std::iter::once(&full).chain(real.as_ref()) {
        let n = norm(cand);
        let in_home = under(&n, &norm(&home)) || under(&n, &norm(ctx.home));
        if in_home {
            if under(&n, &format!("{}/keys", norm(&home)))
                || under(&n, &format!("{}/keys", norm(ctx.home)))
            {
                continue;
            }
            return bad("under the synt home only the keys/ folder can hold a key");
        }
        let uh_n = [norm(&uh), norm(ctx.user_home)];
        let secret_dirs = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gcloud",
            ".kube",
            ".docker",
            ".netrc",
            ".npmrc",
            ".git-credentials",
        ];
        let system = [
            "/etc",
            "/proc",
            "/sys",
            "/dev",
            "/boot",
            "/root",
            "/usr",
            "/bin",
            "/sbin",
            "/private/etc",
            "/system",
            "/library",
            "c:/windows",
            "c:/programdata",
        ];
        if uh_n
            .iter()
            .any(|u| secret_dirs.iter().any(|d| under(&n, &format!("{u}/{d}"))))
            || system.iter().any(|d| under(&n, d))
        {
            return bad("that location holds credentials or system files; use a file of its own");
        }
    }
    if let Some(r) = &real {
        let m = std::fs::metadata(r).map_err(|_| "file path: not readable".to_string())?;
        if !m.is_file() || m.len() > MAX_KEY_FILE {
            return bad("must be a regular file of at most 4 KB");
        }
    }
    Ok(full)
}

/// `MY_KEY` -> `{env:MY_KEY}`, `~/keys/x` or `/run/x` -> `{file:...}`; a ready `{env:}` / `{file:}`
/// passes. Anything else, a key typed by mistake above all, is refused with a pointer to an environment
/// variable, and no error text repeats the input.
fn parse_ref(input: &str, ctx: &Ctx<'_>) -> Result<Ref, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(format!("API key: {KEY_HELP}"));
    }
    let env = |n: &str| -> Result<Ref, String> {
        if !env_name_shape(n) {
            return Err(format!(
                "API key: not a variable name (UPPER_CASE_WITH_UNDERSCORE, up to 64 characters); never type a key here: {KEY_HELP}"
            ));
        }
        if env_denied(n) {
            return Err("API key: that variable holds another tool's or a cloud's credentials (AWS_, GITHUB_, GH_, NPM_, SSH_, GOOGLE_ ...); use a variable of its own, ideally SYNT_...".into());
        }
        Ok(Ref::Env(n.to_string()))
    };
    let file = |p: &str| check_file(p, ctx).map(|_| Ref::File(p.to_string()));
    if let Some(n) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        return env(n);
    }
    if let Some(p) = s.strip_prefix("{file:").and_then(|r| r.strip_suffix('}')) {
        return if looks_like_key(p) || p.contains('}') || p.chars().any(char::is_control) {
            Err(format!("API key: bad file path; {KEY_HELP}"))
        } else {
            file(p)
        };
    }
    if env_name_shape(s) {
        return env(s);
    }
    if looks_like_key(s) {
        return Err(format!(
            "that looks like an API key itself: never type a key here; put it in an environment variable and {KEY_HELP}"
        ));
    }
    if (s.starts_with('/')
        || s.starts_with("~/")
        || s.starts_with("./")
        || s.starts_with('\\')
        || s.as_bytes().get(1) == Some(&b':'))
        && !s.contains('}')
        && !s.chars().any(char::is_control)
    {
        return file(s);
    }
    Err(format!(
        "API key: not an environment variable name or a file path (absolute, ~/ or ./); never type a key here: {KEY_HELP}"
    ))
}

/// The reference text for `input` (see [`parse_ref`]).
pub fn parse_key_ref(input: &str, ctx: &Ctx<'_>) -> Result<String, String> {
    parse_ref(input, ctx).map(|r| r.render())
}

/// Request headers the form never sets: they frame the request, carry another credential or identity.
const DENIED_HEADERS: [&str; 12] = [
    "host",
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "content-length",
    "transfer-encoding",
    "connection",
    "te",
    "upgrade",
    "proxy-connection",
    "keep-alive",
];

fn parse_headers(text: &str, key_set: bool, ctx: &Ctx<'_>) -> Result<Vec<(String, Ref)>, String> {
    let mut out: Vec<(String, Ref)> = vec![];
    for (n, line) in text
        .lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
    {
        let at = format!("header line {}", n + 1);
        let (name, val) = line
            .split_once(':')
            .ok_or_else(|| format!("{at}: write Name: VARIABLE"))?;
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("{at}: the name has letters, digits and - only"));
        }
        let lower = name.to_ascii_lowercase();
        if DENIED_HEADERS.contains(&lower.as_str()) || (key_set && lower == "x-api-key") {
            return Err(format!(
                "{at}: this header is set by the client or is the API key's own"
            ));
        }
        if out.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            return Err(format!("{at}: the header is listed twice"));
        }
        let r = parse_ref(val, ctx).map_err(|e| format!("{at}: {e}"))?;
        out.push((name.to_string(), r));
    }
    if out.len() > MAX_HEADERS {
        return Err(format!("at most {MAX_HEADERS} headers"));
    }
    Ok(out)
}

/// One parsed model line.
#[derive(Debug)]
struct ModelLine {
    id: String,
    name: Option<String>,
    context: Option<u64>,
    output: Option<u64>,
    effort: Vec<String>,
}

fn parse_models(text: &str) -> Result<Vec<ModelLine>, String> {
    let mut out: Vec<ModelLine> = vec![];
    for (n, line) in text
        .lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
    {
        let at = format!("model line {}", n + 1);
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        let id = f[0];
        let id_ok = !id.is_empty()
            && id.len() <= 128
            && id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+')
            })
            && !looks_like_key(id);
        if !id_ok {
            return Err(format!(
                "{at}: the id has letters, digits and . _ - : / @ + only, and is not a key"
            ));
        }
        if out.iter().any(|m| m.id == id) {
            return Err(format!("{at}: the model is listed twice"));
        }
        if f.len() > 5 {
            return Err(format!(
                "{at}: at most 5 fields (id | name | context | max output | effort)"
            ));
        }
        let tokens = |i: usize, what: &str| -> Result<Option<u64>, String> {
            f.get(i)
                .filter(|s| !s.is_empty())
                .map(|s| parse_tokens(s).map_err(|e| format!("{at} {what}: {e}")))
                .transpose()
        };
        let effort: Vec<String> = f
            .get(4)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if effort.iter().any(|l| !EFFORT_LEVELS.contains(&l.as_str())) {
            return Err(format!(
                "{at}: effort levels are {}",
                EFFORT_LEVELS.join(", ")
            ));
        }
        if effort
            .iter()
            .enumerate()
            .any(|(i, l)| effort[..i].contains(l))
        {
            return Err(format!("{at}: effort levels must not repeat"));
        }
        let name = f.get(1).filter(|s| !s.is_empty()).map(|s| s.to_string());
        if name
            .as_deref()
            .is_some_and(|n| looks_like_key(n) || n.chars().any(char::is_control))
        {
            return Err(format!(
                "{at}: the display name looks like a key or has control characters"
            ));
        }
        out.push(ModelLine {
            id: id.into(),
            name,
            context: tokens(2, "context window")?,
            output: tokens(3, "max output")?,
            effort,
        });
    }
    if out.len() > MAX_MODELS {
        return Err(format!("at most {MAX_MODELS} models"));
    }
    Ok(out)
}

/// The key reference and the header references of a form, checked. `None` key: no key (a local server or a
/// gateway authenticated by a header).
struct Parsed {
    url: String,
    key: Option<Ref>,
    headers: Vec<(String, Ref)>,
}

fn parse_form(d: &Draft, ctx: &Ctx<'_>) -> Result<Parsed, String> {
    validate_id(d.id.trim())?;
    let url = validate_base_url(&d.base_url)?;
    let key_set = !d.api_key.trim().is_empty();
    let headers = parse_headers(&d.headers, key_set, ctx)?;
    let key = if key_set {
        Some(parse_ref(&d.api_key, ctx)?)
    } else if !headers.is_empty() || url.starts_with("http:") {
        None
    } else {
        return Err(format!("API key: {KEY_HELP}"));
    };
    Ok(Parsed { url, key, headers })
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().map_or_else(String::new, |u| {
        format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""))
            + &u.port().map_or(String::new(), |p| format!(":{p}"))
    })
}

fn confirm_of(refs: &[Ref], url: &str) -> Option<Confirm> {
    let risky: Vec<&Ref> = refs.iter().filter(|r| r.needs_confirm()).collect();
    if risky.is_empty() {
        return None;
    }
    let dest = host_of(url);
    let names = risky
        .iter()
        .map(|r| r.render())
        .collect::<Vec<_>>()
        .join(", ");
    Some(Confirm {
        token: format!("{names}@{dest}"),
        text: format!(
            "This provider will send the value of {names} (for a file: its content) to {dest} with its requests, and Test connection does the same. Confirm that this is the right variable or file and the right host."
        ),
    })
}

/// The click-through the form needs before it may be saved: `None` when every reference is a `SYNT_*`
/// variable. It is asked again whenever a reference or the host changes (the token differs).
pub fn confirmation(d: &Draft, ctx: &Ctx<'_>) -> Result<Option<Confirm>, String> {
    let p = parse_form(d, ctx)?;
    let refs: Vec<Ref> = p
        .key
        .into_iter()
        .chain(p.headers.into_iter().map(|(_, r)| r))
        .collect();
    Ok(confirm_of(&refs, &p.url))
}

/// What Test connection of the saved `id` sends, checked again (a hand-edited file may hold a name or a path
/// the form would refuse). `needs_click`: show `text` and wait for a click first.
pub struct Preflight {
    pub text: String,
    pub needs_click: bool,
}

pub fn preflight(id: &str, ctx: &Ctx<'_>) -> Result<Preflight, String> {
    let root = read(ctx.home)?;
    let e = group_of(&root, id)
        .and_then(|g| root[g].get(id))
        .ok_or_else(|| "this provider is not in your synt.json".to_string())?;
    let url = e
        .get("baseURL")
        .and_then(Value::as_str)
        .map(redact_url)
        .unwrap_or_default();
    let mut refs = vec![];
    let mut literal = false;
    for v in std::iter::once(e.get("apiKey")).chain(
        e.get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|h| h.values().map(Some)),
    ) {
        match v.and_then(Value::as_str) {
            Some(s) if is_ref(s) => refs.push(parse_ref(s, ctx)?),
            Some(_) => literal = true,
            None => {}
        }
    }
    let shown: Vec<String> = refs
        .iter()
        .map(|r| match r {
            Ref::Env(n) => format!("the value of {{env:{n}}}"),
            Ref::File(p) => format!("the content of file {p}"),
        })
        .chain(literal.then(|| "the key written in synt.json".to_string()))
        .collect();
    let text = if shown.is_empty() {
        format!("Sends no credential to {}.", san(&url))
    } else {
        format!(
            "Sends {} to {}.",
            san(&shown.join(", ")),
            san(&host_of(&url))
        )
    };
    Ok(Preflight {
        needs_click: literal || refs.iter().any(Ref::needs_confirm),
        text,
    })
}

fn path_of(home: &Path) -> std::path::PathBuf {
    home.join("synt.json")
}

fn read(home: &Path) -> Result<Value, String> {
    Ok(synthesis_settings::read_json(&path_of(home), "synt.json")?.unwrap_or_else(|| json!({})))
}

/// `provider` wins over `providers` on an id collision (the loader's order), so it is looked up first.
fn group_of(root: &Value, id: &str) -> Option<&'static str> {
    ["provider", "providers"]
        .into_iter()
        .find(|g| root.get(g).and_then(|m| m.get(id)).is_some())
}

fn redact_url(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(mut p) => {
            let _ = p.set_username("");
            let _ = p.set_password(None);
            p.set_query(None);
            p.set_fragment(None);
            p.to_string()
        }
        Err(_) => "(not a valid URL)".into(),
    }
}

fn is_ref(s: &str) -> bool {
    (s.starts_with("{env:") || s.starts_with("{file:")) && s.ends_with('}')
}

fn protocol_of(e: &Value) -> Protocol {
    match e.get("type").and_then(Value::as_str) {
        Some("openai") => Protocol::OpenAi,
        _ => Protocol::Anthropic,
    }
}

fn model_ids(e: &Value) -> Vec<String> {
    match e.get("models") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        _ => vec![],
    }
}

/// Providers of the user file, in file order. `Err`: the file exists but cannot be used.
pub fn entries(home: &Path) -> Result<Vec<Entry>, String> {
    let root = read(home)?;
    let mut out = vec![];
    for g in ["providers", "provider"] {
        for (id, e) in root.get(g).and_then(Value::as_object).into_iter().flatten() {
            if out.iter().any(|o: &Entry| &o.id == id) {
                continue;
            }
            let key = match e.get("apiKey") {
                Some(Value::String(s)) if is_ref(s) => s.clone(),
                Some(Value::String(_)) => "literal in file (hidden)".into(),
                _ => "none".into(),
            };
            out.push(Entry {
                id: san(id),
                protocol: protocol_of(e).as_str(),
                base_url: e
                    .get("baseURL")
                    .and_then(Value::as_str)
                    .map(|u| san(&redact_url(u)))
                    .unwrap_or_default(),
                key: san(&key),
                models: model_ids(e).iter().map(|m| san(m)).collect(),
            });
        }
    }
    Ok(out)
}

/// The form for editing `id`. A key or header value that is not a reference is NOT copied
/// (it would be shown): the field stays empty and saving replaces it.
pub fn draft_of(home: &Path, id: &str) -> Result<Draft, String> {
    let root = read(home)?;
    let e = group_of(&root, id)
        .and_then(|g| root[g].get(id))
        .ok_or_else(|| "this provider is not in your synt.json".to_string())?;
    let api_key = match e.get("apiKey") {
        Some(Value::String(s)) if is_ref(s) => s
            .strip_prefix("{env:")
            .and_then(|r| r.strip_suffix('}'))
            .unwrap_or(s)
            .to_string(),
        _ => String::new(),
    };
    let headers = e
        .get("headers")
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| Some(format!("{k}: {}", v.as_str().filter(|s| is_ref(s))?)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let tok = |v: Option<&Value>| {
        v.and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default()
    };
    let models = match e.get("models") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(mid, m)| {
                let effort = m
                    .pointer("/capabilities/effort_tiers")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                let name = m.get("name").and_then(Value::as_str).unwrap_or_default();
                format!(
                    "{mid} | {name} | {} | {} | {effort}",
                    tok(m.pointer("/limit/context")),
                    tok(m.pointer("/limit/output"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => model_ids(e).join("\n"),
    };
    Ok(Draft {
        id: id.into(),
        protocol: protocol_of(e),
        base_url: e
            .get("baseURL")
            .and_then(Value::as_str)
            .map(|u| san(&redact_url(u)))
            .unwrap_or_default(),
        api_key: san(&api_key),
        headers: san(&headers),
        models: san(&models),
        confirmed: String::new(),
    })
}

/// Validates `d` and writes `providers.<id>` (creating `synt.json` with `$schema` when it is new).
/// `editing`: the id must already be there (and only its managed fields change); otherwise it must not be.
pub fn save(ctx: &Ctx<'_>, d: &Draft, editing: bool) -> Result<(), String> {
    let home = ctx.home;
    let id = d.id.trim();
    let p = parse_form(d, ctx)?;
    let refs: Vec<Ref> = p
        .key
        .iter()
        .cloned()
        .chain(p.headers.iter().map(|(_, r)| r.clone()))
        .collect();
    if let Some(c) = confirm_of(&refs, &p.url)
        && d.confirmed != c.token
    {
        return Err(format!("needs confirmation: {}", c.text));
    }
    let url = p.url;
    let key = p.key.map(|r| r.render());
    let headers: Map<String, Value> = p
        .headers
        .into_iter()
        .map(|(k, r)| (k, Value::String(r.render())))
        .collect();
    let models = parse_models(&d.models)?;
    let no = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    synthesis_settings::edit_json_atomic(&path_of(home), "synt.json", |root| {
        let top = root
            .as_object_mut()
            .ok_or_else(|| no("synt.json is not an object".into()))?;
        if top.is_empty() {
            top.insert("$schema".into(), json!(SCHEMA_V1));
        }
        let group = group_of(&Value::Object(top.clone()), id);
        match (editing, group) {
            (false, Some(_)) => {
                return Err(no(format!(
                    "provider {id} already exists in your synt.json; edit it instead"
                )));
            }
            (true, None) => return Err(no(format!("provider {id} is not in your synt.json"))),
            _ => {}
        }
        let group = group.unwrap_or("providers");
        let map = top
            .entry(group)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| no(format!("\"{group}\" in synt.json is not an object")))?;
        let entry = map
            .entry(id)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| no(format!("the entry of {id} is not an object")))?;
        entry.insert("type".into(), json!(d.protocol.as_str()));
        entry.insert("baseURL".into(), json!(url));
        // a top-level apiKey wins over options.apiKey in the loader; no key = explicit "none"
        entry.insert(
            "apiKey".into(),
            key.clone().map_or(json!(false), Value::String),
        );
        if headers.is_empty() {
            entry.shift_remove("headers");
        } else {
            entry.insert("headers".into(), Value::Object(headers.clone()));
        }
        if models.is_empty() {
            entry.shift_remove("models");
        } else {
            let old = match entry.get("models") {
                Some(Value::Object(o)) => o.clone(),
                _ => Map::new(),
            };
            let mut new = Map::new();
            for m in &models {
                // fields the form does not manage (vendor, quality, ...) stay
                let mut o = old
                    .get(&m.id)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                set_or_drop(&mut o, "name", m.name.as_ref().map(|n| json!(n)));
                let mut limit = sub(&o, "limit");
                set_or_drop(&mut limit, "context", m.context.map(|n| json!(n)));
                set_or_drop(&mut limit, "output", m.output.map(|n| json!(n)));
                put_unless_empty(&mut o, "limit", limit);
                let mut caps = sub(&o, "capabilities");
                set_or_drop(
                    &mut caps,
                    "effort_tiers",
                    (!m.effort.is_empty()).then(|| json!(m.effort)),
                );
                put_unless_empty(&mut o, "capabilities", caps);
                new.insert(m.id.clone(), Value::Object(o));
            }
            entry.insert("models".into(), Value::Object(new));
        }
        // the loader's own verdict: never leave a file it would refuse
        validate_versioned_config(root, "synt.json").map_err(|e| no(e.to_string()))
    })
    .map_err(|e| e.to_string())
}

fn sub(o: &Map<String, Value>, k: &str) -> Map<String, Value> {
    o.get(k)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn set_or_drop(o: &mut Map<String, Value>, k: &str, v: Option<Value>) {
    match v {
        Some(v) => {
            o.insert(k.into(), v);
        }
        None => {
            o.shift_remove(k);
        }
    }
}

fn put_unless_empty(o: &mut Map<String, Value>, k: &str, v: Map<String, Value>) {
    if v.is_empty() {
        o.shift_remove(k);
    } else {
        o.insert(k.into(), Value::Object(v));
    }
}

/// Removes `providers.<id>` (or `provider.<id>`) and nothing else.
pub fn remove(home: &Path, id: &str) -> Result<(), String> {
    let no = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    synthesis_settings::edit_json_atomic(&path_of(home), "synt.json", |root| {
        let g = group_of(root, id)
            .ok_or_else(|| no(format!("provider {id} is not in your synt.json")))?;
        if let Some(m) = root.get_mut(g).and_then(Value::as_object_mut) {
            m.shift_remove(id);
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(home, user_home)` inside one temp dir.
    fn dirs(t: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let r = std::fs::canonicalize(t.path()).unwrap();
        let (h, u) = (r.join("home"), r.join("user"));
        std::fs::create_dir_all(&h).unwrap();
        std::fs::create_dir_all(&u).unwrap();
        (h, u)
    }

    fn d(id: &str) -> Draft {
        Draft {
            id: id.into(),
            protocol: Protocol::OpenAi,
            base_url: "https://llm.example.com/v1".into(),
            api_key: "SYNT_ACME_KEY".into(),
            models: "m1 | Model One | 128k | 8k | low,high\nm2".into(),
            ..Default::default()
        }
    }

    fn file(h: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(h.join("synt.json")).unwrap()).unwrap()
    }

    #[test]
    fn names() {
        for ok in ["acme", "a1", "my-llm_2.x"] {
            assert!(validate_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "A",
            "-a",
            "a/b",
            "a b",
            &"a".repeat(41),
            "anthropic",
            "openai",
        ] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls() {
        for ok in [
            "https://x.example/v1",
            "http://localhost:11434/v1",
            "http://127.0.0.1:1/v1",
            "http://[::1]:8080",
            "HTTP://LOCALHOST:1/v1",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok}");
        }
        assert_eq!(
            validate_base_url("HTTPS://X.Example/v1").unwrap(),
            "https://x.example/v1"
        );
        for bad in [
            "http://x.example/v1",
            "HTTP://x.example/v1",
            "ftp://x.example",
            "https://u:p@x.example",
            "https://u@x.example",
            "https://x.example/v1?key=abc",
            "https://x.example/#a",
            "x.example",
            "http://localhost.evil.com",
            "",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn internal_addresses_get_a_note_not_a_refusal() {
        for w in [
            "https://10.0.0.5/v1",
            "https://172.16.0.1",
            "https://192.168.1.1",
            "https://169.254.169.254/latest",
            "https://[fc00::1]/",
            "https://[fe80::1]/",
        ] {
            assert!(url_warning(w).is_some(), "{w}");
            assert!(validate_base_url(w).is_ok(), "{w}");
        }
        for ok in [
            "https://llm.example.com",
            "https://8.8.8.8",
            "http://127.0.0.1:1",
            "http://[::1]:1",
        ] {
            assert!(url_warning(ok).is_none(), "{ok}");
        }
    }

    #[test]
    fn env_references_synt_free_others_confirmed_cloud_credentials_refused() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        assert_eq!(
            parse_key_ref("SYNT_ACME_KEY", &c).unwrap(),
            "{env:SYNT_ACME_KEY}"
        );
        assert_eq!(parse_key_ref(" {env:MY_KEY} ", &c).unwrap(), "{env:MY_KEY}");
        for denied in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "NPM_TOKEN",
            "SSH_AUTH_SOCK",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AZURE_CLIENT_SECRET",
            "{env:AWS_SECRET_ACCESS_KEY}",
        ] {
            let e = parse_key_ref(denied, &c).unwrap_err();
            assert!(e.contains("another tool"), "{denied}: {e}");
        }
        // ANTHROPIC_ / OPENAI_ names are allowed, with the confirmation
        let mut f = d("acme");
        f.api_key = "OPENAI_API_KEY".into();
        let need = confirmation(&f, &c)
            .unwrap()
            .expect("needs a click-through");
        assert!(
            need.text.contains("{env:OPENAI_API_KEY}")
                && need.text.contains("https://llm.example.com")
        );
        assert!(
            save(&c, &f, false)
                .unwrap_err()
                .starts_with("needs confirmation")
        );
        assert!(!h.join("synt.json").exists());
        // the token the person accepted lets it through, once
        f.confirmed = need.token.clone();
        save(&c, &f, false).unwrap();
        // a change of host or of variable asks again
        let mut g = f.clone();
        g.id = "acme2".into();
        g.base_url = "https://other.example.com/v1".into();
        assert_ne!(confirmation(&g, &c).unwrap().unwrap().token, need.token);
        assert!(
            save(&c, &g, false)
                .unwrap_err()
                .starts_with("needs confirmation")
        );
        let mut k = f.clone();
        k.id = "acme3".into();
        k.api_key = "MY_OTHER_KEY".into();
        assert!(save(&c, &k, false).is_err());
        // SYNT_ names need none
        assert!(confirmation(&d("x"), &c).unwrap().is_none());
    }

    #[test]
    fn file_references_are_confined() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        std::fs::create_dir_all(h.join("keys")).unwrap();
        std::fs::write(h.join("keys/acme"), "k").unwrap();
        std::fs::create_dir_all(u.join(".ssh")).unwrap();
        std::fs::write(u.join(".ssh/id_ed25519"), "k").unwrap();
        std::fs::create_dir_all(u.join("secrets")).unwrap();
        std::fs::write(u.join("secrets/acme"), "k").unwrap();
        std::fs::write(u.join("secrets/big"), vec![b'x'; 5000]).unwrap();
        let ok = |p: &str| parse_key_ref(p, &c).unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(ok("./keys/acme"), "{file:./keys/acme}");
        assert_eq!(ok("~/secrets/acme"), "{file:~/secrets/acme}");
        assert_eq!(ok("{file:~/secrets/not-yet}"), "{file:~/secrets/not-yet}");
        for bad in [
            "~/.ssh/id_ed25519",
            "~/.aws/credentials",
            "~/secrets/../.ssh/id_ed25519",
            "../x",
            "~/../etc/passwd",
            "/etc/passwd",
            "/proc/self/environ",
            "//host/share/k",
            "\\\\host\\share\\k",
            "\\\\?\\C:\\k",
            "keys/acme",
            "~",
            "./chain.key",
            "./trust.key",
            "./run/x",
            "./synt.json",
            "~/secrets/big",
            "~/secrets",
            "{file:}",
        ] {
            assert!(parse_key_ref(bad, &c).is_err(), "{bad}");
        }
        // a link out of an allowed place is judged by where it lands
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(u.join(".ssh/id_ed25519"), u.join("secrets/link")).unwrap();
            assert!(parse_key_ref("~/secrets/link", &c).is_err());
        }
        // every file reference is confirmed
        let mut f = d("acme");
        f.api_key = "./keys/acme".into();
        assert!(confirmation(&f, &c).unwrap().is_some());
    }

    #[test]
    fn raw_keys_are_refused_without_being_echoed() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        for raw in [
            "sk-ant-api03-abcdefghijklmnop",
            "SK-ANT-API03-ABCDEFGHIJKLMNOP",
            "sk_test_abcdefghijklmnop",
            "sk_live_abcdefghijklmnop",
            "gsk_abcdefghijkl",
            "xai-abcdefghijkl",
            "pplx-abcdefghijkl",
            "hf_AbCdEfGhIjKlMn",
            "ya29.a0AbCdEfGhIj",
            "SG.abcdefghijkl.mnop",
            "AccountKey=abcdefghijkl",
            "AKIAABCDEFGHIJKLMNOP",
            "ASIAABCDEFGHIJKLMNOP",
            "github_pat_11ABCDEFG",
            "glpat-abcdefghijkl",
            "xoxb-123456789012",
            "ghp_abcdefghijkl",
            "eyJhbGciOiJIUzI1NiJ9",
            // base64 with a slash is not a path
            "abcDEF123/ghiJKL456+mnoPQR789==",
            // all caps / digits, no snake shape
            "ABCDEF1234567890ABCDEF12",
            "0123456789abcdef0123456789abcdef",
            // snake-shaped but too long, a long segment, or mostly digits
            "A_BCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZ_ABCDEFGHIJKL",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ_X",
            "A_1234567890123456",
            "my key",
            "{env:lower}",
            "{env:}",
            "{file:sk-ant-api03-abcdefghij}",
        ] {
            let e = parse_key_ref(raw, &c).unwrap_err();
            assert!(
                !e.contains(raw.trim_start_matches("{env:").trim_start_matches("{file:")),
                "{raw} echoed: {e}"
            );
        }
        assert!(parse_key_ref("", &c).is_err());
    }

    #[test]
    fn headers_are_vetted_and_never_echoed() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        let ok = parse_headers("X-Org: SYNT_ORG_ID\n\nX-Tok: ~/t", false, &c).unwrap();
        assert_eq!(ok[0].1.render(), "{env:SYNT_ORG_ID}");
        assert_eq!(ok[1].1.render(), "{file:~/t}");
        for bad in [
            "Host: SYNT_A_B",
            "authorization: SYNT_A_B",
            "Proxy-Authorization: SYNT_A_B",
            "COOKIE: SYNT_A_B",
            "Content-Length: SYNT_A_B",
            "Transfer-Encoding: SYNT_A_B",
            "Connection: SYNT_A_B",
            "TE: SYNT_A_B",
            "Upgrade: SYNT_A_B",
            "X-A: SYNT_A_B\nx-a: SYNT_C_D",
            "X-Org: sk-abcdefghijklmnop",
            "no colon",
            "Bad Name: SYNT_A_B",
        ] {
            let e = parse_headers(bad, false, &c).unwrap_err();
            assert!(!e.contains("abcdefghij"), "{bad}: {e}");
        }
        // x-api-key duplicates the key header only when a key is set
        assert!(parse_headers("X-Api-Key: SYNT_A_B", true, &c).is_err());
        assert!(parse_headers("X-Api-Key: SYNT_A_B", false, &c).is_ok());
    }

    #[test]
    fn models() {
        let m = parse_models("a | A | 1m | 64k | low,max\nb/c:1").unwrap();
        assert_eq!((m[0].context, m[0].output), (Some(1_000_000), Some(64_000)));
        assert_eq!(m[1].id, "b/c:1");
        for bad in [
            "a\na",
            "a | | 5",
            "a | | | | turbo",
            "a | | | | low,low",
            "bad id",
            "a | b | c | d | low | x",
            "sk-abcdefghijklmnop",
            "a | sk-abcdefghijklmnop",
        ] {
            let e = parse_models(bad).unwrap_err();
            assert!(!e.contains("abcdefghij"), "{bad}: {e}");
        }
    }

    #[test]
    fn save_creates_and_keeps_other_keys_and_extra_fields() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        save(&c, &d("acme"), false).unwrap();
        let f = file(&h);
        assert_eq!(f["$schema"], SCHEMA_V1);
        let e = &f["providers"]["acme"];
        assert_eq!(
            (&e["type"], &e["apiKey"]),
            (&json!("openai"), &json!("{env:SYNT_ACME_KEY}"))
        );
        assert_eq!(
            e["models"]["m1"]["limit"],
            json!({"context": 128000, "output": 8000})
        );
        assert_eq!(
            e["models"]["m1"]["capabilities"]["effort_tiers"],
            json!(["low", "high"])
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(h.join("synt.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(
            h.join("synt.json"),
            r#"{"$schema":"config.json/v1","model":"zed/a","providers":{"zed":{"type":"openai","baseURL":"https://z.example","apiKey":"{env:SYNT_Z_KEY}","discover":false,"models":{"a":{"vendor":"v","name":"old"}}},"mid":{"type":"openai"}},"mcpServers":{}}"#,
        )
        .unwrap();
        let mut z = draft_of(&h, "zed").unwrap();
        assert_eq!(
            (z.api_key.as_str(), z.models.as_str()),
            ("SYNT_Z_KEY", "a | old |  |  | ")
        );
        z.models = "a | new\nb".into();
        save(&c, &z, true).unwrap();
        let f = file(&h);
        assert_eq!(
            f.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["$schema", "model", "providers", "mcpServers"]
        );
        assert_eq!(
            f["providers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["zed", "mid"]
        );
        assert_eq!(f["providers"]["zed"]["discover"], false);
        assert_eq!(
            f["providers"]["zed"]["models"]["a"],
            json!({"vendor": "v", "name": "new"})
        );
        assert!(
            save(&c, &d("zed"), false)
                .unwrap_err()
                .contains("already exists")
        );
        assert!(save(&c, &d("nope"), true).is_err());
        remove(&h, "zed").unwrap();
        assert!(remove(&h, "zed").is_err());
        assert_eq!(
            entries(&h)
                .unwrap()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["mid"]
        );
    }

    #[test]
    fn a_literal_key_in_the_file_is_never_shown_and_hand_edited_text_is_sanitised() {
        let t = tempfile::tempdir().unwrap();
        let (h, _) = dirs(&t);
        std::fs::write(
            h.join("synt.json"),
            "{\"$schema\":\"config.json/v1\",\"providers\":{\"p\":{\"type\":\"openai\",\"baseURL\":\"https://u:pw@p.example/v1?k=1\",\"apiKey\":\"sk-secretsecretsecret\",\"headers\":{\"X\":\"hunter2\"},\"models\":[\"m\\u001b[31mred\"]}}}",
        )
        .unwrap();
        let e = entries(&h).unwrap();
        let draft = draft_of(&h, "p").unwrap();
        let shown = format!("{e:?}{draft:?}");
        for s in ["sk-secret", "pw", "k=1", "hunter2", "\u{1b}"] {
            assert!(!shown.contains(s), "{s:?} leaked: {shown}");
        }
    }

    #[test]
    fn a_malformed_or_invalid_file_is_kept_untouched() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        for bad in [
            "{\"providers\": {",
            "[]",
            r#"{"$schema":"config.json/v1","bogus":1}"#,
        ] {
            std::fs::write(h.join("synt.json"), bad).unwrap();
            assert!(save(&c, &d("acme"), false).is_err(), "{bad}");
            assert!(remove(&h, "acme").is_err());
            assert_eq!(std::fs::read_to_string(h.join("synt.json")).unwrap(), bad);
            assert_eq!(entries(&h).is_err(), !bad.contains("bogus"));
        }
    }

    #[test]
    fn a_keyless_remote_provider_is_refused_but_local_or_header_auth_is_fine() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        let mut p = d("acme");
        p.api_key = String::new();
        assert!(save(&c, &p, false).unwrap_err().contains("API key"));
        p.headers = "X-Auth: SYNT_ACME_TOKEN".into();
        save(&c, &p, false).unwrap();
        assert_eq!(file(&h)["providers"]["acme"]["apiKey"], false);
        let mut l = d("local");
        l.api_key = String::new();
        l.base_url = "HTTP://localhost:11434/v1".into();
        save(&c, &l, false).unwrap();
    }

    #[test]
    fn the_test_preflight_names_what_is_sent_and_refuses_hand_edited_secrets_paths() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
        };
        save(&c, &d("acme"), false).unwrap();
        let p = preflight("acme", &c).unwrap();
        assert!(!p.needs_click);
        assert!(
            p.text.contains("{env:SYNT_ACME_KEY}") && p.text.contains("https://llm.example.com"),
            "{}",
            p.text
        );
        let edit = |key: &str| {
            std::fs::write(
                h.join("synt.json"),
                format!(r#"{{"$schema":"config.json/v1","providers":{{"x":{{"type":"openai","baseURL":"https://evil.example/v1","apiKey":"{key}"}}}}}}"#),
            )
            .unwrap();
            preflight("x", &c)
        };
        let p = edit("{env:MY_KEY}").unwrap();
        assert!(p.needs_click && p.text.contains("https://evil.example"));
        assert!(edit("{env:AWS_SECRET_ACCESS_KEY}").is_err());
        assert!(edit("{file:~/.ssh/id_rsa}").is_err());
        assert!(edit("{file:\\\\\\\\host\\\\s}").is_err());
        assert!(edit("{file:/etc/passwd}").is_err());
        let lit = edit("sk-literal-literal-literal").unwrap();
        assert!(
            lit.needs_click && !lit.text.contains("literal-literal"),
            "{}",
            lit.text
        );
    }
}
