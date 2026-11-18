//! The Terminal tab of an offscreen shot draws a fixed byte script (`--pty-script`): the grid area is not
//! blank and holds the script's colours. Needs `--features shot` (headless renderer).
#![cfg(feature = "shot")]

#[test]
fn a_byte_script_is_drawn_in_the_terminal_tab() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("o.png");
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/design/pty-script.txt");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_synthesis-desktop"))
        .args([
            "--shot",
            out.to_str().unwrap(),
            "--size",
            "1400x900",
            "--tab",
            "terminal",
            "--check",
            "--pty-script",
        ])
        .arg(&script)
        .env("SYNT_HOME", dir.path().join("h"))
        .status()
        .unwrap();
    assert!(st.success());
    let img = image::open(&out).unwrap().to_rgb8();
    // the script's green "ok" (92,191,126) and red "FAILED" (228,108,104) are on screen
    let near = |c: [u8; 3]| {
        img.pixels()
            .filter(|p| p.0.iter().zip(c).all(|(a, b)| a.abs_diff(b) < 12))
            .count()
    };
    assert!(near([92, 191, 126]) > 20, "no green text");
    assert!(near([228, 108, 104]) > 20, "no red text");
}
