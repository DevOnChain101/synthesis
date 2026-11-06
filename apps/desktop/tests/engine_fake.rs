//! The real engine in this process against a scripted model: a task streams events into the
//! conversation reducer, a Write waits for an approval, and the answer decides whether the file is
//! created (docs/specs/desktop.md section 1). Needs the `test-provider` feature of synthesis-app,
//! which the dev-dependency below turns on for tests only.

use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_desktop::engine::{Choice, Engine, UiMsg};
use synthesis_view::convo::{ApprovalState, Convo, Item};

struct Rig {
    engine: Engine,
    rx: UnboundedReceiver<UiMsg>,
    convo: Convo,
    target: std::path::PathBuf,
    done: Option<Result<(), String>>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ws = root.join("ws");
    let home = root.join("home");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let target = ws.join("hello.txt");
    let script = serde_json::json!([
        {"text": "Writing the file.", "tools": [
            {"id": "w1", "name": "Write", "input": {"file_path": target, "content": "hi\n"}}
        ]},
        {"text": "Finished."}
    ]);
    let script_path = root.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let (engine, rx) = Engine::start(
        &ws,
        &env,
        None,
        Some(&format!("fake:{}", script_path.display())),
        // no bubblewrap on CI Linux; the sandbox is not what this test is about
        Some("off"),
    )
    .expect("engine starts");
    Rig {
        engine,
        rx,
        convo: Convo::default(),
        target,
        done: None,
        _dir: dir,
    }
}

impl Rig {
    /// Feeds engine messages into the reducer until `stop` holds.
    fn pump(&mut self, what: &str, stop: impl Fn(&Rig) -> bool) {
        let t0 = Instant::now();
        while !stop(self) {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {what}");
            match self.rx.try_recv() {
                Ok(UiMsg::Event(e)) => self.convo.apply(&e),
                Ok(UiMsg::TaskDone(r)) => self.done = Some(r),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// The approval event reaches the reducer a moment before the approver registers its reply
    /// slot (the harness emits, then asks), so a fast answer can land first: retry until it takes.
    fn answer(&mut self, id: &str, choice: Choice) {
        self.pump("approval slot", |r| r.engine.answer(id, choice));
    }

    fn pending_id(&self) -> String {
        self.convo
            .items
            .iter()
            .find_map(|i| match i {
                Item::Approval {
                    id,
                    state: ApprovalState::Pending,
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .expect("a pending approval")
    }
}

#[test]
fn allow_once_runs_the_write_and_the_task_finishes() {
    let mut r = rig();
    r.engine.send("make hello.txt".into()).unwrap();
    r.pump("approval", |r| r.convo.needs_you() == 1);
    assert!(!r.target.exists(), "nothing is written before the answer");
    let id = r.pending_id();
    r.answer(&id, Choice::Once);
    r.pump("task end", |r| r.done.is_some());
    assert_eq!(r.done, Some(Ok(())));
    assert_eq!(std::fs::read_to_string(&r.target).unwrap(), "hi\n");
    assert_eq!(r.convo.needs_you(), 0);
    assert!(
        r.convo
            .items
            .iter()
            .any(|i| matches!(i, Item::Assistant { text, .. } if text == "Finished."))
    );
    r.engine.close();
}

#[test]
fn deny_leaves_the_file_uncreated() {
    let mut r = rig();
    r.engine.send("make hello.txt".into()).unwrap();
    r.pump("approval", |r| r.convo.needs_you() == 1);
    let id = r.pending_id();
    r.answer(&id, Choice::Deny);
    r.pump("task end", |r| r.done.is_some());
    assert!(!r.target.exists());
    assert!(r.convo.items.iter().any(|i| matches!(
        i,
        Item::Approval {
            state: ApprovalState::Denied,
            ..
        }
    )));
    r.engine.close();
}
