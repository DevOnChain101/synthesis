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

/// Most files one Bash command's directory operands add to the snapshot.
const SNAPSHOT_FILES: usize = 200;

/// Existing regular files that `cmd` (rm, mv, cp destination, sed -i) is about to change, as
/// absolute paths: files named directly, and the files below a directory operand up to a cap.
fn bash_snapshot_files(cmd: &str, cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    'targets: for t in synthesis_permissions::bash::mutation_targets(cmd) {
        let p = cwd.join(&t);
        // a symlink operand is not followed: `rm link` removes the link, not what it points at
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_file() {
            out.push(p);
        } else if md.is_dir() {
            let mut stack = vec![p];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() && e.file_name() != ".git" {
                        stack.push(e.path());
                    } else if ft.is_file() {
                        out.push(e.path());
                    }
                    if out.len() >= SNAPSHOT_FILES {
                        break 'targets;
                    }
                }
            }
        }
        if out.len() >= SNAPSHOT_FILES {
            break;
        }
    }
    out
}

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
    /// Bash calls requested but not started yet: their command, for the snapshot at start.
    bash_cmds: std::collections::HashMap<String, String>,
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
            bash_cmds: std::collections::HashMap::new(),
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
            // A mutating Bash command (rm, mv, cp, sed -i) is not tracked by the file tools: when it
            // actually starts (the event is persisted before the command runs, after the permission
            // decision) the files it names are snapshotted. The tree walk happens here, not while
            // the writer lock is held.
            match &env.event {
                Event::ToolRequested {
                    tool_call_id,
                    name,
                    input,
                    ..
                } if name == "Bash" => {
                    if let Some(c) = input.get("command").and_then(|c| c.as_str()) {
                        self.bash_cmds.insert(tool_call_id.clone(), c.to_string());
                    }
                }
                // a call that never starts (denied) or ended: forget it
                Event::ToolCompleted { tool_call_id, .. } => {
                    self.bash_cmds.remove(tool_call_id);
                }
                _ => {}
            }
            let snap = match &env.event {
                Event::ToolStarted {
                    tool_call_id, cwd, ..
                } => self
                    .bash_cmds
                    .remove(tool_call_id)
                    .map(|c| bash_snapshot_files(&c, std::path::Path::new(cwd)))
                    .unwrap_or_default(),
                _ => vec![],
            };
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
                eprintln!("synt: warning: rewind hash: {e}");
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
                eprintln!("synt: warning: checkpoint: {e}");
            }
            for file in &snap {
                let key = file.to_string_lossy().into_owned();
                if self.checkpointed.contains(&key) {
                    continue;
                }
                match w.create_checkpoint(
                    &format!("before-bash-{}", self.checkpointed.len() + 1),
                    std::slice::from_ref(file),
                ) {
                    Ok(_) => {
                        self.checkpointed.insert(key);
                    }
                    Err(synthesis_session::Error::Invalid(_)) => {}
                    Err(e) => eprintln!("synt: warning: checkpoint: {e}"),
                }
            }
            drop(w);
            if seq != env.seq + self.base {
                return Err(bad(format!("store seq {seq} != stream seq {}", env.seq)));
            }
            if let Some((tasks, transcript)) = &self.derived {
                if let Err(e) = tasks.on_event(&env) {
                    eprintln!("synt: warning: task rows: {e}");
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
            // the sink only learns "write failed"; say why, or the run ends as "event sink failed"
            if let Err(e) = self.line(&line[..line.len() - 1]) {
                let _ = writeln!(io::stderr(), "synt: event stream: {e}");
                return Err(e);
            }
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

#[cfg(test)]
mod snapshot_tests {
    use super::bash_snapshot_files;

    #[test]
    fn rm_mv_sed_operands_resolve_through_cd_dotdir_and_directories() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("sub/tree/deep")).unwrap();
        for f in [
            "a.txt",
            "sub/b.txt",
            "sub/tree/c.txt",
            "sub/tree/deep/d.txt",
        ] {
            std::fs::write(r.join(f), "x").unwrap();
        }
        let names = |cmd: &str| {
            let mut v: Vec<String> = bash_snapshot_files(cmd, r)
                .iter()
                .map(|p| {
                    p.strip_prefix(r)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            v.sort();
            v
        };
        // `./a.txt` and `a.txt` are one file; a missing operand is skipped, the rest still counts
        assert_eq!(names("rm ./a.txt a.txt nothing.txt"), ["a.txt"]);
        assert_eq!(names("cd sub && rm b.txt"), ["sub/b.txt"]);
        assert_eq!(names("cd sub && sed -i s/x/y/ ../a.txt"), ["a.txt"]);
        // a directory: its files, not the directory
        assert_eq!(
            names("rm -rf sub/tree"),
            ["sub/tree/c.txt", "sub/tree/deep/d.txt"]
        );
        assert_eq!(names("cp a.txt sub/b.txt"), ["sub/b.txt"]);
        assert!(names("ls -la").is_empty());
    }
}

#[cfg(test)]
mod snapshot_big_tests {
    use super::{SNAPSHOT_FILES, bash_snapshot_files};

    #[test]
    fn a_big_tree_stops_at_the_cap_and_symlink_operands_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("big")).unwrap();
        for i in 0..(SNAPSHOT_FILES * 3) {
            std::fs::write(r.join("big").join(format!("f{i}")), "x").unwrap();
        }
        let t = std::time::Instant::now();
        assert_eq!(bash_snapshot_files("rm -rf big", r).len(), SNAPSHOT_FILES);
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        #[cfg(unix)]
        {
            std::fs::write(r.join("real.txt"), "x").unwrap();
            std::os::unix::fs::symlink(r.join("real.txt"), r.join("link")).unwrap();
            std::os::unix::fs::symlink(r.join("big"), r.join("dlink")).unwrap();
            assert!(bash_snapshot_files("rm link", r).is_empty());
            assert!(bash_snapshot_files("rm -rf dlink", r).is_empty());
        }
    }
}
