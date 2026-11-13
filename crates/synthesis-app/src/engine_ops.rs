//! The session-maintenance methods of `synt engine --stdio` (protocol 3): `/rewind`, `/clean`, `/memory` and
//! `#` notes, `/effort`, `/context`, `/doctor` and `/mcp`. They run in the engine on its own machine's
//! store and files; the client sends the person's choice, never a path, an id list or a flag the engine did
//! not issue. Both destructive flows are two steps with a single-use token the ENGINE issues at the dry run
//! (a rewind or a clean cannot be applied without one, and a token is only good for what it was issued for).
//! Errors are fixed texts or scrubbed of the engine's own folders; every answer is bounded and printable.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use synthesis_harness::sanitize_for_display as clean;
use synthesis_memory::{Filter, Memory, NewMemory, OffsetDateTime, Trust};
use synthesis_session::{CleanScope, StoreOptions};

use crate::engine_methods::{Ctx, check_id, check_key, failed};
use crate::rewind::{self, Entry, Step};

/// A dry run or a clean plan is good for this long.
const TOKEN_TTL: Duration = Duration::from_secs(600);
const MAX_ENTRIES: usize = 2000;
const MAX_CHECKPOINTS: usize = 500;
const MAX_CP_FILES: usize = 200;
pub const MAX_NOTE_BYTES: usize = 8192;
const MAX_NOTE_CHARS: usize = 2000;
const MAX_NOTE_LINES: usize = 40;
const MAX_QUERY: usize = 500;
const MAX_ROWS: usize = 100;
const MAX_DOCTOR_ROWS: usize = 300;
const MAX_MCP_LIST: usize = 100;

const SCOPES: [(CleanScope, &str); 3] = [
    (CleanScope::ClosedEmpty, "ClosedEmpty"),
    (CleanScope::Inactive, "Inactive"),
    (CleanScope::All, "All"),
];

/// The engine's own text with its folders hidden (`hide`: the store home, the person's home, the session folder).
pub fn scrub(e: &str, hide: &[&Path]) -> String {
    let mut s = e.to_string();
    for p in hide {
        // the folder as given and as the system resolves it (`/var` and `/private/var`, links)
        let real = synthesis_core::canonicalize(p).ok();
        for q in std::iter::once(p.to_path_buf()).chain(real) {
            let t = q.to_string_lossy();
            if t.len() > 1 {
                s = s.replace(&*t, "<folder>");
            }
        }
    }
    clean(&s)
}

fn token(kind: &str) -> String {
    format!("{kind}-{}", uuid::Uuid::new_v4().simple())
}

struct RewindPlan {
    token: String,
    sid: String,
    checkpoint: Option<String>,
    entries: Vec<Entry>,
    at: Instant,
}

struct CleanTok {
    token: String,
    scope: CleanScope,
    ids: Vec<String>,
    at: Instant,
}

/// What the engine issued and has not seen come back.
#[derive(Default)]
pub struct Tokens {
    rewind: Option<RewindPlan>,
    clean: Vec<CleanTok>,
}

impl Tokens {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

// ---- rewind

fn load(ctx: &Ctx, sid: &str, hide: &[&Path]) -> Result<rewind::Loaded, String> {
    rewind::load(ctx.home, ctx.user_home, sid, false).map_err(|e| scrub(&e, hide))
}

pub fn rewind_list(ctx: &Ctx, sid: &str, hide: &[&Path]) -> Result<Value, String> {
    let l = load(ctx, sid, hide)?;
    let rows: Vec<Value> = l
        .cps
        .iter()
        .take(MAX_CHECKPOINTS)
        .map(|c| {
            json!({"name": clean(&c.name), "when": clean(&c.created_at), "after_message": c.message_count,
                "files": c.files.iter().take(MAX_CP_FILES).map(|f| clean(&f.path)).collect::<Vec<_>>()})
        })
        .collect();
    Ok(json!({"checkpoints": rows}))
}

/// The dry run: what a rewind of `checkpoint` (None: all) would do now, and the token that applies exactly that.
pub fn rewind_dry_run(
    t: &mut Tokens,
    ctx: &Ctx,
    sid: &str,
    checkpoint: Option<String>,
    hide: &[&Path],
) -> Result<Value, String> {
    if let Some(c) = &checkpoint {
        check_key(c).map_err(|_| "not a checkpoint name".to_string())?;
    }
    t.rewind = None;
    let entries = rewind::plan(&load(ctx, sid, hide)?, checkpoint.as_deref(), false)
        .map_err(|e| scrub(&e, hide))?;
    if entries.len() > MAX_ENTRIES {
        return Err("too many files for one rewind".into());
    }
    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            let (kind, reason) = match &e.step {
                Step::Restore { .. } => ("restore", None),
                Step::Remove => ("remove", None),
                Step::Skip { reason, .. } => ("skip", Some(clean(reason))),
            };
            json!({"shown": clean(&e.shown), "kind": kind, "size": e.size, "reason": reason})
        })
        .collect();
    let tok = token("rw");
    t.rewind = Some(RewindPlan {
        token: tok.clone(),
        sid: sid.into(),
        checkpoint,
        entries,
        at: Instant::now(),
    });
    Ok(json!({"token": tok, "entries": rows}))
}

/// Writes what the dry run showed, if it is still exactly that (`apply_confirmed` plans again and compares).
/// The token is spent by the attempt, whatever the answer.
pub fn rewind_apply(
    t: &mut Tokens,
    ctx: &Ctx,
    sid: &str,
    tok: &str,
    hide: &[&Path],
) -> Result<Value, String> {
    let p = t
        .rewind
        .take()
        .ok_or("there is no dry run to apply: run the dry run first")?;
    if p.token != tok || p.sid != sid || p.at.elapsed() > TOKEN_TTL {
        return Err("that dry run is no longer valid: run it again".into());
    }
    let l = load(ctx, sid, hide)?;
    let done = rewind::apply_confirmed(&l, p.checkpoint.as_deref(), &p.entries)
        .map_err(|e| scrub(&e, hide))?;
    let (mut restored, mut removed, mut skipped) = (0, 0, 0);
    let mut failed_rows = vec![];
    let mut changed = vec![];
    for (e, res) in done {
        match (&e.step, res) {
            (Step::Skip { .. }, _) => skipped += 1,
            (_, Err(why)) => failed_rows.push(scrub(&why, hide)),
            (Step::Restore { .. }, Ok(())) => {
                restored += 1;
                changed.push(clean(&e.shown));
            }
            (_, Ok(())) => {
                removed += 1;
                changed.push(clean(&e.shown));
            }
        }
    }
    Ok(
        json!({"restored": restored, "removed": removed, "skipped": skipped, "failed": failed_rows, "changed": changed}),
    )
}

// ---- clean

fn session_store(ctx: &Ctx) -> Result<synthesis_session::Store, String> {
    synthesis_session::Store::open(ctx.home, StoreOptions::default())
        .map_err(failed("cannot read the session store"))
}

/// The three scopes with their counts, each with its token. The own session is never in a plan.
pub fn clean_plan(t: &mut Tokens, ctx: &Ctx, own: &str) -> Result<Value, String> {
    let store = session_store(ctx)?;
    let now = OffsetDateTime::now_utc();
    t.clean.clear();
    let mut out = vec![];
    for (scope, name) in SCOPES {
        let p = store
            .clean_plan(scope, &[own.to_string()], now)
            .map_err(failed("cannot count the sessions"))?;
        let c = &p.counts;
        let tok = token("cl");
        out.push(json!({"scope": name, "token": tok, "counts": {
            "sessions": c.sessions, "workspaces": c.workspaces, "messages": c.messages, "events": c.events,
            "disk_bytes": c.disk_bytes, "screenshot_bytes": c.screenshot_bytes, "snapshot_bytes": c.snapshot_bytes,
            "log_bytes": c.log_bytes, "skipped_running": c.skipped_running, "skipped_other": c.skipped_other}}));
        t.clean.push(CleanTok {
            token: tok,
            scope,
            ids: p.ids,
            at: Instant::now(),
        });
    }
    Ok(json!({"plans": out}))
}

/// Deletes what the plan of `tok` listed (the store checks each session again). A token is single use.
pub fn clean_apply(
    t: &mut Tokens,
    ctx: &Ctx,
    own: &str,
    tok: &str,
    backup: bool,
    hide: &[&Path],
) -> Result<Value, String> {
    let at = t.clean.iter().position(|c| c.token == tok);
    let c = at.map(|i| t.clean.remove(i));
    t.clean.clear();
    let c = c
        .filter(|c| c.at.elapsed() <= TOKEN_TTL)
        .ok_or("that clean plan is no longer valid: run /clean again")?;
    let store = session_store(ctx)?;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.clean(
            c.scope,
            &[own.to_string()],
            OffsetDateTime::now_utc(),
            &c.ids,
            backup,
        )
    }))
    .map_err(|_| "the clean worker panicked: some sessions may be deleted, run /clean again")?
    .map_err(failed("cannot delete the sessions"))?;
    Ok(
        json!({"deleted": r.deleted, "events": r.events, "skipped_running": r.skipped_running,
        "skipped_other": r.skipped_other, "freed_bytes": r.freed_bytes,
        "failed": r.failed.iter().take(100).map(|f| scrub(f, hide)).collect::<Vec<_>>(),
        "backup": r.backup.map(|b| scrub(&b.display().to_string(), hide))}),
    )
}

// ---- memory

/// Where memory is read from for one folder: the same rules as the window's own `/memory` (project scope only
/// in a trusted folder with memory on and not opted out; otherwise the user scope alone).
pub struct Mem {
    home: std::path::PathBuf,
    repo: crate::memcmd::Repo,
    project: bool,
    why_not: Option<&'static str>,
    secrets: Vec<String>,
}

impl Mem {
    pub fn new(ctx: &Ctx, cwd: &Path, trusted: bool) -> Result<Self, String> {
        let settings = ctx.home.join("settings.json");
        if !crate::memcmd::enabled(&settings) {
            return Err("memory is off (synt.memory.enabled is false in the user settings)".into());
        }
        let repo = crate::memcmd::repo_of(cwd);
        let why_not = if !trusted {
            Some(
                "this folder is not trusted, so project memory is off here; only the user scope is available",
            )
        } else if crate::memcmd::disabled_here(&settings, &repo) {
            Some("memory is turned off for this repository; only the user scope is available")
        } else {
            None
        };
        let secrets = ctx
            .env
            .iter()
            .filter(|(k, v)| synthesis_config::is_secret_env_name(k) && v.len() >= 8)
            .map(|(_, v)| v.to_string())
            .collect();
        Ok(Self {
            home: ctx.home.into(),
            repo,
            project: why_not.is_none(),
            why_not,
            secrets,
        })
    }

    fn open(&self) -> Result<synthesis_memory::Store, String> {
        synthesis_memory::Store::open(&self.home, self.secrets.clone())
            .map_err(failed("cannot open the memory store"))
    }

    fn filter(&self, limit: usize) -> Filter {
        Filter {
            workspace: Some(if self.project {
                self.repo.key.clone()
            } else {
                String::new()
            }),
            kind: None,
            status: Some("active".into()),
            limit,
        }
    }

    fn visible(&self, m: &Memory) -> bool {
        m.scope == "user"
            || (self.project && m.scope == "workspace" && m.scope_key == self.repo.key)
    }

    fn get(&self, s: &synthesis_memory::Store, id: &str) -> Result<Memory, String> {
        check_id(id).map_err(|_| "not a memory id".to_string())?;
        match s.get(id).map_err(failed("cannot read the memory store"))? {
            Some(m) if self.visible(&m) => Ok(m),
            Some(_) => Err("that record is not in the memory this folder can use".into()),
            None => Err("no such record".into()),
        }
    }

    fn row(&self, m: &Memory) -> Value {
        json!({"id": clean(&m.id), "kind": clean(&m.kind), "scope": clean(&m.scope), "trust": m.trust.as_str(),
            "status": clean(&m.status), "updated_at": clean(&m.updated_at), "title": clean(&m.title)})
    }

    fn rows(&self, v: Vec<Memory>) -> Value {
        json!({"records": v.iter().take(MAX_ROWS).map(|m| self.row(m)).collect::<Vec<_>>(), "note": self.why_not})
    }

    pub fn list(&self) -> Result<Value, String> {
        let s = self.open()?;
        Ok(self.rows(
            s.list(&self.filter(50))
                .map_err(failed("cannot read the memory store"))?,
        ))
    }

    pub fn retrieve(&self, query: &str) -> Result<Value, String> {
        if query.trim().is_empty() || query.len() > MAX_QUERY || query.chars().any(char::is_control)
        {
            return Err("not a search text".into());
        }
        let s = self.open()?;
        Ok(self.rows(
            s.search(query, &self.filter(10))
                .map_err(failed("cannot read the memory store"))?,
        ))
    }

    pub fn show(&self, id: &str) -> Result<Value, String> {
        let m = self.get(&self.open()?, id)?;
        let text: String = clean(&crate::memcmd::markdown(&m))
            .chars()
            .take(64 << 10)
            .collect();
        Ok(json!({"text": text, "untrusted": m.trust == Trust::Untrusted}))
    }

    /// Saves a note the person typed and confirmed (trust `owner`); the same refusals as the window's own.
    pub fn add(&self, text: &str, user: bool) -> Result<Value, String> {
        check_len(text)?;
        let mut s = self.open()?;
        if s.redact(text) != text.trim() {
            return Err("not saved: the text contains something shaped like a secret (a key, token or password); remove it and try again".into());
        }
        let (scope, key) = if user || !self.project {
            ("user", String::new())
        } else {
            ("workspace", self.repo.key.clone())
        };
        if scope != "user" {
            s.touch_workspace(&self.repo.key, &self.repo.root, &self.repo.label)
                .map_err(failed("cannot write the memory store"))?;
        }
        let title: String = text.chars().take(80).collect();
        let mut n = NewMemory::new(scope, &key, "fact", &title, text, Trust::Owner);
        n.confidence = 1.0;
        n.source = json!({"origin": "desktop-wsl"});
        let o = s.put(n).map_err(failed("cannot write the memory store"))?;
        let rule = o
            .rule
            .map(|r| format!(": quarantined by rule {r}, kept but not used until accepted"));
        Ok(
            json!({"text": format!("{} {} ({}){}", o.op, o.id, o.status, rule.unwrap_or_default()), "scope": scope}),
        )
    }

    pub fn forget(&self, id: &str) -> Result<Value, String> {
        let mut s = self.open()?;
        self.get(&s, id)?;
        s.set_status(id, "archived")
            .map_err(failed("cannot write the memory store"))?;
        Ok(json!({"text": format!("archived {id}")}))
    }
}

fn check_len(text: &str) -> Result<(), String> {
    if text.trim().is_empty()
        || text.chars().count() > MAX_NOTE_CHARS
        || text.lines().count() > MAX_NOTE_LINES
    {
        return Err(format!(
            "a note must be 1 to {MAX_NOTE_CHARS} characters and at most {MAX_NOTE_LINES} lines"
        ));
    }
    Ok(())
}

// ---- effort, context, doctor, mcp

/// `/effort`: a tier the model declares, or none (auto). The choice is kept in this machine's state for the next start.
pub fn effort(
    ctx: &Ctx,
    live: &crate::host::Live,
    tiers: &[String],
    effort: Option<String>,
) -> Result<Value, String> {
    if let Some(e) = &effort
        && !tiers.contains(e)
    {
        return Err("that model does not declare that effort tier".into());
    }
    synthesis_config::save_effort(ctx.home, effort.as_deref())
        .map_err(failed("cannot save the effort"))?;
    live.harness.set_reasoning_effort(effort.clone());
    Ok(json!({"effort": effort}))
}

/// `/context`: writes `limit.context` of the model in use into the engine's own user `synt.json`.
pub fn context(ctx: &Ctx, label: &str, value: &str, hide: &[&Path]) -> Result<Value, String> {
    if value.len() > 32 || value.chars().any(char::is_control) {
        return Err("not a token count".into());
    }
    let tokens = if value.trim().eq_ignore_ascii_case("unset") {
        None
    } else {
        Some(crate::models::parse_tokens(value).map_err(|e| scrub(&e, hide))?)
    };
    let (p, m) = label.split_once('/').unwrap_or(("", label));
    crate::models::set_context_limit(ctx.home, p, m, tokens).map_err(|e| scrub(&e, hide))?;
    Ok(json!({"label": clean(label), "tokens": tokens}))
}

pub fn doctor(ctx: &Ctx, cwd: &Path, trusted: bool) -> Result<Value, String> {
    let rows = crate::doctor::rows_capped(ctx.env, cwd, trusted, Duration::from_secs(20))?;
    let level = |l| match l {
        crate::doctor::Level::Ok => "ok",
        crate::doctor::Level::Warn => "warn",
        crate::doctor::Level::Fail => "fail",
    };
    Ok(json!({"rows": rows.iter().take(MAX_DOCTOR_ROWS)
        .map(|r| json!({"level": level(r.level), "text": clean(&r.text.chars().take(2000).collect::<String>())}))
        .collect::<Vec<_>>()}))
}

pub fn mcp_list(m: Option<&std::sync::Arc<synthesis_mcp::McpManager>>) -> Value {
    let Some(m) = m else {
        return json!({"servers": []});
    };
    let tools = m.tools();
    let servers: Vec<Value> = m
        .status()
        .into_iter()
        .take(MAX_MCP_LIST)
        .map(|s| {
            let t: Vec<Value> = tools
                .iter()
                .filter(|t| t.server == s.name && !t.synthetic)
                .take(100)
                .map(|t| json!([clean(&t.tool), clean(t.description.lines().next().unwrap_or(""))]))
                .collect();
            json!({"name": clean(&s.name), "state": format!("{:?}", s.state).to_lowercase(),
                "transport": s.transport, "tool_count": s.tool_count, "error": s.error.as_deref().map(clean), "tools": t})
        })
        .collect();
    json!({"servers": servers})
}

/// Servers the user file may hold when added over the protocol.
const MAX_USER_SERVERS: usize = 32;

/// One writer at a time for the read-modify-write of the user `synt.json` (a lock file next to it).
fn edit_lock(ctx: &Ctx) -> Result<std::fs::File, String> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(ctx.home.join(".mcp-edit.lock"))
        .map_err(failed("cannot lock the settings file"))?;
    f.lock().map_err(failed("cannot lock the settings file"))?;
    Ok(f)
}

fn user_server_count(ctx: &Ctx) -> usize {
    std::fs::read_to_string(ctx.home.join("synt.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["mcpServers"].as_object().map(|o| o.len()))
        .unwrap_or(0)
}

/// What `mcp.add` carries. Only `name` and `url` can be used: the other fields exist so that a request that
/// tries them is refused with a reason (rather than silently losing them).
pub struct McpAdd {
    pub name: String,
    pub url: Option<String>,
    pub transport: String,
    pub env: Vec<String>,
    pub header: Vec<String>,
    pub command: Vec<String>,
}

/// `mcp.add`: a REMOTE (`https` URL) server into the USER file of this machine, with `synt mcp add`'s
/// name and URL guards. Over the protocol it can never be a server that runs a command (that would be code
/// execution in the engine's machine with no approval), carry environment variables (`LD_PRELOAD`, `PATH`, ...)
/// or headers (`${VAR}` in a header would send an engine secret to a URL the client chose), or replace an
/// existing server. A stdio server is added by hand in the engine's own `synt.json`, or (if ever needed here)
/// through the engine's approval dialogs, never automatically.
pub fn mcp_add(ctx: &Ctx, a: McpAdd, hide: &[&Path]) -> Result<Value, String> {
    if !a.command.is_empty() || !a.env.is_empty() || !a.header.is_empty() {
        return Err("only a URL server can be added over the protocol: a command, environment variables or headers are refused (edit the engine's synt.json by hand)".into());
    }
    let url = a.url.ok_or("a URL is required")?;
    if url.len() > 2048
        || url.contains('$')
        || url.chars().any(|c| c.is_control() || c == ' ')
        || !matches!(a.transport.as_str(), "http" | "sse")
    {
        return Err("not an https server URL".into());
    }
    // https only, no user:password, no loopback, and nothing that is or resolves to a private, link-local
    // (the cloud metadata address) or other internal address. This is the add-time check only: at every connect
    // the transports resolve the name themselves and connect ONLY to addresses that pass the same blocklist
    // (`synthesis_mcp`'s guard resolver), so a name that later answers differently is not followed
    let parsed =
        synthesis_mcp::Url::parse(&url).map_err(|_| "not an https server URL".to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(failed("cannot check the URL"))?;
    rt.block_on(synthesis_mcp::check_remote_url(&parsed))
        .map_err(|e| scrub(&e, hide))?;
    let _lock = edit_lock(ctx)?;
    if user_server_count(ctx) >= MAX_USER_SERVERS {
        return Err(format!(
            "at most {MAX_USER_SERVERS} servers can be added over the protocol"
        ));
    }
    let args = crate::mcp::AddArgs {
        name: a.name,
        scope: crate::mcp::ScopeArg::User,
        url: Some(url),
        transport: a.transport,
        env: vec![],
        header: vec![],
        cwd: None,
        force: false,
        command: vec![],
    };
    crate::mcp::add_server(&ctx.home.join("synt.json"), &args).map_err(|e| scrub(&e, hide))?;
    Ok(json!({"added": clean(&args.name)}))
}

/// `mcp.remove`: a server of the USER file only (never a project or local entry: those files are not opened),
/// and not one a project server of the same name shadows in the live session.
pub fn mcp_remove(
    ctx: &Ctx,
    live: Option<&std::sync::Arc<synthesis_mcp::McpManager>>,
    name: &str,
    hide: &[&Path],
) -> Result<Value, String> {
    if !synthesis_mcp::valid_server_name(name) {
        return Err("not a server name".into());
    }
    if live.is_some_and(|m| {
        m.status()
            .iter()
            .any(|s| s.name == name && s.scope.as_deref().is_some_and(|x| x != "user"))
    }) {
        return Err("that server comes from the project, not from your user settings".into());
    }
    let _lock = edit_lock(ctx)?;
    crate::mcp::remove_server(&ctx.home.join("synt.json"), name).map_err(|e| scrub(&e, hide))?;
    Ok(json!({"removed": clean(name)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_config::Env;

    fn env() -> Env {
        Env::from_pairs::<&str, &str>([])
    }

    #[test]
    fn folders_are_hidden_from_error_text() {
        let home = std::path::PathBuf::from("/home/u/.synt");
        let s = scrub(
            "cannot open /home/u/.synt/synt.json\u{1b}[0m",
            &[&home, Path::new("/")],
        );
        assert_eq!(
            s,
            "cannot open <folder>/synt.json\\u{1b}[0m".replace("\\u{1b}", &clean("\u{1b}"))
        );
        assert!(!s.contains("/home/u"));
    }

    #[test]
    fn a_memory_note_is_checked_scoped_and_forgotten() {
        let d = tempfile::tempdir().unwrap();
        let (home, proj) = (d.path().join("h"), d.path().join("p"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        let e = Env::from_pairs([("SYNT_API_KEY", "sk-live-abcdefgh12345")]);
        let ctx = Ctx {
            home: &home,
            user_home: None,
            env: &e,
            enforcing: true,
        };
        let m = Mem::new(&ctx, &proj, true).unwrap();
        let r = m.add("Tests run with cargo test", false).unwrap();
        assert_eq!(r["scope"], "workspace");
        let rows = m.list().unwrap();
        let id = rows["records"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(rows["records"][0]["trust"], "owner");
        assert_eq!(
            m.retrieve("cargo").unwrap()["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // secrets, empty and oversized text, odd ids and queries are refused
        for t in [
            "key is sk-live-abcdefgh12345 ok",
            "",
            "   ",
            &"x".repeat(MAX_NOTE_CHARS + 1),
            &"a\n".repeat(41),
        ] {
            assert!(m.add(t, false).is_err(), "{t:?}");
        }
        assert!(m.show("../x").is_err() && m.forget("a b").is_err() && m.retrieve("\n").is_err());
        assert!(m.forget("m-nothere").is_err());
        assert!(
            m.show(&id).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("cargo test")
        );
        m.forget(&id).unwrap();
        assert!(m.list().unwrap()["records"].as_array().unwrap().is_empty());
        // an untrusted folder sees and writes only the user scope
        let u = Mem::new(&ctx, &proj, false).unwrap();
        assert_eq!(u.add("my own preference", false).unwrap()["scope"], "user");
        assert!(
            u.list().unwrap()["note"]
                .as_str()
                .unwrap()
                .contains("not trusted")
        );
        assert_eq!(
            m.list().unwrap()["records"].as_array().unwrap().len(),
            1,
            "user notes are seen everywhere"
        );
    }

    #[test]
    fn a_clean_plan_token_is_single_use_and_the_own_session_is_never_listed() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        let e = env();
        let ctx = Ctx {
            home: &home,
            user_home: None,
            env: &e,
            enforcing: true,
        };
        let mut t = Tokens::default();
        let p = clean_plan(&mut t, &ctx, "own").unwrap();
        let plans = p["plans"].as_array().unwrap();
        assert_eq!(plans.len(), 3);
        // no token, a made-up token, or a token of an older plan: nothing is deleted
        for bad in ["", "cl-0", "../x"] {
            assert!(
                clean_apply(&mut t, &ctx, "own", bad, false, &[])
                    .unwrap_err()
                    .contains("no longer valid")
            );
        }
        let old = plans[0]["token"].as_str().unwrap().to_string();
        clean_plan(&mut t, &ctx, "own").unwrap();
        assert!(
            clean_apply(&mut t, &ctx, "own", &old, false, &[]).is_err(),
            "a replaced plan's token is dead"
        );
        let p = clean_plan(&mut t, &ctx, "own").unwrap();
        let tok = p["plans"][0]["token"].as_str().unwrap().to_string();
        assert!(clean_apply(&mut t, &ctx, "own", &tok, false, &[]).is_ok());
        assert!(
            clean_apply(&mut t, &ctx, "own", &tok, false, &[]).is_err(),
            "single use"
        );
    }

    #[test]
    fn a_rewind_needs_its_own_dry_run() {
        let d = tempfile::tempdir().unwrap();
        let (home, user) = (d.path().join("h"), d.path().join("u"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let e = env();
        let ctx = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &e,
            enforcing: true,
        };
        let mut t = Tokens::default();
        // no dry run: refused, and a made-up token is no better
        assert!(
            rewind_apply(&mut t, &ctx, "s1", "rw-1", &[])
                .unwrap_err()
                .contains("dry run")
        );
        // a session that is not there: the error is fixed text with no folder in it
        let err = rewind_dry_run(&mut t, &ctx, "s1", None, &[&home, &user]).unwrap_err();
        assert!(!err.contains(&*d.path().to_string_lossy()), "{err}");
        assert!(
            rewind_dry_run(&mut t, &ctx, "s1", Some("a\nb".into()), &[])
                .unwrap_err()
                .contains("checkpoint")
        );
    }

    #[test]
    fn mcp_over_the_wire_is_a_url_server_and_nothing_that_runs_code() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        let e = env();
        let ctx = Ctx {
            home: &home,
            user_home: None,
            env: &e,
            enforcing: true,
        };
        let add = |name: &str, url: Option<&str>| McpAdd {
            name: name.into(),
            url: url.map(Into::into),
            transport: "http".into(),
            env: vec![],
            header: vec![],
            command: vec![],
        };
        let ok = Some("https://8.8.8.8/mcp");
        // a command (stdio), environment, headers: refused whatever else is right
        let cmd = McpAdd {
            command: vec!["sh".into(), "-c".into(), "id".into()],
            ..add("a", ok)
        };
        assert!(
            mcp_add(&ctx, cmd, &[])
                .unwrap_err()
                .contains("only a URL server")
        );
        let cmd = McpAdd {
            command: vec!["srv".into()],
            ..add("a", None)
        };
        assert!(mcp_add(&ctx, cmd, &[]).is_err());
        for k in [
            "LD_PRELOAD=/x.so",
            "DYLD_INSERT_LIBRARIES=/x",
            "PATH=/tmp",
            "A=b",
        ] {
            let m = McpAdd {
                env: vec![k.into()],
                ..add("a", ok)
            };
            assert!(
                mcp_add(&ctx, m, &[])
                    .unwrap_err()
                    .contains("only a URL server"),
                "{k}"
            );
        }
        let m = McpAdd {
            header: vec!["Authorization: Bearer ${SYNT_API_KEY}".into()],
            ..add("a", ok)
        };
        assert!(mcp_add(&ctx, m, &[]).is_err());
        assert!(
            !home.join("synt.json").exists(),
            "nothing was written by any of these"
        );
        // the URL: https or http only, no expansion, no spaces, name checked
        for u in [
            "file:///etc/passwd",
            "ftp://x/y",
            "https://x/${SYNT_API_KEY}",
            "https://a b",
            "javascript:1",
            "",
        ] {
            assert!(mcp_add(&ctx, add("a", Some(u)), &[]).is_err(), "{u}");
        }
        assert!(
            mcp_add(&ctx, add("../x", ok), &[]).is_err()
                && mcp_add(&ctx, add("a", None), &[]).is_err()
        );
        mcp_add(&ctx, add("a", ok), &[]).unwrap();
        // never replaced (no `force`), and the error hides the folder
        let again = mcp_add(&ctx, add("a", Some("https://1.1.1.1/x")), &[&home]).unwrap_err();
        assert!(
            again.contains("already exists") && !again.contains(&*home.to_string_lossy()),
            "{again}"
        );
        assert!(
            std::fs::read_to_string(home.join("synt.json"))
                .unwrap()
                .contains("8.8.8.8")
        );
        // remove: only a server of the user file; a bad name or a missing one is an error
        assert!(
            mcp_remove(&ctx, None, "a b", &[]).is_err()
                && mcp_remove(&ctx, None, "zzz", &[]).is_err()
        );
        mcp_remove(&ctx, None, "a", &[]).unwrap();
        assert!(mcp_remove(&ctx, None, "a", &[]).is_err());
        // the number of servers added this way is capped
        for i in 0..MAX_USER_SERVERS {
            let _ = mcp_add(&ctx, add(&format!("s{i}"), ok), &[]);
        }
        let full = mcp_add(&ctx, add("one-more", ok), &[]).unwrap_err();
        assert!(full.contains("at most"), "{full}");
    }

    #[test]
    fn context_needs_a_declared_provider_and_hides_the_folder() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        let e = env();
        let ctx = Ctx {
            home: &home,
            user_home: None,
            env: &e,
            enforcing: true,
        };
        let err = context(&ctx, "p/m", "1m", &[&home]).unwrap_err();
        assert!(!err.contains(&*home.to_string_lossy()), "{err}");
        std::fs::write(
            home.join("synt.json"),
            r#"{"providers":{"p":{"models":{"m":{}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            context(&ctx, "p/m", "1m", &[&home]).unwrap()["tokens"],
            1_000_000
        );
        assert!(
            context(&ctx, "p/m", "5", &[&home]).is_err()
                && context(&ctx, "p/m", &"9".repeat(40), &[]).is_err()
        );
        assert!(context(&ctx, "p/m", "unset", &[&home]).unwrap()["tokens"].is_null());
    }
}
