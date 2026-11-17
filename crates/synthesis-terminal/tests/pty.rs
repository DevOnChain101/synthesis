//! PTY tests (spec terminal-browser.md T1). Spawn/echo/resize/env run on every OS (ConPTY on
//! Windows); the sandbox wrap, flood, window and kill tests need a POSIX shell and are unix only.
//! They use only names they set themselves: no dependence on `TERM`, `$HOME` or the system tmpdir.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use synthesis_core::WorkspaceContext;
use synthesis_terminal::{Error, Pty, PtySpec, SandboxMode, ShellKind, TerminalRuntime};

const SID: &str = "s";

struct Fx {
    _dir: tempfile::TempDir,
    #[cfg_attr(not(unix), allow(dead_code))]
    root: PathBuf,
}

fn session_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for k in ["PATH", "Path", "SystemRoot", "ComSpec", "PATHEXT"] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.to_string(), v);
        }
    }
    // bait: none of these may reach the shell
    for (k, v) in [
        ("SYNT_API_KEY", "k1"),
        ("SYNT_FOO", "x"),
        ("GITHUB_TOKEN", "t1"),
        ("SYNT_AGENT_SESSION", "1"),
    ] {
        env.insert(k.into(), v.into());
    }
    env
}

fn open(rt: &TerminalRuntime) -> Fx {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("w");
    std::fs::create_dir_all(&root).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], base.join("art"), "pg").unwrap();
    rt.open_session(SID, Arc::new(ws), session_env());
    Fx { _dir: dir, root }
}

fn rt_off() -> TerminalRuntime {
    TerminalRuntime::new(SandboxMode::Off)
}

fn wait_line(p: &Pty, want: impl Fn(&str) -> bool, within: Duration) -> bool {
    let end = Instant::now() + within;
    loop {
        if p.screen_text(5000).lines().any(&want) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A line that ENDS with `marker` and is not the echoed command: dash prints its prompt (`$ `)
/// and cmd its (`C:\dir>`) on the same line as the output, and the typed command is echoed.
fn wait_out(p: &Pty, marker: &str, within: Duration) -> bool {
    wait_line(
        p,
        |l| l.trim_end().ends_with(marker) && !l.contains("echo"),
        within,
    )
}

/// `echo synt-42` whose typed form differs from its output (POSIX arithmetic, cmd literal).
fn echo_cmd(rt: &TerminalRuntime) -> &'static str {
    match rt.shell().kind {
        ShellKind::Posix => "echo synt-$((6*7))\r",
        ShellKind::Cmd => "echo synt-42\r",
    }
}

#[test]
fn spawn_echo_resize() {
    let rt = rt_off();
    let _fx = open(&rt);
    let p = rt.open_pty(SID, PtySpec::new(100, 30)).unwrap();
    assert!(p.pid.is_some() && p.unsandboxed_by_request());
    p.write_input(echo_cmd(&rt).as_bytes()).unwrap();
    assert!(
        wait_out(&p, "synt-42", Duration::from_secs(20)),
        "{}",
        p.screen_text(50)
    );

    p.resize(120, 40).unwrap();
    assert!(p.resize(0, 40).is_err() && p.resize(120, 501).is_err());
    let (cmd, ok): (&str, fn(&str) -> bool) = match rt.shell().kind {
        ShellKind::Posix => ("stty size\r", |l| {
            l.trim_end().ends_with("40 120") && !l.contains("stty")
        }),
        ShellKind::Cmd => ("mode con\r", |l| {
            l.trim().starts_with("Columns:") && l.trim().ends_with("120")
        }),
    };
    p.write_input(cmd.as_bytes()).unwrap();
    assert!(
        wait_line(&p, ok, Duration::from_secs(20)),
        "{}",
        p.screen_text(50)
    );
    // the raw log has the session's output, 0600 on unix
    assert!(p.log_bytes() > 0 && p.log.exists());
    rt.close_pty(SID, &p.id).unwrap();
    assert!(matches!(rt.pty(SID, &p.id), Err(Error::UnknownProcess(_))));
}

#[test]
fn child_env_has_no_synt_or_secret_names() {
    let rt = rt_off();
    let _fx = open(&rt);
    let mut spec = PtySpec::new(200, 50);
    spec.env.insert("SYNT_MORE".into(), "y".into());
    spec.env.insert("MY_PASSWORD".into(), "p".into());
    let p = rt.open_pty(SID, spec).unwrap();
    let cmd = match rt.shell().kind {
        ShellKind::Posix => "env; echo done-$((6*7))\r",
        ShellKind::Cmd => "set & echo done-42\r",
    };
    p.write_input(cmd.as_bytes()).unwrap();
    assert!(
        wait_out(&p, "done-42", Duration::from_secs(20)),
        "{}",
        p.screen_text(50)
    );
    let text = p.screen_text(5000);
    for bad in [
        "SYNT_API_KEY",
        "SYNT_FOO",
        "SYNT_MORE",
        "GITHUB_TOKEN",
        "MY_PASSWORD",
    ] {
        assert!(!text.contains(bad), "{bad} leaked:\n{text}");
    }
    assert!(text.contains("SYNT_AGENT_SESSION=1"), "{text}"); // the output-only marker stays
    assert!(text.contains("TERM=xterm-256color"), "{text}");
}

#[test]
fn enforce_without_policy_refuses_and_spawns_nothing() {
    let rt = TerminalRuntime::new(SandboxMode::Enforce);
    let _fx = open(&rt);
    let e = rt.open_pty(SID, PtySpec::new(80, 24)).err().unwrap();
    assert!(matches!(e, Error::SandboxUnavailable(_)), "{e}");
    assert!(matches!(
        rt.open_pty("nope", PtySpec::new(80, 24)).err().unwrap(),
        Error::UnknownSession(_)
    ));
    assert!(matches!(
        rt.open_pty(SID, PtySpec::new(0, 24)).err().unwrap(),
        Error::InvalidSpec(_)
    ));
}

#[test]
fn live_pty_cap_per_session() {
    let rt = rt_off();
    let _fx = open(&rt);
    let ptys: Vec<_> = (0..synthesis_terminal::MAX_PTYS)
        .map(|_| rt.open_pty(SID, PtySpec::new(80, 24)).unwrap())
        .collect();
    let e = rt.open_pty(SID, PtySpec::new(80, 24)).err().unwrap();
    assert!(matches!(e, Error::LimitExceeded(_)), "{e}");
    drop(ptys);
}

#[cfg(unix)]
mod unix {
    use super::*;
    use synthesis_sandbox::{
        Guarantee, SandboxBackend, SandboxCapabilities, SandboxError, SandboxPolicy,
        SandboxedCommand,
    };
    use synthesis_terminal::Limits;

    /// Wraps the shell in `sh -c 'echo wrapped-42; exec "$@"'`: proves the wrap decides argv.
    struct Wrap;

    impl SandboxBackend for Wrap {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                filesystem: Guarantee::Strong,
                network: Guarantee::Strong,
                process_tree: Guarantee::Strong,
                notes: vec![],
            }
        }
        fn wrap(
            &self,
            _: &SandboxPolicy,
            c: &SandboxedCommand,
        ) -> Result<SandboxedCommand, SandboxError> {
            let mut w = c.clone();
            w.program = "/bin/sh".into();
            w.args = ["-c", "echo wrapped-$((6*7)); exec \"$@\"", "sh"]
                .map(Into::into)
                .into_iter()
                .chain([c.program.clone()])
                .chain(c.args.clone())
                .collect();
            Ok(w)
        }
    }

    #[test]
    fn sandbox_wrap_is_what_runs() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce).with_sandbox_backend(Arc::new(Wrap));
        let fx = open(&rt);
        let mut spec = PtySpec::new(80, 24);
        spec.sandbox = Some(SandboxPolicy {
            writable_roots: vec![fx.root.clone()],
            ..SandboxPolicy::default()
        });
        let p = rt.open_pty(SID, spec).unwrap();
        assert!(!p.unsandboxed_by_request());
        assert!(wait_out(&p, "wrapped-42", Duration::from_secs(20)));
        // the wrapped command still runs in the session cwd with the scrubbed env
        p.write_input(b"pwd\r").unwrap();
        let root = fx.root.to_string_lossy().into_owned();
        assert!(
            wait_line(
                &p,
                |l| l.trim_end().ends_with(&root) && !l.contains("pwd"),
                Duration::from_secs(20)
            ),
            "{}",
            p.screen_text(50)
        );
    }

    fn pid_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The digits after `prefix` on a screen line (a shell prompt may precede it).
    fn pid_of(p: &Pty, prefix: &str) -> String {
        let find = |text: String| {
            text.lines().find_map(|l| {
                let rest = &l[l.rfind(prefix)? + prefix.len()..];
                let d: String = rest.chars().take_while(char::is_ascii_digit).collect();
                (!d.is_empty() && d.len() == rest.trim_end().len()).then_some(d)
            })
        };
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(d) = find(p.screen_text(100)) {
                return d;
            }
            assert!(Instant::now() < end, "{}", p.screen_text(50));
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Exited PTYs stay registered (each holds a screen) until closed: open-exit-repeat is capped.
    #[test]
    fn exited_ptys_still_count_against_the_cap() {
        let rt = rt_off();
        let _fx = open(&rt);
        let mut ptys = vec![];
        for _ in 0..synthesis_terminal::MAX_PTYS {
            let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
            p.write_input(b"exit\r").unwrap();
            ptys.push(p);
        }
        let end = Instant::now() + Duration::from_secs(20);
        while ptys.iter().any(|p| p.exit_code().is_none()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(ptys.iter().all(|p| p.exit_code().is_some()));
        let e = rt.open_pty(SID, PtySpec::new(80, 24)).err().unwrap();
        assert!(matches!(e, Error::LimitExceeded(_)), "{e}");
        rt.close_pty(SID, &ptys[0].id).unwrap(); // closing an exited, reaped PTY frees the slot
        assert!(rt.open_pty(SID, PtySpec::new(80, 24)).is_ok());
    }

    #[test]
    fn a_missing_cwd_is_an_error_not_a_home_fallback() {
        let rt = rt_off();
        let fx = open(&rt);
        let mut spec = PtySpec::new(80, 24);
        spec.cwd = Some(fx.root.join("nope"));
        let e = rt.open_pty(SID, spec).err().unwrap();
        assert!(matches!(e, Error::InvalidSpec(_)), "{e}");
    }

    fn wait_dead(pid: &str) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while pid_alive(pid) {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    #[test]
    fn close_kills_the_foreground_job_and_the_shell() {
        let rt = rt_off();
        let _fx = open(&rt);
        let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
        let shell = p.pid.unwrap().to_string();
        p.write_input(b"sh -c 'echo job-$$; sleep 4243'\r").unwrap();
        let job = pid_of(&p, "job-");
        assert!(pid_alive(&job));
        rt.close_pty(SID, &p.id).unwrap();
        assert!(wait_dead(&job), "foreground job survived close");
        assert!(wait_dead(&shell), "shell survived close");
    }

    #[test]
    fn dropping_the_runtime_kills_ptys() {
        let rt = rt_off();
        let _fx = open(&rt);
        let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
        let shell = p.pid.unwrap().to_string();
        drop(p);
        drop(rt);
        assert!(wait_dead(&shell));
    }

    /// Known limit (spec 1.7): a job that leaves the shell's group (`&` under job control, like
    /// `setsid`) may outlive the close without a PID-namespace sandbox (bubblewrap on Linux);
    /// bash happens to hang it up. What must hold: the shell dies and close does not hang.
    #[test]
    fn escaped_background_job_after_close_is_the_documented_limit() {
        let rt = rt_off();
        let _fx = open(&rt);
        let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
        let shell = p.pid.unwrap().to_string();
        p.write_input(b"sh -c 'echo bg-$$; exec sleep 4244' &\r")
            .unwrap();
        let bg = pid_of(&p, "bg-");
        let t = Instant::now();
        rt.close_pty(SID, &p.id).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(wait_dead(&shell));
        // the reader thread ends after close even if a straggler still holds the slave
        let end = Instant::now() + Duration::from_secs(5);
        while !p.output_ended() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(p.output_ended(), "reader outlived close");
        let _ = std::process::Command::new("kill")
            .args(["-9", &bg])
            .stderr(std::process::Stdio::null())
            .status();
    }

    const FLOOD: &str =
        "yes abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz | head -c 209715200\r";

    #[test]
    fn flood_200_mb_hits_the_log_cap_and_kills_the_tree() {
        const CAP: u64 = 8 << 20;
        let rt = rt_off().with_limits(Limits {
            max_log_bytes: CAP,
            ..Limits::default()
        });
        let _fx = open(&rt);
        let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
        p.write_input(FLOOD.as_bytes()).unwrap();
        let end = Instant::now() + Duration::from_secs(60);
        while !p.output_limit_exceeded() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            p.output_limit_exceeded(),
            "cap not hit, {} bytes",
            p.log_bytes()
        );
        let end = Instant::now() + Duration::from_secs(10);
        while p.exit_code().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            p.exit_code().is_some(),
            "shell still alive after the cap, pid {:?}\n{}",
            p.pid,
            p.screen_text(10)
        );
        let on_disk = std::fs::metadata(&p.log).unwrap().len();
        assert!(on_disk <= CAP, "{on_disk} bytes in the log");
        // memory: the scrollback is capped whatever came through
        assert!(p.screen_text(usize::MAX).lines().count() <= 5000 + 24);
    }

    #[test]
    fn unacknowledged_output_stops_the_reader_at_the_window() {
        let rt = rt_off();
        let _fx = open(&rt);
        let p = rt.open_pty(SID, PtySpec::new(80, 24)).unwrap();
        let out = p.subscribe();
        p.write_input(FLOOD.as_bytes()).unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        while p.pending_bytes() < synthesis_terminal::WINDOW && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        let (a, b) = (p.log_bytes(), {
            std::thread::sleep(Duration::from_millis(300));
            p.log_bytes()
        });
        assert_eq!(a, b, "reader kept reading with a full window");
        assert!(a <= (synthesis_terminal::WINDOW + (64 << 10)) as u64, "{a}");
        // acknowledging lets it continue
        let mut got = 0;
        while got < 4 << 20 {
            let chunk = out
                .recv_timeout(Duration::from_secs(10))
                .expect("reader did not resume");
            got += chunk.len();
            out.ack(chunk.len());
        }
        assert!(p.log_bytes() > b);
        rt.close_pty(SID, &p.id).unwrap();
    }
}
