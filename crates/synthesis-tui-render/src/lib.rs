//! Pure, deterministic renderer for the Synthesis TUI (R5b-1): no terminal I/O, no env reads, time is
//! passed explicitly. Ported from the Node `render.js`, `agentview.js`, `flow.js`, ...
#![allow(clippy::too_many_arguments)] // render entry points mirror the Node signatures

pub mod agent;
pub mod dash;
pub mod dialogs;
pub mod flow;
pub mod folders;
pub mod greeting;
pub mod highlight;
pub mod logo;
mod logo_art;
pub mod modelname;
pub mod picker;
pub mod theme;
pub mod util;

// The session/activity model lives in `synthesis-view`; the render keeps its old path.
pub use synthesis_view::model;
