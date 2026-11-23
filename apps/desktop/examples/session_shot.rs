//! Development only: the Super Agent outcome and worker frames on the sample data, offscreen, one PNG per state (see
//! `superagent::session::shot`). `cargo run -p synthesis-desktop --features shot --example session_shot -- DIR`.

#[cfg(feature = "shot")]
fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for s in [
        "outcome",
        "outcome-loading",
        "outcome-error",
        "worker-live",
        "worker-queued",
        "worker-finished",
        "worker-off",
    ] {
        let p = std::path::Path::new(&dir).join(format!("frame-{s}.png"));
        if let Err(e) = synthesis_desktop::superagent::session::shot::render(&p, s, (900., 1000.)) {
            eprintln!("session_shot: {s}: {e}");
        }
    }
}

#[cfg(not(feature = "shot"))]
fn main() {}
