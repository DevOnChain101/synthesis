//! Where a hosted session lives (spec "Mode switch"). A session id means something only inside one store: this
//! host's, or the one of the engine in a WSL distro (spec 9). The id therefore travels with its [`Scope`].

use std::path::PathBuf;

/// Where a session id can be looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Local,
    /// The distro the engine runs in (the name `wsl.exe` resolved).
    Wsl(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Local => f.write_str("local"),
            Scope::Wsl(d) => write!(f, "wsl:{d}"),
        }
    }
}

/// A session a Super Agent frame hosts, as its engine reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosted {
    pub scope: Scope,
    /// The store's session id (never the frame id).
    pub sid: String,
    pub folder: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_names_its_store() {
        assert_eq!(Scope::Local.to_string(), "local");
        assert_eq!(Scope::Wsl("U".into()).to_string(), "wsl:U");
    }
}
