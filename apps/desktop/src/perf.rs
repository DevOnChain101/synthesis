//! `SYNT_PERF=1`: once a second, one stderr line with what the UI did in that second: renders of the
//! chat view (count, total and worst time of building the element tree), snapshot rebuilds and the
//! notifies that asked for a redraw. Off, a counter costs one relaxed load.

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering::Relaxed},
};
use std::time::Instant;

static ON: OnceLock<bool> = OnceLock::new();
static RENDERS: AtomicU64 = AtomicU64::new(0);
static RENDER_US: AtomicU64 = AtomicU64::new(0);
static RENDER_MAX_US: AtomicU64 = AtomicU64::new(0);
static SNAPS: AtomicU64 = AtomicU64::new(0);
static SNAP_US: AtomicU64 = AtomicU64::new(0);
static NOTIFIES: AtomicU64 = AtomicU64::new(0);
static SECOND: OnceLock<std::sync::Mutex<Instant>> = OnceLock::new();

fn on() -> bool {
    *ON.get_or_init(|| {
        synthesis_config::Env::from_process()
            .get_nonempty("SYNT_PERF")
            .is_some()
    })
}

/// One redraw request from a message batch or the clock.
pub fn notify() {
    if on() {
        NOTIFIES.fetch_add(1, Relaxed);
    }
}

pub struct Timed(Option<(Instant, bool)>);

/// Times a render (`true`) or a snapshot rebuild (`false`) until dropped.
pub fn timed(render: bool) -> Timed {
    Timed(on().then(|| (Instant::now(), render)))
}

impl Drop for Timed {
    fn drop(&mut self) {
        let Some((t0, render)) = self.0 else { return };
        let us = t0.elapsed().as_micros() as u64;
        if render {
            RENDERS.fetch_add(1, Relaxed);
            RENDER_US.fetch_add(us, Relaxed);
            RENDER_MAX_US.fetch_max(us, Relaxed);
            report();
        } else {
            SNAPS.fetch_add(1, Relaxed);
            SNAP_US.fetch_add(us, Relaxed);
        }
    }
}

fn report() {
    let mut t = SECOND.get_or_init(|| Instant::now().into()).lock().unwrap();
    if t.elapsed().as_secs() < 1 {
        return;
    }
    *t = Instant::now();
    eprintln!("{}", take());
}

/// The counters of the last interval as a line; resets them.
pub fn take() -> String {
    let (n, us) = (RENDERS.swap(0, Relaxed), RENDER_US.swap(0, Relaxed));
    let (s, sus) = (SNAPS.swap(0, Relaxed), SNAP_US.swap(0, Relaxed));
    format!(
        "synthesis-desktop: perf: renders {n} (avg {} us, max {} us) snapshots {s} (avg {} us) notifies {}",
        us.checked_div(n).unwrap_or(0),
        RENDER_MAX_US.swap(0, Relaxed),
        sus.checked_div(s).unwrap_or(0),
        NOTIFIES.swap(0, Relaxed),
    )
}
