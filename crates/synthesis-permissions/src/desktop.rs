//! Desktop-control policy (docs/DESKTOP-CONTROL.md, owner policy 2026-11-03). Pure functions
//! shared by the permission engine, the harness and `synthesis-desktop`, so all three agree.
//!
//! - An app Synthesis launched from the project is allowed in every permission mode.
//! - Any other app needs a dialog per app per session and exists only in `default` mode.
//! - The hard-deny list below is never lifted by an approval, a rule or a mode.

use crate::Mode;

/// Tools of the desktop family. Peers and sub-agents never get them.
pub fn is_desktop_tool(name: &str) -> bool {
    name.starts_with("desktop_")
}

/// Where the app under control came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Launched by Synthesis from a path inside the workspace: the app the agent is building.
    Project,
    /// Anything else (outside the workspace, or attached).
    External,
}

/// `app_id` prefix of an external app (the id is minted by the tool, never by the model).
pub const EXTERNAL_PREFIX: &str = "ext-";

pub fn origin_of_app_id(app_id: &str) -> Origin {
    if app_id.starts_with(EXTERNAL_PREFIX) {
        Origin::External
    } else {
        Origin::Project
    }
}

/// A launch target is a file path when it has a separator; anything else is a bundle id or a
/// program name, which is never "inside the workspace".
pub fn is_path_target(target: &str) -> bool {
    target.contains('/') || target.contains('\\')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allow,
    /// One dialog per app per session (`hard`: no allow rule or mode skips it).
    Ask,
    Deny(&'static str),
}

pub const PLAN_REFUSED: &str =
    "plan mode does not execute code, and launching or driving a desktop app does";

pub const EXTERNAL_ONLY_MANUAL: &str = "controlling an app that Synthesis did not launch from this project works only in manual (default) permission mode, with a dialog per app; no rule or other mode can approve it";

/// Rule (a) and (b): the decision for one call by where the app came from and the mode.
pub fn gate(origin: Origin, mode: Mode) -> Gate {
    match (origin, mode) {
        // plan mode must not execute code, and a desktop app is code
        (_, Mode::Plan) => Gate::Deny(PLAN_REFUSED),
        (Origin::Project, _) => Gate::Allow,
        (Origin::External, Mode::Default) => Gate::Ask,
        (Origin::External, _) => Gate::Deny(EXTERNAL_ONLY_MANUAL),
    }
}

/// Exact (case-folded) bundle ids and executable names, with the family for the message.
const DENIED: &[(&str, &[&str])] = &[
    (
        "a password manager",
        &[
            "com.1password.1password",
            "com.agilebits.onepassword",
            "com.agilebits.onepassword7",
            "com.agilebits.onepassword-osx",
            "1password",
            "1password 7",
            "com.bitwarden.desktop",
            "bitwarden",
            "org.keepassxc.keepassxc",
            "com.apple.keychainaccess",
            "keychain access",
            "keychainaccess",
            "com.dashlane.dashlanephonefinal",
            "com.dashlane.dashlane",
            "dashlane",
            "com.lastpass.lastpass",
            "lastpass",
            "com.apple.passwords",
            "passwords",
            "authy",
            "com.authy.authy",
            "proton pass",
            "protonpass",
            "me.proton.pass.electron",
            "nordpass",
            "enpass",
            "authenticator",
            "seahorse",
            "org.gnome.seahorse.application",
            "kwalletmanager",
            "kwalletmanager5",
            "bitwarden.exe",
        ],
    ),
    (
        "a terminal or shell host",
        &[
            "terminal",
            "com.apple.terminal",
            "iterm",
            "iterm2",
            "com.googlecode.iterm2",
            "windowsterminal",
            "wt",
            "cmd",
            "powershell",
            "pwsh",
            "conhost",
            "openconsole",
            "alacritty",
            "org.alacritty",
            "kitty",
            "net.kovidgoyal.kitty",
            "wezterm",
            "wezterm-gui",
            "com.github.wez.wezterm",
            "ghostty",
            "com.mitchellh.ghostty",
            "hyper",
            "co.zeit.hyper",
            "warp",
            "dev.warp.warp-stable",
            "gnome-terminal",
            "gnome-terminal-server",
            "konsole",
            "xterm",
            "urxvt",
            "terminator",
            "tilix",
            "xfce4-terminal",
            "tmux",
        ],
    ),
    (
        "a script runner or script editor",
        &[
            "script editor",
            "com.apple.scripteditor2",
            "automator",
            "com.apple.automator",
            "shortcuts",
            "com.apple.shortcuts",
        ],
    ),
    (
        "a task manager",
        &[
            "activity monitor",
            "com.apple.activitymonitor",
            "taskmgr",
            "task manager",
            "resmon",
            "htop",
            "gnome-system-monitor",
            "ksysguard",
            "plasma-systemmonitor",
        ],
    ),
    (
        "a remote-desktop app",
        &[
            "teamviewer",
            "anydesk",
            "mstsc",
            "microsoft remote desktop",
            "com.microsoft.rdc.macos",
            "vncviewer",
            "tigervnc",
            "remmina",
            "rustdesk",
            "parsec",
            "splashtop",
            "chrome remote desktop",
            "apple remote desktop",
        ],
    ),
    (
        "system settings",
        &[
            "com.apple.systempreferences",
            "system settings",
            "system preferences",
            "systemsettings",
            "control",
            "credwiz",
            "gnome-control-center",
        ],
    ),
    (
        "the desktop shell",
        &[
            "com.apple.finder",
            "finder",
            "explorer",
            "com.apple.dock",
            "dock",
            "loginwindow",
            "securityagent",
            "coreservicesuiagent",
            "universalaccessauthwarn",
        ],
    ),
    (
        "a web browser (use the browser_* tools)",
        &[
            "com.google.chrome",
            "com.google.chrome.canary",
            "com.google.chrome.beta",
            "com.google.chrome.dev",
            "google chrome",
            "google-chrome",
            "google-chrome-stable",
            "chrome",
            "chromium",
            "chromium-browser",
            "org.chromium.chromium",
            "org.mozilla.firefox",
            "org.mozilla.nightly",
            "org.mozilla.firefoxdeveloperedition",
            "firefox",
            "com.apple.safari",
            "safari",
            "com.microsoft.edgemac",
            "microsoft edge",
            "msedge",
            "com.brave.browser",
            "brave browser",
            "brave",
            "brave-browser",
            "company.thebrowser.browser",
            "arc",
            "com.operasoftware.opera",
            "opera",
            "com.vivaldi.vivaldi",
            "vivaldi",
            "iexplore",
            "tor browser",
            "torbrowser",
            "org.torproject.torbrowser",
        ],
    ),
];

/// Case-folded identifiers a launch target or a window's app name can be known by: the whole
/// string, its file name (also without a `.exe`/`.app` suffix), and every `.app` bundle
/// component of a path (so `X.app/Contents/MacOS/anything` is X). Other parent directories
/// never count: `/home/u/firefox/proj/app` is not Firefox.
fn candidates(s: &str) -> Vec<String> {
    let strip = |n: &str| {
        let n = n.trim().to_lowercase();
        for suf in [".exe", ".app", ".bat", ".cmd"] {
            if let Some(b) = n.strip_suffix(suf) {
                return b.to_owned();
            }
        }
        n
    };
    let parts: Vec<&str> = s.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let mut v = vec![s.trim().to_lowercase()];
    if let Some(last) = parts.last() {
        v.push(strip(last));
        v.push(last.trim().to_lowercase());
    }
    for p in &parts {
        if p.to_lowercase().ends_with(".app") {
            v.push(strip(p));
        }
    }
    v
}

/// The family when `id` (a bundle id, executable name or path) is on the hard-deny list.
/// Matching is exact on a case-folded name, except `keepass*` (every KeePass variant).
pub fn hard_denied(id: &str) -> Option<&'static str> {
    for c in candidates(id) {
        if c.starts_with("keepass") {
            return Some("a password manager");
        }
        if let Some((family, _)) = DENIED.iter().find(|(_, names)| names.contains(&c.as_str())) {
            return Some(family);
        }
    }
    None
}

/// Characters that can reorder or hide text on screen: control, bidi embeddings, overrides and
/// isolates, zero-width and other invisible format characters.
fn is_spoofing(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}' | '\u{061C}' | '\u{180E}')
}

/// Text safe for a dialog or an event: control, bidi and zero-width characters dropped, length
/// capped (the cut is shown).
pub fn sanitize(s: &str) -> String {
    sanitize_to(s, 120)
}

pub fn sanitize_to(s: &str, max: usize) -> String {
    let t: String = s.chars().filter(|c| !is_spoofing(*c)).collect();
    let n = t.chars().count();
    if n > max {
        format!(
            "{}...[+{} chars cut]",
            t.chars().take(max).collect::<String>(),
            n - max
        )
    } else {
        t
    }
}

/// The whole command line of a launch as the person must see it: target, every argument (up to
/// 20), cwd and env additions (none), each sanitized, cuts shown.
pub fn describe_launch(input: &serde_json::Value) -> String {
    let get = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    let mut out = format!("target {:?}", sanitize_to(get("target"), 300));
    let args: Vec<&str> = input
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    if args.is_empty() {
        out += ", no arguments";
    } else {
        out += &format!(", {} argument(s):", args.len());
        for a in args.iter().take(20) {
            out += &format!(" {:?}", sanitize_to(a, 200));
        }
        if args.len() > 20 {
            out += &format!(" ...[+{} more arguments not shown]", args.len() - 20);
        }
    }
    let cwd = get("cwd");
    out += &if cwd.is_empty() {
        ", in the session directory".to_owned()
    } else {
        format!(", cwd {:?}", sanitize_to(cwd, 300))
    };
    out + ", environment additions: none"
}

fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The launch as a shell command line, so the permission engine can judge it exactly like a
/// Bash command (the tool itself never goes through a shell with this string).
pub fn launch_command_line(target: &str, args: &[String], cwd: Option<&str>) -> String {
    let mut c = String::new();
    if let Some(d) = cwd {
        c += &format!("cd {} && ", posix_quote(d));
    }
    c += &posix_quote(target);
    for a in args {
        c += &format!(" {}", posix_quote(a));
    }
    c
}

/// Programs that run other code or open other programs: never a desktop target, whatever the
/// location or the name they were copied to (the tool also compares file content).
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "ash",
    "busybox",
    "env",
    "sudo",
    "doas",
    "su",
    "xargs",
    "nohup",
    "nice",
    "timeout",
    "open",
    "start",
    "xdg-open",
    "gio",
    "launchctl",
    "osascript",
    "cscript",
    "wscript",
    "mshta",
    "rundll32",
    "regsvr32",
    "cmd",
    "powershell",
    "pwsh",
    "python",
    "py",
    "pythonw",
    "node",
    "nodejs",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "java",
    "javaw",
    "dotnet",
    "wine",
    "expect",
    "tclsh",
    "wish",
    "awk",
    "gawk",
    "make",
    "ssh",
    "scp",
    "nc",
    "ncat",
    "socat",
    "curl",
    "wget",
];

/// Why a target cannot be launched by name alone (a shell, an interpreter, a launcher).
pub fn refused_target(target: &str) -> Option<&'static str> {
    let name = target
        .rsplit(['/', '\\'])
        .find(|p| !p.is_empty())?
        .to_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    let family = |p: &str| {
        stem.strip_prefix(p)
            .is_some_and(|r| r.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    (INTERPRETERS.contains(&stem)
        || ["python", "ruby", "perl", "node", "php", "lua", "pwsh"]
            .iter()
            .any(|p| family(p)))
    .then_some("a shell, script interpreter or launcher runs other code and is not a GUI app")
}

/// Interpreter-style arguments (`-c`, `-e`, `--eval`, `-m`, `/c` ...): refused for every target.
pub fn refused_arg(arg: &str) -> bool {
    let a = arg.to_lowercase();
    matches!(
        a.as_str(),
        "-c" | "-e"
            | "-m"
            | "-r"
            | "-p"
            | "-x"
            | "--eval"
            | "--exec"
            | "--command"
            | "--print"
            | "--cmd"
            | "-command"
            | "-encodedcommand"
            | "-enc"
            | "-file"
            | "/c"
            | "/k"
            | "/r"
            | "--run"
    ) || a.starts_with("--eval=")
        || a.starts_with("--command=")
        || a.starts_with("--exec=")
}

/// `press` combos allowed inside an app: a plain named key, or one of ctrl/cmd/alt (optionally
/// with shift) plus a single letter or digit. No window-manager, screenshot, lock, quit, hide or
/// task-switch shortcut (cmd+space, cmd+tab, cmd+q, cmd+h, cmd+m, cmd+shift+3/4/5, ctrl+cmd+q,
/// alt+f4, ctrl+alt+delete, any super/win key ...).
pub fn press_allowed(combo: &str) -> Result<(), &'static str> {
    let c = combo.to_lowercase();
    let parts: Vec<&str> = c.split('+').collect();
    let (key, mods) = parts.split_last().ok_or("empty combo")?;
    const PLAIN: &[&str] = &[
        "return",
        "enter",
        "escape",
        "esc",
        "tab",
        "space",
        "backspace",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "f1",
        "f2",
        "f3",
        "f4",
        "f5",
        "f6",
        "f7",
        "f8",
        "f9",
        "f10",
        "f11",
        "f12",
    ];
    let single = key.len() == 1 && key.bytes().all(|b| b.is_ascii_alphanumeric());
    if mods.is_empty() {
        return if PLAIN.contains(key) || single {
            Ok(())
        } else {
            Err("unknown key")
        };
    }
    if !single {
        return Err("a combo with a modifier may only use a letter or digit");
    }
    let (mut shift, mut other) = (0, vec![]);
    for m in mods {
        match *m {
            "shift" => shift += 1,
            "ctrl" | "control" => other.push("ctrl"),
            "cmd" | "command" => other.push("cmd"),
            "alt" | "option" => other.push("alt"),
            _ => return Err("this modifier is not allowed (super/win/meta keys reach the system)"),
        }
    }
    if shift > 1 || other.len() != 1 {
        return Err("exactly one of ctrl, cmd or alt (plus optionally shift) is allowed");
    }
    let m = other[0];
    if m == "cmd"
        && (matches!(*key, "q" | "h" | "m")
            || (shift == 1 && key.bytes().all(|b| b.is_ascii_digit())))
    {
        return Err("this shortcut quits, hides or captures outside the app");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_by_origin_and_mode_plan_never() {
        for m in Mode::ALL {
            let (p, e) = (gate(Origin::Project, m), gate(Origin::External, m));
            match m {
                Mode::Plan => assert!(matches!((&p, &e), (Gate::Deny(_), Gate::Deny(_)))),
                Mode::Default => assert_eq!((p, e), (Gate::Allow, Gate::Ask)),
                _ => assert!(p == Gate::Allow && matches!(e, Gate::Deny(_)), "{m:?}"),
            }
        }
    }

    #[test]
    fn app_ids_carry_their_origin() {
        assert_eq!(origin_of_app_id("ext-3"), Origin::External);
        assert_eq!(origin_of_app_id("app-1"), Origin::Project);
    }

    #[test]
    fn hard_deny_matches_file_names_and_bundles_not_parent_directories() {
        for id in [
            "com.apple.Safari",
            "COM.1PASSWORD.1PASSWORD",
            "/Applications/1Password.app/Contents/MacOS/1Password",
            r"C:\Program Files\KeePass Password Safe 2\KeePass.exe",
            r"C:\Windows\System32\control.exe",
            r"C:\Windows\explorer.exe",
            "/usr/bin/firefox",
            "Keychain Access",
            "KeePassXC",
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
            r"C:\Windows\System32\WindowsTerminal.exe",
            "/usr/bin/gnome-terminal",
            "/Applications/iTerm.app/Contents/MacOS/iTerm2",
            "/System/Applications/Passwords.app/Contents/MacOS/Passwords",
            "/Applications/Proton Pass.app/Contents/MacOS/Proton Pass",
            "/Applications/AnyDesk.app/Contents/MacOS/AnyDesk",
            "/Applications/Tor Browser.app/Contents/MacOS/firefox",
            "/System/Applications/Utilities/Activity Monitor.app/Contents/MacOS/Activity Monitor",
            "/System/Library/CoreServices/SecurityAgent.app/Contents/MacOS/SecurityAgent",
            "Script Editor",
        ] {
            assert!(hard_denied(id).is_some(), "{id}");
        }
        for id in [
            "/work/proj/target/debug/safari-tests",
            "/work/proj/dist/MyApp.app",
            "controller",
            "/home/u/firefox/projects/myapp",
            "/home/u/Terminal/work/app",
            r"C:\Users\u\chrome\proj\app.exe",
        ] {
            assert!(hard_denied(id).is_none(), "{id}");
        }
    }

    #[test]
    fn shells_interpreters_and_interpreter_args_are_refused() {
        for t in [
            "/bin/sh",
            "/usr/bin/python3",
            "/usr/bin/python3.12",
            r"C:\Windows\System32\cmd.exe",
            "/x/node",
            "/x/OSASCRIPT",
            "/usr/bin/env",
            "/x/pwsh.exe",
            "/usr/bin/java",
        ] {
            assert!(refused_target(t).is_some(), "{t}");
        }
        for t in [
            "/work/proj/target/debug/myapp",
            "/work/proj/shell-app",
            "/work/pythonista",
        ] {
            assert!(refused_target(t).is_none(), "{t}");
        }
        for a in [
            "-c",
            "-e",
            "--eval",
            "--eval=1+1",
            "-m",
            "/c",
            "-Command",
            "--command=x",
        ] {
            assert!(refused_arg(a), "{a}");
        }
        for a in ["--config", "file.txt", "-v", "--port=80", "-h"] {
            assert!(!refused_arg(a), "{a}");
        }
    }

    #[test]
    fn press_allows_plain_keys_and_single_modifier_letters_only() {
        for ok in [
            "return",
            "escape",
            "tab",
            "a",
            "7",
            "f5",
            "cmd+c",
            "ctrl+z",
            "alt+f",
            "shift+cmd+z",
            "ctrl+shift+t",
            "Cmd+V",
        ] {
            assert!(press_allowed(ok).is_ok(), "{ok}");
        }
        for bad in [
            "cmd+space",
            "cmd+tab",
            "cmd+q",
            "cmd+h",
            "cmd+m",
            "cmd+alt+esc",
            "cmd+option+escape",
            "ctrl+alt+delete",
            "alt+tab",
            "alt+f4",
            "win+d",
            "super+l",
            "meta+r",
            "ctrl+cmd+q",
            "cmd+shift+3",
            "cmd+shift+4",
            "cmd+shift+5",
            "cmd+shift+q",
            "ctrl+alt+t",
            "cmd",
            "shift+shift+a",
            "volumeup",
            "ctrl+shift+escape",
            "",
        ] {
            assert!(press_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dialog_text_shows_the_whole_command_line_without_spoofing_characters() {
        let v = serde_json::json!({
            "target": "/opt/safe\u{202E}gpj.exe",
            "args": ["--a\u{200B}b", "x\ny", "q".repeat(500)],
            "cwd": "/w\u{2066}d",
        });
        let d = describe_launch(&v);
        assert!(!d.chars().any(is_spoofing), "{d:?}");
        assert!(
            d.contains("safegpj.exe") && d.contains("--ab") && d.contains("3 argument(s)"),
            "{d}"
        );
        assert!(
            d.contains("chars cut]")
                && d.contains("cwd")
                && d.contains("environment additions: none"),
            "{d}"
        );
        let many = serde_json::json!({"target": "/a", "args": (0..30).map(|i| i.to_string()).collect::<Vec<_>>()});
        assert!(describe_launch(&many).contains("+10 more arguments not shown"));
    }

    #[test]
    fn sanitize_drops_control_and_bidi_and_shows_the_cut() {
        assert_eq!(sanitize("a\nb\u{1b}[31mc\u{202E}d"), "ab[31mcd");
        assert!(sanitize(&"x".repeat(500)).ends_with("[+380 chars cut]"));
    }

    #[test]
    fn launch_command_line_quotes_every_word() {
        let c = launch_command_line("/a b/app", &["it's".into(), "$(x)".into()], Some("/w"));
        assert_eq!(c, "cd '/w' && '/a b/app' 'it'\\''s' '$(x)'");
    }
}
