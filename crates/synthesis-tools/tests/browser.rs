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
    std::env::var_os("SYNT_BROWSER_TEST_NO_SANDBOX").is_some_and(|v| !v.is_empty())
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

/// Asserts a tool call succeeded; the failure message carries the tool's own output.
fn ok(o: &ToolOutput, what: &str) {
    assert!(!o.is_error, "{what}: {}", text(o));
}

fn ref_of(tree: &str, needle: &str) -> String {
    let line = tree
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{tree}"));
    let i = line.find("[ref=").unwrap() + 5;
    line[i..line[i..].find(']').unwrap() + i].to_owned()
}

/// A launcher that adds `--synt-test-tag=<tag>` to the real browser's command line (`exec` keeps the pid), so
/// a test can find ITS browser among those of the tests running in parallel in this binary.
fn tagged_launcher(dir: &std::path::Path, tag: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let real = synthesis_browser::discover(
        &synthesis_config::Env::from_process(),
        synthesis_browser::Os::current(),
        &|p| p.is_file(),
    )
    .unwrap();
    let sh = dir.join("tagged-browser");
    std::fs::write(
        &sh,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\" --synt-test-tag={tag}\n",
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    sh.to_string_lossy().into_owned()
}

fn pgrep(args: &[&str]) -> Vec<String> {
    let out = std::process::Command::new("pgrep")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(Into::into)
        .collect()
}

/// The tagged browser and every process below it (its helpers re-parent once it exits, so take this while it lives).
fn tagged_tree(tag: &str) -> HashSet<String> {
    let mut all: HashSet<String> = pgrep(&["-f", &format!("synt-test-tag={tag}")])
        .into_iter()
        .collect();
    let mut todo: Vec<String> = all.iter().cloned().collect();
    while let Some(p) = todo.pop() {
        for c in pgrep(&["-P", &p]) {
            if all.insert(c.clone()) {
                todo.push(c);
            }
        }
    }
    all
}

fn alive(pid: &str) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[tokio::test(flavor = "multi_thread")]
async fn login_flow_sensitive_fill_policy_screenshot_and_session_close() {
    if !have_browser() {
        return;
    }
    let base = serve().await;
    let d = tempfile::tempdir().unwrap();
    let tag = format!("login-{}", std::process::id());
    let launcher = tagged_launcher(d.path(), &tag);
    let (reg, mut c) = (registry(false, true), ctx(d.path(), "s1"));
    c.env = Arc::new(BTreeMap::from([(
        "SYNT_BROWSER_PATH".to_owned(),
        launcher.clone(),
    )]));
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
    let path = o.meta["artifact_path"]
        .as_str()
        .unwrap_or_else(|| panic!("no screenshot: {o:?}"))
        .to_string();
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
    let live = tagged_tree(&tag);
    assert!(!live.is_empty(), "browser should be running");
    reg.close_session("s1");
    wait_gone(&live).await;

    // URL policy block (default policy blocks loopback) surfaces as a tool error.
    let (reg2, mut c2) = (registry(true, false), ctx(d.path(), "s2"));
    c2.env = c.env.clone();
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
    let live = tagged_tree(&tag);
    assert!(!live.is_empty(), "the second browser should be running");
    reg2.close_session("s2");
    wait_gone(&live).await;
}

async fn wait_gone(pids: &HashSet<String>) {
    for _ in 0..100 {
        if !pids.iter().any(|p| alive(p)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let left: Vec<_> = pids.iter().filter(|p| alive(p)).cloned().collect();
    let ps = std::process::Command::new("ps")
        .args([
            "-o",
            "pid,ppid,pgid,stat,etime,command",
            "-p",
            &left.join(","),
        ])
        .output()
        .unwrap();
    panic!(
        "browser processes still alive after close_session:\n{}",
        String::from_utf8_lossy(&ps.stdout)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_browser_is_a_clear_tool_error() {
    let d = tempfile::tempdir().unwrap();
    let mut c = ctx(d.path(), "s3");
    c.env = Arc::new(BTreeMap::from([(
        "SYNT_BROWSER_PATH".to_string(),
        "/nonexistent/chrome".to_string(),
    )]));
    let o = registry(false, true)
        .execute("browser_snapshot", json!({}), &c)
        .await;
    assert!(
        o.is_error && o.meta["error"] == "browser_unavailable",
        "{o:?}"
    );
    assert!(text(&o).contains("SYNT_BROWSER_PATH"), "{o:?}");
}

// ---- browser_fill_secret / browser_login ------------------------------------------------------

const PW: &str = "Zq9-hunter2/S3CRET+value";
const PW_VAR: &str = "SYNT_TEST_LOGIN_PASSWORD";

/// The person, scripted: answers `confirm_secret` with `yes` and records what was shown.
#[derive(Debug)]
struct Person {
    yes: bool,
    asked: Mutex<Vec<(String, String)>>,
    /// (profile, domains shown) of every `confirm_profile`
    profiles: Mutex<Vec<(String, Vec<String>)>>,
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
    fn confirm_profile<'a>(
        &'a self,
        profile: &'a str,
        domains: &'a [String],
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        self.profiles
            .lock()
            .unwrap()
            .push((profile.into(), domains.to_vec()));
        Box::pin(async move { self.yes })
    }
}

/// The real process environment plus extra variables (what `synt run` hands the tools).
fn proc_env(extra: &[(&str, &str)]) -> synthesis_config::Env {
    synthesis_config::Env::from_pairs(
        std::env::vars().chain(extra.iter().map(|(k, v)| (k.to_string(), v.to_string()))),
    )
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
    secret_registry_with(origins, kind, host, PW)
}

fn secret_registry_with(
    origins: &[&str],
    kind: Option<&str>,
    host: Arc<Person>,
    value: &str,
) -> Registry {
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
            process_env: Some(proc_env(&[(PW_VAR, value)])),
            allow_unsandboxed: true,
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
        profiles: Mutex::default(),
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
    // pixels cannot be masked: no screenshot while the page that took the secret is still up
    let o = run!("browser_screenshot", json!({}));
    assert!(
        o.is_error && o.meta["error"] == "refused" && text(&o).contains("until the page navigates"),
        "{o:?}"
    );
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
        profiles: Mutex::default(),
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
        profiles: Mutex::default(),
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
        process_env: Some(proc_env(&[])),
        allow_unsandboxed: true,
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
        profiles: Mutex::default(),
    });
    let cfg = |home: Option<std::path::PathBuf>, host: Option<Arc<Person>>| BrowserConfig {
        home,
        host: host.map(|h| h as Arc<dyn BrowserHost>),
        allow_unsandboxed: true,
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
    let person = Arc::new(Person {
        yes: true,
        asked: Mutex::default(),
        profiles: Mutex::default(),
    });
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
                allow_unsandboxed: true,
                host: Some(person.clone()),
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
    // the person was shown the profile name and the domains it holds, before the browser started
    assert_eq!(
        person.profiles.lock().unwrap()[0],
        ("acme".to_string(), vec!["127.0.0.1".to_string()])
    );
}

fn person(yes: bool) -> Arc<Person> {
    Arc::new(Person {
        yes,
        asked: Mutex::default(),
        profiles: Mutex::default(),
    })
}

fn saved_profile(home: &std::path::Path) {
    let dir = synthesis_browser::profile::create(home, "acme").unwrap();
    synthesis_browser::profile::add_domain(&dir, &url::Url::parse("http://127.0.0.1").unwrap())
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_tools_refuse_without_a_sandbox_unless_the_user_allows_it() {
    let (d, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    saved_profile(home.path());
    let p = person(true);
    let mk = |sandboxed: bool, allow: bool| {
        Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_browser(BrowserConfig {
                home: Some(home.path().to_owned()),
                host: Some(p.clone()),
                process_env: Some(proc_env(&[])),
                sandboxed,
                allow_unsandboxed: allow,
                ..Default::default()
            })
    };
    let reg = mk(false, false);
    let c = ctx(d.path(), "sb1");
    for (tool, input) in [
        ("browser_login", json!({"origin": "https://example.com"})),
        ("browser_fill_secret", json!({"ref": "e1", "secret": "pw"})),
        (
            "browser_navigate",
            json!({"url": "about:blank", "profile": "acme"}),
        ),
    ] {
        let o = reg.execute(tool, input, &c).await;
        assert!(
            o.is_error && text(&o).contains("--sandbox enforce"),
            "{tool}: {o:?}"
        );
    }
    assert!(
        p.profiles.lock().unwrap().is_empty(),
        "nobody was bothered, no browser started"
    );
    // the sandbox is enforced: the sandbox check passes (the next refusal is a different one)
    let o = mk(true, false)
        .execute(
            "browser_fill_secret",
            json!({"ref": "e1", "secret": "pw"}),
            &ctx(d.path(), "sb2"),
        )
        .await;
    assert!(!text(&o).contains("--sandbox enforce"), "{o:?}");
    let _ = o;
    // a plain navigation without a profile never needed it
    let c = ctx(d.path(), "sb3");
    let o = mk(false, false)
        .execute("browser_navigate", json!({"url": "about:blank"}), &c)
        .await;
    assert!(!text(&o).contains("--sandbox enforce"), "{o:?}");
    mk(false, false).close_session("sb3");
}

#[tokio::test(flavor = "multi_thread")]
async fn using_a_saved_login_needs_the_person_unless_pre_approved() {
    if !have_browser() {
        return;
    }
    let (d, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    saved_profile(home.path());
    let base = serve_secret(None).await;
    let mk = |p: &Arc<Person>, pre: &[&str]| {
        Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_browser(BrowserConfig {
                policy: UrlPolicy {
                    allow_private_network: true,
                    ..Default::default()
                },
                no_sandbox: no_sandbox(),
                home: Some(home.path().to_owned()),
                host: Some(p.clone()),
                allow_unsandboxed: true,
                pre_approved_profiles: pre.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
    };
    let nav = json!({"url": format!("{base}/login"), "profile": "acme"});
    // the person says no: refused, no browser, and not even bypass/auto can matter (not the engine's call)
    let no = person(false);
    let reg = mk(&no, &[]);
    let c = ctx(d.path(), "ap1");
    let o = reg.execute("browser_navigate", nav.clone(), &c).await;
    assert!(o.is_error && text(&o).contains("did not approve"), "{o:?}");
    assert_eq!(no.profiles.lock().unwrap().len(), 1);
    // yes: asked once for the session, however many calls follow
    let yes = person(true);
    let reg = mk(&yes, &[]);
    let c = ctx(d.path(), "ap2");
    let o = reg.execute("browser_navigate", nav.clone(), &c).await;
    ok(&o, "navigate after the person said yes");
    reg.execute("browser_snapshot", json!({}), &c).await;
    assert_eq!(yes.profiles.lock().unwrap().len(), 1);
    reg.close_session("ap2");
    // pre-approved in user settings: nobody is asked (a run with no one to ask still works)
    let silent = person(false);
    let reg = mk(&silent, &["acme"]);
    let c = ctx(d.path(), "ap3");
    let o = reg.execute("browser_navigate", nav, &c).await;
    ok(&o, "navigate with a pre-approved profile");
    assert!(silent.profiles.lock().unwrap().is_empty());
    reg.close_session("ap3");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_is_found_through_the_process_env_not_the_project_env() {
    let d = tempfile::tempdir().unwrap();
    // the tool env (settings `env`, project-controlled) names a good path; the process env a bad one
    let mut c = ctx(d.path(), "env1");
    c.env = Arc::new(BTreeMap::from([(
        "SYNT_BROWSER_PATH".to_string(),
        "/project/chose/this/browser".to_string(),
    )]));
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            process_env: Some(synthesis_config::Env::from_pairs([(
                "SYNT_BROWSER_PATH",
                "/process/env/browser",
            )])),
            ..Default::default()
        });
    let o = reg.execute("browser_snapshot", json!({}), &c).await;
    assert!(
        o.is_error && o.meta["error"] == "browser_unavailable",
        "{o:?}"
    );
    let t = text(&o);
    assert!(
        t.contains("/process/env/browser") && !t.contains("/project/chose"),
        "{t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_approved_secret_pairs_skip_the_prompt_and_only_those() {
    if !have_browser() {
        return;
    }
    let base = serve_secret(None).await;
    let d = tempfile::tempdir().unwrap();
    let nobody = person(false);
    let reg = secret_registry(&[&base], None, nobody.clone()).with_browser(BrowserConfig {
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
        process_env: Some(proc_env(&[(PW_VAR, PW)])),
        allow_unsandboxed: true,
        host: Some(nobody.clone()),
        pre_approved_secrets: [("pw".to_string(), base.clone())].into(),
        ..Default::default()
    });
    let c = ctx(d.path(), "pa1");
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
    assert!(!o.is_error, "{o:?}");
    assert!(nobody.asked.lock().unwrap().is_empty());
    reg.close_session("pa1");
}

fn fields_page(other: &str) -> String {
    format!(
        r#"<html><body style="margin:0">
<form action="/login"><input aria-label="good"></form>
<div style="opacity:0"><input aria-label="faded"></div>
<input aria-label="offscreen" style="position:absolute;left:-5000px;top:100px">
<div style="position:relative;width:200px"><input aria-label="covered"><div style="position:absolute;inset:0;background:red"></div></div>
<div style="margin-left:100px;width:60px;height:30px;overflow:hidden"><input aria-label="clipped" style="width:100px"></div>
<div style="clip-path:inset(0 90% 0 0)"><input aria-label="clip-path"></div>
<input aria-label="tiny" style="width:2px;height:2px;padding:0;border:0">
<input aria-label="locked" readonly><input aria-label="off" disabled>
<form action="{other}/login"><input aria-label="foreign-action"></form>
<form action="/login"><input aria-label="foreign-formaction"><button formaction="{other}/x">go</button></form>
<input aria-label="foreign-form-attr" form="f2"><form id="f2" action="{other}/z"></form>
</body></html>"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_real_visible_field_of_a_safe_form_receives_the_secret() {
    if !have_browser() {
        return;
    }
    let other = serve_secret(None).await;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let page = fields_page(&other);
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let page = page.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await;
                let r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = s.write_all(r.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    let base = format!("http://{addr}");
    let d = tempfile::tempdir().unwrap();
    let reg = secret_registry(&[&base], None, person(true));
    let c = ctx(d.path(), "fld1");
    reg.execute("browser_navigate", json!({"url": format!("{base}/")}), &c)
        .await;
    let tree = text(&reg.execute("browser_snapshot", json!({}), &c).await);
    let fill = |label: &str| {
        let r = ref_of(&tree, &format!("textbox \"{label}\""));
        let (reg, c) = (&reg, &c);
        async move {
            reg.execute("browser_fill_secret", json!({"ref": r, "secret": "pw"}), c)
                .await
        }
    };
    for (label, why) in [
        ("faded", "hidden"),
        ("offscreen", "viewport"),
        ("covered", "covers"),
        ("clipped", "clipped"),
        ("clip-path", "clipped"),
        ("tiny", "too small"),
        ("locked", "editable"),
        ("foreign-action", "another origin"),
        ("foreign-formaction", "another origin"),
        ("foreign-form-attr", "another origin"),
    ] {
        let o = fill(label).await;
        assert!(
            o.is_error && o.meta["error"] == "refused" && text(&o).contains(why),
            "{label}: {o:?}"
        );
        assert!(text(&o).contains("nothing was typed"), "{label}: {o:?}");
    }
    let o = fill("good").await;
    assert!(!o.is_error, "{o:?}");
    reg.close_session("fld1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_secret_blocks_snapshots_and_screenshots_until_the_page_navigates() {
    if !have_browser() {
        return;
    }
    let base = serve_secret(None).await;
    let d = tempfile::tempdir().unwrap();
    let reg = secret_registry_with(&[&base], None, person(true), "abc");
    let c = ctx(d.path(), "short1");
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
    assert!(!o.is_error, "{o:?}");
    for t in ["browser_snapshot", "browser_screenshot"] {
        let o = reg.execute(t, json!({}), &c).await;
        assert!(
            o.is_error && text(&o).contains("until the page navigates"),
            "{t}: {o:?}"
        );
    }
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let o = reg.execute("browser_snapshot", json!({}), &c).await;
    ok(&o, "snapshot");
    reg.close_session("short1");
}

const USER_VAR: &str = "SYNT_TEST_USER_TOKEN";

/// Two secrets on one origin: a SHORT username (unmaskable) and the long password.
fn two_secret_registry(base: &str, host: Arc<Person>) -> Registry {
    let (secrets, diag) = synthesis_browser::secret::parse_secrets(
        json!({
            "user": {"source": format!("{{env:{USER_VAR}}}"), "origins": [base], "field_kind": "username"},
            "pw": {"source": format!("{{env:{PW_VAR}}}"), "origins": [base], "field_kind": "password"},
        })
        .as_object()
        .unwrap(),
    );
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
            process_env: Some(proc_env(&[(USER_VAR, "abc"), (PW_VAR, PW)])),
            allow_unsandboxed: true,
            host: Some(host),
            ..Default::default()
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_secret_keeps_the_guard_after_a_long_one_and_covers_console_network_tabs() {
    if !have_browser() {
        return;
    }
    let base = serve_secret(None).await;
    let d = tempfile::tempdir().unwrap();
    let reg = two_secret_registry(&base, person(true));
    let c = ctx(d.path(), "g1");
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let user = field_ref(&reg, &c, "textbox \"Username\"").await;
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    for (r, name) in [(&user, "user"), (&pw, "pw")] {
        let o = reg
            .execute("browser_fill_secret", json!({"ref": r, "secret": name}), &c)
            .await;
        assert!(!o.is_error, "{name}: {o:?}");
    }
    // the long password did not release the guard the short username armed
    for t in [
        "browser_snapshot",
        "browser_screenshot",
        "browser_console",
        "browser_network",
        "browser_tabs",
    ] {
        let o = reg.execute(t, json!({}), &c).await;
        assert!(
            o.is_error && text(&o).contains("until the page navigates"),
            "{t}: {o:?}"
        );
    }
    // a same-document navigation keeps it armed: the page text is withheld, not returned
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": format!("{base}/login#x")}),
            &c,
        )
        .await;
    assert!(
        text(&o).contains("withheld") && !text(&o).contains("Sign in"),
        "{o:?}"
    );
    // a real navigation releases it
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    for t in [
        "browser_snapshot",
        "browser_screenshot",
        "browser_console",
        "browser_network",
        "browser_tabs",
    ] {
        let o = reg.execute(t, json!({}), &c).await;
        ok(&o, t);
    }
    reg.close_session("g1");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_person_is_not_asked_a_third_time_after_two_refusals() {
    if !have_browser() {
        return;
    }
    let base = serve_secret(None).await;
    let d = tempfile::tempdir().unwrap();
    let no = person(false);
    let reg = two_secret_registry(&base, no.clone());
    let c = ctx(d.path(), "rf1");
    reg.execute(
        "browser_navigate",
        json!({"url": format!("{base}/login")}),
        &c,
    )
    .await;
    let pw = field_ref(&reg, &c, "textbox \"Password\"").await;
    let mut last = String::new();
    for _ in 0..4 {
        let o = reg
            .execute(
                "browser_fill_secret",
                json!({"ref": pw, "secret": "pw"}),
                &c,
            )
            .await;
        assert!(o.is_error, "expected an error: {}", text(&o));
        last = text(&o);
    }
    assert_eq!(
        no.asked.lock().unwrap().len(),
        2,
        "asked twice, then silent"
    );
    assert!(
        last.contains("twice") && last.contains("not asking again"),
        "{last}"
    );
    reg.close_session("rf1");
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_login_refusals_are_cached_and_stdio_mcp_blocks_the_credential_tools() {
    let (d, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    saved_profile(home.path());
    let no = person(false);
    let mk = |mcp: &[&str], allow: bool| {
        Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_browser(BrowserConfig {
                home: Some(home.path().to_owned()),
                host: Some(no.clone()),
                process_env: Some(proc_env(&[])),
                sandboxed: true,
                mcp_stdio: mcp.iter().map(|s| s.to_string()).collect(),
                allow_with_mcp: allow,
                ..Default::default()
            })
    };
    // two refusals, then silence (no browser was started: the question comes first)
    let reg = mk(&[], false);
    let c = ctx(d.path(), "pr1");
    let nav = json!({"url": "about:blank", "profile": "acme"});
    let mut last = String::new();
    for _ in 0..3 {
        let o = reg.execute("browser_navigate", nav.clone(), &c).await;
        assert!(o.is_error, "expected an error: {}", text(&o));
        last = text(&o);
    }
    assert_eq!(no.profiles.lock().unwrap().len(), 2);
    assert!(last.contains("twice"), "{last}");
    // a stdio MCP server runs outside the sandbox: all three entry points refuse, naming it
    let reg = mk(&["files", "git"], false);
    let c = ctx(d.path(), "pr2");
    for (tool, input) in [
        ("browser_login", json!({"origin": "https://example.com"})),
        ("browser_fill_secret", json!({"ref": "e1", "secret": "pw"})),
        ("browser_navigate", nav.clone()),
    ] {
        let o = reg.execute(tool, input, &c).await;
        let t = text(&o);
        assert!(
            o.is_error && t.contains("files, git") && t.contains("allow_with_mcp"),
            "{tool}: {t}"
        );
    }
    assert_eq!(
        no.profiles.lock().unwrap().len(),
        2,
        "refused before anyone was asked"
    );
    // the user's setting lifts exactly that check
    let o = mk(&["files"], true)
        .execute("browser_navigate", nav, &ctx(d.path(), "pr3"))
        .await;
    assert!(!text(&o).contains("allow_with_mcp"), "{o:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_cannot_choose_a_saved_login_and_nobody_is_asked() {
    let (d, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    saved_profile(home.path());
    let p = person(true);
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            home: Some(home.path().to_owned()),
            host: Some(p.clone()),
            process_env: Some(proc_env(&[])),
            allow_unsandboxed: true,
            ..Default::default()
        });
    let mut c = ctx(d.path(), "sub1");
    c.sub_agent = true;
    let o = reg
        .execute(
            "browser_navigate",
            json!({"url": "about:blank", "profile": "acme"}),
            &c,
        )
        .await;
    assert!(o.is_error && text(&o).contains("sub-agents"), "{o:?}");
    assert!(
        p.profiles.lock().unwrap().is_empty(),
        "refused before any dialog"
    );
}

/// The person for loopback questions: approves only the listed `host:port` targets and records every question.
#[derive(Debug, Default)]
struct LoopbackPerson {
    approve: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
}

impl BrowserHost for LoopbackPerson {
    fn login<'a>(
        &'a self,
        _: &'a LoginRequest,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("not in this test".into()) })
    }
    fn confirm_profile<'a>(
        &'a self,
        _: &'a str,
        _: &'a [String],
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn confirm_secret<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn confirm_loopback<'a>(
        &'a self,
        target: &'a str,
        _: &'a str,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        self.asked.lock().unwrap().push(target.to_owned());
        Box::pin(async move { self.approve.lock().unwrap().iter().any(|a| a == target) })
    }
}

fn strict_registry(host: Option<Arc<dyn BrowserHost>>) -> Registry {
    Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            no_sandbox: no_sandbox(),
            host,
            ..Default::default()
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reaches_loopback_only_on_the_target_the_person_approved() {
    if !have_browser() {
        return;
    }
    let (a, b) = (serve().await, serve().await);
    let (ta, tb) = (a.replace("http://", ""), b.replace("http://", ""));
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(LoopbackPerson {
        approve: Mutex::new(vec![ta.clone()]),
        ..Default::default()
    });
    let reg = strict_registry(Some(person.clone()));
    let c = ctx(d.path(), "lb1");
    let go = |u: String| json!({"url": u});
    let o = reg
        .execute("browser_navigate", go(format!("{a}/login")), &c)
        .await;
    ok(&o, "approved target");
    // another port of loopback needs its own approval, which the person withholds
    let o = reg
        .execute("browser_navigate", go(format!("{b}/login")), &c)
        .await;
    assert!(o.is_error && text(&o).contains("approves"), "{o:?}");
    // LAN and metadata addresses have no approval path: nobody is asked
    for u in [
        "http://169.254.169.254/",
        "http://10.0.0.1:3000/",
        "http://192.168.1.1/",
    ] {
        let o = reg.execute("browser_navigate", go(u.into()), &c).await;
        assert!(o.is_error && text(&o).contains("private"), "{u}: {o:?}");
    }
    assert_eq!(*person.asked.lock().unwrap(), vec![ta, tb]);
    reg.close_session("lb1");

    // nobody to ask (headless without a terminal): it stays blocked, and the error says so
    let reg = strict_registry(None);
    let c = ctx(d.path(), "lb2");
    let o = reg
        .execute("browser_navigate", go(format!("{a}/login")), &c)
        .await;
    assert!(
        o.is_error && text(&o).contains("nobody can be asked"),
        "{o:?}"
    );
    reg.close_session("lb2");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loopback_page_the_person_opened_is_not_the_agents_until_approved() {
    if !have_browser() {
        return;
    }
    let a = serve().await;
    let ta = a.replace("http://", "");
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(LoopbackPerson::default());
    let tap = Arc::new(synthesis_tools::BrowserTap::default());
    let cfg = BrowserConfig {
        no_sandbox: no_sandbox(),
        host: Some(person.clone()),
        view: Some(tap.clone()),
        ..Default::default()
    };
    tap.enable_start(
        cfg.clone(),
        d.path().join("art"),
        synthesis_config::Env::from_process(),
    );
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(cfg);
    // the person starts the browser from the tab and opens the dev server
    tap.start().await.unwrap();
    let view = tap.view().unwrap();
    view.navigate(&format!("{a}/login"), synthesis_browser::WaitUntil::Load)
        .await
        .unwrap();
    // the agent adopts that browser, but may not read or act on the person's loopback page
    let c = ctx(d.path(), "lb3");
    let o = reg.execute("browser_snapshot", json!({}), &c).await;
    assert!(o.is_error && text(&o).contains("approves"), "{o:?}");
    assert_eq!(*person.asked.lock().unwrap(), vec![ta.clone()]);
    // after approval the same page works for the agent
    person.approve.lock().unwrap().push(ta);
    let o = reg.execute("browser_snapshot", json!({}), &c).await;
    ok(&o, "snapshot after approval");
    ok(&reg.execute("browser_tabs", json!({}), &c).await, "tabs");
    reg.close_session("lb3");
}

/// A dev server whose `/m` page logs, fetches and throws markers named after `tag`.
async fn serve_markers(tag: &'static str) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = if head.starts_with("GET /m ") {
                    format!(
                        "<html><head><title>TITLE_{tag}</title></head><body><script>console.log('CONSOLE_{tag}');fetch('/PATH_{tag}');setTimeout(()=>{{throw new Error('ERR_{tag}')}},10)</script></body></html>"
                    )
                } else {
                    "nope".to_owned()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_wide_console_network_and_tabs_need_approval_for_a_page_the_person_showed() {
    if !have_browser() {
        return;
    }
    let a = serve_markers("A").await;
    let ta = a.replace("http://", "");
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(LoopbackPerson::default());
    let tap = Arc::new(synthesis_tools::BrowserTap::default());
    let cfg = BrowserConfig {
        no_sandbox: no_sandbox(),
        host: Some(person.clone()),
        view: Some(tap.clone()),
        ..Default::default()
    };
    tap.enable_start(
        cfg.clone(),
        d.path().join("art"),
        synthesis_config::Env::from_process(),
    );
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(cfg);
    tap.start().await.unwrap();
    // the person opens the dev server; its page logs, fetches and throws
    tap.view()
        .unwrap()
        .navigate(&format!("{a}/m"), synthesis_browser::WaitUntil::Load)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    // the agent moves the same tab to a page that needs no approval
    let c0 = ctx(d.path(), "rw0");
    ok(
        &reg.execute("browser_navigate", json!({"url": "about:blank"}), &c0)
            .await,
        "about:blank",
    );
    // (1)(2)(3) every runtime-wide reader is refused without approval and shows nothing of the person's page
    for (n, (tool, input)) in [
        ("browser_console", json!({})),
        ("browser_network", json!({})),
        ("browser_tabs", json!({})),
        ("browser_tabs", json!({"action": "select", "index": 0})),
    ]
    .into_iter()
    .enumerate()
    {
        let c = ctx(d.path(), &format!("rw-no-{n}"));
        let o = reg.execute(tool, input, &c).await;
        let t = text(&o);
        assert!(o.is_error && t.contains("approves"), "{tool}: {o:?}");
        for m in ["CONSOLE_A", "ERR_A", "PATH_A", "TITLE_A"] {
            assert!(!t.contains(m), "{tool} leaked {m}: {t}");
        }
    }
    assert!(person.asked.lock().unwrap().iter().all(|t| *t == ta));
    // after the person approves, console (with the page error), network and tabs work
    person.approve.lock().unwrap().push(ta.clone());
    let c = ctx(d.path(), "rw-yes");
    let console = text(&reg.execute("browser_console", json!({}), &c).await);
    assert!(
        console.contains("CONSOLE_A") && console.contains("ERR_A"),
        "{console}"
    );
    let net = text(&reg.execute("browser_network", json!({}), &c).await);
    assert!(net.contains("PATH_A"), "{net}");
    ok(&reg.execute("browser_tabs", json!({}), &c).await, "tabs");
    reg.close_session("rw-yes");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_only_session_reads_console_network_and_tabs_without_a_prompt() {
    if !have_browser() {
        return;
    }
    let a = serve_markers("B").await;
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(LoopbackPerson::default());
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(BrowserConfig {
            policy: UrlPolicy {
                allow_private_network: true,
                ..Default::default()
            },
            no_sandbox: no_sandbox(),
            host: Some(person.clone()),
            ..Default::default()
        });
    let c = ctx(d.path(), "rw-agent");
    ok(
        &reg.execute("browser_navigate", json!({"url": format!("{a}/m")}), &c)
            .await,
        "navigate",
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let console = text(&reg.execute("browser_console", json!({}), &c).await);
    assert!(console.contains("CONSOLE_B"), "{console}");
    ok(
        &reg.execute("browser_network", json!({}), &c).await,
        "network",
    );
    ok(&reg.execute("browser_tabs", json!({}), &c).await, "tabs");
    assert!(
        person.asked.lock().unwrap().is_empty(),
        "nobody is asked in an agent-only session"
    );
    reg.close_session("rw-agent");
}

const MOUSE_PAGE: &str = r#"<html><head><title>Mouse</title><style>body{margin:0}
#m{position:absolute;left:100px;top:100px;width:100px;height:40px;background:#ccc}
#h{position:absolute;left:300px;top:100px;width:20px;height:20px;background:red}
#tall{position:absolute;top:0;left:0;width:1px;height:4000px}</style></head><body>
<div id="m">menu</div><div id="h"></div><div id="tall"></div><pre id="log" style="position:fixed;left:0;top:300px"></pre>
<script>const L=t=>document.getElementById('log').textContent+=' '+t;let drag=false;
m.onmouseover=()=>L('over-menu');m.ondblclick=()=>L('dbl');m.oncontextmenu=e=>{e.preventDefault();L('ctx')};
h.onmousedown=()=>{drag=true;L('drag-start')};
addEventListener('mouseup',e=>{if(drag){drag=false;L('drag-end:'+e.clientX)}});
addEventListener('scroll',()=>L('scrolled'));</script></body></html>"#;

async fn serve_html(html: &'static str) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_mouse_hovers_double_right_clicks_drags_and_wheels_in_a_real_page() {
    if !have_browser() {
        return;
    }
    let base = serve_html(MOUSE_PAGE).await;
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (registry(false, true), ctx(d.path(), "mouse1"));
    let run = |i: Value| reg.execute("browser_mouse", i, &c);
    ok(
        &reg.execute("browser_navigate", json!({"url": format!("{base}/")}), &c)
            .await,
        "navigate",
    );
    let seen = |t: &'static str| {
        reg.execute(
            "browser_wait_for",
            json!({"text": t, "timeout_ms": 5000}),
            &c,
        )
    };
    let o = run(json!({"action": "move", "x": 150, "y": 120})).await;
    ok(&o, "move");
    assert_eq!(o.meta["action"]["action"], "hover");
    assert_eq!(
        (o.meta["action"]["x"].clone(), o.meta["action"]["y"].clone()),
        (json!(150), json!(120))
    );
    ok(&seen("over-menu").await, "hover reached the page");
    let o = run(json!({"action": "double_click", "x": 150, "y": 120})).await;
    ok(&o, "double");
    ok(&seen("dbl").await, "dblclick");
    ok(
        &run(json!({"action": "right_click", "x": 150, "y": 120})).await,
        "right",
    );
    ok(&seen("ctx").await, "contextmenu");
    ok(
        &run(json!({"action": "drag", "x": 310, "y": 110, "to_x": 500, "to_y": 110})).await,
        "drag",
    );
    ok(
        &seen("drag-end:500").await,
        "the drag ended where it was told to",
    );
    // an element target works as for browser_click
    let tree = text(&reg.execute("browser_snapshot", json!({}), &c).await);
    assert!(tree.contains("menu"), "{tree}");
    // a huge wheel is clamped, not refused; the page scrolls
    let o = run(json!({"action": "wheel", "x": 50, "y": 50, "dy": 1e12})).await;
    ok(&o, "wheel");
    assert!(text(&o).contains("5000"), "{}", text(&o));
    ok(&seen("scrolled").await, "wheel scrolled the page");
    // untrusted numbers and shapes are refused before anything is sent
    for bad in [
        json!({"action": "click", "x": -1, "y": 5}),
        json!({"action": "click", "x": 5, "y": 100000}),
        json!({"action": "click", "x": "5", "y": 5}),
        json!({"action": "click", "x": 5}),
        json!({"action": "click", "ref": "e1", "x": 5, "y": 5}),
        json!({"action": "click"}),
        json!({"action": "drag", "x": 5, "y": 5}),
        json!({"action": "wheel", "x": 5, "y": 5}),
        json!({"action": "wheel", "dy": "a"}),
        json!({"action": "fly", "x": 5, "y": 5}),
        json!({"x": 5, "y": 5}),
    ] {
        let o = run(bad.clone()).await;
        assert!(o.is_error, "{bad}: {}", text(&o));
    }
    reg.close_session("mouse1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_drag_lets_go_of_the_button() {
    if !have_browser() {
        return;
    }
    let base = serve_html(MOUSE_PAGE).await;
    let d = tempfile::tempdir().unwrap();
    let reg = registry(false, true);
    let c = ctx(d.path(), "mouse3");
    ok(
        &reg.execute("browser_navigate", json!({"url": format!("{base}/")}), &c)
            .await,
        "navigate",
    );
    // the pointer starts at the drag's start, so the press comes at once
    ok(
        &reg.execute(
            "browser_mouse",
            json!({"action": "move", "x": 310, "y": 110}),
            &c,
        )
        .await,
        "move",
    );
    // the cancel arrives while the pointer is on its way; the button must come up (the page logs the mouseup)
    let stop = c.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        stop.cancel();
    });
    let o = reg
        .execute(
            "browser_mouse",
            json!({"action": "drag", "x": 310, "y": 110, "to_x": 1200, "to_y": 700}),
            &c,
        )
        .await;
    eprintln!("cancelled drag: {}", text(&o));
    let c2 = ctx(d.path(), "mouse3");
    ok(
        &reg.execute(
            "browser_wait_for",
            json!({"text": "drag-start", "timeout_ms": 5000}),
            &c2,
        )
        .await,
        "the press reached the page before the cancel",
    );
    let o = reg
        .execute(
            "browser_wait_for",
            json!({"text": "drag-end", "timeout_ms": 5000}),
            &c2,
        )
        .await;
    ok(&o, "the held button was released after the cancel");
    // and the next call starts clean: a move reports no drag
    ok(
        &reg.execute(
            "browser_mouse",
            json!({"action": "move", "x": 5, "y": 5}),
            &c2,
        )
        .await,
        "move after",
    );
    reg.close_session("mouse3");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_mouse_is_gated_like_click_on_a_loopback_page_the_person_opened() {
    if !have_browser() {
        return;
    }
    let a = serve_html(MOUSE_PAGE).await;
    let d = tempfile::tempdir().unwrap();
    let person = Arc::new(LoopbackPerson::default());
    let tap = Arc::new(synthesis_tools::BrowserTap::default());
    let cfg = BrowserConfig {
        no_sandbox: no_sandbox(),
        host: Some(person.clone()),
        view: Some(tap.clone()),
        ..Default::default()
    };
    tap.enable_start(
        cfg.clone(),
        d.path().join("art"),
        synthesis_config::Env::from_process(),
    );
    let reg = Registry::unsandboxed_for_tests()
        .with_new_tools()
        .with_browser(cfg);
    tap.start().await.unwrap();
    tap.view()
        .unwrap()
        .navigate(&format!("{a}/"), synthesis_browser::WaitUntil::Load)
        .await
        .unwrap();
    let c = ctx(d.path(), "mouse2");
    let o = reg
        .execute(
            "browser_mouse",
            json!({"action": "move", "x": 150, "y": 120}),
            &c,
        )
        .await;
    assert!(o.is_error && text(&o).contains("approves"), "{o:?}");
    assert_eq!(person.asked.lock().unwrap().len(), 1);
    reg.close_session("mouse2");
}
