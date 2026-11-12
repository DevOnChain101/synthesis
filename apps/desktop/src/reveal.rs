//! `/reveal`: shows a folder or file in the system file manager. The command is built here (pure, tested
//! on every OS); only `open` spawns. Programs are absolute paths, never looked up on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::wsl;

/// A path text that may reach a program argument: one line, not an option.
fn safe(p: &str) -> Result<(), String> {
    if p.is_empty() || p.chars().any(char::is_control) || p.starts_with('-') {
        return Err("that path cannot be shown".into());
    }
    Ok(())
}

/// What to show for `arg` (empty: the session folder `root`). Linux paths of a WSL engine become Windows ones.
pub fn target(arg: &str, root: &str, distro: Option<&str>) -> Result<String, String> {
    let arg = arg.trim();
    let t = match (arg, distro) {
        ("", _) => root.to_string(),
        (a, Some(d)) if a.starts_with('/') => wsl::reveal_path(d, a)?,
        (a, _) => a.to_string(),
    };
    safe(&t)?;
    Ok(t)
}

/// `(program, args)` for this OS.
pub fn command(path: &str, windows: bool, mac: bool) -> (PathBuf, Vec<String>) {
    if windows {
        // explorer.exe sits in the Windows folder, not System32
        let root = synthesis_platform::system32("explorer.exe")
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        (root.join("explorer.exe"), vec![format!("/select,{path}")])
    } else if mac {
        ("/usr/bin/open".into(), vec!["-R".into(), path.into()])
    } else {
        // no portable "select": open the folder (the parent of a file)
        let p = Path::new(path);
        let dir = if p.is_dir() {
            p
        } else {
            p.parent().unwrap_or(p)
        };
        (
            "/usr/bin/xdg-open".into(),
            vec![dir.to_string_lossy().into_owned()],
        )
    }
}

pub fn open(path: &str) -> Result<(), String> {
    safe(path)?;
    let (prog, args) = command(path, cfg!(windows), cfg!(target_os = "macos"));
    if !prog.is_absolute() {
        return Err("cannot find the file manager".into());
    }
    Command::new(&prog)
        .args(&args)
        .stdin(Stdio::null())
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
        assert_eq!(target("", r"C:\w\r", None).unwrap(), r"C:\w\r");
        assert_eq!(
            target("/home/me/a.rs", r"C:\w", Some("U")).unwrap(),
            r"\\wsl.localhost\U\home\me\a.rs"
        );
        assert_eq!(
            target("/mnt/c/w/a.rs", r"C:\w", Some("U")).unwrap(),
            r"C:\w\a.rs"
        );
        assert_eq!(
            target("/tmp/x", "/h", None).unwrap(),
            "/tmp/x",
            "a local path is as given"
        );
        for bad in ["-rf", "a\nb", "/mnt/c/../x"] {
            assert!(target(bad, "/h", Some("U")).is_err(), "{bad}");
        }
        assert!(target("", "", None).is_err());
    }

    #[test]
    fn the_command_is_an_absolute_program_with_the_path_as_one_argument() {
        let (p, a) = command(r"\\wsl.localhost\U\x y", true, false);
        assert!(p.ends_with("explorer.exe"));
        assert_eq!(a, vec![r"/select,\\wsl.localhost\U\x y".to_string()]);
        assert_eq!(
            command("/a", false, true),
            ("/usr/bin/open".into(), vec!["-R".into(), "/a".into()])
        );
        assert_eq!(
            command("/nonexistent/f", false, false).0,
            PathBuf::from("/usr/bin/xdg-open")
        );
    }
}
