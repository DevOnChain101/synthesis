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
//!   or read-only, and no writable root may be on a drive ([`check_policy`]).
//! - seccomp (helper `--no-vsock`, in front of bwrap): `socket(AF_VSOCK)`, io_uring and the x32
//!   ABI fail.
//! - Landlock is REFUSED on WSL: it cannot stop `connect()` to a pathname unix socket.
//!
//! What remains: the binfmt entry cannot be removed without root; a process started before the
//! sandbox, or one holding the socket fd, is not covered; a workspace on a Windows drive is
//! always refused (see [`check_policy`]); interop closure is NOT proven until the
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
    /// Of `/mnt/wsl`, `/mnt/wslg` and `/usr/lib/wsl/drivers`, the ones that exist: masked with a
    /// tmpfs (the last one, the Windows driver store, only while drives are hidden: GPU
    /// compute needs it, and `hide_windows_drives=false` is the opt-out).
    pub mask: Vec<PathBuf>,
    /// Every Windows drive: the drvfs mounts plus any existing `/mnt/<single letter>` directory,
    /// so a drive that `/proc/mounts` does not show is still hidden / read-only.
    pub drives: Vec<PathBuf>,
}

/// The mask candidates of [`WslCtx::mask`].
pub(crate) const MASKABLE: [&str; 3] = ["/mnt/wsl", "/mnt/wslg", "/usr/lib/wsl/drivers"];
pub(crate) const DRIVER_STORE: &str = "/usr/lib/wsl/drivers";

/// Drives = mounts plus the `/mnt/<letter>` directories in `mnt_letters` (names, injectable).
pub(crate) fn drives_of(host: &WslHost, mnt_letters: &[String]) -> Vec<PathBuf> {
    let mut v = host.windows_mounts.clone();
    for l in mnt_letters.iter().filter(|l| l.len() == 1 && l.is_ascii()) {
        let d = Path::new(DEFAULT_MOUNT_ROOT).join(l);
        if !v.contains(&d) {
            v.push(d);
        }
    }
    v
}

impl WslCtx {
    pub fn current() -> Option<Self> {
        let host = host()?;
        let resolv = std::fs::canonicalize("/etc/resolv.conf")
            .ok()
            .filter(|t| t.starts_with("/mnt/wsl") || t.starts_with("/mnt/wslg"));
        let mask = MASKABLE
            .map(PathBuf::from)
            .into_iter()
            .filter(|m| m.exists())
            .collect();
        let letters: Vec<String> = std::fs::read_dir(DEFAULT_MOUNT_ROOT)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        let drives = drives_of(&host, &letters);
        Some(Self {
            host,
            resolv,
            mask,
            drives,
        })
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

/// Canonicalise every path of `policy` (so the check and the bind arguments agree), then apply
/// the one WSL rule for writes: NO writable root and no temp dir may be on a Windows drive
/// (anything under `/mnt`, a drvfs mount point, or a root that contains one, `/` included).
/// A workspace on a drive is therefore ALWAYS refused today. An allow-list of drive workspaces
/// was dropped on purpose: the standard policy always carries `.git/hooks`, `.synt` and
/// `AGENTS.md` as protected paths, and a bind mount cannot protect them there (names on a drive
/// are case-insensitive and may have 8.3 aliases, trailing dots or spaces, NTFS streams), so
/// every such workspace would be refused anyway; a deny-list of system directories would only
/// invite bypasses. Move the project into the Linux filesystem (`~/`). Violations are
/// `InvalidPolicy`, never a degradation to unconfined. Returns the canonical policy to use.
pub fn check_policy(policy: &SandboxPolicy, host: &WslHost) -> Result<SandboxPolicy, SandboxError> {
    check_policy_with(policy, host, &canon_lossy)
}

/// [`check_policy`] with an injected canonicaliser (tests use the identity).
pub fn check_policy_with(
    policy: &SandboxPolicy,
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
    for root in p.writable_roots.iter().chain(&p.tmp_dir) {
        let contains_drive = Path::new(DEFAULT_MOUNT_ROOT).starts_with(root)
            || host.windows_mounts.iter().any(|m| m.starts_with(root));
        if contains_drive || host.is_windows_path(root) {
            return Err(SandboxError::InvalidPolicy(format!(
                "WSL: {} is on (or contains) a Windows drive; writes under /mnt are always denied and a workspace there cannot be protected (.git/hooks, .synt); move the project into the Linux filesystem (~/)",
                root.display()
            )));
        }
    }
    Ok(p)
}

/// [`check_policy`] against the real host, for callers that must refuse before they create
/// anything (the terminal creates the private temp dir only after this passes).
pub fn preflight(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    match host() {
        Some(h) => check_policy(policy, &h).map(|_| ()),
        None => Ok(()),
    }
}

/// Fail closed when the drives cannot be located: a host that was detected as WSL but whose
/// `/proc/mounts` is unreadable (or only remembered) has unknown drives.
pub(crate) fn require_known_mounts(host: &WslHost) -> Result<(), SandboxError> {
    if host.mounts_known {
        Ok(())
    } else {
        Err(SandboxError::Unavailable(
            "WSL: /proc/mounts could not be read, so the Windows drives cannot be located; refusing to run commands (use --sandbox off at your own risk)".into(),
        ))
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
            mounts_known: true,
        }
    }
    fn check(p: &SandboxPolicy, h: &WslHost) -> Result<SandboxPolicy, SandboxError> {
        check_policy_with(p, h, &|x| x.to_path_buf())
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
    fn writable_roots_on_a_drive_are_always_refused() {
        let h = host();
        // Linux paths pass, with the standard policy of a Linux workspace
        assert!(check(&pol(&["/home/u/ws"], Some("/home/u/t")), &h).is_ok());
        let std = SandboxPolicy::standard(Path::new("/home/u/ws"), Path::new("/home/u/t"));
        assert!(check(&std, &h).is_ok());
        for (roots, tmp) in [
            (vec!["/"], None),
            (vec!["/mnt"], None),
            (vec!["/mnt/c"], None),
            (vec!["/mnt/c/Users"], None),
            (vec!["/mnt/c/Users/u"], None),
            (vec!["/mnt/c/Users/u/ws"], None), // deep workspace: refused too
            (vec!["/mnt/wsl/x/y"], None),
            (vec!["/mnt/wslg/a/b/c"], None),
            (vec!["/windir"], None), // ancestor of a custom mount
            (vec!["/windir/d"], None),
            (vec!["/windir/d/a/b/ws"], None),
            (vec!["/home/u/ws", "/mnt/c/Users/u/ws"], None),
            (vec!["/home/u/ws"], Some("/mnt/c/Users/u/tmp")),
            (vec!["/home/u/ws"], Some("/windir/d/t")),
        ] {
            let e = check(&pol(&roots, tmp), &h);
            assert!(
                matches!(e, Err(SandboxError::InvalidPolicy(_))),
                "{roots:?} {tmp:?}"
            );
        }
        let e = check(&pol(&["/mnt/c/Users/u/ws"], None), &h).unwrap_err();
        assert!(
            e.to_string()
                .contains("move the project into the Linux filesystem"),
            "{e}"
        );
    }

    #[test]
    fn drives_include_unlisted_mnt_letters_and_unknown_mounts_fail_closed() {
        let mut h = host();
        let d = drives_of(&h, &["c".into(), "z".into(), "wsl".into(), "ab".into()]);
        assert_eq!(
            d,
            [
                PathBuf::from("/mnt/c"),
                PathBuf::from("/windir/d"),
                PathBuf::from("/mnt/z")
            ]
        );
        assert!(require_known_mounts(&h).is_ok());
        h.mounts_known = false;
        assert!(matches!(
            require_known_mounts(&h),
            Err(SandboxError::Unavailable(_))
        ));
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
