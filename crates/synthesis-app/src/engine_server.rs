//! `synt engine --stdio` (INTERNAL): the engine of one session driven over stdin/stdout by another
//! process (the desktop app, with the engine inside WSL). The protocol is in [`crate::proto`]; the
//! session work is the same [`Live`] the desktop window uses. stdout carries protocol lines only
//! (one writer, one lock); diagnostics go to stderr, printable and short. stdin EOF ends the session
//! like `close`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;
use synthesis_permissions::Mode;

use crate::asks::Asks;
use crate::engine_fs::{self, Images, Put};
use crate::engine_methods::{
    Ctx, Mode as TrustMode, check_id, check_instructions, check_key, check_model,
};
use crate::engine_ops::{self, Tokens};
use crate::host::{CancelSlot, Info, Live, lock};
use crate::proto::{Call, FATAL, MAX_LINE, Out, REFUSED, Request, read_line};
use crate::run::{Common, Front, Limits, Native, prepare};
use crate::trust::{Grant, TRUST_CHANGED, TrustCheck, launch_common, root_of, trust_changed_error};
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

fn started(info: &Info, live: &Live, trusted: bool, notes: Vec<String>, trust: Value) -> Value {
    let mut notices = info.notices.clone();
    notices.extend(notes);
    json!({
        "sid": live.sid, "mode": live.mode().as_str(), "label": info.label, "models": info.models, "effort_tiers": info.effort_tiers,
        "context_window": info.context_window, "notices": notices, "cwd": info.cwd,
        "root": info.root, "stale_model": info.stale_model, "trusted": trusted,
        // what the Trust dialog shows: the client needs no call to open it
        "trust": trust,
    })
}

/// What a (re)start of the session is built from. Everything the client can choose is here, and each
/// field is checked by the engine before it is used.
#[derive(Clone, Default)]
struct Spec {
    resume: Option<String>,
    model: Option<String>,
}

/// What the reader thread needs besides the wire: it answers `answer`, `answer_input` and `cancel` itself,
/// so a slow store call on the worker never delays them.
struct Oob {
    asks: Asks,
    cancel: Mutex<Option<CancelSlot>>,
    /// Sends queued so far, and how many of them a `cancel` already covers: a cancel that arrives while its send
    /// still waits in the queue is applied to that send when the worker starts it (a cancel is never lost).
    sends_queued: AtomicU64,
    cancel_upto: AtomicU64,
}

/// One engine process: one folder, one session at a time (restarted in place by the methods below).
struct Srv<'a> {
    common: &'a Common,
    limits: &'a Limits,
    env: &'a Env,
    wire: Arc<Wire>,
    asks: Asks,
    oob: Arc<Oob>,
    live: Option<Live>,
    cwd: Option<PathBuf>,
    /// A session was started once: `start` is one-shot, whatever happens to it later.
    began: bool,
    spec: Spec,
    /// The session-only trust of this process (carried across its restarts, gone with it).
    grant: Option<Grant>,
    /// Is the live session running with the folder trusted.
    trusted: bool,
    /// A `compact` between tasks is running.
    compacting: Arc<AtomicBool>,
    /// Requests over the last second (the rate bound of the store-backed methods).
    window: (Instant, u32),
    last_restart: Option<Instant>,
    /// `provider/model` and the effort tiers of the live session, as its start report said.
    label: String,
    tiers: Vec<String>,
    /// Images waiting for a `send`, and the rewind and clean plans issued (both end with the session).
    images: Images,
    tokens: Tokens,
    /// The last scan, walk or health check: these are limited to a few a second.
    heavy_at: Option<Instant>,
    /// A heavy call is under way: the gap is counted from its END (a slow scan must not leave the next one free).
    heavy_running: bool,
    /// The scan the pages of `fs.list` are cut from.
    tree: Option<synthesis_view::project::ProjectData>,
}

/// The shortest gap between two methods that walk the folder, run git or check the installation.
const HEAVY_GAP: Duration = Duration::from_millis(250);

/// Requests per second the store-backed methods accept.
const MAX_CALLS_PER_SEC: u32 = 50;
/// A restart ends a session (hooks, MCP, usage): at most one per this long.
const RESTART_GAP: Duration = Duration::from_secs(1);
/// Requests waiting for the worker; past it the client is told `too many requests`.
const QUEUE: usize = 64;

fn refused(e: impl std::fmt::Display) -> String {
    format!("{REFUSED}{e}")
}

/// What a call ends with. The kind is decided where the error is made, never read back from its text: only
/// code after the old session was closed can make a `Fatal`, and everything else (every `?`) is a `Refused`,
/// which means nothing was ended.
#[derive(Debug)]
enum CallError {
    /// Nothing was ended; the session goes on.
    Refused(String),
    /// The session was ended and none could be started: the engine exits.
    Fatal(String),
}

impl From<String> for CallError {
    fn from(e: String) -> Self {
        Self::Refused(e)
    }
}

impl From<&str> for CallError {
    fn from(e: &str) -> Self {
        Self::Refused(e.into())
    }
}

impl CallError {
    /// The wire text. The prefixes mark a restart request's error (the client reads them); the other methods'
    /// errors are plain text.
    fn wire(self, restart: bool) -> String {
        match self {
            Self::Refused(e) if restart => refused(e),
            Self::Refused(e) => e,
            Self::Fatal(e) => format!("{FATAL}{e}"),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn start(
    srv: &Srv,
    cwd: &Path,
    spec: &Spec,
    grant: Option<&Grant>,
    remembered: bool,
) -> Result<(Live, Value, bool), String> {
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("start: cwd must be an existing absolute folder path on this machine".into());
    }
    let h = crate::homes(srv.env)?;
    let mut c = srv.common.clone();
    if let Some(id) = &spec.resume {
        srv.ctx(&h).check_resumable(id, cwd)?;
        // checked above: the same folder, so the workspace filter of `--resume` may stay off
        c.resume = Some(id.clone());
        c.resume_any_workspace = true;
    }
    if let Some(m) = &spec.model {
        check_model(m)?;
        c.model = Some(m.clone());
    }
    let user = (!h.user.as_os_str().is_empty()).then_some(h.user.as_path());
    // the same launch rules as the desktop's local engine: a grant for this root, else (when `remembered`) a
    // remembered trust (MAC-checked, unchanged, enforcing sandbox); the trusted configuration is compared again
    // before anything of the project runs
    let (c, notes, verify) = launch_common(
        &c,
        &root_of(cwd),
        grant,
        remembered,
        cwd,
        &h.synthesis,
        user,
    );
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
        on_stop: None,
    };
    let mut p = prepare(&c, cwd, srv.env, srv.limits, Front::Native(native))?;
    if let Some(Err(files)) = verify.as_ref().map(TrustCheck::verify) {
        return Err(trust_changed_error(&files));
    }
    let history = std::mem::take(&mut p.history);
    let (live, info) = Live::begin(p, srv.asks.clone(), history, c.cleanup_worktree)?;
    let trust = srv.ctx(&h).trust_state(cwd, grant, trusted);
    let v = started(&info, &live, trusted, notes, trust);
    Ok((live, v, trusted))
}

/// The folders of this machine an error text must not carry.
fn hide_paths<'x>(h: &'x crate::Homes, cwd: Option<&'x Path>) -> Vec<&'x Path> {
    let mut v = vec![h.synthesis.as_path(), h.user.as_path()];
    v.extend(cwd);
    v
}

/// [`Srv::ctx`] without borrowing the server (the methods that also change its state).
fn ctx_of<'x>(common: &Common, env: &'x Env, h: &'x crate::Homes) -> Ctx<'x> {
    let user = (!h.user.as_os_str().is_empty()).then_some(h.user.as_path());
    Ctx {
        home: &h.synthesis,
        user_home: user,
        env,
        enforcing: common.sandbox_mode() == synthesis_terminal::SandboxMode::Enforce,
    }
}

impl Srv<'_> {
    fn ctx<'x>(&'x self, h: &'x crate::Homes) -> Ctx<'x> {
        ctx_of(self.common, self.env, h)
    }

    /// The folder the files methods look at: the session's own working folder, never a client path.
    fn files_root(&self) -> Result<PathBuf, String> {
        Ok(self.session()?.host.workspace.cwd.clone())
    }

    /// Methods that walk the folder, run git or check the installation: a few a second at most.
    fn heavy(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if self
            .heavy_at
            .is_some_and(|t| now.duration_since(t) < HEAVY_GAP)
        {
            return Err("too many requests".into());
        }
        self.heavy_at = Some(now);
        self.heavy_running = true;
        Ok(())
    }

    fn session(&self) -> Result<&Live, String> {
        self.live
            .as_ref()
            .ok_or_else(|| "not started: send start first".to_string())
    }

    /// Makes `live` the session and lets the reader thread cancel its tasks.
    fn adopt(&mut self, live: Live, trusted: bool, report: &Value) {
        self.label = report["label"].as_str().unwrap_or_default().to_string();
        self.tiers = serde_json::from_value(report["effort_tiers"].clone()).unwrap_or_default();
        // what was issued or held for the session that ended is not valid for the next one
        self.images.clear();
        self.tokens.clear();
        self.tree = None;
        *lock(&self.oob.cancel) = Some(live.cancel_slot());
        self.live = Some(live);
        self.trusted = trusted;
    }

    /// A restart replaces the session: refused while a task runs, a dialog waits or a compaction runs.
    fn idle(&self) -> Result<(), String> {
        let l = self.session()?;
        if l.running() || !self.asks.is_idle() || self.compacting.load(Ordering::SeqCst) {
            return Err("busy: finish or cancel the task and answer the open dialogs first".into());
        }
        Ok(())
    }

    /// What a restart needs before it may begin, and what `trust.set` checks BEFORE it writes any row: no task,
    /// no dialog, no compaction, and not within a second of the last restart.
    fn preflight(&self) -> Result<(), String> {
        self.idle()?;
        if self.last_restart.is_some_and(|t| t.elapsed() < RESTART_GAP) {
            return Err("too many restarts: wait a second".into());
        }
        Ok(())
    }

    /// Ends the session and starts the next: `spec` and `grant` for it. When it cannot start, the session
    /// that was running continues (resumed) and the report says why: as it was, or UNTRUSTED when the failure
    /// was a trust change or the restart was a trust decision (nothing of an earlier grant is re-applied).
    /// Only if that fails too is the error `Fatal`: there is no session and the engine exits. Every other
    /// `Err` is a `Refused`: nothing was ended. `after` runs once the new session is running (store housekeeping of
    /// `session.close` and `workspace.close`); its failure is a note.
    fn restart(
        &mut self,
        spec: Spec,
        grant: Option<Grant>,
        trust_change: bool,
        what: &str,
        after: impl FnOnce(&Ctx, &str, &str) -> Result<Value, String>,
    ) -> Result<Value, CallError> {
        self.preflight()?;
        let cwd = self.cwd.clone().ok_or("not started")?;
        let h = crate::homes(self.env).map_err(|_| "the home folder is not known".to_string())?;
        let old = self.live.take().ok_or("not started")?;
        // from here on nothing is "refused": the old session is ending
        let r = self
            .replace(old, &cwd, &h, spec, grant, trust_change, what, after)
            .map_err(CallError::Fatal);
        // the gap runs from the END of a restart (a slow one must not leave the next one free to follow at once)
        self.last_restart = Some(Instant::now());
        r
    }

    /// The part of a restart after the old session was taken: every `Err` of it is fatal.
    #[allow(clippy::too_many_arguments)]
    fn replace(
        &mut self,
        old: Live,
        cwd: &Path,
        h: &crate::Homes,
        spec: Spec,
        grant: Option<Grant>,
        trust_change: bool,
        what: &str,
        after: impl FnOnce(&Ctx, &str, &str) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let (sid, old_mode) = (old.sid.clone(), old.mode());
        old.close();
        self.trusted = false;
        self.wire.msg(&Out::Restarted);
        let mut notes: Vec<String> = vec![];
        let first = start(self, cwd, &spec, grant.as_ref(), true);
        let mut extra = Value::Null;
        let (live, mut v, trusted) = match first {
            Ok(r) => {
                // only a session-only grant with a manifest is carried: a remembered one is read from the store each time
                (self.spec, self.grant) = (spec, grant.filter(|g| g.2 && g.1.is_some()));
                if let (Some(n), Some(o)) = (r.1["sid"].as_str(), Some(sid.as_str())) {
                    match after(&self.ctx(h), o, n) {
                        Ok(x) => extra = x,
                        Err(e) => notes.push(format!("{what}: {}", sanitize_for_display(&e))),
                    }
                }
                r
            }
            Err(e) => {
                let back = Spec {
                    resume: Some(sid),
                    model: self.spec.model.clone(),
                };
                let mut tries: Vec<(Option<Grant>, bool)> = if trust_change {
                    vec![(None, false)]
                } else {
                    vec![(self.grant.clone(), true), (None, false)]
                };
                let mut why = e;
                let mut done = None;
                for (i, (g, remembered)) in tries.drain(..).enumerate() {
                    // the second try (untrusted) only after a trust change
                    if i > 0 && !why.starts_with(TRUST_CHANGED) {
                        break;
                    }
                    match start(self, cwd, &back, g.as_ref(), remembered) {
                        Ok(r) => {
                            let untrusted = g.is_none() && !remembered;
                            notes.push(format!(
                                "{what}: {}; {}",
                                sanitize_for_display(&why),
                                if untrusted {
                                    "the session continues UNTRUSTED (no trust is applied)"
                                } else {
                                    "the session continues as it was"
                                }
                            ));
                            self.grant = g;
                            done = Some(r);
                            break;
                        }
                        Err(e2) => why = e2,
                    }
                }
                let Some(r) = done else {
                    self.grant = None;
                    return Err(format!(
                        "{what}: {}; and the session could not continue: the engine ends",
                        sanitize_for_display(&why)
                    ));
                };
                self.spec = back;
                r
            }
        };
        let new_mode = live.mode();
        if old_mode != new_mode {
            notes.push(format!(
                "the permission mode is {} again (it was {})",
                new_mode.as_str(),
                old_mode.as_str()
            ));
        }
        if let Some(n) = v["notices"].as_array_mut() {
            n.extend(notes.into_iter().map(Value::String));
        }
        v["mode"] = Value::String(new_mode.as_str().into());
        if !extra.is_null() {
            v["closed_report"] = extra;
        }
        self.adopt(live, trusted, &v);
        Ok(v)
    }

    fn call(&mut self, call: Call) -> Result<Value, CallError> {
        let r = self.call_inner(call);
        if std::mem::take(&mut self.heavy_running) {
            self.heavy_at = Some(Instant::now());
        }
        r
    }

    fn call_inner(&mut self, call: Call) -> Result<Value, CallError> {
        let h = crate::homes(self.env)?;
        let ws_key = |s: &Self| {
            s.live
                .as_ref()
                .map(|l| fold_dir(&l.host.workspace.root.to_string_lossy()))
        };
        match call {
            Call::WorkspacesList => Ok(self.ctx(&h).workspaces_list(self.cwd.as_deref())?),
            Call::SessionsList { workspace } => Ok(self.ctx(&h).sessions_list(&workspace)?),
            Call::SessionPreview { id } => Ok(self.ctx(&h).preview(&id)?),
            Call::UsageSummary { tz_min } => Ok(self.ctx(&h).usage(tz_min, now_ms())?),
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
                // before ANY row is written: a restart that would be refused must leave the store alone
                self.preflight()?;
                if mode == TrustMode::Deny {
                    return Ok(json!({"trusted": self.trusted, "restarted": false}));
                }
                let cwd = self.cwd.clone().ok_or("not started")?;
                let (grant, undo) = self.ctx(&h).grant(&cwd, mode)?;
                let spec = Spec {
                    resume: Some(self.session()?.sid.clone()),
                    model: self.spec.model.clone(),
                };
                let mut r =
                    self.restart(spec, Some(grant), true, "trust", |_, _, _| Ok(Value::Null));
                // a restart that did not end up trusted keeps nothing: the rows go back, and a row that cannot
                // be put back is said (never silently left to trust the next start)
                if !self.trusted || r.is_err() {
                    let failed = self.ctx(&h).undo(undo);
                    if let Ok(v) = &mut r
                        && let Some(n) = v["notices"].as_array_mut()
                    {
                        n.push(Value::String("trust: nothing was remembered".into()));
                        n.extend(failed.into_iter().map(|f| {
                            Value::String(format!(
                                "trust: could not undo the remembered trust of {f}; run /trust revoke for it"
                            ))
                        }));
                    } else if !failed.is_empty() {
                        r = Err(CallError::Fatal(format!(
                            "trust: the restart failed and the remembered trust of {} could not be undone; revoke it before the next start",
                            failed.join(", ")
                        )));
                    }
                }
                r
            }
            Call::ModelSet { model } => {
                check_model(&model)?;
                let spec = Spec {
                    resume: Some(self.session()?.sid.clone()),
                    model: Some(model),
                };
                self.restart(spec, self.grant.clone(), false, "/model", |_, _, _| {
                    Ok(Value::Null)
                })
            }
            Call::SessionNew => {
                let spec = Spec {
                    resume: None,
                    model: self.spec.model.clone(),
                };
                self.restart(spec, self.grant.clone(), false, "/new", |_, _, _| {
                    Ok(Value::Null)
                })
            }
            Call::SessionResume { id } => {
                // refused before anything is ended: a session that cannot be resumed here changes nothing
                let cwd = self.cwd.clone().ok_or("not started: send start first")?;
                self.ctx(&h).check_resumable(&id, &cwd)?;
                let spec = Spec {
                    resume: Some(id),
                    model: self.spec.model.clone(),
                };
                self.restart(spec, self.grant.clone(), false, "/resume", |_, _, _| {
                    Ok(Value::Null)
                })
            }
            Call::SessionClose { id } => {
                check_id(&id)?;
                if self.session()?.sid != id {
                    return Err("only this engine's own session can be closed".into());
                }
                let spec = Spec {
                    resume: None,
                    model: self.spec.model.clone(),
                };
                // hidden only once the fresh session runs: a failed restart resumes the session, still listed
                self.restart(spec, self.grant.clone(), false, "/close", |c, old, _| {
                    c.close_session(old).map(|_| Value::Null)
                })
            }
            Call::WorkspaceClose { workspace } => {
                check_key(&workspace)?;
                if ws_key(self).as_deref() == Some(workspace.as_str()) {
                    let spec = Spec {
                        resume: None,
                        model: self.spec.model.clone(),
                    };
                    self.restart(
                        spec,
                        self.grant.clone(),
                        false,
                        "/close-workspace",
                        |c, _, new| {
                            let mut r = c.close_workspace(&workspace)?;
                            // the fresh session holds its own lock: not "left open by another process"
                            if let Some(h) = r["held"].as_array_mut() {
                                h.retain(|x| x.as_str() != Some(new));
                            }
                            Ok(r)
                        },
                    )
                } else {
                    Ok(self.ctx(&h).close_workspace(&workspace)?)
                }
            }
            Call::SessionRename { id, title } => {
                check_id(&id)?;
                let l = self.session()?;
                if l.sid != id {
                    return Err("only this engine's own session can be renamed".into());
                }
                match &l.host.store {
                    Some(s) => Ok(crate::rename::rename(s, &title).map(|t| json!({"title": t}))?),
                    None => Err("/rename: this session is not stored".into()),
                }
            }
            Call::FsList { dir, offset } => {
                // a page after the first continues the same scan; the first one reads the folder again
                if offset == 0 || self.tree.is_none() {
                    self.heavy()?;
                    let root = self.files_root()?;
                    self.tree = Some(engine_fs::scan(&root, self.trusted, self.env));
                }
                let d = self.tree.as_ref().ok_or("no scan")?;
                Ok(engine_fs::list_of(d, &dir, offset as usize, self.trusted)?)
            }
            Call::FsRead { path } => Ok(engine_fs::read(&self.files_root()?, &path)?),
            Call::FsGitStatus => {
                if self.trusted {
                    self.heavy()?;
                }
                Ok(engine_fs::git_status(
                    &self.files_root()?,
                    self.trusted,
                    self.env,
                )?)
            }
            Call::FsGitDiff => {
                if self.trusted {
                    self.heavy()?;
                }
                Ok(engine_fs::git_diff(
                    &self.files_root()?,
                    self.trusted,
                    self.env,
                )?)
            }
            Call::ImagePut {
                upload,
                seq,
                mime,
                size,
                data,
                last,
            } => {
                self.session()?;
                let p = Put {
                    upload: upload.as_deref(),
                    seq,
                    mime: mime.as_deref(),
                    size,
                    data: &data,
                    last,
                };
                Ok(self.images.put(&p, Instant::now())?)
            }
            Call::ImageDrop { id } => {
                check_id(&id)?;
                self.images.remove(&[id]);
                Ok(json!({}))
            }
            Call::RewindList => {
                let sid = self.session()?.sid.clone();
                let ctx = ctx_of(self.common, self.env, &h);
                Ok(engine_ops::rewind_list(
                    &ctx,
                    &sid,
                    &hide_paths(&h, self.cwd.as_deref()),
                )?)
            }
            Call::RewindDryRun { checkpoint } => {
                self.idle()?;
                let sid = self.session()?.sid.clone();
                let ctx = ctx_of(self.common, self.env, &h);
                let hide = hide_paths(&h, self.cwd.as_deref());
                Ok(engine_ops::rewind_dry_run(
                    &mut self.tokens,
                    &ctx,
                    &sid,
                    checkpoint,
                    &hide,
                )?)
            }
            Call::RewindApply { token } => {
                self.idle()?;
                let sid = self.session()?.sid.clone();
                let ctx = ctx_of(self.common, self.env, &h);
                let hide = hide_paths(&h, self.cwd.as_deref());
                Ok(engine_ops::rewind_apply(
                    &mut self.tokens,
                    &ctx,
                    &sid,
                    &token,
                    &hide,
                )?)
            }
            Call::CleanPlan => {
                self.heavy()?;
                self.idle()?;
                let sid = self.session()?.sid.clone();
                let ctx = ctx_of(self.common, self.env, &h);
                Ok(engine_ops::clean_plan(&mut self.tokens, &ctx, &sid)?)
            }
            Call::CleanApply { token, backup } => {
                self.heavy()?;
                self.idle()?;
                let sid = self.session()?.sid.clone();
                let ctx = ctx_of(self.common, self.env, &h);
                Ok(engine_ops::clean_apply(
                    &mut self.tokens,
                    &ctx,
                    &sid,
                    &token,
                    backup,
                    &hide_paths(&h, self.cwd.as_deref()),
                )?)
            }
            Call::MemoryList
            | Call::MemoryRetrieve { .. }
            | Call::MemoryShow { .. }
            | Call::MemoryAdd { .. }
            | Call::MemoryForget { .. } => {
                let cwd = self.cwd.clone().ok_or("not started: send start first")?;
                let m = engine_ops::Mem::new(&self.ctx(&h), &cwd, self.trusted)?;
                Ok(match call {
                    Call::MemoryList => m.list()?,
                    Call::MemoryRetrieve { query } => m.retrieve(&query)?,
                    Call::MemoryShow { id } => m.show(&id)?,
                    Call::MemoryAdd { text, user } => m.add(&text, user)?,
                    Call::MemoryForget { id } => m.forget(&id)?,
                    _ => unreachable!("matched above"),
                })
            }
            Call::MemoryNote { text, user } => {
                if text.len() > engine_ops::MAX_NOTE_BYTES {
                    return Err(format!(
                        "a note must be at most {} bytes",
                        engine_ops::MAX_NOTE_BYTES
                    )
                    .into());
                }
                // a project note is only for a folder the engine trusts (its instruction file is loaded only then);
                // the client is trusted to be the window the person confirmed the note in
                if !user && !self.trusted {
                    return Err(
                        "a project note needs a trusted folder: save it to the user file instead"
                            .into(),
                    );
                }
                let hide = hide_paths(&h, self.cwd.as_deref());
                let done = self
                    .session()?
                    .notes
                    .save(user, &text)
                    .map_err(|e| engine_ops::scrub(&e, &hide))?;
                Ok(json!({"text": engine_ops::scrub(&done, &hide)}))
            }
            Call::ConfigEffort { effort } => {
                let ctx = ctx_of(self.common, self.env, &h);
                Ok(engine_ops::effort(
                    &ctx,
                    self.session()?,
                    &self.tiers,
                    effort,
                )?)
            }
            Call::ContextSet { value } => {
                let ctx = ctx_of(self.common, self.env, &h);
                let hide = hide_paths(&h, self.cwd.as_deref());
                self.session()?;
                Ok(engine_ops::context(&ctx, &self.label, &value, &hide)?)
            }
            Call::DoctorSummary => {
                self.heavy()?;
                let cwd = self.cwd.clone().ok_or("not started: send start first")?;
                Ok(engine_ops::doctor(&self.ctx(&h), &cwd, self.trusted)?)
            }
            Call::McpList => Ok(engine_ops::mcp_list(self.session()?.mcp.as_ref())),
            Call::McpAdd {
                name,
                url,
                transport,
                env,
                header,
                command,
            } => {
                let hide = hide_paths(&h, self.cwd.as_deref());
                let a = engine_ops::McpAdd {
                    name,
                    url,
                    transport,
                    env,
                    header,
                    command,
                };
                Ok(engine_ops::mcp_add(&self.ctx(&h), a, &hide)?)
            }
            Call::McpRemove { name } => {
                let hide = hide_paths(&h, self.cwd.as_deref());
                Ok(engine_ops::mcp_remove(
                    &self.ctx(&h),
                    self.session()?.mcp.as_ref(),
                    &name,
                    &hide,
                )?)
            }
            Call::Clear => {
                let l = self.session()?;
                if l.running() || self.compacting.load(Ordering::SeqCst) {
                    return Err("a task is running".into());
                }
                lock(&l.history).clear();
                Ok(json!({}))
            }
            Call::Compact { instructions } => {
                if let Some(i) = &instructions {
                    check_instructions(i)?;
                }
                Ok(self.compact(instructions)?)
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
        if self.compacting.load(Ordering::SeqCst) {
            return Err("a compaction is already running".into());
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
        let busy = self.compacting.clone();
        busy.store(true, Ordering::SeqCst);
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
            busy.store(false, Ordering::SeqCst);
            wire.msg(&Out::Compacted {
                error: r.err().map(|e| sanitize_for_display(&e.to_string())),
            });
        });
        Ok(json!({"started": false}))
    }

    /// At most [`MAX_CALLS_PER_SEC`] store-backed requests a second; the rest are refused.
    fn allow(&mut self) -> bool {
        within_limit(&mut self.window, Instant::now())
    }
}

/// One second's count of requests: true while it is at most [`MAX_CALLS_PER_SEC`].
fn within_limit(window: &mut (Instant, u32), now: Instant) -> bool {
    if now.duration_since(window.0) >= Duration::from_secs(1) {
        *window = (now, 0);
    }
    window.1 += 1;
    window.1 <= MAX_CALLS_PER_SEC
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

/// What the reader thread hands the worker.
enum Item {
    Req(Request),
    /// The input ended (`Err`: it broke).
    End(Result<(), String>),
}

/// Reads and parses the requests. `answer`, `answer_input` and `cancel` are served here, at once, whatever
/// the worker is busy with; everything else is queued for it (a full queue is refused, never waited for).
fn read_requests(mut input: impl BufRead, wire: &Wire, oob: &Oob, queue: &mpsc::SyncSender<Item>) {
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
                        Err(refused(
                            "malformed request: unknown method or invalid parameters",
                        )),
                    );
                }
                continue;
            }
        };
        let id = req.id;
        match req.call {
            Call::Cancel => {
                oob.cancel_upto
                    .store(oob.sends_queued.load(Ordering::SeqCst), Ordering::SeqCst);
                if let Some(slot) = lock(&oob.cancel).as_ref()
                    && let Some(t) = lock(slot).as_ref()
                {
                    t.cancel();
                }
                wire.result(id, Ok(json!({})));
            }
            Call::Answer { id: ask, choice } => {
                let ok = oob.asks.answer(&ask, choice);
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            Call::AnswerInput { id: ask, reply } => {
                let ok = oob
                    .asks
                    .answer_input(&ask, reply.map(|r| r.into_user_reply()));
                wire.result(id, Ok(json!({"accepted": ok})));
            }
            call => {
                let is_send = matches!(call, Call::Send { .. });
                match queue.try_send(Item::Req(Request { id, call })) {
                    Ok(()) => {
                        if is_send {
                            oob.sends_queued.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Err(mpsc::TrySendError::Full(_)) => {
                        wire.result(id, Err(refused("too many requests")));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                }
            }
        }
    };
    // blocks when the queue is full: the worker is alive then (or gone, which unblocks it)
    let _ = queue.send(Item::End(end));
}

/// Serves one session until `close` or the end of `input`. `Err`: the stream broke (a line over the
/// limit, a read error); the session is ended before returning either way. The reader thread is not joined
/// (it may sit in a blocking read): the process exits after this returns.
pub fn serve(
    common: &Common,
    limits: &Limits,
    env: &Env,
    input: impl BufRead + Send + 'static,
    output: Box<dyn Write + Send>,
) -> Result<(), String> {
    let wire = Arc::new(Wire(Mutex::new(output)));
    let asks = Asks::default();
    let oob = Arc::new(Oob {
        asks: asks.clone(),
        cancel: Mutex::new(None),
        sends_queued: AtomicU64::new(0),
        cancel_upto: AtomicU64::new(0),
    });
    let (queue, work) = mpsc::sync_channel::<Item>(QUEUE);
    {
        let (wire, oob) = (wire.clone(), oob.clone());
        std::thread::spawn(move || read_requests(input, &wire, &oob, &queue));
    }
    let mut srv = Srv {
        common,
        limits,
        env,
        wire: wire.clone(),
        asks,
        oob,
        live: None,
        cwd: None,
        began: false,
        spec: Spec::default(),
        grant: None,
        trusted: false,
        compacting: Arc::default(),
        window: (Instant::now(), 0),
        last_restart: None,
        label: String::new(),
        tiers: vec![],
        images: Images::default(),
        tokens: Tokens::default(),
        heavy_at: None,
        heavy_running: false,
        tree: None,
    };
    wire.msg(&Out::hello());
    let mut closing = None;
    // sends dequeued so far: the ordinal a pending cancel is compared with
    let mut sends_seen: u64 = 0;
    let end = loop {
        let req = match work.recv() {
            Ok(Item::Req(r)) => r,
            Ok(Item::End(r)) => break r,
            Err(_) => break Ok(()),
        };
        let id = req.id;
        match req.call {
            Call::Start { .. } if srv.began => wire.result(id, Err("already started".into())),
            Call::Start { cwd, resume, model } => {
                let cwd = PathBuf::from(cwd);
                let spec = Spec { resume, model };
                match start(&srv, &cwd, &spec, None, true) {
                    Ok((l, v, trusted)) => {
                        srv.adopt(l, trusted, &v);
                        srv.cwd = Some(cwd);
                        srv.spec = spec;
                        srv.began = true;
                        wire.result(id, Ok(v));
                    }
                    Err(e) => wire.result(id, Err(e)),
                }
            }
            Call::Send { .. } if srv.compacting.load(Ordering::SeqCst) => {
                // the compaction works on a copy of the history and then replaces it: a task started meanwhile would lose its messages
                sends_seen += 1;
                wire.result(
                    id,
                    Err(refused(
                        "busy: a compaction is running; try again when it ended",
                    )),
                );
            }
            Call::Send { text, images } => {
                sends_seen += 1;
                let w = wire.clone();
                let attached = srv.images.peek(&images);
                let r = match (&srv.live, attached) {
                    (None, _) => Err("not started: send start first".to_string()),
                    (_, Err(e)) => Err(e),
                    (Some(l), Ok(imgs)) => l
                        .start_task(text, imgs, false, move |r| {
                            w.msg(&Out::TaskDone { error: r.err() });
                        })
                        .map_err(String::from),
                };
                if r.is_ok() {
                    srv.images.remove(&images);
                }
                // a `cancel` that reached the reader before this send was dequeued applies to it
                if r.is_ok()
                    && sends_seen <= srv.oob.cancel_upto.load(Ordering::SeqCst)
                    && let Some(l) = &srv.live
                {
                    l.cancel();
                }
                wire.result(id, r.map(|()| json!({})));
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
            Call::Close => {
                closing = Some(id);
                break Ok(());
            }
            other => {
                let restart = other.is_restart();
                let r = if srv.allow() {
                    srv.call(other)
                } else {
                    Err(CallError::Refused("too many requests".into()))
                };
                let fatal = matches!(r, Err(CallError::Fatal(_)));
                wire.result(id, r.map_err(|e| e.wire(restart)));
                if fatal {
                    break Ok(());
                }
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

// The reader thread owns its input: it must be `Send` on every OS (a `StdinLock` guard is not on Windows).
const _: () = {
    const fn send<T: Send>() {}
    send::<std::io::BufReader<std::io::Stdin>>();
};

/// `synt engine --stdio`: stdin and stdout of this process. Project trust is not available over the
/// protocol yet, so the trust flags are refused: an engine is untrusted for project configuration.
pub fn run_stdio(
    common: &Common,
    limits: &Limits,
    governed: bool,
    env: &Env,
) -> Result<u8, String> {
    if common.trust_project || !common.approve_mcp.is_empty() {
        return Err(
            "engine --stdio: --trust-project and --approve-mcp are not supported (the project stays untrusted)"
                .into(),
        );
    }
    // a headless engine is unattended: the governed defaults apply (wall cap, cost cap required,
    // worktree, ...; `--no-cost-cap` opts out of the cost cap only). A desktop host (`--host
    // desktop`) has a person at the window and keeps the interactive behaviour.
    let mut common = common.clone();
    common.governed = governed;
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
        // `Stdin` (not its lock guard) is `Send`: the reader thread owns it and locks per read
        std::io::BufReader::new(std::io::stdin()),
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

    #[test]
    fn cancel_and_answer_are_served_at_once_while_the_worker_is_busy() {
        let sink = Sink::default();
        let wire = Arc::new(Wire(Mutex::new(Box::new(sink.clone()))));
        let oob = Arc::new(Oob {
            asks: Asks::default(),
            cancel: Mutex::new(None),
            sends_queued: AtomicU64::new(0),
            cancel_upto: AtomicU64::new(0),
        });
        // a queue of one that nobody reads: the worker is stuck on a slow call
        let (q, rx) = mpsc::sync_channel(1);
        let input = std::io::Cursor::new(
            [
                r#"{"id":1,"method":"workspaces.list"}"#,
                r#"{"id":2,"method":"workspaces.list"}"#,
                r#"{"id":3,"method":"workspaces.list"}"#,
                r#"{"id":9,"method":"cancel"}"#,
                r#"{"id":10,"method":"answer","params":{"id":"x","choice":"once"}}"#,
            ]
            .join("\n")
            .into_bytes(),
        );
        let (w, o) = (wire.clone(), oob.clone());
        let t = std::thread::spawn(move || read_requests(input, &w, &o, &q));
        let t0 = Instant::now();
        let text = loop {
            let out = String::from_utf8(lock(&sink.0).clone()).unwrap();
            if out.contains(r#""id":10"#) || t0.elapsed() > Duration::from_secs(10) {
                break out;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(rx); // lets the reader's closing item go
        t.join().unwrap();
        assert!(
            text.contains(r#""id":9"#) && text.contains(r#""id":10"#),
            "{text}"
        );
        // the two that did not fit were refused, not waited for
        assert_eq!(text.matches("too many requests").count(), 2, "{text}");
    }

    #[test]
    fn the_rate_limit_is_exactly_fifty_a_second() {
        let t0 = Instant::now();
        let mut w = (t0, 0);
        for i in 1..=MAX_CALLS_PER_SEC {
            assert!(within_limit(&mut w, t0), "request {i} of the second");
        }
        assert!(!within_limit(&mut w, t0), "the 51st is the first refused");
        assert!(!within_limit(&mut w, t0 + Duration::from_millis(999)));
        assert!(
            within_limit(&mut w, t0 + Duration::from_secs(1)),
            "a new second starts"
        );
    }

    #[test]
    fn a_cancel_that_arrives_before_its_queued_send_is_remembered_for_that_send_only() {
        let sink = Sink::default();
        let wire = Arc::new(Wire(Mutex::new(Box::new(sink.clone()))));
        let oob = Arc::new(Oob {
            asks: Asks::default(),
            cancel: Mutex::new(None),
            sends_queued: AtomicU64::new(0),
            cancel_upto: AtomicU64::new(0),
        });
        let (q, rx) = mpsc::sync_channel(8);
        let input = std::io::Cursor::new(
            [
                r#"{"id":1,"method":"send","params":{"text":"a"}}"#,
                r#"{"id":2,"method":"cancel"}"#,
                r#"{"id":3,"method":"send","params":{"text":"b"}}"#,
            ]
            .join("\n")
            .into_bytes(),
        );
        read_requests(input, &wire, &oob, &q);
        // the cancel covers send #1 (still queued when it came), not the send queued after it
        assert_eq!(oob.cancel_upto.load(Ordering::SeqCst), 1);
        assert_eq!(oob.sends_queued.load(Ordering::SeqCst), 2);
        let queued = rx.try_iter().filter(|i| matches!(i, Item::Req(_))).count();
        assert_eq!(queued, 2, "both sends still reach the worker");
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
