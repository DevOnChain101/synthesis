//! The typed catalogue of settings a UI may show: sections, fields, kinds, scope, edit policy.
//! Pure data. Every key exists in `synthesis-settings` / the crates that read it
//! (docs/specs/config.md §6.1); nothing here is invented.

use serde_json::{Value, json};
use synthesis_settings::UI_PANELS;

/// Sections of the Settings view. Titles follow the topology cards of `cli.html`; the design has no
/// settings form for them (see docs/specs/desktop.md §6), so each is mapped to real keys below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    UserPrompt,
    Memory,
    Execution,
    Guards,
    Hooks,
    Mcp,
    Skills,
    SubAgents,
    /// Not a card in the design: the `synt.tui` toggles the CLI already has.
    Interface,
    /// Not a card in the design: how the desktop window looks (`synt.tui.opacity`, `blur`, logo, motion).
    Appearance,
}

impl Section {
    pub const ALL: [Section; 10] = [
        Section::UserPrompt,
        Section::Memory,
        Section::Execution,
        Section::Guards,
        Section::Hooks,
        Section::Mcp,
        Section::Skills,
        Section::SubAgents,
        Section::Interface,
        Section::Appearance,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::UserPrompt => "USER & PROMPT",
            Section::Memory => "MEMORY",
            Section::Execution => "EXECUTION",
            Section::Guards => "GUARDS",
            Section::Hooks => "HOOKS",
            Section::Mcp => "MCP",
            Section::Skills => "SKILLS",
            Section::SubAgents => "SUB-AGENTS",
            Section::Interface => "INTERFACE",
            Section::Appearance => "APPEARANCE",
        }
    }

    /// Why a section has no (or few) editable fields.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Section::UserPrompt => Some(
                "no backend key: the user name and the task are session state; the prompt rules are AGENTS.md files; model and effort live in synt.json and state.json (/model, /effort)",
            ),
            Section::Skills => Some(
                "no backend key: skills are files in <home>/skills and <project>/.synt/skills; there is no on/off setting",
            ),
            Section::SubAgents => Some(
                "no backend key for the planner/coder/reviewer toggles: agents are files in <home>/agents (project ones only with --trust-project)",
            ),
            Section::Guards => Some(
                "no backend key for sandbox or network: they are `--sandbox` and `--network` flags of the run, not settings",
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ListItem {
    /// Non-empty text of at most 1024 characters.
    Text,
    /// A valid MCP server name.
    ServerName,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    Enum(&'static [&'static str]),
    Int {
        min: u64,
        max: u64,
    },
    Float {
        min: f64,
        max: f64,
    },
    /// A whole percentage shown as a slider.
    Percent {
        min: u64,
        max: u64,
    },
    List(ListItem),
    /// A single-line name of at most 255 characters (no control characters, not starting with `-`).
    Text,
    /// An object or list the UI shows but never writes.
    ReadOnly,
}

/// Which files the loader reads the key from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The user settings file only; project, local, `--settings` and managed copies are ignored.
    UserOnly,
    /// The user and the managed settings file (managed wins); project, local and `--settings` copies
    /// are ignored. For keys that decide where the agent runs, which no repository or command line may set.
    UserManaged,
    /// User, `--settings`, managed; project and local only when the project is trusted.
    Layered,
    /// As `Layered`, but an untrusted project may also set it (it can only tighten: deny/ask,
    /// safe modes, MCP disables).
    Tighten,
}

/// May the UI write it (to the USER file)?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Yes,
    /// Writable after the person confirms: the reason is what to tell them.
    Confirm(&'static str),
    /// Never from the UI; the reason is shown next to the value.
    No(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Dotted path in settings.json, e.g. `synt.tui.panels.mcp`.
    pub key: String,
    pub section: Section,
    pub label: String,
    pub kind: Kind,
    /// `None`: no fixed default (the built-in behaviour is described in `help`).
    pub default: Option<Value>,
    pub help: String,
    pub scope: Scope,
    /// A running session keeps the old value until it is restarted.
    pub restart: bool,
    pub edit: Edit,
    /// Heading of the card the field sits in, inside its section's tab (see [`group_of`]).
    pub group: &'static str,
}

impl Field {
    fn new(key: impl Into<String>, section: Section, label: impl Into<String>, kind: Kind) -> Self {
        Field {
            key: key.into(),
            section,
            label: label.into(),
            kind,
            default: None,
            help: String::new(),
            scope: Scope::UserOnly,
            restart: true,
            edit: Edit::Yes,
            group: "",
        }
    }
    fn def(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
    fn help(mut self, h: &str) -> Self {
        self.help = h.into();
        self
    }
    fn scope(mut self, s: Scope) -> Self {
        self.scope = s;
        self
    }
    fn live(mut self) -> Self {
        self.restart = false;
        self
    }
    fn edit(mut self, e: Edit) -> Self {
        self.edit = e;
        self
    }
    /// A value the UI shows but never writes.
    fn shown(self, why: &'static str) -> Self {
        self.edit(Edit::No(why))
    }
    /// The security-sensitive keys: anything that can widen what the agent may do or send.
    pub fn sensitive(&self) -> bool {
        !matches!(self.edit, Edit::Yes)
    }
    /// Segments of `key`.
    pub fn path(&self) -> impl Iterator<Item = &str> {
        self.key.split('.')
    }
}

pub struct SettingsSchema {
    pub fields: Vec<Field>,
}

impl Default for SettingsSchema {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsSchema {
    pub fn new() -> Self {
        use Kind::*;
        use Section::*;
        let b = |k: &str, s, l: &str, d: bool| Field::new(k, s, l, Bool).def(json!(d));
        let mut f = vec![
            // ---- MEMORY
            b("synt.memory.enabled", Memory, "Auto-memory", true)
                .help("Retrieve and store memories across sessions. A project file can only turn it off."),
            b("synt.memory.reflection", Memory, "Memory reflection", false)
                .help("Send a digest of each task (request, tool calls, user turns) to the model provider to write memories.")
                .edit(Edit::Confirm("this sends task digests to your model provider")),
            Field::new("synt.memory.daily_budget_usd", Memory, "Reflection daily budget (USD)", Float { min: 0.0, max: 1000.0 })
                .def(json!(0.5))
                .help("Most the reflection model calls may cost per day."),
            Field::new("synt.memory.disabled_workspaces", Memory, "Workspaces without memory", List(ListItem::Text))
                .help("Glob patterns matched against the project root; memory is off there."),
            Field::new("synt.images.retentionDays", Memory, "Keep stored images (days)", Int { min: 0, max: 3650 })
                .def(json!(30))
                .help("Days a pasted or returned image stays in the session store; 0 keeps them forever."),
            // ---- EXECUTION
            Field::new("synt.budget.max_turns", Execution, "Max turns", Int { min: 1, max: 1_000_000 })
                .scope(Scope::Layered).help("Model turns per task. Unset: the built-in limit."),
            Field::new("synt.budget.max_tool_calls", Execution, "Max tool calls", Int { min: 1, max: 1_000_000 })
                .scope(Scope::Layered).help("Tool calls per task. Unset: the built-in limit."),
            Field::new("synt.budget.max_verify_attempts", Execution, "Max verify attempts", Int { min: 1, max: 1_000_000 })
                .scope(Scope::Layered).help("Verification retries per task (synt.verification.max_attempts overrides it)."),
            Field::new("synt.verification.max_attempts", Execution, "Verification attempts", Int { min: 1, max: 100 })
                .scope(Scope::Layered).help("Attempts of the verification gate and the session budget."),
            Field::new("synt.verification.inferred_only", Execution, "Inferred-only checks", Enum(&["cap_unverified", "allow_passed"]))
                .def(json!("cap_unverified")).scope(Scope::Layered)
                .help("cap_unverified: a task with only inferred checks stays unverified. allow_passed: it may pass.")
                .edit(Edit::Confirm("allow_passed lets inferred checks alone mark a task passed")),
            Field::new("synt.verification.checks", Execution, "Required checks", ReadOnly).scope(Scope::Layered)
                .shown("each check is a shell command the verifier runs; edit settings.json by hand"),
            b("synt.context.auto_compact", Execution, "Auto-compact context", true)
                .help("Compact the conversation before the context window fills."),
            Field::new("synt.context.auto_compact_threshold", Execution, "Compact at", Float { min: 0.5, max: 0.95 })
                .def(json!(0.85)).help("Fraction of the context window that triggers compaction."),
            b("synt.shell.user_commands_model_turn", Execution, "Model turn after !cmd", true)
                .help("The model analyses the result of a `!cmd` (`!!cmd` never does)."),
            b("synt.shell.user_commands_unsandboxed", Execution, "Run !cmd outside the sandbox", false)
                .help("Off: `!cmd` runs in the same sandbox and network mode as the Bash tool.")
                .edit(Edit::Confirm("this runs your `!` commands without the OS sandbox")),
            b("synt.sandbox.wsl.hide_windows_drives", Execution, "WSL: hide Windows drives from commands", true)
                .help("On WSL the sandbox hides /mnt/c and the other Windows drives from commands (the workspace and --sandbox-read folders on a drive stay). Off: drives stay readable (still never writable). Costs when on: sibling projects on a drive and symlinks into /mnt/c are not visible.")
                .edit(Edit::Confirm("this lets sandboxed commands READ your Windows drives")),
            b("synt.modelInfo.fetch", Execution, "Fetch model info at start", true)
                .help("Ask each provider's /v1/models for context windows and limits."),
            Field::new("synt.compat.write_node_transcripts", Execution, "Write Node-format transcripts", Bool)
                .scope(Scope::Layered).help("Also write transcripts the old Node CLI can read."),
            Field::new("synt.browser.login_timeout_sec", Execution, "Browser login timeout (s)", Int { min: 10, max: 3600 })
                .def(json!(600)).help("How long a browser login waits for the person."),
            Field::new("synt.browser.allow_unsandboxed", Execution, "Browser without sandbox", Bool).def(json!(false))
                .shown("turns the browser sandbox off; edit settings.json by hand"),
            Field::new("synt.browser.secrets", Execution, "Browser login secrets", ReadOnly)
                .shown("references to credentials; edit settings.json by hand ({env:} / {file:} only)"),
            Field::new("synt.browser.profiles", Execution, "Browser profiles", ReadOnly)
                .shown("decides which domains a login may browse; edit settings.json by hand"),
            Field::new("synt.browser.approved", Execution, "Browser pre-approvals", ReadOnly)
                .shown("pre-approves logins for unattended runs; edit settings.json by hand"),
            Field::new("synt.pricing", Execution, "Model prices", ReadOnly)
                .shown("a table keyed by provider/model; edit settings.json by hand"),
            Field::new("env", Execution, "Command environment", ReadOnly).scope(Scope::Layered)
                .shown("variables for Bash, hooks and verification; secret-shaped names are refused by the loader, edit settings.json by hand"),
            // ---- GUARDS
            Field::new("permissions.defaultMode", Guards, "Default permission mode", Enum(&["default", "acceptEdits", "plan", "dontAsk"]))
                .def(json!("default")).scope(Scope::Tighten)
                .help("Mode a session starts in. auto and bypassPermissions are never set from settings.")
                .edit(Edit::Confirm("a mode other than default changes what runs without asking")),
            Field::new("permissions.blockReadsOutsideWorkingDirectories", Guards, "Block reads outside the workspace", Bool)
                .def(json!(false)).help("Refuse file reads outside the working directories.")
                .edit(Edit::Confirm("turning this off lets the agent read outside the workspace")),
            Field::new("permissions.allow", Guards, "Allow rules", List(ListItem::Text)).scope(Scope::Layered)
                .shown("an allow rule grants a tool without asking; add them through the approval prompt (allow always) or by hand"),
            Field::new("permissions.deny", Guards, "Deny rules", List(ListItem::Text)).scope(Scope::Tighten)
                .shown("rule syntax is validated by synthesis-permissions, not wired here yet; edit by hand"),
            Field::new("permissions.ask", Guards, "Ask rules", List(ListItem::Text)).scope(Scope::Tighten)
                .shown("rule syntax is validated by synthesis-permissions, not wired here yet; edit by hand"),
            Field::new("permissions.additionalDirectories", Guards, "Additional directories", List(ListItem::Text)).scope(Scope::Layered)
                .shown("widens the filesystem the agent may touch; edit by hand"),
            Field::new("autoMode", Guards, "Auto mode classifier", ReadOnly)
                .shown("steers the classifier that approves without asking; edit settings.json by hand"),
            // ---- HOOKS
            Field::new("hooks", Hooks, "Hooks", ReadOnly).scope(Scope::Layered)
                .shown("hooks are commands run with your privileges; edit settings.json by hand"),
            Field::new("disableAllHooks", Hooks, "Disable all hooks", Bool).def(json!(false)).scope(Scope::Layered)
                .help("Run no hooks at all.")
                .edit(Edit::Confirm("this also switches off hooks you rely on as safeguards")),
            // ---- MCP
            Field::new("synt.mcp.toolSearch", Mcp, "MCP tool search", Enum(&["auto", "on", "off"]))
                .def(json!("auto")).help("auto: defer MCP tool schemas behind ToolSearch once they take 10% of the context. on: always. off: send all."),
            Field::new("synt.mcp.env_passthrough", Mcp, "Env passthrough to MCP servers", List(ListItem::Text))
                .shown("names of variables handed to stdio servers, secrets included; edit settings.json by hand"),
            Field::new("synt.mcp.servers", Mcp, "Read/write tool guard", ReadOnly)
                .shown("per-server write_tools/read_tools; read_tools weakens the guard, edit by hand"),
            Field::new("enabledMcpjsonServers", Mcp, "Approved project MCP servers", List(ListItem::ServerName)).scope(Scope::Layered)
                .shown("approving a project server lets a repository start a process; use --approve-mcp NAME@HASH"),
            Field::new("enableAllProjectMcpServers", Mcp, "Approve all project MCP servers", Bool).scope(Scope::Layered)
                .shown("lets any repository start processes; edit by hand"),
            Field::new("disabledMcpjsonServers", Mcp, "Disabled MCP servers", List(ListItem::ServerName)).scope(Scope::Tighten)
                .help("Project servers that never start (beats every approval)."),
            // ---- SUB-AGENTS
            Field::new("synt.peers", SubAgents, "Peer sessions", ReadOnly)
                .shown("messaging between sessions (off by default); edit settings.json by hand"),
        ];
        // ---- WSL (Windows desktop only; user file only, a project file can never turn it on)
        f.push(
            b("synt.desktop.wsl.enabled", Execution, "Run the agent in WSL2", false)
                .scope(Scope::UserManaged)
                .help("Windows only. Runs the Linux engine inside WSL2 and works on Linux paths there. Engine settings and API keys live inside WSL (~/.synt there), not on Windows; use {file:} provider keys, because wsl.exe --exec does not load the shell profile. A managed settings file may set it and then wins (on forces WSL, off forbids it). This is NOT isolation: until the sandbox work lands, the agent can run Windows programs (interop) and write /mnt/c."),
        );
        f.push(
            Field::new("synt.desktop.wsl.distro", Execution, "WSL distro", Text)
                .scope(Scope::UserManaged)
                .help("Windows only. The distro to use; unset is WSL's default. WSL1 is refused. A managed settings file may set it and then wins."),
        );
        f.push(
            Field::new("synt.desktop.max_cost_usd", Execution, "WSL cost cap per task (USD)", Float { min: 0.01, max: 1_000_000.0 })
                .def(json!(10.0))
                .scope(Scope::UserManaged)
                .help("Windows only. The spend limit the desktop puts on each task of a WSL session (default 10). Needs a price for the model: built in for Anthropic and OpenAI models, else synt.pricing inside WSL. A managed settings file wins."),
        );
        f.push(
            b("synt.desktop.no_cost_cap", Execution, "WSL: no cost cap", false)
                .scope(Scope::UserManaged)
                .help("Windows only. Explicitly run WSL sessions without a spend limit (the cost cap above is then not passed). A managed settings file wins."),
        );
        // ---- APPEARANCE
        f.push(
            Field::new(
                "synt.tui.opacity",
                Appearance,
                "Window opacity",
                Percent {
                    min: u64::from(synthesis_settings::UI_OPACITY_MIN),
                    max: 100,
                },
            )
            .def(json!(100))
            .live()
            .help("How solid the window's backgrounds are, the title bar included. Text always stays opaque; below 30 % the window is too faint to read."),
        );
        f.push(
            b("synt.tui.blur", Appearance, "Background blur", false)
                .live()
                .help("Blur what is behind the window. It shows only when opacity is under 100 %."),
        );
        f.push(
            Field::new(
                "synt.tui.logo",
                Interface,
                "Welcome logo",
                Enum(&["art", "off"]),
            )
            .def(json!("art"))
            .live()
            .help("The braille symbol on the welcome screen."),
        );
        // ---- INTERFACE
        for (k, l, d, h) in [
            (
                "show_thinking",
                "Show thinking",
                true,
                "Draw the model's thinking.",
            ),
            (
                "verbose",
                "Verbose rows",
                false,
                "Also draw internal rows (memory, mode, permission, sub-agent bookkeeping).",
            ),
            (
                "diffs_expanded",
                "Expand diffs",
                false,
                "Long tool output, code and diffs start expanded.",
            ),
            (
                "queue_indicators",
                "Queue indicators",
                true,
                "The `queued:` lines above the prompt.",
            ),
            (
                "motion",
                "Motion",
                true,
                "Spinners and animations; off draws them still.",
            ),
            (
                "mouse_capture",
                "Mouse capture",
                true,
                "Click, drag-select and wheel; off leaves the terminal's own selection.",
            ),
            (
                "copy_on_select",
                "Copy on select",
                true,
                "Drag-select copies when the button is released.",
            ),
        ] {
            f.push(b(&format!("synt.tui.{k}"), Interface, l, d).live().help(h));
        }
        for (k, l) in UI_PANELS {
            f.push(
                b(&format!("synt.tui.panels.{k}"), Interface, l, true)
                    .live()
                    .help("Show this panel of the dash."),
            );
        }
        for (k, l, d, h) in [
            (
                "focus_mode",
                "Focus mode",
                false,
                "Hide every panel except the chat and its sub-agent view. The panel switches above keep their values and apply again when this is off.",
            ),
            (
                "header",
                "Show header",
                true,
                "The title strip with the session, model and health chips. Off: the chat reclaims its height.",
            ),
            (
                "footer",
                "Show footer",
                true,
                "The key-hint bar at the bottom. Off: the chat reclaims its height; the permission mode stays under the input.",
            ),
        ] {
            f.push(b(&format!("synt.tui.{k}"), Interface, l, d).live().help(h));
        }
        for x in &mut f {
            x.group = group_of(&x.key, x.section);
        }
        SettingsSchema { fields: f }
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    pub fn section(&self, s: Section) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(move |f| f.section == s)
    }
}

/// The card heading a key is shown under inside its tab. Every key has one (a test walks the schema).
pub fn group_of(key: &str, section: Section) -> &'static str {
    use Section::*;
    match section {
        Appearance => "Window",
        Interface
            if key.starts_with("synt.tui.panels.")
                || ["focus_mode", "header", "footer"]
                    .iter()
                    .any(|k| key == format!("synt.tui.{k}")) =>
        {
            "Panels"
        }
        Interface
            if ["motion", "mouse_capture", "copy_on_select"]
                .iter()
                .any(|k| key.ends_with(k)) =>
        {
            "Behaviour"
        }
        Interface => "Chat",
        Memory if key.starts_with("synt.images.") => "Images",
        Memory => "Memory",
        Execution
            if key.starts_with("synt.budget.")
                || key == "synt.desktop.max_cost_usd"
                || key == "synt.desktop.no_cost_cap" =>
        {
            "Limits"
        }
        Execution if key.starts_with("synt.verification.") => "Verification",
        Execution
            if key.starts_with("synt.context.")
                || key.starts_with("synt.modelInfo.")
                || key == "synt.pricing" =>
        {
            "Models and context"
        }
        Execution
            if key.starts_with("synt.shell.")
                || key.starts_with("synt.sandbox.")
                || key == "env" =>
        {
            "Shell and environment"
        }
        Execution if key.starts_with("synt.browser.") => "Browser",
        Execution if key.starts_with("synt.desktop.wsl.") => "WSL",
        Execution => "Compatibility",
        Guards if key == "autoMode" => "Auto mode",
        Guards => "Permissions",
        Hooks => "Hooks",
        Mcp if key == "synt.mcp.toolSearch" => "Tool search",
        Mcp => "Servers",
        SubAgents => "Peers",
        UserPrompt | Skills => "General",
    }
}

/// Checks `v` against the kind (the loaders' own validators run on top, see `store`). The message
/// does not name the key; the caller does.
pub fn validate_kind(kind: Kind, v: &Value) -> Result<(), String> {
    match kind {
        Kind::Bool if v.is_boolean() => Ok(()),
        Kind::Bool => Err("must be true or false".into()),
        Kind::Enum(opts) => match v.as_str() {
            Some(s) if opts.contains(&s) => Ok(()),
            _ => Err(format!("must be one of {}", opts.join(", "))),
        },
        Kind::Int { min, max } => match v.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(()),
            _ => Err(format!("must be a whole number from {min} to {max}")),
        },
        Kind::Percent { min, max } => match v.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(()),
            _ => Err(format!("must be a whole number from {min} to {max}")),
        },
        Kind::Float { min, max } => match v.as_f64() {
            Some(n) if n.is_finite() && n >= min && n <= max => Ok(()),
            _ => Err(format!("must be a number from {min} to {max}")),
        },
        Kind::List(item) => {
            let ok = v.as_array().is_some_and(|a| {
                a.iter().all(|x| {
                    x.as_str().is_some_and(|s| match item {
                        ListItem::Text => {
                            !s.is_empty() && s.chars().count() <= 1024 && !s.contains('\0')
                        }
                        ListItem::ServerName => synthesis_mcp::valid_server_name(s),
                    })
                })
            });
            if ok {
                Ok(())
            } else {
                Err("must be a list of valid strings".into())
            }
        }
        Kind::Text => match v.as_str() {
            Some(s)
                if !s.is_empty()
                    && s.chars().count() <= 255
                    && !s.starts_with('-')
                    && !s.chars().any(char::is_control) =>
            {
                Ok(())
            }
            _ => Err("must be a name of up to 255 characters, not starting with `-`".into()),
        },
        Kind::ReadOnly => Err("is read-only".into()),
    }
}
