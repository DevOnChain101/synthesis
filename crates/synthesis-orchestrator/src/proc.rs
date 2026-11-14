//! Killing a worker and everything it started (ADR-017 "Cancel").

use synthesis_session::ProcessProbe;

/// Descendants of `pid` (children, grandchildren, ...), found before anything is killed so a
/// process that re-parents to init cannot slip away. Empty where the OS offers no listing.
#[cfg(unix)]
fn descendants(pid: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("ps")
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

/// Kills `pid`, its process group (the probe) and every descendant found beforehand, leaves
/// first. Errors mean "already gone".
pub fn kill_tree(probe: &dyn ProcessProbe, pid: u32) {
    let below = descendants(pid);
    let _ = probe.kill_tree(pid);
    #[cfg(unix)]
    for d in below.into_iter().rev() {
        if let Some(p) = rustix::process::Pid::from_raw(d as i32) {
            let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
        }
    }
    #[cfg(not(unix))]
    let _ = below;
}
