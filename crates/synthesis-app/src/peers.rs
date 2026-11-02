//! Cross-session peers wiring (messaging.md Part B): settings, run dir, key, endpoint, registry
//! entry. Off unless `synt.peers.enabled` in USER settings or `--peers`.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};
use synthesis_config::Env;
use synthesis_core::AgentSession;
use synthesis_harness::PeerLink;
use synthesis_peers::net::Listener;
use synthesis_peers::{
    Config, Entry, Registration, load_or_create_key, name_taken, platform_ok, read_entries,
    remove_entry, run_dir,
};

/// A started endpoint: the link for the harness and the listener for its server.
pub struct Started {
    pub link: Arc<PeerLink>,
    pub listener: Listener,
}

/// `[a-z0-9][a-z0-9_-]{0,31}`
pub fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    n.len() <= 32
        && c.next()
            .is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_' || x == '-')
}

/// What the session is for, shown to peers: `--about`, else the first non-empty line of
/// `AGENTS.md` at the workspace root (a regular file, first 4 KiB). Redacted, control
/// characters removed, at most 120 characters.
pub fn about_of(
    flag: Option<&str>,
    root: &Path,
    redact: &dyn Fn(&str) -> String,
) -> Option<String> {
    let text = match flag {
        Some(f) => f.to_string(),
        None => {
            use std::io::Read;
            let p = root.join("AGENTS.md");
            if !std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
                return None;
            }
            let mut buf = Vec::new();
            std::fs::File::open(&p)
                .ok()?
                .take(4096)
                .read_to_end(&mut buf)
                .ok()?;
            String::from_utf8_lossy(&buf)
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| !l.is_empty())?
                .to_string()
        }
    };
    let line: String = redact(&text)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line: String = line.trim().chars().take(120).collect();
    (!line.is_empty()).then_some(line)
}

/// True when no live process holds the session's writer lock (B4 Stale). A missing lock file
/// (`--no-session-store`) proves nothing, so it counts as alive.
pub fn session_lock_free(home: &Path, session_id: &str) -> bool {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return false;
    }
    std::fs::File::open(home.join("sessions").join(session_id).join("lock"))
        .is_ok_and(|f| f.try_lock().is_ok())
}

/// Starts the session's peer endpoint when enabled. Must run inside the tokio runtime context.
/// A bad or taken `--name` is an error; any other failure turns peers off for this session with
/// a notice.
#[allow(clippy::too_many_arguments)]
pub fn setup(
    force_on: bool,
    name: Option<&str>,
    about: Option<String>,
    peers: &Map<String, Value>,
    home: &Path,
    env: &Env,
    session: &AgentSession,
    notices: &mut Vec<String>,
) -> Result<Option<Started>, String> {
    if let Some(n) = name
        && !valid_name(n)
    {
        return Err(format!(
            "--name {n:?}: use 1-32 characters of [a-z0-9_-], starting with a letter or digit"
        ));
    }
    let ws = &session.workspace;
    let mut config =
        Config::from_settings(peers, &ws.root.to_string_lossy()).with_repo(&ws.repo_key);
    config.enabled |= force_on;
    if !config.enabled {
        return Ok(None);
    }
    let mut off = |why: String| {
        notices.push(format!("peers are off for this session: {why}"));
        Ok(None)
    };
    let profile = env
        .get_nonempty("USERPROFILE")
        .map(std::path::PathBuf::from);
    if !platform_ok(home, profile.as_deref()) {
        return off("SYNT_HOME must be under your user profile on Windows".into());
    }
    let run = run_dir(home);
    let key = match load_or_create_key(&run) {
        Ok(k) => k,
        Err(e) => return off(format!("cannot prepare {}: {e}", "the run directory")),
    };
    let lock_free = {
        let home = home.to_path_buf();
        Arc::new(move |e: &Entry| session_lock_free(&home, &e.session_id))
    };
    if let Some(n) = name {
        let mut live = read_entries(&run);
        live.retain(|e| {
            let dead = lock_free(e);
            if dead {
                remove_entry(&run, &e.session_id);
            }
            !dead
        });
        if name_taken(&live, &ws.workspace_key, n) {
            return Err(format!(
                "--name {n}: a live session in this workspace already has that name"
            ));
        }
    }
    let listener = match Listener::bind(&run) {
        Ok(l) => l,
        Err(e) => return off(format!("cannot open the peer endpoint ({e})")),
    };
    let entry = Entry {
        v: 1,
        session_id: session.id.clone(),
        session_name: name.map(String::from),
        workspace_key: ws.workspace_key.clone(),
        workspace_label: ws.workspace_label.clone(),
        workspace_root: ws.root.to_string_lossy().into_owned(),
        cwd: ws.cwd.to_string_lossy().into_owned(),
        pid: std::process::id(),
        pid_start: None,
        endpoint: listener.endpoint(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        repo_key: ws.repo_key.clone(),
        about,
    };
    let reg = match Registration::create(&run, &entry) {
        Ok(r) => r,
        Err(e) => return off(format!("cannot write the session registry entry ({e})")),
    };
    let link = PeerLink::new(
        config,
        key,
        run,
        entry,
        session.clone(),
        lock_free,
        listener.kind(),
    );
    link.attach(reg);
    if let Err(e) = link.announce() {
        notices.push(format!(
            "peers: this session cannot be queued for while offline ({e})"
        ));
    }
    Ok(Some(Started { link, listener }))
}

/// One `synt doctor` block.
pub fn doctor_lines(
    peers: &Map<String, Value>,
    home: &Path,
    env: &Env,
    sandbox_enforcing: bool,
) -> Vec<String> {
    let c = Config::from_settings(peers, "<workspace>");
    let mut v = vec![format!(
        "peers (cross-session messaging, `synt.peers.enabled` in user settings or --peers): {}",
        if c.enabled { "enabled" } else { "off" }
    )];
    if !c.enabled {
        v.push(
            "  hint: turn on with `synt.peers.enabled: true` in user settings.json or --peers; then set accept_delegation, accept_notify and share_commands (docs/CLI.md)"
                .into(),
        );
        return v;
    }
    let profile = env
        .get_nonempty("USERPROFILE")
        .map(std::path::PathBuf::from);
    v.push(format!(
        "  platform ok: {}",
        if platform_ok(home, profile.as_deref()) {
            "yes"
        } else {
            "no (Windows: SYNT_HOME must be under the user profile)"
        }
    ));
    #[cfg(unix)]
    {
        let probe = run_dir(home).join("0123456789abcdef.sock");
        let fits = probe.as_os_str().len() <= synthesis_peers::net::MAX_SOCKET_PATH;
        v.push(format!(
            "  socket path: {}",
            if fits {
                "fits".to_string()
            } else {
                format!(
                    "too long (over {} bytes): peers stay off; use a shorter SYNT_HOME",
                    synthesis_peers::net::MAX_SOCKET_PATH
                )
            }
        ));
    }
    if !sandbox_enforcing {
        v.push(
            "  warning: the sandbox is not enforcing here; with peers on, commands the agent runs could read the peer key (the run directory is only denied inside the sandbox)"
                .into(),
        );
    }
    v.push(
        "  note: a session answers peers only when its model has a price (`synt.pricing` in user settings.json); otherwise it declines with no_price"
            .into(),
    );
    v.push(format!(
        "  accept_delegation: {} (delegation_allow: {} rule(s), max hops {}); a delegated task runs in this session's own permission mode, capped by the sender's",
        if c.accept_delegation { "ON" } else { "off" },
        c.delegation_allow.len(),
        c.delegation_max_hops
    ));
    v.extend(
        c.delegation_warnings()
            .into_iter()
            .map(|w| format!("  warning: {w}")),
    );
    v.push(format!(
        "  accept_notify: {} (notify_on_end results from peers)",
        if c.accept_notify { "ON" } else { "off" }
    ));
    v.push(format!(
        "  share_commands: {} command(s) peers may ask this session to run",
        c.share_commands.len()
    ));
    v.push(format!(
        "  allow: {} rule(s), deny: {} rule(s), initiate {}, answer {}, max_hops {}, max_cost_usd {}",
        c.allow.len(),
        c.deny.len(),
        c.initiate,
        c.answer,
        c.max_hops,
        c.max_cost_usd
    ));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_names_the_peer_settings_and_hints_when_off() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::default();
        let off = doctor_lines(&Map::new(), home.path(), &env, true).join("\n");
        assert!(
            off.contains("off") && off.contains("accept_delegation"),
            "{off}"
        );
        let on: Map<String, Value> = serde_json::from_str(
            r#"{"enabled":true,"accept_notify":true,"share_commands":["cargo test"]}"#,
        )
        .unwrap();
        let on = doctor_lines(&on, home.path(), &env, true).join("\n");
        for want in [
            "accept_delegation: off",
            "accept_notify: ON",
            "share_commands: 1 command(s)",
        ] {
            assert!(on.contains(want), "{want}\n{on}");
        }
    }

    #[test]
    fn names() {
        for ok in ["a", "backend", "api-2", "x_y", "0day"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a b", "a/b", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn about_comes_from_the_flag_or_the_first_line_of_synthesis_md() {
        let t = tempfile::tempdir().unwrap();
        let red = |s: &str| s.replace("sk-SECRET", "[REDACTED]");
        assert_eq!(about_of(None, t.path(), &red), None);
        std::fs::write(
            t.path().join("AGENTS.md"),
            "\n# Billing API sk-SECRET\nmore\n",
        )
        .unwrap();
        assert_eq!(
            about_of(None, t.path(), &red).as_deref(),
            Some("Billing API [REDACTED]")
        );
        assert_eq!(
            about_of(Some("flag\u{7}x"), t.path(), &red).as_deref(),
            Some("flag x")
        );
        let long = "x".repeat(300);
        assert_eq!(about_of(Some(&long), t.path(), &red).unwrap().len(), 120);
        assert_eq!(about_of(Some("  "), t.path(), &red), None);
    }

    #[test]
    fn lock_probe_needs_a_lock_file_and_a_safe_id() {
        let t = tempfile::tempdir().unwrap();
        assert!(
            !session_lock_free(t.path(), "nope"),
            "no lock file proves nothing"
        );
        assert!(!session_lock_free(t.path(), "../x"));
        let d = t.path().join("sessions").join("s1");
        std::fs::create_dir_all(&d).unwrap();
        let f = std::fs::File::create(d.join("lock")).unwrap();
        assert!(
            session_lock_free(t.path(), "s1"),
            "unheld lock: the holder is gone"
        );
        f.lock().unwrap();
        assert!(!session_lock_free(t.path(), "s1"), "held lock: alive");
    }
}
