//! What a personal shell is started as (spec 1.2, 1.8). Pure: the environment and the platform are
//! parameters, so every OS flavour is tested on any machine.

use std::path::PathBuf;

use synthesis_config::Env;

/// Program, arguments, directory and the WHOLE environment of the shell (nothing is inherited).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Shown in the tab and in the error: `zsh`, `bash`, `cmd`, `WSL Ubuntu`.
    pub label: String,
}

/// Where the shell runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Where {
    /// This machine, in this folder.
    Here(PathBuf),
    /// WSL mode: the distro and the folder inside it (the engine's own, never a Windows path).
    Wsl { distro: String, cwd: String },
}

/// Names `is_secret_shaped` drops (an AUTH or SESSION segment) that hold no secret, only a socket path or a
/// session id; without them ssh-agent, git over ssh and the desktop bus break in the person's own shell.
/// Applied only to these exact names; `SYNT_*` is never kept.
const KEEP: [&str; 7] = [
    "SSH_AUTH_SOCK",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_RUNTIME_DIR",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
];

/// The person's environment minus every `SYNT_*` name and every secret-shaped name (N3), plus the emulator's
/// `TERM` and `COLORTERM`. A login shell re-reads the person's profile, which may export their own keys: that
/// is their shell. `windows` folds the case of names (the environment is case-insensitive there).
pub fn scrub(env: &Env, windows: bool) -> Vec<(String, String)> {
    let ours = |k: &str| {
        ["TERM", "COLORTERM"].iter().any(|n| {
            if windows {
                k.eq_ignore_ascii_case(n)
            } else {
                k == *n
            }
        })
    };
    let mut out: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            !(up.starts_with("SYNT_")
                || (synthesis_config::is_secret_shaped(&up) && !KEEP.contains(&up.as_str()))
                || ours(k))
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    out.push(("TERM".into(), "xterm-256color".into()));
    out.push(("COLORTERM".into(), "truecolor".into()));
    out
}

fn base(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .trim_end_matches(".exe")
        .to_string()
}

/// The launch for `at`. `shell` is `$SHELL` (unix), `windows_shell` the program `resolve_shell` found
/// (Git Bash or `cmd.exe`) and whether it is a POSIX one, `wsl_exe` the absolute `System32\wsl.exe`.
pub fn plan(
    at: &Where,
    env: &Env,
    windows: bool,
    wsl_exe: Option<PathBuf>,
    windows_shell: Option<(PathBuf, bool)>,
) -> Result<Launch, String> {
    match at {
        Where::Wsl { distro, cwd } => {
            let exe = wsl_exe.ok_or("wsl.exe was not found")?;
            // values that wsl.exe would read as options, or that hold control characters, are refused
            if [distro, cwd]
                .iter()
                .any(|v| v.is_empty() || v.starts_with('-') || v.chars().any(char::is_control))
                || !cwd.starts_with('/')
            {
                return Err("the WSL distro or folder is not a usable value".into());
            }
            // the engine's own allow-list (paths, locale, proxy, TLS roots, temp), no SYNT_*, no WSLENV
            let mut vars: Vec<(String, String)> = crate::remote::allowed_env(env)
                .into_iter()
                .filter(|(k, _)| !k.starts_with("SYNT_") && !k.eq_ignore_ascii_case("WSLENV"))
                .collect();
            vars.push(("TERM".into(), "xterm-256color".into()));
            Ok(Launch {
                program: exe,
                args: ["-d", distro, "--cd", cwd].map(String::from).to_vec(),
                cwd: None,
                env: vars,
                label: format!("WSL {distro}"),
            })
        }
        Where::Here(dir) => {
            let (program, args) = if windows {
                let (p, posix) = windows_shell.ok_or("no shell was found")?;
                let args = if posix {
                    vec!["-l".into(), "-i".into()]
                } else {
                    vec![]
                };
                (p, args)
            } else {
                // an absolute `$SHELL`; else `/bin/sh`
                let sh = env
                    .get_nonempty("SHELL")
                    .filter(|s| s.starts_with('/') && !s.chars().any(char::is_control))
                    .unwrap_or("/bin/sh");
                (PathBuf::from(sh), vec!["-l".to_string()])
            };
            let label = base(&program.to_string_lossy());
            Ok(Launch {
                program,
                args,
                cwd: Some(dir.clone()),
                env: scrub(env, windows),
                label,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env::from_pairs([
            ("PATH", "/usr/bin"),
            ("HOME", "/h"),
            ("SHELL", "/bin/zsh"),
            ("TERM", "dumb"),
            ("SYNT_HOME", "/h/.synt"),
            ("SYNT_API_KEY", "k"),
            ("SYNT_AGENT_SESSION", "1"),
            ("ANTHROPIC_API_KEY", "k"),
            ("GITHUB_TOKEN", "t"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
            ("synt_lower", "x"),
            ("SSH_AUTH_SOCK", "/run/agent.sock"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/bus"),
            ("XDG_SESSION_ID", "2"),
            ("SYNT_SESSION_ID", "no"),
            ("SESSION_TOKEN", "no"),
            ("LANG", "C.UTF-8"),
        ])
    }

    /// Red team: the personal shell's environment has no `SYNT_*` and no secret-shaped name.
    #[test]
    fn a_personal_shell_env_has_no_synt_and_no_secret_names() {
        for windows in [false, true] {
            let l = plan(
                &Where::Here("/w".into()),
                &env(),
                windows,
                None,
                Some(("C:/Git/bin/bash.exe".into(), true)),
            )
            .unwrap();
            let mut names: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                [
                    "COLORTERM",
                    "DBUS_SESSION_BUS_ADDRESS",
                    "HOME",
                    "LANG",
                    "PATH",
                    "SHELL",
                    "SSH_AUTH_SOCK",
                    "TERM",
                    "XDG_SESSION_ID"
                ],
                "windows={windows}"
            );
            assert_eq!(
                l.env.iter().find(|(k, _)| k == "TERM").unwrap().1,
                "xterm-256color"
            );
        }
    }

    #[test]
    fn unix_runs_the_login_shell_by_absolute_path_else_bin_sh() {
        let at = Where::Here("/w".into());
        let l = plan(&at, &env(), false, None, None).unwrap();
        assert_eq!(
            (l.program.to_str(), l.args.as_slice(), l.label.as_str()),
            (Some("/bin/zsh"), &["-l".to_string()][..], "zsh")
        );
        assert_eq!(l.cwd, Some("/w".into()));
        for bad in ["zsh", "", "/bin/zsh\n-c"] {
            let e = Env::from_pairs([("SHELL", bad)]);
            assert_eq!(
                plan(&at, &e, false, None, None).unwrap().program,
                PathBuf::from("/bin/sh"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn windows_shells() {
        let at = Where::Here("C:/w".into());
        let cmd = plan(
            &at,
            &env(),
            true,
            None,
            Some(("C:/Windows/cmd.exe".into(), false)),
        )
        .unwrap();
        assert!(cmd.args.is_empty());
        assert_eq!(cmd.label, "cmd");
        assert!(plan(&at, &env(), true, None, None).is_err());
    }

    #[test]
    fn wsl_goes_through_wsl_exe_with_the_engine_allow_list() {
        let at = Where::Wsl {
            distro: "Ubuntu".into(),
            cwd: "/home/p".into(),
        };
        let wsl = Some(PathBuf::from(r"C:\Windows\System32\wsl.exe"));
        let l = plan(&at, &env(), true, wsl, None).unwrap();
        assert_eq!(l.args, ["-d", "Ubuntu", "--cd", "/home/p"]);
        assert_eq!(l.label, "WSL Ubuntu");
        let names: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            names.contains(&"PATH") && names.contains(&"LANG"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|k| k.starts_with("SYNT_")
                || k.contains("KEY")
                || k.contains("TOKEN")
                || k.eq(&"WSLENV")),
            "{names:?}"
        );
        assert!(plan(&at, &env(), true, None, None).is_err());
        let wsl = || Some(PathBuf::from(r"C:\Windows\System32\wsl.exe"));
        for (d, c) in [
            ("-e", "/h"),
            ("Ubuntu", "-x"),
            ("Ubuntu", "rel"),
            ("Ub\nuntu", "/h"),
            ("Ubuntu", "/h\u{7}"),
            ("", "/h"),
        ] {
            let at = Where::Wsl {
                distro: d.into(),
                cwd: c.into(),
            };
            assert!(plan(&at, &env(), true, wsl(), None).is_err(), "{d:?} {c:?}");
        }
    }
}
