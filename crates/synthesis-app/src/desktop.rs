//! Desktop control wiring for `synt` (docs/DESKTOP-CONTROL.md): the `synt.desktop` user settings,
//! the audit sink that turns the tools' records into `desktop.*` events, and the `synt doctor`
//! lines. The tools themselves are in `synthesis-tools`, the runtime in `synthesis-desktop`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use synthesis_config::Env;
use synthesis_desktop::{Pin, PinStatus, pin};
use synthesis_events::Event;
use synthesis_harness::{EventSink, Scope};
use synthesis_tools::{DesktopAudit, DesktopConfig, DesktopHost};

/// What `synt.desktop` of the USER settings declares. Off unless `enabled` is true.
#[derive(Debug, Default, PartialEq)]
pub struct DesktopSettings {
    pub enabled: bool,
    /// Absolute path of the agent-desktop executable.
    pub binary: Option<PathBuf>,
    /// Run a binary that does not match the pin (at the person's own risk).
    pub allow_unpinned: bool,
    /// Let `desktop_act` send key combos (off by default).
    pub allow_press: bool,
    pub actions_per_sec: Option<f64>,
    pub max_actions: Option<u32>,
    pub warnings: Vec<String>,
}

pub fn parse_settings(raw: &Map<String, Value>) -> DesktopSettings {
    let mut s = DesktopSettings {
        enabled: raw.get("enabled") == Some(&Value::Bool(true)),
        allow_unpinned: raw.get("allow_unpinned") == Some(&Value::Bool(true)),
        allow_press: raw.get("allow_press") == Some(&Value::Bool(true)),
        ..Default::default()
    };
    if let Some(b) = raw.get("binary") {
        match b.as_str().map(PathBuf::from).filter(|p| p.is_absolute()) {
            Some(p) => s.binary = Some(p),
            None => s
                .warnings
                .push("synt.desktop.binary: not an absolute path, ignored".into()),
        }
    }
    if let Some(v) = raw.get("actions_per_second") {
        match v.as_f64().filter(|f| (0.1..=10.0).contains(f)) {
            Some(f) => s.actions_per_sec = Some(f),
            None => s
                .warnings
                .push("synt.desktop.actions_per_second: expected 0.1 to 10, ignored".into()),
        }
    }
    if let Some(v) = raw.get("max_actions") {
        match v.as_u64().filter(|n| (1..=5000).contains(n)) {
            Some(n) => s.max_actions = Some(n as u32),
            None => s
                .warnings
                .push("synt.desktop.max_actions: expected 1 to 5000, ignored".into()),
        }
    }
    s
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "agent-desktop.exe"
    } else {
        "agent-desktop"
    }
}

impl DesktopSettings {
    /// The configured binary, or the one next to the `synt` executable.
    pub fn binary_path(&self, exe_dir: Option<&Path>) -> Option<PathBuf> {
        self.binary.clone().or_else(|| {
            exe_dir
                .map(|d| d.join(binary_name()))
                .filter(|p| p.is_file())
        })
    }

    /// `None` while the feature is off: the tools are not registered at all.
    pub fn config(
        &self,
        exe_dir: Option<&Path>,
        env: &Env,
        host: Arc<dyn DesktopHost>,
    ) -> Option<DesktopConfig> {
        self.enabled.then(|| DesktopConfig {
            binary: self.binary_path(exe_dir),
            allow_unpinned: self.allow_unpinned,
            allow_press: self.allow_press,
            process_env: Some(env.clone()),
            actions_per_sec: self.actions_per_sec,
            max_actions: self.max_actions,
            host: Some(host),
            ..Default::default()
        })
    }
}

/// Turns the tools' audit records into events (`desktop.launch|act|screenshot|denied`).
pub struct AuditHost {
    pub sink: EventSink,
}

impl std::fmt::Debug for AuditHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuditHost")
    }
}

impl DesktopHost for AuditHost {
    fn audit(&self, e: DesktopAudit) {
        let ev = match e {
            DesktopAudit::Launch {
                app_id,
                pid,
                name,
                external,
            } => Event::DesktopLaunch {
                app_id,
                pid: pid.into(),
                name,
                origin: if external { "external" } else { "project" }.into(),
            },
            DesktopAudit::Act {
                app_id,
                action,
                r#ref,
                delivery,
            } => Event::DesktopAct {
                app_id,
                action,
                r#ref,
                delivery,
            },
            DesktopAudit::Screenshot {
                app_id,
                window_id,
                path,
            } => Event::DesktopScreenshot {
                app_id,
                window_id,
                path,
            },
            DesktopAudit::Denied { tool, reason } => Event::DesktopDenied { tool, reason },
        };
        self.sink.emit(&Scope::default(), ev);
    }
}

/// `agent-desktop permissions` with a scrubbed environment and a private state dir; only called
/// for a binary that matched the pin.
fn permissions(bin: &Path, env: &Env, dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("permissions")
        .env_clear()
        .env("AGENT_DESKTOP_HOME", dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for k in [
        "HOME",
        "USERPROFILE",
        "PATH",
        "TMPDIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "SystemRoot",
    ] {
        if let Some(v) = env.get_nonempty(k) {
            cmd.env(k, v);
        }
    }
    (|| {
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        while child.try_wait().map_err(|e| e.to_string())?.is_none() {
            if t0.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                return Err("timed out".to_string());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let data = synthesis_desktop::envelope::parse(&out.stdout, "permissions")
            .map_err(|e| e.to_string())?;
        let show = |k: &str| synthesis_desktop::text::clean(&data[k].to_string(), 40);
        Ok(format!(
            "accessibility {}, screen recording {}, automation {}",
            show("accessibility"),
            show("screen_recording"),
            show("automation")
        ))
    })()
}

/// The temp directory from the injected Env (`TMPDIR`, `TEMP`, `TMP`), never the process's.
fn temp_root(env: &Env) -> PathBuf {
    ["TMPDIR", "TEMP", "TMP"]
        .iter()
        .find_map(|k| env.get_nonempty(k))
        .map_or_else(
            || {
                PathBuf::from(if cfg!(windows) {
                    r"C:\Windows\Temp"
                } else {
                    "/tmp"
                })
            },
            PathBuf::from,
        )
}

/// Adds the desktop tools to `registry` when the user settings turn them on; otherwise they do
/// not exist at all. The one place `run` wires the feature.
pub fn attach(
    registry: synthesis_tools::Registry,
    s: &DesktopSettings,
    exe_dir: Option<&Path>,
    env: &Env,
    sink: EventSink,
) -> synthesis_tools::Registry {
    match s.config(exe_dir, env, Arc::new(AuditHost { sink })) {
        Some(cfg) => registry.with_desktop(cfg),
        None => registry,
    }
}

/// Lines for `synt doctor`.
pub fn doctor_lines(s: &DesktopSettings, exe_dir: Option<&Path>, env: &Env) -> Vec<String> {
    doctor_lines_with(s, exe_dir, env, permissions)
}

fn doctor_lines_with(
    s: &DesktopSettings,
    exe_dir: Option<&Path>,
    env: &Env,
    run_permissions: impl Fn(&Path, &Env, &Path) -> Result<String, String>,
) -> Vec<String> {
    let mut v = vec![format!(
        "desktop control: {}",
        if s.enabled {
            "enabled (synt.desktop.enabled)"
        } else {
            "disabled (off by default; synt.desktop.enabled: true in your user settings turns it on)"
        }
    )];
    if s.enabled {
        let caps = synthesis_sandbox::detect().capabilities();
        v.push(format!(
            "desktop sandbox WARNING: the filesystem guarantee of this machine's sandbox is {:?}{}. A launched GUI app is confined only as far as that backend goes: macOS Seatbelt is documented as NOT a security boundary, X11 has no per-client isolation, and on macOS the Accessibility grant of the terminal app is inherited by the app's children. Treat desktop_launch as best-effort; the sandbox is never widened for GUI apps (docs/DESKTOP-CONTROL.md, release blocker).",
            caps.filesystem,
            if cfg!(target_os = "macos") { " (Seatbelt)" } else { "" }
        ));
    }
    if synthesis_desktop::is_wsl() {
        v.push("desktop: WSL is not supported (it has no desktop session of its own)".into());
    }
    match synthesis_desktop::display_available(env) {
        Ok(()) => v.push(
            "desktop session: a graphical session looks present (not verified: no OS call is made)"
                .into(),
        ),
        Err(e) => v.push(format!(
            "desktop session: not available ({e}); the desktop tools return desktop_unavailable"
        )),
    }
    let Some(bin) = s.binary_path(exe_dir) else {
        v.push("desktop binary: not configured (synt.desktop.binary, or agent-desktop next to the synt executable)".into());
        v.push("desktop pin: not checked (no binary)".into());
        v.push("desktop OS permissions: not checked (no binary)".into());
        return v;
    };
    v.push(format!("desktop binary: {}", bin.display()));
    // Hash and run the same private copy, so nothing can swap the file in between.
    // in a random private directory (created exclusively, 0700), removed when this returns
    let tmp = tempfile::Builder::new()
        .prefix("synt-desktop-doctor-")
        .tempdir_in(temp_root(env));
    let dir = tmp
        .as_ref()
        .map(|t| t.path().to_path_buf())
        .unwrap_or_default();
    let staged = match &tmp {
        Ok(_) => pin::stage(&bin, &dir.join("bin")),
        Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
    };
    let status = match &staged {
        Ok(c) => pin::check(c, &Pin::builtin()),
        Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
    };
    let verified = matches!(status, Ok(PinStatus::Match));
    v.push(match &status {
        Ok(PinStatus::Match) => "desktop pin: sha256 matches the pinned release".into(),
        Ok(PinStatus::Unpinned) => format!(
            "desktop pin: UNVERIFIED (placeholder): this build has no pinned agent-desktop release yet; the tools refuse to run{}",
            if s.allow_unpinned { " unless allow_unpinned (set: they will run it unverified)" } else { " (synt.desktop.allow_unpinned overrides, at your own risk)" }
        ),
        Ok(PinStatus::Mismatch { .. }) => "desktop pin: the binary does NOT match the pinned sha256".into(),
        Err(e) => format!("desktop pin: cannot read the binary ({e})"),
    });
    v.push(match (&staged, verified) {
        (Ok(c), true) => match run_permissions(c, env, &dir.join("state")) {
            Ok(p) => format!("desktop OS permissions: {p}"),
            Err(e) => format!("desktop OS permissions: could not be read ({e})"),
        },
        _ => "desktop OS permissions: not checked (the binary is not pin-verified, so doctor does not run it)".into(),
    });
    if cfg!(target_os = "macos") {
        v.push("desktop macOS: the Accessibility grant lands on the terminal app that runs synt, so every command the agent runs there inherits it; a signed helper .app that holds the grant instead is recommended (not built yet)".into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An absolute path on every OS (`/u/ad` is relative on Windows).
    fn abs() -> String {
        if cfg!(windows) {
            r"C:\u\ad".into()
        } else {
            "/u/ad".into()
        }
    }

    fn raw(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn off_unless_enabled_is_exactly_true() {
        assert!(!parse_settings(&raw(json!({}))).enabled);
        assert!(!parse_settings(&raw(json!({"enabled": "true"}))).enabled);
        assert!(!parse_settings(&raw(json!({"enabled": 1}))).enabled);
        let s = parse_settings(&raw(
            json!({"enabled": true, "allow_unpinned": true, "binary": &abs(), "actions_per_second": 5, "max_actions": 40}),
        ));
        assert!(s.enabled && s.allow_unpinned && s.warnings.is_empty());
        assert_eq!(
            (s.binary, s.actions_per_sec, s.max_actions),
            (Some(abs().into()), Some(5.0), Some(40))
        );
        let bad = parse_settings(&raw(
            json!({"binary": "rel/ad", "actions_per_second": 99, "max_actions": 0}),
        ));
        assert_eq!(bad.warnings.len(), 3);
        assert!(bad.binary.is_none() && bad.actions_per_sec.is_none() && bad.max_actions.is_none());
    }

    #[test]
    fn disabled_means_no_tool_config() {
        #[derive(Debug)]
        struct H;
        impl DesktopHost for H {
            fn audit(&self, _: DesktopAudit) {}
        }
        let off = parse_settings(&raw(json!({"binary": &abs()})));
        assert!(off.config(None, &Env::default(), Arc::new(H)).is_none());
        let on = parse_settings(&raw(json!({"enabled": true, "binary": &abs()})));
        assert!(on.config(None, &Env::default(), Arc::new(H)).is_some());
    }

    #[test]
    fn allow_press_is_off_unless_exactly_true() {
        assert!(!parse_settings(&raw(json!({}))).allow_press);
        assert!(!parse_settings(&raw(json!({"allow_press": "yes"}))).allow_press);
        assert!(parse_settings(&raw(json!({"allow_press": true}))).allow_press);
    }

    #[test]
    fn the_run_wiring_registers_the_desktop_tools_only_when_enabled() {
        let sink = || EventSink::new(std::io::sink(), "s", vec![]);
        let names = |r: &synthesis_tools::Registry| {
            r.specs().filter(|s| s.name.starts_with("desktop_")).count()
        };
        let reg = || synthesis_tools::Registry::unsandboxed_for_tests().with_new_tools();
        let env = Env::default();
        for raw_settings in [
            json!({}),
            json!({"binary": abs()}),
            json!({"enabled": "true"}),
            json!({"enabled": false}),
        ] {
            let s = parse_settings(&raw(raw_settings));
            assert_eq!(names(&attach(reg(), &s, None, &env, sink())), 0);
        }
        let s = parse_settings(&raw(json!({"enabled": true, "binary": abs()})));
        assert_eq!(names(&attach(reg(), &s, None, &env, sink())), 7);
    }

    #[test]
    fn doctor_does_not_run_an_unverified_binary() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("agent-desktop");
        std::fs::write(&bin, b"unverified binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = Env::from_pairs([("DISPLAY", ":0")]);
        let s = parse_settings(&raw(json!({"enabled": true, "binary": bin})));
        let calls = std::cell::Cell::new(0);
        let text = doctor_lines_with(&s, None, &env, |_, _, _| {
            calls.set(calls.get() + 1);
            Ok("called".into())
        })
        .join("\n");
        assert!(
            text.contains("desktop pin: UNVERIFIED")
                && text.contains("not checked (the binary is not pin-verified"),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "unverified binary reached the runner");
        let none = doctor_lines(&parse_settings(&raw(json!({}))), None, &env).join("\n");
        assert!(
            none.contains("disabled")
                && none.contains("not configured")
                && none.contains("OS permissions: not checked"),
            "{none}"
        );
    }
}
