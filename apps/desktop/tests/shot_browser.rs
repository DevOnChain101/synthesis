//! The browser tab of an offscreen shot draws the saved screenshot (a generated gradient PNG): the image region
//! of the rendered frame is not blank. Needs `--features shot,test-provider` (headless renderer).
#![cfg(all(feature = "shot", feature = "test-provider"))]

fn variance_of_image_area(size: &str, rect: impl Fn(u32, u32) -> (u32, u32, u32, u32)) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("s.json");
    std::fs::write(&script, r#"[{"text":"hi"}]"#).unwrap();
    let out = dir.path().join("o.png");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_synthesis-desktop"))
        .args([
            "--shot",
            out.to_str().unwrap(),
            "--size",
            size,
            "--tab",
            "browser",
            "--live",
        ])
        .arg(&script)
        .args(["--prompt", "hello"])
        .env("SYNT_HOME", dir.path().join("h"))
        .status()
        .unwrap();
    assert!(st.success());
    let img = image::open(&out).unwrap().to_luma8();
    let (x, y, w, h) = rect(img.width(), img.height());
    let px: Vec<f64> = (y..y + h)
        .flat_map(|j| (x..x + w).map(move |i| (i, j)))
        .map(|(i, j)| f64::from(img.get_pixel(i, j).0[0]))
        .collect();
    let mean = px.iter().sum::<f64>() / px.len() as f64;
    px.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / px.len() as f64
}

#[test]
fn the_saved_screenshot_is_drawn_not_blank() {
    // 1400x900 at 2x: the 300 px image box starts below the url bar (about y 430..1130 px; the area below the url bar and above the history rows is blank without it, x 590..1710 px)
    let var = variance_of_image_area("1400x900", |_, _| (700, 700, 600, 300));
    assert!(var > 100., "the screenshot area is blank (variance {var})");
}
