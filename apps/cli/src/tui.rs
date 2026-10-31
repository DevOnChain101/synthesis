//! `synth` / `synth tui`: the interactive app over the same harness wiring as `synth run`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Args;
use synthesis_config::Env;
use synthesis_core::TaskStatus;
use synthesis_harness::TaskOutcome;
use synthesis_tui::{ExitReason, OnceOptions, Options, Tty, load_resume};
use synthesis_tui_base::Platform;
use synthesis_tui_base::sgr::ColorMode;

use synthesis_app::homes;
use synthesis_app::run::{
    Common, Front, Limits, NO_MODEL, Prepared, prepare, sandbox_default_notice, sandbox_notices,
    sandbox_status,
};

#[derive(Args)]
pub struct TuiArgs {
    /// Start folder (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Reopen a saved session (id or unique prefix).
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,
    /// Print one frame to stdout and exit (no terminal needed).
    #[arg(long)]
    pub once: bool,
    #[command(flatten)]
    pub common: Common,
}

const USAGE: &str = "synth: the interactive app needs a terminal on stdin and stdout.\nUsage: synth [--cwd DIR] [--resume ID] [--once] [--worktree] [--sandbox MODE] [--network MODE] ...\n       synth run [--cwd DIR] --prompt TEXT   (headless; events as JSONL)\n       synth --help";

fn start_dir(a: &TuiArgs) -> Result<PathBuf, String> {
    // `--cwd DIR`, else the directory `synth` was started in (the only place the process cwd is read).
    let d = match a.cwd.as_ref() {
        Some(d) => d.clone(),
        None => synthesis_app::launch_dir()?,
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `SYNTHESIS_ONCE_SCREEN=config[:panels|appearance|behaviour|mouse]` makes `--once` draw the
/// `/config` screen: the headless way to look at it (docs/CLI.md).
fn config_group(env: &Env) -> Option<usize> {
    let v = env.get_nonempty("SYNTHESIS_ONCE_SCREEN")?;
    let group = v.strip_prefix("config")?;
    match group.strip_prefix(':') {
        None if group.is_empty() => Some(0),
        Some("panels") => Some(0),
        Some("appearance") => Some(1),
        Some("behaviour") => Some(2),
        Some("mouse") => Some(3),
        _ => None,
    }
}

fn once(a: &TuiArgs, env: &Env) -> Result<u8, String> {
    let h = homes(env)?;
    let resume = a
        .resume
        .as_deref()
        .map(|id| load_resume(&h.synthesis, id))
        .transpose()?;
    let cwd = match &resume {
        Some(r) => r.record.cwd.clone(),
        None => start_dir(a)?.to_string_lossy().into_owned(),
    };
    let (cols, rows) = synthesis_tui::size_from_env(env);
    let (tui, tui_notes) = synthesis_tui::TuiSettings::load_user(&h.synthesis);
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
            .chain(tui_notes)
            .collect(),
        tui,
        env: env.clone(),
        config_group: config_group(env),
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
    let mut cwd = start_dir(&a)?;
    let granted = root_of(&cwd);
    let mut resume = a
        .resume
        .as_deref()
        .map(|id| load_resume(&h.synthesis, id))
        .transpose()?;
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
        if model.is_some() {
            common.model.clone_from(&model);
        }
        let p = match prepare(
            &common,
            &cwd,
            &env,
            &Limits::default(),
            Front::Tui,
            resume.as_ref(),
        ) {
            Ok(p) => p,
            Err(e) if switch_from.is_none() && is_first_run(&e) => {
                // nothing is configured yet: the app starts anyway, with a setup card
                let (tui, tui_notes) = synthesis_tui::TuiSettings::load_user(&h.synthesis);
                let mut o = synthesis_tui::SetupOptions::new(
                    cwd.to_string_lossy().into_owned(),
                    env!("CARGO_PKG_VERSION").into(),
                    env.clone(),
                    setup_card(&env, &h.synthesis, &cwd, &e),
                );
                o.tui = tui;
                o.tui_diagnostics = tui_notes;
                o.synthesis_home = Some(h.synthesis.clone());
                o.saved = synthesis_tui::list_saved(&h.synthesis, None).unwrap_or_default();
                match synthesis_tui::run_setup(o, Tty::stdio(&env)).map_err(|e| e.to_string())? {
                    ExitReason::OpenFolder(p) => {
                        cwd = synthesis_core::canonicalize(&p)
                            .map_err(|e| format!("{}: {e}", p.display()))?;
                        resume = None;
                    }
                    ExitReason::Resume(id) => resume = Some(load_resume(&h.synthesis, &id)?),
                    _ => return Ok(0),
                }
                continue;
            }
            Err(e) => {
                let Some((prev, sid)) = switch_from.take() else {
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
        let Prepared {
            harness,
            session,
            sid,
            link,
            tokio,
            rt,
            mcp,
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
        let mut o = Options::new(
            harness.clone(),
            session,
            link.unwrap_or_default(),
            tokio.handle().clone(),
            env.clone(),
        );
        o.version = env!("CARGO_PKG_VERSION").into();
        o.model = Some(label);
        o.models = models;
        o.effort_tiers = effort_tiers;
        o.sandbox = sandbox;
        o.startup_notes = notices;
        let (tui, tui_notes) = synthesis_tui::TuiSettings::load_user(&ph.synthesis);
        o.tui = tui;
        o.tui_diagnostics = tui_notes.clone();
        o.startup_notes.extend(tui_notes);
        o.synthesis_home = Some(ph.synthesis.clone());
        o.memory = Some(notes.hook());
        o.startup_notes.extend(switch_note.take());
        switch_from = None;
        if let Some(m) = &mcp {
            let (report, status) = (m.clone(), m.clone());
            o.mcp_report = Some(Box::new(move || mcp_lines(&report)));
            o.mcp_status = Some(Box::new(move || mcp_live(&status)));
        }
        o.saved = synthesis_tui::list_saved(&ph.synthesis, None).unwrap_or_default();
        o.resume = resume.take();
        let exit = synthesis_tui::run_interactive(o, Tty::stdio(&env)).map_err(|e| e.to_string());
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
        harness.end_session(&outcome);
        rt.close_session(&sid);
        if let Some(m) = &mcp {
            tokio.block_on(m.close());
        }
        if let Some(w) = &worktree {
            eprintln!("synth: {}", w.finish(a.common.cleanup_worktree));
        }
        drop(harness);
        match exit?.reason {
            ExitReason::OpenFolder(p) => {
                cwd = synthesis_core::canonicalize(&p)
                    .map_err(|e| format!("{}: {e}", p.display()))?;
            }
            ExitReason::Resume(id) => resume = Some(load_resume(&ph.synthesis, &id)?),
            ExitReason::SwitchModel(m) => {
                switch_from = Some((model.replace(m), sid.clone()));
                resume = load_resume(&ph.synthesis, &sid).ok();
            }
            ExitReason::Quit | ExitReason::InputClosed => return Ok(0),
        }
    }
}

/// Start errors that mean "nothing is set up yet": the interactive app opens its first-run card
/// instead of exiting (`synth run` keeps failing with exit 2).
fn is_first_run(e: &str) -> bool {
    e.starts_with(NO_MODEL)
        || e.starts_with("no provider configured")
        || e.starts_with("no API key for provider")
}

/// The first-run card: what to do, which providers and variables were found (names only, never
/// values), and copyable examples. Lines indented by four spaces are examples.
pub(crate) fn setup_card(env: &Env, home: &Path, cwd: &Path, reason: &str) -> Vec<String> {
    use synthesis_config::{ProjectTrust, resolve_credential};
    let mut found = vec![];
    let h = synthesis_app::homes(env).ok();
    if let Some(cfg) = h
        .as_ref()
        .and_then(|h| synthesis_app::load_config(env, h, Some(cwd), ProjectTrust::Untrusted).ok())
    {
        for (id, p) in &cfg.providers {
            let key = resolve_credential(id, Some(p), home, env).is_some() || p.is_no_auth();
            found.push(format!(
                "{id} ({})",
                if key { "key found" } else { "no key" }
            ));
        }
    }
    let vars: Vec<&str> = [
        "SYNTHESIS_PROVIDER",
        "SYNTHESIS_MODEL",
        "SYNTHESIS_API_KEY",
        "SYNTHESIS_AUTH_TOKEN",
        "SYNTHESIS_BASE_URL",
    ]
    .into_iter()
    .filter(|n| env.get_nonempty(n).is_some())
    .collect();
    let file = home.join("synthesis.json");
    let ex = |s: &str| format!("    {s}");
    vec![
        "Synthesis needs a model and an API key before it can run a task. Nothing has been sent anywhere.".into(),
        format!("why: {reason}"),
        format!(
            "found: providers {} · variables set {} · config file {}",
            if found.is_empty() { "none".to_string() } else { found.join(", ") },
            if vars.is_empty() { "none".to_string() } else { vars.join(", ") },
            if file.exists() { "present" } else { "missing" }
        ),
        format!("1. Create {} with a provider and a model. An OpenAI-compatible gateway:", file.display()),
        ex("{"),
        ex("  \"$schema\": \"config.json/v1\","),
        ex("  \"model\": \"gateway/my-model\","),
        ex("  \"providers\": {\"gateway\": {\"type\": \"openai\", \"baseURL\": \"https://llm.example.com/v1\","),
        ex("    \"apiKey\": \"{env:SYNTHESIS_API_KEY}\", \"models\": [\"my-model\"]}}"),
        ex("}"),
        "   Anthropic: \"model\": \"anthropic/MODEL_ID\" and \"providers\": {\"anthropic\": {\"apiKey\": \"{env:SYNTHESIS_API_KEY}\"}} (`synth models` lists the ids your key can use).".into(),
        "2. Export the key in the shell that starts synth (Synthesis never writes or stores keys):".into(),
        ex("export SYNTHESIS_API_KEY=..."),
        "3. Check it from a shell: `synth auth status` shows what was found, `synth models` lists the models.".into(),
        "4. Type /reload here (or restart synth). Later, /model provider/model picks another model.".into(),
        "Works now: /config (panels, icons) · /themes · /help · /exit. Anything else you type is not sent.".into(),
    ]
}

/// What the panels show of the MCP servers: name, state, tool count, last error.
fn mcp_live(m: &synthesis_mcp::McpManager) -> Vec<synthesis_tui::McpLive> {
    use synthesis_harness::sanitize_for_display as clean;
    m.status()
        .into_iter()
        .map(|s| synthesis_tui::McpLive {
            name: clean(&s.name),
            state: format!("{:?}", s.state).to_lowercase(),
            tool_count: s.tool_count as u32,
            error: s.error.as_deref().map(clean).unwrap_or_default(),
        })
        .collect()
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
            let p = prepare(&c, cwd, &env, &Limits::default(), Front::Tui, None).unwrap();
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
