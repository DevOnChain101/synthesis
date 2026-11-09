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
                "sandbox: {} on {}, enforce NOT ready ({}); a run refuses to start unless --sandbox best-effort|off",
                best.name(),
                std::env::consts::OS,
                san(&e)
            ),
        ),
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
    v
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
}
