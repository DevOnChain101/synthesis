//! Shared, redacting event sink. One `EventWriter` per session keeps `seq` monotonic across the
//! main agent and its sub-agents; the scope (task/turn/agent) is stamped per call.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use synthesis_events::{Envelope, Event, EventWriter, Redactor, Rejected};

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
    observer: Mutex<Option<Observer>>,
}

/// Called with every persisted (already redacted) envelope, outside the writer lock. Must not block.
pub type Observer = Arc<dyn Fn(&Envelope) + Send + Sync>;

/// Most values registered from one secret file.
const MAX_FILE_VALUES: usize = 256;

#[derive(Clone)]
pub struct EventSink(Arc<Inner>);

impl EventSink {
    /// `secrets` are exact values masked anywhere in a payload, on top of the built-in patterns.
    pub fn new(
        out: impl Write + Send + 'static,
        session_id: impl Into<String>,
        secrets: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::with_redactor(out, session_id, Redactor::new(secrets))
    }

    /// The session's own redactor (shared with the session store, which learns the same values).
    pub fn with_redactor(
        out: impl Write + Send + 'static,
        session_id: impl Into<String>,
        redactor: Redactor,
    ) -> Self {
        let out: Box<dyn Write + Send> = Box::new(out);
        Self(Arc::new(Inner {
            writer: Mutex::new(EventWriter::new(out, session_id, redactor)),
            broken: AtomicBool::new(false),
            last_seq: AtomicU64::new(0),
            observer: Mutex::new(None),
        }))
    }

    /// Taps the stream of persisted events (the notifier); replaces a previous observer.
    pub fn observe(&self, f: Observer) {
        *self.0.observer.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// A failed write marks the sink broken (the harness then stops the task: an unauditable
    /// run must not continue) and returns `None`.
    pub fn emit(&self, scope: &Scope, event: Event) -> Option<Envelope> {
        let mut w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.task_id.clone_from(&scope.task_id);
        w.turn_id.clone_from(&scope.turn_id);
        w.agent_id.clone_from(&scope.agent_id);
        let written = w.emit(event);
        drop(w);
        match written {
            Ok(env) => {
                self.0.last_seq.store(env.seq, Ordering::SeqCst);
                let obs = self
                    .0
                    .observer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(o) = obs {
                    o(&env);
                }
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

    /// The session's redactor; clones share the values learned from now on (the secret gate's input).
    pub fn redactor(&self) -> Redactor {
        let w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.redactor().clone()
    }

    /// Masks every value of a `KEY=value` / `key: value` file read from now on (a secret file the
    /// user approved reading), wherever it appears in events and logs.
    pub fn add_secrets_from(&self, text: &str) {
        let w = self.0.writer.lock().unwrap_or_else(|e| e.into_inner());
        let r = w.redactor();
        let (mut evicted, mut unprotected, mut tried, mut over) = (false, 0usize, 0usize, 0usize);
        // a value of a secret-named key (DB_PASSWORD, API_TOKEN) is registered from 4 characters
        let mut add = |v: &str, min: usize| {
            // one file registers at most MAX_FILE_VALUES values (a long file cannot slow every request)
            if tried >= MAX_FILE_VALUES {
                over += 1;
                return;
            }
            tried += 1;
            match r.add_secret_min(v, min) {
                Ok(e) => evicted |= e,
                Err(rejected) => {
                    // a short value of an ordinary key (PORT=80) is just not a secret
                    if min < 8 || rejected != Rejected::TooShort {
                        unprotected += 1;
                    }
                }
            }
        };
        // a PEM body: every base64 line is a secret by itself
        let mut in_pem = false;
        for line in text.lines().take(2000) {
            let l = line.trim();
            if l.starts_with("-----BEGIN") {
                in_pem = true;
            } else if l.starts_with("-----END") {
                in_pem = false;
            } else if in_pem {
                add(l, 8);
            } else if let Some((k, v)) = l.split_once(['=', ':']) {
                let named = synthesis_config::is_secret_shaped(k.trim().trim_matches(['"', '\'']));
                add(v.trim(), if named { 4 } else { 8 });
            }
        }
        if evicted {
            eprintln!(
                "synt: warning: the list of secret values to mask is full; the oldest were dropped"
            );
        }
        if unprotected > 0 {
            eprintln!(
                "synt: warning: {unprotected} secret value(s) read from that file are too short (under 4 characters), look like Synthesis's own markers or are words of the request format, so they cannot be masked; treat them as exposed"
            );
        }
        if over > 0 {
            eprintln!(
                "synt: warning: that file holds more than {MAX_FILE_VALUES} values: {over} were not registered for masking; the secret patterns still apply"
            );
        }
    }

    pub fn is_broken(&self) -> bool {
        self.0.broken.load(Ordering::SeqCst)
    }

    pub fn last_seq(&self) -> u64 {
        self.0.last_seq.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_file_registers_short_values_of_secret_named_keys_only() {
        let sink = EventSink::new(Vec::new(), "s", []);
        sink.add_secrets_from("DB_PASSWORD=zq7x\nPORT=8081\nAPI_TOKEN=ab\nNAME=longvalue-1234");
        assert_eq!(
            sink.redact("a zq7x b 8081 c ab d zq7xy longvalue-1234"),
            "a [REDACTED:secret] b 8081 c ab d zq7xy [REDACTED:secret]"
        );
    }
}
