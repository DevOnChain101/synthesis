//! `synth memory ...`: the person's view of the long-term store (docs/specs/memory.md §10), and the
//! glue the run uses to read and write it. Memory text is data: it is printed sanitized and never
//! reaches the permission engine.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use synthesis_config::Env;
use synthesis_core::{PathPolicy, repo_key, workspace_label};
use synthesis_harness::sanitize_for_display as san;
use synthesis_memory::{Filter, Memory, NewMemory, Store, Trust, now_iso};
use synthesis_settings::find_project_root;

use crate::{Homes, homes};

/// Identity of the repository a directory belongs to (memory.md §4).
pub struct Repo {
    pub root: PathBuf,
    pub key: String,
    pub label: String,
}

pub fn repo_of(cwd: &Path) -> Repo {
    let root = find_project_root(cwd);
    Repo {
        key: repo_key(&root, PathPolicy::NATIVE),
        label: workspace_label(&root),
        root,
    }
}

/// `synthesis.memory.enabled` of the USER settings.json (default on). A repository cannot turn it on.
pub fn enabled(user_settings: &Path) -> bool {
    std::fs::read(user_settings)
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.pointer("/synthesis/memory/enabled")?.as_bool())
        .unwrap_or(true)
}

/// Opens the store for the run; any failure turns memory off with one warning (it never changes a
/// task result).
pub fn open_for_run(h: &Homes, secrets: &[String], repo: &Repo) -> Option<Store> {
    let mut s = match Store::open(&h.synthesis, secrets.to_vec()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("synth: warning: {}", san(&e.to_string()));
            return None;
        }
    };
    if let Err(e) = s.touch_workspace(&repo.key, &repo.root, &repo.label) {
        eprintln!("synth: warning: {}", san(&e.to_string()));
        return None;
    }
    Some(s)
}

#[derive(Args)]
pub struct Scope {
    /// Directory whose repository is meant (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Every record, not just this repository's and the user's.
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum MemoryCmd {
    /// List records (this repository plus user scope).
    List {
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        kind: Option<String>,
        /// active (default), proposed, quarantined, stale, superseded, archived, or `any`.
        #[arg(long, default_value = "active")]
        status: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Full-text search (BM25).
    Search {
        query: String,
        #[command(flatten)]
        scope: Scope,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// One record in markdown form.
    Show { id: String },
    /// Save a note you typed (trust `owner`).
    Add {
        text: String,
        #[arg(long, default_value = "fact", value_parser = ["fact", "preference", "lesson"])]
        kind: String,
        /// User scope (all repositories) instead of this repository.
        #[arg(long)]
        user: bool,
        #[arg(long)]
        title: Option<String>,
        /// Single-valued slot, e.g. `cmd.test`: a new value replaces the old one.
        #[arg(long)]
        slot: Option<String>,
        #[arg(long)]
        pin: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Hard-delete a record, or every record of this repository with --workspace.
    Delete {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        workspace: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Archive a wrong record (keeps provenance).
    Forget {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Make a proposed or quarantined record active after you read it.
    Approve { id: String },
    /// Pending review items: proposed and quarantined records.
    Review {
        #[command(flatten)]
        scope: Scope,
    },
    /// jsonl (lossless) or md (for review); secrets are masked again.
    Export {
        #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "md"])]
        format: String,
        #[command(flatten)]
        scope: Scope,
    },
    /// Import jsonl from `export`; imported records never keep owner or verified trust.
    Import { file: PathBuf },
    /// Counts, size, queued jobs.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Delete episodes older than 365 days; archive decayed, unused records.
    Prune,
    /// Stop (`on`) or resume (`off`) memory for one repository.
    Optout {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
}

/// `list`/`show` line: the markdown form of memory.md §10.
pub fn markdown(m: &Memory) -> String {
    let scope = match m.scope.as_str() {
        "workspace" => format!("workspace {}", &m.scope_key[..m.scope_key.len().min(8)]),
        s => s.to_string(),
    };
    let slot = m
        .slot
        .as_deref()
        .map_or(String::new(), |s| format!("slot {s} · "));
    san(&format!(
        "## {} · {} · {} · {} · {}\n{}used {} (ok {}) · {}\n{}\n{}",
        m.id,
        m.kind,
        scope,
        m.trust.as_str(),
        m.status,
        slot,
        m.use_count,
        m.success_count,
        m.updated_at.get(..10).unwrap_or(&m.updated_at),
        m.title,
        m.body
    ))
}

fn cwd_of(c: &Option<PathBuf>) -> PathBuf {
    c.clone().unwrap_or_else(|| ".".into())
}

fn filter(scope: &Scope, kind: Option<String>, status: &str, limit: usize) -> Filter {
    Filter {
        workspace: (!scope.all).then(|| repo_of(&cwd_of(&scope.cwd)).key),
        kind,
        status: (status != "any").then(|| status.to_string()),
        limit,
    }
}

pub fn run_cmd(env: &Env, cmd: MemoryCmd) -> Result<u8, String> {
    let h = homes(env)?;
    let mut s = Store::open(&h.synthesis, vec![]).map_err(|e| e.to_string())?;
    let e = |x: synthesis_memory::Error| x.to_string();
    match cmd {
        MemoryCmd::List {
            scope,
            kind,
            status,
            limit,
        } => {
            let rows = s.list(&filter(&scope, kind, &status, limit)).map_err(e)?;
            for m in &rows {
                out!("{}\n", markdown(m));
            }
            out!("{} record(s)", rows.len());
        }
        MemoryCmd::Search {
            query,
            scope,
            limit,
        } => {
            let f = filter(&scope, None, "active", limit);
            for m in s.search(&query, &f).map_err(e)? {
                out!("{}\n", markdown(&m));
            }
        }
        MemoryCmd::Show { id } => match s.get(&id).map_err(e)? {
            Some(m) => {
                out!("{}", markdown(&m));
                out!(
                    "flags: {} · source: {}",
                    m.flags.join(","),
                    san(&m.source.to_string())
                );
            }
            None => return Err(format!("no record {id}")),
        },
        MemoryCmd::Add {
            text,
            kind,
            user,
            title,
            slot,
            pin,
            cwd,
        } => {
            let repo = repo_of(&cwd_of(&cwd));
            let (scope, key) = if user {
                ("user", "")
            } else {
                ("workspace", repo.key.as_str())
            };
            let title = title.unwrap_or_else(|| text.chars().take(80).collect());
            let mut n = NewMemory::new(scope, key, &kind, &title, &text, Trust::Owner);
            n.slot = slot;
            n.pinned = pin;
            n.confidence = 1.0;
            n.source = serde_json::json!({"origin": "cli"});
            if !user {
                s.touch_workspace(&repo.key, &repo.root, &repo.label)
                    .map_err(e)?;
            }
            let o = s.put(n).map_err(e)?;
            out!("{} {} ({})", o.op, o.id, o.status);
            if let Some(r) = o.rule {
                out!(
                    "quarantined by rule {r}: structure that could steer the model; see `synth memory review`"
                );
            }
        }
        MemoryCmd::Delete { id, workspace, cwd } => match (id, workspace) {
            (Some(id), false) => {
                if !s.delete(&id).map_err(e)? {
                    return Err(format!("no record {id}"));
                }
                out!("deleted {id}");
            }
            (None, true) => {
                let n = s.delete_workspace(&repo_of(&cwd_of(&cwd)).key).map_err(e)?;
                out!("deleted {n} record(s)");
            }
            _ => return Err("pass a record id or --workspace".into()),
        },
        MemoryCmd::Forget { id, reason } => {
            if !s.set_status(&id, "archived").map_err(e)? {
                return Err(format!("no record {id}"));
            }
            out!("archived {id} ({})", san(&reason));
        }
        MemoryCmd::Approve { id } => {
            if !s.set_status(&id, "active").map_err(e)? {
                return Err(format!("no record {id}"));
            }
            out!("{id} is active");
        }
        MemoryCmd::Review { scope } => {
            let mut n = 0;
            for status in ["proposed", "quarantined"] {
                for m in s.list(&filter(&scope, None, status, 100)).map_err(e)? {
                    out!("{}\n", markdown(&m));
                    n += 1;
                }
            }
            out!(
                "{n} item(s) to review; `synth memory approve <id>` or `forget <id> --reason ...`"
            );
        }
        MemoryCmd::Export { format, scope } => {
            let key = (!scope.all).then(|| repo_of(&cwd_of(&scope.cwd)).key);
            if format == "jsonl" {
                print!("{}", s.export_jsonl(key.as_deref()).map_err(e)?);
            } else {
                let f = Filter {
                    workspace: key,
                    limit: usize::MAX,
                    ..Filter::default()
                };
                for m in s.list(&f).map_err(e)? {
                    out!("{}\n", s.redact(&markdown(&m)));
                }
            }
        }
        MemoryCmd::Import { file } => {
            let text =
                std::fs::read_to_string(&file).map_err(|x| format!("{}: {x}", file.display()))?;
            let (done, skipped) = s.import_jsonl(&text).map_err(e)?;
            out!("imported {} record(s), skipped {skipped}", done.len());
        }
        MemoryCmd::Stats { json } => {
            let st = s.stats().map_err(e)?;
            if json {
                out!("{}", serde_json::to_string(&st).map_err(|x| x.to_string())?);
            } else {
                out!("path: {}", s.path().display());
                out!("records: {} ({} quarantined)", st.total, st.quarantined);
                for (title, rows) in [
                    ("kind", &st.by_kind),
                    ("status", &st.by_status),
                    ("trust", &st.by_trust),
                ] {
                    let v: Vec<String> = rows.iter().map(|(k, n)| format!("{k} {n}")).collect();
                    out!("{title}: {}", v.join(", "));
                }
                out!("size: {} bytes", st.db_bytes);
                out!(
                    "jobs: {} queued, {} failed · proposals pending: {}",
                    st.queued_jobs,
                    st.failed_jobs,
                    st.pending_proposals
                );
            }
        }
        MemoryCmd::Prune => {
            let (d, a) = s
                .prune(365, synthesis_memory::OffsetDateTime::now_utc())
                .map_err(e)?;
            out!(
                "deleted {d} old episode(s), archived {a} decayed record(s) at {}",
                now_iso()
            );
        }
        MemoryCmd::Optout { state, cwd } => {
            let repo = repo_of(&cwd_of(&cwd));
            s.touch_workspace(&repo.key, &repo.root, &repo.label)
                .map_err(e)?;
            s.set_opt_out(&repo.key, state == "on").map_err(e)?;
            out!(
                "memory for {} is {}",
                repo.label,
                if state == "on" { "off" } else { "on" }
            );
        }
    }
    Ok(0)
}

/// The run's open store (a Mutex: the task runs on another thread).
pub struct MemoryRun {
    pub store: std::sync::Mutex<Store>,
    pub key: String,
    pub sid: String,
}

fn warn(e: impl std::fmt::Display) {
    eprintln!("synth: warning: memory: {}", san(&e.to_string()));
}

/// Retrieval at task start: the frozen `<memory>` block. In a repository that is not trusted only
/// user-scope `owner` notes are injected (memory.md §9). A failure is a warning, never a task error.
pub fn inject(
    store: &mut Store,
    repo: &Repo,
    text: &str,
    sid: &str,
    project_trusted: bool,
) -> Option<String> {
    let now = synthesis_memory::OffsetDateTime::now_utc();
    let mut q = synthesis_memory::Query::new(text, Some(&repo.key), now);
    q.root = Some(&repo.root);
    let mut r = store.retrieve(&q).map_err(warn).ok()?;
    if !project_trusted {
        r.notes
            .retain(|n| n.memory.scope == "user" && n.memory.trust == Trust::Owner);
    }
    let ids: Vec<String> = r.notes.iter().map(|n| n.memory.id.clone()).collect();
    if !ids.is_empty() {
        store
            .record_uses(&ids, sid, None, "injected")
            .map_err(warn)
            .ok()?;
    }
    synthesis_memory::render(&r, now)
}

/// Deterministic episode from data the harness already holds (no model call), plus the outcome of
/// the notes this task was given.
pub fn task_ended(m: &MemoryRun, request: &str, out: &synthesis_harness::TaskOutcome) {
    let mut s = m.store.lock().unwrap_or_else(|e| e.into_inner());
    let status = format!("{:?}", out.status);
    let ok = out.status == synthesis_events::TaskStatus::Completed;
    let clip = |t: &str, n: usize| t.chars().take(n).collect::<String>();
    let mut body = format!(
        "Request: {}\nStatus: {status}: {}",
        clip(request, 400),
        clip(&out.reason, 200)
    );
    if !out.changed_files.is_empty() {
        let files: Vec<&str> = out
            .changed_files
            .iter()
            .take(10)
            .map(String::as_str)
            .collect();
        body.push_str(&format!("\nFiles changed: {}", files.join(", ")));
    }
    if let Some(r) = &out.verification_report_id {
        body.push_str(&format!("\nVerification report: {r}"));
    }
    let mut n = NewMemory::new(
        "workspace",
        &m.key,
        "episode",
        &format!(
            "{status}: {}",
            clip(request.lines().next().unwrap_or(""), 90)
        ),
        &body,
        Trust::Verified,
    );
    n.source =
        serde_json::json!({"origin": "task_end", "session_id": m.sid, "task_id": out.task_id});
    if let Err(e) = s.put(n) {
        warn(e);
    }
    if let Err(e) = s.finish_uses(&m.sid, None, ok) {
        warn(e);
    }
}
