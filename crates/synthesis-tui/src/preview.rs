//! The diff an Edit / MultiEdit / Write would make, for the approval dialog: the same structure
//! and numbering the tool reports afterwards, computed from the file as it is now. Display only,
//! best effort: a file that cannot be read or does not contain the text just has no preview.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Largest file read for a preview.
const MAX_BYTES: u64 = 1 << 20;

fn resolve(cwd: &Path, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// `None`: not a file-change tool, the file is outside `cwd`, unreadable or too large, or an
/// edit does not apply.
pub fn change_diff(cwd: &Path, tool: &str, input: &Value) -> Option<Value> {
    if !matches!(tool, "Edit" | "MultiEdit" | "Write") {
        return None;
    }
    let file = input.get("file_path")?.as_str()?;
    let abs = resolve(cwd, file);
    // only files the session may touch anyway; never a read outside the workspace
    let root = cwd.canonicalize().ok()?;
    let exists = abs.exists();
    let real = if exists {
        abs.canonicalize().ok()?
    } else {
        abs.parent()?.canonicalize().ok()?.join(abs.file_name()?)
    };
    if !real.starts_with(&root) {
        return None;
    }
    let before = if exists {
        if std::fs::metadata(&real).ok()?.len() > MAX_BYTES {
            return None;
        }
        String::from_utf8(std::fs::read(&real).ok()?).ok()?
    } else {
        String::new()
    };
    if tool == "Write" {
        let content = input.get("content")?.as_str()?;
        let kind = if exists { "overwrite" } else { "create" };
        return Some(synthesis_tools::file_diff(file, kind, &before, content));
    }
    let edits: Vec<&Value> = match input.get("edits").and_then(Value::as_array) {
        Some(a) => a.iter().collect(),
        None => vec![input],
    };
    let mut text = before.clone();
    for e in edits {
        let old = e.get("old_string")?.as_str()?;
        let new = e.get("new_string")?.as_str()?;
        if old.is_empty() || !text.contains(old) {
            return None;
        }
        text = if e.get("replace_all").and_then(Value::as_bool) == Some(true) {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
    }
    Some(synthesis_tools::file_diff(file, "update", &before, &text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_is_numbered_from_the_real_file_and_outside_paths_have_no_preview() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path().canonicalize().unwrap();
        std::fs::write(cwd.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let v = change_diff(
            &cwd,
            "Edit",
            &json!({"file_path": "a.txt", "old_string": "three", "new_string": "3"}),
        )
        .unwrap();
        let line = &v["hunks"][0]["lines"];
        assert!(line.to_string().contains("\"old_no\":3"), "{v}");
        assert_eq!(
            (v["added"].as_u64(), v["removed"].as_u64()),
            (Some(1), Some(1))
        );
        let missing = json!({"file_path": "a.txt", "old_string": "nope", "new_string": "x"});
        assert!(change_diff(&cwd, "Edit", &missing).is_none());
        let outside = json!({"file_path": "/etc/hosts", "old_string": "a", "new_string": "b"});
        assert!(change_diff(&cwd, "Edit", &outside).is_none());
        let w = change_diff(
            &cwd,
            "Write",
            &json!({"file_path": "n.txt", "content": "x\ny\n"}),
        )
        .unwrap();
        assert_eq!(w["kind"], "create");
    }
}
