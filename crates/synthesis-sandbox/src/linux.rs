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
/// WSL2 reaches the Windows host over AF_VSOCK (hv_sock), which a network namespace does not
/// cover. Install seccomp filters on the current process (inherited by every descendant, also
/// through bwrap), all fail closed (an architecture without a filter here, or a kernel that
/// refuses one, is an error):
/// 1. `socket(AF_VSOCK, ..)` fails with EPERM (a distinctive errno: an unfiltered kernel says
///    EAFNOSUPPORT or succeeds, so a test can tell the filter from a missing vsock module);
/// 2. `io_uring_setup/enter/register` fail with ENOSYS (IORING_OP_SOCKET, kernel >= 5.19, would
///    otherwise create a vsock socket without a `socket` syscall; programs fall back as on a
///    kernel without io_uring);
/// 3. x86_64 only: every syscall number with bit 30 set (the x32 ABI, `nr >= 0x40000000`) fails
///    with ENOSYS, so the number checks above cannot be dodged through x32.
///
/// Only the native architecture is allowed: a 32-bit compat process is killed by the filters'
/// architecture check. AF_NETLINK is NOT blocked: it cannot reach Windows and tools (`ip`, glibc
/// `getifaddrs`) need it.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn deny_vsock() -> Result<(), SandboxError> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    // Same numbers on x86_64 and aarch64 for io_uring (the generic table), not for socket.
    #[cfg(target_arch = "x86_64")]
    const SYS_SOCKET: i64 = 41;
    #[cfg(target_arch = "aarch64")]
    const SYS_SOCKET: i64 = 198;
    const IO_URING: [i64; 3] = [425, 426, 427];
    let un = |e: &dyn std::fmt::Display| SandboxError::Unavailable(format!("seccomp: {e}"));
    let arch = || std::env::consts::ARCH.try_into().map_err(|e| un(&e));
    let install = |f: SeccompFilter| -> Result<(), SandboxError> {
        let bpf: BpfProgram = f.try_into().map_err(|e| un(&e))?;
        seccompiler::apply_filter(&bpf).map_err(|e| un(&e))
    };
    let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, AF_VSOCK)
        .map_err(|e| un(&e))?;
    let rule = SeccompRule::new(vec![cond]).map_err(|e| un(&e))?;
    install(
        SeccompFilter::new(
            [(SYS_SOCKET, vec![rule])].into_iter().collect(),
            SeccompAction::Allow,
            SeccompAction::Errno(EPERM),
            arch()?,
        )
        .map_err(|e| un(&e))?,
    )?;
    // an empty rule list matches the syscall unconditionally
    install(
        SeccompFilter::new(
            IO_URING.into_iter().map(|n| (n, vec![])).collect(),
            SeccompAction::Allow,
            SeccompAction::Errno(ENOSYS),
            arch()?,
        )
        .map_err(|e| un(&e))?,
    )?;
    #[cfg(target_arch = "x86_64")]
    {
        let prog = x32_deny_program();
        seccompiler::apply_filter(&prog).map_err(|e| un(&e))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const AF_VSOCK: u64 = 40;
#[cfg(target_os = "linux")]
const EPERM: u32 = 1;
#[cfg(target_os = "linux")]
const ENOSYS: u32 = 38;

/// Classic BPF: `if (nr >= 0x40000000) return ERRNO(ENOSYS); return ALLOW;`. The architecture
/// check of the other filters kills foreign ABIs, so only the x32 bit matters here.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn x32_deny_program() -> Vec<seccompiler::sock_filter> {
    const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const BPF_JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const BPF_RET_K: u16 = 0x06;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    let i = |code, jt, jf, k| seccompiler::sock_filter { code, jt, jf, k };
    vec![
        i(BPF_LD_W_ABS, 0, 0, 0), // seccomp_data.nr
        i(BPF_JGE_K, 0, 1, 0x4000_0000),
        i(BPF_RET_K, 0, 0, RET_ERRNO | ENOSYS),
        i(BPF_RET_K, 0, 0, RET_ALLOW),
    ]
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
pub fn deny_vsock() -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported(
        "no seccomp filter for this CPU architecture (needed on WSL to deny AF_VSOCK)".into(),
    ))
}

/// `synthesis-sandbox-exec --egress-bridge`: runs INSIDE the bubblewrap network namespace.
/// Listens on `127.0.0.1:port` (the namespace's private loopback), forwards every connection to
/// the proxy's unix socket with plain threads, and runs `program` as a child so it stays the
/// parent. Returns the child's exit code (`128 + signal` when killed). Binds before the child
/// starts, so the proxy address works from its first instruction.
#[cfg(target_os = "linux")]
pub fn run_egress_bridge(
    sock: &Path,
    port: u16,
    program: &std::ffi::OsStr,
    args: &[OsString],
) -> Result<i32, SandboxError> {
    use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Forwards in flight; more are dropped (a runaway command cannot make the bridge spawn
    /// threads without bound).
    const MAX_FORWARDS: usize = 128;
    /// A silent connection ends after this (the proxy has its own, longer, idle limit).
    const IO_TIMEOUT: Duration = Duration::from_secs(300);
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn forward(tcp: TcpStream, sock: PathBuf, _slot: Slot) {
        let Ok(unix) = UnixStream::connect(sock) else {
            return;
        };
        let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
        let _ = unix.set_read_timeout(Some(IO_TIMEOUT));
        let (Ok(tcp2), Ok(unix2)) = (tcp.try_clone(), unix.try_clone()) else {
            return;
        };
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut &tcp2, &mut &unix2);
            let _ = unix2.shutdown(Shutdown::Write);
        });
        let _ = std::io::copy(&mut &unix, &mut &tcp);
        let _ = tcp.shutdown(Shutdown::Write);
    }

    let unavailable = |m: String| SandboxError::Unavailable(format!("egress bridge: {m}"));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| unavailable(format!("cannot listen on 127.0.0.1:{port}: {e}")))?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .spawn()
        .map_err(|e| unavailable(format!("{}: {e}", program.to_string_lossy())))?;
    let sock = sock.to_path_buf();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_FORWARDS {
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
                continue; // dropped: the command sees a reset
            }
            let (sock, slot) = (sock.clone(), Slot);
            std::thread::spawn(move || forward(conn, sock, slot));
        }
    });
    let status = child.wait().map_err(|e| unavailable(e.to_string()))?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
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
    helper_check(exe).ok()
}

/// Why the helper next to `exe` is (not) usable. Trusted means: `exe` itself still exists (a
/// replaced binary shows as `... (deleted)` in `/proc`); the helper is a regular file, not a
/// symlink, owned by root or the current user, not group/world-writable; its directory and every
/// ancestor up to `/` (symlinks resolved) is owned by root or the current user and not
/// world-writable (an ancestor with the sticky bit, like `/tmp`, is tolerated; the helper's own
/// directory is not). A group-writable directory is refused unless the current user owns it and
/// its group is their private group (gid == uid, no other members): that is what a default
/// `umask 0002` gives `mkdir -p` and `target/release`.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(crate) fn helper_check(exe: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    // The owner of /proc/self is the process's effective uid (no libc call needed). If it cannot
    // be read the helper is NOT trusted (fail closed). Off Linux (test builds only) there is no
    // /proc: the owner of the executable's directory stands in.
    #[cfg(target_os = "linux")]
    let euid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .map_err(|e| format!("cannot determine the effective uid (/proc/self: {e}); the sandbox helper is not trusted"))?;
    #[cfg(not(target_os = "linux"))]
    let euid = exe
        .parent()
        .and_then(|d| std::fs::metadata(d).ok())
        .map(|m| m.uid())
        .ok_or("cannot determine the effective uid")?;
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let trusted = |owner: u32, gid: u32, own: bool| group_trusted(&group, euid, owner, gid, own);
    helper_check_with(exe, euid, &trusted)
}

/// Whether write access for group `gid` on a directory owned by `owner` is acceptable, from the
/// `/etc/group` text. `/etc/group` does not list primary-group members, so only groups that are
/// private by construction or by name are trusted:
/// - yours: `owner == euid`, `gid == owner` (user-private group) and EVERY line for that gid
///   lists nobody;
/// - root's: `gid == 0` and every line for gid 0 lists nobody;
/// - `staff` (name `staff`, gid 50: Debian's `/usr/local` is root:staff 2775) on an ANCESTOR
///   only, never on the helper's own directory.
///
/// Everything else (service accounts' groups such as www-data or mail, `users`, ...) is refused.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn group_trusted(text: &str, euid: u32, owner: u32, gid: u32, own: bool) -> bool {
    let lines: Vec<(&str, bool)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<_> = l.split(':').collect();
            (f.len() >= 4 && f[2].parse() == Ok(gid)).then(|| (f[0], f[3].trim().is_empty()))
        })
        .collect();
    let all_empty = !lines.is_empty() && lines.iter().all(|(_, empty)| *empty);
    (owner == euid && owner == gid && all_empty)
        || (owner == 0 && gid == 0 && all_empty)
        || (owner == 0
            && !own
            && gid == 50
            && lines.iter().all(|(n, _)| *n == "staff")
            && !lines.is_empty())
}

/// What is wrong with a directory on the helper's path, if anything (pure, so the rules are
/// tested with root-owned and foreign modes too). `own` = the helper's own directory;
/// `group_trusted(owner, gid, own)` says whether write access for that group is acceptable.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn dir_problem(
    (mode, uid, gid): (u32, u32, u32),
    own: bool,
    euid: u32,
    group_trusted: &dyn Fn(u32, u32, bool) -> bool,
) -> Option<&'static str> {
    let (world, sticky, group_w) = (mode & 0o002 != 0, mode & 0o1000 != 0, mode & 0o020 != 0);
    if uid != 0 && uid != euid {
        Some("is not owned by root or you")
    } else if world && (own || !sticky) {
        Some("is world-writable")
    } else if world {
        // A sticky world-writable ancestor (`/tmp`: 1777) is tolerated: others cannot rename or
        // remove entries they do not own. Its group bit is moot, anyone can write it anyway.
        None
    } else if group_w && !group_trusted(uid, gid, own) {
        Some(
            "is group-writable (refused unless it is yours with your private group, root's own group, or `staff` above the helper's directory)",
        )
    } else {
        None
    }
}

/// [`helper_check`] with the effective uid (`None` = skip owner checks) and the "this gid is the
/// user's private group" test injected.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(crate) fn helper_check_with(
    exe: &Path,
    euid: u32,
    group_trusted: &dyn Fn(u32, u32, bool) -> bool,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return Err("the running synt binary was replaced or deleted (an upgrade in progress?); restart synt".into());
    }
    let dir = exe.parent().ok_or("the executable has no directory")?;
    let h = dir.join(crate::HELPER_NAME);
    let name = crate::HELPER_NAME;
    let lm = std::fs::symlink_metadata(&h)
        .map_err(|_| format!("{name} is missing next to {}; reinstall synt (the archive and `scripts/install.sh` ship it; `cargo install` / `cargo build -p synthesis-cli` do not build it: also run `cargo build -p synthesis-sandbox`)", exe.display()))?;
    if lm.file_type().is_symlink() || !lm.is_file() {
        return Err(format!(
            "{name} is not a regular file (a symlink is refused)"
        ));
    }
    let owner_ok = |m: &std::fs::Metadata| m.uid() == 0 || m.uid() == euid;
    if !owner_ok(&lm) || lm.mode() & 0o022 != 0 {
        return Err(format!(
            "{name} must be owned by root or you and not writable by group/others"
        ));
    }
    let real = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (i, d) in real.ancestors().enumerate() {
        let m = std::fs::metadata(d).map_err(|e| format!("{}: {e}", d.display()))?;
        let facts = (m.mode(), m.uid(), m.gid());
        if let Some(why) = dir_problem(facts, i == 0, euid, group_trusted) {
            return Err(format!(
                "{} {why} (mode {:o}, uid {}, gid {}); the sandbox helper must not be replaceable by others: group-writable and world-writable directories are refused (`chmod go-w {}`)",
                d.display(),
                facts.0 & 0o7777,
                facts.1,
                facts.2,
                d.display()
            ));
        }
    }
    Ok(h)
}

/// Whether the seccomp/Landlock helper next to the running executable is usable; the error says
/// why not (for `synt doctor`).
pub fn helper_status() -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
        helper_check(&exe)
    }
    #[cfg(not(target_os = "linux"))]
    Err("Linux only".into())
}

/// The helper is part of the sandbox: a command that may write it could replace it and run the
/// next one without the filter / Landlock policy. Refused when it lies under any writable root
/// or the private temp dir (symlinks resolved). Used by EVERY path that runs the helper.
pub(crate) fn helper_outside_writable(
    helper: &Path,
    policy: &SandboxPolicy,
) -> Result<(), SandboxError> {
    let real = canon_lossy(helper);
    match policy
        .writable_roots
        .iter()
        .chain(&policy.tmp_dir)
        .find(|r| real.starts_with(canon_lossy(r)))
    {
        Some(r) => Err(SandboxError::Unavailable(format!(
            "the sandbox helper {} lies under the writable root {}; a command could replace it. Install synt outside the workspace and the temp dir",
            helper.display(),
            r.display()
        ))),
        None => Ok(()),
    }
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
    // WSL: Landlock cannot hide the interop socket, so it is never offered (see `wsl`).
    if crate::wsl::refuse_landlock(crate::wsl::host().as_ref()).is_ok() {
        v.extend(LinuxLandlockBackend::detect().map(|b| Box::new(b) as Box<dyn SandboxBackend>));
    }
    v
}

/// Why a Linux backend that exists on this host is not selected (empty when none is skipped).
pub fn skipped_backends() -> Vec<String> {
    let mut v = vec![];
    #[cfg(target_os = "linux")]
    if let Err(e) = bwrap_probe() {
        v.push(e.clone());
    }
    if crate::wsl::is_wsl() {
        v.push(crate::wsl::LANDLOCK_NOT_USED.into());
    }
    v
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
            notes: vec![
                format!("landlock ABI {}; kernel-enforced and inherited by all descendants", self.abi),
                "allow-list only: deny_paths overlapping an allowed root are rejected".into(),
                "network Deny blocks TCP bind/connect only (ABI >= 4); UDP and AF_UNIX stay open (Strong network needs bubblewrap)".into(),
                "network Loopback is unsupported (TCP ports only, no addresses); allow_exec=false and Proxied are rejected; tree kill is the terminal runtime's job".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        crate::wsl::refuse_landlock(crate::wsl::host().as_ref())?;
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
        if matches!(policy.network, NetworkPolicy::Proxied(_)) {
            return Err(SandboxError::Unsupported(
                "landlock cannot confine a command to the egress proxy: --network hosts needs bubblewrap or Seatbelt".into(),
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
        helper_outside_writable(&self.helper, &p)?;
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
    /// The `synthesis-sandbox-exec` helper; on WSL it runs in front of bwrap to install the
    /// seccomp filter (no AF_VSOCK). `None` there means commands are refused.
    helper: Option<PathBuf>,
}

impl BwrapBackend {
    /// Some only if a trusted `bwrap` passed the cached [`bwrap_probe`].
    pub fn detect() -> Option<Self> {
        #[cfg(target_os = "linux")]
        let helper = std::env::current_exe()
            .ok()
            .and_then(|e| helper_next_to(&e));
        #[cfg(not(target_os = "linux"))]
        let helper = None;
        bwrap_probe().as_ref().ok().map(|path| Self {
            path: path.clone(),
            helper,
        })
    }

    /// Like [`Self::detect`] with the seccomp helper injected (tests and embedders; `detect` finds
    /// it next to the current executable).
    pub fn with_helper(helper: PathBuf) -> Option<Self> {
        Self::detect().map(|b| Self {
            helper: Some(helper),
            ..b
        })
    }

    pub(crate) fn args(policy: &SandboxPolicy, cmd: &SandboxedCommand) -> Vec<std::ffi::OsString> {
        Self::args_on(policy, cmd, crate::wsl::WslCtx::current().as_ref())
    }

    /// `wsl` = Some on WSL: `/mnt/wsl` and `/mnt/wslg` are tmpfs (the real `resolv.conf` target
    /// is bound back read-only), every Windows drive is hidden (default) or read-only, all
    /// before the writable binds so a workspace on a drive stays writable; `/run`, hence
    /// `/run/WSL`, is already a tmpfs. `policy` must be canonical (see `wsl::check_policy`).
    pub(crate) fn args_on(
        policy: &SandboxPolicy,
        cmd: &SandboxedCommand,
        wsl: Option<&crate::wsl::WslCtx>,
    ) -> Vec<std::ffi::OsString> {
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
        if let Some(ctx) = wsl {
            for m in &ctx.mask {
                // the driver store is only masked together with the drives (GPU opt-out)
                if m == Path::new(crate::wsl::DRIVER_STORE) && !policy.wsl_hide_drives {
                    continue;
                }
                push(&[m], "--tmpfs");
            }
            if let Some(r) = &ctx.resolv {
                push(&[r, r], "--ro-bind"); // DNS keeps working (a symlink into /mnt/wsl)
            }
            for m in &ctx.drives {
                if policy.wsl_hide_drives {
                    push(&[m], "--tmpfs");
                } else {
                    push(&[m, m], "--ro-bind");
                }
            }
            if policy.wsl_hide_drives {
                // read access to the explicit readable roots on a drive comes back, read-only
                for r in policy
                    .readable_roots
                    .iter()
                    .filter(|r| ctx.drives.iter().any(|m| r.starts_with(m)))
                {
                    push(&[r, r], "--ro-bind");
                }
            }
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
        if !matches!(policy.network, NetworkPolicy::Allow) {
            push(&[], "--unshare-net");
        }
        // Proxied: the proxy's private run dir (after the tmpfs mounts above, which may hide it);
        // read-only is enough to connect to the socket.
        if let NetworkPolicy::Proxied(EgressEndpoint::Unix(sock)) = &policy.network
            && let Some(dir) = sock.parent()
        {
            push(&[dir, dir], "--ro-bind");
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
        match mode {
            // Fresh net namespace whose only exit is the bridge; needs the helper (bridge, seccomp).
            // Capped at Weak: the root is bound read-only, not hidden, so a command can still
            // connect() to a host PATHNAME unix socket outside /run, /var/run, /tmp and the denied
            // paths (docs/specs/sandbox-hardening.md, residual risk). Strong waits for the read
            // allow-list.
            NetworkPolicy::Proxied(EgressEndpoint::Unix(_)) if self.helper.is_some() => {
                Guarantee::Weak
            }
            _ => network_guarantee_for(mode, self.capabilities().network, Guarantee::Strong),
        }
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::Weak,
            network: Guarantee::Strong,
            process_tree: Guarantee::Strong,
            notes: vec![
                "writes confined to writable_roots, tmp_dir and a private /tmp; /run is a fresh tmpfs; reads are NOT confined except deny_paths (whole fs mounted read-only)".into(),
                "Weak: defence in depth, not a security boundary".into(),
                "network Deny/Loopback use a fresh net namespace; with Loopback its `lo` is up but PRIVATE to the sandbox: a dev server and tests inside it work, host loopback services are unreachable; Proxied keeps the fresh namespace and bridges 127.0.0.1:3128 to the proxy's unix socket (needs the helper) but is Weak: host pathname unix sockets outside /run, /var/run, /tmp and the denied paths stay connectable until the read allow-list lands; allow_exec=false is rejected".into(),
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
        self.wrap_on(policy, c, crate::wsl::WslCtx::current().as_ref())
    }
}

impl BwrapBackend {
    /// [`SandboxBackend::wrap`] for an explicit WSL context (`Some` = WSL; tests inject one).
    /// On WSL the policy is checked and canonicalised first (any violation is an error, never
    /// a downgrade) and the command runs behind the helper that denies AF_VSOCK.
    pub(crate) fn wrap_on(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
        wsl: Option<&crate::wsl::WslCtx>,
    ) -> Result<SandboxedCommand, SandboxError> {
        let checked;
        let policy = match wsl {
            Some(ctx) => {
                crate::wsl::require_known_mounts(&ctx.host)?;
                checked = crate::wsl::check_policy(policy, &ctx.host)?;
                &checked
            }
            None => policy,
        };
        let bridge = match &policy.network {
            NetworkPolicy::Proxied(EgressEndpoint::Unix(sock)) => Some(sock),
            NetworkPolicy::Proxied(EgressEndpoint::Tcp { .. }) => {
                return Err(SandboxError::Unsupported(
                    "bubblewrap reaches the egress proxy over a unix socket, not TCP".into(),
                ));
            }
            _ => None,
        };
        let helper_or_err = |why: &str| {
            self.helper.clone().ok_or_else(|| {
                SandboxError::Unavailable(format!(
                    "{why}: the sandbox helper is required: {}",
                    helper_status().err().unwrap_or_else(|| "not found".into())
                ))
            })
        };
        let mut inner = c.clone();
        if let Some(sock) = bridge {
            // Inside the namespace the helper stays the parent of the command and forwards
            // 127.0.0.1:3128 to the proxy socket.
            let helper = helper_or_err("--network hosts")?;
            helper_outside_writable(&helper, policy)?;
            inner.program = helper.into();
            inner.args = [
                "--egress-bridge".into(),
                sock.clone().into_os_string(),
                EGRESS_BRIDGE_PORT.to_string().into(),
                "--".into(),
                c.program.clone(),
            ]
            .into_iter()
            .chain(c.args.iter().cloned())
            .collect();
        }
        let mut args = Self::args_on(policy, &inner, wsl);
        let mut program: OsString = self.path.clone().into();
        // The seccomp deny (vsock, io_uring, x32) runs in front of bwrap: on WSL, and whenever
        // the network is proxied (io_uring could otherwise open sockets around the namespace).
        if wsl.is_some() || bridge.is_some() {
            let helper = helper_or_err(if wsl.is_some() {
                "WSL"
            } else {
                "--network hosts"
            })?;
            helper_outside_writable(&helper, policy)?;
            args.splice(0..0, ["--no-vsock".into(), "--".into(), program]);
            program = helper.into();
        }
        Ok(SandboxedCommand {
            program,
            args,
            env: crate::build_env_on(policy, c, wsl.map(|x| &x.host)),
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

    #[cfg(unix)] // renders POSIX mount arguments
    #[test]
    fn bwrap_blanks_a_denied_key_file_and_a_denied_dir() {
        let t = tempfile::tempdir().unwrap();
        let (key, run) = (t.path().join("chain.key"), t.path().join("run"));
        std::fs::write(&key, "K").unwrap();
        std::fs::create_dir(&run).unwrap();
        let pol = SandboxPolicy {
            deny_paths: vec![key.clone(), run.clone()],
            ..Default::default()
        };
        let j = BwrapBackend::args(&pol, &SandboxedCommand::default())
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            j.contains(&format!("--ro-bind /dev/null {}", key.display())),
            "{j}"
        );
        assert!(j.contains(&format!("--tmpfs {}", run.display())), "{j}");
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

    #[cfg(unix)]
    fn wsl_ctx(drive: &Path, resolv: Option<&Path>) -> crate::wsl::WslCtx {
        crate::wsl::WslCtx {
            host: synthesis_platform::wsl::WslHost {
                version: synthesis_platform::wsl::WslVersion::V2,
                windows_mounts: vec![drive.to_path_buf()],
                mounts_known: true,
            },
            resolv: resolv.map(Path::to_path_buf),
            mask: vec![
                "/mnt/wsl".into(),
                "/mnt/wslg".into(),
                "/usr/lib/wsl/drivers".into(),
            ],
            // an unlisted /mnt/z drive is hidden too
            drives: vec![drive.to_path_buf(), "/mnt/z".into()],
        }
    }

    #[cfg(unix)]
    fn joined(a: Vec<std::ffi::OsString>) -> String {
        a.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_wsl_masks_wsl_dirs_and_hides_drives_before_binds() {
        let drive = Path::new("/mnt/c");
        let ctx = wsl_ctx(drive, Some(Path::new("/mnt/wsl/resolv.conf")));
        let pol = SandboxPolicy {
            writable_roots: vec![p("/home/u/ws")],
            readable_roots: vec![p("/mnt/c/tools"), p("/home/u/.cargo")],
            ..Default::default()
        };
        let j = joined(BwrapBackend::args_on(
            &pol,
            &SandboxedCommand::default(),
            Some(&ctx),
        ));
        let at = |n: &str| j.find(n).unwrap_or_else(|| panic!("{n} in {j}"));
        assert!(at("--tmpfs /run") < at("--tmpfs /mnt/wsl"), "{j}");
        at("--tmpfs /mnt/wslg");
        // the driver store and an unlisted /mnt/<letter> drive are hidden by default
        at("--tmpfs /usr/lib/wsl/drivers");
        at("--tmpfs /mnt/z ");
        // resolv.conf comes back read-only after the mask
        assert!(
            at("--tmpfs /mnt/wsl ") < at("--ro-bind /mnt/wsl/resolv.conf /mnt/wsl/resolv.conf"),
            "{j}"
        );
        // drive hidden, then only the explicit readable root on it is bound back (read-only)
        assert!(
            at("--tmpfs /mnt/c ") < at("--ro-bind /mnt/c/tools /mnt/c/tools"),
            "{j}"
        );
        assert!(at("--tmpfs /mnt/c ") < at("--bind /home/u/ws"), "{j}");
        assert!(!j.contains("--ro-bind /mnt/c /mnt/c"), "{j}");
        assert!(
            !j.contains("--ro-bind /home/u/.cargo"),
            "only drive roots are re-bound: {j}"
        );
        // opt-out: the drive stays visible but read-only
        let open = SandboxPolicy {
            wsl_hide_drives: false,
            ..pol.clone()
        };
        let j = joined(BwrapBackend::args_on(
            &open,
            &SandboxedCommand::default(),
            Some(&ctx),
        ));
        assert!(
            j.contains("--ro-bind /mnt/c /mnt/c") && !j.contains("--tmpfs /mnt/c "),
            "{j}"
        );
        assert!(
            j.contains("--tmpfs /mnt/wsl "),
            "masking /mnt/wsl is not optional: {j}"
        );
        // not WSL: none of it
        let j = joined(BwrapBackend::args_on(
            &pol,
            &SandboxedCommand::default(),
            None,
        ));
        assert!(!j.contains("/mnt/"), "{j}");
    }

    #[cfg(unix)]
    #[test]
    fn helper_trust_rejects_symlink_writable_dir_deleted_exe_and_missing() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        let exe = dir.join("synt");
        let real = file_with_mode(t.path(), "real-helper", 0o755);
        // missing: the message says how to get it
        let e = helper_check(&exe).unwrap_err();
        assert!(
            e.contains("missing") && e.contains("cargo build -p synthesis-sandbox"),
            "{e}"
        );
        // a symlink (even to a good file) is refused
        std::os::unix::fs::symlink(&real, dir.join(crate::HELPER_NAME)).unwrap();
        assert!(helper_check(&exe).unwrap_err().contains("symlink"));
        std::fs::remove_file(dir.join(crate::HELPER_NAME)).unwrap();
        // a good file, then a group/world-writable directory
        file_with_mode(&dir, crate::HELPER_NAME, 0o755);
        assert!(helper_check(&exe).is_ok());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let e = helper_check(&exe).unwrap_err();
        assert!(
            e.contains("world-writable") && e.contains("chmod go-w"),
            "{e}"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // the running binary was replaced
        let gone = dir.join("synt (deleted)");
        assert!(
            helper_check(&gone)
                .unwrap_err()
                .contains("replaced or deleted")
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_dir_rules_group_world_and_ancestors() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let t = tempfile::tempdir().unwrap();
        let anc = t.path().canonicalize().unwrap().join("anc");
        let dir = anc.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("synt");
        file_with_mode(&dir, crate::HELPER_NAME, 0o755);
        let mode =
            |d: &Path, m| std::fs::set_permissions(d, std::fs::Permissions::from_mode(m)).unwrap();
        let me = std::fs::metadata(&dir).unwrap().uid();
        let gid = std::fs::metadata(&dir).unwrap().gid();
        let mine = |o: u32, g: u32, _own: bool| o == me && g == gid; // "the user's private group"
        let foreign = |_: u32, _: u32, _: bool| false;
        let run = |p: &dyn Fn(u32, u32, bool) -> bool| helper_check_with(&exe, me, p);
        mode(&anc, 0o755);
        // 0775 in the user's private group (default umask 0002): accepted
        mode(&dir, 0o775);
        assert!(run(&mine).is_ok());
        // 0775 in a foreign group, and anything world-writable: refused, with the reason
        let e = run(&foreign).unwrap_err();
        assert!(e.contains("group-writable") && e.contains("refused"), "{e}");
        mode(&dir, 0o777);
        assert!(run(&mine).unwrap_err().contains("world-writable"));
        mode(&dir, 0o1777); // sticky does not excuse the helper's own directory
        assert!(run(&mine).unwrap_err().contains("world-writable"));
        mode(&dir, 0o755);
        // ancestors: world-writable without sticky refused, with sticky tolerated, group rule applies
        mode(&anc, 0o777);
        assert!(run(&mine).unwrap_err().contains("world-writable"));
        mode(&anc, 0o1777);
        assert!(run(&mine).is_ok());
        mode(&anc, 0o775);
        assert!(run(&foreign).unwrap_err().contains("group-writable"));
        assert!(run(&mine).is_ok());
        // a foreign owner is refused whatever the mode
        assert!(helper_check_with(&exe, me.wrapping_add(1), &mine).is_err());
        mode(&anc, 0o755);
    }

    #[cfg(unix)]
    const GROUPS: &str = "root:x:0:\ndaemon:x:1:\nmail:x:8:\nwww-data:x:33:\nstaff:x:50:\nusers:x:100:a,b\nme:x:1001:\nshared:x:1002:\nshared2:x:1002:bob\n";

    #[cfg(unix)]
    #[test]
    fn dir_rules_for_the_layouts_real_machines_have() {
        let t = |o: u32, g: u32, own: bool| group_trusted(GROUPS, 1001, o, g, own);
        let p = |m, u, g, own| dir_problem((m, u, g), own, 1001, &t);
        // /tmp (1777 root:root), /usr/local/bin (0755 root), the runner's 0700 tempdir, ~/.synt/bin (0775 own group)
        assert_eq!(
            p(0o41777, 0, 0, false),
            None,
            "sticky world-writable ancestor"
        );
        assert_eq!(p(0o40755, 0, 0, true), None);
        assert_eq!(p(0o40700, 1001, 1001, true), None);
        assert_eq!(p(0o40775, 1001, 1001, true), None);
        assert_eq!(
            p(0o42775, 0, 0, false),
            None,
            "root:root group-writable ancestor"
        );
        assert_eq!(
            p(0o42775, 0, 50, false),
            None,
            "Debian /usr/local: root:staff 2775 above the helper dir"
        );
        // refused: the security intent
        let refused = |m, u, g, own, why: &str, what: &str| {
            let e = p(m, u, g, own).unwrap_or_else(|| panic!("{what}: accepted"));
            assert!(e.contains(why), "{what}: {e}");
        };
        refused(0o40777, 1001, 1001, false, "world-writable", "no sticky");
        refused(
            0o41777,
            1001,
            1001,
            true,
            "world-writable",
            "own dir, even sticky",
        );
        refused(0o40775, 1001, 100, true, "group-writable", "foreign group");
        refused(
            0o40775,
            0,
            1500,
            false,
            "group-writable",
            "user group under root",
        );
        refused(
            0o40775,
            0,
            33,
            false,
            "group-writable",
            "www-data style service group",
        );
        refused(0o40775, 0, 8, false, "group-writable", "mail");
        refused(0o40775, 0, 100, false, "group-writable", "users");
        refused(
            0o42775,
            0,
            50,
            true,
            "group-writable",
            "staff on the helper's own dir",
        );
        refused(0o40755, 1002, 1002, false, "not owned", "foreign owner");
        refused(
            0o41777,
            1002,
            1002,
            false,
            "not owned",
            "foreign sticky dir",
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_trust_reads_every_etc_group_line() {
        let g = |text: &str, owner, gid, own| group_trusted(text, 1001, owner, gid, own);
        assert!(g(GROUPS, 0, 0, true) && g(GROUPS, 1001, 1001, true));
        // gid 0 that lists members, or a duplicate gid line that lists one: refused
        assert!(!g("root:x:0:mallory\n", 0, 0, true));
        assert!(!g("root:x:0:\nroot2:x:0:mallory\n", 0, 0, true));
        assert!(!g("me:x:1001:\nme2:x:1001:bob\n", 1001, 1001, true));
        // your private group needs gid == uid; an unknown gid and an unreadable file never trust
        assert!(!g(GROUPS, 1001, 1002, true) && !g(GROUPS, 0, 4242, false));
        assert!(!g("", 0, 0, true) && !g("", 1001, 1001, true));
        // staff only by name AND gid, only above the helper's directory, never for a user's dir
        assert!(g(GROUPS, 0, 50, false) && !g(GROUPS, 0, 50, true));
        assert!(
            !g("other:x:50:\n", 0, 50, false),
            "gid 50 under another name"
        );
        assert!(
            !g(GROUPS, 1001, 50, false),
            "staff on a dir you own is not the /usr/local case"
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_must_not_lie_under_any_writable_root_or_the_temp_dir() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let (ws, tmp, bin) = (base.join("ws"), base.join("tmp"), base.join("bin"));
        for d in [&ws, &tmp, &bin, &ws.join("target/debug")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let pol = |w: Vec<PathBuf>, tmp_dir: Option<PathBuf>| SandboxPolicy {
            writable_roots: w,
            tmp_dir,
            ..Default::default()
        };
        let ok = pol(vec![ws.clone()], Some(tmp.clone()));
        assert!(helper_outside_writable(&bin.join("h"), &ok).is_ok());
        for (helper, policy, what) in [
            (ws.join("target/debug/h"), ok.clone(), "under the workspace"),
            (tmp.join("h"), ok.clone(), "under tmp_dir"),
            (
                bin.join("h"),
                pol(vec![base.clone()], None),
                "under a writable root above it",
            ),
            (
                bin.join("h"),
                pol(vec![], Some(bin.clone())),
                "in tmp_dir itself",
            ),
        ] {
            let e = helper_outside_writable(&helper, &policy).unwrap_err();
            assert!(
                matches!(e, SandboxError::Unavailable(_))
                    && e.to_string().contains("could replace"),
                "{what}: {e}"
            );
        }
        // every backend that runs the helper applies it: Landlock and the WSL bubblewrap path
        let l = LinuxLandlockBackend::new(tmp.join("synthesis-sandbox-exec"), 4);
        let c = SandboxedCommand {
            cwd: Some(ws.clone()),
            ..Default::default()
        };
        assert!(
            l.wrap(&ok, &c)
                .unwrap_err()
                .to_string()
                .contains("could replace")
        );
        let l = LinuxLandlockBackend::new(bin.join("synthesis-sandbox-exec"), 4);
        assert!(l.wrap(&ok, &c).is_ok());
        let ctx = wsl_ctx(Path::new("/mnt/c"), None);
        let b = BwrapBackend {
            path: bin.join("bwrap"),
            helper: Some(ws.join("target/debug/synthesis-sandbox-exec")),
        };
        assert!(
            b.wrap_on(&ok, &c, Some(&ctx))
                .unwrap_err()
                .to_string()
                .contains("could replace")
        );
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_wsl_wrap_refuses_unknown_mounts_and_a_helper_in_the_workspace() {
        let ctx = wsl_ctx(Path::new("/mnt/c"), None);
        let c = SandboxedCommand {
            cwd: Some(p("/work")),
            ..Default::default()
        };
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            ..Default::default()
        };
        let b = BwrapBackend {
            path: p("/b"),
            helper: Some(p("/work/synthesis-sandbox-exec")),
        };
        let e = b.wrap_on(&pol, &c, Some(&ctx)).unwrap_err();
        assert!(e.to_string().contains("under the writable root"), "{e}");
        let mut blind = wsl_ctx(Path::new("/mnt/c"), None);
        blind.host.mounts_known = false;
        let ok = BwrapBackend {
            path: p("/b"),
            helper: Some(p("/h/x")),
        };
        let e = ok.wrap_on(&pol, &c, Some(&blind)).unwrap_err();
        assert!(
            matches!(e, SandboxError::Unavailable(_)) && e.to_string().contains("/proc/mounts"),
            "{e}"
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn x32_program_denies_bit_30_syscalls_only() {
        let p = x32_deny_program();
        assert_eq!(p.len(), 4);
        // ld nr; jge 0x40000000 (true -> next, false -> skip one); ret errno(ENOSYS); ret allow
        assert_eq!((p[0].code, p[0].k), (0x20, 0));
        assert_eq!(
            (p[1].code, p[1].jt, p[1].jf, p[1].k),
            (0x35, 0, 1, 0x4000_0000)
        );
        assert_eq!((p[2].code, p[2].k), (0x06, 0x0005_0000 | 38));
        assert_eq!((p[3].code, p[3].k), (0x06, 0x7fff_0000));
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_wsl_wrap_checks_policy_runs_behind_the_vsock_helper_and_scrubs_env() {
        let b = BwrapBackend {
            path: p("/usr/bin/bwrap"),
            helper: Some(p("/h/synthesis-sandbox-exec")),
        };
        let ctx = wsl_ctx(Path::new("/mnt/c"), None);
        let c = SandboxedCommand {
            program: "ls".into(),
            env: vec![
                ("WSL_INTEROP".into(), "x".into()),
                ("DISPLAY".into(), ":0".into()),
            ],
            cwd: Some(p("/work")),
            ..Default::default()
        };
        let pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            ..Default::default()
        };
        let w = b.wrap_on(&pol, &c, Some(&ctx)).unwrap();
        assert_eq!(w.program, p("/h/synthesis-sandbox-exec").into_os_string());
        assert_eq!(&w.args[..3], ["--no-vsock", "--", "/usr/bin/bwrap"]);
        assert!(w.env.is_empty(), "{:?}", w.env);
        // no helper on WSL: refused, not run without the seccomp filter
        let nohelper = BwrapBackend { helper: None, ..b };
        assert!(matches!(
            nohelper.wrap_on(&pol, &c, Some(&ctx)),
            Err(SandboxError::Unavailable(_))
        ));
        // the drive policy is enforced here (a drive root), as InvalidPolicy
        let bad = SandboxPolicy {
            writable_roots: vec![p("/mnt/c")],
            ..Default::default()
        };
        assert!(matches!(
            BwrapBackend {
                helper: Some(p("/h/x")),
                path: p("/b")
            }
            .wrap_on(&bad, &c, Some(&ctx)),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_proxied_bridges_the_socket_behind_the_seccomp_helper() {
        let b = BwrapBackend {
            path: p("/usr/bin/bwrap"),
            helper: Some(p("/h/synthesis-sandbox-exec")),
        };
        let c = SandboxedCommand {
            program: "curl".into(),
            args: vec!["-V".into()],
            cwd: Some(p("/work")),
            ..Default::default()
        };
        let mut pol = SandboxPolicy {
            writable_roots: vec![p("/work")],
            network: NetworkPolicy::Proxied(EgressEndpoint::Unix(p("/run-dir/e.sock"))),
            ..Default::default()
        };
        // not WSL: the seccomp deny still runs in front of bwrap
        let w = b.wrap_on(&pol, &c, None).unwrap();
        assert_eq!(w.program, p("/h/synthesis-sandbox-exec").into_os_string());
        assert_eq!(&w.args[..3], ["--no-vsock", "--", "/usr/bin/bwrap"]);
        let j = joined(w.args.clone());
        assert!(j.contains("--unshare-net") && !j.contains("--share-net"));
        assert!(j.contains("--ro-bind /run-dir /run-dir"), "{j}");
        assert!(
            j.ends_with(
                "-- /h/synthesis-sandbox-exec --egress-bridge /run-dir/e.sock 3128 -- curl -V"
            ),
            "{j}"
        );
        assert!(
            w.env
                .iter()
                .any(|(k, v)| k == "HTTPS_PROXY" && v == "http://127.0.0.1:3128")
        );
        assert_eq!(b.network_guarantee(&pol.network), Guarantee::Weak);
        // no helper: no bridge and no seccomp, so refused (and not claimed)
        let nohelper = BwrapBackend { helper: None, ..b };
        assert!(matches!(
            nohelper.wrap_on(&pol, &c, None),
            Err(SandboxError::Unavailable(_))
        ));
        assert_eq!(nohelper.network_guarantee(&pol.network), Guarantee::None);
        // a helper inside the writable root could be replaced by the command
        let inside = BwrapBackend {
            helper: Some(p("/work/h")),
            path: p("/b"),
        };
        assert!(inside.wrap_on(&pol, &c, None).is_err());
        // TCP endpoints are Seatbelt's
        pol.network = NetworkPolicy::Proxied(EgressEndpoint::Tcp {
            port: 1,
            token: "t".into(),
        });
        let b2 = BwrapBackend {
            path: p("/b"),
            helper: Some(p("/h/x")),
        };
        assert!(matches!(
            b2.wrap_on(&pol, &c, None),
            Err(SandboxError::Unsupported(_))
        ));
        assert_eq!(b2.network_guarantee(&pol.network), Guarantee::None);
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
            helper: None,
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

    // ---- real bubblewrap, Linux only. A skipped test is a PANIC when SYNT_REQUIRE_BWRAP is set (CI).
    #[cfg(target_os = "linux")]
    mod real {
        use super::*;
        use std::process::Command;

        fn bwrap() -> Option<BwrapBackend> {
            let b = BwrapBackend::detect().map(|b| BwrapBackend {
                helper: Some("/not-run-in-these-tests".into()),
                ..b
            });
            if b.is_none() {
                assert!(
                    std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
                    "SYNT_REQUIRE_BWRAP is set but bubblewrap is not usable here"
                );
                eprintln!("skipped: bubblewrap is not usable here");
            }
            b
        }

        struct Fx {
            _t: tempfile::TempDir,
            drive: PathBuf,
            ws: PathBuf,
        }

        /// A fake Windows drive (a plain directory) holding a secret, and a workspace beside it.
        fn fx() -> Fx {
            let t = tempfile::tempdir().unwrap();
            let drive = t.path().canonicalize().unwrap().join("c");
            // the workspace is NOT on the drive: a drive workspace is always refused
            let ws = t.path().canonicalize().unwrap().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            std::fs::create_dir_all(&drive).unwrap();
            std::fs::create_dir_all(drive.join("Windows")).unwrap(); // a real directory on PATH
            std::fs::write(drive.join("secret.txt"), "windows file").unwrap();
            Fx { _t: t, drive, ws }
        }

        /// Run `script` in the sandbox of a fake WSL host (the helper is skipped: the seccomp part
        /// has its own test).
        fn run(
            f: &Fx,
            hide: bool,
            readable: Vec<PathBuf>,
            env: Vec<(&str, String)>,
            script: &str,
        ) -> Option<String> {
            let b = bwrap()?;
            let ctx = crate::wsl::WslCtx {
                host: synthesis_platform::wsl::WslHost {
                    version: synthesis_platform::wsl::WslVersion::V2,
                    windows_mounts: vec![f.drive.clone()],
                    mounts_known: true,
                },
                resolv: None,
                mask: vec![],
                drives: vec![f.drive.clone()],
            };
            let c = SandboxedCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env: env.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
                cwd: Some(f.ws.clone()),
                ..Default::default()
            };
            let pol = SandboxPolicy {
                writable_roots: vec![f.ws.clone()],
                readable_roots: readable,
                network: NetworkPolicy::Allow,
                wsl_hide_drives: hide,
                ..Default::default()
            };
            let w = b.wrap_on(&pol, &c, Some(&ctx)).unwrap();
            // behind the helper: [helper, --no-vsock, --, bwrap, args...]; run bwrap directly
            assert_eq!(&w.args[..2], ["--no-vsock", "--"]);
            let o = Command::new(&w.args[2])
                .args(&w.args[3..])
                .env_clear()
                .envs(w.env.iter().cloned())
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "bwrap failed ({}): stderr={:?} stdout={:?} args={:?}",
                o.status,
                String::from_utf8_lossy(&o.stderr),
                String::from_utf8_lossy(&o.stdout),
                &w.args[3..]
            );
            Some(String::from_utf8_lossy(&o.stdout).into_owned())
        }

        #[test]
        fn a_denied_key_file_reads_empty_inside() {
            let Some(b) = bwrap() else { return };
            // not under /tmp: bwrap mounts a fresh tmpfs there, which would hide the file anyway
            let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let key = d.path().join("chain.key");
            std::fs::write(&key, "KEYBYTES").unwrap();
            let cat = |deny: Vec<PathBuf>| {
                let c = SandboxedCommand {
                    program: "/bin/cat".into(),
                    args: vec![key.clone().into_os_string()],
                    ..Default::default()
                };
                let pol = SandboxPolicy {
                    deny_paths: deny,
                    network: NetworkPolicy::Allow,
                    ..Default::default()
                };
                let w = b.wrap_on(&pol, &c, None).unwrap();
                // no WSL and no proxy: not behind the helper, so `program` is bwrap itself
                assert_eq!(w.program, b.path.clone().into_os_string());
                let o = Command::new(&w.program)
                    .args(&w.args)
                    .env_clear()
                    .envs(w.env.iter().cloned())
                    .output()
                    .unwrap();
                assert!(o.status.success(), "bwrap failed: {o:?}");
                String::from_utf8_lossy(&o.stdout).into_owned()
            };
            assert_eq!(
                cat(vec![]),
                "KEYBYTES",
                "control: readable without the deny"
            );
            assert_eq!(cat(vec![key.clone()]), "", "denied: reads empty");
        }

        #[test]
        fn env_path_and_run_are_clean_inside() {
            let f = fx();
            let path = format!("/usr/bin:/bin:{}/Windows", f.drive.display());
            let Some(out) = run(
                &f,
                true,
                vec![],
                vec![
                    ("WSL_INTEROP", "/run/WSL/1_interop".into()),
                    ("WSLENV", "X".into()),
                    ("DISPLAY", ":0".into()),
                    ("XDG_RUNTIME_DIR", "/run/user/1".into()),
                    ("PULSE_SERVER", "x".into()),
                    ("WAYLAND_DISPLAY", "w".into()),
                    ("PATH", path),
                ],
                "echo \"v=${WSL_INTEROP-u}${WSLENV-u}${DISPLAY-u}${XDG_RUNTIME_DIR-u}${PULSE_SERVER-u}${WAYLAND_DISPLAY-u}\"; echo \"path=$PATH\"; echo \"run=$(ls -A /run | wc -l)\"",
            ) else {
                return;
            };
            assert!(out.contains("v=uuuuuu"), "{out}");
            // canonical entries (/bin is /usr/bin on merged-usr), and the drive directory is gone
            let path = out
                .lines()
                .find_map(|l| l.strip_prefix("path="))
                .expect(&out);
            assert!(
                path.starts_with("/usr/bin") && !path.contains("Windows"),
                "{out}"
            );
            assert!(
                out.contains("run=0"),
                "/run is a fresh tmpfs, so /run/WSL cannot exist: {out}"
            );
        }

        #[test]
        fn drive_is_hidden_by_default_and_the_workspace_stays_writable() {
            let f = fx();
            let script = format!(
                "ls -A {d}; test -e {d}/secret.txt && echo SECRET-VISIBLE; echo a > {w}/ok.txt && echo ws-written; (echo b > {d}/evil.txt) 2>/dev/null && echo DRIVE-WRITTEN; true",
                d = f.drive.display(),
                w = f.ws.display()
            );
            let Some(out) = run(&f, true, vec![], vec![], &script) else {
                return;
            };
            assert!(out.contains("ws-written"), "{out}");
            assert!(
                !out.contains("secret.txt"),
                "the hidden drive lists nothing: {out}"
            );
            assert!(!out.contains("SECRET-VISIBLE"), "{out}");
            // a write to the hidden drive lands in the throwaway tmpfs, never on the real directory
            assert!(f.ws.join("ok.txt").exists() && !f.drive.join("evil.txt").exists());
        }

        #[test]
        fn readable_root_on_a_drive_comes_back_read_only() {
            let f = fx();
            let tools = f.drive.join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            std::fs::write(tools.join("t.txt"), "t").unwrap();
            let script = format!(
                "cat {t}/t.txt; test -e {d}/secret.txt && echo SECRET-VISIBLE; (echo x > {t}/n) 2>/dev/null && echo TOOLS-WRITTEN; true",
                t = tools.display(),
                d = f.drive.display()
            );
            let Some(out) = run(&f, true, vec![tools.clone()], vec![], &script) else {
                return;
            };
            assert!(
                out.starts_with('t')
                    && !out.contains("SECRET-VISIBLE")
                    && !out.contains("TOOLS-WRITTEN"),
                "{out}"
            );
        }

        #[test]
        fn opt_out_keeps_the_drive_visible_but_read_only_by_mount_flag() {
            let f = fx();
            // the mountinfo options of the bound mounts INSIDE the sandbox (not just a failed write)
            let script = format!(
                "cat {d}/secret.txt; echo; awk '$5==\"{d}\"{{print \"drive:\" $6}} $5==\"{w}\"{{print \"ws:\" $6}}' /proc/self/mountinfo",
                d = f.drive.display(),
                w = f.ws.display()
            );
            let Some(out) = run(&f, false, vec![], vec![], &script) else {
                return;
            };
            let flags = |tag: &str| {
                out.lines()
                    .find_map(|l| l.strip_prefix(tag))
                    .unwrap_or_else(|| panic!("no {tag} mount in {out}"))
                    .split(',')
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            };
            assert!(out.contains("windows file"), "{out}");
            assert!(flags("drive:").contains(&"ro".to_string()), "{out}");
            assert!(flags("ws:").contains(&"rw".to_string()), "{out}");
        }
    }
}
