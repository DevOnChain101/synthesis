//! `synth` / `synth tui`: the interactive app over the same harness wiring as `synth run`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use synthesis_config::Env;
use synthesis_core::TaskStatus;
use synthesis_harness::TaskOutcome;
use synthesis_tui::{Exit, ExitReason, OnceOptions, Options, Resume, Tty, load_resume};
use synthesis_tui_base::Platform;
use synthesis_tui_base::sgr::ColorMode;

use serde_json::json;
use synthesis_app::run::{
    Common, Front, Limits, Prepared, finish_store, prepare, sandbox_default_notice,
    sandbox_notices, sandbox_status,
};
use synthesis_app::{homes, launch_dir};
use synthesis_core::AgentSession;
use synthesis_events::Event;
use synthesis_harness::{EventSink, Harness, Scope, sanitize_for_display};
use synthesis_hooks::HookEvent;
use synthesis_tui::{BoxFut, TaskHost};
use tokio_util::sync::CancellationToken;

#[derive(Args)]
pub struct TuiArgs {
    /// Start folder (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Print one frame to stdout and exit (no terminal needed).
    #[arg(long)]
    pub once: bool,
    #[command(flatten)]
    pub common: Common,
}

const USAGE: &str = "synth: the interactive app needs a terminal on stdin and stdout.\nUsage: synth [--cwd DIR] [--resume ID | --continue] [--once] [--worktree] [--sandbox MODE] [--network MODE] ...\n       synth run --cwd DIR --prompt TEXT   (headless; events as JSONL)\n       synth --help";

fn start_dir(a: &TuiArgs) -> Result<PathBuf, String> {
    let d = match &a.cwd {
        Some(d) => d.clone(),
        None => launch_dir()?,
    };
    synthesis_core::canonicalize(&d).map_err(|e| format!("--cwd {}: {e}", d.display()))
}

/// Canonical project root of `cwd`.
fn root_of(cwd: &Path) -> PathBuf {
    let r = synthesis_settings::find_project_root(cwd);
    synthesis_core::canonicalize(&r).unwrap_or(r)
}

/// `--trust-project` / `--approve-mcp` were granted for ONE project root. After OpenFolder or
/// Resume moved to another root they are dropped (Untrusted, no re-prompt in v1: relaunch with
/// --cwd for that project to trust it).
fn common_for(c: &Common, granted: &Path, cwd: &Path) -> Common {
    let mut c = c.clone();
    if c.trust_project && root_of(cwd) != granted {
        eprintln!(
            "synth: project root changed; --trust-project and --approve-mcp apply only to {}, so this project is untrusted",
            synthesis_harness::sanitize_for_display(&granted.display().to_string())
        );
        c.trust_project = false;
        c.approve_mcp.clear();
    }
    c
}

/// `--resume ID` (id or unique prefix) or `--continue` (newest session of the start folder).
fn requested_resume(a: &TuiArgs, h: &synthesis_app::Homes) -> Result<Option<Resume>, String> {
    if a.common.fork_session {
        return Err("--fork-session is only for `synth run`".into());
    }
    let id = match (&a.common.resume, a.common.continue_session) {
        (Some(id), _) => id.clone(),
        (None, true) => {
            let cwd = start_dir(a)?.to_string_lossy().into_owned();
            synthesis_tui::list_saved(&h.synthesis, Some(&cwd))?
                .into_iter()
                .next()
                .map(|s| s.id)
                .ok_or("--continue: no saved session for this directory")?
        }
        (None, false) => return Ok(None),
    };
    load_resume(&h.synthesis, &id).map(Some)
}

/// No model configured: the setup card (the key itself stays in an environment variable or a file
/// the user manages: nothing is asked for or stored here).
fn first_run(env: &Env, why: &str) -> std::io::Result<synthesis_tui::SetupExit> {
    let lines = vec![
        "Synthesis needs a model and an API key before it can run a task.".to_string(),
        format!("why: {why}"),
        String::new(),
        "1. Keep the key in an environment variable you manage (never in a repository):".into(),
        "    export MY_PROVIDER_KEY=...".into(),
        "2. Point your user config at it, in $SYNTHESIS_HOME/synthesis.json (~/.synthesis by default):".into(),
        r#"    {"$schema": "config.json/v1", "model": "anthropic/<model>","#.into(),
        r#"     "providers": {"anthropic": {"apiKey": "{env:MY_PROVIDER_KEY}"}}}"#.into(),
        "3. `synth models` lists what is configured, `synth doctor` checks the installation;".into(),
        "   or start once with `synth --model provider/model`.".into(),
    ];
    synthesis_tui::run_setup(
        synthesis_tui::SetupOptions {
            version: env!("CARGO_PKG_VERSION").into(),
            env: env.clone(),
            lines,
        },
        Tty::stdio(env),
    )
}

/// Retention of stored images (`synthesis.images.retentionDays`, default 30): done once at start,
/// before the screen is taken over. Failures are a warning, never a reason not to start.
fn purge_old_images(synthesis_home: &Path) {
    let (days, diag) =
        synthesis_app::ctx::read_image_retention(&synthesis_home.join("settings.json"));
    for d in diag {
        eprintln!("synth: warning: {d}");
    }
    if days == 0 {
        return;
    }
    let store =
        synthesis_session::Store::open(synthesis_home, synthesis_session::StoreOptions::default());
    match store.and_then(|s| s.purge_images(days, time::OffsetDateTime::now_utc())) {
        Ok(0) => {}
        Ok(n) => eprintln!("synth: removed {n} stored image(s) older than {days} days"),
        Err(e) => eprintln!("synth: warning: image retention: {e}"),
    }
}

/// Minutes east of UTC for the greeting ("Good morning"); 0 when the zone cannot be read. No
/// std or dependency gives the local zone safely, so unix asks `date`; Windows greets by UTC.
fn local_offset_min() -> i32 {
    if !cfg!(unix) {
        return 0;
    }
    std::process::Command::new("/bin/date")
        .arg("+%z")
        .output()
        .map_or(0, |o| zone_minutes(&String::from_utf8_lossy(&o.stdout)))
}

/// `+0530` / `-0800` (what `date +%z` prints) as minutes east of UTC.
fn zone_minutes(t: &str) -> i32 {
    let t = t.trim();
    let (sign, d) = t.split_at(t.len().min(1));
    match (sign, d.len() == 4, d.parse::<i32>()) {
        ("+", true, Ok(n)) => n / 100 * 60 + n % 100,
        ("-", true, Ok(n)) => -(n / 100 * 60 + n % 100),
        _ => 0,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn once(a: &TuiArgs, env: &Env) -> Result<u8, String> {
    let h = homes(env)?;
    let resume = requested_resume(a, &h)?;
    let cwd = match &resume {
        Some(r) => r.record.cwd.clone(),
        None => start_dir(a)?.to_string_lossy().into_owned(),
    };
    let (cols, rows) = synthesis_tui::size_from_env(env);
    let o = OnceOptions {
        cwd,
        session_id: resume
            .as_ref()
            .map_or_else(String::new, |r| r.record.id.clone()),
        model: resume
            .as_ref()
            .map_or_else(String::new, |r| r.record.model_config_id.clone()),
        version: env!("CARGO_PKG_VERSION").into(),
        budget: resume
            .as_ref()
            .map(|r| r.record.budget.clone())
            .unwrap_or_default(),
        sandbox: sandbox_status(a.common.sandbox_mode(), false),
        mode: synthesis_permissions::Mode::Default,
        events: resume.map(|r| r.events).unwrap_or_default(),
        saved: synthesis_tui::list_saved(&h.synthesis, None).unwrap_or_default(),
        cols,
        rows,
        now: now_ms(),
        color: ColorMode::detect(env, Platform::current()),
        // --once starts nothing (no MCP, no agents): only the flag warnings apply.
        startup_notes: sandbox_notices(a.common.sandbox_mode(), a.common.network)
            .into_iter()
            .chain(sandbox_default_notice(&a.common))
            .collect(),
    };
    // colour escapes belong to a terminal: into a pipe or file the frame is plain text
    let mut frame = Vec::new();
    let res = synthesis_tui::render_once(&o, &mut frame).and_then(|()| {
        let text = String::from_utf8_lossy(&frame);
        let text = if std::io::stdout().is_terminal() {
            text.into_owned()
        } else {
            synthesis_tui_base::text::strip_ansi(&text)
        };
        std::io::Write::write_all(&mut std::io::stdout(), text.as_bytes())
    });
    match res {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.to_string()),
        _ => Ok(0),
    }
}

pub fn launch(a: TuiArgs, env: Env) -> Result<u8, String> {
    if a.once {
        return once(&a, &env);
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    let h = homes(&env)?;
    purge_old_images(&h.synthesis);
    let mut cwd = start_dir(&a)?;
    let granted = root_of(&cwd);
    let mut resume = requested_resume(&a, &h)?;
    // `/model NAME` restarts on the same session with that model; a model that cannot be built
    // falls back to the one in use, with a note.
    let mut model: Option<String> = None;
    let mut switch_from: Option<(Option<String>, String)> = None;
    let mut switch_note: Option<String> = None;
    loop {
        if let Some(r) = &resume {
            cwd = PathBuf::from(&r.record.cwd);
        }
        let mut common = common_for(&a.common, &granted, &cwd);
        // the stored session continues in place (workspace, history, store); the picker may name
        // a session of another folder, which is why the workspace check is off
        common.resume = resume.as_ref().map(|r| r.record.id.clone());
        common.continue_session = false;
        common.resume_any_workspace = resume.is_some();
        if model.is_some() {
            common.model.clone_from(&model);
        }
        let p = match prepare(&common, &cwd, &env, &Limits::default(), Front::Tui) {
            Ok(p) => p,
            Err(e) => {
                let Some((prev, sid)) = switch_from.take() else {
                    // nothing configured yet: a setup card instead of an error exit
                    if e.starts_with("no model configured") {
                        match first_run(&env, &e) {
                            Ok(synthesis_tui::SetupExit::Reload) => continue,
                            Ok(synthesis_tui::SetupExit::Quit) => return Ok(0),
                            Err(io) => return Err(format!("{e} ({io})")),
                        }
                    }
                    return Err(e);
                };
                switch_note = Some(format!(
                    "/model: could not switch ({e}); staying on the previous model"
                ));
                model = prev;
                resume = load_resume(&h.synthesis, &sid).ok();
                continue;
            }
        };
        let (exit, sid, ph_synthesis) = round(
            p,
            a.common.cleanup_worktree,
            &env,
            switch_note.take(),
            resume.take(),
            &mut |o| synthesis_tui::run_interactive(o, Tty::stdio(&env)),
        );
        switch_from = None;
        match exit?.reason {
            ExitReason::OpenFolder(p) => {
                cwd = synthesis_core::canonicalize(&p)
                    .map_err(|e| format!("{}: {e}", p.display()))?;
            }
            ExitReason::Resume(id) => resume = Some(load_resume(&ph_synthesis, &id)?),
            ExitReason::SwitchModel(m) => {
                switch_from = Some((model.replace(m), sid.clone()));
                resume = load_resume(&ph_synthesis, &sid).ok();
            }
            ExitReason::Quit | ExitReason::InputClosed => return Ok(0),
        }
    }
}

/// One run of the app on a prepared session: start it, run the UI, then end it whatever
/// happened (also when it could not start): `session.completed`, MCP servers closed, worktree
/// reported. Returns the UI's result, the session id and the synthesis home.
fn round(
    p: Prepared,
    cleanup_worktree: bool,
    env: &Env,
    switch_note: Option<String>,
    resume: Option<Resume>,
    ui: &mut dyn FnMut(Options) -> std::io::Result<Exit>,
) -> (Result<Exit, String>, String, PathBuf) {
    let Prepared {
        harness,
        context,
        skills,
        sink,
        mut session,
        sid,
        link,
        tokio,
        rt,
        store,
        transcript,
        mcp,
        memory,
        resumed_from_seq,
        worktree,
        label,
        models,
        effort_tiers,
        sandbox,
        notices,
        homes: ph,
        notes,
        ..
    } = p;
    // Started here (not by the app) so the session hooks and `memory.retrieved` follow
    // `session.started`, as in `synth run`.
    let started = match resumed_from_seq {
        Some(seq) => {
            harness.resume_session(&session, seq);
            Ok(())
        }
        None => harness.start_session(&mut session, "startup"),
    };
    let began = started.is_ok();
    // a start that failed skips the UI but not the shutdown below
    let exit = 'ui: {
        if let Err(e) = started {
            break 'ui Err(e.to_string());
        }
        let source = if resumed_from_seq.is_some() {
            "resume"
        } else {
            "startup"
        };
        tokio.block_on(harness.session_hook(
            HookEvent::SessionStart,
            json!({"source": source}),
            Some(source),
        ));
        if let Some(m) = &memory {
            synthesis_app::memcmd::announce(m);
        }
        let mut o = Options::new(
            harness.clone(),
            session,
            link.unwrap_or_default(),
            tokio.handle().clone(),
            env.clone(),
        );
        o.extra_commands = skills
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.description.clone(),
                    "skill",
                    String::new(),
                )
            })
            .chain(mcp.iter().flat_map(|m| m.prompts()).map(|p| {
                let name = format!(
                    "mcp__{}__{}",
                    synthesis_app::mcpinput::sanitize_server(&p.server),
                    p.name
                );
                let hint = p
                    .arguments
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>();
                (name, p.description, "MCP prompt", hint.join(" "))
            }))
            .map(
                |(name, desc, kind, hint)| synthesis_tui_base::complete::Command {
                    name: sanitize_for_display(&name),
                    aliases: vec![],
                    description: sanitize_for_display(&format!("{kind}: {desc}")),
                    argument_hint: sanitize_for_display(&hint),
                    local: false,
                },
            )
            .collect();
        o.host = Some(Arc::new(Host {
            mcp: mcp.clone(),
            memory: memory.clone(),
            context,
            harness: harness.clone(),
            sink,
            store: store.clone(),
            transcript: transcript.clone(),
            skills: skills.iter().map(|s| s.name.clone()).collect(),
        }));
        o.version = env!("CARGO_PKG_VERSION").into();
        o.tz_offset_min = local_offset_min();
        let (tui, tui_diag) = synthesis_settings::read_user_tui(&ph.synthesis);
        o.tui = tui;
        o.tui_diagnostics = tui_diag;
        o.synthesis_home = Some(ph.synthesis.clone());
        o.model = Some(label);
        o.models = models;
        o.effort_tiers = effort_tiers;
        o.sandbox = sandbox;
        o.startup_notes = notices;
        o.memory = Some(notes.hook());
        o.startup_notes.extend(switch_note);
        if let Some(m) = &mcp {
            let m = m.clone();
            o.mcp_report = Some(Box::new(move || mcp_lines(&m)));
        }
        o.saved = synthesis_tui::list_saved(&ph.synthesis, None).unwrap_or_default();
        o.resume = resume;
        ui(o).map_err(|e| e.to_string())
    };
    // The harness ends the session whatever the UI returned.
    let outcome = exit
        .as_ref()
        .ok()
        .and_then(|e| e.last_outcome.clone())
        .unwrap_or(TaskOutcome {
            task_id: String::new(),
            status: TaskStatus::Completed,
            reason: "closed".into(),
            verification_report_id: None,
            final_text: String::new(),
            usage: synthesis_events::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
            changed_files: vec![],
            needed_permissions: vec![],
            messages: vec![],
        });
    let mut outcome = outcome;
    if !began {
        outcome.status = TaskStatus::Failed;
        outcome.reason = "session_start_failed".into();
    }
    if began {
        tokio.block_on(harness.session_hook(
            HookEvent::SessionEnd,
            json!({"reason": outcome.reason}),
            Some(&outcome.reason),
        ));
    }
    // the usage of the tasks that ran (the app returns the session unless a task outlived it)
    if let (
        Some(s),
        Ok(Exit {
            session: Some(sess),
            last_outcome: Some(out),
            ..
        }),
    ) = (&store, &exit)
    {
        finish_store(s, transcript.as_deref(), out, &sess.usage);
    }
    harness.end_session(&outcome);
    if let Some(t) = &transcript {
        t.clear_notifier(); // break the sink <-> transcript cycle: the next session needs the store lock
    }
    rt.close_session(&sid);
    if let Some(m) = &mcp {
        tokio.block_on(m.close());
    }
    if let Some(w) = &worktree {
        eprintln!("synth: {}", w.finish(cleanup_worktree));
    }
    drop(harness);
    // the global hook holds the memory run, and with it the event sink and the session
    // store's write lock: the restarted session (model switch, resume) must be able to open it
    synthesis_tools::set_learned_skills(None);
    (exit, sid, ph.synthesis)
}

/// What `synth run` does around its task, for every task the person starts in the app:
/// MCP mentions and prompts, skills, the memory block for the typed prompt, the memory episode
/// afterwards and the stored usage.
struct Host {
    mcp: Option<Arc<synthesis_mcp::McpManager>>,
    memory: Option<Arc<synthesis_app::memcmd::MemoryRun>>,
    context: Arc<synthesis_app::ctx::SynthesisContext>,
    harness: Arc<Harness>,
    sink: EventSink,
    store: Option<synthesis_app::events::Store>,
    transcript: Option<Arc<synthesis_app::transcript::Transcript>>,
    skills: Vec<String>,
}

impl Host {
    /// `/skill-name args` runs that skill: the model is told to load it (it is not typed text it
    /// could mistake for a command).
    fn skill_request(&self, typed: &str) -> Option<String> {
        let rest = typed.trim_start().strip_prefix('/')?;
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        self.skills.iter().any(|s| s == name).then(|| {
            let args = args.trim();
            if args.is_empty() {
                format!("Run the skill \"{name}\" with the Skill tool.")
            } else {
                format!("Run the skill \"{name}\" with the Skill tool. Arguments: {args}")
            }
        })
    }
}

impl TaskHost for Host {
    fn before<'a>(
        &'a self,
        typed: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Result<String, String>> {
        Box::pin(async move {
            if let Some(r) = self.skill_request(typed) {
                return Ok(r);
            }
            let (request, notes) =
                synthesis_app::mcpinput::expand(self.mcp.as_ref(), typed, cancel).await?;
            for n in notes {
                self.sink.emit(
                    &Scope::default(),
                    Event::Notice {
                        level: "warning".into(),
                        text: sanitize_for_display(&n),
                    },
                );
            }
            if let Some(m) = &self.memory {
                self.context
                    .set_memory(synthesis_app::memcmd::retrieve_for_task(m, typed));
            }
            Ok(request)
        })
    }

    fn after<'a>(
        &'a self,
        typed: &'a str,
        out: &'a synthesis_harness::TaskOutcome,
        session: &'a AgentSession,
    ) -> BoxFut<'a, ()> {
        Box::pin(async move {
            if let Some(m) = &self.memory {
                let verified = self.harness.verified_commands(&out.task_id);
                synthesis_app::memcmd::task_ended(m, typed, out, &verified);
                // reflection is a model call (opt-in): the screen does not wait for it
                if m.reflect.is_some() {
                    let m = m.clone();
                    tokio::spawn(async move {
                        if let Some(rf) = &m.reflect {
                            synthesis_app::memcmd::reflect_jobs(&m.store, rf, 2).await;
                        }
                    });
                }
            }
            if let Some(s) = &self.store {
                finish_store(s, self.transcript.as_deref(), out, &session.usage);
            }
        })
    }
}

/// `/mcp`: one block per server with state, tools and the last error.
fn mcp_lines(m: &synthesis_mcp::McpManager) -> Vec<String> {
    use synthesis_harness::sanitize_for_display as clean;
    let status = m.status();
    if status.is_empty() {
        return vec!["MCP: no servers are configured for this session".into()];
    }
    let tools = m.tools();
    let mut v = vec![format!("MCP servers ({}):", status.len())];
    for s in status {
        let state = format!("{:?}", s.state).to_lowercase();
        v.push(format!(
            "  {} · {state} · {}{} · {} tools",
            clean(&s.name),
            s.transport,
            s.scope
                .as_deref()
                .map_or(String::new(), |c| format!(" · {}", clean(c))),
            s.tool_count
        ));
        if let Some(e) = &s.error {
            v.push(format!("    error: {}", clean(e)));
        }
        for t in tools.iter().filter(|t| t.server == s.name && !t.synthetic) {
            v.push(format!(
                "    {} · {}",
                clean(&t.tool),
                clean(t.description.lines().next().unwrap_or(""))
            ));
        }
        if matches!(s.state, synthesis_mcp::ServerState::Failed) {
            v.extend(
                m.stderr_tail(&s.name)
                    .iter()
                    .rev()
                    .take(3)
                    .rev()
                    .map(|l| format!("    stderr: {}", clean(l))),
            );
        }
    }
    v
}

#[cfg(test)]
mod zone_tests {
    #[test]
    fn date_offsets_parse() {
        assert_eq!(super::zone_minutes("+0700\n"), 420);
        assert_eq!(super::zone_minutes("-0530"), -330);
        assert_eq!(super::zone_minutes("garbage"), 0);
        assert_eq!(super::zone_minutes(""), 0);
    }
}

#[cfg(all(test, unix, feature = "test-provider"))]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: Common,
    }

    fn hostile_repo(dir: &Path, marker: &Path) {
        let w = |f: &str, t: String| {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        let srv = json!({"command": "sh", "args": ["-c", format!("touch {}", marker.display())]});
        w(
            ".mcp.json",
            json!({"mcpServers": {"proj": srv}}).to_string(),
        );
        w(
            ".synthesis/config.json",
            json!({"$schema": "config.json/v1", "mcp": {"cfg": srv}}).to_string(),
        );
        w(".synthesis/settings.json", json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": format!("touch {}", marker.display())}]}]}, "enableAllProjectMcpServers": true}).to_string());
        w(
            ".synthesis/agents/evil.md",
            "---\nname: evil\ndescription: d\n---\nbody".into(),
        );
    }

    #[test]
    fn trust_does_not_follow_a_relaunch_to_another_project() {
        let d = tempfile::tempdir().unwrap();
        let root = synthesis_core::canonicalize(d.path()).unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        let (ma, mb) = (root.join("marker-a"), root.join("marker-b"));
        for (r, m) in [(&a, &ma), (&b, &mb)] {
            std::fs::create_dir_all(r.join(".git")).unwrap();
            hostile_repo(r, m);
        }
        let script = root.join("s.json");
        std::fs::write(&script, "[{\"text\":\"ok\"}]").unwrap();
        let provider = format!("fake:{}", script.display());
        let w = W::parse_from([
            "t",
            "--provider",
            &provider,
            "--sandbox",
            "off", // not a sandbox test: must not depend on the host
            "--trust-project",
            "--approve-mcp",
            "proj",
            "--no-session-store",
        ]);
        let env = Env::from_pairs([
            ("HOME", root.to_string_lossy().into_owned()),
            (
                "SYNTHESIS_HOME",
                root.join("synthesis").to_string_lossy().into_owned(),
            ),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
        ]);
        let granted = root_of(&a);
        let run = |cwd: &Path| {
            let c = common_for(&w.c, &granted, cwd);
            let p = prepare(&c, cwd, &env, &Limits::default(), Front::Tui).unwrap();
            (c.trust_project, p.mcp.is_some())
        };
        // the project it was granted for keeps it (positive control: the server starts)
        assert_eq!(run(&a), (true, true));
        assert!(ma.exists());
        // OpenFolder/Resume to a second hostile repo: untrusted, nothing of it is used
        assert_eq!(run(&b), (false, false));
        assert!(!mb.exists(), "hostile repo must not run anything");
    }
}

#[cfg(all(test, unix, feature = "test-provider"))]
mod start_failure_tests {
    use super::*;
    use clap::Parser;
    use std::process::Command;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: Common,
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A session that cannot start (here: already started) must still be ended, its servers
    /// closed and its worktree reported / cleaned: `return Err` used to skip all of it.
    #[test]
    fn a_session_that_cannot_start_is_still_ended_and_its_worktree_cleaned() {
        failed_start(false);
    }

    /// `--cleanup-worktree` must not delete gitignored files (.env, build output): `git worktree
    /// remove` would, so the worktree is kept and its path reported.
    #[test]
    fn a_failed_start_keeps_a_worktree_with_gitignored_files() {
        failed_start(true);
    }

    fn failed_start(with_ignored: bool) {
        let d = tempfile::tempdir().unwrap();
        let root = synthesis_core::canonicalize(d.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("f"), "x").unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        if !(git(&repo, &["init", "-q"])
            && git(&repo, &["add", "."])
            && git(&repo, &["commit", "-qm", "c"]))
        {
            return; // no git here: nothing to test
        }
        let script = root.join("s.json");
        std::fs::write(&script, "[{\"text\":\"ok\"}]").unwrap();
        let provider = format!("fake:{}", script.display());
        let w = W::parse_from([
            "t",
            "--provider",
            &provider,
            "--sandbox",
            "off",
            "--no-memory",
            "--worktree",
            "--cleanup-worktree",
        ]);
        let env = Env::from_pairs([
            ("HOME", root.to_string_lossy().into_owned()),
            (
                "SYNTHESIS_HOME",
                root.join("synthesis").to_string_lossy().into_owned(),
            ),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
        ]);
        let mut p = prepare(&w.c, &repo, &env, &Limits::default(), Front::Tui).unwrap();
        let wt = p.worktree.as_ref().unwrap().info.worktree.clone();
        assert!(wt.exists());
        if with_ignored {
            std::fs::write(wt.join(".env"), "TOKEN=1").unwrap();
        }
        p.harness.start_session(&mut p.session, "startup").unwrap(); // the app's own start now fails
        let (exit, _sid, home) = round(p, true, &env, None, None, &mut |_| {
            panic!("the UI must not run for a session that did not start")
        });
        assert!(exit.is_err());
        if with_ignored {
            assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "TOKEN=1");
        } else {
            assert!(!wt.exists(), "worktree cleanup was skipped");
        }
        let c = rusqlite::Connection::open(home.join("synthesis.db")).unwrap();
        let ended: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'session.failed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, 1, "end_session was skipped");
    }
}
