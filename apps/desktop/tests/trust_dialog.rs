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
    assert!(l.ask_trust(1000, Some(std::path::Path::new("/nonexistent/home"))));
    assert!(
        !l.ask_trust(1000, Some(std::path::Path::new("/nonexistent/home"))),
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
    l.ask_trust(1000, Some(std::path::Path::new("/nonexistent/home")));
    // Esc is never held back by the arming delay
    let key = l.reject();
    assert_eq!(key, None, "a picker closes without an answer");
    assert!(l.dialogs.is_empty());
    assert!(!l.trusted);
    // Not now answers without an action
    assert!(l.ask_trust(5000, Some(std::path::Path::new("/nonexistent/home"))));
    let key = l.pick(0, 5000 + ARM_MS).unwrap();
    assert_eq!(key, "no");
    assert_eq!(l.answer(&key, 6000), None);
    assert!(!l.trusted);
    // the person may be asked again by the next click
    assert!(l.ask_trust(9000, Some(std::path::Path::new("/nonexistent/home"))));
}

#[test]
fn no_dialog_for_a_trusted_folder_or_without_a_session() {
    let (mut l, _d) = rig();
    l.trusted = true;
    assert!(!l.ask_trust(0, Some(std::path::Path::new("/nonexistent/home"))));
    assert!(l.dialogs.is_empty());
    assert!(
        !Live::new().ask_trust(0, Some(std::path::Path::new("/nonexistent/home"))),
        "no engine yet"
    );
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
    assert!(l.ask_trust(0, Some(std::path::Path::new("/nonexistent/home"))));
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
    assert!(
        t.contains("is NOT covered")
            && t.contains("never starts the project's hooks or MCP servers"),
        "{t}"
    );
    // a subfolder of a repository: the grant is the repository, and the dialog says so
    let (mut l, d) = rig_in("pkg/app");
    assert!(l.ask_trust(0, Some(std::path::Path::new("/nonexistent/home"))));
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
        Some((root, None, true)),
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
            Some((root, None, true)),
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
            .contains("including repositories cloned there later")
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
    assert!(l.ask_trust(0, Some(std::path::Path::new("/nonexistent/home"))));
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
    {
        let policy = synthesis_session::trust::Policy {
            home: Some(std::path::PathBuf::from("/nonexistent/home")),
            synt_home: home.clone(),
        };
        let fp = synthesis_session::trust::fingerprint_root(&app).unwrap();
        synthesis_session::trust::add(&home, &app, "folder", &fp, &policy).unwrap();
    }
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
    assert_eq!(
        synthesis_session::trust::list(&home).unwrap().records.len(),
        1
    );
    // an unknown target changes nothing
    l.submit("/trust revoke 9");
    assert!(l.dialogs.is_empty());
    // confirmed: gone, and the note says when it takes effect
    l.submit(&format!("/trust revoke {}", app.display()));
    let k = l.pick(1, i64::MAX).unwrap();
    assert_eq!(k, "revoke");
    assert_eq!(l.answer(&k, 1000), None);
    assert!(
        synthesis_session::trust::list(&home)
            .unwrap()
            .records
            .is_empty()
    );
    let last = format!("{:?}", l.view.convo.items.last());
    assert!(last.contains("next engine start"), "{last}");
}

#[test]
fn trust_says_when_remembered_trust_is_off_and_counts_rows_it_ignores() {
    // the test engine runs with --sandbox off: remembered trust is not honoured, and /trust says so
    let (mut l, _d, app) = rig_nested();
    let home = l.engine.as_ref().unwrap().synthesis_home.clone();
    l.submit("/trust");
    let t = format!("{:?}", l.view.convo.items.last());
    assert!(
        t.contains("NOT honoured") && t.contains("sandbox is off"),
        "{t}"
    );
    assert!(!t.contains("unverified rows"));
    // a row written without the key (what a shell could do to the database) is ignored and counted
    let policy = synthesis_session::trust::Policy {
        home: Some(std::path::PathBuf::from("/nonexistent/home")),
        synt_home: home.clone(),
    };
    let fp = synthesis_session::trust::fingerprint_root(&app).unwrap();
    synthesis_session::trust::add(&home, &app, "folder", &fp, &policy).unwrap();
    std::fs::remove_file(home.join("run/trust.key")).unwrap();
    l.submit("/trust");
    let t = format!("{:?}", l.view.convo.items.last());
    assert!(t.contains("unverified rows ignored: 1"), "{t}");
    assert!(t.contains("no remembered folder"), "{t}");
}

#[test]
fn an_unknown_home_folder_offers_no_trust() {
    let (mut l, _d) = rig();
    assert!(!l.ask_trust(0, None));
    assert!(l.dialogs.is_empty());
    let note = format!("{:?}", l.view.convo.items.last());
    assert!(note.contains("home folder is unknown"), "{note}");
}

/// The trusted configuration is checked again after the engine loaded it and before any hook runs: a file
/// changed in that window (here from the hook point, as a racing writer would) aborts the start.
#[test]
fn a_file_changed_between_the_check_and_the_load_aborts_the_trusted_start() {
    use clap::Parser as _;
    use std::sync::Arc;
    use synthesis_desktop::engine::{Args, Engine, StartOpts, TRUST_CHANGED, TrustCheck};

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".synt")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(ws.join(".synt/settings.json"), "{}").unwrap();
    let sp = base.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let model = format!("fake:{}", sp.display());
    let common = Args::try_parse_from([
        "synthesis-desktop",
        "--sandbox",
        "off",
        "--provider",
        &model,
        "--trust-project",
    ])
    .unwrap()
    .common;
    let manifest = synthesis_session::trust::fingerprint(&ws, &ws).unwrap();
    let start = |hook: Option<Arc<dyn Fn() + Send + Sync>>| {
        Engine::start_with(
            StartOpts {
                project: ws.clone(),
                common: common.clone(),
                resume: None,
                notes: vec![],
                verify: Some(TrustCheck {
                    root: ws.clone(),
                    cwd: ws.clone(),
                    manifest: manifest.clone(),
                    before_compare: hook,
                }),
            },
            &env,
        )
        .map(|_| ())
    };
    // unchanged: the engine starts
    start(None).unwrap();
    // changed in the window: refused, and the file is named
    let target = ws.join(".synt/settings.json");
    let hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || std::fs::write(&target, r#"{"hooks":{"x":1}}"#).unwrap());
    let err = start(Some(hook)).unwrap_err();
    assert!(err.starts_with(TRUST_CHANGED), "{err}");
    assert!(err.contains(".synt/settings.json"), "{err}");
}

/// Nothing the project names has run when the trusted configuration is found changed: the check runs before
/// the project's files are read, again before any MCP server starts, and once more before any hook runs. A
/// control run shows the markers DO appear when nothing is tampered with, so the absence proves something.
#[test]
fn nothing_the_project_names_runs_before_the_trusted_configuration_is_verified() {
    use clap::Parser as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use synthesis_desktop::engine::{Args, Engine, StartOpts, TRUST_CHANGED, TrustCheck};

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".synt")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let (hook_mark, mcp_mark) = (base.join("hook-ran"), base.join("mcp-ran"));
    let settings = ws.join(".synt/settings.json");
    std::fs::write(
        &settings,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"touch {}"}}]}}]}}}}"#,
            hook_mark.display()
        ),
    )
    .unwrap();
    std::fs::write(
        ws.join(".mcp.json"),
        format!(
            r#"{{"mcpServers":{{"s":{{"command":"sh","args":["-c","touch {}; sleep 1"]}}}}}}"#,
            mcp_mark.display()
        ),
    )
    .unwrap();
    let sp = base.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let model = format!("fake:{}", sp.display());
    let args = |trust: bool| {
        let mut a = vec![
            "synthesis-desktop",
            "--sandbox",
            "off",
            "--provider",
            &model,
            "--approve-mcp",
            "s",
        ];
        if trust {
            a.push("--trust-project");
        }
        Args::try_parse_from(a).unwrap().common
    };
    let manifest = synthesis_session::trust::fingerprint(&ws, &ws).unwrap();
    let clear = || {
        let _ = std::fs::remove_file(&hook_mark);
        let _ = std::fs::remove_file(&mcp_mark);
    };
    let start = |trust: bool, tamper_at: Option<usize>| {
        clear();
        let calls = Arc::new(AtomicUsize::new(0));
        let s2 = settings.clone();
        let hook: Option<Arc<dyn Fn() + Send + Sync>> = tamper_at.map(|n| {
            let calls = calls.clone();
            Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) + 1 == n {
                    // a writer that races the start: any change to the file changes the manifest
                    let mut t = std::fs::read_to_string(&s2).unwrap();
                    t.push('\n');
                    std::fs::write(&s2, t).unwrap();
                }
            }) as Arc<dyn Fn() + Send + Sync>
        });
        let r = Engine::start_with(
            StartOpts {
                project: ws.clone(),
                common: args(trust),
                resume: None,
                notes: vec![],
                verify: trust.then(|| TrustCheck {
                    root: ws.clone(),
                    cwd: ws.clone(),
                    manifest: synthesis_session::trust::fingerprint(&ws, &ws).unwrap(),
                    before_compare: hook,
                }),
            },
            &env,
        );
        // the MCP child is started detached: give it a moment to touch its marker
        std::thread::sleep(std::time::Duration::from_millis(1500));
        r.map(|(e, _rx)| e.trusted)
    };
    // control: untampered, trusted: the SessionStart hook and the MCP server both ran
    assert_eq!(start(true, None).ok(), Some(true));
    assert!(hook_mark.exists(), "control: the hook should run");
    assert!(mcp_mark.exists(), "control: the MCP server should start");
    let _ = manifest;
    // tampered at the first check (before any project file is read) or the second (before MCP starts)
    for n in [1, 2] {
        let err = start(true, Some(n)).err().unwrap();
        assert!(err.starts_with(TRUST_CHANGED), "{err}");
        assert!(
            !hook_mark.exists() && !mcp_mark.exists(),
            "check {n}: something already ran"
        );
    }
    // tampered at the third (after prepare): an MCP server may be up by then, but no hook has run
    let err = start(true, Some(3)).err().unwrap();
    assert!(err.starts_with(TRUST_CHANGED), "{err}");
    assert!(
        !hook_mark.exists(),
        "no SessionStart hook ran in the aborted attempt"
    );
    // the retry the window makes: untrusted, no hook (project hooks are inert untrusted)
    assert_eq!(start(false, None).ok(), Some(false));
    assert!(
        !hook_mark.exists(),
        "an untrusted start runs no project hook"
    );
}

/// A remembered trust never runs the project's commands: with the project's hook and MCP server configured and
/// even approved on the command line, neither starts and a notice names them; the same project started with
/// the launch flag (the trust of THIS run) runs both. Settings of the project still load either way.
#[test]
fn a_remembered_trust_leaves_project_hooks_and_mcp_servers_inert_and_says_so() {
    use clap::Parser as _;
    use synthesis_desktop::engine::{Args, Engine, StartOpts};

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".synt")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let (hook_mark, mcp_mark) = (base.join("hook-ran"), base.join("mcp-ran"));
    std::fs::write(
        ws.join(".synt/settings.json"),
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"touch {}"}}]}}]}}}}"#,
            hook_mark.display()
        ),
    )
    .unwrap();
    std::fs::write(
        ws.join(".mcp.json"),
        format!(
            r#"{{"mcpServers":{{"s":{{"command":"sh","args":["-c","touch {}; sleep 1"]}}}}}}"#,
            mcp_mark.display()
        ),
    )
    .unwrap();
    let sp = base.join("script.json");
    std::fs::write(&sp, r#"[{"text": "ok"}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let model = format!("fake:{}", sp.display());
    let common = |inert: bool| {
        let mut c = Args::try_parse_from([
            "synthesis-desktop",
            "--sandbox",
            "off",
            "--provider",
            &model,
            "--trust-project",
            "--approve-mcp",
            "s",
        ])
        .unwrap()
        .common;
        c.project_exec_inert = inert;
        c
    };
    let start = |inert: bool| {
        let _ = std::fs::remove_file(&hook_mark);
        let _ = std::fs::remove_file(&mcp_mark);
        let (e, _rx) = Engine::start_with(
            StartOpts {
                project: ws.clone(),
                common: common(inert),
                resume: None,
                notes: vec![],
                verify: None,
            },
            &env,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        e
    };
    let e = start(true);
    assert!(
        e.trusted,
        "the folder is trusted: settings, rules, skills and the rest load"
    );
    assert!(!hook_mark.exists(), "a remembered trust ran a project hook");
    assert!(
        !mcp_mark.exists(),
        "a remembered trust started a project MCP server"
    );
    let banner = e
        .notices
        .iter()
        .find(|n| n.contains("not started"))
        .expect("a notice names what was left out");
    assert!(
        banner.contains("hook SessionStart") && banner.contains("MCP server s"),
        "{banner}"
    );
    assert!(
        banner.contains("never runs project commands") && banner.contains("Trust for this session"),
        "{banner}"
    );
    drop(e);
    // the trust of this run (session-only or the launch flag): both run, and there is no such notice
    let e = start(false);
    assert!(
        hook_mark.exists(),
        "the hook should run under a trust of this run"
    );
    assert!(
        mcp_mark.exists(),
        "the MCP server should start under a trust of this run"
    );
    assert!(!e.notices.iter().any(|n| n.contains("not started")));
}
