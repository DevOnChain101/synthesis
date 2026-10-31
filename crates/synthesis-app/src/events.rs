//! The event stream writer: every harness event line is persisted (when a store is open) and
//! only then written to stdout (and the `--events-out` copy), flushed per event.

use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use synthesis_core::SessionStatus;
use synthesis_events::{Envelope, Event};
use synthesis_session::SessionWriter;

use crate::taskdb::TaskDb;
use crate::transcript::Transcript;

pub type Store = Arc<Mutex<SessionWriter>>;

pub struct Persist {
    buf: Vec<u8>,
    out: Box<dyn Write + Send>,
    tee: Option<File>,
    store: Option<Store>,
    /// Derived rows and the incremental transcript; failures warn, the event stream is the audit.
    derived: Option<(TaskDb, Arc<Transcript>)>,
    /// Events already in the store when this writer started (resumed session): the stream's
    /// `seq` counts from 1 again, the store's does not.
    base: u64,
    /// Set when the stdout reader went away: the run is cancelled, the store keeps recording.
    closed: Option<(Arc<AtomicBool>, CancellationToken)>,
}

impl Persist {
    pub fn new(
        out: Box<dyn Write + Send>,
        tee: Option<File>,
        store: Option<Store>,
        derived: Option<(TaskDb, Arc<Transcript>)>,
        base: u64,
        closed: Option<(Arc<AtomicBool>, CancellationToken)>,
    ) -> Self {
        Self {
            buf: Vec::new(),
            out,
            tee,
            store,
            derived,
            base,
            closed,
        }
    }

    fn line(&mut self, line: &[u8]) -> io::Result<()> {
        if let Some(store) = &self.store {
            let bad = |m: String| io::Error::other(m);
            let env: Envelope = serde_json::from_slice(line).map_err(|e| bad(e.to_string()))?;
            let mut w = store.lock().unwrap_or_else(|e| e.into_inner());
            w.task_id.clone_from(&env.task_id);
            w.turn_id.clone_from(&env.turn_id);
            w.agent_id.clone_from(&env.agent_id);
            let seq = match &env.event {
                // Keeps `sessions.status` in step; the store emits the same status_changed event.
                Event::SessionStatusChanged { to, reason, .. } => {
                    let to: SessionStatus = serde_json::from_value(to.clone().into())
                        .map_err(|e| bad(e.to_string()))?;
                    w.update_status(to, reason.as_deref())
                        .map_err(|e| bad(e.to_string()))?
                }
                e => {
                    w.append_event(e.clone())
                        .map_err(|e| bad(e.to_string()))?
                        .seq
                }
            };
            drop(w);
            if seq != env.seq + self.base {
                return Err(bad(format!("store seq {seq} != stream seq {}", env.seq)));
            }
            if let Some((tasks, transcript)) = &self.derived {
                if let Err(e) = tasks.on_event(&env) {
                    eprintln!("synth: warning: task rows: {e}");
                }
                transcript.on_event(&env);
            }
        }
        let shown = self
            .out
            .write_all(line)
            .and_then(|()| self.out.write_all(b"\n"))
            .and_then(|()| self.out.flush());
        match (shown, &self.closed) {
            (Err(e), Some((flag, cancel))) if e.kind() == io::ErrorKind::BrokenPipe => {
                if !flag.swap(true, Ordering::SeqCst) {
                    cancel.cancel();
                }
                self.out = Box::new(io::sink()); // later events are still persisted
            }
            (r, _) => r?,
        }
        if let Some(t) = &mut self.tee {
            t.write_all(line)?;
            t.write_all(b"\n")?;
            t.flush()?;
        }
        Ok(())
    }
}

impl Write for Persist {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            self.line(&line[..line.len() - 1])?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
