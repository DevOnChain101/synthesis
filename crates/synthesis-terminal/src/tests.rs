use super::*;
use tempfile::TempDir;

struct Fx {
    _dir: TempDir,
    rt: TerminalRuntime,
    cwd: PathBuf,
}

const SID: &str = "s1";

fn fx() -> Fx {
    fx_with(TerminalRuntime::new(SandboxMode::Off))
}

fn fx_with(rt: TerminalRuntime) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(base.join("w")).unwrap();
    let ws = WorkspaceContext::open(
        "w",
        &base.join("w"),
        &base.join("w"),
        &[],
        base.join("art"),
        "pg",
    )
    .unwrap();
    let cwd = ws.cwd.clone();
    let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
    rt.open_session(SID, Arc::new(ws), env);
    Fx { _dir: dir, rt, cwd }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn assert_dies(pid: u32) {
    for _ in 0..300 {
        if !alive(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("pid {pid} still alive");
}

async fn read_pid(path: &Path) -> u32 {
    for _ in 0..300 {
        if let Ok(s) = fs::read_to_string(path)
            && let Ok(p) = s.trim().parse()
        {
            return p;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("no pid file");
}

async fn exec(f: &Fx, spec: ExecSpec) -> ExecResult {
    f.rt.exec(SID, spec, &CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn exec_exit_code_and_output() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("echo out; echo err >&2; exit 3")).await;
    assert_eq!(r.exit_code, Some(3));
    assert!(r.output.contains("out") && r.output.contains("err"));
    assert!(!r.truncated && !r.timed_out && !r.cancelled);
    assert!(r.log_path.starts_with(f.cwd.parent().unwrap().join("art")));
}

/// A command runs in its own session: no controlling terminal (so no /dev/tty to read the
/// person's prompt answers from or to push keystrokes into), and it leads its own group.
#[cfg(unix)]
#[tokio::test]
async fn commands_have_no_controlling_terminal() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("ps -o tty=,pgid=,pid= -p $$")).await;
    let cols: Vec<&str> = r.output.split_whitespace().collect();
    assert!(cols[0].starts_with('?'), "tty column: {}", r.output);
    assert_eq!(cols[1], cols[2], "own process group leader: {}", r.output);
    let r = exec(&f, ExecSpec::new("(exec 3<>/dev/tty) 2>/dev/null; echo rc=$?")).await;
    assert!(!r.output.contains("rc=0"), "/dev/tty opened: {}", r.output);
}

#[tokio::test]
async fn cwd_is_explicit_not_inherited() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("pwd -P")).await;
    assert_eq!(r.output.trim(), f.cwd.to_str().unwrap());
    assert_ne!(
        PathBuf::from(r.output.trim()),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    );
    fs::create_dir(f.cwd.join("sub")).unwrap();
    let mut spec = ExecSpec::new("pwd -P");
    spec.cwd = Some("sub".into());
    assert_eq!(
        exec(&f, spec).await.output.trim(),
        f.cwd.join("sub").to_str().unwrap()
    );
}

#[tokio::test]
async fn host_env_is_not_inherited() {
    let f = fx();
    // cargo sets this for the test process; the session env does not carry it.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let r = exec(&f, ExecSpec::new("echo \"[${CARGO_MANIFEST_DIR-unset}]\"")).await;
    assert_eq!(r.output.trim(), "[unset]");
}

#[tokio::test]
async fn timeout_kills_process_and_grandchild() {
    let f = fx();
    let pidfile = f.cwd.join("gc.pid");
    let mut spec = ExecSpec::new("sleep 60 & echo $! > gc.pid; wait");
    spec.timeout = Duration::from_millis(300);
    let r = exec(&f, spec).await;
    assert!(r.timed_out);
    assert_eq!(r.signal, Some(9));
    assert_dies(read_pid(&pidfile).await).await;
}

#[tokio::test]
async fn timeout_can_move_the_command_to_the_background_instead_of_killing_it() {
    let f = fx();
    let mut spec = ExecSpec::new("echo started; sleep 1; echo finished");
    spec.timeout = Duration::from_millis(300);
    spec.background_on_timeout = true;
    let r = exec(&f, spec).await;
    let id = r.background_id.clone().expect("backgrounded");
    assert!(!r.timed_out && r.exit_code.is_none(), "{r:?}");
    assert!(r.output.contains("started") && !r.output.contains("finished"));
    // it is a normal session process now: listed, still running, finishes on its own
    assert!(f.rt.get_status(SID, &id).unwrap().running);
    for _ in 0..300 {
        if !f.rt.get_status(SID, &id).unwrap().running {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    let st = f.rt.get_status(SID, &id).unwrap();
    assert_eq!(st.exit_code, Some(0));
    assert!(f.rt.read_logs(SID, &id, None, 1000).unwrap().data.contains("finished"));
    // a command that ends in time is a plain result
    let mut quick = ExecSpec::new("echo hi");
    quick.background_on_timeout = true;
    assert!(exec(&f, quick).await.background_id.is_none());
}

#[tokio::test]
async fn background_on_timeout_kills_when_the_session_is_full() {
    let rt = TerminalRuntime::new(SandboxMode::Off).with_limits(Limits {
        max_live_procs: 0,
        ..Limits::default()
    });
    let f = fx_with(rt);
    let mut spec = ExecSpec::new("sleep 30");
    spec.timeout = Duration::from_millis(200);
    spec.background_on_timeout = true;
    let r = exec(&f, spec).await;
    assert!(r.timed_out && r.background_id.is_none());
}

#[tokio::test]
async fn cancellation_kills_tree() {
    let f = fx();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let r =
        f.rt.exec(
            SID,
            ExecSpec::new("sleep 60 & echo $! > gc.pid; wait"),
            &cancel,
        )
        .await
        .unwrap();
    assert!(r.cancelled && !r.timed_out);
    assert_dies(read_pid(&f.cwd.join("gc.pid")).await).await;
}

#[tokio::test]
async fn exec_cleans_up_leftover_background_children() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("sleep 60 & echo $! > gc.pid")).await;
    assert_eq!(r.exit_code, Some(0));
    assert_dies(read_pid(&f.cwd.join("gc.pid")).await).await;
}

#[tokio::test]
async fn truncation_keeps_full_log() {
    let f = fx();
    let mut spec = ExecSpec::new("i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done");
    spec.max_output_bytes = 1000;
    let r = exec(&f, spec).await;
    assert!(r.truncated && r.output.contains("bytes truncated"));
    assert!(r.output.starts_with("line0") && r.output.trim_end().ends_with("line1999"));
    assert!(r.output.len() < 1200);
    let full = fs::read_to_string(&r.log_path).unwrap();
    assert_eq!(full.len() as u64, r.total_bytes);
    assert_eq!(full.lines().count(), 2000);

    let mut spec = ExecSpec::new("seq 1 100");
    spec.max_output_lines = 10;
    let r = exec(&f, spec).await;
    assert!(r.truncated && r.output.contains("lines truncated") && r.output.contains("100"));
}

#[tokio::test]
async fn spawn_and_read_logs_with_cursor() {
    let f = fx();
    let h =
        f.rt.spawn(
            SID,
            SpawnSpec::new("echo one; sleep 0.3; echo two; sleep 60"),
        )
        .await
        .unwrap();
    assert!(h.ready && h.log_cursor == "0");
    let mut data = String::new();
    let mut cursor = None;
    for _ in 0..300 {
        let c =
            f.rt.read_logs(SID, &h.process_id, cursor.as_deref(), 4)
                .unwrap();
        assert!(c.data.len() <= 4);
        data += &c.data;
        cursor = Some(c.cursor);
        if data.contains("two") {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(data, "one\ntwo\n"); // incremental, no repeats or gaps
    let c =
        f.rt.read_logs(SID, &h.process_id, cursor.as_deref(), 100)
            .unwrap();
    assert!(c.data.is_empty() && !c.eof);
    assert!(f.rt.get_status(SID, &h.process_id).unwrap().running);
    f.rt.signal(SID, &h.process_id, Signal::Kill).unwrap();
}

#[tokio::test]
async fn read_logs_does_not_split_utf8() {
    let f = fx();
    let h =
        f.rt.spawn(SID, SpawnSpec::new("printf 'h\\303\\251llo'"))
            .await
            .unwrap();
    sleep(Duration::from_millis(200)).await;
    let a = f.rt.read_logs(SID, &h.process_id, None, 2).unwrap(); // 'h' + half of e-acute
    assert_eq!(a.data, "h");
    let b =
        f.rt.read_logs(SID, &h.process_id, Some(&a.cursor), 100)
            .unwrap();
    assert_eq!(b.data, "\u{e9}llo");
    assert!(b.eof);
}

#[tokio::test]
async fn ready_by_log_pattern() {
    let f = fx();
    let mut spec = SpawnSpec::new("sleep 0.2; echo listening on 8080; sleep 60");
    spec.ready = Some(Readiness::LogPattern(r"listening on \d+".into()));
    let h = f.rt.spawn(SID, spec).await.unwrap();
    assert!(h.ready);
    f.rt.close_session(SID);
    assert!(f.rt.get_status(SID, &h.process_id).is_err());
}

#[tokio::test]
async fn not_ready_on_timeout_or_early_exit() {
    let f = fx();
    let mut spec = SpawnSpec::new("sleep 60");
    spec.ready = Some(Readiness::LogPattern("never".into()));
    spec.ready_timeout = Duration::from_millis(200);
    assert!(!f.rt.spawn(SID, spec).await.unwrap().ready);

    let mut spec = SpawnSpec::new("exit 1");
    spec.ready = Some(Readiness::Port(1));
    let t = Instant::now();
    let h = f.rt.spawn(SID, spec).await.unwrap();
    assert!(!h.ready && t.elapsed() < Duration::from_secs(5));
    assert_eq!(
        f.rt.get_status(SID, &h.process_id).unwrap().exit_code,
        Some(1)
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// perl is present on macOS and Linux CI images; no sleep-for-a-port races on nc flavours.
fn perl_server(port: u16) -> String {
    format!(
        "perl -MIO::Socket::INET -e '$s=IO::Socket::INET->new(Listen=>5,ReuseAddr=>1,LocalAddr=>\"127.0.0.1\",LocalPort=>{port}) or die; \
         while($c=$s->accept){{ sysread($c,$b,4096); print $c \"HTTP/1.1 200 OK\\r\\nContent-Length: 0\\r\\n\\r\\n\"; close $c }}'"
    )
}

#[tokio::test]
async fn ready_by_tcp_port_and_http() {
    let f = fx();
    let port = free_port();
    let mut spec = SpawnSpec::new(perl_server(port));
    spec.ready = Some(Readiness::Port(port));
    let h = f.rt.spawn(SID, spec).await.unwrap();
    assert!(h.ready);
    assert!(http_ok(&format!("http://127.0.0.1:{port}/health")).await);
    assert!(!http_ok("https://127.0.0.1/").await);
    f.rt.signal(SID, &h.process_id, Signal::Kill).unwrap();
    wait_status(&f, &h.process_id, |s| !s.running).await; // reaps
    assert_dies(h.pid).await;

    let port = free_port();
    let mut spec = SpawnSpec::new(perl_server(port));
    spec.ready = Some(Readiness::Http(format!("http://127.0.0.1:{port}/")));
    assert!(f.rt.spawn(SID, spec).await.unwrap().ready);
}

async fn wait_status(f: &Fx, id: &str, done: impl Fn(&ProcessStatus) -> bool) -> ProcessStatus {
    for _ in 0..300 {
        let s = f.rt.get_status(SID, id).unwrap();
        if done(&s) {
            return s;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("status never reached");
}

#[tokio::test]
async fn signal_term_vs_kill() {
    let f = fx();
    let h = f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
    f.rt.signal(SID, &h.process_id, Signal::Term).unwrap();
    let s = wait_status(&f, &h.process_id, |s| !s.running).await;
    assert_eq!(s.signal, Some(15));

    let mut spec = SpawnSpec::new("trap '' TERM INT; echo armed; while :; do sleep 0.1; done");
    spec.ready = Some(Readiness::LogPattern("armed".into()));
    let h = f.rt.spawn(SID, spec).await.unwrap();
    assert!(h.ready);
    f.rt.signal(SID, &h.process_id, Signal::Term).unwrap();
    f.rt.signal(SID, &h.process_id, Signal::Int).unwrap();
    sleep(Duration::from_millis(300)).await;
    assert!(
        f.rt.get_status(SID, &h.process_id).unwrap().running,
        "TERM/INT ignored"
    );
    f.rt.signal(SID, &h.process_id, Signal::Kill).unwrap();
    let s = wait_status(&f, &h.process_id, |s| !s.running).await;
    assert_eq!(s.signal, Some(9));
    assert_eq!(s.to_event_info().signal.as_deref(), Some("SIGKILL"));
}

#[tokio::test]
async fn close_session_leaves_no_live_processes() {
    let f = fx();
    let h =
        f.rt.spawn(SID, SpawnSpec::new("sleep 60 & echo $! > gc.pid; sleep 60"))
            .await
            .unwrap();
    let gc = read_pid(&f.cwd.join("gc.pid")).await;
    assert!(alive(h.pid) && alive(gc));
    assert_eq!(f.rt.list(SID).unwrap().len(), 1);
    f.rt.close_session(SID);
    assert_dies(h.pid).await; // reaped off-thread, so eventual
    assert_dies(gc).await;
    assert!(matches!(f.rt.list(SID), Err(Error::UnknownSession(_))));
}

#[tokio::test]
async fn dropping_runtime_kills_everything() {
    let f = fx();
    let h =
        f.rt.spawn(SID, SpawnSpec::new("sleep 60 & echo $! > gc.pid; sleep 60"))
            .await
            .unwrap();
    let gc = read_pid(&f.cwd.join("gc.pid")).await;
    let Fx { rt, .. } = f;
    drop(rt);
    assert_dies(h.pid).await;
    assert_dies(gc).await;
}

#[tokio::test]
async fn errors() {
    let f = fx();
    assert!(matches!(
        f.rt.exec("nope", ExecSpec::new("true"), &CancellationToken::new())
            .await,
        Err(Error::UnknownSession(_))
    ));
    assert!(matches!(
        f.rt.get_status(SID, "x"),
        Err(Error::UnknownProcess(_))
    ));
    assert!(matches!(
        f.rt.exec(SID, ExecSpec::new("  "), &CancellationToken::new())
            .await,
        Err(Error::InvalidSpec(_))
    ));
}

fn with_stdin(cmd: &str, data: Vec<u8>) -> ExecSpec {
    let mut spec = ExecSpec::new(cmd);
    spec.stdin = Some(data);
    spec
}

#[tokio::test]
async fn stdin_small_payload() {
    let f = fx();
    let r = exec(&f, with_stdin("cat", b"hello stdin".to_vec())).await;
    assert_eq!((r.exit_code, r.output.as_str()), (Some(0), "hello stdin"));
}

#[tokio::test]
async fn stdin_default_is_null() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("cat; echo done")).await;
    assert_eq!(r.output.trim(), "done");
}

#[tokio::test]
async fn stdin_large_payload_does_not_deadlock() {
    let f = fx();
    // 3 MiB in, 3 MiB out: neither pipe nor log may stall the other side.
    let data = vec![b'x'; 3 << 20];
    let mut spec = with_stdin("cat", data);
    spec.timeout = Duration::from_secs(30);
    let r = exec(&f, spec).await;
    assert_eq!((r.exit_code, r.timed_out), (Some(0), false));
    assert_eq!(r.total_bytes, 3 << 20);
}

#[tokio::test]
async fn stdin_ignored_by_child() {
    let f = fx();
    let mut spec = with_stdin("echo ignored", vec![b'x'; 2 << 20]);
    spec.timeout = Duration::from_secs(30);
    let r = exec(&f, spec).await;
    assert_eq!((r.exit_code, r.output.trim()), (Some(0), "ignored"));
    assert!(!r.timed_out);
}

#[tokio::test]
async fn stdin_child_exits_early() {
    let f = fx();
    let mut spec = with_stdin(
        "head -c 10 >/dev/null; echo early; exit 4",
        vec![b'y'; 4 << 20],
    );
    spec.timeout = Duration::from_secs(30);
    let r = exec(&f, spec).await;
    assert_eq!((r.exit_code, r.output.trim()), (Some(4), "early"));
    assert!(!r.timed_out);
}

#[tokio::test]
async fn stderr_separate() {
    let f = fx();
    let mut spec = ExecSpec::new("echo out; echo err >&2; exit 2");
    spec.stderr = StderrMode::Separate;
    let r = exec(&f, spec).await;
    assert_eq!(r.exit_code, Some(2));
    assert_eq!((r.output.as_str(), r.stderr.as_str()), ("out\n", "err\n"));
    let p = r.stderr_log_path.unwrap();
    assert_ne!(p, r.log_path);
    assert_eq!(fs::read_to_string(p).unwrap(), "err\n");
}

#[tokio::test]
async fn stderr_separate_truncates_each_stream() {
    let f = fx();
    let mut spec = ExecSpec::new("seq 1 100; seq 1 100 >&2");
    spec.stderr = StderrMode::Separate;
    spec.max_output_lines = 10;
    let r = exec(&f, spec).await;
    assert!(r.truncated && r.stderr_truncated);
    assert!(r.stderr.contains("lines truncated"));
    assert_eq!(r.stderr_total_bytes, r.total_bytes);
}

#[tokio::test]
async fn merged_leaves_stderr_fields_empty() {
    let f = fx();
    let r = exec(&f, ExecSpec::new("echo err >&2")).await;
    assert_eq!((r.output.as_str(), r.stderr.as_str()), ("err\n", ""));
    assert!(r.stderr_log_path.is_none());
}

// F3: commands always run under a fixed POSIX sh, never $SHELL.
#[tokio::test]
async fn runs_under_fixed_sh_not_user_shell() {
    let f = fx();
    assert_eq!(f.rt.shell().program, PathBuf::from("/bin/sh"));
    assert!(f.rt.shell().is_posix());
    // zsh process substitution is a syntax error in sh, so `touch` never runs.
    let r = exec(&f, ExecSpec::new("cat =(touch pwn)")).await;
    assert_ne!(r.exit_code, Some(0));
    assert!(!f.cwd.join("pwn").exists());
    let r = exec(&f, ExecSpec::new("ls *(e:'touch pwn2':)")).await;
    assert_ne!(r.exit_code, Some(0));
    assert!(!f.cwd.join("pwn2").exists());
}

// F7: spec.cwd cannot leave the workspace.
#[tokio::test]
async fn spec_cwd_cannot_escape_workspace() {
    let f = fx();
    let outside = f.cwd.parent().unwrap().to_path_buf();
    for bad in [
        outside.clone(),
        PathBuf::from(".."),
        PathBuf::from("../../etc"),
    ] {
        let mut spec = ExecSpec::new("pwd");
        spec.cwd = Some(bad);
        let r = f.rt.exec(SID, spec, &CancellationToken::new()).await;
        assert!(matches!(r, Err(Error::InvalidSpec(_))), "{r:?}");
    }
    std::os::unix::fs::symlink(&outside, f.cwd.join("link")).unwrap();
    let mut spec = ExecSpec::new("pwd");
    spec.cwd = Some("link".into());
    assert!(matches!(
        f.rt.exec(SID, spec, &CancellationToken::new()).await,
        Err(Error::InvalidSpec(_))
    ));
    let mut sp = SpawnSpec::new("pwd");
    sp.cwd = Some(outside);
    assert!(matches!(
        f.rt.spawn(SID, sp).await,
        Err(Error::InvalidSpec(_))
    ));
    // absolute path inside the workspace is fine
    fs::create_dir(f.cwd.join("ok")).unwrap();
    let mut spec = ExecSpec::new("pwd -P");
    spec.cwd = Some(f.cwd.join("ok"));
    assert!(exec(&f, spec).await.output.trim().ends_with("/ok"));
}

fn limited(max_log: u64, runtime: Duration, live: usize) -> Fx {
    fx_with(TerminalRuntime::new(SandboxMode::Off).with_limits(Limits {
        max_log_bytes: max_log,
        spawn_max_runtime: runtime,
        max_live_procs: live,
        ..Limits::default()
    }))
}

// F8
#[tokio::test]
async fn exec_output_cap_kills_tree() {
    let f = limited(1 << 20, Duration::from_secs(60), 32);
    let r = exec(&f, ExecSpec::new("yes")).await;
    assert!(r.output_limit_exceeded);
    assert!(!r.timed_out);
    assert_eq!(r.signal, Some(9));
    let len = fs::metadata(&r.log_path).unwrap().len();
    assert!(len < 64 << 20, "log grew to {len}");
}

#[tokio::test]
async fn exec_under_cap_is_not_flagged() {
    let f = limited(1 << 20, Duration::from_secs(60), 32);
    assert!(
        !exec(&f, ExecSpec::new("echo hi"))
            .await
            .output_limit_exceeded
    );
}

#[tokio::test]
async fn spawn_output_cap_is_enforced_without_polling() {
    let f = limited(1 << 20, Duration::from_secs(60), 32);
    let h = f.rt.spawn(SID, SpawnSpec::new("yes")).await.unwrap();
    let st = wait_status(&f, &h.process_id, |s| !s.running).await;
    assert!(st.output_limit_exceeded);
    assert_dies(h.pid).await;
}

#[tokio::test]
async fn spawn_max_runtime_kills_service() {
    let f = limited(1 << 20, Duration::from_millis(300), 32);
    let h = f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
    let st = wait_status(&f, &h.process_id, |s| !s.running).await;
    assert!(st.max_runtime_exceeded && !st.output_limit_exceeded);
    assert_dies(h.pid).await;
}

#[tokio::test]
async fn live_spawn_cap_per_session() {
    let f = limited(1 << 20, Duration::from_secs(60), 2);
    f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
    let b = f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
    let e =
        f.rt.spawn(SID, SpawnSpec::new("sleep 60"))
            .await
            .unwrap_err();
    assert!(matches!(e, Error::LimitExceeded(_)));
    assert!(e.to_string().contains("2 live"), "{e}");
    // a finished process frees a slot
    f.rt.signal(SID, &b.process_id, Signal::Kill).unwrap();
    wait_status(&f, &b.process_id, |s| !s.running).await;
    f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
}

// L4
#[tokio::test]
async fn logs_are_private_unpredictable_and_never_followed() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx();
    let a = exec(&f, ExecSpec::new("true")).await;
    let b = exec(&f, ExecSpec::new("true")).await;
    assert_ne!(a.log_path, b.log_path);
    let mode = fs::metadata(&a.log_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // a pre-planted symlink at a guessable name is not used and not written through
    let dir = a.log_path.parent().unwrap();
    let target = f.cwd.join("victim");
    for n in 1..=50 {
        let _ = std::os::unix::fs::symlink(&target, dir.join(format!("proc_{n}.log")));
    }
    let r = exec(&f, ExecSpec::new("echo pwned")).await;
    assert!(!target.exists());
    assert!(r.output.contains("pwned"));
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_session_inside_async_does_not_block_executor() {
    let f = fx();
    let h = f.rt.spawn(SID, SpawnSpec::new("sleep 60")).await.unwrap();
    let t = Instant::now();
    f.rt.close_session(SID);
    assert!(t.elapsed() < Duration::from_millis(3), "{:?}", t.elapsed());
    assert_dies(h.pid).await;
}

// L4: the session quota spans processes, each of which is under the per-process cap.
#[tokio::test]
async fn session_log_quota_spans_processes() {
    let f = fx_with(TerminalRuntime::new(SandboxMode::Off).with_limits(Limits {
        max_session_log_bytes: 1_000_000,
        ..Limits::default()
    }));
    let first = exec(&f, ExecSpec::new("head -c 600000 /dev/zero")).await;
    assert!(!first.output_limit_exceeded && first.total_bytes == 600_000);
    let second = exec(&f, ExecSpec::new("head -c 600000 /dev/zero")).await;
    assert!(
        second.output_limit_exceeded,
        "1.2 MB total must trip the 1 MB session quota"
    );
    let err =
        f.rt.exec(SID, ExecSpec::new("true"), &CancellationToken::new())
            .await
            .unwrap_err();
    assert!(matches!(err, Error::LimitExceeded(_)), "{err}");
    assert!(matches!(
        f.rt.spawn(SID, SpawnSpec::new("true")).await,
        Err(Error::LimitExceeded(_))
    ));
}

#[tokio::test]
async fn session_log_quota_kills_a_runaway_writer() {
    let f = fx_with(TerminalRuntime::new(SandboxMode::Off).with_limits(Limits {
        max_session_log_bytes: 2 << 20,
        ..Limits::default()
    }));
    let r = exec(&f, ExecSpec::new("yes")).await;
    assert!(r.output_limit_exceeded && !r.timed_out);
}
