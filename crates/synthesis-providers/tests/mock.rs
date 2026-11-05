//! End-to-end tests against a local mock HTTP server (tokio TcpListener, no external network).

use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use synthesis_config::{Env, Protocol};
use synthesis_providers::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-test-SECRETSECRETSECRET1234";

enum Reply {
    Status {
        code: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// 200 SSE body written in small pieces. `cut`: chunked encoding, then die mid-chunk.
    Sse {
        body: Vec<u8>,
        piece: usize,
        cut: bool,
    },
    /// Sends headers, then nothing.
    Stall,
    /// Reads the request, then sends nothing at all (not even headers).
    Silent,
}

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn req(&self, i: usize) -> String {
        self.requests.lock().unwrap()[i].clone()
    }
}

async fn start(replies: Vec<Reply>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (reqs, n, replies) = (
        requests.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(replies),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (reqs, replies) = (reqs.clone(), replies.clone());
            let i = n.fetch_add(1, Ordering::SeqCst).min(replies.len() - 1);
            tokio::spawn(async move {
                let _ = sock.set_nodelay(true);
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, clen) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        break (p + 4, cl);
                    }
                };
                while buf.len() < head_end + clen {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                reqs.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                match &replies[i] {
                    Reply::Status {
                        code,
                        headers,
                        body,
                    } => {
                        let mut h = format!(
                            "HTTP/1.1 {code} X\r\ncontent-length: {}\r\nconnection: close\r\ncontent-type: application/json\r\n",
                            body.len()
                        );
                        for (k, v) in headers {
                            h.push_str(&format!("{k}: {v}\r\n"));
                        }
                        let _ = sock.write_all(format!("{h}\r\n{body}").as_bytes()).await;
                    }
                    Reply::Sse { body, piece, cut } => {
                        let te = if *cut {
                            "transfer-encoding: chunked"
                        } else {
                            "connection: close"
                        };
                        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{te}\r\n\r\n").as_bytes()).await;
                        for part in body.chunks(*piece) {
                            if *cut {
                                let _ = sock
                                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                                    .await;
                                let _ = sock.write_all(part).await;
                                let _ = sock.write_all(b"\r\n").await;
                            } else {
                                let _ = sock.write_all(part).await;
                            }
                            let _ = sock.flush().await;
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        if *cut {
                            let _ = sock.write_all(b"ff\r\npartial").await; // promised 255 bytes, dies
                        }
                    }
                    Reply::Silent => tokio::time::sleep(Duration::from_secs(30)).await,
                    Reply::Stall => {
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
                            )
                            .await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    Mock { base, requests }
}

fn status(code: u16, headers: Vec<(&'static str, String)>, body: Value) -> Reply {
    Reply::Status {
        code,
        headers,
        body: body.to_string(),
    }
}

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {e}\n\n",
                e["type"].as_str().unwrap_or("message")
            )
        })
        .collect::<String>()
        .into_bytes()
}

fn fast_retry(max: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries: max,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(20),
        ..Default::default()
    }
}

fn provider(
    proto: Protocol,
    mock: &Mock,
    auth: Auth,
    retry: RetryPolicy,
) -> Arc<dyn ModelProvider> {
    ProviderSpec {
        id: "gw".into(),
        protocol: proto,
        base_url: Some(mock.base.clone()),
        auth,
        headers: Default::default(),
        models_meta: Default::default(),
    }
    .connect(&Env::default(), &NetSettings::default(), retry)
    .unwrap()
}

fn req() -> Request {
    serde_json::from_value(json!({"model":"m","maxTokens":50,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"system":"s"})).unwrap()
}

async fn collect(
    p: &dyn ModelProvider,
    cancel: CancellationToken,
) -> (Vec<StreamEvent>, Option<ProviderError>) {
    let mut s = p.stream(&req(), cancel);
    let (mut ev, mut err) = (vec![], None);
    while let Some(i) = s.next().await {
        match i {
            Ok(e) => ev.push(e),
            Err(e) => err = Some(e),
        }
    }
    (ev, err)
}

fn fold(events: &[StreamEvent]) -> AssistantMessage {
    let mut b = MessageBuilder::new();
    events.iter().for_each(|e| b.push(e).unwrap());
    b.finish().unwrap()
}

fn anthropic_events() -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"héllo 日本語 🎉"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls é\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"ping"}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
        json!({"type":"message_stop"}),
    ]
}

#[tokio::test]
async fn anthropic_stream_split_mid_event_and_mid_utf8() {
    // piece = 1 byte: every multi-byte character and every event is split.
    let mock = start(vec![Reply::Sse {
        body: sse(&anthropic_events()),
        piece: 1,
        cut: false,
    }])
    .await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(0),
    );
    let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    let m = fold(&ev);
    assert_eq!(m.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(
        m.content,
        vec![
            Block::Thinking {
                thinking: "hm".into(),
                signature: Some("SIG".into())
            },
            Block::Text {
                text: "héllo 日本語 🎉".into()
            },
            Block::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: Some(json!({"command":"ls é"}))
            },
        ]
    );
    assert_eq!(
        m.usage,
        Usage {
            input_tokens: Some(10),
            output_tokens: Some(12),
            cache_read_input_tokens: Some(4),
            cache_creation_input_tokens: Some(2)
        }
    );
    let raw = mock.req(0).to_lowercase();
    assert!(raw.starts_with("post /v1/messages "), "{raw}");
    assert!(
        raw.contains(&format!("x-api-key: {}", KEY.to_lowercase()))
            && raw.contains("anthropic-version: 2023-06-01")
    );
    assert!(!raw.contains("authorization:"));
    assert!(raw.contains(r#""stream":true"#));
}

#[tokio::test]
async fn anthropic_bearer_for_gateway_credential() {
    let mock = start(vec![Reply::Sse {
        body: sse(&[json!({"type":"message_stop"})]),
        piece: 64,
        cut: false,
    }])
    .await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::Bearer(Secret::new(KEY)),
        fast_retry(0),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    let raw = mock.req(0).to_lowercase();
    assert!(
        raw.contains(&format!("authorization: bearer {}", KEY.to_lowercase()))
            && !raw.contains("x-api-key")
    );
}

fn openai_chunks() -> Vec<u8> {
    let c = |v: Value| format!("data: {v}\n\n");
    let mut s = String::new();
    s += &c(json!({"id":"c1","model":"gpt-5","choices":[{"delta":{"content":"héllo "}}]}));
    s += &c(
        json!({"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Bash","arguments":""}}]}}]}),
    );
    s += &c(
        json!({"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}),
    );
    s += &c(
        json!({"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}),
    );
    s += &c(json!({"id":"c1","choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
    s += &c(
        json!({"id":"c1","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":5}}}),
    );
    s += "data: [DONE]\n\n";
    s.into_bytes()
}

#[tokio::test]
async fn openai_tool_deltas_usage_and_bearer() {
    let mock = start(vec![Reply::Sse {
        body: openai_chunks(),
        piece: 2,
        cut: false,
    }])
    .await;
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(0),
    );
    let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    let m = fold(&ev);
    assert_eq!(m.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(
        m.content[1],
        Block::ToolUse {
            id: "call_1".into(),
            name: "Bash".into(),
            input: Some(json!({"command":"ls"}))
        }
    );
    assert_eq!(m.usage.cache_read_input_tokens, Some(5));
    assert_eq!(m.usage.input_tokens, Some(9));
    let raw = mock.req(0).to_lowercase();
    assert!(
        raw.starts_with("post /chat/completions ")
            && raw.contains(&format!("authorization: bearer {}", KEY.to_lowercase()))
    );
}

#[tokio::test]
async fn openai_invalid_tool_json_is_reported_not_repaired() {
    let c = |v: Value| format!("data: {v}\n\n");
    let body = c(
        json!({"id":"c","choices":[{"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"X","arguments":"{\"a\":"}}]}}]}),
    ) + &c(json!({"id":"c","choices":[{"delta":{},"finish_reason":"length"}]}))
        + &c(json!({"id":"c","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}));
    let mock = start(vec![Reply::Sse {
        body: body.into_bytes(),
        piece: 100,
        cut: false,
    }])
    .await;
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(0),
    );
    let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none());
    let m = fold(&ev);
    assert_eq!(m.stop_reason, Some(StopReason::MaxTokens));
    assert_eq!(
        m.content,
        vec![Block::ToolUse {
            id: "t".into(),
            name: "X".into(),
            input: None
        }]
    );
    assert_eq!(m.invalid_tool_inputs.len(), 1);
    assert_eq!(m.invalid_tool_inputs[0].raw, "{\"a\":");
}

#[tokio::test]
async fn rate_limit_honours_retry_after_then_succeeds() {
    let mock = start(vec![
        status(
            429,
            vec![("retry-after", "1".into())],
            json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}),
        ),
        Reply::Sse {
            body: sse(&[json!({"type":"message_stop"})]),
            piece: 64,
            cut: false,
        },
    ])
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let mut retry = fast_retry(3);
    retry.on_retry = Some(Arc::new(move |n: &RetryNotice| {
        s2.lock().unwrap().push((n.attempt, n.delay))
    }));
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        retry,
    );
    let t = Instant::now();
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert!(
        t.elapsed() >= Duration::from_secs(1),
        "waited {:?}",
        t.elapsed()
    );
    assert_eq!(mock.count(), 2);
    assert_eq!(*seen.lock().unwrap(), vec![(1, Duration::from_secs(1))]);
}

#[tokio::test]
async fn server_error_retried_then_success_and_exhaustion() {
    let ok = || Reply::Sse {
        body: sse(&[json!({"type":"message_stop"})]),
        piece: 64,
        cut: false,
    };
    let mock = start(vec![
        status(503, vec![], json!({"error":{"message":"x"}})),
        status(
            529,
            vec![],
            json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
        ),
        ok(),
    ])
    .await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(3),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(mock.count(), 3);

    let mock = start(vec![status(
        500,
        vec![],
        json!({"error":{"message":"boom"}}),
    )])
    .await;
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(2),
    );
    let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let err = err.unwrap();
    assert!(ev.is_empty() && err.kind == ErrorKind::Server && err.status == Some(500));
    assert_eq!(mock.count(), 3, "1 try + 2 retries");
}

#[tokio::test]
async fn auth_and_invalid_requests_are_not_retried_and_never_leak_the_key() {
    // OpenAI-style 401 echoes (part of) the key back in the message.
    let body = json!({"error":{"message":format!("Incorrect API key provided: {KEY}"),"type":"invalid_request_error","code":"invalid_api_key"}});
    let mock = start(vec![status(401, vec![], body)]).await;
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(3),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let err = err.unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert_eq!(mock.count(), 1);
    for text in [
        err.to_string(),
        format!("{err:?}"),
        err.message().to_string(),
    ] {
        assert!(!text.contains("SECRETSECRET"), "{text}");
    }

    let mock = start(vec![status(400, vec![], json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}))]).await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(3),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let err = err.unwrap();
    assert!(err.context_overflow() && !err.retryable());
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn network_error_text_has_no_credentials() {
    // Nothing listens on port 1; the base URL carries the key as userinfo.
    let mock = Mock {
        base: format!("http://u:{KEY}@127.0.0.1:1"),
        requests: Default::default(),
    };
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(1),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let err = err.unwrap();
    assert_eq!(err.kind, ErrorKind::Network);
    assert!(
        !err.to_string().contains("SECRETSECRET") && !format!("{err:?}").contains("SECRETSECRET"),
        "{err}"
    );
    assert!(!format!("{:?}", Secret::new(KEY)).contains("SECRET"));
    // Friendly: host:port only, a retry hint, no URL or userinfo.
    let m = err.message();
    assert!(
        m.starts_with("cannot reach 127.0.0.1:1 ") && m.contains("retry"),
        "{m}"
    );
    assert!(!m.contains("http"), "{m}");
}

#[tokio::test]
async fn mid_stream_disconnect_is_an_error_and_not_replayed() {
    let evs = anthropic_events();
    for cut in [true, false] {
        // cut: transport error inside a chunked body; !cut: clean EOF before message_stop.
        let body = sse(&evs[..7]);
        let mock = start(vec![Reply::Sse {
            body,
            piece: 40,
            cut,
        }])
        .await;
        let p = provider(
            Protocol::Anthropic,
            &mock,
            Auth::ApiKey(Secret::new(KEY)),
            fast_retry(3),
        );
        let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
        let err = err.expect("must fail");
        assert!(err.mid_stream && !err.retryable(), "{err}");
        assert!(!ev.is_empty(), "events before the cut are still delivered");
        assert_eq!(
            mock.count(),
            1,
            "no replay after output was delivered (cut={cut})"
        );
    }
}

#[tokio::test]
async fn cancellation_stops_a_stalled_stream_and_a_retry_wait() {
    let mock = start(vec![Reply::Stall]).await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(0),
    );
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let (_, err) = collect(p.as_ref(), cancel).await;
    assert_eq!(err.unwrap().kind, ErrorKind::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(5));

    // Cancel while waiting out a long Retry-After.
    let mock = start(vec![status(
        429,
        vec![("retry-after", "30".into())],
        json!({"error":{"message":"x"}}),
    )])
    .await;
    let p = provider(
        Protocol::OpenAi,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(3),
    );
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let (_, err) = collect(p.as_ref(), cancel).await;
    assert_eq!(err.unwrap().kind, ErrorKind::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(5) && mock.count() == 1);
}

#[tokio::test]
async fn in_stream_error_event_is_normalized() {
    let mut evs = anthropic_events()[..2].to_vec();
    evs.push(json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}));
    let mock = start(vec![Reply::Sse {
        body: sse(&evs),
        piece: 64,
        cut: false,
    }])
    .await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(3),
    );
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let err = err.unwrap();
    assert_eq!(err.kind, ErrorKind::Overloaded);
    assert!(!err.retryable());
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn list_models_follows_anthropic_pages() {
    let mock = start(vec![
        status(
            200,
            vec![],
            json!({"data":[{"id":"a","display_name":"A"}],"has_more":true,"last_id":"a"}),
        ),
        status(
            200,
            vec![],
            json!({"data":[{"id":"b","max_input_tokens":9}],"has_more":false}),
        ),
    ])
    .await;
    let p = provider(
        Protocol::Anthropic,
        &mock,
        Auth::ApiKey(Secret::new(KEY)),
        fast_retry(0),
    );
    let models = p.list_models(&CancellationToken::new()).await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[1].context_window, Some(9));
    assert!(mock.req(1).contains("after_id=a"));
}

#[tokio::test]
async fn proxy_comes_from_the_injected_env_only() {
    // HTTP_PROXY in the injected Env routes a plain-http request through the proxy (absolute-URI form).
    let proxy = start(vec![Reply::Sse {
        body: sse(&[json!({"type":"message_stop"})]),
        piece: 64,
        cut: false,
    }])
    .await;
    let env = Env::from_pairs([("http_proxy", proxy.base.as_str())]);
    let spec = || ProviderSpec {
        id: "gw".into(),
        protocol: Protocol::Anthropic,
        base_url: Some("http://llm.internal.test".into()),
        auth: Auth::ApiKey(Secret::new(KEY)),
        headers: Default::default(),
        models_meta: Default::default(),
    };
    let p = spec()
        .connect(&env, &NetSettings::default(), fast_retry(0))
        .unwrap();
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert!(
        proxy
            .req(0)
            .starts_with("POST http://llm.internal.test/v1/messages "),
        "{}",
        proxy.req(0)
    );

    // NO_PROXY bypasses it: the (dead) proxy is never contacted.
    let direct = start(vec![Reply::Sse {
        body: sse(&[json!({"type":"message_stop"})]),
        piece: 64,
        cut: false,
    }])
    .await;
    let env = Env::from_pairs([
        ("HTTP_PROXY", "http://127.0.0.1:1"),
        ("NO_PROXY", "127.0.0.1"),
    ]);
    let mut s = spec();
    s.base_url = Some(direct.base.clone());
    let p = s
        .connect(&env, &NetSettings::default(), fast_retry(0))
        .unwrap();
    let (_, err) = collect(p.as_ref(), CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(direct.count(), 1);
}

fn provider_with_header_timeout(
    mock: &Mock,
    retry: RetryPolicy,
    ms: u64,
) -> Arc<dyn ModelProvider> {
    ProviderSpec {
        id: "gw".into(),
        protocol: Protocol::Anthropic,
        base_url: Some(mock.base.clone()),
        auth: Auth::ApiKey(Secret::new(KEY)),
        headers: Default::default(),
        models_meta: Default::default(),
    }
    .connect(
        &Env::default(),
        &NetSettings {
            header_timeout: Duration::from_millis(ms),
            ..Default::default()
        },
        retry,
    )
    .unwrap()
}

#[tokio::test]
async fn a_server_that_never_sends_headers_is_a_retryable_timeout() {
    let m = start(vec![Reply::Silent]).await;
    let p = provider_with_header_timeout(&m, fast_retry(1), 150);
    let t = Instant::now();
    let (ev, err) = collect(p.as_ref(), CancellationToken::new()).await;
    let e = err.expect("a timeout error");
    assert!(ev.is_empty());
    assert!(e.to_string().contains("did not answer within"), "{e}");
    assert!(e.retryable());
    assert_eq!(m.count(), 2, "one retry");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancel_ends_the_wait_for_headers_at_once() {
    let m = start(vec![Reply::Silent]).await;
    let p = provider_with_header_timeout(&m, fast_retry(0), 60_000);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let (_, err) = collect(p.as_ref(), cancel).await;
    assert_eq!(err.unwrap().kind, ErrorKind::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn the_effort_tier_reaches_both_protocols() {
    for (proto, key, events) in [
        (
            Protocol::Anthropic,
            "output_config",
            sse(&anthropic_events()),
        ),
        (
            Protocol::OpenAi,
            "reasoning_effort",
            b"data: [DONE]\n\n".to_vec(),
        ),
    ] {
        let mock = start(vec![Reply::Sse {
            body: events,
            piece: 64,
            cut: false,
        }])
        .await;
        let p = provider(proto, &mock, Auth::ApiKey(Secret::new(KEY)), fast_retry(0));
        let mut r = req();
        r.reasoning_effort = Some("high".into());
        let mut s = p.stream(&r, CancellationToken::new());
        while s.next().await.is_some() {}
        let sent = mock.req(0);
        assert!(
            sent.contains(key) && sent.contains("\"high\""),
            "{key}: {sent}"
        );
        // and nothing when no tier is chosen
        let mock = start(vec![Reply::Sse {
            body: sse(&anthropic_events()),
            piece: 64,
            cut: false,
        }])
        .await;
        let p = provider(proto, &mock, Auth::ApiKey(Secret::new(KEY)), fast_retry(0));
        let mut s = p.stream(&req(), CancellationToken::new());
        while s.next().await.is_some() {}
        assert!(!mock.req(0).contains(key));
    }
}

fn cached(proto: Protocol, base: &str, cache: &std::path::Path) -> Arc<dyn ModelProvider> {
    ProviderSpec {
        id: "gw".into(),
        protocol: proto,
        base_url: Some(base.into()),
        auth: Auth::ApiKey(Secret::new(KEY)),
        headers: Default::default(),
        models_meta: Default::default(),
    }
    .connect_cached(
        &Env::default(),
        &NetSettings::default(),
        fast_retry(0),
        Some(cache.to_path_buf()),
    )
    .unwrap()
}

fn tmp_cache(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("synt-mock-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d.join("cache/models/gw.json")
}

#[tokio::test]
async fn anthropic_model_info_is_fetched_cached_and_reused_without_network() {
    let mock = start(vec![status(
        200,
        vec![],
        json!({"data":[
            {"id":"big","max_input_tokens":1_000_000,"max_tokens":64_000,
             "capabilities":{"image_input":{"supported":false},"effort":{"supported":true,"high":{"supported":true}}}},
            {"id":"small","max_input_tokens":200_000,"max_tokens":8_000}
        ],"has_more":false}),
    )])
    .await;
    let cache = tmp_cache("anth");
    let p = cached(Protocol::Anthropic, &mock.base, &cache);
    // unknown until the fetch ran
    assert_eq!(p.capabilities("big").context_window, None);
    p.refresh_model_info(false, &CancellationToken::new())
        .await
        .unwrap();
    let big = p.capabilities("big");
    assert_eq!(
        (big.context_window, big.max_output, big.images),
        (Some(1_000_000), Some(64_000), false)
    );
    // the same provider serves another model of the list with its own numbers
    assert_eq!(p.capabilities("small").context_window, Some(200_000));
    assert_eq!(p.capabilities("small").max_output, Some(8_000));
    let file = std::fs::read_to_string(&cache).unwrap();
    assert!(!file.contains(KEY));
    assert!(file.contains("\"effort_tiers\""));

    // a second start: fresh cache, no request, same numbers
    let q = cached(Protocol::Anthropic, &mock.base, &cache);
    assert_eq!(q.capabilities("big").context_window, Some(1_000_000));
    q.refresh_model_info(false, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(mock.count(), 1);
    // --refresh forces it
    q.refresh_model_info(true, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(mock.count(), 2);
    let _ = std::fs::remove_dir_all(cache.ancestors().nth(3).unwrap());
}

#[tokio::test]
async fn openai_compatible_shapes_and_offline_fallback_to_a_stale_cache() {
    let mock = start(vec![status(
        200,
        vec![],
        json!({"data":[
            {"id":"a","context_length":131072},
            {"id":"b","context_window":32000},
            {"id":"c","top_provider":{"context_length":64000,"max_completion_tokens":4096}},
            {"id":"d"}
        ]}),
    )])
    .await;
    let cache = tmp_cache("oai");
    let p = cached(Protocol::OpenAi, &mock.base, &cache);
    p.refresh_model_info(false, &CancellationToken::new())
        .await
        .unwrap();
    let w = |m: &str| p.capabilities(m).context_window;
    assert_eq!(
        (w("a"), w("b"), w("c"), w("d")),
        (Some(131_072), Some(32_000), Some(64_000), None)
    );
    assert_eq!(p.capabilities("c").max_output, Some(4096));

    // age the cache past the TTL, then go offline (nothing listens on port 1)
    let mut f: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    f["fetched_at"] = json!(1);
    f["base_url"] = json!("http://127.0.0.1:1");
    std::fs::write(&cache, serde_json::to_vec(&f).unwrap()).unwrap();
    let off = cached(Protocol::OpenAi, "http://127.0.0.1:1", &cache);
    assert_eq!(
        off.capabilities("a").context_window,
        Some(131_072),
        "stale cache is used"
    );
    assert!(
        off.refresh_model_info(false, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        off.capabilities("a").context_window,
        Some(131_072),
        "and kept"
    );

    // another endpoint: the cache does not apply
    let other = cached(Protocol::OpenAi, "http://127.0.0.1:2", &cache);
    assert_eq!(other.capabilities("a").context_window, None);
    let _ = std::fs::remove_dir_all(cache.ancestors().nth(3).unwrap());
}

#[tokio::test]
async fn config_beats_the_fetched_value_and_a_silent_server_times_out() {
    let mock = start(vec![status(
        200,
        vec![],
        json!({"data":[{"id":"m","max_input_tokens":1_000_000}]}),
    )])
    .await;
    let mut meta = indexmap::IndexMap::new();
    meta.insert(
        "m".to_string(),
        synthesis_config::ModelMeta {
            context_window: Some(50_000.0),
            ..Default::default()
        },
    );
    let spec = |base: &str| ProviderSpec {
        id: "gw".into(),
        protocol: Protocol::Anthropic,
        base_url: Some(base.into()),
        auth: Auth::ApiKey(Secret::new(KEY)),
        headers: Default::default(),
        models_meta: meta.clone(),
    };
    let p = spec(&mock.base)
        .connect(&Env::default(), &NetSettings::default(), fast_retry(0))
        .unwrap();
    p.refresh_model_info(false, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(p.capabilities("m").context_window, Some(50_000));

    let silent = start(vec![Reply::Silent]).await;
    let q = spec(&silent.base)
        .connect(&Env::default(), &NetSettings::default(), fast_retry(0))
        .unwrap();
    let t = Instant::now();
    let r = tokio::time::timeout(
        Duration::from_millis(300),
        q.refresh_model_info(false, &CancellationToken::new()),
    )
    .await;
    assert!(r.is_err(), "the caller's timeout ends the wait");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn only_openai_native_gets_the_builtin_table() {
    let mk = |base: Option<&str>| {
        ProviderSpec {
            id: "openai".into(),
            protocol: Protocol::OpenAi,
            base_url: base.map(String::from),
            auth: Auth::Bearer(Secret::new(KEY)),
            headers: Default::default(),
            models_meta: Default::default(),
        }
        .connect(&Env::default(), &NetSettings::default(), fast_retry(0))
        .unwrap()
    };
    assert_eq!(
        mk(None).capabilities("gpt-4o").context_window,
        Some(128_000)
    );
    assert_eq!(
        mk(Some("https://gw.example/v1"))
            .capabilities("gpt-4o")
            .context_window,
        None
    );
}
