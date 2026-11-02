//! "Writes show up as `anonymous`": proves the credentials configured for a server reach it on
//! every request, over stdio (env), streamable HTTP (Bearer, Basic, custom headers) and legacy
//! SSE, across reconnects, HTTP session expiry and `tools/list_changed`.
//!
//! The server is `examples/atlassian_like_server`: it records `author` on every write and answers
//! `whoami`; over HTTP it derives the principal from the request headers, over stdio from
//! `JIRA_USERNAME` + `JIRA_API_TOKEN`, and says `anonymous` otherwise.
#![allow(clippy::field_reassign_with_default)] // rmcp result structs are non_exhaustive
#[allow(dead_code)]
#[path = "../examples/atlassian_like_server.rs"]
mod atl;
mod common;

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::response::sse::{Event, Sse};
use common::*;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_mcp::{McpManager, McpServerConfig, ServerState, TransportKind};
use tokio::sync::mpsc;

const BASIC: &str = "Basic YW5hQGV4YW1wbGUuY29tOmFwaS10b2tlbi0xMjM="; // ana@example.com:api-token-123

fn atl_cfg(name: &str) -> McpServerConfig {
    McpServerConfig::stdio(
        name,
        example_path("atlassian_like_server").to_string_lossy(),
        vec![],
    )
}

fn user_env(pairs: &[(&str, &str)]) -> Env {
    let mut v: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), "/home/ana".into()),
        ("USERPROFILE".into(), "C:\\Users\\ana".into()),
        ("APPDATA".into(), "C:\\Users\\ana\\AppData\\Roaming".into()),
    ];
    v.extend(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    Env::from_pairs(v)
}

fn opts_with(env: Env, passthrough: &[&str]) -> synthesis_mcp::McpOptions {
    let mut o = opts(env);
    o.env_passthrough = passthrough.iter().map(|s| s.to_string()).collect();
    o
}

fn structured(r: &synthesis_mcp::McpToolResult) -> Value {
    assert!(!r.is_error, "{}", r.text());
    r.structured.clone().expect("structuredContent")
}

/// whoami + read + WRITE as `who`; the page is re-read to prove the stored author.
async fn assert_identity(m: &McpManager, who: &str) {
    let w = structured(&call(m, "atl", "whoami", json!({})).await);
    assert_eq!(w["principal"], who, "whoami");
    let page = structured(&call(m, "atl", "getPage", json!({"id": "100"})).await);
    let next = page["version"]["number"].as_i64().unwrap() + 1;
    let up = structured(
        &call(
            m,
            "atl",
            "updatePage",
            json!({"id": "100", "title": "Runbook", "body": "b", "version": {"number": next}}),
        )
        .await,
    );
    assert_eq!(up["author"], who, "author recorded on the write");
    let again = structured(&call(m, "atl", "getPage", json!({"id": "100"})).await);
    assert_eq!(again["author"], who);
    let issue = structured(
        &call(
            m,
            "atl",
            "editIssue",
            json!({"issueIdOrKey": "PROJ-1", "fields": {"description": "x"}}),
        )
        .await,
    );
    assert_eq!(issue["author"], who);
}

// ---- H1: stdio env -----------------------------------------------------------------------------

#[tokio::test]
async fn stdio_explicit_env_credentials_are_the_identity_after_reconnect_and_list_changed() {
    let mut c = atl_cfg("atl");
    c.scope = Some("user".into());
    c.env
        .insert("JIRA_USERNAME".into(), "ana@example.com".into());
    c.env.insert("JIRA_API_TOKEN".into(), "${ATL_TOKEN}".into());
    let m = McpManager::new(
        vec![c],
        opts_with(user_env(&[("ATL_TOKEN", "api-token-123")]), &["ATL_TOKEN"]),
    );
    m.start().await;
    assert_eq!(
        m.status()[0].state,
        ServerState::Connected,
        "{:?}",
        m.status()
    );
    assert_identity(&m, "ana@example.com").await;

    m.reconnect("atl").await;
    assert_eq!(m.status()[0].state, ServerState::Connected);
    assert_identity(&m, "ana@example.com").await;

    call(&m, "atl", "triggerListChanged", json!({})).await;
    wait_until("extraTool", || {
        m.tools().iter().any(|t| t.tool == "extraTool")
    })
    .await;
    assert_identity(&m, "ana@example.com").await;
    m.close().await;
}

#[tokio::test]
async fn stdio_without_credentials_the_server_says_anonymous() {
    // negative control: this is exactly what a server shows when the env never arrived
    let mut c = atl_cfg("atl");
    c.scope = Some("user".into());
    let m = McpManager::new(vec![c], opts(user_env(&[])));
    m.start().await;
    assert_identity(&m, "anonymous").await;
    m.close().await;
}

#[tokio::test]
async fn stdio_secret_shaped_names_in_the_servers_own_env_are_never_filtered() {
    let names = [
        "JIRA_API_TOKEN",
        "CONFLUENCE_API_TOKEN",
        "ATLASSIAN_API_TOKEN",
        "MY_API_KEY",
        "DB_PASSWORD",
        "SESSION_COOKIE",
        "OAUTH_SECRET",
    ];
    // the PARENT also has a JIRA_API_TOKEN and other secrets: only the server's own entries count
    let env = user_env(&[
        ("JIRA_API_TOKEN", "PARENT-LEAK"),
        ("PARENT_ONLY_TOKEN", "PARENT-LEAK-2"),
        ("AWS_SECRET_ACCESS_KEY", "PARENT-LEAK-3"),
    ]);
    for scope in ["user", "project"] {
        let mut c = echo_cfg("echo", &[]);
        c.scope = Some(scope.into());
        for n in names {
            c.env.insert(n.into(), format!("literal-{n}-value"));
        }
        let m = McpManager::new(vec![c], opts(env.clone()));
        m.start().await;
        let get = |n: &'static str| {
            let m = m.clone();
            async move { call(&m, "echo", "env", json!({"name": n})).await.text() }
        };
        for n in names {
            assert_eq!(get(n).await, format!("literal-{n}-value"), "{scope} {n}");
        }
        assert_eq!(get("PARENT_ONLY_TOKEN").await, "<unset>");
        assert_eq!(get("AWS_SECRET_ACCESS_KEY").await, "<unset>");
        m.close().await;
    }
    // without an explicit entry the parent's JIRA_API_TOKEN is not inherited either
    let m = McpManager::new(vec![echo_cfg("echo", &[])], opts(env));
    m.start().await;
    let r = call(&m, "echo", "env", json!({"name": "JIRA_API_TOKEN"})).await;
    assert_eq!(r.text(), "<unset>");
    m.close().await;
}

#[tokio::test]
async fn stdio_home_userprofile_appdata_reach_the_child_and_server_env_wins() {
    let mut c = echo_cfg("echo", &[]);
    c.env.insert("HOME".into(), "/override".into());
    let m = McpManager::new(vec![c], opts(user_env(&[])));
    m.start().await;
    let get = |n: &'static str| {
        let m = m.clone();
        async move { call(&m, "echo", "env", json!({"name": n})).await.text() }
    };
    assert_eq!(get("HOME").await, "/override", "server env > base env");
    assert_eq!(get("USERPROFILE").await, "C:\\Users\\ana");
    assert_eq!(get("APPDATA").await, "C:\\Users\\ana\\AppData\\Roaming");
    m.close().await;
}

#[tokio::test]
async fn expansion_failures_are_loud_and_nothing_starts_unauthenticated() {
    // unset, empty, and listed-but-unset; user scope and project scope; env, stdio and http
    let cases: Vec<(&str, Env, &[&str])> = vec![
        ("user", user_env(&[]), &["CONFLUENCE_TOKEN"]),
        (
            "user",
            user_env(&[("CONFLUENCE_TOKEN", "")]),
            &["CONFLUENCE_TOKEN"],
        ),
        ("user", user_env(&[("CONFLUENCE_TOKEN", "t")]), &[]),
        (
            "project",
            user_env(&[("CONFLUENCE_TOKEN", "t")]),
            &["CONFLUENCE_TOKEN"],
        ),
    ];
    for (scope, env, pass) in cases {
        let mut s = atl_cfg("atl");
        s.scope = Some(scope.into());
        s.env
            .insert("JIRA_API_TOKEN".into(), "${CONFLUENCE_TOKEN}".into());
        let mut h = McpServerConfig::http("web", "http://127.0.0.1:1/mcp");
        h.scope = Some(scope.into());
        h.headers
            .insert("Authorization".into(), "Bearer ${CONFLUENCE_TOKEN}".into());
        let mut b = h.clone();
        b.name = "basic".into();
        b.headers
            .insert("Authorization".into(), "Basic ${CONFLUENCE_TOKEN}".into());
        let m = McpManager::new(vec![s, h, b], opts_with(env, pass));
        m.start().await;
        for st in m.status() {
            let e = st.error.clone().unwrap_or_default();
            assert_eq!(st.state, ServerState::Failed, "{scope} {st:?}");
            assert!(
                e.contains("CONFLUENCE_TOKEN") && e.contains("not available"),
                "{e}"
            );
            assert!(
                e.contains(if scope == "user" {
                    "synt.mcp.env_passthrough in your user settings.json"
                } else {
                    "user synt.json"
                }),
                "{e}"
            );
        }
    }
}

// ---- H2: streamable HTTP -----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    session: bool,
    headers: HeaderMap,
}
type Log = Arc<Mutex<Vec<Seen>>>;

#[derive(Clone)]
struct Gate {
    log: Log,
    expire: Arc<AtomicBool>,
    want: Vec<(&'static str, String)>,
}

async fn gate(State(g): State<Gate>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let seen = Seen {
        method: req.method().to_string(),
        session: req.headers().contains_key("mcp-session-id"),
        headers: req.headers().clone(),
    };
    g.log.lock().unwrap().push(seen.clone());
    let ok = g
        .want
        .iter()
        .all(|(k, v)| seen.headers.get(*k).and_then(|x| x.to_str().ok()) == Some(v.as_str()));
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if seen.method == "POST" && seen.session && g.expire.swap(false, Ordering::SeqCst) {
        return Err(StatusCode::NOT_FOUND); // the server lost the session
    }
    Ok(next.run(req).await)
}

async fn serve_http(
    want: Vec<(&'static str, String)>,
    rules: Vec<(&str, String, &str)>,
) -> (String, Log, Arc<AtomicBool>) {
    let mut server = atl::Atlassian::seeded(true, false, false, "stdio-only".into());
    server.rules = rules
        .into_iter()
        .map(|(h, v, p)| (h.to_string(), v, p.to_string()))
        .collect();
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let (log, expire) = (Log::default(), Arc::new(AtomicBool::new(false)));
    let g = Gate {
        log: log.clone(),
        expire: expire.clone(),
        want,
    };
    let app = axum::Router::new()
        .nest_service("/mcp", svc)
        .layer(middleware::from_fn_with_state(g, gate));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, log, expire)
}

fn http_cfg(url: &str, headers: &[(&str, &str)]) -> McpServerConfig {
    let mut c = McpServerConfig::http("atl", url);
    c.scope = Some("user".into());
    for (k, v) in headers {
        c.headers.insert(k.to_string(), v.to_string());
    }
    c
}

/// Every request the server saw carried `header: value`, and the lifecycle requests all happened.
fn assert_all_carry(log: &Log, header: &str, value: &str) {
    let l = log.lock().unwrap();
    assert!(!l.is_empty());
    for s in l.iter() {
        assert_eq!(
            s.headers.get(header).and_then(|v| v.to_str().ok()),
            Some(value),
            "{} (session={}) lacks {header}",
            s.method,
            s.session
        );
    }
    assert!(
        l.iter().any(|s| s.method == "DELETE"),
        "session DELETE seen"
    );
    assert!(l.iter().any(|s| s.method == "GET"), "event stream GET seen");
    let initializes = l
        .iter()
        .filter(|s| s.method == "POST" && !s.session)
        .count();
    assert!(
        initializes >= 3,
        "connect, reconnect and the re-initialisation after the 404 all sent it ({initializes})"
    );
}

async fn http_lifecycle(m: &McpManager, expire: &Arc<AtomicBool>, who: &str) {
    m.start().await;
    assert_eq!(
        m.status()[0].state,
        ServerState::Connected,
        "{:?}",
        m.status()
    );
    assert_identity(m, who).await;
    m.reconnect("atl").await;
    assert_identity(m, who).await;
    call(m, "atl", "triggerListChanged", json!({})).await;
    wait_until("extraTool", || {
        m.tools().iter().any(|t| t.tool == "extraTool")
    })
    .await;
    assert_identity(m, who).await;
    expire.store(true, Ordering::SeqCst);
    assert_identity(m, who).await;
    assert!(!expire.load(Ordering::SeqCst), "the 404 was served");
    m.close().await;
}

#[tokio::test]
async fn http_bearer_is_sent_on_every_request() {
    let (url, log, expire) = serve_http(
        vec![("authorization", "Bearer tok-abc".into())],
        vec![("authorization", "Bearer tok-abc".into(), "bearer-user")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(&url, &[("Authorization", "Bearer ${TOK}")])],
        opts_with(user_env(&[("TOK", "tok-abc")]), &["TOK"]),
    );
    http_lifecycle(&m, &expire, "bearer-user").await;
    assert_all_carry(&log, "authorization", "Bearer tok-abc");
}

#[tokio::test]
async fn http_basic_authorization_works_and_is_sent_on_every_request() {
    let (url, log, expire) = serve_http(
        vec![("authorization", BASIC.into())],
        vec![("authorization", BASIC.into(), "ana@example.com")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(&url, &[("Authorization", "Basic ${B64}")])],
        opts_with(
            user_env(&[("B64", "YW5hQGV4YW1wbGUuY29tOmFwaS10b2tlbi0xMjM=")]),
            &["B64"],
        ),
    );
    http_lifecycle(&m, &expire, "ana@example.com").await;
    assert_all_carry(&log, "authorization", BASIC);
}

#[tokio::test]
async fn http_jenkins_style_basic_from_env_is_not_anonymous() {
    // Authorization: Basic base64("ci-bot:jenkins-api-token-456"), computed outside Synthesis
    let basic = "Basic Y2ktYm90OmplbmtpbnMtYXBpLXRva2VuLTQ1Ng==";
    let (url, log, expire) = serve_http(
        vec![("authorization", basic.into())],
        vec![("authorization", basic.into(), "ci-bot")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(
            &url,
            &[("Authorization", "Basic ${JENKINS_BASIC}")],
        )],
        opts_with(
            user_env(&[("JENKINS_BASIC", "Y2ktYm90OmplbmtpbnMtYXBpLXRva2VuLTQ1Ng==")]),
            &["JENKINS_BASIC"],
        ),
    );
    http_lifecycle(&m, &expire, "ci-bot").await;
    assert_all_carry(&log, "authorization", basic);
}

#[tokio::test]
async fn http_gitlab_style_private_token_and_bearer_are_not_anonymous() {
    // PRIVATE-TOKEN: custom header, required by the server on every request
    let (url, log, expire) = serve_http(
        vec![("private-token", "glpat-test-1".into())],
        vec![("private-token", "glpat-test-1".into(), "gl-user")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(&url, &[("PRIVATE-TOKEN", "${GITLAB_PAT}")])],
        opts_with(user_env(&[("GITLAB_PAT", "glpat-test-1")]), &["GITLAB_PAT"]),
    );
    http_lifecycle(&m, &expire, "gl-user").await;
    assert_all_carry(&log, "private-token", "glpat-test-1");

    // the same PAT as Authorization: Bearer
    let (url, log, expire) = serve_http(
        vec![("authorization", "Bearer glpat-test-1".into())],
        vec![("authorization", "Bearer glpat-test-1".into(), "gl-user")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(&url, &[("Authorization", "Bearer ${GITLAB_PAT}")])],
        opts_with(user_env(&[("GITLAB_PAT", "glpat-test-1")]), &["GITLAB_PAT"]),
    );
    http_lifecycle(&m, &expire, "gl-user").await;
    assert_all_carry(&log, "authorization", "Bearer glpat-test-1");

    // negative control: header omitted -> anonymous
    let (url, _, _) = serve_http(
        vec![],
        vec![("private-token", "glpat-test-1".into(), "gl-user")],
    )
    .await;
    let m = McpManager::new(vec![http_cfg(&url, &[])], opts(user_env(&[])));
    m.start().await;
    assert_identity(&m, "anonymous").await;
    m.close().await;
}

#[tokio::test]
async fn http_custom_x_and_cookie_headers_are_sent_on_every_request() {
    let (url, log, expire) = serve_http(
        vec![("x-api-key", "k-1".into()), ("cookie", "sid=abc".into())],
        vec![("x-api-key", "k-1".into(), "key-user")],
    )
    .await;
    let m = McpManager::new(
        vec![http_cfg(
            &url,
            &[("X-Api-Key", "k-1"), ("Cookie", "sid=abc")],
        )],
        opts(user_env(&[])),
    );
    http_lifecycle(&m, &expire, "key-user").await;
    assert_all_carry(&log, "x-api-key", "k-1");
    assert_all_carry(&log, "cookie", "sid=abc");
}

#[tokio::test]
async fn http_without_headers_the_server_says_anonymous_and_a_wrong_scheme_is_refused() {
    let (url, _, _) = serve_http(
        vec![],
        vec![("authorization", "Bearer tok-abc".into(), "bearer-user")],
    )
    .await;
    let m = McpManager::new(vec![http_cfg(&url, &[])], opts(user_env(&[])));
    m.start().await;
    assert_identity(&m, "anonymous").await;
    m.close().await;

    // a server that insists on credentials: Failed with 401 and no credential in the message
    let (url, _, _) = serve_http(vec![("authorization", BASIC.into())], vec![]).await;
    let m = McpManager::new(
        vec![http_cfg(
            &url,
            &[("Authorization", "Basic d3Jvbmc6d3Jvbmc=")],
        )],
        opts(user_env(&[])),
    );
    m.start().await;
    let e = m.status()[0].error.clone().unwrap();
    assert_eq!(m.status()[0].state, ServerState::Failed);
    assert!(!e.contains("d3Jvbmc6d3Jvbmc="), "{e}");

    // an empty token after the scheme is refused before any request
    let (url, log, _) = serve_http(vec![], vec![]).await;
    let m = McpManager::new(
        vec![http_cfg(&url, &[("Authorization", "Bearer ")])],
        opts(user_env(&[])),
    );
    m.start().await;
    assert!(m.status()[0].error.clone().unwrap().contains("no token"));
    assert!(log.lock().unwrap().is_empty());
}

// ---- H3: legacy SSE ----------------------------------------------------------------------------

#[derive(Clone)]
struct SseFake {
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<Event>>>>,
    endpoint: String,
    log: Log,
    want: Vec<(&'static str, String)>,
    principal: &'static str,
}

impl SseFake {
    fn authorized(&self, h: &HeaderMap) -> bool {
        self.want
            .iter()
            .all(|(k, v)| h.get(*k).and_then(|x| x.to_str().ok()) == Some(v.as_str()))
    }
    fn result(&self, method: &str, p: &Value) -> Value {
        let text = |v: Value| json!({"content": [{"type":"text","text": v.to_string()}], "structuredContent": v});
        match method {
            "initialize" => json!({"protocolVersion": p["protocolVersion"],
                "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}),
            "tools/list" => {
                let tools: Vec<Value> = ["whoami", "getPage", "updatePage", "editIssue"]
                    .iter()
                    .map(|n| json!({"name": n, "inputSchema": {"type":"object","properties":{}}}))
                    .collect();
                json!({ "tools": tools })
            }
            "tools/call" => match p["name"].as_str().unwrap_or("") {
                "whoami" => text(json!({"principal": self.principal})),
                "getPage" => {
                    text(json!({"id":"100","version":{"number":3},"author":self.principal}))
                }
                _ => text(json!({"id":"100","version":{"number":4},"author":self.principal})),
            },
            _ => json!({}),
        }
    }
}

async fn sse_stream(
    State(f): State<SseFake>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    f.log.lock().unwrap().push(Seen {
        method: "GET".into(),
        session: false,
        headers: headers.clone(),
    });
    if !f.authorized(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(Event::default().event("endpoint").data(&f.endpoint))
        .unwrap();
    *f.tx.lock().unwrap() = Some(tx);
    Ok(Sse::new(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|e| (Ok(e), rx))
    })))
}

async fn sse_post(State(f): State<SseFake>, headers: HeaderMap, body: String) -> StatusCode {
    f.log.lock().unwrap().push(Seen {
        method: "POST".into(),
        session: false,
        headers: headers.clone(),
    });
    if !f.authorized(&headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let m: Value = serde_json::from_str(&body).unwrap();
    if let (Some(id), Some(method)) = (m.get("id"), m["method"].as_str()) {
        let r = json!({"jsonrpc":"2.0","id":id,"result":f.result(method, &m["params"])});
        f.tx.lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(Event::default().event("message").data(r.to_string()))
            .unwrap();
    }
    StatusCode::ACCEPTED
}

/// `endpoint`: how the server announces the POST url given its own `host:port`.
async fn serve_sse(
    want: Vec<(&'static str, String)>,
    endpoint: impl FnOnce(&str) -> String,
) -> (String, Log) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let log = Log::default();
    let f = SseFake {
        tx: Default::default(),
        endpoint: endpoint(&addr),
        log: log.clone(),
        want,
        principal: "ana@example.com",
    };
    let app = axum::Router::new()
        .route("/sse", axum::routing::get(sse_stream))
        .route("/messages", axum::routing::post(sse_post))
        .with_state(f);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/sse"), log)
}

fn sse_cfg(url: &str, headers: &[(&str, &str)]) -> McpServerConfig {
    let mut c = http_cfg(url, headers);
    c.transport = Some(TransportKind::Sse);
    c
}

#[tokio::test]
async fn sse_get_and_every_post_carry_the_headers_relative_or_absolute_endpoint() {
    let want = vec![
        ("authorization", BASIC.to_string()),
        ("x-api-key", "k-1".to_string()),
    ];
    type Endpoint = fn(&str) -> String;
    let endpoints: [Endpoint; 2] = [
        |_| "/messages?sessionId=1".to_string(),
        |addr| format!("http://{addr}/messages?sessionId=2"),
    ];
    for endpoint in endpoints {
        let (url, log) = serve_sse(want.clone(), endpoint).await;
        let m = McpManager::new(
            vec![sse_cfg(
                &url,
                &[("Authorization", "Basic ${B64}"), ("X-Api-Key", "k-1")],
            )],
            opts_with(
                user_env(&[("B64", "YW5hQGV4YW1wbGUuY29tOmFwaS10b2tlbi0xMjM=")]),
                &["B64"],
            ),
        );
        m.start().await;
        assert_eq!(
            m.status()[0].state,
            ServerState::Connected,
            "{:?}",
            m.status()
        );
        assert_identity(&m, "ana@example.com").await;
        m.reconnect("atl").await; // second GET + new POST stream
        assert_identity(&m, "ana@example.com").await;
        m.close().await;
        let l = log.lock().unwrap();
        let (gets, posts) = (
            l.iter().filter(|s| s.method == "GET").count(),
            l.iter().filter(|s| s.method == "POST").count(),
        );
        assert!(gets >= 2 && posts >= 12, "gets={gets} posts={posts}");
        for s in l.iter() {
            let h = |n: &str| s.headers.get(n).and_then(|v| v.to_str().ok());
            assert_eq!(
                (h("authorization"), h("x-api-key")),
                (Some(BASIC), Some("k-1")),
                "{} lacks credentials",
                s.method
            );
        }
    }
}

#[tokio::test]
async fn sse_unauthenticated_posts_are_rejected_and_surface_as_a_failure() {
    let (url, _) = serve_sse(vec![("authorization", "Bearer t".into())], |_| {
        "/messages".into()
    })
    .await;
    let m = McpManager::new(vec![sse_cfg(&url, &[])], opts(user_env(&[])));
    m.start().await;
    assert!(m.status()[0].error.clone().unwrap().contains("401"));
}

#[tokio::test]
async fn sse_redirect_to_another_origin_never_carries_the_headers() {
    // B is permissive and logs; A redirects its stream to B
    let (b_url, b_log) = serve_sse(vec![], |_| "/messages".into()).await;
    let loc = b_url.clone();
    let a = axum::Router::new().route(
        "/sse",
        axum::routing::get(move || {
            let loc = loc.clone();
            async move { (StatusCode::FOUND, [(axum::http::header::LOCATION, loc)], "") }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a_url = format!("http://{}/sse", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, a).await.unwrap() });
    let m = McpManager::new(
        vec![sse_cfg(
            &a_url,
            &[
                ("Authorization", BASIC),
                ("X-Api-Key", "k-1"),
                ("Cookie", "s=1"),
            ],
        )],
        opts(user_env(&[])),
    );
    m.start().await;
    assert_eq!(
        m.status()[0].state,
        ServerState::Connected,
        "{:?}",
        m.status()
    );
    m.close().await;
    let l = b_log.lock().unwrap();
    assert!(l.iter().any(|s| s.method == "POST"));
    for s in l.iter() {
        for h in ["authorization", "x-api-key", "cookie"] {
            assert!(s.headers.get(h).is_none(), "{} leaked {h} to B", s.method);
        }
    }
}
