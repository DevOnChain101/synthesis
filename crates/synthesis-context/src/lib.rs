//! Context engineering (design doc 5.5): assembly in the documented order, token budgeting,
//! compaction with valid tool_use/tool_result pairing, long-output truncation with artifact
//! references, `@mention` expansion and instruction-file loading.
//!
//! Repository files, mentioned files and tool output are untrusted data. They are always emitted
//! inside delimited blocks (see [`REPO_INSTRUCTIONS_TAG`]) and closing tags inside them are
//! defanged so content cannot break out of its block.

mod assemble;
mod compact;
mod estimate;
mod goal;
mod images;
mod instructions;
mod mentions;
mod meter;
mod pairing;
mod state;
mod truncate;

pub use assemble::*;
pub use compact::*;
pub use estimate::*;
pub use goal::*;
pub use images::*;
pub use instructions::*;
pub use mentions::*;
pub use meter::*;
pub use pairing::*;
pub use state::*;
pub use truncate::*;

/// Tag that delimits untrusted repository instructions in the system prompt.
pub const REPO_INSTRUCTIONS_TAG: &str = "repository_instructions";

const FENCE_TAGS: [&str; 9] = [
    "memory",
    "note",
    "file",
    "directory",
    "repository_instructions",
    "user_instructions",
    "task_state",
    "relevant_files",
    "recent_tool_evidence",
];

/// Neutralises our closing tags inside untrusted text (`</file>` becomes `<\/file>`), ignoring
/// case and whitespace inside the tag (`</FILE >`, `</ file>`).
pub(crate) fn defang(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("</") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let name = after.trim_start();
        let n = name
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(name.len());
        let hit = name[n..].trim_start().starts_with('>')
            && FENCE_TAGS
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&name[..n]));
        out.push_str(if hit { "<\\/" } else { "</" });
        rest = after;
    }
    out.push_str(rest);
    out
}

pub use synthesis_core::secrets::is_secret_path;

pub(crate) fn attr(s: &str) -> String {
    s.replace('"', "&quot;").replace(['\n', '\r'], " ")
}
