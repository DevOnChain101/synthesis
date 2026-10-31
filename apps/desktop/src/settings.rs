//! `synthesis.desktop.*` in the USER `settings.json` (never the project's; docs/specs/desktop.md
//! section 6): `mode`, `plainTerminal`, `font`. No secrets.

use std::path::Path;

use serde_json::{Map, Value};

/// Bundled OFL fonts (apps/desktop/assets/fonts) plus the system font. The default is monospace
/// for code, diff and terminal only; the interface uses Inter (`brand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontChoice {
    #[default]
    JetBrainsMono,
    FiraCode,
    IbmPlexMono,
    System,
}

impl FontChoice {
    pub const ALL: [FontChoice; 4] = [
        FontChoice::JetBrainsMono,
        FontChoice::FiraCode,
        FontChoice::IbmPlexMono,
        FontChoice::System,
    ];

    /// The value stored in `synthesis.desktop.font`.
    pub fn key(self) -> &'static str {
        match self {
            FontChoice::JetBrainsMono => "jetbrains-mono",
            FontChoice::FiraCode => "fira-code",
            FontChoice::IbmPlexMono => "ibm-plex-mono",
            FontChoice::System => "system",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.key() == k)
    }

    pub fn label(self) -> &'static str {
        match self {
            FontChoice::JetBrainsMono => "JetBrains Mono",
            FontChoice::FiraCode => "Fira Code",
            FontChoice::IbmPlexMono => "IBM Plex Mono",
            FontChoice::System => "System font",
        }
    }

    /// Family name as the font file declares it (what GPUI resolves).
    pub fn family(self) -> &'static str {
        match self {
            FontChoice::System => ".SystemUIFont",
            f => f.label(),
        }
    }
}

/// Which interface the window shows: the TUI renderer in a terminal grid, or the native GPUI views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    Tui,
    #[default]
    Gpui,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Tui => "tui",
            Mode::Gpui => "gpui",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DesktopSettings {
    pub mode: Mode,
    /// Mode 1 only: a plain shell with a chat bar instead of the agent TUI.
    pub plain_terminal: bool,
    pub font: FontChoice,
}

fn desktop(v: &Value) -> Option<&Map<String, Value>> {
    v.get("synthesis")?.get("desktop")?.as_object()
}

/// Missing file, missing keys and unknown values all give the defaults.
pub fn load(home: &Path) -> DesktopSettings {
    let v: Value = std::fs::read_to_string(home.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let d = desktop(&v);
    let get = |k: &str| d.and_then(|d| d.get(k));
    DesktopSettings {
        mode: match get("mode").and_then(Value::as_str) {
            Some("tui") => Mode::Tui,
            _ => Mode::Gpui,
        },
        plain_terminal: get("plainTerminal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        font: get("font")
            .and_then(Value::as_str)
            .and_then(FontChoice::from_key)
            .unwrap_or_default(),
    }
}

/// Sets one `synthesis.desktop.<key>`; every other key is kept (atomic, see
/// `synthesis_settings::edit_json_atomic`).
pub fn set(home: &Path, key: &str, value: Value) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    synthesis_settings::edit_json_atomic(&home.join("settings.json"), "user settings", |v| {
        let section = v
            .as_object_mut()
            .ok_or_else(|| bad("settings are not an object"))?
            .entry("synthesis")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| bad("`synthesis` is not an object"))?
            .entry("desktop")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| bad("`synthesis.desktop` is not an object"))?;
        section.insert(key.into(), value);
        Ok(())
    })
}

pub fn save_font(home: &Path, f: FontChoice) -> std::io::Result<()> {
    set(home, "font", f.key().into())
}

pub fn save_mode(home: &Path, m: Mode) -> std::io::Result<()> {
    set(home, "mode", m.key().into())
}

/// The provider the person last connected: kind, id, base URL and model, never the key
/// (`synthesis.desktop.provider` in the user settings).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Remembered {
    /// `anthropic` or `openai` (the protocol name).
    pub kind: String,
    pub id: String,
    pub base_url: String,
    pub model: String,
}

pub fn load_remembered(home: &Path) -> Option<Remembered> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("settings.json")).ok()?).ok()?;
    let p = desktop(&v)?.get("provider")?;
    let text = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let r = Remembered {
        kind: text("kind"),
        id: text("id"),
        base_url: text("baseURL"),
        model: text("model"),
    };
    (!r.id.is_empty() && !r.base_url.is_empty()).then_some(r)
}

pub fn save_remembered(home: &Path, r: &Remembered) -> std::io::Result<()> {
    set(
        home,
        "provider",
        serde_json::json!({"kind": r.kind, "id": r.id, "baseURL": r.base_url, "model": r.model}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_without_a_file() {
        let d = tempfile::tempdir().unwrap();
        let s = load(d.path());
        assert_eq!(s.font, FontChoice::JetBrainsMono, "mono is the default");
        assert_eq!(s.mode, Mode::Gpui);
        assert!(!s.plain_terminal);
    }

    #[test]
    fn font_key_round_trips_and_keeps_other_settings() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("settings.json"),
            r#"{"permissions":{"allow":["Bash(ls)"]},"synthesis":{"tui":{"theme":"mono"},"desktop":{"plainTerminal":true}}}"#,
        )
        .unwrap();
        for f in FontChoice::ALL {
            save_font(d.path(), f).unwrap();
            assert_eq!(load(d.path()).font, f);
        }
        save_mode(d.path(), Mode::Tui).unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(v["synthesis"]["desktop"]["font"], "system");
        assert_eq!(v["synthesis"]["desktop"]["mode"], "tui");
        assert_eq!(v["synthesis"]["desktop"]["plainTerminal"], true);
        assert_eq!(v["synthesis"]["tui"]["theme"], "mono");
        assert_eq!(v["permissions"], json!({"allow": ["Bash(ls)"]}));
    }

    #[test]
    fn unknown_font_falls_back_and_malformed_file_is_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("settings.json"),
            r#"{"synthesis":{"desktop":{"font":"comic"}}}"#,
        )
        .unwrap();
        assert_eq!(load(d.path()).font, FontChoice::JetBrainsMono);
        std::fs::write(d.path().join("settings.json"), "{ broken").unwrap();
        assert!(save_font(d.path(), FontChoice::FiraCode).is_err());
        assert_eq!(
            std::fs::read_to_string(d.path().join("settings.json")).unwrap(),
            "{ broken"
        );
    }

    #[test]
    fn the_connected_provider_is_remembered_without_a_key() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_remembered(d.path()), None);
        let r = Remembered {
            kind: "openai".into(),
            id: "acme".into(),
            base_url: "https://x.example/v1".into(),
            model: "m1".into(),
        };
        save_remembered(d.path(), &r).unwrap();
        assert_eq!(load_remembered(d.path()), Some(r));
        let raw = std::fs::read_to_string(d.path().join("settings.json")).unwrap();
        assert!(!raw.to_lowercase().contains("key"), "{raw}");
    }
}
