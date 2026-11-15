//! Disk-usage watchdog for one sandboxed command: how much the writable roots and the private
//! temp dir GREW since the command started. A thread walks the roots (a few seconds apart,
//! slower on a big tree) and publishes the growth; `Proc::enforce` kills the tree when it
//! reaches the quota. It measures apparent file sizes (a sparse file over-counts, which only
//! kills early), net of deletions, and a fast writer overshoots by what it writes between two
//! walks; the per-file cap is `RLIMIT_FSIZE` (Linux). shortcut: a walk stops after `MAX_ENTRIES`
//! entries, so a tree larger than that under-counts; raise it or switch to `statvfs` if that bites.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

const MAX_ENTRIES: usize = 2_000_000;
/// Pause between walks; the thread also waits four times as long as the last walk took.
pub(crate) const EVERY: Duration = Duration::from_secs(5);

pub(crate) struct Watch {
    growth: AtomicU64,
    max: u64,
}

impl Watch {
    /// Starts the thread; it ends when the last `Arc` is dropped.
    pub(crate) fn start(roots: Vec<PathBuf>, max: u64, every: Duration) -> Arc<Self> {
        let w = Arc::new(Self {
            growth: AtomicU64::new(0),
            max,
        });
        let weak = Arc::downgrade(&w);
        let _ = std::thread::Builder::new()
            .name("synt-disk-watch".into())
            .spawn(move || watch(&weak, &roots, every));
        w
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.growth.load(Ordering::Relaxed) >= self.max
    }

    pub(crate) fn max(&self) -> u64 {
        self.max
    }
}

fn watch(w: &Weak<Watch>, roots: &[PathBuf], every: Duration) {
    let mut base = None;
    while w.strong_count() > 0 {
        let t = std::time::Instant::now();
        let used: u64 = roots.iter().map(|r| size_of_tree(r)).sum();
        let base = *base.get_or_insert(used);
        let Some(s) = w.upgrade() else { return };
        s.growth.store(used.saturating_sub(base), Ordering::Relaxed);
        drop(s);
        std::thread::sleep(every.max(t.elapsed() * 4));
    }
}

/// Sum of regular-file sizes under `root` (symlinks are not followed, errors are skipped).
fn size_of_tree(root: &Path) -> u64 {
    let (mut total, mut seen) = (0u64, 0usize);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return total;
            }
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) if m.is_file() => total += m.len(),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_over_the_start_trips_the_quota() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("old"), vec![0u8; 5000]).unwrap(); // already there: not counted
        let w = Watch::start(
            vec![t.path().to_path_buf()],
            1000,
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!w.exceeded());
        std::fs::write(t.path().join("new"), vec![0u8; 1500]).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !w.exceeded() && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(w.exceeded());
    }

    #[test]
    fn size_of_tree_counts_files_not_symlinks() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("d")).unwrap();
        std::fs::write(t.path().join("d/a"), [0u8; 10]).unwrap();
        std::fs::write(t.path().join("b"), [0u8; 5]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("d/a"), t.path().join("l")).unwrap();
        assert_eq!(size_of_tree(t.path()), 15);
    }
}
