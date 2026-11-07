//! Brand assets and font roles (docs/specs/desktop.md section 8). The mark comes from
//! `design/01-brand/logo`, embedded in the binary; chrome text uses the UI sans (Inter, bundled
//! under the SIL OFL) and the user-selected monospace is for code, diff, terminal and tokens only.

use std::sync::{Arc, OnceLock};

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

/// Family name of the monospace of the TUI and Agent views (`cli.html` `--mono`, first choice).
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
