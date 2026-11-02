//! Prompt autocompletion (port of `complete.js`):
//! `/` at the start of the prompt completes slash commands and skills, `@` anywhere completes
//! sub-agents first, then the entries of the typed folder whose names START with the typed text.
//! Only the context folders (cwd + additional directories) are ever listed.
//!
//! The filesystem is a trait ([`CompletionFs`], real implementation [`StdFs`]).
//! Differences from Node: no 5 s directory cache (re-read per call; add one if profiling asks
//! for it); names sort by a case-folded comparison, lowercase first on ties, instead of ICU
//! collation, so accents and punctuation can order differently; scores index by `char`, not
//! UTF-16 unit.

use crate::editor::LineEditor;
use crate::text::is_js_space;
use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};

pub const MAX_CHILDREN: usize = 2000;
pub const MAX_ITEMS: usize = 200;

pub struct DirEntry {
    pub name: String,
    /// A directory, or a link to one.
    pub is_dir: bool,
}

pub struct FileMeta {
    pub size: u64,
    pub mtime_ms: i64,
}

pub trait CompletionFs {
    /// Absolute path with links resolved, `None` when it does not exist.
    fn realpath(&self, p: &Path) -> Option<PathBuf>;
    /// Children of a directory in any order, `None` when unreadable.
    fn read_dir(&self, abs: &Path) -> Option<Vec<DirEntry>>;
    /// `lstat` of a path.
    fn lstat(&self, abs: &Path) -> Option<FileMeta>;
}

pub struct StdFs;

impl CompletionFs for StdFs {
    fn realpath(&self, p: &Path) -> Option<PathBuf> {
        dunce::canonicalize(p).ok()
    }

    fn read_dir(&self, abs: &Path) -> Option<Vec<DirEntry>> {
        let mut kids = Vec::new();
        for e in std::fs::read_dir(abs).ok()?.flatten() {
            let ft = e.file_type().ok();
            let mut is_dir = ft.is_some_and(|t| t.is_dir());
            if !is_dir && ft.is_some_and(|t| t.is_symlink()) {
                is_dir = std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()); // dangling link: a file
            }
            kids.push(DirEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        Some(kids)
    }

    fn lstat(&self, abs: &Path) -> Option<FileMeta> {
        let m = std::fs::symlink_metadata(abs).ok()?;
        let mtime_ms = m
            .modified()
            .ok()
            .map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_millis() as i64,
                Err(e) => -(e.duration().as_millis() as i64),
            })
            .unwrap_or(0);
        Some(FileMeta {
            size: m.len(),
            mtime_ms,
        })
    }
}

/// A slash command or skill.
#[derive(Clone, Debug, Default)]
pub struct Command {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub argument_hint: String,
    /// Local commands rank above skills with the same score.
    pub local: bool,
}

/// An agent that `@name` can address (offered only at the start of a message), or, with
/// `start_new`, a definition that `@agent:<id>` starts.
#[derive(Clone, Debug, Default)]
pub struct AgentRef {
    /// Instance name (or definition name when `start_new`).
    pub id: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub queued: usize,
    pub start_new: bool,
}

pub struct Options<'a> {
    pub commands: &'a [Command],
    pub cwd: &'a Path,
    pub agents: &'a [AgentRef],
    /// Additional directories, offered at the top level.
    pub dirs: &'a [PathBuf],
    pub limit: usize,
}

impl<'a> Options<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            commands: &[],
            cwd,
            agents: &[],
            dirs: &[],
            limit: MAX_ITEMS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    File,
    Command,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Clone, Debug)]
enum Detail {
    Fixed(String),
    /// Looked up lazily: one lstat only for rows that are actually drawn.
    Path {
        abs: PathBuf,
        is_dir: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Item {
    score: f64,
    order: usize,
    pub label: String,
    pub insert: String,
    /// The folder listing was cut at [`MAX_CHILDREN`] and this is its last row.
    pub more: bool,
    detail: Detail,
}

impl Item {
    /// Second column: command description, `folder`, or `size · age` of a file.
    pub fn detail(&self, fs: &dyn CompletionFs, now_ms: i64) -> String {
        match &self.detail {
            Detail::Fixed(s) => s.clone(),
            Detail::Path { abs, is_dir } => {
                let mut d = match (fs.lstat(abs), is_dir) {
                    (_, true) | (None, _) => {
                        if *is_dir {
                            "folder".into()
                        } else {
                            String::new()
                        }
                    }
                    (Some(m), false) => {
                        format!("{} · {}", fmt_size(m.size), fmt_age(now_ms - m.mtime_ms))
                    }
                };
                if self.more {
                    d.push_str(" … more");
                }
                d
            }
        }
    }
}

#[derive(Debug)]
pub struct Suggestions {
    pub token: Token,
    pub items: Vec<Item>,
}

fn fmt_size(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1} kB", n as f64 / 1e3)
    } else {
        format!("{n} B")
    }
}

fn fmt_age(ms: i64) -> String {
    let m = (ms as f64 / 60_000.0 + 0.5).floor() as i64; // JS Math.round
    if m < 1 {
        "just now".into()
    } else if m < 60 {
        format!("{m}m ago")
    } else {
        let h = (m as f64 / 60.0 + 0.5).floor() as i64;
        if h < 48 {
            format!("{h}h ago")
        } else {
            format!("{}d ago", (h as f64 / 24.0 + 0.5).floor() as i64)
        }
    }
}

/// Case-folded comparison, lowercase before uppercase on ties (stand-in for `Intl.Collator`).
fn collate(a: &str, b: &str) -> Ordering {
    let key = |s: &str| s.to_lowercase();
    key(a).cmp(&key(b)).then_with(|| b.cmp(a))
}

/// `path.resolve(base, p)`: absolute result with `.` and `..` folded lexically.
fn resolve(base: &Path, p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in base.join(p).components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// `path.relative(root, p)` is `''` or neither starts with `..` nor is absolute.
fn within(root: &Path, p: &Path) -> bool {
    let key = |p: &Path| PathBuf::from(synthesis_platform::normalize_for_match(p));
    key(p).starts_with(key(root))
}

/// Absolute path for a typed path (`/` or `\` separated, relative to the first root or
/// absolute) when it lies inside one of the context folders with links resolved, else `None`.
pub fn resolve_in_scope(typed: &str, roots: &[PathBuf], fs: &dyn CompletionFs) -> Option<PathBuf> {
    let abs = resolve(&roots[0], Path::new(&typed.replace('\\', "/")));
    let rp = fs.realpath(&abs)?;
    roots.iter().find_map(|root| {
        let rr = fs.realpath(root)?;
        (within(&resolve(&roots[0], root), &abs) && within(&rr, &rp)).then(|| abs.clone())
    })
}

fn path_item(rel: String, abs: PathBuf, is_dir: bool, order: usize) -> Item {
    Item {
        score: 1.0,
        order,
        insert: format!("@{rel}{}", if is_dir { "" } else { " " }),
        label: format!("@{rel}"),
        more: false,
        detail: Detail::Path { abs, is_dir },
    }
}

/// `@` suggestions: the query is split at its last `/` into a folder and a name part; the
/// folder's children whose names start with the name part are listed.
fn file_items(
    query: &str,
    roots: &[PathBuf],
    agents: &[AgentRef],
    fs: &dyn CompletionFs,
) -> (Vec<Item>, bool) {
    let q = query.replace('\\', "/");
    let cut = q.rfind('/').map_or(0, |i| i + 1);
    let (dir_part, lower) = (&q[..cut], q[cut..].to_lowercase());
    let starts = |name: &str| name.to_lowercase().starts_with(&lower);
    let mut items = Vec::new();
    let mut o = 0;
    if dir_part.is_empty() {
        for a in agents {
            let (label, detail) = if a.start_new {
                // definitions are offered once the person typed `agent:`
                (format!("agent:{}", a.id), "start a new agent".to_string())
            } else {
                let queued = if a.queued > 0 {
                    format!(" · {} queued", a.queued)
                } else {
                    String::new()
                };
                (
                    a.id.clone(),
                    format!(
                        "{} · {}{queued} · agent",
                        a.kind.as_deref().unwrap_or("agent"),
                        a.status.as_deref().unwrap_or("idle")
                    ),
                )
            };
            if label.to_lowercase().starts_with(&lower)
                && (!a.start_new || lower.starts_with("agent:"))
            {
                items.push(Item {
                    score: 2.0,
                    order: o,
                    label: format!("@{label}"),
                    insert: format!("@{label} "),
                    more: false,
                    detail: Detail::Fixed(detail),
                });
                o += 1;
            }
        }
    }
    let abs = resolve_in_scope(if dir_part.is_empty() { "." } else { dir_part }, roots, fs);
    let mut capped = false;
    if let Some((abs, mut kids)) = abs.and_then(|a| fs.read_dir(&a).map(|k| (a, k))) {
        kids.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| collate(&a.name, &b.name))
        });
        capped = kids.len() > MAX_CHILDREN;
        kids.truncate(MAX_CHILDREN);
        for k in kids {
            if k.name.starts_with('.') && !lower.starts_with('.') {
                continue;
            }
            if starts(&k.name) {
                let rel = format!("{dir_part}{}{}", k.name, if k.is_dir { "/" } else { "" });
                items.push(path_item(rel, abs.join(&k.name), k.is_dir, o));
                o += 1;
            }
        }
    }
    if dir_part.is_empty() {
        for d in &roots[1..] {
            let dir = resolve(&roots[0], d);
            let shown = if cfg!(windows) {
                dir.to_string_lossy().replace('\\', "/")
            } else {
                dir.to_string_lossy().into_owned()
            };
            let base = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if starts(&base) {
                items.push(path_item(
                    format!("{}/", shown.trim_end_matches('/')),
                    dir,
                    true,
                    o,
                ));
                o += 1;
            }
        }
    }
    (items, capped)
}

/// Fuzzy score: all query chars in order; earlier, contiguous and basename matches score
/// higher. `None` when it does not match.
pub fn score(query: &str, target: &str) -> Option<f64> {
    if query.is_empty() {
        return Some(0.0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = target.to_lowercase().chars().collect();
    let len = t.len() as f64;
    let from = t.len().saturating_sub(2);
    let base_start = t
        .get(..=from.min(t.len().saturating_sub(1)))
        .and_then(|h| h.iter().rposition(|&c| c == '/'))
        .map_or(0, |i| i + 1);
    if t[base_start..].starts_with(&q) {
        return Some(1000.0 - target.chars().count() as f64);
    }
    if let Some(i) = t.windows(q.len()).position(|w| w == q.as_slice()) {
        return Some(800.0 - i as f64 - len / 10.0);
    }
    let (mut ti, mut s, mut run) = (0usize, 0.0, 0.0);
    for &ch in &q {
        let found = ti + t.get(ti..)?.iter().position(|&c| c == ch)?;
        run = if found == ti { run + 1.0 } else { 0.0 };
        s += 10.0 + run * 5.0 - ((found - ti) as f64).min(9.0);
        ti = found + 1;
    }
    Some(s - len / 10.0)
}

/// The token being completed at `cur`, if any.
pub fn token_at(chars: &[char], cur: usize) -> Option<Token> {
    let mut i = cur;
    while i > 0 && !is_js_space(chars[i - 1]) {
        i -= 1;
    }
    let word: String = chars[i..cur].iter().collect();
    if let Some(q) = word.strip_prefix('@') {
        return Some(Token {
            kind: TokenKind::File,
            start: i,
            end: cur,
            query: q.to_string(),
        });
    }
    // "/" completion only for the command word at the very start
    if i == 0 && word.starts_with('/') && !chars[cur..].contains(&' ') && !chars.contains(&'\n') {
        return Some(Token {
            kind: TokenKind::Command,
            start: 0,
            end: cur,
            query: word[1..].to_string(),
        });
    }
    None
}

/// Suggestions for the current editor state.
pub fn suggest(editor: &LineEditor, opts: &Options, fs: &dyn CompletionFs) -> Option<Suggestions> {
    let token = token_at(&editor.chars, editor.cur)?;
    let mut capped = false;
    let mut items = Vec::new();
    if token.kind == TokenKind::Command {
        let mut seen = std::collections::HashSet::new();
        for c in opts.commands {
            if !seen.insert(c.name.as_str()) {
                continue;
            }
            let best = std::iter::once(&c.name)
                .chain(&c.aliases)
                .filter_map(|n| score(&token.query, n))
                .fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))));
            let Some(best) = best else { continue };
            let detail: Vec<&str> = [c.argument_hint.as_str(), c.description.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            items.push(Item {
                score: best + if c.local { 50.0 } else { 0.0 },
                order: 0,
                label: format!("/{}", c.name),
                insert: format!("/{} ", c.name),
                more: false,
                detail: Detail::Fixed(detail.join(" · ")),
            });
        }
    } else {
        let roots: Vec<PathBuf> = std::iter::once(opts.cwd.to_path_buf())
            .chain(opts.dirs.iter().cloned())
            .collect();
        // Agents only at the start of the message: a mention further in is always a file.
        let agents = if token.start == 0 { opts.agents } else { &[] };
        (items, capped) = file_items(&token.query, &roots, agents, fs);
    }
    items.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.order.cmp(&b.order))
            .then_with(|| collate(&a.label, &b.label))
    });
    items.truncate(opts.limit);
    // the listing was cut at MAX_CHILDREN
    if let Some(last) = items
        .last_mut()
        .filter(|l| capped && matches!(l.detail, Detail::Path { .. }))
    {
        last.more = true;
    }
    (!items.is_empty()).then_some(Suggestions { token, items })
}

/// Replace the token with the item's text and put the cursor after it.
pub fn apply(editor: &mut LineEditor, token: &Token, item: &Item) {
    let ins: Vec<char> = item.insert.chars().collect();
    let n = ins.len();
    editor.chars.splice(token.start..token.end, ins);
    editor.cur = token.start + n;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for f in files {
            let p = root.path().join(f);
            if f.ends_with('/') {
                fs::create_dir_all(p).unwrap();
            } else {
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, "x").unwrap();
            }
        }
        root
    }

    /// Canonical path (macOS /var is a link to /private/var).
    fn real(d: &tempfile::TempDir) -> PathBuf {
        dunce::canonicalize(d.path()).unwrap()
    }

    fn items(q: &str, o: &Options) -> Vec<String> {
        let mut e = LineEditor::new();
        e.insert(&format!("@{q}"));
        suggest(&e, o, &StdFs).map_or(vec![], |s| s.items.into_iter().map(|i| i.label).collect())
    }

    fn labels(q: &str, cwd: &Path) -> Vec<String> {
        items(q, &Options::new(cwd))
    }

    fn fwd(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    // --- translated from Node test/complete.test.js ---

    #[test]
    fn slash_commands_and_at_files() {
        let cmds = [
            Command {
                name: "review".into(),
                description: "Review a PR".into(),
                ..Default::default()
            },
            Command {
                name: "model".into(),
                ..Default::default()
            },
        ];
        let cwd = tree(&["agentview.js", "other.js"]);
        let mut o = Options::new(cwd.path());
        o.commands = &cmds;
        let mut e = LineEditor::new();
        e.insert("/rev");
        let s = suggest(&e, &o, &StdFs).unwrap();
        assert_eq!(s.items[0].label, "/review");
        assert_eq!(s.items[0].detail(&StdFs, 0), "Review a PR");
        apply(&mut e, &s.token, &s.items[0]);
        assert_eq!(e.text(), "/review ");
        let mut f = LineEditor::new();
        f.insert("look at @agentvi");
        let s = suggest(&f, &Options::new(cwd.path()), &StdFs).unwrap();
        assert_eq!(s.items[0].label, "@agentview.js");
        let mut plain = LineEditor::new();
        plain.insert("plain text");
        assert!(suggest(&plain, &Options::new(cwd.path()), &StdFs).is_none());
    }

    #[test]
    fn sub_agent_names_come_before_files() {
        let cwd = tree(&["worker.txt", "other.txt"]);
        let agents = [AgentRef {
            id: "worker".into(),
            kind: Some("developer".into()),
            status: Some("running".into()),
            ..AgentRef::default()
        }];
        let mut o = Options::new(cwd.path());
        o.agents = &agents;
        let mut e = LineEditor::new();
        e.insert("@wor");
        let s = suggest(&e, &o, &StdFs).unwrap();
        assert_eq!(
            s.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["@worker", "@worker.txt"]
        );
        assert!(s.items[0].detail(&StdFs, 0).contains("developer · running"));
        // mid-text the same token is only a file
        let mut mid = LineEditor::new();
        mid.insert("look at @wor");
        let s = suggest(&mid, &o, &StdFs).unwrap();
        assert_eq!(
            s.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["@worker.txt"]
        );
    }

    #[test]
    fn definitions_are_offered_after_agent_colon() {
        let cwd = tree(&["other.txt"]);
        let agents = [
            AgentRef {
                id: "worker".into(),
                ..AgentRef::default()
            },
            AgentRef {
                id: "reviewer".into(),
                start_new: true,
                ..AgentRef::default()
            },
        ];
        let mut o = Options::new(cwd.path());
        o.agents = &agents;
        let labels = |text: &str| {
            let mut e = LineEditor::new();
            e.insert(text);
            suggest(&e, &o, &StdFs)
                .map(|s| s.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(labels("@agent:rev"), ["@agent:reviewer"]);
        assert_eq!(labels("@w"), ["@worker"]);
        assert!(
            labels("@rev").is_empty(),
            "definitions need the agent: prefix"
        );
    }

    // --- translated from Node test/complete-dir.test.js ---

    #[test]
    fn empty_query_lists_folders_first_then_files_hidden_after_dot() {
        let root = tree(&[
            "zdir/a.txt",
            "adir/b.txt",
            "b.txt",
            "A.md",
            ".hidden",
            ".github/x.yml",
        ]);
        let cwd = real(&root);
        assert_eq!(labels("", &cwd), ["@adir/", "@zdir/", "@A.md", "@b.txt"]);
        assert_eq!(labels("adir/", &cwd), ["@adir/b.txt"]);
        assert_eq!(labels(".", &cwd), ["@.github/", "@.hidden"]);
    }

    #[test]
    fn names_match_by_prefix_case_insensitively() {
        let root = tree(&[
            "src/a.js",
            "Scripts/b.js",
            "SECURITY.md",
            "tests/t.js",
            "readme.md",
        ]);
        let cwd = real(&root);
        assert_eq!(labels("s", &cwd), ["@Scripts/", "@src/", "@SECURITY.md"]);
        assert_eq!(labels("S", &cwd), ["@Scripts/", "@src/", "@SECURITY.md"]);
        assert_eq!(labels("sr", &cwd), ["@src/"]);
        assert!(labels("rdm", &cwd).is_empty(), "subsequences do not match");
        assert!(labels("est", &cwd).is_empty(), "neither do substrings");
    }

    #[test]
    fn drill_down_inserts_folder_without_space_file_with_one() {
        let root = tree(&["src/native/a.js", "src/nav.js"]);
        let cwd = real(&root);
        let mut e = LineEditor::new();
        e.insert("@src/na");
        let s = suggest(&e, &Options::new(&cwd), &StdFs).unwrap();
        let l: Vec<_> = s.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(l, ["@src/native/", "@src/nav.js"]);
        assert_eq!(s.items[0].insert, "@src/native/");
        assert_eq!(s.items[1].insert, "@src/nav.js ");
        assert_eq!(s.items[0].detail(&StdFs, 0), "folder");
        let now = StdFs.lstat(&cwd.join("src/nav.js")).unwrap().mtime_ms + 5 * 60_000;
        assert_eq!(s.items[1].detail(&StdFs, now), "1 B · 5m ago");
        assert_eq!(labels("src\\na", &cwd), ["@src/native/", "@src/nav.js"]);
    }

    #[test]
    fn file_next_to_a_huge_folder_is_listed_directly() {
        let root = tree(&["zzz.txt", "aaa/"]);
        let cwd = real(&root);
        for i in 0..4100 {
            fs::write(cwd.join("aaa").join(format!("f{i}")), "").unwrap();
        }
        assert_eq!(labels("zz", &cwd), ["@zzz.txt"]);
    }

    #[test]
    fn nothing_outside_the_context_folders_is_listed() {
        let outside = tree(&["o.txt", "sub/p.txt"]);
        let root = tree(&["proj/a.txt"]);
        let (outside, cwd) = (real(&outside), real(&root).join("proj"));
        for q in ["nope/x", "../", "sub/../../", "~/", "/etc/", "C:\\"] {
            assert!(labels(q, &cwd).is_empty(), "{q}");
        }
        let abs = format!("{}/", fwd(&outside));
        assert!(labels(&abs, &cwd).is_empty());
        assert_eq!(
            labels(&abs, &outside),
            [format!("@{abs}sub/"), format!("@{abs}o.txt")],
            "inside the context folder an absolute path is fine"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, cwd.join("escape")).unwrap();
            assert!(
                labels("escape/", &cwd).is_empty(),
                "a link that leaves the folder lists nothing"
            );
        }
    }

    #[test]
    fn additional_directories_are_offered_at_top_level_and_can_be_entered() {
        let extra = tree(&["lib/x.js", "y.txt"]);
        let root = tree(&["a.txt"]);
        let (extra, cwd) = (real(&extra), real(&root));
        let dirs = [extra.clone()];
        let mut o = Options::new(&cwd);
        o.dirs = &dirs;
        assert_eq!(
            items("", &o),
            ["@a.txt".to_string(), format!("@{}/", fwd(&extra))]
        );
        let inner = format!("{}/", fwd(&extra));
        assert_eq!(
            items(&inner, &o),
            [format!("@{inner}lib/"), format!("@{inner}y.txt")]
        );
    }

    #[test]
    fn big_folder_caps_at_2000_and_reports_more() {
        let root = tree(&[]);
        let cwd = real(&root);
        for i in 0..2500 {
            fs::write(cwd.join(format!("f{i:04}.txt")), "").unwrap();
        }
        let mut e = LineEditor::new();
        e.insert("@");
        assert_eq!(
            suggest(&e, &Options::new(&cwd), &StdFs)
                .unwrap()
                .items
                .len(),
            MAX_ITEMS
        );
        let mut o = Options::new(&cwd);
        o.limit = 5000;
        let all = suggest(&e, &o, &StdFs).unwrap();
        assert_eq!(all.items.len(), 2000);
        assert!(
            all.items[1999]
                .detail(&StdFs, i64::MAX / 4)
                .ends_with("… more")
        );
        assert!(!all.items[0].detail(&StdFs, i64::MAX / 4).contains("more"));
    }

    #[test]
    fn scoring() {
        assert_eq!(score("", "x"), Some(0.0));
        assert!(score("rev", "review") > score("rv", "review"));
        assert_eq!(score("zz", "review"), None);
        assert_eq!(score("rev", "a/review"), Some(1000.0 - 8.0));
    }

    struct MemFs;
    impl CompletionFs for MemFs {
        fn realpath(&self, p: &Path) -> Option<PathBuf> {
            Some(p.to_path_buf())
        }
        fn read_dir(&self, _: &Path) -> Option<Vec<DirEntry>> {
            Some(vec![DirEntry {
                name: "virtual.txt".into(),
                is_dir: false,
            }])
        }
        fn lstat(&self, _: &Path) -> Option<FileMeta> {
            None
        }
    }

    #[test]
    fn filesystem_is_injectable() {
        let mut e = LineEditor::new();
        e.insert("@v");
        let s = suggest(&e, &Options::new(Path::new("/nowhere")), &MemFs).unwrap();
        assert_eq!(s.items[0].label, "@virtual.txt");
        assert_eq!(s.items[0].detail(&MemFs, 0), "");
    }
}
