//! The Trust dialog the Files tab raises in an untrusted folder: the real engine on a scripted model, a
//! temp `SYNT_HOME`. Declining (Not now, Esc), type-ahead and the arming delay never trust anything.

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_desktop::engine::Engine;
use synthesis_desktop::live::{ARM_MS, Action, DialogKind, Live, PickerKind, TrustGrant};

fn rig() -> (Live, tempfile::TempDir) {
    rig_in("")
}

/// A session started in `sub` of a project folder `ws` that holds `.git` (so the trust root is `ws`).
fn rig_in(sub: &str) -> (Live, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    if !sub.is_empty() {
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join(sub)).unwrap();
    }
    let ws = ws.join(sub);
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
    assert!(l.ask_trust(1000, None));
    assert!(
        !l.ask_trust(1000, None),
        "a second click does not stack another"
    );
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
    assert_eq!(d.options[1].0, "remember");
    assert_eq!(d.options[2].0, "session");
    assert!(d.needs_person());
    // keys typed before the person engaged it cannot answer: Enter and digits go to the prompt
    assert!(!d.answerable());
    // a click inside the arming delay does not answer either
    assert_eq!(
        l.pick(1, 1000 + ARM_MS - 1),
        None,
        "remember cannot be picked early either"
    );
    assert_eq!(l.pick(2, 1000 + ARM_MS - 1), None);
    assert_eq!(l.dialogs.len(), 1);
    // after the delay, the explicit option answers: the window then restarts the session trusted
    let key = l.pick(2, 1000 + ARM_MS).unwrap();
    let root = l.trust_root.clone().unwrap();
    assert_eq!(
        l.answer(&key, 2000),
        Some(Action::TrustFolder(TrustGrant {
            root,
            remember: None
        }))
    );
    assert!(l.dialogs.is_empty());
}

#[test]
fn not_now_and_esc_trust_nothing_even_inside_the_arming_delay() {
    let (mut l, _d) = rig();
    l.ask_trust(1000, None);
    // Esc is never held back by the arming delay
    let key = l.reject();
    assert_eq!(key, None, "a picker closes without an answer");
    assert!(l.dialogs.is_empty());
    assert!(!l.trusted);
    // Not now answers without an action
    assert!(l.ask_trust(5000, None));
    let key = l.pick(0, 5000 + ARM_MS).unwrap();
    assert_eq!(key, "no");
    assert_eq!(l.answer(&key, 6000), None);
    assert!(!l.trusted);
    // the person may be asked again by the next click
    assert!(l.ask_trust(9000, None));
}

#[test]
fn no_dialog_for_a_trusted_folder_or_without_a_session() {
    let (mut l, _d) = rig();
    l.trusted = true;
    assert!(!l.ask_trust(0, None));
    assert!(l.dialogs.is_empty());
    assert!(!Live::new().ask_trust(0, None), "no engine yet");
}

fn headers(l: &Live) -> String {
    match &l.dialogs[0].kind {
        DialogKind::Picker { headers, .. } => headers
            .iter()
            .map(|(_, h)| h.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => unreachable!(),
    }
}

#[test]
fn the_dialog_names_the_root_and_says_when_it_is_not_the_folder_you_are_in() {
    let (mut l, d) = rig();
    assert!(l.ask_trust(0, None));
    let root = std::fs::canonicalize(d.path()).unwrap().join("ws");
    let t = headers(&l);
    assert!(
        t.contains(&format!("Trust root: {}", root.display())),
        "{t}"
    );
    assert!(!t.contains("NOT the folder you are in"), "{t}");
    assert!(
        t.contains("permission and allow rules") && t.contains("NAME@HASH"),
        "{t}"
    );
    assert!(t.contains("fresh session") && t.contains("hooks"), "{t}");
    // a subfolder of a repository: the grant is the repository, and the dialog says so
    let (mut l, d) = rig_in("pkg/app");
    assert!(l.ask_trust(0, None));
    let root = std::fs::canonicalize(d.path()).unwrap().join("ws");
    let t = headers(&l);
    assert!(
        t.contains(&format!("Trust root: {}", root.display())),
        "{t}"
    );
    assert!(t.contains("NOT the folder you are in"), "{t}");
    assert_eq!(l.trust_root.as_deref(), Some(root.as_path()));
}

#[test]
fn a_home_folder_or_the_config_folder_is_refused_not_offered() {
    let (mut l, d) = rig();
    let ws = std::fs::canonicalize(d.path()).unwrap().join("ws");
    // the root would be the user's home
    assert!(!l.ask_trust(0, Some(&ws)));
    assert!(l.dialogs.is_empty());
    assert!(l.trust_root.is_none());
    let note = format!("{:?}", l.view.convo.items.last());
    assert!(
        note.contains("trust not offered") && note.contains("home folder"),
        "{note}"
    );
    // a home that merely lies below the root is refused as well (the root is a parent of it)
    assert!(!l.ask_trust(0, Some(&ws.join("sub"))));
    // an unrelated home is fine
    assert!(l.ask_trust(0, Some(&d.path().join("elsewhere"))));
}

/// (M2/M3) A restart with the folder trusted: on a healthy project it comes back trusted; when the project
/// cannot start at all, `build` returns the error (the window then clears its session trust and shows the
/// failure). The retry-untrusted arm of `build` is the `Target::Trust` match arm; a failure that only a
/// trusted launch has cannot be provoked from a test (config and settings errors apply untrusted too).
#[test]
fn a_trusted_restart_comes_back_trusted_and_a_broken_project_reports_the_error() {
    use clap::Parser as _;
    use synthesis_desktop::engine::{self, Args};
    use synthesis_desktop::tui_view::driver::{LiveCfg, Target, build, trust_fallback_note};

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".synt")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let sp = base.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let model = format!("fake:{}", sp.display());
    let args = Args::try_parse_from([
        "synthesis-desktop",
        "--sandbox",
        "off",
        "--provider",
        &model,
        "--approve-mcp",
        "db",
    ])
    .unwrap();
    let cfg = LiveCfg {
        env,
        args,
        project: ws.clone(),
        granted: ws.clone(),
        synthesis_home: home,
        home: None,
        tz_min: 0,
        tui: Default::default(),
    };
    let root = engine::root_of(&ws);
    let l = build(
        &cfg,
        ws.clone(),
        None,
        Some(Target::Trust),
        None,
        Some(root),
        vec![],
    )
    .unwrap();
    assert!(l.engine.trusted);
    let l = build(
        &cfg,
        ws.clone(),
        None,
        Some(Target::Trust),
        None,
        None,
        vec![],
    )
    .unwrap();
    assert!(!l.engine.trusted, "no session trust: the launch flags rule");
    std::fs::write(ws.join(".synt/config.json"), "{not json").unwrap();
    let root = engine::root_of(&ws);
    assert!(
        build(
            &cfg,
            ws,
            None,
            Some(Target::Trust),
            None,
            Some(root),
            vec![]
        )
        .is_err()
    );
    assert!(trust_fallback_note("x").contains("continues untrusted"));
}

/// A project `app` in `proj` (a real project parent), the store at `home`, both under one temp folder.
fn rig_nested() -> (Live, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (app, home) = (root.join("proj/app"), root.join("home"));
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let sp = root.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let (engine, _rx) = Engine::start(
        &app,
        &env,
        None,
        Some(&format!("fake:{}", sp.display())),
        Some("off"),
    )
    .unwrap();
    let mut live = Live::new();
    live.attach(Arc::new(engine), None);
    (live, dir, app)
}

#[test]
fn remember_and_parent_are_offered_and_answer_with_what_to_record() {
    let (mut l, d, app) = rig_nested();
    let proj = app.parent().unwrap().to_path_buf();
    let home = d.path().join("elsewhere");
    assert!(l.ask_trust(0, Some(&home)));
    let keys: Vec<_> = l.dialogs[0].options.iter().map(|o| o.0.as_str()).collect();
    assert_eq!(keys, ["no", "remember", "session", "parent"]);
    assert!(
        l.dialogs[0].options[3]
            .1
            .contains("all its subfolders, remembered")
    );
    // the dialog is not engaged: Enter/digits are for the prompt, and the delay holds clicks back
    assert!(!l.dialogs[0].answerable());
    assert_eq!(l.pick(1, ARM_MS - 1), None);
    let k = l.pick(1, ARM_MS).unwrap();
    let grant = match l.answer(&k, 1000) {
        Some(Action::TrustFolder(g)) => g,
        o => panic!("{o:?}"),
    };
    assert_eq!(grant.root, app);
    assert_eq!(grant.remember, Some((app.clone(), "folder")));
    // the parent grant records the parent
    assert!(l.ask_trust(5000, Some(&home)));
    let k = l.pick(3, 5000 + ARM_MS).unwrap();
    let Some(Action::TrustFolder(g)) = l.answer(&k, 6000) else {
        panic!()
    };
    assert_eq!((g.root, g.remember), (app, Some((proj, "parent"))));
}

#[test]
fn the_parent_is_not_offered_when_it_is_home_a_filesystem_root_or_holds_the_config() {
    // `rig()`: the config folder lies next to the project, inside the parent: no parent option
    let (mut l, _d) = rig();
    assert!(l.ask_trust(0, None));
    assert!(l.dialogs[0].options.iter().all(|o| o.0 != "parent"));
    // the parent is the user's home: no parent option, the folder itself is still offered
    let (mut l, d, app) = rig_nested();
    let home = app.parent().unwrap().to_path_buf();
    assert!(l.ask_trust(0, Some(&home)));
    assert!(l.dialogs[0].options.iter().all(|o| o.0 != "parent"));
    drop(d);
}

#[test]
fn trust_lists_and_revokes_with_a_confirmation_that_keeps_by_default() {
    let (mut l, _d, app) = rig_nested();
    let home = l.engine.as_ref().unwrap().synthesis_home.clone();
    synthesis_session::trust::add(
        &home,
        &app,
        "folder",
        &synthesis_session::trust::fingerprint(&app),
    )
    .unwrap();
    l.submit("/trust");
    let last = format!("{:?}", l.view.convo.items.last());
    assert!(
        last.contains("1.") && last.contains("proj/app") && last.contains("config unchanged"),
        "{last}"
    );
    // revoke asks first; Esc keeps it
    l.submit("/trust revoke 1");
    assert_eq!(l.dialogs[0].options[0].0, "keep");
    assert_eq!(l.dialogs[0].sel, 0);
    assert!(l.dialogs[0].needs_person());
    l.reject();
    assert_eq!(synthesis_session::trust::list(&home).unwrap().len(), 1);
    // an unknown target changes nothing
    l.submit("/trust revoke 9");
    assert!(l.dialogs.is_empty());
    // confirmed: gone, and the note says when it takes effect
    l.submit(&format!("/trust revoke {}", app.display()));
    let k = l.pick(1, i64::MAX).unwrap();
    assert_eq!(k, "revoke");
    assert_eq!(l.answer(&k, 1000), None);
    assert!(synthesis_session::trust::list(&home).unwrap().is_empty());
    let last = format!("{:?}", l.view.convo.items.last());
    assert!(last.contains("next engine start"), "{last}");
}
