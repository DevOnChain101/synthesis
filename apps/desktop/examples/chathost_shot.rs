//! Development only: the Super Agent session chat on a sample conversation, offscreen (see
//! `superagent::chathost::shot`). `cargo run -p synthesis-desktop --features shot --example chathost_shot -- DIR`.

#[cfg(feature = "shot")]
fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for (name, idle) in [("chathost", false), ("chathost-idle", true)] {
        let p = std::path::Path::new(&dir).join(format!("{name}.png"));
        if let Err(e) =
            synthesis_desktop::superagent::chathost::shot::render(&p, idle, (900., 820.))
        {
            eprintln!("chathost_shot: {name}: {e}");
        }
    }
}

#[cfg(not(feature = "shot"))]
fn main() {}
