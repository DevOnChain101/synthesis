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
    assert!(snap.prompts.iter().any(|p| p.text == "make hello.txt"));
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
