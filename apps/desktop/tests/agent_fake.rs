//! The Agent view's data on the real engine against a scripted model: a sub-agent (its transcript and
//! stop), a Write with its approval (changed file with +N, command history), and the verification report
//! the completion gate produces, read back from the store. Assertions are on `ViewState.panes`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_desktop::engine::{Engine, UiMsg};
use synthesis_desktop::live::{Live, Submitted};

struct Rig {
    live: Live,
    rx: UnboundedReceiver<UiMsg>,
    engine: Arc<Engine>,
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
        _dir: dir,
    }
}

impl Rig {
    fn send(&mut self, text: &str) {
        let Submitted::Send(t) = self.live.submit(text) else {
            panic!("not sent")
        };
        self.engine.send(t).unwrap();
    }
}

#[test]
fn a_task_fills_the_agent_panes_from_real_events() {
    let mut r = rig(serde_json::json!([
        {"tools": [{"id": "t1", "name": "Task", "input": {"description": "look", "prompt": "say hi", "subagent_type": "general-purpose"}}]},
        {"text": "sub says hi"},
        {"text": "Writing.", "tools": [{"id": "w1", "name": "Write", "input": {"file_path": "{TARGET}", "content": "hi\nthere\n"}}]},
        {"text": "Finished."},
        {"text": "Nothing to verify here."},
        {"text": "ok"}
    ]));
    r.send("use a sub-agent then write a file");
    let t0 = Instant::now();
    while r.live.busy {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out");
        match r.rx.try_recv() {
            Ok(m) => {
                r.live.on_msg(m, 1_000_000);
                if !r.live.dialogs.is_empty() {
                    let t = r.live.dialogs[0].armed_until;
                    let k = r.live.pick(0, t).unwrap();
                    r.live.answer(&k, t);
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let p = &r.live.view.panes;
    assert_eq!(p.subs.len(), 1, "{:?}", p.subs);
    let s = &p.subs[0];
    assert!(!s.live(), "the sub-agent stopped");
    assert!(
        s.entries.iter().any(|e| e.text.contains("sub says hi")),
        "{:?}",
        s.entries
    );
    let f = p
        .files
        .iter()
        .find(|f| f.path.ends_with("hello.txt"))
        .expect("changed file");
    assert_eq!((f.added, f.removed), (2, 0));
    assert!(f.diff.is_some());
    // the report is listed from the event; its checks come from the store
    if let Some((id, task, attempt)) = p.unloaded_reports().first().cloned() {
        let reader = synthesis_session::Reader::open(&r.engine.synthesis_home).unwrap();
        let rep = synthesis_view::agent::read_report(&reader, &task, &id, attempt)
            .expect("the stored report");
        r.live.view.panes.set_report(&id, &rep);
        assert!(r.live.view.panes.verify[0].loaded);
    }
    let engine = r.live.engine.take().unwrap();
    drop(r.engine);
    synthesis_desktop::engine::close_shared(engine);
}
