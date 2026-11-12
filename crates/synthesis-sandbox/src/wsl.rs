//! WSL hardening: WSL is not isolation, so on WSL the sandbox also closes the Windows interop.
//!
//! What a Linux process in WSL can reach: Windows programs through the `WSLInterop` binfmt entry
//! and the socket named by `WSL_INTEROP` (`/run/WSL/<pid>_interop`), `cmd.exe` / `powershell.exe`
//! found on a `PATH` that WSL extends with `/mnt/c/...`, and every Windows drive mounted writable
//! under `/mnt/*` (drvfs / 9p). Controls, by mechanism:
//!
//! - Environment ([`scrub_env`], every backend and `Env::child_env`): `WSL*` variables removed
//!   and Windows directories removed from `PATH`. A process that already knows a path can still
//!   use it; this only stops accidents and lazy attacks.
//! - bubblewrap (`BwrapBackend`): `/run` is a fresh tmpfs, so `/run/WSL` and the interop socket do
//!   not exist; every Windows drive is bound read-only; only a writable root that lies strictly
//!   inside a drive and contains the cwd is writable ([`check_writable_roots`]).
//! - Landlock is REFUSED on WSL: it is an allow-list for files, but it cannot stop `connect()` to
//!   a pathname unix socket (no AF_UNIX scoping), so a Windows `.exe` copied into the workspace
//!   could still reach the interop socket through binfmt. Without bubblewrap, commands are
//!   refused (also in best-effort) unless the owner sets the sandbox mode to `off`.
//!
//! What remains: the binfmt entry cannot be removed without root; a process started before the
//! sandbox (or one that holds the socket fd) is not covered; Windows drives stay READABLE (only
//! writes are denied), so Windows-side files such as `/mnt/c/Users/*/.ssh` can be read.

use std::ffi::OsString;
use std::path::Path;

use synthesis_platform::wsl::{WslHost, is_interop_var};

use crate::{SandboxError, SandboxPolicy, canon_lossy};

/// The real host when running on WSL (probed once), else `None`.
pub fn host() -> Option<&'static WslHost> {
    WslHost::real()
}

/// Drop `WSL*` variables and Windows directories from `PATH`/`Path`.
pub fn scrub_env(env: &mut Vec<(OsString, OsString)>, host: &WslHost) {
    env.retain(|(k, _)| !is_interop_var(&k.to_string_lossy()));
    for (k, v) in env.iter_mut() {
        if k == "PATH" || k == "Path" {
            *v = host.scrub_path(v);
        }
    }
}

/// Writes on Windows drives are denied. A writable root (or the temp dir) on a drive is allowed
/// only if it lies strictly inside the drive (never `/mnt`, `/mnt/c` or a mount point) and
/// contains the command's cwd: the workspace itself. Violations are `InvalidPolicy`, never a
/// degradation to unconfined.
pub fn check_writable_roots(
    policy: &SandboxPolicy,
    cwd: Option<&Path>,
    host: &WslHost,
) -> Result<(), SandboxError> {
    let deny = |p: &Path, why: &str| {
        Err(SandboxError::InvalidPolicy(format!(
            "WSL: {} is on a Windows drive and {why}; writes under /mnt are denied outside the workspace",
            p.display()
        )))
    };
    let cwd = cwd.map(canon_lossy);
    for root in policy.writable_roots.iter().chain(&policy.tmp_dir) {
        let real = canon_lossy(root);
        if !host.is_windows_path(&real) {
            continue;
        }
        if policy.tmp_dir.as_ref() == Some(root) {
            return deny(root, "cannot be the temp dir");
        }
        let inside_drive = host
            .windows_mounts
            .iter()
            .any(|m| real.starts_with(m) && real != *m)
            || (real.starts_with(synthesis_platform::wsl::DEFAULT_MOUNT_ROOT)
                && real.components().count() >= 4);
        if !inside_drive {
            return deny(root, "is a drive or mount root");
        }
        if !cwd.as_ref().is_some_and(|c| c.starts_with(&real)) {
            return deny(root, "does not contain the working directory");
        }
    }
    Ok(())
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
    use std::path::PathBuf;
    use synthesis_platform::wsl::WslVersion;

    fn host() -> WslHost {
        WslHost {
            version: WslVersion::V2,
            windows_mounts: vec![PathBuf::from("/mnt/c"), PathBuf::from("/windir/d")],
        }
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
        let mut env: Vec<(OsString, OsString)> = vec![
            ("WSL_INTEROP".into(), "/run/WSL/1_interop".into()),
            ("WSLENV".into(), "A".into()),
            ("WSL_DISTRO_NAME".into(), "U".into()),
            ("PATH".into(), path),
            ("HOME".into(), "/home/u".into()),
        ];
        scrub_env(&mut env, &host());
        let names: Vec<_> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "HOME"]);
        assert_eq!(env[0].1, "/usr/bin");
        // injected through build_env_on, the same result for a real command env
        let c = crate::SandboxedCommand {
            env: vec![("WSL_INTEROP".into(), "x".into())],
            ..Default::default()
        };
        assert!(crate::build_env_on(&SandboxPolicy::default(), &c, Some(&host())).is_empty());
        assert_eq!(
            crate::build_env_on(&SandboxPolicy::default(), &c, None).len(),
            1
        );
    }

    #[test]
    fn mount_policy() {
        let h = host();
        let ok = |p: &SandboxPolicy, cwd: &str| check_writable_roots(p, Some(Path::new(cwd)), &h);
        // linux paths and the workspace inside a drive (containing the cwd)
        assert!(ok(&pol(&["/home/u/ws"], Some("/home/u/t")), "/home/u/ws").is_ok());
        assert!(ok(&pol(&["/mnt/c/Users/u/ws"], None), "/mnt/c/Users/u/ws/sub").is_ok());
        assert!(ok(&pol(&["/windir/d/ws"], None), "/windir/d/ws").is_ok());
        // drive roots, /mnt, other trees on the drive, no cwd, drive temp dir
        for (roots, tmp, cwd) in [
            (vec!["/mnt/c"], None, "/mnt/c/a"),
            (vec!["/mnt"], None, "/mnt/c/a"),
            (vec!["/windir/d"], None, "/windir/d/a"),
            (vec!["/home/u/ws", "/mnt/c/Users"], None, "/home/u/ws"),
            (vec!["/mnt/c/Users/u/ws"], None, "/home/u/ws"),
            (vec!["/home/u/ws"], Some("/mnt/c/tmp"), "/home/u/ws"),
        ] {
            let e = ok(&pol(&roots, tmp), cwd);
            assert!(
                matches!(e, Err(SandboxError::InvalidPolicy(_))),
                "{roots:?} {tmp:?}"
            );
        }
        assert!(check_writable_roots(&pol(&["/mnt/c/Users/u/ws"], None), None, &h).is_err());
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
