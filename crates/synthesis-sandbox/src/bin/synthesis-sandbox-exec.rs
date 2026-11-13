//! Landlock re-exec helper: `synthesis-sandbox-exec '<policy json>' -- <program> [args...]`.
//! Applies the policy to itself (fail closed if the kernel cannot enforce it), then replaces
//! itself with the program. Exit 126: sandbox could not be applied; 127: exec failed.
//! `synthesis-sandbox-exec --no-vsock -- <program> [args...]` (WSL, in front of bubblewrap)
//! installs the seccomp filter that denies AF_VSOCK sockets instead of a Landlock policy.
//! `synthesis-sandbox-exec --egress-bridge <socket> <port> -- <program> [args...]` (inside the
//! bubblewrap net namespace, `--network hosts`) forwards `127.0.0.1:<port>` to the egress proxy's
//! unix socket while `<program>` runs as its child; exit code = the program's.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::os::unix::process::CommandExt;

    let args: Vec<_> = std::env::args_os().skip(1).collect();
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

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("synthesis-sandbox-exec: Landlock is Linux-only");
    std::process::ExitCode::from(126)
}
