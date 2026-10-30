//! Cross-session peers wiring (messaging.md Part B): settings, run dir, key, endpoint, registry
//! entry. Off unless `synthesis.peers.enabled` in USER settings or `--peers`.

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
    let mut config = Config::from_settings(peers, &ws.root.to_string_lossy());
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
        return off("SYNTHESIS_HOME must be under your user profile on Windows".into());
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
    Ok(Some(Started { link, listener }))
}

/// One `synth doctor` block.
pub fn doctor_lines(peers: &Map<String, Value>, home: &Path, env: &Env) -> Vec<String> {
    let c = Config::from_settings(peers, "<workspace>");
    let mut v = vec![format!(
        "peers (cross-session messaging, `synthesis.peers.enabled` in user settings or --peers): {}",
        if c.enabled { "enabled" } else { "off" }
    )];
    if !c.enabled {
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
            "no (Windows: SYNTHESIS_HOME must be under the user profile)"
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
                    "too long (over {} bytes): peers stay off; use a shorter SYNTHESIS_HOME",
                    synthesis_peers::net::MAX_SOCKET_PATH
                )
            }
        ));
    }
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
    fn names() {
        for ok in ["a", "backend", "api-2", "x_y", "0day"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a b", "a/b", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
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
