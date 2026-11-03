//! Desktop control for Synthesis agents (docs/DESKTOP-CONTROL.md): launch the app under test,
//! read its UI through the OS accessibility tree, act on it, take a window screenshot. The work
//! is done by the pinned `agent-desktop` CLI, run as a child process; this crate scopes it
//! (window ids of the launched process tree only), checks everything it prints and labels every
//! string from the UI as untrusted. The tools are in `synthesis-tools`.

pub mod actions;
pub mod envelope;
pub mod error;
pub mod limiter;
pub mod pin;
pub mod proctree;
pub mod runtime;
pub mod snapshot;
pub mod text;
pub mod untrusted;

pub use actions::Action;
pub use envelope::Delivery;
pub use error::{DesktopError, Result};
pub use pin::{Pin, PinStatus};
pub use proctree::{ProcSource, ProcTable, SystemProcs};
pub use runtime::{
    App, AppGuard, DesktopOptions, DesktopRuntime, Shot, WaitFor, Window, Windows,
    display_available, is_wsl,
};
pub use snapshot::Snapshot;
pub use untrusted::UntrustedText;
