//! Development only (feature `shot`): `synthesis-desktop --shot out.png [--size WxH] [--hide a,b]` draws
//! the TUI view into a headless gpui context (real text shaping, Metal/wgpu renderer, no window server)
//! and writes the frame to a PNG, so no Screen Recording permission and no display are needed.

use std::{path::PathBuf, sync::Arc};

use gpui::{AppContext as _, HeadlessAppContext, px, size};

use super::TuiView;
use super::layout::Hidden;

pub struct Args {
    path: PathBuf,
    w: f32,
    h: f32,
    hidden: Hidden,
}

/// `None` when `--shot` is absent.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let path = PathBuf::from(val("--shot")?);
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1400., 900.));
    let mut hidden = Hidden::default();
    for k in val("--hide").iter().flat_map(|s| s.split(',')) {
        if !hidden.hide(k) {
            eprintln!("synthesis-desktop: --hide: {k} is not a panel key");
        }
    }
    Some(Args { path, w, h, hidden })
}

pub fn render(a: Args) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(crate::brand::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::ui::register_fonts(cx));
    let hidden = a.hidden;
    let win = cx.open_window(size(px(a.w), px(a.h)), |_, cx| {
        cx.new(|_| TuiView::new(0., hidden))
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(&a.path)?;
    eprintln!(
        "wrote {} ({}x{})",
        a.path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
