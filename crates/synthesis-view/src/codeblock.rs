//! Fenced blocks in a model message: split a text into prose and blocks, classify each block (a shell COMMAND,
//! CODE, a unified DIFF or plain TEXT), decide when a command may be offered to a terminal, and colour diff lines.
//! Pure and deterministic; the drawing and the terminal are the front end's. The model's text is untrusted: a
//! block is only ever classified here, never run. Running takes a real click through [`Gate`].

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use regex::Regex;

use crate::toolview::Tone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A shell command: Copy and Run in terminal.
    Command,
    Code,
    /// A unified diff: coloured, Copy.
    Diff,
    /// Output, prose, an agent's slash command: Copy only.
    Text,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Code => "code",
            Self::Diff => "diff",
            Self::Text => "text",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// First word of the info string, lowercase (empty when none).
    pub lang: String,
    pub body: String,
    /// The closing fence was seen (an open block is still streaming).
    pub closed: bool,
    pub kind: Kind,
    /// The body holds control, bidi or invisible characters: shown escaped, nothing is copied or run.
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Prose(String),
    Block(Block),
}

/// A bidi, zero-width, tag, variation or other format/control character (newline and tab excepted).
pub fn invisible(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    c.is_control()
        || matches!(c,
            '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}'
            | '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}'
            | '\u{ffa0}' | '\u{fff9}'..='\u{fffc}' | '\u{2800}' | '\u{1d173}'..='\u{1d17a}'
            | '\u{1bca0}'..='\u{1bca3}' | '\u{13430}'..='\u{13438}' | '\u{e0000}'..='\u{e0fff}')
}

/// `s` with every invisible character made visible as `\u{..}` (what a hidden block shows).
pub fn escape_invisible(s: &str) -> String {
    s.chars()
        .map(|c| {
            if invisible(c) {
                format!("\\u{{{:04x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A fence line: `(indent, marker char, marker length, info string)`.
fn fence_open(line: &str) -> Option<(usize, char, usize, &str)> {
    let t = line.trim_start_matches(' ');
    let indent = line.len() - t.len();
    let c = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = t.chars().take_while(|&x| x == c).count();
    let info = t[n..].trim();
    (indent <= 3 && n >= 3 && !(c == '`' && info.contains('`'))).then_some((indent, c, n, info))
}

fn fence_close(line: &str, c: char, n: usize) -> bool {
    let t = line.trim_start_matches(' ');
    line.len() - t.len() <= 3
        && t.chars().take_while(|&x| x == c).count() >= n
        && t.trim_end().chars().all(|x| x == c)
}

/// The text cut into prose and fenced blocks, in order. An unclosed fence runs to the end (`closed: false`).
pub fn split(text: &str) -> Vec<Seg> {
    let (mut out, mut prose) = (Vec::new(), String::new());
    let mut lines = text.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let l = line.trim_end_matches(['\n', '\r']);
        let Some((indent, c, n, info)) = fence_open(l) else {
            prose.push_str(line);
            continue;
        };
        if !prose.is_empty() {
            out.push(Seg::Prose(std::mem::take(&mut prose)));
        }
        let lang = info
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{'))
            .next()
            .unwrap_or("")
            .trim_start_matches('.')
            .to_lowercase();
        let (mut body, mut closed) = (Vec::new(), false);
        for b in lines.by_ref() {
            let b = b.trim_end_matches(['\n', '\r']);
            if fence_close(b, c, n) {
                closed = true;
                break;
            }
            let cut = b.len() - b.trim_start_matches(' ').len();
            body.push(&b[cut.min(indent)..]);
        }
        let body = body.join("\n");
        out.push(Seg::Block(Block {
            kind: classify(&lang, &body),
            hidden: body.chars().any(invisible),
            lang,
            body,
            closed,
        }));
    }
    if !prose.is_empty() {
        out.push(Seg::Prose(prose));
    }
    out
}

const SHELL: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "shell",
    "console",
    "terminal",
    "shell-session",
    "shellsession",
    "powershell",
    "pwsh",
    "cmd",
    "bat",
    "batch",
];
/// Languages whose lines are a session transcript: only the `$ ` lines are commands.
const SESSION: &[&str] = &["console", "terminal", "shell-session", "shellsession"];
const PLAIN: &[&str] = &[
    "",
    "text",
    "txt",
    "plain",
    "plaintext",
    "output",
    "log",
    "markdown",
    "md",
];
/// Programs that start a command in an unlabelled block. Words that are also English (make, find, go, open,
/// set, cat, head, test, which, source ...) are left out: they need a label or a `$ ` prompt.
const EXES: &[&str] = &[
    "git",
    "cargo",
    "rustup",
    "rustc",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "node",
    "python",
    "python3",
    "pip",
    "pip3",
    "uv",
    "docker",
    "docker-compose",
    "kubectl",
    "helm",
    "terraform",
    "brew",
    "apt",
    "apt-get",
    "dnf",
    "pacman",
    "winget",
    "choco",
    "curl",
    "wget",
    "sudo",
    "cd",
    "ls",
    "mkdir",
    "rm",
    "cp",
    "mv",
    "chmod",
    "chown",
    "export",
    "ssh",
    "scp",
    "rsync",
    "tar",
    "unzip",
    "gh",
    "synt",
    "mvn",
    "gradle",
    "dotnet",
    "pytest",
    "tsc",
    "echo",
    "pwd",
    "ln",
    "grep",
    "rg",
    "sed",
    "awk",
    "systemctl",
    "nvm",
    "xargs",
    "cmake",
];

/// Whether the info string names a unified diff.
fn diff_lang(lang: &str) -> bool {
    matches!(lang, "diff" | "patch" | "udiff")
}

fn hunk_header(l: &str) -> Option<(usize, usize)> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(r"^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@").expect("hunk regex is valid")
    });
    let c = r.captures(l)?;
    let n = |i| c.get(i).map_or(1, |m| m.as_str().parse().unwrap_or(1));
    Some((n(1), n(2)))
}

/// A unified diff: labelled so, or (unlabelled or plain) a hunk header with a changed line, or a `---`/`+++` pair.
pub fn is_diff(lang: &str, body: &str) -> bool {
    if diff_lang(lang) {
        return true;
    }
    if !PLAIN.contains(&lang) || matches!(lang, "markdown" | "md") {
        return false;
    }
    let ls: Vec<&str> = body.lines().collect();
    let hunk = ls.iter().any(|l| hunk_header(l).is_some());
    let changed = ls
        .iter()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count();
    let pair = ls
        .windows(2)
        .any(|w| w[0].starts_with("--- ") && w[1].starts_with("+++ "));
    (hunk && changed >= 1) || (pair && changed >= 1) || (hunk && pair)
}

/// The kind of a block, conservatively: a command only when the label says shell, or the lines all look like one.
pub fn classify(lang: &str, body: &str) -> Kind {
    if is_diff(lang, body) {
        Kind::Diff
    } else if SHELL.contains(&lang) {
        if command_text(lang, body).trim().is_empty() {
            Kind::Text
        } else {
            Kind::Command
        }
    } else if PLAIN.contains(&lang) {
        if looks_like_commands(body) {
            Kind::Command
        } else {
            Kind::Text
        }
    } else {
        Kind::Code
    }
}

/// `$ `, `# ` (root) are not stripped: only `$ ` and `> `.
fn strip_prompt(l: &str) -> Option<&str> {
    let t = l.trim_start();
    t.strip_prefix("$ ").or_else(|| (t == "$").then_some(""))
}

/// The text to copy or run for a command block: prompts removed (a session transcript keeps only its `$ ` lines).
pub fn command_text(lang: &str, body: &str) -> String {
    let ls: Vec<&str> = body.lines().collect();
    let gt = |l: &&str| l.trim().is_empty() || l.trim_start().starts_with("> ");
    let out: Vec<&str> = if SESSION.contains(&lang) && ls.iter().any(|l| strip_prompt(l).is_some())
    {
        ls.iter().filter_map(|l| strip_prompt(l)).collect()
    } else if ls.iter().all(gt) {
        ls.iter()
            .map(|l| l.trim_start().strip_prefix("> ").unwrap_or(l))
            .collect()
    } else {
        ls.iter().map(|l| strip_prompt(l).unwrap_or(l)).collect()
    };
    out.join("\n").trim_matches('\n').trim_end().to_string()
}

fn looks_like_commands(body: &str) -> bool {
    let (mut any, mut cont) = (false, false);
    for raw in body.lines() {
        let l = strip_prompt(raw).unwrap_or(raw).trim();
        if l.is_empty() || l.starts_with('#') {
            cont = false;
            continue;
        }
        let ok = cont || command_line(l);
        cont = l.ends_with('\\');
        if !ok {
            return false;
        }
        any = true;
    }
    any
}

fn command_line(l: &str) -> bool {
    let mut it = l.split_whitespace();
    let first = it.next().unwrap_or("");
    // an agent's slash command (`/add-plugin x`) is not a shell command; a path has a second slash
    if first.starts_with('/') && !first[1..].contains('/') {
        return false;
    }
    let start = EXES.contains(&first)
        || first.starts_with("./")
        || first.starts_with("../")
        || first.starts_with("~/")
        || (first.starts_with('/') && first.len() > 1)
        || env_assign(first) && it.next().is_some();
    let prose_end = l.ends_with(['!', '?']);
    let sentence = l.ends_with('.') && l[..l.len() - 1].ends_with(|c: char| c.is_alphabetic());
    let code = l.contains("=>") || l.ends_with(['{', '(', ',', '[']) || l.starts_with("//");
    start && !prose_end && !sentence && !code
}

fn env_assign(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether running the command needs a second, explicit click for what it does (not for its size).
// shortcut: denylist. A pattern list cannot know every harmful command (an alias, a script the command calls, a
// tool that deletes by its own flags); it widens what asks twice, it does not prove a command safe. An allowlist
// of programs is the stricter design and is an owner decision.
pub fn risky(cmd: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    const PARTS: &[&str] = &[
        // deletion, privilege, disks
        r"\b(?:rm|unlink|rmdir|del|erase|shred|truncate|killall|pkill|sudo|doas|chown|mkfs\S*|fdisk|parted|wipefs)\b",
        r"\bsu\s+-?\s*(?:\n|$)|\bdd\b[^\n]*\b(?:if|of)=|\bfind\b[^\n]*(?:-delete|-exec|-execdir)|\bxargs\b",
        // pipes into a shell or interpreter, eval, substitution, quote tricks
        r"\|\s*(?:sudo\s+)?(?:\w*sh|python3?|perl|ruby|node|iex|pwsh|powershell|nc|ncat|netcat|socat)\b",
        r"\beval\b|\$\(|`|<\(|\biex\b|\binvoke-expression\b|\bsource\s+<",
        // inline interpreters, shells, powershell, cmd
        r"\b(?:python[0-9.]*|perl|ruby|node|php|(?:ba|z|da|k|fi)?sh)\b[^\n|;&]*\s-[a-z]*[cer]\b",
        r"\b(?:powershell|pwsh)\b[^\n]*\s-(?:enc\w*|e|ec|c|command)\b|\bcmd(?:\.exe)?\s+/[ck]\b",
        // network: uploads, downloads, copies out, listeners
        r"\bcurl\b[^\n]*\s(?:-[a-zA-Z]*[FdTo]\b|--(?:form|data\S*|upload-file|json|output)\b)",
        r"\bwget\b[^\n]*(?:--post-\S+|--body-\S+|\s-[a-zA-Z]*[Oo]\b|--output\S*)",
        r"\b(?:scp|rsync|sftp|ssh|ftp|telnet|nc|ncat|netcat|socat|curl|wget|iwr|irm|invoke-webrequest|invoke-restmethod)\b|https?://",
        r"\bchmod\b|\bchown\b|\bbase64\b[^\n]*(?:-d|--decode)",
        // what a command may read or change outside the project
        r"\$\{?HOME\b|~[/\\]|\s~(?:\s|$)|\.ssh\b|\.aws\b|\.env\b|\.npmrc|\.git-credentials|\.netrc|\.kube|\.gnupg|/etc\b|/dev/|%USERPROFILE%|\$env:",
        r"tee\s+-a|>>\s*\S*\.(?:bashrc|zshrc|profile|bash_profile|zprofile)\b",
        // git that discards or rewrites
        r"\bgit\s+push\b[^\n]*(?:--force|\s-[a-z]*f\b|\s\+\S)|\bgit\s+(?:reset\s+--hard|clean|stash\s+drop|restore|filter-branch|update-ref\s+-d|branch\s+-D)\b",
        r"\bgit\s+checkout\s+(?:--\s|\.(?:\s|$))",
        // fork bomb, raw disks, power
        r":\(\)\s*\{|>\s*/dev/|\b(?:shutdown|reboot|halt|poweroff)\b|\binit\s+[06]\b|\bformat\s+[a-z]:|\b(?:rd|rmdir)\s+/s|\breg\s+delete|\bcrontab\s+-r",
        r"\bremove-item\b|\bkill\s+-9\s+-?1\b",
        // Windows and macOS: registry, tasks, downloaders, script hosts, files written by cmdlets, launch agents
        r"\breg\s+(?:add|delete|import)\b|\b(?:schtasks|bitsadmin|certutil|mshta|rundll32|regsvr32|start-process|osascript|launchctl|wmic|msiexec)\b",
        r"\b(?:powershell|pwsh)\b[^\n]*\s-(?:file|executionpolicy|ep)\b|\bsc\s+(?:create|config|delete)\b|\bdefaults\s+write\b",
        r"\b(?:set-content|out-file|add-content|new-item|copy-item|move-item|set-itemproperty|invoke-command|stop-process)\b",
        // files overwritten or processes ended
        r"\b(?:mv|cp|ln|tee|install|ren|rename|move|copy|xcopy|robocopy|kill|taskkill)\b|\bsed\b[^\n]*\s-[a-z]*i|\bperl\b[^\n]*\s-[a-z]*i",
        // installs and deploys
        r"\b(?:npm|pnpm|yarn|bun)\s+(?:i|install|add|ci|exec|dlx|x|publish|remove|uninstall)\b|\bnpx\b|\bpipx?\s+(?:install|uninstall)\b",
        r"\buv\s+(?:pip|tool|add)\b|\bcargo\s+(?:install|publish|add|uninstall)\b|\bbrew\s+(?:install|uninstall|upgrade|reinstall)\b",
        r"\b(?:apt|apt-get|dnf|yum|pacman|zypper|apk|winget|choco|scoop)\b|\bgem\s+install\b|\bgo\s+(?:install|get)\b",
        r"\b(?:docker|podman)\b[^\n]*(?:\brun\b|\s-v\b|--volume|--privileged|\bexec\b|compose\s+up|system\s+prune|\brm\b|volume\s+rm)",
        r"\bkubectl\s+(?:delete|apply|exec|create|replace|patch|edit|scale)\b|\bterraform\s+(?:apply|destroy)\b",
    ];
    let r = R.get_or_init(|| {
        Regex::new(&format!("(?i)(?:{})", PARTS.join("|"))).expect("risk regex is valid")
    });
    // a continuation joins the next line (`\`, `^` of cmd, a backtick of PowerShell), so its flags count
    let joined = join_continuations(cmd);
    r.is_match(&joined) || truncating_redirect(&joined) || disguised_command_word(&joined)
}

fn join_continuations(cmd: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\\^`][ \t]*\r?\n").expect("continuation regex is valid"))
        .replace_all(cmd, " ")
        .into_owned()
}

/// Splits at `;`, `&`, `|`, `(`, `)` and newlines outside quotes, then at spaces outside quotes.
fn command_words(cmd: &str) -> Vec<Vec<String>> {
    let (mut segs, mut words, mut w) = (Vec::new(), Vec::new(), String::new());
    let mut quote: Option<char> = None;
    let mut esc = false;
    for c in cmd.chars() {
        if esc {
            (esc, _) = (false, w.push(c));
        } else if c == '\\' && quote != Some('\'') {
            (esc, _) = (true, w.push(c));
        } else if let Some(q) = quote {
            quote = (c != q).then_some(q);
            w.push(c);
        } else if c == '\'' || c == '"' {
            (quote, _) = (Some(c), w.push(c));
        } else if c.is_whitespace() && c != '\n' || matches!(c, ';' | '&' | '|' | '(' | ')' | '\n')
        {
            if !w.is_empty() {
                words.push(std::mem::take(&mut w));
            }
            if !c.is_whitespace() || c == '\n' {
                segs.push(std::mem::take(&mut words));
            }
        } else {
            w.push(c);
        }
    }
    if !w.is_empty() {
        words.push(w);
    }
    segs.push(words);
    segs
}

/// A command word (the first word of a statement, after `NAME=value` words) that holds a quote, a backslash,
/// `$` or a glob character: `r''m`, `$'\x72m'`, `r${x}m`, `/bin/r?`. What it runs is not what it says.
fn disguised_command_word(cmd: &str) -> bool {
    command_words(cmd).iter().any(|ws| {
        ws.iter().find(|w| !env_assign(w)).is_some_and(|w| {
            !matches!(w.as_str(), "[" | "[[")
                && w.contains(['\'', '"', '\\', '$', '*', '?', '[', '`'])
        })
    })
}

/// A `> file` that overwrites something other than an obvious build output.
fn truncating_redirect(cmd: &str) -> bool {
    const OUT: [&str; 6] = [
        "target/",
        "build/",
        "dist/",
        "out/",
        "./target/",
        "./build/",
    ];
    let b = cmd.as_bytes();
    (0..b.len()).any(|i| {
        let prev = i.checked_sub(1).map(|j| b[j]);
        let next = b.get(i + 1).copied();
        if b[i] != b'>'
            || matches!(prev, Some(b'>' | b'-' | b'=' | b'<'))
            || matches!(next, Some(b'>' | b'&' | b'='))
        {
            return false;
        }
        let rest = cmd[i + 1..].trim_start_matches(['|', ' ']);
        let target = rest.split_whitespace().next().unwrap_or("");
        !OUT.iter()
            .any(|o| target.trim_matches(['"', '\'']).starts_with(o))
    })
}

/// Why a second click is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Any other command: "Approve and run".
    Always,
    /// Several lines, sent as one paste: "Approve and run N lines".
    Lines(usize),
    /// A risky command, shown whole first.
    Risky,
    /// The message follows web, browser or MCP content.
    Untrusted,
}

impl Need {
    /// Why `cmd` needs an approval click (every command does; risky or untrusted first).
    pub fn of(cmd: &str, tainted: bool) -> Self {
        let lines = cmd.lines().filter(|l| !l.trim().is_empty()).count();
        if risky(cmd) {
            Self::Risky
        } else if tainted {
            Self::Untrusted
        } else if lines > 1 {
            Self::Lines(lines)
        } else {
            Self::Always
        }
    }

    /// The approval control's label; the warning ones keep their reason.
    pub fn label(self) -> String {
        match self {
            Self::Always => "Approve and run".into(),
            Self::Lines(n) => format!("Approve and run {n} lines"),
            Self::Risky => "Approve: risky command, run anyway".into(),
            Self::Untrusted => "Approve: untrusted content, run anyway".into(),
        }
    }

    /// Drawn in the danger colour.
    pub fn warn(self) -> bool {
        matches!(self, Self::Risky | Self::Untrusted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Ignored,
    /// First click: arms the approval (nothing runs).
    Confirm(Need),
    Run,
}

/// Lines of a block the chat draws, and the bytes of a command Run takes: what is not drawn is not run.
pub const MAX_LINES: usize = 400;
pub const MAX_RUN_BYTES: usize = 16 * 1024;

/// Why Run is not offered for `cmd` (the stripped command of `b`), if it is not.
pub fn run_refusal(b: &Block, cmd: &str) -> Option<&'static str> {
    if b.body.lines().count() > MAX_LINES || cmd.len() > MAX_RUN_BYTES {
        Some("too long to run from here")
    } else if cmd.contains('\t') {
        Some("has a tab")
    } else {
        None
    }
}

/// Milliseconds a block must have been drawn complete before a click counts, and the same gap between the
/// two clicks of a confirmation (a double click cannot confirm).
pub const ARM_MS: u64 = 400;
/// The transcript must not have scrolled or changed shape for this long (a click lands where it was aimed).
pub const QUIET_MS: u64 = 500;
/// The pointer must have rested on the button this long before a click counts.
pub const HOVER_MS: u64 = 300;
/// A pending approval lapses after this long.
pub const CONFIRM_MS: u64 = 15_000;
const MAX_SEEN: usize = 256;

/// What the front end knows about one press.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    /// A real mouse (or touch) click, once: not a key, not a double click, not a call from code.
    pub mouse: bool,
    /// The message holding the block is still streaming.
    pub streaming: bool,
    /// Nothing streams anywhere in the chat and no turn runs (the chat is not about to move under the pointer).
    pub quiet: bool,
    /// The message follows web, browser or MCP content: the command always asks twice.
    pub tainted: bool,
    /// A terminal can be opened here.
    pub runnable: bool,
}

pub fn key(cmd: &str) -> u64 {
    let mut h = DefaultHasher::new();
    cmd.hash(&mut h);
    h.finish()
}

/// The state machine behind Run: nothing runs without a real click on a block that has been complete on screen
/// for [`ARM_MS`], with the transcript still for [`QUIET_MS`] and the pointer resting on the button for
/// [`HOVER_MS`]; every command needs a second, approving click.
#[derive(Default)]
pub struct Gate {
    /// Block key to the time it was first drawn complete (not streaming).
    seen: HashMap<u64, u64>,
    pending: Option<(u64, u64)>,
    /// Shape of the transcript at the last look (scroll offset, height, row count, width) and when it changed.
    shape: u64,
    shifted: u64,
    /// The button under the pointer and since when.
    hover: Option<(u64, u64)>,
}

impl Gate {
    /// The block `key` was drawn at `now` ms; only a complete block of a finished message starts its arm clock.
    pub fn drawn(&mut self, key: u64, now: u64, complete: bool) {
        if !complete {
            self.seen.remove(&key);
            return;
        }
        if self.seen.len() >= MAX_SEEN && !self.seen.contains_key(&key) {
            self.seen.clear();
        }
        self.seen.entry(key).or_insert(now);
    }

    /// The transcript was drawn with this `shape`; a different one than the last time is a scroll or a layout
    /// change, and clicks wait for [`QUIET_MS`].
    pub fn layout(&mut self, shape: u64, now: u64) {
        if shape != self.shape {
            (self.shape, self.shifted) = (shape, now);
            self.cancel();
        }
    }

    /// The pointer entered (`on`) or left the Run button of `key`.
    pub fn hovered(&mut self, key: u64, on: bool, now: u64) {
        if on {
            if self.hover.is_none_or(|(k, _)| k != key) {
                self.hover = Some((key, now));
            }
        } else if self.hover.is_some_and(|(k, _)| k == key) {
            self.hover = None;
            // the pointer left the approval control: that is "elsewhere"
            self.cancel();
        }
    }

    /// Drops a pending approval (Esc, the pointer leaving, a scroll or layout change). Never approves.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// The next click on `key` is the second one of a confirmation (the button then reads [`Need::label`]).
    pub fn confirming(&self, key: u64, now: u64) -> bool {
        self.pending
            .is_some_and(|(k, t)| k == key && now.saturating_sub(t) <= CONFIRM_MS)
    }

    pub fn press(&mut self, b: &Block, cmd: &str, ctx: Ctx, now: u64) -> Press {
        let k = key(cmd);
        let armed = self
            .seen
            .get(&k)
            .is_some_and(|&t| now.saturating_sub(t) >= ARM_MS);
        let rested = self
            .hover
            .is_some_and(|(hk, t)| hk == k && now.saturating_sub(t) >= HOVER_MS);
        if !ctx.mouse
            || ctx.streaming
            || !ctx.quiet
            || !ctx.runnable
            || !b.closed
            || b.hidden
            || b.kind != Kind::Command
            || run_refusal(b, cmd).is_some()
            || cmd.trim().is_empty()
            || !armed
            || !rested
            || now.saturating_sub(self.shifted) < QUIET_MS
        {
            return Press::Ignored;
        }
        if let Some((pk, t)) = self.pending
            && pk == k
            && now.saturating_sub(t) <= CONFIRM_MS
        {
            if now.saturating_sub(t) < ARM_MS {
                return Press::Ignored;
            }
            self.pending = None;
            return Press::Run;
        }
        self.pending = Some((k, now));
        Press::Confirm(Need::of(cmd, ctx.tainted))
    }
}

/// The tone of every line of a unified diff, in order. Hunk counts tell a removed `-- x` line from a `---` header.
pub fn diff_tones(body: &str) -> Vec<(Tone, &str)> {
    const HEAD: [&str; 11] = [
        "diff ",
        "index ",
        "--- ",
        "+++ ",
        "new file",
        "deleted file",
        "similarity",
        "rename ",
        "old mode",
        "new mode",
        "Binary files",
    ];
    let (mut old, mut new) = (0usize, 0usize);
    body.lines()
        .map(|l| {
            if old > 0 || new > 0 {
                match l.chars().next() {
                    Some('+') => {
                        new = new.saturating_sub(1);
                        return (Tone::Add, l);
                    }
                    Some('-') => {
                        old = old.saturating_sub(1);
                        return (Tone::Del, l);
                    }
                    Some('\\') => return (Tone::Dim, l),
                    Some(' ') | None => {
                        (old, new) = (old.saturating_sub(1), new.saturating_sub(1));
                        return (Tone::Text, l);
                    }
                    // the hunk ended early (a cut diff): this line is a header again
                    _ => (old, new) = (0, 0),
                }
            }
            if let Some((o, n)) = hunk_header(l) {
                (old, new) = (o, n);
                (Tone::Hunk, l)
            } else if HEAD.iter().any(|h| l.starts_with(h)) {
                (Tone::Strong, l)
            } else if l.starts_with('+') {
                (Tone::Add, l)
            } else if l.starts_with('-') {
                (Tone::Del, l)
            } else {
                (Tone::Text, l)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
