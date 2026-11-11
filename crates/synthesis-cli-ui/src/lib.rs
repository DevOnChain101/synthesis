//! Interactive terminal app (R5c), as a library: `synthesis_cli_ui::run_interactive`.
//!
//! The app is a client of the harness: it starts tasks, shows the event stream, answers
//! approvals/questions and cancels through the task's token. It never owns a process, a model
//! request or the session store. Input bytes -> `KeyParser` -> `App` -> `render_frame` ->
//! `Screen` diff-draw; stdin/stdout sit behind [`Tty`] so the whole thing runs against a
//! [`ScriptedTty`] in tests. Environment is read only from the injected `synthesis_config::Env`.

#[macro_use]
mod sanitize;
mod app;
mod chat;
mod link;
pub mod links;
pub mod peers_view;
mod preview;
mod resume;
mod route;
mod runtime;
mod setup;
mod status;
mod tty;

pub use app::local_commands;
pub use link::{EventWriter, UiLink};
pub use links::{OpenTarget, Opener, PathTarget, SystemOpener};
pub use resume::{Resume, load_resume};
pub use runtime::{
    BoxFut, Exit, ExitReason, Hold, MemoryHook, OnceOptions, Options, TaskHost, render_once,
    run_interactive, size_from_env,
};
pub use setup::{SetupExit, SetupOptions, card as setup_card, run_setup};
pub use status::{SandboxStatus, status_line};
pub use synthesis_view::sessions::{SavedSession, list_saved};
pub use tty::{ScriptedTty, Tty, replay};

pub(crate) fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
