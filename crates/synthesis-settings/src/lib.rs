//! `settings.json` loader (port of Node `claudecfg.js` `mergeSettings`, `loadSynthesisEnv`,
//! `loadAutoModeConfig`, `loadBlockOutsideReads`, `findProjectRoot`; docs/specs/config.md).
//!
//! Layers, lowest to highest: `<synthesis_home>/settings.json` (user), `<project_root>/.synthesis/settings.json`
//! (project), `<project_root>/.synthesis/settings.local.json` (local, gitignored, where "allow always"
//! persists; same trust rules as project), then the managed file ([`managed_settings_path`]; the
//! organisation's, always trusted). The CLI flags sit between local and managed: precedence is
//! managed > CLI > local > project > user. Lists are unions (deduped, order kept); scalars: last layer wins. Unknown keys are
//! ignored. A file that exists but is malformed, not an object or unreadable is an ERROR (`Settings::errors`, also in `diagnostics`): its deny rules cannot apply, so the CLI refuses to start (owner decision D5, as Claude Code's Settings Error). Node only warned and went on without it. Node has no local or managed layer. Because lists are unions, a managed `deny`/`ask` rule can never be removed or overridden by a lower layer, the CLI or an allow rule; a managed `defaultMode` ([`Settings::managed_default_mode`]) beats `--permission-mode`.
//!
//! SECURITY (deviation from Node): the project file comes from a possibly hostile repository.
//! Unless the caller passes [`ProjectTrust::Trusted`], it can only TIGHTEN: `deny`/`ask` rules, the
//! modes `default|manual|plan|dontAsk` and `mcp` disables are kept; `allow` rules, hooks,
//! `additionalDirectories`, `disableAllHooks`, `env`, MCP approvals and the `synth` section are
//! dropped with a diagnostic (counts and key names only, never values). `auto` and
//! `bypassPermissions` are refused from the project even when trusted (as Node).
//!
//! Not ported: Claude Code's `~/.claude/settings.json` layer (needs `CLAUDE_CONFIG_DIR`, which Rust
//! does not read; config.md §4), `source` tags on hook entries.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use synthesis_hooks::{HookGroup, HooksConfig};

pub use synthesis_config::{LoadContext, ProjectTrust};

/// Highest `synthesis.version` this build understands; a newer section is ignored with a diagnostic.
pub const SYNTHESIS_SECTION_VERSION: u64 = 1;
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Modes an untrusted project may select: none of them allows more than the default.
const UNTRUSTED_PROJECT_MODES: [&str; 4] = ["default", "manual", "plan", "dontAsk"];
const PROJECT_FORBIDDEN_MODES: [&str; 2] = ["auto", "bypassPermissions"];

/// Plain permission inputs; the harness converts these to its permission engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    pub additional_directories: Vec<String>,
    /// Raw `defaultMode` string, `"default"` when unset (mode names are interpreted downstream).
    pub default_mode: String,
    /// A project `defaultMode` that was refused (Node `ignoredDefaultMode`).
    pub ignored_default_mode: Option<String>,
    /// User-only.
    pub block_reads_outside_working_directories: bool,
}

impl Default for PermissionRules {
    fn default() -> Self {
        Self {
            allow: vec![],
            ask: vec![],
            deny: vec![],
            additional_directories: vec![],
            default_mode: "default".into(),
            ignored_default_mode: None,
            block_reads_outside_working_directories: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpApproval {
    pub enabled_mcpjson_servers: Vec<String>,
    pub disabled_mcpjson_servers: Vec<String>,
    pub enable_all_project_mcp_servers: bool,
}

/// User-only (`autoMode`): steers the classifier that approves without asking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoModeConfig {
    pub environment: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub soft_deny: Option<Vec<String>>,
    pub hard_deny: Option<Vec<String>>,
    pub classify_all_shell: Option<bool>,
    pub use_auto_mode_during_plan: Option<bool>,
    /// Model that judges auto-mode calls (a model of the session's provider); `None` = the session model.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_turns: Option<u64>,
    pub max_tool_calls: Option<u64>,
    pub max_verify_attempts: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compat {
    pub write_node_transcripts: Option<bool>,
}

/// The Rust-only top-level `"synthesis"` object (config.md §6). Every field optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SynthesisSettings {
    pub budget: Budget,
    /// Verification policy keys, shallow-merged per key; interpreted by `synthesis-verification`.
    pub verification: Map<String, Value>,
    /// `peers` (messaging.md B8.5), user settings only; interpreted by `synthesis-peers`.
    pub peers: Map<String, Value>,
    pub compat: Compat,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub project_root: PathBuf,
    pub permissions: PermissionRules,
    /// `permissions.defaultMode` of the managed file: overrides the CLI flag and every other layer.
    pub managed_default_mode: Option<String>,
    pub hooks: HooksConfig,
    pub disable_all_hooks: bool,
    pub mcp_approval: McpApproval,
    /// settings.json `env` block (string/number/bool values), secret-shaped names refused.
    pub env: BTreeMap<String, String>,
    pub env_refused: Vec<String>,
    pub auto_mode: AutoModeConfig,
    pub synthesis: SynthesisSettings,
    /// Non-secret notes: malformed files, ignored/refused items. Never contains setting values.
    pub diagnostics: Vec<String>,
    /// A settings file that exists but cannot be used (malformed JSON, not an object, unreadable,
    /// oversized): its deny/ask rules are NOT in force, so callers must refuse to start (`synth run`
    /// exits 2) instead of running without them. Also in `diagnostics`.
    pub errors: Vec<String>,
}

/// Nearest ancestor of `cwd` (inclusive) holding `.synthesis` or `.git`, else `cwd`. Lexical walk from
/// the given path; no process cwd, no canonicalisation. The project root decides which settings
/// (hooks, MCP, agents) and which writable sandbox root apply, so a marker only counts when the
/// directory and the marker are owned by the current user (unix) and not world-writable, and the
/// directory is not a filesystem root or the temp dir or one of its ancestors (a planted
/// `/tmp/.git` must not capture every project below it). Otherwise the walk goes on.
pub fn find_project_root(cwd: &Path) -> PathBuf {
    let tmp = std::env::temp_dir();
    let tmp = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    cwd.ancestors()
        .filter(|d| !d.as_os_str().is_empty())
        .find(|d| {
            [".synthesis", ".git"]
                .iter()
                .any(|m| d.join(m).exists() && marker_trusted(d, &d.join(m), &tmp))
        })
        .unwrap_or(cwd)
        .to_path_buf()
}

fn marker_trusted(dir: &Path, marker: &Path, tmp: &Path) -> bool {
    if dir.parent().is_none() || tmp.starts_with(dir) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = rustix::process::geteuid().as_raw();
        [dir, marker].iter().all(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == me && m.mode() & 0o002 == 0)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = marker;
        true
    }
}

/// Node `isSecretEnvKey`: secret-looking substring or provider-ish prefix.
pub fn is_secret_env_key(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "COOKIE",
        "SESSION",
    ]
    .iter()
    .any(|s| up.contains(s))
        || [
            "ANTHROPIC_",
            "CLAUDE_CODE_",
            "OPENAI_",
            "AWS_",
            "AZURE_",
            "GOOGLE_",
            "VERTEX_",
            "SYNTHESIS_",
        ]
        .iter()
        .any(|p| key.starts_with(p))
}

/// OS-specific path of the managed (organisation) settings file. Not overridable by environment
/// variables: an override would let the user bypass it.
pub fn managed_settings_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        "/Library/Application Support/Synthesis/managed-settings.json".into()
    } else if cfg!(windows) {
        "C:\\ProgramData\\Synthesis\\managed-settings.json".into()
    } else {
        "/etc/synthesis/managed-settings.json".into()
    }
}

/// `<project_root>/.synthesis/settings.local.json`.
pub fn local_settings_path(project_root: &Path) -> PathBuf {
    project_root.join(".synthesis").join("settings.local.json")
}

pub fn load_settings(ctx: &LoadContext<'_>, trust: ProjectTrust) -> Settings {
    load_settings_with_managed(ctx, trust, &managed_settings_path())
}

/// [`load_settings`] with an explicit managed file (tests).
pub fn load_settings_with_managed(
    ctx: &LoadContext<'_>,
    trust: ProjectTrust,
    managed: &Path,
) -> Settings {
    load_settings_layers(ctx, trust, managed, None)
}

/// [`load_settings`] plus the `--settings <file>` layer: typed on the command line, so trusted;
/// it sits between local settings and the managed file (managed > flag > local > project > user).
pub fn load_settings_with_file(
    ctx: &LoadContext<'_>,
    trust: ProjectTrust,
    extra: Option<&Path>,
) -> Settings {
    load_settings_layers(ctx, trust, &managed_settings_path(), extra)
}

fn load_settings_layers(
    ctx: &LoadContext<'_>,
    trust: ProjectTrust,
    managed: &Path,
    extra: Option<&Path>,
) -> Settings {
    let mut project_root = find_project_root(ctx.cwd);
    if ctx.user_home.starts_with(&project_root) {
        project_root = ctx.cwd.to_path_buf(); // home (or an ancestor) is never a project
    }
    let mut s = Settings {
        project_root: project_root.clone(),
        ..Settings::default()
    };
    let t = trust == ProjectTrust::Trusted;
    // (path, label, user-level, trusted)
    let mut layers = vec![
        (
            ctx.synthesis_home.join("settings.json"),
            "user settings",
            true,
            true,
        ),
        (
            project_root.join(".synthesis").join("settings.json"),
            "project settings",
            false,
            t,
        ),
        (
            local_settings_path(&project_root),
            "local settings",
            false,
            t,
        ),
        (managed.to_path_buf(), "managed settings", true, true),
    ];
    if let Some(f) = extra {
        layers.insert(3, (f.to_path_buf(), "--settings file", true, true));
    }
    for (path, what, user, trusted) in &layers {
        match read_json(path, what) {
            Ok(Some(raw)) => {
                s.apply(&raw, what, *user, *trusted);
                if *what == "managed settings" {
                    s.managed_default_mode =
                        raw["permissions"]["defaultMode"].as_str().map(String::from);
                }
            }
            Ok(None) => {}
            Err(e) => {
                s.diagnostics.push(e.clone());
                s.errors.push(e);
            }
        }
    }
    s
}

/// Persists "allow always" rules into the local settings file (created with its own
/// `.synthesis/.gitignore` entry). Existing keys are kept; rules are appended without duplicates.
pub fn persist_allow_rules(project_root: &Path, rules: &[String]) -> std::io::Result<()> {
    let path = local_settings_path(project_root);
    let dir = path.parent().unwrap_or(project_root);
    std::fs::create_dir_all(dir)?;
    let mut v = match read_json(&path, "local settings") {
        Ok(Some(v)) => v,
        Ok(None) => Value::Object(Map::new()),
        Err(e) => return Err(std::io::Error::other(e)), // never overwrite a file we cannot parse
    };
    let allow = v
        .as_object_mut()
        .map(|o| {
            o.entry("permissions")
                .or_insert_with(|| Value::Object(Map::new()))
        })
        .and_then(Value::as_object_mut)
        .map(|p| p.entry("allow").or_insert_with(|| Value::Array(vec![])))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| std::io::Error::other("local settings: permissions.allow is not a list"))?;
    for r in rules {
        if !allow.iter().any(|x| x.as_str() == Some(r)) {
            allow.push(Value::String(r.clone()));
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
    let gi = dir.join(".gitignore");
    let cur = std::fs::read_to_string(&gi).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == "settings.local.json") {
        let sep = if cur.is_empty() || cur.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        std::fs::write(&gi, format!("{cur}{sep}settings.local.json\n"))?;
    }
    Ok(())
}

/// Read-modify-write of one JSON file: the file is read again first (edits made meanwhile
/// survive), `edit` changes it (a missing file starts as `{}`), every other key and the key order
/// are kept. A file that cannot be parsed is never overwritten (`InvalidData`). The write goes
/// through a temp file and a rename, with the permissions of the file it replaces. An `Err` from
/// `edit` writes nothing, so `edit` is also where the caller validates the result.
pub fn edit_json_atomic(
    path: &Path,
    what: &str,
    edit: impl FnOnce(&mut Value) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut v = match read_json(path, what) {
        Ok(Some(v)) => v,
        Ok(None) => Value::Object(Map::new()),
        Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    };
    edit(&mut v)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
    let done = std::fs::metadata(path)
        .and_then(|m| std::fs::set_permissions(&tmp, m.permissions()))
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// `Ok(None)`: no such file. `Err`: it exists but is unusable (names the file, never a value).
fn read_json(path: &Path, what: &str) -> Result<Option<Value>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{what} {}: {e}", path.display())),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{what} {}: not a regular file or larger than 1 MiB",
            path.display()
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES).read_to_string(&mut text))
        .map_err(|e| format!("{what} {}: unreadable ({e})", path.display()))?;
    // A UTF-8 BOM (Windows editors) is not part of the JSON.
    match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
        Ok(v @ Value::Object(_)) => Ok(Some(v)),
        Ok(_) => Err(format!(
            "{what} {}: the top level must be a JSON object",
            path.display()
        )),
        Err(e) => Err(format!("malformed JSON in {what} {}: {e}", path.display())),
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn union(into: &mut Vec<String>, add: Vec<String>) {
    for x in add {
        if !into.contains(&x) {
            into.push(x);
        }
    }
}

impl Settings {
    /// `user`: user-level file (user, managed). `trusted`: user-level, or project/local with `Trusted`.
    fn apply(&mut self, raw: &Value, what: &str, user: bool, trusted: bool) {
        let perm = raw.get("permissions").filter(|p| p.is_object());
        let list = |k| strings(perm.and_then(|p| p.get(k)));
        let p = &mut self.permissions;
        union(&mut p.deny, list("deny"));
        union(&mut p.ask, list("ask"));
        let (allow, dirs) = (list("allow"), list("additionalDirectories"));
        if trusted {
            union(&mut p.allow, allow);
            p.additional_directories.extend(dirs);
        } else if !allow.is_empty() || !dirs.is_empty() {
            self.diagnostics.push(format!(
                "{what}: ignored {} allow rule(s) and {} additionalDirectories (project not trusted)",
                allow.len(),
                dirs.len()
            ));
        }
        if let Some(mode) = perm
            .and_then(|p| p.get("defaultMode"))
            .and_then(Value::as_str)
        {
            let refused = !user
                && (PROJECT_FORBIDDEN_MODES.contains(&mode)
                    || (!trusted && !UNTRUSTED_PROJECT_MODES.contains(&mode)));
            if refused {
                p.ignored_default_mode = Some(mode.into());
                self.diagnostics.push(format!(
                    "{what}: defaultMode \"{mode}\" is not allowed from a project, ignored"
                ));
            } else {
                p.default_mode = mode.into();
            }
        }
        if user {
            if let Some(b) = perm
                .and_then(|p| p.get("blockReadsOutsideWorkingDirectories"))
                .map(|b| b == &Value::Bool(true))
            {
                p.block_reads_outside_working_directories = b;
            }
            self.apply_auto_mode(raw);
        }

        if let Some(hooks) = raw.get("hooks").and_then(Value::as_object) {
            self.apply_hooks(hooks, trusted, what);
        }
        if trusted {
            if let Some(b) = raw.get("disableAllHooks").and_then(Value::as_bool) {
                self.disable_all_hooks = b;
            }
            self.apply_env(raw);
            self.apply_synthesis(raw, what, user);
            // `user` is also true for the managed file: only the user settings file may set tui
            if what != "user settings" && raw.get("synthesis").and_then(|n| n.get("tui")).is_some()
            {
                self.diagnostics.push(format!(
                    "{what}: synthesis.tui is read from user settings only, ignored"
                ));
            }
            if !user && raw.get("synthesis").and_then(|n| n.get("peers")).is_some() {
                self.diagnostics.push(format!(
                    "{what}: synthesis.peers is read from user settings only, ignored"
                ));
            }
        } else if raw.get("disableAllHooks").is_some()
            || raw.get("env").is_some()
            || raw.get("synthesis").is_some()
        {
            self.diagnostics.push(format!(
                "{what}: ignored disableAllHooks/env/synthesis keys (project not trusted)"
            ));
        }

        let m = &mut self.mcp_approval;
        union(
            &mut m.disabled_mcpjson_servers,
            strings(raw.get("disabledMcpjsonServers")),
        );
        if trusted {
            union(
                &mut m.enabled_mcpjson_servers,
                strings(raw.get("enabledMcpjsonServers")),
            );
            if let Some(b) = raw
                .get("enableAllProjectMcpServers")
                .and_then(Value::as_bool)
            {
                m.enable_all_project_mcp_servers = b;
            }
        } else if raw.get("enabledMcpjsonServers").is_some()
            || raw.get("enableAllProjectMcpServers").is_some()
        {
            self.diagnostics.push(format!(
                "{what}: ignored MCP approvals (project not trusted)"
            ));
        }
    }

    fn apply_hooks(&mut self, hooks: &Map<String, Value>, trusted: bool, what: &str) {
        let mut dropped = 0usize;
        for (event, entries) in hooks {
            let Some(entries) = entries.as_array() else {
                continue;
            };
            for entry in entries {
                if !trusted {
                    dropped += 1;
                } else if let Ok(g) = serde_json::from_value::<HookGroup>(entry.clone()) {
                    self.hooks.entry(event.clone()).or_default().push(g);
                } else {
                    self.diagnostics.push(format!(
                        "{what}: a hook group has an invalid shape, skipped"
                    ));
                }
            }
        }
        if dropped > 0 {
            self.diagnostics.push(format!(
                "{what}: ignored {dropped} hook group(s) (project not trusted)"
            ));
        }
    }

    fn apply_env(&mut self, raw: &Value) {
        let Some(env) = raw.get("env").and_then(Value::as_object) else {
            return;
        };
        for (k, v) in env {
            if is_secret_env_key(k) {
                if !self.env_refused.contains(k) {
                    self.env_refused.push(k.clone());
                }
                continue;
            }
            match v {
                Value::String(s) => self.env.insert(k.clone(), s.clone()),
                Value::Number(_) | Value::Bool(_) => self.env.insert(k.clone(), v.to_string()),
                _ => None,
            };
        }
    }

    fn apply_auto_mode(&mut self, raw: &Value) {
        let Some(am) = raw.get("autoMode").and_then(Value::as_object) else {
            return;
        };
        let list = |k| am.get(k).filter(|v| v.is_array()).map(|v| strings(Some(v)));
        let flag = |k| am.get(k).and_then(Value::as_bool);
        self.auto_mode = AutoModeConfig {
            environment: list("environment"),
            allow: list("allow"),
            soft_deny: list("soft_deny"),
            hard_deny: list("hard_deny"),
            classify_all_shell: flag("classifyAllShell"),
            use_auto_mode_during_plan: flag("useAutoModeDuringPlan"),
            model: am
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(|m| m.trim().to_string()),
        };
    }

    fn apply_synthesis(&mut self, raw: &Value, what: &str, user: bool) {
        let Some(n) = raw.get("synthesis").and_then(Value::as_object) else {
            return;
        };
        let version = n.get("version").and_then(Value::as_u64).unwrap_or(1);
        if version > SYNTHESIS_SECTION_VERSION {
            self.diagnostics.push(format!(
                "{what}: synthesis section version {version} is newer than supported {SYNTHESIS_SECTION_VERSION}, ignored"
            ));
            return;
        }
        if user && let Some(p) = n.get("peers").and_then(Value::as_object) {
            self.synthesis.peers.extend(p.clone());
        }
        let num = |o: Option<&Value>, k: &str| o.and_then(|b| b.get(k)).and_then(Value::as_u64);
        let b = n.get("budget").filter(|b| b.is_object());
        let t = &mut self.synthesis.budget;
        t.max_turns = num(b, "max_turns").or(t.max_turns);
        t.max_tool_calls = num(b, "max_tool_calls").or(t.max_tool_calls);
        t.max_verify_attempts = num(b, "max_verify_attempts").or(t.max_verify_attempts);
        if let Some(v) = n.get("verification").and_then(Value::as_object) {
            self.synthesis.verification.extend(v.clone());
        }
        if let Some(w) = n
            .get("compat")
            .and_then(|c| c.get("write_node_transcripts"))
            .and_then(Value::as_bool)
        {
            self.synthesis.compat.write_node_transcripts = Some(w);
        }
    }
}

// ---------------------------------------------------------------- synthesis.tui (USER file only)

/// Panels of the dash sidebar that can be switched off (Workspace, Running sessions and the chat
/// are always on).
pub const TUI_PANELS: [(&str, &str); 4] = [
    ("session", "Session info (model, mode, effort, cwd, uptime)"),
    ("mcp", "MCP servers"),
    ("tools", "Tools & Skills used in this session"),
    ("activity", "Activity feed"),
];

/// `synthesis.tui` of the USER settings.json: what the interactive app shows and how it behaves.
/// Defined here once; `/config` edits it, the app reads it. Project, local and managed files never
/// set it (a diagnostic says so).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuiSettings {
    /// `[session, mcp, tools, activity]`: shown.
    pub panels: [bool; 4],
    /// `logo`: `art` (the braille symbol on the welcome) or `off`.
    pub logo: bool,
    pub show_thinking: bool,
    /// Long tool output, code and diffs start expanded (what ctrl+o toggles).
    pub diffs_expanded: bool,
    /// The `queued: ...` lines above the prompt while a task runs.
    pub queue_indicators: bool,
    /// Spinners and animations; off draws them still and idles the redraw timer.
    pub motion: bool,
    /// Mouse reporting (click, drag-select, wheel). Off leaves the terminal's own selection.
    pub mouse_capture: bool,
    /// Drag-select copies when the button is released.
    pub copy_on_select: bool,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            panels: [true; 4],
            logo: true,
            show_thinking: true,
            diffs_expanded: false,
            queue_indicators: true,
            motion: true,
            mouse_capture: true,
            copy_on_select: true,
        }
    }
}

impl TuiSettings {
    /// Reads `synthesis.tui` of a parsed settings file. A wrong type or an unknown value keeps the
    /// default and yields a diagnostic that names the key, never the value.
    pub fn from_settings(v: &Value) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut diags = vec![];
        let Some(tui) = v.get("synthesis").and_then(|n| n.get("tui")) else {
            return (s, diags);
        };
        let Some(tui) = tui.as_object() else {
            diags.push("synthesis.tui is not an object, ignored".into());
            return (s, diags);
        };
        let mut flag = |key: &str, slot: &mut bool| match tui.get(key) {
            None => {}
            Some(Value::Bool(b)) => *slot = *b,
            Some(_) => diags.push(format!("synthesis.tui.{key} must be true or false, kept")),
        };
        flag("show_thinking", &mut s.show_thinking);
        flag("diffs_expanded", &mut s.diffs_expanded);
        flag("queue_indicators", &mut s.queue_indicators);
        flag("motion", &mut s.motion);
        flag("mouse_capture", &mut s.mouse_capture);
        flag("copy_on_select", &mut s.copy_on_select);
        match tui.get("logo").map(Value::as_str) {
            None => {}
            Some(Some("art")) => s.logo = true,
            Some(Some("off")) => s.logo = false,
            Some(_) => diags.push("synthesis.tui.logo must be art or off, using art".into()),
        }
        match tui.get("panels") {
            None => {}
            Some(Value::Object(p)) => {
                for (k, v) in p {
                    let Some(i) = TUI_PANELS.iter().position(|(n, _)| n == k) else {
                        diags.push(format!("synthesis.tui.panels.{k} is not a panel, ignored"));
                        continue;
                    };
                    match v {
                        Value::Bool(b) => s.panels[i] = *b,
                        _ => diags.push(format!(
                            "synthesis.tui.panels.{k} must be true or false, kept"
                        )),
                    }
                }
            }
            Some(_) => diags.push("synthesis.tui.panels is not an object, ignored".into()),
        }
        for k in tui.keys() {
            if ![
                "panels",
                "logo",
                "show_thinking",
                "diffs_expanded",
                "queue_indicators",
                "motion",
                "mouse_capture",
                "copy_on_select",
            ]
            .contains(&k.as_str())
            {
                diags.push(format!("synthesis.tui.{k} is not a setting, ignored"));
            }
        }
        (s, diags)
    }

    /// The subtree to write back: every key, so the file shows what is in force.
    pub fn to_value(&self) -> Value {
        let panels: Map<String, Value> = TUI_PANELS
            .iter()
            .zip(self.panels)
            .map(|((n, _), on)| ((*n).to_string(), Value::Bool(on)))
            .collect();
        serde_json::json!({
            "panels": panels,
            "logo": if self.logo { "art" } else { "off" },
            "show_thinking": self.show_thinking,
            "diffs_expanded": self.diffs_expanded,
            "queue_indicators": self.queue_indicators,
            "motion": self.motion,
            "mouse_capture": self.mouse_capture,
            "copy_on_select": self.copy_on_select,
        })
    }
}

/// `synthesis.tui` of `<synthesis_home>/settings.json`; a missing or unreadable file is the
/// defaults.
pub fn read_user_tui(synthesis_home: &Path) -> (TuiSettings, Vec<String>) {
    let v = std::fs::read(synthesis_home.join("settings.json"))
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    TuiSettings::from_settings(&v)
}

/// Writes `tui` as the `synthesis.tui` subtree of `<synthesis_home>/settings.json` (the `/config`
/// screen). Every other key and the key order are kept; a file that cannot be parsed, or whose
/// `synthesis` is not an object, is never overwritten (`InvalidData`). Never writes a credential:
/// only the keys of [`TuiSettings`] go into the subtree. See [`edit_json_atomic`].
pub fn persist_user_tui(synthesis_home: &Path, tui: &TuiSettings) -> std::io::Result<()> {
    let tui = tui.to_value();
    edit_json_atomic(
        &synthesis_home.join("settings.json"),
        "user settings",
        |v| {
            let section = v
                .as_object_mut()
                .map(|o| {
                    o.entry("synthesis")
                        .or_insert_with(|| Value::Object(Map::new()))
                })
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "user settings: `synthesis` is not an object",
                    )
                })?;
            section.insert("tui".into(), tui.clone());
            Ok(())
        },
    )
}
