//! A whole round trip through the live state: prompt -> tool call -> approval dialog -> answer ->
//! final text, and a question dialog, on the real engine against a scripted model. The assertions are
//! on `Live` and the `Snapshot` the window paints from it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_desktop::engine::{Engine, UiMsg};
use synthesis_desktop::live::{DialogKind, Live, Submitted};
use synthesis_desktop::tui_view::snapshot;
use synthesis_view::convo::{ApprovalState, Item, ToolState};

struct Rig {
    live: Live,
    rx: UnboundedReceiver<UiMsg>,
    engine: Arc<Engine>,
    target: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(script: serde_json::Value) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let target = ws.join("hello.txt");
    let text = script
        .to_string()
        .replace("{TARGET}", &target.to_string_lossy().replace('\\', "\\\\"));
    let sp = root.join("script.json");
    std::fs::write(&sp, text).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let (engine, rx) = Engine::start(
        &ws,
        &env,
        None,
        Some(&format!("fake:{}", sp.display())),
        Some("off"),
    )
    .unwrap();
    let engine = Arc::new(engine);
    let mut live = Live::new();
    live.attach(engine.clone(), None);
    Rig {
        live,
        rx,
        engine,
        target,
        _dir: dir,
    }
}

impl Rig {
    fn pump(&mut self, what: &str, stop: impl Fn(&Live) -> bool) {
        let t0 = Instant::now();
        while !stop(&self.live) {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {what}");
            match self.rx.try_recv() {
                Ok(m) => {
                    self.live.on_msg(m, 1_000_000);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    fn send(&mut self, text: &str) {
        let Submitted::Send(t) = self.live.submit(text) else {
            panic!("not sent")
        };
        self.engine.send(t).unwrap();
    }
}

#[test]
fn prompt_tool_approval_answer_round_trip() {
    let mut r = rig(serde_json::json!([
        {"text": "Writing the file.", "tools": [{"id": "w1", "name": "Write", "input": {"file_path": "{TARGET}", "content": "hi\n"}}]},
        {"text": "Finished."},
        // the verification gate sends the model a follow-up after a change
        {"text": "Nothing to verify here."}
    ]));
    r.send("make hello.txt");
    r.pump("dialog", |l| !l.dialogs.is_empty());
    assert!(!r.target.exists(), "nothing is written before the answer");
    let DialogKind::Approval(a) = &r.live.dialogs[0].kind else {
        panic!()
    };
    assert_eq!(a.tool, "Write");
    assert!(!a.reason.is_empty(), "the reason is shown");
    assert_eq!(r.live.dialogs[0].options.last().unwrap().0, "no");
    // the window paints a "needs you" state and a pending row from the same state
    let snap = snapshot::build(&r.live, 1_000_000, 0, None);
    assert!(snap.top.health.contains("need"), "{}", snap.top.health);
    assert!(r.live.view.convo.items.iter().any(|i| matches!(
        i,
        Item::Approval {
            state: ApprovalState::Pending,
            ..
        }
    )));
    // Enter is held back while the dialog arms; then "yes" runs the tool
    let t0 = r.live.dialogs[0].armed_until;
    assert_eq!(r.live.pick(0, t0 - 1), None);
    let k = r.live.pick(0, t0).unwrap();
    r.live.answer(&k, t0);
    r.pump("task end", |l| !l.busy);
    assert_eq!(std::fs::read_to_string(&r.target).unwrap(), "hi\n");
    let items = &r.live.view.convo.items;
    assert!(
        items
            .iter()
            .any(|i| matches!(i, Item::User(t) if t == "make hello.txt"))
    );
    assert!(items.iter().any(|i| matches!(i, Item::Tool { name, state: ToolState::Ok, diff: Some(_), .. } if name == "Write")));
    assert!(
        items
            .iter()
            .any(|i| matches!(i, Item::Assistant { text, .. } if text == "Finished."))
    );
    let ended = items.iter().any(|i| matches!(i, Item::TaskEnd { .. }));
    assert!(ended, "{items:?}");
    let snap = snapshot::build(&r.live, 1_000_000, 0, None);
    assert!(snap.activity.iter().any(|p| p.text == "make hello.txt"));
    assert_eq!(snap.top.health, "system healthy");
    assert!(snap.context.input != "0");
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

#[test]
fn a_question_dialog_is_answered_and_the_tool_result_carries_the_choice() {
    let mut r = rig(serde_json::json!([
        {"text": "", "tools": [{"id": "q1", "name": "AskUserQuestion", "input": {"questions": [
            {"question": "Which colour?", "header": "Colour", "multiSelect": false,
             "options": [{"label": "red"}, {"label": "blue"}]}]}}]},
        {"text": "Thanks."}
    ]));
    r.send("pick a colour");
    r.pump("question", |l| !l.dialogs.is_empty());
    assert!(matches!(
        r.live.dialogs[0].kind,
        DialogKind::Question { .. }
    ));
    let t = r.live.dialogs[0].armed_until;
    assert_eq!(r.live.pick(1, t), None, "choosing toggles");
    let k = r.live.pick(3, t).expect("send answers");
    r.live.answer(&k, t);
    r.pump("task end", |l| !l.busy);
    // the harness gave the model the chosen label
    assert!(r.live.view.convo.items.iter().any(|i| matches!(i, Item::Tool { name, output, .. } if name == "AskUserQuestion" && output.contains("blue"))), "{:?}", r.live.view.convo.items);
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

#[test]
fn an_other_answer_reaches_the_model_as_typed() {
    let mut r = rig(serde_json::json!([
        {"text": "", "tools": [{"id": "q1", "name": "AskUserQuestion", "input": {"questions": [
            {"question": "Which colour?", "header": "Colour", "multiSelect": false,
             "options": [{"label": "red"}, {"label": "blue"}]}]}}]},
        {"text": "Thanks."}
    ]));
    r.send("pick a colour");
    r.pump("question", |l| !l.dialogs.is_empty());
    let t = r.live.dialogs[0].armed_until;
    assert_eq!(r.live.pick(2, t), None, "Other opens the text field");
    assert!(r.live.type_other("teal"));
    r.live.commit_other();
    let k = r.live.pick(3, t).expect("send answers");
    r.live.answer(&k, t);
    r.pump("task end", |l| !l.busy);
    assert!(r.live.view.convo.items.iter().any(|i| matches!(i, Item::Tool { name, output, .. } if name == "AskUserQuestion" && output.contains("teal"))), "{:?}", r.live.view.convo.items);
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

/// Answers every permission dialog (once) until the task ends.
fn run_to_end(r: &mut Rig) {
    let t0 = Instant::now();
    loop {
        r.pump("dialog or task end", |l| !l.dialogs.is_empty() || !l.busy);
        if r.live.dialogs.is_empty() {
            break;
        }
        let armed = r.live.dialogs[0].armed_until;
        let k = r.live.pick(0, armed).unwrap();
        r.live.answer(&k, armed);
        assert!(t0.elapsed() < Duration::from_secs(60), "too many dialogs");
    }
}

/// The data behind the diff, terminal and browser tabs: a Bash run, a Write and a browser_screenshot on the
/// real engine, then a `browser.frame` event as `browser_screenshot` would emit it.
#[test]
fn the_diff_terminal_and_browser_tabs_read_the_session() {
    let mut r = rig(serde_json::json!([
        {"text": "Running it.", "tools": [{"id": "b1", "name": "Bash", "input": {"command": "echo hello"}}]},
        {"text": "Writing.", "tools": [{"id": "w1", "name": "Write", "input": {"file_path": "{TARGET}", "content": "hi\nthere\n"}}]},
        {"text": "A shot.", "tools": [{"id": "s1", "name": "browser_screenshot", "input": {}}]},
        {"text": "Finished."},
        {"text": "Nothing to verify here."}
    ]));
    r.send("do the three things");
    run_to_end(&mut r);
    let p = &r.live.view.panes;
    // terminal: the command, finished, with its output
    let c = p
        .commands
        .iter()
        .find(|c| c.command == "echo hello")
        .expect("the Bash run is in the history");
    assert_eq!(c.state, synthesis_view::agent::CmdState::Ok, "{c:?}");
    assert!(c.tail.iter().any(|l| l.contains("hello")), "{:?}", c.tail);
    // diff: the written file with +2 -0 and its diff
    let f = p
        .files
        .iter()
        .find(|f| f.path.ends_with("hello.txt"))
        .expect("the Write is a changed file");
    assert_eq!((f.added, f.removed), (2, 0));
    assert!(f.diff.is_some());
    // browser: the call is listed, and whatever frame the run saved is kept
    let frames_before = p.frames.len();
    assert!(
        p.browser.iter().any(|b| b.name == "browser_screenshot"),
        "{:?}",
        p.browser
    );
    // a saved frame arrives as an event; the tab keeps the history and the latest
    let ev: synthesis_events::Envelope = serde_json::from_value(serde_json::json!({
        "v": 1, "seq": 9999, "ts": "2026-11-07T10:00:05Z", "session_id": "s", "task_id": null, "turn_id": null,
        "type": "browser.frame",
        "payload": {"path": "artifacts/shot-1.png", "url": "http://localhost:3000", "w": 1280, "h": 800, "bytes": 52_000}
    }))
    .unwrap();
    r.live.view.apply(&ev);
    let p = &r.live.view.panes;
    assert_eq!(p.frames.len(), frames_before + 1);
    assert_eq!(p.screenshot.as_deref(), Some("artifacts/shot-1.png"));
    // the counters of the tab strip and the usage numbers come from the same state
    let snap = snapshot::build(&r.live, 1_000_000, 0, None);
    assert_eq!(
        snap.head.stats,
        "1 files | 0 LOC".replace("0 LOC", &format!("{} LOC", snapshot::tok(2)))
    );
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

/// The saved screenshot of a session lives in `<home>/sessions/<sid>/artifacts` and the browser tab opens it through
/// `viewable_screenshot` with that folder as the root: a PNG written there is viewable, one elsewhere is not.
#[test]
fn a_frame_saved_in_the_session_artifacts_is_viewable() {
    let r = rig(serde_json::json!([{"text": "ok"}]));
    let session = r.engine.synthesis_home.join("sessions").join(&r.engine.sid);
    let dir = session.join("artifacts");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot-1.png");
    std::fs::write(&png, b"\x89PNG").unwrap();
    let elsewhere = r.target.with_file_name("other.png");
    std::fs::write(&elsewhere, b"\x89PNG").unwrap();
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
    assert!(synthesis_view::agent::viewable_screenshot(&session, &s(&png)).is_some());
    assert!(synthesis_view::agent::viewable_screenshot(&session, &s(&elsewhere)).is_none());
    let mut r = r;
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

#[test]
fn a_stale_stored_model_opens_the_picker_on_the_first_prompt_and_keeps_the_prompt() {
    let mut r = rig(serde_json::json!([]));
    r.live.stale_model = true;
    assert_eq!(r.live.submit("hello"), Submitted::Nothing);
    assert_eq!(r.live.dialogs.len(), 1, "the model picker is open");
    assert!(!r.live.busy, "nothing was sent");
    // the Refresh row is first; typing filters; a filter with no match says so
    assert_eq!(r.live.dialogs[0].options[0].0, "refresh");
    assert!(r.live.model_filter_key(Some("zzz"), false));
    let last = r.live.dialogs[0].options.last().unwrap();
    assert_eq!(last.0, "info");
    assert!(r.live.model_filter_key(None, true));
    // Esc keeps the model in use and the prompt goes out
    assert_eq!(r.live.reject(), None);
    assert!(r.live.dialogs.is_empty() && r.live.model_pick.is_none());
    assert_eq!(r.live.next_queued().as_deref(), Some("hello"));
    // asked once
    r.live.busy = false;
    assert_eq!(r.live.submit("again"), Submitted::Send("again".into()));
    let _ = &r.engine;
}

#[test]
fn a_refresh_row_runs_once_and_its_failure_shows_as_a_row() {
    use synthesis_desktop::live::Action;
    let mut r = rig(serde_json::json!([]));
    assert_eq!(r.live.submit("/model"), Submitted::Nothing);
    let now = 1_000_000;
    let key = r.live.pick(0, now).unwrap();
    assert_eq!(r.live.answer(&key, now), Some(Action::RefreshModels));
    assert_eq!(
        r.live.dialogs.len(),
        1,
        "the picker stays open while it refreshes"
    );
    // a second press while one runs starts nothing
    let key = r.live.pick(0, now).unwrap();
    assert_eq!(r.live.answer(&key, now), None);
    r.live
        .models_refreshed(vec![], vec!["omniroute: no answer in time".into()]);
    let d = &r.live.dialogs[0];
    assert!(
        d.options[1]
            .1
            .contains("refresh failed - omniroute: no answer in time")
    );
    assert!(!r.live.model_pick.as_ref().unwrap().refreshing);
}

#[test]
fn open_is_a_known_command_that_hands_the_typed_path_to_the_window() {
    use synthesis_desktop::live::Action;
    let mut r = rig(serde_json::json!([]));
    assert_eq!(
        r.live.submit("/open ~/work"),
        Submitted::Do(Action::Open("~/work".into()))
    );
    assert_eq!(
        r.live.submit("/open"),
        Submitted::Nothing,
        "no path: an error row"
    );
}

#[test]
fn send_now_routing() {
    let mut r = rig(serde_json::json!([]));
    let inject = |to: &str, text: &str| Submitted::Inject {
        to: to.into(),
        text: text.into(),
    };
    // idle: starts a task, send now or not
    assert_eq!(r.live.submit_as("a", true), Submitted::Send("a".into()));
    // busy: Enter queues, Cmd+Enter injects into the running turn
    assert_eq!(r.live.submit("b"), Submitted::Nothing);
    assert_eq!(r.live.submit_as("c", true), inject("main", "c"));
    assert_eq!(r.live.queue, ["b"]);
    // commands and shell lines are never injected; they queue as today
    for t in ["!ls", "!!ls"] {
        assert_eq!(r.live.submit_as(t, true), Submitted::Nothing, "{t}");
    }
    assert!(!matches!(
        r.live.submit_as("/help", true),
        Submitted::Inject { .. }
    ));
    assert_eq!(r.live.queue.len(), 3);
    // "Send now" on a queued row takes it out of the queue; a command row stays
    assert_eq!(r.live.send_queued_now(0), inject("main", "b"));
    assert_eq!(r.live.send_queued_now(0), Submitted::Nothing);
    assert_eq!(r.live.queue.len(), 2);
    // a sub-agent target delivers with plain Enter, and falls back to main when it is gone
    r.live.agents = vec!["worker".into()];
    r.live.cycle_target();
    assert_eq!(r.live.target, "worker");
    assert_eq!(r.live.submit("d"), inject("worker", "d"));
    r.live.agents.clear();
    // the agent is gone: nothing is sent or queued, the text stays in the prompt
    assert_eq!(r.live.submit("e"), Submitted::Held);
    assert_eq!(r.live.target, "main");
    assert_eq!(r.live.queue.len(), 2);
    assert_eq!(
        r.live.submit("e"),
        Submitted::Nothing,
        "main target: queued"
    );
    r.live.queue.pop_back();
    // a multi-line message is a legitimate send now; only a first line with `/` or `!` is a command
    assert_eq!(
        r.live.submit_as("one\n/two", true),
        inject("main", "one\n/two")
    );
    assert_eq!(r.live.submit_as("/x\nmore", true), Submitted::Nothing);
    assert!(
        r.engine
            .send_now("main".into(), "!ls\nmore".into())
            .is_err()
    );
    // an open dialog holds delivery: the line stays queued (main) or in the prompt (sub-agent)
    r.live.queue.clear();
    r.live.open_setup("test", 0);
    assert_eq!(r.live.submit_as("f", true), Submitted::Nothing);
    assert_eq!(r.live.queue, ["f"]);
    r.live.agents = vec!["worker".into()];
    r.live.cycle_target();
    assert_eq!(r.live.submit("g"), Submitted::Held);
    assert_eq!(r.live.queue, ["f"]);
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}

#[test]
fn a_send_now_during_an_approval_is_delivered_after_the_tool_result_and_leaves_the_queue_alone() {
    let mut r = rig(serde_json::json!([
        {"text": "Writing the file.", "tools": [{"id": "w1", "name": "Write", "input": {"file_path": "{TARGET}", "content": "hi\n"}}]},
        {"text": "Finished."},
        {"text": "Nothing to verify here."}
    ]));
    r.send("make hello.txt");
    r.pump("dialog", |l| !l.dialogs.is_empty());
    r.live.queue.push_back("later".into());
    // the window queues a line sent now while the dialog is open ...
    assert_eq!(
        r.live.submit_as("queued by the window", true),
        Submitted::Nothing
    );
    r.live.queue.pop_back();
    // ... but the engine still holds a line that gets through (a race) until the answer
    r.live
        .injected
        .push(("main".into(), "also add a note".into()));
    r.engine
        .send_now("main".into(), "also add a note".into())
        .unwrap();
    r.pump("accepted", |l| !l.pending.is_empty());
    let user_rows = |l: &Live| {
        l.view
            .convo
            .items
            .iter()
            .filter(|i| matches!(i, Item::User(t) if t == "also add a note"))
            .count()
    };
    assert_eq!(user_rows(&r.live), 0, "an approval holds delivery");
    assert!(!r.target.exists());
    let t0 = r.live.dialogs[0].armed_until;
    let k = r.live.pick(0, t0).unwrap();
    r.live.answer(&k, t0);
    r.pump("task end", |l| !l.busy);
    let items = &r.live.view.convo.items;
    let at = |f: &dyn Fn(&Item) -> bool| items.iter().position(f).unwrap();
    let tool = at(&|i| matches!(i, Item::Tool { name, .. } if name == "Write"));
    let user = at(&|i| matches!(i, Item::User(t) if t == "also add a note"));
    assert!(user > tool, "delivered after the tool result: {items:?}");
    assert!(matches!(&items[user + 1], Item::Notice { text, .. } if text.contains("sent now")));
    assert!(r.live.pending.is_empty());
    assert_eq!(r.live.queue, ["later"], "the queue is untouched");
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}
