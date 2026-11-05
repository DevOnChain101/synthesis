use std::fs;
use std::time::SystemTime;

use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde_json::Value;
use synthesis_core::ToolContext;

use crate::ToolOutput;
use crate::util::*;

const MAX_RESULTS: usize = 100;

pub(crate) fn run(input: &Value, ctx: &ToolContext) -> ToolOutput {
    let Some(pattern) = str_field(input, "pattern") else {
        return ToolOutput::error("Glob: \"pattern\" is required.");
    };
    let base = match resolve(ctx, "Glob", str_field(input, "path").unwrap_or("")) {
        Ok(p) => p,
        Err(m) => return ToolOutput::error(m),
    };
    // `*` never crosses `/`; case-sensitive like Node's glob (checked on macOS).
    let matcher = match GlobBuilder::new(pattern).literal_separator(true).build() {
        Ok(g) => g.compile_matcher(),
        Err(e) => return ToolOutput::error(format!("Glob: {e}")),
    };
    // Node skips node_modules/.git unless the pattern names them, and (minimatch dot:false)
    // dot-entries unless some pattern segment starts with a dot.
    // shortcut: any dotted segment enables hidden entries everywhere, not per segment.
    let skip_vcs = !(pattern.contains("node_modules") || pattern.contains(".git"));
    let allow_hidden = pattern.split(['/', '\\']).any(|s| s.starts_with('.'));

    let walk = WalkBuilder::new(&base)
        .standard_filters(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !(skip_vcs && (name == "node_modules" || name == ".git"))
                && (allow_hidden || !name.starts_with('.'))
        })
        .build();

    // secret-shaped entries are listed only when the pattern itself names them
    let explicit = synthesis_core::secrets::names_secret(pattern);
    let mut found: Vec<(std::path::PathBuf, SystemTime)> = Vec::new();
    for entry in walk.flatten() {
        if ctx.cancel.is_cancelled() {
            return ToolOutput::error("Glob: cancelled.");
        }
        let Ok(rel) = entry.path().strip_prefix(&base) else {
            continue;
        };
        if rel.as_os_str().is_empty() || !matcher.is_match(rel) {
            continue;
        }
        if (!explicit || ctx.skip_secrets)
            && synthesis_core::secrets::is_secret_shaped(entry.path())
        {
            continue;
        }
        let mtime = fs::metadata(entry.path())
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((entry.into_path(), mtime));
    }
    found.sort_by_key(|a| std::cmp::Reverse(a.1)); // stable: ties keep name order

    if found.is_empty() {
        return ToolOutput::text("No files matched.");
    }
    let mut text = found
        .iter()
        .take(MAX_RESULTS)
        .map(|(p, _)| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if found.len() > MAX_RESULTS {
        text += &format!(
            "\n\n… truncated to {MAX_RESULTS} of {} matches.",
            found.len()
        );
    }
    ToolOutput::text(text)
}
