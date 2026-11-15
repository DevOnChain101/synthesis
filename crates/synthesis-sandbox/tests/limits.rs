//! `synthesis-sandbox-exec --limits`: the rlimits reach the command (read back from
//! `/proc/self/limits`), a bad spec fails closed. Linux only; the cgroup part needs a delegated
//! cgroup v2 and is only reported, never required.
#![cfg(target_os = "linux")]

use std::process::Command;

const HELPER: &str = env!("CARGO_BIN_EXE_synthesis-sandbox-exec");

/// (soft, hard) of the `/proc/self/limits` row that starts with `name`.
fn limit(table: &str, name: &str) -> (String, String) {
    let row = table.lines().find(|l| l.starts_with(name)).expect(name);
    let t: Vec<_> = row.split_whitespace().rev().collect();
    // "Max file size  1048576  1048576  bytes": the unit is last; "Max cpu time" has one too
    let at = t
        .iter()
        .position(|x| x.parse::<u64>().is_ok() || *x == "unlimited")
        .unwrap();
    (t[at + 1].to_string(), t[at].to_string())
}

#[test]
fn limits_reach_the_command() {
    let o = Command::new(HELPER)
        .args([
            "--limits",
            "core=0,fsize=1048576,nofile=64,cpu=600",
            "--",
            "/bin/cat",
            "/proc/self/limits",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let t = String::from_utf8_lossy(&o.stdout);
    let both = |n: &str, v: &str| assert_eq!(limit(&t, n), (v.into(), v.into()), "{n}\n{t}");
    both("Max core file size", "0");
    both("Max file size", "1048576");
    both("Max open files", "64");
    both("Max cpu time", "600");
}

#[test]
fn a_bad_spec_runs_nothing() {
    for spec in ["core=1", "fsize=x", "bogus=1", ""] {
        let o = Command::new(HELPER)
            .args(["--limits", spec, "--", "/bin/echo", "RAN"])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(126), "{spec}: {o:?}");
        assert!(o.stdout.is_empty(), "{spec}");
    }
}

#[test]
fn cgroup_status_is_a_reason_or_ok() {
    // either is a valid answer on a CI runner; it must not panic and an Err names a reason
    if let Err(e) = synthesis_sandbox::cgroup_status() {
        assert!(!e.is_empty());
        eprintln!("cgroup limits unenforced here: {e}");
    }
}
