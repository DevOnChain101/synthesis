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
use std::sync::atomic::{AtomicBool, Ordering};

/// Returns a file's text, or `None` when it is missing or unreadable.
pub type Read<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What the probe may look at, all injected (tests need no WSL).
pub struct Probe<'a> {
    /// File text (`/proc/version`, `/proc/mounts`, `/etc/wsl.conf`, ...).
    pub read: Read<'a>,
    /// Whether a path exists (a directory such as `/run/WSL` has no text).
    pub exists: &'a dyn Fn(&str) -> bool,
    /// Whether an environment variable is set.
    pub env_set: &'a dyn Fn(&str) -> bool,
}

/// Where Windows drives are mounted by default. Always treated as Windows, whatever `/proc/mounts` says.
pub const DEFAULT_MOUNT_ROOT: &str = "/mnt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WslVersion {
    V1,
    V2,
    /// WSL is indicated by other signals but the kernel string does not say which (custom kernel).
    Unknown,
}

/// WSL version from the kernel's own words: both versions say "microsoft" (WSL1 `...-Microsoft`,
/// WSL2 `...-microsoft-standard-WSL2`); only WSL2 also says "wsl2". `None` = the kernel strings
/// do not say WSL (other signals may still, see [`WslHost::probe`]).
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
/// `WSL2_GUI_APPS_ENABLED`, ...) or point at WSLg / host-integration sockets (`XDG_RUNTIME_DIR`,
/// `WAYLAND_DISPLAY`, `DISPLAY`, `PULSE_SERVER`): none of them may reach a sandboxed child.
pub fn is_interop_var(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("WSL")
        || [
            "XDG_RUNTIME_DIR",
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "PULSE_SERVER",
        ]
        .contains(&name)
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
            let v = v.trim();
            // a quoted value ends at the closing quote; otherwise `#` starts a comment
            let v = match v.strip_prefix('"') {
                Some(q) => q.split('"').next().unwrap_or(""),
                None => v.split('#').next().unwrap_or("").trim(),
            };
            found = Some(v.to_string()); // the last one wins
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
    /// WSL if ANY positive signal says so: the kernel string, the `WSLInterop` binfmt entry,
    /// `/run/WSL`, `/mnt/wsl`, or `WSL_INTEROP` / `WSL_DISTRO_NAME` in the environment. Only
    /// positive signals count: a missing file never says "not WSL" on its own (a custom kernel
    /// string, an unreadable `/proc`), it just contributes nothing. `None` = no signal at all.
    pub fn probe(p: &Probe) -> Option<Self> {
        let kernel = version(p.read);
        let signal = kernel.is_some()
            || (p.read)("/proc/sys/fs/binfmt_misc/WSLInterop").is_some()
            || (p.exists)("/proc/sys/fs/binfmt_misc/WSLInterop")
            || (p.exists)("/run/WSL")
            || (p.exists)("/mnt/wsl")
            || (p.env_set)("WSL_INTEROP")
            || (p.env_set)("WSL_DISTRO_NAME");
        signal.then(|| Self {
            version: kernel.unwrap_or(WslVersion::Unknown),
            windows_mounts: (p.read)("/proc/mounts")
                .map(|m| drvfs_mounts(&m))
                .unwrap_or_default(),
        })
    }

    /// The host as it is NOW (mounts are re-read on every call, never cached stale). Once WSL
    /// was seen in this process it stays WSL (positive-only), even if a later probe finds no
    /// signal. `None` off Linux or when there has never been a signal.
    pub fn current() -> Option<WslHost> {
        static SEEN: AtomicBool = AtomicBool::new(false);
        if !cfg!(target_os = "linux") {
            return None;
        }
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let exists = |p: &str| Path::new(p).exists();
        let env_set = |k: &str| std::env::var_os(k).is_some();
        let host = Self::probe(&Probe {
            read: &read,
            exists: &exists,
            env_set: &env_set,
        });
        match host {
            Some(h) => {
                SEEN.store(true, Ordering::Relaxed);
                Some(h)
            }
            None if SEEN.load(Ordering::Relaxed) => Some(Self {
                version: WslVersion::Unknown,
                windows_mounts: vec![],
            }),
            None => None,
        }
    }

    /// Under `/mnt` or under a drvfs mount point. Lexical: canonicalise `p` first when it matters.
    pub fn is_windows_path(&self, p: &Path) -> bool {
        p.starts_with(DEFAULT_MOUNT_ROOT) || self.windows_mounts.iter().any(|m| p.starts_with(m))
    }

    /// `path_var` (a `PATH` value) without Windows directories, so `cmd.exe` and friends are not
    /// found. Every entry is canonicalised (a symlink into `/mnt` is caught); relative entries,
    /// entries that cannot be resolved and entries that resolve to a Windows path are dropped.
    /// Fails closed: a value that cannot be re-joined becomes empty.
    pub fn scrub_path(&self, path_var: &OsStr) -> OsString {
        self.scrub_path_with(path_var, &|d| std::fs::canonicalize(d).ok())
    }

    /// [`Self::scrub_path`] with an injected canonicaliser (`None` = cannot be resolved).
    pub fn scrub_path_with(
        &self,
        path_var: &OsStr,
        canon: &dyn Fn(&Path) -> Option<PathBuf>,
    ) -> OsString {
        let kept = std::env::split_paths(path_var)
            .filter(|d| d.has_root())
            .filter(|d| canon(d).is_some_and(|c| !self.is_windows_path(&c)));
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
    fn scrub_path_drops_windows_relative_and_unresolvable_dirs() {
        let h = WslHost {
            version: WslVersion::V2,
            windows_mounts: vec!["/windir/d".into()],
        };
        let input = std::env::join_paths([
            "/usr/bin",
            "/mnt/c/Windows/system32",
            "/windir/d/tools",
            "rel/bin",
            "/gone/bin",
            "/link-to-c/bin",
            "/home/u/.cargo/bin",
        ])
        .unwrap();
        // fake canonicaliser: a symlink into /mnt resolves there, /gone does not exist
        let canon = |d: &Path| match d.to_str().unwrap() {
            "/gone/bin" => None,
            "/link-to-c/bin" => Some(PathBuf::from("/mnt/c/Tools")),
            _ => Some(d.to_path_buf()),
        };
        let out = h.scrub_path_with(&input, &canon);
        let kept: Vec<_> = std::env::split_paths(&out).collect();
        assert_eq!(
            kept,
            [
                PathBuf::from("/usr/bin"),
                PathBuf::from("/home/u/.cargo/bin")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn scrub_path_resolves_real_symlinks() {
        let t = std::env::temp_dir().join(format!("synt-wsl-scrub-{}", std::process::id()));
        std::fs::create_dir_all(t.join("real")).unwrap();
        std::os::unix::fs::symlink(t.join("real"), t.join("link")).unwrap();
        let real = std::fs::canonicalize(t.join("real")).unwrap();
        // the temp dir plays the part of a Windows drive
        let h = WslHost {
            version: WslVersion::V2,
            windows_mounts: vec![real],
        };
        let input = std::env::join_paths([t.join("link"), "/usr/bin".into()]).unwrap();
        let out = h.scrub_path(&input);
        std::fs::remove_dir_all(&t).unwrap();
        assert_eq!(
            std::env::split_paths(&out).collect::<Vec<_>>(),
            [PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn any_positive_signal_means_wsl_and_absence_never_does() {
        type Case = (
            &'static str,
            &'static [&'static str],
            &'static [&'static str],
            &'static [&'static str],
        );
        // (name, files, existing paths, env names)
        let cases: [Case; 8] = [
            ("kernel", &["/proc/version"], &[], &[]),
            (
                "binfmt text",
                &["/proc/sys/fs/binfmt_misc/WSLInterop"],
                &[],
                &[],
            ),
            (
                "binfmt exists",
                &[],
                &["/proc/sys/fs/binfmt_misc/WSLInterop"],
                &[],
            ),
            ("run/WSL", &[], &["/run/WSL"], &[]),
            ("mnt/wsl", &[], &["/mnt/wsl"], &[]),
            ("interop env", &[], &[], &["WSL_INTEROP"]),
            ("distro env", &[], &[], &["WSL_DISTRO_NAME"]),
            // custom kernel without the word, but the env says WSL
            (
                "custom kernel",
                &["/proc/version:custom"],
                &[],
                &["WSL_DISTRO_NAME"],
            ),
        ];
        for (name, files, exist, envs) in cases {
            let read = |p: &str| {
                files.iter().find_map(|f| match f.split_once(':') {
                    Some((k, v)) if k == p => Some(v.to_string()),
                    None if *f == p && p == "/proc/version" => {
                        Some("Linux 5.15.1-microsoft-standard-WSL2".to_string())
                    }
                    None if *f == p => Some("enabled".to_string()),
                    _ => None,
                })
            };
            let exists = |p: &str| exist.contains(&p);
            let env_set = |k: &str| envs.contains(&k);
            let h = WslHost::probe(&Probe {
                read: &read,
                exists: &exists,
                env_set: &env_set,
            });
            assert!(h.is_some(), "{name}");
        }
        let none = WslHost::probe(&Probe {
            read: &|p| (p == "/proc/version").then(|| "Linux 6.1.0-custom".to_string()),
            exists: &|_| false,
            env_set: &|_| false,
        });
        assert_eq!(none, None);
        // other signals without the kernel word: version unknown, not guessed
        let h = WslHost::probe(&Probe {
            read: &|p| (p == "/proc/mounts").then(|| "C: /mnt/c drvfs rw 0 0\n".to_string()),
            exists: &|p| p == "/run/WSL",
            env_set: &|_| false,
        })
        .unwrap();
        assert_eq!(h.version, WslVersion::Unknown);
        assert_eq!(h.windows_mounts, [PathBuf::from("/mnt/c")]);
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
        let c = "[interop]\nenabled=false # off\nappendWindowsPath = \"false\" # no\n";
        assert_eq!(
            conf_value(c, "interop", "enabled").as_deref(),
            Some("false")
        );
        assert_eq!(
            conf_value(c, "interop", "appendWindowsPath").as_deref(),
            Some("false")
        );
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
