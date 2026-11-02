//! Performance sanity: a 120x40 frame with a 5k-item history. The warm case (row cache filled) is
//! what a UI redraw costs; the cold case also lays out every item once.

use serde_json::json;
use std::time::Instant;
use synthesis_tui_render::agent::{Agent, Item};
use synthesis_tui_render::dash::*;
use synthesis_tui_render::model::Session;

fn mk(f: impl FnOnce(&mut Item)) -> Item {
    let mut it = Item::default();
    f(&mut it);
    it
}

fn history(n: usize) -> Vec<Item> {
    (0..n)
        .map(|i| match i % 4 {
            0 => mk(|it| {
                it.kind = "user".into();
                it.text = format!("please look at module {i} and explain how the retry loop works");
            }),
            1 => mk(|it| {
                it.kind = "text".into();
                it.text = format!("# Notes {i}\n- first **point** with `code`\n- second point that is long enough to wrap around the pane width a few times over\n\n```rs\nfn f(x: u32) -> u32 {{ x + 1 }}\n```");
            }),
            2 => mk(|it| {
                it.kind = "tool".into();
                it.name = "Bash".into();
                it.input = json!({"command": format!("cargo test -p crate{i}")});
                it.status = "done".into();
                it.result = "ok\nline two\nline three\nline four".into();
            }),
            _ => mk(|it| {
                it.kind = "tool".into();
                it.name = "Edit".into();
                it.input = json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b\nc"});
                it.status = "done".into();
                it.result = "ok".into();
            }),
        })
        .collect()
}

#[test]
fn frame_with_5k_history_is_fast() {
    let session = Session::new("perf");
    let agent = Agent {
        cwd: "/work/proj".into(),
        mode: "default".into(),
        status: "idle".into(),
        items: history(5000),
        ..Default::default()
    };
    let usage = Usage::default();
    let view = DashView::default();
    let st = DashState {
        session: &session,
        sessions: &[],
        index: 0,
        usage: &usage,
        follow: false,
        history: &[],
        usage_metric: "tokens",
        mcp_servers: &[],
        mcp_live: None,
        workspaces: None,
        active_workspace: "",
        agent: Some(&agent),
        chat_agent: None,
        editor: Some(synthesis_tui_render::agent::EditorView { chars: &[], cur: 0 }),
        focus: Focus::Agent,
        view: &view,
        agent_session_id: None,
        tz_offset_min: 0,
    };
    let t = Instant::now();
    let cold = render_frame(&st, 120, 40, 0, 1_790_942_400_000);
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut best = f64::MAX;
    for f in 1..=10 {
        let t = Instant::now();
        let fr = render_frame(&st, 120, 40, f, 1_790_942_400_000);
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(fr.lines.len(), 40);
    }
    assert_eq!(cold.lines.len(), 40);
    println!(
        "PERF 120x40, 5k items: cold {cold_ms:.2} ms, warm best-of-10 {best:.2} ms (debug_assertions: {})",
        cfg!(debug_assertions)
    );
    if !cfg!(debug_assertions) {
        assert!(best < 20.0, "warm frame took {best:.2} ms");
    }
}
