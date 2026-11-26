//! Development only: the Super Agent home on the sample data, offscreen, one PNG per state (see
//! `superagent::home::shot`). `cargo run -p synthesis-desktop --features shot --example home_shot -- DIR`.

#[cfg(feature = "shot")]
fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for s in ["normal", "queue", "card", "empty", "loading", "error"] {
        let p = std::path::Path::new(&dir).join(format!("home-{s}.png"));
        if let Err(e) = synthesis_desktop::superagent::home::shot::render(&p, s, (900., 1000.)) {
            eprintln!("home_shot: {s}: {e}");
        }
    }
}

#[cfg(not(feature = "shot"))]
fn main() {}
