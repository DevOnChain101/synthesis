//! `synt`: the interactive terminal app (no subcommand), headless run, `auth status`, `models`, `doctor`.
//!
//! The process environment is read once (`Env::from_process`) and passed down. The binary never
//! asks for the process cwd: `run` requires `--cwd`; other commands take an optional `--cwd`.
//! The harness wiring (`prepare`, `Common`, `Prepared`) lives in `synthesis-app`.

#[cfg(all(feature = "test-provider", not(debug_assertions)))]
compile_error!("feature `test-provider` must never be enabled in release builds");

#[macro_use]
extern crate synthesis_app;

mod cli_app;
mod import;
mod info;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use synthesis_app::{CwdArg, login, mcp, memcmd, rewind, run, sessions};
use synthesis_config::Env;

#[derive(Parser)]
#[command(
    name = "synt",
    display_name = "Synthesis",
    version,
    about = "Synthesis autonomous coding agent",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// No subcommand: the interactive app (needs a terminal).
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    app: cli_app::AppArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// INTERNAL: the engine of one session over stdin/stdout JSON lines (docs/specs/desktop.md).
    #[command(hide = true)]
    Engine(Box<EngineArgs>),
    /// Run one task headless; events (JSONL) go to stdout.
    Run(Box<run::RunArgs>),
    /// Credential status per provider.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// List configured models.
    Models(info::ModelsArgs),
    /// Diagnose the installation.
    Doctor(CwdArg),
    /// Manage MCP server entries in your own config files (never credentials).
    Mcp {
        #[command(subcommand)]
        cmd: mcp::McpCmd,
    },
    /// Copy/convert another tool's setup into Synthesis's own files (dry run unless --apply).
    Import {
        #[command(subcommand)]
        cmd: import::ImportCmd,
    },
    /// Finish a `browser_login` wait from another terminal.
    Browser {
        #[command(subcommand)]
        cmd: BrowserCmd,
    },
    /// Put the files a run changed back to their state before its first write (dry run unless --apply).
    Rewind(rewind::RewindArgs),
    /// Ask the process running SESSION to stop (graceful cancel, then killed after 15 s).
    Stop(sessions::StopArgs),
    /// Inspect stored sessions: `show` (timeline, budget use, verification reports) and `verify` (event hash chain).
    Sessions {
        #[command(subcommand)]
        cmd: sessions::SessionsCmd,
    },
    /// Long-term memory: list, search, add, review, export (data only; never permissions).
    Memory {
        #[command(subcommand)]
        cmd: memcmd::MemoryCmd,
    },
}

/// Who is on the other end of the engine's pipe.
#[derive(Clone, Copy, clap::ValueEnum)]
enum HostArg {
    /// The desktop window: a person watches, so the headless defaults (worktree, wall cap, strict
    /// store, required cost cap) do not apply. The desktop passes its own `--max-cost-usd`.
    Desktop,
}

#[derive(clap::Args)]
struct EngineArgs {
    /// Without it the engine is a headless worker and gets the unattended defaults
    /// (docs/UNATTENDED.md).
    #[arg(long, value_enum)]
    host: Option<HostArg>,
    /// The only transport.
    #[arg(long)]
    stdio: bool,
    /// Cost cap per task (USD); required unless --no-cost-cap.
    #[arg(long)]
    max_cost_usd: Option<f64>,
    /// Accept spending without a cost cap.
    #[arg(long, conflicts_with_all = ["max_cost_usd", "unattended"])]
    no_cost_cap: bool,
    /// Wall-clock cap per task in seconds (default 1800).
    #[arg(long)]
    max_wall_secs: Option<u64>,
    /// Cap on input + output + cache tokens per task.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u64>,
    #[command(flatten)]
    common: run::Common,
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// Tell a waiting `browser_login` that you finished logging in to PROFILE.
    Done { profile: String },
}

#[derive(Subcommand)]
enum AuthCmd {
    Status(CwdArg),
}

fn main() -> ExitCode {
    let cli = Cli::parse(); // usage errors exit 2, --version/--help exit 0
    let env = Env::from_process();
    let r = match cli.cmd {
        None => cli_app::launch(cli.app, env),
        Some(Cmd::Engine(a)) if !a.stdio => Err("engine: --stdio is required".into()),
        Some(Cmd::Engine(a)) => {
            if a.max_cost_usd.is_some_and(|c| !c.is_finite() || c <= 0.0) {
                Err("--max-cost-usd must be a positive number".into())
            } else {
                let limits = run::Limits {
                    max_cost_usd: a.max_cost_usd,
                    max_wall_secs: a.max_wall_secs,
                    max_tokens: a.max_tokens,
                    no_cost_cap: a.no_cost_cap,
                    ..run::Limits::default()
                };
                synthesis_app::engine_server::run_stdio(&a.common, &limits, a.host.is_none(), &env)
            }
        }
        Some(Cmd::Run(a)) => run::run(*a, env),
        Some(Cmd::Auth {
            cmd: AuthCmd::Status(a),
        }) => info::auth_status(&env, a.cwd.as_deref()),
        Some(Cmd::Models(a)) => info::models(&env, &a),
        Some(Cmd::Doctor(a)) => info::doctor(&env, a.cwd.as_deref()),
        Some(Cmd::Mcp { cmd }) => mcp::run_cmd(&env, cmd),
        Some(Cmd::Import { cmd }) => import::run_cmd(&env, cmd),
        Some(Cmd::Memory { cmd }) => memcmd::run_cmd(&env, cmd),
        Some(Cmd::Rewind(a)) => rewind::run_cmd(&env, a),
        Some(Cmd::Stop(a)) => sessions::stop_cmd(&env, a),
        Some(Cmd::Sessions { cmd }) => sessions::run_cmd(&env, cmd),
        Some(Cmd::Browser {
            cmd: BrowserCmd::Done { profile },
        }) => login::done_cmd(&env, &profile),
    };
    match r {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("synt: error: {msg}");
            ExitCode::from(2)
        }
    }
}
