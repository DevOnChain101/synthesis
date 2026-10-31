//! Incremental conversation persistence: every message goes to the store (and so to the Node
//! transcript mirror) as soon as it exists, so a crash or SIGKILL loses at most the message
//! being produced.
//!
//! Two feeds, one position counter. `sync` persists what the harness history holds beyond the
//! counter (user prompts, tool results); it runs before every model request and at the end.
//! The assistant message is persisted from its `assistant.message` event the moment the stream
//! ends, ahead of its tools, so a kill during a tool call still leaves the tool_use on disk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::Value;
use synthesis_events::{Envelope, Event};
use synthesis_providers::{Message, Role};
use synthesis_session::NewMessage;

use crate::events::Store;

pub struct Transcript {
    store: Store,
    done: AtomicUsize,
    compacted: AtomicBool,
}

impl Transcript {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            done: AtomicUsize::new(0),
            compacted: AtomicBool::new(false),
        }
    }

    /// Resumed session: the first `n` history messages are already stored.
    pub fn skip(&self, n: usize) {
        self.done.store(n, Ordering::SeqCst);
    }

    fn append(&self, msg: NewMessage) -> bool {
        let r = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_message(msg);
        if let Err(e) = &r {
            eprintln!("synth: warning: session store: {e}");
        }
        r.is_ok()
    }

    /// Persists `history[done..]`. After a compaction the history was rewritten: its summary is
    /// not a new message, so the counter just moves to its end.
    pub fn sync(&self, history: &[Message]) {
        if self.compacted.swap(false, Ordering::SeqCst) {
            self.done.store(history.len(), Ordering::SeqCst);
            return;
        }
        let mut n = self.done.load(Ordering::SeqCst);
        while let Some(m) = history.get(n) {
            let Ok(content) = serde_json::to_value(&m.content) else {
                break;
            };
            let msg = match m.role {
                Role::Assistant => NewMessage::assistant(content),
                _ => NewMessage::user(content),
            };
            if !self.append(msg) {
                break;
            }
            n += 1;
            self.done.store(n, Ordering::SeqCst);
        }
    }

    /// Called for every persisted event line.
    pub fn on_event(&self, env: &Envelope) {
        match &env.event {
            Event::AssistantMessage { content, .. } if env.agent_id.is_none() => {
                if self.append(NewMessage::assistant(Value::Array(content.clone()))) {
                    self.done.fetch_add(1, Ordering::SeqCst);
                }
            }
            Event::ContextCompacted { .. } if env.agent_id.is_none() => {
                self.compacted.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}
