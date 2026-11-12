//! Landlock re-exec helper: `synthesis-sandbox-exec '<policy json>' -- <program> [args...]`.
//! Applies the policy to itself (fail closed if the kernel cannot enforce it), then replaces
//! itself with the program. Exit 126: sandbox could not be applied; 127: exec failed.
//! `synthesis-sandbox-exec --no-vsock -- <program> [args...]` (WSL, in front of bubblewrap)
//! installs the seccomp filter that denies AF_VSOCK sockets instead of a Landlock policy.

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
