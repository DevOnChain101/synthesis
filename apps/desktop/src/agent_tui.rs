//! Mode 1 agent mode with the real engine: `prepare(Front::Tui)` then `run_interactive` on its own
//! thread against the in-memory terminal of `d0`. Process-wide state of `run_interactive` (see the
//! D0 results in docs/specs/desktop.md): the host starts it at most once per window (it chains one
//! panic hook per run) and passes its own clipboard sink so nothing writes OSC 52 to our stdout.

use clap::Parser as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use synthesis_app::run::{Common, Front, Limits, Prepared, prepare};
use synthesis_config::Env;
use synthesis_tui::{Options, Tty, run_interactive};

use crate::d0::{ChanReader, ChanWriter, Rig};

#[derive(clap::Parser)]
struct Defaults {
    #[command(flatten)]
    c: Common,
}

/// Copy requests of the TUI go to the window, which writes the OS clipboard.
struct ChanClip(UnboundedSender<String>);

impl synthesis_tui_base::clipboard::ClipboardSink for ChanClip {
    fn copy(&self, text: &str) -> io::Result<()> {
        self.0
            .unbounded_send(text.to_string())
            .map_err(|_| io::Error::other("window closed"))
    }
}

pub fn start(
    cols: u32,
    rows: u32,
    project: PathBuf,
    env: Env,
    clip: UnboundedSender<String>,
    provider: Option<String>,
) -> Rig {
    let (keys, key_rx) = channel();
    let (out_tx, bytes) = unbounded();
    let size = Arc::new(Mutex::new((cols, rows)));
    let size2 = size.clone();
    let join = std::thread::spawn(move || {
        let mut common = Defaults::parse_from(["synthesis-desktop"]).c;
        common.provider = provider;
        let p = match prepare(
            &common,
            &project,
            &env,
            &Limits::default(),
            Front::Tui,
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                // shown in the grid: the pane is the only place this person can read it
                let _ = out_tx.unbounded_send(format!("\x1b[31m{e}\x1b[0m\r\n").into_bytes());
                return Err(io::Error::other(e));
            }
        };
        let Prepared {
            harness,
            session,
            sid,
            link,
            tokio,
            rt,
            mcp,
            label,
            models,
            effort_tiers,
            sandbox,
            notices,
            homes,
            notes,
            ..
        } = p;
        let mut o = Options::new(
            harness.clone(),
            session,
            link.unwrap_or_default(),
            tokio.handle().clone(),
            env.clone(),
        );
        o.version = env!("CARGO_PKG_VERSION").into();
        o.model = Some(label);
        o.models = models;
        o.effort_tiers = effort_tiers;
        o.sandbox = sandbox;
        o.startup_notes = notices;
        let (tui, tui_notes) = synthesis_tui::TuiSettings::load_user(&homes.synthesis);
        o.tui = tui;
        o.startup_notes.extend(tui_notes);
        o.synthesis_home = Some(homes.synthesis.clone());
        o.memory = Some(notes.hook());
        o.saved = synthesis_tui::list_saved(&homes.synthesis, None).unwrap_or_default();
        o.min_frame = Duration::from_millis(8);
        o.tick = Duration::from_millis(33);
        o.idle_tick = Duration::from_millis(100);
        o.clipboard = Some(vec![Box::new(ChanClip(clip))]);
        let tty = Tty {
            input: Box::new(ChanReader(key_rx, Vec::new())),
            output: Box::new(ChanWriter(out_tx)),
            size: Box::new(move || *size2.lock().unwrap_or_else(|e| e.into_inner())),
            raw_mode: false,
            signals: false,
        };
        let r = run_interactive(o, tty);
        if let Ok(e) = &r
            && let Some(outcome) = &e.last_outcome
        {
            harness.end_session(outcome);
        }
        rt.close_session(&sid);
        if let Some(m) = &mcp {
            tokio.block_on(m.close());
        }
        r
    });
    Rig {
        keys,
        bytes,
        size,
        join: Some(join),
    }
}
