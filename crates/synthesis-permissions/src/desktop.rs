//! Desktop-control policy (docs/DESKTOP-CONTROL.md, owner policy 2026-11-03). Pure functions
//! shared by the permission engine, the harness and `synthesis-desktop`, so all three agree.
//!
//! - An app Synthesis launched from the project is allowed in every permission mode.
//! - Any other app needs a dialog per app per session and exists only in `default` mode.
//! - The hard-deny list below is never lifted by an approval, a rule or a mode.

use crate::Mode;
#[cfg(unix)]
use sha2::{Digest, Sha256};

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

/// Largest executable that is hashed for launch evidence and for the tool's recheck. Both use
/// this one cap, so the file the engine identified is the file the tool compares (bigger: refused).
pub const MAX_HASH_BYTES: u64 = 256 * 1024 * 1024;

/// Evidence for the exact executable judged by the engine. The harness creates this before an
/// approval, and the tool compares it again immediately before spawning. It is carried to the
/// tool in ToolContext, not accepted from model input.
#[cfg(unix)]
pub fn launch_token(
    target: &str,
    origin: Origin,
    external_approved: bool,
) -> Result<String, String> {
    let real = resolve_executable(target)?.ok_or("the executable does not exist")?;
    let meta = std::fs::metadata(&real).map_err(|_| "cannot stat the executable")?;
    if !meta.is_file() {
        return Err("the executable is not a file".into());
    }
    if meta.len() > MAX_HASH_BYTES {
        return Err("the executable is too large to be identified (over 256 MiB)".into());
    }
    let mut file = std::fs::File::open(&real).map_err(|_| "cannot read the executable")?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|_| "cannot hash the executable")?;
    let modified = meta
        .modified()
        .map_err(|_| "cannot read modification time")?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid modification time")?;
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    Ok(serde_json::json!({
        "path": real.to_string_lossy(), "size": meta.len(),
        "mtime": modified.as_nanos().to_string(), "sha256": format!("{:x}", hash.finalize()),
        "dev": identity.0, "ino": identity.1,
        "origin": if origin == Origin::External { "external" } else { "project" },
        "external_approved": external_approved,
    })
    .to_string())
}

/// Windows has no stable standard-library file identity and no enforceable desktop sandbox.
/// Keep approval evidence unavailable so enabling a future backend cannot silently allow launch.
#[cfg(not(unix))]
pub fn launch_token(
    _target: &str,
    _origin: Origin,
    _external_approved: bool,
) -> Result<String, String> {
    Err("desktop launch identity is unavailable on this platform".into())
}

/// Binds evidence to one planned tool call. The plan consumes this value when it runs that call.
pub fn scoped_launch_token(
    task: &str,
    turn: &str,
    call: &str,
    target: &str,
    origin: Origin,
    external_approved: bool,
) -> Result<String, String> {
    Ok(format!(
        "{task}:{turn}:{call}:{}",
        launch_token(target, origin, external_approved)?
    ))
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
            "microsoft authenticator",
            "google authenticator",
            "raivo",
            "raivo otp",
            "2fas",
            "keeper",
            "keeper password manager",
            "exodus",
            "electrum",
            "ledger live",
            "ledger",
            "trezor",
            "trezor suite",
            "keychain",
            "passwörter",
            "mots de passe",
            "contraseñas",
            "パスワード",
            "mật khẩu",
            "schlüsselbundverwaltung",
            "trousseau d’accès",
            "trousseau d'accès",
            "acceso a llaveros",
            "キーチェーンアクセス",
            "truy cập chuỗi khóa",
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
            "ターミナル",
            "putty",
            "mintty",
            "wsl",
            "x-terminal-emulator",
            "regedit",
            "mmc",
            "consent",
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
            "screen sharing",
            "com.apple.screensharing",
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
            "disk utility",
            "com.apple.diskutility",
            "console",
            "com.apple.console",
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
        for suf in [".exe", ".app", ".bat", ".cmd", ".lnk", ".appimage"] {
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

/// Like [`hard_denied`], and also when a denied name stands as a whole word inside `name` (a
/// bundle's display name such as "1Password 7 - Password Manager" or "Authy Desktop", or a bundle
/// id with a suffix). Names under 5 characters are exact-only: "wt", "arc", "dock" would hit too
/// many ordinary apps. A hint for bundle metadata and installed-app scans, not for plain file
/// names (`console-app` stays launchable).
pub fn hard_denied_loose(name: &str) -> Option<&'static str> {
    if let Some(f) = hard_denied(name) {
        return Some(f);
    }
    let low = name.trim().to_lowercase();
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    DENIED.iter().find_map(|(family, names)| {
        names
            .iter()
            .filter(|n| n.chars().count() >= 5)
            .any(|n| {
                low.match_indices(n).any(|(i, _)| {
                    !word(low[..i].chars().next_back()) && !word(low[i + n.len()..].chars().next())
                })
            })
            .then_some(*family)
    })
}

/// Characters that can reorder or hide text on screen: control, bidi embeddings, overrides and
/// isolates, zero-width and other invisible format characters.
pub fn is_spoofing(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{3164}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{FFA0}'
                | '\u{2800}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
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

/// Most arguments a launch may carry: the dialog shows all of them.
pub const MAX_ARGS: usize = 20;

/// The executable that would really run for `target`: links resolved, and for an `.app` bundle
/// the single file in `Contents/MacOS`, which must itself resolve inside the bundle. `Ok(None)`
/// when `target` does not exist (nothing to resolve).
pub fn resolve_executable(target: &str) -> Result<Option<std::path::PathBuf>, String> {
    // Windows canonical paths carry a `\\?\` prefix that no other path in the engine has
    let plain = |p: std::path::PathBuf| -> std::path::PathBuf {
        let t = p.to_string_lossy();
        match t.strip_prefix(r"\\?\") {
            Some(r) if !r.starts_with("UNC") => r.into(),
            _ => p,
        }
    };
    let Ok(real) = std::fs::canonicalize(target).map(plain) else {
        return Ok(None);
    };
    if !(real.is_dir()
        && real
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app")))
    {
        return Ok(Some(real));
    }
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(real.join("Contents/MacOS"))
        .map_err(|_| "the bundle has no Contents/MacOS".to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    let [one] = files.as_slice() else {
        return Err("cannot tell which executable of the bundle to run (it needs exactly one file in Contents/MacOS)".into());
    };
    let inner = std::fs::canonicalize(one)
        .map(plain)
        .map_err(|_| "the bundle's executable cannot be resolved".to_string())?;
    if !inner.starts_with(&real) || !inner.is_file() {
        return Err("the bundle's executable resolves outside the bundle".into());
    }
    Ok(Some(inner))
}

/// The whole command line of a launch as the person must see it: target (and the executable it
/// resolves to when that differs), every argument, cwd and the environment, each sanitized, cuts
/// shown.
pub fn describe_launch(input: &serde_json::Value) -> String {
    let get = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    let mut out = format!("target {:?}", sanitize_to(get("target"), 300));
    match resolve_executable(get("target")) {
        Ok(Some(r)) if r != std::path::Path::new(get("target")) => {
            out += &format!(
                " (runs the executable {:?})",
                sanitize_to(&r.to_string_lossy(), 300)
            );
        }
        Err(e) => out += &format!(" (cannot be resolved: {e})"),
        _ => {}
    }
    let args: Vec<&str> = input
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    if args.is_empty() {
        out += ", no arguments";
    } else {
        out += &format!(", {} argument(s):", args.len());
        for a in &args {
            out += &format!(" {:?}", sanitize_to(a, 200));
        }
    }
    let cwd = get("cwd");
    out += &if cwd.is_empty() {
        ", in the session directory".to_owned()
    } else {
        format!(", cwd {:?}", sanitize_to(cwd, 300))
    };
    out + ", environment: the session's allowlisted variables plus DISPLAY when the desktop has one, nothing else"
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
    "xcrun",
    "arch",
    "caffeinate",
    "time",
    "systemd-run",
    "flatpak",
    "gtk-launch",
    "wsl",
    "exec",
    "command",
    "setsid",
    "script",
    "stdbuf",
    "chroot",
    "strace",
    "ltrace",
    "gdb",
    "lldb",
    "electron",
    "npx",
    "npm",
    "yarn",
    "pnpm",
    "cargo",
    "go",
    "rustc",
    "gcc",
    "cc",
    "clang",
    "swift",
    "osacompile",
    "automator",
    "sandbox-exec",
    "ruby3",
    "find",
    "git",
    "vim",
    "nvim",
    "emacs",
    "luajit",
    "rscript",
    "julia",
    "sed",
    "cmake",
    "gradle",
    "mvn",
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

/// Script file extensions: a program given one runs it (hints, not a guarantee).
const SCRIPT_EXT: &[&str] = &[
    ".js",
    ".mjs",
    ".cjs",
    ".ts",
    ".py",
    ".pyw",
    ".rb",
    ".pl",
    ".php",
    ".sh",
    ".bash",
    ".zsh",
    ".ps1",
    ".bat",
    ".cmd",
    ".vbs",
    ".wsf",
    ".lua",
    ".asar",
    ".jar",
    ".scpt",
    ".applescript",
];

/// Interpreter-style, debugger and profile-redirecting arguments: refused for every target. This
/// is a list of known dangerous spellings: hints, not a guarantee (the sandbox and the Bash
/// verdict are the control).
pub fn refused_arg(arg: &str) -> bool {
    let a = arg.to_lowercase();
    const EXACT: &[&str] = &[
        "-c",
        "-e",
        "-m",
        "-r",
        "-p",
        "-x",
        "-lc",
        "-ic",
        "-pe",
        "-ne",
        "-le",
        "-ec",
        "-xc",
        "-ce",
        "-command",
        "-encodedcommand",
        "-enc",
        "-file",
        "-exec",
        "-eval",
        "/c",
        "/k",
        "/r",
    ];
    const PREFIX: &[&str] = &[
        "--eval",
        "--exec",
        "--command",
        "--print",
        "--cmd",
        "--run",
        "--require",
        "--import",
        "--inspect",
        "--remote-debugging",
        "--remote-allow-origins",
        "--js-flags",
        "--user-data-dir",
        "--load-extension",
        "--no-sandbox",
        "--disable-web-security",
        "--disable-gpu-sandbox",
        "--flag-switches",
        "--renderer-cmd-prefix",
        "--utility-cmd-prefix",
    ];
    EXACT.contains(&a.as_str())
        || PREFIX.iter().any(|p| a.starts_with(p))
        || (!a.starts_with('-') && SCRIPT_EXT.iter().any(|e| a.ends_with(e)))
        || ((a.starts_with('/')
            || a.starts_with("./")
            || a.starts_with("../")
            || a.starts_with('\\')
            || a.as_bytes().get(1) == Some(&b':'))
            && std::path::Path::new(&a).extension().is_none())
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

    #[cfg(unix)]
    #[test]
    fn launch_evidence_binds_origin_approval_and_file_identity() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("app");
        std::fs::write(&p, b"one").unwrap();
        let path = p.to_str().unwrap();
        let approved = launch_token(path, Origin::External, true).unwrap();
        assert_eq!(
            approved,
            launch_token(path, Origin::External, true).unwrap()
        );
        assert_ne!(
            approved,
            launch_token(path, Origin::Project, false).unwrap()
        );
        assert_ne!(
            approved,
            launch_token(path, Origin::External, false).unwrap()
        );
        std::fs::write(&p, b"two").unwrap();
        assert_ne!(
            approved,
            launch_token(path, Origin::External, true).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn launch_evidence_changes_when_a_link_is_retargeted() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let first = d.path().join("first");
        let second = d.path().join("second");
        let link = d.path().join("app");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        symlink(&first, &link).unwrap();
        let before = launch_token(link.to_str().unwrap(), Origin::Project, false).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        assert_ne!(
            before,
            launch_token(link.to_str().unwrap(), Origin::External, true).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_evidence_is_unavailable_on_windows() {
        for origin in [Origin::Project, Origin::External] {
            assert!(launch_token(r"C:\\app.exe", origin, origin == Origin::External).is_err());
        }
    }

    #[test]
    fn interpreter_flags_and_extensionless_paths_are_refused() {
        for arg in [
            "-exec",
            "-eval",
            "-e",
            "-c",
            "-command",
            "-enc",
            "-encodedcommand",
            "-file",
            "./run-script",
            r"C:\\work\\run-script",
        ] {
            assert!(refused_arg(arg), "{arg}");
        }
        for name in [
            "find", "git", "vim", "nvim", "emacs", "luajit", "Rscript", "julia", "sed", "cmake",
            "gradle", "mvn",
        ] {
            assert!(refused_target(name).is_some(), "{name}");
        }
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
            "f1",
            "f5",
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
            d.contains("chars cut]") && d.contains("cwd") && d.contains("DISPLAY"),
            "{d}"
        );
        // every argument is shown (the launch refuses more than MAX_ARGS)
        let many = serde_json::json!({"target": "/a", "args": (0..MAX_ARGS).map(|i| i.to_string()).collect::<Vec<_>>()});
        let d = describe_launch(&many);
        assert!(d.contains("\"19\"") && !d.contains("not shown"), "{d}");
    }

    #[test]
    fn sanitize_drops_control_and_bidi_and_shows_the_cut() {
        assert_eq!(sanitize("a\nb\u{1b}[31mc\u{202E}d"), "ab[31mcd");
        assert!(sanitize(&"x".repeat(500)).ends_with("[+380 chars cut]"));
    }

    #[test]
    fn more_spoofing_characters_are_dropped() {
        let t = "a\u{E0041}\u{3164}\u{2800}\u{2028}\u{FE0F}\u{E0101}\u{202D}b";
        assert_eq!(sanitize(t), "ab");
    }

    #[test]
    fn localized_and_wider_names_are_denied_and_suffixes_are_stripped() {
        for id in [
            "Passwörter",
            "Mots de passe",
            "Contraseñas",
            "パスワード",
            "Mật khẩu",
            "Schlüsselbundverwaltung",
            "Acceso a Llaveros",
            "キーチェーンアクセス",
            "Screen Sharing",
            "Console",
            "Disk Utility",
            "Microsoft Authenticator",
            "Raivo OTP",
            "2FAS",
            "Keeper",
            "Exodus",
            "Electrum",
            "Ledger Live",
            "Trezor Suite",
            "/x/putty.exe",
            "/x/MobaXterm/mintty.exe",
            "regedit.exe",
            "mmc.exe",
            "wsl.exe",
            "/usr/bin/x-terminal-emulator",
            "/home/u/Firefox.AppImage",
            "/x/Terminal.lnk",
        ] {
            assert!(hard_denied(id).is_some(), "{id}");
        }
    }

    #[test]
    fn arguments_that_run_code_or_redirect_a_profile_are_refused() {
        for a in [
            "-lc",
            "-ic",
            "-pe",
            "--require=./x.js",
            "--inspect",
            "--inspect-brk=9229",
            "--remote-debugging-port=9222",
            "--remote-allow-origins=*",
            "--js-flags=--expose-gc",
            "--user-data-dir=/tmp/p",
            "app.js",
            "run.PY",
            "x.ps1",
            "main.asar",
            "--import=x",
            "--no-sandbox",
        ] {
            assert!(refused_arg(a), "{a}");
        }
        for a in [
            "--port=80",
            "readme.txt",
            "--config",
            "-v",
            "--theme=dark",
            "photo.png",
        ] {
            assert!(!refused_arg(a), "{a}");
        }
        for t in [
            "/x/xcrun",
            "/x/arch",
            "/x/caffeinate",
            "/x/nohup",
            "/x/sudo",
            "/x/flatpak",
            "/x/gtk-launch",
            "/x/xdg-open",
            "/x/wsl.exe",
            "/x/busybox",
            "/x/electron",
            "/x/systemd-run",
        ] {
            assert!(refused_target(t).is_some(), "{t}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_resolves_to_its_one_inner_executable_inside_the_bundle() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let r = std::fs::canonicalize(d.path()).unwrap();
        let app = |n: &str| {
            let p = r.join(format!("{n}.app/Contents/MacOS"));
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let ok = app("Good");
        std::fs::write(ok.join("good"), "x").unwrap();
        let bundle = r.join("Good.app");
        assert_eq!(
            resolve_executable(bundle.to_str().unwrap()).unwrap(),
            Some(ok.join("good"))
        );
        let two = app("Two");
        std::fs::write(two.join("a"), "x").unwrap();
        std::fs::write(two.join("b"), "x").unwrap();
        assert!(resolve_executable(r.join("Two.app").to_str().unwrap()).is_err());
        let out = r.join("outside-bin");
        std::fs::write(&out, "x").unwrap();
        let ln = app("Link");
        symlink(&out, ln.join("link")).unwrap();
        assert!(resolve_executable(r.join("Link.app").to_str().unwrap()).is_err());
        let dir = r.join("outside-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x"), "x").unwrap();
        std::fs::create_dir_all(r.join("DirLink.app/Contents")).unwrap();
        symlink(&dir, r.join("DirLink.app/Contents/MacOS")).unwrap();
        assert!(resolve_executable(r.join("DirLink.app").to_str().unwrap()).is_err());
        assert_eq!(
            resolve_executable(out.to_str().unwrap()).unwrap(),
            Some(out.clone())
        );
        assert_eq!(
            resolve_executable(r.join("nope").to_str().unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn display_names_and_ids_of_denied_apps_match_as_whole_words() {
        for n in [
            "1Password 7 - Password Manager",
            "Authy Desktop",
            "com.1password.1password.helper",
            "KeePassXC",
            "Bitwarden (beta)",
            "My Ledger Live",
        ] {
            assert!(hard_denied_loose(n).is_some(), "{n}");
        }
        for n in [
            "TerminalUI",
            "bitwardenish",
            "ledgerly",
            "Passwordless Demo",
            "My App",
        ] {
            assert!(hard_denied_loose(n).is_none(), "{n}");
        }
        // plain names stay exact: only bundle metadata and installed-app scans use the loose form
        assert!(hard_denied("Authy Desktop").is_none());
    }

    #[test]
    fn launch_command_line_quotes_every_word() {
        let c = launch_command_line("/a b/app", &["it's".into(), "$(x)".into()], Some("/w"));
        assert_eq!(c, "cd '/w' && '/a b/app' 'it'\\''s' '$(x)'");
    }
}
