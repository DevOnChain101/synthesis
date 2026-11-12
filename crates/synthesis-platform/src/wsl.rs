//! WSL detection and the Windows-interop surface of a Linux process running inside it.
//!
//! WSL is not isolation: a Linux process can start Windows programs (`cmd.exe`, `powershell.exe`)
//! through the `WSLInterop` binfmt entry and the interop socket named by `WSL_INTEROP`
//! (under `/run/WSL`), and can write the Windows drives mounted under `/mnt/*` (drvfs / 9p).
//! This module only *describes* that surface; `synthesis-sandbox` and `Env::child_env` act on it.
//!
//! Everything reads files through an injected `read` (path -> text), so it is tested without WSL.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Returns a file's text, or `None` when it is missing or unreadable.
pub type Read<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Where Windows drives are mounted by default. Always treated as Windows, whatever `/proc/mounts` says.
pub const DEFAULT_MOUNT_ROOT: &str = "/mnt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WslVersion {
    V1,
    V2,
}

/// WSL version from the kernel's own words: both versions say "microsoft" (WSL1 `...-Microsoft`,
/// WSL2 `...-microsoft-standard-WSL2`); only WSL2 also says "wsl2". `None` = not WSL.
pub fn version(read: Read) -> Option<WslVersion> {
    let text = ["/proc/sys/kernel/osrelease", "/proc/version"]
        .iter()
        .filter_map(|p| read(p))
        .map(|t| t.to_lowercase())
        .collect::<Vec<_>>();
    if !text.iter().any(|t| t.contains("microsoft")) {
        return None;
    }
    Some(if text.iter().any(|t| t.contains("wsl2")) {
        WslVersion::V2
    } else {
        WslVersion::V1
    })
}

/// Whether the running kernel has the `WSLInterop` binfmt entry enabled: `Some(false)` when the
/// entry is missing or disabled (interop off), `None` when binfmt_misc itself cannot be read.
pub fn interop_registered(read: Read) -> Option<bool> {
    let status = read("/proc/sys/fs/binfmt_misc/status");
    match read("/proc/sys/fs/binfmt_misc/WSLInterop") {
        Some(t) => Some(t.lines().next().is_some_and(|l| l.trim() == "enabled")),
        None => status.map(|_| false),
    }
}

/// Variables that carry Windows interop state (`WSL_INTEROP`, `WSL_DISTRO_NAME`, `WSLENV`,
/// `WSL2_GUI_APPS_ENABLED`, ...).
pub fn is_interop_var(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("WSL")
}

/// `key` of `[section]` in `/etc/wsl.conf` text (ini; `#`/`;` comments, case-insensitive names,
/// surrounding quotes removed). `None` when absent.
pub fn conf_value(conf: &str, section: &str, key: &str) -> Option<String> {
    let mut cur = String::new();
    let mut found = None;
    for l in conf.lines().map(str::trim) {
        if l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(name) = l.strip_prefix('[').and_then(|r| r.split(']').next()) {
            cur = name.trim().to_ascii_lowercase();
        } else if let Some((k, v)) = l.split_once('=')
            && cur == section.to_ascii_lowercase()
            && k.trim().eq_ignore_ascii_case(key)
        {
            found = Some(v.trim().trim_matches('"').to_string()); // the last one wins
        }
    }
    found
}

/// A host recognised as WSL: its version and the mount points that are Windows drives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslHost {
    pub version: WslVersion,
    /// Mount points of `drvfs` (and `9p` with `aname=drvfs`) filesystems, from `/proc/mounts`.
    pub windows_mounts: Vec<PathBuf>,
}

impl WslHost {
    /// `None` when the kernel is not WSL.
    pub fn probe(read: Read) -> Option<Self> {
        let version = version(read)?;
        let windows_mounts = read("/proc/mounts")
            .map(|m| drvfs_mounts(&m))
            .unwrap_or_default();
        Some(Self {
            version,
            windows_mounts,
        })
    }

    /// The real host, probed once per process (`None` off Linux).
    pub fn real() -> Option<&'static WslHost> {
        static HOST: OnceLock<Option<WslHost>> = OnceLock::new();
        HOST.get_or_init(|| {
            cfg!(target_os = "linux")
                .then(|| Self::probe(&|p| std::fs::read_to_string(p).ok()))
                .flatten()
        })
        .as_ref()
    }

    /// Under `/mnt` or under a drvfs mount point. Lexical: canonicalise `p` first when it matters.
    pub fn is_windows_path(&self, p: &Path) -> bool {
        p.starts_with(DEFAULT_MOUNT_ROOT) || self.windows_mounts.iter().any(|m| p.starts_with(m))
    }

    /// `path_var` (a `PATH` value) without Windows directories, so `cmd.exe` and friends are not
    /// found. Fails closed: a value that cannot be re-joined becomes empty.
    pub fn scrub_path(&self, path_var: &OsStr) -> OsString {
        let kept = std::env::split_paths(path_var).filter(|d| !self.is_windows_path(d));
        std::env::join_paths(kept).unwrap_or_default()
    }
}

/// Mount points of Windows drives in `/proc/mounts` text (octal escapes such as `\040` decoded).
pub fn drvfs_mounts(proc_mounts: &str) -> Vec<PathBuf> {
    proc_mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_dev, mnt, fstype, opts) = (f.next()?, f.next()?, f.next()?, f.next()?);
            let windows = fstype == "drvfs" || (fstype == "9p" && opts.contains("aname=drvfs"));
            windows.then(|| PathBuf::from(unescape_mount(mnt)))
        })
        .collect()
}

fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], oct) {
            (b'\\', Some(d)) => {
                let v = d.iter().fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
                out.push(u8::try_from(v).unwrap_or(b'?'));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |p| {
            list.iter()
                .find(|(k, _)| *k == p)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn version_from_kernel_strings() {
        let v2 = files(&[(
            "/proc/version",
            "Linux version 5.15.153.1-microsoft-standard-WSL2 (root@x)",
        )]);
        let v1 = files(&[("/proc/sys/kernel/osrelease", "4.4.0-19041-Microsoft")]);
        let no = files(&[("/proc/version", "Linux version 6.1.0-custom-wsl-lookalike")]);
        assert_eq!(version(&v2), Some(WslVersion::V2));
        assert_eq!(version(&v1), Some(WslVersion::V1));
        assert_eq!(version(&no), None);
        assert_eq!(version(&|_| None), None);
    }

    #[test]
    fn interop_registration() {
        let on = files(&[(
            "/proc/sys/fs/binfmt_misc/WSLInterop",
            "enabled\ninterpreter /init\n",
        )]);
        let off = files(&[("/proc/sys/fs/binfmt_misc/WSLInterop", "disabled\n")]);
        let gone = files(&[("/proc/sys/fs/binfmt_misc/status", "enabled\n")]);
        assert_eq!(interop_registered(&on), Some(true));
        assert_eq!(interop_registered(&off), Some(false));
        assert_eq!(interop_registered(&gone), Some(false));
        assert_eq!(interop_registered(&|_| None), None);
    }

    #[test]
    fn mounts_and_paths() {
        let m = "C: /mnt/c drvfs rw,noatime,uid=1000 0 0\n\
                 D:\\ /windir/d\\040x 9p rw,aname=drvfs;uid=1000 0 0\n\
                 tmpfs /run tmpfs rw 0 0\n\
                 /dev/sdc / ext4 rw 0 0\n\
                 drivers /usr/lib/wsl/drivers 9p ro,aname=drivers 0 0\n";
        let mounts = drvfs_mounts(m);
        assert_eq!(
            mounts,
            [PathBuf::from("/mnt/c"), PathBuf::from("/windir/d x")]
        );
        let h = WslHost {
            version: WslVersion::V2,
            windows_mounts: mounts,
        };
        for yes in ["/mnt/c/Windows", "/mnt/wsl", "/windir/d x/proj", "/mnt"] {
            assert!(h.is_windows_path(Path::new(yes)), "{yes}");
        }
        for no in ["/home/u", "/mntx/a", "/usr/lib/wsl/drivers", "/windir"] {
            assert!(!h.is_windows_path(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn scrub_path_drops_only_windows_dirs() {
        let h = WslHost {
            version: WslVersion::V2,
            windows_mounts: vec!["/windir/d".into()],
        };
        let input = std::env::join_paths([
            "/usr/bin",
            "/mnt/c/Windows/system32",
            "/windir/d/tools",
            "/home/u/.cargo/bin",
        ])
        .unwrap();
        let out = h.scrub_path(&input);
        let kept: Vec<_> = std::env::split_paths(&out).collect();
        assert_eq!(
            kept,
            [
                PathBuf::from("/usr/bin"),
                PathBuf::from("/home/u/.cargo/bin")
            ]
        );
    }

    #[test]
    fn wsl_conf_values() {
        let c = "# c\n[interop]\nenabled = false\nappendWindowsPath=false\n[automount]\nroot = \"/windir/\"\noptions=\"metadata,umask=22\"\n[boot]\nenabled=true\n";
        assert_eq!(
            conf_value(c, "interop", "enabled").as_deref(),
            Some("false")
        );
        assert_eq!(
            conf_value(c, "interop", "appendwindowspath").as_deref(),
            Some("false")
        );
        assert_eq!(
            conf_value(c, "automount", "root").as_deref(),
            Some("/windir/")
        );
        assert_eq!(
            conf_value(c, "automount", "options").as_deref(),
            Some("metadata,umask=22")
        );
        assert_eq!(conf_value(c, "automount", "enabled"), None);
        assert_eq!(conf_value("", "interop", "enabled"), None);
    }

    #[test]
    fn interop_var_names() {
        for v in [
            "WSL_INTEROP",
            "WSL_DISTRO_NAME",
            "WSLENV",
            "WSL2_GUI_APPS_ENABLED",
        ] {
            assert!(is_interop_var(v), "{v}");
        }
        assert!(!is_interop_var("PATH") && !is_interop_var("HOME"));
    }
}
