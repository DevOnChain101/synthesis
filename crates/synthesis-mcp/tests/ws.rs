//! WebSocket transport against a local fake server (initialize, tools/list, tools/call).
mod common;
use common::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use synthesis_mcp::{McpManager, McpServerConfig, ServerState, TransportKind};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

fn result(method: &str, p: &Value) -> Value {
    match method {
        "initialize" => json!({"protocolVersion": p["protocolVersion"],
            "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}),
        "tools/list" => {
            json!({"tools": [{"name": "ping", "inputSchema": {"type":"object","properties":{}}}]})
        }
        "tools/call" => json!({"content": [{"type":"text","text":"pong"}]}),
        _ => json!({}),
    }
}

/// Serves one connection; `Some(token)` demands `Authorization: Bearer token` on the handshake.
#[allow(clippy::result_large_err)] // the handshake callback type is fixed by tungstenite
async fn serve(token: Option<&'static str>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let cb = move |req: &Request, mut resp: Response| {
                let ok = token.is_none_or(|t| {
                    req.headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        == Some(&format!("Bearer {t}"))
                });
                if !ok {
                    return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(401)
                        .body(None)
                        .unwrap());
                }
                resp.headers_mut()
                    .insert("Sec-WebSocket-Protocol", "mcp".parse().unwrap());
                Ok(resp)
            };
            let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(s, cb).await else {
                continue;
            };
            tokio::spawn(async move {
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let m: Value = serde_json::from_str(t.as_str()).unwrap();
                    if let (Some(id), Some(method)) = (m.get("id"), m["method"].as_str()) {
                        let r =
                            json!({"jsonrpc":"2.0","id":id,"result":result(method, &m["params"])});
                        ws.send(Message::text(r.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    format!("ws://{addr}/mcp")
}

fn cfg(url: &str) -> McpServerConfig {
    let mut c = McpServerConfig::http("w", url);
    c.transport = Some(TransportKind::Ws);
    c
}

#[tokio::test]
async fn ws_connects_lists_and_calls_with_a_handshake_header() {
    let url = serve(Some("tok")).await;
    let mut c = cfg(&url);
    c.headers
        .insert("Authorization".into(), "Bearer tok".into());
    let m = McpManager::new(vec![c], opts(base_env()));
    m.start().await;
    let st = m.status();
    assert_eq!(
        (st[0].state, st[0].transport),
        (ServerState::Connected, "ws"),
        "{:?}",
        st[0]
    );
    assert_eq!(call(&m, "w", "ping", json!({})).await.text(), "pong");
    m.close().await;

    // a configured Sec-WebSocket-* header is refused, not allowed to replace the transport's
    let mut c = cfg(&url);
    c.headers
        .insert("Authorization".into(), "Bearer tok".into());
    c.headers
        .insert("sec-websocket-protocol".into(), "evil".into());
    let m = McpManager::new(vec![c], opts(base_env()));
    m.start().await;
    assert_eq!(m.status()[0].state, ServerState::Failed);

    let m = McpManager::new(vec![cfg(&url)], opts(base_env()));
    m.start().await;
    assert_eq!(
        m.status()[0].state,
        ServerState::Failed,
        "no token: handshake refused"
    );

    // plain ws to a public host is refused before any connection
    let m = McpManager::new(vec![cfg("ws://example.com/mcp")], opts(base_env()));
    m.start().await;
    assert_eq!(m.status()[0].state, ServerState::Failed);
}
