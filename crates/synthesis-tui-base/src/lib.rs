//! Low-level terminal UI layer (R5a): faithful ports of the Node `keys`, `sgr`, `screen`,
//! `selection`, `editor` and `complete` modules plus the width helpers of `theme`.
//! Pinned by `tests/golden/{keys,sgr,selection}`; known differences are listed in
//! `tests/golden_divergences.txt` and in the module docs.

pub mod clipboard;
pub mod complete;
pub mod editor;
pub mod keys;
pub mod screen;
pub mod selection;
pub mod sgr;
pub mod terminal;
pub mod text;

/// Operating system family, injected so behaviour that differs per OS is testable everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    /// Node's `process.platform` names (used by the golden fixtures).
    pub fn from_node(name: &str) -> Self {
        match name {
            "win32" => Self::Windows,
            "darwin" => Self::MacOs,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}
