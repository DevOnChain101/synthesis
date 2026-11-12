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

use crate::asks::Asks;
use crate::host::{Info, Live, lock};
use crate::proto::{Call, Out, Request, read_line};
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

/// The event sink's `Write`: each JSONL line the sink writes becomes one protocol line.
struct EventLines {
    wire: Arc<Wire>,
    buf: Vec<u8>,
}

impl std::io::Write for EventLines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            if line.len() > 1 {
                self.wire.line(&line[..line.len() - 1]);
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn started(info: &Info, sid: &str, trusted: bool) -> Value {
    json!({
        "sid": sid, "label": info.label, "models": info.models, "effort_tiers": info.effort_tiers,
        "context_window": info.context_window, "notices": info.notices, "cwd": info.cwd,
        "root": info.root, "stale_model": info.stale_model, "trusted": trusted,
    })
}

fn start(
    common: &Common,
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
        out: Box::new(EventLines {
            wire: wire.clone(),
            buf: vec![],
        }),
        approver: asks.approver(notify.clone()),
        input: asks.input(notify),
        pre_start: None,
    };
    let p = prepare(common, cwd, env, &Limits::default(), Front::Native(native))?;
    let (live, info) = Live::begin(p, asks.clone(), vec![], common.cleanup_worktree)?;
    let v = started(&info, &live.sid, common.trust_project);
    Ok((live, v))
}

/// Serves one session until `close` or the end of `input`. `Err`: the stream broke (a line over the
/// limit, a read error); the session is ended before returning either way.
pub fn serve(
    common: &Common,
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
            Call::Start { cwd } => match start(common, env, &wire, &asks, &cwd) {
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

/// `synt engine --stdio`: the process's own stdin and stdout.
pub fn run_stdio(common: &Common, env: &Env) -> Result<u8, String> {
    serve(
        common,
        env,
        std::io::stdin().lock(),
        Box::new(std::io::stdout()),
    )?;
    Ok(0)
}
