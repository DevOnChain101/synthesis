//! `synth`: headless run, `auth status`, `models`, `doctor`.
//!
//! The process environment is read once (`Env::from_process`) and passed down. The binary never
//! asks for the process cwd: `run` requires `--cwd`; other commands take an optional `--cwd`.

#[cfg(all(feature = "test-provider", not(debug_assertions)))]
compile_error!("feature `test-provider` must never be enabled in release builds");

/// `println!` that exits quietly when the reader went away (`synth doctor | head`).
macro_rules! out {
    ($($a:tt)*) => {{
        use std::io::Write;
        if let Err(e) = writeln!(std::io::stdout(), $($a)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("synth: error: stdout: {e}");
            std::process::exit(1);
        }
    }};
}

mod agents;
mod ctx;
mod events;
#[cfg(feature = "test-provider")]
mod fake;
mod import;
mod info;
mod mcp;
mod memcmd;
mod memory;
mod run;
mod taskdb;
mod transcript;
mod tui;
mod verify_cfg;
mod wt;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use synthesis_config::{
    Config, Env, LoadContext, ProjectTrust, default_synthesis_home, load_config_with_trust,
};

#[derive(Parser)]
#[command(
    name = "synth",
    version,
    about = "Synthesis autonomous coding agent",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// No subcommand: the interactive app (needs a terminal).
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    tui: tui::TuiArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one task headless; events (JSONL) go to stdout.
    Run(Box<run::RunArgs>),
    /// Credential status per provider.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// List configured models.
    Models(info::ModelsArgs),
    /// The interactive app (same as running `synth` with no subcommand).
    Tui(Box<tui::TuiArgs>),
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
    /// Long-term memory: list, search, add, review, export (data only; never permissions).
    Memory {
        #[command(subcommand)]
        cmd: memcmd::MemoryCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    Status(CwdArg),
}

#[derive(Args)]
pub struct CwdArg {
    /// Project directory whose `.synthesis/config.json` is considered (default: none).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

pub struct Homes {
    pub user: PathBuf,
    pub synthesis: PathBuf,
}

pub fn homes(env: &Env) -> Result<Homes, String> {
    let user = env
        .get_nonempty("HOME")
        .or_else(|| env.get_nonempty("USERPROFILE"))
        .map(PathBuf::from);
    if user.is_none() && env.get_nonempty("SYNTHESIS_HOME").is_none() {
        return Err("neither HOME nor SYNTHESIS_HOME is set".into());
    }
    let user = user.unwrap_or_default();
    Ok(Homes {
        synthesis: default_synthesis_home(env, &user),
        user,
    })
}

/// Config for `cwd`; without a cwd the project layer points at a path that cannot exist.
pub fn load_config(
    env: &Env,
    h: &Homes,
    cwd: Option<&Path>,
    trust: ProjectTrust,
) -> Result<Config, String> {
    let none = h.synthesis.join("no-project");
    let ctx = LoadContext {
        cwd: cwd.unwrap_or(&none),
        synthesis_home: &h.synthesis,
        user_home: &h.user,
        env,
    };
    load_config_with_trust(&ctx, trust).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse(); // usage errors exit 2, --version/--help exit 0
    let env = Env::from_process();
    let r = match cli.cmd {
        None => tui::launch(cli.tui, env),
        Some(Cmd::Tui(a)) => tui::launch(*a, env),
        Some(Cmd::Run(a)) => run::run(*a, env),
        Some(Cmd::Auth {
            cmd: AuthCmd::Status(a),
        }) => info::auth_status(&env, a.cwd.as_deref()),
        Some(Cmd::Models(a)) => info::models(&env, &a),
        Some(Cmd::Doctor(a)) => info::doctor(&env, a.cwd.as_deref()),
        Some(Cmd::Mcp { cmd }) => mcp::run_cmd(&env, cmd),
        Some(Cmd::Import { cmd }) => import::run_cmd(&env, cmd),
        Some(Cmd::Memory { cmd }) => memcmd::run_cmd(&env, cmd),
    };
    match r {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("synth: error: {msg}");
            ExitCode::from(2)
        }
    }
}
