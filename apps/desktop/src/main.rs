//! `synthesis-desktop`: the native desktop app (docs/specs/desktop.md).
// Release builds on Windows use the GUI subsystem (no console window); debug builds keep the console.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use clap::Parser as _;
use synthesis_app::{homes, launch_dir};
use synthesis_config::Env;
use synthesis_desktop::engine::{Args, root_of};
use synthesis_desktop::live::local_offset_min;
use synthesis_desktop::tui_view::driver::LiveCfg;

/// The flags and the process environment, read once here (the engine only sees this snapshot).
fn live_cfg(args: Args, env: Env) -> Result<LiveCfg, String> {
    let dir = match &args.cwd {
        Some(d) => d.clone(),
        // A Finder launch starts in `/`; never make the filesystem root the workspace.
        None => match launch_dir()? {
            d if d.parent().is_none() => homes(&env)?.user,
            d => d,
        },
    };
    let project =
        synthesis_core::canonicalize(&dir).map_err(|e| format!("--cwd {}: {e}", dir.display()))?;
    let h = homes(&env)?;
    let (tui, diag) = synthesis_settings::read_user_tui(&h.synthesis);
    for d in diag {
        eprintln!("synthesis-desktop: warning: {d}");
    }
    Ok(LiveCfg {
        granted: root_of(&project),
        project,
        synthesis_home: h.synthesis,
        home: Some(h.user.to_string_lossy().into_owned()),
        tz_min: local_offset_min(),
        tui,
        args,
        env,
    })
}

fn main() {
    #[cfg(feature = "shot")]
    if let Some(a) = synthesis_desktop::tui_view::shot::parse(&std::env::args().collect::<Vec<_>>())
    {
        if let Err(e) = synthesis_desktop::tui_view::shot::render(a) {
            eprintln!("synthesis-desktop: shot: {e}");
            std::process::exit(1);
        }
        return;
    }
    let args = Args::parse();
    let live = if args.sample {
        None
    } else {
        match live_cfg(args, Env::from_process()) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("synthesis-desktop: {e}");
                std::process::exit(2);
            }
        }
    };
    gpui_platform::application()
        .with_assets(synthesis_desktop::brand::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            synthesis_desktop::ui::register_fonts(cx);
            synthesis_desktop::theme::apply("JetBrains Mono", cx);
            synthesis_desktop::ui::open(cx, live);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
