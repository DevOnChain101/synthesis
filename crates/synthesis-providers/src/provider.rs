//! `ModelProvider` trait, provider construction from `synthesis_config` (credential and auth-header
//! decisions ported from Node `providers/index.js`) and model discovery.

use crate::capabilities::ProviderCapabilities;
use crate::error::{ErrorKind, ProviderError, Redaction};
use crate::http::{
    Endpoint, EventStream, Mapper, NetSettings, build_client, header, proxy_secrets, spawn_stream,
};
use crate::retry::RetryPolicy;
use crate::types::Request;
use crate::{anthropic, openai};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use synthesis_config::{
    Credential, CredentialKind, Env, ModelMeta, Protocol, Provider, TypeSource,
};
use tokio_util::sync::CancellationToken;

/// Secret string whose `Debug` never shows the value.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// How the credential goes on the wire (OpenAI always uses Bearer regardless).
#[derive(Debug, Clone)]
pub enum Auth {
    /// `x-api-key` (Anthropic).
    ApiKey(Secret),
    /// `Authorization: Bearer`.
    Bearer(Secret),
}

#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub id: String,
    pub protocol: Protocol,
    pub base_url: Option<String>,
    pub auth: Auth,
    pub headers: IndexMap<String, String>,
    pub models_meta: IndexMap<String, ModelMeta>,
}

/// Model entry from a discovery call (Node `normalizeModel`); serialized in Node's camelCase.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: Value,
    pub display_name: Value,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub capabilities: Map<String, Value>,
}

/// Tolerates the Anthropic shape and OpenAI-compatible gateway shapes.
pub fn normalize_model(m: &Value) -> ModelInfo {
    let id = m.get("id").cloned().unwrap_or(Value::Null);
    let num = |keys: &[&str]| {
        keys.iter()
            .filter_map(|k| m.get(*k).filter(|v| !v.is_null()))
            .find_map(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
    };
    let name = ["display_name", "displayName"]
        .iter()
        .filter_map(|k| m.get(*k).and_then(Value::as_str))
        .find(|s| !s.is_empty());
    ModelInfo {
        display_name: name.map_or_else(|| id.clone(), |s| Value::String(s.into())),
        id,
        context_window: num(&["context_length", "max_input_tokens", "context_window"]),
        max_output: num(&["max_output_tokens"]),
        capabilities: m
            .get("capabilities")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    }
}

pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;
    fn protocol(&self) -> Protocol;
    fn capabilities(&self, model: &str) -> ProviderCapabilities;
    /// Starts the request on a spawned task (needs a Tokio runtime) and returns its events.
    /// Dropping the stream or cancelling `cancel` aborts the request. Pre-stream failures are
    /// retried per policy; after the first byte the stream is never replayed.
    fn stream(&self, req: &Request, cancel: CancellationToken) -> EventStream;
    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>, ProviderError>>;
}

/// Node `anthropicUsesBearer`: explicit `authHeader` wins; otherwise the real Anthropic API
/// (id `anthropic`, an `*.anthropic.com` host or an `sk-ant-` key) uses `x-api-key` and
/// gateways use Bearer. The key is only prefix-checked.
pub fn anthropic_uses_bearer(
    provider_id: Option<&str>,
    auth_header: Option<&str>,
    base_url: Option<&str>,
    key: Option<&str>,
) -> bool {
    if let Some(h) = auth_header.filter(|h| !h.is_empty()) {
        return h == "bearer";
    }
    if provider_id == Some("anthropic") {
        return false;
    }
    if let Some(host) = base_url.and_then(url_host)
        && (host == "api.anthropic.com" || host.ends_with(".anthropic.com"))
    {
        return false;
    }
    !key.is_some_and(|k| k.starts_with("sk-ant-"))
}

fn url_host(u: &str) -> Option<String> {
    // Same host extraction as `new URL(u).hostname` for absolute http(s) URLs.
    let rest = u.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let host = auth.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_lowercase())
}

impl ProviderSpec {
    /// Port of Node `providerForId`. The caller resolves `cred` with
    /// `synthesis_config::resolve_credential` (SYNT_* env, config, auth.json); this crate never reads
    /// keys from the environment. `None` is accepted only for no-auth providers.
    pub fn from_config(
        id: &str,
        p: &Provider,
        cred: Option<&Credential>,
    ) -> Result<Self, ProviderError> {
        let mut protocol = p.protocol;
        let auth = if p.is_no_auth() {
            Auth::ApiKey(Secret::new("placeholder"))
        } else {
            let c = cred.ok_or_else(|| {
                ProviderError::new(
                    ErrorKind::Auth,
                    format!("No credential for provider \"{id}\". Set SYNT_API_KEY / SYNT_AUTH_TOKEN or the provider's apiKey in config."),
                )
            })?;
            // Nothing explicit decided the protocol and the key is an Anthropic one.
            if p.type_source == TypeSource::Default && c.secret.starts_with("sk-ant-") {
                protocol = Protocol::Anthropic;
            }
            let secret = Secret::new(c.secret.clone());
            let bearer = match c.kind {
                CredentialKind::Token => true,
                CredentialKind::Api => match p.auth_header.as_deref().filter(|h| !h.is_empty()) {
                    Some(h) => h == "bearer",
                    None => {
                        protocol == Protocol::Anthropic
                            && anthropic_uses_bearer(
                                Some(id),
                                None,
                                p.base_url.as_deref(),
                                Some(&c.secret),
                            )
                    }
                },
            };
            if bearer {
                Auth::Bearer(secret)
            } else {
                Auth::ApiKey(secret)
            }
        };
        Ok(Self {
            id: id.to_string(),
            protocol,
            base_url: p.base_url.clone(),
            auth,
            headers: p.headers.clone().unwrap_or_default(),
            models_meta: p.models_meta.clone().unwrap_or_default(),
        })
    }

    /// Builds the client. `env` supplies proxy and CA settings (see [`build_client`]).
    pub fn connect(
        self,
        env: &Env,
        net: &NetSettings,
        retry: RetryPolicy,
    ) -> Result<Arc<dyn ModelProvider>, ProviderError> {
        let secret = match &self.auth {
            Auth::ApiKey(s) | Auth::Bearer(s) => s.0.clone(),
        };
        let mut secrets = vec![secret.clone()];
        secrets.extend(proxy_secrets(env));
        secrets.extend(self.headers.values().filter(|v| v.len() >= 8).cloned());
        let redaction = Redaction::new(secrets);

        let mut headers = HeaderMap::new();
        let mut put = |h: (reqwest::header::HeaderName, reqwest::header::HeaderValue)| {
            headers.insert(h.0, h.1);
        };
        let bearer = || header("authorization", &format!("Bearer {secret}"), true);
        match (self.protocol, &self.auth) {
            (Protocol::Anthropic, Auth::ApiKey(_)) => put(header("x-api-key", &secret, true)?),
            _ => put(bearer()?),
        }
        if self.protocol == Protocol::Anthropic {
            put(header(
                "anthropic-version",
                anthropic::DEFAULT_VERSION,
                false,
            )?);
        }
        // User headers last: they may override anthropic-version or add anthropic-beta.
        for (k, v) in &self.headers {
            put(header(k, v, false)?);
        }
        let ep = Endpoint {
            client: build_client(env, net)?,
            headers,
            retry,
            redaction,
            idle_timeout: net.idle_timeout,
        };
        let base = match self.protocol {
            Protocol::Anthropic => anthropic::base_url(self.base_url.as_deref()),
            Protocol::OpenAi => openai::base_url(self.base_url.as_deref()),
        };
        Ok(Arc::new(HttpProvider {
            id: self.id,
            protocol: self.protocol,
            base: base.trim_end_matches('/').to_string(),
            ep: Arc::new(ep),
            models_meta: self.models_meta,
        }))
    }
}

struct HttpProvider {
    id: String,
    protocol: Protocol,
    base: String,
    ep: Arc<Endpoint>,
    models_meta: IndexMap<String, ModelMeta>,
}

impl HttpProvider {
    fn models_url(&self) -> String {
        match self.protocol {
            Protocol::Anthropic => format!("{}/v1/models", self.base),
            Protocol::OpenAi => format!("{}/models", self.base),
        }
    }

    async fn page(&self, url: &str, cancel: &CancellationToken) -> Result<Value, ProviderError> {
        let resp = self.ep.send(url, None, cancel).await?;
        let text = resp
            .text()
            .await
            .map_err(|e| self.ep.redaction.transport(e))?;
        serde_json::from_str(&text).map_err(|_| {
            ProviderError::new(ErrorKind::InvalidResponse, "model list is not valid JSON")
        })
    }
}

impl ModelProvider for HttpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn capabilities(&self, model: &str) -> ProviderCapabilities {
        ProviderCapabilities::resolve(self.protocol, self.models_meta.get(model))
    }

    fn stream(&self, req: &Request, cancel: CancellationToken) -> EventStream {
        let (url, body, mapper): (String, Value, Box<dyn Mapper>) = match self.protocol {
            Protocol::Anthropic => (
                format!("{}/v1/messages", self.base),
                anthropic::build_body(req),
                Box::new(anthropic::AnthropicMapper::new(self.ep.redaction.clone())),
            ),
            Protocol::OpenAi => (
                format!("{}/chat/completions", self.base),
                openai::build_body(req),
                Box::new(openai::OpenAiMapper::new(self.ep.redaction.clone())),
            ),
        };
        let body = serde_json::to_vec(&body).unwrap_or_default();
        spawn_stream(self.ep.clone(), url, body, mapper, cancel)
    }

    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let mut after: Option<String> = None;
            // Anthropic paginates with has_more / last_id; OpenAI returns one page.
            for _ in 0..100 {
                let mut url = self.models_url();
                if self.protocol == Protocol::Anthropic {
                    url.push_str("?limit=1000");
                    if let Some(a) = &after {
                        url.push_str(&format!("&after_id={a}"));
                    }
                }
                let page = self.page(&url, cancel).await?;
                out.extend(
                    page.get("data")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(normalize_model),
                );
                match (
                    page.get("has_more") == Some(&Value::Bool(true)),
                    page.get("last_id").and_then(Value::as_str),
                ) {
                    (true, Some(last)) => after = Some(last.to_string()),
                    _ => break,
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_config::{ApiKey, CredentialKind};

    fn cred(kind: CredentialKind, s: &str) -> Credential {
        Credential {
            kind,
            secret: s.into(),
            source: "test".into(),
        }
    }

    fn kind_of(p: &Provider, c: Option<&Credential>) -> &'static str {
        match ProviderSpec::from_config("gw", p, c).unwrap().auth {
            Auth::ApiKey(_) => "x-api-key",
            Auth::Bearer(_) => "bearer",
        }
    }

    #[test]
    fn auth_selection_matches_node() {
        let mut p = Provider {
            protocol: Protocol::Anthropic,
            base_url: Some("https://gw.test".into()),
            ..Default::default()
        };
        assert_eq!(
            kind_of(&p, Some(&cred(CredentialKind::Api, "sk-x"))),
            "bearer"
        );
        assert_eq!(
            kind_of(&p, Some(&cred(CredentialKind::Api, "sk-ant-1"))),
            "x-api-key"
        );
        assert_eq!(
            kind_of(&p, Some(&cred(CredentialKind::Token, "t"))),
            "bearer"
        );
        p.auth_header = Some("x-api-key".into());
        assert_eq!(
            kind_of(&p, Some(&cred(CredentialKind::Api, "sk-x"))),
            "x-api-key"
        );
        // missing credential is an auth error without a key in it; no-auth providers need none
        assert_eq!(
            ProviderSpec::from_config("gw", &p, None).unwrap_err().kind,
            ErrorKind::Auth
        );
        p.api_key = Some(ApiKey::Disabled);
        assert!(ProviderSpec::from_config("gw", &p, None).is_ok());
        // undecided protocol + sk-ant- key => Anthropic
        let q = Provider {
            type_source: TypeSource::Default,
            protocol: Protocol::OpenAi,
            ..Default::default()
        };
        let s = ProviderSpec::from_config("gw", &q, Some(&cred(CredentialKind::Api, "sk-ant-9")))
            .unwrap();
        assert_eq!(s.protocol, Protocol::Anthropic);
    }
}
