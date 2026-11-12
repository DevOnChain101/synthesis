//! `/reveal`: shows a folder or file in the system file manager. What is shown is checked here (pure,
//! tested on every OS): an absolute local path, a drive path, or a path of a WSL distro; never a URL, a
//! relative path or a network share (opening one would make Windows send credentials to it). The command
//! is built here too; only `open` spawns. Programs are absolute paths, never looked up on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::wsl;

/// Characters that never belong in a path handed to a program: control characters (CR and LF too), the
/// quote that would end Explorer's quoted argument, and the Windows wildcard and redirection characters.
/// `%` is expanded by Explorer (`%USERPROFILE%`).
const FORBIDDEN: [char; 7] = ['"', '|', '<', '>', '*', '?', '%'];

fn safe(p: &str) -> Result<(), String> {
    if p.is_empty()
        || p.starts_with('-')
        || p.chars().any(|c| c.is_control() || FORBIDDEN.contains(&c))
    {
        return Err("that path cannot be shown".into());
    }
    Ok(())
}

/// Is `p` a path this OS family opens without a network round trip? Windows: `X:\` or `X:/`, or a share of
/// a WSL distro (`\\wsl.localhost\<distro>`, `\\wsl$\<distro>`). Elsewhere: starts with `/`. A URL
/// (`http:`, `smb:`, `mailto:`, `shell:`) and any other UNC path are not.
fn absolute(p: &str, windows: bool) -> bool {
    if !windows {
        return p.starts_with('/');
    }
    // no component may be `.`, `..`, end in `.` or a space (Windows drops those, so the name shown is not
    // the one opened), or hold `:` (an alternate data stream); an empty one (a trailing `\`) is fine
    let clean = |rest: &str| {
        rest.split(['\\', '/'])
            .all(|c| !matches!(c, "." | "..") && !c.ends_with(['.', ' ']) && !c.contains(':'))
    };
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        return clean(&p[3..]);
    }
    let Some(after) = p.strip_prefix(r"\\").or_else(|| p.strip_prefix("//")) else {
        return false;
    };
    let mut it = after.split(['\\', '/']);
    let host = it.next().unwrap_or("");
    let distro = it.next().unwrap_or("");
    (host.eq_ignore_ascii_case("wsl.localhost") || host.eq_ignore_ascii_case("wsl$"))
        && !distro.is_empty()
        && clean(&after[host.len() + 1..])
}

/// What to show for `arg` (empty: the session folder `root`). Linux paths of a WSL engine become Windows
/// ones. A relative argument is refused (there is no cwd to resolve it against that the person can see).
pub fn target(
    arg: &str,
    root: &str,
    distro: Option<&str>,
    windows: bool,
) -> Result<String, String> {
    let arg = arg.trim();
    let (t, win) = match (arg, distro) {
        ("", _) => (root.to_string(), windows),
        (a, Some(d)) if a.starts_with('/') => (wsl::reveal_path(d, a)?, true),
        (a, _) => (a.to_string(), windows),
    };
    safe(&t)?;
    if !absolute(&t, win) {
        return Err("give a full path (a drive path, or one of the WSL distro)".into());
    }
    Ok(t)
}

/// Explorer's documented raw command line for a checked path. A trailing `\` would read as an escaped
/// quote (`"X:\dir\"`), so it is dropped and the folder is opened instead of selected; a drive root keeps
/// its `\` (a bare `X:` is "the current folder of X"), unquoted (it holds no space).
pub fn explorer_arg(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.len() == path.len() {
        format!("/select,\"{path}\"")
    } else if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        format!("\"{trimmed}\"")
    }
}

/// `(program, argument)` for this OS. On Windows the argument is the raw text of Explorer's documented
/// form `/select,"path"` (the path holds no quote, see [`safe`]).
pub fn command(path: &str, windows: bool, mac: bool) -> (PathBuf, String) {
    if windows {
        // explorer.exe sits in the Windows folder, not System32
        let root = synthesis_platform::system32("explorer.exe")
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        (root.join("explorer.exe"), explorer_arg(path))
    } else if mac {
        ("/usr/bin/open".into(), path.into())
    } else {
        // no portable "select": open the folder (the parent of a file); the argument starts with `/`
        let p = Path::new(path);
        let dir = if p.is_dir() {
            p
        } else {
            p.parent().unwrap_or(p)
        };
        (
            "/usr/bin/xdg-open".into(),
            dir.to_string_lossy().into_owned(),
        )
    }
}

/// Opens an already checked [`target`].
pub fn open(path: &str) -> Result<(), String> {
    safe(path)?;
    let (windows, mac) = (cfg!(windows), cfg!(target_os = "macos"));
    if !absolute(path, windows) {
        return Err("give a full path".into());
    }
    let (prog, arg) = command(path, windows, mac);
    if !prog.is_absolute() {
        return Err("cannot find the file manager".into());
    }
    let mut cmd = Command::new(&prog);
    if windows {
        #[cfg(windows)]
        std::os::windows::process::CommandExt::raw_arg(&mut cmd, &arg);
    } else if mac {
        cmd.arg("-R").arg(&arg);
    } else {
        cmd.arg(&arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("cannot open the file manager: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_the_folder_or_a_path_made_windows_for_a_wsl_engine() {
        let t = |a, r, d, w| target(a, r, d, w);
        assert_eq!(t("", r"C:\w\r", None, true).unwrap(), r"C:\w\r");
        assert_eq!(
            t("/home/me/a.rs", r"C:\w", Some("U"), true).unwrap(),
            r"\\wsl.localhost\U\home\me\a.rs"
        );
        assert_eq!(
            t("/mnt/c/w/a.rs", r"C:\w", Some("U"), true).unwrap(),
            r"C:\w\a.rs"
        );
        assert_eq!(t("/tmp/x", "/h", None, false).unwrap(), "/tmp/x");
        assert_eq!(t(r"d:/x", "", None, true).unwrap(), "d:/x");
        assert_eq!(
            t(r"\\wsl$\U\home", "", None, true).unwrap(),
            r"\\wsl$\U\home"
        );
        assert_eq!(
            t(r"//WSL.localhost/U/x", "", None, true).unwrap(),
            "//WSL.localhost/U/x"
        );
    }

    #[test]
    fn only_absolute_local_paths_are_shown_never_urls_shares_or_relative_ones() {
        let bad_win = [
            r"\\server\share\x",
            r"\\wsl.localhost",
            r"\\wsl.localhost\",
            r"\\wsl.localhostx\U",
            "//evil/share",
            "http://evil/x",
            "https://evil",
            "smb://evil/x",
            "mailto:a@b",
            "shell:startup",
            "file:///C:/x",
            r"C:",
            r"C:x",
            r"\x",
            r"\\?\C:\x",
            "rel\\x",
            "x",
        ];
        for p in bad_win {
            assert!(target(p, "", None, true).is_err(), "windows: {p}");
        }
        // on a POSIX host (xdg-open must never see a scheme) only `/...` goes on
        for p in [
            "http://evil/x",
            "smb:x",
            "mailto:a@b",
            "shell:x",
            "rel/x",
            "~/x",
            "C:\\x",
            "file:///etc",
        ] {
            assert!(target(p, "", None, false).is_err(), "posix: {p}");
        }
        assert!(target("", "", None, false).is_err(), "no folder");
        assert!(
            target("", "relative", None, false).is_err(),
            "even the folder is checked"
        );
        // a Linux path of a WSL engine goes through the mapper, which refuses `..`
        assert!(target("/mnt/c/../x", "", Some("U"), true).is_err());
    }

    #[test]
    fn metacharacters_never_reach_a_program_on_any_os() {
        for bad in [
            "C:\\x\" /e,\\\\evil\\a",
            "C:\\a|b",
            "C:\\a<b",
            "C:\\a>b",
            "C:\\a*b",
            "C:\\a?b",
            "C:\\a\nb",
            "C:\\a\rb",
            "C:\\a\u{1b}b",
            "-rf",
        ] {
            for w in [true, false] {
                assert!(target(bad, "", None, w).is_err(), "{bad:?}");
                assert!(open(bad).is_err(), "{bad:?}");
            }
        }
        for bad in ["/a\"b", "/a\nb", "/a?b"] {
            assert!(target(bad, "", None, false).is_err(), "{bad:?}");
            assert!(target(bad, "", Some("U"), true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn explorer_gets_the_documented_quoted_form_and_the_others_a_plain_path() {
        let (p, a) = command(r"\\wsl.localhost\U\x y", true, false);
        assert!(p.ends_with("explorer.exe"));
        assert_eq!(a, r#"/select,"\\wsl.localhost\U\x y""#);
        assert_eq!(
            command("/a", false, true),
            (PathBuf::from("/usr/bin/open"), "/a".into())
        );
        assert_eq!(
            command("/nonexistent/f", false, false).0,
            PathBuf::from("/usr/bin/xdg-open")
        );
        assert_eq!(command("/nonexistent/f", false, false).1, "/nonexistent");
    }

    #[test]
    fn windows_components_that_windows_would_rewrite_are_refused() {
        for bad in [
            r"C:\a\..\b",
            r"C:\a\.\b",
            r"C:\a.\b",
            r"C:\a \b",
            r"C:\a\b.",
            r"C:\a\b:stream",
            r"\\wsl.localhost\U.\x",
            r"\\wsl.localhost\U \x",
            r"\\wsl.localhost\..\x",
            r"\\wsl.localhost\U\a\..\b",
            r"\\wsl$\U\.\x",
            r"\\wsl$\U\x.",
            r"C:\%USERPROFILE%\x",
        ] {
            assert!(target(bad, "", None, true).is_err(), "{bad}");
        }
        for ok in [
            r"C:\",
            r"C:\a b\c.d",
            r"C:\a\",
            r"\\wsl.localhost\U",
            r"\\wsl$\U-1\a b\",
        ] {
            assert!(target(ok, "", None, true).is_ok(), "{ok}");
        }
    }

    #[test]
    fn a_trailing_backslash_never_ends_up_before_the_closing_quote() {
        assert_eq!(explorer_arg(r"C:\dir\f.txt"), r#"/select,"C:\dir\f.txt""#);
        assert_eq!(
            explorer_arg(r"C:\dir\"),
            r#""C:\dir""#,
            "the folder is opened"
        );
        assert_eq!(explorer_arg(r"C:\a b\\"), r#""C:\a b""#);
        assert_eq!(
            explorer_arg(r"C:\"),
            r"C:\",
            "a drive root keeps its backslash, unquoted"
        );
        assert_eq!(
            explorer_arg(r"\\wsl.localhost\U\x\"),
            r#""\\wsl.localhost\U\x""#
        );
        assert_eq!(
            explorer_arg(r"\\wsl.localhost\U"),
            r#"/select,"\\wsl.localhost\U""#
        );
        assert!(!explorer_arg(r"C:\x\").contains(r#"\""#));
    }

    /// Runs on the Windows CI only: the real Explorer path and the real command line for awkward paths.
    #[cfg(windows)]
    #[test]
    fn on_windows_explorer_is_found_and_awkward_paths_become_safe_command_lines() {
        let (prog, _) = command(r"C:\Windows", true, false);
        assert!(prog.is_absolute() && prog.exists(), "{}", prog.display());
        for (p, want) in [
            (r"C:\Program Files\x y\", r#""C:\Program Files\x y""#),
            (
                r"C:\Windows\System32\drivers\etc\hosts",
                r#"/select,"C:\Windows\System32\drivers\etc\hosts""#,
            ),
            (
                r"\\wsl.localhost\Ubuntu\home\me\a b.txt",
                r#"/select,"\\wsl.localhost\Ubuntu\home\me\a b.txt""#,
            ),
            (r"D:\", r"D:\"),
        ] {
            let t = target(p, "", None, true).unwrap();
            assert_eq!(command(&t, true, false).1, want, "{p}");
        }
        for bad in [
            r"C:\50%\x",
            r"C:\%TEMP%",
            "C:\\a b\\x\"",
            r"http://x/y",
            r"\\evil\share\x",
        ] {
            assert!(target(bad, "", None, true).is_err(), "{bad}");
        }
    }
}
