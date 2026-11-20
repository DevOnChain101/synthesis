//! The person's input to the page (the desktop pane, through `PageView` only): validated events and the CDP
//! `Input.*` parameters they become. Nothing here is reachable from the agent's tools.

use serde_json::{Value, json};

use crate::{BrowserError, Result};

/// Longest `Input.insertText` (characters): a paste or an IME commit, never a bulk channel.
pub const MAX_TEXT: usize = 32 * 1024;
/// Pointer coordinates are CSS pixels of the viewport, which is at most `ViewMetrics::MAX_PIXELS` wide.
const MAX_COORD: f64 = 4096.0;
const MAX_WHEEL: f64 = 5000.0;

/// Modifier bits as CDP has them.
pub const ALT: u8 = 1;
pub const CTRL: u8 = 2;
pub const META: u8 = 4;
pub const SHIFT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageButton {
    Left,
    Middle,
    Right,
}

impl PageButton {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }
    fn bit(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
}

/// One thing the person did in the pane. Positions are CSS pixels of the page viewport.
#[derive(Clone, Debug, PartialEq)]
pub enum PageInput {
    /// `buttons`: bit 1 left, 2 right, 4 middle, as held down.
    Move { x: f64, y: f64, buttons: u8 },
    Button {
        down: bool,
        x: f64,
        y: f64,
        button: PageButton,
        clicks: u8,
        mods: u8,
    },
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        mods: u8,
    },
    /// A whole keystroke (down, then up): a DOM `key` name (`Enter`, `ArrowLeft`, `a`).
    Key { key: String, mods: u8 },
    /// Text for `Input.insertText` (paste, IME commit, a character the key table has no code for).
    Text(String),
}

impl PageInput {
    /// Keys and text can type a secret; the pointer cannot.
    pub fn types(&self) -> bool {
        matches!(self, Self::Key { .. } | Self::Text(_))
    }

    pub(crate) fn check_text(t: &str) -> Result<()> {
        if t.is_empty() || t.chars().count() > MAX_TEXT {
            return Err(BrowserError::Page(format!(
                "text must be 1..={MAX_TEXT} characters"
            )));
        }
        Ok(())
    }

    /// `Input.dispatchMouseEvent` parameters of a pointer event (`None` for keys and text).
    pub(crate) fn mouse_params(&self) -> Result<Option<Value>> {
        let at = |x: f64, y: f64| {
            if [x, y]
                .iter()
                .all(|v| v.is_finite() && (0.0..=MAX_COORD).contains(v))
            {
                Ok(())
            } else {
                Err(BrowserError::Page("pointer outside the page".into()))
            }
        };
        Ok(Some(match *self {
            Self::Move { x, y, buttons } => {
                at(x, y)?;
                let held = buttons & 7;
                let b = [PageButton::Left, PageButton::Right, PageButton::Middle]
                    .into_iter()
                    .find(|b| held & b.bit() != 0);
                json!({"type": "mouseMoved", "x": x, "y": y, "button": b.map_or("none", PageButton::name), "buttons": held})
            }
            Self::Button {
                down,
                x,
                y,
                button,
                clicks,
                mods,
            } => {
                at(x, y)?;
                json!({"type": if down { "mousePressed" } else { "mouseReleased" }, "x": x, "y": y,
                    "button": button.name(), "buttons": if down { button.bit() } else { 0 },
                    "clickCount": clicks.clamp(1, 3), "modifiers": mods & 15})
            }
            Self::Wheel { x, y, dx, dy, mods } => {
                at(x, y)?;
                if !(dx.is_finite() && dy.is_finite()) {
                    return Err(BrowserError::Page("wheel delta is not a number".into()));
                }
                json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx.clamp(-MAX_WHEEL, MAX_WHEEL),
                    "deltaY": dy.clamp(-MAX_WHEEL, MAX_WHEEL), "modifiers": mods & 15})
            }
            _ => return Ok(None),
        }))
    }
}

/// The editing command a Ctrl/Cmd chord stands for (Chrome runs no command from a synthesized key otherwise).
fn command(key: &str, mods: u8) -> Option<&'static str> {
    match (key.to_ascii_lowercase().as_str(), mods & SHIFT != 0) {
        ("a", _) => Some("selectAll"),
        ("z", false) => Some("undo"),
        ("z", true) => Some("redo"),
        _ => None,
    }
}

/// The key-down and key-up events of `key` with `mods` held. Ctrl or Cmd turns a character into a shortcut: no text.
pub(crate) fn key_events(key: &str, mods: u8) -> Option<[Value; 2]> {
    let (k, code, vk, text) = crate::key_def(key)?;
    let mods = mods & 15;
    let text = text.filter(|_| mods & (CTRL | META) == 0);
    let mut down = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" }, "key": k, "code": code, "windowsVirtualKeyCode": vk});
    if let Some(t) = &text {
        down["text"] = json!(t);
    }
    let mut up = json!({"type": "keyUp", "key": k, "code": code, "windowsVirtualKeyCode": vk});
    if mods != 0 {
        down["modifiers"] = json!(mods);
        up["modifiers"] = json!(mods);
    }
    if mods & (CTRL | META) != 0
        && let Some(c) = command(&k, mods)
    {
        down["commands"] = json!([c]);
    }
    Some([down, up])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_modifiers_text_and_commands() {
        let [d, u] = key_events("a", 0).unwrap();
        assert_eq!(
            (d["type"].as_str(), d["text"].as_str()),
            (Some("keyDown"), Some("a"))
        );
        assert_eq!(u["type"], "keyUp");
        assert!(d.get("modifiers").is_none());
        // Ctrl/Cmd+A is a shortcut: no text, a select-all command
        let [d, u] = key_events("a", CTRL | SHIFT).unwrap();
        assert_eq!(d["type"], "rawKeyDown");
        assert!(d.get("text").is_none());
        assert_eq!(
            (d["modifiers"].clone(), u["modifiers"].clone()),
            (json!(10), json!(10))
        );
        assert_eq!(d["commands"], json!(["selectAll"]));
        assert_eq!(
            key_events("z", META).unwrap()[0]["commands"],
            json!(["undo"])
        );
        assert_eq!(
            key_events("Z", META | SHIFT).unwrap()[0]["commands"],
            json!(["redo"])
        );
        // named keys keep their code and virtual key; unknown names are refused
        let [d, _] = key_events("ArrowLeft", ALT).unwrap();
        assert_eq!(
            (d["windowsVirtualKeyCode"].clone(), d["modifiers"].clone()),
            (json!(37), json!(1))
        );
        assert_eq!(key_events("Enter", 0).unwrap()[0]["text"], "\r");
        assert!(key_events("F13", 0).is_none() && key_events("ab", 0).is_none());
    }

    #[test]
    fn pointer_events_are_validated_and_bounded() {
        let p = PageInput::Button {
            down: true,
            x: 10.5,
            y: 20.0,
            button: PageButton::Right,
            clicks: 9,
            mods: 255,
        };
        let v = p.mouse_params().unwrap().unwrap();
        assert_eq!(
            (v["type"].as_str(), v["button"].as_str()),
            (Some("mousePressed"), Some("right"))
        );
        assert_eq!(
            (
                v["clickCount"].clone(),
                v["modifiers"].clone(),
                v["buttons"].clone()
            ),
            (json!(3), json!(15), json!(2))
        );
        let m = PageInput::Move {
            x: 1.0,
            y: 2.0,
            buttons: 0xff,
        }
        .mouse_params()
        .unwrap()
        .unwrap();
        assert_eq!(
            (m["button"].as_str(), m["buttons"].clone()),
            (Some("left"), json!(7))
        );
        let w = PageInput::Wheel {
            x: 1.0,
            y: 2.0,
            dx: 1e9,
            dy: -1e9,
            mods: 0,
        }
        .mouse_params()
        .unwrap()
        .unwrap();
        assert_eq!(
            (w["deltaX"].clone(), w["deltaY"].clone()),
            (json!(5000.0), json!(-5000.0))
        );
        for bad in [f64::NAN, f64::INFINITY, -1.0, 4097.0] {
            assert!(
                PageInput::Move {
                    x: bad,
                    y: 0.0,
                    buttons: 0
                }
                .mouse_params()
                .is_err(),
                "{bad}"
            );
        }
        assert!(
            PageInput::Wheel {
                x: 0.0,
                y: 0.0,
                dx: f64::NAN,
                dy: 0.0,
                mods: 0
            }
            .mouse_params()
            .is_err()
        );
        assert!(
            PageInput::Text("x".into())
                .mouse_params()
                .unwrap()
                .is_none()
        );
        assert!(PageInput::check_text("").is_err());
        assert!(PageInput::check_text(&"x".repeat(MAX_TEXT + 1)).is_err());
        assert!(PageInput::check_text(&"é".repeat(MAX_TEXT)).is_ok());
    }
}
