//! `synt engine --stdio` (INTERNAL): the engine of one session driven over stdin/stdout by another
//! process (the desktop app, with the engine inside WSL). The protocol is in [`crate::proto`]; the
//! session work is the same [`Live`] the desktop window uses. stdout carries protocol lines only
//! (one writer, one lock); diagnostics go to stderr, printable and short. stdin EOF ends the session
//! like `close`.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;
use synthesis_permissions::Mode;

use crate::asks::Asks;
use crate::host::{Info, Live, lock};
use crate::proto::{Call, MAX_LINE, Out, Request, read_line};
use crate::run::{Common, Front, Limits, Native, prepare};

/// The single writer of stdout: whole lines under one lock, flushed.
struct Wire(Mutex<Box<dyn Write + Send>>);

impl Wire {
    fn line(&self, bytes: &[u8]) -> bool {
        let mut w = lock(&self.0);
        w.write_all(bytes).is_ok() && w.write_all(b"\n").is_ok() && w.flush().is_ok()
    }

    fn msg(&self, m: &Out) -> bool {
        serde_json::to_vec(m).is_ok_and(|b| self.line(&b))
    }

    fn result(&self, id: u64, r: Result<Value, String>) {
        self.msg(&match r {
            Ok(v) => Out::Result {
                id,
                ok: Some(v),
                error: None,
            },
            Err(e) => Out::Result {
                id,
                ok: None,
                error: Some(sanitize_for_display(&e)),
            },
        });
    }
}

/// The event sink's `Write`: each JSONL line the sink writes becomes one protocol line. A line over `cap`
/// is dropped (the rest of it is skipped) and the client is told, so it never has to treat it as hostile.
struct EventLines {
    wire: Arc<Wire>,
    buf: Vec<u8>,
    cap: usize,
    /// Inside a dropped line: skip up to its end.
    skipping: bool,
}

impl EventLines {
    fn new(wire: Arc<Wire>) -> Self {
        Self {
            wire,
            buf: vec![],
            cap: MAX_LINE,
            skipping: false,
        }
    }

    fn dropped(&self) {
        self.wire.msg(&Out::Notice {
            text: "an event over the line limit was dropped".into(),
        });
    }
}

impl std::io::Write for EventLines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut rest = bytes;
        while let Some(i) = rest.iter().position(|b| *b == b'\n') {
            let (head, tail) = rest.split_at(i);
            rest = &tail[1..];
            if !self.skipping {
                self.buf.extend_from_slice(head);
                if self.buf.len() > self.cap {
                    self.dropped();
                } else if !self.buf.is_empty() {
                    self.wire.line(&self.buf);
                }
            }
            self.skipping = false;
            self.buf.clear();
        }
        if !self.skipping {
            self.buf.extend_from_slice(rest);
            if self.buf.len() > self.cap {
                self.buf.clear();
                self.skipping = true;
                self.dropped();
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn started(info: &Info, live: &Live) -> Value {
    json!({
        "sid": live.sid, "mode": live.mode().as_str(), "label": info.label, "models": info.models, "effort_tiers": info.effort_tiers,
        "context_window": info.context_window, "notices": info.notices, "cwd": info.cwd,
        "root": info.root, "stale_model": info.stale_model, "trusted": false,
    })
}

fn start(
    common: &Common,
    limits: &Limits,
    env: &Env,
    wire: &Arc<Wire>,
    asks: &Asks,
    cwd: &str,
) -> Result<(Live, Value), String> {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("start: cwd must be an existing absolute folder path on this machine".into());
    }
    let notify: crate::asks::Notify = {
        let wire = wire.clone();
        Arc::new(move |a| wire.msg(&Out::from_ask(a)))
    };
    let native = Native {
        out: Box::new(EventLines::new(wire.clone())),
        approver: asks.approver(notify.clone()),
        input: asks.input(notify),
        pre_start: None,
    };
    let p = prepare(common, cwd, env, limits, Front::Native(native))?;
    let (live, info) = Live::begin(p, asks.clone(), vec![], common.cleanup_worktree)?;
    let v = started(&info, &live);
    Ok((live, v))
}

/// Serves one session until `close` or the end of `input`. `Err`: the stream broke (a line over the
/// limit, a read error); the session is ended before returning either way.
pub fn serve(
    common: &Common,
    limits: &Limits,
    env: &Env,
    mut input: impl BufRead,
    output: Box<dyn Write + Send>,
) -> Result<(), String> {
    let wire = Arc::new(Wire(Mutex::new(output)));
    let asks = Asks::default();
    let mut live: Option<Live> = None;
    wire.msg(&Out::hello());
    let mut closing = None;
    let end = loop {
        let line = match read_line(&mut input) {
            Ok(Some(l)) => l,
            Ok(None) => break Ok(()),
            Err(e) => break Err(format!("stdin: {e}")),
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let req: Request = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                // nothing to answer to (no usable id): say so on stderr, keep serving
                eprintln!(
                    "synt engine: ignored a malformed request: {}",
                    sanitize_for_display(&e.to_string())
                );
                continue;
            }
        };
        let id = req.id;
        match req.call {
            Call::Start { .. } if live.is_some() => wire.result(id, Err("already started".into())),
            Call::Start { cwd } => match start(common, limits, env, &wire, &asks, &cwd) {
                Ok((l, v)) => {
                    live = Some(l);
                    wire.result(id, Ok(v));
                }
                Err(e) => wire.result(id, Err(e)),
            },
            Call::Send { text } => {
                let w = wire.clone();
                let r = match &live {
                    None => Err("not started: send start first"),
                    Some(l) => l.start_task(text, vec![], false, move |r| {
                        w.msg(&Out::TaskDone { error: r.err() });
                    }),
                };
                wire.result(id, r.map(|()| json!({})).map_err(String::from));
            }
            Call::Answer { id: ask, choice } => {
                let ok = asks.answer(&ask, choice);
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            Call::AnswerInput { id: ask, reply } => {
                let ok = asks.answer_input(&ask, reply.map(|r| r.into_user_reply()));
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            Call::SetMode { mode } => {
                let r = match (&live, Mode::parse(&mode)) {
                    (None, _) => Err("not started: send start first".to_string()),
                    (_, None | Some(Mode::BypassPermissions)) => {
                        Err("that mode cannot be set over the protocol".to_string())
                    }
                    (Some(l), Some(m)) => l
                        .harness
                        .set_mode(m, false)
                        .map(|_| json!({"mode": l.mode().as_str()}))
                        .map_err(|e| e.to_string()),
                };
                wire.result(id, r);
            }
            Call::Cancel => {
                if let Some(l) = &live {
                    l.cancel();
                }
                wire.result(id, Ok(json!({})));
            }
            Call::Close => {
                closing = Some(id);
                break Ok(());
            }
        }
    };
    // EOF or close: ends the session like the CLI host does (hooks, stored usage, MCP, worktree)
    let finished = live.and_then(Live::close);
    if let Some(id) = closing {
        wire.result(id, Ok(json!({"worktree": finished})));
    }
    end
}

/// The protocol's own handles, and the process's fd 0 and fd 1 pointed away from them: hooks, `$EDITOR`
/// and every other descendant inherits /dev/null as stdin and stderr as stdout, so none can read the
/// client's requests or forge a protocol line. The private handles are close-on-exec.
#[cfg(unix)]
fn protect() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::AsFd;
    let input = std::io::stdin().as_fd().try_clone_to_owned()?;
    let output = std::io::stdout().as_fd().try_clone_to_owned()?;
    rustix::stdio::dup2_stdin(std::fs::File::open("/dev/null")?)?;
    rustix::stdio::dup2_stdout(std::io::stderr().as_fd())?;
    Ok((input.into(), output.into()))
}

/// `synt engine --stdio`: stdin and stdout of this process. Project trust is not available over the
/// protocol yet, so the trust flags are refused: an engine is untrusted for project configuration.
pub fn run_stdio(common: &Common, limits: &Limits, env: &Env) -> Result<u8, String> {
    if common.trust_project || !common.approve_mcp.is_empty() {
        return Err(
            "engine --stdio: --trust-project and --approve-mcp are not supported (the project stays untrusted)"
                .into(),
        );
    }
    // an engine session is unattended from this side: the governed defaults apply (wall cap,
    // cost cap required, worktree, ...); `--no-cost-cap` opts out of the cost cap only
    let mut common = common.clone();
    common.governed = true;
    #[cfg(unix)]
    {
        let (input, output) = protect().map_err(|e| format!("engine: stdio: {e}"))?;
        serve(
            &common,
            limits,
            env,
            std::io::BufReader::new(input),
            Box::new(output),
        )?;
    }
    // shortcut: Windows has no private-handle redirection without unsafe code (forbidden here); a
    // descendant there can still reach the std handles. Upgrade when the engine is run on Windows.
    #[cfg(not(unix))]
    serve(
        &common,
        limits,
        env,
        std::io::stdin().lock(),
        Box::new(std::io::stdout()),
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_oversized_event_is_dropped_with_a_notice_and_the_next_one_passes() {
        let sink = Sink::default();
        let wire = Arc::new(Wire(Mutex::new(Box::new(sink.clone()))));
        let mut w = EventLines::new(wire);
        w.cap = 16;
        w.write_all(b"short\n").unwrap();
        // 40 bytes over three writes: dropped as soon as it is over the cap, rest skipped
        w.write_all(&[b'x'; 20]).unwrap();
        w.write_all(&[b'x'; 20]).unwrap();
        w.write_all(b"\nafter\n").unwrap();
        // a long line that arrives whole
        w.write_all(&[b'y'; 30]).unwrap();
        w.write_all(b"\nlast\n").unwrap();
        let out = String::from_utf8(lock(&sink.0).clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "{out}");
        assert_eq!(lines[0], "short");
        assert!(lines[1].contains(r#""msg":"notice""#));
        assert_eq!(lines[2], "after");
        assert!(lines[3].contains(r#""msg":"notice""#));
        assert_eq!(lines[4], "last");
    }

    /// Body of the re-run child below (ignored: only run when asked for by name).
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn protect_child() {
        use std::io::BufRead;
        let (input, mut output) = protect().unwrap();
        // a descendant inheriting the std handles: it must see /dev/null and stderr
        let st = std::process::Command::new("sh")
            .args(["-c", "echo FORGED; read x && echo GOT:$x"])
            .status()
            .unwrap();
        assert!(st.success() || st.code() == Some(1));
        let mut line = String::new();
        std::io::BufReader::new(input).read_line(&mut line).unwrap();
        writeln!(output, "PRIVATE:{}", line.trim()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn descendants_cannot_forge_or_steal_protocol_lines() {
        use std::process::{Command, Stdio};
        let mut c = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "engine_server::tests::protect_child",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(c.stdin.take().unwrap(), "STOLEN").unwrap();
        let o = c.wait_with_output().unwrap();
        let (out, err) = (
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        );
        assert!(out.contains("PRIVATE:STOLEN"), "{out} / {err}");
        assert!(
            !out.contains("FORGED"),
            "a descendant wrote to the protocol: {out}"
        );
        assert!(
            !out.contains("GOT:"),
            "a descendant read the protocol: {out}"
        );
        assert!(err.contains("FORGED"), "{err}");
    }
}
