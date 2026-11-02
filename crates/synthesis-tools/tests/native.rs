//! Real-process tests for Bash/BashOutput/KillShell/terminal_* and state tests for the
//! Todo/Task/interface tools. Unix only: commands are POSIX sh.
#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_terminal::{NetworkPolicy, SandboxMode, ShellKind, ShellSpec, TerminalRuntime};
use synthesis_tools::{Content, NeedsUser, Registry, ToolOutput};

struct Fx {
    _d: tempfile::TempDir,
    root: PathBuf,
    reg: Registry,
    ctx: ToolContext,
}

#[test]
fn standard_policy_network_defaults_to_deny_and_is_selectable() {
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let root = base.join("w");
    std::fs::create_dir(&root).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], base.join("art"), "pg").unwrap();
    let net = |p: Option<synthesis_terminal::SandboxPolicy>| p.unwrap().network;
    assert_eq!(
        net(synthesis_tools::standard_policy(&ws)),
        NetworkPolicy::Deny
    );
    let lo = synthesis_tools::standard_policy_with(&ws, NetworkPolicy::Loopback);
    assert_eq!(net(lo), NetworkPolicy::Loopback);
}

fn fx_with(reg: Registry, env: &[(&str, &str)]) -> Fx {
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let root = base.join("w");
    std::fs::create_dir(&root).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], base.join("art"), "pg").unwrap();
    let mut e: BTreeMap<String, String> = [("PATH", "/usr/bin:/bin")]
        .iter()
        .chain(env)
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    e.entry("PATH".into())
        .or_insert_with(|| "/usr/bin:/bin".into());
    let ctx = ToolContext {
        session_id: "s".into(),
        task_id: "t".into(),
        turn_id: "u".into(),
        tool_call_id: "c".into(),
        workspace: Arc::new(ws),
        env: Arc::new(e),
        read_ledger: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: PermissionMode::Default,
        skip_secrets: false,
        sub_agent: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    Fx {
        _d: d,
        root,
        reg,
        ctx,
    }
}

fn fx() -> Fx {
    fx_with(Registry::unsandboxed_for_tests().with_new_tools(), &[])
}

impl Fx {
    async fn run(&self, tool: &str, input: Value) -> ToolOutput {
        self.reg.execute(tool, input, &self.ctx).await
    }
}

fn text(o: &ToolOutput) -> &str {
    match &o.content {
        Content::Text(t) => t,
        Content::Blocks(_) => "<blocks>",
    }
}

fn json_of(o: &ToolOutput) -> Value {
    serde_json::from_str(text(o)).unwrap_or_else(|_| panic!("not json: {o:?}"))
}

#[tokio::test]
async fn grep_for_a_responder_skips_secret_shaped_files_and_links_to_them() {
    let mut f = fx();
    let w = f.root.clone();
    for (n, body) in [
        (".env", "TOKEN=needle-env"),
        ("tls.pem", "needle-pem"),
        ("id_rsa", "needle-rsa"),
        (".envrc", "needle-envrc"),
        ("secrets.yaml", "needle-yaml"),
        ("ok.txt", "needle-ok"),
    ] {
        std::fs::write(w.join(n), body).unwrap();
    }
    std::fs::create_dir(w.join(".ssh")).unwrap();
    std::fs::write(w.join(".ssh/config"), "needle-ssh").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(w.join(".env"), w.join("innocent.txt")).unwrap();
    let all = f
        .run(
            "Grep",
            json!({"pattern": "needle", "path": w, "output_mode": "content"}),
        )
        .await;
    assert!(
        text(&all).contains("needle-env"),
        "the main agent still greps everything"
    );
    f.ctx.skip_secrets = true;
    let o = f
        .run(
            "Grep",
            json!({"pattern": "needle", "path": w, "output_mode": "content"}),
        )
        .await;
    let t = text(&o);
    assert!(t.contains("needle-ok"), "{t}");
    for leaked in [
        "needle-env",
        "needle-pem",
        "needle-rsa",
        "needle-envrc",
        "needle-yaml",
        "needle-ssh",
    ] {
        assert!(!t.contains(leaked), "{leaked} leaked: {t}");
    }
    // the file itself as the search root
    let one = f
        .run(
            "Grep",
            json!({"pattern": "needle", "path": w.join(".env"), "output_mode": "content"}),
        )
        .await;
    assert!(!text(&one).contains("needle-env"));
}

fn alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn until<F: FnMut() -> bool>(mut f: F) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn foreground_exit_codes_stdout_stderr() {
    let f = fx();
    let o = f
        .run("Bash", json!({"command": "echo out; echo err >&2"}))
        .await;
    assert!(!o.is_error, "{o:?}");
    assert!(text(&o).contains("out") && text(&o).contains("err"));
    assert_eq!(o.meta["exit_code"], 0);
    assert_eq!(o.meta["shell_kind"], "posix");
    let o = f.run("Bash", json!({"command": "echo hi; exit 3"})).await;
    assert!(o.is_error && text(&o).ends_with("[exit code 3]"), "{o:?}");
    // cwd is the workspace root, not the test process cwd.
    let o = f.run("Bash", json!({"command": "pwd -P"})).await;
    assert_eq!(text(&o).trim(), f.root.to_str().unwrap());
}

#[tokio::test]
async fn timeout_kills_tree() {
    let f = fx();
    let pidfile = f.root.join("gc.pid");
    let cmd = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
    let o = f.run("Bash", json!({"command": cmd, "timeout": 500})).await;
    assert!(
        o.is_error && text(&o).contains("[timed out after 500ms]"),
        "{o:?}"
    );
    assert_eq!(o.meta["timed_out"], true);
    let pid: u64 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    until(|| !alive(pid)).await;
}

#[tokio::test]
async fn interactive_timeout_moves_bash_to_the_background_and_tells_the_model() {
    let f = fx_with(
        Registry::unsandboxed_for_tests()
            .with_new_tools()
            .with_background_on_timeout(true),
        &[],
    );
    let o = f
        .run(
            "Bash",
            json!({"command": "echo begin; sleep 1; echo end", "timeout": 300}),
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    let id = o.meta["bash_id"]
        .as_str()
        .expect("backgrounded")
        .to_string();
    let t = text(&o);
    assert!(
        t.contains("begin")
            && t.contains("NOT killed")
            && t.contains("BashOutput")
            && t.contains(&id),
        "{t}"
    );
    // the model can read the rest with BashOutput once it finishes
    let mut last = String::new();
    for _ in 0..100 {
        let r = f.run("BashOutput", json!({"bash_id": id})).await;
        last = text(&r).to_string();
        if last.contains("exited") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        last.contains("end") && last.contains("exit code 0"),
        "{last}"
    );
}

#[tokio::test]
async fn truncation_keeps_head_and_tail_and_reports_artifact() {
    let f = fx();
    let o = f
        .run(
            "Bash",
            json!({"command": "yes 0123456789 | head -c 100000"}),
        )
        .await;
    assert!(!o.is_error);
    assert!(
        text(&o).len() < 31_000 && text(&o).contains("truncated"),
        "{}",
        text(&o).len()
    );
    assert_eq!(o.meta["truncated"], true);
    assert_eq!(o.meta["total_bytes"], 100_000);
    let p = o.meta["artifact_path"].as_str().unwrap();
    assert!(Path::new(p).starts_with(f.root.parent().unwrap().join("art")));
    assert_eq!(std::fs::metadata(p).unwrap().len(), 100_000);
}

#[tokio::test]
async fn background_output_cursor_filter_and_kill() {
    let f = fx();
    let o = f
        .run(
            "Bash",
            json!({"command": "echo one; echo skip; sleep 60", "run_in_background": true}),
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    let id = o.meta["bash_id"].as_str().unwrap().to_string();
    let pid = o.meta["pid"].as_u64().unwrap();
    until(|| alive(pid)).await;

    let mut seen = String::new();
    for _ in 0..100 {
        let o = f.run("BashOutput", json!({"bash_id": id})).await;
        seen += text(&o);
        if seen.contains("skip") {
            assert!(text(&o).ends_with(&format!("[{id}: running]")), "{o:?}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen.contains("one"));
    // Cursor advanced: nothing new, only the state line.
    let o = f.run("BashOutput", json!({"bash_id": id})).await;
    assert_eq!(text(&o), format!("[{id}: running]"));

    // Filter: matching lines only; bad regex is an error.
    let id2 = f
        .run(
            "Bash",
            json!({"command": "echo keep1; echo drop; echo keep2", "run_in_background": true}),
        )
        .await
        .meta["bash_id"]
        .as_str()
        .unwrap()
        .to_string();
    until(|| std::fs::read_dir(f.root.parent().unwrap().join("art/terminal")).is_ok()).await;
    let mut out = String::new();
    for _ in 0..100 {
        out = text(
            &f.run("BashOutput", json!({"bash_id": id2, "filter": "^keep"}))
                .await,
        )
        .to_string();
        if out.contains("keep2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        out.contains("keep1") && out.contains("keep2") && !out.contains("drop"),
        "{out}"
    );
    assert!(
        f.run("BashOutput", json!({"bash_id": id2, "filter": "("}))
            .await
            .is_error
    );
    assert!(
        f.run("BashOutput", json!({"bash_id": "nope"}))
            .await
            .is_error
    );

    let o = f.run("KillShell", json!({"shell_id": id})).await;
    assert_eq!(text(&o), format!("Killed {id}."));
    until(|| !alive(pid)).await;
    let o = f.run("KillShell", json!({"shell_id": id})).await;
    assert!(text(&o).contains("had already exited"), "{o:?}");
}

#[tokio::test]
async fn spawn_readiness_logs_signal_status() {
    let f = fx();
    let o = f.run("terminal_spawn", json!({"command": "echo booting; sleep 0.2; echo READY-NOW; sleep 60", "name": "svc", "ready": {"kind": "log", "pattern": "READY-NOW", "timeout_ms": 10000}})).await;
    assert!(!o.is_error, "{o:?}");
    let v = json_of(&o);
    assert_eq!(v["ready"], true);
    assert_eq!(v["status"], "running");
    let id = v["process_id"].as_str().unwrap().to_string();
    let pid = v["pid"].as_u64().unwrap();

    let o = f
        .run(
            "terminal_logs",
            json!({"process_id": id, "filter": "READY"}),
        )
        .await;
    assert!(text(&o).starts_with("READY-NOW"), "{o:?}");
    let cursor = o.meta["cursor"].as_str().unwrap().to_string();
    let o = f
        .run("terminal_logs", json!({"process_id": id, "cursor": cursor}))
        .await;
    assert!(text(&o).starts_with(&format!("[{id}: running")), "{o:?}");

    let l = json_of(&f.run("terminal_status", json!({})).await);
    assert_eq!(l[0]["name"], "svc");
    assert_eq!(
        json_of(&f.run("terminal_status", json!({"process_id": id})).await)["status"],
        "running"
    );

    let o = f
        .run(
            "terminal_signal",
            json!({"process_id": id, "signal": "kill"}),
        )
        .await;
    assert_eq!(json_of(&o)["status"], "exited", "{o:?}");
    until(|| !alive(pid)).await;

    // A readiness check that never passes reports ready:false instead of failing.
    let o = f.run("terminal_spawn", json!({"command": "sleep 60", "ready": {"kind": "log", "pattern": "never", "timeout_ms": 200}})).await;
    assert_eq!(json_of(&o)["ready"], false);
    // A readiness URL the probe would refuse is rejected before anything is spawned.
    for url in [
        "http://169.254.169.254:8080/",
        "http://192.0.2.1:8080/",
        "http://u:p@127.0.0.1:8080/",
        "http://127.0.0.1:8080/x?y=1",
        "http://127.0.0.1:80/",
        "http://127.0.0.1:8080/a\r\nHost: x",
    ] {
        let o = f.run("terminal_spawn", json!({"command": "sleep 60", "ready": {"kind": "http", "url": url, "timeout_ms": 300}})).await;
        assert!(o.is_error, "{url}: {o:?}");
        assert!(
            format!("{o:?}").contains("loopback") || format!("{o:?}").contains("127.0.0.1"),
            "{url}: {o:?}"
        );
    }
    // Valid loopback targets are accepted (nothing listens: not ready).
    let o = f.run("terminal_spawn", json!({"command": "sleep 60", "ready": {"kind": "http", "url": "http://localhost:3000/health", "timeout_ms": 200}})).await;
    assert_eq!(json_of(&o)["ready"], false, "{o:?}");
    // Bad readiness / env / tty / cwd are refused.
    for bad in [
        json!({"command": "true", "ready": {"kind": "port"}}),
        json!({"command": "true", "ready": {"kind": "http", "url": "https://x"}}),
        json!({"command": "true", "env": {"LD_PRELOAD": "/x"}}),
        json!({"command": "true", "tty": true}),
        json!({"command": "true", "cwd": ".."}),
    ] {
        let o = f.run("terminal_spawn", bad.clone()).await;
        assert!(o.is_error, "{bad}: {o:?}");
    }
    assert!(
        f.run(
            "terminal_signal",
            json!({"process_id": "nope", "signal": "term"})
        )
        .await
        .is_error
    );
    assert!(
        f.run("terminal_spawn", json!({"command": "true", "bogus": 1}))
            .await
            .meta["error"]
            == "invalid_input"
    );
}

#[tokio::test]
async fn spawn_env_is_allowlisted() {
    let f = fx();
    for name in [
        "PATH",
        "Path",
        "path",
        "PATHEXT",
        "ComSpec",
        "SystemRoot",
        "GIT_SSH_COMMAND",
        "GIT_DIR",
        "NODE_OPTIONS",
        "NODE_PATH",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "RUBYOPT",
        "PERL5OPT",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "PS4",
        "PROMPT_COMMAND",
        "BASH_FUNC_x%%",
        "IFS",
        "HOME",
        "USER",
        "SHELL",
        "TMPDIR",
        "TEMP",
        "HTTPS_PROXY",
        "https_proxy",
        "SSL_CERT_FILE",
        "API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "Node_Env",
        "node_env",
        "LC_",
        "lc_all",
        "VITE_API_KEY",
        "NEXT_PUBLIC_TOKEN",
        "",
        "A=B",
        "FOO",
    ] {
        let o = f
            .run(
                "terminal_spawn",
                json!({"command": "true", "env": {name: "x"}}),
            )
            .await;
        assert!(o.is_error, "{name}: {o:?}");
        assert!(
            text(&o).contains(&format!("{name:?}")),
            "{name}: {}",
            text(&o)
        );
    }
    let o = f
        .run(
            "terminal_spawn",
            json!({"command": "true", "env": {"LC_ALL": "C", "NODE_ENV": "test", "VITE_PORT": "1"}}),
        )
        .await;
    assert!(!o.is_error, "{o:?}");
}

#[tokio::test]
async fn per_call_env_reaches_child() {
    let f = fx();
    let o = f
        .run(
            "terminal_spawn",
            json!({"command": "echo \"v=$LOG_LEVEL\"", "env": {"LOG_LEVEL": "bar"}}),
        )
        .await;
    let id = json_of(&o)["process_id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if text(&f.run("terminal_logs", json!({"process_id": id})).await).contains("v=bar") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("per-call env not seen");
}

#[tokio::test]
async fn child_env_is_allowlisted() {
    let f = fx_with(
        Registry::unsandboxed_for_tests(),
        &[
            ("GITHUB_TOKEN", "tok"),
            ("SYNT_API_KEY", "k"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
            ("LANG", "C"),
        ],
    );
    let o = f.run("Bash", json!({"command": "env"})).await;
    let t = text(&o);
    assert!(t.contains("LANG=C") && t.contains("PATH="), "{t}");
    for leak in [
        "GITHUB_TOKEN",
        "SYNT_API_KEY",
        "AWS_SECRET",
        "tok",
        "CARGO_MANIFEST_DIR",
    ] {
        assert!(!t.contains(leak), "{leak} leaked: {t}");
    }
}

#[tokio::test]
async fn session_end_kills_background_processes() {
    let f = fx();
    let o = f
        .run(
            "Bash",
            json!({"command": "sleep 60", "run_in_background": true}),
        )
        .await;
    let pid = o.meta["pid"].as_u64().unwrap();
    until(|| alive(pid)).await;
    f.reg.close_session("s");
    until(|| !alive(pid)).await;
}

#[tokio::test]
async fn cancel_aborts_foreground() {
    let f = fx();
    let c = f.ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.cancel();
    });
    let o = f
        .run("Bash", json!({"command": "echo started; sleep 60"}))
        .await;
    assert!(o.is_error && text(&o).contains("[aborted]"), "{o:?}");
    let o = f.run("Bash", json!({"command": "true"})).await;
    assert!(o.is_error && text(&o).contains("aborted before start"));
}

#[tokio::test]
async fn cmd_shell_is_flagged_in_meta() {
    let cmd = ShellSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into()],
        kind: ShellKind::Cmd,
        note: None,
    };
    let f = fx_with(
        Registry::with_terminal(Arc::new(TerminalRuntime::with_shell(cmd, SandboxMode::Off))),
        &[],
    );
    let o = f.run("Bash", json!({"command": "echo x"})).await;
    assert_eq!(o.meta["shell_kind"], "cmd", "{o:?}");
}

#[tokio::test]
async fn invalid_shell_inputs_stay_strict() {
    let f = fx();
    for (t, i) in [
        ("Bash", json!({})),
        ("Bash", json!({"command": 5})),
        ("Bash", json!({"command": "ls", "cwd": "/"})),
        ("BashOutput", json!({"bash_id": 1})),
        ("KillShell", json!({})),
        (
            "terminal_signal",
            json!({"process_id": "p", "signal": "hup"}),
        ),
        (
            "terminal_spawn",
            json!({"command": "x", "ready": {"kind": "udp"}}),
        ),
    ] {
        let o = f.run(t, i.clone()).await;
        assert_eq!(o.meta["error"], "invalid_input", "{t} {i}");
    }
    let names: Vec<_> = Registry::unsandboxed_for_tests()
        .specs()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(names.len(), 20);
    assert_eq!(f.reg.specs().count(), 42); // + browser_login, browser_fill_secret
}

#[tokio::test]
async fn todo_and_task_state() {
    let f = fx();
    let o = f
        .run(
            "TodoWrite",
            json!({"todos": [
        {"content": "A", "status": "in_progress", "activeForm": "Doing A"},
        {"content": "B", "status": "pending", "activeForm": "Doing B"}]}),
        )
        .await;
    assert!(!o.is_error && text(&o).starts_with("Todos have been modified"));
    assert_eq!(o.meta["todos"].as_array().unwrap().len(), 2);
    assert!(
        f.run(
            "TodoWrite",
            json!({"todos": [{"content": " ", "status": "pending", "activeForm": "x"}]})
        )
        .await
        .is_error
    );
    assert!(
        f.run(
            "TodoWrite",
            json!({"todos": [{"content": "A", "status": "bogus", "activeForm": "x"}]})
        )
        .await
        .is_error
    );

    let o = f
        .run("TaskCreate", json!({"subject": "C", "description": "d"}))
        .await;
    assert_eq!(json_of(&o)["task"], json!({"id": "3", "subject": "C"}));
    let o = f
        .run(
            "TaskUpdate",
            json!({"taskId": "3", "status": "completed", "activeForm": "Doing C"}),
        )
        .await;
    assert_eq!(json_of(&o)["statusChange"]["to"], "completed");
    let o = f.run("TaskGet", json!({"taskId": "3"})).await;
    let t = &json_of(&o)["task"];
    assert_eq!(
        (t["description"].as_str(), t["activeForm"].as_str()),
        (Some("d"), Some("Doing C"))
    );
    let l = json_of(&f.run("TaskList", json!({})).await);
    assert_eq!(l["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(
        json_of(&f.run("TaskGet", json!({"taskId": "99"})).await),
        json!({"task": null})
    );
    assert!(f.run("TaskUpdate", json!({"taskId": "99"})).await.is_error);
    assert!(
        f.run("TaskUpdate", json!({"taskId": "1", "status": "done"}))
            .await
            .meta["error"]
            == "invalid_input"
    );
    // TodoWrite replaces the whole list; other sessions are untouched.
    f.run("TodoWrite", json!({"todos": []})).await;
    assert_eq!(
        json_of(&f.run("TaskList", json!({})).await)["tasks"],
        json!([])
    );
    let mut other = f.ctx.clone();
    other.session_id = "s2".into();
    f.reg
        .execute("TaskCreate", json!({"subject": "X"}), &other)
        .await;
    assert_eq!(
        json_of(&f.run("TaskList", json!({})).await)["tasks"],
        json!([])
    );
}

#[tokio::test]
async fn task_tools_follow_the_sdk_shapes() {
    let f = fx();
    for (s, m) in [("A", json!({"k": 1})), ("B", json!({})), ("C", json!({}))] {
        f.run(
            "TaskCreate",
            json!({"subject": s, "description": "d", "metadata": m}),
        )
        .await;
    }
    // edges: 1 blocks 2; 3 is blocked by 2; owner and metadata (null removes a key)
    let u = json_of(
        &f.run(
            "TaskUpdate",
            json!({"taskId": "1", "status": "in_progress", "addBlocks": ["2"], "owner": "me",
                "metadata": {"k": null, "j": 2}}),
        )
        .await,
    );
    assert_eq!(
        u,
        json!({"success": true, "taskId": "1", "updatedFields": ["status", "owner", "metadata", "blocks"],
            "statusChange": {"from": "pending", "to": "in_progress"}})
    );
    f.run("TaskUpdate", json!({"taskId": "3", "addBlockedBy": ["2"]}))
        .await;
    let g = json_of(&f.run("TaskGet", json!({"taskId": "2"})).await);
    assert_eq!(
        (&g["task"]["blocks"], &g["task"]["blockedBy"]),
        (&json!(["3"]), &json!(["1"]))
    );
    let g = json_of(&f.run("TaskGet", json!({"taskId": "1"})).await);
    assert_eq!(g["task"]["metadata"], json!({"j": 2}));
    let l = json_of(&f.run("TaskList", json!({})).await);
    assert_eq!(
        l["tasks"][0],
        json!({"id": "1", "subject": "A", "status": "in_progress", "blockedBy": [], "owner": "me"})
    );
    // repaired key names, as Claude Code does
    let u = json_of(
        &f.run(
            "TaskUpdate",
            json!({"task_id": "2", "active_form": "Doing B"}),
        )
        .await,
    );
    assert_eq!(u["updatedFields"], json!(["activeForm"]));
    // unknown edge targets change nothing
    let o = f
        .run(
            "TaskUpdate",
            json!({"taskId": "1", "addBlocks": ["9"], "subject": "Z"}),
        )
        .await;
    assert!(o.is_error && json_of(&o)["success"] == false);
    assert_eq!(
        json_of(&f.run("TaskGet", json!({"taskId": "1"})).await)["task"]["subject"],
        "A"
    );
    // deleted removes the task and its edges; the todo.updated payload (meta) shrinks too
    let o = f
        .run("TaskUpdate", json!({"taskId": "2", "status": "deleted"}))
        .await;
    assert_eq!(
        json_of(&o)["statusChange"],
        json!({"from": "pending", "to": "deleted"})
    );
    assert_eq!(o.meta["todos"].as_array().unwrap().len(), 2);
    let g = json_of(&f.run("TaskGet", json!({"taskId": "3"})).await);
    assert_eq!(g["task"]["blockedBy"], json!([]));
}

#[tokio::test]
async fn interface_tools_return_needs_user() {
    let f = fx();
    let o = f
        .run(
            "AskUserQuestion",
            json!({"questions": [{"question": "Q?", "multiSelect": true,
        "options": [{"label": "a"}, {"label": "b", "description": "bb"}]}]}),
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    let req: NeedsUser = serde_json::from_value(o.meta["needs_user"].clone()).unwrap();
    let NeedsUser::AskUserQuestion { questions } = req else {
        panic!()
    };
    assert!(questions[0].multi_select && questions[0].options.len() == 2);
    assert!(
        f.run("AskUserQuestion", json!({"questions": []}))
            .await
            .is_error
    );
    assert!(
        f.run(
            "AskUserQuestion",
            json!({"questions": [{"question": "Q", "options": []}]})
        )
        .await
        .is_error
    );

    let o = f.run("ExitPlanMode", json!({"plan": "# Plan"})).await;
    assert_eq!(
        o.meta["needs_user"],
        json!({"kind": "exit_plan_mode", "plan": "# Plan"})
    );
    assert!(f.run("ExitPlanMode", json!({"plan": "  "})).await.is_error);
}

#[tokio::test]
async fn grep_skips_gitignored_and_takes_offset() {
    let f = fx();
    let w = f.root.clone();
    std::fs::write(w.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
    std::fs::write(w.join("ignored.txt"), "needle\n").unwrap();
    std::fs::create_dir(w.join("build")).unwrap();
    std::fs::write(w.join("build/out.txt"), "needle\n").unwrap();
    std::fs::write(w.join("kept.txt"), "needle 1\nneedle 2\nneedle 3\n").unwrap();
    let all = f
        .run(
            "Grep",
            json!({"pattern": "needle", "path": w, "output_mode": "content"}),
        )
        .await;
    let t = text(&all);
    assert!(t.contains("kept.txt") && !t.contains("ignored.txt") && !t.contains("out.txt"));
    let page = f
        .run(
            "Grep",
            json!({"pattern": "needle", "path": w.join("kept.txt"), "output_mode": "content", "offset": 1, "head_limit": 1}),
        )
        .await;
    assert!(
        text(&page).contains("needle 2")
            && !text(&page).contains("needle 1")
            && !text(&page).contains("needle 3")
    );
}

#[tokio::test]
async fn read_offset_past_eof_notice() {
    let f = fx();
    std::fs::write(f.root.join("a.txt"), "one\ntwo\n").unwrap();
    let o = f
        .run(
            "Read",
            json!({"file_path": f.root.join("a.txt"), "offset": 50}),
        )
        .await;
    assert!(
        text(&o).contains("past the end of the file"),
        "{}",
        text(&o)
    );
}
