//! Legacy HTTP+SSE client transport (MCP 2024-11-05, e.g. `https://mcp.atlassian.com/v1/sse`).
//! rmcp has no client for it: GET the event stream, wait for the `endpoint` event, POST every
//! client message to that endpoint and read the responses from `message` events.
//!
//! URL policy (same intent as WebFetch): https, or http only to a loopback host; no private,
//! link-local, metadata or other internal addresses; redirects are followed by hand and every hop
//! is checked; the `endpoint` the server announces must stay on the origin of the stream URL.
//! shortcut: DNS is checked before connecting, not pinned, and a failed lookup (egress proxy
//! doing the resolving) passes; upgrade by connecting through a validating resolver as synthesis-tools
//! `web.rs` does.

use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const MAX_REDIRECTS: usize = 5;
const MAX_EVENT_BYTES: usize = 16 << 20;
const POST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct SseError(pub String);

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SseError {}

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local() // includes the cloud metadata address
        || ip.is_broadcast()
        || ip.is_multicast()
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || o[0] >= 240
}

/// Loopback is judged separately (allowed for explicit loopback hosts only).
fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => blocked_v4(v),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => blocked_v4(m) || m.is_loopback(),
            None => {
                let s = v.segments();
                v.is_unspecified()
                    || v.is_multicast()
                    || (s[0] & 0xfe00) == 0xfc00 // ULA
                    || (s[0] & 0xffc0) == 0xfe80 // link-local
            }
        },
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `Err` = the request must not be made.
pub(crate) async fn check_url(u: &Url) -> Result<(), String> {
    let host = u.host_str().ok_or("URL has no host")?.to_string();
    let loopback = is_loopback_host(&host);
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        s => {
            return Err(format!(
                "scheme \"{s}\" is not allowed (https, or http to localhost)"
            ));
        }
    }
    if loopback {
        return Ok(());
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return if blocked(ip) || ip.is_loopback() {
            Err(format!("address {ip} is private/internal"))
        } else {
            Ok(())
        };
    }
    let port = u.port_or_known_default().unwrap_or(443);
    if let Ok(addrs) = tokio::net::lookup_host((host.as_str(), port)).await {
        for a in addrs {
            if blocked(a.ip()) || a.ip().is_loopback() {
                return Err(format!("{host} resolves to a private/internal address"));
            }
        }
    }
    Ok(())
}

/// GET with manual redirects; every hop passes [`check_url`]. Configured headers are not forwarded
/// to another origin.
async fn get_stream(
    client: &Client,
    url: &Url,
    headers: &HeaderMap,
) -> Result<reqwest::Response, String> {
    let mut url = url.clone();
    let origin = url.origin();
    for _ in 0..=MAX_REDIRECTS {
        check_url(&url).await?;
        // configured headers (Authorization, X-Api-Key, Cookie, ...) never leave the origin
        let h = if url.origin() == origin {
            headers.clone()
        } else {
            HeaderMap::new()
        };
        let r = client
            .get(url.clone())
            .headers(h)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("GET failed: {}", e.without_url()))?;
        if r.status().is_redirection() {
            let loc = r
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without Location")?;
            url = url.join(loc).map_err(|_| "bad redirect target")?;
            continue;
        }
        if !r.status().is_success() {
            return Err(format!("HTTP {}", r.status()));
        }
        return Ok(r);
    }
    Err("too many redirects".into())
}

/// `(event, data)` of one SSE block; comment-only blocks give `None`.
fn parse_event(block: &str) -> Option<(String, String)> {
    let (mut event, mut data) = (String::new(), Vec::new());
    for line in block.lines() {
        let (k, v) = line.split_once(':').unwrap_or((line, ""));
        let v = v.strip_prefix(' ').unwrap_or(v);
        match k {
            "event" => event = v.to_string(),
            "data" => data.push(v),
            _ => {}
        }
    }
    (!data.is_empty()).then(|| (event, data.join("\n")))
}

/// Receiving half: JSON-RPC messages from `message` events. Dropping it stops the reader.
pub struct SseRx {
    rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
    task: JoinHandle<()>,
}

impl Stream for SseRx {
    type Item = RxJsonRpcMessage<RoleClient>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for SseRx {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type PostFuture = Pin<Box<dyn Future<Output = Result<(), SseError>> + Send>>;

type Poster = Box<
    dyn Fn(
            TxJsonRpcMessage<RoleClient>,
        ) -> Pin<Box<dyn Future<Output = Result<(), SseError>> + Send>>
        + Send
        + Sync,
>;

/// Sending half: one POST per message.
pub struct SseTx {
    post: Poster,
    pending: Option<PostFuture>,
}

impl futures_util::Sink<TxJsonRpcMessage<RoleClient>> for SseTx {
    type Error = SseError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SseError>> {
        self.poll_flush(cx)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> Result<(), SseError> {
        let f = (self.post)(item);
        self.pending = Some(f);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SseError>> {
        match self.pending.as_mut() {
            None => Poll::Ready(Ok(())),
            Some(f) => {
                let r = f.as_mut().poll(cx);
                if r.is_ready() {
                    self.pending = None;
                }
                r
            }
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SseError>> {
        self.poll_flush(cx)
    }
}

/// Opens the stream and waits for the `endpoint` event. `headers` are sent on the GET and every
/// POST.
pub async fn connect(url: &str, headers: HeaderMap) -> Result<(SseTx, SseRx), String> {
    let url = Url::parse(url).map_err(|_| "invalid url".to_string())?;
    // redirects are handled above; no automatic ones
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let resp = get_stream(&client, &url, &headers).await?;
    let base = resp.url().clone();
    let mut bytes = resp.bytes_stream();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (ep_tx, ep_rx) = tokio::sync::oneshot::channel::<Result<Url, String>>();
    let task_base = base.clone();
    let task = tokio::spawn(async move {
        let mut ep_tx = Some(ep_tx);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(Ok(chunk)) = bytes.next().await {
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_EVENT_BYTES {
                return;
            }
            // events end with a blank line; normalise CRLF first
            let mut text = String::from_utf8_lossy(&buf).replace("\r\n", "\n");
            while let Some(i) = text.find("\n\n") {
                let block: String = text.drain(..i + 2).collect();
                match parse_event(&block) {
                    Some((ev, data)) if ev == "endpoint" => {
                        if let Some(tx) = ep_tx.take() {
                            let _ = tx.send(task_base.join(data.trim()).map_err(|e| e.to_string()));
                        }
                    }
                    Some((ev, data)) if ev.is_empty() || ev == "message" => {
                        if let Ok(m) = serde_json::from_str(&data)
                            && msg_tx.send(m).is_err()
                        {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            buf = text.into_bytes();
        }
    });
    let rx = SseRx { rx: msg_rx, task };
    let endpoint = ep_rx
        .await
        .map_err(|_| "stream ended before the endpoint event".to_string())??;
    if endpoint.origin() != url.origin() && endpoint.origin() != base.origin() {
        return Err("the endpoint announced by the server is on another origin".into());
    }
    check_url(&endpoint).await?;
    // a stream redirected to another host may announce an endpoint there: no credentials for it
    let headers = if endpoint.origin() == url.origin() {
        headers
    } else {
        HeaderMap::new()
    };
    let post: Poster = Box::new(move |m| {
        let (client, endpoint, headers) = (client.clone(), endpoint.clone(), headers.clone());
        Box::pin(async move {
            let r = client
                .post(endpoint)
                .headers(headers)
                .timeout(POST_TIMEOUT)
                .json(&m)
                .send()
                .await
                .map_err(|e| SseError(format!("POST failed: {}", e.without_url())))?;
            if r.status().is_success() {
                Ok(())
            } else {
                Err(SseError(format!("POST: HTTP {}", r.status())))
            }
        })
    });
    Ok((
        SseTx {
            post,
            pending: None,
        },
        rx,
    ))
}

/// Header map from already expanded `(name, value)` pairs.
pub fn header_map(pairs: &[(String, String)]) -> Result<HeaderMap, String> {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| format!("invalid header name \"{k}\""))?;
        let mut val =
            HeaderValue::from_str(v).map_err(|_| format!("invalid value for header \"{k}\""))?;
        val.set_sensitive(true);
        h.insert(name, val);
    }
    Ok(h)
}
