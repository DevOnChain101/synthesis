//! Permission engine: a port of Node `src/native/permissions.js` (docs/specs/tools.md §3).
//!
//! Rules are `Tool(spec)` strings in allow/ask/deny lists. `Permissions::check` decides one tool
//! call under a permission mode, in this order: deny rule, dialog tools, critical `rm`, outside
//! read (blocked / network), ask rule, bypassPermissions, plan, protected paths, allow rules,
//! mode defaults. Everything is explicit: cwd, home, mode and settings are passed in; nothing
//! reads the process environment or the process cwd.
//!
//! Deviations from Node (all tightenings, each pinned by a test):
//! 1. Non-interactive (`Env::interactive == false`): every `ask` becomes `deny`. Node lets an
//!    outside read run when nobody can answer.
//! 2. `recheck` re-runs the policy on input rewritten by a hook or approver (Node does not).
//! 3. `\` is never rewritten to `/` on POSIX; case is folded only for the `PathPolicy`
//!    (windows, macOS), including the protected-path scan (Node folded on win32 only).
//! 4. Relative tool paths resolve against the explicit `cwd`, not the process cwd.
//! 5. Glob `**` also spans a newline in a file name.
//!
//! Node weaknesses fixed on purpose (each pinned by a test in `tests.rs`, each golden case that
//! differs is listed in `tests/golden-divergence.json` with its reason):
//! - `//abs` rule specs are absolute; a bare `Bash` rule applies to every Bash call.
//! - Deny/ask Bash rules see every segment (`& \n ; | && ||`, `(..)`, `$(..)`, backticks,
//!   `sh -c`, `eval`, `sudo`/`env`/`FOO=1`/`/bin/rm` wrappers) and match when the command cannot
//!   be parsed. Allow rules need the strict parser and match each simple command on a word
//!   boundary; if the strict parse fails they are ignored.
//! - Protected paths are checked (also in redirect targets) before any allow rule; the read-only
//!   Bash allowlist is a per-command flag allowlist and its operands must be inside the workspace.
//! - Path rules are evaluated on the lexical path and the realpath; tool input uses a per-tool
//!   path field and an unexpected extra path-like field asks; `Read(..)` deny/ask rules also
//!   cover Grep, Glob and the edit tools, and a `Read(..)` deny rule blocks Bash file readers
//!   (`cat`, `head`, `grep`, `sed`, `awk`, `< file` ...) on that path.
//! - `mcp__*` and `mcp__server__*` are prefix globs (deny/ask); `mcp__*` never works as allow.
//! - An ask rule still prompts under bypassPermissions (Claude Code does the same).
//! - Inner commands are seen by deny/ask rules through `find -exec/-execdir/-ok/-okdir` and
//!   `-delete` (as `rm -rf`), `xargs`, `env`, `nice`, `nohup`, `timeout`, `time`, `sudo`, `doas`,
//!   `watch`, `parallel`, `git -c alias.x=!cmd` / `core.pager|editor|sshCommand|...=cmd`, and
//!   `sh -c` / `eval` bodies. Code in another language on the command line (`python -c`,
//!   `node -e`, `perl -e`, `ruby -e`, `php -r`) cannot be read: every deny/ask Bash rule matches.
//!   Allow: such a command and `sh -c` / `eval` is allowed by an exact spec only, never by
//!   `prefix:*`. Quoted words in a `Bash(..)` spec compare by their unquoted value.
//! - Windows-policy names that alias files (ADS, trailing dot/space, 8.3, devices) are protected.
//! - Design choice: `.env*`, `.ssh`, `.aws`, `.github/workflows`, `.gitattributes` are protected.

//! # Rule scoping matrix (non-path tools)
//!
//! `matches_rule` first matches the tool name (`Tool`, `Edit` for the edit family, `Task` = `Agent`,
//! `mcp__server` for `mcp__server__*`, a trailing-`*` prefix glob such as `browser_*` /
//! `terminal_*`), then the specifier in parentheses:
//!
//! | rule                        | scoped by input field                                   |
//! |-----------------------------|---------------------------------------------------------|
//! | `Task(t)`, `Agent(t)`       | `subagent_type` == `t` (exact, case-sensitive; `t*` prefix) |
//! | `Skill(n)`                  | `skill` == `n` (same rules)                             |
//! | `WebFetch(domain:h)`, `browser_navigate(domain:h)` | `url` host == `h` or a subdomain; both lowercased, IDNA, trailing dot dropped |
//! | Read/Write/Edit/Grep/Glob   | path rules (see `paths`)                                |
//! | anything else with `(..)`   | no scope exists: deny/ask match, allow does NOT         |
//!
//! Fail closed: when the specifier cannot be applied (missing or non-string field, malformed
//! `domain:`), deny/ask rules match and allow rules do not. Exception kept from Node: a URL
//! that does not parse (no host) matches nothing. No specifier = the whole tool.
//!
//! # Recursive Grep / Glob
//!
//! A Grep/Glob whose search directory (`path`, default the cwd; the static root of a Glob
//! pattern) contains the root of a `Read(spec)` deny rule (a relative spec is rooted at the
//! cwd) asks with a reason (`deny` in dontAsk and when non-interactive, also in bypass).
//! A `path` that is a file is judged by the ordinary rule match. The call cannot opt out by
//! its `glob` filter.

pub mod automode;
mod automode_defaults;
pub mod bash;
pub mod paths;

use std::path::{Path, PathBuf};

use serde_json::Value;
use synthesis_core::PathPolicy;

pub use bash::{is_auto_safe_bash, is_broad_allow, is_read_only_bash, is_safe_bash_command};

pub const OUTSIDE_READ_BLOCKED: &str = "Reads outside the working directories are blocked (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir or remove the setting.";

pub const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "BashOutput",
    "TodoWrite",
    "TaskCreate",
    "TaskUpdate",
    "TaskList",
    "TaskGet",
    "Skill",
    "Task",
    "Agent",
    "ListAgents",
    "ListSessions",
    "AskUserQuestion",
    "ExitPlanMode",
];
const EDIT_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];
const PATH_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
];
/// Tools whose `command` is a shell command: judged like `Bash`, never by a blanket allow.
const COMMAND_TOOLS: &[&str] = &["verify_run", "terminal_spawn"];
const DIALOG_TOOLS: &[&str] = &["ExitPlanMode", "AskUserQuestion"];
const OUTSIDE_READ_TOOLS: &[&str] = &["Read", "Grep", "Glob"];
const PROTECTED_DIRS: &[&str] = &[
    ".git",
    ".vscode",
    ".idea",
    ".husky",
    ".cargo",
    ".devcontainer",
    ".yarn",
    ".mvn",
    ".claude",
    ".synthesis",
    ".ssh",
    ".aws",
];
const PROTECTED_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".gitattributes",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_aliases",
    ".bash_logout",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".profile",
    ".envrc",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnp.cjs",
    ".pnp.loader.mjs",
    ".pnpmfile.cjs",
    "bunfig.toml",
    ".bunfig.toml",
    ".bazelrc",
    ".bazelversion",
    ".bazeliskrc",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yml",
    ".lefthook.yaml",
    "gradle-wrapper.properties",
    "maven-wrapper.properties",
    ".devcontainer.json",
    ".ripgreprc",
    "pyrightconfig.json",
    ".mcp.json",
    ".claude.json",
    // instruction files are loaded as trusted prompt context (lowercase: case-folded policies)
    "SYNTHESIS.md",
    "AGENTS.md",
    "synthesis.md",
    "agents.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    BypassPermissions,
}

impl Mode {
    pub const ALL: [Mode; 6] = [
        Mode::Default,
        Mode::AcceptEdits,
        Mode::Plan,
        Mode::Auto,
        Mode::DontAsk,
        Mode::BypassPermissions,
    ];

    /// Stored name (`default`, `acceptEdits`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "acceptEdits",
            Mode::Plan => "plan",
            Mode::Auto => "auto",
            Mode::DontAsk => "dontAsk",
            Mode::BypassPermissions => "bypassPermissions",
        }
    }

    /// Like Node `normalizeMode`: `manual` is the UI name of `default`; unknown -> `None`.
    pub fn parse(s: &str) -> Option<Mode> {
        if s == "manual" {
            return Some(Mode::Default);
        }
        Mode::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
    /// Hand the call to the auto mode classifier.
    Classify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub decision: Decision,
    pub reason: String,
    /// Critical-path removal: in auto the agent asks with a countdown.
    pub critical: bool,
    /// First read outside the working directories (the four-option dialog), with the path.
    pub outside_read: Option<PathBuf>,
    /// The ask came from an explicit ask rule.
    pub rule: bool,
    /// User-facing text for a deny that needs more than `reason`.
    pub message: Option<&'static str>,
    /// SHA-256 (hex) of the tool input JSON, set when that JSON is longer than
    /// `INPUT_DISPLAY_LIMIT` chars (a dialog shows only the head); the reason says so.
    pub full_input_digest: Option<String>,
    /// An `ask` that even a command the user typed must not get past: critical path removal,
    /// a protected path, ambiguous path fields.
    pub hard: bool,
}

/// Chars of input JSON a generic permission dialog shows.
pub const INPUT_DISPLAY_LIMIT: usize = 400;

impl Verdict {
    fn new(decision: Decision, reason: impl Into<String>) -> Self {
        Self {
            decision,
            reason: reason.into(),
            critical: false,
            outside_read: None,
            rule: false,
            message: None,
            full_input_digest: None,
            hard: false,
        }
    }
}

/// `permissions` block of settings.
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
    pub additional_directories: Vec<PathBuf>,
}

/// `autoMode` settings that change policy (user-level only).
#[derive(Debug, Clone, Copy)]
pub struct AutoModeSettings {
    pub classify_all_shell: bool,
    /// Plan mode behaves like auto for Bash/outside reads. Node default: true.
    pub use_auto_mode_during_plan: bool,
}

impl Default for AutoModeSettings {
    fn default() -> Self {
        Self {
            classify_all_shell: false,
            use_auto_mode_during_plan: true,
        }
    }
}

/// Everything the engine needs from the outside world, passed explicitly.
#[derive(Debug, Clone)]
pub struct Env {
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// The session's current mode (what `check_current` uses).
    pub mode: Mode,
    pub auto_mode: AutoModeSettings,
    pub path_policy: PathPolicy,
    /// `false` = nobody can answer a prompt (headless): every `ask` becomes `deny`.
    pub interactive: bool,
    /// Extra protected paths (files or directories, e.g. Synthesis's own state under a custom
    /// `SYNTHESIS_HOME`, which the `.synthesis` directory name does not cover).
    pub protected: Vec<PathBuf>,
    /// Paths nothing may read, whatever the mode or rules (Synthesis's own state, the peer key):
    /// Read/Grep/Glob/LS/edit tools on them or on an ancestor are denied, and so is a Bash command
    /// that names one of them.
    pub deny_read: Vec<PathBuf>,
    /// Also deny reads of secret-shaped files (`.env*`, keys, `.ssh`, `.aws`, `.git-credentials`).
    /// Set for the peer responder, which answers a stranger.
    pub deny_secret_reads: bool,
    /// The repository / project root when the session started in a subdirectory of it. `None` =
    /// `cwd`. Rules written `/x` or `./x`, and the inside-the-workspace test, are anchored here;
    /// a bare cwd-relative path keeps `cwd`.
    pub project_root: Option<PathBuf>,
}

impl Env {
    /// Fail-closed defaults: native path policy, non-interactive.
    pub fn new(cwd: impl Into<PathBuf>, home: impl Into<PathBuf>, mode: Mode) -> Self {
        Self {
            cwd: cwd.into(),
            home: home.into(),
            mode,
            auto_mode: AutoModeSettings::default(),
            path_policy: PathPolicy::NATIVE,
            interactive: false,
            protected: vec![],
            deny_read: vec![],
            deny_secret_reads: false,
            project_root: None,
        }
    }

    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = Some(root.into());
        self
    }

    /// Where `/x` rules and the workspace begin.
    fn anchor(&self) -> &Path {
        self.project_root.as_deref().unwrap_or(&self.cwd)
    }

    pub fn protect(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.protected.extend(paths);
        self
    }

    pub fn interactive(mut self, yes: bool) -> Self {
        self.interactive = yes;
        self
    }

    pub fn deny_reads(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.deny_read.extend(paths);
        self
    }
}

/// `.ssh` / `.aws` / `.gnupg` directories and secret-shaped file names.
fn secret_shaped(p: &Path) -> bool {
    synthesis_core::secrets::is_secret_shaped(p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    pub tool_name: String,
    pub rule_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suggestion {
    /// Always destined for the session; `behavior` is Allow, Ask or Deny (anything else = Allow).
    AddRules {
        rules: Vec<RuleSpec>,
        behavior: Decision,
    },
    SetMode(Mode),
}

struct OutsideRead {
    path: PathBuf,
    network: bool,
}

struct BashFs {
    ok: bool,
    targets: Vec<PathBuf>,
}

/// An exact `Bash(<cmd>)` rule would be read as allow-all (`*`) or a prefix rule (`x:*`).
fn exact_unsafe(cmd: &str) -> bool {
    cmd.is_empty() || cmd == "*" || cmd.ends_with(":*")
}

/// Rule content to offer for one simple command (`words`, original text `whole`); None = offer
/// nothing. `exact_ok == false` (a subcommand of a compound, whose text is re-joined) never
/// offers an exact rule.
fn bash_rule(words: &[&str], whole: &str, exact_ok: bool) -> Option<String> {
    const SUBCOMMAND_TOOLS: &[&str] = &[
        "git", "npm", "npx", "pnpm", "yarn", "docker", "kubectl", "cargo", "go",
    ];
    // a prefix rule on these would allow arbitrary code or destructive calls: exact command only
    const EXACT_ONLY: &[&str] = &[
        "cd", "rm", "rmdir", "timeout", "nice", "nohup", "command", "time", "ssh", "su", "doas",
    ];
    let first = words.first().copied().unwrap_or("");
    // a first word with shell syntax glued on (`whoami;`) is not a command name
    if first.is_empty()
        || exact_unsafe(first)
        || first.contains([';', '&', '|', '<', '>', '(', ')', '`'])
    {
        return None;
    }
    if bash::INTERPRETERS.contains(&first)
        || EXACT_ONLY.contains(&first)
        || first.contains(['=', '/', '$', '\\'])
    {
        return (exact_ok && !exact_unsafe(whole)).then(|| whole.to_string());
    }
    Some(if SUBCOMMAND_TOOLS.contains(&first) {
        format!(
            "{}:*",
            words.iter().take(2).copied().collect::<Vec<_>>().join(" ")
        )
    } else {
        format!("{first}:*")
    })
}

#[derive(Debug, Clone)]
pub struct Permissions {
    pub rules: PermissionRules,
    pub env: Env,
    /// User-level only: refuse every outside read.
    pub block_outside_reads: bool,
    /// The first-outside-read prompt in auto was already answered "keep allowing".
    pub outside_reads_answered: bool,
}

fn command_of(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
}

fn non_empty<'a>(input: &'a Value, k: &str) -> Option<&'a str> {
    input
        .get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The input field(s) that carry the target path of a path tool.
fn path_fields(tool: &str) -> &'static [&'static str] {
    match tool {
        "Read" | "Write" | "Edit" | "MultiEdit" => &["file_path"],
        "NotebookEdit" => &["notebook_path", "file_path"],
        "Grep" | "Glob" => &["path"],
        _ => &[],
    }
}

const PATH_LIKE: &[&str] = &[
    "file_path",
    "path",
    "notebook_path",
    "filepath",
    "filePath",
    "file",
    "filename",
    "paths",
    "files",
    "directory",
    "dir",
    "cwd",
    "target",
    "destination",
    "dest",
    "old_path",
    "new_path",
    "source",
];

/// A path tool whose input carries a path-like field it does not use, or whose own path fields
/// disagree: which path the tool really touches is unclear, so the call asks.
fn ambiguous_paths(tool: &str, input: &Value) -> bool {
    let own = path_fields(tool);
    let Some(obj) = input.as_object().filter(|_| !own.is_empty()) else {
        return false;
    };
    let vals: Vec<&str> = own.iter().filter_map(|k| non_empty(input, k)).collect();
    obj.keys()
        .any(|k| PATH_LIKE.contains(&k.as_str()) && !own.contains(&k.as_str()))
        || vals.windows(2).any(|w| w[0] != w[1])
}

/// The path a path tool acts on (per-tool field map).
fn path_of<'a>(tool: &str, input: &'a Value) -> Option<&'a str> {
    path_fields(tool).iter().find_map(|k| non_empty(input, k))
}

fn url_host(u: &str) -> Option<String> {
    url::Url::parse(u).ok()?.host_str().map(str::to_string)
}

/// The specifier of a rule on a tool that has no path scope. Deny/ask (`any`) match when the
/// specifier cannot be applied (unknown tool/specifier, missing or non-string input field);
/// allow then does not.
fn scope_match(spec: &str, tool: &str, input: &Value, any: bool) -> bool {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let name = |v: Option<&str>| match v {
        // exact, case-sensitive; a trailing `*` is a prefix glob
        Some(v) => spec
            .strip_suffix('*')
            .map_or(v == spec, |p| v.starts_with(p)),
        None => any,
    };
    match tool {
        "Task" | "Agent" => name(field("subagent_type")),
        "Skill" => name(field("skill")),
        "WebFetch" | "browser_navigate" => {
            let Some(d) = spec.strip_prefix("domain:") else {
                return any;
            };
            // IDNA + lowercase through `url`; a trailing dot is the same host
            let Some(d) = url::Host::parse(d.trim_end_matches('.'))
                .ok()
                .map(|h| h.to_string())
            else {
                return any;
            };
            let Some(host) = field("url").and_then(url_host) else {
                return false;
            };
            let host = host.trim_end_matches('.');
            host == d || host.ends_with(&format!(".{d}"))
        }
        _ => any,
    }
}

impl Permissions {
    pub fn new(rules: PermissionRules, env: Env) -> Self {
        Self {
            rules,
            env,
            block_outside_reads: false,
            outside_reads_answered: false,
        }
    }

    pub fn mode(&self) -> Mode {
        self.env.mode
    }

    pub fn set_mode(&mut self, m: Mode) {
        self.env.mode = m;
    }

    /// The allow / deny / ask rules in force (shown by `/permissions`).
    pub fn rules(&self) -> &PermissionRules {
        &self.rules
    }

    /// Adds a session-only allow rule typed by the user; `false` if it was already there.
    pub fn add_allow(&mut self, rule: &str) -> bool {
        let new = !self.rules.allow.iter().any(|r| r == rule);
        if new {
            self.rules.allow.push(rule.to_string());
        }
        new
    }

    /// Removes `rule` from allow, deny and ask; returns how many entries went.
    pub fn remove_rule(&mut self, rule: &str) -> usize {
        let r = &mut self.rules;
        let before = r.allow.len() + r.deny.len() + r.ask.len();
        r.allow.retain(|x| x != rule);
        r.deny.retain(|x| x != rule);
        r.ask.retain(|x| x != rule);
        before - (r.allow.len() + r.deny.len() + r.ask.len())
    }

    fn policy(&self) -> PathPolicy {
        self.env.path_policy
    }

    fn classifies(&self, mode: Mode) -> bool {
        mode == Mode::Auto || (mode == Mode::Plan && self.env.auto_mode.use_auto_mode_during_plan)
    }

    // --- rule matching -------------------------------------------------------------------

    /// Does one non-Bash rule match this call? Port of `matchesRule`.
    fn matches_rule(&self, rule: &str, tool: &str, input: &Value, any: bool) -> bool {
        let (rt, spec) = bash::parse_rule(rule);
        if rt == "*" {
            return true;
        }
        // An `Edit(...)` rule also governs Write/MultiEdit/NotebookEdit; `Task` and `Agent` are one
        // tool; `mcp__server` covers `mcp__server__*`; `browser_*` is a prefix glob on the name.
        // A deny/ask `Read(..)` rule also covers Grep and Glob (they read the same files).
        // (and the edit tools: a file you may not read you may not overwrite blind either)
        let read_family =
            any && rt == "Read" && (matches!(tool, "Grep" | "Glob") || EDIT_TOOLS.contains(&tool));
        let named = if let (true, Some(prefix)) = (rt.starts_with("mcp__"), rt.strip_suffix('*')) {
            // `mcp__*` / `mcp__server__*`; `mcp__*` as an allow rule is never broad enough to matter:
            // callers skip it (see `allow_rules`)
            tool.starts_with(prefix)
        } else if rt.starts_with("mcp__") {
            rt == tool || tool.starts_with(&format!("{rt}__"))
        } else if let Some(prefix) = rt.strip_suffix('*') {
            tool.starts_with(prefix)
        } else {
            rt == tool
                || read_family
                || (rt == "Edit" && EDIT_TOOLS.contains(&tool))
                || (matches!(rt, "Task" | "Agent") && matches!(tool, "Task" | "Agent"))
        };
        if !named {
            return false;
        }
        let Some(spec) = spec else { return true };
        if PATH_TOOLS.contains(&rt) {
            // deny/ask: any target, lexical or real; allow: every target, both forms
            let (spec, base) = self.rule_base(spec);
            let m = |t: &String| {
                // resolve against cwd first: only the rule's own anchor moves
                let t = paths::resolve(&self.env.cwd, t)
                    .to_string_lossy()
                    .into_owned();
                paths::match_path_rule_real(spec, &t, base, &self.env.home, self.policy(), any)
            };
            let targets = self.path_targets(tool, input);
            return if any {
                targets.iter().any(m)
            } else {
                !targets.is_empty() && targets.iter().all(m)
            };
        }
        scope_match(spec, tool, input, any)
    }

    /// `/x` and `./x` rules are written for the project root, wherever the session started; any
    /// other spec (bare, `~/`, `//`) keeps `cwd`. Returns the spec in `/x` form and its base.
    fn rule_base<'a>(&'a self, spec: &'a str) -> (&'a str, &'a Path) {
        match spec.strip_prefix('.') {
            Some(r) if r.starts_with('/') => (r, self.env.anchor()),
            _ if spec.starts_with('/') && !spec.starts_with("//") => (spec, self.env.anchor()),
            _ => (spec, &self.env.cwd),
        }
    }

    /// A `Read(..)` deny rule names something under the directory a Grep / Glob call searches
    /// (its `path`, or the working directory; for Glob the static root of the pattern). The
    /// call names no file the rule could match, so the rule cannot be applied to it directly.
    fn recursive_read_hit(&self, tool: &str, input: &Value) -> Option<String> {
        if !matches!(tool, "Grep" | "Glob") {
            return None;
        }
        let policy = self.policy();
        let mut root = paths::resolve(&self.env.cwd, non_empty(input, "path").unwrap_or("."));
        if let (Some(pat), "Glob") = (non_empty(input, "pattern"), tool) {
            root = PathBuf::from(paths::glob_static_root(pat, &root, policy));
        }
        // a file is checked by the ordinary rule match
        if !root.is_dir() {
            return None;
        }
        self.rules.deny.iter().find_map(|r| {
            let (rt, spec) = bash::parse_rule(r);
            let spec = spec.filter(|_| rt == "Read")?;
            let (spec, base) = self.rule_base(spec);
            let at = paths::rule_root(spec, base, &self.env.home, policy);
            (paths::within(&root, &at, policy) || paths::within(&at, &root, policy))
                .then(|| r.clone())
        })
    }

    /// The paths a call names: its path field, plus the file pattern of Glob / Grep `glob`.
    fn path_targets(&self, tool: &str, input: &Value) -> Vec<String> {
        let own = path_of(tool, input);
        let mut v: Vec<String> = own.map(str::to_string).into_iter().collect();
        let pat = match tool {
            "Glob" => "pattern",
            "Grep" => "glob",
            _ => "",
        };
        if let Some(g) = non_empty(input, pat) {
            let base = paths::resolve(&self.env.cwd, own.unwrap_or("."));
            v.push(paths::resolve(&base, g).to_string_lossy().into_owned());
        }
        v
    }

    /// Engine-enforced read ban (`Env::deny_read`, `Env::deny_secret_reads`); no rule or mode lifts it.
    fn hard_read_denied(&self, tool: &str, input: &Value) -> Option<&'static str> {
        const WHY: &str = "blocked: that path is Synthesis state or a secret and may not be read";
        if self.env.deny_read.is_empty() && !self.env.deny_secret_reads {
            return None;
        }
        let policy = self.policy();
        if tool == "Bash" {
            let cmd = command_of(input);
            let hit = self
                .env
                .deny_read
                .iter()
                .any(|p| cmd.contains(p.to_string_lossy().as_ref()));
            return hit.then_some(WHY);
        }
        let mut targets: Vec<String> = ["file_path", "path", "notebook_path"]
            .iter()
            .filter_map(|k| non_empty(input, k).map(str::to_string))
            .collect();
        if matches!(tool, "Grep" | "Glob" | "LS") && targets.is_empty() {
            targets.push(".".into());
        }
        targets.extend(self.path_targets(tool, input));
        if targets.is_empty() || !(PATH_TOOLS.contains(&tool) || tool == "LS") {
            return None;
        }
        // the roots and their real paths: a state dir reached through a link is still the state dir
        let roots: Vec<PathBuf> = self
            .env
            .deny_read
            .iter()
            .flat_map(|x| [Some(x.clone()), paths::real_of(x)])
            .flatten()
            .collect();
        for t in targets {
            let abs = paths::resolve(&self.env.cwd, &t);
            for c in [Some(paths::lexical_normalize(&abs)), paths::real_of(&abs)]
                .into_iter()
                .flatten()
            {
                // a target inside a denied path, or an ancestor a search would walk into it from
                let hit = roots.iter().any(|x| {
                    paths::within(x, &c, policy)
                        || (self.env.deny_secret_reads && paths::within(&c, x, policy))
                });
                if hit || (self.env.deny_secret_reads && secret_shaped(&c)) {
                    return Some(WHY);
                }
            }
        }
        None
    }

    /// ANY rule matches (deny/ask). Bash: ANY sub-command against ANY rule.
    fn some_match(&self, rules: &[String], tool: &str, input: &Value) -> bool {
        if tool == "Bash" {
            return bash::bash_any_match(rules, command_of(input));
        }
        rules
            .iter()
            .any(|r| self.matches_rule(r, tool, input, true))
    }

    /// Allow: Bash needs EVERY sub-command covered by SOME rule.
    fn allow_match(&self, rules: &[String], tool: &str, input: &Value) -> bool {
        if tool == "Bash" {
            return bash::bash_all_match(rules, command_of(input));
        }
        rules
            .iter()
            .any(|r| self.matches_rule(r, tool, input, false))
    }

    /// Allow rules in force: while in auto the broad ones are left out (suspended, not deleted).
    fn allow_rules(&self, mode: Mode) -> Vec<String> {
        let all = self.env.auto_mode.classify_all_shell;
        let suspend = self.classifies(mode);
        self.rules
            .allow
            .iter()
            .filter(|r| !(suspend && is_broad_allow(r, all)))
            // `mcp__*` would allow every MCP tool, destructive ones included: never as an allow
            .filter(|r| bash::parse_rule(r).0 != "mcp__*")
            .cloned()
            .collect()
    }

    pub fn suspended_rules(&self, mode: Mode) -> Vec<String> {
        let all = self.env.auto_mode.classify_all_shell;
        if !self.classifies(mode) {
            return Vec::new();
        }
        self.rules
            .allow
            .iter()
            .filter(|r| is_broad_allow(r, all))
            .cloned()
            .collect()
    }

    // --- workspace scope -----------------------------------------------------------------

    fn roots(&self) -> Vec<PathBuf> {
        std::iter::once(&self.env.anchor().to_path_buf())
            .chain(&self.rules.additional_directories)
            .map(|d| paths::resolve(&self.env.cwd, &d.to_string_lossy()))
            .collect()
    }

    /// Lexically under cwd/additional dirs (links not resolved).
    fn lex_inside(&self, p: &str) -> bool {
        if p.is_empty() {
            return false;
        }
        let abs = paths::resolve(&self.env.cwd, p);
        self.roots()
            .iter()
            .any(|r| paths::within(r, &abs, self.policy()))
    }

    /// Inside cwd/additional dirs lexically AND after resolving links.
    fn in_scope(&self, p: &Path) -> bool {
        let Some(rp) = paths::real_of(p) else {
            return false;
        };
        self.roots().iter().any(|r| {
            paths::within(r, p, self.policy())
                && !paths::win_unsafe(&paths::relative(r, p), self.policy())
                && paths::within(
                    &paths::real_of(r).unwrap_or_else(|| r.clone()),
                    &rp,
                    self.policy(),
                )
        })
    }

    /// The first path a Read/Grep/Glob call touches outside the working directories (links
    /// resolved); `None` when everything is inside.
    fn outside_read(&self, tool: &str, input: &Value) -> Option<OutsideRead> {
        if !OUTSIDE_READ_TOOLS.contains(&tool) {
            return None;
        }
        let policy = self.policy();
        let mut raw: Vec<String> = Vec::new();
        let field = if tool == "Read" { "file_path" } else { "path" };
        raw.extend(input.get(field).and_then(Value::as_str).map(str::to_string));
        if tool == "Glob"
            && let Some(pat) = non_empty(input, "pattern")
        {
            let base = paths::resolve(&self.env.cwd, non_empty(input, "path").unwrap_or("."));
            raw.push(paths::glob_static_root(pat, &base, policy));
        }
        let mut outside = None;
        for r in raw.iter().filter(|r| !r.is_empty()) {
            if paths::is_network_path(r) {
                return Some(OutsideRead {
                    path: PathBuf::from(r),
                    network: true,
                });
            }
            let abs = paths::resolve(&self.env.cwd, r);
            if outside.is_none() && !self.in_scope(&abs) {
                outside = Some(OutsideRead {
                    path: abs,
                    network: false,
                });
            }
        }
        outside
    }

    fn path_inside_allowed(&self, tool: &str, input: &Value) -> bool {
        path_of(tool, input).is_some_and(|fp| self.in_scope(&paths::resolve(&self.env.cwd, fp)))
    }

    // --- protected and critical paths ------------------------------------------------------

    /// Segments are taken relative to the workspace root when inside one, so a project that
    /// itself lives under e.g. ~/.idea is not entirely protected.
    fn is_protected(&self, p: &Path) -> bool {
        let policy = self.policy();
        let roots: Vec<PathBuf> = self
            .roots()
            .into_iter()
            .flat_map(|r| [Some(r.clone()), paths::real_of(&r)])
            .flatten()
            .collect();
        [Some(paths::lexical_normalize(p)), paths::real_of(p)]
            .into_iter()
            .flatten()
            .any(|c| {
                if self
                    .env
                    .protected
                    .iter()
                    .any(|x| paths::within(x, &c, policy))
                {
                    return true;
                }
                let mut root: Option<&PathBuf> = None;
                for r in roots.iter().filter(|r| paths::within(r, &c, policy)) {
                    if root.is_none_or(|b| r.as_os_str().len() > b.as_os_str().len()) {
                        root = Some(r);
                    }
                }
                let s = match root {
                    Some(r) => paths::relative(r, &c),
                    None => c.to_string_lossy().into_owned(),
                };
                if paths::win_unsafe(&s, policy) {
                    return true;
                }
                // Splits on `\` on POSIX too: over-protecting is the safe side here.
                let segs: Vec<String> = s
                    .split(['\\', '/'])
                    .filter(|x| !x.is_empty())
                    .map(|x| paths::fold(x, policy))
                    .collect();
                let next = |i: usize, n: &str| segs.get(i + 1).is_some_and(|x| x == n);
                segs.iter().enumerate().any(|(i, s)| {
                    PROTECTED_DIRS.contains(&s.as_str())
                        || (s == ".config" && next(i, "git"))
                        || (s == ".github" && next(i, "workflows"))
                }) || segs
                    .last()
                    .is_some_and(|l| PROTECTED_FILES.contains(&l.as_str()) || l.starts_with(".env"))
            })
    }

    /// Per-mode answer for a write to a protected path. `Classify` = ask the auto mode classifier.
    pub fn protected_path_decision(&self, mode: Mode) -> Verdict {
        if self.classifies(mode) {
            return Verdict::new(Decision::Classify, "protected path: classifier review");
        }
        match mode {
            Mode::DontAsk => Verdict::new(Decision::Deny, "dontAsk mode: protected path"),
            Mode::BypassPermissions => Verdict::new(Decision::Allow, "bypassPermissions mode"),
            _ => {
                let mut v = Verdict::new(Decision::Ask, "protected path requires confirmation");
                v.hard = true;
                v
            }
        }
    }

    /// `abs` is the directory part of an rm/rmdir target; `glob` = the target had a wildcard.
    fn is_critical(&self, abs: &Path, glob: bool) -> bool {
        let policy = self.policy();
        let n = |p: &Path| paths::nrm(p, policy);
        let root = fs_root(abs);
        let homes: Vec<PathBuf> = [Some(self.env.home.clone()), paths::real_of(&self.env.home)]
            .into_iter()
            .flatten()
            .collect();
        let roots = self.roots();
        [Some(abs.to_path_buf()), paths::real_of(abs)]
            .into_iter()
            .flatten()
            .any(|c| {
                if n(&c) == n(&root) || n(c.parent().unwrap_or(&c)) == n(&root) {
                    return true;
                }
                if policy.windows_separators && win_top(&c.to_string_lossy()) {
                    return true;
                }
                if homes.iter().any(|h| n(h) == n(&c)) {
                    return true;
                }
                if !glob {
                    return paths::within(&c, &self.env.cwd, policy);
                }
                roots
                    .iter()
                    .any(|r| paths::within(&c, r, policy) && n(&c) != n(r))
            })
    }

    fn critical_target(&self, t: &str, base: Option<&Path>) -> bool {
        // a bare glob (`*`, `.*`, `*/`, `*/*`) empties the directory the command runs in
        if bash::bare_glob(t) || bash::unknown_expansion(t) {
            return true;
        }
        base.and_then(|b| self.resolve_operand(t, b))
            .is_some_and(|(p, glob)| self.is_critical(&p, glob))
    }

    /// rm/rmdir aimed at the root, home, the working directory or its parents, also inside
    /// (...), { ...; }, bash -c '...' and eval.
    fn critical_bash(&self, cmd: &str, depth: u32) -> bool {
        let mut base: Option<PathBuf> = Some(self.env.cwd.clone());
        for seg in bash::lenient_segments(cmd) {
            let w = bash::unwrap_words(&seg, true);
            let name = paths::basename(w.first().map_or("", String::as_str), self.policy());
            if name == "cd" {
                let r = if w.len() == 2 {
                    base.as_deref().and_then(|b| self.resolve_operand(&w[1], b))
                } else {
                    None
                };
                base = r.map(|(p, _)| p);
            } else if bash::SHELLS.contains(&name) {
                // `-c` possibly combined with other short flags
                let i = w
                    .iter()
                    .enumerate()
                    .position(|(k, x)| k > 0 && bash::is_dash_c(x));
                if let Some(i) = i
                    && w.get(i + 1).is_some_and(|s| !s.is_empty())
                    && depth < 3
                    && self.critical_bash(&w[i + 1], depth + 1)
                {
                    return true;
                }
            } else if name == "eval" {
                if depth < 3 && self.critical_bash(&w[1..].join(" "), depth + 1) {
                    return true;
                }
            } else if name == "rm" || name == "rmdir" {
                let dd = w.iter().position(|x| x == "--");
                let targets = w[1..]
                    .iter()
                    .enumerate()
                    .filter(|(k, x)| dd.is_some_and(|d| k + 1 > d) || !x.starts_with('-'));
                if targets
                    .into_iter()
                    .any(|(_, t)| self.critical_target(t, base.as_deref()))
                {
                    return true;
                }
            }
        }
        false
    }

    /// `synth memory <subcommand>` that changes long-term memory, also inside (...), `bash -c` and
    /// eval. Memory is the person's: the agent may read it, never approve, delete, import or opt out.
    fn memory_mutation(&self, cmd: &str, depth: u32) -> bool {
        const MUTATING: [&str; 11] = [
            "add", "delete", "forget", "approve", "reject", "apply", "import", "optout", "prune",
            "promote", "reflect",
        ];
        for seg in bash::lenient_segments(cmd) {
            let w = bash::unwrap_words(&seg, true);
            let name = paths::basename(w.first().map_or("", String::as_str), self.policy());
            if bash::SHELLS.contains(&name) {
                let i = w
                    .iter()
                    .enumerate()
                    .position(|(k, x)| k > 0 && bash::is_dash_c(x));
                if let Some(i) = i
                    && depth < 3
                    && w.get(i + 1)
                        .is_some_and(|s| self.memory_mutation(s, depth + 1))
                {
                    return true;
                }
            } else if name == "eval"
                && depth < 3
                && self.memory_mutation(&w[1..].join(" "), depth + 1)
            {
                return true;
            }
            let mut rest = w.iter().skip_while(|x| {
                let b = paths::basename(x, self.policy());
                b != "synth" && b != "synth.exe"
            });
            if rest.next().is_some() {
                let mut args = rest.filter(|x| !x.starts_with('-'));
                if args.next().is_some_and(|x| x == "memory")
                    && args.next().is_some_and(|x| MUTATING.contains(&x.as_str()))
                {
                    return true;
                }
            }
        }
        false
    }

    /// A static path from a command word: (the file, or the directory before the first wildcard;
    /// had-wildcard). `None` when it cannot be known without running the shell ($ expansions,
    /// braces, ~user) or a wildcard could hit dotfiles.
    fn resolve_operand(&self, op: &str, base: &Path) -> Option<(PathBuf, bool)> {
        if op.contains(['$', '`', '{'])
            || (op.starts_with('~')
                && op[1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c != '/' && c != '\\'))
        {
            return None;
        }
        let owned;
        let op = if op == "~" || op.starts_with("~/") || op.starts_with("~\\") {
            owned = format!("{}{}", self.env.home.to_string_lossy(), &op[1..]);
            owned.as_str()
        } else {
            op
        };
        let g = op.find(['*', '?', '[']);
        // a wildcard may match any directory, so `..` after it can climb out of the static prefix
        if g.is_some() && op.split(['/', '\\']).any(|c| c == "..") {
            return None;
        }
        let dir = match g {
            None => op,
            Some(g) => op[..g].rfind(['/', '\\']).map_or("", |i| &op[..=i]),
        };
        if g.is_some() && op[dir.len()..].starts_with('.') {
            return None;
        }
        Some((
            paths::resolve(base, if dir.is_empty() { "." } else { dir }),
            g.is_some(),
        ))
    }

    /// One simple command that is read-only (`auto`: or build/test) AND whose every non-flag
    /// operand resolves, from `base`, inside the workspace.
    fn seg_safe(&self, w: &[String], base: Option<&Path>, auto: bool) -> bool {
        let ok = if auto {
            bash::is_auto_safe_simple(w)
        } else {
            bash::is_read_only_simple(w)
        };
        // echo only prints: its words are text, not paths
        ok && (w[0] == "echo"
            || w[1..].iter().filter(|a| !a.starts_with('-')).all(|a| {
                base.and_then(|b| self.resolve_operand(a, b))
                    .is_some_and(|(p, _)| self.in_scope(&p))
            }))
    }

    /// Working directory after a simple command (`cd` moves it).
    fn after_cd(&self, seg: &[String], base: Option<PathBuf>) -> Option<PathBuf> {
        if seg[0] != "cd" {
            return base;
        }
        match seg.len() {
            2 => base
                .as_deref()
                .and_then(|b| self.resolve_operand(&seg[1], b))
                .map(|(p, _)| p),
            _ => Some(self.env.home.clone()),
        }
    }

    /// Every segment passes `seg_safe` (tracking `cd`); the scoped `is_read_only_bash` /
    /// `is_auto_safe_bash`.
    fn bash_safe(&self, cmd: &str, auto: bool) -> bool {
        let Some(segs) = bash::parse_simple_commands(cmd).filter(|s| !s.is_empty()) else {
            return false;
        };
        let mut base = Some(self.env.cwd.clone());
        for seg in segs {
            if !self.seg_safe(&seg, base.as_deref(), auto) {
                return false;
            }
            base = self.after_cd(&seg, base);
        }
        true
    }

    /// Does the command write to a protected path: redirect targets (`> .git/x`, `>> ~/.bashrc`)
    /// and the operands of fs/writer commands, found with the lenient splitter so it also works
    /// when the strict parse fails. A command that cannot be parsed counts as a hit.
    fn bash_hits_protected(&self, cmd: &str) -> bool {
        const WRITERS: &[&str] = &["tee", "install", "ln", "truncate", "chmod", "chown", "dd"];
        let Some(segs) = bash::deep_segments(cmd, 0) else {
            return true;
        };
        let mut base = self.env.cwd.clone();
        for seg in segs {
            let w = bash::unwrap_words(&seg, true);
            let name = paths::basename(w.first().map_or("", String::as_str), self.policy());
            let mut ops = bash::redirect_targets(&seg);
            if name == "cd" {
                if let Some((p, _)) = w.get(1).and_then(|d| self.resolve_operand(d, &base)) {
                    base = p;
                }
            } else if bash::FS_CMDS.contains(&name) {
                ops.extend(bash::fs_operands(name, &w[1..]).unwrap_or_else(|| {
                    w[1..]
                        .iter()
                        .filter(|a| !a.starts_with('-'))
                        .cloned()
                        .collect()
                }));
            } else if WRITERS.contains(&name) {
                ops.extend(
                    w[1..]
                        .iter()
                        .filter(|a| !a.starts_with('-'))
                        .map(|a| a.strip_prefix("of=").unwrap_or(a).to_string()),
                );
            }
            if ops.iter().any(|o| {
                self.resolve_operand(o, &base)
                    .is_some_and(|(p, _)| self.is_protected(&p))
            }) {
                return true;
            }
        }
        false
    }

    /// Does the command read a path a `Read(..)` deny rule protects: operands of the commands that
    /// print or scan files and `<` redirects, found with the lenient splitter. Operands that need
    /// the shell to be known (`$VAR`, braces) cannot be checked and are left to the other rules.
    fn bash_reads_denied(&self, cmd: &str) -> bool {
        const READERS: &[&str] = &[
            "cat", "head", "tail", "less", "more", "grep", "egrep", "fgrep", "rg", "sed", "awk",
            "gawk", "nl", "tac", "od", "xxd", "hexdump", "strings", "cut", "sort", "uniq", "wc",
            "diff", "cmp", "bat", "cp", "mv", "tee", "source", ".", "base64", "paste", "fold",
        ];
        let specs: Vec<&str> = self
            .rules
            .deny
            .iter()
            .filter_map(|r| {
                let (rt, spec) = bash::parse_rule(r);
                spec.filter(|_| rt == "Read")
            })
            .collect();
        if specs.is_empty() {
            return false;
        }
        let Some(segs) = bash::deep_segments(cmd, 0) else {
            return false;
        };
        let mut base = self.env.cwd.clone();
        for seg in segs {
            let w = bash::unwrap_words(&seg, true);
            let name = paths::basename(w.first().map_or("", String::as_str), self.policy());
            if name == "cd" {
                if let Some((p, _)) = w.get(1).and_then(|d| self.resolve_operand(d, &base)) {
                    base = p;
                }
                continue;
            }
            // `< file`, `<file`, `cat<file` (here-documents and `<(..)` read no file)
            let mut ops: Vec<String> = Vec::new();
            for (i, x) in seg.iter().enumerate() {
                if let Some(k) = x.rfind('<').filter(|k| !x[..=*k].ends_with("<<")) {
                    match &x[k + 1..] {
                        "" => ops.extend(seg.get(i + 1).cloned()),
                        r if !r.starts_with('(') => ops.push(r.to_string()),
                        _ => {}
                    }
                }
            }
            if READERS.contains(&name) {
                ops.extend(w[1..].iter().filter(|a| !a.starts_with('-')).cloned());
            }
            for o in ops {
                let Some((p, wild)) = self.resolve_operand(&o, &base) else {
                    continue;
                };
                // a wildcard resolves to its directory: test a file in it too
                let hit = |p: &Path| {
                    specs.iter().any(|sp| {
                        paths::match_path_rule_real(
                            sp,
                            &p.to_string_lossy(),
                            &self.env.cwd,
                            &self.env.home,
                            self.policy(),
                            true,
                        )
                    })
                };
                if hit(&p) || (wild && hit(&p.join("x"))) {
                    return true;
                }
            }
        }
        false
    }

    /// acceptEdits view of a Bash command: `ok` = every segment is allowed by `seg_safe` or is an
    /// fs command whose operands are all inside the workspace; `targets` = the resolved fs
    /// operands (collected even when `ok` is false).
    fn bash_fs(&self, cmd: &str, auto: bool) -> BashFs {
        let segs = bash::parse_simple_commands(cmd);
        let mut out = BashFs {
            ok: segs.as_ref().is_some_and(|s| !s.is_empty()),
            targets: Vec::new(),
        };
        let mut base: Option<PathBuf> = Some(self.env.cwd.clone());
        for seg in segs.unwrap_or_default() {
            let w = bash::unwrap_words(&seg, false);
            let name = w.first().map_or("", String::as_str);
            if name == "cd" {
                base = match w.len() {
                    1 => Some(self.env.home.clone()),
                    2 => base
                        .as_deref()
                        .and_then(|b| self.resolve_operand(&w[1], b))
                        .map(|(p, _)| p),
                    _ => None,
                };
                continue;
            }
            if !bash::FS_CMDS.contains(&name) {
                if !self.seg_safe(&w, base.as_deref(), auto) {
                    out.ok = false;
                }
                continue;
            }
            let Some(ops) = bash::fs_operands(name, &w[1..]) else {
                out.ok = false;
                continue;
            };
            for op in ops {
                let Some((p, _)) = base.as_deref().and_then(|b| self.resolve_operand(&op, b))
                else {
                    out.ok = false;
                    continue;
                };
                if (name == "rm" || name == "rmdir") && ends_with_star_segment(&op) {
                    out.ok = false;
                }
                if !self.in_scope(&p) {
                    out.ok = false;
                }
                out.targets.push(p);
            }
        }
        out
    }

    // --- the decision --------------------------------------------------------------------

    /// Decides one tool call under the engine's current mode.
    pub fn check_current(&self, tool: &str, input: &Value) -> Verdict {
        self.check(tool, input, self.env.mode)
    }

    /// Re-runs the policy on input rewritten by a PreToolUse hook or an approver (`updatedInput`).
    /// The earlier verdict and any approval of the old input carry no weight: `deny` means the
    /// call must not run, `ask` means ask again. Use the mode the turn was sent in.
    pub fn recheck(&self, tool: &str, rewritten_input: &Value, mode: Mode) -> Verdict {
        self.check(tool, rewritten_input, mode)
    }

    /// Decides one tool call under `mode` (the mode the turn was sent in, so a later mode switch
    /// cannot change it). Non-interactive environments never return `Ask`.
    pub fn check(&self, tool: &str, input: &Value, mode: Mode) -> Verdict {
        let mut v = self.decide(tool, input, mode);
        if v.decision == Decision::Ask && !self.env.interactive {
            v.decision = Decision::Deny;
            v.reason = format!("non-interactive: no one to approve ({})", v.reason);
        }
        let json = input.to_string();
        if json.chars().count() > INPUT_DISPLAY_LIMIT {
            use sha2::{Digest, Sha256};
            let digest: String = Sha256::digest(json.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            v.reason = format!(
                "{} [input is {} chars; the dialog shows the first {INPUT_DISPLAY_LIMIT}; sha256 {}]",
                v.reason,
                json.chars().count(),
                &digest[..16]
            );
            v.full_input_digest = Some(digest);
        }
        v
    }

    /// Verdict for a shell command the USER typed (`!cmd`): explicit intent, so a plain `ask`
    /// (including an ask rule) becomes `allow`, whatever the mode or interactivity. Deny rules,
    /// protected paths and critical removals still block (`Deny`, or a `hard` `Ask`).
    pub fn check_user_command(&self, command: &str) -> Verdict {
        let mut v = self.decide(
            "Bash",
            &serde_json::json!({ "command": command }),
            Mode::Default,
        );
        if matches!(v.decision, Decision::Ask | Decision::Classify) && !v.hard {
            v.decision = Decision::Allow;
            v.reason = format!("typed by the user ({})", v.reason);
        }
        v
    }

    fn decide(&self, tool: &str, input: &Value, mode: Mode) -> Verdict {
        // tools that run a model-supplied shell command get the Bash verdict for that command
        // (deny/ask/allow rules, critical_bash, protected paths, strict parse); only a tool-name
        // deny rule or ask rule can add to it
        if COMMAND_TOOLS.contains(&tool)
            && let Some(cmd) = input.get("command").and_then(Value::as_str)
        {
            let own = self.decide_tool(tool, input, mode);
            if own.decision == Decision::Deny || (own.decision == Decision::Ask && own.rule) {
                return own;
            }
            return self.decide_tool("Bash", &serde_json::json!({ "command": cmd }), mode);
        }
        self.decide_tool(tool, input, mode)
    }

    fn decide_tool(&self, tool: &str, input: &Value, mode: Mode) -> Verdict {
        use Decision::*;
        fn v(d: Decision, r: impl Into<String>) -> Verdict {
            Verdict::new(d, r)
        }
        // deny: ANY sub-command/match triggers deny.
        if self.some_match(&self.rules.deny, tool, input) {
            return v(Deny, "blocked by a deny rule");
        }
        if let Some(why) = self.hard_read_denied(tool, input) {
            return v(Deny, why);
        }
        if tool == "Bash" && self.bash_reads_denied(command_of(input)) {
            return v(
                Deny,
                "blocked by a Read deny rule (the command reads that path)",
            );
        }
        // a recursive Grep/Glob would read what a `Read(..)` deny rule protects: never silent
        if let Some(rule) = self.recursive_read_hit(tool, input) {
            if mode == Mode::DontAsk {
                return v(
                    Deny,
                    format!("dontAsk mode: search covers paths denied by {rule}"),
                );
            }
            return v(
                Ask,
                format!("search covers paths denied by {rule}; narrow `path`"),
            );
        }
        // Dialogs to the user, not real tool actions: ask in every mode, bypass and dontAsk included.
        if DIALOG_TOOLS.contains(&tool) {
            return v(Ask, "dialog tool");
        }
        // which path the tool touches is unclear: never decided by a rule or a mode (not even bypass)
        if ambiguous_paths(tool, input) {
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode: ambiguous path fields");
            }
            let mut r = v(Ask, "ambiguous path fields in tool input");
            r.hard = true;
            return r;
        }

        // Never auto-approved, whatever the mode or allow rules say.
        let bash_cmd = if tool == "Bash" {
            command_of(input)
        } else {
            ""
        };
        let auto = mode == Mode::Auto;
        let classify = self.classifies(mode);
        if tool == "Bash" && self.critical_bash(bash_cmd, 0) {
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode: critical path");
            }
            // critical: in auto the agent asks with a countdown instead of a plain prompt
            let mut r = v(
                Ask,
                "removes a critical path (root, home, working directory or a parent)",
            );
            r.critical = auto || classify;
            r.hard = true;
            return r;
        }

        if tool == "Bash" && self.memory_mutation(bash_cmd, 0) {
            return v(
                Deny,
                "changes long-term memory: only the person may run `synth memory` approve, apply, delete, import, optout or similar",
            );
        }

        // Read/Grep/Glob outside the working directories
        let outside = self.outside_read(tool, input);
        if outside.is_some() && self.block_outside_reads {
            let mut r = v(Deny, "reads outside the working directories are blocked");
            r.message = Some(OUTSIDE_READ_BLOCKED);
            return r;
        }
        if outside.as_ref().is_some_and(|o| o.network) {
            // prompts in every mode (even with an allow rule or bypassPermissions), never auto-approved
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode: network path");
            }
            return v(Ask, "read from a network path");
        }

        let asked = self.some_match(&self.rules.ask, tool, input);

        // an ask rule is a tripwire the user set on purpose: it prompts in every mode but dontAsk
        if mode == Mode::BypassPermissions && !asked {
            return v(Allow, "bypassPermissions mode");
        }

        if asked {
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode: ask rule becomes deny");
            }
            let mut r = v(Ask, "matched an ask rule");
            r.rule = true;
            return r;
        }

        if mode == Mode::Plan {
            if EDIT_TOOLS.contains(&tool) {
                return v(Deny, "plan mode: read-only");
            }
            if tool == "Bash" && self.bash_safe(bash_cmd, false) {
                return v(Allow, "plan mode: read-only command");
            }
        }

        // protected paths: checked before allow rules, so neither a mode nor a rule approves them
        let fs_info = (tool == "Bash").then(|| self.bash_fs(bash_cmd, auto));
        let fp = if EDIT_TOOLS.contains(&tool) {
            path_of(tool, input)
        } else {
            None
        };
        if fp.is_some_and(|f| self.is_protected(&paths::resolve(&self.env.cwd, f)))
            || fs_info
                .as_ref()
                .is_some_and(|i| i.targets.iter().any(|t| self.is_protected(t)))
            || (tool == "Bash" && self.bash_hits_protected(bash_cmd))
        {
            return self.protected_path_decision(mode);
        }

        // explicit allow rules always win once we're past deny/ask.
        if self.allow_match(&self.allow_rules(mode), tool, input) {
            return v(Allow, "matched an allow rule");
        }

        // auto: the first read outside the working directories is the user's call
        if let Some(o) = &outside
            && classify
            && !self.outside_reads_answered
        {
            let mut r = v(Ask, "read outside the working directories");
            r.outside_read = Some(o.path.clone());
            return r;
        }

        if READ_ONLY_TOOLS.contains(&tool) {
            return v(Allow, "read-only tool");
        }

        // A message can wake another agent and steer it: not read-only, never in plan/dontAsk.
        if tool == "SendMessage" {
            return match mode {
                Mode::Plan | Mode::DontAsk => v(Deny, format!("{} mode", mode.as_str())),
                Mode::AcceptEdits | Mode::Auto => v(Allow, format!("{} mode", mode.as_str())),
                _ => v(Ask, "SendMessage requires confirmation"),
            };
        }

        // both send text (a URL, a query) to a third party, so they ask first
        if tool == "WebFetch" || tool == "WebSearch" {
            if auto {
                return v(Classify, format!("{tool}: classifier review"));
            }
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode");
            }
            return v(Ask, format!("{tool} requires confirmation"));
        }

        if EDIT_TOOLS.contains(&tool) {
            if matches!(mode, Mode::AcceptEdits | Mode::Auto)
                && self.path_inside_allowed(tool, input)
            {
                return v(
                    Allow,
                    format!("{} mode: path inside workspace", mode.as_str()),
                );
            }
            if auto {
                // inside by name but not after resolving links: the user decides, not the classifier
                if fp.is_some_and(|f| self.lex_inside(f)) {
                    return v(Ask, "path resolves outside the workspace (link)");
                }
                return v(Classify, "edit outside the workspace: classifier review");
            }
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode");
            }
            return v(Ask, "edit tool requires confirmation");
        }

        if let Some(fs_info) = fs_info {
            if self.bash_safe(bash_cmd, false) {
                return v(Allow, "read-only command");
            }
            if auto && self.bash_safe(bash_cmd, true) {
                return v(Allow, "auto mode: safe command");
            }
            // Official behaviour: a compound command is allowed when every part is, and a read-only
            // part (`| tail -15`, `echo ---`, `2>&1`) needs no rule of its own.
            {
                let mut base = Some(self.env.cwd.clone());
                let rules = self.allow_rules(mode);
                if bash::bash_all_match_or(&rules, bash_cmd, |seg| {
                    let ok = self.seg_safe(seg, base.as_deref(), false);
                    base = self.after_cd(seg, base.take());
                    ok
                }) {
                    return v(Allow, "matched an allow rule; other parts are read-only");
                }
            }
            if matches!(mode, Mode::AcceptEdits | Mode::Auto) && fs_info.ok {
                return v(
                    Allow,
                    format!("{} mode: file commands inside workspace", mode.as_str()),
                );
            }
            if auto {
                if fs_info
                    .targets
                    .iter()
                    .any(|t| self.lex_inside(&t.to_string_lossy()) && !self.in_scope(t))
                {
                    return v(Ask, "path resolves outside the workspace (link)");
                }
                return v(Classify, "Bash: classifier review");
            }
            if classify {
                return v(Classify, "plan mode: classifier review");
            }
            if mode == Mode::Plan {
                return v(Ask, "plan mode: only read-only commands run unprompted");
            }
            if mode == Mode::DontAsk {
                return v(Deny, "dontAsk mode");
            }
            return v(Ask, "Bash requires confirmation");
        }

        // unknown tool / mcp tool.
        if auto {
            return v(Classify, format!("{tool}: classifier review"));
        }
        if mode == Mode::DontAsk {
            return v(Deny, "dontAsk mode");
        }
        v(Ask, "unknown tool requires confirmation")
    }

    // --- suggestions ---------------------------------------------------------------------

    /// "Allow always" options offered with a prompt.
    pub fn suggestions(&self, tool: &str, input: &Value) -> Vec<Suggestion> {
        let add = |tool_name: &str, content: Option<String>| {
            vec![Suggestion::AddRules {
                rules: vec![RuleSpec {
                    tool_name: tool_name.to_string(),
                    rule_content: content,
                }],
                behavior: Decision::Allow,
            }]
        };
        if tool == "Bash" || COMMAND_TOOLS.contains(&tool) {
            let cmd = command_of(input).trim();
            // the rule is a Bash rule: verify_run/terminal_spawn commands are judged as Bash
            if tool != "Bash" {
                // `*` is allow-all and a `:*` tail is a prefix rule: never offer them
                return if exact_unsafe(cmd) {
                    vec![]
                } else {
                    add("Bash", Some(cmd.to_string()))
                };
            }
            // Compound commands: one prefix rule per simple subcommand (max 5), skipping the ones
            // that never ask (read-only) or are already allowed; exact-only ones (cd, rm, ...) get none.
            let segs = bash::parse_simple_commands(cmd).unwrap_or_default();
            if segs.len() > 1 {
                let mut rules: Vec<RuleSpec> = vec![];
                for seg in &segs {
                    let words: Vec<&str> = seg.iter().map(String::as_str).collect();
                    let text = words.join(" ");
                    let skip = bash::is_read_only_simple(seg)
                        || bash::bash_all_match(&self.rules.allow, &text);
                    let rule = bash_rule(&words, &text, false).filter(|_| !skip);
                    if let Some(c) =
                        rule.filter(|c| !rules.iter().any(|r| r.rule_content.as_deref() == Some(c)))
                    {
                        rules.push(RuleSpec {
                            tool_name: "Bash".into(),
                            rule_content: Some(c),
                        });
                    }
                }
                rules.truncate(5);
                return if rules.is_empty() {
                    vec![]
                } else {
                    vec![Suggestion::AddRules {
                        rules,
                        behavior: Decision::Allow,
                    }]
                };
            }
            let words: Vec<&str> = cmd.split_whitespace().collect();
            return bash_rule(&words, cmd, true).map_or(vec![], |c| add("Bash", Some(c)));
        }
        if EDIT_TOOLS.contains(&tool) {
            return vec![Suggestion::SetMode(Mode::AcceptEdits)];
        }
        if tool == "WebFetch" {
            let host = url_host(input.get("url").and_then(Value::as_str).unwrap_or(""))
                .unwrap_or_default();
            return add("WebFetch", Some(format!("domain:{host}")));
        }
        if tool == "browser_navigate" {
            let host = url_host(input.get("url").and_then(Value::as_str).unwrap_or(""));
            return host.map_or(vec![], |h| add(tool, Some(format!("domain:{h}"))));
        }
        // no scope exists for these: a blanket allow would cover any command, URL or page action
        if tool.starts_with("browser_") {
            return vec![];
        }
        add(tool, None)
    }

    /// Applies accepted suggestions; returns the new mode when one was set.
    pub fn apply(&mut self, suggestions: &[Suggestion]) -> Option<Mode> {
        let mut mode = None;
        for s in suggestions {
            match s {
                Suggestion::SetMode(m) => {
                    mode = Some(*m);
                    self.env.mode = *m;
                }
                Suggestion::AddRules { rules, behavior } => {
                    for r in rules {
                        let rule = match r.rule_content.as_deref().filter(|c| !c.is_empty()) {
                            Some(c) => format!("{}({c})", r.tool_name),
                            None => r.tool_name.clone(),
                        };
                        let bucket = match behavior {
                            Decision::Deny => &mut self.rules.deny,
                            Decision::Ask => &mut self.rules.ask,
                            _ => &mut self.rules.allow,
                        };
                        if !bucket.contains(&rule) {
                            bucket.push(rule);
                        }
                    }
                }
            }
        }
        mode
    }
}

/// `path.parse(p).root`.
fn fs_root(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut r = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => r.push(c.as_os_str()),
            _ => break,
        }
    }
    r
}

/// `C:\` and `C:\Windows`: a drive root or its direct children.
fn win_top(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == ':'
        && matches!(b[2], '/' | '\\')
        && !b[3..].iter().any(|c| matches!(c, '/' | '\\'))
}

/// `/[\\/]\*[\\/]?$/`
fn ends_with_star_segment(op: &str) -> bool {
    let t = op.strip_suffix(['/', '\\']).unwrap_or(op);
    t.strip_suffix('*')
        .is_some_and(|r| r.ends_with(['/', '\\']))
}

#[cfg(test)]
mod tests;
