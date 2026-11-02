//! Incremental conversation persistence: every message goes to the store (and so to the Node
//! transcript mirror) as soon as it exists, so a crash or SIGKILL loses at most the message
//! being produced.
//!
//! Two feeds, one position counter. `sync` persists what the harness history holds beyond the
//! counter (user prompts, tool results); it runs before every model request and at the end.
//! The assistant message is persisted from its `assistant.message` event the moment the stream
//! ends, ahead of its tools, so a kill during a tool call still leaves the tool_use on disk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use synthesis_events::{Envelope, Event};
use synthesis_providers::{Content, Message, Role};
use synthesis_session::{Error, NewMessage};

use crate::events::Store;

/// Where a persistence problem is told to the person (a `notice` event): not stderr, which the
/// TUI owns.
pub type Notifier = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Transcript {
    store: Store,
    done: AtomicUsize,
    compacted: AtomicBool,
    notifier: Mutex<Option<Notifier>>,
    /// Problems found while a persisted event was being handled. That runs inside the event sink,
    /// which must not be re-entered, so they are told at the next `sync`.
    pending: Mutex<Vec<String>>,
}

impl Transcript {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            done: AtomicUsize::new(0),
            compacted: AtomicBool::new(false),
            notifier: Mutex::new(None),
            pending: Mutex::new(vec![]),
        }
    }

    pub fn set_notifier(&self, n: Notifier) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    /// The notifier holds the event sink, which holds the store (and its write lock) and, through
    /// the sink, this transcript: a cycle. A host that restarts sessions in one process breaks it
    /// here when the session ends.
    pub fn clear_notifier(&self) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Resumed session: the first `n` history messages are already stored.
    pub fn skip(&self, n: usize) {
        self.done.store(n, Ordering::SeqCst);
    }

    fn append(&self, msg: NewMessage) -> Result<(), Error> {
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_message(msg)
            .map(|_| ())
    }

    fn tell(&self, text: String) {
        let n = self
            .notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match n {
            Some(n) => n(&text),
            None => eprintln!("synt: warning: {text}"),
        }
    }

    fn flush_pending(&self) {
        let v = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for t in v {
            self.tell(t);
        }
    }

    /// Persists `history[done..]`. After a compaction the history was rewritten: its summary is
    /// not a new message, so the counter just moves to its end. A message over the store's size
    /// limit is stored with its images replaced by `[image]` (it keeps its place and the later
    /// messages are not held up); any other failure stops here and is told once.
    pub fn sync(&self, history: &[Message]) {
        self.flush_pending();
        if self.compacted.swap(false, Ordering::SeqCst) {
            self.done.store(history.len(), Ordering::SeqCst);
            return;
        }
        let mut n = self.done.load(Ordering::SeqCst);
        while let Some(m) = history.get(n) {
            let Ok(content) = serde_json::to_value(&m.content) else {
                break;
            };
            let new = |c: Value| match m.role {
                Role::Assistant => NewMessage::assistant(c),
                _ => NewMessage::user(c),
            };
            match self.append(new(content)) {
                Ok(()) => {}
                Err(Error::TooLarge { size, max, .. }) => {
                    let slim = strip_images(&m.content);
                    let stored = serde_json::to_value(&slim)
                        .ok()
                        .is_some_and(|c| self.append(new(c)).is_ok());
                    if !stored {
                        self.tell(format!(
                            "session store: message {n} is {size} bytes (limit {max}) and could not be stored; later messages are not saved"
                        ));
                        break;
                    }
                    self.tell(format!(
                        "session store: message {n} is {size} bytes (limit {max}); it was stored with its images replaced by [image]"
                    ));
                }
                Err(e) => {
                    self.tell(format!(
                        "session store: {e}; this and later messages are not saved (use --no-session-store to run without it)"
                    ));
                    break;
                }
            }
            n += 1;
            self.done.store(n, Ordering::SeqCst);
        }
    }

    /// Called for every persisted event line.
    pub fn on_event(&self, env: &Envelope) {
        match &env.event {
            Event::AssistantMessage { content, .. } if env.agent_id.is_none() => {
                match self.append(NewMessage::assistant(Value::Array(content.clone()))) {
                    Ok(()) => {
                        self.done.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => self
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!("session store: {e}")),
                }
            }
            Event::ContextCompacted { .. } if env.agent_id.is_none() => {
                self.compacted.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

fn strip_images(c: &Content) -> Content {
    let mut c = c.clone();
    c.strip_images();
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_core::{Budget, PermissionMode, WorkspaceContext};
    use synthesis_providers::{Block, ImageSource};
    use synthesis_session::{NewSession, Store as Sessions, StoreOptions};

    fn rig() -> (tempfile::TempDir, Store, Transcript) {
        let d = tempfile::tempdir().unwrap();
        let ws = WorkspaceContext::open("w", d.path(), d.path(), &[], d.path().join(".art"), "s1")
            .unwrap();
        let w = Sessions::open(&d.path().join("home"), StoreOptions::default())
            .unwrap()
            .create_session(NewSession {
                id: Some("s1".into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::Default,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        let store: Store = Arc::new(Mutex::new(w));
        let t = Transcript::new(store.clone());
        (d, store, t)
    }

    fn user(blocks: Vec<Block>) -> Message {
        Message {
            role: Role::User,
            content: Content::Blocks(blocks),
        }
    }

    fn image(len: usize) -> Block {
        Block::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "A".repeat(len),
            },
        }
    }

    /// An over-limit message is stored without its images (same place), the later ones follow,
    /// and the person is told through the notifier, not stderr.
    #[test]
    fn an_oversized_message_is_stored_without_images_and_does_not_stall_the_rest() {
        let (_d, store, t) = rig();
        let told = Arc::new(Mutex::new(Vec::<String>::new()));
        let t2 = told.clone();
        t.set_notifier(Arc::new(move |m| t2.lock().unwrap().push(m.to_string())));
        let text = |s: &str| Block::Text { text: s.into() };
        let history = vec![
            user(vec![text("first")]),
            user(vec![text("with picture"), image(9 * 1024 * 1024)]),
            user(vec![text("third")]),
        ];
        t.sync(&history);
        let rows = store.lock().unwrap().read_messages("s1").unwrap();
        assert_eq!(rows.len(), 3, "the later message was not held up");
        let second = rows[1].content.to_string();
        assert!(
            second.contains("[image]") && second.contains("with picture"),
            "{second}"
        );
        assert!(rows[2].content.to_string().contains("third"));
        let told = told.lock().unwrap();
        assert!(
            told.len() == 1 && told[0].contains("replaced by [image]"),
            "{told:?}"
        );
        // a second sync stores nothing twice
        t.sync(&history);
        assert_eq!(store.lock().unwrap().read_messages("s1").unwrap().len(), 3);
    }

    /// A screenshot a TOOL returned is image bytes too: over the limit it must not stall the
    /// persistence of everything after it.
    #[test]
    fn an_oversized_tool_result_image_is_stored_without_it_and_later_messages_follow() {
        let (_d, store, t) = rig();
        t.set_notifier(Arc::new(|_| {}));
        let shot = Block::ToolResult {
            tool_use_id: "t1".into(),
            content: Some(serde_json::json!([
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                              "data": "A".repeat(9 * 1024 * 1024)}}
            ])),
            is_error: None,
        };
        let history = vec![
            user(vec![shot]),
            user(vec![Block::Text {
                text: "after".into(),
            }]),
        ];
        t.sync(&history);
        let rows = store.lock().unwrap().read_messages("s1").unwrap();
        assert_eq!(rows.len(), 2, "persistence did not stall at the screenshot");
        let first = rows[0].content.to_string();
        assert!(
            first.contains("screenshot taken") && first.contains("[image]"),
            "{first}"
        );
        assert!(!first.contains("AAAAAAAA"));
        assert!(rows[1].content.to_string().contains("after"));
    }
}
