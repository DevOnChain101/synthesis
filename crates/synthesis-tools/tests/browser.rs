//! browser_* tools against a real Chrome-family browser and a local fixture server (the pattern of
//! crates/synthesis-browser/tests). Without a browser each test prints SKIP and passes.
#![cfg(unix)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_browser::UrlPolicy;
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::{BrowserConfig, BrowserHost, Content, LoginRequest, Registry, ToolOutput};
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
            ..Default::default()
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
        sub_agent: false,
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

// ---- browser_fill_secret / browser_login ------------------------------------------------------

const PW: &str = "Zq9-hunter2/S3CRET+value";
const PW_VAR: &str = "SYNTHESIS_TEST_LOGIN_PASSWORD";

/// The person, scripted: answers `confirm_secret` with `yes` and records what was shown.
#[derive(Debug)]
struct Person {
    yes: bool,
    asked: Mutex<Vec<(String, String)>>,
}

impl BrowserHost for Person {
    fn login<'a>(
        &'a self,
        _: &'a LoginRequest,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("not in this test".into()) })
    }
    fn confirm_secret<'a>(
        &'a self,
        secret: &'a str,
        origin: &'a str,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        self.asked
            .lock()
            .unwrap()
            .push((secret.into(), origin.into()));
        Box::pin(async move { self.yes })
    }
}

const SECRET_PAGE: &str = r#"<html><head><title>Sign in</title></head><body>
<label>Username <input id="u"></label><label>Password <input id="p" type="password"></label>
<button onclick="go()">Go</button><div id="out"></div>
<script>function go(){const v=document.getElementById('p').value;console.log('typed:'+v);
document.getElementById('out').textContent='echo:'+v;fetch('/api/x?v='+encodeURIComponent(v)).catch(()=>{})}</script>
</body></html>"#;

/// A tiny server: /login (form), /redir -> other origin /login, /framed (iframe of the other origin).
async fn serve_secret(other: Option<String>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let other = other.clone().unwrap_or_default();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let resp = if head.starts_with("GET /login") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_PAGE}",
                        SECRET_PAGE.len()
                    )
                } else if head.starts_with("GET /redir") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {other}/login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else if head.starts_with("GET /framed") {
                    let b = format!(
                        "<html><body><iframe src=\"{other}/login\"></iframe></body></html>"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        .to_string()
                };
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn secret_registry(origins: &[&str], kind: Option<&str>, host: Arc<Person>) -> Registry {
    let mut spec = json!({"source": format!("{{env:{PW_VAR}}}"), "origins": origins});
    if let Some(k) = kind {
        spec["field_kind"] = json!(k);
    }
    let (secrets, diag) =
        synthesis_browser::secret::parse_secrets(json!({"pw": spec}).as_object().unwrap());
    assert!(diag.is_empty(), "{diag:?}");
    Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            policy: UrlPolicy {
                allow_private_network: true,
                ..Default::default()
            },
            no_sandbox: no_sandbox(),
            secrets,
            secret_env: synthesis_config::Env::from_pairs([(PW_VAR, PW)]),
            host: Some(host),
            ..Default::default()
        })
}

async fn field_ref(reg: &Registry, c: &ToolContext, needle: &str) -> String {
    let t = text(&reg.execute("browser_snapshot", json!({}), c).await);
    ref_of(&t, needle)
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_secret_types_the_value_and_it_reaches_no_output() {
    if !have_browser() {
        return;
    }
    let other = serve_secret(None).await;
    let base = serve_secret(Some(other.clone())).await;
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(Person {
        yes: true,
        asked: Mutex::default(),
    });
    let reg = secret_registry(&[&base], Some("password"), person.clone());
    let c = ctx(d.path(), "sec1");
    let mut seen = vec![]; // everything the model, the events or the transcript could contain
    macro_rules! run {
        ($t:expr, $i:expr) => {{
            let o = reg.execute($t, $i, &c).await;
            seen.push(format!("{o:?}"));
            o
        }};
    }

    run!("browser_navigate", json!({"url": format!("{base}/login")}));
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    let user = field_ref(&reg, &c, "textbox \"Username\"").await;

    // a password secret never goes into a plain text field
    let o = run!("browser_fill_secret", json!({"ref": user, "secret": "pw"}));
    assert!(
        o.is_error && o.meta["error"] == "refused" && text(&o).contains("type=password"),
        "{o:?}"
    );
    // unknown secret name
    let o = run!("browser_fill_secret", json!({"ref": pw, "secret": "nope"}));
    assert!(o.is_error && text(&o).contains("no secret named"), "{o:?}");

    let o = run!("browser_fill_secret", json!({"ref": pw, "secret": "pw"}));
    assert!(!o.is_error, "{o:?}");
    let out: Value = serde_json::from_str(&text(&o)).unwrap();
    assert_eq!(out, json!({"ok": true, "filled": true, "secret": "pw"}));
    // the person saw the NAME and the origin, once per (secret, origin)
    run!("browser_fill_secret", json!({"ref": pw, "secret": "pw"}));
    assert_eq!(
        *person.asked.lock().unwrap(),
        [("pw".to_string(), base.clone())]
    );

    // the page now holds the value and echoes it into text, console and a request URL: all masked
    run!("browser_click", json!({"role": "button", "name": "Go"}));
    run!("browser_wait_for", json!({"text": "echo:"}));
    let snap = text(&run!("browser_snapshot", json!({})));
    assert!(snap.contains("echo:[REDACTED:secret]"), "{snap}");
    let con = text(&run!("browser_console", json!({})));
    assert!(con.contains("typed:[REDACTED:secret]"), "{con}");
    let net = text(&run!("browser_network", json!({})));
    assert!(net.contains("/api/x") && !net.contains("S3CRET"), "{net}");
    run!("browser_tabs", json!({}));
    run!("browser_screenshot", json!({}));
    for s in &seen {
        assert!(
            !s.contains(PW) && !s.contains("S3CRET"),
            "secret leaked: {s}"
        );
    }

    // a redirect to another origin: the page is no longer the bound one
    run!("browser_navigate", json!({"url": format!("{base}/redir")}));
    let pw2 = field_ref(&reg, &c, "textbox \"Password\"").await;
    let o = run!("browser_fill_secret", json!({"ref": pw2, "secret": "pw"}));
    assert!(o.is_error && text(&o).contains("is bound to"), "{o:?}");
    assert!(text(&o).contains(&other), "{o:?}");

    // the right page with a cross-origin iframe: the iframe's field is refused
    run!("browser_navigate", json!({"url": format!("{base}/framed")}));
    let snap = text(&run!("browser_snapshot", json!({})));
    if let Some(line) = snap.lines().find(|l| l.contains("textbox \"Password\"")) {
        let r = line
            .split("[ref=")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap();
        let o = run!("browser_fill_secret", json!({"ref": r, "secret": "pw"}));
        assert!(o.is_error && text(&o).contains("cross-origin"), "{o:?}");
    } else {
        eprintln!(
            "NOTE: this Chrome's AX tree hides cross-origin iframe fields: refusal not exercised"
        );
    }
    for s in &seen {
        assert!(!s.contains(PW), "secret leaked: {s}");
    }
    reg.close_session("sec1");
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_secret_needs_the_person_the_binding_and_a_value() {
    if !have_browser() {
        return;
    }
    let base = serve_secret(None).await;
    let d = tempfile::tempdir().unwrap();
    // the person says no: nothing is typed
    let no = Arc::new(Person {
        yes: false,
        asked: Mutex::default(),
    });
    let reg = secret_registry(&[&base], None, no.clone());
    let c = ctx(d.path(), "sec2");
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    let o = reg
        .execute(
            "browser_fill_secret",
            json!({"ref": pw, "secret": "pw"}),
            &c,
        )
        .await;
    assert!(o.is_error && text(&o).contains("did not approve"), "{o:?}");
    assert_eq!(no.asked.lock().unwrap().len(), 1);
    reg.close_session("sec2");

    // bound to another origin (same host, other port): refused without asking anyone
    let yes = Arc::new(Person {
        yes: true,
        asked: Mutex::default(),
    });
    let reg = secret_registry(&["http://127.0.0.1:9"], None, yes.clone());
    let c = ctx(d.path(), "sec3");
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    let o = reg
        .execute(
            "browser_fill_secret",
            json!({"ref": pw, "secret": "pw"}),
            &c,
        )
        .await;
    assert!(o.is_error && text(&o).contains("not one secret"), "{o:?}");
    assert!(yes.asked.lock().unwrap().is_empty());
    reg.close_session("sec3");

    // the variable is unset: the error names the source, never a value; nobody is asked
    let reg = secret_registry(&[&base], None, yes.clone()).with_browser(BrowserConfig {
        policy: UrlPolicy {
            allow_private_network: true,
            ..Default::default()
        },
        no_sandbox: no_sandbox(),
        secrets: synthesis_browser::secret::parse_secrets(
            json!({"pw": {"source": format!("{{env:{PW_VAR}}}"), "origins": [base]}})
                .as_object()
                .unwrap(),
        )
        .0,
        host: Some(yes.clone()),
        ..Default::default()
    });
    let c = ctx(d.path(), "sec4");
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    let o = reg
        .execute(
            "browser_fill_secret",
            json!({"ref": pw, "secret": "pw"}),
            &c,
        )
        .await;
    assert!(
        o.is_error && text(&o).contains(PW_VAR) && text(&o).contains("missing or empty"),
        "{o:?}"
    );
    assert!(yes.asked.lock().unwrap().is_empty());
    reg.close_session("sec4");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_login_checks_come_before_any_browser_starts() {
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let person = Arc::new(Person {
        yes: true,
        asked: Mutex::default(),
    });
    let cfg = |home: Option<std::path::PathBuf>, host: Option<Arc<Person>>| BrowserConfig {
        home,
        host: host.map(|h| h as Arc<dyn BrowserHost>),
        ..Default::default()
    };
    let run = |cfg: BrowserConfig, i: Value, sub: bool| {
        let reg = Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_browser(cfg);
        let mut c = ctx(d.path(), "login1");
        c.sub_agent = sub;
        async move { reg.execute("browser_login", i, &c).await }
    };
    let good = cfg(Some(home.path().to_owned()), Some(person.clone()));
    for bad in [
        "http://example.com",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user:pw@example.com",
        "https://169.254.169.254",
        "https://10.0.0.1",
        "https://example.com/path",
    ] {
        let o = run(good.clone(), json!({"origin": bad}), false).await;
        assert!(
            o.is_error && text(&o).contains("login origin"),
            "{bad}: {o:?}"
        );
    }
    for bad in ["../x", "A", "a b", "a/b"] {
        let o = run(
            good.clone(),
            json!({"origin": "https://example.com", "profile": bad}),
            false,
        )
        .await;
        assert!(
            o.is_error && text(&o).contains("invalid profile name"),
            "{bad}: {o:?}"
        );
    }
    let o = run(good.clone(), json!({"origin": "https://example.com"}), true).await;
    assert!(o.is_error && text(&o).contains("sub-agents"), "{o:?}");
    // nobody to ask: points at option B (on Linux without a display the display error comes first)
    let o = run(
        cfg(Some(home.path().to_owned()), None),
        json!({"origin": "https://example.com"}),
        false,
    )
    .await;
    assert!(
        o.is_error && text(&o).contains("browser_fill_secret"),
        "{o:?}"
    );
    assert!(
        !home.path().join("browser-profiles").exists(),
        "nothing was created"
    );
    assert!(person.asked.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_must_exist_and_limits_the_domains() {
    if !have_browser() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let base = serve_secret(None).await;
    let mk = || {
        Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_browser(BrowserConfig {
                policy: UrlPolicy {
                    allow_private_network: true,
                    ..Default::default()
                },
                no_sandbox: no_sandbox(),
                home: Some(home.path().to_owned()),
                ..Default::default()
            })
    };
    // no such profile
    let reg = mk();
    let c = ctx(d.path(), "p1");
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login"), "profile": "acme"}),
            &c,
        )
        .await;
    assert!(o.is_error && text(&o).contains("log in first"), "{o:?}");
    // a path is not a profile name
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login"), "profile": "../../x"}),
            &c,
        )
        .await;
    assert!(
        o.is_error && text(&o).contains("invalid profile name"),
        "{o:?}"
    );

    // a saved profile: navigation is limited to the domains the person logged in to
    let dir = synthesis_browser::profile::create(home.path(), "acme").unwrap();
    synthesis_browser::profile::add_domain(&dir, &url::Url::parse("http://127.0.0.1").unwrap())
        .unwrap();
    let c = ctx(d.path(), "p2");
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login"), "profile": "acme"}),
            &c,
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": base.replace("127.0.0.1", "localhost") + "/login"}),
            &c,
        )
        .await;
    assert!(
        o.is_error && o.meta["error"] == "url_policy" && text(&o).contains("profile's domains"),
        "{o:?}"
    );
    // a second session on the same profile fails cleanly while the first is open
    let c3 = ctx(d.path(), "p3");
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login"), "profile": "acme"}),
            &c3,
        )
        .await;
    assert!(o.is_error && text(&o).contains("already open"), "{o:?}");
    // the profile is chosen by the first call only
    let c4 = ctx(d.path(), "p4");
    reg.execute("browser_navigate", json!({"url": "about:blank"}), &c4)
        .await;
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": "about:blank", "profile": "acme"}),
            &c4,
        )
        .await;
    assert!(
        o.is_error && text(&o).contains("first browser call"),
        "{o:?}"
    );
    for sid in ["p2", "p4"] {
        reg.close_session(sid);
    }
}
