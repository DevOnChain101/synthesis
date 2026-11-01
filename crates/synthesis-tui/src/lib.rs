//! Interactive terminal app (R5c), as a library: `synthesis_tui::run_interactive`.
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
mod preview;
mod resume;
mod route;
mod runtime;
mod status;
mod tty;

pub use link::{EventWriter, UiLink};
pub use links::{OpenTarget, Opener, PathTarget, SystemOpener};
pub use resume::{Resume, list_saved, load_resume};
pub use runtime::{
    Exit, ExitReason, MemoryHook, OnceOptions, Options, render_once, run_interactive, size_from_env,
};
pub use status::{SandboxStatus, status_line};
pub use tty::{ScriptedTty, Tty, replay};

/// A session in the store, for the sidebar and the resume picker.
#[derive(Clone, Debug, Default)]
pub struct SavedSession {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
    /// Epoch ms of `updated_at` (0 when unknown).
    pub mtime: i64,
}

pub(crate) fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
