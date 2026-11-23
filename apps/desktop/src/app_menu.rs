//! The native application menu bar (macOS shows it at the top of the screen; elsewhere gpui ignores it).
//!
//! Key rules: the Edit items bind NO key (Cmd+C/V/X/A/Z already reach the focused control through its own
//! key handling, a bound key would fire twice); clicking one replays that keystroke on the active window,
//! so the menu and the keyboard run the same code. Cmd+Q, Cmd+H, Cmd+M, Cmd+, and Cmd+1/2 are bound here
//! because nothing else in the app handles them (the shell's own Cmd+,/1/2 hook is then only a fallback).

use gpui::{
    App, KeyBinding, Keystroke, Menu, MenuItem, OsAction, PromptLevel, SystemMenuType, actions,
};

use crate::ui::{Shell, View};

actions!(
    synthesis_menu,
    [
        About,
        Settings,
        ViewTui,
        ViewSuperAgent,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        ToggleFullScreen,
        Docs,
        Undo,
        Redo,
        Cut,
        Copy,
        Paste,
        SelectAll,
    ]
);

/// The repo (`packaging/REPO`); its README is the documentation entry.
pub const DOCS_URL: &str = "https://github.com/DevOnChain101/synthesis#readme";

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Synthesis").items([
            MenuItem::action("About Synthesis", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", Settings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Synthesis", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Synthesis", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("TUI", ViewTui),
            MenuItem::action("Super Agent", ViewSuperAgent),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullScreen),
        ]),
        // gpui marks the menu named "Window" as the system window menu
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("Synthesis Documentation", Docs)]),
    ]
}

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("cmd-,", Settings, None),
        KeyBinding::new("cmd-1", ViewTui, None),
        KeyBinding::new("cmd-2", ViewSuperAgent, None),
    ]
}

/// What an Edit item does: the keystroke the focused control would have got from the keyboard.
fn edit_key(action: &str) -> Option<&'static str> {
    Some(match action {
        "Undo" => "cmd-z",
        "Redo" => "cmd-shift-z",
        "Cut" => "cmd-x",
        "Copy" => "cmd-c",
        "Paste" => "cmd-v",
        "SelectAll" => "cmd-a",
        _ => return None,
    })
}

fn with_shell(
    cx: &mut App,
    f: impl FnOnce(&mut Shell, &mut gpui::Window, &mut gpui::Context<Shell>),
) {
    if let Some(w) = cx.active_window().and_then(|w| w.downcast::<Shell>()) {
        let _ = w.update(cx, |s, window, cx| f(s, window, cx));
    }
}

fn replay(cx: &mut App, action: &str) {
    let Some(ks) = edit_key(action).and_then(|k| Keystroke::parse(k).ok()) else {
        return;
    };
    if let Some(w) = cx.active_window() {
        let _ = w.update(cx, |_, window, cx| window.dispatch_keystroke(ks, cx));
    }
}

/// Registers the actions; on macOS also the keys and the menu bar.
pub fn install(cx: &mut App) {
    cx.on_action(|_: &About, cx| {
        with_shell(cx, |_, window, cx| {
            let version = format!("Version {}", env!("CARGO_PKG_VERSION"));
            let rx = window.prompt(PromptLevel::Info, "Synthesis", Some(&version), &["OK"], cx);
            window
                .spawn(cx, async move |_| {
                    let _ = rx.await;
                })
                .detach();
        })
    });
    cx.on_action(|_: &Settings, cx| with_shell(cx, |s, window, cx| s.toggle_settings(window, cx)));
    cx.on_action(|_: &ViewTui, cx| with_shell(cx, |s, w, cx| s.go(View::Tui, w, cx)));
    cx.on_action(|_: &ViewSuperAgent, cx| with_shell(cx, |s, w, cx| s.go(View::SuperAgent, w, cx)));
    // the app's own quit path (engine shutdown, then `cx.quit`), same as /quit
    cx.on_action(|_: &Quit, cx| with_shell(cx, |s, _, cx| s.quit(cx)));
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| with_shell(cx, |_, w, _| w.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_shell(cx, |_, w, _| w.zoom_window()));
    cx.on_action(|_: &ToggleFullScreen, cx| with_shell(cx, |_, w, _| w.toggle_fullscreen()));
    cx.on_action(|_: &Docs, cx| cx.open_url(DOCS_URL));
    cx.on_action(|_: &Undo, cx| replay(cx, "Undo"));
    cx.on_action(|_: &Redo, cx| replay(cx, "Redo"));
    cx.on_action(|_: &Cut, cx| replay(cx, "Cut"));
    cx.on_action(|_: &Copy, cx| replay(cx, "Copy"));
    cx.on_action(|_: &Paste, cx| replay(cx, "Paste"));
    cx.on_action(|_: &SelectAll, cx| replay(cx, "SelectAll"));
    if cfg!(target_os = "macos") {
        cx.bind_keys(bindings());
        cx.set_menus(menus());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action as _;
    use std::collections::HashSet;

    fn names(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                MenuItem::SystemMenu(m) => Some(m.name.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn menu_order_and_names() {
        let m = menus();
        let top: Vec<_> = m.iter().map(|m| m.name.to_string()).collect();
        assert_eq!(top, ["Synthesis", "Edit", "View", "Window", "Help"]);
        assert_eq!(
            names(&m[0].items),
            [
                "About Synthesis",
                "Settings…",
                "Services",
                "Hide Synthesis",
                "Hide Others",
                "Show All",
                "Quit Synthesis"
            ]
        );
        assert_eq!(
            names(&m[1].items),
            ["Undo", "Redo", "Cut", "Copy", "Paste", "Select All"]
        );
    }

    #[test]
    fn quit_item_and_key_are_the_guarded_action() {
        let quit = Quit.name();
        let has = |i: &MenuItem| matches!(i, MenuItem::Action { name, action, .. } if name.as_ref() == "Quit Synthesis" && action.name() == quit);
        assert!(menus()[0].items.iter().any(has));
        assert!(bindings().iter().any(|b| b.action().name() == quit
            && b.keystrokes()[0].unparse(gpui::OperatingSystem::Mac) == "cmd-q"));
    }

    #[test]
    fn keys_are_unique_and_leave_the_text_keys_alone() {
        let keys: Vec<String> = bindings()
            .iter()
            .map(|b| b.keystrokes()[0].unparse(gpui::OperatingSystem::Mac))
            .collect();
        assert_eq!(keys.iter().collect::<HashSet<_>>().len(), keys.len());
        for k in &keys {
            assert!(
                !["cmd-c", "cmd-v", "cmd-x", "cmd-a", "cmd-z", "cmd-shift-z"].contains(&k.as_str()),
                "{k} belongs to the focused control"
            );
        }
        // every Edit item replays a key; none of them is bound as a menu key
        for i in &menus()[1].items {
            if let MenuItem::Action { action, .. } = i {
                let n = action
                    .name()
                    .rsplit("::")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                assert!(edit_key(&n).is_some(), "{n}");
                assert!(!keys.contains(&edit_key(&n).unwrap().to_string()));
            }
        }
    }
}
