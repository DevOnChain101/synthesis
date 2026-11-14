//! Crash recovery (spec workspace-context.md §3: running -> interrupted on reopen).

use std::io;
use std::path::Path;
use std::process::Command;

use synthesis_core::SessionStatus;

use crate::error::{Error, Result};
use crate::store::{Store, StoreOptions};

/// OS access needed to judge a recorded process. Injected so tests can fake pid reuse.
pub trait ProcessProbe {
    /// Opaque start-time token of the live process `pid`, `None` if there is none.
    fn start_time(&self, pid: u32) -> Option<String>;
    /// Kills `pid` and its descendants (process group on POSIX, `taskkill /T` on Windows).
    fn kill_tree(&self, pid: u32) -> io::Result<()>;
    /// Process group id of the calling process, when the OS has one. Killing `-pid` for that
    /// pid would take the caller down with it, so recovery refuses it.
    fn own_pgid(&self) -> Option<u32> {
        None
    }
}

pub struct OsProbe;

/// Start-time token of `pid`; record it next to the pid right after spawning
/// (`ProcessRecord::pid_start`) so recovery can tell the process from a pid reuser.
pub fn process_start_time(pid: u32) -> Option<String> {
    OsProbe.start_time(pid)
}

fn run(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

impl ProcessProbe for OsProbe {
    fn start_time(&self, pid: u32) -> Option<String> {
        if cfg!(windows) {
            // Unverified on a real Windows machine.
            let ps = synthesis_platform::system32("WindowsPowerShell/v1.0/powershell.exe");
            let script = format!(
                "(Get-Process -Id {pid} -ErrorAction SilentlyContinue).StartTime.ToUniversalTime().ToString('o')"
            );
            run(Command::new(ps).args(["-NoProfile", "-NonInteractive", "-Command", &script]))
        } else if cfg!(target_os = "linux") {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // Field 22 (starttime); `comm` may contain spaces, so count from the last ')'.
            let ticks = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .to_string();
            let boot =
                std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
            Some(format!("{}:{ticks}", boot.trim()))
        } else {
            // lstart has 1 s resolution, so the command name is part of the token (a pid
            // reused within the same second by a different program no longer matches).
            run(Command::new("/bin/ps").args(["-o", "lstart=,comm=", "-p", &pid.to_string()]))
        }
    }

    fn own_pgid(&self) -> Option<u32> {
        if cfg!(windows) {
            return None;
        }
        run(Command::new("/bin/ps").args(["-o", "pgid=", "-p", &std::process::id().to_string()]))?
            .parse()
            .ok()
    }

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        let status = if cfg!(windows) {
            Command::new(synthesis_platform::system32("taskkill.exe"))
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status()?
        } else {
            // Children run in their own process group (pgid == pid); try the group, then the
            // pid itself. The pid is a number we format, never user text.
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "kill -KILL -- \"-$0\" 2>/dev/null; kill -KILL \"$0\"",
                    &pid.to_string(),
                ])
                .status()?
        };
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "could not kill pid {pid}: {status}"
            )))
        }
    }
}

/// Last checks before `kill_tree`: never pid 0/1, never ourselves or our own process group, and
/// the start-time token must still equal the recorded one (the pid may have been reused since
/// the first probe).
pub fn kill_guard(pid: u32, recorded: Option<&str>, probe: &dyn ProcessProbe) -> io::Result<()> {
    let refuse = |why: &str| {
        Err(io::Error::other(format!(
            "refusing to kill pid {pid}: {why}"
        )))
    };
    if pid <= 1 {
        return refuse("reserved pid");
    }
    if pid == std::process::id() || probe.own_pgid() == Some(pid) {
        return refuse("it is this process or its process group");
    }
    match (probe.start_time(pid), recorded) {
        (Some(now), Some(rec)) if now == rec => Ok(()),
        _ => refuse("identity changed since it was verified"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    /// Only list live orphans; they stay `running` in the table (so a rerun lists them again).
    Report,
    /// Kill orphans whose identity was verified by start time.
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanState {
    /// pid alive and its start time equals the recorded one.
    Alive,
    /// pid alive but no start time was recorded: identity unprovable, never killed.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub session_id: String,
    pub process_id: String,
    pub pid: u32,
    pub command: String,
    pub state: OrphanState,
    pub killed: bool,
    pub kill_error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Sessions moved running/waiting_approval/verifying/cancelling -> interrupted.
    pub interrupted: Vec<String>,
    /// Sessions skipped because a live process holds their writer lock.
    pub skipped_live: Vec<String>,
    pub orphans: Vec<Orphan>,
    /// Process rows closed as `lost`: no such pid, a different start time (pid reuse), or no pid.
    pub lost: Vec<String>,
}

/// Idempotent: a second run finds nothing left to change (orphans left by `Report` are
/// listed again, since they are still alive).
pub fn recover(synthesis_home: &Path, action: OrphanAction) -> Result<RecoveryReport> {
    recover_with(synthesis_home, action, &OsProbe)
}

pub fn recover_with(
    synthesis_home: &Path,
    action: OrphanAction,
    probe: &dyn ProcessProbe,
) -> Result<RecoveryReport> {
    let store = Store::open(synthesis_home, StoreOptions::default())?;
    let reader = store.reader()?;
    let mut report = RecoveryReport::default();
    let ids: Vec<String> = {
        let mut st = reader.conn.prepare(
            "SELECT id FROM sessions WHERE status IN ('running','waiting_approval','verifying','cancelling')
             OR id IN (SELECT session_id FROM processes WHERE status='running') ORDER BY id",
        )?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in ids {
        let mut w = match store.open_locked(&id) {
            Ok(w) => w,
            Err(Error::Locked(_)) => {
                report.skipped_live.push(id);
                continue;
            }
            Err(e) => return Err(e),
        };
        if matches!(
            w.get_session(&id)?.status,
            SessionStatus::Running | SessionStatus::WaitingApproval | SessionStatus::Verifying
        ) {
            w.update_status(
                SessionStatus::Interrupted,
                Some("writer process exited; found running on reopen"),
            )?;
            report.interrupted.push(id.clone());
        }
        if w.get_session(&id)?.status == SessionStatus::Cancelling {
            // The core state machine lacks cancelling -> interrupted: store-level repair.
            w.repair_cancelling("writer process exited; found cancelling on reopen")?;
            report.interrupted.push(id.clone());
        }
        for p in w
            .processes(&id)?
            .into_iter()
            .filter(|p| p.status == "running")
        {
            let Some(pid) = p.pid else {
                w.finish_process(&p.id, "lost", None)?;
                report.lost.push(p.id);
                continue;
            };
            let state = match (probe.start_time(pid), &p.pid_start) {
                (None, _) => None,
                (Some(now), Some(rec)) if &now != rec => None, // pid reused by another process
                (Some(_), Some(_)) => Some(OrphanState::Alive),
                (Some(_), None) => Some(OrphanState::Unverified),
            };
            let Some(state) = state else {
                w.finish_process(&p.id, "lost", None)?;
                report.lost.push(p.id);
                continue;
            };
            let mut o = Orphan {
                session_id: id.clone(),
                process_id: p.id.clone(),
                pid,
                command: p.command,
                state,
                killed: false,
                kill_error: None,
            };
            if action == OrphanAction::Kill && o.state == OrphanState::Alive {
                match kill_guard(pid, p.pid_start.as_deref(), probe)
                    .and_then(|()| probe.kill_tree(pid))
                {
                    Ok(()) => {
                        o.killed = true;
                        w.finish_process(&p.id, "killed", None)?;
                    }
                    Err(e) => o.kill_error = Some(e.to_string()),
                }
            }
            report.orphans.push(o);
        }
    }
    Ok(report)
}
