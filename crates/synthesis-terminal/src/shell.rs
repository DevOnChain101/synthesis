//! Shell resolver, ported from nemo-cli `src/native/shell.js` (`findGitBash` / `resolveShell`).
//! Env and file-exists are injected so the Windows logic is testable on any OS.
//!
//! Deviation from Node: `$SHELL` is never consulted. The permission classifier parses POSIX sh,
//! so commands must run under exactly that: `/bin/sh -c` on POSIX (no user rc files, aliases or
//! zsh/fish-only syntax such as `=(...)` and glob qualifiers), Git Bash on Windows.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// POSIX shell or Git Bash: `<program> -c <command>`.
    Posix,
    /// cmd.exe fallback: `/d /s /c` with a verbatim command line.
    Cmd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: PathBuf,
    /// Arguments placed before the command string.
    pub args: Vec<String>,
    pub kind: ShellKind,
    /// Warning to surface to the user (cmd.exe fallback).
    pub note: Option<String>,
}

impl ShellSpec {
    /// False for cmd.exe: the harness/permissions must not auto-allow commands classified
    /// with POSIX rules when this is false.
    pub fn is_posix(&self) -> bool {
        self.kind == ShellKind::Posix
    }
}

type GetEnv<'a> = &'a dyn Fn(&str) -> Option<OsString>;
type Exists<'a> = &'a dyn Fn(&Path) -> bool;

/// Windows: first existing of the Git for Windows install dirs and the
/// `bin\bash.exe` next to a `git.exe` on PATH. Returns the hit and the number of places tried.
/// Never `System32\bash.exe` (the WSL launcher).
pub fn find_git_bash(env: GetEnv, exists: Exists) -> (Option<PathBuf>, usize) {
    let mut tried = 0;
    let mut check = |p: PathBuf| {
        tried += 1;
        exists(&p).then_some(p)
    };
    let git = |root: OsString| PathBuf::from(root).join("Git");
    let bases = [
        env("ProgramFiles").map(git),
        env("ProgramFiles(x86)").map(git),
        env("LOCALAPPDATA").map(|r| PathBuf::from(r).join("Programs").join("Git")),
        env("USERPROFILE").map(|r| PathBuf::from(r).join("scoop/apps/git/current")),
    ];
    for base in bases.into_iter().flatten() {
        if let Some(hit) = check(base.join("bin").join("bash.exe")) {
            return (Some(hit), tried);
        }
    }
    let path = env("PATH").or_else(|| env("Path")).unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        // a relative PATH entry would resolve against the current directory
        if !dir.is_absolute() || check(dir.join("git.exe")).is_none() {
            continue;
        }
        for up in ["..", "../.."] {
            if let Some(hit) = check(dir.join(up).join("bin").join("bash.exe")) {
                return (Some(hit), tried);
            }
        }
    }
    (None, tried)
}

/// Picks the shell for the given platform flavour (`windows` is a parameter for testability).
pub fn resolve_shell(windows: bool, env: GetEnv, exists: Exists) -> ShellSpec {
    if windows {
        let (found, tried) = find_git_bash(env, exists);
        if let Some(program) = found {
            return ShellSpec {
                program,
                args: vec!["-c".into()],
                kind: ShellKind::Posix,
                note: None,
            };
        }
        return ShellSpec {
            program: env("ComSpec")
                .map(PathBuf::from)
                .unwrap_or_else(|| synthesis_platform::system32("cmd.exe")),
            args: vec!["/d".into(), "/s".into(), "/c".into()],
            kind: ShellKind::Cmd,
            note: Some(format!(
                "Git Bash was not found (looked in {tried} places); using cmd.exe, which does not \
                 understand POSIX-style commands. Install Git for Windows."
            )),
        };
    }
    // Fixed on purpose, see the module docs. `exists` is only needed on Windows.
    let program = PathBuf::from("/bin/sh");
    ShellSpec {
        program,
        args: vec!["-c".into()],
        kind: ShellKind::Posix,
        note: None,
    }
}

/// Resolves against the real environment and file system.
pub fn native_shell() -> ShellSpec {
    resolve_shell(cfg!(windows), &|k| std::env::var_os(k), &|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).map(OsString::from)
    }

    #[test]
    fn posix_is_always_bin_sh_never_shell_var() {
        for e in [&[("SHELL", "/bin/zsh")][..], &[("SHELL", "/nope")], &[]] {
            let s = resolve_shell(false, &env(e), &|_| true);
            assert_eq!(
                (s.program.clone(), s.args.clone()),
                (PathBuf::from("/bin/sh"), vec!["-c".to_string()])
            );
            assert!(s.is_posix() && s.kind == ShellKind::Posix);
        }
    }

    #[test]
    fn windows_ignores_shell_var() {
        let bash = r"C:\evilash.exe";
        let s = resolve_shell(true, &env(&[("SHELL", bash)]), &|p| p == Path::new(bash));
        assert_eq!(s.kind, ShellKind::Cmd);
        assert!(!s.is_posix());
    }

    #[test]
    fn windows_finds_program_files_git() {
        let want = PathBuf::from("PF").join("Git").join("bin").join("bash.exe");
        let s = resolve_shell(true, &env(&[("ProgramFiles", "PF")]), &|p| p == want);
        assert_eq!((s.program, s.kind, s.note), (want, ShellKind::Posix, None));
    }

    #[test]
    fn windows_finds_bash_next_to_git_on_path() {
        let dir = std::env::temp_dir().join("G").join("cmd");
        let git = dir.join("git.exe");
        let want = dir.join("..").join("bin").join("bash.exe");
        let path = std::env::join_paths([&dir]).unwrap();
        let s = resolve_shell(true, &|k| (k == "PATH").then(|| path.clone()), &|p| {
            p == git || p == want
        });
        assert_eq!(s.program, want);
    }

    #[test]
    fn a_relative_path_entry_is_never_searched() {
        let dir = PathBuf::from("rel").join("cmd");
        let path = std::env::join_paths([&dir]).unwrap();
        let s = resolve_shell(true, &|k| (k == "PATH").then(|| path.clone()), &|_| true);
        assert_eq!(s.kind, ShellKind::Cmd);
    }

    #[test]
    fn windows_falls_back_to_cmd_with_note() {
        let s = resolve_shell(true, &env(&[("ComSpec", r"C:\X\cmd.exe")]), &|_| false);
        assert_eq!(s.kind, ShellKind::Cmd);
        assert!(!s.is_posix());
        assert_eq!(s.program, PathBuf::from(r"C:\X\cmd.exe"));
        assert_eq!(s.args, ["/d", "/s", "/c"]);
        assert!(s.note.unwrap().contains("Git Bash was not found"));
    }
}
