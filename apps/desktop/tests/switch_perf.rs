//! Regression guard for the workspace/session switch: the store panels, the resume replay and the
//! per-frame snapshot must not grow with the square of the store (generous budgets; they catch a
//! quadratic, not a slow disk). `SYNT_PERF_EVENTS=2000 cargo test --release --test switch_perf --
//! --nocapture` prints the numbers for the heavy store (3 workspaces x 40 sessions, the first of each N events).

use std::path::Path;
use std::time::{Duration, Instant};

use clap::Parser as _;
use serde_json::json;
use synthesis_config::Env;
use synthesis_core::{PermissionMode, SessionStatus, WorkspaceContext};
use synthesis_desktop::engine::{self, Args};
use synthesis_desktop::live::Live;
use synthesis_desktop::tui_view::driver::{LiveCfg, Target, build, load_convo, read_store};
use synthesis_desktop::tui_view::snapshot;
use synthesis_events::{Budget, Event, ToolStatus};
use synthesis_session::{NewMessage, NewSession, Store, StoreOptions};

const PER: usize = 20;

fn fill(store: &Store, id: &str, dir: &Path, events: usize) {
    let ws = WorkspaceContext::open("w1", dir, dir, &[], dir.join(".art"), "pg").unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some(id.into()),
            workspace: &ws,
            model_config_id: "m".into(),
            permission_mode: PermissionMode::AcceptEdits,
            permission_policy_id: "default".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    let text = "fix the build and explain what went wrong";
    w.append_message(NewMessage::user(json!(text))).unwrap();
    let mut n = 0;
    while n < events {
        let call = format!("c{n}");
        let evs = [
            Event::UserMessage {
                text: text.into(),
                display_text: None,
                images: vec![],
            },
            Event::TurnStarted {
                model: "m".into(),
                provider: "p".into(),
                iteration: 1,
            },
            Event::AssistantMessage {
                message_id: None,
                content: vec![json!({"type": "text", "text": "looking at it"})],
                stop_reason: "tool_use".into(),
                usage: None,
            },
            Event::ToolStarted {
                tool_call_id: call.clone(),
                name: "Read".into(),
                side_effect: "read".into(),
                cwd: dir.to_string_lossy().into(),
            },
            Event::ToolCompleted {
                tool_call_id: call,
                name: "Read".into(),
                status: ToolStatus::Ok,
                is_error: false,
                not_run: false,
                duration_ms: 3,
                preview: "line one\nline two".into(),
                bytes: 17,
                truncated: false,
                result_ref: None,
                origin: None,
                output: Some("line one\nline two".into()),
                output_truncated: false,
                output_bytes: 17,
                diff: None,
            },
            Event::TurnCompleted {
                stop_reason: "end_turn".into(),
                usage: None,
                duration_ms: 10,
            },
        ];
        n += evs.len();
        for e in evs {
            w.append_event(e).unwrap();
        }
    }
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Idle, None).unwrap();
}

#[test]
fn switch_and_resume_stay_within_budget() {
    // the first session of each workspace is the long one (2000 events; the env var raises it)
    let long: usize = std::env::var("SYNT_PERF_EVENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let t = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(t.path()).unwrap();
    let home = base.join("home");
    let opts = StoreOptions {
        write_node_transcripts: false,
        ..StoreOptions::default()
    };
    let store = Store::open(&home, opts).unwrap();
    let dirs: Vec<_> = (0..3)
        .map(|i| {
            let d = base.join(format!("ws{i}"));
            std::fs::create_dir_all(&d).unwrap();
            d
        })
        .collect();
    let t0 = Instant::now();
    for (i, d) in dirs.iter().enumerate() {
        for s in 0..40 {
            let n = if s == 0 { long } else { PER };
            fill(&store, &format!("s{i}-{s:02}-perfid"), d, n);
        }
    }
    eprintln!("perf: filled the store in {:?}", t0.elapsed());
    drop(store);

    let trace = std::env::var("SYNT_TRACE_UI").unwrap_or_default();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
        ("SYNT_TRACE_UI", trace.as_str()),
    ]);
    let script = base.join("script.json");
    std::fs::write(&script, r#"[{"text":"ok"}]"#).unwrap();
    let provider = format!("fake:{}", script.display());
    let cfg = LiveCfg {
        granted: engine::root_of(&dirs[0]),
        project: dirs[0].clone(),
        synthesis_home: home.clone(),
        home: None,
        tz_min: 0,
        tui: Default::default(),
        args: Args::try_parse_from([
            "synthesis-desktop",
            "--provider",
            &provider,
            "--sandbox",
            "off",
        ])
        .unwrap(),
        env,
    };
    let time = |what: &str, budget: Duration, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        let d = t.elapsed();
        eprintln!("perf: {what}: {d:?} (budget {budget:?})");
        assert!(d < budget, "{what} took {d:?}, budget {budget:?}");
    };

    {
        let r = synthesis_session::Reader::open(&home).unwrap();
        let t = Instant::now();
        let recs = r.list_used_sessions(None).unwrap();
        eprintln!(
            "perf: list_used_sessions {} rows: {:?}",
            recs.len(),
            t.elapsed()
        );
        let t = Instant::now();
        let opts = synthesis_view::workspaces::LoadOpts::default();
        synthesis_view::workspaces::WorkspaceList::load(&r, None, &opts).unwrap();
        eprintln!("perf: WorkspaceList::load: {:?}", t.elapsed());
        let t = Instant::now();
        synthesis_view::usage::load_from(&r, 1_900_000_000_000, 0);
        eprintln!("perf: usage::load_from: {:?}", t.elapsed());
    }
    // the workspace list + usage history, read on every store refresh (switch, task end)
    time(
        "store listing (120 sessions)",
        Duration::from_millis(200),
        &mut || {
            let (ws, _) = read_store(&cfg, dirs[0].to_str()).unwrap();
            assert_eq!(ws.workspaces.len(), 3);
        },
    );

    // resume: load the events, replay them into the view state
    let mut resume = None;
    time(
        "load_resume (long session)",
        Duration::from_secs(2),
        &mut || {
            resume = Some(synthesis_tui::load_resume(&home, "s0-00-perfid").unwrap());
        },
    );
    let resume = resume.unwrap();
    let mut view = synthesis_view::state::ViewState::default();
    time(
        "replay (long session)",
        Duration::from_millis(500),
        &mut || {
            for e in &resume.events {
                view.apply(e);
            }
        },
    );
    eprintln!(
        "perf: {} events, {} items",
        resume.events.len(),
        view.convo.items.len()
    );

    // the preview of a selected row: what the chat shows before any engine exists
    let mut convo = None;
    time(
        "preview load_convo (long session)",
        Duration::from_millis(1000),
        &mut || convo = Some(load_convo(&home, "s0-00-perfid").unwrap()),
    );
    assert_eq!(convo.unwrap().items.len(), view.convo.items.len());
    time(
        "preview load_convo (short session)",
        Duration::from_millis(200),
        &mut || {
            load_convo(&home, "s0-01-perfid").unwrap();
        },
    );

    // the frame after the switch, and every dirty frame (the 1 s tick marks it dirty)
    let mut live = Live::new();
    live.view.convo = view.convo;
    live.view.session = view.session;
    live.view.status = view.status;
    time(
        "snapshot build (long session)",
        Duration::from_millis(50),
        &mut || {
            let _ = snapshot::build(&live, 1_000_000, 0, None);
        },
    );

    // the engine relaunch of a click on a session (A -> B -> A): off the UI thread, but it is the
    // time until the conversation shows
    let mut prior = None;
    for id in ["s0-00-perfid", "s1-00-perfid", "s0-00-perfid"] {
        let mut l = None;
        time(
            &format!("engine launch, resume {id}"),
            Duration::from_secs(5),
            &mut || {
                l = Some(
                    build(
                        &cfg,
                        dirs[0].clone(),
                        prior.take(),
                        Some(Target::Resume(id.into())),
                        None,
                        None,
                        vec![],
                    )
                    .unwrap(),
                );
            },
        );
        prior = l.map(|l| std::sync::Arc::new(l.engine));
    }
}

/// Manual: `SYNT_PERF_HOME=<a COPY of ~/.synt>` replays A -> B -> A on its newest sessions with the
/// real config, memory and project files (scripted model). Skipped when the variable is unset.
#[test]
fn copy_of_a_real_home() {
    let Ok(home) = std::env::var("SYNT_PERF_HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);
    let script = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(script.path(), r#"[{"text":"ok"}]"#).unwrap();
    let provider = format!("fake:{}", script.path().display());
    let env = Env::from_pairs([
        ("HOME", std::env::var("HOME").unwrap()),
        ("SYNT_HOME", home.to_string_lossy().into_owned()),
        ("SYNT_TRACE_UI", "1".into()),
    ]);
    let args = Args::try_parse_from([
        "synthesis-desktop",
        "--provider",
        &provider,
        "--sandbox",
        "off",
    ])
    .unwrap();
    let project = std::path::PathBuf::from(env.get("HOME").unwrap());
    let cfg = LiveCfg {
        granted: engine::root_of(&project),
        project: project.clone(),
        synthesis_home: home.clone(),
        home: None,
        tz_min: 0,
        tui: Default::default(),
        args,
        env,
    };
    let (ws, _) = read_store(&cfg, None).unwrap();
    let ids: Vec<_> = ws
        .workspaces
        .iter()
        .filter(|w| w.available)
        .filter_map(|w| w.sessions.first().map(|s| s.id.clone()))
        .take(2)
        .collect();
    eprintln!(
        "perf: {} workspaces, switching between {} sessions",
        ws.workspaces.len(),
        ids.len()
    );
    let mut prior = None;
    for id in [&ids[0], &ids[1], &ids[0]] {
        let t = Instant::now();
        let l = build(
            &cfg,
            project.clone(),
            prior.take(),
            Some(Target::Resume(id.clone())),
            None,
            None,
            vec![],
        );
        eprintln!("perf: real home, resume {id}: {:?}", t.elapsed());
        prior = l.ok().map(|l| std::sync::Arc::new(l.engine));
    }
}
