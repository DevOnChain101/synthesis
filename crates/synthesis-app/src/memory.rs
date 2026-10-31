//! The instruction file a project's notes live in.

use std::path::{Path, PathBuf};

/// User: `$SYNTHESIS_HOME/SYNTHESIS.md`. Project: the root's `SYNTHESIS.md` or `AGENTS.md` when one exists
/// (the files the loader prefers), else `<root>/.synthesis/SYNTHESIS.md`, which is loaded next to them.
pub fn project_file(root: &Path) -> PathBuf {
    ["SYNTHESIS.md", "AGENTS.md"]
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join(".synthesis").join("SYNTHESIS.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_file_prefers_existing_synthesis_then_agents_else_dot_synthesis() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        assert_eq!(project_file(p), p.join(".synthesis/SYNTHESIS.md"));
        std::fs::write(p.join("AGENTS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("AGENTS.md"));
        std::fs::write(p.join("SYNTHESIS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("SYNTHESIS.md"));
    }
}
