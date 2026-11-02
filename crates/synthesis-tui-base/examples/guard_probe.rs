//! Child process for tests/guard_signals.rs: arms a TerminalGuard on stdout, prints `ready`,
//! then waits for a signal (default) or panics (`panic` argument).

use std::io::Write;
use synthesis_tui_base::terminal::{GuardOptions, TerminalGuard};

fn main() {
    let _guard =
        TerminalGuard::enter(std::io::stdout(), GuardOptions { raw_mode: false }).expect("guard");
    println!("ready");
    std::io::stdout().flush().unwrap();
    if std::env::args().nth(1).as_deref() == Some("panic") {
        panic!("probe panic");
    }
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
