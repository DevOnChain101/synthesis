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
    // a blank area has variance 0; this gradient measures about 74 in the region (the old bound of 100 was above it)
    assert!(var > 20., "the screenshot area is blank (variance {var})");
}

/// Bounding box (x0, y0, x1, y1) of the saturated pixels: the gradient frame is the only strongly coloured thing in the tab.
fn coloured_box(img: &image::RgbImage) -> Option<(u32, u32, u32, u32)> {
    let mut b: Option<(u32, u32, u32, u32)> = None;
    // the chat's own panes also carry a few accent pixels: count a row or column only when many pixels are coloured
    let hot = |x: u32, y: u32| {
        let p = img.get_pixel(x, y).0;
        p.iter().max().unwrap() - p.iter().min().unwrap() > 90
    };
    let (w, h) = img.dimensions();
    let rows: Vec<u32> = (0..h)
        .filter(|&y| (0..w).filter(|&x| hot(x, y)).count() > 300)
        .collect();
    let cols: Vec<u32> = (0..w)
        .filter(|&x| (0..h).filter(|&y| hot(x, y)).count() > 300)
        .collect();
    if let (Some(y0), Some(y1), Some(x0), Some(x1)) =
        (rows.first(), rows.last(), cols.first(), cols.last())
    {
        b = Some((*x0, *y0, *x1, *y1));
    }
    b
}

fn live_shot(extra: &[&str]) -> image::RgbImage {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("s.json");
    std::fs::write(&script, r#"[{"text":"hi"}]"#).unwrap();
    let out = dir.path().join("o.png");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_synthesis-desktop"))
        .args([
            "--shot",
            out.to_str().unwrap(),
            "--size",
            "1400x900",
            "--tab",
            "browser",
            "--browser-frame",
        ])
        .args(extra)
        .arg("--live")
        .arg(&script)
        .args(["--prompt", "hello"])
        .env("SYNT_HOME", dir.path().join("h"))
        .status()
        .unwrap();
    assert!(st.success());
    image::open(&out).unwrap().to_rgb8()
}

/// The live page view (`--browser-frame`: a fake browser with one fixed frame): the frame is drawn letterboxed, so the
/// coloured area has the frame's own 1280x800 aspect ratio and is big (not a thumbnail, not blank).
#[test]
fn the_live_page_view_draws_the_fixed_frame_letterboxed() {
    let img = live_shot(&[]);
    let (x0, y0, x1, y1) = coloured_box(&img).expect("no frame drawn");
    let (w, h) = ((x1 - x0 + 1) as f64, (y1 - y0 + 1) as f64);
    assert!(w > 600., "the frame is too small: {w}x{h}");
    assert!((w / h - 1.6).abs() < 0.05, "the frame is not 8:5: {w}x{h}");
}

/// A typed address the policy refuses shows its reason under the bar and no frame navigation happens (the fake browser
/// is never asked); an http address is offered to the person's own browser.
#[test]
fn a_refused_address_shows_in_the_bar_area() {
    let plain = live_shot(&[]);
    let refused = live_shot(&["--browser-go", "http://10.0.0.1"]);
    // the refusal line is red text: reddish pixels appear that the plain shot does not have
    let red = |i: &image::RgbImage| {
        i.pixels()
            .filter(|p| p.0[0] > 200 && p.0[1] < 140 && p.0[2] < 140)
            .count()
    };
    assert!(
        red(&refused) > red(&plain) + 200,
        "no refusal text: {} vs {}",
        red(&refused),
        red(&plain)
    );
}
