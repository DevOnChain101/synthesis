//! Cross-platform helpers for pitfalls hit in the Node CLI.

use std::path::{Path, PathBuf};

/// Absolute path of a Windows system tool under `System32` (avoids PATH hijacking).
#[cfg(windows)]
pub fn system32(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

/// Non-Windows: no System32, so the bare name is resolved via PATH.
#[cfg(not(windows))]
pub fn system32(name: &str) -> PathBuf {
    PathBuf::from(name)
}

/// Expands `~`, `~/x` and `~\x` against the home dir; `~user` is left untouched.
pub fn expand_tilde(s: &str) -> PathBuf {
    let rest = if s == "~" {
        ""
    } else if let Some(r) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        r
    } else {
        return PathBuf::from(s);
    };
    match home_dir() {
        Some(home) if rest.is_empty() => home,
        Some(home) => home.join(rest),
        None => PathBuf::from(s),
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

/// Key for comparing paths: case-folded on Windows and macOS, verbatim elsewhere.
/// Never rewrites `\` (a valid filename char on POSIX).
pub fn normalize_for_match(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(any(windows, target_os = "macos")) {
        s.to_lowercase()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_variants() {
        let home = home_dir().expect("home set");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x"), home.join("x"));
        assert_eq!(expand_tilde("~\\x"), home.join("x"));
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
        assert_eq!(expand_tilde("a/~"), PathBuf::from("a/~"));
    }

    #[test]
    fn normalize_keeps_backslash() {
        let n = normalize_for_match(Path::new("a\\B"));
        assert!(n.contains('\\'));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn normalize_folds_case() {
        assert_eq!(normalize_for_match(Path::new("/Foo/BAR")), "/foo/bar");
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn normalize_keeps_case() {
        assert_eq!(normalize_for_match(Path::new("/Foo/BAR")), "/Foo/BAR");
    }

    #[cfg(windows)]
    #[test]
    fn system32_is_absolute() {
        let p = system32("taskkill.exe");
        assert!(p.is_absolute() && p.ends_with("System32\\taskkill.exe"));
    }

    #[cfg(not(windows))]
    #[test]
    fn system32_passthrough() {
        assert_eq!(system32("x"), PathBuf::from("x"));
    }
}

pub mod wsl;
