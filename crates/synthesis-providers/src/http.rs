//! HTTP plumbing shared by both clients: client construction (proxy / CA from an injected
//! `Env`), authenticated send with the single retry layer, and the SSE stream driver.

use crate::error::{ErrorKind, ProviderError, Redaction};
use crate::retry::RetryPolicy;
use crate::sse::{SseEvent, SseParser};
use crate::types::StreamEvent;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use std::time::Duration;
use synthesis_config::Env;
use tokio_util::sync::CancellationToken;

pub type EventStream = BoxStream<'static, Result<StreamEvent, ProviderError>>;

#[derive(Debug, Clone)]
pub struct NetSettings {
    pub connect_timeout: Duration,
    /// Max silence between two network chunks of a stream.
    pub idle_timeout: Duration,
    /// Max wait for the response headers after the request was sent (a server that accepts the
    /// connection and then says nothing).
    pub header_timeout: Duration,
    /// Longest wait for the body of one model-list page (a gateway may drip or never finish).
    pub list_deadline: Duration,
}

impl Default for NetSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(300),
            header_timeout: Duration::from_secs(120),
            list_deadline: Duration::from_secs(30),
        }
    }
}

/// First non-empty of `NAME` / `name` (the standard proxy variable spellings).
fn var<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
    env.get_nonempty(name)
        .or_else(|| env.get_nonempty(&name.to_lowercase()))
}

/// Values of the proxy variables: they may carry credentials, so they join the redaction list.
pub(crate) fn proxy_secrets(env: &Env) -> Vec<String> {
    ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"]
        .iter()
        .filter_map(|n| var(env, n).map(String::from))
        .collect()
}

/// Port of Node `net.js` onto the standard variables, read from `env` only:
/// proxy = `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (+ lowercase) with `NO_PROXY` matching done
/// by reqwest; CA = platform trust store (rustls-platform-verifier) merged with the PEM bundles
/// named by `SSL_CERT_FILE` and the files in `SSL_CERT_DIR`. Unreadable CA files are skipped
/// (Node's merge is best-effort too).
pub fn build_client(env: &Env, s: &NetSettings) -> Result<reqwest::Client, ProviderError> {
    let bad = |m: String| ProviderError::new(ErrorKind::InvalidRequest, m);
    let mut b = reqwest::Client::builder()
        .connect_timeout(s.connect_timeout)
        .user_agent(concat!("synthesis-providers/", env!("CARGO_PKG_VERSION")))
        // Never follow redirects: a redirect must not carry the API key to another host.
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let no_proxy = var(env, "NO_PROXY").and_then(reqwest::NoProxy::from_string);
    // First matching proxy wins: scheme-specific before ALL_PROXY.
    for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        if let Some(url) = var(env, name) {
            let made = match name {
                "HTTPS_PROXY" => reqwest::Proxy::https(url),
                "HTTP_PROXY" => reqwest::Proxy::http(url),
                _ => reqwest::Proxy::all(url),
            };
            // The value may hold credentials: name the variable, never echo it.
            let p = made.map_err(|_| bad(format!("invalid proxy URL in {name}")))?;
            b = b.proxy(p.no_proxy(no_proxy.clone()));
        }
    }
    for cert in extra_roots(env) {
        b = b.add_root_certificate(cert);
    }
    b.build().map_err(|e| {
        // reqwest's own text is just "builder error"; the cause (e.g. no CA certificates) is in the chain.
        let e = e.without_url();
        let mut m = format!("cannot build HTTP client: {e}");
        let mut src = std::error::Error::source(&e);
        while let Some(c) = src {
            m += &format!(": {c}");
            src = c.source();
        }
        bad(m)
    })
}

fn extra_roots(env: &Env) -> Vec<reqwest::Certificate> {
    let mut files = Vec::new();
    if let Some(f) = env.get_nonempty("SSL_CERT_FILE") {
        files.push(f.into());
    }
    if let Some(d) = env.get_nonempty("SSL_CERT_DIR") {
        for dir in std::env::split_paths(d) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                files.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_file()));
            }
        }
    }
    files
        .iter()
        .filter_map(|f: &std::path::PathBuf| std::fs::read(f).ok())
        .filter_map(|pem| reqwest::Certificate::from_pem_bundle(&pem).ok())
        .flatten()
        .collect()
}

pub(crate) fn header(
    name: &str,
    value: &str,
    sensitive: bool,
) -> Result<(HeaderName, HeaderValue), ProviderError> {
    let bad = || {
        ProviderError::new(
            ErrorKind::InvalidRequest,
            format!("invalid HTTP header \"{name}\""),
        )
    };
    let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| bad())?;
    let mut v = HeaderValue::from_str(value).map_err(|_| bad())?;
    v.set_sensitive(sensitive);
    Ok((n, v))
}

pub(crate) struct Endpoint {
    pub client: reqwest::Client,
    pub headers: HeaderMap,
    pub retry: RetryPolicy,
    pub redaction: Redaction,
    pub idle_timeout: Duration,
    pub header_timeout: Duration,
}

/// `Retry-After` as seconds (`retry-after-ms` also honoured). HTTP-date form is not parsed
/// (shortcut: neither Anthropic nor OpenAI sends it; the normal backoff applies instead).
fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let num = |name: &str| h.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    if let Some(ms) = num("retry-after-ms").filter(|v| *v >= 0.0) {
        return Some(Duration::from_secs_f64(ms / 1000.0));
    }
    let secs = num(RETRY_AFTER.as_str()).filter(|v| *v >= 0.0 && v.is_finite())?;
    Some(Duration::from_secs_f64(secs))
}

impl Endpoint {
    /// One logical request: POST when `body` is given, else GET. Retries per policy; returns the
    /// 2xx response with the body unread.
    pub async fn send(
        &self,
        url: &str,
        body: Option<&[u8]>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, ProviderError> {
        self.retry
            .run(cancel, || async {
                let mut rb = match body {
                    Some(b) => self
                        .client
                        .post(url)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(b.to_vec()),
                    None => self.client.get(url),
                };
                rb = rb.headers(self.headers.clone());
                // Not a `reqwest` timeout: that one would also cover the body of a stream.
                let resp = tokio::time::timeout(self.header_timeout, rb.send())
                    .await
                    .map_err(|_| {
                        ProviderError::new(
                            ErrorKind::Network,
                            format!(
                                "the provider did not answer within {}s (no response headers)",
                                self.header_timeout.as_secs()
                            ),
                        )
                    })?
                    .map_err(|e| self.redaction.transport(e))?;
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let ra = retry_after(resp.headers());
                let text = resp.text().await.map_err(|e| self.redaction.transport(e))?;
                Err(self.redaction.api_error(Some(status.as_u16()), &text, ra))
            })
            .await
    }
}

/// Turns SSE events into normalized events; one instance per request.
pub trait Mapper: Send {
    fn feed(&mut self, ev: SseEvent) -> Result<Vec<StreamEvent>, ProviderError>;
    /// Called at clean end of stream: synthetic closers, or an error when truncated.
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}

pub(crate) fn spawn_stream(
    ep: std::sync::Arc<Endpoint>,
    url: String,
    body: Vec<u8>,
    mut mapper: Box<dyn Mapper>,
    cancel: CancellationToken,
) -> EventStream {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        if let Err(e) = drive(&ep, &url, &body, mapper.as_mut(), &cancel, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) })
        .boxed()
}

async fn drive(
    ep: &Endpoint,
    url: &str,
    body: &[u8],
    mapper: &mut dyn Mapper,
    cancel: &CancellationToken,
    tx: &tokio::sync::mpsc::Sender<Result<StreamEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let resp = ep.send(url, Some(body), cancel).await?;
    let mut bytes = Box::pin(resp.bytes_stream());
    let mut sse = SseParser::new();
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::cancelled()),
            () = tx.closed() => return Ok(()),
            r = tokio::time::timeout(ep.idle_timeout, bytes.next()) => r,
        };
        let chunk = match next {
            Err(_) => {
                return Err(
                    ProviderError::new(ErrorKind::Network, "stream idle timeout").mid_stream(),
                );
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(ep.redaction.transport(e).mid_stream()),
            Ok(Some(Ok(c))) => c,
        };
        for ev in sse.push(&chunk) {
            for out in mapper.feed(ev).map_err(ProviderError::mid_stream)? {
                if tx.send(Ok(out)).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
    for out in mapper.finish().map_err(ProviderError::mid_stream)? {
        if tx.send(Ok(out)).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}
