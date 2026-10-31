//! browser_* tools against a real Chrome-family browser and a local fixture server (the pattern of
//! crates/synthesis-browser/tests). Without a browser each test prints SKIP and passes.
#![cfg(unix)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_browser::UrlPolicy;
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::{BrowserConfig, Content, Registry, ToolOutput};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const LOGIN: &str = r#"<html><head><title>Login</title></head><body><label>Username <input id="u"></label>
<button onclick="login()">Log in</button><p id="msg"></p>
<script>async function login(){const r=await fetch('/api/login',{method:'POST',body:JSON.stringify({username:document.getElementById('u').value})});
const j=await r.json();document.getElementById('msg').textContent='Error: '+j.error}</script></body></html>"#;
const SECRET: &str = "hunter2-S3CRET-value";

async fn serve() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (status, ctype, body) = if head.starts_with("GET /login") {
                    ("200 OK", "text/html", LOGIN)
                } else if head.starts_with("POST /api/login") {
                    (
                        "422 Unprocessable Entity",
                        "application/json",
                        r#"{"error":"expected field email, got username"}"#,
                    )
                } else {
                    ("404 Not Found", "text/plain", "nope")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn no_sandbox() -> bool {
    std::env::var_os("SYNTHESIS_BROWSER_TEST_NO_SANDBOX").is_some_and(|v| !v.is_empty())
}

/// False (and prints SKIP) when no browser is installed.
fn have_browser() -> bool {
    let env = synthesis_config::Env::from_process();
    match synthesis_browser::discover(&env, synthesis_browser::Os::current(), &|p| p.is_file()) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("SKIP browser tool tests: {e}");
            false
        }
    }
}

fn registry(images: bool, private: bool) -> Registry {
    Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            policy: UrlPolicy {
                allow_private_network: private,
                ..Default::default()
            },
            no_sandbox: no_sandbox(),
            supports_images: images,
        })
}

fn ctx(dir: &std::path::Path, session: &str) -> ToolContext {
    let root = std::fs::canonicalize(dir).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], root.join("art"), "pg").unwrap();
    ToolContext {
        session_id: session.into(),
        task_id: "t".into(),
        turn_id: "u".into(),
        tool_call_id: "c".into(),
        workspace: Arc::new(ws),
        env: Arc::new(std::env::vars().collect::<BTreeMap<_, _>>()),
        read_ledger: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: PermissionMode::Default,
        skip_secrets: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

fn text(o: &ToolOutput) -> String {
    match &o.content {
        Content::Text(t) => t.clone(),
        Content::Blocks(b) => format!("{b:?}"),
    }
}

fn ref_of(tree: &str, needle: &str) -> String {
    let line = tree
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{tree}"));
    let i = line.find("[ref=").unwrap() + 5;
    line[i..line[i..].find(']').unwrap() + i].to_owned()
}

/// Pids of processes whose command line mentions a Synthesis browser profile.
fn browser_pids() -> HashSet<String> {
    let out = std::process::Command::new("pgrep")
        .args(["-f", "synthesis-browser-"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(Into::into)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn login_flow_sensitive_fill_policy_screenshot_and_session_close() {
    if !have_browser() {
        return;
    }
    let base = serve().await;
    let d = tempfile::tempdir().unwrap();
    let before = browser_pids();
    let (reg, c) = (registry(false, true), ctx(d.path(), "s1"));
    let run = |t: &'static str, i: Value| reg.execute(t, i, &c);
    let mut all = vec![]; // everything the model could see from a sensitive fill

    let o = run("browser_navigate", json!({"url": format!("{base}/login")})).await;
    assert!(!o.is_error, "{o:?}");
    let t = text(&o);
    assert!(
        t.contains("<untrusted_page_content") && t.contains("title: Login"),
        "{t}"
    );

    let tree = text(&run("browser_snapshot", json!({})).await);
    assert!(tree.starts_with("<untrusted_page_content"), "{tree}");
    let user = ref_of(&tree, "textbox \"Username\"");

    let fill = run(
        "browser_fill",
        json!({"ref": user, "value": SECRET, "sensitive": true}),
    )
    .await;
    assert!(!fill.is_error, "{fill:?}");
    all.push(format!("{fill:?}"));
    let red = reg.redact_input(
        "browser_fill",
        &json!({"ref": user, "value": SECRET, "sensitive": true}),
    );
    assert_eq!(red["value"], "[REDACTED]");
    assert_eq!(
        reg.redact_input("browser_fill", &json!({"ref": "e1", "value": "plain"}))["value"],
        "plain"
    );

    let o = run("browser_click", json!({"role": "button", "name": "Log in"})).await;
    assert!(!o.is_error, "{o:?}");
    all.push(format!("{o:?}"));
    let o = run("browser_wait_for", json!({"text": "Error:"})).await;
    assert!(!o.is_error, "{o:?}");
    let after = text(&run("browser_snapshot", json!({})).await);
    assert!(
        after.contains("expected field email, got username"),
        "{after}"
    );

    let net = text(&run("browser_network", json!({"failed_only": true})).await);
    assert!(net.contains("/api/login") && net.contains("422"), "{net}");
    let con = run("browser_console", json!({})).await;
    assert!(!con.is_error && con.meta["cursor"].is_string(), "{con:?}");
    let tabs = text(&run("browser_tabs", json!({})).await);
    assert!(tabs.contains("Login"), "{tabs}");

    assert!(
        all.iter().all(|s| !s.contains(SECRET)),
        "secret leaked: {all:?}"
    );

    // No image support: artifact path + page summary; the PNG exists.
    let o = run("browser_screenshot", json!({})).await;
    let path = o.meta["artifact_path"].as_str().unwrap().to_string();
    assert!(std::fs::metadata(&path).unwrap().len() > 100, "{path}");
    assert!(matches!(o.content, Content::Text(_)) && text(&o).contains(&path));
    // ... and artifact_read describes it instead of dumping bytes.
    let o = run("artifact_read", json!({"artifact_id": "screenshot-1"})).await;
    assert_eq!(o.meta["binary"], true, "{o:?}");

    // Unknown properties and unsupported selectors are errors.
    let o = run("browser_click", json!({"ref": "e1", "bogus": 1})).await;
    assert_eq!(o.meta["error"], "invalid_input");
    let o = run("browser_click", json!({"selector": "#x"})).await;
    assert!(o.is_error && text(&o).contains("not supported"), "{o:?}");

    // Same browser, new registry config would not apply to a live session: use a second session.
    let live: HashSet<_> = browser_pids().difference(&before).cloned().collect();
    assert!(!live.is_empty(), "browser should be running");
    reg.close_session("s1");
    wait_gone(&live).await;

    // URL policy block (default policy blocks loopback) surfaces as a tool error.
    let (reg2, c2) = (registry(true, false), ctx(d.path(), "s2"));
    let o = reg2
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login")}),
            &c2,
        )
        .await;
    assert!(o.is_error && o.meta["error"] == "url_policy", "{o:?}");
    assert!(text(&o).contains("URL policy"), "{o:?}");

    // Image-capable provider: the screenshot is an image block.
    let o = reg2
        .execute("browser_navigate", json!({"url": "about:blank"}), &c2)
        .await;
    assert!(!o.is_error, "{o:?}");
    let o = reg2
        .execute("browser_screenshot", json!({"full_page": true}), &c2)
        .await;
    let Content::Blocks(b) = &o.content else {
        panic!("{o:?}")
    };
    assert!(matches!(b[0], synthesis_tools::Block::Image { .. }));
    reg2.close_session("s2");
    wait_gone(&browser_pids().difference(&before).cloned().collect()).await;
}

async fn wait_gone(pids: &HashSet<String>) {
    for _ in 0..100 {
        if browser_pids().is_disjoint(pids) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "browser processes still alive after close_session: {:?}",
        browser_pids()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_browser_is_a_clear_tool_error() {
    let d = tempfile::tempdir().unwrap();
    let mut c = ctx(d.path(), "s3");
    c.env = Arc::new(BTreeMap::from([(
        "SYNTHESIS_BROWSER_PATH".to_string(),
        "/nonexistent/chrome".to_string(),
    )]));
    let o = registry(false, true)
        .execute("browser_snapshot", json!({}), &c)
        .await;
    assert!(
        o.is_error && o.meta["error"] == "browser_unavailable",
        "{o:?}"
    );
    assert!(text(&o).contains("SYNTHESIS_BROWSER_PATH"), "{o:?}");
}
