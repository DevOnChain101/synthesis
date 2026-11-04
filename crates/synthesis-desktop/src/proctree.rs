//! Which processes belong to the app under test. Every agent-desktop call is scoped to windows
//! owned by the launched process tree, so the tree is read from the OS each time (children
//! start and exit). No unsafe: the table comes from `ps` (macOS, Linux) or PowerShell (Windows).

use std::collections::{HashMap, HashSet};

use crate::error::{DesktopError, Result};

/// `(pid, parent pid, start time text)` of every process. The start time tells a reused pid
/// from the process that had it.
#[derive(Debug, Clone, Default)]
pub struct ProcTable(HashMap<u32, (u32, Option<String>)>);

impl ProcTable {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u32, u32)>) -> Self {
        Self(pairs.into_iter().map(|(p, pp)| (p, (pp, None))).collect())
    }

    /// With start times: `(pid, ppid, start)`.
    pub fn from_triples(t: impl IntoIterator<Item = (u32, u32, &'static str)>) -> Self {
        Self(
            t.into_iter()
                .map(|(p, pp, s)| (p, (pp, Some(s.to_owned()))))
                .collect(),
        )
    }

    /// Parses lines of `<pid> <ppid> [start time ...]` (`ps -o pid=,ppid=,lstart=` and the
    /// PowerShell script).
    pub fn parse(out: &str) -> Self {
        Self(
            out.lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let (pid, ppid) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
                    let start = it.collect::<Vec<_>>().join(" ");
                    Some((pid, (ppid, (!start.is_empty()).then_some(start))))
                })
                .collect(),
        )
    }

    /// The start time text of a process, when the source gave one.
    pub fn start_of(&self, pid: u32) -> Option<String> {
        self.0.get(&pid)?.1.clone()
    }

    pub fn contains(&self, pid: u32) -> bool {
        self.0.contains_key(&pid)
    }

    /// `root` and everything below it.
    pub fn descendants(&self, root: u32) -> HashSet<u32> {
        let mut set = HashSet::from([root]);
        loop {
            let before = set.len();
            for (pid, (ppid, _)) in &self.0 {
                if set.contains(ppid) {
                    set.insert(*pid);
                }
            }
            if set.len() == before {
                return set;
            }
        }
    }

    /// `pid` and every ancestor (the terminal or IDE that hosts this process is among them).
    pub fn ancestors(&self, pid: u32) -> HashSet<u32> {
        let mut set = HashSet::new();
        let mut cur = pid;
        while cur > 1 && set.insert(cur) {
            match self.0.get(&cur) {
                Some((p, _)) => cur = *p,
                None => break,
            }
        }
        set
    }
}

/// Where the table comes from (tests inject a fixed one).
pub trait ProcSource: Send + Sync {
    fn table(&self) -> Result<ProcTable>;
}

/// The real process table.
pub struct SystemProcs {
    /// `SystemRoot` (Windows), from the injected Env.
    pub system_root: Option<String>,
}

impl ProcSource for SystemProcs {
    fn table(&self) -> Result<ProcTable> {
        let out = if cfg!(windows) {
            let root = self.system_root.as_deref().unwrap_or(r"C:\Windows");
            std::process::Command::new(format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"
            ))
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                r#"Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId) $($_.ParentProcessId) $($_.CreationDate.Ticks)" }"#,
            ])
            .output()
        } else {
            // /bin/ps on macOS and most Linux, /usr/bin/ps where /bin is not merged into /usr
            let ps = ["/bin/ps", "/usr/bin/ps"]
                .into_iter()
                .find(|p| std::path::Path::new(p).is_file())
                .unwrap_or("/bin/ps");
            std::process::Command::new(ps)
                .args(["-A", "-o", "pid=,ppid=,lstart="])
                .output()
        }
        .map_err(|e| DesktopError::Unavailable(format!("cannot read the process table: {e}")))?;
        if !out.status.success() {
            return Err(DesktopError::Unavailable(
                "the process table command failed".into(),
            ));
        }
        Ok(ProcTable::parse(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_and_ancestors() {
        // 1 -> 10 (terminal) -> 20 (synt) -> 30 (app) -> 31, 32 -> 33 ; 99 unrelated
        let t = ProcTable::parse("1 0\n10 1\n20 10\n30 20\n31 30\n32 30\n33 32\n99 1\njunk\n");
        assert_eq!(t.descendants(30), HashSet::from([30, 31, 32, 33]));
        assert_eq!(t.descendants(31), HashSet::from([31]));
        assert_eq!(t.ancestors(20), HashSet::from([20, 10]));
        assert!(!t.descendants(30).contains(&99));
    }

    #[test]
    fn start_times_are_parsed_and_compared() {
        let t = ProcTable::parse("30 20 Mon Nov  3 10:00:00 2026\n31 30\n");
        assert_eq!(t.start_of(30).as_deref(), Some("Mon Nov 3 10:00:00 2026"));
        assert_eq!(t.start_of(31), None);
        assert!(t.contains(31) && !t.contains(32));
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let t = ProcTable::from_pairs([(5, 6), (6, 5)]);
        assert_eq!(t.ancestors(5).len(), 2);
        assert_eq!(t.descendants(5), HashSet::from([5, 6]));
    }

    #[test]
    fn the_real_table_contains_this_process() {
        if cfg!(windows) {
            return;
        }
        let t = SystemProcs { system_root: None }.table().unwrap();
        assert!(
            t.descendants(std::process::id())
                .contains(&std::process::id())
        );
        assert!(t.ancestors(std::process::id()).len() >= 2);
    }
}
