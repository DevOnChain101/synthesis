//! `!cmd` against a scripted TTY: real harness, real Bash tool, `FakeProvider`, no network.
#![cfg(unix)]

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use synthesis_config::Env;
use synthesis_core::{AgentSession, Budget, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_harness::{EventSink, Harness, HarnessConfig, ToolBox};
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_tools::Registry;
use synthesis_tui::{Exit, Options, ScriptedTty, UiLink, run_interactive};

struct Rig {
    tty: ScriptedTty,
    join: Option<JoinHandle<io::Result<Exit>>>,
    provider: Arc<FakeProvider>,
    work: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

#[derive(Default)]
struct Knobs {
    deny: Vec<String>,
    no_model_turn: bool,
}

fn rig(turns: Vec<Turn>, k: Knobs) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("w")).unwrap();
    let (script, tty) = ScriptedTty::new(120, 40);
    let provider = Arc::new(FakeProvider::new(turns));
    let (p2, r2) = (provider.clone(), root.clone());
    let join = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ws =
            WorkspaceContext::open("w", &r2.join("w"), &r2.join("w"), &[], r2.join("art"), "pg")
                .unwrap();
        let session = AgentSession {
            id: "s1".into(),
            task_id: None,
            status: SessionStatus::Created,
            workspace: ws,
            model_config_id: "fake/model".into(),
            permission_policy_id: "p".into(),
            permission_mode: synthesis_core::PermissionMode::Default,
            event_cursor: 0,
            budget: Budget::default(),
            usage: BudgetUsage::default(),
            created_at: String::new(),
            updated_at: String::new(),
            parent: None,
        };
        let link = UiLink::new();
        let sink = EventSink::new(link.event_writer(), "s1", []);
        let perms = Permissions::new(
            PermissionRules {
                deny: k.deny,
                ..PermissionRules::default()
            },
            synthesis_permissions::Env::new(r2.join("w"), r2.join("home"), Mode::Default)
                .interactive(true),
        );
        let mut cfg = HarnessConfig::new("fake-model");
        cfg.recovery_delay = Duration::from_millis(1);
        cfg.shell.user_commands_model_turn = !k.no_model_turn;
        let harness = Harness::new(
            p2,
            ToolBox::new(Registry::unsandboxed_for_tests()),
            perms,
            sink,
            cfg,
        )
        .with_approver(link.approver())
        .with_input(link.input());
        let mut o = Options::new(
            Arc::new(harness),
            session,
            link,
            rt.handle().clone(),
            Env::from_pairs([("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
        );
        o.min_frame = Duration::from_millis(1);
        o.stream_frame = Duration::from_millis(1);
        o.tick = Duration::from_millis(10);
        o.idle_tick = Duration::from_millis(20);
        o.quit_grace = Duration::from_secs(3);
        run_interactive(o, tty)
    });
    Rig {
        tty: script,
        join: Some(join),
        provider,
        work: root.join("w"),
        _dir: dir,
    }
}

impl Rig {
    fn wait_for(&self, needle: &str) {
        let t0 = Instant::now();
        loop {
            let s = self.tty.screen_text();
            if s.contains(needle) {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "timed out waiting for {needle:?}; screen:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn run(&self, cmd: &str) {
        self.tty.send(cmd);
        self.tty.send("\r");
    }

    fn finish(&mut self) -> Exit {
        self.join.take().unwrap().join().unwrap().unwrap()
    }

    fn history_text(&self, request: usize) -> String {
        format!("{:?}", self.provider.requests()[request].messages)
    }
}

const READY: &str = "Enter send";

#[test]
fn bang_command_runs_enters_the_conversation_and_the_model_analyses_it() {
    let mut r = rig(
        vec![Turn::text("The command printed hi and exited 0.")],
        Knobs::default(),
    );
    r.wait_for(READY);
    r.tty.send("!");
    r.wait_for("! bash mode");
    r.run("echo hi");
    r.wait_for("> !echo hi"); // what the user typed
    r.wait_for("The command printed hi and exited 0."); // the model turn's reply
    let s = r.tty.screen_text();
    assert!(s.contains("Bash(") && s.contains("echo hi"), "{s}");
    assert!(
        !s.contains("Synthesis note") && !s.contains("not typed by the user"),
        "the implicit prompt is not shown as user text: {s}"
    );
    let reqs = r.provider.requests();
    assert_eq!(reqs.len(), 1, "one model turn");
    let h = r.history_text(0);
    assert!(
        h.contains("<user-bash-input>echo hi</user-bash-input>")
            && h.contains("<user-bash-output exit=0 truncated=false>")
            && h.contains("Synthesis note, not typed by the user"),
        "{h}"
    );
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn double_bang_records_without_a_model_turn_and_the_next_prompt_sees_it() {
    let mut r = rig(vec![Turn::text("I saw the output.")], Knobs::default());
    r.wait_for(READY);
    r.tty.send("!");
    r.tty.send("!");
    r.run("echo recorded-line");
    r.wait_for("recorded-line");
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.provider.requests().is_empty(), "no model turn for !!");
    r.tty.send("what did it print");
    r.tty.send("\r");
    r.wait_for("I saw the output.");
    let h = r.history_text(0);
    assert!(
        h.contains("<user-bash-output exit=0") && h.contains("recorded-line"),
        "{h}"
    );
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn setting_off_means_no_model_turn_for_bang() {
    let mut r = rig(
        vec![],
        Knobs {
            no_model_turn: true,
            ..Knobs::default()
        },
    );
    r.wait_for(READY);
    r.tty.send("!");
    r.run("echo quiet");
    r.wait_for("quiet");
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.provider.requests().is_empty());
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn a_deny_rule_blocks_the_typed_command_and_shows_why() {
    let mut r = rig(
        vec![],
        Knobs {
            deny: vec!["Bash(touch:*)".into()],
            ..Knobs::default()
        },
    );
    r.wait_for(READY);
    r.tty.send("!");
    r.run("touch nope.txt");
    r.wait_for("Blocked: blocked by a deny rule");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!r.work.join("nope.txt").exists());
    assert!(r.provider.requests().is_empty(), "nothing to analyse");
    // the prompt is usable again
    r.tty.send("!");
    r.run("echo still-works");
    r.wait_for("still-works");
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn esc_cancels_a_long_command_and_no_model_turn_follows() {
    let mut r = rig(vec![Turn::text("must not be asked")], Knobs::default());
    r.wait_for(READY);
    r.tty.send("!");
    r.run("sleep 60 & echo $! > child.pid; echo ready-$((6*7)); wait");
    r.wait_for("ready-42"); // the output; the command text shows `$((6*7))`
    r.tty.send("\x1b");
    let t0 = Instant::now();
    // the spinner is gone and the prompt takes input again
    r.tty.send("!");
    loop {
        // typed while the command still ran would be kept in the editor; wait for idle
        if !r.tty.screen_text().contains("esc to interrupt") {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "{}",
            r.tty.screen_text()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(r.provider.requests().is_empty(), "cancelled: no model turn");
    let pid: i32 = std::fs::read_to_string(r.work.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = |p: i32| {
        std::process::Command::new("kill")
            .args(["-0", &p.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    let t1 = Instant::now();
    while alive(pid) {
        assert!(
            t1.elapsed() < Duration::from_secs(10),
            "child {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    r.tty.send("\x03");
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn running_command_shows_the_live_tail_of_its_output() {
    let mut r = rig(
        vec![],
        Knobs {
            no_model_turn: true,
            ..Knobs::default()
        },
    );
    r.wait_for(READY);
    r.tty.send("!");
    r.run("for i in 1 2 3 4 5 6 7 8 9 10; do echo line$i; done; sleep 30");
    r.wait_for("line10");
    let s = r.tty.screen_text();
    assert!(
        s.contains("line5") && !s.contains("line4"),
        "tail of 6 lines:\n{s}"
    );
    assert!(s.contains("+4 more lines (ctrl+o to expand)"), "{s}");
    r.tty.send("\x0f"); // ctrl+o: everything
    r.wait_for("(ctrl+o to collapse)");
    assert!(r.tty.screen_text().contains("line1 "));
    r.tty.send("\x1b");
    r.wait_for("interrupted");
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn bash_mode_keys_and_paste_never_start_a_command() {
    let mut r = rig(vec![], Knobs::default());
    r.wait_for(READY);
    r.tty.send("!");
    r.wait_for("! bash mode");
    r.tty.send("?"); // literal in the shell prompt, not /help
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.tty.screen_text().contains("commands and keys"));
    r.tty.send("\x7f\x7f"); // backspace twice: text, then the `!` itself
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.tty.screen_text().contains("! bash mode"));
    r.tty.send("!");
    r.wait_for("! bash mode");
    r.tty.send("\x1b"); // esc leaves it
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.tty.screen_text().contains("! bash mode"));
    // a paste that starts with `!` is text, not a command
    r.tty.send("\x1b[200~!echo pasted\x1b[201~");
    r.wait_for("echo pasted");
    assert!(!r.tty.screen_text().contains("! bash mode"));
    // keys right after a paste are held back for a moment (forged paste ends)
    std::thread::sleep(Duration::from_millis(800));
    r.tty.send("\x03"); // ctrl+c clears the draft
    std::thread::sleep(Duration::from_millis(200));
    r.tty.send("\x04");
    r.finish();
}
