//! Linux: Landlock (via the `synthesis-sandbox-exec` re-exec helper) and bubblewrap.
//!
//! Landlock restrictions are irrevocable and apply to the *calling* process, so they must be
//! applied in the child between fork and exec. `std`'s `pre_exec` is `unsafe` (forbidden in
//! this workspace), so [`LinuxLandlockBackend::wrap`] runs the safe helper binary
//! (`src/bin/synthesis-sandbox-exec.rs`) which calls [`restrict_current_process`] and then
//! `exec`s the target. The helper must sit in the same directory as the current executable
//! (no parent dirs, not group/world-writable) or be injected with [`LinuxLandlockBackend::new`];
//! the policy travels as strict versioned JSON in argv.
//!
//! Landlock limits (documented, not hidden):
//! - Allow-list only. It cannot deny a path inside an allowed root, so a `deny_path` that
//!   overlaps any readable/writable root is an ERROR (`Unsupported`), never silently ignored;
//!   only deny paths outside every root (which are already unreachable) are accepted.
//! - `wrap` adds read access to system dirs (`/usr`, `/bin`, `/lib*`, `/etc`, `/proc`, ...) and
//!   write access to `/dev` and the policy's private `tmp_dir` so a shell can run; a deny path
//!   under those is an error. The real `/tmp` and `/run` are NOT granted (`/run/systemd/resolve`
//!   read-only only when the network is allowed, for DNS).
//! - Network: ABI >= 4 can block TCP bind/connect only. UDP and AF_UNIX are not covered (GAP:
//!   a process can still connect to any pathname unix socket it can reach by DAC, e.g.
//!   `/run/docker.sock`, because Landlock only gained unix-socket scoping in later ABIs). The
//!   network guarantee is therefore at most `Weak`, never `Strong`; a policy that requires
//!   `Strong` network needs bubblewrap (`--unshare-net` removes every socket family). Below
//!   ABI 4 `network: Deny` is refused.
//! - `/proc` stays readable (other processes' `environ`/`cmdline` of the same uid): Landlock
//!   has no pid namespace. bubblewrap mounts a private `/proc` of its own pid namespace.
//! - `readonly_paths` inside a writable root are rejected like `deny_paths` (allow-list only).
//! - `allow_exec = false` is unsupported (the helper itself must exec the target afterwards).
//! - Filesystem is `Strong` from ABI 3 (truncate handled), `Weak` below.
//!
//! `detect` prefers bubblewrap when it is usable (it honours `deny_paths`/`readonly_paths`
//! inside the workspace and removes AF_UNIX with the net namespace), then Landlock, else no
//! sandbox (the terminal runtime refuses that unless explicitly allowed).
//!
//! bubblewrap needs unprivileged user namespaces; Ubuntu 24.04 blocks them via AppArmor
//! (`kernel.apparmor_restrict_unprivileged_userns=1`), so `detect` probes by running it.
//! `bwrap` is located only at the absolute [`BWRAP_CANDIDATES`] (never via `PATH`) and must not
//! be group/world-writable. bwrap exposes the whole filesystem read-only (readable_roots is not
//! a read restriction); deny_paths are hidden, `/run`, `/var/run` and `/tmp` are fresh tmpfs
//! (so no docker/ssh-agent/tmux sockets), `readonly_paths` are read-only bind mounts (a bind
//! mount point cannot be renamed or removed: EBUSY), and `tmp_dir` is bind-mounted writable.
//! **Weak is defence in depth, not a security boundary.** None of this has been run on a Linux
//! kernel yet.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlockRule {
    pub path: PathBuf,
    pub write: bool,
}

/// Landlock cannot protect a path that a command may create later either, so the policy is still
/// refused; say so instead of implying the path exists.
fn not_yet(p: &Path) -> &'static str {
    if p.exists() {
        ""
    } else {
        " (not present now; a command could create it)"
    }
}

/// Pure policy -> Landlock rule translation (portable, unit-testable anywhere).
pub fn landlock_rules(policy: &SandboxPolicy) -> Result<Vec<LandlockRule>, SandboxError> {
    validate(policy)?;
    let roots = policy.readable_roots.iter().chain(&policy.writable_roots);
    for d in &policy.deny_paths {
        if roots.clone().any(|r| d.starts_with(r) || r.starts_with(d)) {
            return Err(SandboxError::Unsupported(format!(
                "Landlock cannot deny {d:?}: it overlaps an allowed root{}",
                not_yet(d)
            )));
        }
    }
    // Read-only paths only conflict with writable roots (reading them is fine).
    let writable = || policy.writable_roots.iter().chain(&policy.tmp_dir);
    for d in &policy.readonly_paths {
        if writable().any(|r| d.starts_with(r) || r.starts_with(d)) {
            return Err(SandboxError::Unsupported(format!(
                "Landlock cannot make {d:?} read-only: it overlaps a writable root{}",
                not_yet(d)
            )));
        }
    }
    let rule = |p: &PathBuf, write| LandlockRule {
        path: p.clone(),
        write,
    };
    Ok(policy
        .readable_roots
        .iter()
        .map(|p| rule(p, false))
        .chain(policy.writable_roots.iter().map(|p| rule(p, true)))
        .collect())
}

#[cfg(target_os = "linux")]
mod ll {
    use super::*;
    use landlock::*;

    /// Highest Landlock ABI the running kernel accepts, or None if unsupported/disabled.
    pub fn landlock_abi() -> Option<u8> {
        [
            (ABI::V5, 5),
            (ABI::V4, 4),
            (ABI::V3, 3),
            (ABI::V2, 2),
            (ABI::V1, 1),
        ]
        .into_iter()
        .find(|(abi, _)| {
            Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(*abi))
                .and_then(|r| r.create())
                .is_ok()
        })
        .map(|(_, n)| n)
    }

    /// Irrevocably confine the current process. Call only in a helper about to exec.
    /// `allow_exec=false` also forbids exec, so the helper must exec before restricting
    /// only if it wants the target to run; otherwise nothing can run.
    pub fn restrict_current_process(policy: &SandboxPolicy) -> Result<RulesetStatus, SandboxError> {
        let rules = landlock_rules(policy)?;
        let un = |e: &dyn std::fmt::Display| SandboxError::Unavailable(format!("landlock: {e}"));
        let abi = ABI::V5;
        let mut rs = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| un(&e))?;
        if policy.network == NetworkPolicy::Deny {
            // Hard requirement: if the kernel can't block TCP we fail rather than pretend.
            rs = rs
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
                .map_err(|e| un(&e))?;
        }
        let mut rs = rs.create().map_err(|e| un(&e))?;
        for r in rules {
            let mut acc = if r.write {
                AccessFs::from_all(abi)
            } else {
                AccessFs::from_read(abi)
            };
            if !policy.allow_exec {
                acc.remove(AccessFs::Execute);
            }
            let fd = PathFd::new(&r.path).map_err(|e| un(&e))?;
            rs = rs.add_rule(PathBeneath::new(fd, acc)).map_err(|e| un(&e))?;
        }
        let status = rs.restrict_self().map_err(|e| un(&e))?;
        if status.ruleset == RulesetStatus::NotEnforced {
            return Err(SandboxError::Unavailable(
                "landlock not enforced by this kernel".into(),
            ));
        }
        Ok(status.ruleset)
    }
}
#[cfg(target_os = "linux")]
pub use ll::{landlock_abi, restrict_current_process};

const SYSTEM_READ: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/proc", "/sys", "/opt",
];
const SYSTEM_WRITE: &[&str] = &["/dev"];
/// Read-only DNS config behind `/etc/resolv.conf` on systemd-resolved hosts.
const DNS_READ: &str = "/run/systemd/resolve";

/// Fixed absolute locations for `bwrap` (never `PATH`, which a hostile env could point anywhere).
const BWRAP_CANDIDATES: &[&str] = &[
    "/usr/bin/bwrap",
    "/bin/bwrap",
    "/usr/local/bin/bwrap",
    "/run/current-system/sw/bin/bwrap",
];
/// `true` for the probe, also by absolute path (bwrap would otherwise search `PATH`).
const TRUE_CANDIDATES: &[&str] = &["/usr/bin/true", "/bin/true"];

/// A regular file that group/other cannot write (so only its owner/root can swap it).
fn trusted_file(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    let safe = std::os::unix::fs::MetadataExt::mode(&m) & 0o022 == 0;
    #[cfg(not(unix))]
    let safe = true;
    m.is_file() && safe
}

/// The helper binary: only in the same directory as `exe`, and trusted.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn helper_next_to(exe: &Path) -> Option<PathBuf> {
    let h = exe.parent()?.join(crate::HELPER_NAME);
    trusted_file(&h).then_some(h)
}

/// Landlock through the `synthesis-sandbox-exec` helper. `abi` is the kernel's highest Landlock ABI.
pub struct LinuxLandlockBackend {
    helper: PathBuf,
    abi: u8,
}

impl LinuxLandlockBackend {
    pub fn new(helper: PathBuf, abi: u8) -> Self {
        Self { helper, abi }
    }

    /// Some only on Linux with Landlock enabled and a trusted helper in the same directory as
    /// the current executable (tests and embedders inject one with [`Self::new`] instead).
    #[cfg(target_os = "linux")]
    pub fn detect() -> Option<Self> {
        let abi = landlock_abi()?;
        let helper = helper_next_to(&std::env::current_exe().ok()?)?;
        Some(Self { helper, abi })
    }
}

/// Usable Linux backends, best first: bubblewrap (only if its probe passed), then Landlock.
#[cfg(target_os = "linux")]
pub(crate) fn detect_linux() -> Vec<Box<dyn SandboxBackend>> {
    let mut v: Vec<Box<dyn SandboxBackend>> = vec![];
    // Test hook: simulate a host where only Landlock works (e.g. Ubuntu 24.04 with userns blocked).
    #[cfg(all(feature = "test-backend", debug_assertions))]
    let skip_bwrap =
        std::env::var_os("SYNT_TEST_FORCE_BACKEND").is_some_and(|v| v == "landlock-only");
    #[cfg(not(all(feature = "test-backend", debug_assertions)))]
    let skip_bwrap = false;
    if !skip_bwrap {
        v.extend(BwrapBackend::detect().map(|b| Box::new(b) as Box<dyn SandboxBackend>));
    }
    v.extend(LinuxLandlockBackend::detect().map(|b| Box::new(b) as Box<dyn SandboxBackend>));
    v
}

/// Why a Linux backend that exists on this host is not selected (empty when none is skipped).
pub fn skipped_backends() -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Err(e) = bwrap_probe() {
        return vec![e.clone()];
    }
    vec![]
}

static BWRAP_PROBE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();

/// Cached per process: a trusted `bwrap` that really runs `true` with the argument set used for
/// commands, or the precise failure with a suggested remedy.
pub fn bwrap_probe() -> &'static Result<PathBuf, String> {
    BWRAP_PROBE.get_or_init(probe_bwrap)
}

fn probe_bwrap() -> Result<PathBuf, String> {
    let tru = TRUE_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or("bubblewrap not usable: no /usr/bin/true or /bin/true for the probe")?;
    let found: Vec<_> = BWRAP_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .filter(|p| trusted_file(p))
        .collect();
    // /bin -> /usr/bin on merged-usr systems: probe (and report) each real binary once.
    let mut seen = vec![];
    let found: Vec<_> = found
        .into_iter()
        .filter(|p| {
            let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            !seen.contains(&real) && {
                seen.push(real);
                true
            }
        })
        .collect();
    if found.is_empty() {
        return Err("bubblewrap not found at a trusted absolute path (/usr/bin/bwrap, ...); install it (e.g. `apt install bubblewrap`)".into());
    }
    let cmd = SandboxedCommand {
        program: tru.into(),
        ..Default::default()
    };
    // Exactly the arguments commands get (user/pid/ipc/net namespaces, /proc, /dev, tmpfs).
    let args = BwrapBackend::args(&SandboxPolicy::default(), &cmd);
    let mut errs = vec![];
    for path in found {
        match std::process::Command::new(&path)
            .env_clear()
            .args(&args)
            .output()
        {
            Ok(o) if o.status.success() => return Ok(path),
            Ok(o) => errs.push(probe_failure(&path, &String::from_utf8_lossy(&o.stderr))),
            Err(e) => errs.push(format!(
                "bubblewrap not usable: cannot run {}: {e}",
                path.display()
            )),
        }
    }
    Err(errs.join("; "))
}

/// Pure: the probe's stderr -> reason plus remedy.
pub(crate) fn probe_failure(path: &Path, stderr: &str) -> String {
    // First line, without Debian's trailing "See <url> or <url>" pointer.
    let e = stderr
        .lines()
        .next()
        .unwrap_or("")
        .split(" See <")
        .next()
        .unwrap_or("")
        .trim();
    let remedy = if e.contains("mount proc") {
        "/proc is masked, as inside Docker: start the container with `--security-opt systempaths=unconfined` (and a seccomp profile or `--cap-add SYS_ADMIN` that allows mount), or use --sandbox best-effort|off"
    } else if e.contains("namespace")
        || e.contains("uid map")
        || e.contains("Operation not permitted")
    {
        "unprivileged user namespaces are blocked: Ubuntu 24.04: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` or an AppArmor profile for bwrap; Debian: `sudo sysctl -w kernel.unprivileged_userns_clone=1`; Docker: the default seccomp profile blocks it; or use --sandbox best-effort|off"
    } else {
        "run the bwrap command by hand to see why, or use --sandbox best-effort|off"
    };
    format!(
        "bubblewrap not usable: {} failed its probe: {}; remedy: {remedy}",
        path.display(),
        if e.is_empty() { "(no output)" } else { e }
    )
}

impl SandboxBackend for LinuxLandlockBackend {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        let g = |ok: bool, g| if ok { g } else { Guarantee::None };
        SandboxCapabilities {
            filesystem: if self.abi >= 3 { Guarantee::Strong } else { Guarantee::Weak },
            network: g(self.abi >= 4, Guarantee::Weak),
            process_tree: Guarantee::Strong,
            // abstract Unix sockets (the host X server at @/tmp/.X11-unix/X0) are not scoped
            gui: Guarantee::None,
            notes: vec![
                "gui: None. Landlock does not scope abstract Unix sockets, so the app could reach the host display server".into(),
                format!("landlock ABI {}; kernel-enforced and inherited by all descendants", self.abi),
                "allow-list only: deny_paths overlapping an allowed root are rejected".into(),
                "network Deny blocks TCP bind/connect only (ABI >= 4); UDP and AF_UNIX stay open (Strong network needs bubblewrap)".into(),
                "network Loopback is unsupported (TCP ports only, no addresses); allow_exec=false and AllowHosts are rejected; tree kill is the terminal runtime's job".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        if !policy.allow_exec {
            return Err(SandboxError::Unsupported(
                "landlock helper cannot forbid exec".into(),
            ));
        }
        if policy.network == NetworkPolicy::Loopback {
            return Err(SandboxError::Unsupported(
                "landlock filters TCP ports, not addresses: network Loopback needs bubblewrap or Seatbelt".into(),
            ));
        }
        if policy.network == NetworkPolicy::Deny && self.abi < 4 {
            return Err(SandboxError::Unsupported(format!(
                "landlock ABI {} cannot block network (needs 4)",
                self.abi
            )));
        }
        let mut p = policy.clone();
        let existing = |v: &[&str]| {
            v.iter()
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .collect::<Vec<_>>()
        };
        p.readable_roots.extend(existing(SYSTEM_READ));
        if p.network == NetworkPolicy::Allow {
            p.readable_roots.extend(existing(&[DNS_READ]));
        }
        p.writable_roots.extend(existing(SYSTEM_WRITE));
        p.writable_roots.extend(p.tmp_dir.clone());
        landlock_rules(&p)?; // overlap errors surface here, not inside the child
        let mut args: Vec<OsString> = vec![
            crate::encode_policy(&p)?.into(),
            "--".into(),
            c.program.clone(),
        ];
        args.extend(c.args.iter().cloned());
        Ok(SandboxedCommand {
            program: self.helper.clone().into(),
            args,
            env: build_env(policy, c),
            cwd: c.cwd.clone(),
            clear_env: true,
        })
    }
}

pub struct BwrapBackend {
    path: PathBuf,
}

impl BwrapBackend {
    /// Some only if a trusted `bwrap` passed the cached [`bwrap_probe`].
    pub fn detect() -> Option<Self> {
        bwrap_probe()
            .as_ref()
            .ok()
            .map(|path| Self { path: path.clone() })
    }

    pub(crate) fn args(policy: &SandboxPolicy, cmd: &SandboxedCommand) -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = [
            "--die-with-parent",
            "--unshare-pid",
            "--unshare-ipc",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run", // hides docker.sock, ssh-agent, gpg-agent, systemd and dbus sockets
        ]
        .map(Into::into)
        .into();
        let mut push = |parts: &[&Path], flag: &str| {
            a.push(flag.into());
            a.extend(parts.iter().map(|p| p.as_os_str().to_owned()));
        };
        // /var/run is normally a symlink to /run (already hidden); mask it only when it is real.
        if std::fs::symlink_metadata("/var/run").is_ok_and(|m| m.is_dir()) {
            push(&[Path::new("/var/run")], "--tmpfs");
        }
        for w in policy.writable_roots.iter().chain(&policy.tmp_dir) {
            push(&[w, w], "--bind");
        }
        let hidden = |p: &Path| p.starts_with("/run") || p.starts_with("/var/run");
        for r in policy
            .readonly_paths
            .iter()
            .filter(|p| p.exists() && !hidden(p))
        {
            push(&[r, r], "--ro-bind");
        }
        for d in policy.deny_paths.iter().filter(|p| !hidden(p)) {
            if d.is_dir() {
                push(&[d], "--tmpfs");
            } else if d.exists() {
                push(&[Path::new("/dev/null"), d], "--ro-bind");
            }
        }
        // Loopback: same namespace; bwrap itself brings `lo` up in it (private to the sandbox).
        if matches!(
            policy.network,
            NetworkPolicy::Deny | NetworkPolicy::Loopback
        ) {
            a.push("--unshare-net".into());
        }
        if let Some(cwd) = &cmd.cwd {
            a.extend(["--chdir".into(), cwd.as_os_str().to_owned()]);
        }
        a.push("--".into());
        a.push(cmd.program.clone());
        a.extend(cmd.args.iter().cloned());
        a
    }
}

impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    fn network_guarantee(&self, mode: &NetworkPolicy) -> Guarantee {
        network_guarantee_for(mode, self.capabilities().network, Guarantee::Strong)
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::Weak,
            network: Guarantee::Strong,
            process_tree: Guarantee::Strong,
            gui: Guarantee::Weak,
            notes: vec![
                "gui: Weak. A private Xvfb + dbus behind bwrap --unshare-net would make it Strong; not implemented".into(),
                "writes confined to writable_roots, tmp_dir and a private /tmp; /run is a fresh tmpfs; reads are NOT confined except deny_paths (whole fs mounted read-only)".into(),
                "Weak: defence in depth, not a security boundary".into(),
                "network Deny/Loopback use a fresh net namespace; with Loopback its `lo` is up but PRIVATE to the sandbox: a dev server and tests inside it work, host loopback services are unreachable; AllowHosts and allow_exec=false are rejected".into(),
                "requires unprivileged user namespaces (blocked by default on Ubuntu 24.04 AppArmor)".into(),
                "nonexistent deny_paths/readonly_paths are not protected; mount points make existing ones rename-proof".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        if !policy.allow_exec {
            return Err(SandboxError::Unsupported(
                "bubblewrap cannot forbid exec".into(),
            ));
        }
        Ok(SandboxedCommand {
            program: self.path.clone().into(),
            args: Self::args(policy, c),
            env: build_env(policy, c),
            cwd: None,
            clear_env: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // POSIX-looking test paths get a drive letter on Windows so they are absolute there too.
    fn p(s: &str) -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            format!("C:{s}")
        } else {
            s.into()
        })
    }

    #[test]
    fn landlock_translation() {
        let pol = SandboxPolicy {
            readable_roots: vec![p("/usr")],
            writable_roots: vec![p("/work")],
            deny_paths: vec![p("/home/u/.ssh")],
            ..Default::default()
        };
        let r = landlock_rules(&pol).unwrap();
        assert_eq!(
            r,
            vec![
                LandlockRule {
                    path: p("/usr"),
                    write: false
                },
                LandlockRule {
                    path: p("/work"),
                    write: true
                },
            ]
        );
    }

    #[test]
    fn probe_failure_names_reason_and_remedy() {
        let b = Path::new("/usr/bin/bwrap");
        let docker = probe_failure(
            b,
            "bwrap: Can't mount proc on /newroot/proc: Operation not permitted\n",
        );
        assert!(docker.contains("Can't mount proc") && docker.contains("systempaths=unconfined"));
        let ns = probe_failure(b, "bwrap: No permissions to create new namespace");
        assert!(
            ns.contains("apparmor_restrict_unprivileged_userns=0"),
            "{ns}"
        );
        assert!(probe_failure(b, "").contains("(no output)"));
    }

    #[test]
    fn probe_runs_the_command_argument_set() {
        let c = SandboxedCommand {
            program: "/bin/true".into(),
            ..Default::default()
        };
        let j = BwrapBackend::args(&SandboxPolicy::default(), &c)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        for f in [
            "--unshare-pid",
            "--unshare-net",
            "--proc /proc",
            "--dev /dev",
            "--tmpfs /tmp",
            "--ro-bind / /",
        ] {
            assert!(j.contains(f), "{f} in {j}");
        }
    }

    #[test]
    fn landlock_message_only_claims_existing_paths() {
        let pol = SandboxPolicy {
            writable_roots: vec![p("/synthesis-nonexistent-ws")],
            readonly_paths: vec![p("/synthesis-nonexistent-ws/.git/hooks")],
            ..Default::default()
        };
        let e = landlock_rules(&pol).unwrap_err().to_string();
        assert!(e.contains("not present now"), "{e}");
    }

    #[test]
    fn landlock_rejects_deny_inside_root() {
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            deny_paths: vec![p("/work/.git/hooks")],
            ..Default::default()
        };
        assert!(matches!(
            landlock_rules(&pol),
            Err(SandboxError::Unsupported(_))
        ));
    }

    #[test]
    fn landlock_wrap_shape_and_failures() {
        let b = LinuxLandlockBackend::new(p("/h/synthesis-sandbox-exec"), 4);
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            ..Default::default()
        };
        let c = SandboxedCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: Some(p("/work")),
            ..Default::default()
        };
        let w = b.wrap(&pol, &c).unwrap();
        assert_eq!(w.program, p("/h/synthesis-sandbox-exec").into_os_string());
        assert!(w.clear_env && w.cwd == c.cwd);
        assert_eq!(&w.args[1..], ["--", "sh", "-c", "true"]);
        let back = crate::decode_policy(w.args[0].to_str().unwrap()).unwrap();
        assert!(back.writable_roots.contains(&p("/work")) && back.network == NetworkPolicy::Deny);
        let old = LinuxLandlockBackend::new(p("/h"), 3);
        assert!(matches!(
            old.wrap(&pol, &c),
            Err(SandboxError::Unsupported(_))
        ));
        assert_eq!(old.capabilities().network, Guarantee::None);
        let no_exec = SandboxPolicy {
            allow_exec: false,
            ..pol.clone()
        };
        assert!(b.wrap(&no_exec, &c).is_err());
        let deny = SandboxPolicy {
            deny_paths: vec![p("/work/.git")],
            ..pol
        };
        assert!(matches!(
            b.wrap(&deny, &c),
            Err(SandboxError::Unsupported(_))
        ));
    }

    #[test]
    fn landlock_wrap_private_tmp_and_no_real_tmp_or_run() {
        let b = LinuxLandlockBackend::new(p("/h/x"), 4);
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            tmp_dir: Some(p("/work-tmp")),
            ..Default::default()
        };
        let w = b.wrap(&pol, &SandboxedCommand::default()).unwrap();
        let back = crate::decode_policy(w.args[0].to_str().unwrap()).unwrap();
        assert!(back.writable_roots.contains(&p("/work-tmp")));
        for bad in ["/tmp", "/run", "/var/run"] {
            assert!(
                !back
                    .writable_roots
                    .iter()
                    .chain(&back.readable_roots)
                    .any(|r| *r == p(bad)),
                "{bad} must not be granted"
            );
        }
        assert!(
            w.env
                .contains(&("TMPDIR".into(), p("/work-tmp").into_os_string()))
        );
    }

    #[test]
    fn landlock_rejects_readonly_inside_writable_root() {
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            readonly_paths: vec![p("/work/.git/hooks")],
            ..Default::default()
        };
        assert!(matches!(
            landlock_rules(&pol),
            Err(SandboxError::Unsupported(_))
        ));
        let outside = SandboxPolicy {
            readonly_paths: vec![p("/elsewhere")],
            ..pol
        };
        assert!(landlock_rules(&outside).is_ok());
    }

    #[cfg(unix)]
    fn file_with_mode(dir: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let f = dir.join(name);
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
        f
    }

    #[cfg(unix)]
    #[test]
    fn helper_only_next_to_exe_and_not_writable_by_others() {
        let t = tempfile::tempdir().unwrap();
        let (inner, exe) = (t.path().join("deps"), t.path().join("deps/test-bin"));
        std::fs::create_dir(&inner).unwrap();
        // A helper one directory up (where cargo puts bins next to test binaries) is NOT used.
        file_with_mode(t.path(), crate::HELPER_NAME, 0o755);
        assert_eq!(helper_next_to(&exe), None);
        for bad in [0o775, 0o757, 0o777] {
            let f = file_with_mode(&inner, crate::HELPER_NAME, bad);
            assert_eq!(helper_next_to(&exe), None, "{bad:o}");
            std::fs::remove_file(f).unwrap();
        }
        let f = file_with_mode(&inner, crate::HELPER_NAME, 0o755);
        assert_eq!(helper_next_to(&exe), Some(f));
    }

    #[cfg(unix)] // the candidates are POSIX paths: not absolute on a Windows host
    #[test]
    fn external_binaries_are_absolute_candidates_only() {
        assert!(
            BWRAP_CANDIDATES
                .iter()
                .chain(TRUE_CANDIDATES)
                .all(|c| Path::new(c).is_absolute())
        );
    }

    #[cfg(unix)] // renders POSIX mount arguments (`--bind /work /work`)
    #[test]
    fn bwrap_hides_run_and_binds_private_tmp_and_readonly() {
        let t = tempfile::tempdir().unwrap();
        let (ro, tmp) = (t.path().join("config"), t.path().join("tmp"));
        std::fs::write(&ro, "").unwrap();
        std::fs::create_dir(&tmp).unwrap();
        let pol = SandboxPolicy {
            writable_roots: vec![t.path().to_path_buf()],
            readonly_paths: vec![ro.clone()],
            deny_paths: vec![p("/run/docker.sock")],
            tmp_dir: Some(tmp.clone()),
            ..Default::default()
        };
        let j = BwrapBackend::args(&pol, &SandboxedCommand::default())
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let at = |needle: &str| j.find(needle).unwrap_or_else(|| panic!("{needle} in {j}"));
        assert!(at("--tmpfs /run") < at("--bind"), "{j}");
        assert!(
            at("--bind") < at(&format!("--ro-bind {0} {0}", ro.display())),
            "{j}"
        );
        at(&format!("--bind {0} {0}", tmp.display()));
        assert!(j.contains("--tmpfs /tmp") && j.contains("--unshare-net"));
        assert!(
            !j.contains("docker.sock"),
            "already hidden by the /run tmpfs: {j}"
        );
    }

    #[cfg(unix)] // renders POSIX mount arguments (`--bind /work /work`)
    #[test]
    fn bwrap_worktree_git_pin_is_readonly() {
        let t = tempfile::tempdir().unwrap();
        let (wt, main_git) = (t.path().join("wt"), t.path().join("main/.git"));
        let gd = main_git.join("worktrees/x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&gd).unwrap();
        let pointer = wt.join(".git");
        std::fs::write(&pointer, format!("gitdir: {}\n", gd.display())).unwrap();
        let mut pol = SandboxPolicy {
            writable_roots: vec![wt.clone()],
            readable_roots: vec![main_git.clone()],
            ..Default::default()
        };
        pol.protect([pointer.clone()]);
        pol.ensure_unwritable(&gd).unwrap();
        let a: Vec<String> = BwrapBackend::args(&pol, &SandboxedCommand::default())
            .iter()
            .map(|s| s.to_string_lossy().into())
            .collect();
        let j = a.join(" ");
        let at = |n: &str| j.find(n).unwrap_or_else(|| panic!("{n} in {j}"));
        // pointer file: ro-bind over the writable bind (a file bind also blocks rename/unlink)
        assert!(
            at(&format!("--bind {0} {0}", wt.display()))
                < at(&format!("--ro-bind {0} {0}", pointer.display())),
            "{j}"
        );
        // git dir is never writable-bound (readable via the read-only root)
        assert!(!j.contains(&format!("--bind {0} {0}", gd.display())), "{j}");
        assert!(
            !j.contains(&format!("--bind {0} {0}", main_git.display())),
            "{j}"
        );
        // Landlock cannot express a read-only file inside a writable root
        assert!(matches!(
            landlock_rules(&pol),
            Err(SandboxError::Unsupported(_))
        ));
    }

    #[cfg(unix)] // renders POSIX mount arguments (`--bind /work /work`)
    #[test]
    fn bwrap_args_shape() {
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            ..Default::default()
        };
        let c = SandboxedCommand {
            program: "ls".into(),
            cwd: Some(p("/work")),
            ..Default::default()
        };
        let a: Vec<String> = BwrapBackend::args(&pol, &c)
            .iter()
            .map(|s| s.to_string_lossy().into())
            .collect();
        let j = a.join(" ");
        assert!(
            j.contains("--bind /work /work") && j.contains("--unshare-net") && j.ends_with("-- ls")
        );
    }

    #[test]
    fn loopback_bwrap_unshares_net_landlock_refuses() {
        let mut pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            network: NetworkPolicy::Loopback,
            ..Default::default()
        };
        let c = SandboxedCommand {
            program: "ls".into(),
            ..Default::default()
        };
        let has_net = |pol: &SandboxPolicy| {
            BwrapBackend::args(pol, &c)
                .iter()
                .any(|a| a == "--unshare-net")
        };
        assert!(has_net(&pol));
        pol.network = NetworkPolicy::Allow;
        assert!(!has_net(&pol));
        pol.network = NetworkPolicy::Loopback;
        let b = BwrapBackend {
            path: p("/usr/bin/bwrap"),
        };
        assert_eq!(
            b.network_guarantee(&NetworkPolicy::Loopback),
            Guarantee::Strong
        );
        for abi in [3, 5] {
            let l = LinuxLandlockBackend::new(p("/h"), abi);
            assert!(matches!(
                l.wrap(&pol, &c),
                Err(SandboxError::Unsupported(_))
            ));
            assert_eq!(
                l.network_guarantee(&NetworkPolicy::Loopback),
                Guarantee::None
            );
        }
        let l = LinuxLandlockBackend::new(p("/h"), 5);
        assert_eq!(l.network_guarantee(&NetworkPolicy::Deny), Guarantee::Weak);
        assert!(matches!(
            crate::encode_policy(&pol),
            Err(SandboxError::Unsupported(_))
        ));
    }
}
