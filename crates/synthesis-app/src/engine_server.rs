//! `synt engine --stdio` (INTERNAL): the engine of one session driven over stdin/stdout by another
//! process (the desktop app, with the engine inside WSL). The protocol is in [`crate::proto`]; the
//! session work is the same [`Live`] the desktop window uses. stdout carries protocol lines only
//! (one writer, one lock); diagnostics go to stderr, printable and short. stdin EOF ends the session
//! like `close`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;
use synthesis_permissions::Mode;

use crate::asks::Asks;
use crate::engine_methods::{
    Ctx, Mode as TrustMode, check_id, check_instructions, check_key, check_model,
};
use crate::host::{Info, Live, lock};
use crate::proto::{Call, MAX_LINE, Out, Request, read_line};
use crate::run::{Common, Front, Limits, Native, prepare};
use crate::trust::{Grant, TrustCheck, launch_common, root_of, trust_changed_error};
use synthesis_view::workspaces::fold_dir;

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

fn started(info: &Info, live: &Live, trusted: bool, notes: Vec<String>) -> Value {
    let mut notices = info.notices.clone();
    notices.extend(notes);
    json!({
        "sid": live.sid, "mode": live.mode().as_str(), "label": info.label, "models": info.models, "effort_tiers": info.effort_tiers,
        "context_window": info.context_window, "notices": notices, "cwd": info.cwd,
        "root": info.root, "stale_model": info.stale_model, "trusted": trusted,
    })
}

/// What a (re)start of the session is built from. Everything the client can choose is here, and each
/// field is checked by the engine before it is used.
#[derive(Clone, Default)]
struct Spec {
    resume: Option<String>,
    model: Option<String>,
}

/// One engine process: one folder, one session at a time (restarted in place by the methods below).
struct Srv<'a> {
    common: &'a Common,
    env: &'a Env,
    wire: Arc<Wire>,
    asks: Asks,
    live: Option<Live>,
    cwd: Option<PathBuf>,
    spec: Spec,
    /// The session-only trust of this process (carried across its restarts, gone with it).
    grant: Option<Grant>,
    /// Is the live session running with the folder trusted.
    trusted: bool,
    /// Requests over the last second (the rate bound of every method but `send`/`answer`/`cancel`).
    window: (Instant, u32),
}

/// Requests per second the store-backed methods accept.
const MAX_CALLS_PER_SEC: u32 = 50;

fn start(
    srv: &Srv,
    cwd: &Path,
    spec: &Spec,
    grant: Option<&Grant>,
) -> Result<(Live, Value, bool), String> {
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("start: cwd must be an existing absolute folder path on this machine".into());
    }
    let h = crate::homes(srv.env)?;
    let user = (!h.user.as_os_str().is_empty()).then_some(h.user.as_path());
    let mut c = srv.common.clone();
    if let Some(id) = &spec.resume {
        Ctx {
            home: &h.synthesis,
            user_home: user,
            env: srv.env,
            enforcing: false,
        }
        .check_resumable(id, cwd)?;
        // checked above: the same folder, so the workspace filter of `--resume` may stay off
        c.resume = Some(id.clone());
        c.resume_any_workspace = true;
    }
    if let Some(m) = &spec.model {
        check_model(m)?;
        c.model = Some(m.clone());
    }
    // the same launch rules as the desktop's local engine: a grant for this root, else a remembered trust
    // (MAC-checked, unchanged, enforcing sandbox); the trusted configuration is compared again before anything runs
    let (c, notes, verify) = launch_common(&c, &root_of(cwd), grant, true, cwd, &h.synthesis, user);
    let trusted = c.trust_project;
    let notify: crate::asks::Notify = {
        let wire = srv.wire.clone();
        Arc::new(move |a| wire.msg(&Out::from_ask(a)))
    };
    let native = Native {
        out: Box::new(EventLines::new(srv.wire.clone())),
        approver: srv.asks.approver(notify.clone()),
        input: srv.asks.input(notify),
        pre_start: verify.clone().map(|v| {
            Arc::new(move || v.verify().map_err(|f| trust_changed_error(&f)))
                as crate::run::PreStart
        }),
    };
    let mut p = prepare(&c, cwd, srv.env, &Limits::default(), Front::Native(native))?;
    if let Some(Err(files)) = verify.as_ref().map(TrustCheck::verify) {
        return Err(trust_changed_error(&files));
    }
    let history = std::mem::take(&mut p.history);
    let (live, info) = Live::begin(p, srv.asks.clone(), history, c.cleanup_worktree)?;
    let v = started(&info, &live, trusted, notes);
    Ok((live, v, trusted))
}

impl Srv<'_> {
    fn ctx<'x>(&'x self, h: &'x crate::Homes) -> Ctx<'x> {
        let user = (!h.user.as_os_str().is_empty()).then_some(h.user.as_path());
        Ctx {
            home: &h.synthesis,
            user_home: user,
            env: self.env,
            enforcing: self.common.sandbox_mode() == synthesis_terminal::SandboxMode::Enforce,
        }
    }

    fn session(&self) -> Result<&Live, String> {
        self.live
            .as_ref()
            .ok_or_else(|| "not started: send start first".to_string())
    }

    /// A restart replaces the session: refused while a task runs or a dialog waits.
    fn idle(&self) -> Result<(), String> {
        let l = self.session()?;
        if l.running() || !self.asks.is_idle() {
            return Err("busy: finish or cancel the task and answer the open dialogs first".into());
        }
        Ok(())
    }

    /// Ends the session and starts the next: `spec` and `grant` for it. When it cannot start, the session
    /// that was running continues (resumed under what it had) and the report says why; only if that fails
    /// too is there no session. `between` runs once the old session is closed (its lock is free).
    fn restart(
        &mut self,
        spec: Spec,
        grant: Option<Grant>,
        what: &str,
        between: impl FnOnce(&Ctx) -> Result<(), String>,
    ) -> Result<Value, String> {
        self.idle()?;
        let cwd = self.cwd.clone().ok_or("not started")?;
        let h = crate::homes(self.env)?;
        let old = self.live.take().ok_or("not started")?;
        let sid = old.sid.clone();
        old.close();
        self.trusted = false;
        self.wire.msg(&Out::Restarted);
        let step = between(&self.ctx(&h));
        let first = match step {
            Ok(()) => start(self, &cwd, &spec, grant.as_ref()),
            Err(e) => Err(e),
        };
        let (live, v, trusted) = match first {
            Ok(r) => {
                // only a session-only grant with a manifest is carried: a remembered one is read from the store each time
                (self.spec, self.grant) = (spec, grant.filter(|g| g.2 && g.1.is_some()));
                r
            }
            Err(e) => {
                let back = Spec {
                    resume: Some(sid),
                    model: self.spec.model.clone(),
                };
                let (live, mut v, trusted) = start(self, &cwd, &back, self.grant.as_ref())
                    .map_err(|e2| {
                        format!("{what}: {e}; and the session could not continue: {e2}")
                    })?;
                let note = format!(
                    "{what}: {}; the session continues as it was",
                    sanitize_for_display(&e)
                );
                if let Some(n) = v["notices"].as_array_mut() {
                    n.push(Value::String(note));
                }
                self.spec = back;
                (live, v, trusted)
            }
        };
        self.live = Some(live);
        self.trusted = trusted;
        Ok(v)
    }

    fn call(&mut self, call: Call) -> Result<Value, String> {
        let h = crate::homes(self.env)?;
        let own_ws = || {
            self.cwd
                .as_deref()
                .map(|c| fold_dir(&root_of(c).to_string_lossy()))
        };
        match call {
            Call::WorkspacesList => self.ctx(&h).workspaces_list(self.cwd.as_deref()),
            Call::SessionsList { workspace } => self.ctx(&h).sessions_list(&workspace),
            Call::SessionPreview { id } => self.ctx(&h).preview(&id),
            Call::UsageSummary { tz_min } => self.ctx(&h).usage(tz_min, now_ms()),
            Call::TrustState => {
                let cwd = self.cwd.as_deref().ok_or("not started: send start first")?;
                Ok(self
                    .ctx(&h)
                    .trust_state(cwd, self.grant.as_ref(), self.trusted))
            }
            Call::TrustRevoke { root } => {
                let r = self.ctx(&h).revoke(&root, self.grant.as_ref())?;
                if r["session_grant_ended"] == true {
                    self.grant = None;
                }
                Ok(r)
            }
            Call::TrustSet { mode } => {
                self.idle()?;
                if mode == TrustMode::Deny {
                    return Ok(json!({"trusted": self.trusted, "restarted": false}));
                }
                let cwd = self.cwd.clone().ok_or("not started")?;
                let (grant, undo) = self.ctx(&h).grant(&cwd, mode)?;
                let spec = Spec {
                    resume: Some(self.session()?.sid.clone()),
                    model: self.spec.model.clone(),
                };
                let before = self.grant.clone();
                let r = self.restart(spec, Some(grant), "trust", |_| Ok(()));
                // a restart that did not end up trusted keeps nothing: the rows go back, the grant too
                if !self.trusted || r.is_err() {
                    self.ctx(&h).undo(undo);
                    self.grant = before;
                }
                r
            }
            Call::ModelSet { model } => {
                check_model(&model)?;
                let spec = Spec {
                    resume: Some(self.session()?.sid.clone()),
                    model: Some(model),
                };
                self.restart(spec, self.grant.clone(), "/model", |_| Ok(()))
            }
            Call::SessionNew => self.restart(
                Spec {
                    resume: None,
                    model: self.spec.model.clone(),
                },
                self.grant.clone(),
                "/new",
                |_| Ok(()),
            ),
            Call::SessionResume { id } => {
                // refused before anything is ended: a session that cannot be resumed here changes nothing
                let cwd = self.cwd.clone().ok_or("not started: send start first")?;
                self.ctx(&h).check_resumable(&id, &cwd)?;
                self.restart(
                    Spec {
                        resume: Some(id),
                        model: None,
                    },
                    self.grant.clone(),
                    "/resume",
                    |_| Ok(()),
                )
            }
            Call::SessionClose { id } => {
                check_id(&id)?;
                if self.session()?.sid != id {
                    return Err("only this engine's own session can be closed".into());
                }
                self.restart(
                    Spec {
                        resume: None,
                        model: self.spec.model.clone(),
                    },
                    self.grant.clone(),
                    "/close",
                    |c| c.close_session(&id).map(|_| ()),
                )
            }
            Call::WorkspaceClose { workspace } => {
                check_key(&workspace)?;
                if own_ws().as_deref() == Some(workspace.as_str()) {
                    let ws = workspace.clone();
                    let report = std::cell::RefCell::new(Value::Null);
                    let mut v = self.restart(
                        Spec {
                            resume: None,
                            model: self.spec.model.clone(),
                        },
                        self.grant.clone(),
                        "/close-workspace",
                        |c| {
                            *report.borrow_mut() = c.close_workspace(&ws)?;
                            Ok(())
                        },
                    )?;
                    v["closed_report"] = report.into_inner();
                    Ok(v)
                } else {
                    self.ctx(&h).close_workspace(&workspace)
                }
            }
            Call::SessionRename { id, title } => {
                check_id(&id)?;
                let l = self.session()?;
                if l.sid != id {
                    return Err("only this engine's own session can be renamed".into());
                }
                match &l.host.store {
                    Some(s) => crate::rename::rename(s, &title).map(|t| json!({"title": t})),
                    None => Err("/rename: this session is not stored".into()),
                }
            }
            Call::Clear => {
                lock(&self.session()?.history).clear();
                Ok(json!({}))
            }
            Call::Compact { instructions } => {
                if let Some(i) = &instructions {
                    check_instructions(i)?;
                }
                self.compact(instructions)
            }
            _ => Err("not a store method".into()),
        }
    }

    fn compact(&self, instructions: Option<String>) -> Result<Value, String> {
        let live = self.session()?;
        if live.running() {
            live.harness.request_compact(instructions);
            return Ok(json!({"started": true}));
        }
        let Some(session) = lock(&live.session).clone() else {
            return Err("a task is running".into());
        };
        let history = lock(&live.history).clone();
        let Some(rt) = live.tokio.as_ref().filter(|_| !history.is_empty()) else {
            return Err("nothing to compact yet".into());
        };
        let (h, slot, wire) = (
            live.harness.clone(),
            live.history.clone(),
            self.wire.clone(),
        );
        rt.spawn(async move {
            let r = h
                .compact_now(
                    &session,
                    history,
                    instructions,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map(|m| *lock(&slot) = m);
            wire.msg(&Out::Compacted {
                error: r.err().map(|e| sanitize_for_display(&e.to_string())),
            });
        });
        Ok(json!({"started": false}))
    }

    /// At most [`MAX_CALLS_PER_SEC`] store-backed requests a second; the rest are refused.
    fn allow(&mut self) -> bool {
        if self.window.0.elapsed() >= Duration::from_secs(1) {
            self.window = (Instant::now(), 0);
        }
        self.window.1 += 1;
        self.window.1 <= MAX_CALLS_PER_SEC
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
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
    let mut srv = Srv {
        common,
        env,
        wire: wire.clone(),
        asks: Asks::default(),
        live: None,
        cwd: None,
        spec: Spec::default(),
        grant: None,
        trusted: false,
        window: (Instant::now(), 0),
    };
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
                eprintln!(
                    "synt engine: refused a malformed request: {}",
                    sanitize_for_display(&e.to_string())
                );
                // a request with a usable id is answered (the client does not wait out a timeout)
                if let Some(id) = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(|v| v["id"].as_u64())
                {
                    wire.result(
                        id,
                        Err("malformed request: unknown method or invalid parameters".into()),
                    );
                }
                continue;
            }
        };
        let id = req.id;
        match req.call {
            Call::Start { .. } if srv.live.is_some() => {
                wire.result(id, Err("already started".into()))
            }
            Call::Start { cwd, resume, model } => {
                let cwd = PathBuf::from(cwd);
                let spec = Spec { resume, model };
                match start(&srv, &cwd, &spec, None) {
                    Ok((l, v, trusted)) => {
                        srv.live = Some(l);
                        srv.cwd = Some(cwd);
                        srv.spec = spec;
                        srv.trusted = trusted;
                        wire.result(id, Ok(v));
                    }
                    Err(e) => wire.result(id, Err(e)),
                }
            }
            Call::Send { text } => {
                let w = wire.clone();
                let r = match &srv.live {
                    None => Err("not started: send start first"),
                    Some(l) => l.start_task(text, vec![], false, move |r| {
                        w.msg(&Out::TaskDone { error: r.err() });
                    }),
                };
                wire.result(id, r.map(|()| json!({})).map_err(String::from));
            }
            Call::Answer { id: ask, choice } => {
                let ok = srv.asks.answer(&ask, choice);
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            Call::AnswerInput { id: ask, reply } => {
                let ok = srv
                    .asks
                    .answer_input(&ask, reply.map(|r| r.into_user_reply()));
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            Call::SetMode { mode } => {
                let r = match (&srv.live, Mode::parse(&mode)) {
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
                if let Some(l) = &srv.live {
                    l.cancel();
                }
                wire.result(id, Ok(json!({})));
            }
            Call::Close => {
                closing = Some(id);
                break Ok(());
            }
            other => {
                let r = if srv.allow() {
                    srv.call(other)
                } else {
                    Err("too many requests".into())
                };
                wire.result(id, r);
            }
        }
    };
    // EOF or close: ends the session like the CLI host does (hooks, stored usage, MCP, worktree)
    let finished = srv.live.take().and_then(Live::close);
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
pub fn run_stdio(common: &Common, env: &Env) -> Result<u8, String> {
    if common.trust_project || !common.approve_mcp.is_empty() {
        return Err(
            "engine --stdio: --trust-project and --approve-mcp are not supported (the project stays untrusted)"
                .into(),
        );
    }
    #[cfg(unix)]
    {
        let (input, output) = protect().map_err(|e| format!("engine: stdio: {e}"))?;
        serve(
            common,
            env,
            std::io::BufReader::new(input),
            Box::new(output),
        )?;
    }
    // shortcut: Windows has no private-handle redirection without unsafe code (forbidden here); a
    // descendant there can still reach the std handles. Upgrade when the engine is run on Windows.
    #[cfg(not(unix))]
    serve(
        common,
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
