//! Rust-native expectations for the chat-pane layout (thinking vs answer, one blank line between
//! blocks) and for the line-numbered diff under file-change rows. See also `display_limits.rs`.

use serde_json::{Value, json};
use synthesis_tui_base::text::{char_width, strip_ansi};
use synthesis_tui_render::agent::{Agent, AgentOpts, EditorView, Item, render_agent};

const NOW: i64 = 1_000_000;

fn draw(items: Vec<Value>, w: usize, expanded: bool, frame: usize) -> Vec<String> {
    let agent = Agent {
        items: items
            .into_iter()
            .map(|v| serde_json::from_value::<Item>(v).unwrap())
            .collect(),
        ..Agent::default()
    };
    draw_agent(&agent, w, expanded, frame)
}

fn draw_agent(agent: &Agent, w: usize, expanded: bool, frame: usize) -> Vec<String> {
    let opts = AgentOpts {
        expanded,
        ..AgentOpts::default()
    };
    let ed = EditorView { chars: &[], cur: 0 };
    let out = render_agent(agent, &[], &ed, w, 900, frame, NOW, &opts);
    out.lines
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .collect()
}

/// The conversation rows only: from the first non-blank row to the last, before the prompt box.
fn chat(lines: &[String]) -> Vec<String> {
    let first = lines.iter().position(|l| !l.is_empty()).unwrap();
    let end = lines
        .iter()
        .position(|l| l.starts_with('╭') || l.starts_with("· Working"))
        .unwrap_or(lines.len());
    let mut v: Vec<String> = lines[first..end].to_vec();
    while v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v
}

fn thinking(text: &str) -> Value {
    json!({"kind": "thinking", "text": text, "start": 1000, "end": 5200, "ms": 4200})
}
fn answer(text: &str) -> Value {
    json!({"kind": "text", "text": text})
}
fn tool(name: &str) -> Value {
    json!({"kind": "tool", "name": name, "status": "done", "input": {"command": "ls"}, "result": "ok"})
}
fn note(text: &str) -> Value {
    json!({"kind": "note", "text": text})
}

// ------------------------------------------------------------------ thinking

#[test]
fn thinking_block_has_header_duration_gutter_and_one_blank_before_the_answer() {
    let l = chat(&draw(
        vec![thinking("first\nsecond"), answer("The answer")],
        80,
        false,
        0,
    ));
    assert_eq!(l[0], "✻ Thinking · 4.2s", "{l:#?}");
    assert_eq!((l[1].as_str(), l[2].as_str()), ("│ first", "│ second"));
    assert_eq!(l[3], "", "exactly one blank");
    assert!(l[4].starts_with("● The answer"), "{l:#?}");
    assert_eq!(l.len(), 5);
}

#[test]
fn streaming_thinking_shows_live_timer_and_spinner_then_settles_without_moving_rows() {
    let it = json!({"kind": "thinking", "streaming": true, "text": "a", "start": NOW - 3100});
    let a = chat(&draw(vec![answer("before"), it.clone()], 80, false, 0));
    assert!(a.iter().any(|x| x == "· Thinking · 3.1s"), "{a:#?}");
    let b = chat(&draw(vec![answer("before"), it], 80, false, 3));
    assert!(b.iter().any(|x| x == "✻ Thinking · 3.1s"), "{b:#?}");
    // earlier blocks keep their rows while the block streams and when it ends
    let done = chat(&draw(vec![answer("before"), thinking("a")], 80, false, 0));
    assert_eq!(a.len(), done.len());
    assert_eq!(a[..2], done[..2]);
}

#[test]
fn empty_and_whitespace_thinking_is_omitted() {
    for t in ["", "  \n\t\n"] {
        let l = chat(&draw(
            vec![answer("hi"), thinking(t), answer("there")],
            80,
            false,
            0,
        ));
        assert!(!l.iter().any(|x| x.contains("Thinking")), "{l:#?}");
        assert_eq!(l.iter().filter(|x| x.is_empty()).count(), 1, "{l:#?}");
    }
}

#[test]
fn thinking_is_never_clamped_and_long_lines_wrap_under_the_gutter() {
    let long = "w".repeat(300);
    let text = format!(
        "{}\n{long}",
        (1..=30)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    for w in [40, 80] {
        let l = chat(&draw(vec![thinking(&text)], w, false, 0));
        assert!(
            l.iter()
                .all(|x| !x.contains("more lines") && !x.contains('…'))
        );
        let body = &l[1..];
        assert!(body.iter().all(|x| x.starts_with("│ ")), "w={w}: {body:#?}");
        assert_eq!(
            body.iter().map(|x| x.matches('w').count()).sum::<usize>(),
            300
        );
        assert!(
            l.iter()
                .all(|x| x.chars().map(char_width).sum::<usize>() <= w),
            "w={w}"
        );
    }
}

#[test]
fn multiple_thinking_blocks_each_get_a_header() {
    let l = chat(&draw(
        vec![
            thinking("one"),
            tool("Bash"),
            thinking("two"),
            answer("done"),
        ],
        60,
        false,
        0,
    ));
    assert_eq!(
        l.iter().filter(|x| x.starts_with("✻ Thinking")).count(),
        2,
        "{l:#?}"
    );
}

#[test]
fn exactly_one_blank_line_between_blocks_and_none_at_the_end() {
    let items = vec![
        json!({"kind": "user", "text": "go"}),
        thinking("t"),
        answer("a\n\n"),
        tool("Bash"),
        note("heads up"),
        thinking("t2\n\n"),
        answer("\nb\n"),
        note("n1"),
        note("n2"),
        tool("Bash"),
        answer("last\n\n\n"),
    ];
    for w in [40, 80, 120] {
        let l = chat(&draw(items.clone(), w, false, 0));
        assert!(!l.is_empty());
        for (i, pair) in l.windows(2).enumerate() {
            assert!(
                !(pair[0].is_empty() && pair[1].is_empty()),
                "w={w} double blank at {i}: {l:#?}"
            );
        }
        assert!(!l.last().unwrap().is_empty(), "w={w}: trailing blank");
        // a note is set off from a different kind before it
        let at = l.iter().position(|x| x.contains("heads up")).unwrap();
        assert_eq!(l[at - 1], "", "w={w}: {l:#?}");
        let n1 = l.iter().position(|x| x.contains("n1")).unwrap();
        assert!(l[n1 + 1].contains("n2"), "consecutive notes stay together");
    }
}

#[test]
fn row_cache_follows_the_thinking_text_and_finish() {
    let mut agent = Agent {
        items: vec![
            serde_json::from_value(
                json!({"kind": "thinking", "streaming": true, "text": "a", "start": 1}),
            )
            .unwrap(),
        ],
        ..Agent::default()
    };
    assert!(chat(&draw_agent(&agent, 80, false, 0)).contains(&"│ a".to_string()));
    let it = &mut agent.items[0];
    it.text.push_str(" b");
    it.streaming = false;
    it.end = Some(2);
    it.ms = 1500;
    it.rev += 1;
    let l = chat(&draw_agent(&agent, 80, false, 0));
    assert_eq!(
        (l[0].as_str(), l[1].as_str()),
        ("✻ Thinking · 1.5s", "│ a b")
    );
}

// ------------------------------------------------------------------ diff

fn hunk(old_start: u32, new_start: u32, lines: Vec<Value>) -> Value {
    let old_lines = lines.iter().filter(|l| l["tag"] != "+").count();
    let new_lines = lines.iter().filter(|l| l["tag"] != "-").count();
    json!({"old_start": old_start, "old_lines": old_lines, "new_start": new_start, "new_lines": new_lines, "lines": lines})
}
fn ctx(o: u32, n: u32, t: &str) -> Value {
    json!({"tag": " ", "old_no": o, "new_no": n, "text": t})
}
fn del(o: u32, t: &str) -> Value {
    json!({"tag": "-", "old_no": o, "text": t})
}
fn add(n: u32, t: &str) -> Value {
    json!({"tag": "+", "new_no": n, "text": t})
}
fn edit_row(diff: Value) -> Value {
    json!({"kind": "tool", "name": "Edit", "status": "done", "result": "Edited x",
        "input": {"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}, "diff": diff})
}

#[test]
fn edit_row_shows_header_summary_and_numbered_gutter_at_all_widths() {
    let diff = json!({"path": "src/lib.rs", "kind": "update", "added": 2, "removed": 1, "truncated": false,
        "hunks": [hunk(40, 40, vec![ctx(40, 40, "ctx"), del(41, "old"), add(41, "new"), add(42, "new2"), ctx(42, 43, "tail")])]});
    for w in [40, 80, 120] {
        let l = chat(&draw(vec![edit_row(diff.clone())], w, false, 0));
        assert_eq!(l[0], "● Update(src/lib.rs)", "w={w}");
        // the summary wraps at 40 columns; the gutter rows do not
        let at = l.iter().position(|x| x.starts_with("      40 40")).unwrap();
        let summary = l[1..at]
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            summary.contains("Updated src/lib.rs — +2 −1 (lines 40–43)"),
            "w={w}: {l:#?}"
        );
        // old / new numbers in fixed columns, marker, text
        assert_eq!(l[at], "      40 40   ctx");
        assert_eq!(l[at + 1], "      41    − old");
        assert_eq!(l[at + 2], "         41 + new");
        assert_eq!(l[at + 3], "         42 + new2");
        assert_eq!(l[at + 4], "      42 43   tail");
    }
}

#[test]
fn diff_body_clamps_at_six_rows_and_expands_with_numbers() {
    let lines: Vec<Value> = (1..=12).map(|i| add(i, &format!("row {i}"))).collect();
    let diff = json!({"path": "n.txt", "kind": "create", "added": 12, "removed": 0, "truncated": false,
        "hunks": [hunk(0, 1, lines)]});
    let it = json!({"kind": "tool", "name": "Write", "status": "done", "result": "Created",
        "input": {"file_path": "n.txt", "content": "x"}, "diff": diff});
    let l = chat(&draw(vec![it.clone()], 80, false, 0));
    assert_eq!(l[0], "● Create(n.txt)");
    assert_eq!(l[1], "  ⎿  Created n.txt (12 lines)");
    assert_eq!(
        l.iter().filter(|x| x.contains("+ row")).count(),
        6,
        "{l:#?}"
    );
    assert!(
        l.iter()
            .any(|x| x.ends_with("… +6 more lines (ctrl+o to expand)"))
    );
    let l = chat(&draw(vec![it], 80, true, 0));
    assert_eq!(l.iter().filter(|x| x.contains("+ row")).count(), 12);
    assert!(l.iter().any(|x| x.contains("12 + row 12")), "{l:#?}");
    assert!(l.iter().any(|x| x.contains("(ctrl+o to collapse)")));
}

#[test]
fn multiple_hunks_get_separators_and_overwrite_is_a_real_diff() {
    let diff = json!({"path": "a.rs", "kind": "overwrite", "added": 2, "removed": 2, "truncated": false,
        "hunks": [hunk(3, 3, vec![del(3, "x"), add(3, "X")]), hunk(40, 40, vec![del(40, "y"), add(40, "Y")])]});
    let it = json!({"kind": "tool", "name": "Write", "status": "done", "result": "Updated",
        "input": {"file_path": "a.rs", "content": "..."}, "diff": diff});
    let l = chat(&draw(vec![it], 80, true, 0));
    assert_eq!(l[0], "● Write(a.rs)");
    assert!(l[1].contains("Updated a.rs — +2 −2 (lines 3–40)"), "{l:#?}");
    assert!(l.iter().any(|x| x.trim() == "@@ -3,1 +3,1 @@"));
    assert!(l.iter().any(|x| x.trim() == "@@ -40,1 +40,1 @@"));
}

#[test]
fn long_lines_wrap_keeping_the_gutter_tabs_expand_and_wide_chars_fit() {
    let long = format!("\tlet s = \"{}\";", "日本語".repeat(40));
    let diff = json!({"path": "a.rs", "kind": "update", "added": 1, "removed": 0, "truncated": false,
        "hunks": [hunk(7, 7, vec![add(7, &long)])]});
    for w in [40, 80] {
        let l = chat(&draw(vec![edit_row(diff.clone())], w, true, 0));
        let at = l.iter().position(|x| x.starts_with("        7 +")).unwrap();
        let body = &l[at..];
        assert!(body.len() > 1, "wrapped: {body:#?}");
        assert!(
            body[0].starts_with("        7 +     let s"),
            "tab -> 4 columns: {:?}",
            body[0]
        );
        for b in body[1..].iter().filter(|b| !b.contains("ctrl+o")) {
            assert!(
                b.starts_with("            ") && !b.starts_with("             "),
                "continuation keeps the gutter blank: {b:?}"
            );
        }
        assert!(
            l.iter()
                .all(|x| x.chars().map(char_width).sum::<usize>() <= w),
            "w={w}: {l:#?}"
        );
        assert_eq!(
            body.iter().map(|x| x.matches('日').count()).sum::<usize>(),
            40
        );
    }
}

#[test]
fn secret_file_shows_counts_only() {
    let diff = json!({"path": ".env", "kind": "overwrite", "added": 2, "removed": 1, "truncated": false,
        "hidden": true, "hunks": []});
    let it = json!({"kind": "tool", "name": "Write", "status": "done", "result": "Updated",
        "input": {"file_path": ".env", "content": "TOKEN=abc"}, "diff": diff});
    let l = chat(&draw(vec![it], 80, true, 0)).join("\n");
    assert!(
        l.contains("Updated .env — +2 −1 (content hidden: secret file)"),
        "{l}"
    );
    assert!(!l.contains("TOKEN"), "{l}");
}

#[test]
fn truncated_diff_says_so() {
    let diff = json!({"path": "a", "kind": "create", "added": 5000, "removed": 0, "truncated": true,
        "hunks": [hunk(0, 1, vec![add(1, "x")])]});
    let it = json!({"kind": "tool", "name": "Write", "status": "done", "result": "ok",
        "input": {"file_path": "a", "content": "x"}, "diff": diff});
    let l = chat(&draw(vec![it], 80, true, 0)).join("\n");
    assert!(l.contains("diff truncated"), "{l}");
}

#[test]
fn without_a_stored_diff_the_input_is_diffed_and_numbered_from_one() {
    let it = json!({"kind": "tool", "name": "Edit", "status": "done", "result": "Edited",
        "input": {"file_path": "src/a.rs", "old_string": "a\nb", "new_string": "c"}});
    let l = chat(&draw(vec![it], 80, false, 0));
    assert_eq!(l[0], "● Update(src/a.rs)");
    assert_eq!(
        l[1],
        "  ⎿  Updated src/a.rs — +1 −2 (line numbers unavailable)"
    );
    assert_eq!(l[2], "      1   − a");
    assert_eq!(l[3], "      2   − b");
    assert_eq!(l[4], "        1 + c");
}

#[test]
fn read_row_reports_the_line_range() {
    let result: String = (10..=12).map(|i| format!("{i:>6}\tline\n")).collect();
    let it = json!({"kind": "tool", "name": "Read", "status": "done",
        "input": {"file_path": "src/a.rs", "offset": 10}, "result": result});
    let l = chat(&draw(vec![it], 80, false, 0));
    assert_eq!(l[1], "  ⎿  lines 10–12 of 12");
}
