//! The checks behind `synt doctor` that the desktop app shows too (`/doctor`, the Settings summary):
//! the legacy-name hints (moved here from the CLI, output unchanged) and [`rows`], one graded row per
//! check. Names of variables only, never a value.

use std::path::Path;

use synthesis_config::{Env, ProjectTrust, resolve_credential};
use synthesis_harness::sanitize_for_display as san;
use synthesis_settings::{LoadContext, load_settings};

use crate::{homes, load_config};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn glyph(self) -> &'static str {
        match self {
            Level::Ok => "\u{2713}",
            Level::Warn => "!",
            Level::Fail => "\u{2717}",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub level: Level,
    pub text: String,
}

/// `12 ok, 2 warn, 0 fail`.
pub fn summary(rows: &[Row]) -> String {
    let n = |l| rows.iter().filter(|r| r.level == l).count();
    format!(
        "{} ok, {} warn, {} fail",
        n(Level::Ok),
        n(Level::Warn),
        n(Level::Fail)
    )
}

/// `synt.cli` keys the desktop TUI tab has no use for (it has no mouse mode, queue lines or expanded diffs).
const TERMINAL_ONLY_KEYS: [&str; 4] = [
    "mouse_capture",
    "copy_on_select",
    "diffs_expanded",
    "queue_indicators",
];

/// Proxy and TLS variables `synt doctor` reports (set or not; never the value, a proxy URL can hold a password).
const STANDARD_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Runs `f` on its own thread and gives up after `limit` (the thread is left to finish by itself).
pub fn capped<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    limit: std::time::Duration,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .map_err(|_| format!("timed out after {} s", limit.as_secs()))
}

/// [`rows`] off the caller's thread, with a cap (a slow disk or a huge store must not freeze a window).
pub fn rows_capped(
    env: &Env,
    cwd: &Path,
    trusted: bool,
    limit: std::time::Duration,
) -> Result<Vec<Row>, String> {
    let (env, cwd) = (env.clone(), cwd.to_path_buf());
    capped(move || rows(&env, &cwd, trusted), limit)
}

/// WSL checks (only called on WSL). Warnings only: Synthesis never edits `/etc/wsl.conf`. Names of
/// variables only, and counts instead of PATH entries, never a value.
pub(crate) fn wsl_rows(
    host: &synthesis_platform::wsl::WslHost,
    read: synthesis_platform::wsl::Read,
    path_var: Option<&str>,
    exe: Option<&Path>,
    bwrap: &Result<std::path::PathBuf, String>,
    landlock: Option<u8>,
) -> Vec<(Level, String)> {
    use synthesis_platform::wsl::{WslVersion, conf_value, interop_registered};
    let mut v = vec![];
    let mut add = |level, text: String| v.push((level, text));
    let conf = read("/etc/wsl.conf").unwrap_or_default();
    // wsl.conf says what the next start will do; only the kernel says what is true now
    let conf_bool = |section: &str, key: &str| {
        conf_value(&conf, section, key).map(|x| !x.eq_ignore_ascii_case("false"))
    };
    match host.version {
        WslVersion::V2 => add(Level::Ok, "wsl: WSL2".into()),
        WslVersion::V1 => add(
            Level::Warn,
            "wsl: this is WSL1 (no bubblewrap, so no sandbox); convert it from PowerShell: `wsl --set-version <distro> 2`".into(),
        ),
        WslVersion::Unknown => add(
            Level::Warn,
            "wsl: WSL was detected but the kernel does not say whether it is WSL1 or WSL2 (custom kernel?); the sandbox needs WSL2 with bubblewrap".into(),
        ),
    }
    let (kernel, conf_on) = (interop_registered(read), conf_bool("interop", "enabled"));
    let how = format!(
        "/etc/wsl.conf: {}, running kernel: {}",
        conf_on.map_or("not set", |b| if b { "enabled" } else { "disabled" }),
        kernel.map_or("unknown", |b| if b { "enabled" } else { "disabled" })
    );
    let fix = "To disable it put `[interop]` `enabled=false` in /etc/wsl.conf, then run `wsl --shutdown` in PowerShell";
    match (kernel, conf_on) {
        (Some(false), _) => add(
            Level::Ok,
            format!("wsl: Windows interop is disabled ({how})"),
        ),
        (Some(true), _) | (None, Some(true)) => add(
            Level::Warn,
            format!(
                "wsl: Windows interop is enabled ({how}). Synthesis blocks it only inside its sandbox; your shell, user-level MCP servers and `!` commands can still start cmd.exe/powershell.exe. {fix}"
            ),
        ),
        (None, Some(false)) => add(
            Level::Ok,
            format!("wsl: Windows interop is disabled ({how})"),
        ),
        (None, None) => add(
            Level::Warn,
            format!(
                "wsl: cannot tell whether Windows interop is enabled ({how}); assume it is. {fix}"
            ),
        ),
    }
    let win_dirs = path_var.map_or(0, |p| {
        std::env::split_paths(p)
            .filter(|d| host.is_windows_path(d))
            .count()
    });
    if win_dirs > 0 || conf_bool("interop", "appendWindowsPath") == Some(true) {
        add(
            Level::Warn,
            format!(
                "wsl: PATH holds {win_dirs} Windows director{}. Sandboxed commands, hooks and MCP servers get them removed; to drop them everywhere put `[interop]` `appendWindowsPath=false` in /etc/wsl.conf, then run `wsl --shutdown`",
                if win_dirs == 1 { "y" } else { "ies" },
            ),
        );
    } else {
        add(Level::Ok, "wsl: no Windows directories in PATH".into());
    }
    let mounts = read("/proc/mounts").unwrap_or_default();
    let opts_of = |m: &Path| {
        mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let (_, mnt, _, opts) = (f.next()?, f.next()?, f.next()?, f.next()?);
                (Path::new(mnt) == m).then(|| opts.to_string())
            })
            .next()
    };
    let root = conf_value(&conf, "automount", "root").unwrap_or_else(|| "/mnt/".into());
    let conf_opts = conf_value(&conf, "automount", "options").unwrap_or_default();
    let metadata = conf_opts.split(',').any(|o| o.trim() == "metadata");
    let rw = host
        .windows_mounts
        .iter()
        .filter(|m| opts_of(m).is_some_and(|o| o.split(',').any(|x| x == "rw")))
        .count();
    add(
        Level::Ok, // information only: the sandbox hides or read-only-mounts the drives either way
        format!(
            "wsl: info: automount root {root}, options \"{conf_opts}\"{}",
            if metadata {
                ""
            } else {
                " (no `metadata`: Linux permissions on Windows drives are not stored; irrelevant to the sandbox)"
            }
        ),
    );
    if rw > 0 {
        add(
            Level::Warn,
            format!(
                "wsl: {rw} Windows drive(s) are mounted read-write (e.g. /mnt/c). The sandbox binds them read-only except the workspace; other processes can write them. To stop mounting drives put `[automount]` `enabled=false` in /etc/wsl.conf, then run `wsl --shutdown`"
            ),
        );
    } else {
        add(
            Level::Ok,
            "wsl: no Windows drive is mounted read-write".into(),
        );
    }
    match (bwrap, landlock) {
        (Ok(_), l) => add(
            Level::Ok,
            format!(
                "wsl: bubblewrap works (the only sandbox used on WSL; Landlock {} is not used: it cannot hide the interop socket)",
                l.map_or("unavailable".into(), |a| format!("ABI {a}"))
            ),
        ),
        (Err(e), l) => add(
            Level::Warn,
            format!(
                "wsl: bubblewrap does not work ({}); without it commands are refused (also in best-effort) unless --sandbox off. Install it: `sudo apt install bubblewrap`. Landlock {} is not used on WSL",
                san(e),
                l.map_or("unavailable".into(), |a| format!("ABI {a}"))
            ),
        ),
    }
    if exe.is_some_and(|e| host.is_windows_path(e)) {
        add(
            Level::Warn,
            "wsl: this `synt` binary lives on a Windows drive (/mnt/*), where Windows programs can replace it; install it under your Linux home".into(),
        );
    }
    v
}

/// Doctor rows for the folder `cwd`. Read-only: nothing is created, started or migrated.
pub fn rows(env: &Env, cwd: &Path, trusted: bool) -> Vec<Row> {
    let mut v: Vec<Row> = vec![];
    let mut add = |level, text: String| v.push(Row { level, text });
    add(
        Level::Ok,
        format!(
            "Synthesis {} on {} {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    );
    let h = match homes(env) {
        Ok(h) => h,
        Err(e) => {
            add(Level::Fail, format!("home: {}", san(&e)));
            return v;
        }
    };
    add(
        if h.synthesis.is_dir() {
            Level::Ok
        } else {
            Level::Warn
        },
        format!(
            "SYNT_HOME: {}{}",
            h.synthesis.display(),
            if h.synthesis.is_dir() {
                ""
            } else {
                " (does not exist yet; the first run creates it)"
            }
        ),
    );
    // providers: configuration, credentials (the source, never the key), model caches
    let trust = if trusted {
        ProjectTrust::Trusted
    } else {
        ProjectTrust::Untrusted
    };
    match load_config(env, &h, Some(cwd), ProjectTrust::Untrusted) {
        Err(e) => add(Level::Fail, format!("config: INVALID: {}", san(&e))),
        Ok(cfg) => {
            add(
                if cfg.providers.is_empty() {
                    Level::Warn
                } else {
                    Level::Ok
                },
                format!(
                    "config: {} provider(s), default model: {}",
                    cfg.providers.len(),
                    san(cfg.model.as_deref().unwrap_or("none"))
                ),
            );
            for w in &cfg.warnings {
                add(Level::Warn, format!("config: {}", san(w)));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            for (id, p) in &cfg.providers {
                let id = san(id);
                match resolve_credential(&id, Some(p), &h.synthesis, env) {
                    Some(c) => add(
                        Level::Ok,
                        format!("provider {id}: credential from {}", san(&c.source)),
                    ),
                    None if p.is_no_auth() => add(
                        Level::Ok,
                        format!("provider {id}: no authentication required"),
                    ),
                    None => add(
                        Level::Warn,
                        format!(
                            "provider {id}: no credential (SYNT_API_KEY, or an {{env:NAME}} reference in synt.json)"
                        ),
                    ),
                }
                if let Some(c) = crate::models::cache_of(&h.synthesis, &id, p) {
                    let days = now.saturating_sub(c.fetched_at) / 86_400;
                    add(
                        if days > 30 { Level::Warn } else { Level::Ok },
                        format!(
                            "models {id}: {} cached, refreshed {days} day(s) ago{}",
                            c.models.len(),
                            if days > 30 {
                                " (synt models --refresh)"
                            } else {
                                ""
                            }
                        ),
                    );
                }
            }
        }
    }
    // environment: names only
    let mut names: Vec<&str> = env
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("SYNT_"))
        .collect();
    names.sort_unstable();
    names.extend(
        STANDARD_VARS
            .iter()
            .copied()
            .filter(|k| env.get_nonempty(k).is_some()),
    );
    add(
        Level::Ok,
        format!(
            "environment (names only, values are never shown): {}",
            if names.is_empty() {
                "none set".into()
            } else {
                san(&names.join(", "))
            }
        ),
    );
    for l in legacy_notes(env, &h, cwd, std::env::current_exe().ok().as_deref()) {
        add(Level::Warn, san(&l));
    }
    // sandbox for tool commands
    let best = synthesis_sandbox::detect();
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let policy = synthesis_sandbox::SandboxPolicy::standard(&root, &std::env::temp_dir());
    match crate::run::sandbox_preflight(&*best, Some(&policy)) {
        Ok(()) => add(
            Level::Ok,
            format!(
                "sandbox: {} on {}, enforce ready",
                best.name(),
                std::env::consts::OS
            ),
        ),
        Err(e) => add(
            Level::Fail,
            format!(
                "sandbox: {} on {}, enforce NOT ready ({}); a run refuses to start unless --sandbox {}",
                best.name(),
                std::env::consts::OS,
                san(&e),
                if synthesis_sandbox::wsl::is_wsl() {
                    "off"
                } else {
                    "best-effort|off"
                }
            ),
        ),
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = synthesis_sandbox::helper_status() {
        // Not needed while bubblewrap works off WSL (Landlock is only the fallback there); on WSL
        // it installs the seccomp filter, so commands are refused without it.
        let needed = synthesis_sandbox::wsl::is_wsl() || synthesis_sandbox::bwrap_probe().is_err();
        add(
            if needed { Level::Warn } else { Level::Ok },
            format!(
                "sandbox helper {}: {} (group-writable and world-writable directories are refused; the Landlock backend and, on WSL, the seccomp filter need it; without it commands are refused under enforce{})",
                if needed {
                    "missing or untrusted"
                } else {
                    "info: missing or untrusted, not needed while bubblewrap works"
                },
                san(&e),
                if needed { "" } else { "; bubblewrap is in use" }
            ),
        );
    }
    if let Some(host) = synthesis_sandbox::wsl::host().as_ref() {
        #[cfg(target_os = "linux")]
        let landlock = synthesis_sandbox::landlock_abi();
        #[cfg(not(target_os = "linux"))]
        let landlock: Option<u8> = None;
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let exe = std::env::current_exe().ok();
        for (level, text) in wsl_rows(
            host,
            &read,
            env.get("PATH"),
            exe.as_deref(),
            synthesis_sandbox::bwrap_probe(),
            landlock,
        ) {
            add(level, text);
        }
    }
    // settings: hooks, refused env, loader diagnostics; MCP servers
    let st = load_settings(
        &LoadContext {
            cwd,
            synthesis_home: &h.synthesis,
            user_home: &h.user,
            env,
        },
        trust,
    );
    add(
        Level::Ok,
        format!(
            "hooks: {} event(s){}",
            st.hooks.len(),
            if st.disable_all_hooks {
                " (all disabled)"
            } else {
                ""
            }
        ),
    );
    if !st.env_refused.is_empty() {
        add(
            Level::Warn,
            format!(
                "settings env refused (secret-shaped names): {}",
                san(&st.env_refused.join(", "))
            ),
        );
    }
    for d in &st.diagnostics {
        add(Level::Warn, format!("settings: {}", san(d)));
    }
    if let Ok(cfg) = load_config(env, &h, Some(cwd), ProjectTrust::Untrusted) {
        let plan = crate::mcp::plan(&cfg, &st, &st, cwd, trust, &[]);
        add(
            Level::Ok,
            format!(
                "mcp: {} server(s) configured (doctor never starts them)",
                plan.servers.len()
            ),
        );
        for (n, why) in &plan.skipped {
            add(
                Level::Warn,
                format!("mcp {}: skipped, {}", san(n), san(why)),
            );
        }
    }
    // long-term memory, then the session store
    for l in crate::memcmd::doctor_lines(&h) {
        let level = if l.contains("UNUSABLE") {
            Level::Fail
        } else {
            Level::Ok
        };
        add(level, san(l.trim_start()));
    }
    let db = h.synthesis.join("synt.db");
    if db.is_file() {
        let size = std::fs::metadata(&db).map_or(0, |m| m.len());
        let check =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| {
                    c.busy_timeout(std::time::Duration::from_millis(1000))?;
                    c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                });
        match check {
            Ok(c) if c == "ok" => add(
                Level::Ok,
                format!("session store: synt.db {} KiB, integrity ok", size / 1024),
            ),
            Ok(c) => add(
                Level::Fail,
                format!(
                    "session store: synt.db integrity check failed ({})",
                    san(&c)
                ),
            ),
            Err(e) => add(
                Level::Fail,
                format!(
                    "session store: synt.db cannot be read ({})",
                    san(&e.to_string())
                ),
            ),
        }
    } else {
        add(
            Level::Ok,
            "session store: none yet (the first session creates synt.db)".into(),
        );
    }
    // outcome orchestration: opt-in, so doctor only reports; it never migrates or creates anything
    let schema = db
        .is_file()
        .then(|| {
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| {
                    c.busy_timeout(std::time::Duration::from_millis(1000))?;
                    c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                })
                .ok()
        })
        .flatten();
    let marker_ok = schema.is_none_or(|v| v < synthesis_session::SCHEMA_VERSION_OUTCOMES)
        || rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| {
                c.busy_timeout(std::time::Duration::from_millis(1000))?;
                c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM schema_migrations WHERE version = ?1)",
                    [synthesis_session::OUTCOME_DDL_MARKER],
                    |r| r.get::<_, bool>(0),
                )
            })
            .unwrap_or(false);
    let (level, text) = orchestrator_row(st.synthesis.orchestrator.enabled, schema, marker_ok);
    add(level, text);
    if st.synthesis.orchestrator.fail_closed {
        add(
            Level::Warn,
            "orchestrator: the managed settings are unusable, so outcome orchestration is switched off".into(),
        );
    }
    v
}

/// The `orchestrator:` line: the setting, the store's schema version, what each means together.
fn orchestrator_row(flag: bool, schema: Option<i64>, marker_ok: bool) -> (Level, String) {
    use synthesis_session::{SCHEMA_VERSION_OUTCOMES, SCHEMA_VERSION_SUPPORTED};
    let on = if flag { "on" } else { "off (default)" };
    match schema {
        // a developer database that enabled an earlier draft of v5 (no DDL revision marker)
        Some(n) if n >= SCHEMA_VERSION_OUTCOMES && n <= SCHEMA_VERSION_SUPPORTED && !marker_ok => (
            Level::Fail,
            "orchestrator: the outcome tables come from an earlier draft of the v5 schema and are refused; stop every surface, delete synt.db-wal and synt.db-shm, restore backups/pre-v5-*/synt.db, and enable again".into(),
        ),
        Some(n) if n > SCHEMA_VERSION_SUPPORTED => (
            Level::Fail,
            format!(
                "orchestrator: store schema v{n} is newer than this build supports (v{SCHEMA_VERSION_SUPPORTED}); update this binary, or restore backups/pre-v5-*/synt.db with the older one"
            ),
        ),
        Some(n) if n >= SCHEMA_VERSION_OUTCOMES && flag => (
            Level::Ok,
            format!("orchestrator: on, store schema v{n} (outcomes enabled; releases without outcomes refuse this store)"),
        ),
        Some(n) if n >= SCHEMA_VERSION_OUTCOMES => (
            Level::Ok,
            format!("orchestrator: {on}, store schema v{n} (outcomes were enabled earlier; releases without outcomes refuse this store)"),
        ),
        Some(n) if flag => (
            Level::Warn,
            format!("orchestrator: setting is on but the store is schema v{n}: the explicit enable step (backup, then migrate to v{SCHEMA_VERSION_OUTCOMES}) has not run, so outcome features stay off"),
        ),
        Some(n) => (
            Level::Ok,
            format!("orchestrator: {on}, store schema v{n} (a plain open never migrates; enabling is an explicit step with a backup)"),
        ),
        None if flag => (
            Level::Warn,
            "orchestrator: setting is on but there is no readable session store yet; outcome features stay off until the explicit enable step runs".into(),
        ),
        None => (Level::Ok, format!("orchestrator: {on}, no session store yet")),
    }
}

/// Hints for people coming from the earlier "Nemo" builds, and a PATH shadowing warning. Nothing
/// legacy is ever read: the old `~/.nemo` and `NEMO_*` are only detected (names, never values).
pub fn legacy_notes(
    env: &Env,
    h: &crate::Homes,
    project: &Path,
    current_exe: Option<&Path>,
) -> Vec<String> {
    let mut v = Vec::new();
    let mut old: Vec<&str> = env
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.to_ascii_uppercase().starts_with("NEMO_"))
        .collect();
    old.sort_unstable();
    if !old.is_empty() {
        v.push(format!(
            "warning: ignored legacy variables (Synthesis reads SYNT_* only): {}; rename each NEMO_X to SYNT_X",
            old.join(", ")
        ));
    }
    let mut renamed: Vec<&str> = env
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("SYNTHESIS_"))
        .collect();
    renamed.sort_unstable();
    for k in renamed {
        // Name only, never the value, and the variable is not honoured.
        v.push(format!(
            "warning: ignored legacy variable {k} (renamed 2026-11-02): set SYNT_{} instead",
            &k["SYNTHESIS_".len()..]
        ));
    }
    // Renamed 2026-11-11 (the terminal app is the CLI, `tui` is the desktop tab): never read, never moved.
    let mut cli_env: Vec<&str> = env
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("SYNT_TUI_"))
        .collect();
    cli_env.sort_unstable();
    for k in cli_env {
        v.push(format!(
            "warning: ignored legacy variable {k} (renamed 2026-11-11): set SYNT_CLI_{} instead",
            &k["SYNT_TUI_".len()..]
        ));
    }
    // `synt.tui` now belongs to the desktop TUI tab; the terminal app reads `synt.cli`. Only keys the
    // desktop never shows can only have come from the terminal app.
    let user_settings: Option<serde_json::Value> = std::fs::read(h.synthesis.join("settings.json"))
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(old) = user_settings
        .as_ref()
        .and_then(|j| j.get("synt"))
        .filter(|s| s.get("cli").is_none())
        .and_then(|s| s.get("tui"))
        .and_then(|t| t.as_object())
    {
        let keys: Vec<&str> = TERMINAL_ONLY_KEYS
            .into_iter()
            .filter(|k| old.contains_key(*k))
            .collect();
        if !keys.is_empty() {
            v.push(format!(
                "note: synt.tui.{} in {} is ignored by the terminal app (it reads synt.cli; synt.tui is the desktop TUI tab). To migrate, move these keys (or the whole \"tui\" object) to \"cli\" under \"synt\"",
                keys.join(", synt.tui."),
                h.synthesis.join("settings.json").display()
            ));
        }
    }
    // Behaviour and budgets are shared by every surface under `synt.orchestrator`; a copy under a
    // surface's rendering section is never read.
    for section in ["cli", "tui"] {
        if let Some(o) = user_settings
            .as_ref()
            .and_then(|j| j.get("synt"))
            .and_then(|s| s.get(section))
            .and_then(|t| t.as_object())
        {
            let keys: Vec<&str> = std::iter::once("orchestrator")
                .chain(
                    synthesis_settings::ORCHESTRATOR_KEYS
                        .into_iter()
                        .filter(|k| !matches!(*k, "enabled" | "attempt" | "version")),
                )
                .filter(|k| o.contains_key(*k))
                .collect();
            if !keys.is_empty() {
                v.push(format!(
                    "note: synt.{section}.{} in {} is ignored (orchestrator settings are shared by every surface and live under synt.orchestrator, in the user or managed settings file)",
                    keys.join(&format!(", synt.{section}.")),
                    h.synthesis.join("settings.json").display()
                ));
            }
        }
    }
    let old_home = h.user.join(".nemo");
    if old_home.is_dir() && !h.synthesis.exists() {
        v.push(format!(
            "note: {} exists and {} does not. Synthesis never reads the old location. To migrate, review and run:\n  cp -R {} {}   (Windows: Copy-Item -Recurse)\nthen inside the copy rename nemo.json to synt.json and NEMO.md to AGENTS.md, the \"nemo\" key of settings.json to \"synt\", and in each project .nemo to .synt",
            old_home.display(),
            h.synthesis.display(),
            old_home.display(),
            h.synthesis.display()
        ));
    }
    // Names of the config folder before 2026-11-02 (`.synthesis`, `synthesis.json`, `synthesis.db`): detected, never read or moved.
    let old_dir = h.user.join(".synthesis");
    if old_dir.is_dir() && !h.synthesis.exists() {
        v.push(format!(
            "note: {} exists and {} does not. Synthesis never reads the old location. To migrate: mv {} {}   (Windows: Move-Item)",
            old_dir.display(),
            h.synthesis.display(),
            old_dir.display(),
            h.synthesis.display()
        ));
    }
    for (dir, old, new) in [
        (h.synthesis.as_path(), "synthesis.json", "synt.json"),
        (h.synthesis.as_path(), "synthesis.db", "synt.db"),
        (h.synthesis.as_path(), "SYNTHESIS.md", "AGENTS.md"),
        (project, "SYNTHESIS.md", "AGENTS.md"),
        (project, "synthesis.json", "synt.json"),
    ] {
        if dir.join(old).is_file() {
            v.push(format!(
                "note: {} is ignored (the file is now {new}). To migrate: mv {} {}",
                dir.join(old).display(),
                dir.join(old).display(),
                dir.join(new).display()
            ));
        }
    }
    if project.join(".synthesis").is_dir() && !project.join(".synt").exists() {
        v.push(format!(
            "note: {} is ignored (the project folder is now .synt). To migrate: mv {} {}",
            project.join(".synthesis").display(),
            project.join(".synthesis").display(),
            project.join(".synt").display()
        ));
    }
    let first = env.get_nonempty("PATH").and_then(|p| {
        std::env::split_paths(p)
            .flat_map(|d| [d.join("synt"), d.join("synt.exe")])
            .find(|c| c.is_file())
    });
    if let (Some(first), Some(me)) = (first, current_exe)
        && std::fs::canonicalize(&first).ok() != std::fs::canonicalize(me).ok()
    {
        v.push(format!(
            "warning: `synt` on PATH resolves to {}, not this executable ({})",
            first.display(),
            me.display()
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl_text(rows: &[(Level, String)]) -> String {
        rows.iter().map(|(l, t)| format!("{l:?} {t}\n")).collect()
    }

    #[test]
    fn wsl_interop_state_comes_from_the_kernel_not_a_default() {
        use synthesis_platform::wsl::{WslHost, WslVersion};
        let host = WslHost {
            version: WslVersion::Unknown,
            windows_mounts: vec![],
            mounts_known: true,
        };
        let run = |files: &'static [(&'static str, &'static str)]| {
            let read = move |p: &str| {
                files
                    .iter()
                    .find(|(k, _)| *k == p)
                    .map(|(_, v)| v.to_string())
            };
            wsl_text(&wsl_rows(
                &host,
                &read,
                None,
                None,
                &Ok("/usr/bin/bwrap".into()),
                None,
            ))
        };
        // nothing readable: not "enabled by default", but unknown
        let t = run(&[]);
        assert!(
            t.contains("cannot tell whether Windows interop is enabled"),
            "{t}"
        );
        assert!(t.contains("custom kernel"), "{t}");
        // the kernel says disabled, whatever wsl.conf says; a trailing comment is parsed
        let t = run(&[
            ("/proc/sys/fs/binfmt_misc/status", "enabled"),
            ("/etc/wsl.conf", "[interop]\nenabled=true\n"),
        ]);
        assert!(t.contains("Ok wsl: Windows interop is disabled"), "{t}");
        let t = run(&[("/etc/wsl.conf", "[interop]\nenabled=false # no\n")]);
        assert!(t.contains("Ok wsl: Windows interop is disabled"), "{t}");
    }

    #[test]
    fn wsl_rows_warn_with_remedies_and_never_print_values() {
        use synthesis_platform::wsl::{WslHost, WslVersion};
        let host = WslHost {
            version: WslVersion::V1,
            windows_mounts: vec!["/mnt/c".into()],
            mounts_known: true,
        };
        let files = |p: &str| match p {
            "/proc/sys/fs/binfmt_misc/WSLInterop" => Some("enabled\n".to_string()),
            "/proc/mounts" => Some("C: /mnt/c drvfs rw,noatime 0 0\n".to_string()),
            _ => None,
        };
        // joined with the host's separator (`;` on Windows), like the real PATH
        let path = std::env::join_paths(["/usr/bin", "/mnt/c/SECRET-DIR/bin"]).unwrap();
        let path = path.to_str().unwrap();
        let r = wsl_rows(
            &host,
            &files,
            Some(path),
            Some(Path::new("/mnt/c/tools/synt")),
            &Err("bubblewrap not found".into()),
            Some(5),
        );
        let t = wsl_text(&r);
        assert!(r.iter().all(|(l, _)| *l != Level::Fail), "{t}");
        for need in [
            "WSL1",
            "wsl --set-version",
            "interop is enabled",
            "enabled=false",
            "1 Windows directory",
            "appendWindowsPath=false",
            "info: automount",
            "read-write",
            "bubblewrap does not work",
            "apt install bubblewrap",
            "lives on a Windows drive",
        ] {
            assert!(t.contains(need), "{need} in {t}");
        }
        assert!(!t.contains("SECRET-DIR"), "{t}");
        // a hardened host has no warning
        let conf =
            "[interop]\nenabled=false\nappendWindowsPath=false\n[automount]\noptions=metadata\n";
        let good = |p: &str| match p {
            "/etc/wsl.conf" => Some(conf.to_string()),
            "/proc/sys/fs/binfmt_misc/status" => Some("enabled".to_string()),
            _ => None,
        };
        let host2 = WslHost {
            version: WslVersion::V2,
            windows_mounts: vec![],
            mounts_known: true,
        };
        let r = wsl_rows(
            &host2,
            &good,
            Some("/usr/bin"),
            None,
            &Ok("/usr/bin/bwrap".into()),
            None,
        );
        assert!(r.iter().all(|(l, _)| *l == Level::Ok), "{}", wsl_text(&r));
    }

    #[test]
    fn legacy_notes_detect_old_home_vars_and_path_shadowing() {
        let d = tempfile::tempdir().unwrap();
        let (user, bin) = (d.path().join("u"), d.path().join("bin"));
        std::fs::create_dir_all(user.join(".nemo")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("synt"), "").unwrap();
        let h = crate::Homes {
            synthesis: user.join(".synt"),
            user,
        };
        let path = bin.to_str().unwrap();
        let env = Env::from_pairs([("NEMO_API_KEY", "secret-value"), ("PATH", path)]);
        let v = legacy_notes(&env, &h, d.path(), Some(&d.path().join("other"))).join("\n");
        assert!(
            v.contains("NEMO_API_KEY") && !v.contains("secret-value"),
            "{v}"
        );
        assert!(v.contains("cp -R") && v.contains("resolves to"), "{v}");
        // this executable is the first on PATH, the new home exists, no legacy variables: silent
        std::fs::create_dir_all(&h.synthesis).unwrap();
        let env = Env::from_pairs([("PATH", path)]);
        assert!(legacy_notes(&env, &h, d.path(), Some(&bin.join("synt"))).is_empty());
    }

    #[test]
    fn legacy_notes_detect_the_tui_to_cli_rename_names_only() {
        let d = tempfile::tempdir().unwrap();
        let user = d.path().join("u");
        let h = crate::Homes {
            synthesis: user.join(".synt"),
            user,
        };
        std::fs::create_dir_all(&h.synthesis).unwrap();
        let file = h.synthesis.join("settings.json");
        std::fs::write(
            &file,
            r#"{"synt":{"tui":{"mouse_capture":false,"motion":false}}}"#,
        )
        .unwrap();
        let env = Env::from_pairs([("PATH", ""), ("SYNT_TUI_X", "secret-value")]);
        let v = legacy_notes(&env, &h, d.path(), None).join("\n");
        assert!(
            v.contains("SYNT_CLI_X") && !v.contains("secret-value"),
            "{v}"
        );
        assert!(
            v.contains("synt.tui.mouse_capture") && !v.contains("synt.tui.motion"),
            "{v}"
        );
        // migrated, or only desktop keys: silent
        std::fs::write(
            &file,
            r#"{"synt":{"tui":{"mouse_capture":false},"cli":{}}}"#,
        )
        .unwrap();
        assert!(legacy_notes(&Env::from_pairs([("PATH", "")]), &h, d.path(), None).is_empty());
        std::fs::write(&file, r#"{"synt":{"tui":{"opacity":80}}}"#).unwrap();
        assert!(legacy_notes(&Env::from_pairs([("PATH", "")]), &h, d.path(), None).is_empty());
    }

    #[test]
    fn legacy_notes_detect_pre_synt_config_names_without_touching_them() {
        let d = tempfile::tempdir().unwrap();
        let (user, proj) = (d.path().join("u"), d.path().join("p"));
        std::fs::create_dir_all(user.join(".synthesis")).unwrap();
        std::fs::create_dir_all(proj.join(".synthesis")).unwrap();
        let h = crate::Homes {
            synthesis: user.join(".synt"),
            user: user.clone(),
        };
        let env = Env::from_pairs([("PATH", ""), ("SYNTHESIS_HOME", "/secret/value")]);
        let v = legacy_notes(&env, &h, &proj, None).join("\n");
        assert!(v.contains("mv ") && v.contains(".synt"), "{v}");
        assert_eq!(v.matches("To migrate").count(), 2, "{v}");
        assert!(
            v.contains("SYNTHESIS_HOME") && v.contains("SYNT_HOME"),
            "{v}"
        );
        assert!(!v.contains("/secret/value"), "{v}");
        // inside a new-style home the old file names are flagged too
        std::fs::create_dir_all(&h.synthesis).unwrap();
        std::fs::write(h.synthesis.join("synthesis.json"), "{}").unwrap();
        std::fs::write(proj.join("SYNTHESIS.md"), "x").unwrap();
        std::fs::write(proj.join("synthesis.json"), "{}").unwrap();
        let v = legacy_notes(&env, &h, &proj, None).join("\n");
        assert!(v.contains("synthesis.json is ignored"), "{v}");
        assert!(
            v.contains(&proj.join("synthesis.json").display().to_string())
                && v.contains(&proj.join("synt.json").display().to_string()),
            "{v}"
        );
        assert!(
            v.contains("SYNTHESIS.md is ignored") && v.contains("AGENTS.md"),
            "{v}"
        );
        assert!(user.join(".synthesis").is_dir() && proj.join(".synthesis").is_dir());
    }

    #[test]
    fn a_slow_check_is_given_up_on() {
        use std::time::Duration;
        let slow = capped(
            || std::thread::sleep(Duration::from_millis(500)),
            Duration::from_millis(20),
        );
        assert!(slow.unwrap_err().contains("timed out"));
        assert_eq!(capped(|| 7, Duration::from_secs(5)), Ok(7));
    }

    #[test]
    fn rows_name_variables_but_never_show_a_value() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("u");
        std::fs::create_dir_all(home.join(".nemo")).unwrap();
        let env = Env::from_pairs([
            ("HOME", home.to_str().unwrap()),
            ("SYNT_API_KEY", "sk-live-VALUE-1"),
            ("NEMO_API_KEY", "nemo-VALUE-2"),
            ("SYNTHESIS_TOKEN", "old-VALUE-3"),
            ("HTTPS_PROXY", "http://user:pass-VALUE-4@proxy:8080"),
            ("SSL_CERT_FILE", "/etc/VALUE-5.pem"),
            ("PATH", ""),
        ]);
        let r = rows(&env, d.path(), false);
        let all = r
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("VALUE"), "{all}");
        for name in [
            "SYNT_API_KEY",
            "HTTPS_PROXY",
            "SSL_CERT_FILE",
            "NEMO_API_KEY",
            "SYNTHESIS_TOKEN",
        ] {
            assert!(all.contains(name), "{name} missing in\n{all}");
        }
        assert!(
            r.iter()
                .any(|r| r.level == Level::Warn && r.text.contains("NEMO_API_KEY"))
        );
        assert!(all.contains("cp -R"), "{all}");
        assert!(summary(&r).contains("warn"));
    }

    #[test]
    fn orchestrator_row_says_what_flag_and_schema_mean_together() {
        let r = |flag, v| orchestrator_row(flag, v, true);
        let (l, t) = r(false, Some(4));
        assert!(
            matches!(l, Level::Ok) && t.contains("off (default)") && t.contains("v4"),
            "{t}"
        );
        assert!(t.contains("never migrates"), "{t}");
        let (l, t) = r(true, Some(4));
        assert!(
            matches!(l, Level::Warn) && t.contains("outcome features stay off"),
            "{t}"
        );
        let (l, t) = r(true, Some(5));
        assert!(
            matches!(l, Level::Ok) && t.starts_with("orchestrator: on"),
            "{t}"
        );
        let (l, t) = r(false, Some(5));
        assert!(
            matches!(l, Level::Ok) && t.contains("enabled earlier"),
            "{t}"
        );
        let (l, t) = r(false, Some(99));
        assert!(
            matches!(l, Level::Fail) && t.contains("newer than this build"),
            "{t}"
        );
        let (l, _) = r(true, None);
        assert!(matches!(l, Level::Warn));
        let (l, t) = orchestrator_row(true, Some(5), false);
        assert!(
            matches!(l, Level::Fail) && t.contains("earlier draft") && t.contains("pre-v5"),
            "{t}"
        );
        let (l, _) = orchestrator_row(false, Some(4), false);
        assert!(
            matches!(l, Level::Ok),
            "v4 has no outcome tables to be outdated"
        );
        let (l, t) = r(false, None);
        assert!(
            matches!(l, Level::Ok) && t.contains("no session store"),
            "{t}"
        );
    }

    #[test]
    fn doctor_reports_orchestrator_keys_left_under_a_surface_section_names_only() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"synt":{"cli":{"orchestrator":{"enabled":true},"max_cost_usd":9999},"tui":{"verbose":true}}}"#,
        )
        .unwrap();
        let h = crate::Homes {
            user: d.path().to_path_buf(),
            synthesis: home,
        };
        let notes = legacy_notes(&Env::default(), &h, d.path(), None);
        let n = notes
            .iter()
            .find(|n| n.contains("synt.cli."))
            .expect("a note");
        assert!(
            n.contains("synt.cli.orchestrator, synt.cli.max_cost_usd"),
            "{n}"
        );
        assert!(n.contains("synt.orchestrator"), "{n}");
        assert!(!n.contains("9999"), "values never shown: {n}");
        assert!(!notes.iter().any(|n| n.contains("synt.tui.")), "{notes:?}");
    }
}
