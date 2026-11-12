//! `synt stop SESSION`, `synt sessions show|verify`, and the poller a running `synt run` uses to
//! see a stop request (docs/UNATTENDED.md).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use synthesis_config::Env;
use synthesis_events::Record;
use synthesis_harness::sanitize_for_display;
use synthesis_session::{Reader, Store, StoreOptions};
use tokio_util::sync::CancellationToken;

use crate::homes;

/// Seconds between the graceful cancel and the hard kill after a stop request.
pub const STOP_GRACE_SECS: u64 = 15;

#[derive(clap::Args)]
pub struct StopArgs {
    /// Session id (from the `synt: session ...` line or `session.started`).
    pub session: String,
}

#[derive(clap::Subcommand)]
pub enum SessionsCmd {
    /// Timeline and budget use of a stored session.
    Show {
        session: String,
        /// Every event, one line each (streaming deltas left out).
        #[arg(long)]
        events: bool,
        /// The stored verification reports, in full.
        #[arg(long)]
        report: bool,
    },
    /// Recompute the event hash chain; exit 1 when an event was changed or removed.
    Verify { session: String },
}

/// Asks the process running SESSION to stop: it cancels gracefully, then is killed after
/// [`STOP_GRACE_SECS`]. Works across processes through the session store.
pub fn stop_cmd(env: &Env, a: StopArgs) -> Result<u8, String> {
    let h = homes(env)?;
    let store = Store::open(&h.synthesis, StoreOptions::default()).map_err(|e| e.to_string())?;
    if !store.request_stop(&a.session).map_err(|e| e.to_string())? {
        return Err(format!("no session {}", sanitize_for_display(&a.session)));
    }
    out!(
        "stop requested for {}; the running process cancels within a second and is killed after {STOP_GRACE_SECS}s if it does not end",
        sanitize_for_display(&a.session)
    );
    Ok(0)
}

pub fn run_cmd(env: &Env, cmd: SessionsCmd) -> Result<u8, String> {
    let h = homes(env)?;
    let r = Reader::open(&h.synthesis).map_err(|e| e.to_string())?;
    match cmd {
        SessionsCmd::Verify { session } => {
            let rep = r.verify_chain(&session).map_err(|e| e.to_string())?;
            match rep.broken {
                None => {
                    out!(
                        "ok: {} event(s) verified, {} from before the chain existed (not covered)",
                        rep.chained,
                        rep.unchained
                    );
                    out!(
                        "limit: this detects edits by anything that cannot also rewrite the session row; a same-user process outside the sandbox can rewrite both"
                    );
                    Ok(0)
                }
                Some(b) => {
                    out!("TAMPERED at event {}: {}", b.seq, b.why);
                    Ok(1)
                }
            }
        }
        SessionsCmd::Show {
            session,
            events,
            report,
        } => show(&r, &session, events, report),
    }
}

fn short(v: &Value, n: usize) -> String {
    let s = v.as_str().map_or_else(|| v.to_string(), String::from);
    sanitize_for_display(&s.chars().take(n).collect::<String>()).replace('\n', " ")
}

fn show(r: &Reader, id: &str, events: bool, report: bool) -> Result<u8, String> {
    let e = |x: synthesis_session::Error| x.to_string();
    let s = r.get_session(id).map_err(e)?;
    out!(
        "session {}  status {:?}  model {}",
        s.id,
        s.status,
        s.model_config_id
    );
    out!("workspace {}", sanitize_for_display(&s.cwd));
    if let Some(w) = &s.git_worktree {
        out!("worktree {}", sanitize_for_display(w));
    }
    let (b, u) = (&s.budget, &s.usage);
    out!(
        "budget use: turns {}/{}  tool calls {}/{}  cost ${:.4}{}  wall {}s{}  verify attempts {}/{}{}",
        u.turns,
        b.max_turns,
        u.tool_calls,
        b.max_tool_calls,
        u.cost_usd,
        b.max_cost_usd.map_or(String::new(), |c| format!("/{c}")),
        u.wall_ms / 1000,
        b.max_wall_ms
            .map_or(String::new(), |m| format!("/{}", m / 1000)),
        u.verify_attempts,
        b.max_verify_attempts,
        b.max_tokens_total
            .map_or(String::new(), |t| format!("  token cap {t}")),
    );
    for (tid, status, reason, request) in r.tasks(id).map_err(e)? {
        out!(
            "task {tid}  {status}{}  {}",
            reason.map_or(String::new(), |x| format!(
                " ({})",
                short(&Value::from(x), 80)
            )),
            short(&Value::from(request), 80)
        );
        if report {
            for (attempt, status, rep) in r.verification_reports(&tid).map_err(e)? {
                out!("  verification attempt {attempt}: {status}");
                out!("{}", serde_json::to_string_pretty(&rep).unwrap_or_default());
            }
        }
    }
    if events {
        let mut from = 0;
        loop {
            let page = r.read_events(id, from, 500).map_err(e)?;
            if page.is_empty() {
                break;
            }
            for rec in page {
                let (seq, v) = match rec {
                    Record::Known(env) => (
                        env.seq,
                        serde_json::to_value(&env.event).unwrap_or_default(),
                    ),
                    Record::Unknown(raw) => (raw.seq, serde_json::json!({"type": raw.kind})),
                };
                from = seq;
                let kind = v["type"].as_str().unwrap_or("?");
                if kind == "model.delta" {
                    continue;
                }
                let p = &v["payload"];
                let detail = [
                    "name", "status", "reason", "summary", "key", "text", "message",
                ]
                .iter()
                .find_map(|k| p.get(k))
                .map(|d| short(d, 100))
                .unwrap_or_default();
                out!("{seq:>5}  {kind:<28} {detail}");
            }
        }
    }
    Ok(0)
}

/// Polls the store once a second; on a stop request cancels `cancel`, and after the grace period
/// calls `hard` (kill the process groups) and exits 130.
pub fn watch_stop(
    home: PathBuf,
    sid: String,
    cancel: CancellationToken,
    hard: Arc<dyn Fn() + Send + Sync>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let asked = Reader::open(&home)
                .and_then(|r| r.stop_requested(&sid))
                .unwrap_or(false);
            if !asked {
                continue;
            }
            eprintln!(
                "synt: stop requested (synt stop); cancelling, killing in {STOP_GRACE_SECS}s"
            );
            cancel.cancel();
            tokio::time::sleep(Duration::from_secs(STOP_GRACE_SECS)).await;
            eprintln!("synt: did not end in time; killing");
            hard();
            std::process::exit(130);
        }
    });
}
