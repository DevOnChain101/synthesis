//! The harness wiring shared by `synth` and the desktop app: `run::prepare` builds the harness,
//! session, sandbox, MCP, memory and peers from the config files; the front ends (headless run,
//! TUI, desktop) take the `Prepared` it returns. Moved out of `apps/cli` without behaviour change
//! (docs/specs/desktop.md section 1).

#[cfg(all(feature = "test-provider", not(debug_assertions)))]
compile_error!("feature `test-provider` must never be enabled in release builds");

/// `println!` that exits quietly when the reader went away (`synth doctor | head`).
#[macro_export]
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

pub mod agents;
pub mod ctx;
pub mod events;
#[cfg(feature = "test-provider")]
pub mod fake;
pub mod mcp;
pub mod memcmd;
pub mod memory;
mod memtools;
pub mod peers;
pub mod run;
pub mod taskdb;
pub mod transcript;
pub mod verify_cfg;
pub mod wt;

use std::path::{Path, PathBuf};

use clap::Args;
use synthesis_config::{
    Config, Env, LoadContext, ProjectTrust, default_synthesis_home, load_config_with_trust,
};

#[derive(Args)]
pub struct CwdArg {
    /// Project directory whose `.synthesis/config.json` is considered (default: none).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

/// The directory `synth` was started in. This is the ONE place the process cwd is read (the CLI
/// boundary): the libraries still take an explicit cwd, `--cwd DIR` always wins.
#[allow(clippy::disallowed_methods)]
pub fn launch_dir() -> Result<PathBuf, String> {
    std::env::current_dir()
        .map_err(|e| format!("cannot read the current directory: {e}; pass --cwd DIR"))
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
