//! Ctrl+click / right click / alt+click on urls and paths through the whole chain (SGR mouse bytes
//! -> key parser -> app -> opener), with a recording opener instead of a real desktop, and OSC 8.
#![cfg(unix)]

use std::io;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use synthesis_config::Env;
use synthesis_core::{AgentSession, Budget, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_harness::{EventSink, Harness, HarnessConfig, ToolBox};
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_tools::Registry;
use synthesis_tui::{Exit, OpenTarget, Opener, Options, ScriptedTty, UiLink, run_interactive};

struct Rec(Arc<Mutex<Vec<OpenTarget>>>);
impl Opener for Rec {
    fn open(&self, t: &OpenTarget) -> io::Result<()> {
        self.0.lock().unwrap().push(t.clone());
        Ok(())
    }
}

struct Rig {
    tty: ScriptedTty,
    join: Option<JoinHandle<io::Result<Exit>>>,
    _harness: Arc<Harness>,
    _dir: tempfile::TempDir,
}

fn rig(reply: &str, hyperlinks: &'static str, opened: Arc<Mutex<Vec<OpenTarget>>>) -> Rig {
    let start = synthesis_core::PermissionMode::Default;
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("w")).unwrap();
    std::fs::write(root.join("w").join("notes.txt"), "x").unwrap();
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
            Arc::new(FakeProvider::new(vec![Turn::text(reply)])),
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
            Env::from_pairs([
                ("TERM", "xterm-256color"),
                ("SYNTHESIS_HYPERLINKS", hyperlinks),
            ]),
        );
        o.opener = Some(Box::new(Rec(opened)));
        o.min_frame = Duration::from_millis(1);
        o.tick = Duration::from_millis(10);
        o.idle_tick = Duration::from_millis(20);
        o.quit_grace = Duration::from_secs(3);
        run_interactive(o, tty)
    });
    Rig {
        tty: script,
        join: Some(join),
        _harness: harness,
        _dir: dir,
    }
}

impl Rig {
    fn wait_for(&self, needle: &str) {
        let t0 = Instant::now();
        while !self.tty.screen_text().contains(needle) {
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "timed out waiting for {needle:?}; screen:\n{}",
                self.tty.screen_text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn cell_of(&self, needle: &str) -> (usize, usize) {
        for (y, l) in self.tty.screen().iter().enumerate() {
            if let Some(i) = l.find(needle) {
                return (l[..i].chars().count() + 1, y + 1);
            }
        }
        panic!("{needle:?} not on screen:\n{}", self.tty.screen_text());
    }

    fn quit(&mut self) {
        self.tty.send("\x04");
        let _ = self.join.take().unwrap().join();
    }
}

fn ask(r: &Rig, needle: &str) {
    r.wait_for("Enter send");
    r.tty.send("q");
    r.tty.send("\r");
    r.wait_for(needle);
}

#[test]
fn ctrl_right_and_alt_click_open_urls_and_reveal_paths() {
    let opened = Arc::new(Mutex::new(vec![]));
    let mut r = rig(
        "docs https://example.com/guide and notes.txt:7 or gone.txt end",
        "0",
        opened.clone(),
    );
    ask(&r, "gone.txt end");
    let (ux, uy) = r.cell_of("https://example.com/guide");
    let (px, py) = r.cell_of("notes.txt");
    let (gx, gy) = r.cell_of("gone.txt");
    // SGR: 16 = ctrl + left (kitty, Windows Terminal), 2 = right (macOS ctrl+click), 8 = alt + left
    for code in [16, 2, 8] {
        r.tty.send(&format!("\x1b[<{code};{};{uy}M", ux + 3));
        r.tty.send(&format!("\x1b[<{code};{};{py}M", px + 2));
    }
    let t0 = Instant::now();
    while opened.lock().unwrap().len() < 6 {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "{:?}",
            opened.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let got = opened.lock().unwrap().clone();
    for pair in got.chunks(2) {
        assert_eq!(pair[0], OpenTarget::Url("https://example.com/guide".into()));
        assert!(
            matches!(&pair[1], OpenTarget::RevealFile(p) if p.ends_with("w/notes.txt")),
            "{pair:?}"
        );
    }
    r.tty.send(&format!("\x1b[<16;{};{gy}M", gx + 2));
    r.wait_for("not found gone.txt");
    assert_eq!(
        opened.lock().unwrap().len(),
        6,
        "a missing path opens nothing"
    );
    r.tty.send(&format!("\x1b[<0;{};{uy}M", ux + 3)); // plain click
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(opened.lock().unwrap().len(), 6);
    r.quit();
}

#[test]
fn osc8_links_are_emitted_only_when_asked_and_do_not_move_the_text() {
    for (flag, want) in [("1", true), ("0", false)] {
        let mut r = rig("see https://example.com/guide now", flag, Arc::default());
        ask(&r, "https://example.com/guide now");
        let raw = r.tty.raw();
        assert_eq!(
            raw.contains("\x1b]8;;https://example.com/guide\x1b\\"),
            want
        );
        assert!(
            r.tty
                .screen_text()
                .contains("see https://example.com/guide now")
        );
        r.quit();
    }
}
