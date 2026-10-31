//! `synth memory ...`: the person's view of the long-term store (docs/specs/memory.md §10), and the
//! glue the run uses to read and write it. Memory text is data: it is printed sanitized and never
//! reaches the permission engine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use synthesis_config::{Env, ProjectTrust};
use synthesis_core::{PathPolicy, repo_key, workspace_label};
use synthesis_events::Event;
use synthesis_harness::sanitize_for_display as san;
use synthesis_harness::{EventSink, Scope as EvScope};
use synthesis_memory::{
    Digest, Filter, Memory, NewMemory, Store, ToolRow, Trust, Written, now_iso,
};
use synthesis_providers::{Block, Content, ModelProvider, Role};
use synthesis_settings::find_project_root;
use tokio_util::sync::CancellationToken;

use crate::memory::project_file;
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
    flag(user_settings, "enabled", true)
}

/// Reflection sends the task's digest (request, tool calls, user turns) to the model provider, so it
/// is opt-in: `synthesis.memory.reflection: true` in the USER settings.json (the owner has not
/// confirmed a default policy; a repository cannot turn it on).
pub fn reflection_enabled(user_settings: &Path) -> bool {
    flag(user_settings, "reflection", false)
}

/// `synthesis.memory.<key>` of the user settings.json as a bool.
fn flag(user_settings: &Path, key: &str, default: bool) -> bool {
    std::fs::read(user_settings)
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.pointer(&format!("/synthesis/memory/{key}"))?.as_bool())
        .unwrap_or(default)
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
    /// Edit a record in $VISUAL/$EDITOR (title on the first line, then the body); saves a new `owner` version.
    Edit { id: String },
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
    /// Run the queued reflection jobs now (one capped model call each); `--retry` re-queues failed ones.
    Reflect {
        /// Provider id; with --model NAME, or give --model provider/model. Default: smallModel.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Pending proposals (skills and instruction-file additions) with their diffs.
    Proposals,
    /// Show a proposal; with --yes write it (a skill becomes active, a bullet is appended).
    Apply {
        id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Decline a proposal.
    Reject { id: String },
    /// Look for promotion candidates now.
    Promote {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Stop (`on`) or resume (`off`) memory for one repository.
    Optout {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
}

/// Commands that change what the model will later read or what gets written to instruction files.
fn mutates(cmd: &MemoryCmd) -> bool {
    match cmd {
        MemoryCmd::Apply { yes, .. } => *yes,
        MemoryCmd::List { .. }
        | MemoryCmd::Search { .. }
        | MemoryCmd::Show { .. }
        | MemoryCmd::Review { .. }
        | MemoryCmd::Export { .. }
        | MemoryCmd::Stats { .. }
        | MemoryCmd::Proposals => false,
        _ => true,
    }
}

/// The agent must not approve, delete, import or opt out of its own memory: such a command needs a
/// person at a terminal, and refuses inside a process the core started for the agent (the marker
/// `SYNTHESIS_AGENT_SESSION` is set by the core for children; it is output, not a setting).
/// A permission rule also denies these from `Bash`; this is the second lock.
fn refuse_unattended(env: &Env, cmd: &MemoryCmd) -> Result<(), String> {
    use std::io::IsTerminal;
    if !mutates(cmd) {
        return Ok(());
    }
    if env.get(synthesis_config::AGENT_SESSION_VAR).is_some() {
        return Err("this command changes memory and cannot run from an agent session; run it yourself in a terminal".into());
    }
    // test builds only (the feature cannot be enabled in release): lets the test harness drive it
    #[cfg(feature = "test-provider")]
    if env.get("SYNTHESIS_TEST_ATTENDED").is_some() {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "this command changes memory and needs an interactive terminal (stdin and stdout)"
                .into(),
        );
    }
    Ok(())
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
    refuse_unattended(env, &cmd)?;
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
        MemoryCmd::Edit { id } => edit(env, &mut s, &id)?,
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
            let len = std::fs::metadata(&file)
                .map_err(|x| format!("{}: {x}", file.display()))?
                .len();
            if len > 5 << 20 {
                return Err("import file is over 5 MiB".into());
            }
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
        MemoryCmd::Reflect {
            provider,
            model,
            limit,
            retry,
            cwd,
        } => {
            if retry {
                out!("re-queued {} failed job(s)", s.retry_failed().map_err(e)?);
            }
            let cfg = crate::load_config(env, &h, Some(&cwd_of(&cwd)), ProjectTrust::Untrusted)?;
            let model_arg = model.or_else(|| cfg.small_model.clone());
            let res = crate::run::resolve_provider(
                provider.as_deref(),
                model_arg.as_deref(),
                &cfg,
                &h.synthesis,
                env,
            )?;
            let store = Mutex::new(Store::open(&h.synthesis, res.secrets.clone()).map_err(e)?);
            let rf = Reflector {
                provider: res.provider,
                model: res.model,
                cancel: CancellationToken::new(),
                sink: None,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|x| x.to_string())?;
            let (done, failed) = rt.block_on(reflect_jobs(&store, &rf, limit));
            out!("reflection: {done} done, {failed} failed");
        }
        MemoryCmd::Proposals => {
            let ps = s.proposals(Some("pending")).map_err(e)?;
            for p in &ps {
                out!("## {} · {} · {}\n{}", p.id, p.kind, p.target, san(&p.diff));
            }
            out!("{} pending", ps.len());
        }
        MemoryCmd::Reject { id } => {
            if !s.decide_proposal(&id, "rejected").map_err(e)? {
                return Err(format!("no pending proposal {id}"));
            }
            out!("rejected {id}");
        }
        MemoryCmd::Promote { cwd } => {
            let repo = repo_of(&cwd_of(&cwd));
            let made = synthesis_memory::propose_promotions(
                &mut s,
                &repo.key,
                &project_file(&repo.root),
                &h.synthesis.join("SYNTHESIS.md"),
            )
            .map_err(e)?;
            out!("{} new proposal(s)", made.len());
        }
        MemoryCmd::Apply { id, yes, cwd } => {
            let Some(p) = s.proposal(&id).map_err(e)? else {
                return Err(format!("no proposal {id}"));
            };
            out!("## {} · {} · {}\n{}", p.id, p.kind, p.target, san(&p.diff));
            if !yes {
                out!("nothing written; run again with --yes to apply");
                return Ok(0);
            }
            if p.kind == "skill" {
                let o = synthesis_memory::approve_skill(&mut s, &p).map_err(e)?;
                out!(
                    "skill active as learned:{} ({})",
                    p.target.trim_start_matches("skill:"),
                    o.id
                );
            } else {
                let repo = repo_of(&cwd_of(&cwd));
                let path = match p.target.as_str() {
                    "user" => h.synthesis.join("SYNTHESIS.md"),
                    "project" => {
                        let own = p
                            .from_memory_ids
                            .iter()
                            .filter_map(|i| s.get(i).ok().flatten())
                            .all(|m| m.scope_key == repo.key);
                        if !own {
                            return Err(
                                "this note belongs to another repository: run from there (--cwd)"
                                    .into(),
                            );
                        }
                        project_file(&repo.root)
                    }
                    _ => return Err("unknown proposal target".into()),
                };
                synthesis_memory::apply_instructions(&mut s, &p, &path).map_err(e)?;
                out!("appended to {}", path.display());
            }
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

/// First line = title, then a blank line, then the body.
fn edit_text(m: &Memory) -> String {
    format!("{}\n\n{}\n", m.title, m.body)
}

fn parse_edit(text: &str) -> Option<(String, String)> {
    let (title, body) = text.split_once('\n').unwrap_or((text, ""));
    let (title, body) = (title.trim(), body.trim());
    (!title.is_empty() && !body.is_empty()).then(|| (title.to_string(), body.to_string()))
}

/// `synth memory edit ID`: the markdown form in `$VISUAL`/`$EDITOR`, saved as a new `owner` version
/// through the normal write path (secrets masked, caps, scanner), so a bad edit is refused and the
/// old record stays. `run_cmd` has already required a terminal. shortcut: the editor command is
/// split on spaces (no shell quoting); use a wrapper script for a command that needs quotes.
fn edit(env: &Env, s: &mut Store, id: &str) -> Result<(), String> {
    let editor = env
        .get_nonempty("VISUAL")
        .or_else(|| env.get_nonempty("EDITOR"))
        .ok_or("set $VISUAL or $EDITOR to edit a note")?;
    let old = s
        .get(id)
        .map_err(|x| x.to_string())?
        .ok_or_else(|| format!("no record {id}"))?;
    if matches!(old.kind.as_str(), "episode")
        || matches!(old.status.as_str(), "superseded" | "archived")
    {
        return Err(format!(
            "{id} ({}, {}) cannot be edited",
            old.kind, old.status
        ));
    }
    let path = std::env::temp_dir().join(format!("synthesis-memory-{}.md", uuid::Uuid::new_v4()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(
        &mut o
            .open(&path)
            .map_err(|x| format!("{}: {x}", path.display()))?,
        edit_text(&old).as_bytes(),
    )
    .map_err(|x| x.to_string())?;
    let result = (|| {
        let mut parts = editor.split_whitespace();
        let status = std::process::Command::new(parts.next().ok_or("empty $EDITOR")?)
            .args(parts)
            .arg(&path)
            .status()
            .map_err(|x| format!("cannot start the editor `{editor}`: {x}"))?;
        if !status.success() {
            return Err(format!("the editor exited with {status}; nothing saved"));
        }
        let bytes = std::fs::read(&path).map_err(|x| x.to_string())?;
        let text = String::from_utf8(bytes).map_err(|_| "the edited file is not UTF-8")?;
        let (title, body) =
            parse_edit(&text).ok_or("the title (first line) and the body are both required")?;
        if (title.as_str(), body.as_str()) == (old.title.trim(), old.body.trim()) {
            out!("no changes");
            return Ok(());
        }
        let mut n = NewMemory::new(
            &old.scope,
            &old.scope_key,
            &old.kind,
            &title,
            &body,
            Trust::Owner,
        );
        n.slot = old.slot.clone();
        n.pinned = old.pinned;
        n.tags = old.tags.clone();
        n.anchors = old.anchors.clone();
        n.confidence = 1.0;
        n.source = serde_json::json!({"origin": "cli_edit", "edits": id});
        let r = s.revise(id, n).map_err(|x| x.to_string())?;
        match (r.op, r.rule) {
            ("quarantined", rule) => out!(
                "not applied: {} was quarantined by rule {} (structure that could steer the model); {id} is unchanged",
                r.id,
                rule.unwrap_or("?")
            ),
            ("deduped", _) => out!("no changes (identical to {})", r.id),
            _ => out!("edited {id}: new owner version {} ({})", r.id, r.status),
        }
        Ok(())
    })();
    // on a refused save the person's text is the only copy: keep it and say where
    match &result {
        Err(e) if !e.starts_with("the editor") && !e.starts_with("cannot start") => {
            Err(format!("{e}; your text is kept at {}", path.display()))
        }
        _ => {
            let _ = std::fs::remove_file(&path);
            result
        }
    }
}

/// `synth doctor` rows for memory: where the store is, how big, whether it is healthy, what trust the
/// records have, and whether anything leaves the machine. Read-only: nothing is created or migrated.
pub fn doctor_lines(h: &Homes) -> Vec<String> {
    let settings = h.synthesis.join("settings.json");
    let path = h.synthesis.join("memory").join("memory.db");
    let mut v = vec![format!(
        "memory: {} ({})",
        path.display(),
        if enabled(&settings) {
            "enabled"
        } else {
            "disabled by synthesis.memory.enabled"
        }
    )];
    match store_facts(&path) {
        None => v.push(
            "  store: not created yet (the first `synth run` or `synth memory add` creates it)"
                .into(),
        ),
        Some(Err(e)) => v.push(format!(
            "  store: UNUSABLE ({}); runs continue without memory",
            san(&e)
        )),
        Some(Ok(f)) => v.extend(f),
    }
    v.push(
        "  masking: on (every field passes the secret redactor; a note over 20% masked is dropped)"
            .into(),
    );
    v.push(format!(
        "  reflection: {} (sends the task digest to the model provider; opt in with synthesis.memory.reflection: true)",
        if reflection_enabled(&settings) { "ON" } else { "off" }
    ));
    v
}

fn store_facts(path: &Path) -> Option<Result<Vec<String>, String>> {
    let size = std::fs::metadata(path).ok()?.len();
    let facts = || -> rusqlite::Result<Vec<String>> {
        let c = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        c.busy_timeout(std::time::Duration::from_millis(1000))?;
        let one = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0));
        let ver = one("SELECT coalesce(max(version), 0) FROM schema_migrations")?;
        let check: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        let mut trust =
            c.prepare("SELECT trust, count(*) FROM memories GROUP BY trust ORDER BY trust")?;
        let by_trust: Vec<String> = trust
            .query_map([], |r| {
                Ok(format!(
                    "{} {}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let oldest: Option<String> = c.query_row(
            "SELECT min(created_at) FROM reflection_jobs WHERE status='queued'",
            [],
            |r| r.get(0),
        )?;
        let last_err: Option<String> = rusqlite::OptionalExtension::optional(c.query_row(
            "SELECT error FROM reflection_jobs WHERE error IS NOT NULL ORDER BY created_at DESC LIMIT 1",
            [],
            |r| r.get::<_, Option<String>>(0),
        ))?
        .flatten();
        Ok(vec![
            format!(
                "  store: schema v{ver}, {size} bytes, quick_check {}",
                san(&check)
            ),
            format!(
                "  records: {} ({} quarantined, {} proposed); trust: {}",
                one("SELECT count(*) FROM memories")?,
                one("SELECT count(*) FROM memories WHERE status='quarantined'")?,
                one("SELECT count(*) FROM memories WHERE status='proposed'")?,
                if by_trust.is_empty() {
                    "none".into()
                } else {
                    by_trust.join(", ")
                }
            ),
            format!(
                "  jobs: {} queued{}, {} failed{} · proposals pending: {}",
                one("SELECT count(*) FROM reflection_jobs WHERE status='queued'")?,
                oldest.map_or(String::new(), |t| format!(" (oldest {t})")),
                one("SELECT count(*) FROM reflection_jobs WHERE status='failed'")?,
                last_err.map_or(String::new(), |e| format!(
                    " (last error: {})",
                    san(&e.chars().take(120).collect::<String>())
                )),
                one("SELECT count(*) FROM proposals WHERE status='pending'")?
            ),
        ])
    };
    Some(facts().map_err(|e| e.to_string()))
}

/// Model used for reflection (memory.md D5): `smallModel` when it names the run's provider, else
/// the run's own model.
pub fn small_model(cfg: &synthesis_config::Config, label: &str, fallback: &str) -> String {
    cfg.small_model
        .as_deref()
        .and_then(|s| synthesis_config::parse_model(Some(s), cfg).ok())
        .and_then(|p| {
            let id = p.provider_id?;
            label.starts_with(&format!("{id}/")).then_some(p.model)
        })
        .unwrap_or_else(|| fallback.to_string())
}

pub struct Reflector {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub cancel: CancellationToken,
    /// `memory.reflection.*` and `memory.written` go here (None for `synth memory reflect`).
    pub sink: Option<EventSink>,
}

/// The run's open store (a Mutex: the task runs on another thread).
pub struct MemoryRun {
    pub store: Mutex<Store>,
    pub key: String,
    pub sid: String,
    /// None when `synthesis.memory.reflection` is false.
    pub reflect: Option<Reflector>,
    pub root: PathBuf,
    pub home: PathBuf,
    pub project_trusted: bool,
    /// `memory.*` events of this run.
    pub sink: Option<EventSink>,
    /// Ids the `Memory*` tools wrote this run (lowered to `untrusted` if the task turns out tainted).
    pub tool_written: Mutex<Vec<String>>,
    /// The `memory.retrieved` event, held until the session has started (`announce`).
    pub retrieved: Mutex<Option<Event>>,
}

/// Emits `memory.retrieved` once; call it right after `session.started`.
pub fn announce(m: &MemoryRun) {
    if let Some(ev) = m.retrieved.lock().unwrap_or_else(|e| e.into_inner()).take() {
        emit(m.sink.as_ref(), None, ev);
    }
}

fn emit(sink: Option<&EventSink>, task: Option<&str>, ev: Event) {
    if let Some(s) = sink {
        s.emit(
            &EvScope {
                task_id: task.map(String::from),
                ..EvScope::default()
            },
            ev,
        );
    }
}

/// `memory.written`, plus `memory.quarantined` when the scanner parked the record.
pub fn emit_written(sink: Option<&EventSink>, task: Option<&str>, w: &Written) {
    emit(
        sink,
        task,
        Event::MemoryWritten {
            id: w.id.clone(),
            op: w.op.into(),
            kind: w.kind.clone(),
            scope: w.scope.clone(),
            trust: w.trust.as_str().into(),
            status: w.status.clone(),
        },
    );
    if let Some(rule) = w.rule {
        emit(
            sink,
            task,
            Event::MemoryQuarantined {
                id: w.id.clone(),
                rule: rule.into(),
            },
        );
    }
}

pub struct StartArgs<'a> {
    pub enabled: bool,
    pub home: &'a Homes,
    pub repo: &'a Repo,
    pub prompt: &'a str,
    pub sid: &'a str,
    pub project_trusted: bool,
    pub secrets: &'a [String],
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub sink: Option<EventSink>,
}

/// Opens memory for a run and retrieves the block. No store, an opted-out repository or a disabled
/// setting all mean "no memory this run".
pub fn start(a: &StartArgs, cancel: CancellationToken) -> (Option<Arc<MemoryRun>>, Option<String>) {
    // a hook left by an earlier session in this process must not outlive its store
    synthesis_tools::set_learned_skills(None);
    let settings = a.home.synthesis.join("settings.json");
    if !a.enabled || !enabled(&settings) || disabled_here(&settings, a.repo) {
        return (None, None);
    }
    let Some(mut s) = open_for_run(a.home, a.secrets, a.repo) else {
        return (None, None);
    };
    if s.opted_out(&a.repo.key) {
        return (None, None);
    }
    let mut retrieved = None;
    let block = inject(
        &mut s,
        a.repo,
        a.prompt,
        a.sid,
        a.project_trusted,
        &mut retrieved,
    );
    let reflect = reflection_enabled(&settings).then(|| Reflector {
        provider: a.provider.clone(),
        model: a.model.clone(),
        cancel,
        sink: a.sink.clone(),
    });
    let run = MemoryRun {
        store: Mutex::new(s),
        key: a.repo.key.clone(),
        sid: a.sid.to_string(),
        reflect,
        root: a.repo.root.clone(),
        home: a.home.synthesis.clone(),
        project_trusted: a.project_trusted,
        sink: a.sink.clone(),
        tool_written: Mutex::default(),
        retrieved: Mutex::new(retrieved),
    };
    let run = Arc::new(run);
    let for_skills = run.clone();
    synthesis_tools::set_learned_skills(Some(Arc::new(move || learned(&for_skills))));
    (Some(run), block)
}

/// Approved skills for the Skill tool; the stats ride in the description (`[learned, ok 7/8]`).
fn learned(m: &MemoryRun) -> Vec<synthesis_tools::LearnedSkill> {
    let s = m.store.lock().unwrap_or_else(|e| e.into_inner());
    s.learned_skills(&m.key)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| {
            Some(synthesis_tools::LearnedSkill {
                name: k.name?,
                description: format!(
                    "[learned, ok {}/{}] {}",
                    k.success_count,
                    k.success_count + k.failure_count,
                    k.title
                ),
                body: k.body,
            })
        })
        .collect()
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
    retrieved: &mut Option<Event>,
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
    *retrieved = Some(Event::MemoryRetrieved {
        ids: ids.clone(),
        tokens: r.tokens as u64,
        candidates: r.candidates as u64,
    });
    if !ids.is_empty() {
        store
            .record_uses(&ids, sid, None, "injected")
            .map_err(warn)
            .ok()?;
    }
    synthesis_memory::render(&r, now)
}

/// Injected notes whose id the assistant wrote or passed to a tool: the only sign that one was used.
fn cited_notes(s: &Store, m: &MemoryRun, out: &synthesis_harness::TaskOutcome) -> Vec<String> {
    let mut text = String::new();
    for msg in out.messages.iter().filter(|x| x.role == Role::Assistant) {
        match &msg.content {
            Content::Text(t) => text.push_str(t),
            Content::Blocks(bs) => {
                for b in bs {
                    match b {
                        Block::Text { text: t } => text.push_str(t),
                        Block::ToolUse { input: Some(i), .. } => text.push_str(&i.to_string()),
                        _ => {}
                    }
                    text.push('\n');
                }
            }
        }
    }
    s.open_use_ids(&m.sid, None)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| text.contains(id.as_str()))
        .collect()
}

fn clip(t: &str, n: usize) -> String {
    t.chars().take(n).collect()
}

/// What identifies a tool call for the digest; a call that touches a secret path is left out so the
/// reflection model never sees (or cites) it.
fn tool_arg(input: &serde_json::Value) -> Option<String> {
    let field = |k: &str| input.get(k).and_then(|v| v.as_str());
    for k in ["file_path", "notebook_path", "path"] {
        if field(k).is_some_and(|p| synthesis_core::secrets::is_secret_path(Path::new(p))) {
            return None;
        }
    }
    let arg = [
        "command",
        "file_path",
        "notebook_path",
        "path",
        "pattern",
        "url",
        "query",
        "skill",
    ]
    .iter()
    .find_map(|k| field(k))
    .unwrap_or("");
    Some(clip(arg, 120))
}

pub fn digest_of(request: &str, out: &synthesis_harness::TaskOutcome) -> Digest {
    let mut d = Digest {
        request: clip(request, 400),
        status: format!("{:?}", out.status),
        reason: clip(&out.reason, 200),
        files: out.changed_files.iter().take(10).cloned().collect(),
        verification: out.verification_report_id.clone(),
        denied: !out.needed_permissions.is_empty(),
        ..Digest::default()
    };
    for m in &out.messages {
        match &m.content {
            Content::Text(t) if m.role == Role::User => d.user_turns.push(clip(t, 300)),
            Content::Blocks(bs) => {
                let mut user_text = String::new();
                for b in bs {
                    match b {
                        Block::Text { text } if m.role == Role::User => user_text.push_str(text),
                        Block::ToolUse {
                            id,
                            name,
                            input: Some(input),
                        } => {
                            if let Some(arg) = tool_arg(input) {
                                d.tools.push(ToolRow {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arg,
                                    failed: false,
                                });
                            }
                        }
                        Block::ToolResult {
                            tool_use_id,
                            is_error: Some(true),
                            ..
                        } => {
                            if let Some(t) = d.tools.iter_mut().find(|t| &t.id == tool_use_id) {
                                t.failed = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !user_text.is_empty() {
                    d.user_turns.push(clip(&user_text, 300));
                }
            }
            Content::Text(_) => {}
        }
    }
    d
}

/// Deterministic episode from data the harness already holds (no model call), the outcome of the
/// notes this task was given, and a queued reflection job when the task qualifies.
pub fn task_ended(m: &MemoryRun, request: &str, out: &synthesis_harness::TaskOutcome) {
    let mut s = m.store.lock().unwrap_or_else(|e| e.into_inner());
    let status = format!("{:?}", out.status);
    let ok = out.status == synthesis_events::TaskStatus::Completed;
    // out.reason is model-influenced text: it is left out of the episode
    let mut body = format!("Request: {}\nStatus: {status}", clip(request, 400));
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
    let title = format!(
        "{status}: {}",
        clip(request.lines().next().unwrap_or(""), 90)
    );
    let mut n = NewMemory::new(
        "workspace",
        &m.key,
        "episode",
        &title,
        &body,
        // a headless prompt may come from automation or a pipe: never above `agent`
        if m.project_trusted {
            Trust::Agent
        } else {
            Trust::Untrusted
        },
    );
    n.source =
        serde_json::json!({"origin": "task_end", "session_id": m.sid, "task_id": out.task_id});
    let trust = n.trust;
    match s.put(n) {
        Ok(o) => emit_written(
            m.sink.as_ref(),
            Some(&out.task_id),
            &Written::new(o, "episode", "workspace", trust),
        ),
        Err(e) => warn(e),
    }
    let mut d = digest_of(request, out);
    d.untrusted_project = !m.project_trusted;
    let skills = s.learned_skills(&m.key).unwrap_or_default();
    let used: Vec<String> = d
        .tools
        .iter()
        .filter(|t| t.name == "Skill")
        .filter_map(|t| t.arg.strip_prefix("learned:"))
        .filter_map(|n| skills.iter().find(|k| k.name.as_deref() == Some(n)))
        .map(|k| k.id.clone())
        .collect();
    if !used.is_empty()
        && let Err(e) = s.record_uses(&used, &m.sid, None, "searched")
    {
        warn(e);
    }
    // what the model saved with MemoryWrite in a task that touched the network is untrusted too
    if d.tainted() {
        let ids = std::mem::take(&mut *m.tool_written.lock().unwrap_or_else(|e| e.into_inner()));
        for id in ids {
            if let Err(e) = s.demote(&id) {
                warn(e);
            }
        }
    }
    // only notes the task used count for or against them, and never from a tainted task
    let counted = if d.tainted() {
        vec![]
    } else {
        let mut u = used;
        u.extend(cited_notes(&s, m, out));
        u
    };
    if let Err(e) = s.finish_uses(&m.sid, None, ok, &counted) {
        warn(e);
    }
    if let Ok(weak) = s.demote_weak_skills()
        && !weak.is_empty()
    {
        eprintln!(
            "synth: memory: {} learned skill(s) went back to review (low success)",
            weak.len()
        );
    }
    let project = crate::memory::project_file(&m.root);
    let user = m.home.join("SYNTHESIS.md");
    if let Ok(made) = synthesis_memory::propose_promotions(&mut s, &m.key, &project, &user)
        && !made.is_empty()
    {
        for id in &made {
            emit(
                m.sink.as_ref(),
                Some(&out.task_id),
                Event::MemoryProposalCreated { id: id.clone() },
            );
        }
        eprintln!(
            "synth: memory: {} proposal(s) to review: synth memory proposals",
            made.len()
        );
    }
    if m.reflect.is_some()
        && d.qualifies()
        && let Ok(payload) = serde_json::to_string(&d)
        && let Err(e) = s.enqueue_job(&m.key, &m.sid, &out.task_id, &payload)
    {
        warn(e);
    }
}

const REFLECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Drains up to `max` queued jobs, one capped model call each (60 s, daily call cap). A failure
/// puts the job back; nothing here can change a task result. Returns (done, failed).
pub async fn reflect_jobs(store: &Mutex<Store>, r: &Reflector, max: usize) -> (usize, usize) {
    let (mut done, mut failed) = (0, 0);
    for _ in 0..max {
        let now = synthesis_memory::OffsetDateTime::now_utc();
        let job = {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            if s.reflections_today(now).unwrap_or(i64::MAX) >= synthesis_memory::MAX_CALLS_PER_DAY {
                eprintln!("synth: memory: daily reflection cap reached; jobs stay queued");
                break;
            }
            match s.claim_job("synth", now) {
                Ok(Some(j)) => j,
                _ => break,
            }
        };
        let task = Some(job.task_id.as_str());
        emit(
            r.sink.as_ref(),
            task,
            Event::MemoryReflectionStarted {
                job_id: job.id.clone(),
            },
        );
        let result = reflect_one(store, r, &job).await;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(a) => {
                let _ = s.finish_job(&job.id, None);
                done += 1;
                for w in &a.written {
                    emit_written(r.sink.as_ref(), task, w);
                }
                emit(
                    r.sink.as_ref(),
                    task,
                    Event::MemoryReflectionCompleted {
                        job_id: job.id.clone(),
                        ops: a.written.len() as u64,
                        tokens: None,
                        cost_usd: None,
                    },
                );
                eprintln!(
                    "synth: memory: reflection saved {} new, {} updated, {} to review, {} quarantined, {} dropped",
                    a.added, a.updated, a.proposed, a.quarantined, a.dropped
                );
            }
            Err(e) => {
                let _ = s.finish_job(&job.id, Some(&e));
                failed += 1;
                emit(
                    r.sink.as_ref(),
                    task,
                    Event::MemoryReflectionFailed {
                        job_id: job.id.clone(),
                        error: san(&e).chars().take(500).collect(),
                    },
                );
                warn(format!("reflection failed: {e}"));
            }
        }
    }
    (done, failed)
}

async fn reflect_one(
    store: &Mutex<Store>,
    r: &Reflector,
    job: &synthesis_memory::Job,
) -> Result<synthesis_memory::Applied, String> {
    use synthesis_context::Summarizer;
    let d: Digest = serde_json::from_str(&job.payload).map_err(|e| format!("bad payload: {e}"))?;
    let (system, user) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let f = Filter {
            workspace: Some(job.workspace_key.clone()),
            status: Some("active".into()),
            limit: 10,
            ..Filter::default()
        };
        let existing = s.search(&d.request, &f).map_err(|e| e.to_string())?;
        synthesis_memory::prompt(&d, &existing)
    };
    let sum =
        crate::ctx::ProviderSummarizer::new(r.provider.clone(), r.model.clone(), r.cancel.clone())
            .with_max_tokens(2000);
    let reply = tokio::time::timeout(REFLECT_TIMEOUT, sum.summarize(&system, &user))
        .await
        .map_err(|_| "timed out after 60 s".to_string())?
        .map_err(|e| e.to_string())?;
    let (ops, bad) = synthesis_memory::parse_ops(&reply);
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
    let c = synthesis_memory::Ctx {
        workspace_key: &job.workspace_key,
        session_id: &job.session_id,
        task_id: &job.task_id,
        job_id: &job.id,
    };
    let mut a = synthesis_memory::apply(&mut s, &d, ops, &c).map_err(|e| e.to_string())?;
    a.dropped += bad;
    Ok(a)
}

/// D7 (memory.md §15): a repository's `.synthesis/settings.json` may only turn memory OFF
/// (`synthesis.memory.enabled: false`); the user's `synthesis.memory.disabled_workspaces` globs
/// match the repository root.
pub fn disabled_here(user_settings: &Path, repo: &Repo) -> bool {
    let read = |p: &Path| {
        std::fs::read(p)
            .ok()
            .filter(|b| b.len() <= 1 << 20)
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    };
    let off = read(&repo.root.join(".synthesis").join("settings.json"))
        .and_then(|v| v.pointer("/synthesis/memory/enabled")?.as_bool())
        == Some(false);
    let root = repo.root.to_string_lossy().replace('\\', "/");
    let listed = read(user_settings)
        .and_then(|v| v.pointer("/synthesis/memory/disabled_workspaces").cloned())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|globs| {
            globs
                .iter()
                .filter_map(|g| g.as_str())
                .filter_map(|g| globset::Glob::new(g).ok())
                .any(|g| g.compile_matcher().is_match(&root))
        });
    off || listed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(root: &Path) -> Repo {
        Repo {
            root: root.to_path_buf(),
            key: "k".into(),
            label: "l".into(),
        }
    }

    #[test]
    fn a_project_can_only_turn_memory_off_and_the_user_can_list_roots() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("repo");
        std::fs::create_dir_all(root.join(".synthesis")).unwrap();
        let user = d.path().join("settings.json");
        assert!(!disabled_here(&user, &repo(&root)));
        // `enabled: true` in a project changes nothing; it cannot switch memory on
        let proj = root.join(".synthesis/settings.json");
        std::fs::write(&proj, r#"{"synthesis":{"memory":{"enabled":true}}}"#).unwrap();
        assert!(!disabled_here(&user, &repo(&root)));
        std::fs::write(&proj, r#"{"synthesis":{"memory":{"enabled":false}}}"#).unwrap();
        assert!(disabled_here(&user, &repo(&root)));
        std::fs::remove_file(&proj).unwrap();
        std::fs::write(
            &user,
            r#"{"synthesis":{"memory":{"disabled_workspaces":["**/secret-*","/nope/**"]}}}"#,
        )
        .unwrap();
        assert!(!disabled_here(&user, &repo(&root)));
        assert!(disabled_here(&user, &repo(&d.path().join("secret-client"))));
    }

    #[test]
    fn reflection_is_off_unless_the_user_settings_say_true() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("settings.json");
        assert!(!reflection_enabled(&s), "no file");
        std::fs::write(&s, r#"{"synthesis":{"memory":{"enabled":true}}}"#).unwrap();
        assert!(!reflection_enabled(&s), "key absent");
        std::fs::write(&s, r#"{"synthesis":{"memory":{"reflection":true}}}"#).unwrap();
        assert!(reflection_enabled(&s));
        assert!(enabled(&s), "memory itself stays default-on");
    }

    #[test]
    fn edit_text_round_trips_and_needs_title_and_body() {
        assert_eq!(
            parse_edit("T\n\nbody line\nmore\n"),
            Some(("T".into(), "body line\nmore".into()))
        );
        assert_eq!(parse_edit("only title\n"), None);
        assert_eq!(parse_edit("\n\nbody"), None);
    }
}
