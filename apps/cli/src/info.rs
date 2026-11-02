//! `auth status`, `models`, `doctor`. Output is for humans (or `--json`); never prints a secret.

use std::ffi::OsString;
use std::path::Path;

use clap::Args;
use serde_json::json;
use synthesis_config::{Config, Env, ProjectTrust, list_models, resolve_credential};
use synthesis_harness::sanitize_for_display as san;
use synthesis_providers::{NetSettings, ProviderSpec, RetryPolicy};
use synthesis_sandbox::SandboxBackend;
use tokio_util::sync::CancellationToken;

use synthesis_app::{homes, load_config};

pub fn auth_status(env: &Env, cwd: Option<&Path>) -> Result<u8, String> {
    let h = homes(env)?;
    let cfg = load_config(env, &h, cwd, ProjectTrust::Untrusted)?;
    let flat = env
        .get_nonempty("SYNTHESIS_PROVIDER")
        .unwrap_or("anthropic");
    out!("SYNTHESIS_HOME: {}", h.synthesis.display());
    out!("flat SYNTHESIS_API_KEY / SYNTHESIS_AUTH_TOKEN apply to provider: {flat}");
    for (id, p) in &cfg.providers {
        match resolve_credential(id, Some(p), &h.synthesis, env) {
            Some(c) => out!("{id}: {} (source: {})", c.masked(), c.source),
            None if p.is_no_auth() => out!("{id}: no authentication required"),
            None => out!("{id}: not configured"),
        }
    }
    Ok(0)
}

#[derive(Args)]
pub struct ModelsArgs {
    #[command(flatten)]
    pub cwd: synthesis_app::CwdArg,
    #[arg(long)]
    json: bool,
    /// Ask each provider with a credential for its model list (network).
    #[arg(long)]
    refresh: bool,
}

pub fn models(env: &Env, a: &ModelsArgs) -> Result<u8, String> {
    let h = homes(env)?;
    let cfg = load_config(env, &h, a.cwd.cwd.as_deref(), ProjectTrust::Untrusted)?;
    let mut rows: Vec<(String, String, String, &str)> = list_models(&cfg, None)
        .into_iter()
        .map(|m| (m.value, m.provider, m.model, "config"))
        .collect();
    if a.refresh {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        for (id, p) in cfg
            .providers
            .iter()
            .filter(|(_, p)| p.discover != Some(false))
        {
            let cred = resolve_credential(id, Some(p), &h.synthesis, env);
            let found = ProviderSpec::from_config(id, p, cred.as_ref())
                .and_then(|s| s.connect(env, &NetSettings::default(), RetryPolicy::default()))
                .map(|prov| {
                    rt.block_on(async move { prov.list_models(&CancellationToken::new()).await })
                });
            match found {
                Ok(Ok(list)) => {
                    for m in list.iter().filter_map(|m| m.id.as_str()) {
                        let value = format!("{id}/{m}");
                        if !rows.iter().any(|r| r.0 == value) {
                            rows.push((value, id.clone(), m.to_string(), "discovered"));
                        }
                    }
                }
                Ok(Err(e)) | Err(e) => {
                    eprintln!("synth: {id}: model discovery failed: {}", e.message())
                }
            }
        }
    }
    if a.json {
        let models: Vec<_> = rows
            .iter()
            .map(|(v, p, m, s)| json!({"value": v, "provider": p, "model": m, "source": s}))
            .collect();
        out!("{}", json!({"default": cfg.model, "models": models}));
    } else {
        for (v, ..) in &rows {
            out!("{v}");
        }
    }
    Ok(0)
}

fn guarantees(b: &dyn SandboxBackend) -> String {
    let c = b.capabilities();
    format!(
        "{}: filesystem={:?} network={:?} process_tree={:?}\n      {}",
        b.name(),
        c.filesystem,
        c.network,
        c.process_tree,
        c.notes.join("\n      ")
    )
}

/// Per-mode guarantee of one backend, e.g. `deny=Weak loopback=None allow=Strong`.
fn network_support(b: &dyn SandboxBackend) -> String {
    use synthesis_sandbox::NetworkPolicy::{Allow, Deny, Loopback};
    [("deny", Deny), ("loopback", Loopback), ("allow", Allow)]
        .iter()
        .map(|(n, m)| format!("{n}={:?}", b.network_guarantee(m)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Linux backend availability; `bwrap` is the cached real probe (exact command arguments, run
/// on `true`): Ok(network support) or the precise failure with its remedy.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_backend_lines(
    landlock_net: Option<String>,
    bwrap: Result<String, &String>,
) -> Vec<String> {
    let l = match landlock_net {
        Some(s) => format!("  landlock available: true; network {s}"),
        None => "  landlock available: false".into(),
    };
    let w = match bwrap {
        Ok(s) => format!("  bwrap available: true (probe ran /bin/true); network {s}"),
        Err(e) => format!("  bwrap available: false: {}", san(e)),
    };
    vec![l, w]
}

fn git_version(env: &Env) -> Option<String> {
    let path = env.get_nonempty("PATH")?;
    let exe = std::env::split_paths(path)
        .flat_map(|d| [d.join("git"), d.join("git.exe")])
        .find(|p| p.is_file())?;
    let out = std::process::Command::new(exe)
        .arg("--version")
        .env_clear()
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn doctor(env: &Env, cwd: Option<&Path>) -> Result<u8, String> {
    let mut code = 0;
    out!(
        "synth {} on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let get = |k: &str| env.get(k).map(OsString::from);
    let sh = synthesis_terminal::resolve_shell(cfg!(windows), &get, &|p: &Path| p.exists());
    out!(
        "shell: {} kind={:?} is_posix={}{}",
        sh.program.display(),
        sh.kind,
        sh.is_posix(),
        sh.note.map(|n| format!(" ({n})")).unwrap_or_default()
    );

    let best = synthesis_sandbox::detect();
    // The exact preflight `synth run` does under --sandbox enforce, on the standard policy for
    // this workspace (default network deny), not just "does a backend exist".
    let root = std::fs::canonicalize(cwd.unwrap_or(Path::new("."))).unwrap_or_else(|_| ".".into());
    let policy = synthesis_sandbox::SandboxPolicy::standard(&root, &std::env::temp_dir());
    let ready = synthesis_app::run::sandbox_preflight(&*best, Some(&policy));
    out!("sandbox for tool commands (`synth run --sandbox enforce|best-effort|off`):");
    out!(
        "  default mode: enforce; `synth run` {}",
        if ready.is_ok() {
            "will confine commands"
        } else {
            "will REFUSE to start (exit 2); use --sandbox best-effort|off to run unconfined"
        }
    );
    match &ready {
        Ok(()) => out!("  enforce_ready: yes"),
        Err(e) => out!("  enforce_ready: no ({})", san(e)),
    }
    if best.name() == "landlock" {
        out!(
            "  hint: only Landlock is usable. It cannot make read-only paths (.git/hooks, .git/config) inside the writable workspace, so --sandbox enforce needs a working bubblewrap (see the bwrap line below)."
        );
    }
    out!("  selected {}", guarantees(&*synthesis_sandbox::detect()));
    out!(
        "  network (`synth run --network deny|loopback|allow`, default deny): {}",
        network_support(&*best)
    );
    #[cfg(target_os = "macos")]
    if let Some(b) = synthesis_sandbox::SeatbeltBackend::detect() {
        out!(
            "  seatbelt available: true; network {}",
            network_support(&b)
        );
    } else {
        out!("  seatbelt available: false");
    }
    #[cfg(target_os = "linux")]
    for l in linux_backend_lines(
        synthesis_sandbox::LinuxLandlockBackend::detect().map(|b| network_support(&b)),
        synthesis_sandbox::bwrap_probe()
            .as_ref()
            .map(|_| network_support(&synthesis_sandbox::BwrapBackend::detect().unwrap())),
    ) {
        out!("{l}");
    }
    out!(
        "  loopback: macOS exposes HOST loopback services; bwrap = private loopback; landlock/windows unsupported"
    );

    match synthesis_browser::discover(env, synthesis_browser::Os::current(), &|p| p.exists()) {
        Ok(p) => out!("browser: {}", p.display()),
        Err(e) => out!("browser: not found ({e})"),
    }
    out!(
        "git: {}",
        git_version(env).unwrap_or_else(|| "not found on PATH".into())
    );
    for v in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        out!(
            "{v}: {}",
            if env.get_nonempty(v).is_some() {
                "set"
            } else {
                "unset"
            }
        );
    }

    let h = homes(env)?;
    out!(
        "SYNTHESIS_HOME: {} (exists: {})",
        h.synthesis.display(),
        h.synthesis.is_dir()
    );
    for l in legacy_notes(env, &h, std::env::current_exe().ok().as_deref()) {
        out!("{l}");
    }
    for l in synthesis_app::login::doctor_lines(env, &h) {
        out!("{l}");
    }
    for l in synthesis_app::memcmd::doctor_lines(&h) {
        out!("{l}");
    }
    match load_config(env, &h, cwd, ProjectTrust::Untrusted) {
        Ok(cfg) => {
            report_config(&cfg);
            report_discovery(env, &h, &cfg, cwd);
        }
        Err(e) => {
            out!("config: INVALID: {e}");
            code = 2;
        }
    }
    Ok(code)
}

/// Hints for people coming from the earlier "Nemo" builds, and a PATH shadowing warning. Nothing
/// legacy is ever read: the old `~/.nemo` and `NEMO_*` are only detected (names, never values).
fn legacy_notes(env: &Env, h: &synthesis_app::Homes, current_exe: Option<&Path>) -> Vec<String> {
    let mut v = Vec::new();
    let mut old: Vec<&str> = env
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.to_ascii_uppercase().starts_with("NEMO_"))
        .collect();
    old.sort_unstable();
    if !old.is_empty() {
        v.push(format!(
            "warning: ignored legacy variables (Synthesis reads SYNTHESIS_* only): {}; rename each NEMO_X to SYNTHESIS_X",
            old.join(", ")
        ));
    }
    let old_home = h.user.join(".nemo");
    if old_home.is_dir() && !h.synthesis.exists() {
        v.push(format!(
            "note: {} exists and {} does not. Synthesis never reads the old location. To migrate, review and run:\n  cp -R {} {}   (Windows: Copy-Item -Recurse)\nthen inside the copy rename nemo.json to synthesis.json and NEMO.md to SYNTHESIS.md, the \"nemo\" key of settings.json to \"synthesis\", and in each project .nemo to .synthesis",
            old_home.display(),
            h.synthesis.display(),
            old_home.display(),
            h.synthesis.display()
        ));
    }
    let first = env.get_nonempty("PATH").and_then(|p| {
        std::env::split_paths(p)
            .flat_map(|d| [d.join("synth"), d.join("synth.exe")])
            .find(|c| c.is_file())
    });
    if let (Some(first), Some(me)) = (first, current_exe)
        && std::fs::canonicalize(&first).ok() != std::fs::canonicalize(me).ok()
    {
        v.push(format!(
            "warning: `synth` on PATH resolves to {}, not this executable ({})",
            first.display(),
            me.display()
        ));
    }
    v
}

fn report_config(cfg: &Config) {
    out!(
        "config: ok, {} provider(s), default model: {}",
        cfg.providers.len(),
        cfg.model.as_deref().unwrap_or("none")
    );
    for w in &cfg.warnings {
        out!("  warning: {}", san(w));
    }
}

/// Settings, MCP, skills and agents as the next `synth run` would see them for `cwd` with an
/// untrusted project. Counts and names only, never a value.
fn report_discovery(env: &Env, h: &synthesis_app::Homes, cfg: &Config, cwd: Option<&Path>) {
    use synthesis_settings::{LoadContext, load_settings};
    let none = h.synthesis.join("no-project");
    let cwd = cwd.unwrap_or(&none);
    let st = load_settings(
        &LoadContext {
            cwd,
            synthesis_home: &h.synthesis,
            user_home: &h.user,
            env,
        },
        ProjectTrust::Untrusted,
    );
    let file = |p: std::path::PathBuf| {
        format!(
            "{} ({})",
            p.display(),
            if p.is_file() { "present" } else { "absent" }
        )
    };
    let enforcing = {
        let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let policy = synthesis_sandbox::SandboxPolicy::standard(&root, &std::env::temp_dir());
        synthesis_app::run::sandbox_preflight(&*synthesis_sandbox::detect(), Some(&policy)).is_ok()
    };
    for l in synthesis_app::peers::doctor_lines(&st.synthesis.peers, &h.synthesis, env, enforcing) {
        out!("{l}");
    }
    out!("settings.json:");
    out!("  user file: {}", file(h.synthesis.join("settings.json")));
    out!(
        "  project file: {}",
        file(st.project_root.join(".synthesis").join("settings.json"))
    );
    let p = &st.permissions;
    out!(
        "  permissions: {} allow, {} deny, {} ask rule(s), default mode {}",
        p.allow.len(),
        p.deny.len(),
        p.ask.len(),
        p.default_mode
    );
    out!(
        "  hooks: {} event(s){}; env: {} variable(s) applied, {} refused (secret-shaped){}",
        st.hooks.len(),
        if st.disable_all_hooks {
            " (all disabled)"
        } else {
            ""
        },
        st.env.len(),
        st.env_refused.len(),
        if st.env_refused.is_empty() {
            String::new()
        } else {
            format!(": {}", san(&st.env_refused.join(", ")))
        }
    );
    let v = synthesis_app::verify_cfg::parse(&st.synthesis.verification);
    out!(
        "  synthesis section: verification {} key(s), {} required check(s)",
        st.synthesis.verification.len(),
        v.checks.len()
    );
    let (ac, ac_diag) = synthesis_app::ctx::read_auto_compact(&h.synthesis.join("settings.json"));
    out!(
        "  context: auto_compact {}, threshold {} of the model window (synthesis.context.auto_compact / auto_compact_threshold in the user settings.json)",
        if ac.enabled { "on" } else { "off" },
        ac.threshold
    );
    for d in st.diagnostics.iter().chain(&v.diagnostics).chain(&ac_diag) {
        out!("  warning: {}", san(d));
    }

    let plan = synthesis_app::mcp::plan(cfg, &st, &st, cwd, ProjectTrust::Untrusted, &[]);
    out!(
        "mcp: {} server(s) would start, {} skipped (never started by doctor)",
        plan.servers.len(),
        plan.skipped.len()
    );
    {
        let b = synthesis_app::login::parse_settings(&st.synthesis.browser);
        let stdio = synthesis_app::login::stdio_servers(&plan.servers);
        for l in synthesis_app::login::exposure_warnings(&h.synthesis, &b, &stdio, enforcing) {
            out!("warning: {}", san(&l));
        }
    }
    let pass = synthesis_app::mcp::read_passthrough(&h.synthesis.join("settings.json"));
    for s in &plan.servers {
        out!(
            "  {} [{}] {}",
            san(&s.name),
            s.scope.as_deref().unwrap_or("?"),
            san(&synthesis_app::mcp::describe(s)),
        );
        for l in synthesis_app::mcp::credentials(s, env, &pass) {
            out!("    {}", san(&l));
        }
    }
    for (n, why) in &plan.skipped {
        out!("  {}: skipped, {}", san(n), san(why));
    }
    out!(
        "  env passthrough for ${{VAR}} (user scope only): {}",
        if pass.is_empty() {
            "none".into()
        } else {
            san(&pass.join(", "))
        }
    );

    let root = &st.project_root;
    let skills = synthesis_tools::discover_skills(env, root);
    let proj = skills.iter().filter(|s| !s.trusted).count();
    out!(
        "skills: {} ({} untrusted project skill(s))",
        skills.len(),
        proj
    );
    for s in &skills {
        out!(
            "  {}{}",
            san(&s.name),
            if s.trusted {
                ""
            } else {
                " [untrusted project]"
            }
        );
    }
    let ag = synthesis_app::agents::load(&h.synthesis, root, ProjectTrust::Untrusted);
    out!(
        "agents: {} user, {} project definition(s) ({} project not loaded: project untrusted)",
        ag.user,
        ag.project,
        ag.skipped.len()
    );
    crate::import::doctor_report(&h.synthesis, root);
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
        std::fs::write(bin.join("synth"), "").unwrap();
        let h = synthesis_app::Homes {
            synthesis: user.join(".synthesis"),
            user,
        };
        let path = bin.to_str().unwrap();
        let env = Env::from_pairs([("NEMO_API_KEY", "secret-value"), ("PATH", path)]);
        let v = legacy_notes(&env, &h, Some(&d.path().join("other"))).join("\n");
        assert!(
            v.contains("NEMO_API_KEY") && !v.contains("secret-value"),
            "{v}"
        );
        assert!(v.contains("cp -R") && v.contains("resolves to"), "{v}");
        // this executable is the first on PATH, the new home exists, no legacy variables: silent
        std::fs::create_dir_all(&h.synthesis).unwrap();
        let env = Env::from_pairs([("PATH", path)]);
        assert!(legacy_notes(&env, &h, Some(&bin.join("synth"))).is_empty());
    }

    #[test]
    fn doctor_lines_show_the_probe_failure_and_remedy() {
        let why = "bubblewrap not usable: /usr/bin/bwrap failed its probe: bwrap: Can't mount proc on /newroot/proc: Operation not permitted; remedy: --security-opt systempaths=unconfined".to_string();
        let v = linux_backend_lines(Some("deny=Weak".into()), Err(&why)).join("\n");
        assert!(
            v.contains("bwrap available: false") && v.contains("Can't mount proc"),
            "{v}"
        );
        assert!(
            v.contains("systempaths=unconfined") && v.contains("landlock available: true"),
            "{v}"
        );
        let ok = linux_backend_lines(None, Ok("deny=Strong".into())).join("\n");
        assert!(
            ok.contains("bwrap available: true") && ok.contains("landlock available: false"),
            "{ok}"
        );
    }
}
