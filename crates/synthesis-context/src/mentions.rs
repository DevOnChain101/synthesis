//! `@path` mentions in a user message (port of mentions.js). Every mention that resolves, via
//! `WorkspaceContext::resolve`, to an existing file or directory inside the workspace is appended
//! as a data block; anything else stays plain text.

use crate::{attr, defang, is_secret_path};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use synthesis_core::WorkspaceContext;

pub struct MentionOptions<'a> {
    pub file_cap: usize,
    pub total_cap: usize,
    pub dir_cap: usize,
    /// Read-deny rule from the permission layer: `true` means the path must not be inlined.
    pub deny: Option<&'a dyn Fn(&Path) -> bool>,
}

impl Default for MentionOptions<'_> {
    fn default() -> Self {
        Self {
            file_cap: 40 * 1024,
            total_cap: 120 * 1024,
            dir_cap: 200,
            deny: None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    /// Text to append to the user message (empty when nothing was mentioned).
    pub text: String,
    /// Files whose content was inlined (so the harness can record them as read).
    pub files: Vec<PathBuf>,
    /// Mentions that named nothing inside the workspace (they stay literal in the message).
    pub missing: Vec<String>,
}

/// `@name` words and `@"name with spaces"`; the flag is true for the quoted form.
fn tokens(text: &str) -> Vec<(&str, bool)> {
    let (mut out, mut s) = (Vec::new(), text);
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return out;
        }
        if let Some((name, rest)) = s.strip_prefix("@\"").and_then(|q| q.split_once('"')) {
            out.push((name, true));
            s = rest;
            continue;
        }
        let (t, rest) = s.split_at(s.find(char::is_whitespace).unwrap_or(s.len()));
        out.extend(t.strip_prefix('@').map(|n| (n, false)));
        s = rest;
    }
}

const IMAGE_EXT: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

fn file_block(abs: &Path, rel: &str, size: u64, cap: usize) -> std::io::Result<(String, usize)> {
    let mut buf = Vec::new();
    fs::File::open(abs)?
        .take(cap as u64)
        .read_to_end(&mut buf)?;
    if buf.iter().take(8000).any(|&b| b == 0) {
        return Ok((
            format!(
                "<file path=\"{}\">[binary file, {size} bytes]</file>",
                attr(rel)
            ),
            0,
        ));
    }
    let body = String::from_utf8_lossy(&buf);
    let body = body.strip_suffix('\n').unwrap_or(&body);
    let numbered: Vec<String> = body
        .split('\n')
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
        .collect();
    let note = if size > buf.len() as u64 {
        format!("\n[truncated, {size} bytes]")
    } else {
        String::new()
    };
    Ok((
        format!(
            "<file path=\"{}\">\n{}{note}\n</file>",
            attr(rel),
            defang(&numbered.join("\n"))
        ),
        buf.len(),
    ))
}

fn dir_block(abs: &Path, rel: &str, cap: usize) -> std::io::Result<String> {
    let mut names: Vec<String> = fs::read_dir(abs)?
        .filter_map(Result::ok)
        .map(|e| {
            e.file_name().to_string_lossy().into_owned() + if e.path().is_dir() { "/" } else { "" }
        })
        .collect();
    names.sort();
    let more = if names.len() > cap {
        format!("\n... {} more", names.len() - cap)
    } else {
        String::new()
    };
    names.truncate(cap);
    Ok(format!(
        "<directory path=\"{}\">\n{}{more}\n</directory>",
        attr(rel),
        defang(&names.join("\n"))
    ))
}

pub fn expand_mentions(text: &str, ws: &WorkspaceContext, opts: &MentionOptions) -> Mentions {
    let mut out = Mentions::default();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut blocks = Vec::new();
    let mut budget = opts.total_cap;
    for (tok, quoted) in tokens(text) {
        let trimmed = if quoted {
            tok
        } else {
            tok.trim_end_matches(['.', ',', ';', ':', ')'])
        };
        let hit = [tok, trimmed]
            .into_iter()
            .filter(|c| !c.is_empty())
            .find_map(|c| {
                let abs = ws.resolve(c).ok()?; // `..` and symlink escapes fail here
                let md = fs::metadata(&abs).ok()?;
                Some((abs, c.trim_end_matches('/').to_string(), md))
            });
        let Some((abs, rel, md)) = hit else {
            // `@server:scheme://uri` is an MCP resource, not a file
            if !tok.is_empty() && !tok.contains("://") && !out.missing.iter().any(|m| m == trimmed)
            {
                out.missing.push(trimmed.to_string());
            }
            continue;
        };
        if seen.contains(&abs) || !(md.is_file() || md.is_dir()) {
            continue;
        }
        seen.push(abs.clone());
        if abs
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| IMAGE_EXT.contains(&e.to_ascii_lowercase().as_str()))
        {
            continue; // images keep their attach path
        }
        if is_secret_path(&abs) || opts.deny.is_some_and(|d| d(&abs)) {
            blocks.push(format!(
                "<file path=\"{}\">[not included: blocked by permissions]</file>",
                attr(&rel)
            ));
        } else if md.is_dir() {
            blocks.extend(dir_block(&abs, &rel, opts.dir_cap).ok());
        } else if budget == 0 {
            blocks.push(format!(
                "<file path=\"{}\">[not included: size limit]</file>",
                attr(&rel)
            ));
        } else if let Ok((b, used)) = file_block(&abs, &rel, md.len(), opts.file_cap.min(budget)) {
            budget -= used;
            blocks.push(b);
            if used > 0 {
                out.files.push(abs);
            }
        }
    }
    if !blocks.is_empty() {
        out.text = format!("\n\n{}", blocks.join("\n"));
    }
    out
}
