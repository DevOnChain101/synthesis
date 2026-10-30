//! `synthesis.tui` of the USER settings file (spec docs/specs/tui-design.md §7): theme, glyphs, logo,
//! which panels are on, the chat and mouse switches. Project, local and managed files never set it
//! (`synthesis-settings` reports that). `/config` edits these and writes them back on close.

use serde_json::{Map, Value, json};
use std::path::Path;
use synthesis_config::Env;
use synthesis_tui_base::sgr::ColorMode;
use synthesis_tui_render::layout::{PanelId, Panels, REGISTRY};
use synthesis_tui_render::theme::{RenderConfig, ThemeId, select_theme};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlyphChoice {
    #[default]
    Auto,
    Unicode,
    Ascii,
}

impl GlyphChoice {
    pub const CHOICES: [(&'static str, GlyphChoice); 3] = [
        ("auto", GlyphChoice::Auto),
        ("unicode", GlyphChoice::Unicode),
        ("ascii", GlyphChoice::Ascii),
    ];
}

/// `synthesis.tui.logo`. The inline-image form arrives with phase P4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogoChoice {
    #[default]
    Art,
    Off,
}

impl LogoChoice {
    pub const CHOICES: [(&'static str, LogoChoice); 2] =
        [("art", LogoChoice::Art), ("off", LogoChoice::Off)];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuiSettings {
    pub theme: ThemeId,
    pub glyphs: GlyphChoice,
    pub logo: LogoChoice,
    pub panels: Panels,
    pub show_thinking: bool,
    pub diffs_expanded: bool,
    pub queue_indicators: bool,
    pub motion: bool,
    pub mouse_capture: bool,
    pub copy_on_select: bool,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            theme: ThemeId::Synthesis,
            glyphs: GlyphChoice::Auto,
            logo: LogoChoice::Art,
            panels: Panels::default(),
            show_thinking: true,
            diffs_expanded: false,
            queue_indicators: true,
            motion: true,
            mouse_capture: true,
            copy_on_select: true,
        }
    }
}

fn choice<T: Copy>(choices: &[(&str, T)], name: &str) -> Option<T> {
    choices.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

fn name_of<T: Copy + PartialEq>(choices: &[(&'static str, T)], v: T) -> &'static str {
    choices.iter().find(|(_, c)| *c == v).map_or("", |(n, _)| n)
}

impl TuiSettings {
    pub fn theme_name(&self) -> &'static str {
        name_of(&ThemeId::CHOICES, self.theme)
    }
    pub fn glyphs_name(&self) -> &'static str {
        name_of(&GlyphChoice::CHOICES, self.glyphs)
    }
    pub fn logo_name(&self) -> &'static str {
        name_of(&LogoChoice::CHOICES, self.logo)
    }

    /// Reads `synthesis.tui` of a parsed settings file. A wrong type or an unknown value keeps the
    /// default and yields a diagnostic that names the key, never the value.
    pub fn from_settings(v: &Value) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut diags = vec![];
        let Some(tui) = v.get("synthesis").and_then(|n| n.get("tui")) else {
            return (s, diags);
        };
        let Some(tui) = tui.as_object() else {
            diags.push("synthesis.tui is not an object, ignored".into());
            return (s, diags);
        };
        match tui
            .get("theme")
            // auto: the default colour theme; 16 colours and NO_COLOR pick theirs in `render_config`
            .map(|t| {
                t.as_str().and_then(|n| {
                    if n == "auto" {
                        Some(s.theme)
                    } else {
                        ThemeId::parse(n)
                    }
                })
            }) {
            Some(Some(t)) => s.theme = t,
            Some(None) => {
                diags.push("synthesis.tui.theme is not a known theme, using synthesis".into())
            }
            None => {}
        }
        match tui.get("glyphs").map(Value::as_str) {
            Some(Some(n)) if choice(&GlyphChoice::CHOICES, n).is_some() => {
                s.glyphs = choice(&GlyphChoice::CHOICES, n).unwrap_or_default()
            }
            None => {}
            Some(_) => {
                diags.push("synthesis.tui.glyphs must be auto, unicode or ascii, using auto".into())
            }
        }
        match tui.get("logo").map(Value::as_str) {
            Some(Some(n)) if choice(&LogoChoice::CHOICES, n).is_some() => {
                s.logo = choice(&LogoChoice::CHOICES, n).unwrap_or_default()
            }
            None => {}
            Some(_) => diags.push("synthesis.tui.logo must be art or off, using art".into()),
        }
        if let Some(p) = tui.get("panels") {
            match p.as_object() {
                None => diags.push("synthesis.tui.panels is not an object, ignored".into()),
                Some(p) => {
                    for (k, v) in p {
                        let Some(id) = PanelId::parse(k) else {
                            diags.push(format!("synthesis.tui.panels.{k} is not a panel, ignored"));
                            continue;
                        };
                        if REGISTRY[id as usize].locked.is_some() {
                            diags.push(format!(
                                "synthesis.tui.panels.{k}: {k} is always visible, ignored"
                            ));
                        } else if let Some(on) = v.as_bool() {
                            s.panels.set(id, on);
                        } else {
                            diags.push(format!(
                                "synthesis.tui.panels.{k} must be true or false, using the default"
                            ));
                        }
                    }
                }
            }
        }
        let mut flag = |obj: &str, key: &str, slot: &mut bool| {
            let v = if obj.is_empty() {
                tui.get(key)
            } else {
                tui.get(obj).and_then(|o| o.get(key))
            };
            let path = if obj.is_empty() {
                key.to_string()
            } else {
                format!("{obj}.{key}")
            };
            match v.map(Value::as_bool) {
                Some(Some(b)) => *slot = b,
                None => {}
                Some(None) => diags.push(format!(
                    "synthesis.tui.{path} must be true or false, using the default"
                )),
            }
        };
        flag("chat", "show_thinking", &mut s.show_thinking);
        flag("chat", "diffs_expanded", &mut s.diffs_expanded);
        flag("chat", "queue_indicators", &mut s.queue_indicators);
        flag("", "motion", &mut s.motion);
        flag("mouse", "capture", &mut s.mouse_capture);
        flag("mouse", "copy_on_select", &mut s.copy_on_select);
        (s, diags)
    }

    /// The `synthesis.tui` subtree `/config` writes: every key, with its current value.
    pub fn to_value(&self) -> Value {
        let mut panels = Map::new();
        for sp in REGISTRY.iter().filter(|p| p.locked.is_none()) {
            panels.insert(sp.key.into(), json!(self.panels.is_on(sp.id)));
        }
        json!({
            "theme": self.theme_name(),
            "glyphs": self.glyphs_name(),
            "logo": self.logo_name(),
            "panels": panels,
            "chat": {
                "show_thinking": self.show_thinking,
                "diffs_expanded": self.diffs_expanded,
                "queue_indicators": self.queue_indicators,
            },
            "motion": self.motion,
            "mouse": {"capture": self.mouse_capture, "copy_on_select": self.copy_on_select},
        })
    }

    /// `<synthesis_home>/settings.json`. A missing file is the defaults; an unreadable one is the
    /// defaults plus a diagnostic (the file itself is never touched).
    pub fn load_user(home: &Path) -> (Self, Vec<String>) {
        let path = home.join("settings.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (Self::default(), vec![]);
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => Self::from_settings(&v),
            Err(_) => (
                Self::default(),
                vec!["settings.json is unreadable; using the default theme".into()],
            ),
        }
    }

    /// The theme and glyph set for this terminal: `NO_COLOR` and 16 colours override the theme;
    /// `SYNTHESIS_ASCII` and a basic `TERM` choose ASCII glyphs unless the user said otherwise.
    pub fn render_config(&self, mode: ColorMode, env: &Env) -> RenderConfig {
        let ascii = match self.glyphs {
            GlyphChoice::Ascii => true,
            GlyphChoice::Unicode => false,
            GlyphChoice::Auto => match env.get_nonempty("SYNTHESIS_ASCII") {
                Some("1" | "true") => true,
                Some("0" | "false") => false,
                _ => matches!(
                    env.get("TERM"),
                    Some("linux" | "vt100" | "vt220" | "cons25" | "dumb")
                ),
            },
        };
        RenderConfig {
            theme: select_theme(mode, self.theme),
            ascii,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn theme_and_glyphs_are_read_and_bad_values_keep_defaults() {
        let (s, d) = TuiSettings::from_settings(
            &json!({"synthesis": {"tui": {"theme": "pearl", "glyphs": "ascii"}}}),
        );
        assert_eq!(
            (s.theme, s.glyphs, d.len()),
            (ThemeId::Pearl, GlyphChoice::Ascii, 0)
        );
        let (s, d) = TuiSettings::from_settings(
            &json!({"synthesis": {"tui": {"theme": "neon", "glyphs": 3}}}),
        );
        assert_eq!(s, TuiSettings::default());
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|m| !m.contains("neon")), "{d:?}");
        assert_eq!(TuiSettings::from_settings(&json!({})).1.len(), 0);
        // the default is synthesis; every named theme and `auto` are accepted
        assert_eq!(TuiSettings::default().theme, ThemeId::Synthesis);
        for (name, want) in [
            ("classic", ThemeId::Classic),
            ("graphite", ThemeId::Graphite),
            ("synthesis", ThemeId::Synthesis),
            ("auto", ThemeId::Synthesis),
        ] {
            let (s, d) =
                TuiSettings::from_settings(&json!({"synthesis": {"tui": {"theme": name}}}));
            assert_eq!((s.theme, d.len()), (want, 0), "{name}");
        }
    }

    #[test]
    fn no_color_and_16_colours_override_the_theme_and_ascii_follows_the_env() {
        let s = TuiSettings {
            theme: ThemeId::Midnight,
            ..Default::default()
        };
        let e = env(&[]);
        assert_eq!(
            s.render_config(ColorMode::TrueColor, &e).theme,
            ThemeId::Midnight
        );
        assert_eq!(s.render_config(ColorMode::Mono, &e).theme, ThemeId::Mono);
        assert_eq!(
            s.render_config(ColorMode::Ansi16, &e).theme,
            ThemeId::Ansi16
        );
        assert!(!s.render_config(ColorMode::TrueColor, &e).ascii);
        assert!(
            s.render_config(ColorMode::TrueColor, &env(&[("SYNTHESIS_ASCII", "1")]))
                .ascii
        );
        assert!(
            s.render_config(ColorMode::TrueColor, &env(&[("TERM", "linux")]))
                .ascii
        );
        assert!(
            s.render_config(ColorMode::TrueColor, &env(&[("TERM", "dumb")]))
                .ascii
        );
        assert!(
            !s.render_config(
                ColorMode::TrueColor,
                &env(&[("TERM", "linux"), ("SYNTHESIS_ASCII", "0")])
            )
            .ascii
        );
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn panels_and_switches_are_read_and_bad_keys_are_named_without_their_values() {
        let (s, d) = TuiSettings::from_settings(&json!({"synthesis": {"tui": {
            "panels": {"session": false, "topology": false, "workspace": false, "nope": true, "context": "yes"},
            "logo": "off", "motion": false, "chat": {"show_thinking": false, "diffs_expanded": 3},
            "mouse": {"capture": false}
        }}}));
        assert!(!s.panels.is_on(PanelId::Session) && !s.panels.is_on(PanelId::Topology));
        assert!(
            s.panels.is_on(PanelId::Workspace),
            "locked panels cannot be hidden"
        );
        assert!(
            s.panels.is_on(PanelId::Context),
            "a wrong type keeps the default"
        );
        assert_eq!(
            (
                s.logo,
                s.motion,
                s.show_thinking,
                s.diffs_expanded,
                s.mouse_capture
            ),
            (LogoChoice::Off, false, false, false, false)
        );
        assert_eq!(d.len(), 4, "{d:?}");
        assert!(d.iter().any(|m| m.contains("panels.nope is not a panel")));
        assert!(
            d.iter()
                .any(|m| m.contains("workspace") && m.contains("always visible"))
        );
        assert!(d.iter().all(|m| !m.contains("yes")), "{d:?}");
    }

    #[test]
    fn to_value_round_trips_and_lists_only_hideable_panels() {
        let mut s = TuiSettings {
            theme: ThemeId::Pearl,
            motion: false,
            ..Default::default()
        };
        s.panels.set(PanelId::Topology, false);
        let v = s.to_value();
        assert!(v["panels"].get("workspace").is_none() && v["panels"].get("subagents").is_none());
        assert_eq!(v["panels"]["topology"], false);
        let (back, d) = TuiSettings::from_settings(&json!({"synthesis": {"tui": v}}));
        assert_eq!((back, d.len()), (s, 0));
        // the defaults are the schema of the spec
        let d = TuiSettings::default().to_value();
        assert_eq!(
            (
                d["theme"].as_str(),
                d["glyphs"].as_str(),
                d["logo"].as_str()
            ),
            (Some("synthesis"), Some("auto"), Some("art"))
        );
    }
}
