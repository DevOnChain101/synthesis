//! The Super Agent -> TUI mode switch (spec "Mode switch"): which sessions the Super Agent has open, and where
//! each one lives. A session id means something only inside one store: this host's, or the one of the engine in a
//! WSL distro (spec 9). The id therefore travels with its [`Scope`], and the TUI lists only its own scope's.

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

/// Every hosted session, and which one was in front (or the last in front).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handoff {
    pub sessions: Vec<Hosted>,
    pub focused: Option<(Scope, String)>,
}

/// What the TUI of scope `own` does with a handoff.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan<'a> {
    /// Sessions of this scope: they are in the TUI's lists (once each).
    pub listed: Vec<&'a Hosted>,
    /// Sessions of another scope: never looked up here.
    pub skipped: Vec<&'a Hosted>,
    /// The session to highlight and load, when it is one of `listed`.
    pub select: Option<&'a Hosted>,
}

impl Handoff {
    pub fn plan(&self, own: &Scope) -> Plan<'_> {
        let mut p = Plan::default();
        for h in &self.sessions {
            if &h.scope != own {
                p.skipped.push(h);
            } else if !p.listed.iter().any(|l| l.sid == h.sid) {
                p.listed.push(h);
            }
        }
        p.select = self
            .focused
            .as_ref()
            .and_then(|(s, id)| p.listed.iter().find(|h| &h.scope == s && &h.sid == id))
            .copied();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(scope: Scope, sid: &str) -> Hosted {
        Hosted {
            scope,
            sid: sid.into(),
            folder: PathBuf::from("/w").join(sid),
        }
    }

    #[test]
    fn same_scope_is_listed_once_each_and_the_focused_one_is_selected() {
        let w = Scope::Wsl("Ubuntu".into());
        let ho = Handoff {
            sessions: vec![
                h(Scope::Local, "a"),
                h(w.clone(), "a"),
                h(Scope::Local, "b"),
                h(Scope::Local, "b"),
                h(Scope::Local, "c"),
            ],
            focused: Some((Scope::Local, "b".into())),
        };
        let p = ho.plan(&Scope::Local);
        let ids: Vec<_> = p.listed.iter().map(|h| h.sid.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.select.map(|h| h.sid.as_str()), Some("b"));
    }

    #[test]
    fn an_id_of_another_scope_is_skipped_even_when_it_is_the_focused_one() {
        let w = Scope::Wsl("Ubuntu".into());
        let ho = Handoff {
            sessions: vec![h(w.clone(), "x"), h(Scope::Local, "y")],
            focused: Some((w.clone(), "x".into())),
        };
        let p = ho.plan(&Scope::Local);
        assert_eq!(p.listed.len(), 1);
        assert_eq!(p.skipped[0].sid, "x");
        assert_eq!(p.select, None, "the id is not resolved in the wrong store");
        let q = ho.plan(&Scope::Wsl("Debian".into()));
        assert!(q.listed.is_empty() && q.select.is_none());
        assert_eq!(ho.plan(&w).select.map(|h| h.sid.as_str()), Some("x"));
        assert_eq!(Scope::Wsl("U".into()).to_string(), "wsl:U");
    }
}
