//! Permission modes, local commands and dialog option shapes (port of `dialogs.js`).

use serde_json::Value;

pub const MODES: [&str; 5] = [
    "default",
    "acceptEdits",
    "plan",
    "auto",
    "bypassPermissions",
];
pub const VALID_MODES: [&str; 6] = [
    "default",
    "acceptEdits",
    "plan",
    "auto",
    "bypassPermissions",
    "dontAsk",
];

/// (name, description, argumentHint)
pub const LOCAL_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("help", "commands and keys", None),
    (
        "clear",
        "start a new conversation in this tab",
        Some("[name]"),
    ),
    ("new", "open a new session in this workspace", None),
    ("handoff", "summarize this session for a handoff", None),
    (
        "fork",
        "handoff, then open a new session in this workspace",
        None,
    ),
    (
        "close",
        "close this session tab and forget it at startup (the last one quits)",
        None,
    ),
    (
        "resume",
        "continue the selected session, or a session id",
        Some("[session id]"),
    ),
    (
        "model",
        "pick a model, or change it directly",
        Some("[opus | sonnet | haiku | …]"),
    ),
    (
        "effort",
        "pick reasoning effort, or set it directly",
        Some("[low | medium | high | xhigh | max | auto]"),
    ),
    ("agents", "list configured agents and show their file", None),
    (
        "status",
        "session, model, version, tools and MCP servers",
        None,
    ),
    ("context", "what is using the context window", None),
    (
        "image",
        "attach an image file to the next message",
        Some("<path>"),
    ),
    (
        "cost",
        "turns and cost of this conversation (alias: /usage)",
        None,
    ),
    (
        "usage",
        "session tokens, turns, model and context window (alias: /cost)",
        None,
    ),
    (
        "goal",
        "set a goal and keep working until it is met, or show/clear it",
        Some("[condition | clear]"),
    ),
    (
        "compact",
        "summarize the conversation so far to free up context",
        Some("[instructions]"),
    ),
    (
        "autocompact",
        "configure the auto-compact threshold",
        Some("[auto | <tokens> | off]"),
    ),
    (
        "init",
        "analyze the repo and create or improve AGENTS.md",
        None,
    ),
    (
        "import",
        "import config from another coding CLI",
        Some("[claude]"),
    ),
    ("rename", "rename this conversation", Some("[name]")),
    ("recap", "generate a one-line session recap", None),
    (
        "mcp",
        "list configured MCP servers, or manage them",
        Some("[reconnect|enable|disable [server|all]]"),
    ),
    (
        "memory",
        "show loaded memory files, or print one’s path",
        Some("[open <n>]"),
    ),
    (
        "reload-skills",
        "reload commands, skills, agents and memory from disk (alias: /reload)",
        None,
    ),
    ("reload", "alias for /reload-skills", None),
    ("config", "show config, or set a key", Some("[key=value]")),
    (
        "permissions",
        "list permission rules, or add one for this session",
        Some("[allow|deny|ask <rule>]"),
    ),
    ("hooks", "list configured hooks per event", None),
    (
        "add-dir",
        "allow tool access to another directory for this session",
        Some("<path>"),
    ),
    ("todos", "show the current todo list", None),
    (
        "rewind",
        "go back to before an earlier prompt: conversation and/or the files it changed",
        Some("[<n> [both|chat|files]]"),
    ),
    (
        "bashes",
        "list background commands, or stop one (alias: /tasks)",
        Some("[kill <id|all>]"),
    ),
    (
        "export",
        "write the conversation as markdown",
        Some("[file]"),
    ),
    (
        "copy",
        "copy the last assistant reply to the clipboard",
        None,
    ),
    (
        "doctor",
        "check node, proxy, credentials, base URL and tool availability",
        None,
    ),
    ("review", "review the current git diff", None),
    (
        "security-review",
        "review the current git diff for security issues",
        None,
    ),
    (
        "color",
        "set the prompt bar colour for this session",
        Some("[red|blue|green|yellow|purple|orange|pink|cyan|default]"),
    ),
    (
        "focus",
        "toggle focus view (just prompt, summary and response)",
        Some("[on|off]"),
    ),
    ("exit", "quit the monitor", None),
];

pub const HELP: &[&str] = &[
    "enter send (while a task runs it is queued: ↑ edits it, esc drops it) · alt+enter / shift+enter / \\+enter newline · alt+backspace delete word back · alt+d delete word forward · ctrl+w delete back to whitespace · ctrl+u clear to line start",
    "esc interrupt the task (partial output is kept) · esc esc on an empty prompt edits the last prompt · shift+tab cycle the permission mode",
    "! run a shell command (!! only records it) · / commands and skills · @ mention a file",
    "/model /mcp /agents /permissions /status /compact /clear /new /resume /focus /exit",
    "↑/↓ prompt history · ctrl+r search history · ctrl+o show all of long tasks, bash output and code (6 lines by default) · right-click in the prompt pastes the clipboard · drag, double or triple click copies · pgup/pgdn scroll · ctrl+l redraw a garbled screen",
    "ctrl+n or /new open a session in another folder · /resume continues a saved one (this folder first)",
    "dialogs: ↑/↓ or 1-9 choose · click an option · space toggles · ←/→ switch question · enter confirm · esc reject · x shows the whole input",
];

/// (key, label, mode)
pub type OptionDef = (&'static str, &'static str, Option<&'static str>);

pub const OUTSIDE_READ_OPTIONS: [(&str, &str); 4] = [
    (
        "outside-keep",
        "Yes, and keep allowing any reads outside the working directories",
    ),
    (
        "outside-block",
        "No, and block reads outside the working directories from now on",
    ),
    ("outside-ask", "No, and ask again next time (esc)"),
    ("outside-once", "Yes, but ask again next time"),
];

pub const PLAN_OPTIONS: [OptionDef; 3] = [
    ("auto", "Yes, and use auto mode", Some("auto")),
    ("manual", "Yes, manually approve edits", Some("default")),
    ("keep", "No, keep planning (esc)", None),
];

/// 'manual' is the UI name of the stored value 'default'; `None` for anything unknown.
pub fn normalize_mode(m: Option<&str>) -> Option<&'static str> {
    let m = if m == Some("manual") {
        Some("default")
    } else {
        m
    };
    VALID_MODES.iter().copied().find(|v| Some(*v) == m)
}

pub fn mode_name(m: Option<&str>) -> String {
    match m {
        None | Some("") => "manual".into(),
        Some("default") => "manual".into(),
        Some("acceptEdits") => "accept edits".into(),
        Some("plan") => "plan".into(),
        Some("auto") => "auto".into(),
        Some("dontAsk") => "don’t ask".into(),
        Some("bypassPermissions") => "bypass permissions".into(),
        Some(o) => o.into(),
    }
}

/// Label for the "don't ask again" option of a permission request.
pub fn always_label(suggestions: &[Value]) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    for sg in suggestions {
        let t = s(sg, "type");
        if t == "setMode" && s(sg, "mode") == "acceptEdits" {
            return "Yes, allow all edits during this session".into();
        }
        if t == "setMode" {
            return format!("Yes, and switch to {} mode", s(sg, "mode"));
        }
        if (t == "addRules" || t == "replaceRules")
            && let Some(rules) = sg
                .get("rules")
                .and_then(Value::as_array)
                .filter(|r| !r.is_empty())
        {
            {
                let names: Vec<String> = rules
                    .iter()
                    .map(|r| {
                        let rc = s(r, "ruleContent");
                        if rc.is_empty() {
                            s(r, "toolName")
                        } else {
                            format!("{}({rc})", s(r, "toolName"))
                        }
                    })
                    .collect();
                return format!("Yes, and don't ask again for {}", names.join(", "));
            }
        }
        if t == "addDirectories"
            && let Some(d) = sg.get("directories").and_then(Value::as_array)
        {
            let ds: Vec<&str> = d.iter().filter_map(Value::as_str).collect();
            return format!("Yes, and allow access to {}", ds.join(", "));
        }
    }
    "Yes, and don't ask again this session".into()
}

/// (key, label) options of a permission request.
pub fn permission_options(suggestions: &[Value], offer_auto: bool) -> Vec<(String, String)> {
    let mut o = vec![("yes".to_string(), "Yes".to_string())];
    if !suggestions.is_empty() {
        o.push(("always".into(), always_label(suggestions)));
    }
    if offer_auto {
        o.push(("auto-mode".into(), "Yes, and switch to auto mode".into()));
    }
    o.push((
        "no".into(),
        "No, and tell Synthesis what to do differently (esc)".into(),
    ));
    o
}
