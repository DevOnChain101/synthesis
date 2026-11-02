//! The instruction file a project's notes live in.

use std::path::{Path, PathBuf};

/// User: `$SYNT_HOME/AGENTS.md`. Project: the root's `AGENTS.md` when it exists
/// (the files the loader prefers), else `<root>/.synt/AGENTS.md`, which is loaded next to them.
pub fn project_file(root: &Path) -> PathBuf {
    Some(root.join("AGENTS.md"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| root.join(".synt").join("AGENTS.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_file_prefers_existing_synthesis_then_agents_else_dot_synthesis() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        assert_eq!(project_file(p), p.join(".synt/AGENTS.md"));
        std::fs::write(p.join("AGENTS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("AGENTS.md"));
        std::fs::write(p.join("AGENTS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("AGENTS.md"));
    }
}
