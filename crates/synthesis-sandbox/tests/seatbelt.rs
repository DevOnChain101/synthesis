//! Real enforcement tests through /usr/bin/sandbox-exec. macOS only.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

use synthesis_sandbox::{
    NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand, SeatbeltBackend,
};

fn run(policy: &SandboxPolicy, cwd: &Path, prog: &str, args: &[&str]) -> Output {
    let cmd = SandboxedCommand {
        program: prog.into(),
        args: args.iter().map(Into::into).collect(),
        cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    };
    let w = SeatbeltBackend::detect()
        .expect("sandbox-exec present")
        .wrap(policy, &cmd)
        .unwrap();
    Command::new(&w.program)
        .args(&w.args)
        .env_clear()
        .envs(w.env.iter().cloned())
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// The `net_probe` example (target/<profile>/examples): a plain Rust binary to run inside the
/// sandbox. Not python3/perl: `/usr/bin/*` interpreters are xcrun shims that fail under Seatbelt.
fn net_probe() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push("examples");
    p.push("net_probe");
    assert!(p.exists(), "build the example first: {}", p.display());
    p
}

/// Dirs live under target/, not the system temp dir (which the sandbox always makes writable).
fn tmp() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn sh(policy: &SandboxPolicy, cwd: &Path, script: &str) -> Output {
    run(policy, cwd, "/bin/sh", &["-c", script])
}

fn pol(w: &Path) -> SandboxPolicy {
    SandboxPolicy {
        writable_roots: vec![w.canonicalize().unwrap()],
        ..Default::default()
    }
}

#[test]
fn write_inside_ok_outside_denied() {
    let (a, b) = (tmp(), tmp());
    let p = pol(a.path());
    let ok = sh(&p, a.path(), "echo hi > in.txt");
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(a.path().join("in.txt").exists());
    let out = b.path().join("out.txt");
    let bad = sh(&p, a.path(), &format!("echo hi > '{}'", out.display()));
    assert!(!bad.status.success() && !out.exists());
}

#[test]
fn denied_path_unreadable_and_unwritable_inside_root() {
    let a = tmp();
    std::fs::create_dir(a.path().join("secret")).unwrap();
    std::fs::write(a.path().join("secret/k"), "TOPSECRET").unwrap();
    std::fs::write(a.path().join("open"), "fine").unwrap();
    let mut p = pol(a.path());
    p.deny_paths = vec![a.path().join("secret")];
    assert!(sh(&p, a.path(), "cat open").status.success());
    let r = sh(&p, a.path(), "cat secret/k");
    assert!(!r.status.success() && !String::from_utf8_lossy(&r.stdout).contains("TOPSECRET"));
    assert!(!sh(&p, a.path(), "echo x > secret/new").status.success());
    assert!(!a.path().join("secret/new").exists());
}

#[test]
fn network_denied_unless_allowed() {
    let a = tmp();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    let mut p = pol(a.path());
    let nc = |p: &SandboxPolicy| {
        run(
            p,
            a.path(),
            "/usr/bin/nc",
            &["-z", "-w", "2", "127.0.0.1", &port],
        )
    };
    assert!(!nc(&p).status.success(), "connect must be denied");
    p.network = NetworkPolicy::Allow;
    assert!(
        nc(&p).status.success(),
        "control: connect works when allowed"
    );
}

#[test]
fn grandchildren_stay_confined() {
    let (a, b) = (tmp(), tmp());
    let out = b.path().join("g.txt");
    let s = format!("/bin/sh -c \"/bin/sh -c 'echo x > {}'\"", out.display());
    assert!(!sh(&pol(a.path()), a.path(), &s).status.success());
    assert!(!out.exists());
}

#[test]
fn hostile_path_names_do_not_widen_access() {
    let base = tmp();
    let weird = base.path().join("q\"uo)te (x)\nnl é\\");
    let sibling = base.path().join("sibling");
    std::fs::create_dir(&weird).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    let mut p = pol(&weird);
    assert!(sh(&p, &weird, "echo hi > f").status.success());
    assert!(weird.join("f").exists());
    let target = sibling.join("x");
    assert!(
        !sh(&p, &weird, &format!("echo hi > '{}'", target.display()))
            .status
            .success()
    );
    assert!(!target.exists());
    // A weird-named deny path inside a writable root is still denied.
    let sub = weird.join("d\")\n(allow default)");
    std::fs::create_dir(&sub).unwrap();
    p.deny_paths = vec![sub.clone()];
    assert!(!sh(&p, &weird, "echo x > \"$1\"/f").status.success() || !sub.join("f").exists());
    let w = SandboxedCommand {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "echo x > \"$1/f\"".into(),
            "sh".into(),
            sub.clone().into(),
        ],
        ..Default::default()
    };
    let w = SeatbeltBackend.wrap(&p, &w).unwrap();
    let o = Command::new(&w.program)
        .args(&w.args)
        .env_clear()
        .output()
        .unwrap();
    assert!(!o.status.success() && !sub.join("f").exists());
}

#[test]
fn allow_exec_false_blocks_other_programs() {
    // bash, not sh: macOS /bin/sh re-execs /bin/bash as a "variant", which this policy forbids.
    let a = tmp();
    let mut p = pol(a.path());
    p.allow_exec = false;
    let r = run(&p, a.path(), "/bin/bash", &["-c", "/bin/echo spawned"]);
    assert!(!r.status.success() && !String::from_utf8_lossy(&r.stdout).contains("spawned"));
    let ok = run(&p, a.path(), "/bin/bash", &["-c", "echo builtin-ok"]);
    assert!(String::from_utf8_lossy(&ok.stdout).contains("builtin-ok"));
}

fn home(p: &str) -> std::path::PathBuf {
    Path::new(&std::env::var("HOME").unwrap()).join(p)
}

/// The mach-lookup allowlist must keep the common toolchains working. Tools that are not
/// installed on this machine are skipped (and said so), not faked.
#[test]
fn common_toolchains_run_with_mach_allowlist() {
    let (w, t) = (tmp(), tmp());
    let mut p = pol(w.path());
    p.tmp_dir = Some(t.path().canonicalize().unwrap());
    p.readable_roots = [".nvm", ".cargo", ".rustup"]
        .iter()
        .map(|d| home(d))
        .filter(|d| d.exists())
        .collect();
    let find = |n: &str| {
        let path = std::env::var_os("PATH").unwrap();
        std::env::split_paths(&path)
            .map(|d| d.join(n))
            .find(|f| f.is_file())
    };
    let cases: [(&str, &[&str]); 7] = [
        (
            "sh",
            &[
                "-c",
                "echo ok; ls -l . >/dev/null; id -un >/dev/null; whoami >/dev/null",
            ],
        ),
        ("git", &["init", "-q"]),
        ("git", &["status", "--short"]),
        (
            "node",
            &["-e", "require('fs').writeFileSync('n.txt', String(1+1))"],
        ),
        (
            "python3",
            &[
                "-c",
                "import os,pwd,ssl; open('p.txt','w').write(pwd.getpwuid(os.getuid()).pw_name)",
            ],
        ),
        ("cargo", &["--version"]),
        ("rustc", &["--version"]),
    ];
    for (tool, args) in cases {
        let Some(bin) = find(tool) else {
            eprintln!("SKIPPED {tool}: not installed");
            continue;
        };
        // /usr/bin/{git,python3} are xcrun shims; on CI runners they cannot load libxcrun under Seatbelt.
        if std::env::var_os("CI").is_some()
            && bin.starts_with("/usr/bin")
            && matches!(tool, "git" | "python3")
        {
            eprintln!("SKIPPED {tool}: {} is an xcrun shim", bin.display());
            continue;
        }
        let o = run(&p, w.path(), bin.to_str().unwrap(), args);
        assert!(
            o.status.success(),
            "{tool} {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

#[test]
fn mach_lookup_is_an_allowlist_not_blanket() {
    let (s, _) = synthesis_sandbox::seatbelt_profile(&SandboxPolicy::default(), None);
    assert!(!s.contains("(allow mach-lookup)"), "{s}");
    assert!(s.contains("(allow mach-lookup (global-name"));
    // Real check: the pasteboard service is not on the list. Skipped when the control
    // (unsandboxed pbpaste) does not work on this machine, e.g. headless CI.
    if Command::new("/usr/bin/pbpaste")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let a = tmp();
        assert!(
            !run(&pol(a.path()), a.path(), "/usr/bin/pbpaste", &[])
                .status
                .success()
        );
    }
}

#[test]
fn only_the_private_tmp_is_writable() {
    let (w, t) = (tmp(), tmp());
    let mut p = pol(w.path());
    p.tmp_dir = Some(t.path().canonicalize().unwrap());
    let name = format!("synthesis-sb-{}", std::process::id());
    let ok = sh(
        &p,
        w.path(),
        "echo $TMPDIR; echo x > \"$TMPDIR/f\" && cat \"$TMPDIR/f\"",
    );
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(t.path().join("f").exists());
    assert!(
        String::from_utf8_lossy(&ok.stdout)
            .starts_with(t.path().canonicalize().unwrap().to_str().unwrap())
    );
    for real in ["/private/tmp", "/tmp", "/private/var/tmp"] {
        let f = format!("{real}/{name}");
        assert!(
            !sh(&p, w.path(), &format!("echo x > {f}")).status.success(),
            "{real}"
        );
        assert!(!Path::new(&f).exists(), "{real}");
    }
}

#[test]
fn readonly_paths_and_their_parents_cannot_be_modified_or_renamed() {
    let w = tmp();
    let root = w.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    let mut p = pol(&root);
    p.readonly_paths = vec![root.join(".git/hooks"), root.join(".git/config")];
    let ok = |s: &str| sh(&p, &root, s).status.success();
    assert!(
        ok("cat .git/config .git/hooks/pre-commit"),
        "read stays allowed"
    );
    assert!(
        ok("echo x > .git/index.lock && mv .git/index.lock .git/index"),
        "siblings stay writable"
    );
    for bad in [
        "echo x >> .git/config",
        "echo x > .git/hooks/post-commit",
        "rm .git/hooks/pre-commit",
        "mv .git/config .git/config.bak",
        "mv .git/hooks .git/hooks.bak",
        "mv .git .git.bak",
        "rm -r .git",
    ] {
        assert!(!ok(bad), "{bad}");
    }
    assert_eq!(
        std::fs::read_to_string(root.join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert!(root.join(".git/hooks/pre-commit").exists() && !root.join(".git.bak").exists());
}

#[test]
fn unix_sockets_follow_network_policy() {
    let (a, s) = (tmp(), tmp());
    let sock = s.path().join("s.sock");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let probe = net_probe();
    let sock = sock.to_str().unwrap();
    let mut p = pol(a.path());
    p.readable_roots = vec![
        s.path().canonicalize().unwrap(),
        probe.parent().unwrap().canonicalize().unwrap(),
    ];
    let py = |p: &SandboxPolicy| run(p, a.path(), probe.to_str().unwrap(), &["unix", sock]);
    assert!(!py(&p).status.success(), "AF_UNIX connect must be denied");
    p.network = NetworkPolicy::Allow;
    let c = py(&p);
    assert!(
        c.status.success(),
        "control: {}",
        String::from_utf8_lossy(&c.stderr)
    );
}

#[test]
fn loopback_allows_local_bind_and_connect_only() {
    let a = tmp();
    // Child binds, connects and accepts on 127.0.0.1 and ::1, then probes non-loopback targets.
    let probe = net_probe();
    let mut p = pol(a.path());
    p.network = NetworkPolicy::Loopback;
    p.readable_roots = vec![probe.parent().unwrap().canonicalize().unwrap()];
    let o = run(&p, a.path(), probe.to_str().unwrap(), &["loopback"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("LOOPBACK_OK"),
        "{out} {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        !out.contains("REMOTE_CONNECTED") && out.matches("remote_denied").count() == 2,
        "{out}"
    );
}

#[test]
fn deny_still_blocks_loopback_and_allow_works() {
    let a = tmp();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    let nc = |net| {
        let mut p = pol(a.path());
        p.network = net;
        run(
            &p,
            a.path(),
            "/usr/bin/nc",
            &["-z", "-w", "2", "127.0.0.1", &port],
        )
        .status
        .success()
    };
    assert!(!nc(NetworkPolicy::Deny));
    assert!(
        nc(NetworkPolicy::Loopback),
        "Loopback reaches a host loopback listener"
    );
    assert!(nc(NetworkPolicy::Allow));
}

#[test]
fn synthesis_state_db_and_memory_are_unreadable() {
    let a = tmp();
    let home = a.path().join("synthesis-home");
    std::fs::create_dir_all(home.join("memory")).unwrap();
    std::fs::create_dir_all(home.join("projects/p")).unwrap();
    std::fs::create_dir_all(home.join("browser-profiles/acme/Default")).unwrap();
    std::fs::write(
        home.join("browser-profiles/acme/Default/Cookies"),
        "COOKIES",
    )
    .unwrap();
    std::fs::write(home.join("projects/p/s.jsonl"), "TRANSCRIPT").unwrap();
    std::fs::write(home.join("synt.db"), "SESSIONS").unwrap();
    std::fs::write(home.join("chain.key"), "CHAINKEY").unwrap();
    std::fs::create_dir_all(home.join("run")).unwrap();
    std::fs::write(home.join("run/trust.key"), "TRUSTKEY").unwrap();
    std::fs::write(home.join("memory/memory.db"), "MEMORIES").unwrap();
    std::fs::write(home.join("other.txt"), "fine").unwrap();
    let mut p = pol(a.path());
    // control first: nothing denied, everything under the workspace root is readable
    for f in [
        "synt.db",
        "chain.key",
        "run/trust.key",
        "memory/memory.db",
        "projects/p/s.jsonl",
        "other.txt",
    ] {
        let r = sh(&p, a.path(), &format!("cat synthesis-home/{f}"));
        assert!(r.status.success(), "control {f}");
    }
    p.deny_paths = synthesis_sandbox::synthesis_state_paths(&home);
    assert!(
        sh(&p, a.path(), "cat synthesis-home/other.txt")
            .status
            .success()
    );
    for f in [
        "synt.db",
        "chain.key",
        "run/trust.key",
        "memory/memory.db",
        "projects/p/s.jsonl",
        "browser-profiles/acme/Default/Cookies",
    ] {
        let r = sh(&p, a.path(), &format!("cat synthesis-home/{f}"));
        let out = String::from_utf8_lossy(&r.stdout);
        assert!(
            !r.status.success()
                && !out.contains("COOKIES")
                && !out.contains("SESSIONS")
                && !out.contains("CHAINKEY")
                && !out.contains("TRUSTKEY")
                && !out.contains("MEMORIES")
                && !out.contains("TRANSCRIPT"),
            "{f} must be unreadable"
        );
    }
}

/// The standard policy: the repository is writable, but the project settings (hooks, MCP,
/// agents) and instruction files are not, whether or not they exist yet.
#[test]
fn standard_policy_blocks_planting_synthesis_settings_and_instruction_files() {
    let w = tmp();
    let root = w.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(root.join(".git/modules/sub")).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
    let mut p = SandboxPolicy::standard(&root, &root.join("private-tmp"));
    p.network = NetworkPolicy::Deny;
    p.readable_roots.clear();
    p.deny_paths.clear();
    p.allow_exec = true;
    let ok = |s: &str| sh(&p, &root, s).status.success();
    assert!(
        ok("echo hi > ordinary.txt"),
        "the rest of the repo stays writable"
    );
    for bad in [
        "mkdir .synt",
        "mkdir -p .synt/agents && echo x > .synt/agents/a.md",
        "echo '{}' > .synt/settings.json",
        "echo x > AGENTS.md",
        "echo x >> AGENTS.md",
        "echo x > .gitmodules",
        "echo x > .git/modules/sub/config",
        "echo x > .git/hooks/pre-commit",
    ] {
        assert!(!ok(bad), "{bad}");
    }
    assert!(!root.join(".synt").exists());
    assert!(!root.join(".gitmodules").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        "rules\n"
    );
}

/// `NetworkPolicy::Proxied`: the only reachable address is the egress proxy's loopback port, and
/// the proxy lets through only the allowed literal. Runs the real proxy.
#[test]
fn proxied_reaches_the_allowed_literal_through_the_proxy_and_nothing_else() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use synthesis_egress::{Allowlist, Proxy, system_lookup};
    use synthesis_sandbox::EgressEndpoint;

    let server = || {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                );
            }
        });
        port
    };
    let (allowed, other) = (server(), server());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let allow = Allowlist::parse(&[format!("127.0.0.1:{allowed}")]).unwrap();
    let proxy = rt
        .block_on(async {
            Proxy::new(allow, system_lookup(), Arc::new(|_| {}))
                .serve_tcp()
                .await
        })
        .unwrap();
    let (port, token) = proxy.tcp.clone().unwrap();
    let dir = tmp();
    let mut p = pol(dir.path());
    p.network = NetworkPolicy::Proxied(EgressEndpoint::Tcp { port, token });
    let curl = |args: &[&str]| {
        let mut a = vec!["-sS", "--max-time", "8"];
        a.extend_from_slice(args);
        run(&p, dir.path(), "/usr/bin/curl", &a)
    };
    // through the proxy: a tunnel (NO_PROXY would otherwise skip 127.0.0.1)
    let o = curl(&[
        "--noproxy",
        "",
        "-p",
        &format!("http://127.0.0.1:{allowed}/"),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.stdout, b"hello");
    // another loopback port: denied by the proxy, and directly by the sandbox
    let url = format!("http://127.0.0.1:{other}/");
    assert!(!curl(&["--noproxy", "", "-p", &url]).status.success());
    for direct in [url.as_str(), "http://1.1.1.1/"] {
        assert!(
            !curl(&["--noproxy", "*", direct]).status.success(),
            "{direct} must be unreachable without the proxy"
        );
    }
}
