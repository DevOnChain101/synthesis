//! D0 spike (docs/specs/desktop.md section 7): `synthesis_tui::run_interactive` running in this
//! process against an in-memory terminal (two channels, no PTY, no stdio), with a scripted model.
//! The real engine wiring (`synthesis_app::run::prepare`) replaces the fake harness in D2.

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use synthesis_config::Env;
use synthesis_core::{AgentSession, Budget, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_harness::{EventSink, Harness, HarnessConfig, ToolBox};
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_tools::Registry;
use synthesis_tui::{Exit, Options, Tty, UiLink, run_interactive};

pub(crate) struct ChanReader(pub(crate) Receiver<Vec<u8>>, pub(crate) Vec<u8>);

impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(v) => self.1 = v,
                Err(_) => return Ok(0), // the window closed: end of input
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        self.1.drain(..n);
        Ok(n)
    }
}

pub(crate) struct ChanWriter(pub(crate) UnboundedSender<Vec<u8>>);

impl Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let _ = self.0.unbounded_send(b.to_vec());
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The window side of the in-memory terminal.
pub struct Rig {
    /// Key and paste bytes for the TUI.
    pub keys: Sender<Vec<u8>>,
    /// What the TUI draws.
    pub bytes: UnboundedReceiver<Vec<u8>>,
    /// Cells (columns, rows); the TUI polls it for resizes.
    pub size: Arc<Mutex<(u32, u32)>>,
    pub(crate) join: Option<JoinHandle<io::Result<Exit>>>,
}

impl Rig {
    /// Ends the input and waits for the TUI thread.
    pub fn finish(mut self) -> io::Result<Exit> {
        let (dead, _) = channel();
        self.keys = dead; // drops the live sender: the TUI sees end of input
        self.join
            .take()
            .map(|j| {
                j.join()
                    .unwrap_or_else(|_| Err(io::Error::other("tui thread panicked")))
            })
            .unwrap_or_else(|| Err(io::Error::other("already finished")))
    }
}

/// Starts the TUI on its own thread; the scripted model answers every prompt with `reply`.
pub fn start(cols: u32, rows: u32, reply: &str) -> Rig {
    let (keys, key_rx) = channel();
    let (out_tx, bytes) = unbounded();
    let size = Arc::new(Mutex::new((cols, rows)));
    let size2 = size.clone();
    let reply = reply.to_string();
    let join = std::thread::spawn(move || {
        let dir = std::env::temp_dir().join(format!("synthesis-d0-{}", std::process::id()));
        let root = dir.join("w");
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(root)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let ws = WorkspaceContext::open("w", &root, &root, &[], dir.join("art"), "pg")
            .map_err(io::Error::other)?;
        let session = AgentSession {
            id: "d0".into(),
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
        let sink = EventSink::new(link.event_writer(), "d0", []);
        let perms = Permissions::new(
            PermissionRules::default(),
            synthesis_permissions::Env::new(root.clone(), dir.join("home"), Mode::Default)
                .interactive(true),
        );
        let turns: Vec<Turn> = (0..8).map(|_| Turn::text(&reply)).collect();
        let harness = Arc::new(
            Harness::new(
                Arc::new(FakeProvider::new(turns)),
                ToolBox::new(Registry::unsandboxed_for_tests()),
                perms,
                sink,
                HarnessConfig::new("fake-model"),
            )
            .with_approver(link.approver())
            .with_input(link.input()),
        );
        let mut o = Options::new(
            harness,
            session,
            link,
            rt.handle().clone(),
            Env::from_pairs([("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
        );
        o.min_frame = Duration::from_millis(8);
        o.tick = Duration::from_millis(33);
        o.idle_tick = Duration::from_millis(100); // also the resize polling period
        // No OSC 52 on the process stdout (the default clipboard chain would write there).
        o.clipboard = Some(vec![]);
        o.paste_source = Some(vec![]);
        let tty = Tty {
            input: Box::new(ChanReader(key_rx, Vec::new())),
            output: Box::new(ChanWriter(out_tx)),
            size: Box::new(move || *size2.lock().unwrap_or_else(|e| e.into_inner())),
            raw_mode: false,
            signals: false,
        };
        let r = run_interactive(o, tty);
        let _ = std::fs::remove_dir_all(&dir);
        r
    });
    Rig {
        keys,
        bytes,
        size,
        join: Some(join),
    }
}
