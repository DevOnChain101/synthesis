//! Brand assets and font roles (docs/specs/desktop.md section 8). The mark comes from
//! `design/01-brand/logo`, embedded in the binary; chrome text uses the UI sans (Inter, bundled
//! under the SIL OFL) and the user-selected monospace is for code, diff, terminal and tokens only.

use std::sync::{Arc, OnceLock};

use gpui::{Image, ImageFormat};

/// The symbol with transparent background (256 px), for the rail, the landing and the topology.
pub const MARK_PNG: &[u8] =
    include_bytes!("../../../design/01-brand/logo/png/synthesis-symbol-transparent-256.png");

/// Family name of the bundled UI sans.
pub const UI_FONT: &str = "Inter";

pub const UI_FONT_FILES: [&[u8]; 2] = [
    include_bytes!("../assets/fonts/Inter-Regular.otf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.otf"),
];

pub fn mark() -> Arc<Image> {
    static MARK: OnceLock<Arc<Image>> = OnceLock::new();
    MARK.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, MARK_PNG.to_vec())))
        .clone()
}

// Lucide icons the design uses beyond gpui-kit's default set (one icon set, one stroke weight).
gpui_kit_assets::icon_assets!(ExtraIcons, [Activity, GitBranch, Wrench, Zap]);

/// The app's asset source: the default gpui-kit set plus [`ExtraIcons`].
pub struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(b) = ExtraIcons.load(path)? {
            return Ok(Some(b));
        }
        gpui_kit_assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut all = gpui_kit_assets::Assets.list(path)?;
        all.extend(ExtraIcons.list(path)?);
        all.sort();
        all.dedup();
        Ok(all)
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
    fn every_icon_the_ui_draws_loads() {
        use gpui::AssetSource as _;
        let src = include_str!("ui.rs").to_string() + include_str!("ui/views.rs");
        // literal `icon("x", ..)` calls plus the names passed to `nav_row`, `page` and the chips
        let named = [
            "activity",
            "wrench",
            "network",
            "git-branch",
            "cpu",
            "folder",
            "search",
            "inbox",
            "settings",
        ];
        let mut n = 0;
        for name in src
            .split("icon(\"")
            .skip(1)
            .map(|p| p.split('"').next().unwrap())
            .chain(named)
        {
            let path = format!("icons/{name}.svg");
            assert!(Assets.load(&path).unwrap().is_some(), "{path}");
            n += 1;
        }
        assert!(n > 15);
    }

    #[test]
    fn chrome_is_sans_and_code_is_the_chosen_mono() {
        assert_eq!(family(Role::Chrome, "Fira Code"), "Inter");
        assert_eq!(family(Role::Code, "Fira Code"), "Fira Code");
    }
}
