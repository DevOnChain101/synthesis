//! Running the Linux engine inside WSL2 (docs/specs/desktop.md section 9). Everything here is plain
//! logic over an injected `wsl.exe` runner, so it is tested on every OS; only the real spawn (`run_wsl`,
//! and `Remote::connect` of the engine itself) touches a process, and `start` is only called on Windows.
//! Nothing is installed or downloaded here; keys are never copied (no `WSLENV`, no `SYNT_*` to wsl.exe).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;

use crate::engine::{Engine, UiMsg};
use crate::remote::Launcher;

/// A cold distro (the VM boots) may need this long to say hello.
pub const COLD_START: Duration = Duration::from_secs(60);
const QUICK: Duration = Duration::from_secs(15);
const MAX_OUT: u64 = 1 << 20;
/// Shown while the distro boots.
pub const STARTING: &str = "starting distro";
/// Said in Settings and the docs: the engine reads its own settings and keys inside WSL.
pub const KEYS_NOTICE: &str = "Engine settings and API keys live inside WSL (~/.synt there), never on Windows. Use {file:} provider keys: wsl.exe --exec does not load the shell profile.";
/// Refusal of what needs the session store, until the protocol carries it.
pub const LATER: &str = "in WSL mode, available later";

/// What a `wsl.exe` call gave back.
pub struct Out {
    pub ok: bool,
    pub stdout: Vec<u8>,
}

/// Runs `wsl.exe` with these arguments and waits (injected: tests answer from a table).
pub type Run<'a> = &'a dyn Fn(&[&str], Duration) -> Result<Out, String>;

/// The real runner: no shell, closed environment, no console window, output capped, killed on timeout.
pub fn run_wsl(exe: &Path, args: &[&str], wait: Duration) -> Result<Out, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    // the exe is <root>\System32\wsl.exe and wsl.exe needs SystemRoot even in a closed environment
    if let Some(root) = exe.parent().and_then(Path::parent) {
        cmd.env("SystemRoot", root);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run wsl.exe ({}): {e}", exe.display()))?;
    let mut stdout = child.stdout.take().ok_or("wsl.exe has no pipe")?;
    let (done, got) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = vec![];
        let _ = stdout.by_ref().take(MAX_OUT).read_to_end(&mut b);
        let _ = done.send(b);
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if t0.elapsed() > wait => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "wsl.exe did not answer within {} s",
                    wait.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    Ok(Out {
        ok: status.success(),
        // bounded: a descendant that kept the pipe open must not hang this call
        stdout: got.recv_timeout(Duration::from_secs(2)).unwrap_or_default(),
    })
}

/// wsl.exe prints UTF-16LE (with or without a BOM) for its own messages and the distro list; a Linux
/// program run through it prints UTF-8 (which never holds a NUL). Both become text with `\n` line ends.
pub fn decode(b: &[u8]) -> String {
    let s = if b.contains(&0) {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(b).into_owned()
    };
    s.trim_start_matches('\u{feff}').replace("\r\n", "\n")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub version: u8,
}

/// Parses `wsl.exe -l -v`: `[*] NAME STATE VERSION` per line. Read from the right (VERSION, STATE) so a
/// translated header or state word cannot break it; a line whose last field is not a number (the header) is skipped.
pub fn parse_distros(text: &str) -> Vec<Distro> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (default, l) = match l.strip_prefix('*') {
                Some(r) => (true, r),
                None => (false, l),
            };
            let f: Vec<&str> = l.split_whitespace().collect();
            let [name, state, version] = f[..] else {
                return None;
            };
            Some(Distro {
                name: name.into(),
                default,
                running: state.eq_ignore_ascii_case("running"),
                version: version.parse().ok()?,
            })
        })
        .collect()
}

/// The distro to use: `wanted` by name (case-insensitive) or WSL's default. WSL1 is refused.
pub fn pick(all: &[Distro], wanted: &str) -> Result<Distro, String> {
    let wanted = wanted.trim();
    let d = if wanted.is_empty() {
        all.iter().find(|d| d.default)
    } else {
        all.iter().find(|d| d.name.eq_ignore_ascii_case(wanted))
    };
    let d = d.ok_or_else(|| {
        if wanted.is_empty() {
            "WSL has no default distro; install one (wsl.exe --install) or choose one in Settings"
                .to_string()
        } else {
            format!("no WSL distro named {wanted:?}; see wsl.exe -l -v")
        }
    })?;
    if d.version != 2 {
        return Err(format!(
            "{} runs WSL {}; only WSL 2 is supported (wsl.exe --set-version {} 2)",
            d.name, d.version, d.name
        ));
    }
    // the name goes to wsl.exe as an argument: it must not read as an option
    if d.name.starts_with('-') || d.name.chars().any(char::is_control) {
        return Err(format!("{:?} is not a usable distro name", d.name));
    }
    Ok(d.clone())
}

/// Text to run inside the distro for the matching engine; shown, never run by the desktop (the Install
/// button is a later increment).
pub fn install_command(version: &str) -> String {
    format!(
        "curl --proto '=https' --tlsv1.2 -fsSL https://github.com/DevOnChain101/synthesis/releases/download/v{version}/install.sh | SYNT_VERSION={version} SYNT_INSTALL_DIR=\"$HOME/.synt/bin\" sh"
    )
}

#[derive(Debug)]
pub struct Ready {
    pub distro: Distro,
    /// Absolute path of `synt` in the distro.
    pub synt: String,
}

fn text_of(o: Out) -> Result<String, String> {
    if o.ok {
        Ok(decode(&o.stdout))
    } else {
        Err("the command failed".into())
    }
}

/// `wsl.exe --status`: WSL exists and works.
pub fn status(run: Run) -> Result<(), String> {
    match run(&["--status"], QUICK) {
        Ok(o) if o.ok => Ok(()),
        Ok(_) => Err("WSL is not available: wsl.exe --status failed (turn on the Windows feature or run wsl.exe --install)".into()),
        Err(e) => Err(format!("WSL is not available: {e}")),
    }
}

/// Checks WSL, picks the distro and finds an engine of exactly `version` in it. The error is the text
/// to show.
pub fn probe(run: Run, wanted: &str, version: &str) -> Result<Ready, String> {
    probe_raw(run, wanted, version).map_err(|e| sanitize_for_display(&e))
}

fn probe_raw(run: Run, wanted: &str, version: &str) -> Result<Ready, String> {
    status(run)?;
    let list = text_of(run(&["-l", "-v"], QUICK)?).map_err(|e| format!("wsl.exe -l -v: {e}"))?;
    let distro = pick(&parse_distros(&list), wanted)?;
    // `--exec` expands no `~`, so a fixed shell resolves it. No login shell and a fixed PATH: nothing a
    // profile or the Windows PATH (which WSL appends, /mnt/c/...) adds can be picked.
    let script = r#"PATH=/usr/local/bin:/usr/bin:/bin; p="$HOME/.synt/bin/synt"; if [ -x "$p" ]; then echo "$p"; elif [ -x /usr/local/bin/synt ]; then echo /usr/local/bin/synt; fi"#;
    let found = run(
        &["-d", &distro.name, "--exec", "/bin/sh", "-c", script],
        COLD_START,
    )
    .map_err(|e| format!("the distro {} did not start: {e}", distro.name))?;
    let synt = decode(&found.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .filter(|l| found.ok && native(l))
        .map(String::from)
        .ok_or_else(|| {
            format!(
                "synt is not installed in {}. Run this inside it: {}",
                distro.name,
                install_command(version)
            )
        })?;
    let out = run(&["-d", &distro.name, "--exec", &synt, "--version"], QUICK)?;
    let said = decode(&out.stdout);
    if !out.ok || !said.split_whitespace().any(|w| w == version) {
        return Err(format!(
            "synt in {} is {:?}, this app needs exactly {version}. Run this inside it: {}",
            distro.name,
            said.trim().chars().take(60).collect::<String>(),
            install_command(version)
        ));
    }
    Ok(Ready { distro, synt })
}

/// An absolute Linux path on one line.
fn plain(s: &str) -> bool {
    s.starts_with('/') && !s.chars().any(char::is_control)
}

/// `plain`, and not on a Windows-mounted drive (`/mnt/c/...` is writable by any Windows process).
fn native(s: &str) -> bool {
    plain(s) && s != "/mnt" && !s.starts_with("/mnt/")
}

/// `\\wsl.localhost\D\rest` and `\\wsl$\D\rest` (also `//` and `\\?\UNC\`): `D\rest`.
fn unc_rest(p: &str) -> Option<&str> {
    let after = p
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| p.strip_prefix(r"\\"))
        .or_else(|| p.strip_prefix("//"))?;
    let (host, rest) = after.split_once(['\\', '/']).unwrap_or((after, ""));
    (host.eq_ignore_ascii_case("wsl.localhost") || host.eq_ignore_ascii_case("wsl$"))
        .then_some(rest)
}

/// A Windows folder as the path inside `distro`. A WSL UNC path of the selected distro is just
/// re-rooted; another distro's is refused; a drive path asks `wslpath -u` in the distro (`wslpath`), never a guess.
pub fn to_linux(
    path: &str,
    distro: &str,
    wslpath: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    if let Some(rest) = unc_rest(path) {
        let (d, rest) = rest.split_once(['\\', '/']).unwrap_or((rest, ""));
        if !d.eq_ignore_ascii_case(distro) {
            return Err(format!(
                "that folder is in the WSL distro {d}, but the selected distro is {distro}"
            ));
        }
        let parts: Vec<&str> = rest.split(['\\', '/']).filter(|c| !c.is_empty()).collect();
        if parts.iter().any(|c| {
            matches!(*c, "." | "..") || c.ends_with(['.', ' ']) || c.chars().any(char::is_control)
        }) {
            return Err("that folder path is not valid".into());
        }
        return Ok(format!("/{}", parts.join("/")));
    }
    let drive = path.strip_prefix(r"\\?\").unwrap_or(path);
    let b = drive.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let l = wslpath(drive)?;
        let l = l.trim();
        return if plain(l) {
            Ok(l.into())
        } else {
            Err(format!("wslpath gave an unusable path for {drive}"))
        };
    }
    Err(format!(
        "{path} is not a folder WSL can open (use a drive folder or one inside {distro})"
    ))
}

/// A path inside the distro as Explorer shows it (for "Reveal").
pub fn to_display(distro: &str, linux: &str) -> String {
    format!(r"\\wsl.localhost\{distro}{}", linux.replace('/', r"\"))
}

/// A Linux path of the engine for the system file manager: `/mnt/<drive>/x` is the Windows drive path
/// `D:\x`, anything else the distro's UNC path. The path must be absolute, one line, without `.`/`..`.
pub fn reveal_path(distro: &str, linux: &str) -> Result<String, String> {
    let parts: Vec<&str> = linux.split('/').skip(1).filter(|c| !c.is_empty()).collect();
    if !plain(linux) || parts.iter().any(|c| matches!(*c, "." | "..")) {
        return Err(format!("{linux:?} is not a path that can be shown"));
    }
    if let [m, d, rest @ ..] = &parts[..]
        && *m == "mnt"
        && d.len() == 1
        && d.as_bytes()[0].is_ascii_alphabetic()
    {
        let drive = d.to_ascii_uppercase();
        return Ok(format!(r"{drive}:\{}", rest.join(r"\")));
    }
    Ok(to_display(distro, &format!("/{}", parts.join("/"))))
}

/// `wsl.exe -d <distro> --cd <cwd> --exec <synt> engine --stdio`. The program is injected. wsl.exe gets
/// the allowlisted environment minus every `SYNT_*` (a key is never handed over, and nothing is
/// exported into the distro: no `WSLENV`).
pub fn launcher(wsl_exe: &Path, distro: &str, cwd: &str, synt: &str, env: &Env) -> Launcher {
    let args = [
        "-d", distro, "--cd", cwd, "--exec", synt, "engine", "--stdio",
    ];
    let mut l = Launcher::new(wsl_exe, args.map(Into::into).to_vec(), env);
    l.env.retain(|(k, _)| !k.starts_with("SYNT_"));
    l.hello_timeout = COLD_START;
    l.start_cwd = Some(cwd.into());
    l
}

/// `(enabled, distro)` from the USER settings file, then the MANAGED one, which wins per key (the
/// Settings view locks a key the managed file sets). A project or local file is never read. A file that
/// is absent is "off"; one that exists but cannot be read, is over 1 MiB, is not JSON (BOM tolerated,
/// read by the core's own `read_json`) or holds a wrong type is an error: the caller refuses to start and
/// never falls back to the local engine.
pub fn read_setting(home: &Path, managed: &Path) -> Result<(bool, String), String> {
    let mut out = (false, String::new());
    for (path, what) in [
        (home.join("settings.json"), "user settings"),
        (managed.to_path_buf(), "managed settings"),
    ] {
        let Some(doc) = synthesis_settings::read_json(&path, what)? else {
            continue;
        };
        let mut w = &doc;
        for seg in ["synt", "desktop", "wsl"] {
            match w.get(seg) {
                None => break,
                Some(v) if v.is_object() => w = v,
                Some(_) => return Err(format!("{what}: synt.desktop.wsl is not an object")),
            }
        }
        if let Some(v) = w.get("enabled") {
            out.0 = v
                .as_bool()
                .ok_or_else(|| format!("{what}: synt.desktop.wsl.enabled must be true or false"))?;
        }
        if let Some(v) = w.get("distro") {
            synthesis_settings_ui::validate_kind(synthesis_settings_ui::Kind::Text, v)
                .map_err(|e| format!("{what}: synt.desktop.wsl.distro {e}"))?;
            out.1 = v.as_str().unwrap_or_default().into();
        }
    }
    Ok(out)
}

fn managed_path() -> PathBuf {
    synthesis_settings::managed_settings_path()
}

/// Is the WSL mode on? Windows only: elsewhere the toggle is disabled and nothing is read. An unusable
/// settings file is an `Err` to show, not "off".
pub fn enabled(home: &Path) -> Result<bool, String> {
    enabled_on(cfg!(windows), home, &managed_path())
}

/// [`enabled`] with the platform and the managed file given (tests).
pub fn enabled_on(windows: bool, home: &Path, managed: &Path) -> Result<bool, String> {
    if !windows {
        return Ok(false);
    }
    read_setting(home, managed).map(|s| s.0)
}

/// The engine's own report of its folder against the Linux path that was asked for: the session folder
/// must be exactly it, and the root (the repository top) it or an ancestor. Anything else is refused:
/// the agent would work somewhere other than where the person chose (a symlink, a failed `--cd`).
pub fn check_reported(asked: &str, cwd: Option<&str>, root: Option<&str>) -> Result<(), String> {
    let inside = |r: &str| {
        asked == r || r == "/" || asked.starts_with(&format!("{}/", r.trim_end_matches('/')))
    };
    match (cwd, root) {
        (Some(c), Some(r)) if c == asked && inside(r) => Ok(()),
        (c, r) => Err(format!(
            "the engine reports its folder as {} (root {}) but {asked} was asked; open the real path of the folder (no links)",
            c.unwrap_or("nothing"),
            r.unwrap_or("nothing")
        )),
    }
}

/// Probes, converts the folder and starts the engine in the distro. Blocking. `project` is the folder the
/// person chose on Windows and stays the session's host-side folder; the engine gets the Linux path.
pub fn start_via(
    run: Run,
    wsl_exe: &Path,
    wanted: &str,
    version: &str,
    env: &Env,
    project: &Path,
) -> Result<(Engine, UnboundedReceiver<UiMsg>), String> {
    let go = || {
        let ready = probe(run, wanted, version)?;
        let d = ready.distro.name.as_str();
        let host = project
            .to_str()
            .ok_or("the folder path is not valid text")?;
        let cwd = to_linux(host, d, |p| {
            text_of(run(
                &["-d", d, "--exec", "/usr/bin/wslpath", "-u", p],
                QUICK,
            )?)
        })?;
        let (mut e, rx) =
            Engine::start_remote(&launcher(wsl_exe, d, &cwd, &ready.synt, env), project)?;
        // refused (the engine is dropped, which closes it) when it is not where it was asked to be
        check_reported(&cwd, e.remote_cwd.as_deref(), e.remote_root.as_deref())?;
        e.remote_distro = Some(d.to_string());
        Ok((e, rx))
    };
    go().map_err(|e: String| sanitize_for_display(&e))
}

/// `System32\wsl.exe` by absolute path; refused if `SystemRoot` is not an absolute path.
fn system_wsl() -> Result<PathBuf, String> {
    let exe = synthesis_platform::system32("wsl.exe");
    if exe.is_absolute() {
        Ok(exe)
    } else {
        Err("SystemRoot is not an absolute path; refusing to run wsl.exe".into())
    }
}

/// The real thing, on Windows: `System32\wsl.exe` and this app's exact version.
pub fn start(
    env: &Env,
    home: &Path,
    project: &Path,
) -> Result<(Engine, UnboundedReceiver<UiMsg>), String> {
    let exe = system_wsl()?;
    let (_, distro) = read_setting(home, &managed_path())?;
    let run = |a: &[&str], w| run_wsl(&exe, a, w);
    start_via(
        &run,
        &exe,
        &distro,
        synthesis_app::proto::VERSION,
        env,
        project,
    )
}

/// Why the toggle cannot be turned on (`None`: it can). Off Windows always; on Windows when
/// `wsl.exe --status` fails (asked once).
pub fn toggle_lock() -> Option<String> {
    static LOCK: OnceLock<Option<String>> = OnceLock::new();
    if !cfg!(windows) {
        return Some("Windows only".into());
    }
    LOCK.get_or_init(|| {
        let r = system_wsl().and_then(|exe| status(&|a, w| run_wsl(&exe, a, w)));
        r.err().map(|e| sanitize_for_display(&e))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str, bom: bool) -> Vec<u8> {
        let mut v: Vec<u8> = if bom { vec![0xFF, 0xFE] } else { vec![] };
        v.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        v
    }

    const LIST: &str = "  NAME            STATE           VERSION\r\n* Ubuntu-24.04    Running         2\r\n  Debian          Stopped         2\r\n  old             Stopped         1\r\n";

    #[test]
    fn the_distro_list_is_read_from_utf16_with_or_without_bom_and_crlf() {
        for bom in [true, false] {
            let d = parse_distros(&decode(&utf16(LIST, bom)));
            assert_eq!(d.len(), 3, "header skipped");
            assert_eq!(
                d[0],
                Distro {
                    name: "Ubuntu-24.04".into(),
                    default: true,
                    running: true,
                    version: 2
                }
            );
            assert!(!d[1].default && !d[1].running && d[1].version == 2);
            assert_eq!(d[2].version, 1);
        }
        assert_eq!(
            parse_distros(&decode(LIST.as_bytes())).len(),
            3,
            "utf-8 too"
        );
    }

    #[test]
    fn a_distro_is_the_default_or_named_and_wsl1_is_refused() {
        let d = parse_distros(LIST);
        assert_eq!(pick(&d, "").unwrap().name, "Ubuntu-24.04");
        assert_eq!(pick(&d, " debian ").unwrap().name, "Debian");
        assert!(pick(&d, "old").unwrap_err().contains("only WSL 2"));
        assert!(pick(&d, "nope").unwrap_err().contains("no WSL distro"));
        assert!(pick(&[], "").unwrap_err().contains("no default"));
        let dash = parse_distros("* -x Running 2");
        assert!(pick(&dash, "").is_err(), "a name that reads as an option");
    }

    #[test]
    fn paths_convert_only_for_the_selected_distro() {
        let no = |_: &str| -> Result<String, String> { panic!("no wslpath for a UNC path") };
        for (p, want) in [
            (r"\\wsl.localhost\Ubuntu\home\me\repo", "/home/me/repo"),
            (r"\\wsl$\Ubuntu\home\me\repo\\", "/home/me/repo"),
            (r"\\WSL.LOCALHOST\ubuntu\Home\Me", "/Home/Me"),
            (r"\\wsl$\Ubuntu", "/"),
            (r"\\wsl$\Ubuntu\\", "/"),
            (r"\\?\UNC\wsl.localhost\Ubuntu\a b", "/a b"),
            ("//wsl.localhost/Ubuntu/x/", "/x"),
        ] {
            assert_eq!(to_linux(p, "Ubuntu", no).as_deref(), Ok(want), "{p}");
        }
        let other = to_linux(r"\\wsl$\Debian\x", "Ubuntu", no).unwrap_err();
        assert!(
            other.contains("Debian") && other.contains("Ubuntu"),
            "{other}"
        );
        assert!(to_linux(r"\\wsl$\Ubuntu\a\..\..\b", "Ubuntu", no).is_err());
        for p in [r"\\server\share\x", "relative\\x", "/home/x", ""] {
            assert!(to_linux(p, "Ubuntu", no).is_err(), "{p}");
        }
    }

    #[test]
    fn drive_paths_go_through_wslpath_never_a_guess() {
        let stub = |p: &str| -> Result<String, String> {
            Ok(format!(
                "/mnt/{}",
                p.replace(':', "").replace('\\', "/").to_lowercase()
            ))
        };
        assert_eq!(
            to_linux(r"C:\Work\Repo", "U", stub).unwrap(),
            "/mnt/c/work/repo"
        );
        assert_eq!(to_linux(r"\\?\D:\x\", "U", stub).unwrap(), "/mnt/d/x/");
        assert!(to_linux(r"C:\x", "U", |_| Err("boom".into())).is_err());
        // an answer that is not an absolute one-line path is refused
        assert!(to_linux(r"C:\x", "U", |_| Ok("rel\nx".into())).is_err());
    }

    #[test]
    fn the_explorer_path_of_a_linux_folder() {
        assert_eq!(
            to_display("Ubuntu", "/home/me"),
            r"\\wsl.localhost\Ubuntu\home\me"
        );
    }

    fn table(
        version_said: &str,
        synt: Option<&str>,
        status_ok: bool,
    ) -> impl Fn(&[&str], Duration) -> Result<Out, String> {
        let (v, s) = (version_said.to_string(), synt.map(String::from));
        move |a: &[&str], _| {
            let ok = |b: Vec<u8>| {
                Ok(Out {
                    ok: true,
                    stdout: b,
                })
            };
            match a {
                ["--status"] if status_ok => ok(vec![]),
                ["--status"] => Ok(Out {
                    ok: false,
                    stdout: vec![],
                }),
                ["-l", "-v"] => ok(utf16(LIST, true)),
                ["-d", _, "--exec", "/bin/sh", ..] => match &s {
                    Some(p) => ok(format!("motd\n{p}\n").into_bytes()),
                    None => Ok(Out {
                        ok: false,
                        stdout: vec![],
                    }),
                },
                ["-d", _, "--exec", _, "--version"] => ok(v.clone().into_bytes()),
                _ => Err(format!("unexpected {a:?}")),
            }
        }
    }

    #[test]
    fn probe_finds_the_engine_or_says_what_to_run() {
        let r = probe(
            &table("synt 1.2.3\n", Some("/home/me/.synt/bin/synt"), true),
            "",
            "1.2.3",
        )
        .unwrap();
        assert_eq!(
            (r.distro.name.as_str(), r.synt.as_str()),
            ("Ubuntu-24.04", "/home/me/.synt/bin/synt")
        );
        let wrong = probe(&table("synt 1.2.2", Some("/x/synt"), true), "", "1.2.3")
            .err()
            .unwrap();
        assert!(
            wrong.contains("exactly 1.2.3") && wrong.contains("install.sh"),
            "{wrong}"
        );
        let none = probe(&table("", None, true), "", "1.2.3").err().unwrap();
        assert!(
            none.contains("not installed") && none.contains("SYNT_VERSION=1.2.3"),
            "{none}"
        );
        let off = probe(&table("", None, false), "", "1.2.3").err().unwrap();
        assert!(off.contains("WSL is not available"), "{off}");
        let one = probe(&table("synt 1.2.3", Some("/x/synt"), true), "old", "1.2.3")
            .err()
            .unwrap();
        assert!(one.contains("only WSL 2"), "{one}");
    }

    #[test]
    fn the_launcher_is_the_documented_command_and_hands_over_no_key() {
        let env = Env::from_pairs([
            ("SystemRoot", r"C:\Windows"),
            ("SYNT_API_KEY", "secret"),
            ("WSLENV", "SYNT_API_KEY"),
            ("GITHUB_TOKEN", "t"),
        ]);
        let l = launcher(
            Path::new(r"C:\Windows\System32\wsl.exe"),
            "Ubuntu",
            "/home/me/r",
            "/h/.synt/bin/synt",
            &env,
        );
        let args: Vec<_> = l.args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-d",
                "Ubuntu",
                "--cd",
                "/home/me/r",
                "--exec",
                "/h/.synt/bin/synt",
                "engine",
                "--stdio"
            ]
        );
        assert_eq!(l.program, r"C:\Windows\System32\wsl.exe");
        assert_eq!(
            l.env,
            vec![("SystemRoot".to_string(), r"C:\Windows".to_string())]
        );
        assert_eq!(
            (l.hello_timeout, l.start_cwd.as_deref()),
            (COLD_START, Some("/home/me/r"))
        );
    }

    #[test]
    fn the_setting_is_read_user_then_managed_and_a_bad_file_is_an_error_not_off() {
        let d = tempfile::tempdir().unwrap();
        let (user, managed) = (
            d.path().join("settings.json"),
            d.path().join("managed.json"),
        );
        let read = || read_setting(d.path(), &managed);
        assert_eq!(read(), Ok((false, String::new())), "absent files: off");
        std::fs::write(
            &user,
            "\u{feff}{\"synt\":{\"desktop\":{\"wsl\":{\"enabled\":true,\"distro\":\"Debian\"}}}}",
        )
        .unwrap();
        assert_eq!(read(), Ok((true, "Debian".into())), "BOM tolerated");
        // the managed file wins per key; a key it does not set stays the user's
        std::fs::write(
            &managed,
            r#"{"synt":{"desktop":{"wsl":{"enabled":false}}}}"#,
        )
        .unwrap();
        assert_eq!(read(), Ok((false, "Debian".into())));
        std::fs::remove_file(&managed).unwrap();
        for bad in [
            "{not json",
            "[]",
            r#"{"synt":{"desktop":{"wsl":{"enabled":"yes"}}}}"#,
            r#"{"synt":{"desktop":{"wsl":{"enabled":1}}}}"#,
            r#"{"synt":{"desktop":{"wsl":{"distro":3}}}}"#,
            r#"{"synt":{"desktop":{"wsl":{"distro":"-x"}}}}"#,
            r#"{"synt":{"desktop":{"wsl":5}}}"#,
        ] {
            std::fs::write(&user, bad).unwrap();
            assert!(read().is_err(), "{bad}");
        }
        std::fs::write(&user, vec![b' '; (1 << 20) + 1]).unwrap();
        assert!(read().is_err(), "oversized");
        std::fs::remove_file(&user).unwrap();
        std::fs::create_dir(&user).unwrap();
        assert!(read().is_err(), "not a file");
        if !cfg!(windows) {
            assert_eq!(toggle_lock().as_deref(), Some("Windows only"));
            assert_eq!(enabled(d.path()), Ok(false));
        }
    }

    #[test]
    fn engine_paths_on_a_windows_drive_are_refused_and_odd_components_too() {
        assert!(native("/home/me/.synt/bin/synt") && native("/usr/local/bin/synt"));
        for p in ["/mnt/c/x/synt", "/mnt", "rel", "/a\nb"] {
            assert!(!native(p), "{p}");
        }
        let no = |_: &str| -> Result<String, String> { unreachable!() };
        for p in [r"\\wsl$\U\a\.\b", r"\\wsl$\U\a.\b", r"\\wsl$\U\a \b"] {
            assert!(to_linux(p, "U", no).is_err(), "{p}");
        }
        let t = table("synt 1.2.3", Some("/mnt/c/evil/synt"), true);
        assert!(
            probe(&t, "", "1.2.3")
                .unwrap_err()
                .contains("not installed")
        );
        assert!(install_command("1.2.3").contains("releases/download/v1.2.3/install.sh"));
    }

    #[test]
    fn managed_policy_beats_the_user_file_in_every_combination() {
        let d = tempfile::tempdir().unwrap();
        let (user, managed) = (d.path().join("settings.json"), d.path().join("m.json"));
        let doc = |v: Option<&str>| match v {
            None => String::from("{}"),
            Some(x) => format!(r#"{{"synt":{{"desktop":{{"wsl":{x}}}}}}}"#),
        };
        let t = r#"{"enabled":true}"#;
        let f = r#"{"enabled":false}"#;
        // (user, managed) -> enabled
        for (u, m, want) in [
            (None, None, false),
            (Some(t), None, true),
            (Some(f), None, false),
            (None, Some(t), true),
            (None, Some(f), false),
            (Some(t), Some(f), false), // managed off cannot be overridden
            (Some(f), Some(t), true),  // managed on cannot be switched off
            (Some(t), Some(t), true),
            (Some(f), Some(f), false),
        ] {
            std::fs::write(&user, doc(u)).unwrap();
            std::fs::write(&managed, doc(m)).unwrap();
            assert_eq!(
                enabled_on(true, d.path(), &managed),
                Ok(want),
                "{u:?} {m:?}"
            );
        }
        // a managed distro wins; a user one stays when managed sets none
        std::fs::write(&user, doc(Some(r#"{"enabled":true,"distro":"U"}"#))).unwrap();
        std::fs::write(&managed, doc(Some(r#"{"distro":"M"}"#))).unwrap();
        assert_eq!(read_setting(d.path(), &managed), Ok((true, "M".into())));
        std::fs::write(&managed, doc(Some(r#"{"enabled":true}"#))).unwrap();
        assert_eq!(read_setting(d.path(), &managed), Ok((true, "U".into())));
        // an invalid managed file fails closed, whatever the user file says (also when it says off)
        std::fs::write(&user, doc(Some(f))).unwrap();
        for bad in [
            "{nope",
            "[]",
            &doc(Some(r#"{"enabled":"on"}"#)),
            &doc(Some(r#"{"distro":1}"#)),
        ] {
            std::fs::write(&managed, bad).unwrap();
            assert!(enabled_on(true, d.path(), &managed).is_err(), "{bad}");
            assert!(read_setting(d.path(), &managed).is_err(), "{bad}");
        }
        // not Windows: nothing is read, nothing runs in WSL
        assert_eq!(enabled_on(false, d.path(), &managed), Ok(false));
    }

    #[test]
    fn a_reveal_path_is_a_windows_drive_path_or_the_distro_unc() {
        for (l, want) in [
            ("/mnt/c/work/a b.rs", r"C:\work\a b.rs"),
            ("/mnt/d", r"D:\"),
            ("/mnt/c/", r"C:\"),
            ("/home/me/x/", r"\\wsl.localhost\U\home\me\x"),
            ("/mnt/wsl/x", r"\\wsl.localhost\U\mnt\wsl\x"),
            ("/", r"\\wsl.localhost\U\"),
        ] {
            assert_eq!(reveal_path("U", l).as_deref(), Ok(want), "{l}");
        }
        for bad in ["rel", "/a/../b", "/a/./b", "/a\nb", ""] {
            assert!(reveal_path("U", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_engine_must_be_where_it_was_asked_to_be() {
        let ok = |a, c, r| check_reported(a, Some(c), Some(r));
        assert!(ok("/h/r", "/h/r", "/h/r").is_ok());
        assert!(
            ok("/h/r/sub", "/h/r/sub", "/h/r").is_ok(),
            "root is an ancestor"
        );
        assert!(ok("/h/r", "/h/r", "/").is_ok());
        assert!(ok("/h/r", "/h/other", "/h/other").is_err());
        assert!(
            ok("/h/r", "/h/r", "/h/r/sub").is_err(),
            "root below the folder"
        );
        assert!(
            ok("/h/r", "/h/r", "/h/rx").is_err(),
            "a prefix is not an ancestor"
        );
        assert!(check_reported("/h/r", None, Some("/h/r")).is_err());
        assert!(check_reported("/h/r", Some("/h/r"), None).is_err());
    }
}
