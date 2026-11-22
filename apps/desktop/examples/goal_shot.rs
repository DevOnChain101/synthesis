//! Development only: the Super Agent goal form on sample data, offscreen, one PNG per state (see
//! `superagent::goal::shot`). `cargo run -p synthesis-desktop --features shot --example goal_shot -- DIR`.

#[cfg(feature = "shot")]
fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for kind in ["writes", "ro"] {
        for s in ["empty", "filled", "errors", "review", "dialog"] {
            let p = std::path::Path::new(&dir).join(format!("goal-{kind}-{s}.png"));
            if let Err(e) =
                synthesis_desktop::superagent::goal::shot::render(&p, kind, s, (900., 1000.))
            {
                eprintln!("goal_shot: {kind} {s}: {e}");
            }
        }
    }
}

#[cfg(not(feature = "shot"))]
fn main() {}
