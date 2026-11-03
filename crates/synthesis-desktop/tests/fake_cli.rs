//! The runtime against the fake agent-desktop (src/bin/fake_agent_desktop.rs): no real binary,
//! no OS permission, no window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_desktop::{
    Action, DesktopError, DesktopOptions, DesktopRuntime, Pin, PinStatus, ProcSource, ProcTable,
    WaitFor, pin,
};
use synthesis_permissions::desktop::Origin;

const FAKE: &str = env!("CARGO_BIN_EXE_fake-agent-desktop");

struct Fixed(ProcTable);
impl ProcSource for Fixed {
    fn table(&self) -> synthesis_desktop::Result<ProcTable> {
        Ok(self.0.clone())
    }
}

fn env() -> Env {
    // the display variable keeps the Linux "no graphical session" check out of the way
    Env::from_pairs([("DISPLAY", ":0"), ("HOME", "/h"), ("ANTHROPIC_API_KEY", "sk-leak")])
}

fn env_ok(cmd: &str, data: Value) -> Value {
    json!({"version":"2.5","ok":true,"command":cmd,"data":data})
}

fn env_err(cmd: &str, code: &str, retry: &str) -> Value {
    json!({"version":"2.5","ok":false,"command":cmd,"error":{"code":code,"message":"m","disposition":{"delivery":if retry=="safe" {"not_delivered"} else {"delivered_unverified"},"retry":retry}}})
}

/// Process tree: 1 -> 10 (terminal) -> 20 (synt = this test) ; app 30 -> 31 ; 99 unrelated
fn table() -> ProcTable {
    ProcTable::from_pairs([(10, 1), (20, 10), (30, 20), (31, 30), (99, 1), (std::process::id(), 10)])
}

fn windows() -> Value {
    json!([
        {"id":"w-app","title":"Demo","app_name":"demo","pid":30},
        {"id":"w-child","title":"Child","app_name":"demo-helper","pid":31},
        {"id":"w-other","title":"Mail","app_name":"Mail","pid":99},
        {"id":"w-term","title":"zsh","app_name":"Terminal","pid":10},
        {"id":"w-chrome","title":"Tab","app_name":"Google Chrome","pid":31}
    ])
}

fn snapshot(sid: &str, ids: &[u32]) -> Value {
    let kids: Vec<Value> = ids
        .iter()
        .map(|n| json!({"role":"button","name":format!("B{n}"),"ref_id":format!("@{sid}:e{n}"),"available_actions":["Click"]}))
        .collect();
    env_ok("snapshot", json!({"app":"demo","window":{"id":"w-app","title":"Demo"},"snapshot_id":sid,"ref_count":ids.len(),"complete":true,
        "tree":{"role":"window","name":"Demo","children":kids}}))
}

async fn start(script: Value, tune: impl FnOnce(&mut DesktopOptions)) -> (tempfile::TempDir, DesktopRuntime) {
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("desktop");
    std::fs::create_dir_all(state.join("state")).unwrap();
    let mut s = script;
    s["version"] = env_ok("version", json!({"version":"9.9.9","os":"test"}));
    std::fs::write(state.join("state/fake.json"), s.to_string()).unwrap();
    let mut o = DesktopOptions::new(PathBuf::from(FAKE), state);
    o.allow_unpinned = true;
    o.call_timeout = Duration::from_secs(10);
    tune(&mut o);
    let rt = DesktopRuntime::start(o, &env(), Arc::new(Fixed(table()))).await.unwrap();
    (d, rt)
}

fn calls(d: &tempfile::TempDir) -> Vec<Vec<String>> {
    std::fs::read_to_string(d.path().join("desktop/state/calls.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn an_unpinned_or_mismatching_binary_is_refused_unless_allowed() {
    let d = tempfile::tempdir().unwrap();
    let mk = |allow: bool, pin: Pin| {
        let mut o = DesktopOptions::new(PathBuf::from(FAKE), d.path().join("desktop"));
        o.allow_unpinned = allow;
        o.pin = pin;
        o
    };
    std::fs::create_dir_all(d.path().join("desktop/state")).unwrap();
    std::fs::write(
        d.path().join("desktop/state/fake.json"),
        json!({"version": env_ok("version", json!({"version":"1.2.3"}))}).to_string(),
    )
    .unwrap();
    let procs = || -> Arc<dyn ProcSource> { Arc::new(Fixed(table())) };
    // the compiled-in pin is a placeholder: nothing matches
    let e = DesktopRuntime::start(mk(false, Pin::builtin()), &env(), procs()).await.err().unwrap();
    assert_eq!(e.kind(), "desktop_unavailable");
    assert!(e.to_string().contains("no verified agent-desktop pin"), "{e}");
    // a wrong hash
    let wrong = Pin { version: "1.2.3".into(), sha256: "0".repeat(64) };
    let e = DesktopRuntime::start(mk(false, wrong), &env(), procs()).await.err().unwrap();
    assert!(e.to_string().contains("does not match the pinned sha256"), "{e}");
    // the right hash and version
    let sha = pin::sha256_file(&PathBuf::from(FAKE)).unwrap();
    let good = Pin { version: "1.2.3".into(), sha256: sha.clone() };
    let rt = DesktopRuntime::start(mk(false, good), &env(), procs()).await.unwrap();
    assert_eq!(rt.pin_status, PinStatus::Match);
    // the right hash, a different version
    let skew = Pin { version: "2.0.0".into(), sha256: sha };
    let e = DesktopRuntime::start(mk(false, skew), &env(), procs()).await.err().unwrap();
    assert!(e.to_string().contains("reports version 1.2.3"), "{e}");
    // allow_unpinned runs it
    let rt = DesktopRuntime::start(mk(true, Pin::builtin()), &env(), procs()).await.unwrap();
    assert_eq!(rt.version, "1.2.3");
}

#[tokio::test]
async fn windows_are_filtered_to_the_process_tree_and_policy() {
    let (_d, rt) = start(json!({"list-windows": env_ok("list-windows", windows())}), |_| {}).await;
    let app = rt.register_app(30, Origin::Project, "/ws/target/debug/demo").unwrap();
    assert_eq!(app.id, "app-1");
    let ws = rt.windows(&app.id).await.unwrap();
    let ids: Vec<&str> = ws.windows.iter().map(|w| w.id.as_str()).collect();
    // the app's and its child's window; not the unrelated app, not the terminal, not the browser
    assert_eq!(ids, ["w-app", "w-child"]);
    assert_eq!(ws.hidden, ["Google Chrome"]);
    // a window outside the tree cannot be addressed by id either
    let e = rt.snapshot(&app.id, Some("w-other"), None, false, 50).await.unwrap_err();
    assert_eq!(e.kind(), "refused");
    // two windows and no window_id: the model must choose
    let e = rt.snapshot(&app.id, None, None, false, 50).await.unwrap_err();
    assert!(e.to_string().contains("w-app, w-child"), "{e}");
}

#[tokio::test]
async fn hard_denied_targets_cannot_be_registered() {
    let (_d, rt) = start(json!({}), |_| {}).await;
    for t in ["/Applications/1Password.app/Contents/MacOS/1Password", "C:\\x\\chrome.exe"] {
        assert_eq!(rt.register_app(30, Origin::External, t).unwrap_err().kind(), "refused");
    }
    assert_eq!(rt.register_app(30, Origin::External, "/opt/x/tool").unwrap().id, "ext-1");
}

#[tokio::test]
async fn snapshot_act_wait_screenshot_loop_is_always_window_scoped() {
    let one = json!([{"id":"w-app","title":"Demo","app_name":"demo","pid":30}]);
    let script = json!({
        "list-windows": env_ok("list-windows", one),
        "snapshot": [snapshot("s1", &[1, 2]), snapshot("s2", &[1])],
        "click": [
            env_ok("click", json!({"action":"click","disposition":{"delivery":"delivered_unverified","retry":"unsafe"}})),
            env_err("click", "STALE_REF", "safe")
        ],
        "type": env_ok("type", json!({"disposition":{"delivery":"delivered_verified","retry":"unsafe"}})),
        "press": env_ok("press", json!({"disposition":{"delivery":"delivered_unverified","retry":"unsafe"}})),
        "screenshot": {"_fake":{"png":true}, "version":"2.5","ok":true,"command":"screenshot","data":{"path":"x","format":"png","width":1,"height":1}},
    });
    let (d, rt) = start(script, |_| {}).await;
    let app = rt.register_app(30, Origin::Project, "/ws/demo").unwrap();

    let s = rt.snapshot(&app.id, None, None, false, 50).await.unwrap();
    assert_eq!(s.refs, ["@s1:e1", "@s1:e2"]);
    // acting on a ref nobody showed us is refused before any process starts
    let n = calls(&d).len();
    let e = rt.act(&app.id, Action::Click, Some("@zzz:e1"), None, None).await.unwrap_err();
    assert_eq!(e.kind(), "stale_ref");
    assert_eq!(calls(&d).len(), n);

    let del = rt.act(&app.id, Action::Click, Some("@s1:e1"), None, None).await.unwrap();
    assert!(del.delivered() && !del.verified());
    let del = rt.act(&app.id, Action::Type, Some("@s1:e2"), Some("hello"), None).await.unwrap();
    assert!(del.verified());
    rt.act(&app.id, Action::Press, None, Some("return"), None).await.unwrap();
    // the CLI says the ref is gone: reported as stale, and the whole snapshot is poisoned
    let e = rt.act(&app.id, Action::Click, Some("@s1:e2"), None, None).await.unwrap_err();
    assert_eq!(e.kind(), "stale_ref");
    let e = rt.act(&app.id, Action::Click, Some("@s1:e1"), None, None).await.unwrap_err();
    assert!(e.to_string().contains("older snapshot"), "{e}");
    // a new snapshot makes new refs usable
    let s2 = rt.snapshot(&app.id, None, None, false, 50).await.unwrap();
    assert_eq!(s2.refs, ["@s2:e1"]);

    let shot = rt.screenshot(&app.id, None).await.unwrap();
    assert!(shot.path.starts_with(d.path().join("desktop/shots")) && shot.bytes > 8);

    // every call that reaches the app names the window; none uses --app, --headed or --screen
    for c in calls(&d) {
        let cmd = c[0].as_str();
        assert!(!c.iter().any(|a| matches!(a.as_str(), "--app" | "--headed" | "--screen")), "{c:?}");
        match cmd {
            "snapshot" | "screenshot" | "press" => {
                let i = c.iter().position(|a| a == "--window-id").unwrap_or_else(|| panic!("{c:?}"));
                assert_eq!(c[i + 1], "w-app");
            }
            "click" | "type" => assert!(c[1].starts_with("@s"), "{c:?}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn wait_polls_the_apps_own_window() {
    let one = json!([{"id":"w-app","title":"Demo","app_name":"demo","pid":30}]);
    let script = json!({
        "list-windows": [env_ok("list-windows", one.clone()), env_ok("list-windows", one), env_ok("list-windows", json!([]))],
        "snapshot": [snapshot("s1", &[1]), snapshot("s1", &[1])],
    });
    let (_d, rt) = start(script, |_| {}).await;
    let app = rt.register_app(30, Origin::Project, "/ws/demo").unwrap();
    // "B1" is in the tree; "Nope" never is
    rt.wait(&app.id, WaitFor::Text("b1".into()), None, Duration::from_secs(5)).await.unwrap();
    let (_d2, rt2) = start(
        json!({"list-windows": env_ok("list-windows", json!([{"id":"w-app","title":"D","app_name":"demo","pid":30}])), "snapshot": snapshot("s1", &[1])}),
        |_| {},
    )
    .await;
    let app2 = rt2.register_app(30, Origin::Project, "/ws/demo").unwrap();
    let e = rt2.wait(&app2.id, WaitFor::Text("Nope".into()), None, Duration::from_millis(600)).await.unwrap_err();
    assert_eq!(e.kind(), "timeout");
    // window_gone: the third list is empty
    let (_d3, rt3) = start(
        json!({"list-windows": [env_ok("list-windows", json!([{"id":"w-app","title":"D","app_name":"demo","pid":30}])), env_ok("list-windows", json!([]))]}),
        |_| {},
    )
    .await;
    let app3 = rt3.register_app(30, Origin::Project, "/ws/demo").unwrap();
    rt3.wait(&app3.id, WaitFor::WindowGone(Some("w-app".into())), None, Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn a_slow_command_is_killed_and_flooding_output_is_cut() {
    let (_d, rt) = start(
        json!({"list-windows": {"_fake":{"sleep_ms":8000}, "version":"2.5","ok":true,"command":"list-windows","data":[]}}),
        |o| o.call_timeout = Duration::from_millis(2000),
    )
    .await;
    let app = rt.register_app(30, Origin::Project, "/ws/demo").unwrap();
    let t = std::time::Instant::now();
    let e = rt.windows(&app.id).await.unwrap_err();
    assert_eq!(e.kind(), "timeout");
    assert!(t.elapsed() < Duration::from_secs(6));
    let (_d, rt) = start(
        json!({"list-windows": {"_fake":{"flood": 2_000_000}}}),
        |o| o.max_output = 100_000,
    )
    .await;
    let app = rt.register_app(30, Origin::Project, "/ws/demo").unwrap();
    let e = rt.windows(&app.id).await.unwrap_err();
    assert_eq!(e.kind(), "protocol_error");
    assert!(e.to_string().contains("output over"), "{e}");
}

#[tokio::test]
async fn actions_are_rate_limited_per_second_and_per_session() {
    let script = json!({
        "list-windows": env_ok("list-windows", json!([{"id":"w-app","title":"D","app_name":"demo","pid":30}])),
        "snapshot": snapshot("s1", &[1]),
        "click": env_ok("click", json!({"disposition":{"delivery":"delivered_unverified","retry":"unsafe"}})),
    });
    let (_d, rt) = start(script, |o| {
        o.actions_per_sec = 20.0;
        o.max_actions = 3;
    })
    .await;
    let app = rt.register_app(30, Origin::Project, "/ws/demo").unwrap();
    rt.snapshot(&app.id, None, None, false, 50).await.unwrap();
    let t = std::time::Instant::now();
    for _ in 0..3 {
        rt.act(&app.id, Action::Click, Some("@s1:e1"), None, None).await.unwrap();
    }
    assert!(t.elapsed() >= Duration::from_millis(100), "paced at 20/s: {:?}", t.elapsed());
    let e = rt.act(&app.id, Action::Click, Some("@s1:e1"), None, None).await.unwrap_err();
    assert!(matches!(e, DesktopError::RateLimited(_)));
}

#[tokio::test]
async fn the_child_gets_a_scrubbed_environment() {
    let (d, rt) = start(json!({}), |_| {}).await;
    drop(rt);
    // AGENT_DESKTOP_HOME is the private state dir; the child saw only the allowlist
    let names = std::fs::read_to_string(d.path().join("desktop/state/env.log")).unwrap();
    let names: Vec<&str> = names.lines().collect();
    assert!(names.contains(&"AGENT_DESKTOP_HOME") && names.contains(&"HOME"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("ANTHROPIC") || n.contains("KEY")), "{names:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in ["desktop", "desktop/state", "desktop/shots"] {
            let m = std::fs::metadata(d.path().join(p)).unwrap().permissions().mode();
            assert_eq!(m & 0o077, 0, "{p} is {m:o}");
        }
    }
}
