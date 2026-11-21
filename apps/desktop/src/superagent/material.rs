//! Window material: opaque everywhere, glass only on macOS 27+ when the user setting allows it
//! (super-agent-v3 DESIGN-NOTES section 4). Detection only; the native glass view is not built here.

use synthesis_settings::UiSettings;

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

/// Glass needs a supported system and the user's `synt.tui.blur` (the existing opt-in for what is behind the
/// window; off by default, like the mock's "macOS 27 glass" switch). Anything else is opaque.
pub fn material_for(settings: &UiSettings, os: Os, version: Option<&str>) -> Material {
    if settings.blur && version.is_some_and(|v| glass_supported(os, v, None)) {
        Material::Glass
    } else {
        Material::Opaque
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
    fn material_follows_the_blur_setting_and_the_system() {
        let mut s = UiSettings::default();
        assert_eq!(material_for(&s, Os::Mac, Some("27.0")), Material::Opaque);
        s.blur = true;
        assert_eq!(material_for(&s, Os::Mac, Some("27.0")), Material::Glass);
        assert_eq!(material_for(&s, Os::Mac, Some("26.4")), Material::Opaque);
        assert_eq!(material_for(&s, Os::Mac, None), Material::Opaque);
        for os in [Os::Windows, Os::Linux] {
            assert_eq!(material_for(&s, os, Some("27.0")), Material::Opaque);
        }
    }

    #[test]
    fn macos_version_only_on_macos() {
        assert_eq!(macos_version().is_some(), cfg!(target_os = "macos"));
    }
}
