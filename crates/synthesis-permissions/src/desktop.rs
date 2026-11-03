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

pub const EXTERNAL_ONLY_MANUAL: &str = "controlling an app that Synthesis did not launch from this project works only in manual (default) permission mode, with a dialog per app; no rule or other mode can approve it";

/// Rule (a) and (b): the decision for one call by where the app came from and the mode.
pub fn gate(origin: Origin, mode: Mode) -> Gate {
    match (origin, mode) {
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
        ],
    ),
];

/// Case-folded identifiers a launch target or a window's app name can be known by: the whole
/// string, the file name, and the file name without a `.exe`/`.app` suffix; for a path, also
/// every `.app` component (so `X.app/Contents/MacOS/anything` is X).
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
    let mut v = vec![s.trim().to_lowercase(), strip(s)];
    for part in s.split(['/', '\\']).filter(|p| !p.is_empty()) {
        v.push(strip(part));
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

/// Text safe for a dialog or an event: control characters dropped, length capped.
pub fn sanitize(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(120).collect();
    if s.chars().count() > 120 {
        format!("{t}...")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_apps_are_allowed_in_every_mode_external_only_in_default() {
        for m in Mode::ALL {
            assert_eq!(gate(Origin::Project, m), Gate::Allow, "{m:?}");
            let g = gate(Origin::External, m);
            if m == Mode::Default {
                assert_eq!(g, Gate::Ask);
            } else {
                assert!(matches!(g, Gate::Deny(_)), "{m:?} -> {g:?}");
            }
        }
    }

    #[test]
    fn app_ids_carry_their_origin() {
        assert_eq!(origin_of_app_id("ext-3"), Origin::External);
        assert_eq!(origin_of_app_id("app-1"), Origin::Project);
    }

    #[test]
    fn hard_deny_matches_exact_names_case_folded() {
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
        ] {
            assert!(hard_denied(id).is_some(), "{id}");
        }
        // exact match, not substring: the user's own app named like a denied word is fine
        for id in [
            "/work/proj/target/debug/safari-tests",
            "/work/proj/dist/MyApp.app",
            "controller",
        ] {
            assert!(hard_denied(id).is_none(), "{id}");
        }
    }

    #[test]
    fn sanitize_drops_control_characters_and_caps() {
        assert_eq!(sanitize("a\nb\u{1b}[31mc"), "ab[31mc");
        assert!(sanitize(&"x".repeat(500)).len() < 130);
    }
}
