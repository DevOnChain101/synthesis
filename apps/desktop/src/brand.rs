//! Brand assets and font roles (docs/specs/desktop.md section 8). The mark comes from
//! `design/01-brand/logo`, embedded in the binary; chrome text uses the UI sans (Inter, bundled
//! under the SIL OFL) and the user-selected monospace is for code, diff, terminal and tokens only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{Image, ImageFormat};

/// The symbol with transparent background (256 px), for the rail.
pub const MARK_PNG: &[u8] =
    include_bytes!("../../../design/01-brand/logo/png/synthesis-symbol-transparent-256.png");

/// Family name of the bundled UI sans.
pub const UI_FONT: &str = "Inter";

pub const UI_FONT_FILES: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/Inter-Regular.otf"),
    include_bytes!("../assets/fonts/Inter-Medium.otf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.otf"),
];

/// Family name of the monospace of the TUI and Outcomes views (`cli.html` `--mono`, first choice).
pub const MONO_FONT: &str = "Fira Code";

/// Fira Code regular, medium (the design's body weight) and bold, SIL OFL.
pub const MONO_FONT_FILES: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/FiraCode-Regular.ttf"),
    include_bytes!("../assets/fonts/FiraCode-Medium.ttf"),
    include_bytes!("../assets/fonts/FiraCode-Bold.ttf"),
];

pub fn mark() -> Arc<Image> {
    static MARK: OnceLock<Arc<Image>> = OnceLock::new();
    MARK.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, MARK_PNG.to_vec())))
        .clone()
}

/// The mark as a texture of exactly `logical * scale` device pixels. gpui samples a texture bilinearly without
/// mipmaps, so drawing the 256 px PNG at 16-96 px skips most source pixels and the edge turns into stairs; this
/// resamples once (Lanczos in premultiplied alpha, no fringe) and caches per size.
fn mark_at(device: u32) -> Arc<gpui::RenderImage> {
    use image::{RgbaImage, imageops};
    static CACHE: OnceLock<Mutex<HashMap<u32, Arc<gpui::RenderImage>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry(device)
        .or_insert_with(|| {
            let mut img = image::load_from_memory(MARK_PNG).unwrap().into_rgba8();
            let premul = |img: &mut RgbaImage, up: bool| {
                for p in img.pixels_mut() {
                    let a = p[3] as f32 / 255.;
                    let max = if up { 255. } else { p[3] as f32 };
                    for c in &mut p.0[..3] {
                        let v = if up {
                            *c as f32 / a.max(1. / 255.)
                        } else {
                            *c as f32 * a
                        };
                        *c = v.round().min(max) as u8;
                    }
                }
            };
            if device < img.width() {
                premul(&mut img, false);
                img = imageops::resize(&img, device, device, imageops::FilterType::Lanczos3);
                premul(&mut img, true);
            }
            for p in img.pixels_mut() {
                p.0.swap(0, 2); // gpui wants BGRA
            }
            Arc::new(gpui::RenderImage::new(vec![image::Frame::new(img)]))
        })
        .clone()
}

/// The mark drawn at `size` logical px, sharp at the window's scale factor.
pub fn mark_img(size: f32) -> gpui::Img {
    gpui::img(gpui::ImageSource::Custom(Arc::new(move |window, _| {
        let device = (size * window.scale_factor()).round().max(1.) as u32;
        Some(Ok(mark_at(device)))
    })))
}

gpui_kit_assets::icon_assets!(
    Extra,
    [
        GitBranch,
        ChevronsRight,
        Activity,
        Wrench,
        Clock,
        Sparkles,
        ChartBar,
        MessageSquare,
        Send,
        Webhook,
        Shield,
        Users,
        Layers,
        Database,
        GitCompare,
        Link,
    ]
);

/// The app's asset source: the default gpui-kit (Lucide) icon set plus the few icons of the
/// `cli.html` chrome that the default set lacks (embedded one by one, not the whole 7 MB catalogue).
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match Extra.load(path)? {
            Some(b) => Ok(Some(b)),
            None => gpui_kit_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut v = gpui_kit_assets::Assets.list(path)?;
        v.extend(Extra.list(path)?);
        Ok(v)
    }
}

/// What a piece of text is: decides the family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Sidebars, titles, buttons, chat prose, labels.
    Chrome,
    /// Code, diff, terminal, tool commands, tokens.
    Code,
}

pub fn family(role: Role, code_font: &str) -> &str {
    match role {
        Role::Chrome => UI_FONT,
        Role::Code => code_font,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_is_a_png_and_embedded() {
        assert!(MARK_PNG.starts_with(b"\x89PNG"));
        assert!(MARK_PNG.len() > 1000);
    }

    #[test]
    fn inter_is_embedded_and_declares_its_family() {
        for f in UI_FONT_FILES {
            assert!(f.starts_with(b"OTTO"), "OpenType/CFF");
            assert!(f.windows(5).any(|w| w == b"Inter"));
        }
    }

    #[test]
    fn every_icon_the_shell_draws_loads() {
        use gpui::AssetSource as _;
        for name in crate::ui::ICONS {
            assert!(
                Assets.load(&format!("icons/{name}.svg")).unwrap().is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn chrome_is_sans_and_code_is_the_chosen_mono() {
        assert_eq!(family(Role::Chrome, "Fira Code"), "Inter");
        assert_eq!(family(Role::Code, "Fira Code"), "Fira Code");
    }
}
