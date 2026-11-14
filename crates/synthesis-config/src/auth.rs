//! Credential resolution (docs/specs/config.md §5, §8; ADR-011). Read-only: this
//! crate never writes credentials. Port of the read side of Node `auth.js`.

use crate::env::Env;
use crate::provider::{ApiKey, Provider};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// API key (`x-api-key` for Anthropic, Bearer for OpenAI).
    Api,
    /// Bearer-style token for a compatible gateway.
    Token,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub kind: CredentialKind,
    pub secret: String,
    /// e.g. `env SYNT_AUTH_TOKEN`, `config {env:X}`, `config inline`, `auth.json`.
    pub source: String,
}

impl Credential {
    pub fn masked(&self) -> String {
        mask(&self.secret)
    }
}

// Secrets must not reach logs or events: Debug shows the masked value only.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("kind", &self.kind)
            .field("secret", &self.masked())
            .field("source", &self.source)
            .finish()
    }
}

/// `abcd…wxyz` for values of 16+ chars; shorter ones show no characters at all.
pub fn mask(value: &str) -> String {
    let c: Vec<char> = value.chars().collect();
    match c.len() {
        0 => String::new(),
        1..=15 => "…".into(),
        n => format!(
            "{}…{}",
            c[..4].iter().collect::<String>(),
            c[n - 4..].iter().collect::<String>()
        ),
    }
}

/// Tier 1: `SYNT_AUTH_TOKEN` / `SYNT_API_KEY`, only for the `SYNT_PROVIDER` id
/// (default `anthropic`, case-insensitive). The token wins.
pub fn env_credential(id: &str, env: &Env) -> Option<Credential> {
    let target = env.get_nonempty("SYNT_PROVIDER").unwrap_or("anthropic");
    if id.to_lowercase() != target.to_lowercase() {
        return None;
    }
    let mk = |kind, name: &str| {
        env.get_nonempty(name).map(|v| Credential {
            kind,
            secret: v.to_string(),
            source: format!("env {name}"),
        })
    };
    mk(CredentialKind::Token, "SYNT_AUTH_TOKEN").or_else(|| mk(CredentialKind::Api, "SYNT_API_KEY"))
}

/// `SYNT_API_KEY` / `SYNT_AUTH_TOKEN` go to the provider named by `SYNT_PROVIDER`. When that is only the
/// implicit default (`anthropic`, `SYNT_PROVIDER` unset), the provider must still point at the vendor (no
/// `baseURL`, the vendor's host, or the `SYNT_BASE_URL` the user set): a `synt.json` entry that re-uses the
/// id with another URL would otherwise receive a key meant for the vendor.
fn env_may_target(provider: Option<&Provider>, env: &Env) -> bool {
    if env.get_nonempty("SYNT_PROVIDER").is_some() {
        return true;
    }
    let Some(url) = provider.and_then(|p| p.base_url.as_deref()) else {
        return true;
    };
    if env.get_nonempty("SYNT_BASE_URL") == Some(url) {
        return true;
    }
    url::Url::parse(url).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case("api.anthropic.com"))
    })
}

/// Why `SYNT_API_KEY` / `SYNT_AUTH_TOKEN` were not used for a provider they name by default.
pub const ENV_CREDENTIAL_BLOCKED: &str = "SYNT_API_KEY is only sent to the vendor URL; set SYNT_PROVIDER=<id> or SYNT_BASE_URL, or use a {env:NAME} apiKey";

/// True when the flat env credential exists for `id` but [`resolve_credential`] withholds it (see above).
pub fn env_credential_blocked(id: &str, provider: Option<&Provider>, env: &Env) -> bool {
    env_credential(id, env).is_some() && !env_may_target(provider, env)
}

/// Tier 3: legacy `<synthesis_home>/auth.json` written by the Node CLI. Read-only.
pub fn file_credential(id: &str, synthesis_home: &Path) -> Option<Credential> {
    let data: Value =
        serde_json::from_slice(&std::fs::read(synthesis_home.join("auth.json")).ok()?).ok()?;
    let entry = data.get(id)?;
    let text = |k: &str| {
        entry
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let mk = |kind, secret: &str| Credential {
        kind,
        secret: secret.to_string(),
        source: "auth.json".into(),
    };
    if entry.get("type").and_then(Value::as_str) == Some("token")
        && let Some(t) = text("token")
    {
        return Some(mk(CredentialKind::Token, t));
    }
    text("key").map(|k| mk(CredentialKind::Api, k))
}

/// env (gated to `SYNT_PROVIDER`) > provider config `authToken`/`apiKey` (already
/// substituted) > legacy `auth.json`.
pub fn resolve_credential(
    id: &str,
    provider: Option<&Provider>,
    synthesis_home: &Path,
    env: &Env,
) -> Option<Credential> {
    if let Some(c) = env_credential(id, env).filter(|_| env_may_target(provider, env)) {
        return Some(c);
    }
    if let Some(p) = provider {
        let src = |s: &Option<String>| format!("config {}", s.as_deref().unwrap_or("inline"));
        if let Some(t) = p.auth_token.as_deref().filter(|t| !t.is_empty()) {
            return Some(Credential {
                kind: CredentialKind::Token,
                secret: t.into(),
                source: src(&p.auth_token_source),
            });
        }
        if let Some(ApiKey::Value(k)) = p
            .api_key
            .as_ref()
            .filter(|k| !matches!(k, ApiKey::Value(v) if v.is_empty()))
        {
            return Some(Credential {
                kind: CredentialKind::Api,
                secret: k.clone(),
                source: src(&p.api_key_source),
            });
        }
    }
    file_credential(id, synthesis_home)
}
