//! Custom providers of the USER `synt.json` for a settings UI: validate a form, write
//! `providers.<id>` atomically (other keys, their order and the entry's unmanaged fields stay),
//! list and remove. ADR-011: a credential is only ever a `{env:NAME}` / `{file:path}` reference;
//! input that looks like a key is refused and nothing here reads, returns or logs a key value.

use std::path::Path;

use serde_json::{Map, Value, json};
use synthesis_config::{EFFORT_LEVELS, Protocol, SCHEMA_V1, validate_versioned_config};

use crate::models::parse_tokens;

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

const MAX_HEADERS: usize = 8;
const MAX_MODELS: usize = 64;

pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 40
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err("provider name: 1-40 of a-z 0-9 - _ . , starting with a letter or digit".into())
    }
}

/// https, or http for a loopback host only; no userinfo, query or fragment (they can carry a key).
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let u = url::Url::parse(raw).map_err(|_| "base URL: not a valid URL".to_string())?;
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
    Ok(raw.to_string())
}

/// Text that is a key itself (the same families the `synt mcp add` guard refuses).
fn looks_like_key(s: &str) -> bool {
    const PREFIXES: [&str; 14] = [
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "glpat-",
        "npm_",
        "sk_live_",
        "xoxa-",
        "xoxs-",
        "eyJ",
    ];
    PREFIXES
        .iter()
        .any(|p| s.starts_with(p) && s.len() >= p.len() + 8)
}

const KEY_HELP: &str = "give the NAME of an environment variable that holds the key (for example MY_KEY) or a file path";

fn env_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && n.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && n.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `MY_KEY` -> `{env:MY_KEY}`, `~/keys/x` or `/run/x` -> `{file:...}`; a ready `{env:}` / `{file:}`
/// passes. A key typed by mistake is refused with a pointer to an environment variable, and no
/// error text repeats the input.
pub fn parse_key_ref(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(format!("API key: {KEY_HELP}"));
    }
    if looks_like_key(s) {
        return Err(format!(
            "that looks like an API key itself: never type a key here; put it in an environment variable and {KEY_HELP}"
        ));
    }
    let path_ok = |p: &str| !p.is_empty() && !p.contains('}') && !p.chars().any(char::is_control);
    if let Some(n) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        return env_name(n).then(|| s.to_string()).ok_or_else(|| {
            format!("API key: {{env:...}} needs an upper-case variable name; {KEY_HELP}")
        });
    }
    if let Some(p) = s.strip_prefix("{file:").and_then(|r| r.strip_suffix('}')) {
        return path_ok(p)
            .then(|| s.to_string())
            .ok_or_else(|| format!("API key: bad {{file:...}} path; {KEY_HELP}"));
    }
    if env_name(s) {
        return Ok(format!("{{env:{s}}}"));
    }
    if (s.contains('/') || s.contains('\\') || s.starts_with('~')) && path_ok(s) {
        return Ok(format!("{{file:{s}}}"));
    }
    Err(format!(
        "API key: not an environment variable name (upper case, digits, _) or a file path; never type a key here: {KEY_HELP}"
    ))
}

fn parse_headers(text: &str) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (name, val) = line
            .split_once(':')
            .ok_or_else(|| "a header is written Name: VARIABLE".to_string())?;
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("header name: letters, digits and - only".into());
        }
        // a header value is a reference too, as the key is
        let val = parse_key_ref(val).map_err(|e| format!("header {name}: {e}"))?;
        out.insert(name.to_string(), Value::String(val));
    }
    if out.len() > MAX_HEADERS {
        return Err(format!("at most {MAX_HEADERS} headers"));
    }
    Ok(out)
}

/// One parsed model line.
struct ModelLine {
    id: String,
    name: Option<String>,
    context: Option<u64>,
    output: Option<u64>,
    effort: Vec<String>,
}

fn parse_models(text: &str) -> Result<Vec<ModelLine>, String> {
    let mut out: Vec<ModelLine> = vec![];
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        let id = f[0];
        let id_ok = !id.is_empty()
            && id.len() <= 128
            && id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+')
            });
        if !id_ok {
            return Err(format!(
                "model id \"{}\": letters, digits and . _ - : / @ + only",
                id.chars().take(40).collect::<String>()
            ));
        }
        if out.iter().any(|m| m.id == id) {
            return Err(format!("model {id} is listed twice"));
        }
        if f.len() > 5 {
            return Err(format!(
                "model {id}: at most 5 fields (id | name | context | max output | effort)"
            ));
        }
        let tokens = |i: usize, what: &str| -> Result<Option<u64>, String> {
            f.get(i)
                .filter(|s| !s.is_empty())
                .map(|s| parse_tokens(s).map_err(|e| format!("model {id} {what}: {e}")))
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
        if let Some(bad) = effort.iter().find(|l| !EFFORT_LEVELS.contains(&l.as_str())) {
            return Err(format!(
                "model {id}: effort \"{}\" is not one of {}",
                bad.chars().take(20).collect::<String>(),
                EFFORT_LEVELS.join(", ")
            ));
        }
        if effort
            .iter()
            .enumerate()
            .any(|(i, l)| effort[..i].contains(l))
        {
            return Err(format!("model {id}: effort levels must not repeat"));
        }
        out.push(ModelLine {
            id: id.into(),
            name: f.get(1).filter(|s| !s.is_empty()).map(|s| s.to_string()),
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
                id: id.clone(),
                protocol: protocol_of(e).as_str(),
                base_url: e
                    .get("baseURL")
                    .and_then(Value::as_str)
                    .map(redact_url)
                    .unwrap_or_default(),
                key,
                models: model_ids(e),
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
        .ok_or_else(|| format!("provider {id} is not in your synt.json"))?;
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
            .map(redact_url)
            .unwrap_or_default(),
        api_key,
        headers,
        models,
    })
}

/// Validates `d` and writes `providers.<id>` (creating `synt.json` with `$schema` when it is new).
/// `editing`: the id must already be there (and only its managed fields change); otherwise it must not be.
pub fn save(home: &Path, d: &Draft, editing: bool) -> Result<(), String> {
    let id = d.id.trim();
    validate_id(id)?;
    let url = validate_base_url(&d.base_url)?;
    let headers = parse_headers(&d.headers)?;
    let key = if d.api_key.trim().is_empty() {
        // no key: a local server, or a gateway authenticated by a header
        if !headers.is_empty() || url.starts_with("http:") {
            None
        } else {
            return Err(format!("API key: {KEY_HELP}"));
        }
    } else {
        Some(parse_key_ref(&d.api_key)?)
    };
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

    fn d(id: &str) -> Draft {
        Draft {
            id: id.into(),
            protocol: Protocol::OpenAi,
            base_url: "https://llm.example.com/v1".into(),
            api_key: "MY_KEY".into(),
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
        for bad in ["", "A", "-a", "a/b", "a b", &"a".repeat(41)] {
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
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://x.example/v1",
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
    fn key_references_and_raw_keys() {
        assert_eq!(parse_key_ref("MY_KEY").unwrap(), "{env:MY_KEY}");
        assert_eq!(parse_key_ref(" {env:A_B} ").unwrap(), "{env:A_B}");
        assert_eq!(parse_key_ref("~/keys/acme").unwrap(), "{file:~/keys/acme}");
        assert_eq!(
            parse_key_ref("C:\\keys\\acme").unwrap(),
            "{file:C:\\keys\\acme}"
        );
        assert_eq!(parse_key_ref("{file:/run/k}").unwrap(), "{file:/run/k}");
        for raw in [
            "sk-ant-api03-abcdefghijklmnop",
            "sk-1234567890abcd",
            "ghp_abcdefghijkl",
            "eyJhbGciOiJIUzI1NiJ9",
            "abc123def456",
            "my key",
            "{env:lower}",
            "{env:}",
        ] {
            let e = parse_key_ref(raw).unwrap_err();
            assert!(!e.contains(raw), "the error must not echo the input: {e}");
        }
        assert!(parse_key_ref("").is_err());
    }

    #[test]
    fn headers_and_models() {
        let h = parse_headers("X-Org: ORG_ID\n\nX-Tok: ~/t").unwrap();
        assert_eq!(h["X-Org"], "{env:ORG_ID}");
        assert_eq!(h["X-Tok"], "{file:~/t}");
        assert!(parse_headers("X-Org: sk-abcdefghijklmnop").is_err());
        assert!(parse_headers("no colon").is_err());
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
        ] {
            assert!(parse_models(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn save_creates_and_keeps_other_keys_and_extra_fields() {
        let t = tempfile::tempdir().unwrap();
        save(t.path(), &d("acme"), false).unwrap();
        let f = file(t.path());
        assert_eq!(f["$schema"], SCHEMA_V1);
        let e = &f["providers"]["acme"];
        assert_eq!(
            (&e["type"], &e["apiKey"]),
            (&json!("openai"), &json!("{env:MY_KEY}"))
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
            let mode = std::fs::metadata(t.path().join("synt.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // a second provider and the entry's own extras
        std::fs::write(
            t.path().join("synt.json"),
            r#"{"$schema":"config.json/v1","model":"zed/a","providers":{"zed":{"type":"openai","baseURL":"https://z.example","apiKey":"{env:Z}","discover":false,"models":{"a":{"vendor":"v","name":"old"}}},"mid":{"type":"openai"}},"mcpServers":{}}"#,
        )
        .unwrap();
        let mut z = draft_of(t.path(), "zed").unwrap();
        assert_eq!(
            (z.api_key.as_str(), z.models.as_str()),
            ("Z", "a | old |  |  | ")
        );
        z.models = "a | new\nb".into();
        save(t.path(), &z, true).unwrap();
        let f = file(t.path());
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
        // new vs edit
        assert!(
            save(t.path(), &d("zed"), false)
                .unwrap_err()
                .contains("already exists")
        );
        assert!(save(t.path(), &d("nope"), true).is_err());
        // remove
        remove(t.path(), "zed").unwrap();
        assert!(remove(t.path(), "zed").is_err());
        assert_eq!(
            entries(t.path())
                .unwrap()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["mid"]
        );
    }

    #[test]
    fn a_literal_key_in_the_file_is_never_shown() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join("synt.json"),
            r#"{"$schema":"config.json/v1","providers":{"p":{"type":"openai","baseURL":"https://u:pw@p.example/v1?k=1","apiKey":"sk-secretsecretsecret","headers":{"X":"hunter2"}}}}"#,
        )
        .unwrap();
        let e = entries(t.path()).unwrap();
        let draft = draft_of(t.path(), "p").unwrap();
        let shown = format!("{e:?}{draft:?}");
        for s in ["sk-secret", "pw", "k=1", "hunter2"] {
            assert!(!shown.contains(s), "{s} leaked: {shown}");
        }
    }

    #[test]
    fn a_malformed_or_invalid_file_is_kept_untouched() {
        let t = tempfile::tempdir().unwrap();
        for bad in [
            "{\"providers\": {",
            "[]",
            r#"{"$schema":"config.json/v1","bogus":1}"#,
        ] {
            std::fs::write(t.path().join("synt.json"), bad).unwrap();
            assert!(save(t.path(), &d("acme"), false).is_err(), "{bad}");
            assert!(remove(t.path(), "acme").is_err());
            assert_eq!(
                std::fs::read_to_string(t.path().join("synt.json")).unwrap(),
                bad
            );
            // only an unreadable file is an error for the list; a schema problem shows when saving
            assert_eq!(entries(t.path()).is_err(), !bad.contains("bogus"));
        }
    }

    #[test]
    fn a_keyless_remote_provider_is_refused_but_local_or_header_auth_is_fine() {
        let t = tempfile::tempdir().unwrap();
        let mut p = d("acme");
        p.api_key = String::new();
        assert!(save(t.path(), &p, false).unwrap_err().contains("API key"));
        p.headers = "X-Auth: ACME_TOKEN".into();
        save(t.path(), &p, false).unwrap();
        assert_eq!(file(t.path())["providers"]["acme"]["apiKey"], false);
        let mut l = d("local");
        l.api_key = String::new();
        l.base_url = "http://localhost:11434/v1".into();
        save(t.path(), &l, false).unwrap();
    }
}
