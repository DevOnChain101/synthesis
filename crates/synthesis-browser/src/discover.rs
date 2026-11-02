//! Find an installed Chrome-family browser. Never downloads one.

use crate::error::{BrowserError, Result};
use std::path::{Path, PathBuf};
use synthesis_config::Env;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

/// Windows env names are case-insensitive.
fn var<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, v)| k.eq_ignore_ascii_case(name) && !v.is_empty())
        .map(|(_, v)| v)
}

/// Candidate binaries in priority order (Chrome, Chromium, Edge).
pub fn candidates(os: Os, env: &Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match os {
        Os::Mac => {
            let apps = [
                "Google Chrome.app/Contents/MacOS/Google Chrome",
                "Chromium.app/Contents/MacOS/Chromium",
                "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ];
            let mut roots = vec![PathBuf::from("/Applications")];
            if let Some(h) = var(env, "HOME") {
                roots.push(Path::new(h).join("Applications"));
            }
            for r in roots {
                out.extend(apps.iter().map(|a| r.join(a)));
            }
        }
        Os::Linux => {
            let names = [
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "microsoft-edge",
                "microsoft-edge-stable",
            ];
            if let Some(path) = var(env, "PATH") {
                for dir in std::env::split_paths(path) {
                    out.extend(names.iter().map(|n| dir.join(n)));
                }
            }
            out.extend(names.iter().map(|n| Path::new("/snap/bin").join(n)));
        }
        Os::Windows => {
            let rel = [
                r"Google\Chrome\Application\chrome.exe",
                r"Chromium\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
            ];
            for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(root) = var(env, key) {
                    out.extend(rel.iter().map(|r| Path::new(root).join(r)));
                }
            }
        }
    }
    out
}

/// `SYNT_BROWSER_PATH` wins and must exist; otherwise the first existing standard location.
pub fn discover(env: &Env, os: Os, exists: &dyn Fn(&Path) -> bool) -> Result<PathBuf> {
    if let Some(p) = env.get_nonempty("SYNT_BROWSER_PATH") {
        let p = PathBuf::from(p);
        return if exists(&p) {
            Ok(p)
        } else {
            Err(BrowserError::NotFound(format!(
                "SYNT_BROWSER_PATH={} does not exist",
                p.display()
            )))
        };
    }
    let list = candidates(os, env);
    list.iter().find(|p| exists(p)).cloned().ok_or_else(|| {
        let shown: Vec<_> = list.iter().take(8).map(|p| p.display().to_string()).collect();
        BrowserError::NotFound(format!(
            "no Chrome, Chromium or Edge found (looked at {}{}). Install one or set SYNT_BROWSER_PATH; Synthesis never downloads a browser",
            shown.join(", "),
            if list.len() > 8 { ", ..." } else { "" }
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_and_must_exist() {
        let env = Env::from_pairs([("SYNT_BROWSER_PATH", "/opt/b/chrome")]);
        assert_eq!(
            discover(&env, Os::Linux, &|_| true).unwrap(),
            PathBuf::from("/opt/b/chrome")
        );
        let err = discover(&env, Os::Linux, &|_| false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SYNT_BROWSER_PATH=/opt/b/chrome"), "{err}");
    }

    #[test]
    fn mac_standard_locations_in_order() {
        let env = Env::from_pairs([("HOME", "/Users/u")]);
        let want = PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium");
        let got = discover(&env, Os::Mac, &|p| p == want || p.starts_with("/Users/u")).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn linux_searches_path_then_snap() {
        let path = std::env::join_paths(["/a", "/b"]).unwrap();
        let env = Env::from_pairs([("PATH", path.to_str().unwrap())]);
        let want = Path::new("/b").join("chromium");
        assert_eq!(discover(&env, Os::Linux, &|p| p == want).unwrap(), want);
    }

    #[test]
    fn windows_uses_program_files_case_insensitively() {
        let env = Env::from_pairs([("PROGRAMFILES(X86)", r"C:\PF86")]);
        let c = candidates(Os::Windows, &env);
        assert!(
            c.iter()
                .any(|p| p.ends_with(r"Microsoft\Edge\Application\msedge.exe"))
        );
        assert!(c.iter().all(|p| p.starts_with(r"C:\PF86")));
    }

    #[test]
    fn none_found_is_a_clear_error() {
        let err = discover(&Env::default(), Os::Mac, &|_| false).unwrap_err();
        assert!(matches!(err, BrowserError::NotFound(_)));
        assert!(err.to_string().contains("never downloads"));
    }
}
