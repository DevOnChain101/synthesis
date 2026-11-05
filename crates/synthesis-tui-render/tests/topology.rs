//! Topology wires: render goldens of the flow canvas with 0..4 sub-agents at several panel sizes
//! (`tests/golden/topology/<n>-agents.txt`, colour-agnostic); rewrite with
//! `SYNT_UPDATE_GOLDEN=1 cargo test -p synthesis-tui-render --test topology`.
//! Endpoint rules: a node with one wire attaches at the midpoint of its edge; the core's wires on
//! one edge are mirror-symmetric about that edge's centre (unit tests in `flow.rs`).

use serde_json::json;
use std::path::PathBuf;
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::flow::{FlowOpts, render_flow};
use synthesis_tui_render::model::{Session, apply_entry};

const NOW: i64 = 1_800_000_000_000;

fn session(agents: usize) -> Session {
    let mut s = Session::new("s1");
    for i in 0..agents {
        apply_entry(
            &mut s,
            &json!({
                "type": "assistant",
                "timestamp": "2027-01-15T08:00:00.000Z",
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": format!("t{i}"), "name": "Task",
                    "input": {"subagent_type": "explore", "description": format!("scan part {i}")}
                }]}
            }),
        );
    }
    s
}

fn scene(agents: usize) -> String {
    let s = session(agents);
    let mut out = String::new();
    // stack 1 column, stack 2 columns, ring (side ports off / on), wide ring, fan fallback
    for (w, h) in [
        (40, 48),
        (60, 30),
        (70, 14),
        (70, 22),
        (110, 22),
        (150, 30),
        (60, 11),
    ] {
        out += &format!("--- {w}x{h}\n");
        for row in render_flow(
            &s,
            w,
            h,
            0,
            NOW,
            // a known window: the wires are under test, not the unknown-window "?"
            FlowOpts {
                context_size: Some(200_000.0),
                ..FlowOpts::default()
            },
        ) {
            out += strip_ansi(&row).trim_end();
            out.push('\n');
        }
    }
    out
}

#[test]
fn wires_attach_at_edge_midpoints_with_0_to_4_sub_agents() {
    for n in [0, 1, 2, 3, 4] {
        let text = scene(n);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/golden/topology")
            .join(format!("{n}-agents.txt"));
        if std::env::var_os("SYNT_UPDATE_GOLDEN").is_some_and(|v| v == "1") {
            std::fs::write(&path, &text).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{n} agents");
    }
}
