//! Pure, deterministic renderer for the Synthesis TUI (R5b-1): no terminal I/O, no env reads, time is
//! passed explicitly. Ported from the Node `render.js`, `agentview.js`, `flow.js`, ...
#![allow(clippy::too_many_arguments)] // render entry points mirror the Node signatures

pub mod agent;
pub mod dash;
pub mod dialogs;
pub mod flow;
pub mod folders;
pub mod highlight;
pub mod model;
pub mod modelname;
pub mod picker;
pub mod theme;
pub mod util;
