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
    /// Files that already have a "before first write" checkpoint in this run.
    checkpointed: std::collections::HashSet<String>,
    /// File-changing tool calls in flight: their path, hashed when the call completes.
    writing: std::collections::HashMap<String, String>,
    /// Raised when a call that taints the task is requested (web, browser, MCP, network Bash):
    /// the Memory tools read it, so a note saved afterwards is `untrusted` at once.
    taint: Option<Arc<AtomicBool>>,
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
            checkpointed: std::collections::HashSet::new(),
            writing: std::collections::HashMap::new(),
            taint: None,
            closed,
        }
    }

    pub fn with_taint(mut self, flag: Arc<AtomicBool>) -> Self {
        self.taint = Some(flag);
        self
    }

    fn line(&mut self, line: &[u8]) -> io::Result<()> {
        if let Some(flag) = self.taint.as_ref().filter(|f| !f.load(Ordering::SeqCst))
            && let Ok(env) = serde_json::from_slice::<Envelope>(line)
            && let Event::ToolRequested { name, input, .. } = &env.event
            && synthesis_memory::taints_task(
                name,
                input.get("command").and_then(|c| c.as_str()).unwrap_or(""),
            )
        {
            flag.store(true, Ordering::SeqCst);
        }
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
            if let Event::ToolCompleted { tool_call_id, .. } = &env.event
                && let Some(p) = self.writing.remove(tool_call_id)
                && let Err(e) = w.record_post_run_hash(std::path::Path::new(&p))
            {
                eprintln!("synth: warning: rewind hash: {e}");
            }
            // restore point before the first write to a file (docs: checkpoints); best effort
            if let Event::ToolRequested {
                tool_call_id,
                name,
                input,
                ..
            } = &env.event
                && matches!(
                    name.as_str(),
                    "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
                )
                && let Some(p) = ["file_path", "notebook_path"]
                    .iter()
                    .find_map(|k| input.get(k).and_then(|v| v.as_str()))
                && self
                    .writing
                    .insert(tool_call_id.clone(), p.to_string())
                    .is_none()
                && self.checkpointed.insert(p.to_string())
                && let Err(e) = w.create_checkpoint(
                    &format!("before-first-write-{}", self.checkpointed.len()),
                    &[std::path::PathBuf::from(p)],
                )
            {
                eprintln!("synth: warning: checkpoint: {e}");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(name: &str, input: serde_json::Value) -> Vec<u8> {
        let env = Envelope {
            v: 1,
            seq: 1,
            ts: "2026-01-01T00:00:00Z".into(),
            session_id: "s".into(),
            task_id: None,
            turn_id: None,
            agent_id: None,
            event: Event::ToolRequested {
                tool_call_id: "c".into(),
                name: name.into(),
                input,
                input_truncated: false,
                origin: None,
            },
        };
        let mut l = serde_json::to_vec(&env).unwrap();
        l.push(b'\n');
        l
    }

    #[test]
    fn web_browser_mcp_and_network_bash_requests_raise_the_taint_flag() {
        for (name, input, tainted) in [
            ("Read", serde_json::json!({"file_path": "a"}), false),
            ("Bash", serde_json::json!({"command": "ls"}), false),
            ("WebFetch", serde_json::json!({"url": "http://x"}), true),
            ("mcp__srv__tool", serde_json::json!({}), true),
            (
                "Bash",
                serde_json::json!({"command": "curl http://x"}),
                true,
            ),
        ] {
            let flag = Arc::new(AtomicBool::new(false));
            let mut p = Persist::new(Box::new(io::sink()), None, None, None, 0, None)
                .with_taint(flag.clone());
            p.write_all(&requested(name, input)).unwrap();
            assert_eq!(flag.load(Ordering::SeqCst), tainted, "{name}");
        }
    }
}
