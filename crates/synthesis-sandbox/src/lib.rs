//! OS-level sandboxing for commands the agent runs (design doc 5.7 / 5.12, ADR-010).
//!
//! This crate only *wraps* a command: `backend.wrap(policy, cmd)` returns a new
//! [`SandboxedCommand`] (usually `sandbox-exec ...` or `bwrap ...` around the original).
//! Integration point: `synthesis-terminal` should build its `Command` from the wrapped
//! `program/args/cwd/env` (and call `env_clear()` when `clear_env` is set) instead of the
//! raw command. Callers MUST consult [`SandboxBackend::capabilities`] and tell the user
//! what is enforced; `wrap` on a backend with `None` guarantees runs the command unconfined.
//!
//! `synthesis-terminal` does this: `TerminalRuntime` wraps the shell command when a policy is set and
//! (in its default `Enforce` mode) refuses to run when the backend is [`NullSandbox`]/unusable or
//! `SandboxPolicy::required` is not met. [`detect`] itself still falls back to [`NullSandbox`];
//! the caller decides whether that is acceptable.
//!
//! Guarantee model: nothing here is a container. Seatbelt is deprecated and best-effort,
//! Landlock is allow-list only, Windows has no filesystem/network confinement yet.
//!
//! **`Guarantee::Weak` is defence in depth, NOT a security boundary.** Do not rely on a `Weak`
//! backend (Seatbelt, bubblewrap filesystem, Landlock network) to contain a hostile process;
//! treat it as a seat belt that stops accidents and lazy attacks. Only `Strong` is a boundary,
//! and even that is not a container or VM.

mod helper;
mod linux;
mod seatbelt;
mod windows;
pub mod wsl;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub use helper::{HELPER_NAME, decode_policy, encode_policy, parse_helper_args};
pub use linux::{
    BwrapBackend, LandlockRule, LinuxLandlockBackend, bwrap_probe, helper_status, landlock_rules,
    skipped_backends,
};
#[cfg(target_os = "linux")]
pub use linux::{deny_vsock, landlock_abi, restrict_current_process, run_egress_bridge};
pub use seatbelt::{SeatbeltBackend, seatbelt_profile};
pub use windows::WindowsJobBackend;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network at all (default).
    Deny,
    /// Only loopback (127.0.0.0/8, ::1) TCP/UDP; everything else, and AF_UNIX, is denied.
    /// Lets a dev server and its tests run inside one sandbox. Opt-in; the trade-off differs
    /// per backend (ask [`SandboxBackend::network_guarantee`]):
    /// - Seatbelt: the REAL host loopback. Sandboxed code can reach the user's local TCP
    ///   services (databases, Redis, docker TCP, dev servers). A bind to `0.0.0.0` is also
    ///   accepted by the kernel filter, so a listener there is reachable from the LAN.
    /// - bubblewrap: a fresh network namespace whose `lo` bwrap brings up; the loopback is
    ///   PRIVATE to the sandbox. Server and tests inside it work, host services are unreachable.
    /// - Landlock: unsupported (it can filter TCP ports, not addresses); `wrap` refuses.
    /// - Windows: no network confinement at all.
    Loopback,
    Allow,
    /// The only way out is the per-run egress proxy (`synthesis-egress`) at this endpoint; the
    /// host allowlist stays in the proxy, this crate only does the plumbing. Backends that
    /// cannot confine the process to that endpoint report `Guarantee::None` and `wrap` refuses
    /// (fail closed): Landlock, Windows and the null sandbox always; bubblewrap takes
    /// [`EgressEndpoint::Unix`], Seatbelt [`EgressEndpoint::Tcp`].
    Proxied(EgressEndpoint),
}

/// Where the egress proxy listens.
#[derive(Clone, PartialEq, Eq)]
pub enum EgressEndpoint {
    /// A unix socket in a private (0700) directory; bubblewrap binds the directory read-only
    /// into the sandbox and bridges `127.0.0.1:3128` inside it to the socket.
    Unix(PathBuf),
    /// `127.0.0.1:port` on the host with HTTP Basic credentials `synt:<token>` (Seatbelt).
    Tcp { port: u16, token: String },
}

// The token is a credential: never print it.
impl fmt::Debug for EgressEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(p) => f.debug_tuple("Unix").field(p).finish(),
            Self::Tcp { port, .. } => f
                .debug_struct("Tcp")
                .field("port", port)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// Port of the in-sandbox bridge to the proxy socket (bubblewrap).
pub const EGRESS_BRIDGE_PORT: u16 = 3128;

impl EgressEndpoint {
    /// Proxy URL for `HTTP_PROXY` and friends.
    fn proxy_url(&self) -> String {
        match self {
            Self::Unix(_) => format!("http://127.0.0.1:{EGRESS_BRIDGE_PORT}"),
            Self::Tcp { port, token } => format!("http://synt:{token}@127.0.0.1:{port}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    pub readable_roots: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    /// Always inaccessible, even inside a root (e.g. `~/.ssh`).
    pub deny_paths: Vec<PathBuf>,
    /// Readable but never writable/renamable/removable, even inside a writable root
    /// (e.g. `.git/hooks`, `.git/config`). Landlock cannot express this inside a writable root
    /// and rejects such a policy (`Unsupported`); Seatbelt and bubblewrap enforce it.
    pub readonly_paths: Vec<PathBuf>,
    /// Private temp dir for the command: the only temp location it may write (the real
    /// `/tmp`, `/var/folders`, `/run` are not granted). `TMPDIR` is forced to it. The caller
    /// creates it (0700). `None` = no temp location at all.
    pub tmp_dir: Option<PathBuf>,
    pub network: NetworkPolicy,
    /// false: only the initial program may run (no fork/exec of others) where enforceable.
    pub allow_exec: bool,
    /// Names copied from the current process env; everything else is dropped.
    pub env_passthrough: Vec<String>,
    /// Minimum guarantee the caller demands. The runtime refuses to run (instead of running
    /// weaker) when the backend's filesystem guarantee, or its
    /// [`network_guarantee`](SandboxBackend::network_guarantee) for `network` (`Deny` or
    /// `Loopback`), is below this. Defaults to `Weak`, so an unusable backend
    /// ([`NullSandbox`]) is refused; the terminal's `Enforce` mode never goes below `Weak`.
    pub required: Guarantee,
    /// WSL only: hide the Windows drives from READS too (a tmpfs over every drvfs mount; the
    /// workspace and `readable_roots` on a drive are bound back). Default true; the user turns it
    /// off with `synt.sandbox.wsl.hide_windows_drives` (user or managed settings).
    pub wsl_hide_drives: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            readable_roots: vec![],
            writable_roots: vec![],
            deny_paths: vec![],
            readonly_paths: vec![],
            tmp_dir: None,
            network: NetworkPolicy::Deny,
            allow_exec: true,
            env_passthrough: vec![],
            required: Guarantee::Weak,
            wsl_hide_drives: true,
        }
    }
}

impl SandboxPolicy {
    /// Recommended policy for an agent session: writes only in `workspace_root` and `tmp_dir`
    /// (private temp), network denied, hooks/config of the workspace's `.git` read-only, and
    /// credential stores plus control sockets (docker, ssh-agent, tmux, `$XDG_RUNTIME_DIR`)
    /// inaccessible. System dirs are added by the backends; add `readable_roots` for toolchains
    /// installed under `$HOME`. Denied paths are resolved from the process env at call time.
    ///
    /// `Weak` backends (Seatbelt, bwrap filesystem) are defence in depth, not a security
    /// boundary. Landlock rejects this policy (it cannot protect `.git/hooks` inside the
    /// writable workspace); use bubblewrap or Seatbelt, or drop `readonly_paths` knowingly.
    pub fn standard(workspace_root: &Path, tmp_dir: &Path) -> Self {
        let env_path = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        // POSIX socket paths are not absolute paths on Windows (validate would reject them).
        let mut deny = if cfg!(unix) {
            [
                "/var/run/docker.sock",
                "/run/docker.sock",
                "/var/run/podman",
                "/run/podman",
            ]
            .map(PathBuf::from)
            .to_vec()
        } else {
            vec![]
        };
        let home = env_path("HOME").or_else(|| env_path("USERPROFILE"));
        if let Some(home) = &home {
            deny.extend(credential_paths(
                home,
                env_path("APPDATA").as_deref(),
                env_path("LOCALAPPDATA").as_deref(),
            ));
        }
        // Socket directories: the whole runtime dir (docker, gpg-agent, ssh-agent live there)
        // and the parent dirs of the agent / tmux sockets named in the environment.
        deny.extend(env_path("XDG_RUNTIME_DIR"));
        deny.extend(env_path("TMUX_TMPDIR"));
        deny.extend(env_path("SSH_AUTH_SOCK").and_then(|p| p.parent().map(Path::to_path_buf)));
        let tmux = std::env::var("TMUX").ok();
        deny.extend(
            tmux.as_deref()
                .and_then(|t| t.split(',').next())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .and_then(|p| p.parent().map(Path::to_path_buf)),
        );
        // Never deny a root we are about to grant (misconfigured env); the grant would be moot.
        deny.retain(|d| !workspace_root.starts_with(d) && !tmp_dir.starts_with(d));
        deny.sort();
        deny.dedup();
        let reads = home.map_or(vec![], |h| {
            toolchain_read_roots(&h, &deny, std::env::var_os("PATH").as_deref())
        });
        Self {
            readable_roots: reads,
            writable_roots: vec![workspace_root.to_path_buf()],
            tmp_dir: Some(tmp_dir.to_path_buf()),
            deny_paths: deny,
            // The root is writable, so everything Synthesis or git later trusts as code or prompt must
            // be read-only: hooks/config (also of submodules), project settings (hooks, MCP,
            // agents, skills) and the instruction files loaded as trusted context.
            readonly_paths: [
                ".git/hooks",
                ".git/config",
                ".git/modules",
                ".gitmodules",
                ".synt",
                "AGENTS.md",
            ]
            .map(|p| workspace_root.join(p))
            .to_vec(),
            ..Self::default()
        }
    }

    /// Validate an extra read root (`--sandbox-read`) and return its canonical form. Rejects a
    /// missing path, a filesystem/drive root, `$HOME`/`%USERPROFILE%`, and any path that is, lies
    /// inside, or is an ancestor of a denied path (symlinks resolved first), since a read grant
    /// there would expose the credential stores in `deny_paths`.
    pub fn check_read_root(&self, path: &Path) -> Result<PathBuf, String> {
        let shown = path.display();
        let c = dunce::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
        if c.parent().is_none() {
            return Err(format!("{shown}: a filesystem root cannot be granted"));
        }
        for k in ["HOME", "USERPROFILE"] {
            let home = std::env::var_os(k).map(PathBuf::from);
            if home.is_some_and(|h| canon_lossy(&h) == c) {
                return Err(format!("{shown}: the home directory cannot be granted"));
            }
        }
        for d in &self.deny_paths {
            let d = canon_lossy(d);
            if d.starts_with(&c) || c.starts_with(&d) {
                return Err(format!(
                    "{shown}: overlaps protected path {} (credentials or control sockets)",
                    d.display()
                ));
            }
        }
        Ok(c)
    }

    /// Make `paths` read-only (never writable/renamable/removable), like `.git/hooks`. A path
    /// inside a writable root also gets its ancestors guarded by [`parent_guards`](Self::parent_guards).
    pub fn protect(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.readonly_paths.extend(paths);
    }

    /// Fail unless `path` (compared after symlink resolution) lies outside every writable root
    /// and the private tmp dir. For state the agent must never write by construction, e.g. the
    /// main repo's `.git` when the workspace is a worktree: a `--sandbox-write` of the repo
    /// would otherwise silently re-open it.
    pub fn ensure_unwritable(&self, path: &Path) -> Result<(), SandboxError> {
        let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let target = canon(path);
        match self
            .writable_roots
            .iter()
            .chain(&self.tmp_dir)
            .find(|r| target.starts_with(canon(r)))
        {
            Some(r) => Err(SandboxError::InvalidPolicy(format!(
                "{} must not be writable but lies inside writable root {}",
                path.display(),
                r.display()
            ))),
            None => Ok(()),
        }
    }

    /// Every path the policy names, for absolute-path validation.
    fn all_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.readable_roots
            .iter()
            .chain(&self.writable_roots)
            .chain(&self.deny_paths)
            .chain(&self.readonly_paths)
            .chain(&self.tmp_dir)
    }

    /// Ancestors of denied/read-only paths that lie strictly inside a writable root. A process
    /// that may rename or remove such a directory could move a protected path away (and plant
    /// its own), so backends must also forbid writes to these directory entries themselves.
    pub fn parent_guards(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = vec![];
        let protected = self.deny_paths.iter().chain(&self.readonly_paths);
        for d in protected {
            for a in d.ancestors().skip(1) {
                let inside = self
                    .writable_roots
                    .iter()
                    .chain(&self.tmp_dir)
                    .any(|r| a != r && a.starts_with(r));
                if inside && !out.iter().any(|o| o == a) {
                    out.push(a.to_path_buf());
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Guarantee {
    None,
    Weak,
    Strong,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub filesystem: Guarantee,
    pub network: Guarantee,
    /// Descendants stay confined / can be contained as a tree.
    pub process_tree: Guarantee,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxedCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    /// Output only: spawn with an empty environment plus `env`.
    pub clear_env: bool,
}

#[derive(Debug)]
pub enum SandboxError {
    InvalidPolicy(String),
    Unsupported(String),
    Unavailable(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy(m) => write!(f, "invalid sandbox policy: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported by this sandbox: {m}"),
            Self::Unavailable(m) => write!(f, "sandbox unavailable: {m}"),
        }
    }
}

impl std::error::Error for SandboxError {}

pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> SandboxCapabilities;
    /// What this backend guarantees for `mode`. `None` = cannot honour it (the runtime refuses
    /// under `Enforce`). `Allow` restricts nothing, so it is trivially `Strong`. The default
    /// knows `Deny` only (`capabilities().network`); backends that can do `Loopback` override.
    fn network_guarantee(&self, mode: &NetworkPolicy) -> Guarantee {
        network_guarantee_for(mode, self.capabilities().network, Guarantee::None)
    }
    fn wrap(
        &self,
        policy: &SandboxPolicy,
        command: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError>;
}

/// Per-mode guarantee from the backend's `Deny` and `Loopback` guarantees.
pub(crate) fn network_guarantee_for(
    mode: &NetworkPolicy,
    deny: Guarantee,
    loopback: Guarantee,
) -> Guarantee {
    match mode {
        NetworkPolicy::Allow => Guarantee::Strong,
        NetworkPolicy::Deny => deny,
        NetworkPolicy::Loopback => loopback,
        NetworkPolicy::Proxied(_) => Guarantee::None, // only backends that implement it say more
    }
}

/// No confinement at all; `wrap` returns the command unchanged (plus env filtering).
pub struct NullSandbox;

impl SandboxBackend for NullSandbox {
    fn name(&self) -> &'static str {
        "none"
    }
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::None,
            network: Guarantee::None,
            process_tree: Guarantee::None,
            notes: vec!["no sandbox: commands run with the agent's full privileges".into()],
        }
    }
    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        if matches!(policy.network, NetworkPolicy::Proxied(_)) {
            return Err(SandboxError::Unsupported(
                "no sandbox: a proxied network cannot be enforced".into(),
            ));
        }
        Ok(SandboxedCommand {
            env: build_env(policy, c),
            clear_env: true,
            ..c.clone()
        })
    }
}

/// Usable backends for this OS, probed at call time, best first (bubblewrap -> Landlock on
/// Linux). Empty = nothing can confine. `synthesis-terminal` degrades along this list in best-effort.
// Only the cfg'd extends/pushes inside differ per OS; on Windows exactly one push remains.
#[allow(clippy::vec_init_then_push)]
pub fn detect_chain() -> Vec<Box<dyn SandboxBackend>> {
    #[cfg(all(feature = "test-backend", debug_assertions))]
    if std::env::var_os("SYNT_TEST_FORCE_BACKEND").is_some_and(|v| v == "none") {
        return vec![];
    }
    let mut v: Vec<Box<dyn SandboxBackend>> = vec![];
    #[cfg(target_os = "macos")]
    v.extend(SeatbeltBackend::detect().map(|b| Box::new(b) as Box<dyn SandboxBackend>));
    #[cfg(target_os = "linux")]
    v.extend(linux::detect_linux());
    #[cfg(windows)]
    v.push(Box::new(WindowsJobBackend));
    v
}

/// Best available backend for this OS. **Fails open**: with nothing usable it returns
/// [`NullSandbox`] (`capabilities().filesystem == Guarantee::None`). Callers must check the
/// capabilities; `synthesis-terminal` refuses such a backend unless told otherwise.
pub fn detect() -> Box<dyn SandboxBackend> {
    detect_chain()
        .into_iter()
        .next()
        .unwrap_or_else(|| Box::new(NullSandbox))
}

/// Shared trust-boundary checks; every backend calls this first.
/// Canonicalise the longest existing prefix of `p` and re-append the missing tail, so denied
/// paths that do not exist yet still compare equal to their symlink-resolved spelling.
fn canon_lossy(p: &Path) -> PathBuf {
    let mut tail = vec![];
    let mut cur = p;
    loop {
        if let Ok(c) = dunce::canonicalize(cur) {
            return tail.iter().rev().fold(c, |a, t| a.join(t));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(par), Some(n)) => {
                tail.push(n);
                cur = par;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Synthesis's own state under `synthesis_home` that a sandboxed command must neither read nor write:
/// credentials, settings, the session database (prompts, tool output), the memory store and
/// `projects/` (transcripts, imported notes), `run/` (peer key, sockets, registry).
pub fn synthesis_state_paths(synthesis_home: &Path) -> Vec<PathBuf> {
    [
        "auth.json",
        "settings.json",
        "synt.db",
        "synt.db-wal",
        "synt.db-shm",
        "synt.db-journal",
        "memory",
        "projects",
        // peer endpoint sockets, `peer.key` and the session registry (messaging.md B8.4)
        "run",
        // saved browser logins (cookies of the person's sessions) and the login hand-off files
        "browser-profiles",
    ]
    .map(|n| synthesis_home.join(n))
    .to_vec()
}

/// Credential / secret stores under a user profile that the standard policy hides.
fn credential_paths(home: &Path, appdata: Option<&Path>, local: Option<&Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/gh",
        ".docker/run",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".config/git/credentials",
        ".kube",
        ".config/gcloud",
        ".azure",
        ".config/op",
        ".password-store",
        ".local/share/keyrings",
        ".mozilla",
        ".config/google-chrome",
        ".config/chromium",
        "Library/Application Support/Google/Chrome",
        "Library/Application Support/Chromium",
        "Library/Application Support/Firefox",
        "Library/Keychains",
        ".bash_history",
        ".zsh_history",
        ".cargo/credentials",
        ".cargo/credentials.toml",
        ".config/rclone",
        ".config/doppler",
        ".config/stripe",
        ".config/heroku",
        ".config/netrc",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect();
    v.extend(synthesis_state_paths(&home.join(".synt")));
    v.extend(
        appdata
            .into_iter()
            .flat_map(|a| ["gcloud", "Mozilla", "npm", "gh"].map(|d| a.join(d))),
    );
    v.extend(
        local
            .into_iter()
            .flat_map(|a| ["Google/Chrome", "Chromium", "Microsoft/Edge"].map(|d| a.join(d))),
    );
    v
}

/// Toolchain / config dirs under `$HOME` (relative) that the standard policy makes readable.
const TOOLCHAIN_DIRS: &[&str] = &[
    ".rustup", ".cargo", ".nvm", ".npm", ".pyenv", ".asdf", ".local", ".config", ".volta",
    ".rbenv", ".fnm", ".bun", ".deno", ".sdkman",
];

/// Existing toolchain dirs under `home` (plus Linuxbrew) as read roots. Landlock cannot deny a
/// path inside an allowed root, so a dir that contains a denied path is replaced by its
/// children minus the denied ones, recursively; `.env*`, `*.pem` and `*.key` entries are
/// dropped. shortcut: dirs created later under an expanded dir are not covered, restart to refresh.
fn toolchain_read_roots(
    home: &Path,
    deny: &[PathBuf],
    path_var: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    fn add(dir: PathBuf, deny: &[PathBuf], out: &mut Vec<PathBuf>) {
        let real = canon_lossy(&dir);
        if deny.iter().any(|d| real.starts_with(canon_lossy(d))) {
            return;
        }
        if !deny.iter().any(|d| canon_lossy(d).starts_with(&real)) {
            return out.push(dir);
        }
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if !(n.starts_with(".env") || n.ends_with(".pem") || n.ends_with(".key")) {
                add(e.path(), deny, out);
            }
        }
    }
    let mut out = vec![];
    let dirs = TOOLCHAIN_DIRS.iter().map(|d| home.join(d));
    let fixed = dirs.chain(cfg!(target_os = "linux").then(|| "/home/linuxbrew/.linuxbrew".into()));
    for d in fixed.chain(path_install_prefixes(home, path_var)) {
        if d.is_dir() && !out.iter().any(|o| d.starts_with(o)) {
            add(d, deny, &mut out);
        }
    }
    out
}

/// Install prefixes under `home` of the toolchains on `PATH` (CI runners keep node under
/// `~/hostedtoolcache/node/<v>/<arch>`): per PATH entry, its parent when it is named `bin`
/// (else the entry); plus the same for the real path of well-known tools. Never `home` itself.
fn path_install_prefixes(home: &Path, path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    const TOOLS: &[&str] = &[
        "node", "npm", "npx", "cargo", "rustc", "python3", "pip", "go", "ruby", "java",
    ];
    let home = canon_lossy(home);
    let entries: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    let prefix = |bin_dir: &Path| {
        let up = bin_dir.parent().filter(|u| *u != home);
        let p = match bin_dir.file_name() {
            Some(n) if n == "bin" => up.unwrap_or(bin_dir),
            _ => bin_dir,
        };
        (p != home && p.starts_with(&home)).then(|| p.to_path_buf())
    };
    let mut out: Vec<PathBuf> = vec![];
    let mut push = |p: Option<PathBuf>| {
        if let Some(p) = p.filter(|p| !out.contains(p)) {
            out.push(p);
        }
    };
    for e in entries.iter().filter(|e| e.is_absolute()) {
        push(dunce::canonicalize(e).ok().and_then(|c| prefix(&c)));
        for t in TOOLS {
            let real = dunce::canonicalize(e.join(t)).ok();
            push(real.and_then(|r| prefix(r.parent()?)));
        }
    }
    out
}

pub(crate) fn validate(p: &SandboxPolicy) -> Result<(), SandboxError> {
    if let NetworkPolicy::Proxied(EgressEndpoint::Unix(sock)) = &p.network
        && !sock.is_absolute()
    {
        return Err(SandboxError::InvalidPolicy(format!(
            "path must be absolute: {sock:?}"
        )));
    }
    for path in p.all_paths() {
        if !path.is_absolute() {
            return Err(SandboxError::InvalidPolicy(format!(
                "path must be absolute: {path:?}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn build_env(p: &SandboxPolicy, c: &SandboxedCommand) -> Vec<(OsString, OsString)> {
    build_env_on(p, c, wsl::host().as_ref())
}

pub(crate) fn build_env_on(
    p: &SandboxPolicy,
    c: &SandboxedCommand,
    wsl: Option<&synthesis_platform::wsl::WslHost>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = p
        .env_passthrough
        .iter()
        .filter_map(|n| std::env::var_os(n).map(|v| (OsString::from(n), v)))
        .collect();
    env.extend(c.env.iter().cloned());
    if let Some(t) = &p.tmp_dir {
        env.retain(|(k, _)| k != "TMPDIR");
        env.push(("TMPDIR".into(), t.clone().into_os_string())); // forced: private tmp only
    }
    if let NetworkPolicy::Proxied(ep) = &p.network {
        // Every inherited proxy setting goes (a host `HTTPS_PROXY` would bypass ours or leak its
        // credentials), then ours is the only one.
        env.retain(|(k, _)| !k.to_string_lossy().to_ascii_lowercase().ends_with("_proxy"));
        let url = ep.proxy_url();
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.push((k.into(), url.clone().into()));
        }
        env.push(("NO_PROXY".into(), "localhost,127.0.0.1".into()));
        env.push(("no_proxy".into(), "localhost,127.0.0.1".into()));
        env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
    }
    if let Some(host) = wsl {
        wsl::scrub_env(&mut env, host);
    }
    env
}

/// Resolve to an absolute path using the command's PATH, else the process PATH.
pub(crate) fn resolve_program(c: &SandboxedCommand) -> Option<PathBuf> {
    let prog = Path::new(&c.program);
    if prog.is_absolute() {
        return Some(prog.to_path_buf());
    }
    let path = c
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
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
    fn peer_run_dir_is_synthesis_state() {
        let home = p("/h/.synt");
        assert!(synthesis_state_paths(&home).contains(&home.join("run")));
        // the trust key lives under run/, the remembered-trust rows in the database and its journal
        assert!(synthesis_state_paths(&home).contains(&home.join("synt.db-journal")));
        assert!(home.join("run/trust.key").starts_with(home.join("run")));
        assert!(synthesis_state_paths(&home).contains(&home.join("browser-profiles")));
    }

    #[test]
    fn rejects_relative_paths_and_a_relative_proxy_socket() {
        let mut p = SandboxPolicy {
            readable_roots: vec!["rel".into()],
            ..Default::default()
        };
        assert!(matches!(validate(&p), Err(SandboxError::InvalidPolicy(_))));
        p.readable_roots.clear();
        p.network = NetworkPolicy::Proxied(EgressEndpoint::Unix("rel.sock".into()));
        assert!(matches!(validate(&p), Err(SandboxError::InvalidPolicy(_))));
    }

    #[test]
    fn proxied_env_replaces_every_inherited_proxy_variable() {
        let pol = SandboxPolicy {
            network: NetworkPolicy::Proxied(EgressEndpoint::Tcp {
                port: 4321,
                token: "s3cr3t".into(),
            }),
            env_passthrough: vec!["Https_Proxy".into(), "FTP_PROXY".into(), "KEEP".into()],
            ..Default::default()
        };
        let c = SandboxedCommand {
            env: vec![
                ("http_proxy".into(), "http://evil".into()),
                ("NODE_USE_ENV_PROXY".into(), "0".into()),
            ],
            ..Default::default()
        };
        let env = build_env_on(&pol, &c, None);
        let get = |k: &str| {
            let v: Vec<_> = env.iter().filter(|(n, _)| n == k).collect();
            assert!(v.len() <= 1, "{k} set twice");
            v.first().map(|(_, v)| v.to_string_lossy().into_owned())
        };
        let url = "http://synt:s3cr3t@127.0.0.1:4321";
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(get(k).as_deref(), Some(url), "{k}");
        }
        assert_eq!(get("NO_PROXY").as_deref(), Some("localhost,127.0.0.1"));
        assert_eq!(get("NODE_USE_ENV_PROXY").as_deref(), Some("1"));
        assert_eq!(get("FTP_PROXY"), None);
        assert_eq!(get("Https_Proxy"), None);
        let unix = EgressEndpoint::Unix(p("/run/e.sock"));
        assert_eq!(unix.proxy_url(), "http://127.0.0.1:3128");
        assert!(!format!("{:?}", pol.network).contains("s3cr3t"));
    }

    #[test]
    fn only_bubblewrap_and_seatbelt_can_proxy() {
        let proxied = NetworkPolicy::Proxied(EgressEndpoint::Unix(p("/r/e.sock")));
        assert_eq!(NullSandbox.network_guarantee(&proxied), Guarantee::None);
        assert!(matches!(
            NullSandbox.wrap(
                &SandboxPolicy {
                    network: proxied,
                    ..Default::default()
                },
                &SandboxedCommand::default()
            ),
            Err(SandboxError::Unsupported(_))
        ));
        assert_eq!(
            WindowsJobBackend.network_guarantee(&NetworkPolicy::Proxied(EgressEndpoint::Tcp {
                port: 1,
                token: String::new()
            })),
            Guarantee::None
        );
    }

    #[test]
    fn default_requires_weak() {
        assert_eq!(SandboxPolicy::default().required, Guarantee::Weak);
    }

    #[test]
    fn standard_policy_shape() {
        let ws = p("/ws");
        let pol = SandboxPolicy::standard(&ws, &p("/ws-tmp"));
        assert_eq!(pol.network, NetworkPolicy::Deny);
        assert_eq!(pol.required, Guarantee::Weak);
        assert_eq!(pol.tmp_dir.as_deref(), Some(p("/ws-tmp").as_path()));
        assert!(pol.readonly_paths.contains(&ws.join(".git/hooks")));
        assert!(pol.readonly_paths.contains(&ws.join(".git/config")));
        for p in [".git/modules", ".gitmodules", ".synt", "AGENTS.md"] {
            assert!(pol.readonly_paths.contains(&ws.join(p)), "{p}");
        }
        assert_eq!(
            pol.deny_paths.contains(&"/run/docker.sock".into()),
            cfg!(unix)
        );
        validate(&pol).unwrap();
        // .git (ancestor of both read-only paths) is guarded against rename/removal; /ws is not.
        assert_eq!(pol.parent_guards(), vec![ws.join(".git")]);
    }

    #[test]
    fn credential_paths_cover_common_stores() {
        let v = credential_paths(&p("/h"), Some(&p("/h/AD")), None);
        for d in [
            ".netrc",
            ".npmrc",
            ".git-credentials",
            ".kube",
            ".config/gcloud",
            ".azure",
            ".synt/auth.json",
            ".synt/synt.db",
            ".synt/synt.db-wal",
            ".synt/memory",
            ".zsh_history",
            ".mozilla",
        ] {
            assert!(v.contains(&p("/h").join(d)), "{d}");
        }
        assert!(v.contains(&p("/h/AD").join("gcloud")));
    }

    #[cfg(unix)]
    #[test]
    fn toolchain_roots_exclude_secrets() {
        let td = tempfile::tempdir().unwrap();
        let h = dunce::canonicalize(td.path()).unwrap();
        for d in [
            ".rustup/toolchains",
            ".cargo/registry",
            ".config/gh",
            ".config/nvim",
            ".ssh",
        ] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        for f in [
            ".cargo/credentials.toml",
            ".cargo/config.toml",
            ".config/x.pem",
            ".config/.env",
        ] {
            std::fs::write(h.join(f), "x").unwrap();
        }
        let deny = credential_paths(&h, None, None);
        let r = toolchain_read_roots(&h, &deny, std::env::var_os("PATH").as_deref());
        let ok = |p: &str| r.iter().any(|x| h.join(p).starts_with(x));
        for yes in [
            ".rustup/toolchains",
            ".cargo/registry",
            ".cargo/config.toml",
            ".config/nvim",
        ] {
            assert!(ok(yes), "{yes}");
        }
        for no in [
            ".ssh",
            ".config/gh",
            ".cargo/credentials.toml",
            ".config/x.pem",
            ".config/.env",
        ] {
            assert!(!ok(no), "{no}");
        }
        assert!(
            r.iter()
                .all(|x| !deny.iter().any(|d| d.starts_with(x) || x.starts_with(d)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_install_prefixes_cover_runner_style_node_and_never_home() {
        let td = tempfile::tempdir().unwrap();
        let h = dunce::canonicalize(td.path()).unwrap();
        let root = h.join("hostedtoolcache/node/22.1.0/arm64");
        let npm_dir = root.join("lib/node_modules/npm/bin");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(&npm_dir).unwrap();
        std::fs::create_dir_all(h.join(".ssh")).unwrap();
        std::fs::write(root.join("bin/node"), "").unwrap();
        std::fs::write(npm_dir.join("npm-cli.js"), "").unwrap();
        std::fs::write(root.join("lib/node_modules/npm/.env"), "S=1").unwrap();
        std::os::unix::fs::symlink(npm_dir.join("npm-cli.js"), root.join("bin/npm")).unwrap();
        std::fs::create_dir_all(h.join("bin")).unwrap(); // ~/bin: its parent is $HOME
        let path =
            std::env::join_paths([root.join("bin"), h.join("bin"), "/usr/bin".into()]).unwrap();
        let pre = path_install_prefixes(&h, Some(&path));
        assert!(pre.contains(&root), "{pre:?}");
        assert!(!pre.contains(&h) && pre.contains(&h.join("bin")), "{pre:?}");
        let deny = credential_paths(&h, None, None);
        let r = toolchain_read_roots(&h, &deny, Some(&path));
        let ok = |p: &Path| r.iter().any(|x| p.starts_with(x));
        assert!(ok(&npm_dir.join("npm-cli.js")) && ok(&root.join("bin/node")));
        assert!(!ok(&h.join(".ssh/id_rsa")) && !r.contains(&h));
    }

    #[test]
    fn check_read_root_rejects_ancestors_and_symlinks() {
        let td = tempfile::tempdir().unwrap();
        let t = td.path().to_path_buf();
        let (ok, secret) = (t.join("tools"), t.join("home/.ssh"));
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        let p = SandboxPolicy {
            deny_paths: vec![secret.clone(), t.join("home/.kube")],
            ..Default::default()
        };
        assert!(p.check_read_root(&ok).is_ok());
        for bad in [
            &t.join("home"),
            &secret,
            &secret.join("..").join(".ssh"),
            Path::new("/"),
            &t.join("missing"),
        ] {
            assert!(p.check_read_root(bad).is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        {
            let link = t.join("link");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            assert!(p.check_read_root(&link).is_err());
        }
    }

    #[test]
    fn protect_and_ensure_unwritable() {
        let mut pol = SandboxPolicy::standard(&p("/ws"), &p("/ws-tmp"));
        pol.protect([p("/ws/.git")]);
        assert!(pol.readonly_paths.contains(&p("/ws/.git")));
        pol.ensure_unwritable(&p("/main/.git/worktrees/x")).unwrap();
        for bad in ["/ws", "/ws/sub/.git", "/ws-tmp/a"] {
            assert!(
                matches!(
                    pol.ensure_unwritable(&p(bad)),
                    Err(SandboxError::InvalidPolicy(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn tmpdir_is_forced() {
        let pol = SandboxPolicy {
            tmp_dir: Some(p("/priv")),
            ..Default::default()
        };
        let c = SandboxedCommand {
            env: vec![("TMPDIR".into(), "/tmp".into())],
            ..Default::default()
        };
        let env = build_env(&pol, &c);
        assert_eq!(
            env.iter()
                .filter(|(k, _)| k == "TMPDIR")
                .collect::<Vec<_>>(),
            [&("TMPDIR".into(), p("/priv").into_os_string())]
        );
    }

    #[test]
    fn null_sandbox_reports_none_and_filters_env() {
        let p = SandboxPolicy {
            env_passthrough: vec!["SYNT_NOPE_UNSET".into()],
            ..Default::default()
        };
        let c = SandboxedCommand {
            program: "x".into(),
            ..Default::default()
        };
        let w = NullSandbox.wrap(&p, &c).unwrap();
        assert!(w.clear_env && w.env.is_empty());
        assert_eq!(NullSandbox.capabilities().filesystem, Guarantee::None);
    }

    #[test]
    fn loopback_unsupported_unless_backend_overrides() {
        assert_eq!(
            NullSandbox.network_guarantee(&NetworkPolicy::Loopback),
            Guarantee::None
        );
        assert_eq!(
            NullSandbox.network_guarantee(&NetworkPolicy::Allow),
            Guarantee::Strong
        );
        assert_eq!(
            NullSandbox.network_guarantee(&NetworkPolicy::Deny),
            Guarantee::None
        );
    }
}
