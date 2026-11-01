//! Rust-native expectations for the display-limit policy: ONLY tasks, bash output and code are
//! clamped to 6 lines (footer `… +N more lines (ctrl+o to expand)`, ctrl+o shows all); every
//! other kind is shown whole. The Node-derived goldens encode the old collapse behaviour.

use serde_json::{Value, json};
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{Agent, AgentOpts, EditorView, Item, render_agent};
use synthesis_tui_render::model::Todo;

fn render_with(items: Vec<Value>, todos: &[Value], expanded: bool, w: usize) -> Vec<String> {
    let items: Vec<Item> = items
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
    let todos: Vec<Todo> = todos
        .iter()
        .map(|v| serde_json::from_value(v.clone()).unwrap())
        .collect();
    let agent = Agent {
        items,
        ..Agent::default()
    };
    let opts = AgentOpts {
        expanded,
        ..AgentOpts::default()
    };
    let ed = EditorView { chars: &[], cur: 0 };
    render_agent(&agent, &todos, &ed, w, 800, 0, 1_000_000, &opts)
        .lines
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn render(items: Vec<Value>, expanded: bool) -> Vec<String> {
    render_with(items, &[], expanded, 100)
}

fn numbered(prefix: &str, n: usize) -> String {
    (1..=n)
        .map(|i| format!("{prefix} {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

const MORE: &str = "more lines (ctrl+o to expand)";
const LESS: &str = "(ctrl+o to collapse)";

#[test]
fn code_fences_clamp_at_six_and_expand() {
    let text = format!("intro\n```rs\n{}\n```\noutro", numbered("let v =", 20));
    let item = json!({"kind": "text", "text": text});
    let l = render(vec![item.clone()], false);
    assert_eq!(count(&l, "let v ="), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("let v = 1") && !x.contains("let v = 10"))
    );
    assert!(
        l.iter()
            .any(|x| x.contains("… +14 more lines (ctrl+o to expand)")),
        "{l:#?}"
    );
    assert!(l.iter().any(|x| x.contains("outro")));
    let l = render(vec![item], true);
    assert_eq!(count(&l, "let v ="), 20);
    assert!(l.iter().any(|x| x.contains(LESS)));
    assert!(!l.iter().any(|x| x.contains(MORE)));
    // six lines are not clamped, no footer
    let six = json!({"kind": "text", "text": format!("```\n{}\n```", numbered("x", 6))});
    let l = render(vec![six], false);
    assert!(
        !l.iter()
            .any(|x| x.contains("more lines") || x.contains(LESS)),
        "{l:#?}"
    );
}

#[test]
fn finished_bash_shows_first_six_and_running_bash_the_last_six() {
    let done = json!({"kind": "tool", "name": "Bash", "status": "done",
        "input": {"command": "seq 30"}, "result": numbered("out", 30)});
    let l = render(vec![done.clone()], false);
    assert_eq!(count(&l, "out "), 6, "{l:#?}");
    assert!(l.iter().any(|x| x.ends_with("out 1")));
    assert!(
        l.iter()
            .any(|x| x.contains("… +24 more lines (ctrl+o to expand)"))
    );
    let l = render(vec![done], true);
    assert_eq!(count(&l, "out "), 30);
    assert!(l.iter().any(|x| x.contains(LESS)));

    let run = json!({"kind": "tool", "name": "Bash", "status": "running",
        "input": {"command": "seq 30"}, "output": numbered("out", 30)});
    let l = render(vec![run.clone()], false);
    assert_eq!(count(&l, "out "), 6, "{l:#?}");
    assert!(l.iter().any(|x| x.ends_with("out 30")), "tail: {l:#?}");
    assert!(!l.iter().any(|x| x.ends_with("out 1")));
    assert!(
        l.iter()
            .any(|x| x.contains("… +24 more lines (ctrl+o to expand)"))
    );
    assert_eq!(count(&render(vec![run], true), "out "), 30);

    // terminal_* tools are bash too; the user's own `!cmd` rows as well
    let term = json!({"kind": "tool", "name": "terminal_logs", "status": "done",
        "input": {}, "result": numbered("log", 10)});
    assert_eq!(count(&render(vec![term], false), "log "), 6);
    let user =
        json!({"kind": "bash", "command": "seq 10", "status": "done", "output": numbered("u", 10)});
    let l = render(vec![user], false);
    assert_eq!(count(&l, "u "), 6, "{l:#?}");
}

#[test]
fn code_rows_clamp_at_six_first_lines() {
    let read = json!({"kind": "tool", "name": "Read", "status": "done",
        "input": {"file_path": "a.rs"}, "result": numbered("src", 12)});
    let l = render(vec![read.clone()], false);
    assert_eq!(count(&l, "src "), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("+6 more lines (ctrl+o to expand)"))
    );
    assert_eq!(count(&render(vec![read], true), "src "), 12);

    let write = json!({"kind": "tool", "name": "Write", "status": "done",
        "input": {"file_path": "a.txt", "content": numbered("w", 9)}, "result": "ok"});
    let l = render(vec![write], false);
    assert!(
        l.iter().any(|x| x.contains("Wrote a.txt (9 lines)")),
        "{l:#?}"
    );
    assert_eq!(count(&l, " w "), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("+3 more lines (ctrl+o to expand)"))
    );

    let edit = json!({"kind": "tool", "name": "Edit", "status": "done", "result": "ok",
        "input": {"file_path": "a.txt", "old_string": numbered("old", 5), "new_string": numbered("new", 5)}});
    let l = render(vec![edit.clone()], false);
    assert_eq!(count(&l, "− old") + count(&l, "+ new"), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("+4 more lines (ctrl+o to expand)"))
    );
    let l = render(vec![edit], true);
    assert_eq!(count(&l, "− old") + count(&l, "+ new"), 10);

    let nb = json!({"kind": "tool", "name": "NotebookEdit", "status": "done", "result": "ok",
        "input": {"notebook_path": "n.ipynb", "new_source": numbered("cell", 8)}});
    assert_eq!(count(&render(vec![nb], false), "cell "), 6);
}

#[test]
fn task_lists_clamp_at_six() {
    let todo = json!({"kind": "tool", "name": "TodoWrite", "status": "done", "result": "ok",
        "input": {"todos": (1..=10).map(|i| json!({"content": format!("job {i}"), "status": "pending"})).collect::<Vec<_>>()}});
    let l = render(vec![todo.clone()], false);
    assert_eq!(count(&l, "job "), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("… +4 more lines (ctrl+o to expand)"))
    );
    assert_eq!(count(&render(vec![todo], true), "job "), 10);

    let list = json!({"kind": "tool", "name": "TaskList", "status": "done", "input": {}, "result": numbered("task", 9)});
    assert_eq!(count(&render(vec![list], false), "task "), 6);

    // the live task list above the prompt
    let todos: Vec<Value> = (1..=10)
        .map(|i| json!({"content": format!("step {i}"), "status": "pending"}))
        .collect();
    let l = render_with(vec![], &todos, false, 100);
    assert_eq!(count(&l, "step "), 6, "{l:#?}");
    assert!(
        l.iter()
            .any(|x| x.contains("… +4 more lines (ctrl+o to expand)"))
    );
    let l = render_with(vec![], &todos, true, 100);
    assert_eq!(count(&l, "step "), 10);
    assert!(l.iter().any(|x| x.contains(LESS)));
}

#[test]
fn nothing_else_is_hidden() {
    // 200-line assistant message, no code fence
    let prose = json!({"kind": "text", "text": numbered("prose line", 200)});
    let l = render(vec![prose], false);
    assert_eq!(count(&l, "prose line "), 200);
    assert!(
        !l.iter()
            .any(|x| x.contains("more lines") || x.contains('…'))
    );

    // other tools: 50-line grep result, web, mcp, verify, browser
    for name in [
        "Grep",
        "Glob",
        "WebFetch",
        "WebSearch",
        "mcp__srv__tool",
        "verify_run",
        "browser_snapshot",
        "Skill",
    ] {
        let it = json!({"kind": "tool", "name": name, "status": "done",
            "input": {"pattern": "x"}, "result": numbered("hit", 50)});
        let l = render(vec![it], false);
        assert_eq!(count(&l, "hit "), 50, "{name}: {l:#?}");
        assert!(
            !l.iter()
                .any(|x| x.contains("more lines") || x.contains("ctrl+o")),
            "{name}"
        );
    }

    // errors of non-clamped tools, long lines wrap without "…"
    let err = json!({"kind": "tool", "name": "Grep", "status": "error",
        "input": {}, "result": format!("{}\n{}", numbered("bad", 30), "w".repeat(500))});
    let l = render_with(vec![err], &[], false, 60);
    assert_eq!(count(&l, "bad "), 30);
    // (the prompt hint "Shift+Enter newline" has a w too)
    let w_count = |x: &&String| x.matches('w').count();
    assert_eq!(
        l.iter()
            .filter(|x| !x.contains("Enter send"))
            .map(|x| w_count(&x))
            .sum::<usize>(),
        500
    );
    assert!(!l.iter().any(|x| x.contains('…')), "{l:#?}");

    // thinking, finished and streaming
    for streaming in [false, true] {
        let th = json!({"kind": "thinking", "streaming": streaming, "start": 1, "end": 2,
            "text": numbered("thought", 40)});
        let l = render(vec![th], false);
        assert_eq!(count(&l, "thought "), 40, "streaming={streaming}");
    }

    // notes, command output, results
    let note = json!({"kind": "note", "tone": "error", "text": numbered("note", 30)});
    let out = json!({"kind": "command-output", "text": numbered("cmd", 30)});
    let l = render(vec![note, out], false);
    // a note reflows its text (newlines are spaces) but every word is there
    let all = l.join(" ");
    assert_eq!((all.matches("note ").count(), count(&l, "cmd ")), (30, 30));
}

#[test]
fn expanded_flag_is_part_of_the_row_cache() {
    let it: Item = serde_json::from_value(json!({"kind": "tool", "name": "Bash", "status": "done",
        "input": {"command": "x"}, "result": numbered("o", 20)}))
    .unwrap();
    let agent = Agent {
        items: vec![it],
        ..Agent::default()
    };
    let ed = EditorView { chars: &[], cur: 0 };
    let draw = |expanded| {
        let opts = AgentOpts {
            expanded,
            ..AgentOpts::default()
        };
        render_agent(&agent, &[], &ed, 100, 100, 0, 0, &opts)
            .lines
            .iter()
            .filter(|l| strip_ansi(l).contains(" o "))
            .count()
    };
    // same item (same cache slot) toggled back and forth, also across widths
    assert_eq!(
        (draw(false), draw(true), draw(false), draw(true)),
        (6, 20, 6, 20)
    );
}
