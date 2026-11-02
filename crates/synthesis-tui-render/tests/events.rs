//! The session model built from a `synthesis_events` stream gives the same state as the Node
//! transcript path for the same conversation.

use serde_json::json;
use synthesis_events::{Envelope, Event, Todo, ToolStatus, Usage, WorkspaceInfo};
use synthesis_tui_render::model::{
    Phase, Session, apply_entry, session_from_events, session_phase,
};

fn env(seq: u64, ts: &str, event: Event) -> Envelope {
    Envelope {
        v: 1,
        seq,
        ts: ts.into(),
        session_id: "s1".into(),
        task_id: None,
        turn_id: None,
        agent_id: None,
        event,
    }
}

#[test]
fn event_stream_matches_transcript_model() {
    let content = vec![
        json!({"type": "text", "text": "Looking."}),
        json!({"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test"}}),
    ];
    let events = vec![
        env(
            1,
            "2026-10-02T11:50:00.000Z",
            Event::SessionStarted {
                workspace: WorkspaceInfo {
                    workspace_id: "w".into(),
                    workspace_key: None,
                    root: "/r".into(),
                    cwd: "/work/proj".into(),
                    additional_dirs: vec![],
                    git_worktree: None,
                },
                model: "claude-opus-5.5".into(),
                provider: None,
                permission_mode: "default".into(),
                source: "cli".into(),
                resumed_from_seq: None,
            },
        ),
        env(
            2,
            "2026-10-02T11:50:01.000Z",
            Event::UserMessage {
                text: "run the tests".into(),
                display_text: None,
                images: vec![],
            },
        ),
        env(
            3,
            "2026-10-02T11:50:02.000Z",
            Event::AssistantMessage {
                message_id: Some("m1".into()),
                content: content.clone(),
                stop_reason: "tool_use".into(),
                usage: Some(Usage {
                    input_tokens: 4000,
                    output_tokens: 600,
                    cache_read_input_tokens: Some(20000),
                    cache_creation_input_tokens: None,
                    cost_usd: None,
                }),
            },
        ),
        env(
            4,
            "2026-10-02T11:50:03.000Z",
            Event::TodoUpdated {
                todos: vec![Todo {
                    id: None,
                    content: "Fix".into(),
                    status: "in_progress".into(),
                    active_form: "Fixing".into(),
                }],
            },
        ),
    ];
    let mut s = session_from_events("s1", &events);
    assert_eq!(s.cwd, "/work/proj");
    assert_eq!(s.model, "claude-opus-5.5");
    assert_eq!((s.prompts, s.last_prompt.as_str()), (1, "run the tests"));
    assert_eq!(s.tokens.total(), 24600);
    assert_eq!(s.context_used, 24600);
    assert_eq!(s.todos[0].id, "1");
    let now = synthesis_tui_render::model::parse_ts("2026-10-02T11:50:04.000Z").unwrap();
    let ph = session_phase(&s, now);
    assert_eq!(ph.phase, Phase::Tool);
    assert_eq!(ph.running[0].cat, "verify"); // `cargo test`

    // the tool finishes with an error: counted, phase leaves `tool`
    let done = env(
        5,
        "2026-10-02T11:50:05.000Z",
        Event::ToolCompleted {
            tool_call_id: "t1".into(),
            name: "Bash".into(),
            status: ToolStatus::Error,
            is_error: true,
            not_run: false,
            duration_ms: 10,
            preview: "exit 1".into(),
            bytes: 6,
            truncated: false,
            result_ref: None,
            origin: None,
            output: Some("exit 1".into()),
            output_truncated: false,
            output_bytes: 6,
            diff: None,
        },
    );
    synthesis_tui_render::model::apply_event(&mut s, &done);
    assert_eq!(s.errors, 1);
    assert!(!s.last_verify.as_ref().unwrap().ok);

    // the same conversation as Node transcript entries gives the same counters
    let mut n = Session::new("s1");
    apply_entry(
        &mut n,
        &json!({"type": "user", "timestamp": "2026-10-02T11:50:01.000Z", "message": {"role": "user", "content": "run the tests"}}),
    );
    apply_entry(
        &mut n,
        &json!({"type": "assistant", "timestamp": "2026-10-02T11:50:02.000Z", "message": {"id": "m1", "content": content, "stop_reason": "tool_use",
        "usage": {"input_tokens": 4000, "output_tokens": 600, "cache_read_input_tokens": 20000}}}),
    );
    assert_eq!(n.tokens, s.tokens);
    assert_eq!(n.tool_counts, s.tool_counts);
}
