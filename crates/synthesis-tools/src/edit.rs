use serde_json::{Value, json};
use synthesis_core::ToolContext;

use crate::ToolOutput;
use crate::util::*;

pub(crate) fn run(input: &Value, ctx: &ToolContext) -> ToolOutput {
    let Some(file_path) = str_field(input, "file_path") else {
        return ToolOutput::error("Edit: \"file_path\" is required.");
    };
    let (Some(old), Some(new)) = (
        input.get("old_string").and_then(Value::as_str),
        input.get("new_string").and_then(Value::as_str),
    ) else {
        return ToolOutput::error("Edit: \"old_string\" and \"new_string\" are required.");
    };
    if old == new {
        return ToolOutput::soft_error(
            "Edit: \"old_string\" and \"new_string\" must be different.",
        );
    }
    let replace_all = input.get("replace_all").is_some_and(|v| {
        // JS truthiness of the flag.
        v.as_bool()
            .unwrap_or_else(|| !v.is_null() && v.as_f64() != Some(0.0))
    });

    let abs = match resolve(ctx, "Edit", file_path) {
        Ok(p) => p,
        Err(m) => return ToolOutput::error(m),
    };
    let root = match open_root(ctx, "Edit", &abs) {
        Ok(r) => r,
        Err(o) => return o,
    };
    let md = match root.metadata(&abs) {
        Ok(m) => m,
        Err(e) => {
            let msg = format!(
                "Edit: cannot access {}: {}",
                abs.display(),
                io_msg(&e, &abs)
            );
            return if e.kind() == std::io::ErrorKind::NotFound {
                ToolOutput::soft_error(msg)
            } else {
                ToolOutput::error(msg)
            };
        }
    };
    if md.is_dir() {
        return ToolOutput::soft_error(format!("Edit: {} is a directory.", abs.display()));
    }
    if !is_fresh(ctx, &abs, mtime(&md)) {
        return ToolOutput::soft_error(format!(
            "Edit: {} has not been read in this conversation (or changed since). Read it first.",
            abs.display()
        ));
    }

    // Deliberate change vs Node: Node decodes lossily and writes back, which would corrupt
    // non-UTF-8 files; refuse instead.
    let content = match root
        .read_capped(&abs, MAX_FILE_BYTES)
        .map(String::from_utf8)
    {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            return ToolOutput::error(format!(
                "Edit: {} is not valid UTF-8; refusing to rewrite it.",
                abs.display()
            ));
        }
        Err(e) => return ToolOutput::error(format!("Edit: cannot read {}: {e}", abs.display())),
    };
    // A CRLF file edited with an LF-only old_string is matched on LF text and written back
    // as CRLF (every LF, as Node does).
    let normalize = content.contains("\r\n") && !old.contains("\r\n");
    let hay = if normalize {
        content.replace("\r\n", "\n")
    } else {
        content
    };

    let occurrences = if old.is_empty() {
        0
    } else {
        hay.matches(old).count()
    };
    if occurrences == 0 {
        return ToolOutput::soft_error(format!(
            "Edit: \"old_string\" was not found in {}.",
            abs.display()
        ));
    }
    if occurrences > 1 && !replace_all {
        return ToolOutput::soft_error(format!(
            "Edit: \"old_string\" matches {occurrences} times in {}. Add more context to make it unique, or pass replace_all: true.",
            abs.display()
        ));
    }

    let first = hay.find(old).unwrap_or(0);
    let (replaced, count) = if replace_all {
        (hay.replace(old, new), occurrences)
    } else {
        (hay.replacen(old, new, 1), 1)
    };
    let final_text = if normalize {
        replaced.replace('\n', "\r\n")
    } else {
        replaced.clone()
    };
    if let Err(e) = root.write(&abs, &final_text) {
        return ToolOutput::error(format!("Edit: cannot write {}: {e}", abs.display()));
    }
    match root.metadata(&abs) {
        Ok(m) => ledger_set(ctx, &abs, mtime(&m)),
        Err(e) => return ToolOutput::error(format!("Edit: cannot access {}: {e}", abs.display())),
    }

    let start_line = replaced[..first].matches('\n').count() + 1;
    let new_lines = new.matches('\n').count() + 1;
    let all: Vec<&str> = replaced.split('\n').collect();
    let snippet_start = start_line.saturating_sub(2).max(1);
    let snippet_end = all.len().min(start_line + new_lines + 1);
    let snippet = all[snippet_start - 1..snippet_end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{l}", snippet_start + i))
        .collect::<Vec<_>>()
        .join("\n");

    ToolOutput::text(format!("Edited {}:\n{snippet}", abs.display())).with_meta(json!({
        "replacements": count,
        "diff": crate::diff::for_file(&ctx.workspace.secrets, &ctx.workspace.root, file_path, "update", &hay, &replaced),
    }))
}
