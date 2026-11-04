//! The interactive app end to end: the real `synt` binary on a pseudo-terminal (a small python3
//! driver, `tui_pty_driver.py`), a scripted model and, where needed, a stdio MCP server. Each
//! scenario types into the app and then checks the screen and the session store.
#![cfg(all(feature = "test-provider", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let r = Rig { dir };
        for d in ["home", "ws/.git", "ws/sub/.git"] {
            std::fs::create_dir_all(r.root().join(d)).unwrap();
        }
        std::fs::write(r.root().join("script.json"), script.to_string()).unwrap();
        r
    }

    fn root(&self) -> PathBuf {
        // the app canonicalises: /var -> /private/var on macOS
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
    fn home(&self) -> PathBuf {
        self.root().join("home")
    }
    fn ws(&self) -> PathBuf {
        self.root().join("ws")
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.home().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Runs the app with `steps`; returns the screen text. Fails on a step that did not happen.
    fn drive(&self, steps: Value) -> String {
        self.drive_with(steps, &[])
    }

    /// No `--provider`, no `--sandbox`: the app as a first-time user starts it.
    fn drive_bare(&self, steps: Value) -> String {
        self.run_scenario(
            vec![
                json!(env!("CARGO_BIN_EXE_synt")),
                json!("--cwd"),
                json!(self.ws()),
            ],
            steps,
        )
    }

    fn drive_with(&self, steps: Value, extra: &[&str]) -> String {
        let mut argv = vec![
            json!(env!("CARGO_BIN_EXE_synt")),
            json!("--cwd"),
            json!(self.ws()),
            json!("--provider"),
            json!(format!(
                "fake:{}",
                self.root().join("script.json").display()
            )),
            json!("--sandbox"),
            json!("off"),
        ];
        argv.extend(extra.iter().map(|a| json!(a)));
        self.run_scenario(argv, steps)
    }

    fn run_scenario(&self, argv: Vec<Value>, steps: Value) -> String {
        let scenario = json!({
            "argv": argv,
            "env": {"HOME": self.root(), "SYNT_HOME": self.home()},
            "steps": steps,
        });
        let (sc, out) = (
            self.root().join("scenario.json"),
            self.root().join("out.txt"),
        );
        std::fs::write(&sc, scenario.to_string()).unwrap();
        let driver = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tui_pty_driver.py");
        let st = Command::new("python3")
            .arg("-I")
            .arg(driver)
            .arg(&sc)
            .arg(&out)
            .stdin(Stdio::null())
            .status()
            .expect("python3 is needed for the pty tests");
        assert!(st.success());
        let text = std::fs::read_to_string(out).unwrap();
        assert!(
            text.lines().last().unwrap_or("").starts_with("STATE ok"),
            "a step failed:\n{}",
            squash(&text)
        );
        text
    }

    /// `(type, payload)` of every stored event of the sessions in the store.
    fn events(&self) -> Vec<(String, Value)> {
        let c = rusqlite::Connection::open(self.home().join("synt.db")).unwrap();
        let mut q = c
            .prepare("SELECT type, COALESCE(payload, '{}') FROM events ORDER BY session_id, seq")
            .unwrap();
        q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }
}

fn squash(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn send(s: &str) -> Value {
    json!(["send", s])
}
fn expect(s: &str) -> Value {
    json!(["expect", s, 40])
}
fn expect_new(s: &str) -> Value {
    json!(["expect_new", s, 40])
}
/// Waits until the app has printed nothing for a moment: it is idle and reads keys again.
fn quiet() -> Value {
    json!(["quiet", 0.7])
}

/// Approval, sub-agent, memory row, model switch and a new folder, in one app run.
#[test]
fn approval_subagent_model_switch_and_new_folder() {
    let rig = Rig::new(json!([
        {"text": "first reply"},
        {"tools": [{"name": "Write", "input": {"file_path": "NEW.txt", "content": "x"}}]},
        {"text": "wrote it"},
        {"tools": [{"name": "Task", "input": {"description": "look", "prompt": "say hi",
                                              "subagent_type": "general-purpose"}}]},
        {"text": "sub says hi"},
        {"text": "all done"}
    ]));
    let text = rig.drive(json!([
        expect("Enter send"),
        send("hi\r"),
        expect("first reply"),
        // a Write asks; Enter answers the first option (allow once)
        send("write a file\r"),
        expect("Write"),
        quiet(),
        send("\r"),
        expect("wrote it"),
        send("use a subagent\r"),
        expect("all done"),
        // a command sent while the task still runs is refused ("a task is running")
        quiet(),
        // /model restarts on the same session; the scripted model starts over
        send("/model fake/other\r"),
        // the restarted app is ready when its header shows the new model; keys typed earlier
        // arrive while the terminal is between two raw-mode sessions and are lost
        expect_new("fake/other"),
        quiet(),
        send("again\r"),
        expect_new("first reply"),
        quiet(),
        // /new -> sub/ -> use this folder: a fresh session in that folder
        send("/new\r"),
        expect("use this folder"),
        // wait for each picker state instead of sleeping: a loaded runner drops keys sent too early
        send("sub"),
        expect("sub/"),
        send("\r"),
        expect_new("use this folder"),
        send("\r"),
        expect_new("/sub"),
        send("hello\r"),
        expect_new("first reply")
    ]));
    let flat = squash(&text);
    assert!(flat.contains("memory.written"), "no memory row");
    assert!(!flat.contains("synt: error"), "{flat}");
    let ev = rig.events();
    let n = |t: &str| ev.iter().filter(|e| e.0 == t).count();
    assert!(n("tool.approval_required") >= 1, "no approval event");
    assert!(n("subagent.started") >= 1 && n("subagent.stopped") >= 1);
    // the model switch resumed the first session (session.started with source resume), and the
    // new folder is another session
    let sources: Vec<&str> = ev
        .iter()
        .filter(|e| e.0 == "session.started")
        .filter_map(|e| e.1["source"].as_str())
        .collect();
    assert!(sources.contains(&"resume"), "{sources:?}");
    // the new session's event may land after the screen shows its reply: poll for it
    let roots_of = |ev: &[(String, Value)]| {
        ev.iter()
            .filter(|e| e.0 == "session.started")
            .filter_map(|e| e.1["workspace"]["cwd"].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<String>>()
    };
    let t0 = std::time::Instant::now();
    let mut roots = roots_of(&ev);
    while roots.len() < 2 && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        roots = roots_of(&rig.events());
    }
    assert!(roots.len() >= 2, "{roots:?}");
    // the usage of finished tasks reached the store before the app ended (it is killed, not quit)
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let used: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE json_extract(usage, '$.turns') >= 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(used >= 2, "usage not persisted per task: {used} session(s)");
}

/// `/model` before the first prompt: the session is still Preparing; the restart must resume it.
#[test]
fn model_switch_before_the_first_prompt_keeps_the_app_running() {
    let rig = Rig::new(json!([{"text": "first reply"}]));
    let out = rig.drive(json!([
        expect("Enter send"),
        send("/model fake/other\r"),
        expect_new("fake/other"),
        quiet(),
        send("hi\r"),
        expect_new("first reply")
    ]));
    assert!(!squash(&out).contains("cannot be resumed"), "{out}");
}

/// `/resume <bad id>` and a folder the app cannot open stay in the app with a notice (they used
/// to exit with `synt: error: ...`); a typed path starts a session there.
#[test]
fn resume_of_a_bad_id_and_unopenable_folders_are_notices_not_exits() {
    let rig = Rig::new(json!([{"text": "first reply"}]));
    // a folder whose project config nobody can read (the picker only checks the folder itself)
    let bad = rig.ws().join("cfgbad");
    std::fs::create_dir_all(bad.join(".synt")).unwrap();
    std::fs::create_dir_all(bad.join(".git")).unwrap();
    std::fs::write(bad.join(".synt/config.json"), "{}").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            bad.join(".synt/config.json"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    std::fs::create_dir_all(rig.root().join("x/.git")).unwrap();
    let out = rig.drive(json!([
        expect("Enter send"),
        send("/resume nope-nope\r"),
        expect_new("no saved session matches"),
        quiet(),
        // the unreadable config: a notice, the session goes on
        send("/new\r"),
        expect_new("use this folder"),
        send("./cfgbad"),
        expect_new("open "),
        send("\r"),
        expect_new("cannot open"),
        quiet(),
        // `~/x` is the rig's HOME/x
        send("/new\r"),
        expect_new("use this folder"),
        send("~/x"),
        expect_new("open "),
        send("\r"),
        expect_new("/x"),
        quiet()
    ]));
    let flat = squash(&out);
    assert!(!flat.contains("synt: error"), "{flat}");
    assert!(flat.contains("STATE ok"), "{flat}");
}

/// Choosing a folder while a task runs cancels it and says so in the new session.
#[test]
fn switching_folder_during_a_task_says_the_task_was_cancelled() {
    let rig = Rig::new(json!([{"text": "slow reply", "delay_ms": 20000}]));
    let out = rig.drive(json!([
        expect("Enter send"),
        send("hi"),
        expect_new("hi"),
        send("\r"),
        // the task runs (and the scripted reply is 20 s away) when the footer offers the interrupt
        expect_new("esc to interrupt"),
        send("/new"),
        expect_new("/new"),
        send("\r"),
        expect_new("use this folder"),
        send("./sub"),
        expect_new("open "),
        send("\r"),
        expect_new("task cancelled: switching folder")
    ]));
    assert!(!squash(&out).contains("synt: error"), "{out}");
}

/// Esc with a line the agent has not read yet: one notice (the line is back in the prompt), not
/// also a `message dropped` warning.
#[test]
fn esc_with_an_unread_line_gives_one_notice() {
    let rig = Rig::new(json!([{"text": "slow reply", "delay_ms": 20000}]));
    let out = rig.drive(json!([
        expect("Enter send"),
        send("hi\r"),
        expect_new("esc to interrupt"),
        send("second\r"),
        expect_new("queued:"),
        send("\x1b"),
        expect_new("back in the prompt"),
        quiet()
    ]));
    let flat = squash(&out);
    assert!(!flat.contains("message dropped"), "{flat}");
    assert!(!flat.contains("synt: error"), "{flat}");
}

/// `--resume ID` of a session whose folder was deleted names the session and the folder.
#[test]
fn resume_of_a_deleted_folder_says_which_folder() {
    let rig = Rig::new(json!([{"text": "first reply"}]));
    let gone = rig.root().join("gone");
    std::fs::create_dir_all(gone.join(".git")).unwrap();
    let provider = format!("fake:{}", rig.root().join("script.json").display());
    let argv = |cwd: &Path, extra: &[&str]| {
        let mut a = vec![
            json!(env!("CARGO_BIN_EXE_synt")),
            json!("--cwd"),
            json!(cwd),
            json!("--provider"),
            json!(provider),
            json!("--sandbox"),
            json!("off"),
        ];
        a.extend(extra.iter().map(|x| json!(x)));
        a
    };
    rig.run_scenario(
        argv(&gone, &[]),
        json!([expect("Enter send"), send("hi\r"), expect("first reply")]),
    );
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let id: String = c
        .query_row("SELECT id FROM sessions LIMIT 1", [], |r| r.get(0))
        .unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let out = rig.run_scenario(
        argv(&rig.ws(), &["--resume", &id]),
        json!([expect("no longer exists")]),
    );
    let flat = squash(&out);
    assert!(flat.contains(&format!("cannot resume {id}")), "{flat}");
    assert!(flat.contains(&gone.display().to_string()), "{flat}");
}

/// `/resume ID` inside the app, for a session whose folder was deleted: the same wording as at
/// startup, not the raw OS error.
#[test]
fn in_app_resume_of_a_deleted_folder_says_which_folder() {
    let rig = Rig::new(json!([{"text": "first reply"}]));
    let gone = rig.root().join("gone2");
    std::fs::create_dir_all(gone.join(".git")).unwrap();
    let provider = format!("fake:{}", rig.root().join("script.json").display());
    rig.run_scenario(
        vec![
            json!(env!("CARGO_BIN_EXE_synt")),
            json!("--cwd"),
            json!(gone),
            json!("--provider"),
            json!(provider),
            json!("--sandbox"),
            json!("off"),
        ],
        json!([expect("Enter send"), send("hi\r"), expect("first reply")]),
    );
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let id: String = c
        .query_row("SELECT id FROM sessions LIMIT 1", [], |r| r.get(0))
        .unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let out = rig.drive(json!([
        expect("Enter send"),
        send(&format!("/resume {id}\r")),
        expect_new("no longer exists"),
        quiet()
    ]));
    let flat = squash(&out);
    assert!(flat.contains(&format!("cannot resume {id}")), "{flat}");
    assert!(!flat.contains("os error"), "{flat}");
}

const MCP_SERVER: &str = r#"
import sys, json
log = sys.argv[1]
for line in sys.stdin:
    try:
        m = json.loads(line)
    except Exception:
        continue
    if "id" not in m:
        continue
    method, p = m.get("method"), m.get("params") or {}
    if method == "initialize":
        r = {"protocolVersion": p["protocolVersion"],
             "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
             "serverInfo": {"name": "doc", "version": "1"}}
    elif method == "tools/list":
        r = {"tools": []}
    elif method == "resources/list":
        r = {"resources": [{"uri": "doc://1", "name": "One"}]}
    elif method == "resources/read":
        open(log, "a").write(p["uri"] + "\n")
        r = {"contents": [{"uri": p["uri"], "text": "doc body"}]}
    elif method == "prompts/list":
        r = {"prompts": [{"name": "greet", "description": "Greets",
                          "arguments": [{"name": "who", "required": True}, {"name": "how"}]}]}
    elif method == "prompts/get":
        a = p.get("arguments") or {}
        r = {"messages": [{"role": "user", "content": {"type": "text",
             "text": "Hello %s (%s)" % (a.get("who"), a.get("how"))}}]}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

/// `@server:uri`, `/mcp__server__prompt`, a skill command, and /help listing the last two.
#[test]
fn mcp_mentions_prompts_skills_and_help() {
    let rig = Rig::new(json!([{"text": "ok one"}, {"text": "ok two"}, {"text": "ok three"}]));
    let srv = rig.root().join("server.py");
    std::fs::write(&srv, MCP_SERVER).unwrap();
    let reads = rig.root().join("reads.log");
    rig.write(
        "synt.json",
        &json!({"$schema": "config.json/v1", "mcpServers": {
            "doc": {"command": "python3", "args": [srv, reads]}}})
        .to_string(),
    );
    rig.write(
        "skills/ship/SKILL.md",
        "---\nname: ship\ndescription: Ship the release\n---\nDo the shipping.\n",
    );
    rig.drive(json!([
        expect("Enter send"),
        send("/help\r"),
        expect("mcp__doc__greet"),
        expect("Ship the release"),
        send("/mcp__doc__greet Ann loudly\r"),
        expect("ok one"),
        send("read @doc:doc://1 please\r"),
        expect("ok two"),
        send("/ship now\r"),
        expect("ok three")
    ]));
    let users: Vec<String> = rig
        .events()
        .into_iter()
        .filter(|e| e.0 == "user.message")
        .filter_map(|e| e.1["text"].as_str().map(String::from))
        .collect();
    let has = |s: &str| users.iter().any(|u| u.contains(s));
    assert!(
        has("Hello Ann (loudly)"),
        "MCP prompt not expanded: {users:?}"
    );
    assert!(has("doc body"), "MCP resource not attached: {users:?}");
    assert!(has("Run the skill \"ship\""), "{users:?}");
    assert!(std::fs::read_to_string(reads).unwrap().contains("doc://1"));
}

fn atlassian_like_server() -> PathBuf {
    let exe = Path::new(env!("CARGO_BIN_EXE_synt"));
    let profile = exe.parent().unwrap();
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.args([
        "build",
        "-p",
        "synthesis-mcp",
        "--example",
        "atlassian_like_server",
    ]);
    if profile.file_name().is_some_and(|n| n == "release") {
        c.arg("--release");
    }
    assert!(c.status().unwrap().success(), "cannot build the example");
    profile.join("examples").join("atlassian_like_server")
}

/// ToolSearch and the deferred MCP tool it loads show up as tool rows.
#[test]
fn tool_search_rows() {
    let rig = Rig::new(json!([
        {"tools": [{"name": "ToolSearch", "input": {"query": "select:mcp__atl__whoami"}}]},
        {"tools": [{"name": "mcp__atl__whoami", "input": {}}]},
        {"text": "searched"}
    ]));
    rig.write(
        "synt.json",
        &json!({"$schema": "config.json/v1", "mcpServers": {"atl": {"command": atlassian_like_server()}}})
            .to_string(),
    );
    rig.write(
        "settings.json",
        &json!({"permissions": {"allow": ["mcp__atl"]}, "synt": {"mcp": {"toolSearch": "on"}}})
            .to_string(),
    );
    let text = rig.drive(json!([
        expect("Enter send"),
        send("who am i\r"),
        expect("searched")
    ]));
    let flat = squash(&text);
    assert!(flat.contains("ToolSearch"), "no ToolSearch row: {flat}");
    let done: Vec<String> = rig
        .events()
        .into_iter()
        .filter(|e| e.0 == "tool.completed")
        .map(|e| e.1["status"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(done, ["ok", "ok"], "{done:?}");
}

/// `/config` flips a `synt.tui` setting, applies it at once (the Session panel goes) and writes
/// the USER settings file, leaving its other keys alone.
#[test]
fn config_flips_a_setting_applies_it_and_writes_the_user_file() {
    let rig = Rig::new(json!([{"text": "x"}]));
    rig.write(
        "settings.json",
        r#"{"permissions": {"deny": ["Bash(rm:*)"]}}"#,
    );
    rig.drive(json!([
        expect("Enter send"),
        expect("Session"),
        send("/config\r"),
        expect("Settings (synt.tui)"),
        expect("[x] panel session"),
        send("\r"), // flips the first row: the Session panel off
        expect("[ ] panel session"),
        quiet()
    ]));
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.home().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(v["synt"]["tui"]["panels"]["session"], false);
    assert_eq!(v["synt"]["tui"]["panels"]["tools"], true);
    assert_eq!(
        v["permissions"]["deny"][0], "Bash(rm:*)",
        "other keys survive"
    );
    // no credential or env is ever written by the editor
    assert!(!v.to_string().contains("API") && v.get("env").is_none());
    // the next start applies it: the Session panel is not drawn
    let out = rig.drive(json!([expect("Enter send"), quiet()]));
    assert!(!squash(&out).contains("◆ Session"), "{}", squash(&out));
}

#[test]
fn peers_name_and_agents_all_say_so_when_peers_are_off_and_show_the_table_when_on() {
    let rig = Rig::new(json!([{"text": "x"}]));
    let off = rig.drive(json!([
        expect("Enter send"),
        send("/peers\r"),
        expect("peers are off"),
        send("/name\r"),
        expect("/name: peers are off"),
        send("/agents --all\r"),
        expect("Agents in this session")
    ]));
    assert!(squash(&off).contains("synt.peers.enabled"));
    let on = rig.drive_with(
        json!([
            expect("Enter send"),
            expect("peers on"),
            send("/name\r"),
            expect("Session name for peers: alpha"),
            send("/name beta\r"),
            expect("the name is fixed for the run"),
            send("/peers\r"),
            expect("WORKSPACE"),
            send("/agents --all\r"),
            expect("Agents in this session")
        ]),
        &["--peers", "--name", "alpha"],
    );
    let flat = squash(&on);
    assert!(
        flat.contains("SESSION") && flat.contains("AGENT") && flat.contains("STATUS"),
        "{flat}"
    );
}

/// `/files`, `/diff [file]`, `/terminal` and `/browser`: the data of the old centre tabs as
/// command output.
#[test]
fn files_diff_terminal_and_browser_show_real_data() {
    let rig = Rig::new(json!([
        {"tools": [{"name": "Bash", "input": {"command": "echo hi"}}]},
        {"text": "ran it"}
    ]));
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(rig.ws())
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    std::fs::write(rig.ws().join("a.rs"), "one\n").unwrap();
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "c"]);
    std::fs::write(rig.ws().join("a.rs"), "two\n").unwrap();
    rig.write(
        "settings.json",
        r#"{"permissions": {"allow": ["Bash(echo:*)"]}}"#,
    );
    // not trusted: git is not run at all
    let untrusted = rig.drive(json!([
        expect("Enter send"),
        send("/files\r"),
        expect("this folder is not trusted")
    ]));
    assert!(squash(&untrusted).contains("--trust-project"));
    let out = rig.drive_with(
        json!([
            expect("Enter send"),
            send("/files\r"),
            expect("M a.rs"),
            send("/diff a.rs\r"),
            expect("+1 -1  a.rs"),
            expect("+two"),
            send("run something\r"),
            expect("ran it"),
            send("/terminal\r"),
            expect("shell commands in this session"),
            send("/browser\r"),
            expect("no browser and web calls")
        ]),
        &["--trust-project"],
    );
    assert!(squash(&out).contains("echo hi"), "{}", squash(&out));
}

/// First run: nothing configured. The setup card replaces the exit-2 error, never asks for a key,
/// `q` leaves cleanly.
#[test]
fn first_run_shows_the_setup_card_and_quits_cleanly() {
    let rig = Rig::new(json!([]));
    let out = rig.drive_bare(json!([
        expect("needs a model and an API key"),
        expect("{env:MY_PROVIDER_KEY}"),
        send("q")
    ]));
    let flat = squash(&out);
    assert!(
        flat.contains("check again") && flat.contains("never in a repository"),
        "{flat}"
    );
    assert!(
        flat.contains("STATE ok 0"),
        "q exits 0: {}",
        flat.rsplit("STATE").next().unwrap_or("")
    );
    assert!(!rig.home().join("auth.json").exists(), "nothing was stored");
}
