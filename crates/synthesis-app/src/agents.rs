//! Custom sub-agent definitions: `<synthesis_home>/agents/*.md` (user) and
//! `<project_root>/.synthesis/agents/*.md` (project, repository content: only for a trusted project,
//! enforced by `AgentTable::from_sources`). Format as Node `loadAgentsDir`: Markdown with a
//! `---` frontmatter (`name`, `description`, `tools`); the body is the system prompt.
//! `model`/`profile`/`reasoning` of the Node format are not supported by the Rust harness.

use std::io::Read;
use std::path::Path;

use synthesis_harness::{AgentDef, AgentSource, AgentTable};
use synthesis_settings::ProjectTrust;

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_FILES: usize = 100;

/// `(frontmatter pairs, body)`; no closing `---` means no frontmatter.
fn frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let none = (vec![], text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return none;
    };
    let mut pos = 0;
    let mut data = vec![];
    for line in rest.split_inclusive('\n') {
        pos += line.len();
        let l = line.trim_end_matches(['\r', '\n']);
        if l.trim() == "---" {
            return (data, &rest[pos..]);
        }
        if let Some((k, v)) = l.split_once(':') {
            data.push((
                k.trim().to_string(),
                v.trim().trim_matches(['"', '\'']).to_string(),
            ));
        }
    }
    none
}

fn tool_list(v: &str) -> Vec<String> {
    let v = v
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(v);
    v.split(',')
        .map(|t| t.trim().trim_matches(['"', '\'']).to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse(file_stem: &str, raw: &str) -> AgentDef {
    let (data, body) = frontmatter(raw);
    let get = |k: &str| data.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    AgentDef {
        name: get("name")
            .filter(|n| !n.is_empty())
            .unwrap_or(file_stem)
            .to_string(),
        description: get("description").unwrap_or("").to_string(),
        prompt: body.trim().to_string(),
        tools: get("tools").map(tool_list),
        mode: None,
    }
}

/// Regular `*.md` files only (a symlink in a hostile repo is skipped), capped in size and count.
fn scan(dir: &Path, source: AgentSource, out: &mut Vec<(AgentSource, AgentDef)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .flatten()
        .filter(|e| {
            e.path().extension().is_some_and(|x| x == "md")
                && e.file_type().is_ok_and(|t| t.is_file())
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    for p in files.into_iter().take(MAX_FILES) {
        let mut raw = String::new();
        let ok = std::fs::File::open(&p)
            .and_then(|f| f.take(MAX_FILE_BYTES).read_to_string(&mut raw))
            .is_ok();
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if ok && !stem.is_empty() {
            out.push((source, parse(stem, &raw)));
        }
    }
}

pub struct Loaded {
    pub table: AgentTable,
    /// Names of project agents that were not loaded (project not trusted).
    pub skipped: Vec<String>,
    pub user: usize,
    pub project: usize,
}

pub fn load(synthesis_home: &Path, project_root: &Path, trust: ProjectTrust) -> Loaded {
    let mut defs = vec![];
    scan(&synthesis_home.join("agents"), AgentSource::User, &mut defs);
    let user = defs.len();
    scan(
        &project_root.join(".synthesis").join("agents"),
        AgentSource::Project,
        &mut defs,
    );
    let project = defs.len() - user;
    let (table, skipped) = AgentTable::from_sources(defs, trust);
    Loaded {
        table,
        skipped,
        user,
        project,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, f: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(f), text).unwrap();
    }

    #[test]
    fn project_agents_load_only_when_trusted() {
        let t = tempfile::tempdir().unwrap();
        let (home, root) = (t.path().join("home"), t.path().join("repo"));
        write(
            &home.join("agents"),
            "rev.md",
            "---\nname: reviewer\ndescription: reviews\ntools: [Read, Grep]\n---\nBe strict.\n",
        );
        write(&root.join(".synthesis/agents"), "evil.md", "no frontmatter");
        let l = load(&home, &root, ProjectTrust::Untrusted);
        let r = l.table.get("reviewer").unwrap();
        assert_eq!(
            r.tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string()][..])
        );
        assert_eq!(r.prompt, "Be strict.");
        assert!(l.table.get("evil").is_none());
        assert_eq!(
            (l.skipped.as_slice(), l.user, l.project),
            (&["evil".to_string()][..], 1, 1)
        );
        let l = load(&home, &root, ProjectTrust::Trusted);
        assert!(l.table.get("evil").is_some() && l.skipped.is_empty());
    }
}
