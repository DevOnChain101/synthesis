//! Synthesis desktop app (docs/specs/desktop.md). Everything except `ui` and `vt` painting is
//! plain Rust and unit-tested without a window.

pub mod activity;
pub mod agent_tui;
pub mod convo;
pub mod credentials;
pub mod d0;
pub mod engine;
pub mod onboarding;
pub mod project;
pub mod pty;
pub mod sessions;
pub mod settings;
pub mod theme;
pub mod topology;
pub mod ui;
pub mod vt;
