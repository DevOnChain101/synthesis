//! Project instruction files (CLAUDE.md / AGENTS.md style) with `@import` expansion
//! (port of claudecfg.js buildMemory / expandImports).
//!
//! Project files are untrusted data. Differences from Node, on purpose: a project file's imports
//! must resolve (after symlinks) inside the workspace, and `~/` imports are honoured only in
//! user-scope files, so a cloned repo cannot pull `~/.ssh/...` into the prompt.

use crate::is_secret_path;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use synthesis_core::{WorkspaceContext, contains, realpath_lenient};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Written by the user (e.g. `~/.synt/AGENTS.md`): preferences, still no permission grants.
    User,
    /// Comes from the repository: untrusted.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: Scope,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct InstructionLimits {
    pub file_cap: usize,
    pub total_cap: usize,
    pub max_depth: usize,
}

impl Default for InstructionLimits {
    fn default() -> Self {
        Self {
            file_cap: 32 * 1024,
            total_cap: 32 * 1024,
            // code.claude.com/docs/en/memory: "a maximum depth of four hops"
            max_depth: 4,
        }
    }
}

/// File names looked up in every directory from the workspace root down to the cwd. Per
/// directory only the first non-empty one wins (AGENTS.md > CLAUDE.md, owner decision
/// D2, the Codex one-file-per-directory rule); [`LOCAL_NAME`] is the private extra on top.
pub const DEFAULT_NAMES: [&str; 5] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    NOTES_NAME,
    LOCAL_NAME,
];

/// Gitignored personal notes: loaded next to the winning file, not instead of it.
pub const LOCAL_NAME: &str = "CLAUDE.local.md";

/// Where `#` notes go when the repository has no AGENTS.md: loaded next to the winning
/// file, never instead of it.
pub const NOTES_NAME: &str = ".synt/AGENTS.md";

struct Ctx<'a> {
    ws: &'a WorkspaceContext,
    home: Option<&'a Path>,
    limits: &'a InstructionLimits,
    remaining: usize,
    /// Read-deny rule from the permission layer, as for mentions.
    deny: Option<&'a dyn Fn(&Path) -> bool>,
}

/// No instruction file or `@import`, user-scope included (a `#` note can plant an import), may
/// pull secret files (or anything the permission layer denies) into the prompt.
fn read_denied(real: &Path, ctx: &Ctx) -> bool {
    is_secret_path(real) || ctx.deny.is_some_and(|d| d(real))
}

fn inside_workspace(ws: &WorkspaceContext, real: &Path) -> bool {
    std::iter::once(&ws.root)
        .chain(&ws.additional_dirs)
        .chain([&ws.cwd])
        .any(|r| contains(r, real, ws.path_policy))
}

fn read_capped(path: &Path, cap: usize) -> Option<String> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(cap as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn resolve_import(token: &str, base: &Path, scope: Scope, ctx: &Ctx) -> Option<PathBuf> {
    let p = if let Some(rest) = token.strip_prefix("~/") {
        if scope != Scope::User {
            return None;
        }
        ctx.home?.join(rest)
    } else if Path::new(token).is_absolute() {
        if scope != Scope::User {
            return None;
        }
        PathBuf::from(token)
    } else {
        base.join(token)
    };
    let real = realpath_lenient(&p)?;
    (scope == Scope::User || inside_workspace(ctx.ws, &real))
        .then_some(real)
        .filter(|r| !read_denied(r, ctx))
}

fn expand_imports(
    content: &str,
    base: &Path,
    scope: Scope,
    depth: usize,
    chain: &mut Vec<PathBuf>,
    ctx: &mut Ctx,
) -> String {
    if depth >= ctx.limits.max_depth {
        return content.to_string();
    }
    let mut in_fence = false;
    let lines: Vec<String> = content
        .lines()
        .map(|line| {
            let t = line.trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                in_fence = !in_fence;
                return line.to_string();
            }
            if in_fence {
                return line.to_string();
            }
            let mut out = String::new();
            let mut rest = line;
            let mut prev_ws = true;
            let mut in_span = false;
            while let Some(c) = rest.chars().next() {
                if c == '`' {
                    // import parsing skips inline code spans (official memory docs)
                    in_span = !in_span;
                }
                if c == '@' && prev_ws && !in_span {
                    let tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    let tok = &rest[1..tok_end];
                    if let Some(text) = import_one(tok, base, scope, depth, chain, ctx) {
                        out.push_str(&text);
                    } else {
                        out.push_str(&rest[..tok_end]);
                    }
                    rest = &rest[tok_end..];
                    prev_ws = false;
                    continue;
                }
                prev_ws = c.is_whitespace();
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
            out
        })
        .collect();
    lines.join("\n")
}

fn import_one(
    tok: &str,
    base: &Path,
    scope: Scope,
    depth: usize,
    chain: &mut Vec<PathBuf>,
    ctx: &mut Ctx,
) -> Option<String> {
    if tok.is_empty() || tok.starts_with('@') {
        return None;
    }
    let real = resolve_import(tok, base, scope, ctx)?;
    if chain.contains(&real) {
        return None; // cycle: leave the text as written
    }
    let text = read_capped(&real, ctx.limits.file_cap.min(ctx.remaining))?;
    ctx.remaining = ctx.remaining.saturating_sub(text.len());
    chain.push(real.clone());
    let dir = real.parent().unwrap_or(&real).to_path_buf();
    let out = expand_imports(&text, &dir, scope, depth + 1, chain, ctx);
    chain.pop();
    Some(out)
}

fn load_one(path: &Path, scope: Scope, ctx: &mut Ctx) -> Option<String> {
    let real = realpath_lenient(path)?;
    if scope == Scope::Project && !inside_workspace(ctx.ws, &real) || read_denied(&real, ctx) {
        return None; // e.g. a CLAUDE.md symlinked to a secret
    }
    let text = read_capped(&real, ctx.limits.file_cap.min(ctx.remaining))?;
    ctx.remaining = ctx.remaining.saturating_sub(text.len());
    let dir = real.parent().unwrap_or(&real).to_path_buf();
    Some(expand_imports(&text, &dir, scope, 0, &mut vec![real], ctx))
}

/// User files first, then project files from the workspace root down to the cwd (outermost
/// first, as Node). Within a directory a file whose content equals an earlier one is skipped
/// (CLAUDE.md and AGENTS.md are often copies). `home` is only used for `~/` imports in user files.
pub fn load_instructions(
    ws: &WorkspaceContext,
    user_files: &[PathBuf],
    home: Option<&Path>,
    names: &[&str],
    limits: &InstructionLimits,
) -> Vec<InstructionFile> {
    load_instructions_with(ws, user_files, home, names, limits, None)
}

/// [`load_instructions`] plus the permission layer's read-deny predicate (`true` = do not read).
pub fn load_instructions_with(
    ws: &WorkspaceContext,
    user_files: &[PathBuf],
    home: Option<&Path>,
    names: &[&str],
    limits: &InstructionLimits,
    deny: Option<&dyn Fn(&Path) -> bool>,
) -> Vec<InstructionFile> {
    let mut ctx = Ctx {
        ws,
        home,
        limits,
        remaining: limits.total_cap,
        deny,
    };
    let mut out = Vec::new();
    for f in user_files {
        if let Some(content) = load_one(f, Scope::User, &mut ctx) {
            out.push(InstructionFile {
                path: f.clone(),
                scope: Scope::User,
                content,
            });
        }
    }
    let mut dirs: Vec<&Path> = ws
        .cwd
        .ancestors()
        .take_while(|d| contains(&ws.root, d, ws.path_policy))
        .collect();
    if dirs.is_empty() {
        dirs.push(&ws.cwd);
    }
    dirs.reverse();
    for dir in dirs.into_iter().filter(|d| Some(*d) != home) {
        let mut won = false;
        for name in names {
            let path = dir.join(name);
            let Some(raw) = read_capped(&path, limits.file_cap) else {
                continue;
            };
            let extra = *name == LOCAL_NAME || *name == NOTES_NAME;
            if raw.trim().is_empty() || (won && !extra) {
                continue;
            }
            won |= !extra;
            if let Some(content) = load_one(&path, Scope::Project, &mut ctx) {
                out.push(InstructionFile {
                    path,
                    scope: Scope::Project,
                    content,
                });
            }
        }
    }
    out
}
