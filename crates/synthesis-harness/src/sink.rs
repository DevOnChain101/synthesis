//! Shared, redacting event sink. One `EventWriter` per session keeps `seq` monotonic across the
//! main agent and its sub-agents; the scope (task/turn/agent) is stamped per call.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use synthesis_events::{Envelope, Event, EventWriter, Redactor};

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
}

struct Inner {
    writer: Mutex<EventWriter<Box<dyn Write + Send>>>,
    broken: AtomicBool,
    last_seq: AtomicU64,
}

#[derive(Clone)]
pub struct EventSink(Arc<Inner>);

impl EventSink {
    /// `secrets` are exact values masked anywhere in a payload, on top of the built-in patterns.
    pub fn new(
        out: impl Write + Send + 'static,
        session_id: impl Into<String>,
        secrets: impl IntoIterator<Item = String>,
    ) -> Self {
        let out: Box<dyn Write + Send> = Box::new(out);
        Self(Arc::new(Inner {
            writer: Mutex::new(EventWriter::new(out, session_id, Redactor::new(secrets))),
            broken: AtomicBool::new(false),
            last_seq: AtomicU64::new(0),
        }))
    }

    /// A failed write marks the sink broken (the harness then stops the task: an unauditable
    /// run must not continue) and returns `None`.
    pub fn emit(&self, scope: &Scope, event: Event) -> Option<Envelope> {
        let mut w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.task_id.clone_from(&scope.task_id);
        w.turn_id.clone_from(&scope.turn_id);
        w.agent_id.clone_from(&scope.agent_id);
        match w.emit(event) {
            Ok(env) => {
                self.0.last_seq.store(env.seq, Ordering::SeqCst);
                Some(env)
            }
            Err(_) => {
                self.0.broken.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    /// Masks secrets the way event payloads are masked (for text that leaves the session).
    pub fn redact(&self, s: &str) -> String {
        let w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.redactor().redact_str(s)
    }

    /// Masks every value of a `KEY=value` / `key: value` file read from now on (a secret file the
    /// user approved reading), wherever it appears in events and logs.
    pub fn add_secrets_from(&self, text: &str) {
        let w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        for line in text.lines().take(500) {
            if let Some((_, v)) = line.split_once(['=', ':']) {
                w.redactor().add_secret(v.trim().trim_matches(['"', '\'']));
            }
        }
    }

    pub fn is_broken(&self) -> bool {
        self.0.broken.load(Ordering::SeqCst)
    }

    pub fn last_seq(&self) -> u64 {
        self.0.last_seq.load(Ordering::SeqCst)
    }
}
