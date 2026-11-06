//! The interactive app end to end: the real `synt` binary on a pseudo-terminal (a small python3
//! driver, `tui_pty_driver.py`), a scripted model and, where needed, a stdio MCP server. Each
//! scenario types into the app and then checks the screen and the session store.
#![cfg(all(feature = "test-provider", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Rig {
    dir: tempfile::TempDir,
    /// Terminal (rows, cols); the sidebar's Usage panel needs more than the default.
    size: (u16, u16),
}

impl Rig {
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let r = Rig {
            dir,
            size: (40, 120),
        };
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
    fn write_script(&self, script: Value) {
        std::fs::write(self.root().join("script.json"), script.to_string()).unwrap();
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
        self.drive_bare_with(steps, &[])
    }

    /// `drive_bare` plus flags, for a config-only start where the host has no sandbox backend
    /// (CI Linux has no bubblewrap and the standard policy is refused).
    fn drive_bare_with(&self, steps: Value, extra: &[&str]) -> String {
        let mut argv = vec![
            json!(env!("CARGO_BIN_EXE_synt")),
            json!("--cwd"),
            json!(self.ws()),
        ];
        argv.extend(extra.iter().map(|a| json!(a)));
        self.run_scenario(argv, steps)
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
            "rows": self.size.0,
            "cols": self.size.1,
            // a hard stop for a stuck scenario (the driver kills the app and fails)
            "deadline": 90,
        });
        let (sc, out) = (
            self.root().join("scenario.json"),
            self.root().join("out.txt"),
        );
        std::fs::write(&sc, scenario.to_string()).unwrap();
        let driver = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tui_pty_driver.py");
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(driver)
            .arg(&sc)
            .arg(&out)
            .stdin(Stdio::null())
            .spawn()
            .expect("python3 is needed for the pty tests");
        // the driver has its own 120 s alarm; this is the backstop if even that is stuck
        let end = std::time::Instant::now() + std::time::Duration::from_secs(150);
        let st = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            if std::time::Instant::now() > end {
                let _ = child.kill();
                panic!("pty driver stuck for 150 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(st.success());
        let text = std::fs::read_to_string(out).unwrap();
        assert!(
            text.lines().last().unwrap_or("").starts_with("STATE ok"),
            "a step failed:\n{}",
            squash(&text)
        );
        text
    }

    /// The raw terminal bytes between the scenario's `mark` and `cut` steps.
    fn raw(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.root().join("out.txt.raw")).unwrap())
            .into_owned()
    }

    /// The screen text between the scenario's `mark` and `cut` steps.
    fn span(&self) -> String {
        std::fs::read_to_string(self.root().join("out.txt.span")).unwrap()
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
        // ctrl+n (new workspace) -> sub/ -> use this folder: a fresh session in that folder
        send("\u{e}"),
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
    assert!(!flat.contains("memory.written"), "raw memory row");
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

/// "Don't ask again" on a secret-file read stores a rule for that path, not a tool-wide `Read`,
/// so a later read of another secret file asks again.
#[test]
fn dont_ask_again_on_a_secret_read_is_scoped_to_that_path() {
    let rig = Rig::new(json!([{"text": "unused"}]));
    let (env, key) = (rig.ws().join(".env"), rig.ws().join("id_rsa"));
    std::fs::write(&env, "A=1\n").unwrap();
    std::fs::write(&key, "k\n").unwrap();
    rig.write_script(json!([
        {"tools": [{"name": "Read", "input": {"file_path": env}}]},
        {"text": "read env"},
        {"tools": [{"name": "Read", "input": {"file_path": key}}]},
        {"text": "read key"}
    ]));
    rig.drive_with(
        json!([
            expect("Enter send"),
            send("go\r"),
            expect("Do you want to proceed"),
            quiet(),
            send("2"),
            expect("read env"),
            quiet(),
            send("again\r"),
            // another secret file: the question comes back
            expect_new("Do you want to proceed"),
            quiet(),
            send("\u{1b}")
        ]),
        &["--trust-project"],
    );
    let saved = rig.ws().join(".synt/settings.local.json");
    let t0 = std::time::Instant::now();
    while !saved.exists() && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    let want = format!("Read(//{})", env.to_string_lossy().trim_start_matches('/'));
    assert_eq!(v["permissions"]["allow"], json!([want]), "{v}");
}

/// The chat hides internal rows (memory, approval, mode bookkeeping, `key=value`) but still says
/// a denial, the end of the task and an error; `synt.tui.verbose` shows readable lines.
#[test]
fn chat_hides_internal_rows_keeps_denials_and_done_and_verbose_shows_readable_lines() {
    let script = json!([
        {"text": "first reply"},
        {"tools": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "alpha",
                                              "new_string": "ALPHA"}}]},
        {"text": "after deny"}
    ]);
    let steps = || {
        json!([
            expect("Enter send"),
            send("hi\r"),
            expect("first reply"),
            quiet(),
            send("edit it\r"),
            expect("Do you want to proceed?"),
            quiet(),
            send("\x1b"),
            expect("after deny"),
            quiet(),
            send("/verbose on\r"),
            expect("verbose on"),
            quiet()
        ])
    };
    let rig = Rig::new(script.clone());
    std::fs::write(rig.ws().join("a.txt"), "alpha beta\n").unwrap();
    let flat = squash(&rig.drive(steps()));
    for raw in [
        "memory.written",
        "memory.retrieved",
        "session.mode_changed",
        "approval_resolved",
        "memory:",
        "op=added",
    ] {
        assert!(!flat.contains(raw), "{raw} in the chat:\n{flat}");
    }
    assert!(flat.contains("approval denied (user)"), "{flat}");
    assert!(flat.contains("Done ·"), "{flat}");
    // verbose in the user settings: the same session draws readable one-line forms
    let rig = Rig::new(script);
    std::fs::write(rig.ws().join("a.txt"), "alpha beta\n").unwrap();
    rig.write("settings.json", r#"{"synt": {"tui": {"verbose": true}}}"#);
    let flat = squash(&rig.drive(steps()));
    assert!(flat.contains("memory: episode note added"), "{flat}");
    assert!(flat.contains("approval denied (user)"), "{flat}");
    assert!(
        !flat.contains("memory.written") && !flat.contains("op=added"),
        "{flat}"
    );
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

/// Picking a model in the /model picker (Enter on the current one, arrows + Enter to a model of
/// another provider, a typed name) restarts the app on the same session and never leaves it: each
/// step waits for the restarted app, and the last one needs the app to still answer.
#[test]
fn model_picker_selection_keeps_the_app_running() {
    let rig = Rig::new(json!([{"text": "unused"}]));
    rig.write(
        "synt.json",
        r#"{"$schema":"config.json/v1","model":"gw/big","providers":{
            "gw":{"type":"anthropic","baseURL":"http://127.0.0.1:1","apiKey":false,"models":["big","other"]},
            "oa":{"type":"openai","baseURL":"http://127.0.0.1:1/v1","apiKey":false,"models":["x1","x2"]}}}"#,
    );
    let out = rig.drive_bare_with(
        json!([
            expect("Enter send"),
            quiet(),
            send("/model\r"),
            expect_new("enter select"),
            // the same model: Enter on the current row
            send("\r"),
            quiet(),
            send("/model\r"),
            expect_new("enter select"),
            // another provider: two rows down
            send("\x1b[B\x1b[B\r"),
            expect_new("oa/x1"),
            quiet(),
            send("/model gw/other\r"),
            expect_new("gw/other"),
            quiet(),
            send("/model\r"),
            expect_new("enter select")
        ]),
        &["--sandbox", "off"],
    );
    assert!(!squash(&out).contains("could not switch"), "{out}");
}

/// Picking a model restarts the app on the same session without leaving the alternate screen or
/// raw mode: no `ESC[?1049l` between the pick and the next frame, the header shows the new model,
/// and the switch leaves no extra (empty) session behind.
#[test]
fn model_switch_does_not_leave_the_alternate_screen() {
    let rig = Rig::new(json!([{"text": "unused"}]));
    rig.write(
        "synt.json",
        r#"{"$schema":"config.json/v1","model":"gw/big","providers":{
            "gw":{"type":"anthropic","baseURL":"http://127.0.0.1:1","apiKey":false,"models":["big","other"]}}}"#,
    );
    let out = rig.drive_bare_with(
        json!([
            expect("Enter send"),
            quiet(),
            send("/model\r"),
            expect_new("enter select"),
            json!(["mark"]),
            send("\x1b[B\r"),
            expect_new("gw/other"),
            quiet(),
            json!(["cut"])
        ]),
        &["--sandbox", "off"],
    );
    let raw = rig.raw();
    assert!(raw.contains("gw/other"), "{}", squash(&out));
    assert!(!raw.contains("\x1b[?1049l"), "left the alternate screen");
    assert!(
        !raw.contains("\x1b[?1049h"),
        "entered the alternate screen again"
    );
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert!(n <= 1, "the switch left {n} sessions");
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
        send("\u{e}"), // ctrl+n: new workspace
        expect_new("use this folder"),
        send("./cfgbad"),
        expect_new("open "),
        send("\r"),
        expect_new("cannot open"),
        quiet(),
        // `~/x` is the rig's HOME/x
        send("\u{e}"), // ctrl+n: new workspace
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

/// Typed paths in the Ctrl+N picker always answer: a missing absolute path (which fuzzy-matches the
/// `sub/` row) keeps the picker open with a one-line reason, a symlink row descends on Enter, and
/// a relative `./sub` opens.
#[cfg(unix)]
#[test]
fn folder_picker_typed_paths_give_feedback_and_symlinks_descend() {
    let rig = Rig::new(json!([]));
    std::fs::create_dir_all(rig.ws().join("sub/.git")).unwrap();
    std::fs::create_dir_all(rig.ws().join("real")).unwrap();
    std::os::unix::fs::symlink(rig.ws().join("real"), rig.ws().join("lnk")).unwrap();
    let out = rig.drive(json!([
        expect("Enter send"),
        send("\u{e}"), // ctrl+n: new workspace
        expect_new("use this folder"),
        send("lnk"),
        expect_new("(link)"),
        send("\r"),
        expect_new("/lnk"),
        send("\x1b"),
        send("\u{e}"),
        expect_new("use this folder"),
        send("/sub"),
        expect_new("no such folder: /sub"),
        send("\r"),
        quiet(),
        send("\u{7f}\u{7f}\u{7f}\u{7f}"),
        send("./sub"),
        expect_new("open "),
        send("\r"),
        expect_new("/sub"),
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
        send("\u{e}"), // ctrl+n: new workspace
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

/// A resumed session whose stored model left its provider's list shows a notice at start and
/// keeps the stored label.
#[test]
fn resume_with_a_removed_stored_model_shows_a_notice() {
    let rig = Rig::new(json!([{"text": "unused"}]));
    let conf = |models: &str| {
        format!(
            r#"{{"$schema":"config.json/v1","model":"gw/big","providers":{{"gw":{{"type":"anthropic","baseURL":"http://127.0.0.1:1","apiKey":false,"models":{models}}}}}}}"#
        )
    };
    rig.write("synt.json", &conf(r#"["big","other"]"#));
    let run = Command::new(env!("CARGO_BIN_EXE_synt"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", rig.root())
        .env("SYNT_HOME", rig.home())
        .args([
            "run",
            "--sandbox",
            "off",
            "--model",
            "gw/other",
            "--prompt",
            "hi",
            "--cwd",
        ])
        .arg(rig.ws())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let id: String = c
        .query_row("SELECT id FROM sessions LIMIT 1", [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&run.stderr)));
    rig.write("synt.json", &conf(r#"["big"]"#));
    let out = rig.run_scenario(
        vec![
            json!(env!("CARGO_BIN_EXE_synt")),
            json!("--cwd"),
            json!(rig.ws()),
            json!("--sandbox"),
            json!("off"),
            json!("--resume"),
            json!(id),
        ],
        json!([expect("stored model gw/other is not available")]),
    );
    let flat = squash(&out);
    assert!(flat.contains("not in the models of provider"), "{flat}");
    assert!(flat.contains("using gw/big"), "{flat}");
    let stored: String = c
        .query_row(
            "SELECT model_config FROM sessions WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "\"gw/other\"");
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
        // the notice wraps inside the panel, so wait for its start, not for a phrase across the wrap
        expect_new("cannot resume"),
        quiet()
    ]));
    let flat = squash(&out);
    assert!(flat.contains(&format!("cannot resume {id}")), "{flat}");
    assert!(flat.contains("gone2"), "{flat}");
    assert!(flat.contains("longer exists"), "{flat}");
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
/// Tab inserts the quoted form for a name with spaces, and the file reaches the model.
#[test]
fn at_file_with_a_space_is_completed_quoted_and_attached() {
    let rig = Rig::new(json!([{"text": "ok one"}]));
    std::fs::write(rig.ws().join("my notes.txt"), "kiwi-content\n").unwrap();
    rig.drive(json!([
        expect("Enter send"),
        send("read @my"),
        expect("my notes.txt"),
        send("\t"),
        quiet(),
        send("now\r"),
        expect("ok one")
    ]));
    let users: Vec<String> = rig
        .events()
        .into_iter()
        .filter(|e| e.0 == "user.message")
        .filter_map(|e| e.1["text"].as_str().map(String::from))
        .collect();
    let u = users.first().cloned().unwrap_or_default();
    assert!(u.starts_with("read @\"my notes.txt\" now"), "{users:?}");
    assert!(
        u.contains("<file path=\"my notes.txt\">") && u.contains("kiwi-content"),
        "{users:?}"
    );
}

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

/// The Usage panel loads the stored history off the UI thread: no "collecting history…" any more,
/// and after a prompt the panel shows the tokens of the session.
#[test]
fn usage_panel_shows_stored_history_after_a_prompt() {
    let mut rig = Rig::new(json!([{"text": "first reply"}]));
    rig.size = (48, 160);
    let out = rig.drive(json!([
        expect("Enter send"),
        quiet(),
        send("hi\r"),
        expect_new("first reply"),
        expect_new("tokens \u{b7}")
    ]));
    assert!(!squash(&out).contains("collecting history"), "{out}");
    assert!(squash(&out).contains("7d"), "{}", squash(&out));
}

/// The process dies (SIGKILL, as when the terminal window is closed) in the middle of a task,
/// once while the model is still answering and once while a tool runs. Opening the session again
/// shows it idle: the phase of a fresh process comes from its own runtime, not from the replay.
#[test]
fn kill_mid_task_then_continue_shows_idle() {
    for tool in [false, true] {
        let mut rig = Rig::new(json!([{"text": "never", "delay_ms": 120000}]));
        rig.size = (48, 160);
        let mut steps = vec![expect("Enter send"), quiet(), send("hi\r")];
        if tool {
            rig.write_script(json!([
                {"tools": [{"name": "Bash", "input": {"command": "sleep 20"}}]},
                {"text": "never"}
            ]));
            steps.extend([expect("Bash"), quiet(), send("\r"), json!(["wait", 1.5])]);
        } else {
            steps.push(json!(["wait", 1.5]));
        }
        steps.push(json!(["kill"]));
        rig.drive(json!(steps));
        rig.write_script(json!([{"text": "second reply"}]));
        let out = rig.drive_with(
            json!([
                expect("Enter send"),
                json!(["wait", 1.5]),
                json!(["cut"]),
                send("again\r"),
                expect_new("second reply")
            ]),
            &["--continue"],
        );
        let flat = squash(&out);
        // idle until the person types: after "again" a THINKING frame is the app working
        let idle = squash(&rig.span());
        assert!(!idle.contains("THINKING"), "tool={tool}: {idle}");
        assert!(!idle.contains("RUNNING TOOL"), "tool={tool}: {idle}");
        assert!(
            flat.contains("previous run was interrupted"),
            "tool={tool}: {flat}"
        );
        if tool {
            assert!(
                !flat.contains("running…") && !flat.contains("running.."),
                "tool row settled: {flat}"
            );
            assert!(flat.contains("interrupted"), "tool row ended: {flat}");
        }
    }
}

/// SGR mouse report: left press and release at the 1-based cell `(x, y)`.
fn click(x: u32, y: u32) -> String {
    format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m")
}

/// SGR wheel report (`down`: 65, else 64) at the 1-based cell `(x, y)`.
fn wheel(x: u32, y: u32, down: bool) -> String {
    format!("\x1b[<{};{x};{y}M", if down { 65 } else { 64 })
}

/// Launches that never get a prompt leave empty sessions in the store; the Running panel lists
/// only the ones with a conversation (the open session shows on its own row, never as untitled).
#[test]
fn empty_launches_do_not_fill_the_running_panel() {
    let rig = {
        let mut r = Rig::new(json!([{"text": "first reply"}]));
        r.size = (48, 160);
        r
    };
    rig.drive(json!([
        expect("Enter send"),
        send("alpha-one\r"),
        expect("first reply"),
        quiet()
    ]));
    for _ in 0..2 {
        rig.drive(json!([expect("Enter send"), quiet()]));
    }
    let out = rig.drive(json!([expect("Enter send"), quiet()]));
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4, "every launch stores a session");
    let flat = squash(&out);
    assert!(flat.contains("alpha-one"), "{flat}");
    assert!(!flat.contains("(untitled)"), "{flat}");
}

/// Three workspaces by ctrl+n, then the Workspace / Running / Activity panels by mouse and Tab:
/// clicking a workspace lists its sessions, clicking a stored session resumes it, the Tab order
/// is chat -> Workspace -> Running -> chat, a click on a prompt in Activity scrolls the chat to
/// it, the wheel over Workspace moves its selection and `/new` opens the selected workspace.
#[test]
fn workspace_panels_by_mouse_and_tab_and_new_in_the_selected_workspace() {
    let long = |n: u32| {
        (1..=8)
            .map(|i| format!("r{n} line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rig = {
        let mut r = Rig::new(json!([
            {"text": long(1)}, {"text": long(2)}, {"text": long(3)},
            {"text": long(4)}, {"text": long(5)}, {"text": long(6)}
        ]));
        r.size = (48, 160);
        r
    };
    for d in ["ws2", "ws3"] {
        std::fs::create_dir_all(rig.root().join(d).join(".git")).unwrap();
    }
    // a workspace by typed path, as in the picker
    let open = |name: &str| {
        vec![
            send("\u{e}"),
            expect_new("use this folder"),
            send(&rig.root().join(name).display().to_string()),
            expect_new("open "),
            send("\r"),
            // the chat title names the folder
            expect_new(&format!("/{name}")),
            quiet(),
        ]
    };
    let mut steps = vec![
        expect("Enter send"),
        send("alpha-one\r"),
        expect("r1 line 8"),
        quiet(),
    ];
    steps.extend(open("ws2"));
    steps.extend([send("beta-one\r"), expect_new("r1 line 8"), quiet()]);
    steps.extend(open("ws3"));
    // six prompts in the live session: more than the chat shows
    for (i, w) in [
        "gamma-one",
        "gamma-two",
        "gamma-three",
        "gamma-four",
        "gamma-five",
        "gamma-six",
    ]
    .iter()
    .enumerate()
    {
        steps.extend([
            send(&format!("{w}\r")),
            expect_new(&format!("r{} line 8", i + 1)),
            quiet(),
        ]);
    }
    // Activity: click every sidebar row from the bottom up; the last prompt row hit is the oldest
    let sweep: String = (24..=47).rev().map(|y| click(4, y)).collect();
    steps.extend([send(&sweep), expect_new("gamma-one"), quiet()]);
    // Workspace: live ws3 first, then ws2 (newer) and ws; the third row is ws
    steps.extend([send(&click(4, 6)), expect_new("alpha-one"), quiet()]);
    // its stored session is the first Running row: it resumes
    steps.extend([
        send(&click(4, 9)),
        expect_new("alpha-one"),
        expect_new("r1 line 8"),
        quiet(),
    ]);
    // Tab: chat -> Workspace -> Running -> chat, told apart by the footer
    steps.extend([
        send("\t"),
        expect_new("select workspace"),
        send("\t"),
        expect_new("open it"),
        send("\t"),
        expect_new("workspaces → running"),
        quiet(),
    ]);
    // the wheel over Workspace selects the next workspace: its sessions show in Running
    steps.extend([send(&wheel(4, 5, true)), expect_new("gamma-one"), quiet()]);
    steps.extend([send(&wheel(4, 5, true)), expect_new("beta-one"), quiet()]);
    // /new: a fresh session in the selected workspace (ws2), not in the live one
    steps.extend([send("/new\r"), expect_new("/ws2"), quiet()]);
    let out = rig.drive(json!(steps));
    assert!(!squash(&out).contains("synt: error"), "{}", squash(&out));
    let starts = |ev: &[(String, Value)]| -> Vec<(String, String)> {
        ev.iter()
            .filter(|e| e.0 == "session.started")
            .filter_map(|e| {
                Some((
                    e.1["workspace"]["cwd"].as_str()?.to_string(),
                    e.1["source"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect()
    };
    let in_ws2 = |ev: &[(String, Value)]| {
        starts(ev)
            .iter()
            .filter(|(c, _)| c.ends_with("/ws2"))
            .count()
    };
    let t0 = std::time::Instant::now();
    let mut ev = rig.events();
    while in_ws2(&ev) < 2 && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        ev = rig.events();
    }
    assert_eq!(
        in_ws2(&ev),
        2,
        "ws2 got a second, fresh session: {:?}",
        starts(&ev)
    );
    assert!(
        starts(&ev)
            .iter()
            .any(|(c, s)| c.ends_with("/ws") && s == "resume"),
        "the stored session of ws was resumed: {:?}",
        starts(&ev)
    );
}

/// Text of every row write (`ESC[<row>;1H<text>ESC[K`) in `raw`, CSI sequences removed, grouped
/// into frames (a frame ends where the row number stops growing).
fn frames(raw: &str) -> Vec<std::collections::BTreeMap<usize, String>> {
    let mut out: Vec<std::collections::BTreeMap<usize, String>> = vec![Default::default()];
    let (mut last, mut rest) = (0, raw);
    while let Some(at) = rest.find("\x1b[") {
        rest = &rest[at + 2..];
        let n = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        if !rest[n..].starts_with(";1H") {
            continue;
        }
        let row: usize = rest[..n].parse().unwrap();
        let body = &rest[n + 3..];
        let end = body.find("\x1b[K").unwrap_or(body.len());
        let (mut text, mut chars) = (String::new(), body[..end].chars().peekable());
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                text.push(c);
                continue;
            }
            chars.next();
            while chars.next().is_some_and(|c| !c.is_ascii_alphabetic()) {}
        }
        if row <= last {
            out.push(Default::default());
        }
        last = row;
        out.last_mut().unwrap().insert(row, text);
    }
    out
}

/// The terminal bytes of a relaunch (`mark` .. `cut`) must not flash: no clear, no leaving the
/// alternate screen, and the right-hand panels (Topology, Usage) of the first full frame are
/// already what they stay (the model name in the topology card aside).
fn assert_seamless(raw: &str, rows: usize) {
    for bad in ["\x1b[2J", "\x1b[?1049l", "\x1b[?1049h", "\x1b[3J"] {
        assert!(!raw.contains(bad), "{bad:?} written during the switch");
    }
    let fr = frames(raw);
    let first = fr
        .iter()
        .position(|f| f.len() >= rows)
        .expect("no full frame after the switch");
    let mut settled = fr[first].clone();
    for f in &fr[first + 1..] {
        settled.extend(f.iter().map(|(k, v)| (*k, v.clone())));
    }
    let tail = |t: &str| {
        let c: Vec<char> = t.chars().collect();
        c[c.len().saturating_sub(46)..].iter().collect::<String>()
    };
    for r in 2..=rows {
        let (a, b) = (tail(&fr[first][&r]), tail(&settled[&r]));
        if a != b && !(a.contains("SYNTHESIS CORE") && b.contains("SYNTHESIS CORE")) {
            panic!("row {r} changed after the first frame:\n{a}\n{b}");
        }
    }
    assert!(
        !fr[first].values().any(|t| t.contains("loading usage")),
        "the first frame shows an empty Usage panel"
    );
}

#[test]
fn model_switch_first_frame_is_the_settled_frame_and_nothing_clears() {
    let mut rig = Rig::new(json!([{"text": "first reply"}]));
    rig.size = (48, 160);
    rig.drive(json!([
        expect("Enter send"),
        quiet(),
        send("hi\r"),
        expect_new("first reply"),
        expect_new("tokens \u{b7}"),
        // the Topology user wire carries a conveyor dot for PROMPT_FLASH_MS (5 s) after a
        // prompt: let that flash end first, or its expiry lands in the middle of the relaunch
        // and the wire cell changes after the first frame (a real animation, not stale state)
        json!(["wait", 6]),
        quiet(),
        json!(["mark"]),
        send("/model fake/other\r"),
        expect_new("fake/other"),
        quiet(),
        json!(["cut"])
    ]));
    assert_seamless(&rig.raw(), 48);
}

#[test]
fn new_session_and_running_panel_switch_keep_the_screen_without_a_flash() {
    let mut rig = Rig::new(json!([{"text": "first reply"}]));
    rig.size = (48, 160);
    // /new: a fresh session in the same folder
    rig.drive(json!([
        expect("Enter send"),
        quiet(),
        send("alpha-one\r"),
        expect_new("first reply"),
        quiet(),
        json!(["mark"]),
        send("/new\r"),
        json!(["wait", 2]),
        quiet(),
        json!(["cut"])
    ]));
    assert_seamless(&rig.raw(), 48);
    // a second launch lists the first session in the Running panel: Tab Tab, Down, Enter
    rig.drive(json!([
        expect("Enter send"),
        quiet(),
        send("\t\t"),
        quiet(),
        send("\x1b[B"),
        quiet(),
        json!(["mark"]),
        send("\r"),
        expect_new("alpha-one"),
        json!(["wait", 1]),
        quiet(),
        json!(["cut"])
    ]));
    assert_seamless(&rig.raw(), 48);
}

#[test]
fn close_hides_the_session_and_opens_a_fresh_one_without_a_flash() {
    let mut rig = Rig::new(json!([{"text": "first reply"}]));
    rig.size = (48, 160);
    let out = rig.drive(json!([
        expect("Enter send"),
        quiet(),
        send("alpha-one\r"),
        expect_new("first reply"),
        quiet(),
        json!(["mark"]),
        send("/close\r"),
        json!(["wait", 2]),
        quiet(),
        json!(["cut"])
    ]));
    assert_seamless(&rig.raw(), 48);
    assert!(
        !squash(&out).contains("could not close"),
        "{}",
        squash(&out)
    );
    let c = rusqlite::Connection::open(rig.home().join("synt.db")).unwrap();
    let closed: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE closed_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(closed, 1);
    let msgs: i64 = c
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert!(msgs >= 1, "messages are kept");
    // the next launch does not list the closed session
    let next = rig.drive(json!([expect("Enter send"), quiet()]));
    assert!(!squash(&next).contains("alpha-one"), "{}", squash(&next));
}
