//! Minimal Chrome DevTools Protocol client over a WebSocket (flat session mode).

use crate::error::{BrowserError, Result};
use crate::screencast::Slot;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug, PartialEq)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    Response {
        id: u64,
        result: std::result::Result<Value, String>,
    },
    Event(CdpEvent),
}

pub(crate) fn encode_request(
    id: u64,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> String {
    let mut m = json!({"id": id, "method": method, "params": params});
    if let Some(s) = session {
        m["sessionId"] = json!(s);
    }
    m.to_string()
}

pub(crate) fn decode(text: &str) -> Option<Incoming> {
    let v: Value = serde_json::from_str(text).ok()?;
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = match v.get("error") {
            Some(e) => Err(e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned()),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        return Some(Incoming::Response { id, result });
    }
    Some(Incoming::Event(CdpEvent {
        method: v.get("method")?.as_str()?.to_owned(),
        params: v.get("params").cloned().unwrap_or(Value::Null),
        session_id: v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

pub struct Cdp {
    out: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<CdpEvent>,
    /// Screencast frames go here, never into `events`.
    pub(crate) screencast: Arc<Slot>,
}

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// One pipe frame above this closes the transport (a page-driven browser must not make us buffer
/// without limit).
const MAX_FRAME: usize = 64 << 20;
/// Bound on the queues between the transport tasks and the dispatcher.
const QUEUE: usize = 1024;

impl Cdp {
    /// CDP over a WebSocket (the `--remote-debugging-port` transport).
    pub async fn connect(ws_url: &str, event_capacity: usize) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BrowserError::Launch(format!("DevTools connect: {e}")))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::channel::<String>(QUEUE);
        let (in_tx, in_rx) = mpsc::channel::<String>(QUEUE);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    o = out_rx.recv() => match o {
                        Some(t) => if sink.send(Message::text(t)).await.is_err() { break },
                        None => break,
                    },
                    m = stream.next() => match m {
                        Some(Ok(Message::Text(t))) => if in_tx.send(t.to_string()).await.is_err() { break },
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
        });
        Ok(Self::run(out, in_rx, event_capacity))
    }

    /// CDP over the `--remote-debugging-pipe` transport: NUL-terminated JSON messages.
    pub fn from_pipe(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        event_capacity: usize,
    ) -> Self {
        Self::from_pipe_limited(reader, writer, event_capacity, MAX_FRAME)
    }

    fn from_pipe_limited(
        reader: impl AsyncRead + Unpin + Send + 'static,
        mut writer: impl AsyncWrite + Unpin + Send + 'static,
        event_capacity: usize,
        max_frame: usize,
    ) -> Self {
        let (out, mut out_rx) = mpsc::channel::<String>(QUEUE);
        let (in_tx, in_rx) = mpsc::channel::<String>(QUEUE);
        tokio::spawn(async move {
            while let Some(mut t) = out_rx.recv().await {
                t.push('\0');
                if writer.write_all(t.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut r = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match (&mut r)
                    .take(max_frame as u64 + 1)
                    .read_until(0, &mut buf)
                    .await
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        if buf.len() > max_frame {
                            break; // oversized frame: drop the transport, waiters get `Closed`
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self::run(out, in_rx, event_capacity)
    }

    fn run(
        out: mpsc::Sender<String>,
        mut inc: mpsc::Receiver<String>,
        event_capacity: usize,
    ) -> Self {
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(event_capacity.max(1));
        let (p, ev) = (pending.clone(), events.clone());
        let screencast = Arc::new(Slot::default());
        let frames = screencast.clone();
        let ack_out = out.downgrade();
        tokio::spawn(async move {
            while let Some(t) = inc.recv().await {
                match decode(&t) {
                    Some(Incoming::Response { id, result }) => {
                        if let Some(tx) = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                            let _ = tx.send(result);
                        }
                    }
                    Some(Incoming::Event(e)) if e.method == "Page.screencastFrame" => {
                        // a frame dropped on purpose (oversized, stale) is still acked; id 0 is never
                        // pending, so its response is ignored
                        if let (Some((s, id)), Some(o)) = (frames.push(&e), ack_out.upgrade()) {
                            let m = json!({"sessionId": id});
                            let _ = o.try_send(encode_request(
                                0,
                                "Page.screencastFrameAck",
                                m,
                                Some(&s),
                            ));
                        }
                    }
                    Some(Incoming::Event(e)) => {
                        let _ = ev.send(e);
                    }
                    None => {}
                }
            }
            // Dropping the senders wakes every waiter with `Closed`.
            p.lock().unwrap_or_else(|e| e.into_inner()).clear();
            frames.close();
        });
        Self {
            out,
            pending,
            next_id: AtomicU64::new(1),
            events,
            screencast,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    pub async fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if self
            .out
            .send(encode_request(id, method, params, session))
            .await
            .is_err()
        {
            return Err(BrowserError::Closed);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(BrowserError::Timeout(format!(
                    "{method} got no answer in {}s",
                    CALL_TIMEOUT.as_secs()
                )))
            }
            Ok(Err(_)) => Err(BrowserError::Closed),
            Ok(Ok(Err(message))) => Err(BrowserError::Cdp {
                method: method.to_owned(),
                message,
            }),
            Ok(Ok(Ok(v))) => Ok(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_with_and_without_session() {
        let v: Value =
            serde_json::from_str(&encode_request(7, "Page.enable", json!({}), Some("S1"))).unwrap();
        assert_eq!(
            v,
            json!({"id":7,"method":"Page.enable","params":{},"sessionId":"S1"})
        );
        let v: Value =
            serde_json::from_str(&encode_request(1, "Target.getTargets", json!({}), None)).unwrap();
        assert!(v.get("sessionId").is_none());
    }

    #[test]
    fn decodes_response_error_and_event() {
        assert_eq!(
            decode(r#"{"id":3,"result":{"a":1}}"#),
            Some(Incoming::Response {
                id: 3,
                result: Ok(json!({"a":1}))
            })
        );
        assert_eq!(
            decode(r#"{"id":4,"error":{"code":-32000,"message":"nope"}}"#),
            Some(Incoming::Response {
                id: 4,
                result: Err("nope".into())
            })
        );
        let Some(Incoming::Event(e)) =
            decode(r#"{"method":"Page.loadEventFired","params":{"timestamp":1},"sessionId":"S"}"#)
        else {
            panic!("event")
        };
        assert_eq!(
            (e.method.as_str(), e.session_id.as_deref()),
            ("Page.loadEventFired", Some("S"))
        );
        assert_eq!(decode("not json"), None);
        assert_eq!(decode(r#"{"foo":1}"#), None);
    }

    #[tokio::test]
    async fn screencast_frames_bypass_the_event_broadcast() {
        let (client_r, mut peer_w) = tokio::io::duplex(4096);
        let (client_w, _peer_r) = tokio::io::duplex(1024);
        let cdp = Cdp::from_pipe_limited(client_r, client_w, 16, 4096);
        let mut rx = cdp.subscribe();
        let (_, g) = cdp.screencast.activate(Some("S".into()));
        cdp.screencast.open(g);
        let frame = r#"{"method":"Page.screencastFrame","params":{"data":"aGk=","sessionId":1,"metadata":{}},"sessionId":"S"}"#;
        let other = r#"{"method":"Page.loadEventFired","params":{},"sessionId":"S"}"#;
        peer_w
            .write_all(format!("{frame}\0{other}\0").as_bytes())
            .await
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.method, "Page.loadEventFired"); // the frame, sent first, never showed up
        assert_eq!(
            cdp.screencast
                .received
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn oversized_pipe_frame_closes_the_transport() {
        let (client_r, mut peer_w) = tokio::io::duplex(1024);
        let (client_w, _peer_r) = tokio::io::duplex(1024);
        let cdp = Cdp::from_pipe_limited(client_r, client_w, 16, 64);
        tokio::spawn(async move {
            // 200 bytes without a NUL: above the 64 byte cap
            let _ = peer_w.write_all(&[b'x'; 200]).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            cdp.send(None, "Target.getTargets", json!({})),
        )
        .await
        .expect("send must not hang on an oversized frame");
        assert!(matches!(r, Err(BrowserError::Closed)), "{r:?}");
    }

    #[tokio::test]
    async fn frames_up_to_the_cap_still_work() {
        let (client_r, mut peer_w) = tokio::io::duplex(1024);
        let (client_w, _peer_r) = tokio::io::duplex(1024);
        let cdp = Cdp::from_pipe_limited(client_r, client_w, 16, 64);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = peer_w.write_all(b"{\"id\":1,\"result\":{}}\0").await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert!(cdp.send(None, "X.y", json!({})).await.is_ok());
    }
}
