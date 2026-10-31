//! First-run onboarding logic (docs/specs/desktop.md section 4), without any UI: validation of
//! what the user typed, the `synthesis.json` write (no key in it), and the connection test.
//! The key itself goes to the [`crate::credentials::CredentialStore`], never into a file.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_config::{
    Credential, CredentialKind, Env, Protocol, Provider, TypeSource, validate_versioned_config,
};
use synthesis_providers::{ErrorKind, NetSettings, ProviderSpec, RetryPolicy};
use tokio_util::sync::CancellationToken;

use crate::credentials::Secret;

/// The two wire protocols a provider entry can speak (`type` in `synthesis.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Anthropic,
    OpenAi,
}

impl Kind {
    pub fn protocol(self) -> Protocol {
        match self {
            Kind::Anthropic => Protocol::Anthropic,
            Kind::OpenAi => Protocol::OpenAi,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Anthropic => "Anthropic-compatible",
            Kind::OpenAi => "OpenAI-compatible",
        }
    }

    pub fn default_id(self) -> &'static str {
        self.protocol().as_str()
    }

    /// Prefilled base URL (the official one).
    pub fn default_url(self) -> &'static str {
        match self {
            Kind::Anthropic => "https://api.anthropic.com",
            Kind::OpenAi => "https://api.openai.com/v1",
        }
    }
}

/// Why the form cannot be submitted. The texts are fixed: they never repeat what was typed, so a
/// key pasted into the wrong field cannot end up on screen or in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    UrlInvalid,
    UrlNotHttps,
    UrlHasCredentials,
    IdInvalid,
    KeyEmpty,
    KeyInvalid,
}

impl Problem {
    pub fn text(self) -> &'static str {
        match self {
            Problem::UrlInvalid => "The base URL is not a valid web address.",
            Problem::UrlNotHttps => {
                "The base URL must start with https:// (http:// only for localhost)."
            }
            Problem::UrlHasCredentials => {
                "The base URL must not contain a user, password or query."
            }
            Problem::IdInvalid => "The provider id must be 1-32 characters: a-z, 0-9, - or _.",
            Problem::KeyEmpty => "Enter the API key.",
            Problem::KeyInvalid => "The API key must be one line without spaces.",
        }
    }
}

pub struct CheckedUrl {
    pub url: String,
    pub warning: Option<&'static str>,
}

fn is_loopback(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(a) => a.is_loopback(),
        url::Host::Ipv6(a) => a.is_loopback(),
    }
}

/// `https://` always; `http://` only for loopback, with a warning.
pub fn check_base_url(s: &str) -> Result<CheckedUrl, Problem> {
    let u = url::Url::parse(s.trim()).map_err(|_| Problem::UrlInvalid)?;
    let host = u.host().ok_or(Problem::UrlInvalid)?;
    if !u.username().is_empty() || u.password().is_some() || u.query().is_some() {
        return Err(Problem::UrlHasCredentials);
    }
    let warning = match u.scheme() {
        "https" => None,
        "http" if is_loopback(&host) => {
            Some("Plain http: traffic is not encrypted (localhost only).")
        }
        _ => return Err(Problem::UrlNotHttps),
    };
    Ok(CheckedUrl {
        url: u.as_str().trim_end_matches('/').to_string(),
        warning,
    })
}

pub fn check_id(s: &str) -> Result<&str, Problem> {
    let ok = (1..=32).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    ok.then_some(s).ok_or(Problem::IdInvalid)
}

pub fn check_key(s: &str) -> Result<Secret, Problem> {
    let k = s.trim();
    if k.is_empty() {
        return Err(Problem::KeyEmpty);
    }
    if k.len() > 4096 || k.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Problem::KeyInvalid);
    }
    Ok(Secret::new(k))
}

/// A form that passed every check.
pub struct Valid {
    pub kind: Kind,
    pub id: String,
    pub url: CheckedUrl,
    pub key: Secret,
}

pub fn validate(kind: Kind, id: &str, base_url: &str, key: &str) -> Result<Valid, Problem> {
    Ok(Valid {
        kind,
        id: check_id(id.trim())?.to_string(),
        url: check_base_url(base_url)?,
        key: check_key(key)?,
    })
}

/// Adds `providers.<id> = {type, baseURL}` (and `model` as `<id>/<model>`) to
/// `<home>/synthesis.json` with config.json/v1 keys only: never `apiKey`. Other keys survive; the
/// result is validated by the ported schema before the atomic rename, and a malformed file is
/// never overwritten.
pub fn write_config(
    home: &Path,
    id: &str,
    kind: Kind,
    base_url: &str,
    model: Option<&str>,
) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    synthesis_settings::edit_json_atomic(&home.join("synthesis.json"), "user config", |v| {
        let root = v
            .as_object_mut()
            .ok_or_else(|| bad("config is not an object"))?;
        root.entry("$schema")
            .or_insert_with(|| Value::String(synthesis_config::SCHEMA_V1.into()));
        let providers = root
            .entry("providers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| bad("`providers` is not an object"))?;
        let entry = providers
            .entry(id)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| bad("the provider entry is not an object"))?;
        entry.insert("type".into(), kind.protocol().as_str().into());
        entry.insert("baseURL".into(), base_url.into());
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            root.insert("model".into(), format!("{id}/{m}").into());
        }
        validate_versioned_config(v, "synthesis.json").map_err(|e| bad(&e.to_string()))
    })
}

/// What the connection test found. Never carries a response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok { models: Vec<String> },
    AuthFailed(Option<u16>),
    Unreachable,
    Tls,
    Unexpected(Option<u16>),
}

impl Outcome {
    pub fn text(&self) -> String {
        match self {
            Outcome::Ok { models } => format!("Connected. {} models available.", models.len()),
            Outcome::AuthFailed(s) => format!("The server refused the key{}.", status(*s)),
            Outcome::Unreachable => {
                "Could not reach the server. Check the URL and your network.".into()
            }
            Outcome::Tls => "The TLS certificate was rejected.".into(),
            Outcome::Unexpected(s) => {
                format!(
                    "The server answered unexpectedly{}; the key is not verified.",
                    status(*s)
                )
            }
        }
    }
}

fn status(s: Option<u16>) -> String {
    s.map_or(String::new(), |s| format!(" (status {s})"))
}

/// Lists the models with the typed key through `synthesis-providers` (proxy and CA settings come
/// from `env`). Needs a Tokio runtime.
pub async fn test_connection(env: &Env, v: &Valid) -> Outcome {
    let provider = Provider {
        protocol: v.kind.protocol(),
        type_source: TypeSource::Explicit,
        base_url: Some(v.url.url.clone()),
        ..Provider::default()
    };
    let cred = Credential {
        kind: CredentialKind::Api,
        secret: v.key.expose().to_string(),
        source: "desktop session".into(),
    };
    let net = NetSettings {
        connect_timeout: Duration::from_secs(10),
        ..NetSettings::default()
    };
    let retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let client = match ProviderSpec::from_config(&v.id, &provider, Some(&cred))
        .and_then(|s| s.connect(env, &net, retry))
    {
        Ok(c) => c,
        Err(_) => return Outcome::Unexpected(None),
    };
    let cancel = CancellationToken::new();
    let listed = tokio::time::timeout(Duration::from_secs(20), client.list_models(&cancel)).await;
    match listed {
        Err(_) => Outcome::Unreachable,
        Ok(Ok(models)) => Outcome::Ok {
            models: models
                .iter()
                .filter_map(|m| m.id.as_str().map(String::from))
                .collect(),
        },
        Ok(Err(e)) => match e.kind {
            ErrorKind::Auth => Outcome::AuthFailed(e.status),
            ErrorKind::Network => {
                let m = e.message().to_lowercase();
                if ["certificate", "tls", "ssl"].iter().any(|w| m.contains(w)) {
                    Outcome::Tls
                } else {
                    Outcome::Unreachable
                }
            }
            _ => Outcome::Unexpected(e.status),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn url_rules() {
        let ok = check_base_url(" https://gateway.example.com/v1/ ").unwrap();
        assert_eq!(ok.url, "https://gateway.example.com/v1");
        assert!(ok.warning.is_none());
        for u in [
            "http://localhost:8080",
            "http://127.0.0.1:1234",
            "http://[::1]:9",
        ] {
            assert!(check_base_url(u).unwrap().warning.is_some(), "{u}");
        }
        assert_eq!(
            check_base_url("http://example.com").err(),
            Some(Problem::UrlNotHttps)
        );
        assert_eq!(
            check_base_url("ftp://example.com").err(),
            Some(Problem::UrlNotHttps)
        );
        assert_eq!(check_base_url("not a url").err(), Some(Problem::UrlInvalid));
        assert_eq!(
            check_base_url("https://u:p@example.com").err(),
            Some(Problem::UrlHasCredentials)
        );
        assert_eq!(
            check_base_url("https://example.com/?key=abc").err(),
            Some(Problem::UrlHasCredentials)
        );
    }

    #[test]
    fn id_and_key_rules() {
        assert!(check_id("gateway-2").is_ok());
        for bad in ["", "-x", "Up", "a b", &"a".repeat(33)] {
            assert_eq!(check_id(bad).err(), Some(Problem::IdInvalid), "{bad:?}");
        }
        assert_eq!(check_key("  ").err(), Some(Problem::KeyEmpty));
        assert_eq!(check_key("sk a").err(), Some(Problem::KeyInvalid));
        assert_eq!(check_key("sk\nb").err(), Some(Problem::KeyInvalid));
        assert_eq!(check_key("  sk-abc ").unwrap().expose(), "sk-abc");
    }

    #[test]
    fn problems_never_echo_input() {
        let secret = "sk-sentinel with space";
        let p = validate(Kind::OpenAi, "x", "https://example.com", secret)
            .err()
            .unwrap();
        assert!(!p.text().contains("sentinel"));
    }

    #[test]
    fn config_is_schema_valid_has_no_key_and_keeps_other_keys() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("synthesis.json");
        std::fs::write(
            &f,
            r#"{"$schema":"config.json/v1","providers":{"other":{"type":"openai","baseURL":"https://o.example/v1"}}}"#,
        )
        .unwrap();
        write_config(
            d.path(),
            "gateway",
            Kind::Anthropic,
            "https://g.example.com",
            Some("m1"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        validate_versioned_config(&v, "synthesis.json").unwrap();
        assert_eq!(v["providers"]["gateway"]["type"], "anthropic");
        assert_eq!(
            v["providers"]["gateway"]["baseURL"],
            "https://g.example.com"
        );
        assert_eq!(v["providers"]["other"]["type"], "openai");
        assert_eq!(v["model"], "gateway/m1");
        assert!(!text.to_lowercase().contains("apikey"));
    }

    #[test]
    fn config_creates_the_file_and_refuses_a_malformed_one() {
        let d = tempfile::tempdir().unwrap();
        write_config(
            d.path(),
            "anthropic",
            Kind::Anthropic,
            "https://api.anthropic.com",
            None,
        )
        .unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("synthesis.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["$schema"], "config.json/v1");
        assert!(v.get("model").is_none());

        let bad = tempfile::tempdir().unwrap();
        std::fs::write(bad.path().join("synthesis.json"), "{ nope").unwrap();
        let e = write_config(bad.path(), "x", Kind::OpenAi, "https://x.example", None).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(bad.path().join("synthesis.json")).unwrap(),
            "{ nope"
        );
    }

    /// One-thread HTTP server: 200 with a model list when the request carries `good`, else 401.
    fn fake_server() -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let (code, body) = if req.contains("good") {
                    ("200 OK", r#"{"data":[{"id":"m1"},{"id":"m2"}]}"#)
                } else {
                    (
                        "401 Unauthorized",
                        r#"{"error":{"type":"authentication_error","message":"bad key"}}"#,
                    )
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn connection_ok_and_auth_failed_against_a_fake_server() {
        let url = fake_server();
        let env = Env::from_pairs::<&str, &str>([]);
        for kind in [Kind::Anthropic, Kind::OpenAi] {
            let ok = validate(kind, "gateway", &url, "good-key").unwrap();
            assert_eq!(
                test_connection(&env, &ok).await,
                Outcome::Ok {
                    models: vec!["m1".into(), "m2".into()]
                },
                "{kind:?}"
            );
            let bad = validate(kind, "gateway", &url, "wrong-key").unwrap();
            assert_eq!(
                test_connection(&env, &bad).await,
                Outcome::AuthFailed(Some(401)),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn connection_refused_is_unreachable() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        drop(l);
        let v = validate(Kind::OpenAi, "x", &url, "k").unwrap();
        let env = Env::from_pairs::<&str, &str>([]);
        assert_eq!(test_connection(&env, &v).await, Outcome::Unreachable);
    }
}
