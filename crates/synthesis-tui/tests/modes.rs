//! Permission mode switching against a scripted TTY: Shift+Tab cycle, the bypass confirmation,
//! the footer, and bypass never being a starting mode.
#![cfg(unix)]

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use synthesis_config::Env;
use synthesis_core::{AgentSession, Budget, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_harness::testing::FakeProvider;
use synthesis_harness::{EventSink, Harness, HarnessConfig, ToolBox};
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_tools::Registry;
use synthesis_tui::{Exit, Options, ScriptedTty, UiLink, run_interactive};

struct Rig {
    tty: ScriptedTty,
    join: Option<JoinHandle<io::Result<Exit>>>,
    harness: Arc<Harness>,
    _dir: tempfile::TempDir,
}

fn rig(start: synthesis_core::PermissionMode) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("w")).unwrap();
    let (script, tty) = ScriptedTty::new(120, 40);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ws = WorkspaceContext::open(
        "w",
        &root.join("w"),
        &root.join("w"),
        &[],
        root.join("art"),
        "pg",
    )
    .unwrap();
    let session = AgentSession {
        id: "s1".into(),
        task_id: None,
        status: SessionStatus::Created,
        workspace: ws,
        model_config_id: "fake/model".into(),
        permission_policy_id: "p".into(),
        permission_mode: start,
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
        PermissionRules::default(),
        synthesis_permissions::Env::new(root.join("w"), root.join("home"), Mode::Default)
            .interactive(true),
    );
    let harness = Arc::new(
        Harness::new(
            Arc::new(FakeProvider::new(vec![])),
            ToolBox::new(Registry::unsandboxed_for_tests()),
            perms,
            sink,
            HarnessConfig::new("fake-model"),
        )
        .with_approver(link.approver())
        .with_input(link.input()),
    );
    let h2 = harness.clone();
    let join = std::thread::spawn(move || {
        let mut o = Options::new(
            h2,
            session,
            link,
            rt.handle().clone(),
            Env::from_pairs([("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
        );
        o.min_frame = Duration::from_millis(1);
        o.tick = Duration::from_millis(10);
        o.idle_tick = Duration::from_millis(20);
        o.quit_grace = Duration::from_secs(3);
        run_interactive(o, tty)
    });
    Rig {
        tty: script,
        join: Some(join),
        harness,
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

    fn engine_mode(&self) -> Mode {
        self.harness.permissions().lock().unwrap().mode()
    }

    /// Waits for the host to have applied the switch (the command is handled after the key).
    fn wait_engine(&self, m: Mode) {
        let t0 = Instant::now();
        while self.engine_mode() != m {
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "engine mode stays {:?}",
                self.engine_mode()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn quit(&mut self) {
        self.tty.send("/exit\r");
        let _ = self.join.take().unwrap().join();
    }
}

const READY: &str = "Enter send";
const SHIFT_TAB: &str = "\x1b[Z";

#[test]
fn shift_tab_cycles_default_accept_plan_auto_and_the_footer_follows() {
    let mut r = rig(synthesis_core::PermissionMode::Default);
    r.wait_for(READY);
    for (m, shown) in [
        (Mode::AcceptEdits, "accept edits"),
        (Mode::Plan, "plan"),
        (Mode::Auto, "auto"),
    ] {
        r.tty.send(SHIFT_TAB);
        r.wait_engine(m);
        r.wait_for(shown);
    }
    r.quit();
}

#[test]
fn bypass_needs_the_confirmation_and_one_shift_tab_leaves_it() {
    let mut r = rig(synthesis_core::PermissionMode::Default);
    r.wait_for(READY);
    // default -> acceptEdits -> plan -> auto, then the fourth press asks about bypass
    for m in [Mode::AcceptEdits, Mode::Plan, Mode::Auto] {
        r.tty.send(SHIFT_TAB);
        r.wait_engine(m);
    }
    // Esc keeps the previous mode
    r.tty.send(SHIFT_TAB);
    r.wait_for("Bypass permissions?");
    r.wait_for("ask rules");
    r.tty.send("\x1b");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(r.engine_mode(), Mode::Auto);
    // so does the highlighted default, "No"
    r.tty.send(SHIFT_TAB);
    r.wait_for("Bypass permissions?");
    r.tty.send("\r");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(r.engine_mode(), Mode::Auto);
    // an explicit yes switches the engine
    r.tty.send(SHIFT_TAB);
    r.wait_for("Bypass permissions?");
    r.tty.send("\x1b[B\r");
    r.wait_engine(Mode::BypassPermissions);
    r.wait_for("bypass permissions");
    // and one Shift+Tab leaves it
    r.tty.send(SHIFT_TAB);
    r.wait_engine(Mode::Default);
    r.quit();
}

#[test]
fn a_session_never_starts_in_bypass() {
    let mut r = rig(synthesis_core::PermissionMode::BypassPermissions);
    r.wait_for(READY);
    r.wait_for("manual");
    assert!(!r.tty.screen_text().contains("bypass permissions"));
    assert_eq!(r.engine_mode(), Mode::Default);
    r.quit();
}

#[test]
fn the_core_refuses_an_unconfirmed_bypass_whatever_the_ui_sent() {
    let r = rig(synthesis_core::PermissionMode::Default);
    assert_eq!(
        r.harness.set_mode(Mode::BypassPermissions, false),
        Err(synthesis_harness::ModeError::BypassNotConfirmed)
    );
    assert_eq!(r.engine_mode(), Mode::Default);
}
