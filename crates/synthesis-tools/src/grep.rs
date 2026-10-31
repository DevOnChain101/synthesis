//! Grep with the semantics of Node's pure-JS fallback (the golden capture forces it), on the
//! Rust `regex` engine. No ripgrep subprocess.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use synthesis_core::ToolContext;

use crate::ToolOutput;
use crate::util::*;

const MAX_OUTPUT_LINES: usize = 2000;
/// Longest matched/context line echoed back (chars); longer ones get a marker.
const MAX_LINE_CHARS: usize = 2000;
/// Cap on total output bytes; the scan stops once the collected output passes it.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

fn clip_line(s: &str) -> String {
    match s.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => format!("{} … [line truncated]", &s[..i]),
        None => s.to_string(),
    }
}

const TYPE_EXT: &[(&str, &[&str])] = &[
    ("js", &[".js", ".jsx", ".mjs", ".cjs"]),
    ("ts", &[".ts", ".tsx"]),
    ("py", &[".py"]),
    ("rust", &[".rs"]),
    ("go", &[".go"]),
    ("java", &[".java"]),
    ("c", &[".c", ".h"]),
    ("cpp", &[".cpp", ".cc", ".cxx", ".hpp", ".hh"]),
    ("json", &[".json"]),
    ("md", &[".md", ".markdown"]),
    ("html", &[".html", ".htm"]),
    ("css", &[".css", ".scss", ".less"]),
    ("yaml", &[".yaml", ".yml"]),
    ("sh", &[".sh", ".bash"]),
];

/// Port of Node's minimal glob (`**`, `*`, `?`), no braces. Case-insensitive only where the
/// filesystem is (Windows, macOS). Matched against `/`-separated relative paths.
fn glob_to_regex(glob: &str) -> Regex {
    let mut re = String::from(if cfg!(any(windows, target_os = "macos")) {
        "(?is)^"
    } else {
        "(?s)^"
    });
    let mut it = glob.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '*' if it.peek() == Some(&'*') => {
                it.next();
                re.push_str(".*");
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).expect("escaped glob is a valid regex")
}

/// Whether `rel` (workspace-relative; `\` is a separator only when `win`) or its file name
/// matches the glob regex. Paths are folded to `/` first so one glob works on every OS.
fn glob_hits(re: &Regex, rel: &str, win: bool) -> bool {
    let rel = if win {
        rel.replace('\\', "/")
    } else {
        rel.to_string()
    };
    re.is_match(&rel) || re.is_match(rel.rsplit('/').next().unwrap_or(&rel))
}

fn collect_files(search: &Path, is_dir: bool, ctx: &ToolContext) -> Vec<PathBuf> {
    let secret = |p: &Path| {
        ctx.skip_secrets
            && (synthesis_core::secrets::is_secret_shaped(p)
                || std::fs::canonicalize(p)
                    .is_ok_and(|r| synthesis_core::secrets::is_secret_shaped(&r)))
    };
    if !is_dir {
        return if secret(search) {
            vec![]
        } else {
            vec![search.to_path_buf()]
        };
    }
    WalkBuilder::new(search)
        .standard_filters(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| {
            e.depth() == 0 || {
                let n = e.file_name();
                n != "node_modules" && n != ".git"
            }
        })
        .build()
        .flatten()
        .take_while(|_| !ctx.cancel.is_cancelled())
        // symlinks are not followed (`is_file` is false for them), so a link to a secret is skipped too
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(ignore::DirEntry::into_path)
        .filter(|p| !secret(p))
        .collect()
}

#[derive(PartialEq)]
enum Mode {
    Files,
    Content,
    Count,
}

fn search(
    input: &Value,
    ctx: &ToolContext,
    root: &Root,
    search_path: &Path,
    is_dir: bool,
) -> Result<String, String> {
    let pattern = str_field(input, "pattern").unwrap_or_default();
    let flag = |k: &str| input.get(k).and_then(Value::as_bool).unwrap_or(false);
    let multiline = flag("multiline");
    let mode = match input.get("output_mode").and_then(Value::as_str) {
        Some("content") => Mode::Content,
        Some("count") => Mode::Count,
        _ => Mode::Files,
    };

    let mut files = collect_files(search_path, is_dir, ctx);
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    if let Some(g) = str_field(input, "glob") {
        let re = glob_to_regex(g);
        files.retain(|f| {
            let rel = f.strip_prefix(search_path).unwrap_or(f);
            glob_hits(&re, &rel.to_string_lossy(), cfg!(windows))
        });
    }
    if let Some(exts) = str_field(input, "type")
        .and_then(|t| TYPE_EXT.iter().find(|(k, _)| *k == t))
        .map(|(_, e)| *e)
    {
        files.retain(|f| {
            let ext = f
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                .unwrap_or_default();
            exts.contains(&ext.as_str())
        });
    }

    let re = RegexBuilder::new(pattern)
        .case_insensitive(flag("-i"))
        .multi_line(true)
        .dot_matches_new_line(multiline)
        .build()
        .map_err(|e| format!("invalid pattern: {e}"))?;

    let ctx_n = |k: &str| int_field(input, k);
    let before = ctx_n("-B").or(ctx_n("-C")).unwrap_or(0);
    let after = ctx_n("-A").or(ctx_n("-C")).unwrap_or(0);
    // Deliberate change vs the JS fallback: `-n: false` is honoured (schema promises it).
    let numbers = input.get("-n").and_then(Value::as_bool) != Some(false);
    let line_label = |file: &Path, n: usize, sep: char, text: &str| {
        let text = &clip_line(text);
        if numbers {
            format!("{}:{n}{sep}{text}", file.display())
        } else {
            format!(
                "{}{}{text}",
                file.display(),
                if sep == ':' { ':' } else { '-' }
            )
        }
    };

    let mut matching: Vec<(PathBuf, usize)> = Vec::new();
    let mut content: Vec<String> = Vec::new();
    let mut out_bytes = 0usize;
    for file in files {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        if out_bytes > MAX_OUTPUT_BYTES {
            break;
        }
        // Unreadable, non-regular and over-10-MiB files are skipped.
        let Ok(buf) = root.read_capped(&file, MAX_FILE_BYTES) else {
            continue;
        };
        if is_binary(&buf) {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let mut count = 0usize;
        if multiline {
            let mut starts = Vec::new();
            let mut acc = 0usize;
            for line in text.split('\n') {
                starts.push(acc);
                acc += line.len() + 1;
            }
            for m in re.find_iter(&text) {
                count += 1;
                if mode == Mode::Content {
                    let line_no = starts.partition_point(|&s| s <= m.start()).max(1);
                    let first = m.as_str().split('\n').next().unwrap_or("");
                    content.push(line_label(&file, line_no, ':', first));
                    out_bytes += content.last().map_or(0, String::len);
                }
            }
        } else {
            let lines: Vec<&str> = text.split('\n').collect();
            for (i, line) in lines.iter().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                count += 1;
                if out_bytes > MAX_OUTPUT_BYTES {
                    break; // bounds memory inside one huge file; run() adds the marker
                }
                if mode == Mode::Content {
                    let start = (i as i64 - before).max(0) as usize;
                    let end = (i as i64 + after).min(lines.len() as i64 - 1);
                    for j in start..=usize::try_from(end).unwrap_or(0).max(start) {
                        if j >= lines.len() {
                            break;
                        }
                        let sep = if j == i { ':' } else { '-' };
                        content.push(line_label(&file, j + 1, sep, lines[j]));
                        out_bytes += content.last().map_or(0, String::len);
                    }
                    if before != 0 || after != 0 {
                        content.push("--".to_string());
                    }
                }
            }
        }
        if count > 0 {
            matching.push((file, count));
        }
    }

    Ok(match mode {
        Mode::Files => matching
            .iter()
            .map(|(f, _)| f.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Mode::Count => matching
            .iter()
            .map(|(f, c)| format!("{}:{c}", f.display()))
            .collect::<Vec<_>>()
            .join("\n"),
        Mode::Content => content.join("\n"),
    })
}

pub(crate) fn run(input: &Value, ctx: &ToolContext) -> ToolOutput {
    if str_field(input, "pattern").is_none() {
        return ToolOutput::error("Grep: \"pattern\" is required.");
    }
    let path = match resolve(ctx, "Grep", str_field(input, "path").unwrap_or("")) {
        Ok(p) => p,
        Err(m) => return ToolOutput::error(m),
    };
    let root = match open_root(ctx, "Grep", &path) {
        Ok(r) => r,
        Err(o) => return o,
    };
    let md = match root.metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return ToolOutput::error(format!(
                "Grep: cannot access {}: {}",
                path.display(),
                io_msg(&e, &path)
            ));
        }
    };
    let text = match search(input, ctx, &root, &path, md.is_dir()) {
        Ok(t) => t,
        Err(m) => return ToolOutput::error(format!("Grep: {m}")),
    };
    if text.is_empty() {
        return ToolOutput::text("No matches found.");
    }

    let (text, bytes_cut) = if text.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (&text[..end], true)
    } else {
        (text.as_str(), false)
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let truncated = lines.len() > MAX_OUTPUT_LINES;
    let mut out = lines[..lines.len().min(MAX_OUTPUT_LINES)].join("\n");
    let head = input
        .get("head_limit")
        .and_then(Value::as_f64)
        .filter(|&n| n > 0.0);
    if let Some(n) = head {
        out = out
            .split('\n')
            .take(n as usize)
            .collect::<Vec<_>>()
            .join("\n");
    }
    // Node: `!input.head_limit` is falsy only for 0/absent.
    let head_given = input
        .get("head_limit")
        .and_then(Value::as_f64)
        .is_some_and(|n| n != 0.0);
    if truncated && !head_given {
        out += &format!("\n\n… truncated to {MAX_OUTPUT_LINES} lines.");
    }
    if bytes_cut {
        out += &format!("\n\n… output truncated to {MAX_OUTPUT_BYTES} bytes.");
    }
    ToolOutput::text(out)
}

#[cfg(test)]
mod tests {
    use super::{glob_hits, glob_to_regex};

    #[test]
    fn glob_matches_slash_normalised_relative_paths() {
        let re = glob_to_regex("**/*.md");
        // Windows spelling: backslashes are folded before matching
        assert!(glob_hits(&re, r"sub\deep\c.md", true));
        assert!(!glob_hits(&re, r"sub\c.txt", true));
        let re = glob_to_regex("src/*.js");
        assert!(glob_hits(&re, r"src\a.js", true));
        assert!(!glob_hits(&re, r"src\deep\a.js", true));
        // POSIX: a backslash is an ordinary file-name character
        assert!(!glob_hits(&glob_to_regex("src/*.js"), r"src\a.js", false));
        assert!(glob_hits(&glob_to_regex("*.js"), "a.js", false));
    }
}
