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
    build(&r.cfg, r.ws.clone(), prior, target, model, vec![]).expect("engine starts")
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
        ("/memory", "Instruction files"),
        ("/agents", "Agents in this session"),
        ("/permissions", "mode:"),
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

    // the folder is not trusted: no git, the TUI's wording
    for c in ["/files", "/diff"] {
        assert_eq!(live.submit(c), Submitted::Nothing);
        assert!(
            last_note(&live).contains("not trusted"),
            "{}",
            last_note(&live)
        );
    }
    live.trusted = true;
    assert_eq!(live.submit("/files"), Submitted::Do(Action::Project(None)));
    live.project_done("a.rs".into());
    assert_eq!(last_note(&live), "a.rs");

    // peers are off in this run
    assert_eq!(live.submit("/peers"), Submitted::Nothing);
    assert!(last_note(&live).starts_with("peers are off"));
    live.submit("/name beta");
    assert!(last_note(&live).starts_with("/name: peers are off"));

    live.submit("/focus");
    assert!(live.focus_view);

    // session-only rules, the TUI's check
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
    live.submit("/permissions remove Bash(git status:*)");
    assert_eq!(last_note(&live), "removed 1 rule(s): Bash(git status:*)");
    live.submit("/permissions remove Bash(git status:*)");
    assert_eq!(last_note(&live), "no rule Bash(git status:*)");
    finish(e, &mut live);
}
