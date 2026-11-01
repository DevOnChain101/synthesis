//! `--once` frame equality with `render_frame`, and `--resume` over a real `synthesis-session` store.

use serde_json::json;
use synthesis_core::{Budget, PermissionMode, WorkspaceContext};
use synthesis_events::{Envelope, Event};
use synthesis_permissions::Mode;
use synthesis_session::{NewMessage, NewSession, Store, StoreOptions};
use synthesis_tui::{OnceOptions, SandboxStatus, list_saved, load_resume, render_once};
use synthesis_tui_base::sgr::{ColorMode, encode};
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{Agent, EditorView};
use synthesis_tui_render::dash::{
    DashState, DashView, Focus, Live, SessionRow, Usage, Workspace, render_frame, workspace_key,
};
use synthesis_tui_render::model::session_from_events;
use synthesis_tui_render::util::basename;

const NOW: i64 = 1_800_000_000_000;

fn once(o: &OnceOptions) -> Vec<String> {
    let mut out = Vec::new();
    render_once(o, &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

fn opts(events: Vec<Envelope>) -> OnceOptions {
    OnceOptions {
        cwd: "/work/proj".into(),
        session_id: "s1".into(),
        model: "fake/model".into(),
        version: "0.1".into(),
        budget: Budget::default(),
        sandbox: SandboxStatus {
            mode: "enforce".into(),
            backend: "seatbelt".into(),
            filesystem: "strong".into(),
            network: "strong".into(),
            unsandboxed_by_request: false,
        },
        mode: Mode::Default,
        events,
        saved: vec![],
        cols: 120,
        rows: 40,
        now: NOW,
        color: ColorMode::TrueColor,
        startup_notes: vec![],
    }
}

#[test]
fn once_is_render_frame_plus_the_status_line() {
    let o = opts(vec![]);
    let got = once(&o);
    assert_eq!(got.len(), 40);

    // the same state, assembled from the public renderer API only
    let mut session = session_from_events("s1", []);
    session.cwd = o.cwd.clone();
    session.project = basename(&o.cwd);
    session.model = o.model.clone();
    let agent = Agent {
        id: "s1".into(),
        kind: "main".into(),
        cwd: o.cwd.clone(),
        model: o.model.clone(),
        mode: "default".into(),
        version: "0.1".into(),
        status: "idle".into(),
        ..Agent::default()
    };
    let rows = [SessionRow {
        id: "s1".into(),
        project: basename(&o.cwd),
        cwd: o.cwd.clone(),
        mtime: NOW,
        live: Some(Live {
            name: "synthesis".into(),
            cwd: o.cwd.clone(),
            status: "idle".into(),
        }),
        phase: "idle".into(),
        tokens: 0.0,
        ctx_pct: None,
        waiting: false,
        mode: Some("default".into()),
    }];
    let key = workspace_key(&o.cwd);
    let ws = [Workspace {
        key: key.clone(),
        name: basename(&o.cwd),
        cwd: o.cwd.clone(),
        count: 1,
        waiting: false,
        busy: false,
    }];
    let view = DashView::default();
    let usage = Usage::default();
    let editor: [char; 0] = [];
    let state = DashState {
        session: &session,
        sessions: &rows,
        index: 0,
        usage: &usage,
        follow: false,
        history: &[],
        usage_metric: "tokens",
        mcp_servers: &[],
        mcp_live: None,
        workspaces: Some(&ws),
        active_workspace: &key,
        agent: Some(&agent),
        chat_agent: None,
        editor: Some(EditorView {
            chars: &editor,
            cur: 0,
        }),
        focus: Focus::Agent,
        view: &view,
        agent_session_id: Some("s1"),
        tz_offset_min: 0,
    };
    let frame = render_frame(&state, 120, 39, (NOW / 80) as usize, NOW);
    let want: Vec<String> = frame
        .lines
        .iter()
        .map(|l| encode(l, ColorMode::TrueColor))
        .collect();
    assert_eq!(&got[..39], &want[..], "--once is exactly render_frame");
    let status = strip_ansi(&got[39]);
    assert!(
        status.contains("sandbox: enforce [seatbelt fs:strong net:strong]"),
        "{status}"
    );
    assert!(status.contains("verify n/a"), "{status}");

    // deterministic: same inputs, same bytes
    assert_eq!(got, once(&o));
    // colours follow the injected mode
    let mut mono = opts(vec![]);
    mono.color = ColorMode::Mono;
    assert!(!once(&mono).join("\n").contains("38;2;"));
}

fn tmp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let d = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(d.path())
        .unwrap()
        .join("synthesis-home");
    std::fs::create_dir_all(&home).unwrap();
    let store = Store::open(&home, StoreOptions::default()).unwrap();
    (d, home, store)
}

#[test]
fn resume_loads_events_and_history_and_shows_them() {
    let (d, home, store) = tmp_store();
    let work = std::fs::canonicalize(d.path()).unwrap().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let ws = WorkspaceContext::open("w", &work, &work, &[], work.join(".art"), "pg").unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some("11111111-aaaa-bbbb-cccc-000000000001".into()),
            workspace: &ws,
            model_config_id: "p/m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "default".into(),
            budget: Budget::default(),
            parent: None,
            title: Some("my old session".into()),
        })
        .unwrap();
    w.append_event(Event::UserMessage {
        text: "what was the plan".into(),
        display_text: None,
        images: vec![],
    })
    .unwrap();
    w.append_message(NewMessage::user(json!("what was the plan")))
        .unwrap();
    w.append_message(NewMessage::assistant(
        json!([{"type": "text", "text": "ship it"}]),
    ))
    .unwrap();
    drop(w);

    let saved = list_saved(&home, None).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].title, "my old session");

    let r = load_resume(&home, "11111111").unwrap();
    assert_eq!(r.record.id, "11111111-aaaa-bbbb-cccc-000000000001");
    assert_eq!(r.history.len(), 2, "history feeds the next task");
    assert!(r.events.iter().any(
        |e| matches!(&e.event, Event::UserMessage { text, .. } if text == "what was the plan")
    ));

    let screen = once(&OnceOptions {
        events: r.events.clone(),
        saved,
        ..opts(vec![])
    })
    .join("\n");
    assert!(strip_ansi(&screen).contains("what was the plan"));

    assert!(
        load_resume(&home, "nope")
            .unwrap_err()
            .contains("no saved session")
    );
    assert!(
        load_resume(&home, " ")
            .unwrap_err()
            .contains("empty session id")
    );
}

#[test]
fn once_shows_startup_notes_escaped() {
    let screen = once(&OnceOptions {
        startup_notes: vec!["UNSANDBOXED: --sandbox off \x1b[2J\r\nforged".into()],
        ..opts(vec![])
    })
    .join("\n");
    let s = strip_ansi(&screen);
    assert!(s.contains("UNSANDBOXED: --sandbox off"), "{s}");
    assert!(
        !screen.contains("\x1b[2J") && !screen.contains('\r'),
        "{screen:?}"
    );
}
