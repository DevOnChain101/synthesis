//! D0: `synthesis_tui::run_interactive` in this process against an in-memory terminal, painted
//! into the `alacritty_terminal` grid. The grid text must equal `synthesis_tui::replay` of the
//! same bytes (docs/specs/desktop.md section 7, D0 acceptance).

use std::time::{Duration, Instant};

use synthesis_desktop::d0;
use synthesis_desktop::vt::Grid;
use synthesis_tui::{ExitReason, replay};

const REPLY: &str = "scripted reply from the fake model";

/// Feeds the TUI output into `grid` (and `raw`) until `needle` is on the screen.
fn wait_for(rig: &mut d0::Rig, grid: &mut Grid, raw: &mut Vec<u8>, needle: &str) {
    let t0 = Instant::now();
    loop {
        while let Ok(b) = rig.bytes.try_recv() {
            grid.feed(&b);
            raw.extend_from_slice(&b);
        }
        if grid.lines().join("\n").contains(needle) {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "timed out waiting for {needle:?}; screen:\n{}",
            grid.lines().join("\n")
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn trimmed(lines: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> = lines
        .into_iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    while v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v
}

#[test]
fn tui_runs_in_process_and_the_grid_matches_replay() {
    let (cols, rows) = (100, 30);
    let mut rig = d0::start(cols, rows, REPLY);
    let mut grid = Grid::new(cols as usize, rows as usize);
    let mut raw = Vec::new();

    // the first frame, then a prompt through the key channel, then the model's answer
    wait_for(&mut rig, &mut grid, &mut raw, "Synthesis");
    rig.keys.send(b"hello there\r".to_vec()).unwrap();
    wait_for(&mut rig, &mut grid, &mut raw, REPLY);

    // a resize: the TUI polls the size function and redraws at the new size
    *rig.size.lock().unwrap() = (80, 24);
    let t0 = Instant::now();
    loop {
        while let Ok(b) = rig.bytes.try_recv() {
            raw.extend_from_slice(&b);
        }
        // replay() rebuilds rows from cursor-addressed writes, so it shows the new size once
        // the TUI has redrawn
        if replay(&String::from_utf8_lossy(&raw)).len() <= 24
            || t0.elapsed() > Duration::from_secs(10)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // the grid, fed with exactly the same bytes, reads the same as replay()
    let mut grid = Grid::new(cols as usize, rows as usize);
    grid.feed(&raw);
    let from_grid = trimmed(grid.lines());
    let from_replay = trimmed(replay(&String::from_utf8_lossy(&raw)));
    assert!(from_grid.join("\n").contains(REPLY));
    assert_eq!(from_grid, from_replay);

    // closing the window ends the TUI cleanly (input closed), without touching the process
    let exit = rig.finish().expect("run_interactive returned an error");
    assert!(
        matches!(exit.reason, ExitReason::InputClosed | ExitReason::Quit),
        "{:?}",
        exit.reason
    );
}
