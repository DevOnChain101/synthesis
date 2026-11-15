//! Landlock re-exec helper: `synthesis-sandbox-exec '<policy json>' -- <program> [args...]`.
//! Applies the policy to itself (fail closed if the kernel cannot enforce it), then replaces
//! itself with the program. Exit 126: sandbox could not be applied; 127: exec failed.
//! `synthesis-sandbox-exec --no-vsock -- <program> [args...]` (WSL, in front of bubblewrap)
//! installs the seccomp filter that denies AF_VSOCK sockets instead of a Landlock policy.
//! `synthesis-sandbox-exec --egress-bridge <socket> <port> -- <program> [args...]` (inside the
//! bubblewrap net namespace, `--network hosts`) forwards `127.0.0.1:<port>` to the egress proxy's
//! unix socket while `<program>` runs as its child; exit code = the program's.
//! `synthesis-sandbox-exec --appcontainer <spec json>` (Windows only, see `windows.rs`) runs the
//! program named in the spec inside an AppContainer and Job Object, removes the ACEs it added,
//! and exits with the program's exit code; 126: nothing could be run.
//! Any form may start with `--limits <spec>` (see `limits_arg`): rlimits and, when a cgroup v2 is
//! delegated, memory/tasks caps are applied first. `--limits <spec> -- <program> [args...]`
//! alone applies just those.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::os::unix::process::CommandExt;

    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if let [flag, spec, ..] = args.as_slice()
        && flag == "--limits"
    {
        let limits = match spec
            .to_str()
            .ok_or_else(|| {
                synthesis_sandbox::SandboxError::InvalidPolicy("--limits is not UTF-8".into())
            })
            .and_then(synthesis_sandbox::parse_limits_arg)
        {
            Ok(l) => l,
            Err(e) => return fail(126, &e),
        };
        if let Err(e) = synthesis_sandbox::apply_limits(&limits) {
            return fail(126, &e);
        }
        args.drain(..2);
        if let [sep, program, rest @ ..] = args.as_slice()
            && sep == "--"
        {
            let err = std::process::Command::new(program).args(rest).exec();
            return fail(127, &err);
        }
    }
    if let [flag, sep, program, rest @ ..] = args.as_slice()
        && flag == "--no-vsock"
        && sep == "--"
    {
        if let Err(e) = synthesis_sandbox::deny_vsock() {
            return fail(126, &e);
        }
        let err = std::process::Command::new(program).args(rest).exec();
        return fail(127, &err);
    }
    if let [flag, sock, port, sep, program, rest @ ..] = args.as_slice()
        && flag == "--egress-bridge"
        && sep == "--"
    {
        let Some(port) = port.to_str().and_then(|p| p.parse().ok()) else {
            return fail(126, &"bad bridge port");
        };
        return match synthesis_sandbox::run_egress_bridge(sock.as_ref(), port, program, rest) {
            Ok(code) => std::process::ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => fail(126, &e),
        };
    }
    let (policy, program, rest) = match synthesis_sandbox::parse_helper_args(&args) {
        Ok(v) => v,
        Err(e) => return fail(126, &e),
    };
    if let Err(e) = synthesis_sandbox::restrict_current_process(&policy) {
        return fail(126, &e);
    }
    let err = std::process::Command::new(program).args(rest).exec();
    fail(127, &err)
}

#[cfg(target_os = "linux")]
fn fail(code: u8, e: &dyn std::fmt::Display) -> std::process::ExitCode {
    eprintln!("synthesis-sandbox-exec: {e}");
    std::process::ExitCode::from(code)
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [flag, spec] = args.as_slice() else {
        eprintln!("synthesis-sandbox-exec: usage: --appcontainer <spec json>");
        return std::process::ExitCode::from(126);
    };
    let run = (flag == "--appcontainer")
        .then(|| spec.to_str())
        .flatten()
        .ok_or_else(|| "usage: --appcontainer <spec json>".to_string())
        .and_then(synthesis_sandbox::run_appcontainer);
    match run {
        // the program's own exit code (32 bits on Windows)
        Ok(code) => std::process::exit(code as i32),
        Err(e) => {
            eprintln!("synthesis-sandbox-exec: {e}");
            std::process::ExitCode::from(126)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> std::process::ExitCode {
    eprintln!("synthesis-sandbox-exec: Linux (Landlock) and Windows (AppContainer) only");
    std::process::ExitCode::from(126)
}
