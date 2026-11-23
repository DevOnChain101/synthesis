//! Structured line diff put in `ToolOutput.meta.diff` by Edit / Write / NotebookEdit, computed
//! where the whole file is known so the UI can show real line numbers. The model-facing text is
//! not affected. Shape (also in docs/specs/tools.md):
//! `{path, kind: update|create|overwrite|delete, added, removed, truncated, hidden?,
//!   hunks: [{old_start, old_lines, new_start, new_lines, lines: [{tag, old_no?, new_no?, text}]}]}`

use serde_json::{Value, json};
use similar::{ChangeTag, DiffTag, TextDiff};
use std::time::Duration;

const CONTEXT: usize = 3;
/// Diff lines kept; beyond this `truncated` is true (`added` / `removed` stay exact).
const MAX_LINES: usize = 2000;
/// Bytes of line text kept (the event carries this value as is).
const MAX_BYTES: usize = 64 * 1024;

fn start(range_start: usize, len: usize) -> usize {
    // unified-diff convention: an empty range names the line before it
    if len == 0 {
        range_start
    } else {
        range_start + 1
    }
}

/// `hide`: a secret file, counts only, no text.
pub(crate) fn build(path: &str, kind: &str, old: &str, new: &str, hide: bool) -> Value {
    // a private key in either side hides the text whatever the file is called
    let hide = hide || old.contains("PRIVATE KEY-----") || new.contains("PRIVATE KEY-----");
    let diff = TextDiff::configure()
        .timeout(Duration::from_secs(2))
        .diff_lines(old, new);
    let (mut added, mut removed) = (0, 0);
    for op in diff.ops() {
        if op.tag() != DiffTag::Equal {
            removed += op.old_range().len();
            added += op.new_range().len();
        }
    }
    let (mut hunks, mut left, mut bytes, mut truncated) = (vec![], MAX_LINES, 0, false);
    for group in diff.grouped_ops(CONTEXT) {
        if hide || truncated {
            break;
        }
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (o, n) = (
            first.old_range().start..last.old_range().end,
            first.new_range().start..last.new_range().end,
        );
        let mut lines = vec![];
        'ops: for op in &group {
            for c in diff.iter_changes(op) {
                let text = c.value().trim_end_matches(['\n', '\r']);
                if left == 0 || bytes + text.len() > MAX_BYTES {
                    truncated = true;
                    break 'ops;
                }
                left -= 1;
                bytes += text.len();
                let tag = match c.tag() {
                    ChangeTag::Equal => " ",
                    ChangeTag::Insert => "+",
                    ChangeTag::Delete => "-",
                };
                let mut l = json!({"tag": tag, "text": text});
                if let Some(i) = c.old_index() {
                    l["old_no"] = json!(i + 1);
                }
                if let Some(i) = c.new_index() {
                    l["new_no"] = json!(i + 1);
                }
                lines.push(l);
            }
        }
        hunks.push(json!({
            "old_start": start(o.start, o.len()), "old_lines": o.len(),
            "new_start": start(n.start, n.len()), "new_lines": n.len(),
            "lines": lines,
        }));
    }
    let mut v = json!({
        "path": path, "kind": kind, "added": added, "removed": removed,
        "hunks": hunks, "truncated": truncated,
    });
    if hide {
        v["hidden"] = json!(true);
    }
    v
}

/// `build` for a file path: secret files get counts only.
pub fn for_file(path: &str, kind: &str, old: &str, new: &str) -> Value {
    let hide = // no root here: the whole path counts (a diff hidden too often is safe)
    synthesis_core::secrets::is_secret_shaped_in(std::path::Path::new(path), std::path::Path::new(""));
    build(path, kind, old, new, hide)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &Value, h: usize) -> Vec<String> {
        v["hunks"][h]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                format!(
                    "{}{}:{}:{}",
                    l["tag"].as_str().unwrap(),
                    l["old_no"],
                    l["new_no"],
                    l["text"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("l{i}\n")).collect()
    }

    fn counts(v: &Value) -> (Value, Value) {
        (v["added"].clone(), v["removed"].clone())
    }

    #[test]
    fn single_change_has_context_and_numbers() {
        let old = numbered(10);
        let new = old.replace("l5\n", "five\nfive b\n");
        let v = build("a", "update", &old, &new, false);
        assert_eq!(counts(&v), (json!(2), json!(1)));
        let h = &v["hunks"][0];
        assert_eq!(
            [
                &h["old_start"],
                &h["old_lines"],
                &h["new_start"],
                &h["new_lines"]
            ],
            [&json!(2), &json!(7), &json!(2), &json!(8)]
        );
        let l = lines(&v, 0);
        assert_eq!(l[0], " 2:2:l2");
        assert_eq!(l[3], "-5:null:l5");
        assert_eq!(l[4], "+null:5:five");
        assert_eq!(l[6], " 6:7:l6");
    }

    #[test]
    fn far_apart_changes_make_two_hunks() {
        let old = numbered(30);
        let new = old.replace("l3\n", "x\n").replace("l28\n", "y\n");
        let v = build("a", "update", &old, &new, false);
        assert_eq!(v["hunks"].as_array().unwrap().len(), 2);
        assert_eq!(v["hunks"][1]["old_start"], 25);
    }

    #[test]
    fn first_last_line_whole_replace_and_empty() {
        let v = build("a", "update", "a\nb\nc\n", "A\nb\nC\n", false);
        assert_eq!(counts(&v), (json!(2), json!(2)));
        assert_eq!(v["hunks"].as_array().unwrap().len(), 1);
        let v = build("a", "overwrite", "a\nb\n", "x\ny\nz\n", false);
        assert_eq!(counts(&v), (json!(3), json!(2)));
        let v = build("a", "create", "", "one\ntwo\n", false);
        let h = &v["hunks"][0];
        assert_eq!(
            [&h["old_start"], &h["old_lines"], &h["new_start"]],
            [&json!(0), &json!(0), &json!(1)]
        );
        assert_eq!(lines(&v, 0), ["+null:1:one", "+null:2:two"]);
        let v = build("a", "update", "", "", false);
        assert!(v["hunks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn crlf_no_trailing_newline_unicode_and_tabs() {
        let v = build("a", "update", "a\r\nb\r\nc", "a\r\nB\r\nc", false);
        assert_eq!(lines(&v, 0), [" 1:1:a", "-2:null:b", "+null:2:B", " 3:3:c"]);
        // last line gains a newline: that line differs, nothing panics
        let v = build("a", "update", "x\ny", "x\ny\n", false);
        assert_eq!(counts(&v), (json!(1), json!(1)));
        let v = build("a", "update", "\tcafé\n日本\n", "\tcafé\n日本語\n", false);
        assert_eq!(
            lines(&v, 0),
            [" 1:1:\tcafé", "-2:null:日本", "+null:2:日本語"]
        );
    }

    #[test]
    fn caps_and_secrets() {
        let big = numbered(5000);
        let v = build("a", "create", "", &big, false);
        assert_eq!(v["added"], 5000);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["hunks"][0]["lines"].as_array().unwrap().len(), MAX_LINES);
        let v = for_file("dir/.env", "overwrite", "A=1\n", "A=2\nB=3\n");
        assert_eq!(v["hidden"], true);
        assert_eq!(counts(&v), (json!(2), json!(1)));
        assert!(v["hunks"].as_array().unwrap().is_empty());
        assert!(!v.to_string().contains("A=2"));
        let key =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRETBODY\n-----END OPENSSH PRIVATE KEY-----\n";
        let v = for_file("notes.txt", "create", "", key);
        assert_eq!(v["hidden"], true);
        assert!(!v.to_string().contains("SECRETBODY"));
    }
}
