//! Sub-agent type table (data) and permission-mode narrowing.

use synthesis_core::PermissionMode;
use synthesis_permissions::Mode;

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// System prompt; empty = the generic subagent prompt.
    pub prompt: String,
    /// Tool allow-list; `None` = every tool. `Task`, `AskUserQuestion` and `ExitPlanMode` are
    /// never granted to a child, whatever is listed.
    pub tools: Option<Vec<String>>,
    /// Requested mode; the effective mode is never more permissive than the parent's.
    pub mode: Option<Mode>,
}

/// Where a definition was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    User,
    Project,
}

pub const GENERAL_PURPOSE: &str = "general-purpose";

#[derive(Debug, Clone)]
pub struct AgentTable(Vec<AgentDef>);

impl Default for AgentTable {
    fn default() -> Self {
        Self::new([])
    }
}

impl AgentTable {
    /// `general-purpose` is always present; a definition with the same name replaces it.
    pub fn new(defs: impl IntoIterator<Item = AgentDef>) -> Self {
        // Explore and Plan are read-only builtins: no write tools, and plan mode on top
        let read_only = |name: &str, description: &str, prompt: &str| AgentDef {
            name: name.into(),
            description: description.into(),
            prompt: prompt.into(),
            tools: Some(
                ["Read", "Grep", "Glob", "WebFetch", "WebSearch"]
                    .map(String::from)
                    .to_vec(),
            ),
            mode: Some(Mode::Plan),
        };
        let mut all = vec![
            AgentDef {
                name: GENERAL_PURPOSE.into(),
                description: "General-purpose agent for multi-step research and edits.".into(),
                prompt: String::new(),
                tools: None,
                mode: None,
            },
            read_only(
                "Explore",
                "Fast read-only agent for searching and understanding a codebase; it cannot change anything.",
                "You are a read-only exploration agent. Search and read files to answer the question and report what you found with file paths. You cannot modify anything.",
            ),
            read_only(
                "Plan",
                "Read-only agent that researches the code and returns a step-by-step implementation plan.",
                "You are a planning agent. Read the code you need, then return a concrete step-by-step implementation plan with file paths. You cannot modify anything.",
            ),
        ];
        for d in defs {
            all.retain(|x| x.name != d.name);
            all.push(d);
        }
        Self(all)
    }

    /// Definitions from their sources. Project definitions are files of the repository, i.e.
    /// untrusted input: they load only for a trusted project. Returns the table and the names
    /// that were skipped so the caller can tell the user.
    pub fn from_sources(
        defs: impl IntoIterator<Item = (AgentSource, AgentDef)>,
        trust: synthesis_settings::ProjectTrust,
    ) -> (Self, Vec<String>) {
        let (mut keep, mut skipped) = (vec![], vec![]);
        for (src, d) in defs {
            if src == AgentSource::Project && trust != synthesis_settings::ProjectTrust::Trusted {
                skipped.push(d.name);
            } else {
                keep.push(d);
            }
        }
        (Self::new(keep), skipped)
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.0.iter().find(|d| d.name == name)
    }
}

/// Permissiveness: higher allows more. `Plan` only reads; `DontAsk` never prompts and runs only
/// what rules allow; `Default` prompts; the rest widen what runs without a prompt.
fn rank(m: Mode) -> u8 {
    match m {
        Mode::Plan => 0,
        Mode::DontAsk => 1,
        Mode::Default => 2,
        Mode::AcceptEdits => 3,
        Mode::Auto => 4,
        Mode::BypassPermissions => 5,
    }
}

/// The stricter of two modes: a sub-agent can only be equal or stricter than its parent.
pub fn stricter(a: Mode, b: Mode) -> Mode {
    if rank(a) <= rank(b) { a } else { b }
}

pub fn to_mode(m: PermissionMode) -> Mode {
    match m {
        PermissionMode::Default => Mode::Default,
        PermissionMode::AcceptEdits => Mode::AcceptEdits,
        PermissionMode::Plan => Mode::Plan,
        PermissionMode::Auto => Mode::Auto,
        PermissionMode::DontAsk => Mode::DontAsk,
        PermissionMode::BypassPermissions => Mode::BypassPermissions,
    }
}

pub fn from_mode(m: Mode) -> PermissionMode {
    match m {
        Mode::Default => PermissionMode::Default,
        Mode::AcceptEdits => PermissionMode::AcceptEdits,
        Mode::Plan => PermissionMode::Plan,
        Mode::Auto => PermissionMode::Auto,
        Mode::DontAsk => PermissionMode::DontAsk,
        Mode::BypassPermissions => PermissionMode::BypassPermissions,
    }
}

impl crate::Harness {
    /// `(name, description)` of every agent type the Task tool can start (for `/agents`).
    pub fn agent_defs(&self) -> Vec<(String, String)> {
        self.agents
            .0
            .iter()
            .map(|d| (d.name.clone(), d.description.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explore_agent_is_read_only() {
        let t = AgentTable::default();
        for n in ["Explore", "Plan"] {
            let d = t.get(n).unwrap();
            assert_eq!(d.mode, Some(Mode::Plan));
            let tools = d.tools.as_ref().unwrap();
            assert!(
                tools
                    .iter()
                    .all(|x| !matches!(x.as_str(), "Bash" | "Write" | "Edit" | "NotebookEdit"))
            );
            assert!(tools.contains(&"Read".to_string()));
        }
        assert!(t.get(GENERAL_PURPOSE).unwrap().tools.is_none());
    }

    #[test]
    fn project_agents_load_only_for_a_trusted_project() {
        use synthesis_settings::ProjectTrust::{Trusted, Untrusted};
        let def = |n: &str| AgentDef {
            name: n.into(),
            description: String::new(),
            prompt: String::new(),
            tools: None,
            mode: None,
        };
        let defs = || {
            [
                (AgentSource::User, def("mine")),
                (AgentSource::Project, def("repo")),
            ]
        };
        let (t, skipped) = AgentTable::from_sources(defs(), Untrusted);
        assert!(t.get("mine").is_some() && t.get("repo").is_none());
        assert_eq!(skipped, ["repo"]);
        let (t, skipped) = AgentTable::from_sources(defs(), Trusted);
        assert!(t.get("repo").is_some() && skipped.is_empty());
    }

    #[test]
    fn stricter_never_widens() {
        for a in Mode::ALL {
            for b in Mode::ALL {
                let s = stricter(a, b);
                assert!(rank(s) <= rank(a) && rank(s) <= rank(b), "{a:?} {b:?}");
            }
        }
        assert_eq!(stricter(Mode::Plan, Mode::BypassPermissions), Mode::Plan);
        assert_eq!(
            stricter(Mode::BypassPermissions, Mode::Default),
            Mode::Default
        );
    }
}
