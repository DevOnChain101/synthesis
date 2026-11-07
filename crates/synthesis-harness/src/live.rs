//! Live output of a running command as `tool.output` events, and the whole-result artifact
//! behind `tool.completed.result_ref`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use synthesis_events::{ArtifactRef, Event};
use synthesis_tools::OutputSink;

use crate::sink::{EventSink, Scope};
use crate::util::lock;

/// Most `tool.output` text one call may put in the event stream; the log has the rest.
pub(crate) const LIVE_CAP: usize = 256 * 1024;
/// An unfinished line is held back (so a secret is not split across two redacted events) until it
/// is this long.
const HOLD: usize = 4096;
/// Bytes of a force-flushed long line kept for the next push, so a token cannot straddle events.
const TAIL: usize = 256;
/// Most `process.output` text, and chunks, one process may put in the stream (a tail for the live
/// pane; `tool.output` and the artifact have more).
const PROC_CAP: usize = 64 * 1024;
const PROC_CHUNKS: u64 = 256;
const KEY_MARK: &str = "[private key redacted]\n";
const CUT_MARK: &str = "\n[live output cut here; the full log is in the artifact]\n";

#[derive(Default)]
struct State {
    held: String,
    seq: u64,
    sent: usize,
    capped: bool,
    proc_sent: usize,
    proc_seq: u64,
    /// Inside a `-----BEGIN ... PRIVATE KEY-----` block: lines are dropped until its END line.
    in_key: bool,
}

#[derive(Clone)]
pub(crate) struct Live {
    sink: EventSink,
    scope: Scope,
    id: String,
    origin: Option<&'static str>,
    /// Also send the text as `process.output` of this process id.
    process: Option<String>,
    st: Arc<Mutex<State>>,
}

impl Live {
    pub fn new(sink: EventSink, scope: Scope, id: &str, origin: Option<&'static str>) -> Self {
        Self {
            sink,
            scope,
            id: id.into(),
            origin,
            process: None,
            st: Arc::default(),
        }
    }

    pub fn with_process(mut self, process_id: &str) -> Self {
        self.process = Some(process_id.into());
        self
    }

    /// For `synthesis_tools::LIVE_OUTPUT`.
    pub fn output_sink(&self) -> OutputSink {
        let me = self.clone();
        OutputSink(Arc::new(move |t| me.push(t)))
    }

    fn push(&self, text: &str) {
        let mut st = lock(&self.st);
        st.held.push_str(text);
        let upto = match st.held.rfind('\n') {
            Some(i) => i + 1,
            None if st.held.len() >= HOLD => {
                let mut i = st.held.len() - TAIL;
                while !st.held.is_char_boundary(i) {
                    i -= 1;
                }
                i
            }
            None => return,
        };
        let chunk: String = st.held.drain(..upto).collect();
        self.send(&mut st, chunk);
    }

    /// The call ended: whatever is held goes out.
    pub fn finish(&self) {
        let mut st = lock(&self.st);
        let chunk = std::mem::take(&mut st.held);
        self.send(&mut st, chunk);
    }

    fn send(&self, st: &mut State, chunk: String) {
        let mut chunk = mask_keys(st, &chunk);
        if st.capped || chunk.is_empty() {
            return;
        }
        if st.sent + chunk.len() > LIVE_CAP {
            let mut keep = LIVE_CAP - st.sent;
            while !chunk.is_char_boundary(keep) {
                keep -= 1;
            }
            chunk.truncate(keep);
            chunk.push_str(CUT_MARK);
            st.capped = true;
        }
        st.sent += chunk.len();
        st.seq += 1;
        if let Some(pid) = &self.process
            && st.proc_sent < PROC_CAP
            && st.proc_seq < PROC_CHUNKS
        {
            let mut keep = chunk.len().min(PROC_CAP - st.proc_sent);
            while !chunk.is_char_boundary(keep) {
                keep -= 1;
            }
            st.proc_sent += keep;
            st.proc_seq += 1;
            self.sink.emit(
                &self.scope,
                Event::ProcessOutput {
                    process_id: pid.clone(),
                    tool_call_id: self.id.clone(),
                    text: chunk[..keep].to_string(),
                    seq: st.proc_seq,
                },
            );
        }
        self.sink.emit(
            &self.scope,
            Event::ToolOutput {
                tool_call_id: self.id.clone(),
                stream: "merged".into(),
                text: chunk,
                seq: st.seq,
                origin: self.origin.map(Into::into),
            },
        );
    }
}

/// Drops private-key blocks (BEGIN line to END line, across chunks) leaving one marker.
fn mask_keys(st: &mut State, chunk: &str) -> String {
    let mut out = String::new();
    for line in chunk.split_inclusive('\n') {
        if !st.in_key {
            let Some(i) = line
                .find("-----BEGIN ")
                .filter(|&i| line[i..].contains("PRIVATE KEY-----"))
            else {
                out.push_str(line);
                continue;
            };
            out.push_str(&line[..i]);
            out.push_str(KEY_MARK);
            st.in_key = true;
        }
        if line.contains("-----END ") && line.contains("PRIVATE KEY-----") {
            st.in_key = false;
        }
    }
    out
}

fn describe(root: &Path, file: &Path) -> Option<ArtifactRef> {
    let rel = file.strip_prefix(root).ok()?;
    let mut f = std::fs::File::open(file).ok()?;
    let (mut h, mut size, mut buf) = (Sha256::new(), 0u64, [0u8; 65536]);
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Some(ArtifactRef {
        artifact_id: file.file_stem()?.to_string_lossy().into_owned(),
        path: rel.to_string_lossy().replace('\\', "/"),
        size,
        sha256: format!("{:x}", h.finalize()),
        mime: "text/plain".into(),
    })
}

/// The whole result as an artifact: the command's own log when it lies under the artifact root,
/// else `text` written to `tool-results/<id>.txt` (0600).
pub(crate) fn result_artifact(
    root: &Path,
    id: &str,
    text: &str,
    log: Option<&Path>,
) -> Option<ArtifactRef> {
    if let Some(l) = log.filter(|l| l.starts_with(root))
        && let Some(a) = describe(root, l)
    {
        return Some(a);
    }
    let dir = root.join("tool-results");
    let mut db = std::fs::DirBuilder::new();
    db.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut db, 0o700);
    db.create(&dir).ok()?;
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let file = dir.join(format!("{safe}-{}.txt", &crate::util::uuid()[..8]));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(&file).ok()?.write_all(text.as_bytes()).ok()?;
    describe(root, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Pushes `parts`, finishes, returns each `tool.output` text.
    fn run(parts: &[&str]) -> Vec<String> {
        let buf = Buf::default();
        let live = Live::new(
            EventSink::new(buf.clone(), "s", []),
            Scope::default(),
            "c",
            None,
        );
        for p in parts {
            live.push(p);
        }
        live.finish();
        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["payload"]["text"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn process_output_is_capped_and_never_carries_a_private_key() {
        let buf = Buf::default();
        let live = Live::new(
            EventSink::new(buf.clone(), "s", []),
            Scope::default(),
            "c",
            None,
        )
        .with_process("p");
        live.push("-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n");
        for _ in 0..4 {
            live.push(&format!("{}\n", "x".repeat(PROC_CAP / 2)));
        }
        live.finish();
        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let texts: Vec<String> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "process.output")
            .map(|v| v["payload"]["text"].as_str().unwrap().to_string())
            .collect();
        assert!(!texts.concat().contains("AAAA"));
        assert_eq!(texts.concat().len(), PROC_CAP);
    }

    #[test]
    fn private_key_is_masked_across_chunks_and_ends_at_end_line() {
        // the body lines are not secret-shaped for the Redactor: only Live's state can hide them
        let t = run(&[
            "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n",
            "CCCC\n-----END OPENSSH PRIVATE KEY-----\nafter\n",
        ])
        .join("");
        assert!(t.contains("before") && t.contains("after"), "{t}");
        assert!(
            !t.contains("AAAA") && !t.contains("BBBB") && !t.contains("CCCC"),
            "{t}"
        );
        // unterminated: masked to the end of the call
        let t = run(&["-----BEGIN PRIVATE KEY-----\n", "ZZZZ\n"]).join("");
        assert!(!t.contains("ZZZZ"), "{t}");
    }

    #[cfg(unix)]
    #[test]
    fn tool_results_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        result_artifact(d.path(), "c1", "x", None).unwrap();
        let m = std::fs::metadata(d.path().join("tool-results")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn long_line_flush_keeps_a_tail_so_a_token_is_not_split() {
        let token = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let first = format!("{}{}", "x".repeat(HOLD - 10), &token[..13]);
        let t = run(&[&first, &token[13..]]);
        assert!(t.len() >= 2, "forced flush expected");
        assert!(t.iter().all(|s| !s.contains("sk-abcdefghij")), "{t:?}");
        assert!(t.join("").contains("[REDACTED:sk"), "{t:?}");
    }
}
