//! The whole app against a scripted TTY: real harness, `FakeProvider`, no network, no model.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use std::sync::Mutex;
use synthesis_config::Env;
use synthesis_core::{
    AgentSession, Budget, BudgetUsage, SessionStatus, ToolContext, WorkspaceContext,
};
use synthesis_events::TaskStatus;
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_harness::{EventSink, Harness, HarnessConfig, ToolBox};
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_tools::{BoxFuture, SideEffectClass, Tool, ToolOutput, ToolRuntime, ToolSpec};
use synthesis_tui::{
    Exit, ExitReason, OpenTarget, Opener, Options, ScriptedTty, UiLink, run_interactive,
};
use synthesis_tui_base::clipboard::{ClipboardSink, ClipboardSource};

/// A write-class tool: the policy asks before it runs.
struct Mutate {
    spec: ToolSpec,
    runs: AtomicUsize,
}

impl Mutate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            spec: ToolSpec {
                name: "Mutate".into(),
                description: "changes something".into(),
                input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
                side_effect: SideEffectClass::Write,
                runtime: ToolRuntime::Builtin,
            },
            runs: AtomicUsize::new(0),
        })
    }
    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

impl Tool for Mutate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn execute<'a>(&'a self, _: Value, _: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolOutput::text("changed") })
    }
}

struct Rig {
    tty: ScriptedTty,
    join: Option<JoinHandle<io::Result<Exit>>>,
    pub tool: Arc<Mutate>,
    pub provider: Arc<FakeProvider>,
    _dir: tempfile::TempDir,
}

/// Records what was copied / opened; `fail_open` makes the opener fail like a missing tool.
#[derive(Clone, Default)]
struct Log {
    copied: Arc<Mutex<Vec<String>>>,
    opened: Arc<Mutex<Vec<String>>>,
    fail_open: bool,
}

struct RecClip(Arc<Mutex<Vec<String>>>);
impl ClipboardSink for RecClip {
    fn copy(&self, text: &str) -> io::Result<()> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

/// A clipboard to paste from: counts the reads, answers with `text` or fails.
#[derive(Clone)]
struct FakePaste {
    text: Result<String, String>,
    reads: Arc<AtomicUsize>,
}

impl FakePaste {
    fn new(text: Result<&str, &str>) -> Self {
        Self {
            text: text.map(String::from).map_err(String::from),
            reads: Arc::default(),
        }
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl ClipboardSource for FakePaste {
    fn paste(&self) -> io::Result<String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.text.clone().map_err(io::Error::other)
    }
}

struct RecOpen(Log);
impl Opener for RecOpen {
    fn open(&self, t: &OpenTarget) -> io::Result<()> {
        self.0.opened.lock().unwrap().push(t.describe());
        if self.0.fail_open {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "xdg-open: not found",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Default)]
struct Knobs {
    approval_timeout: Option<Duration>,
    clock: Option<fn() -> i64>,
    stream_frame: Option<Duration>,
    log: Option<Log>,
    notes: Vec<String>,
    paste: Option<FakePaste>,
}

fn rig(turns: Vec<Turn>, k: Knobs) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("w")).unwrap();
    let (script, tty) = ScriptedTty::new(120, 40);
    let tool = Mutate::new();
    let provider = Arc::new(FakeProvider::new(turns));
    let (t2, p2, r2) = (tool.clone(), provider.clone(), root.clone());
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
        let mut link = UiLink::new();
        if let Some(d) = k.approval_timeout {
            link = link.with_approval_timeout(d);
        }
        let sink = EventSink::new(link.event_writer(), "s1", []);
        let perms = Permissions::new(
            PermissionRules::default(),
            synthesis_permissions::Env::new(r2.join("w"), r2.join("home"), Mode::Default)
                .interactive(true),
        );
        let mut cfg = HarnessConfig::new("fake-model");
        cfg.recovery_delay = Duration::from_millis(1);
        let harness = Harness::new(p2, ToolBox::empty().with_tool(t2), perms, sink, cfg)
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
        o.stream_frame = k.stream_frame.unwrap_or(Duration::from_millis(1));
        if let Some(l) = &k.log {
            o.clipboard = Some(vec![Box::new(RecClip(l.copied.clone()))]);
            o.opener = Some(Box::new(RecOpen(l.clone())));
        }
        if let Some(p) = &k.paste {
            o.paste_source = Some(vec![Box::new(p.clone())]);
        }
        o.tick = Duration::from_millis(10);
        o.idle_tick = Duration::from_millis(20);
        o.quit_grace = Duration::from_secs(3);
        o.sandbox.mode = "enforce".into();
        o.startup_notes = k.notes.clone();
        if let Some(c) = k.clock {
            o.clock = c;
        }
        run_interactive(o, tty)
    });
    Rig {
        tty: script,
        join: Some(join),
        tool,
        provider,
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
                t0.elapsed() < Duration::from_secs(10),
                "timed out waiting for {needle:?}; screen:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The approval dialog ignores Enter / digits for a moment after it appears.
    fn wait_armed(&self) {
        let t0 = Instant::now();
        while self.tty.screen_text().contains("wait…") {
            assert!(t0.elapsed() < Duration::from_secs(10), "still arming");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&mut self) -> Exit {
        self.join.take().unwrap().join().unwrap().unwrap()
    }
}

#[test]
fn prompt_to_answer_then_clean_exit_restores_the_terminal() {
    let mut r = rig(vec![Turn::text("The answer is 42")], Knobs::default());
    r.wait_for("Enter send");
    r.tty.send("what is it");
    r.wait_for("what is it");
    r.tty.send("\r");
    r.wait_for("The answer is 42");
    r.wait_for("Done");
    let s = r.tty.screen_text();
    assert!(s.contains("> what is it"), "{s}");
    assert!(
        s.lines().last().unwrap().contains("sandbox: enforce"),
        "{s}"
    );
    r.tty.send("\x04"); // ctrl+d on an empty prompt
    let exit = r.finish();
    assert_eq!(exit.reason, ExitReason::Quit);
    assert!(exit.session.is_some());
    assert_eq!(exit.last_outcome.unwrap().status, TaskStatus::Completed);
    let raw = r.tty.raw();
    assert!(
        raw.contains("\x1b[?2004h"),
        "bracketed paste was switched on"
    );
    assert!(
        raw.ends_with("\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[23;2t"),
        "restore sequence is the last thing written"
    );
    let req = &r.provider.requests()[0];
    assert!(format!("{:?}", req.messages).contains("what is it"));
}

#[test]
fn startup_notes_are_in_the_first_frames_and_escaped() {
    let notes = vec![
        "unsandboxed_by_request: --sandbox off, commands run with the agent's full privileges"
            .to_string(),
        "evil \x1b]0;pwned\x07\x1b[2Jtail".to_string(),
    ];
    let mut r = rig(
        vec![],
        Knobs {
            notes: notes.clone(),
            ..Knobs::default()
        },
    );
    r.wait_for("unsandboxed_by_request: --sandbox off");
    r.wait_for("tail");
    let screen = r.tty.screen_text();
    assert!(screen.contains("evil"), "{screen}");
    let raw = r.tty.raw();
    assert!(
        !raw.contains("\x1b]0;pwned"),
        "hostile OSC reached the terminal"
    );
    assert!(raw.contains("unsandboxed_by_request"));
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn approval_dialog_allow_then_deny() {
    let mut r = rig(
        vec![
            Turn::tool("m1", "Mutate", json!({"x": "one"})),
            Turn::tool("m2", "Mutate", json!({"x": "two"})),
            Turn::text("all done"),
        ],
        Knobs::default(),
    );
    r.wait_for("Enter send");
    r.tty.send("change things");
    r.tty.send("\r");
    r.wait_for("Do you want to proceed?");
    let s = r.tty.screen_text();
    assert!(
        s.contains("Synthesis needs your permission") && s.contains("Mutate"),
        "{s}"
    );
    r.tty.send("1"); // typed 10 ms after the dialog opened: ignored
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        r.tool.runs(),
        0,
        "a keystroke inside the arming delay approved"
    );
    assert!(r.tty.screen_text().contains("Do you want to proceed?"));
    r.wait_armed();
    r.tty.send("1"); // yes
    r.wait_for("changed"); // the tool ran and its result is in the chat
    r.wait_for("Do you want to proceed?"); // the second call asks again
    r.tty.send("\x1b"); // esc: no (cancels even while arming)
    r.wait_for("all done");
    r.wait_for("task unverified"); // something changed and nothing verified it
    assert_eq!(r.tool.runs(), 1, "allowed once, denied once");
    let s = r.tty.screen_text();
    assert!(s.contains("rejected by you"), "{s}");
    assert!(
        s.lines().last().unwrap().contains("verify unverified"),
        "{s}"
    );
    r.tty.send("\x04");
    assert_eq!(r.finish().reason, ExitReason::Quit);
}

#[test]
fn unanswered_approval_expires_and_the_tool_does_not_run() {
    let mut r = rig(
        vec![Turn::tool("m1", "Mutate", json!({})), Turn::text("ok")],
        Knobs {
            approval_timeout: Some(Duration::from_millis(300)),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    r.tty.send("x");
    r.tty.send("\r");
    r.wait_for("Do you want to proceed?");
    r.wait_for("expires unanswered in");
    r.wait_for("approval expired (policy)");
    r.wait_for("ok");
    assert_eq!(r.tool.runs(), 0);
    assert!(
        !r.tty.screen_text().contains("Do you want to proceed?"),
        "dialog is gone"
    );
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn esc_cancels_the_running_task_and_the_app_stays_usable() {
    let mut r = rig(
        vec![
            Turn::text("never shown").delayed(60_000),
            Turn::text("second answer"),
        ],
        Knobs::default(),
    );
    r.wait_for("Enter send");
    r.tty.send("slow one");
    r.tty.send("\r");
    r.wait_for("esc to interrupt");
    r.tty.send("\x1b");
    r.wait_for("※ interrupted");
    assert!(!r.tty.screen_text().contains("esc to interrupt"));
    // a new task works after a cancel
    r.tty.send("again");
    r.tty.send("\r");
    r.wait_for("second answer");
    r.tty.send("\x04");
    let exit = r.finish();
    assert_eq!(exit.last_outcome.unwrap().status, TaskStatus::Completed);
}

#[test]
fn ctrl_c_cancels_first_and_quits_on_the_second() {
    let mut r = rig(vec![Turn::text("never").delayed(60_000)], Knobs::default());
    r.wait_for("Enter send");
    r.tty.send("slow");
    r.tty.send("\r");
    r.wait_for("esc to interrupt");
    r.tty.send("\x03");
    r.wait_for("※ interrupted");
    // idle again: the next ctrl+c only arms, the one after quits
    r.tty.send("\x03");
    r.wait_for("press ctrl+c again to quit");
    r.tty.send("\x03");
    let exit = r.finish();
    assert_eq!(exit.reason, ExitReason::Quit);
    assert_eq!(exit.last_outcome.unwrap().status, TaskStatus::Cancelled);
}

#[test]
fn resize_redraws_at_the_new_size() {
    let mut r = rig(vec![], Knobs::default());
    r.wait_for("Enter send");
    assert_eq!(r.tty.screen().len(), 40);
    r.tty.resize(100, 30);
    let t0 = Instant::now();
    while r.tty.screen().len() != 30 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no redraw after resize"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    r.wait_for("sandbox: enforce");
    assert!(
        r.tty
            .screen()
            .iter()
            .all(|l| synthesis_tui_base_width(l) <= 100)
    );
    // too small: a message, not a broken layout
    r.tty.resize(40, 10);
    r.wait_for("terminal too small");
    r.tty.send("\x04");
    r.finish();
}

fn synthesis_tui_base_width(l: &str) -> usize {
    l.chars().count() // box drawing and text here are single-width
}

#[test]
fn bracketed_paste_keeps_newlines_and_reaches_the_model() {
    let mut r = rig(vec![Turn::text("got it")], Knobs::default());
    r.wait_for("Enter send");
    r.tty.send("\x1b[200~line one\nline two\x1b[201~");
    r.wait_for("line two");
    std::thread::sleep(Duration::from_millis(100)); // bytes within 50 ms of the end marker are still the paste
    r.tty.send("\r");
    r.wait_for("got it");
    let req = &r.provider.requests()[0];
    assert!(
        format!("{:?}", req.messages).contains("line one\\nline two"),
        "{:?}",
        req.messages
    );
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn huge_forged_paste_cannot_answer_an_armed_approval_dialog() {
    let mut r = rig(
        vec![Turn::tool("m1", "Mutate", json!({})), Turn::text("done")],
        Knobs::default(),
    );
    r.wait_for("Enter send");
    r.tty.send("go");
    r.tty.send("\r");
    r.wait_for("Do you want to proceed?");
    r.wait_armed();
    // 3 MiB read in 4096-byte chunks: Enters, digits and forged end markers in the payload
    let mut paste = String::from("\x1b[200~");
    while paste.len() < 3 << 20 {
        paste.push_str("rm -rf x\r1\r2\r3 yes\x1b[201~\r1\r2\n");
    }
    r.tty.send(&paste);
    r.tty.send("\x1b[201~");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.tool.runs(), 0, "a paste answered the approval dialog");
    assert!(r.tty.screen_text().contains("Do you want to proceed?"));
    r.tty.send("\x1b"); // esc: no
    r.wait_for("done");
    assert_eq!(r.tool.runs(), 0);
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn input_closing_ends_the_app_and_restores() {
    let mut r = rig(vec![], Knobs::default());
    r.wait_for("Enter send");
    r.tty.close_input();
    let exit = r.finish();
    assert_eq!(exit.reason, ExitReason::InputClosed);
    assert!(r.tty.raw().contains("\x1b[?1049l"));
}

/// 1-based screen cell of the first occurrence of `needle` (cells == chars for this text).
fn cell_of(r: &Rig, needle: &str) -> (usize, usize) {
    for (y, l) in r.tty.screen().iter().enumerate() {
        if let Some(i) = l.find(needle) {
            return (l[..i].chars().count() + 1, y + 1);
        }
    }
    panic!("{needle:?} not on screen:\n{}", r.tty.screen_text());
}

#[test]
fn drag_selection_copies_through_the_clipboard_chain() {
    let log = Log::default();
    let mut r = rig(
        vec![Turn::text("alpha beta gamma")],
        Knobs {
            log: Some(log.clone()),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    r.tty.send("q");
    r.tty.send("\r");
    r.wait_for("alpha beta gamma");
    r.wait_for("Done");
    let (x, y) = cell_of(&r, "alpha beta gamma");
    r.tty.send(&format!("\x1b[<0;{x};{y}M"));
    r.tty.send(&format!("\x1b[<32;{};{y}M", x + 16));
    r.tty.send(&format!("\x1b[<0;{};{y}m", x + 16));
    r.wait_for("Copied 16 chars");
    assert_eq!(*log.copied.lock().unwrap(), ["alpha beta gamma"]);
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn ctrl_click_opens_a_link_and_a_failing_opener_is_reported() {
    for fail in [false, true] {
        let log = Log {
            fail_open: fail,
            ..Log::default()
        };
        let mut r = rig(
            vec![Turn::text("docs at https://example.com/guide today")],
            Knobs {
                log: Some(log.clone()),
                ..Knobs::default()
            },
        );
        r.wait_for("Enter send");
        r.tty.send("q");
        r.tty.send("\r");
        r.wait_for("https://example.com/guide");
        let (x, y) = cell_of(&r, "https://example.com/guide");
        r.tty.send(&format!("\x1b[<16;{};{y}M", x + 3)); // ctrl + left button
        if fail {
            r.wait_for("could not open https://example.com/guide: xdg-open: not found");
        } else {
            r.wait_for("opening https://example.com/guide");
        }
        assert_eq!(*log.opened.lock().unwrap(), ["https://example.com/guide"]);
        r.tty.send("\x04");
        r.finish();
    }
}

#[test]
fn frames_are_throttled_while_streaming_and_idle_is_quiet() {
    let mut r = rig(
        vec![Turn::text("slow reply").delayed(1500)],
        Knobs {
            stream_frame: Some(Duration::from_millis(300)),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    // idle: nothing is drawn while nothing changes
    std::thread::sleep(Duration::from_millis(100));
    let idle0 = r.tty.raw().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.tty.raw().len(), idle0, "an idle app writes nothing");
    let frames = |r: &Rig| r.tty.raw().matches("\x1b[?2026h").count();
    let before = frames(&r);
    r.tty.send("q");
    r.tty.send("\r");
    r.wait_for("esc to interrupt");
    let t0 = Instant::now();
    r.wait_for("slow reply");
    let drawn = frames(&r) - before;
    let secs = t0.elapsed().as_secs_f64() + 0.2;
    // 1 frame / 300 ms while only a spinner moves, plus the few caused by the keys and events
    assert!(
        (drawn as f64) <= secs / 0.3 + 8.0,
        "{drawn} frames in {secs:.1}s is not throttled"
    );
    r.tty.send("\x04");
    r.finish();
}

fn clock_that_panics() -> i64 {
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Ordering::SeqCst) > 30 {
        panic!("boom in the ui thread");
    }
    1_800_000_000_000
}

#[test]
fn terminal_is_restored_when_the_ui_thread_panics() {
    let mut r = rig(
        vec![],
        Knobs {
            clock: Some(clock_that_panics),
            ..Knobs::default()
        },
    );
    let j = r.join.take().unwrap();
    assert!(j.join().is_err(), "the thread panicked");
    let raw = r.tty.raw();
    assert!(
        raw.contains("\x1b[?1049l") && raw.contains("\x1b[?25h"),
        "restored before unwinding finished"
    );
}

#[cfg(unix)]
mod signals {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use synthesis_tui::Tty;

    const CHILD: &str = "SYNTHESIS_TUI_E2E_CHILD";

    /// Runs the app in this process with the process stdin/stdout as the terminal (a pipe pair,
    /// no raw mode), signals watched. Only does anything when the parent test spawned us.
    #[test]
    fn child_entry() {
        let Ok(mode) = std::env::var(CHILD) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("w")).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
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
            model_config_id: "m".into(),
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
            PermissionRules::default(),
            synthesis_permissions::Env::new(root.join("w"), root.join("h"), Mode::Default),
        );
        let harness = Harness::new(
            Arc::new(FakeProvider::new([])),
            ToolBox::empty(),
            perms,
            sink,
            HarnessConfig::new("m"),
        );
        let mut o = Options::new(
            Arc::new(harness),
            session,
            link,
            rt.handle().clone(),
            Env::from_pairs([("TERM", "xterm")]),
        );
        o.idle_tick = Duration::from_millis(20);
        let tty = Tty {
            input: Box::new(io::stdin()),
            output: Box::new(io::stdout()),
            size: Box::new(|| (100, 30)),
            raw_mode: false,
            signals: mode == "int",
        };
        let exit = run_interactive(o, tty).unwrap();
        println!("EXIT {:?}", exit.reason);
        io::stdout().flush().unwrap();
    }

    struct Child {
        child: std::process::Child,
        out: Arc<Mutex<Vec<u8>>>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    fn spawn_child(mode: &str) -> Child {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "signals::child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let (o2, mut so) = (out.clone(), child.stdout.take().unwrap());
        let reader = std::thread::spawn(move || {
            let mut b = [0u8; 4096];
            while let Ok(n) = so.read(&mut b) {
                if n == 0 {
                    break;
                }
                o2.lock().unwrap().extend_from_slice(&b[..n]);
            }
        });
        Child {
            child,
            out,
            reader: Some(reader),
        }
    }

    impl Child {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
        }
        fn wait_for(&self, needle: &str) {
            let t0 = Instant::now();
            while !self.text().contains(needle) {
                assert!(
                    t0.elapsed() < Duration::from_secs(20),
                    "child never printed {needle:?}: {:?}",
                    self.text()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        /// The first frame is on screen: the guard (SIGHUP/SIGTERM handlers) and the SIGINT
        /// watcher are installed before it is drawn, so signalling earlier is the test's race.
        fn ready(&self) {
            self.wait_for("MISSION CONTROL"); // the title is set when the screen is entered
            self.wait_for("sandbox: unknown");
        }
        /// Exit status, after the reader has drained the pipe: without the join, the child's last
        /// bytes (the restore sequence) can still sit unread in the pipe when `text()` is called.
        /// `Child::wait` closes the child's stdin first, and that EOF races the signal just sent
        /// (the app quits as `InputClosed`, not killed by it), so stdin stays open until it exits.
        fn wait(&mut self) -> std::process::ExitStatus {
            let stdin = self.child.stdin.take();
            let st = self.child.wait().unwrap();
            drop(stdin);
            self.reader.take().unwrap().join().unwrap();
            st
        }
        fn signal(&self, name: &str) {
            let st = Command::new("kill")
                .arg(format!("-{name}"))
                .arg(self.child.id().to_string())
                .status()
                .unwrap();
            assert!(st.success());
        }
    }

    #[test]
    fn sigterm_restores_the_terminal_and_the_exit_status_stays_killed_by_signal() {
        let mut c = spawn_child("term");
        c.ready();
        c.signal("TERM");
        let st = c.wait();
        assert_eq!(
            st.signal(),
            Some(15),
            "default action ran after the restore: {st:?}"
        );
        let t = c.text();
        assert!(
            t.contains("\x1b[?1049l") && t.contains("\x1b[?25h"),
            "alt screen left, cursor back: {t:?}"
        );
    }

    #[test]
    fn sighup_restores_the_terminal() {
        let mut c = spawn_child("hup");
        c.ready();
        c.signal("HUP");
        let st = c.wait();
        assert_eq!(st.signal(), Some(1));
        assert!(c.text().contains("\x1b[?1049l"));
    }

    #[test]
    fn sigint_is_a_ctrl_c_first_arms_second_quits() {
        let mut c = spawn_child("int");
        c.ready();
        c.signal("INT");
        c.wait_for("press ctrl+c again to quit");
        c.signal("INT");
        let st = c.wait();
        assert!(st.success(), "{st:?}");
        let t = c.text();
        assert!(
            t.contains("EXIT Quit") && t.contains("\x1b[?1049l"),
            "{t:?}"
        );
    }
}

/// Right button press then release at a 1-based cell.
fn right_click(r: &Rig, x: usize, y: usize) {
    r.tty.send(&format!("\x1b[<2;{x};{y}M\x1b[<2;{x};{y}m"));
}

#[test]
fn right_click_in_the_prompt_pastes_the_clipboard_without_submitting() {
    let paste = FakePaste::new(Ok("line one\r\nline two"));
    let mut r = rig(
        vec![Turn::text("got it")],
        Knobs {
            paste: Some(paste.clone()),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    let (x, y) = cell_of(&r, "Enter send");
    right_click(&r, x, y);
    r.wait_for("line two");
    assert_eq!(paste.reads(), 1);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        r.provider.requests().is_empty(),
        "a pasted newline submitted the prompt"
    );
    r.tty.send("\r");
    r.wait_for("got it");
    let req = &r.provider.requests()[0];
    assert!(
        format!("{:?}", req.messages).contains("line one\\nline two"),
        "{:?}",
        req.messages
    );
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn right_click_never_answers_or_feeds_an_approval_dialog_and_outside_the_prompt_does_nothing() {
    let paste = FakePaste::new(Ok("1\r\n2\r\nyes"));
    let mut r = rig(
        vec![Turn::tool("m1", "Mutate", json!({})), Turn::text("done")],
        Knobs {
            paste: Some(paste.clone()),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    right_click(&r, 1, 1); // header, outside the prompt
    r.tty.send("go");
    r.tty.send("\r");
    r.wait_for("Do you want to proceed?");
    // unarmed (just opened), then armed: clicks over the dialog and the rows around it
    for armed in [false, true] {
        if armed {
            r.wait_armed();
        }
        let (x, y) = cell_of(&r, "Do you want to proceed?");
        for dy in [0, 2, 4] {
            right_click(&r, x, y + dy);
        }
        right_click(&r, 3, 3);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        paste.reads(),
        0,
        "the clipboard was read while a dialog was open"
    );
    assert_eq!(r.tool.runs(), 0, "a right click answered the dialog");
    assert!(r.tty.screen_text().contains("Do you want to proceed?"));
    r.tty.send("\x1b"); // esc: no
    r.wait_for("done");
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn hostile_clipboard_is_sanitised_capped_and_failures_are_a_toast() {
    let evil = "a\x1b]52;c;ZXZpbA==\x07b\x1b[2Jc\u{202e}d\x08e";
    let mut r = rig(
        vec![],
        Knobs {
            paste: Some(FakePaste::new(Ok(evil))),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    let (x, y) = cell_of(&r, "Enter send");
    right_click(&r, x, y);
    r.wait_for("a]52;c;ZXZpbA==b[2Jcde"); // escape bytes gone, the rest is plain text
    assert!(
        !r.tty.raw().contains("\x1b]52"),
        "clipboard text reached the terminal as OSC 52"
    );
    r.tty.send("\x04");
    r.tty.send("\x15"); // ctrl+u: clear, then quit on empty ctrl+d
    r.tty.send("\x04");
    r.finish();

    // 5 MiB: capped, the prompt refuses it with a note, the app stays alive
    let mut r = rig(
        vec![],
        Knobs {
            paste: Some(FakePaste::new(Ok(&"x".repeat(5 << 20)))),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    let (x, y) = cell_of(&r, "Enter send");
    right_click(&r, x, y);
    r.wait_for("paste is too large");
    r.tty.send("\x04");
    r.finish();

    // no clipboard tool: a toast, no panic
    let mut r = rig(
        vec![],
        Knobs {
            paste: Some(FakePaste::new(Err("xclip not found"))),
            ..Knobs::default()
        },
    );
    r.wait_for("Enter send");
    let (x, y) = cell_of(&r, "Enter send");
    right_click(&r, x, y);
    r.wait_for("clipboard unavailable: xclip not found");
    r.tty.send("\x04");
    r.finish();
}

#[test]
fn ctrl_o_expands_and_collapses_clamped_code_blocks() {
    let code: String = (1..=12).map(|i| format!("let v{i} = {i};\n")).collect();
    let mut r = rig(
        vec![Turn::text(&format!("here:\n```rs\n{code}```\nbye"))],
        Knobs::default(),
    );
    r.wait_for("Enter send");
    r.tty.send("q");
    r.tty.send("\r");
    r.wait_for("… +6 more lines (ctrl+o to expand)");
    let s = r.tty.screen_text();
    assert!(
        s.contains("let v6 = 6;") && !s.contains("let v7 = 7;"),
        "{s}"
    );
    r.tty.send("\x0f");
    r.wait_for("let v12 = 12;");
    r.wait_for("(ctrl+o to collapse)");
    r.tty.send("\x0f");
    r.wait_for("… +6 more lines (ctrl+o to expand)");
    assert!(!r.tty.screen_text().contains("let v12 = 12;"));
    r.tty.send("\x04");
    r.finish();
}
