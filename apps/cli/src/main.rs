//! `synt`: the interactive app (no subcommand / `tui`), headless run, `auth status`, `models`, `doctor`.
//!
//! The process environment is read once (`Env::from_process`) and passed down. The binary never
//! asks for the process cwd: `run` requires `--cwd`; other commands take an optional `--cwd`.
//! The harness wiring (`prepare`, `Common`, `Prepared`) lives in `synthesis-app`.

#[cfg(all(feature = "test-provider", not(debug_assertions)))]
compile_error!("feature `test-provider` must never be enabled in release builds");

#[macro_use]
extern crate synthesis_app;

mod import;
mod info;
mod tui;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use synthesis_app::{CwdArg, mcp, memcmd, rewind, run};
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
    /// The interactive app (same as running `synt` with no subcommand).
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
    /// Put the files a run changed back to their state before its first write (dry run unless --apply).
    Rewind(rewind::RewindArgs),
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
        Some(Cmd::Rewind(a)) => rewind::run_cmd(&env, a),
    };
    match r {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("synt: error: {msg}");
            ExitCode::from(2)
        }
    }
}
