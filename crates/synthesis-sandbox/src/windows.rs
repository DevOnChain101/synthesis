//! Windows: an AppContainer backend (`AppContainerBackend`, filesystem and network `Weak`) behind
//! the `synthesis-sandbox-exec` helper, and `WindowsJobBackend`, the "nothing can confine"
//! report used when the helper or AppContainer is unavailable.
//!
//! ADR-010 exception (owner decision 2026-11-15, docs/specs/sandbox-hardening.md section 5): the
//! Win32 calls live in `synthesis-sandbox-win`, the only crate allowed `unsafe`. This module is
//! safe code: it plans the grants (pure, unit-tested on every OS), quotes the command line and
//! builds the helper's argv. Nothing here touches an ACL: `wrap` is pure (it is also called for
//! the preflight), the helper edits and restores ACLs around each run.
//!
//! Not run on a real Windows machine by the author: CI `windows-latest` only, so every guarantee
//! stays `Weak` and this file says what each mechanism does NOT do.

use std::path::{Path, PathBuf};

use synthesis_sandbox_win::{Access, Grant, Spec, VERSION};

use crate::*;

/// Command lines are limited to 32767 UTF-16 units; the policy travels in the helper's argv.
const MAX_ARGV_CHARS: usize = 30_000;

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\s\x` -> `\\s\x`; anything else unchanged (a `\\.\` device
/// path stays as it is, so `is_local_drive_path` refuses it).
fn simplify(s: &str) -> String {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        r.to_string()
    } else {
        s.to_string()
    }
}

/// Comparison key: simplified, `/` as `\`, lower case (NTFS is case-insensitive), no trailing `\`
/// (except a bare drive root `c:\`).
fn fold(s: &str) -> String {
    let mut k = simplify(s).replace('/', "\\").to_lowercase();
    while k.len() > 3 && k.ends_with('\\') {
        k.pop();
    }
    k
}

/// `X:\...` only: UNC shares, device paths and drive-relative paths are not grantable.
fn is_local_drive_path(k: &str) -> bool {
    let b = k.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

fn is_drive_root(k: &str) -> bool {
    is_local_drive_path(k) && k.len() == 3
}

/// `child` equals `parent` or lies below it (component boundary), both already folded.
fn is_under(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent.trim_end_matches('\\'))
            .is_some_and(|r| r.starts_with('\\'))
}

/// MSVCRT `CommandLineToArgvW`-compatible quoting of one argument.
pub(crate) fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2)); // before the closing quote
    out.push('"');
    out
}

fn add(out: &mut Vec<Grant>, path: PathBuf, access: Access) {
    let g = Grant { path, access };
    if !out.contains(&g) {
        out.push(g);
    }
}

/// What the helper grants the container, from the policy. Order matters (the helper applies it
/// in order and removes it in reverse): reads, then writes, then denies, so a deny is on top.
///
/// - `readable_roots` that exist: read and execute.
/// - `writable_roots` and `tmp_dir`: modify (read, write, delete; not change permissions).
/// - `readonly_paths` that exist and lie in a writable root: write/delete/rename denied.
/// - `parent_guards` that exist: `FILE_DELETE_CHILD` denied on the directory, so its children
///   (`.git/hooks`, `.git/config`) cannot be renamed away. The guard directory ITSELF can still
///   be renamed through its parent (the parent's delete-child right is what allows it and the
///   workspace root must keep it): a documented residual.
/// - `deny_paths` that exist and lie inside a granted root: everything denied. Outside every
///   granted root a path is unreachable anyway (an AppContainer has no ACE there).
///
/// A path that does not exist cannot carry an ACE: a protected path that is created later is NOT
/// protected (same limit as bubblewrap's nonexistent `deny_paths`).
pub(crate) fn plan_grants(p: &SandboxPolicy) -> Result<Vec<Grant>, SandboxError> {
    let bad = |m: String| SandboxError::InvalidPolicy(m);
    let norm =
        |path: &Path| -> PathBuf { PathBuf::from(simplify(&canon_lossy(path).to_string_lossy())) };
    let check = |path: &Path, what: &str| -> Result<PathBuf, SandboxError> {
        let n = norm(path);
        let k = fold(&n.to_string_lossy());
        if !is_local_drive_path(&k) {
            return Err(bad(format!(
                "{what} {}: only local drive paths (C:\\...) can be granted, not UNC shares or device paths",
                path.display()
            )));
        }
        if is_drive_root(&k) {
            return Err(bad(format!(
                "{what} {}: a drive root cannot be granted",
                path.display()
            )));
        }
        Ok(n)
    };
    let mut out: Vec<Grant> = vec![];
    let mut writable = vec![];
    for r in p.readable_roots.iter().filter(|r| r.exists()) {
        add(&mut out, check(r, "readable root")?, Access::Read);
    }
    for w in p.writable_roots.iter().chain(&p.tmp_dir) {
        let n = check(w, "writable root")?;
        writable.push(fold(&n.to_string_lossy()));
        add(&mut out, n, Access::Modify);
    }
    let granted: Vec<String> = out
        .iter()
        .map(|g| fold(&g.path.to_string_lossy()))
        .collect();
    let in_writable = |k: &str| writable.iter().any(|w| is_under(k, w));
    let in_granted = |k: &str| granted.iter().any(|g| is_under(k, g));
    for r in p.readonly_paths.iter().filter(|r| r.exists()) {
        let n = norm(r);
        if in_writable(&fold(&n.to_string_lossy())) {
            add(&mut out, n, Access::DenyWrite);
        }
    }
    for g in p.parent_guards().iter().filter(|g| g.exists()) {
        add(&mut out, norm(g), Access::DenyDeleteChild);
    }
    for d in p.deny_paths.iter().filter(|d| d.exists()) {
        let n = norm(d);
        if in_granted(&fold(&n.to_string_lossy())) {
            add(&mut out, n, Access::DenyAll);
        }
    }
    Ok(out)
}

fn exe_name() -> String {
    format!("{HELPER_NAME}.exe")
}

/// The helper in the directory of `exe`: a regular file, not a symlink.
fn helper_in(exe: &Path) -> Result<PathBuf, String> {
    let h = exe
        .parent()
        .ok_or("the executable has no directory")?
        .join(exe_name());
    let m = std::fs::symlink_metadata(&h).map_err(|_| {
        format!(
            "{} is missing next to {}; reinstall synt (the Windows archive ships it; `cargo build -p synthesis-sandbox` builds it)",
            exe_name(),
            exe.display()
        )
    })?;
    if m.file_type().is_symlink() || !m.is_file() {
        return Err(format!("{} is not a regular file", exe_name()));
    }
    Ok(h)
}

static PROBE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();

/// Cached per process: the trusted helper next to the running executable and a real AppContainer
/// probe (a profile is created and deleted), or the reason it cannot be used. Windows only.
pub fn appcontainer_probe() -> &'static Result<PathBuf, String> {
    PROBE.get_or_init(|| {
        if !cfg!(windows) {
            return Err("AppContainer is Windows-only".into());
        }
        let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
        let h = helper_in(&exe)?;
        synthesis_sandbox_win::probe()
            .map_err(|e| format!("AppContainer is not usable here: {e}"))?;
        Ok(h)
    })
}

/// Run by `synthesis-sandbox-exec --appcontainer <json>`: decode the spec, run it, return the
/// exit code. Strict: unknown fields, a wrong version or a relative path are errors.
pub fn run_appcontainer(json: &str) -> Result<u32, String> {
    let spec: Spec = serde_json::from_str(json).map_err(|e| format!("bad spec: {e}"))?;
    if spec.version != VERSION {
        return Err(format!("unsupported spec version {}", spec.version));
    }
    if !spec.program.is_absolute() || spec.grants.iter().any(|g| !g.path.is_absolute()) {
        return Err("bad spec: paths must be absolute".into());
    }
    synthesis_sandbox_win::run(&spec)
}

/// Windows AppContainer behind the helper. See the spec (section 5) for what it does and does
/// not enforce: filesystem allow-list by ACL, no network, a Job Object. `Weak` on purpose.
pub struct AppContainerBackend {
    helper: PathBuf,
}

impl AppContainerBackend {
    /// Tests and embedders inject a helper; [`Self::detect`] finds the real one.
    pub fn new(helper: PathBuf) -> Self {
        Self { helper }
    }

    pub fn detect() -> Result<Self, String> {
        appcontainer_probe().clone().map(Self::new)
    }

    /// `PATH` lookup that also tries `.exe` and `.com` (batch files must go through `cmd.exe`).
    fn resolve(c: &SandboxedCommand) -> Option<PathBuf> {
        let prog = Path::new(&c.program);
        if prog.is_absolute() {
            return Some(prog.to_path_buf());
        }
        let path = c
            .env
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var_os("PATH"))?;
        std::env::split_paths(&path).find_map(|d| {
            ["", ".exe", ".com"].iter().find_map(|ext| {
                let mut f = d.join(prog).into_os_string();
                f.push(ext);
                let f = PathBuf::from(f);
                f.is_file().then_some(f)
            })
        })
    }
}

impl SandboxBackend for AppContainerBackend {
    fn name(&self) -> &'static str {
        "appcontainer"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::Weak,
            network: Guarantee::Weak,
            process_tree: Guarantee::Weak,
            notes: vec![
                "AppContainer (low-box token): files are reachable only where the policy grants them (explicit ACEs for a per-run container SID, removed at exit) plus the system locations every container may read (Windows, Program Files); `deny_paths` and `readonly_paths` inside a granted root are explicit deny ACEs".into(),
                "network: no capabilities, so no network at all, loopback included; `--network loopback` and `hosts` are refused (a loopback exemption needs administrator rights)".into(),
                "Job Object: kill on close, memory and process-count limits, no clipboard/desktop/handle UI access; only the three standard handles are inherited".into(),
                "NOT enforced: protection of a guarded directory itself against rename (only its children), rlimits other than memory/process count, files under a path that did not exist at start; a launcher killed outright leaves its ACEs (of a dead SID) behind".into(),
                "never run on a real Windows machine by the authors, CI only: Weak, not a security boundary".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        let network = match policy.network {
            NetworkPolicy::Deny => false,
            NetworkPolicy::Allow => true,
            _ => {
                return Err(SandboxError::Unsupported(
                    "appcontainer: only network deny and allow (no loopback exemption without administrator rights, no proxy route)".into(),
                ));
            }
        };
        crate::linux::helper_outside_writable(&self.helper, policy)?;
        let program = if c.program.is_empty() {
            PathBuf::new() // the preflight wraps an empty command; it is never run
        } else {
            Self::resolve(c).ok_or_else(|| {
                SandboxError::Unavailable(format!("cannot find {:?} on PATH", c.program))
            })?
        };
        let mut line = quote_arg(&program.to_string_lossy());
        for a in &c.args {
            let a = a.to_str().ok_or_else(|| {
                SandboxError::Unsupported("appcontainer: arguments must be UTF-8".into())
            })?;
            line.push(' ');
            line.push_str(&quote_arg(a));
        }
        let spec = Spec {
            version: VERSION,
            program,
            command_line: line,
            grants: plan_grants(policy)?,
            network,
            memory: policy.limits.memory,
            tasks: if policy.allow_exec {
                policy
                    .limits
                    .tasks
                    .map(|t| t.min(u64::from(u32::MAX)) as u32)
            } else {
                Some(1)
            },
        };
        let json =
            serde_json::to_string(&spec).map_err(|e| SandboxError::InvalidPolicy(e.to_string()))?;
        if json.len() > MAX_ARGV_CHARS {
            return Err(SandboxError::Unsupported(
                "appcontainer: the policy is too large for a command line".into(),
            ));
        }
        let mut env = build_env(policy, c);
        if let Some(t) = &policy.tmp_dir {
            // Windows programs read TEMP/TMP, not TMPDIR: forced to the private dir
            env.retain(|(k, _)| !k.eq_ignore_ascii_case("TEMP") && !k.eq_ignore_ascii_case("TMP"));
            env.push(("TEMP".into(), t.clone().into_os_string()));
            env.push(("TMP".into(), t.clone().into_os_string()));
        }
        Ok(SandboxedCommand {
            program: self.helper.clone().into_os_string(),
            args: vec!["--appcontainer".into(), json.into()],
            env,
            cwd: c.cwd.clone(),
            clear_env: true,
        })
    }
}

pub struct WindowsJobBackend;

impl SandboxBackend for WindowsJobBackend {
    fn name(&self) -> &'static str {
        "windows-job"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::None,
            network: Guarantee::None,
            process_tree: Guarantee::Weak,
            notes: vec![
                "no filesystem/network confinement (AppContainer/restricted token not implemented)".into(),
                "tree kill comes from the terminal runtime's Job Object; memory and process-count limits are not implemented".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        NullSandbox.wrap(policy, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_no_fs_or_net() {
        let c = WindowsJobBackend.capabilities();
        assert_eq!(
            (c.filesystem, c.network),
            (Guarantee::None, Guarantee::None)
        );
    }

    #[test]
    fn simplify_and_fold_paths() {
        assert_eq!(simplify(r"\\?\C:\Work\x"), r"C:\Work\x");
        assert_eq!(simplify(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(simplify(r"\\.\PhysicalDrive0"), r"\\.\PhysicalDrive0");
        assert_eq!(simplify(r"D:\plain"), r"D:\plain");
        // case, separators and trailing slashes do not matter; a bare root keeps its slash
        assert_eq!(fold(r"\\?\C:/Work\Sub\"), r"c:\work\sub");
        assert_eq!(fold(r"C:\"), r"c:\");
        assert_eq!(fold("c:/"), r"c:\");
    }

    #[test]
    fn only_local_drive_paths_are_grantable() {
        for ok in [r"c:\a", r"z:\a\b"] {
            assert!(is_local_drive_path(ok), "{ok}");
        }
        for bad in [
            r"\\srv\share\a",
            r"\\.\pipe\x",
            "c:a",
            "a",
            "",
            r"/a",
            r"c:",
        ] {
            assert!(!is_local_drive_path(bad), "{bad}");
        }
        assert!(is_drive_root(r"c:\") && !is_drive_root(r"c:\a"));
    }

    #[test]
    fn is_under_respects_component_boundaries_and_case_via_fold() {
        let (w, o) = (fold(r"C:\Work"), fold(r"c:\WORKSHOP"));
        assert!(is_under(&fold(r"c:\work\a\b"), &w) && is_under(&w, &w));
        assert!(!is_under(&o, &w), "workshop is not inside work");
        assert!(
            is_under(&fold(r"C:\x"), &fold("c:/")),
            "everything is under a drive root"
        );
    }

    #[test]
    fn quoting_follows_the_msvcrt_rules() {
        for (a, want) in [
            ("plain", "plain"),
            ("", r#""""#),
            ("a b", r#""a b""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"C:\dir with\", r#""C:\dir with\\""#),
            (r#"a\"b"#, r#""a\\\"b""#),
            (r"back\slash", r"back\slash"),
        ] {
            assert_eq!(quote_arg(a), want, "{a}");
        }
    }

    // Real Windows paths only exist on Windows; elsewhere the pure parts above carry the tests.
    #[cfg(windows)]
    mod plan {
        use super::*;

        fn pol(root: &Path) -> SandboxPolicy {
            let (w, r) = (root.join("ws"), root.join("tools"));
            std::fs::create_dir_all(w.join(".git/hooks")).unwrap();
            std::fs::create_dir_all(w.join("secret")).unwrap();
            std::fs::create_dir_all(r.join("bin")).unwrap();
            std::fs::create_dir_all(root.join("home/.ssh")).unwrap();
            SandboxPolicy {
                readable_roots: vec![r, root.join("missing")],
                writable_roots: vec![w.clone()],
                tmp_dir: Some(root.join("ws-tmp")),
                readonly_paths: vec![w.join(".git/hooks"), w.join(".nope")],
                deny_paths: vec![w.join("secret"), root.join("home/.ssh")],
                ..Default::default()
            }
        }

        #[test]
        fn grants_are_ordered_and_scoped() {
            let td = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(td.path()).unwrap();
            let p = pol(&root);
            let g = plan_grants(&p).unwrap();
            let of = |a: Access| -> Vec<&Path> {
                g.iter()
                    .filter(|x| x.access == a)
                    .map(|x| x.path.as_path())
                    .collect()
            };
            assert_eq!(
                of(Access::Read),
                [root.join("tools").as_path()],
                "a missing read root is skipped"
            );
            assert_eq!(
                of(Access::Modify),
                [root.join("ws").as_path(), root.join("ws-tmp").as_path()]
            );
            assert_eq!(
                of(Access::DenyWrite),
                [root.join("ws/.git/hooks").as_path()]
            );
            assert_eq!(
                of(Access::DenyDeleteChild),
                [root.join("ws/.git").as_path()]
            );
            // only the deny inside a granted root needs an ACE; ~/.ssh is unreachable anyway
            assert_eq!(of(Access::DenyAll), [root.join("ws/secret").as_path()]);
            // reads and writes come first, denies last
            let first_deny = g
                .iter()
                .position(|x| !matches!(x.access, Access::Read | Access::Modify))
                .unwrap();
            assert!(
                g[..first_deny]
                    .iter()
                    .all(|x| matches!(x.access, Access::Read | Access::Modify))
            );
        }

        #[test]
        fn drive_roots_and_unc_are_refused() {
            let mut p = SandboxPolicy {
                writable_roots: vec![PathBuf::from(r"C:\")],
                ..Default::default()
            };
            assert!(matches!(
                plan_grants(&p),
                Err(SandboxError::InvalidPolicy(_))
            ));
            p.writable_roots = vec![PathBuf::from(r"\\server\share\x")];
            assert!(matches!(
                plan_grants(&p),
                Err(SandboxError::InvalidPolicy(_))
            ));
        }
    }

    #[test]
    fn appcontainer_wrap_shapes_the_helper_command() {
        let td = tempfile::tempdir().unwrap();
        let p = |s: &str| td.path().join(s);
        let b = AppContainerBackend::new(p("bin").join("helper.exe"));
        let pol = SandboxPolicy {
            writable_roots: vec![p("ws")],
            tmp_dir: Some(p("tmp")),
            ..Default::default()
        };
        // a relative or unresolvable program is Unavailable, never run unconfined
        let c = SandboxedCommand {
            program: if cfg!(windows) {
                r"C:\Windows\System32\cmd.exe"
            } else {
                "/bin/sh"
            }
            .into(),
            args: vec!["/c".into(), "echo hi".into()],
            cwd: Some(p("ws")),
            ..Default::default()
        };
        if cfg!(windows) {
            let w = b.wrap(&pol, &c).unwrap();
            assert_eq!(w.program, b.helper);
            assert_eq!(w.args[0], "--appcontainer");
            assert!(w.clear_env && w.cwd == c.cwd);
            let spec: Spec = serde_json::from_str(w.args[1].to_str().unwrap()).unwrap();
            assert_eq!(
                spec.command_line,
                r#"C:\Windows\System32\cmd.exe /c "echo hi""#
            );
            assert!(!spec.network);
            assert_eq!(spec.memory, pol.limits.memory);
            for k in ["TEMP", "TMP"] {
                assert!(
                    w.env
                        .iter()
                        .any(|(n, v)| n == k && *v == p("tmp").into_os_string()),
                    "{k}"
                );
            }
        }
        // refusals that hold on every OS
        for net in [
            NetworkPolicy::Loopback,
            NetworkPolicy::Proxied(EgressEndpoint::Tcp {
                port: 1,
                token: String::new(),
            }),
        ] {
            let bad = SandboxPolicy {
                network: net,
                ..pol.clone()
            };
            assert!(matches!(
                b.wrap(&bad, &c),
                Err(SandboxError::Unsupported(_))
            ));
            assert_eq!(b.network_guarantee(&bad.network), Guarantee::None);
        }
        assert_eq!(b.network_guarantee(&NetworkPolicy::Deny), Guarantee::Weak);
        assert_eq!(b.capabilities().filesystem, Guarantee::Weak);
        // the helper inside a writable root could be replaced by the command
        let inside = AppContainerBackend::new(p("ws").join("helper.exe"));
        assert!(matches!(
            inside.wrap(&pol, &c),
            Err(SandboxError::Unavailable(_))
        ));
    }

    #[test]
    fn run_appcontainer_is_strict() {
        for bad in ["", "{}", r#"{"version":9}"#] {
            assert!(run_appcontainer(bad).is_err(), "{bad}");
        }
        let rel = r#"{"version":1,"program":"cmd.exe","command_line":"cmd","grants":[],"network":false,"memory":null,"tasks":null}"#;
        assert!(run_appcontainer(rel).unwrap_err().contains("absolute"));
    }
}
