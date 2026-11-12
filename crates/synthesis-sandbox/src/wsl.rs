//! WSL hardening: WSL is not isolation, so on WSL the sandbox also closes the Windows interop.
//!
//! What a Linux process in WSL can reach: Windows programs through the `WSLInterop` binfmt entry
//! and the socket named by `WSL_INTEROP` (`/run/WSL/<pid>_interop`), `cmd.exe` / `powershell.exe`
//! found on a `PATH` that WSL extends with `/mnt/c/...`, the Windows host over AF_VSOCK, the
//! WSLg sockets under `/mnt/wslg`, and every Windows drive mounted writable under `/mnt/*`
//! (drvfs / 9p). Controls, by mechanism (full table: docs/specs/sandbox.md):
//!
//! - Environment ([`scrub_env`], every backend and `Env::child_env`): `WSL*`, `XDG_RUNTIME_DIR`,
//!   `WAYLAND_DISPLAY`, `DISPLAY`, `PULSE_SERVER` removed; `PATH` entries canonicalised and
//!   anything relative, unresolvable or on a Windows path dropped. Not a boundary.
//! - bubblewrap: `/run` is a fresh tmpfs (no `/run/WSL`), `/mnt/wsl` and `/mnt/wslg` are tmpfs
//!   (the real `resolv.conf` target is bound back read-only), Windows drives are hidden (default)
//!   or read-only, writes only in a workspace that passes [`check_policy`].
//! - seccomp (helper `--no-vsock`, in front of bwrap): `socket(AF_VSOCK)` fails.
//! - Landlock is REFUSED on WSL: it cannot stop `connect()` to a pathname unix socket.
//!
//! What remains: the binfmt entry cannot be removed without root; a process started before the
//! sandbox, or one holding the socket fd, is not covered; a Windows workspace cannot protect
//! `.git/hooks` (see [`check_policy`]) so it is refused; interop closure is NOT proven until the
//! ignored real-WSL2 test has been run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use synthesis_platform::wsl::{DEFAULT_MOUNT_ROOT, WslHost, is_interop_var};

use crate::{SandboxError, SandboxPolicy, canon_lossy};

/// The host as it is now (mounts re-read on every call), `None` when not WSL.
pub fn host() -> Option<WslHost> {
    WslHost::current()
}

pub fn is_wsl() -> bool {
    host().is_some()
}

/// What `BwrapBackend` needs to know about this WSL instance.
pub(crate) struct WslCtx {
    pub host: WslHost,
    /// `/etc/resolv.conf` resolves into `/mnt/wsl` or `/mnt/wslg` (WSL generates it there).
    pub resolv: Option<PathBuf>,
    /// Of `/mnt/wsl` and `/mnt/wslg`, the ones that exist: masked with a tmpfs.
    pub mask: Vec<PathBuf>,
}

impl WslCtx {
    pub fn current() -> Option<Self> {
        let host = host()?;
        let resolv = std::fs::canonicalize("/etc/resolv.conf")
            .ok()
            .filter(|t| t.starts_with("/mnt/wsl") || t.starts_with("/mnt/wslg"));
        let mask = ["/mnt/wsl", "/mnt/wslg"]
            .map(PathBuf::from)
            .into_iter()
            .filter(|m| m.exists())
            .collect();
        Some(Self { host, resolv, mask })
    }
}

/// Drop interop / host-integration variables and Windows directories from `PATH`/`Path`.
pub fn scrub_env(env: &mut Vec<(OsString, OsString)>, host: &WslHost) {
    env.retain(|(k, _)| !is_interop_var(&k.to_string_lossy()));
    for (k, v) in env.iter_mut() {
        if k == "PATH" || k == "Path" {
            *v = host.scrub_path(v);
        }
    }
}

/// Directory names (case-insensitive) below a drive that no writable root may contain.
const SYSTEM_DIRS: &[&str] = &[
    "windows",
    "appdata",
    "programdata",
    "program files",
    "program files (x86)",
    "$recycle.bin",
    "system volume information",
];

/// Fewest path components a workspace on a drive must have below the drive root (`Users/u/ws`).
const MIN_DEPTH_BELOW_DRIVE: usize = 3;

/// Canonicalise every path of `policy` (so the check and the bind arguments agree), then apply
/// the WSL rules; returns the canonical policy to use. Violations are `InvalidPolicy`, never a
/// degradation to unconfined. Rules, for each writable root and the temp dir:
/// - a root that contains a Windows drive (`/`, `/mnt`, a mount point, an ancestor of a custom
///   mount) is refused, wherever it is;
/// - on a drive: not the temp dir; below a real drive (`/mnt/<letter>` or a drvfs mount; `/mnt/wsl`
///   is not one); at least [`MIN_DEPTH_BELOW_DRIVE`] components deep; no [`SYSTEM_DIRS`] component;
///   it must contain the cwd;
/// - `deny_paths` / `readonly_paths` inside such a root are refused: names on a drive are case
///   insensitive and may have 8.3 aliases, so a bind mount cannot protect `.git/hooks` there.
///   Move the project into the Linux filesystem (`~/`).
pub fn check_policy(
    policy: &SandboxPolicy,
    cwd: Option<&Path>,
    host: &WslHost,
) -> Result<SandboxPolicy, SandboxError> {
    check_policy_with(policy, cwd, host, &canon_lossy)
}

/// [`check_policy`] with an injected canonicaliser (tests use the identity).
pub fn check_policy_with(
    policy: &SandboxPolicy,
    cwd: Option<&Path>,
    host: &WslHost,
    canon_one: &dyn Fn(&Path) -> PathBuf,
) -> Result<SandboxPolicy, SandboxError> {
    let canon = |v: &[PathBuf]| v.iter().map(|p| canon_one(p)).collect::<Vec<_>>();
    let mut p = policy.clone();
    p.readable_roots = canon(&p.readable_roots);
    p.writable_roots = canon(&p.writable_roots);
    p.deny_paths = canon(&p.deny_paths);
    p.readonly_paths = canon(&p.readonly_paths);
    p.tmp_dir = p.tmp_dir.as_deref().map(canon_one);
    let cwd = cwd.map(canon_one);
    let deny = |root: &Path, why: &str| {
        Err(SandboxError::InvalidPolicy(format!(
            "WSL: writable root {} {why}; writes under /mnt are denied outside a workspace",
            root.display()
        )))
    };
    for root in p.writable_roots.iter().chain(&p.tmp_dir) {
        if Path::new(DEFAULT_MOUNT_ROOT).starts_with(root)
            || host.windows_mounts.iter().any(|m| m.starts_with(root))
        {
            return deny(root, "contains the Windows drives");
        }
        if !host.is_windows_path(root) {
            continue;
        }
        if p.tmp_dir.as_deref() == Some(root.as_path()) {
            return deny(root, "is on a Windows drive and cannot be the temp dir");
        }
        let Some(rel) = below_drive(root, host) else {
            return deny(root, "is under /mnt but not on a Windows drive");
        };
        if rel.components().count() < MIN_DEPTH_BELOW_DRIVE {
            return deny(root, "is too close to the drive root");
        }
        let sys = rel.components().any(|c| {
            let n = c.as_os_str().to_string_lossy().to_lowercase();
            SYSTEM_DIRS.contains(&n.as_str())
        });
        if sys {
            return deny(root, "is inside a Windows system or profile directory");
        }
        if !cwd.as_ref().is_some_and(|c| c.starts_with(root)) {
            return deny(root, "does not contain the working directory");
        }
        if p.deny_paths
            .iter()
            .chain(&p.readonly_paths)
            .any(|d| d.starts_with(root))
        {
            return deny(
                root,
                "holds protected paths (.git/hooks, .synt, ...) that cannot be enforced on a Windows drive (case-insensitive names, 8.3 aliases); move the project into the Linux filesystem (~/)",
            );
        }
    }
    Ok(p)
}

/// `root` relative to its Windows drive: a drvfs mount point, else `/mnt/<single letter>`.
fn below_drive(root: &Path, host: &WslHost) -> Option<PathBuf> {
    if let Some(rel) = host
        .windows_mounts
        .iter()
        .find_map(|m| root.strip_prefix(m).ok())
    {
        return Some(rel.to_path_buf());
    }
    let rel = root.strip_prefix(DEFAULT_MOUNT_ROOT).ok()?;
    let mut c = rel.components();
    let drive = c.next()?.as_os_str().to_string_lossy().into_owned();
    (drive.len() == 1 && drive.is_ascii()).then(|| c.as_path().to_path_buf())
}

/// [`check_policy`] against the real host, for callers that must refuse before they create
/// anything (the terminal creates the private temp dir only after this passes).
pub fn preflight(policy: &SandboxPolicy, cwd: Option<&Path>) -> Result<(), SandboxError> {
    match host() {
        Some(h) => check_policy(policy, cwd, &h).map(|_| ()),
        None => Ok(()),
    }
}

/// Landlock cannot be used on WSL (see module docs).
pub fn refuse_landlock(host: Option<&WslHost>) -> Result<(), SandboxError> {
    match host {
        None => Ok(()),
        Some(_) => Err(SandboxError::Unsupported(
            "WSL: Landlock cannot hide the Windows interop socket (AF_UNIX); bubblewrap is required, or set the sandbox to off".into(),
        )),
    }
}

/// Reason text for `skipped_backends` / doctor on WSL.
pub const LANDLOCK_NOT_USED: &str = "WSL: Landlock is not used (it cannot block the Windows interop socket); bubblewrap is required (`sudo apt install bubblewrap`), or run with the sandbox off at your own risk";

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_platform::wsl::WslVersion;

    fn host() -> WslHost {
        WslHost {
            version: WslVersion::V2,
            windows_mounts: vec![PathBuf::from("/mnt/c"), PathBuf::from("/windir/d")],
        }
    }
    fn check(
        p: &SandboxPolicy,
        cwd: Option<&Path>,
        h: &WslHost,
    ) -> Result<SandboxPolicy, SandboxError> {
        check_policy_with(p, cwd, h, &|x| x.to_path_buf())
    }
    fn pol(roots: &[&str], tmp: Option<&str>) -> SandboxPolicy {
        SandboxPolicy {
            writable_roots: roots.iter().map(PathBuf::from).collect(),
            tmp_dir: tmp.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn env_scrub() {
        let path = std::env::join_paths(["/usr/bin", "/mnt/c/Windows", "/windir/d/x"]).unwrap();
        let mut env: Vec<(OsString, OsString)> = ["WSL_INTEROP", "WSLENV", "WSL_DISTRO_NAME"]
            .iter()
            .chain(&[
                "XDG_RUNTIME_DIR",
                "WAYLAND_DISPLAY",
                "DISPLAY",
                "PULSE_SERVER",
            ])
            .map(|k| ((*k).into(), "x".into()))
            .collect();
        env.push(("PATH".into(), path));
        env.push(("HOME".into(), "/home/u".into()));
        scrub_env(&mut env, &host());
        let names: Vec<_> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "HOME"]);
        // through build_env_on, the same for a real command env (listed in env_passthrough too)
        let c = crate::SandboxedCommand {
            env: vec![
                ("WSL_INTEROP".into(), "x".into()),
                ("DISPLAY".into(), ":0".into()),
            ],
            ..Default::default()
        };
        assert!(crate::build_env_on(&SandboxPolicy::default(), &c, Some(&host())).is_empty());
        assert_eq!(
            crate::build_env_on(&SandboxPolicy::default(), &c, None).len(),
            2
        );
    }

    #[test]
    fn mount_policy_refuses_everything_but_a_deep_workspace() {
        let h = host();
        let ok = |p: &SandboxPolicy, cwd: &str| check(p, Some(Path::new(cwd)), &h);
        // linux paths, and a workspace deep inside a drive containing the cwd
        assert!(ok(&pol(&["/home/u/ws"], Some("/home/u/t")), "/home/u/ws").is_ok());
        assert!(ok(&pol(&["/mnt/c/Users/u/ws"], None), "/mnt/c/Users/u/ws/sub").is_ok());
        assert!(ok(&pol(&["/windir/d/a/b/ws"], None), "/windir/d/a/b/ws").is_ok());
        for (roots, tmp, cwd) in [
            (vec!["/"], None, "/home/u"),
            (vec!["/mnt"], None, "/mnt/c/a"),
            (vec!["/mnt/c"], None, "/mnt/c/a"),
            (vec!["/mnt/c/Users"], None, "/mnt/c/Users/u"),
            (vec!["/mnt/c/Users/u"], None, "/mnt/c/Users/u/ws"),
            (vec!["/mnt/wsl/x/y"], None, "/mnt/wsl/x/y"),
            (vec!["/mnt/wslg/a/b/c"], None, "/mnt/wslg/a/b/c"),
            // a custom mount is a drive; its ancestor contains the drive
            (vec!["/windir"], None, "/windir/d/a"),
            (vec!["/windir/d"], None, "/windir/d/a"),
            (vec!["/windir/d/a"], None, "/windir/d/a"),
            // system / profile directories, any case
            (
                vec!["/mnt/c/Users/u/AppData/x"],
                None,
                "/mnt/c/Users/u/AppData/x",
            ),
            (vec!["/mnt/c/Windows/a/b"], None, "/mnt/c/Windows/a/b"),
            (
                vec!["/mnt/c/a/Program Files/b"],
                None,
                "/mnt/c/a/Program Files/b",
            ),
            (
                vec!["/mnt/c/ProgramData/a/b"],
                None,
                "/mnt/c/ProgramData/a/b",
            ),
            // another tree on the drive next to a Linux workspace; cwd elsewhere; drive temp dir
            (vec!["/home/u/ws", "/mnt/c/Users/u/ws"], None, "/home/u/ws"),
            (vec!["/mnt/c/Users/u/ws"], None, "/home/u/ws"),
            (vec!["/home/u/ws"], Some("/mnt/c/Users/u/tmp"), "/home/u/ws"),
        ] {
            let e = ok(&pol(&roots, tmp), cwd);
            assert!(
                matches!(e, Err(SandboxError::InvalidPolicy(_))),
                "{roots:?} {tmp:?}"
            );
        }
        assert!(check(&pol(&["/mnt/c/Users/u/ws"], None), None, &h).is_err());
    }

    #[test]
    fn protected_paths_on_a_drive_workspace_are_refused() {
        let h = host();
        let mut p = pol(&["/mnt/c/Users/u/ws"], None);
        assert!(check(&p, Some(Path::new("/mnt/c/Users/u/ws")), &h).is_ok());
        p.readonly_paths = vec!["/mnt/c/Users/u/ws/.git/hooks".into()];
        let e = check(&p, Some(Path::new("/mnt/c/Users/u/ws")), &h).unwrap_err();
        assert!(
            e.to_string()
                .contains("cannot be enforced on a Windows drive"),
            "{e}"
        );
        let mut p = pol(&["/mnt/c/Users/u/ws"], None);
        p.deny_paths = vec!["/mnt/c/Users/u/ws/secret".into()];
        assert!(check(&p, Some(Path::new("/mnt/c/Users/u/ws")), &h).is_err());
        // the standard policy of a Linux workspace is fine
        let std = SandboxPolicy::standard(Path::new("/home/u/ws"), Path::new("/home/u/t"));
        assert!(check(&std, Some(Path::new("/home/u/ws")), &h).is_ok());
    }

    #[test]
    fn landlock_refused_only_on_wsl() {
        assert!(refuse_landlock(None).is_ok());
        assert!(matches!(
            refuse_landlock(Some(&host())),
            Err(SandboxError::Unsupported(_))
        ));
    }
}
