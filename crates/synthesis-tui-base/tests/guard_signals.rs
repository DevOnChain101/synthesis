//! Fixes Node defect D6: the terminal is restored when the process is killed by SIGHUP/SIGTERM
//! or panics. Runs the `guard_probe` example as a child and inspects what it wrote.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use synthesis_tui_base::screen::terminal_restore_sequence;

fn probe(arg: &str) -> std::process::Child {
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/guard_probe");
    assert!(
        path.exists(),
        "cargo test builds examples: {} missing",
        path.display()
    );
    Command::new(path)
        .arg(arg)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn killed_by(signal: &str, number: i32) {
    let mut child = probe("wait");
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    let status = child.wait().unwrap();
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert_eq!(
        rest,
        terminal_restore_sequence(),
        "restore sequence written once, before dying"
    );
    assert_eq!(
        status.signal(),
        Some(number),
        "default action still runs: {status:?}"
    );
}

#[test]
fn sigterm_restores_then_dies_by_sigterm() {
    killed_by("TERM", 15);
}

#[test]
fn sighup_restores_then_dies_by_sighup() {
    killed_by("HUP", 1);
}

#[test]
fn panic_restores_the_terminal() {
    let mut child = probe("panic");
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(out, format!("ready\n{}", terminal_restore_sequence()));
    assert_eq!(status.code(), Some(101));
}
