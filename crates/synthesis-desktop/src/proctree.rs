//! Which processes belong to the app under test. Every agent-desktop call is scoped to windows
//! owned by the launched process tree, so the tree is read from the OS each time (children
//! start and exit). No unsafe: the table comes from `ps` (macOS, Linux) or PowerShell (Windows).

use std::collections::{HashMap, HashSet};

use crate::error::{DesktopError, Result};

/// `(pid, parent pid)` of every process.
#[derive(Debug, Clone, Default)]
pub struct ProcTable(HashMap<u32, u32>);

impl ProcTable {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u32, u32)>) -> Self {
        Self(pairs.into_iter().collect())
    }

    /// Parses lines of `<pid> <ppid>` (both `ps -o pid=,ppid=` and the PowerShell script).
    pub fn parse(out: &str) -> Self {
        Self::from_pairs(out.lines().filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        }))
    }

    /// `root` and everything below it.
    pub fn descendants(&self, root: u32) -> HashSet<u32> {
        let mut set = HashSet::from([root]);
        loop {
            let before = set.len();
            for (pid, ppid) in &self.0 {
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
                Some(p) => cur = *p,
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
                r#"Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId) $($_.ParentProcessId)" }"#,
            ])
            .output()
        } else {
            std::process::Command::new("/bin/ps")
                .args(["-A", "-o", "pid=,ppid="])
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
        assert!(t.descendants(std::process::id()).contains(&std::process::id()));
        assert!(t.ancestors(std::process::id()).len() >= 2);
    }
}
