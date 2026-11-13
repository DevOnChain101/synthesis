//! Synthesis desktop app (docs/specs/desktop.md). Everything except `ui` painting is plain Rust and
//! unit-tested without a window.

pub mod brand;
pub mod clean;
pub mod close_ws;
pub mod doctor_ui;
pub mod engine;
pub mod folder;
pub mod live;
pub mod memory_ui;
pub mod menu;
pub mod models;
pub mod perf;
pub mod remote;
pub mod reveal;
pub mod rewind;
pub mod send_pick;
pub mod settings_view;
pub mod theme;
pub mod trace;
pub mod tui_view;
pub mod ui;
pub mod widgets;
pub mod wsl;
