//! `synth`: headless run, `auth status`, `models`, `doctor`.
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

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use synthesis_app::{CwdArg, mcp, memcmd, run};
use synthesis_config::Env;

#[derive(Parser)]
#[command(
    name = "synth",
    version,
    about = "Synthesis autonomous coding agent",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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

fn main() -> ExitCode {
    let cli = Cli::parse(); // usage errors exit 2, --version/--help exit 0
    let env = Env::from_process();
    let r = match cli.cmd {
        Cmd::Run(a) => run::run(*a, env),
        Cmd::Auth {
            cmd: AuthCmd::Status(a),
        } => info::auth_status(&env, a.cwd.as_deref()),
        Cmd::Models(a) => info::models(&env, &a),
        Cmd::Doctor(a) => info::doctor(&env, a.cwd.as_deref()),
        Cmd::Mcp { cmd } => mcp::run_cmd(&env, cmd),
        Cmd::Import { cmd } => import::run_cmd(&env, cmd),
        Cmd::Memory { cmd } => memcmd::run_cmd(&env, cmd),
    };
    match r {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("synth: error: {msg}");
            ExitCode::from(2)
        }
    }
}
