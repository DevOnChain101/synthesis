//! UI-agnostic view models: typed state and pure reducers over the event stream
//! (`synthesis-events`) and the session store, shared by the classic TUI and the desktop app.
//! No terminal, no GPUI, no drawing: colours, layout and ANSI stay in the front ends.
//!
//! [`state::ViewState`] is the entry point; the other modules are the panels it holds.

pub mod activity;
pub mod convo;
pub mod model;
pub mod project;
pub mod running;
pub mod sessions;
pub mod state;
pub mod status;
pub mod text;
pub mod topology;
pub mod usage;
pub mod workspaces;
