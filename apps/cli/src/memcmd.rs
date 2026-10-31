//! `synth memory ...`: the person's view of the long-term store (docs/specs/memory.md §10), and the
//! glue the run uses to read and write it. Memory text is data: it is printed sanitized and never
//! reaches the permission engine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use synthesis_config::{Env, ProjectTrust};
use synthesis_core::{PathPolicy, repo_key, workspace_label};
use synthesis_harness::sanitize_for_display as san;
use synthesis_memory::{Digest, Filter, Memory, NewMemory, Store, ToolRow, Trust, now_iso};
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
    flag(user_settings, "enabled")
}

/// `synthesis.memory.<key>` of the user settings.json as a bool; default on.
fn flag(user_settings: &Path, key: &str) -> bool {
    std::fs::read(user_settings)
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.pointer(&format!("/synthesis/memory/{key}"))?.as_bool())
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
}

/// Opens memory for a run and retrieves the block. No store, an opted-out repository or a disabled
/// setting all mean "no memory this run".
pub fn start(a: &StartArgs, cancel: CancellationToken) -> (Option<Arc<MemoryRun>>, Option<String>) {
    // a hook left by an earlier session in this process must not outlive its store
    synthesis_tools::set_learned_skills(None);
    let settings = a.home.synthesis.join("settings.json");
    if !a.enabled || !enabled(&settings) {
        return (None, None);
    }
    let Some(mut s) = open_for_run(a.home, a.secrets, a.repo) else {
        return (None, None);
    };
    if s.opted_out(&a.repo.key) {
        return (None, None);
    }
    let block = inject(&mut s, a.repo, a.prompt, a.sid, a.project_trusted);
    let reflect = flag(&settings, "reflection").then(|| Reflector {
        provider: a.provider.clone(),
        model: a.model.clone(),
        cancel,
    });
    let run = MemoryRun {
        store: Mutex::new(s),
        key: a.repo.key.clone(),
        sid: a.sid.to_string(),
        reflect,
        root: a.repo.root.clone(),
        home: a.home.synthesis.clone(),
        project_trusted: a.project_trusted,
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
    if let Err(e) = s.put(n) {
        warn(e);
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
        let result = reflect_one(store, r, &job).await;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(a) => {
                let _ = s.finish_job(&job.id, None);
                done += 1;
                eprintln!(
                    "synth: memory: reflection saved {} new, {} updated, {} to review, {} quarantined, {} dropped",
                    a.added, a.updated, a.proposed, a.quarantined, a.dropped
                );
            }
            Err(e) => {
                let _ = s.finish_job(&job.id, Some(&e));
                failed += 1;
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
