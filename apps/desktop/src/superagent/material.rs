//! Window material: opaque everywhere, glass only on macOS 27+ when the user setting allows it
//! (super-agent-v3 DESIGN-NOTES section 4). Glass is GPUI's `Blurred` window background plus translucent Super Agent
//! fills (`tokens::surface_fill`); the native `NSGlassEffectView` is not used.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::WindowBackgroundAppearance;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Material {
    Opaque,
    Glass,
}

/// macOS with a major version of 27 or more, and the user has not asked the system for reduced transparency.
/// `None` means unknown and does not block (the caller reads the accessibility flag when it can).
pub fn glass_supported(os: Os, version: &str, reduce_transparency: Option<bool>) -> bool {
    os == Os::Mac
        && reduce_transparency != Some(true)
        && version
            .split('.')
            .next()
            .and_then(|m| m.trim().parse::<u32>().ok())
            .is_some_and(|m| m >= 27)
}

/// `sw_vers -productVersion` (absolute path, no shell). `None` off macOS or when it fails.
pub fn macos_version() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !v.is_empty()).then_some(v)
}

/// The Super Agent fills read this; one window per process, so a global like `theme::OPACITY`.
static GLASS: AtomicBool = AtomicBool::new(false);

pub fn set(m: Material) {
    GLASS.store(m == Material::Glass, Ordering::Relaxed);
}

pub fn current() -> Material {
    if GLASS.load(Ordering::Relaxed) {
        Material::Glass
    } else {
        Material::Opaque
    }
}

/// `defaults read com.apple.universalaccess reduceTransparency`: `Some(true)` when the system asks for reduced
/// transparency; `None` when it cannot be read (the key is absent when never set).
fn reduce_transparency() -> Option<bool> {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.universalaccess", "reduceTransparency"])
        .output()
        .ok()?;
    parse_flag(&String::from_utf8(out.stdout).ok()?)
}

fn parse_flag(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// `synt.desktop.material` (`opaque` default, `glass`) on this system. Only an explicit `glass` on a supporting
/// system is glass; anything else is opaque (and nothing is spawned unless the choice is `glass`).
pub fn material_for(
    choice: Option<&str>,
    os: Os,
    version: Option<&str>,
    reduce: Option<bool>,
) -> Material {
    if choice == Some("glass") && version.is_some_and(|v| glass_supported(os, v, reduce)) {
        Material::Glass
    } else {
        Material::Opaque
    }
}

/// [`material_for`] on this machine.
pub fn detect(choice: Option<&str>) -> Material {
    if choice != Some("glass") || !cfg!(target_os = "macos") {
        return Material::Opaque;
    }
    material_for(
        choice,
        Os::current(),
        macos_version().as_deref(),
        reduce_transparency(),
    )
}

/// Whether the Settings row may offer glass here (version only, read once).
pub fn os_supports_glass() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| macos_version().is_some_and(|v| glass_supported(Os::current(), &v, None)))
}

/// `synt.desktop.material` of the user settings file; a missing or unreadable file is `None` (opaque).
pub fn read_choice(home: &std::path::Path) -> Option<String> {
    let doc =
        synthesis_settings::read_json(&home.join("settings.json"), "user settings").ok()??;
    doc.get("synt")?
        .get("desktop")?
        .get("material")?
        .as_str()
        .map(Into::into)
}

/// What the window asks the platform for: glass is `Blurred`, anything else is the TUI's own rule.
pub fn background(tui: &synthesis_settings::UiSettings, m: Material) -> WindowBackgroundAppearance {
    if m == Material::Glass {
        WindowBackgroundAppearance::Blurred
    } else {
        crate::theme::background(tui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_table() {
        for (v, want) in [
            ("14.5", false),
            ("15.7", false),
            ("26.4", false),
            ("27.0", true),
            ("27.1", true),
            ("garbage", false),
            ("", false),
        ] {
            assert_eq!(glass_supported(Os::Mac, v, None), want, "{v:?}");
            assert!(!glass_supported(Os::Windows, v, None));
            assert!(!glass_supported(Os::Linux, v, None));
        }
        assert!(!glass_supported(Os::Mac, "27.0", Some(true)));
        assert!(glass_supported(Os::Mac, "27.0", Some(false)));
    }

    #[test]
    fn material_resolution() {
        use Material::*;
        let g = Some("glass");
        for (choice, os, v, reduce, want) in [
            (g, Os::Mac, Some("27.0"), None, Glass),
            (g, Os::Mac, Some("27.1"), Some(false), Glass),
            (g, Os::Mac, Some("26.9"), None, Opaque),
            (g, Os::Mac, Some("garbage"), None, Opaque),
            (g, Os::Mac, None, None, Opaque),
            (g, Os::Mac, Some("27.0"), Some(true), Opaque),
            (g, Os::Windows, Some("27.0"), None, Opaque),
            (g, Os::Linux, Some("27.0"), None, Opaque),
            (Some("opaque"), Os::Mac, Some("27.0"), None, Opaque),
            (None, Os::Mac, Some("27.0"), None, Opaque),
            (Some("Glass "), Os::Mac, Some("27.0"), None, Opaque),
        ] {
            assert_eq!(
                material_for(choice, os, v, reduce),
                want,
                "{choice:?} {os:?} {v:?} {reduce:?}"
            );
        }
    }

    #[test]
    fn the_flag_and_the_background() {
        assert_eq!(parse_flag("1\n"), Some(true));
        assert_eq!(parse_flag("0"), Some(false));
        assert_eq!(parse_flag("does not exist"), None);
        let tui = synthesis_settings::UiSettings::default();
        assert_eq!(
            background(&tui, Material::Glass),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            background(&tui, Material::Opaque),
            WindowBackgroundAppearance::Opaque
        );
        assert_eq!(detect(None), Material::Opaque);
        if !cfg!(target_os = "macos") {
            assert_eq!(detect(Some("glass")), Material::Opaque);
        }
    }

    #[test]
    fn the_choice_is_read_from_the_user_file() {
        let d = std::env::temp_dir().join(format!("synt-material-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(read_choice(&d), None);
        std::fs::write(
            d.join("settings.json"),
            r#"{"synt":{"desktop":{"material":"glass"}}}"#,
        )
        .unwrap();
        assert_eq!(read_choice(&d).as_deref(), Some("glass"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn macos_version_only_on_macos() {
        assert_eq!(macos_version().is_some(), cfg!(target_os = "macos"));
    }
}
