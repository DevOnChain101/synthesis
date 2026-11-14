//! macOS Seatbelt via `/usr/bin/sandbox-exec`.
//!
//! `sandbox-exec` is deprecated by Apple with no replacement; it still works but may break
//! in any macOS release, so this is best-effort (reported as `Weak`). Known leaks: file
//! metadata (stat/existence) is readable everywhere, and `mach-lookup` is limited to the
//! [`MACH_SERVICES`] allowlist (what sh, git, node, python and cargo need; extend it when a
//! tool fails with a `mach-lookup` denial in the system log). Reported as `Weak`: defence in
//! depth, NOT a security boundary.
//!
//! Injection safety: user paths never appear in the profile text. They are passed as
//! `-D NAME=value` argv entries and referenced with `(param "NAME")`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::*;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/opt/homebrew",
    "/dev",
    "/private/var/db",
];

/// Mach services granted to the sandboxed process. Everything else is denied.
const MACH_SERVICES: &[&str] = &[
    // getpwuid/getgrgid (python pwd, cargo/rustup home dir, ls -l, ssh)
    "com.apple.system.opendirectoryd.libinfo",
    // certificate verification for TLS (only useful with NetworkPolicy::Allow)
    "com.apple.trustd",
    "com.apple.trustd.agent",
];

pub struct SeatbeltBackend;

impl SeatbeltBackend {
    /// Some only if `sandbox-exec` exists AND works here (probed; it fails when this process is
    /// already inside a sandbox that forbids nesting).
    pub fn detect() -> Option<Self> {
        Path::new(SANDBOX_EXEC).is_file().then_some(())?;
        std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .env_clear()
            .output()
            .is_ok_and(|o| o.status.success())
            .then_some(Self)
    }
}

/// Developer dirs that Apple's tool shims (`/usr/bin/git` -> xcrun) must read and exec, resolved
/// once: `$DEVELOPER_DIR`, `xcode-select -p`, the Command Line Tools and every installed
/// `/Applications/Xcode*.app`. For an Xcode the whole `<app>/Contents` is granted (xcrun loads
/// `Contents/SharedFrameworks/*.framework`, not only `Contents/Developer`). Read-only; only
/// existing directories, canonicalised.
fn developer_dirs() -> &'static [PathBuf] {
    static D: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let mut c: Vec<PathBuf> = std::env::var_os("DEVELOPER_DIR")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        let sel = std::process::Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .env_clear()
            .output();
        if let Some(o) = sel.ok().filter(|o| o.status.success()) {
            c.push(PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        }
        c.push("/Library/Developer/CommandLineTools".into());
        for e in std::fs::read_dir("/Applications")
            .into_iter()
            .flatten()
            .flatten()
        {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with("Xcode") && n.ends_with(".app") {
                c.push(e.path().join("Contents"));
            }
        }
        // `<x>.app/Contents/Developer` (from DEVELOPER_DIR / xcode-select) -> `<x>.app/Contents`
        let bundles: Vec<PathBuf> = c.iter().filter_map(|p| xcode_contents(p)).collect();
        c.extend(bundles);
        let mut c: Vec<PathBuf> = c
            .into_iter()
            .filter(|p| p.is_absolute())
            .filter_map(|p| p.canonicalize().ok())
            .filter(|p| p.is_dir())
            .collect();
        c.sort();
        c.dedup();
        c
    })
}

/// `.../X.app/Contents/Developer` -> `.../X.app/Contents`; anything else -> None.
fn xcode_contents(p: &Path) -> Option<PathBuf> {
    (p.file_name()? == "Developer").then_some(())?;
    let contents = p.parent()?;
    let app = contents.parent()?;
    (contents.file_name()? == "Contents" && app.extension()? == "app")
        .then(|| contents.to_path_buf())
}

fn canon(p: &Path) -> PathBuf {
    crate::canon_lossy(p) // a not-yet-existing path resolves through its symlinked parent
}

/// Returns the SBPL text and the `-D` params it references.
pub fn seatbelt_profile(
    policy: &SandboxPolicy,
    exec_literal: Option<&Path>,
) -> (String, Vec<(String, PathBuf)>) {
    let mut params: Vec<(String, PathBuf)> = vec![];
    let mut group = |prefix: &str, paths: Vec<PathBuf>| -> String {
        paths
            .into_iter()
            .map(|p| {
                let name = format!("{prefix}{}", params.len());
                params.push((name.clone(), p));
                format!("(subpath (param \"{name}\"))")
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let canon_all = |v: &[PathBuf]| v.iter().map(|p| canon(p)).collect::<Vec<_>>();

    let tmp: Vec<PathBuf> = policy.tmp_dir.iter().map(|p| canon(p)).collect();
    let mut reads = canon_all(&policy.readable_roots);
    reads.extend(canon_all(&policy.writable_roots));
    reads.extend(tmp.clone());
    reads.extend(developer_dirs().iter().cloned());
    let mut writes = canon_all(&policy.writable_roots);
    writes.extend(tmp); // the real /tmp and /var/folders are NOT granted
    let (r, w, d, ro) = (
        group("R", reads),
        group("W", writes),
        group("D", canon_all(&policy.deny_paths)),
        group("O", canon_all(&policy.readonly_paths)),
    );
    let guards = group("G", canon_all(&policy.parent_guards()));
    let dev = group("X", developer_dirs().to_vec());

    let mut s = String::from(
        "(version 1)\n(deny default)\n(allow sysctl-read)\n\
         (allow signal (target self))\n(allow process-info* (target self))\n\
         (allow file-read-metadata)\n(allow file-read* (literal \"/\"))\n",
    );
    s += "(allow mach-lookup";
    for m in MACH_SERVICES {
        s += &format!(" (global-name \"{m}\")");
    }
    s += ")\n";
    for p in SYSTEM_READ {
        s += &format!("(allow file-read* (subpath \"{p}\"))\n");
    }
    s += "(allow file-write* (literal \"/dev/null\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\"))\n";
    if !r.is_empty() {
        s += &format!("(allow file-read* {r})\n");
    }
    // dyld maps the Xcode frameworks that xcrun-shimmed tools (git, python3) load
    if !dev.is_empty() {
        s += &format!("(allow file-map-executable {dev})\n");
    }
    s += &format!("(allow file-write* {w})\n");
    match (policy.allow_exec, exec_literal) {
        (true, _) => s += "(allow process-fork)\n(allow process-exec)\n",
        (false, Some(_)) => s += "(allow process-exec (literal (param \"EXEC\")))\n",
        (false, None) => {}
    }
    if policy.network == NetworkPolicy::Allow {
        s += "(allow network*)\n";
    }
    // Last match wins in SBPL: denies go after every allow.
    if !ro.is_empty() {
        s += &format!("(deny file-write* {ro})\n");
    }
    if !d.is_empty() {
        s += &format!("(deny file-read* file-write* {d})\n");
    }
    // Parent directory entries of protected paths: no rename/remove that would move them away.
    // `(literal ..)` matches the directory itself, not its children, so creating files in it works.
    let guards = guards.replace("(subpath ", "(literal ");
    if !guards.is_empty() {
        s += &format!("(deny file-write* {guards})\n");
    }
    if policy.network != NetworkPolicy::Allow {
        s += "(deny network*)\n"; // explicit, also covers AF_UNIX connect
    }
    if policy.network == NetworkPolicy::Loopback {
        // After the deny (last match wins). Measured on macOS 15: `network-inbound` is needed for
        // listen/accept; a combined `(local ip ..)(remote ip ..)` rule would let remote hosts in.
        s += "(allow network* (remote ip \"localhost:*\"))\n\
              (allow network-bind network-inbound (local ip \"localhost:*\"))\n";
    }
    if let NetworkPolicy::Proxied(EgressEndpoint::Tcp { port, .. }) = &policy.network {
        // After the deny (last match wins). Only the rendered integer reaches the profile.
        s += &format!("(allow network-outbound (remote ip \"localhost:{port}\"))\n");
    }
    if let (false, Some(e)) = (policy.allow_exec, exec_literal) {
        params.push(("EXEC".into(), canon(e)));
    }
    (s, params)
}

impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::Weak,
            network: Guarantee::Weak,
            process_tree: Guarantee::Strong,
            notes: vec![
                "sandbox-exec is deprecated by Apple; best-effort, may break on any macOS update".into(),
                "kernel-enforced, inherited by all descendants; file metadata (stat) stays readable".into(),
                "network Loopback exposes the HOST loopback (databases, Redis, docker TCP) to sandboxed code; a 0.0.0.0 listener is LAN-reachable; AF_UNIX stays denied".into(),
                "network Proxied: only outbound connects to the egress proxy's loopback port are allowed (the proxy enforces the host list); allow_exec=false permits only the initial program (macOS /bin/sh re-execs bash, so it cannot run)".into(),
                "tree kill is the terminal runtime's job (process group)".into(),
            ],
        }
    }

    fn network_guarantee(&self, mode: &NetworkPolicy) -> Guarantee {
        match mode {
            NetworkPolicy::Proxied(EgressEndpoint::Tcp { .. }) => Guarantee::Weak,
            _ => network_guarantee_for(mode, self.capabilities().network, Guarantee::Weak),
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        validate(policy)?;
        if matches!(
            policy.network,
            NetworkPolicy::Proxied(EgressEndpoint::Unix(_))
        ) {
            return Err(SandboxError::Unsupported(
                "Seatbelt reaches the egress proxy over loopback TCP, not a unix socket".into(),
            ));
        }
        let exec = if policy.allow_exec {
            None
        } else {
            Some(resolve_program(c).ok_or_else(|| {
                SandboxError::InvalidPolicy(
                    "allow_exec=false needs a resolvable program path".into(),
                )
            })?)
        };
        let (profile, params) = seatbelt_profile(policy, exec.as_deref());
        let mut args: Vec<OsString> = vec!["-p".into(), profile.into()];
        for (k, v) in params {
            let mut kv = OsString::from(format!("{k}="));
            kv.push(v);
            args.extend(["-D".into(), kv]);
        }
        args.push(c.program.clone());
        args.extend(c.args.iter().cloned());
        Ok(SandboxedCommand {
            program: SANDBOX_EXEC.into(),
            args,
            env: build_env(policy, c),
            cwd: c.cwd.clone(),
            clear_env: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_deny_path_that_does_not_exist_yet_resolves_through_a_symlinked_parent() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().canonicalize().unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, t.path().join("link")).unwrap();
        let want = real.join("chain.key");
        let p = SandboxPolicy {
            deny_paths: vec![t.path().join("link/chain.key")],
            ..Default::default()
        };
        assert!(seatbelt_profile(&p, None).1.iter().any(|(_, v)| *v == want));
    }

    #[test]
    fn hostile_paths_never_reach_profile_text() {
        let evil =
            PathBuf::from("/tmp/a\")) (allow default) (\n(allow file-write* (subpath \"/\"))é");
        let p = SandboxPolicy {
            writable_roots: vec![evil.clone()],
            deny_paths: vec![evil.clone()],
            ..Default::default()
        };
        let (s, params) = seatbelt_profile(&p, None);
        assert!(!s.contains("allow default") && !s.contains('é'));
        assert!(params.iter().any(|(_, v)| *v == canon(&evil)));
        assert!(s.trim_end().ends_with(')'));
    }

    #[test]
    fn xcode_bundle_contents_is_derived_from_the_developer_dir() {
        let c = |s: &str| xcode_contents(Path::new(s));
        assert_eq!(
            c("/Applications/Xcode_26.app/Contents/Developer"),
            Some("/Applications/Xcode_26.app/Contents".into())
        );
        for no in [
            "/Library/Developer/CommandLineTools",
            "/a/Contents/Developer",
            "/a.app/Developer",
            "/",
        ] {
            assert_eq!(c(no), None, "{no}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn active_developer_dir_is_readable_and_git_runs_inside() {
        let sel = std::process::Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .output()
            .unwrap();
        let sel = PathBuf::from(String::from_utf8_lossy(&sel.stdout).trim()).canonicalize();
        if let Ok(sel) = sel {
            let (s, params) = seatbelt_profile(&SandboxPolicy::default(), None);
            // the developer dir itself, or the whole Xcode bundle `Contents` above it, is granted
            assert!(
                params.iter().any(|(_, v)| sel.starts_with(v)),
                "{sel:?} not in {params:?}"
            );
            assert!(s.contains("(allow file-map-executable "), "{s}");
        }
        if SeatbeltBackend::detect().is_none() {
            return;
        }
        // An unusable git outside the sandbox is the environment's problem, not our policy's.
        if !std::process::Command::new("/usr/bin/git")
            .arg("--version")
            .env_clear()
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("SKIP: /usr/bin/git does not run outside the sandbox either");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![root.clone()],
            ..Default::default()
        };
        let run = |args: &[&str]| {
            let c = SandboxedCommand {
                program: "/usr/bin/git".into(),
                args: args.iter().map(Into::into).collect(),
                cwd: Some(root.clone()),
                ..Default::default()
            };
            let w = SeatbeltBackend.wrap(&policy, &c).unwrap();
            let o = std::process::Command::new(&w.program)
                .args(&w.args)
                .current_dir(&root)
                .env_clear()
                .envs(w.env.iter().map(|(k, v)| (k, v)))
                .output()
                .unwrap();
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            )
        };
        // xcrun may warn about its cache dir on stderr; only the exit status matters
        for args in [
            &["--version"][..],
            &["init", "-q"],
            &["status", "--porcelain"],
        ] {
            let (ok, err) = run(args);
            assert!(ok, "git {args:?} under Seatbelt: {err}");
        }
    }

    #[test]
    fn network_and_exec_rendering() {
        let mut p = SandboxPolicy::default();
        let s = seatbelt_profile(&p, None).0;
        assert!(!s.contains("(allow network*)") && s.contains("(deny network*)"));
        p.network = NetworkPolicy::Allow;
        p.allow_exec = false;
        let (s, params) = seatbelt_profile(&p, Some(Path::new("/bin/sh")));
        assert!(s.contains("(allow network*)") && !s.contains("(deny network*)"));
        assert!(!s.contains("process-fork"));
        assert!(params.iter().any(|(k, _)| k == "EXEC"));
    }

    #[test]
    fn proxied_allows_only_the_proxy_port_after_the_deny() {
        let p = SandboxPolicy {
            network: NetworkPolicy::Proxied(EgressEndpoint::Tcp {
                port: 5150,
                token: "zz9\") (allow network*)".into(),
            }),
            ..Default::default()
        };
        let s = seatbelt_profile(&p, None).0;
        let deny = s.find("(deny network*)").unwrap();
        let allow = s
            .find("(allow network-outbound (remote ip \"localhost:5150\"))")
            .unwrap();
        assert!(deny < allow && !s.contains("(allow network*)") && !s.contains("zz9"));
        assert_eq!(
            SeatbeltBackend.network_guarantee(&p.network),
            Guarantee::Weak
        );
        assert_eq!(
            SeatbeltBackend
                .network_guarantee(&NetworkPolicy::Proxied(EgressEndpoint::Unix("/a".into()))),
            Guarantee::None
        );
    }

    #[test]
    fn loopback_rules_come_after_the_deny() {
        let p = SandboxPolicy {
            network: NetworkPolicy::Loopback,
            ..Default::default()
        };
        let s = seatbelt_profile(&p, None).0;
        let deny = s.find("(deny network*)").unwrap();
        let allow = s
            .find("(allow network* (remote ip \"localhost:*\"))")
            .unwrap();
        assert!(deny < allow && !s.contains("(allow network*)"));
        assert!(s.contains("(allow network-bind network-inbound (local ip \"localhost:*\"))"));
        let b = SeatbeltBackend;
        assert_eq!(
            b.network_guarantee(&NetworkPolicy::Loopback),
            Guarantee::Weak
        );
    }
}
