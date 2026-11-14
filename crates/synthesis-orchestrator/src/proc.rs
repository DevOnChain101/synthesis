//! Killing a worker and everything it started (ADR-017 "Cancel").

use synthesis_session::ProcessProbe;

/// Descendants of `pid` (children, grandchildren, ...), found before anything is killed so a
/// process that re-parents to init cannot slip away. Empty where the OS offers no listing.
#[cfg(unix)]
fn descendants(pid: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let pairs: Vec<(u32, u32)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for (p, pp) in &pairs {
            if *pp == parent && !found.contains(p) {
                found.push(*p);
            }
        }
        i += 1;
    }
    found.remove(0);
    found
}

#[cfg(not(unix))]
fn descendants(_pid: u32) -> Vec<u32> {
    vec![]
}

fn sweep(probe: &dyn ProcessProbe, pid: u32) {
    let below = descendants(pid);
    let _ = probe.kill_tree(pid);
    #[cfg(unix)]
    for d in below.into_iter().rev() {
        if d > 1
            && d != std::process::id()
            && let Some(p) = rustix::process::Pid::from_raw(d as i32)
        {
            let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
        }
    }
    #[cfg(not(unix))]
    let _ = below;
}

/// Kills the tree of a worker this process spawned and has NOT reaped yet (so its pid cannot have
/// been reused). Never pid 0 or 1, never this process.
pub fn kill_child_tree(probe: &dyn ProcessProbe, pid: u32) {
    if pid > 1 && pid != std::process::id() {
        sweep(probe, pid);
    }
}

/// Kills the process group and the descendants of a child this process spawned (in its own
/// group) and has not reaped yet; the part of [`kill_child_tree`] that needs no probe (a handle
/// that is dropped has none).
pub fn kill_unreaped_tree(pid: u32) {
    #[cfg(unix)]
    if pid > 1
        && pid != std::process::id()
        && let Some(p) = rustix::process::Pid::from_raw(pid as i32)
    {
        let below = descendants(pid);
        let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
        for d in below.into_iter().rev() {
            if d > 1
                && d != std::process::id()
                && let Some(q) = rustix::process::Pid::from_raw(d as i32)
            {
                let _ = rustix::process::kill_process(q, rustix::process::Signal::KILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Kills a process we did not spawn (recovery): `kill_guard` re-checks, right before the kill,
/// that it is not pid 0/1, not ourselves or our process group, and still has the start token that
/// was recorded; anything else is refused.
pub fn kill_verified(
    probe: &dyn ProcessProbe,
    pid: u32,
    recorded_start: Option<&str>,
) -> std::io::Result<()> {
    synthesis_session::kill_guard(pid, recorded_start, probe)?;
    sweep(probe, pid);
    Ok(())
}

/// Where a child this process spawned stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildState {
    Running,
    /// Exited, not reaped: its pid is still ours, so its tree can be swept before `wait`.
    ExitedUnreaped,
    /// Already reaped by the kernel (SIGCHLD is ignored: `waitid` answers ECHILD). The pid may
    /// belong to someone else by now, so it is never signalled again.
    Gone,
}

#[cfg(unix)]
fn state_of(r: Result<Option<()>, rustix::io::Errno>) -> ChildState {
    match r {
        Ok(Some(())) => ChildState::ExitedUnreaped,
        Err(rustix::io::Errno::CHILD) => ChildState::Gone,
        _ => ChildState::Running,
    }
}

#[cfg(unix)]
pub fn child_state(pid: u32) -> ChildState {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    let Some(p) = rustix::process::Pid::from_raw(pid as i32) else {
        return ChildState::Running;
    };
    state_of(
        waitid(
            WaitId::Pid(p),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|o| o.map(|_| ())),
    )
}

#[cfg(not(unix))]
pub fn child_state(_pid: u32) -> ChildState {
    ChildState::Running
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_child_the_kernel_already_reaped_is_gone_and_never_signalled() {
        assert_eq!(state_of(Err(rustix::io::Errno::CHILD)), ChildState::Gone);
        assert_eq!(state_of(Ok(Some(()))), ChildState::ExitedUnreaped);
        assert_eq!(state_of(Ok(None)), ChildState::Running);
        assert_eq!(state_of(Err(rustix::io::Errno::INTR)), ChildState::Running);
    }
}
