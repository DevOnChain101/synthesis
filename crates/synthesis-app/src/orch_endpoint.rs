//! The orchestrator's local endpoint (ADR-017 "Endpoint", ADR-021 section 1): a unix socket or
//! Windows named pipe under `$SYNT_HOME/run/`, NDJSON frames and the mutual HMAC handshake of
//! peers, keyed by `run/orch.key`. No TCP, no HTTP.
//!
//! A client that proved the key sends `{"t":"attach","surface":"cli"}`; every mutating call on the
//! connection must then name that same surface. That is what makes `surface` more than a
//! self-asserted field: the stdio engine is read-only ([`crate::outcome_service::Service::read_only`])
//! and only a process that holds the key can queue a command. Ownership and correctness boundary,
//! not a security boundary: a program of the same OS user outside the sandbox can read the key
//! (`run/` is on the sandbox deny list, so an attempt cannot).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_peers::net::{Conn, Listener, connect, handshake_client, handshake_server};
use synthesis_peers::{Key, endpoint_ok, load_or_create_named_key, run_dir, write_private};
use synthesis_session::outcome::Surface;

use crate::outcome_client::{ClientError, Transport};
use crate::outcome_service::{Service, settings_of};
use crate::proto::{Call, Out, PROTOCOL, Request, VERSION};

pub const KEY_FILE: &str = "orch.key";
pub const REGISTRY_FILE: &str = "orch.json";
pub const LOCK_FILE: &str = "orch.lock";
/// Connections served at once; the rest are dropped before the handshake.
const MAX_CLIENTS: usize = 16;
/// Longest a connected client may take to attach.
const ATTACH_WAIT: Duration = Duration::from_secs(5);

/// A call the owner thread must run itself (it takes control-session locks).
pub type Job = (Call, tokio::sync::oneshot::Sender<Result<Value, String>>);

pub struct Ctx {
    pub env: Env,
    pub home: PathBuf,
    pub jobs: std::sync::mpsc::Sender<Job>,
    pub clients: AtomicUsize,
    /// Seconds since the unix epoch of the last call (idle exit, D20).
    pub last_call: AtomicU64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Publishes the endpoint: `run/orch.json` is replaced atomically (0600 on unix).
pub fn write_registry(home: &Path, endpoint: &str) -> std::io::Result<()> {
    let body = json!({"endpoint": endpoint, "pid": std::process::id(), "protocol": PROTOCOL});
    write_private(
        &run_dir(home).join(REGISTRY_FILE),
        body.to_string().as_bytes(),
    )
}

/// The published endpoint, only when it is one this user's orchestrator could have made (a planted
/// entry that names another path or pipe is ignored).
pub fn read_registry(home: &Path) -> Option<String> {
    let run = run_dir(home);
    let v: Value = serde_json::from_slice(&std::fs::read(run.join(REGISTRY_FILE)).ok()?).ok()?;
    let e = v.get("endpoint")?.as_str()?;
    endpoint_ok(&run, e).then(|| e.to_string())
}

/// Removes the registry entry when it still names `endpoint` (a newer owner's entry stays).
pub fn clear_registry(home: &Path, endpoint: &str) {
    if read_registry(home).as_deref() == Some(endpoint) {
        let _ = std::fs::remove_file(run_dir(home).join(REGISTRY_FILE));
    }
}

pub fn load_key(home: &Path) -> std::io::Result<Key> {
    load_or_create_named_key(&run_dir(home), KEY_FILE)
}

// ------------------------------------------------------------------ server

pub async fn serve(mut l: Listener, key: Key, ctx: Arc<Ctx>) {
    loop {
        let c = match l.accept().await {
            Ok(c) => c,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if ctx.clients.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            ctx.clients.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (key, ctx) = (key.clone(), ctx.clone());
        tokio::spawn(async move {
            let _ = connection(c, &key, &ctx).await;
            ctx.clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

async fn connection(mut c: Conn, key: &Key, ctx: &Ctx) -> std::io::Result<()> {
    // the handshake has its own timeout; nothing but nonces is sent before the key is proven
    handshake_server(&mut c, key).await?;
    let bad = |m: &'static str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, m);
    let attach = tokio::time::timeout(ATTACH_WAIT, c.recv())
        .await
        .map_err(|_| bad("no attach"))??
        .ok_or_else(|| bad("closed"))?;
    let surface = (attach.get("t") == Some(&json!("attach")))
        .then(|| serde_json::from_value::<Surface>(attach.get("surface")?.clone()).ok())
        .flatten()
        .ok_or_else(|| bad("bad attach"))?;
    c.send(&json!({"msg": "hello", "version": VERSION, "protocol": PROTOCOL}))
        .await?;
    while let Some(v) = c.recv().await? {
        ctx.last_call.store(now_secs(), Ordering::SeqCst);
        // an unreadable request ends the link: its id cannot be trusted to answer
        let req: Request = serde_json::from_value(v)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad request"))?;
        let (ok, error) = match dispatch(ctx, surface, req.call).await {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let out = serde_json::to_value(Out::Result {
            id: req.id,
            ok,
            error,
        })
        .map_err(std::io::Error::other)?;
        c.send(&out).await?;
    }
    Ok(())
}

/// One call of an attached client. Only outcome methods are served, and a mutating one must name
/// the surface the connection attached as.
pub async fn dispatch(ctx: &Ctx, attached: Surface, call: Call) -> Result<Value, String> {
    if !call.is_outcome() {
        return Err("the orchestrator endpoint serves outcome methods only".into());
    }
    if call.surface().is_some_and(|s| s != attached) {
        return Err("the call names another surface than the one this link attached as".into());
    }
    if matches!(
        call,
        Call::OutcomeCreate { .. } | Call::OrchestratorEnable { .. }
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.jobs
            .send((call, tx))
            .map_err(|_| "the orchestrator is shutting down".to_string())?;
        return rx
            .await
            .map_err(|_| "the orchestrator is shutting down".to_string())?;
    }
    let (env, home) = (ctx.env.clone(), ctx.home.clone());
    tokio::task::spawn_blocking(move || {
        let h = crate::homes(&env)?;
        let on =
            crate::outcome_policy::outcome_policy(&settings_of(&env, &h).synthesis.orchestrator)
                .enabled;
        Service::new(&home, on).handle(call)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ------------------------------------------------------------------ client

/// The `LocalSocket` transport of ADR-021: a blocking client of the orchestrator endpoint.
pub struct LocalSocket {
    rt: tokio::runtime::Runtime,
    conn: Conn,
    next: u64,
    dead: Option<String>,
}

fn io<E: ToString>(e: E) -> ClientError {
    ClientError::Io(e.to_string())
}

impl LocalSocket {
    /// Connects to the running orchestrator, proves the key and attaches as `surface`. Fails (the
    /// text says to start it) when none is running or the entry is stale.
    pub fn connect(home: &Path, surface: Surface) -> Result<Self, ClientError> {
        let endpoint = read_registry(home)
            .ok_or_else(|| io("the orchestrator is not running (no endpoint published)"))?;
        let key = load_key(home).map_err(io)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io)?;
        let conn = rt.block_on(async {
            let mut c = connect(&endpoint).await?;
            handshake_client(&mut c, &key).await?;
            c.send(&json!({"t": "attach", "surface": surface})).await?;
            let hello = c.recv().await?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed before hello")
            })?;
            Ok::<_, std::io::Error>((c, hello))
        });
        let (conn, hello) =
            conn.map_err(|e| io(format!("the orchestrator did not answer: {e}")))?;
        let exact = hello.get("msg") == Some(&json!("hello"))
            && hello.get("version") == Some(&json!(VERSION))
            && hello.get("protocol") == Some(&json!(PROTOCOL));
        if !exact {
            return Err(ClientError::Refused(
                "the orchestrator is another version: restart it".into(),
            ));
        }
        Ok(Self {
            rt,
            conn,
            next: 0,
            dead: None,
        })
    }

    /// [`Self::connect`], starting the orchestrator first when it is not running (auto-start of
    /// plan PR-12). `exe` is the `synt` binary.
    pub fn connect_or_start(
        home: &Path,
        surface: Surface,
        exe: &Path,
    ) -> Result<Self, ClientError> {
        if let Ok(s) = Self::connect(home, surface) {
            return Ok(s);
        }
        spawn_detached(exe, home).map_err(io)?;
        let mut last = io("the orchestrator did not start");
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            match Self::connect(home, surface) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn poison(&mut self, why: &str) -> ClientError {
        self.dead = Some(why.to_string());
        ClientError::Hostile(why.to_string())
    }
}

impl Transport for LocalSocket {
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        if let Some(why) = &self.dead {
            return Err(ClientError::Hostile(why.clone()));
        }
        self.next += 1;
        let id = self.next;
        let req = serde_json::to_value(Request { id, call }).map_err(io)?;
        let got = self.rt.block_on(async {
            self.conn.send(&req).await?;
            self.conn.recv().await
        });
        let v = match got {
            Ok(Some(v)) => v,
            Ok(None) => return Err(io("the orchestrator closed the link")),
            Err(e) => return Err(self.poison(&e.to_string())),
        };
        match serde_json::from_value::<Out>(v) {
            Ok(Out::Result { id: rid, ok, error }) if rid == id => match (ok, error) {
                (Some(v), None) => Ok(v),
                (None, Some(e)) => Err(ClientError::Refused(
                    synthesis_harness::sanitize_for_display(&e),
                )),
                _ => Err(self.poison("a result with neither or both of ok and error")),
            },
            _ => Err(self.poison("a message that is not the result asked for")),
        }
    }
}

/// `synt orchestrator --serve`, detached: own process group (unix) or no console (Windows), no
/// stdin, output to `run/orch.log`. The environment is inherited, so the workers it starts find
/// the same provider settings as the surface that started it.
pub fn spawn_detached(exe: &Path, home: &Path) -> std::io::Result<()> {
    let run = run_dir(home);
    synthesis_peers::net::prepare_run_dir(&run)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run.join("orch.log"))?;
    let mut c = std::process::Command::new(exe);
    // the same home the client resolved, whatever the environment says
    c.args(["orchestrator", "--serve"])
        .env("SYNT_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    // CREATE_NO_WINDOW | DETACHED_PROCESS
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0008);
    c.spawn().map(drop)
}
