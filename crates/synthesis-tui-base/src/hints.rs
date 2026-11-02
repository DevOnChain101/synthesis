//! The ONLY place that reads terminal-identification and display-server variables of OTHER
//! products' conventions (`TERM_PROGRAM`, `WT_SESSION`, `KITTY_WINDOW_ID`, `VTE_VERSION`,
//! `WAYLAND_DISPLAY`, `DISPLAY`), all through the injected `Env`. They only select colour depth,
//! OSC 8 links and which clipboard tool to try; removing them is deleting these functions.

use synthesis_config::Env;

/// `TERM_PROGRAM` (`iTerm.app`, `WezTerm`, `vscode`, ...).
pub fn term_program(env: &Env) -> Option<&str> {
    env.get("TERM_PROGRAM")
}

/// Windows Terminal (`WT_SESSION`).
pub fn windows_terminal(env: &Env) -> bool {
    env.get_nonempty("WT_SESSION").is_some()
}

/// kitty (`KITTY_WINDOW_ID`).
pub fn kitty(env: &Env) -> bool {
    env.get_nonempty("KITTY_WINDOW_ID").is_some()
}

/// VTE-based terminals (`VTE_VERSION`, e.g. 5000 for 0.50).
pub fn vte_version(env: &Env) -> Option<u32> {
    env.get("VTE_VERSION").and_then(|v| v.parse().ok())
}

/// A Wayland session (`WAYLAND_DISPLAY`).
pub fn wayland(env: &Env) -> bool {
    env.get_nonempty("WAYLAND_DISPLAY").is_some()
}

/// An X11 display (`DISPLAY`).
pub fn x11(env: &Env) -> bool {
    env.get_nonempty("DISPLAY").is_some()
}
