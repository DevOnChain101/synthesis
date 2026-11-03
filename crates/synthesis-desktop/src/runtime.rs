//! `DesktopRuntime`: one per session, created on the first desktop call. It runs the pinned
//! agent-desktop CLI one command at a time and scopes every command to a window of the app
//! Synthesis launched (never `--app`, never a display, never `--headed`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use process_wrap::tokio::{CommandWrap, KillOnDrop};
use serde_json::Value;
use synthesis_config::Env;
use synthesis_permissions::desktop::{Origin, hard_denied};
use tokio::io::AsyncReadExt;

use crate::actions::Action;
use crate::envelope::{self, Delivery};
use crate::error::{DesktopError, Result};
use crate::limiter::Limiter;
use crate::pin::{self, Pin, PinStatus};
use crate::proctree::ProcSource;
use crate::snapshot::{self, RefTable, Snapshot};
use crate::text::clean;
use crate::untrusted::UntrustedText;

pub struct DesktopOptions {
    /// The agent-desktop executable.
    pub binary: PathBuf,
    /// Artifact directory of this feature (`<artifact_root>/desktop`), mode 0700.
    pub state_dir: PathBuf,
    /// `synt.desktop.allow_unpinned`: run a binary that does not match the pin.
    pub allow_unpinned: bool,
    pub pin: Pin,
    /// Per command (a wait adds its own timeout).
    pub call_timeout: Duration,
    /// Bytes of stdout accepted from one command.
    pub max_output: usize,
    pub actions_per_sec: f64,
    pub max_actions: u32,
}

impl DesktopOptions {
    pub fn new(binary: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            binary,
            state_dir,
            allow_unpinned: false,
            pin: Pin::builtin(),
            call_timeout: Duration::from_secs(30),
            max_output: 4 * 1024 * 1024,
            actions_per_sec: 2.0,
            max_actions: 300,
        }
    }
}

/// An app Synthesis launched (or, for external ones, the person approved launching).
#[derive(Debug, Clone)]
pub struct App {
    pub id: String,
    pub pid: u32,
    pub origin: Origin,
    /// Display name (the executable's file name), cleaned.
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: String,
    pub title: UntrustedText,
    pub app_name: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub struct Windows {
    pub windows: Vec<Window>,
    /// Windows of the app's process tree that were hidden by policy (hard-deny list, the
    /// terminal or IDE that hosts Synthesis): app names only.
    pub hidden: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Shot {
    pub path: PathBuf,
    pub window_id: String,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub enum WaitFor {
    Text(String),
    /// A ref of a snapshot of this app (agent-desktop waits for it to exist).
    Element(String),
    /// This window (or, with `None`, every window of the app) is gone.
    WindowGone(Option<String>),
}

struct State {
    apps: HashMap<String, App>,
    refs: RefTable,
    limiter: Limiter,
    next_app: u64,
    next_shot: u64,
}

pub struct DesktopRuntime {
    opts: DesktopOptions,
    env: Vec<(String, String)>,
    procs: Arc<dyn ProcSource>,
    host_pid: u32,
    pub pin_status: PinStatus,
    /// What `agent-desktop version` reported.
    pub version: String,
    state: Mutex<State>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Variables the CLI needs to reach the user's desktop session; everything else (above all
/// every credential in the process environment) is not passed.
const ENV_ALLOW: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "PATH",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS", // AT-SPI is reached over the session bus
    "SystemRoot",
    "SYSTEMROOT",
    "windir",
    "USERNAME",
    "APPDATA",
    "LOCALAPPDATA",
];

/// A graphical session is present (Linux needs a display variable; macOS and Windows cannot be
/// told apart from here, except a remote shell on macOS).
pub fn display_available(env: &Env) -> std::result::Result<(), String> {
    if cfg!(target_os = "linux")
        && env.get_nonempty("DISPLAY").is_none()
        && env.get_nonempty("WAYLAND_DISPLAY").is_none()
    {
        return Err("no DISPLAY or WAYLAND_DISPLAY: there is no graphical session".into());
    }
    if cfg!(target_os = "macos") && env.get_nonempty("SSH_CONNECTION").is_some() {
        return Err("this is a remote (ssh) shell: it has no access to the desktop session".into());
    }
    Ok(())
}

/// Windows Subsystem for Linux has no desktop session of its own.
pub fn is_wsl() -> bool {
    cfg!(target_os = "linux")
        && std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|s| {
            s.to_lowercase().contains("microsoft") || s.to_lowercase().contains("wsl")
        })
}

fn private_dir(p: &std::path::Path) -> Result<()> {
    let un = |e: std::io::Error| DesktopError::Unavailable(format!("{}: {e}", p.display()));
    std::fs::create_dir_all(p).map_err(un)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // the feature's root and the dir itself: screenshots and UI state are private
        for d in [p.parent(), Some(p)].into_iter().flatten() {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(un)?;
        }
    }
    Ok(())
}

impl DesktopRuntime {
    pub async fn start(
        opts: DesktopOptions,
        env: &Env,
        procs: Arc<dyn ProcSource>,
    ) -> Result<Self> {
        let un = |m: String| DesktopError::Unavailable(m);
        if is_wsl() {
            return Err(un(
                "WSL is not supported: it has no desktop session of its own".into(),
            ));
        }
        display_available(env).map_err(un)?;
        if !opts.binary.is_file() {
            return Err(un(format!(
                "agent-desktop not found at {} (set synt.desktop.binary in your user settings)",
                opts.binary.display()
            )));
        }
        let (bin, pn) = (opts.binary.clone(), opts.pin.clone());
        let pin_status = tokio::task::spawn_blocking(move || pin::check(&bin, &pn))
            .await
            .map_err(|e| un(e.to_string()))?
            .map_err(|e| un(format!("cannot read {}: {e}", opts.binary.display())))?;
        match (&pin_status, opts.allow_unpinned) {
            (PinStatus::Match, _) | (_, true) => {}
            (PinStatus::Unpinned, false) => {
                return Err(un("this build of Synthesis has no verified agent-desktop pin yet (docs/DESKTOP-CONTROL.md, D0 checklist); set synt.desktop.allow_unpinned to true in your user settings to run an unverified binary at your own risk".into()));
            }
            (PinStatus::Mismatch { .. }, false) => {
                return Err(un(format!(
                    "{} does not match the pinned sha256; refusing to run it (synt.desktop.allow_unpinned overrides, at your own risk)",
                    opts.binary.display()
                )));
            }
        }
        private_dir(&opts.state_dir.join("state"))?;
        private_dir(&opts.state_dir.join("shots"))?;
        let mut child_env: Vec<(String, String)> = ENV_ALLOW
            .iter()
            .filter_map(|k| env.get_nonempty(k).map(|v| ((*k).to_owned(), v.to_owned())))
            .collect();
        child_env.push((
            "AGENT_DESKTOP_HOME".into(),
            opts.state_dir.join("state").display().to_string(),
        ));
        let state = State {
            apps: HashMap::new(),
            refs: RefTable::default(),
            limiter: Limiter::new(opts.actions_per_sec, opts.max_actions),
            next_app: 1,
            next_shot: 1,
        };
        let mut rt = Self {
            opts,
            env: child_env,
            procs,
            host_pid: std::process::id(),
            pin_status,
            version: String::new(),
            state: Mutex::new(state),
        };
        let v = rt
            .run(&["version".into()], "version", rt.opts.call_timeout)
            .await?;
        let version = clean(v["version"].as_str().unwrap_or(""), 40);
        if version.is_empty() {
            return Err(DesktopError::Protocol(
                "agent-desktop version: no version".into(),
            ));
        }
        if rt.opts.pin.is_filled() && version != rt.opts.pin.version {
            return Err(un(format!(
                "agent-desktop reports version {version}, the pin is {}",
                rt.opts.pin.version
            )));
        }
        rt.version = version;
        Ok(rt)
    }

    /// Runs one agent-desktop command (`args` are everything after the binary name) and returns
    /// `data` of its envelope. The process gets its own group / job object and dies with the
    /// call: on timeout, on too much output, and when the future is dropped (cancel, session end).
    pub async fn run(&self, args: &[String], expect: &str, timeout: Duration) -> Result<Value> {
        let mut wrap = CommandWrap::with_new(&self.opts.binary, |c| {
            c.args(args)
                .env_clear()
                .envs(self.env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
        });
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrap.wrap(process_wrap::tokio::JobObject);
        wrap.wrap(KillOnDrop);
        let mut child = wrap.spawn().map_err(|e| {
            DesktopError::Unavailable(format!("cannot start {}: {e}", self.opts.binary.display()))
        })?;
        let Some(mut out) = child.stdout().take() else {
            return Err(DesktopError::Protocol("no stdout".into()));
        };
        let cap = self.opts.max_output;
        let read = async {
            let mut buf = Vec::new();
            (&mut out)
                .take(cap as u64 + 1)
                .read_to_end(&mut buf)
                .await
                .map(|_| buf)
        };
        let buf = match tokio::time::timeout(timeout, read).await {
            Err(_) => {
                let _ = child.start_kill();
                return Err(DesktopError::Timeout(format!(
                    "agent-desktop {expect} did not answer within {} s; it was killed",
                    timeout.as_secs()
                )));
            }
            Ok(Err(e)) => {
                return Err(DesktopError::Protocol(format!(
                    "agent-desktop {expect}: {e}"
                )));
            }
            Ok(Ok(b)) => b,
        };
        if buf.len() > cap {
            let _ = child.start_kill();
            return Err(DesktopError::Protocol(format!(
                "agent-desktop {expect}: output over {cap} bytes; it was killed. Narrow the request (skeleton, root_ref)"
            )));
        }
        // stdout closed: let the process finish (bounded), then parse
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.start_kill();
            }
        }
        envelope::parse(&buf, expect)
    }

    // ---- apps ---------------------------------------------------------------------------

    /// Records an app Synthesis started. The hard-deny list is applied to its name here too.
    pub fn register_app(&self, pid: u32, origin: Origin, target: &str) -> Result<App> {
        if let Some(f) = hard_denied(target) {
            return Err(DesktopError::Refused(format!(
                "{} is {f}: desktop control of it is never allowed",
                clean(target, 80)
            )));
        }
        let name = clean(
            target
                .rsplit(['/', '\\'])
                .find(|p| !p.is_empty())
                .unwrap_or(target),
            80,
        );
        let mut st = lock(&self.state);
        let id = format!(
            "{}{}",
            if origin == Origin::External {
                synthesis_permissions::desktop::EXTERNAL_PREFIX
            } else {
                "app-"
            },
            st.next_app
        );
        st.next_app += 1;
        let app = App {
            id: id.clone(),
            pid,
            origin,
            name,
        };
        st.apps.insert(id, app.clone());
        Ok(app)
    }

    pub fn app(&self, id: &str) -> Result<App> {
        lock(&self.state).apps.get(id).cloned().ok_or_else(|| {
            DesktopError::NotFound(format!("no app {id:?}: launch one with desktop_launch"))
        })
    }

    pub fn forget_app(&self, id: &str) {
        let mut st = lock(&self.state);
        st.apps.remove(id);
        st.refs.forget_app(id);
    }

    pub fn apps(&self) -> Vec<App> {
        lock(&self.state).apps.values().cloned().collect()
    }

    // ---- windows ------------------------------------------------------------------------

    /// Windows owned by the app's process tree, minus what policy hides.
    pub async fn windows(&self, app_id: &str) -> Result<Windows> {
        let app = self.app(app_id)?;
        let procs = self.procs.clone();
        let table = tokio::task::spawn_blocking(move || procs.table())
            .await
            .map_err(|e| DesktopError::Unavailable(e.to_string()))??;
        let tree = table.descendants(app.pid);
        let hosts = table.ancestors(self.host_pid);
        let data = self
            .run(
                &["list-windows".into()],
                "list-windows",
                self.opts.call_timeout,
            )
            .await?;
        let arr = data
            .as_array()
            .or_else(|| data["windows"].as_array())
            .ok_or_else(|| {
                DesktopError::Protocol("agent-desktop list-windows: no window list".into())
            })?;
        let mut out = Windows {
            windows: vec![],
            hidden: vec![],
        };
        for w in arr {
            let (Some(id), Some(pid)) = (
                w["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 128),
                w["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
            ) else {
                continue; // not a window we can name: never offered
            };
            if !tree.contains(&pid) {
                continue;
            }
            let app_name = clean(w["app_name"].as_str().unwrap_or(""), 80);
            if hosts.contains(&pid) || hard_denied(&app_name).is_some() {
                out.hidden.push(app_name);
                continue;
            }
            out.windows.push(Window {
                id: id.to_owned(),
                title: UntrustedText::new(clean(w["title"].as_str().unwrap_or(""), 120)),
                app_name,
                pid,
            });
        }
        Ok(out)
    }

    /// `want` if it is a window of the app, else the app's only window.
    async fn pick_window(&self, app_id: &str, want: Option<&str>) -> Result<Window> {
        let ws = self.windows(app_id).await?;
        match want {
            Some(id) => ws.windows.into_iter().find(|w| w.id == id).ok_or_else(|| {
                DesktopError::Refused(format!(
                    "window {} is not a window of this app",
                    clean(id, 60)
                ))
            }),
            None => {
                let mut it = ws.windows.into_iter();
                match (it.next(), it.next()) {
                    (Some(w), None) => Ok(w),
                    (None, _) => Err(DesktopError::NotFound(
                        "the app has no window (yet); try desktop_wait or desktop_logs".into(),
                    )),
                    (Some(a), Some(b)) => {
                        let ids: Vec<String> = [a, b]
                            .iter()
                            .map(|w| w.id.clone())
                            .chain(it.map(|w| w.id))
                            .collect();
                        Err(DesktopError::Refused(format!(
                            "the app has several windows; pass window_id (one of: {})",
                            ids.join(", ")
                        )))
                    }
                }
            }
        }
    }

    // ---- observe ------------------------------------------------------------------------

    async fn snapshot_of(
        &self,
        window: &str,
        root_ref: Option<&str>,
        skeleton: bool,
        max_nodes: usize,
    ) -> Result<Snapshot> {
        let mut a: Vec<String> = ["snapshot", "--window-id", window, "--compact"]
            .map(Into::into)
            .into();
        if skeleton {
            a.push("--skeleton".into());
        }
        if let Some(r) = root_ref {
            a.extend(["--root".into(), r.to_owned()]);
        }
        let data = self.run(&a, "snapshot", self.opts.call_timeout).await?;
        let s = snapshot::render(&data, max_nodes)?;
        if s.window_id != window {
            return Err(DesktopError::Protocol(
                "agent-desktop snapshot answered for a different window; ignored".into(),
            ));
        }
        Ok(s)
    }

    pub async fn snapshot(
        &self,
        app_id: &str,
        window_id: Option<&str>,
        root_ref: Option<&str>,
        skeleton: bool,
        max_nodes: usize,
    ) -> Result<Snapshot> {
        let want = match root_ref {
            Some(r) => Some(lock(&self.state).refs.window_of(app_id, r)?),
            None => window_id.map(str::to_owned),
        };
        let w = self.pick_window(app_id, want.as_deref()).await?;
        let s = self
            .snapshot_of(&w.id, root_ref, skeleton, max_nodes)
            .await?;
        lock(&self.state)
            .refs
            .record(app_id, &s, root_ref.is_none());
        Ok(s)
    }

    pub async fn screenshot(&self, app_id: &str, window_id: Option<&str>) -> Result<Shot> {
        let w = self.pick_window(app_id, window_id).await?;
        let n = {
            let mut st = lock(&self.state);
            st.next_shot += 1;
            st.next_shot - 1
        };
        let path = self
            .opts
            .state_dir
            .join("shots")
            .join(format!("{app_id}-{n}.png"));
        let args = [
            "screenshot".to_owned(),
            "--window-id".into(),
            w.id.clone(),
            path.display().to_string(),
        ];
        self.run(&args, "screenshot", self.opts.call_timeout)
            .await?;
        let bad = |m: &str| DesktopError::Protocol(format!("agent-desktop screenshot: {m}"));
        let meta = std::fs::metadata(&path).map_err(|_| bad("no file was written"))?;
        if meta.len() < 8 || meta.len() > 25 * 1024 * 1024 {
            return Err(bad("the image has an unexpected size"));
        }
        let mut magic = [0u8; 8];
        std::io::Read::read_exact(
            &mut std::fs::File::open(&path).map_err(|e| bad(&e.to_string()))?,
            &mut magic,
        )
        .map_err(|e| bad(&e.to_string()))?;
        if magic != *b"\x89PNG\r\n\x1a\n" {
            return Err(bad("the file is not a PNG"));
        }
        Ok(Shot {
            path,
            window_id: w.id,
            bytes: meta.len(),
        })
    }

    // ---- act ----------------------------------------------------------------------------

    /// Runs one semantic action. Returns how the CLI says it was delivered.
    pub async fn act(
        &self,
        app_id: &str,
        action: Action,
        r: Option<&str>,
        value: Option<&str>,
        window_id: Option<&str>,
    ) -> Result<Delivery> {
        let wait = lock(&self.state).limiter.take(Instant::now())?;
        tokio::time::sleep(wait).await;
        let want = match (action.needs_ref(), r) {
            (true, Some(r)) => Some(lock(&self.state).refs.window_of(app_id, r)?),
            _ => window_id.map(str::to_owned),
        };
        let w = self.pick_window(app_id, want.as_deref()).await?;
        let args = action.cli_args(r, value, &w.id)?;
        match self
            .run(&args, action.envelope_command(), self.opts.call_timeout)
            .await
        {
            Ok(d) => Ok(envelope::delivery_of(&d)),
            Err(e) => {
                if let (DesktopError::StaleRef(_), Some(r)) = (&e, r) {
                    lock(&self.state).refs.mark_stale(r);
                }
                Err(e)
            }
        }
    }

    /// Waits until `what` holds. Polls the app's own windows (text, window gone) or asks the CLI
    /// (element). Never takes a ref-registering snapshot, so it cannot supersede the model's refs.
    pub async fn wait(
        &self,
        app_id: &str,
        what: WaitFor,
        window_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Duration> {
        let t0 = Instant::now();
        if let WaitFor::Element(r) = &what {
            let w = lock(&self.state).refs.window_of(app_id, r)?;
            self.pick_window(app_id, Some(&w)).await?;
            let a = [
                "wait".to_owned(),
                "--element".into(),
                r.clone(),
                "--timeout".into(),
                timeout.as_millis().to_string(),
            ];
            self.run(&a, "wait", timeout + self.opts.call_timeout)
                .await?;
            return Ok(t0.elapsed());
        }
        loop {
            let done = match &what {
                WaitFor::Text(t) => match self.pick_window(app_id, window_id).await {
                    Ok(w) => self
                        .snapshot_of(&w.id, None, false, 2000)
                        .await?
                        .tree
                        .expose()
                        .to_lowercase()
                        .contains(&t.to_lowercase()),
                    Err(DesktopError::NotFound(_)) => false,
                    Err(e) => return Err(e),
                },
                WaitFor::WindowGone(w) => {
                    let ws = self.windows(app_id).await?.windows;
                    match w {
                        Some(id) => !ws.iter().any(|x| &x.id == id),
                        None => ws.is_empty(),
                    }
                }
                WaitFor::Element(_) => unreachable!(),
            };
            if done {
                return Ok(t0.elapsed());
            }
            if t0.elapsed() >= timeout {
                return Err(DesktopError::Timeout(format!(
                    "the condition did not hold within {} ms",
                    timeout.as_millis()
                )));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
}
