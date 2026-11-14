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
    /// The environment the engine will run with (a flat `SYNT_PROVIDER` override is reported from it).
    pub env: &'a synthesis_config::Env,
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
    /// The path as written and the real path it resolves to now (what a confirmation names).
    File(String, String),
}

impl Ref {
    fn render(&self) -> String {
        match self {
            Ref::Env(n) => format!("{{env:{n}}}"),
            Ref::File(p, _) => format!("{{file:{p}}}"),
        }
    }
    /// What the click-through and the pre-flight name.
    fn shown(&self) -> String {
        match self {
            Ref::Env(n) => format!("the value of {{env:{n}}}"),
            Ref::File(p, real) if p == real => format!("the content of file {p}"),
            Ref::File(p, real) => format!("the content of file {p} (resolves to {real})"),
        }
    }
    /// Needs the person's click-through: any file, any variable that is not the product's own.
    fn needs_confirm(&self) -> bool {
        match self {
            Ref::Env(n) => !n.starts_with("SYNT_"),
            Ref::File(..) => true,
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
    const PREFIXES: [&str; 33] = [
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
        "HOMEBREW_GITHUB_",
        "OP_",
        "CARGO_REGISTRY_",
        "TWINE_",
        "NETLIFY_",
        "VERCEL_",
        "CLOUDFLARE_",
        "SENTRY_",
        "PYPI_",
        "LINODE_",
        "FLY_",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || n.starts_with("RAILWAY_") || n == "DATABASE_URL"
}

/// The product's own credential variables: a custom provider must not share the vendor key (that would send
/// it to another host), so the form refuses them. Other `SYNT_*` names (`SYNT_MYGW_KEY`) are free.
fn env_vendor(n: &str) -> bool {
    matches!(n, "SYNT_API_KEY" | "SYNT_AUTH_TOKEN" | "SYNT_TOKEN")
        || n.starts_with("SYNT_ANTHROPIC_")
        || n.starts_with("SYNT_OPENAI_")
}

/// A `{file:}` path a key may be read from: absolute, `~/` or `./` (relative to the synt home), no `..`, no
/// UNC or device path, nothing that is another tool's secret store or a system or state path; under the
/// synt home only `keys/` (see `synthesis_config::check_credential_file`, which the loader applies again at
/// use time). Returns the real path.
fn check_file(path: &str, ctx: &Ctx<'_>) -> Result<std::path::PathBuf, String> {
    let p = path.replace('\\', "/");
    if p.starts_with("//") {
        return Err("file path: network and device paths are refused".into());
    }
    if p.split('/').any(|c| c == "..") {
        return Err("file path: no .. in the path".into());
    }
    let full = if let Some(r) = p.strip_prefix("~/") {
        ctx.user_home.join(r)
    } else if let Some(r) = p.strip_prefix("./") {
        ctx.home.join(r)
    } else if std::path::Path::new(path).is_absolute() || p.as_bytes().get(1) == Some(&b':') {
        std::path::PathBuf::from(path)
    } else {
        return Err(
            "file path: use an absolute path, ~/... or ./... (relative to the synt home)".into(),
        );
    };
    synthesis_config::check_credential_file(&full, ctx.home, ctx.user_home, true)
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
        if env_vendor(n) {
            return Err("API key: SYNT_API_KEY, SYNT_AUTH_TOKEN and the vendor variables are the product's own key and are never shared with a custom provider; use a variable of its own (for example SYNT_MYGW_KEY)".into());
        }
        Ok(Ref::Env(n.to_string()))
    };
    let file = |p: &str| {
        check_file(p, ctx).map(|real| Ref::File(p.to_string(), real.to_string_lossy().into_owned()))
    };
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

fn confirm_of(refs: &[Ref], url: &str, protocol: Protocol) -> Option<Confirm> {
    let risky: Vec<&Ref> = refs.iter().filter(|r| r.needs_confirm()).collect();
    if risky.is_empty() {
        return None;
    }
    let dest = host_of(url);
    let what = risky
        .iter()
        .map(|r| r.shown())
        .collect::<Vec<_>>()
        .join(", ");
    let proto = protocol.as_str();
    Some(Confirm {
        token: format!("{what}@{dest}/{proto}"),
        text: format!(
            "This provider will send {what} to {dest} ({proto} protocol) with its requests, and Test connection does the same. Confirm that this is the right variable or file and the right host."
        ),
    })
}

/// The click-through the form needs before it may be saved: `None` when every reference is a `SYNT_*`
/// variable. It is asked again whenever a reference, the real path of a file, the host or the protocol
/// changes (the token differs).
pub fn confirmation(d: &Draft, ctx: &Ctx<'_>) -> Result<Option<Confirm>, String> {
    let p = parse_form(d, ctx)?;
    let refs: Vec<Ref> = p
        .key
        .into_iter()
        .chain(p.headers.into_iter().map(|(_, r)| r))
        .collect();
    Ok(confirm_of(&refs, &p.url, d.protocol))
}

/// What Test connection of the saved `id` sends, from what the LOADER will use: `apiKey`, `authToken`,
/// `options.apiKey`, `options.authToken`, `headers`, `options.headers`, and a flat `SYNT_PROVIDER` override.
/// Each is checked again (a hand-edited file may hold what the form refuses). `needs_click`: show `text`
/// and wait for a click; the click authorises `hash` (sources, real file paths, host, protocol), which the
/// worker checks again before it sends.
pub struct Preflight {
    pub text: String,
    pub needs_click: bool,
    pub hash: String,
}

/// `top-level k` if set, else `options.k` (the loader's `rawv(k).or(opt(k))`).
fn eff<'a>(e: &'a Value, k: &str) -> Option<&'a Value> {
    e.get(k)
        .filter(|v| !v.is_null())
        .or_else(|| e.pointer(&format!("/options/{k}")).filter(|v| !v.is_null()))
}

pub fn preflight(id: &str, ctx: &Ctx<'_>) -> Result<Preflight, String> {
    use sha2::{Digest, Sha256};
    let root = read(ctx.home)?;
    let e = group_of(&root, id)
        .and_then(|g| root[g].get(id))
        .ok_or_else(|| "this provider is not in your synt.json".to_string())?;
    let url = eff(e, "baseURL")
        .and_then(Value::as_str)
        .map(redact_url)
        .unwrap_or_default();
    let protocol = protocol_of(id, e);
    let mut refs: Vec<Ref> = vec![];
    let mut literal = false;
    let mut note = |v: Option<&Value>, refs: &mut Vec<Ref>| -> Result<(), String> {
        match v.and_then(Value::as_str) {
            Some(s) if is_ref(s) => refs.push(parse_ref(s, ctx)?),
            Some(s) if !s.is_empty() => literal = true,
            _ => {}
        }
        Ok(())
    };
    // every place the loader reads a credential or a header value from, shadowed ones too
    for k in ["apiKey", "authToken"] {
        note(e.get(k), &mut refs)?;
        note(e.pointer(&format!("/options/{k}")), &mut refs)?;
    }
    for hs in [e.get("headers"), e.pointer("/options/headers")] {
        for v in hs
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|h| h.values())
        {
            note(Some(v), &mut refs)?;
        }
    }
    // SYNT_PROVIDER naming this provider hands it the flat credential (it wins over the configured key)
    let names_it = ctx
        .env
        .get_nonempty("SYNT_PROVIDER")
        .is_some_and(|p| p.eq_ignore_ascii_case(id));
    let mut shown: Vec<String> = refs.iter().map(Ref::shown).collect();
    if literal {
        shown.push("the key written in synt.json".into());
    }
    let mut flat = false;
    if names_it {
        for n in ["SYNT_AUTH_TOKEN", "SYNT_API_KEY"] {
            if ctx.env.get_nonempty(n).is_some() {
                shown.push(format!("the value of {{env:{n}}} (SYNT_PROVIDER names this provider, so it is used instead of the configured key)"));
                flat = true;
            }
        }
        if ctx.env.get_nonempty("SYNT_CUSTOM_HEADERS").is_some() {
            shown.push("the headers of SYNT_CUSTOM_HEADERS".into());
            flat = true;
        }
    }
    shown.dedup();
    let dest = host_of(&url);
    let text = if shown.is_empty() {
        format!(
            "No key or header value is configured; sends nothing secret to {}.",
            san(&dest)
        )
    } else {
        format!(
            "Sends {} to {} ({} protocol).",
            san(&shown.join(", ")),
            san(&dest),
            protocol.as_str()
        )
    };
    let mut sorted = shown.clone();
    sorted.sort();
    let hash = Sha256::digest(
        format!("{sorted:?}|{literal}|{flat}|{url}|{}", protocol.as_str()).as_bytes(),
    )
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
    Ok(Preflight {
        needs_click: literal || flat || refs.iter().any(Ref::needs_confirm),
        text,
        hash,
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

/// A reference that is safe to show: `{env:NAME}` / `{file:path}` whose inside is not a key typed in its place.
fn shown_ref(s: &str) -> Option<String> {
    let inner = s
        .strip_prefix("{env:")
        .or_else(|| s.strip_prefix("{file:"))?
        .strip_suffix('}')?;
    (!looks_like_key(inner)).then(|| san(s))
}

fn is_ref(s: &str) -> bool {
    (s.starts_with("{env:") || s.starts_with("{file:")) && s.ends_with('}')
}

/// The protocol the loader will use (explicit `type`, then `npm`, then the id and the URL).
fn protocol_of(id: &str, e: &Value) -> Protocol {
    let text = |k: &str| e.get(k).and_then(Value::as_str);
    synthesis_config::detect_protocol(synthesis_config::DetectInput {
        id: Some(id),
        explicit_type: text("type"),
        npm: text("npm"),
        base_url: eff(e, "baseURL").and_then(Value::as_str),
        credential: None,
    })
    .0
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
            let key = match eff(e, "apiKey") {
                Some(Value::String(s)) if is_ref(s) => {
                    shown_ref(s).unwrap_or_else(|| "literal in file (hidden)".into())
                }
                Some(Value::String(_)) => "literal in file (hidden)".into(),
                _ => "none".into(),
            };
            out.push(Entry {
                id: san(id),
                protocol: protocol_of(id, e).as_str(),
                base_url: eff(e, "baseURL")
                    .and_then(Value::as_str)
                    .map(|u| san(&redact_url(u)))
                    .unwrap_or_default(),
                key,
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
    let api_key = match eff(e, "apiKey") {
        Some(Value::String(s)) => shown_ref(s)
            .map(|s| {
                s.strip_prefix("{env:")
                    .and_then(|r| r.strip_suffix('}'))
                    .unwrap_or(&s)
                    .to_string()
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let headers = e
        .get("headers")
        .filter(|h| h.is_object())
        .or_else(|| e.pointer("/options/headers"))
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .filter(|(k, _)| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                .filter_map(|(k, v)| Some(format!("{k}: {}", shown_ref(v.as_str()?)?)))
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
        protocol: protocol_of(id, e),
        base_url: eff(e, "baseURL")
            .and_then(Value::as_str)
            .map(|u| san(&redact_url(u)))
            .unwrap_or_default(),
        api_key,
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
    if let Some(c) = confirm_of(&refs, &p.url, d.protocol)
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
        // an authToken is a second credential the form does not manage (and it beats the key)
        for tok in [entry.get("authToken"), entry.get("options").and_then(|o| o.get("authToken"))] {
            if tok.is_some_and(|t| !t.is_null()) {
                return Err(no("this provider has an authToken in synt.json; the form manages apiKey only. Remove the authToken there first".into()));
            }
        }
        // the form's fields win over `options.*`: drop the shadowed copies so no other credential, header or
        // host is left behind them
        if let Some(Value::Object(o)) = entry.get_mut("options") {
            for k in ["apiKey", "headers", "baseURL"] {
                o.shift_remove(k);
            }
            if o.is_empty() {
                entry.shift_remove("options");
            }
        }
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
    use synthesis_config::Env;

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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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
            env: &Env::default(),
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

    fn put(h: &Path, entry: Value) {
        std::fs::write(
            h.join("synt.json"),
            json!({"$schema": "config.json/v1", "providers": {"x": entry}}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn the_preflight_covers_every_place_the_loader_reads_a_credential() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        let base = || json!({"type": "openai", "baseURL": "https://x.example/v1"});
        let with = |extra: Value| {
            let mut e = base();
            e.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            put(&h, e);
            preflight("x", &c)
        };
        // nothing configured: it says so
        let none = with(json!({})).unwrap();
        assert!(
            !none.needs_click && none.text.contains("nothing secret"),
            "{}",
            none.text
        );
        // each field, top-level or under options, is named and needs the click for a non-SYNT variable
        for e in [
            json!({"apiKey": "{env:MY_KEY}"}),
            json!({"authToken": "{env:MY_KEY}"}),
            json!({"options": {"apiKey": "{env:MY_KEY}"}}),
            json!({"options": {"authToken": "{env:MY_KEY}"}}),
            json!({"headers": {"X-A": "{env:MY_KEY}"}}),
            json!({"options": {"headers": {"X-A": "{env:MY_KEY}"}}}),
            json!({"apiKey": "SYNT_OK_KEY", "options": {"apiKey": "{env:MY_KEY}"}}),
        ] {
            let p = with(e.clone()).unwrap();
            assert!(
                p.needs_click
                    && p.text.contains("{env:MY_KEY}")
                    && !p.text.contains("nothing secret"),
                "{e}: {}",
                p.text
            );
        }
        // a denied variable in any of them refuses the test, a literal needs the click without being shown
        for e in [
            json!({"options": {"authToken": "{env:AWS_SECRET_ACCESS_KEY}"}}),
            json!({"options": {"headers": {"X": "{file:~/.ssh/id_rsa}"}}}),
            json!({"authToken": "{env:SYNT_API_KEY}"}),
        ] {
            assert!(with(e.clone()).is_err(), "{e}");
        }
        let lit = with(json!({"options": {"apiKey": "sk-literal-literal-literal"}})).unwrap();
        assert!(lit.needs_click && !lit.text.contains("literal-literal"));
        // SYNT_PROVIDER naming it: the flat key goes there, and it is said
        let env = Env::from_pairs([
            ("SYNT_PROVIDER", "X"),
            ("SYNT_API_KEY", "v"),
            ("SYNT_CUSTOM_HEADERS", "A: b"),
        ]);
        let c2 = Ctx {
            home: &h,
            user_home: &u,
            env: &env,
        };
        put(
            &h,
            json!({"type": "openai", "baseURL": "https://x.example/v1", "apiKey": "{env:SYNT_X_KEY}"}),
        );
        let p = preflight("x", &c2).unwrap();
        assert!(
            p.needs_click
                && p.text.contains("{env:SYNT_API_KEY}")
                && p.text.contains("SYNT_CUSTOM_HEADERS"),
            "{}",
            p.text
        );
        assert!(!preflight("x", &c).unwrap().needs_click);
    }

    #[test]
    fn the_hash_follows_the_host_the_protocol_and_the_real_file() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        std::fs::create_dir_all(u.join("s")).unwrap();
        std::fs::write(u.join("s/a"), "k").unwrap();
        std::fs::write(u.join("s/b"), "k").unwrap();
        let hash = |e: Value| {
            put(&h, e);
            preflight("x", &c).unwrap().hash
        };
        let e = |url: &str, ty: &str, key: &str| json!({"type": ty, "baseURL": url, "apiKey": key});
        let a = hash(e("https://a.example", "openai", "{env:MY_KEY}"));
        assert_eq!(a, hash(e("https://a.example", "openai", "{env:MY_KEY}")));
        for other in [
            e("https://b.example", "openai", "{env:MY_KEY}"),
            e("https://a.example", "anthropic", "{env:MY_KEY}"),
            e("https://a.example", "openai", "{env:MY_OTHER_KEY}"),
        ] {
            assert_ne!(a, hash(other.clone()), "{other}");
        }
        // a path that is swapped for a link to another file changes the real path, so the hash
        #[cfg(unix)]
        {
            let f = hash(e("https://a.example", "openai", "{file:~/s/a}"));
            std::fs::remove_file(u.join("s/a")).unwrap();
            std::os::unix::fs::symlink(u.join("s/b"), u.join("s/a")).unwrap();
            let g = hash(e("https://a.example", "openai", "{file:~/s/a}"));
            assert_ne!(f, g);
            // and the confirmation names where it resolves to
            let mut d = d("acme");
            d.api_key = "~/s/a".into();
            let text = confirmation(&d, &c).unwrap().unwrap().text;
            assert!(
                text.contains("resolves to") && text.contains("s/b"),
                "{text}"
            );
        }
    }

    #[test]
    fn the_products_own_key_is_never_shared_but_other_synt_names_are_free() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        for v in [
            "SYNT_API_KEY",
            "SYNT_AUTH_TOKEN",
            "SYNT_TOKEN",
            "SYNT_ANTHROPIC_API_KEY",
            "SYNT_OPENAI_API_KEY",
            "{env:SYNT_API_KEY}",
        ] {
            let e = parse_key_ref(v, &c).unwrap_err();
            assert!(e.contains("never shared"), "{v}: {e}");
        }
        for v in ["SYNT_MYGW_KEY", "SYNT_MYGW_API_KEY"] {
            assert!(parse_key_ref(v, &c).is_ok(), "{v}");
            assert!(!Ref::Env(v.into()).needs_confirm());
        }
    }

    #[test]
    fn more_secret_stores_and_variables_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        for n in [
            "HOMEBREW_GITHUB_API_TOKEN",
            "OP_SERVICE_ACCOUNT_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "TWINE_PASSWORD",
            "NETLIFY_AUTH_TOKEN",
            "VERCEL_TOKEN",
            "CLOUDFLARE_API_TOKEN",
            "SENTRY_AUTH_TOKEN",
            "PYPI_TOKEN",
            "DOCKER_PASSWORD",
            "TF_TOKEN_X",
            "HEROKU_API_KEY",
            "DIGITALOCEAN_TOKEN",
            "LINODE_TOKEN",
            "FLY_API_TOKEN",
            "RAILWAY_TOKEN",
        ] {
            assert!(parse_key_ref(n, &c).is_err(), "{n}");
        }
        for d in [
            ".config/gh/hosts.yml",
            ".cargo/credentials.toml",
            ".cargo/credentials",
            ".pypirc",
            ".azure/x",
            ".oci/config",
            ".password-store/x",
            "Library/Keychains/login.keychain-db",
            ".npmrc",
            ".netrc",
            ".git-credentials",
            ".docker/config.json",
        ] {
            let p = u.join(d);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "k").unwrap();
            assert!(parse_key_ref(&format!("~/{d}"), &c).is_err(), "{d}");
        }
        for p in [
            "/var/root/k",
            "/home/someone-else/k",
            "/Users/someone-else/k",
            "/etc/k",
        ] {
            assert!(parse_key_ref(p, &c).is_err(), "{p}");
        }
    }

    #[test]
    fn saving_clears_shadowed_options_and_refuses_a_second_credential() {
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        put(
            &h,
            json!({"type": "openai", "options": {"baseURL": "https://old.example/v1", "apiKey": "{env:OLD_KEY}", "headers": {"X-Old": "{env:OLD_HDR}"}, "authHeader": "bearer"}}),
        );
        let mut f = d("x");
        f.base_url = "https://new.example/v1".into();
        save(&c, &f, true).unwrap();
        let e = &file(&h)["providers"]["x"];
        assert_eq!(e["options"], json!({"authHeader": "bearer"}));
        assert_eq!(
            (&e["baseURL"], &e["apiKey"]),
            (
                &json!("https://new.example/v1"),
                &json!("{env:SYNT_ACME_KEY}")
            )
        );
        // an authToken (top-level or under options) is refused, the file stays
        for tok in [
            json!({"authToken": "{env:T_K}"}),
            json!({"options": {"authToken": "{env:T_K}"}}),
        ] {
            let mut entry = json!({"type": "openai", "baseURL": "https://x.example/v1", "apiKey": "{env:SYNT_X_KEY}"});
            entry
                .as_object_mut()
                .unwrap()
                .extend(tok.as_object().unwrap().clone());
            put(&h, entry);
            let before = std::fs::read_to_string(h.join("synt.json")).unwrap();
            assert!(save(&c, &d("x"), true).unwrap_err().contains("authToken"));
            assert_eq!(
                std::fs::read_to_string(h.join("synt.json")).unwrap(),
                before
            );
        }
    }

    #[test]
    fn documented_gateway_setups_list_edit_and_keep_working() {
        // docs/INSTALL.md: npm openai-compatible, options.baseURL, options.apiKey {env:...}
        let t = tempfile::tempdir().unwrap();
        let (h, u) = dirs(&t);
        let c = Ctx {
            home: &h,
            user_home: &u,
            env: &Env::default(),
        };
        put(
            &h,
            json!({"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "https://gateway.example.com/v1", "apiKey": "{env:MY_GATEWAY_KEY}"}, "models": {"my-model": {"capabilities": {"toolCalling": true}}}}),
        );
        let e = &entries(&h).unwrap()[0];
        assert_eq!(
            (e.protocol, e.base_url.as_str(), e.key.as_str()),
            (
                "openai",
                "https://gateway.example.com/v1",
                "{env:MY_GATEWAY_KEY}"
            )
        );
        let mut dr = draft_of(&h, "x").unwrap();
        assert_eq!(
            (dr.api_key.as_str(), dr.base_url.as_str(), dr.protocol),
            (
                "MY_GATEWAY_KEY",
                "https://gateway.example.com/v1",
                Protocol::OpenAi
            )
        );
        let p = preflight("x", &c).unwrap();
        assert!(
            p.needs_click
                && p.text.contains("{env:MY_GATEWAY_KEY}")
                && p.text.contains("openai protocol"),
            "{}",
            p.text
        );
        // saving it as it is asks for the same confirmation, then keeps the model's own fields
        dr.confirmed = confirmation(&dr, &c).unwrap().unwrap().token;
        save(&c, &dr, true).unwrap();
        let f = &file(&h)["providers"]["x"];
        assert_eq!(
            f["models"]["my-model"]["capabilities"],
            json!({"toolCalling": true})
        );
        assert_eq!(f["apiKey"], "{env:MY_GATEWAY_KEY}");
    }

    #[test]
    fn a_key_typed_inside_a_reference_is_not_shown_by_the_list_or_the_form() {
        let t = tempfile::tempdir().unwrap();
        let (h, _) = dirs(&t);
        put(
            &h,
            json!({"type": "openai", "baseURL": "https://x.example/v1", "apiKey": "{env:sk-ant-api03-abcdefghijkl}", "headers": {"X-A": "{file:sk-ant-api03-abcdefghijkl}"}}),
        );
        let shown = format!("{:?}{:?}", entries(&h).unwrap(), draft_of(&h, "x").unwrap());
        assert!(!shown.contains("abcdefghij"), "{shown}");
    }
}
