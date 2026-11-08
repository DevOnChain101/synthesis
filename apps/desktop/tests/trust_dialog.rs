//! The Trust dialog the Files tab raises in an untrusted folder: the real engine on a scripted model, a
//! temp `SYNT_HOME`. Declining (Not now, Esc), type-ahead and the arming delay never trust anything.

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_desktop::engine::Engine;
use synthesis_desktop::live::{ARM_MS, Action, DialogKind, Live, PickerKind};

fn rig() -> (Live, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let sp = root.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let (engine, _rx) = Engine::start(
        &ws,
        &env,
        None,
        Some(&format!("fake:{}", sp.display())),
        Some("off"),
    )
    .unwrap();
    let mut live = Live::new();
    live.attach(Arc::new(engine), None);
    (live, dir)
}

#[test]
fn the_dialog_is_raised_once_with_the_safe_option_first_and_needs_a_deliberate_answer() {
    let (mut l, _d) = rig();
    assert!(!l.trusted, "no --trust-project: untrusted");
    assert!(l.ask_trust(1000));
    assert!(!l.ask_trust(1000), "a second click does not stack another");
    assert_eq!(l.dialogs.len(), 1);
    let d = &l.dialogs[0];
    assert!(matches!(
        d.kind,
        DialogKind::Picker {
            kind: PickerKind::Trust,
            ..
        }
    ));
    assert_eq!((d.sel, d.options[0].0.as_str()), (0, "no"));
    assert_eq!(d.options[1].0, "session");
    assert!(d.needs_person());
    // keys typed before the person engaged it cannot answer: Enter and digits go to the prompt
    assert!(!d.answerable());
    // a click inside the arming delay does not answer either
    assert_eq!(l.pick(1, 1000 + ARM_MS - 1), None);
    assert_eq!(l.dialogs.len(), 1);
    // after the delay, the explicit option answers: the window then restarts the session trusted
    let key = l.pick(1, 1000 + ARM_MS).unwrap();
    assert_eq!(l.answer(&key, 2000), Some(Action::TrustFolder));
    assert!(l.dialogs.is_empty());
}

#[test]
fn not_now_and_esc_trust_nothing_even_inside_the_arming_delay() {
    let (mut l, _d) = rig();
    l.ask_trust(1000);
    // Esc is never held back by the arming delay
    let key = l.reject();
    assert_eq!(key, None, "a picker closes without an answer");
    assert!(l.dialogs.is_empty());
    assert!(!l.trusted);
    // Not now answers without an action
    assert!(l.ask_trust(5000));
    let key = l.pick(0, 5000 + ARM_MS).unwrap();
    assert_eq!(key, "no");
    assert_eq!(l.answer(&key, 6000), None);
    assert!(!l.trusted);
    // the person may be asked again by the next click
    assert!(l.ask_trust(9000));
}

#[test]
fn no_dialog_for_a_trusted_folder_or_without_a_session() {
    let (mut l, _d) = rig();
    l.trusted = true;
    assert!(!l.ask_trust(0));
    assert!(l.dialogs.is_empty());
    assert!(!Live::new().ask_trust(0), "no engine yet");
}
