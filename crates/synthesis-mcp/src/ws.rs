//! WebSocket client transport (`ws://`, `wss://`; subprotocol `mcp`): one JSON-RPC message per
//! text frame in each direction.
//!
//! URL policy is the SSE one (`wss`/https, or `ws`/http only to a loopback host; no private or
//! metadata addresses), checked once before connecting. Custom headers (auth) go on the handshake;
//! a `Sec-WebSocket-*` header is the transport's own and is refused in a config.
//!
//! Scope (owner decision): `ws` is for local development. No proxy support (`HTTP(S)_PROXY` is not
//! used here, unlike http/SSE) and no pinning of the resolved address between the URL check and
//! the connect, so DNS rebinding is a residual risk, the same as for SSE. Messages are capped.

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use reqwest::Url;
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

/// Largest message and frame accepted from the server.
const MAX_MESSAGE: usize = 4 << 20;

pub type WsError = tokio_tungstenite::tungstenite::Error;

/// Connects and returns the `(sink, stream)` pair `rmcp` serves over.
pub async fn connect(
    url: &str,
    headers: &[(String, String)],
) -> Result<
    (
        impl Sink<TxJsonRpcMessage<RoleClient>, Error = WsError> + Unpin + Send + 'static,
        impl Stream<Item = RxJsonRpcMessage<RoleClient>> + Unpin + Send + 'static,
    ),
    String,
> {
    let u = Url::parse(url).map_err(|_| "invalid url".to_string())?;
    // the same policy as http(s): ws -> http, wss -> https
    let mut check = u.clone();
    check
        .set_scheme(match u.scheme() {
            "wss" => "https",
            "ws" => "http",
            _ => return Err("a ws server needs a ws:// or wss:// url".into()),
        })
        .map_err(|()| "invalid url".to_string())?;
    crate::sse::check_url(&check).await?;
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mcp"));
    for (k, v) in headers {
        if k.to_ascii_lowercase().starts_with("sec-websocket-") {
            return Err(format!("header {k:?} is set by the transport"));
        }
        req.headers_mut().insert(
            HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("bad header name {k:?}"))?,
            HeaderValue::from_str(v).map_err(|_| format!("bad value for header {k:?}"))?,
        );
    }
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (ws, _) = tokio_tungstenite::connect_async_with_config(req, Some(cfg), false)
        .await
        .map_err(|e| format!("websocket: {e}"))?;
    let (tx, rx) = ws.split();
    let tx = tx.with(|m: TxJsonRpcMessage<RoleClient>| async move {
        serde_json::to_string(&m)
            .map(Message::text)
            .map_err(|e| WsError::Io(std::io::Error::other(e)))
    });
    let rx = rx.filter_map(|m| async move {
        match m {
            Ok(Message::Text(t)) => serde_json::from_str(t.as_str()).ok(),
            _ => None,
        }
    });
    Ok((Box::pin(tx), Box::pin(rx)))
}
