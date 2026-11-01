//! ctrl+click on a url or a path in the chat (port of `links.js`). Mouse reporting is on, so the
//! terminal does not open links itself: the click cell is looked up in the text of the last
//! drawn frame and the target is handed to the platform opener.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use synthesis_config::Env;
use synthesis_tui_base::Platform;
use synthesis_tui_base::keys::{Key, Mods};
use synthesis_tui_base::selection::{SelRow, slice_cells};
use synthesis_tui_base::text::{char_width, csi_len, strip_ansi, width};

fn forbidden_url(c: char) -> bool {
    c.is_whitespace() || "<>\"'`()[]│┃".contains(c)
}

fn forbidden_path(c: char) -> bool {
    c.is_whitespace() || "<>\"'`|*?│┃".contains(c)
}

/// What the pointer is on, before the file system is asked about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Url(String),
    /// Text that looks like a path (`file.rs:12`, `src/a.rs`, `~/x`, `C:\x`, `file:///x`).
    Path(String),
}

/// Terminal cell where every char of `text` starts, plus the total width.
fn cells(chars: &[char]) -> (Vec<usize>, usize) {
    let mut at = Vec::with_capacity(chars.len());
    let mut cell = 0;
    for &c in chars {
        at.push(cell);
        cell += char_width(c);
    }
    (at, cell)
}

fn hit(chars: &[char], from: usize, to: usize, col: usize) -> Option<String> {
    let (at, total) = cells(chars);
    let end = at.get(to).copied().unwrap_or(total);
    (col >= at[from] && col < end).then(|| chars[from..to].iter().collect())
}

fn starts_with(c: &[char], i: usize, s: &str) -> bool {
    s.chars().enumerate().all(|(k, x)| c.get(i + k) == Some(&x))
}

/// `(from, to)` char ranges of the http(s) urls in a line.
fn url_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut v = vec![];
    let mut i = 0;
    while i < chars.len() {
        if starts_with(chars, i, "http://") || starts_with(chars, i, "https://") {
            let mut to = i;
            while to < chars.len() && !forbidden_url(chars[to]) {
                to += 1;
            }
            while to > i && ".,;:!?".contains(chars[to - 1]) {
                to -= 1;
            }
            v.push((i, to));
            i = to.max(i + 1);
        } else {
            i += 1;
        }
    }
    v
}

/// The http(s) url under terminal cell `col` (0-based) of a drawn line.
pub fn url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = strip_ansi(line).chars().collect();
    url_spans(&chars)
        .into_iter()
        .find_map(|(a, b)| hit(&chars, a, b, col))
}

/// Starts that are paths by themselves: `/a/b`, `~/x`, `C:\x`, `\\server\share`, `file://`.
fn abs_start(c: &[char], i: usize) -> bool {
    let sep = |k: usize| matches!(c.get(k), Some('/' | '\\'));
    let body = |k: usize| c.get(k).is_some_and(|x| !forbidden_path(*x));
    if starts_with(c, i, "file://") && body(i + 7) {
        return true;
    }
    if c[i] == '\\' && c.get(i + 1) == Some(&'\\') && body(i + 2) {
        return true;
    }
    if c[i].is_ascii_alphabetic() && c.get(i + 1) == Some(&':') && sep(i + 2) {
        return true;
    }
    if c[i] == '~' && sep(i + 1) && body(i + 2) {
        return true;
    }
    // "/dir/..." needs two segments so that "and/or" and a lone "/" are not paths
    if c[i] == '/' {
        let mut k = i + 1;
        while body(k) && c[k] != '/' {
            k += 1;
        }
        return k > i + 1 && c.get(k) == Some(&'/');
    }
    false
}

/// A relative path: has a separator (`src/a.rs`, `.\a`) or is a bare `name.ext`. Existence is
/// what decides later, so this only has to be cheap and not match plain words or numbers.
fn rel_ok(tok: &str) -> bool {
    let tok = tok.split('(').next().unwrap_or(tok);
    let Some(first) = tok.chars().next() else {
        return false;
    };
    if !(first.is_alphanumeric() || matches!(first, '.' | '_')) || tok.contains("://") {
        return false;
    }
    if tok.contains(['/', '\\']) {
        return tok.chars().any(char::is_alphanumeric);
    }
    let name = tok.split(':').next().unwrap_or(tok);
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        stem.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && ext.starts_with(|c: char| c.is_alphabetic())
            && ext.chars().all(char::is_alphanumeric)
    })
}

/// End of the path token starting at `i`: stops at `(`, but a `(12)` / `(12,3)` line suffix belongs
/// to it (it is ignored when the path is looked up).
fn path_end(c: &[char], i: usize) -> usize {
    let mut to = i;
    while to < c.len() && !forbidden_path(c[to]) && c[to] != '(' {
        to += 1;
    }
    while to > i && ".,;:!?)]".contains(c[to - 1]) {
        to -= 1;
    }
    if c.get(to) == Some(&'(') {
        let mut k = to + 1;
        while k < c.len() && (c[k].is_ascii_digit() || c[k] == ',') {
            k += 1;
        }
        if k > to + 1 && c.get(k) == Some(&')') {
            to = k + 1;
        }
    }
    to
}

/// `(from, to)` char ranges of the path-like tokens of a line.
fn path_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut v = vec![];
    let mut i = 0;
    while i < chars.len() {
        let boundary = i == 0 || chars[i - 1].is_whitespace() || "(\"'`=".contains(chars[i - 1]);
        let to = if boundary { path_end(chars, i) } else { i };
        if to > i && (abs_start(chars, i) || rel_ok(&chars[i..to].iter().collect::<String>())) {
            v.push((i, to));
            i = to;
        } else {
            i += 1;
        }
    }
    v
}

/// The path-like text under terminal cell `col`. Paths with spaces cannot be told from the words
/// around them and are not matched.
pub fn path_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = strip_ansi(line).chars().collect();
    path_spans(&chars)
        .into_iter()
        .find_map(|(a, b)| hit(&chars, a, b, col))
}

/// The link under `col` of a (logical) line: urls first, then paths.
pub fn link_at(line: &str, col: usize) -> Option<Link> {
    url_at(line, col)
        .map(Link::Url)
        .or_else(|| path_at(line, col).map(Link::Path))
}

/// What a click needs: whether this button + modifiers means "open". Terminals disagree on
/// ctrl+click: kitty / alacritty / Windows Terminal report left + ctrl; macOS terminals turn it
/// into a secondary click (button 2, ctrl bit usually missing); alt/option+click is the fallback.
/// Super/Cmd has no bit in SGR mouse reports, so terminals handle it themselves (OSC 8 links).
/// A right click opens too: it has no other meaning outside the prompt box (the caller keeps
/// that one for paste). Alt+click only counts when something openable is under it.
pub fn open_gesture(button: u32, mods: Mods) -> bool {
    match button {
        0 => mods.ctrl || mods.alt,
        2 => true,
        _ => false,
    }
}

/// The `SYNTHESIS_MOUSE_DEBUG=1` line: what the terminal really sent.
pub fn mouse_debug(k: &Key) -> Option<String> {
    Some(match k {
        Key::Mouse {
            x,
            y,
            button,
            action,
            mods,
        } => format!(
            "mouse {action:?} button={button} ctrl={} alt={} shift={} @{x},{y}",
            mods.ctrl, mods.alt, mods.shift
        ),
        Key::MouseDrag { x, y, button, mods } => format!(
            "motion button={button} ctrl={} alt={} shift={} @{x},{y}",
            mods.ctrl, mods.alt, mods.shift
        ),
        _ => return None,
    })
}

/// A drawn row that continues in the next one, as the renderer marks wraps.
pub fn join_wrapped(
    rows: &dyn Fn(i64) -> Option<SelRow>,
    y: i64,
    cx: i64,
) -> Option<(String, usize)> {
    const CHAIN: i64 = 16;
    let mut start = y;
    while y - start < CHAIN && start > 0 && rows(start - 1).is_some_and(|r| r.soft) {
        start -= 1;
    }
    let (mut text, mut col, mut k) = (String::new(), None, start);
    while let Some(r) = rows(k) {
        if k == y {
            col = (cx >= r.lead).then(|| width(&text) + (cx - r.lead) as usize);
        }
        text.push_str(&slice_cells(&r.text, r.lead, i64::MAX));
        if !r.soft || k - y >= CHAIN {
            break;
        }
        k += 1;
    }
    col.map(|c| (text, c))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `file:///a/b%20c` -> `/a/b c`, `file:///C:/x` -> `C:/x`. Remote hosts are refused.
fn file_url_path(rest: &str) -> Option<String> {
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let path = percent_decode(rest.strip_prefix('/').map(|_| rest)?)?;
    let drive = path.as_bytes();
    if drive.len() >= 3 && drive[2] == b':' && drive[1].is_ascii_alphabetic() {
        return Some(path[1..].to_string());
    }
    Some(path)
}

/// A path as the target platform writes it: `C:\x`, `C:/x` or `\\server\share`.
fn windows_style(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with(r"\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// `file://` URI for OSC 8 from the path text alone (so it is the same on every host): Windows
/// paths become `file:///C:/x` and `file://server/share/x`, a `\\?\` prefix is dropped, only
/// unsafe bytes are percent-encoded.
pub fn file_uri(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let raw = raw.strip_prefix(r"\\?\UNC\").map_or_else(
        || raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string(),
        |r| format!(r"\\{r}"),
    );
    let mut s = raw.clone();
    let mut host = String::new();
    if windows_style(&raw) {
        s = raw.replace('\\', "/");
        if let Some(unc) = s.strip_prefix("//") {
            let (h, rest) = unc.split_once('/').unwrap_or((unc, ""));
            host = h.to_string();
            s = format!("/{rest}");
        } else {
            s.insert(0, '/');
        }
    }
    let mut out = format!("file://{host}");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Terminals known to turn OSC 8 into links they open themselves (cmd/ctrl+click).
pub fn hyperlinks_enabled(env: &Env) -> bool {
    match env.get("SYNTHESIS_HYPERLINKS") {
        Some("0") => return false,
        Some("1") => return true,
        _ => {}
    }
    matches!(
        env.get("TERM_PROGRAM"),
        Some("iTerm.app" | "WezTerm" | "vscode" | "ghostty")
    ) || env.get_nonempty("WT_SESSION").is_some()
        || env.get_nonempty("KITTY_WINDOW_ID").is_some()
        || env
            .get("VTE_VERSION")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|v| v >= 5000)
}

/// `line` (already encoded: only SGR escapes) with OSC 8 around its urls and existing paths. Width
/// is untouched (OSC 8 has none) and the plain line, which the row cache compares, is not changed.
pub fn hyperlink_line(line: &str, base: Option<&Path>, home: Option<&Path>) -> String {
    let chars: Vec<char> = strip_ansi(line).chars().collect();
    let mut spans: Vec<(usize, usize, String)> = url_spans(&chars)
        .into_iter()
        .map(|(a, b)| (a, b, chars[a..b].iter().collect()))
        .collect();
    for (a, b) in path_spans(&chars) {
        let raw: String = chars[a..b].iter().collect();
        if spans.iter().any(|s| a < s.1 && s.0 < b) {
            continue;
        }
        if let Some(t) = existing_path(&raw, home, base) {
            spans.push((a, b, file_uri(&t.path)));
        }
    }
    spans.retain(|s| s.2.chars().all(|c| c > ' ' && c != '\x7f'));
    if spans.is_empty() {
        return line.to_string();
    }
    spans.sort();
    let mut out = String::with_capacity(line.len() + spans.len() * 24);
    let (mut k, mut next, mut open) = (0, 0, false);
    let mut i = 0;
    while i < line.len() {
        if open && spans[next - 1].1 == k {
            out.push_str("\x1b]8;;\x1b\\");
            open = false;
        }
        if let Some(n) = csi_len(line, i) {
            out.push_str(&line[i..i + n]);
            i += n;
            continue;
        }
        if !open && next < spans.len() && spans[next].0 == k {
            out.push_str(&format!("\x1b]8;;{}\x1b\\", spans[next].2));
            open = true;
            next += 1;
        }
        let c = line[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
        k += 1;
    }
    if open {
        out.push_str("\x1b]8;;\x1b\\");
    }
    out
}

/// Windows `canonicalize` returns `\\?\C:\x`; Explorer and OSC 8 want the plain form.
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{r}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(r) => PathBuf::from(r),
        None => p,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathTarget {
    pub path: PathBuf,
    pub dir: bool,
}

/// What a path from the screen points at right now. `file.rs:12:3` / `file.rs(12)` lose their line
/// numbers, `~` is `home`, relative paths are taken from `base` (the session cwd), `file://` urls
/// are decoded. The result is canonical (symlinks followed), `None` when it does not exist.
pub fn existing_path(raw: &str, home: Option<&Path>, base: Option<&Path>) -> Option<PathTarget> {
    if raw.starts_with('-') {
        return None;
    }
    let mut p = match raw.strip_prefix("file://") {
        Some(rest) => file_url_path(rest)?,
        None => raw.to_string(),
    };
    if let Some(rest) = p.strip_prefix('~')
        && rest.starts_with(['/', '\\'])
    {
        p = format!("{}{rest}", home?.display());
    }
    let mut cut = p.len();
    // "name(12)" / "name(12,3)"
    if p.ends_with(')')
        && let Some(open) = p.rfind('(')
        && p[open + 1..cut - 1]
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b',')
        && cut - 1 > open + 1
    {
        cut = open;
    }
    // "name.ext:12" / "name.ext:12:3"
    for _ in 0..2 {
        let (head, tail) = p[..cut].rsplit_once(':').unwrap_or((&p[..cut], "x"));
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && head.contains('.') {
            cut = head.len();
        }
    }
    let mut path = PathBuf::from(&p[..cut]);
    if !path.is_absolute() {
        path = base?.join(path);
    }
    let path = plain(std::fs::canonicalize(&path).ok()?);
    let dir = std::fs::metadata(&path).ok()?.is_dir();
    Some(PathTarget { path, dir })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Url(String),
    Folder(PathBuf),
    /// Shown selected in its folder; never run or opened with its program.
    RevealFile(PathBuf),
}

impl OpenTarget {
    pub fn from_path(t: &PathTarget) -> Self {
        if t.dir {
            Self::Folder(t.path.clone())
        } else {
            Self::RevealFile(t.path.clone())
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Url(u) => u.clone(),
            Self::Folder(p) | Self::RevealFile(p) => p.display().to_string(),
        }
    }
}

pub trait Opener {
    /// Starts the opener; a failure to start is an error (never swallowed).
    fn open(&self, target: &OpenTarget) -> io::Result<()>;
}

/// Extensions the desktop would run or install when opened.
fn launchable(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase());
    matches!(
        ext.as_deref(),
        Some("app" | "command" | "workflow" | "terminal" | "tool" | "pkg" | "desktop")
    )
}

/// `(program, args)` per platform. No shell is involved, so an address can never run as a
/// command. Windows tools are spawned by absolute path under `SystemRoot`.
pub fn command_for(
    platform: Platform,
    env: &Env,
    target: &OpenTarget,
) -> io::Result<(PathBuf, Vec<String>)> {
    if let OpenTarget::Url(u) = target {
        let ok = ["http://", "https://"].iter().any(|p| u.starts_with(p))
            && !u.chars().any(|c| c.is_whitespace() || c.is_control());
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only http and https addresses are opened",
            ));
        }
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();
    if let OpenTarget::Folder(p) | OpenTarget::RevealFile(p) = target {
        // absolute by the rules of the platform being targeted, not of this host
        let t = s(p);
        let abs = match platform {
            Platform::Windows => windows_style(&t) && !t.starts_with(r"\\?\"),
            _ => t.starts_with('/'),
        };
        if !abs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only absolute paths are revealed",
            ));
        }
    }
    let root = PathBuf::from(env.get_nonempty("SystemRoot").unwrap_or(r"C:\Windows"));
    Ok(match (platform, target) {
        (Platform::Windows, OpenTarget::Url(u)) => (
            root.join("System32").join("rundll32.exe"),
            vec!["url.dll,FileProtocolHandler".into(), u.clone()],
        ),
        // Never ShellExecute a path: `/select,` only shows it in its folder.
        (Platform::Windows, OpenTarget::Folder(p) | OpenTarget::RevealFile(p)) => {
            (root.join("explorer.exe"), vec![format!("/select,{}", s(p))])
        }
        (Platform::MacOs, OpenTarget::Url(u)) => ("/usr/bin/open".into(), vec![u.clone()]),
        // `open <dir>` would launch a directory named x.app: always reveal.
        (Platform::MacOs, OpenTarget::Folder(p) | OpenTarget::RevealFile(p)) => {
            ("/usr/bin/open".into(), vec!["-R".into(), s(p)])
        }
        (_, OpenTarget::Url(u)) => ("xdg-open".into(), vec![u.clone()]),
        (_, OpenTarget::Folder(p)) if !launchable(p) => ("xdg-open".into(), vec![s(p)]),
        (_, OpenTarget::Folder(p) | OpenTarget::RevealFile(p)) => (
            "xdg-open".into(),
            vec![s(p.parent().unwrap_or(Path::new("/")))],
        ),
    })
}

pub struct SystemOpener {
    pub platform: Platform,
    pub env: Env,
}

impl Opener for SystemOpener {
    fn open(&self, target: &OpenTarget) -> io::Result<()> {
        let (program, args) = command_for(self.platform, &self.env, target)?;
        spawn_detached(&program, &args)
    }
}

/// Spawn errors carry the program path. The child is reaped on a thread so it does not stay a
/// zombie; its exit status is not tracked (explorer.exe exits 1 on success).
pub fn spawn_detached(program: &Path, args: &[String]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", program.display())))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_tui_base::text::strip_osc8;

    #[test]
    fn finds_url_under_cell_and_trims_punctuation() {
        let l = "see https://example.com/a?b=1. now";
        assert_eq!(url_at(l, 4).as_deref(), Some("https://example.com/a?b=1"));
        assert_eq!(
            url_at(l, 29).as_deref(),
            None,
            "the full stop is not the link"
        );
        assert_eq!(url_at(l, 0), None);
        // wide characters before the link shift cells, not chars
        assert_eq!(url_at("日本 http://a.b", 6).as_deref(), Some("http://a.b"));
        assert_eq!(
            url_at("\x1b[31mhttp://x.y\x1b[0m", 1).as_deref(),
            Some("http://x.y")
        );
    }

    #[test]
    fn finds_paths() {
        assert_eq!(
            path_at("wrote /tmp/a/b.txt:12 ok", 8).as_deref(),
            Some("/tmp/a/b.txt:12")
        );
        // a relative token is only a candidate: existence decides
        assert_eq!(path_at("and/or", 1).as_deref(), Some("and/or"));
        assert_eq!(path_at("in 3.2s", 4), None);
        assert_eq!(path_at("see e.g. this", 1), None);
        assert_eq!(path_at("at (~/proj/x)", 5).as_deref(), Some("~/proj/x"));
        assert_eq!(
            path_at(r"open C:\Users\me\f.txt now", 8).as_deref(),
            Some(r"C:\Users\me\f.txt")
        );
        assert_eq!(path_at("/only", 1), None);
        // diff headers, tool rows, line suffixes, UNC, file urls
        assert_eq!(
            path_at("Update(src/lib.rs)", 10).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(path_at("Update(src/lib.rs)", 2), None);
        assert_eq!(
            path_at("at src/lib.rs(12,3) now", 16).as_deref(),
            Some("src/lib.rs(12,3)")
        );
        assert_eq!(
            path_at("open Cargo.toml.", 6).as_deref(),
            Some("Cargo.toml")
        );
        assert_eq!(
            path_at(r"\\srv\share\a b", 3).as_deref(),
            Some(r"\\srv\share\a")
        );
        assert_eq!(
            path_at("file:///C:/a%20b/c.txt", 9).as_deref(),
            Some("file:///C:/a%20b/c.txt")
        );
        assert_eq!(path_at("https://x.y/a/b", 9), None, "urls are not paths");
        // wide chars before the token shift cells
        assert_eq!(path_at("日本 ./a/b.rs", 6).as_deref(), Some("./a/b.rs"));
        assert_eq!(path_at("日本 ./a/b.rs", 3), None);
    }

    #[test]
    fn link_at_prefers_urls_and_wraps_join() {
        assert_eq!(
            link_at("go https://a.b/c", 5),
            Some(Link::Url("https://a.b/c".into()))
        );
        // a url wrapped over two rows is found from either row
        let rows = |y: i64| match y {
            0 => Some(SelRow {
                text: "  see https://exam".into(),
                soft: true,
                lead: 2,
            }),
            1 => Some(SelRow {
                text: "  ple.com/x tail".into(),
                soft: false,
                lead: 2,
            }),
            _ => None,
        };
        let (text, col) = join_wrapped(&rows, 1, 4).unwrap();
        assert_eq!(text, "see https://example.com/x tail");
        assert_eq!(
            link_at(&text, col),
            Some(Link::Url("https://example.com/x".into()))
        );
        let (text, col) = join_wrapped(&rows, 0, 6).unwrap();
        assert_eq!(
            link_at(&text, col),
            Some(Link::Url("https://example.com/x".into()))
        );
        assert_eq!(join_wrapped(&rows, 1, 0), None, "the gutter is not text");
    }

    #[test]
    fn existing_path_rules() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path().to_path_buf();
        let f = d.join("probe.rs");
        std::fs::write(&f, "x").unwrap();
        let f = plain(std::fs::canonicalize(&f).unwrap()); // macOS /var -> /private/var, Windows \\?\
        let d = plain(std::fs::canonicalize(&d).unwrap());
        let ep = |raw: &str| existing_path(raw, Some(&d), Some(&d));
        for suffix in [":12:3", ":12", "(12)", "(12,3)", ""] {
            let t = ep(&format!("{}{suffix}", f.display())).unwrap();
            assert!(!t.dir && t.path == f, "{suffix}");
        }
        assert!(ep(&d.display().to_string()).unwrap().dir);
        assert_eq!(ep("probe.rs:3").unwrap().path, f, "relative to the base");
        assert_eq!(ep("./probe.rs").unwrap().path, f);
        assert_eq!(ep("~/probe.rs").unwrap().path, f, "~ is home");
        assert_eq!(ep("nope/p.rs"), None);
        assert_eq!(ep("-probe.rs"), None);
        assert_eq!(existing_path("relative/p.rs", None, None), None);
        assert_eq!(existing_path("~/x/y", None, None), None, "no home known");
        let url = file_uri(&f);
        assert_eq!(ep(&url).unwrap().path, f, "file url, percent-decoded");
        assert_eq!(ep("file://elsewhere/etc/passwd"), None, "remote host");
        #[cfg(unix)]
        {
            let link = d.join("lnk");
            std::os::unix::fs::symlink(&f, &link).unwrap();
            assert_eq!(ep("lnk").unwrap().path, f, "symlinks resolve");
            std::os::unix::fs::symlink(d.join("gone"), d.join("dangling")).unwrap();
            assert_eq!(ep("dangling"), None);
        }
    }

    #[test]
    fn opener_commands_per_platform() {
        let env = Env::from_pairs([("SystemRoot", r"D:\Win")]);
        let u = OpenTarget::Url("https://x.y/z".into());
        let (p, a) = command_for(Platform::Windows, &env, &u).unwrap();
        assert_eq!(
            p,
            PathBuf::from(r"D:\Win")
                .join("System32")
                .join("rundll32.exe")
        );
        assert_eq!(a, ["url.dll,FileProtocolHandler", "https://x.y/z"]);
        let f = OpenTarget::RevealFile("/a/b.txt".into());
        assert_eq!(
            command_for(Platform::MacOs, &env, &f).unwrap().1,
            ["-R", "/a/b.txt"]
        );
        assert_eq!(command_for(Platform::Linux, &env, &f).unwrap().1, ["/a"]);
        // absolute is judged by the target platform: a POSIX path is no Windows path
        assert!(command_for(Platform::Windows, &env, &f).is_err());
        let wf = OpenTarget::RevealFile(r"C:\a\b.txt".into());
        assert!(command_for(Platform::Linux, &env, &wf).is_err());
        let (p, a) = command_for(Platform::Windows, &env, &wf).unwrap();
        assert_eq!(p, PathBuf::from(r"D:\Win").join("explorer.exe"));
        assert_eq!(a, [r"/select,C:\a\b.txt"]);
        let unc = OpenTarget::Folder(r"\\srv\share\d".into());
        assert_eq!(
            command_for(Platform::Windows, &env, &unc).unwrap().1,
            [r"/select,\\srv\share\d"]
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:x",
            "https://a b",
            "https://a\x1b]52;c;x",
        ] {
            let bad = OpenTarget::Url(bad.into());
            assert!(command_for(Platform::Linux, &env, &bad).is_err());
        }
        for bad in ["mailto:a@b.c", "data:text/html,x", "ftp://x", "-h"] {
            let bad = OpenTarget::Url(bad.into());
            assert!(command_for(Platform::MacOs, &env, &bad).is_err());
        }
        // argv only, spaces / unicode kept whole; a "-" start or a relative path is refused
        let sp = OpenTarget::RevealFile("/a b/日本 語.txt".into());
        assert_eq!(
            command_for(Platform::MacOs, &env, &sp).unwrap().1,
            ["-R", "/a b/日本 語.txt"]
        );
        let wsp = OpenTarget::RevealFile(r"C:\a b\日本 語.txt".into());
        assert_eq!(
            command_for(Platform::Windows, &env, &wsp).unwrap().1,
            [r"/select,C:\a b\日本 語.txt"]
        );
        for bad in ["-rf", "rel/x"] {
            let t = OpenTarget::RevealFile(bad.into());
            for p in [Platform::MacOs, Platform::Windows, Platform::Linux] {
                assert!(command_for(p, &env, &t).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn paths_are_revealed_never_launched() {
        let env = Env::from_pairs([("SystemRoot", r"D:\Win")]);
        for name in [
            "/a/x.app",
            "/a/y.COMMAND",
            "/a/d",
            "/a/z.pkg",
            "/a/w.workflow",
        ] {
            let d = OpenTarget::Folder(name.into());
            let (p, a) = command_for(Platform::MacOs, &env, &d).unwrap();
            assert_eq!(
                (p, a),
                (
                    "/usr/bin/open".into(),
                    vec!["-R".to_string(), name.to_string()]
                )
            );
            let win = format!("C:{}", name.replace('/', "\\"));
            let d = OpenTarget::Folder(win.clone().into());
            let (_, a) = command_for(Platform::Windows, &env, &d).unwrap();
            assert_eq!(a, [format!("/select,{win}")]);
        }
        let app = OpenTarget::Folder("/a/x.app".into());
        assert_eq!(command_for(Platform::Linux, &env, &app).unwrap().1, ["/a"]);
        let t = OpenTarget::Folder("/a/t.Desktop".into());
        assert_eq!(command_for(Platform::Linux, &env, &t).unwrap().1, ["/a"]);
        let plain = OpenTarget::Folder("/a/d".into());
        assert_eq!(
            command_for(Platform::Linux, &env, &plain).unwrap().1,
            ["/a/d"]
        );
    }

    #[test]
    fn spawn_failure_is_reported() {
        let e = spawn_detached(Path::new("/nonexistent/synthesis-opener"), &[]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("synthesis-opener"), "{e}");
    }

    #[test]
    fn gesture_matrix() {
        let m = |ctrl, alt, shift| Mods { ctrl, alt, shift };
        for (button, mods, want) in [
            (0, m(true, false, false), true), // kitty / alacritty / Windows Terminal
            (0, m(false, true, false), true), // alt / option
            (0, m(true, true, false), true),
            (0, m(false, false, false), false), // plain click is selection
            (0, m(false, false, true), false),
            (2, m(false, false, false), true), // macOS: ctrl+click arrives as a right click
            (2, m(true, false, false), true),
            (1, m(true, false, false), false),
        ] {
            assert_eq!(open_gesture(button, mods), want, "{button} {mods:?}");
        }
    }

    #[test]
    fn mouse_debug_shows_button_and_modifiers() {
        use synthesis_tui_base::keys::MouseAction;
        let k = Key::Mouse {
            x: 5,
            y: 6,
            button: 2,
            action: MouseAction::Down,
            mods: m_ctrl(),
        };
        assert_eq!(
            mouse_debug(&k).unwrap(),
            "mouse Down button=2 ctrl=true alt=false shift=false @5,6"
        );
        assert_eq!(mouse_debug(&Key::Text("x".into())), None);
    }

    fn m_ctrl() -> Mods {
        Mods {
            ctrl: true,
            ..Mods::default()
        }
    }

    #[test]
    fn file_uris_are_percent_encoded_per_platform() {
        assert_eq!(
            file_uri(Path::new("/a b/é#.txt")),
            "file:///a%20b/%C3%A9%23.txt"
        );
        assert_eq!(
            file_uri(Path::new(r"C:\Users\me\a b.txt")),
            "file:///C:/Users/me/a%20b.txt"
        );
        assert_eq!(
            file_uri(Path::new(r"\\?\C:\Users\a b\x.txt")),
            "file:///C:/Users/a%20b/x.txt",
            "verbatim prefix dropped"
        );
        assert_eq!(
            file_uri(Path::new(r"\\?\UNC\srv\share\x.txt")),
            "file://srv/share/x.txt"
        );
        assert_eq!(
            file_uri(Path::new(r"\\srv\share\x.txt")),
            "file://srv/share/x.txt"
        );
    }

    #[test]
    fn hyperlink_support_is_detected_from_the_environment() {
        let on = |p: &[(&str, &str)]| hyperlinks_enabled(&Env::from_pairs(p.iter().copied()));
        assert!(!on(&[]));
        assert!(!on(&[("TERM_PROGRAM", "Apple_Terminal")]));
        for tp in ["iTerm.app", "WezTerm", "vscode", "ghostty"] {
            assert!(on(&[("TERM_PROGRAM", tp)]), "{tp}");
        }
        assert!(on(&[("WT_SESSION", "x")]) && on(&[("KITTY_WINDOW_ID", "1")]));
        assert!(on(&[("VTE_VERSION", "5000")]) && !on(&[("VTE_VERSION", "4999")]));
        assert!(on(&[("SYNTHESIS_HYPERLINKS", "1")]));
        assert!(!on(&[
            ("SYNTHESIS_HYPERLINKS", "0"),
            ("WT_SESSION", "x"),
            ("NO_COLOR", "1")
        ]));
        assert!(
            on(&[("NO_COLOR", "1"), ("WT_SESSION", "x")]),
            "NO_COLOR does not matter"
        );
    }

    #[test]
    fn osc8_wraps_links_without_changing_the_visible_text() {
        let td = tempfile::tempdir().unwrap();
        let f = plain(std::fs::canonicalize(td.path()).unwrap()).join("a b.rs");
        std::fs::write(&f, "x").unwrap();
        let base = f.parent().unwrap();
        let line = "\x1b[1mgo https://e.x/a?b=1. see \x1b[0m\x1b[4ma.rs:3\x1b[0m and /nope/x/y end";
        let out = hyperlink_line(line, Some(base), None);
        // nothing there for a.rs (the file is "a b.rs"), the url is wrapped, sgr is untouched
        assert_eq!(strip_osc8(&out), line);
        assert!(
            out.contains("\x1b]8;;https://e.x/a?b=1\x1b\\https://e.x/a?b=1\x1b]8;;\x1b\\."),
            "{out:?}"
        );
        assert!(!out.contains("file://"), "{out:?}");
        assert_eq!(out.matches("\x1b]8;;\x1b\\").count(), 1);
        std::fs::write(base.join("a.rs"), "x").unwrap();
        let out = hyperlink_line(line, Some(base), None);
        let uri = file_uri(&base.join("a.rs"));
        assert!(
            out.contains(&format!("\x1b]8;;{uri}\x1b\\a.rs:3\x1b]8;;\x1b\\")),
            "{out:?}"
        );
        assert_eq!(strip_osc8(&out), line);
        assert_eq!(width(&strip_osc8(&out)), width(&strip_ansi(line)));
        // a link at the very end of the line is closed
        let out = hyperlink_line("https://e.x/z", None, None);
        assert!(out.ends_with("\x1b]8;;\x1b\\"), "{out:?}");
        // lines without links are returned as they are
        assert_eq!(hyperlink_line("plain", None, None), "plain");
    }
}
