//! The desktop's slash commands and relaunches on the real engine against a scripted model, in a
//! throwaway SYNT_HOME: `/model`, `/resume`, `/new`, `/close`, the info commands, `/clear`, `/compact`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser as _;
use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_desktop::engine::{self, Args, Engine, UiMsg};
use synthesis_desktop::live::{Action, DialogKind, Live, PickerKind, Submitted};
use synthesis_desktop::tui_view::driver::{Launched, LiveCfg, Target, build, read_store};
use synthesis_view::convo::Item;

struct Rig {
    cfg: LiveCfg,
    ws: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let script = root.join("script.json");
    std::fs::write(
        &script,
        r#"[{"text":"One."},{"text":"Two."},{"text":"Three."},{"text":"Four."}]"#,
    )
    .unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let provider = format!("fake:{}", script.display());
    let args = Args::try_parse_from([
        "synthesis-desktop",
        "--provider",
        &provider,
        "--sandbox",
        "off",
    ])
    .unwrap();
    let cfg = LiveCfg {
        granted: engine::root_of(&ws),
        project: ws.clone(),
        synthesis_home: home,
        home: None,
        tz_min: 0,
        tui: Default::default(),
        args,
        env,
    };
    Rig { cfg, ws, _dir: dir }
}

fn start(
    r: &Rig,
    prior: Option<Arc<Engine>>,
    target: Option<Target>,
    model: Option<String>,
) -> Launched {
    build(&r.cfg, r.ws.clone(), prior, target, model, None, vec![]).expect("engine starts")
}

/// The caller of `build` gives a `/close` the folder of the session it closes.
fn start_in(
    r: &Rig,
    dir: &Path,
    prior: Arc<Engine>,
    target: Target,
    model: Option<String>,
) -> Launched {
    build(
        &r.cfg,
        dir.to_path_buf(),
        Some(prior),
        Some(target),
        model,
        None,
        vec![],
    )
    .expect("engine starts")
}

/// Runs one prompt to its end.
fn run_prompt(l: &mut Launched, text: &str) {
    l.engine.send(text.into()).unwrap();
    let t0 = Instant::now();
    let mut live = Live::new();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out");
        match l.rx.try_recv() {
            Ok(UiMsg::TaskDone(r)) => return r.unwrap(),
            Ok(m) => {
                live.on_msg(m, 0);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn live_on(
    e: Engine,
    rx: UnboundedReceiver<UiMsg>,
    cfg: &LiveCfg,
) -> (Live, Arc<Engine>, UnboundedReceiver<UiMsg>) {
    let e = Arc::new(e);
    let mut live = Live::new();
    live.commands = e.commands.clone();
    live.attach(e.clone(), None);
    if let Ok((w, _)) = read_store(cfg, Some(&e.root)) {
        live.view.workspaces = w;
    }
    (live, e, rx)
}

fn last_note(l: &Live) -> String {
    match l.view.convo.items.last() {
        Some(Item::Notice { text, .. }) => text.clone(),
        other => panic!("no notice: {other:?}"),
    }
}

fn finish(e: Arc<Engine>, live: &mut Live) {
    live.engine = None;
    engine::close_shared(e);
}

#[test]
fn relaunches_keep_or_change_the_session() {
    let r = rig();
    let mut a = start(&r, None, None, None);
    run_prompt(&mut a, "first");
    let sid = a.engine.sid.clone();
    let label = a.engine.label.clone();
    assert!(label.starts_with("fake"), "{label}");

    // /model: the same session on another model (the choice is stored)
    let b = start(
        &r,
        Some(Arc::new(a.engine)),
        Some(Target::Model("fake/other".into())),
        None,
    );
    assert_eq!(b.engine.sid, sid, "same session");
    assert!(b.engine.label.ends_with("other"), "{}", b.engine.label);
    assert_eq!(b.model.as_deref(), Some("fake/other"));
    assert!(b.replay.is_some(), "the conversation comes back");

    // /new in another folder: a new session there
    let other = r.cfg.project.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = engine::root_of(&other);
    let c = start(
        &r,
        Some(Arc::new(b.engine)),
        Some(Target::New(other.clone())),
        b.model.clone(),
    );
    assert_ne!(c.engine.sid, sid);
    assert_eq!(c.engine.cwd, other.to_string_lossy());
    assert!(c.replay.is_none());
    assert_eq!(
        c.model.as_deref(),
        Some("fake/other"),
        "the model choice survives /new"
    );

    // /close: the session leaves the lists, a fresh one opens in the same folder
    let closed = c.engine.sid.clone();
    let d = start_in(&r, &other, Arc::new(c.engine), Target::Close, c.model);
    assert_ne!(d.engine.sid, closed);
    assert_eq!(d.engine.cwd, other.to_string_lossy());
    let rd = synthesis_session::Reader::open(&r.cfg.synthesis_home).unwrap();
    assert!(
        rd.list_used_sessions(None)
            .unwrap()
            .iter()
            .all(|s| s.id != closed)
    );

    // /resume: the first session continues on its own model, with its conversation
    let e = start(
        &r,
        Some(Arc::new(d.engine)),
        Some(Target::Resume(sid.clone())),
        d.model,
    );
    assert_eq!(e.engine.sid, sid);
    assert!(e.model.is_none());
    let replay = e.replay.expect("replayed");
    assert!(
        replay
            .convo
            .items
            .iter()
            .any(|i| matches!(i, Item::User(t) if t == "first"))
    );
    engine::close_shared(Arc::new(e.engine));
}

#[test]
fn close_workspace_closes_ours_spares_what_another_process_holds_and_other_folders() {
    let r = rig();
    let key = synthesis_view::workspaces::fold_dir(&r.ws.to_string_lossy());
    let used = |r: &Rig| -> Vec<String> {
        let rd = synthesis_session::Reader::open(&r.cfg.synthesis_home).unwrap();
        rd.list_used_sessions(None)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    // an earlier session of the folder (its process ended), and one in another folder
    let mut old = start(&r, None, None, None);
    run_prompt(&mut old, "earlier");
    let old_sid = old.engine.sid.clone();
    engine::close_shared(Arc::new(old.engine));
    let elsewhere = r.cfg.project.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut far = build(&r.cfg, elsewhere.clone(), None, None, None, None, vec![]).unwrap();
    run_prompt(&mut far, "far away");
    let far_sid = far.engine.sid.clone();
    engine::close_shared(Arc::new(far.engine));
    // the open session of the folder, and the one "another process" holds (its lock)
    let mut cur = start(&r, None, Some(Target::New(r.ws.clone())), None);
    run_prompt(&mut cur, "now");
    let cur_sid = cur.engine.sid.clone();
    let store = synthesis_session::Store::open(
        &r.cfg.synthesis_home,
        synthesis_session::StoreOptions::default(),
    )
    .unwrap();
    let held = {
        let ws =
            synthesis_core::WorkspaceContext::open("w", &r.ws, &r.ws, &[], r.ws.join(".art"), "pg")
                .unwrap();
        let mut w = store
            .create_session(synthesis_session::NewSession {
                id: Some("held-elsewhere".into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: synthesis_core::PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: synthesis_events::Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        w.append_message(synthesis_session::NewMessage::user(serde_json::json!(
            "busy"
        )))
        .unwrap();
        w
    };
    assert_eq!(used(&r).len(), 4);

    let next = start_in(
        &r,
        &r.ws,
        Arc::new(cur.engine),
        Target::CloseWorkspace(key.clone()),
        None,
    );
    assert_ne!(
        next.engine.sid, cur_sid,
        "a fresh session in the same folder"
    );
    // the engine's folder is the canonical one (`\\?\` on Windows)
    assert_eq!(next.engine.cwd, engine::root_of(&r.ws).to_string_lossy());
    let note = next
        .engine
        .notices
        .iter()
        .find(|n| n.contains("/close-workspace"))
        .expect("the result is reported");
    assert!(
        note.contains("closed 2 sessions") && note.contains("1 left open"),
        "{note}"
    );
    let left = used(&r);
    assert!(
        left.contains(&"held-elsewhere".into()) && left.contains(&far_sid),
        "{left:?}"
    );
    assert!(
        !left.contains(&cur_sid) && !left.contains(&old_sid),
        "{left:?}"
    );
    // nothing is deleted: a closed session still resumes by id
    let back = start(
        &r,
        Some(Arc::new(next.engine)),
        Some(Target::Resume(old_sid.clone())),
        None,
    );
    assert_eq!(back.engine.sid, old_sid);
    engine::close_shared(Arc::new(back.engine));
    drop(held);
}

#[test]
fn a_session_that_cannot_be_resumed_leaves_the_current_one_running() {
    let r = rig();
    let mut a = start(&r, None, None, None);
    run_prompt(&mut a, "hello");
    let sid = a.engine.sid.clone();
    let b = start(
        &r,
        Some(Arc::new(a.engine)),
        Some(Target::Resume("no-such-id".into())),
        None,
    );
    assert_eq!(b.engine.sid, sid, "the session we came from continues");
    assert!(
        b.engine.notices.iter().any(|n| n.contains("no-such-id")),
        "{:?}",
        b.engine.notices
    );
    engine::close_shared(Arc::new(b.engine));
}

#[test]
fn pickers_and_info_commands() {
    let r = rig();
    let mut a = start(&r, None, None, None);
    run_prompt(&mut a, "one");
    let first = a.engine.sid.clone();
    let b = start(
        &r,
        Some(Arc::new(a.engine)),
        Some(Target::New(r.ws.clone())),
        None,
    );
    let (mut live, e, _rx) = live_on(b.engine, b.rx, &r.cfg);

    // /resume lists the stored session under its workspace; the open (empty) one is not there
    assert_eq!(live.submit("/resume"), Submitted::Nothing);
    let d = &live.dialogs[0];
    let DialogKind::Picker {
        kind: PickerKind::Resume,
        headers,
        ..
    } = &d.kind
    else {
        panic!("{:?}", d.kind)
    };
    assert_eq!(headers.len(), 1, "one workspace group");
    assert_eq!(d.options.len(), 1);
    assert_eq!(d.options[0].0, first);
    assert!(d.options[0].1.starts_with("one"), "{}", d.options[0].1);
    assert_eq!(live.pick(0, 0).as_deref(), Some(first.as_str()));
    assert_eq!(live.answer(&first, 0), Some(Action::Resume(first.clone())));

    // /model: the current model is marked; picking it changes nothing; Esc closes
    live.submit("/model");
    let DialogKind::Picker {
        kind: PickerKind::Model,
        ..
    } = &live.dialogs[0].kind
    else {
        panic!()
    };
    assert!(
        live.dialogs[0]
            .options
            .iter()
            .any(|(k, l)| *k == e.label && l.contains("current"))
    );
    assert_eq!(live.reject(), None);
    assert!(live.dialogs.is_empty());
    assert_eq!(
        live.submit("/model fake/x"),
        Submitted::Do(Action::Model("fake/x".into()))
    );

    // the info commands answer in the chat
    for (cmd, needle) in [
        ("/mcp", "MCP"),
        ("/memory status", "Instruction files"),
        ("/agents list", "Agents in this session"),
        ("/permissions list", "mode:"),
        ("/perms list", "mode:"),
        ("/status", "session:"),
        ("/terminal", "no shell commands"),
        ("/images", "no images"),
        ("/effort", "no effort tiers"),
    ] {
        assert_eq!(live.submit(cmd), Submitted::Nothing, "{cmd}");
        assert!(
            last_note(&live).contains(needle),
            "{cmd}: {}",
            last_note(&live)
        );
    }
    // without an argument these open a picker (nothing is posted); a typed argument keeps working
    for cmd in ["/memory", "/agents", "/permissions", "/perms"] {
        let n = live.view.convo.items.len();
        assert_eq!(live.submit(cmd), Submitted::Nothing, "{cmd}");
        assert_eq!(live.dialogs.len(), 1, "{cmd} opens a picker");
        assert_eq!(live.view.convo.items.len(), n, "{cmd}");
        assert_eq!(live.reject(), None);
    }
    // `/agents --all` adds the other sessions (peers are off in this rig, and it says so); `/quit` is `/exit`
    for cmd in ["/agents --all", "/agents -a"] {
        assert_eq!(live.submit(cmd), Submitted::Nothing, "{cmd}");
        assert!(last_note(&live).contains("peers are off"), "{cmd}");
    }
    assert_eq!(live.submit("/quit"), Submitted::Quit);
    assert_eq!(live.submit("/new"), Submitted::Do(Action::New));
    assert_eq!(live.submit("/close"), Submitted::Do(Action::Close));
    assert_eq!(live.submit("/clear"), Submitted::Do(Action::Clear));
    live.clear_conversation();
    assert_eq!(
        live.view.convo.items.len(),
        1,
        "only the new-conversation note"
    );
    // nothing to compact in an empty conversation
    assert!(e.compact(None).is_err());
    finish(e, &mut live);
}

#[test]
fn files_diff_focus_peers_name_and_permission_rules() {
    let r = rig();
    let a = start(&r, None, None, None);
    let (mut live, e, _rx) = live_on(a.engine, a.rx, &r.cfg);

    // /files and /diff show the centre panel's tabs and post nothing (an untrusted folder is said by the tab)
    let before = live.view.convo.items.len();
    assert_eq!(live.submit("/files"), Submitted::Do(Action::ShowFiles));
    assert_eq!(live.submit("/diff"), Submitted::Do(Action::ShowDiff(None)));
    assert_eq!(
        live.submit("/diff a.rs"),
        Submitted::Do(Action::ShowDiff(Some("a.rs".into())))
    );
    assert_eq!(
        live.view.convo.items.len(),
        before,
        "nothing posted in the chat"
    );

    // peers are off in this run
    assert_eq!(live.submit("/peers"), Submitted::Nothing);
    assert!(last_note(&live).starts_with("peers are off"));
    live.submit("/name beta");
    assert!(last_note(&live).starts_with("/name: peers are off"));

    live.submit("/focus on");
    assert!(live.focus_view);

    // session-only rules, the CLI's check
    live.submit("/permissions allow Bash(git status:*)");
    assert_eq!(
        last_note(&live),
        "allowed for this session: Bash(git status:*)"
    );
    assert!(
        e.permissions_report()
            .iter()
            .any(|l| l.trim() == "Bash(git status:*)")
    );
    live.submit("/permissions allow Bash(git status:*)");
    assert_eq!(last_note(&live), "that allow rule already exists");
    live.submit("/permissions allow not a rule(");
    assert!(last_note(&live).starts_with("expected a rule like"));
    live.submit("/permissions allow");
    assert!(last_note(&live).starts_with("usage:"));
    // the picker chain: /permissions -> remove a rule -> the session's rules, the pick removes it
    live.submit("/permissions");
    let i = live.dialogs[0]
        .options
        .iter()
        .position(|o| o.0.starts_with("menu:"))
        .unwrap();
    let k = live.pick(i, i64::MAX).unwrap();
    assert_eq!(live.answer(&k, 0), None);
    let key = "/permissions remove Bash(git status:*)";
    assert!(live.dialogs[0].options.iter().any(|o| o.0 == key));
    let i = live.dialogs[0]
        .options
        .iter()
        .position(|o| o.0 == key)
        .unwrap();
    let k = live.pick(i, i64::MAX).unwrap();
    assert_eq!(live.answer(&k, 0), None);
    assert_eq!(last_note(&live), "removed 1 rule(s): Bash(git status:*)");
    live.submit("/permissions allow Bash(git status:*)");
    live.submit("/permissions remove Bash(git status:*)");
    assert_eq!(last_note(&live), "removed 1 rule(s): Bash(git status:*)");
    live.submit("/permissions remove Bash(git status:*)");
    assert_eq!(last_note(&live), "no rule Bash(git status:*)");
    finish(e, &mut live);
}

fn titles(r: &Rig, e: &Engine) -> Vec<String> {
    let (w, _) = read_store(&r.cfg, Some(&e.root)).unwrap();
    w.workspaces
        .iter()
        .flat_map(|w| w.sessions.iter().map(|s| s.title.clone()))
        .collect()
}

#[test]
fn rename_sets_the_stored_title_and_the_first_prompt_never_overwrites_it() {
    let r = rig();
    let a = start(&r, None, None, None);
    let (mut live, e, mut rx) = live_on(a.engine, a.rx, &r.cfg);

    // refused: empty, hidden characters, too long, secret-shaped; nothing is stored
    for bad in [
        "/rename \u{202e}evil",
        "/rename a\u{200b}b",
        &format!("/rename {}", "x".repeat(121)),
        "/rename sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        live.submit(bad);
        assert!(
            matches!(
                live.view.convo.items.last(),
                Some(Item::Notice { warning: true, .. })
            ),
            "{bad}"
        );
    }
    // no argument: shows the title and the usage, clears nothing
    live.submit("/rename");
    assert!(last_note(&live).contains("usage: /rename <title>"));

    live.submit("/rename   Fix   the build ");
    assert_eq!(last_note(&live), "Session renamed: Fix the build");
    assert_eq!(live.title_shown(), "Fix the build");
    live.submit("/rename");
    assert!(last_note(&live).starts_with("Session title: Fix the build"));

    // a prompt after the rename: the lists keep the person's title, not the first prompt
    e.send("a different first prompt".into()).unwrap();
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out");
        match rx.try_recv() {
            Ok(UiMsg::TaskDone(r)) => break r.unwrap(),
            Ok(m) => {
                live.on_msg(m, 0);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        };
    }
    assert_eq!(titles(&r, &e), ["Fix the build"]);
    let rd = synthesis_session::Reader::open(&r.cfg.synthesis_home).unwrap();
    assert_eq!(
        rd.get_session(&e.sid).unwrap().title.as_deref(),
        Some("Fix the build")
    );
    // the open Workspace/Running lists change at once
    live.view.workspaces = read_store(&r.cfg, Some(&e.root)).unwrap().0;
    live.submit("/rename Second name");
    assert_eq!(
        live.view.workspaces.workspaces[0].sessions[0].title,
        "Second name"
    );
    assert_eq!(titles(&r, &e), ["Second name"]);
    finish(e, &mut live);
}
