//! Golden transcripts (tests/golden/transcripts, captured from the Node reference).
//!
//! The Node fixtures also pin parser.js's dashboard model (`session`, `phase`, `total_tokens`,
//! classifyTool/summarizeTool, activity). That is dashboard logic, not session storage,
//! and is not ported here: see `DIVERGENCES`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_core::WorkspaceContext;
use synthesis_events::Budget;
use synthesis_session::node::{
    items_from_entries, load_all_items, parse_lines, prompts_of_items, session_meta,
};
use synthesis_session::{NewSession, Store, StoreOptions};

/// (case id or `*:<fn>`, reason). A case listed here is not compared at all.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "classify-tool-table",
        "parser.js classifyTool: dashboard categories, not storage",
    ),
    (
        "summarize-tool-table",
        "parser.js summarizeTool: feed text for the dashboard, not storage",
    ),
    (
        "file-load-activity",
        "loadTranscriptActivity: dashboard Activity panel (parser.js applyEntry)",
    ),
    (
        "file-load-session-settles-open-tools",
        "loadTranscriptSession: dashboard session model (parser.js applyEntry/settleSession)",
    ),
];
/// Parts of a `replay` expectation that are not compared (same reason: parser.js dashboard model).
const REPLAY_NOT_COMPARED: &[&str] = &["session", "phase", "total_tokens"];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/transcripts")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn cases() -> Vec<(String, Value, Value)> {
    let mut v: Vec<_> = fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                read(&p.join("input.json")),
                read(&p.join("expected.json")),
            )
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn run_case(id: &str, input: &Value, expected: &Value, tmp: &Path) -> Value {
    match input["op"].as_str().unwrap() {
        "replay" => {
            let entries = input["entries"].as_array().unwrap();
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| l as usize);
            json!({"items": items_from_entries(entries, limit)})
        }
        "parseLines" => Value::Array(parse_lines(input["text"].as_str().unwrap())),
        "promptsOfItems" => {
            let items = input["items"].as_array().unwrap();
            json!(prompts_of_items(
                items,
                input["max"].as_u64().unwrap() as usize
            ))
        }
        "loadFile" => {
            let file = tmp.join("s1.jsonl");
            if let Some(c) = input["content"].as_str() {
                fs::write(&file, c).unwrap();
            }
            match input["fn"].as_str().unwrap() {
                "loadAllTranscriptItems" => Value::Array(load_all_items(&file)),
                "sessionMeta" => {
                    let m = session_meta(&file);
                    json!({"cwd": m.cwd, "firstPrompt": m.first_prompt})
                }
                f => panic!("{id}: unlisted fn {f}, expected {expected}"),
            }
        }
        op => panic!("{id}: unlisted op {op}"),
    }
}

#[test]
fn golden_transcripts_reader() {
    let (mut passed, mut skipped) = (0, 0);
    let all = cases();
    for (name, _) in DIVERGENCES {
        assert!(
            all.iter().any(|c| c.0 == *name),
            "stale allow-list entry {name}"
        );
    }
    for (id, input, expected) in &all {
        if DIVERGENCES.iter().any(|(n, _)| n == id) {
            skipped += 1;
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let got = run_case(id, input, expected, tmp.path());
        let want = if input["op"] == "replay" {
            let mut e = expected.clone();
            REPLAY_NOT_COMPARED.iter().for_each(|k| {
                e.as_object_mut().unwrap().remove(*k);
            });
            e
        } else {
            expected.clone()
        };
        assert_eq!(got, want, "golden case {id}");
        passed += 1;
    }
    eprintln!(
        "golden reader: {passed} compared and passed, {skipped} allow-listed of {}",
        all.len()
    );
    assert_eq!(passed + skipped, all.len());
}

/// C.4: Node JSONL -> import -> Rust export -> Node reader == Node reader on the original.
#[test]
fn golden_transcripts_round_trip_through_store() {
    let mut n = 0;
    for (id, input, _) in cases() {
        if input["op"] != "replay" {
            continue;
        }
        let entries = input["entries"].as_array().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (home, ws_dir) = (dir.path().join("home"), dir.path().join("ws"));
        fs::create_dir_all(&ws_dir).unwrap();
        let src = dir.path().join("src.jsonl");
        let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
        fs::write(&src, &text).unwrap();

        let ws = WorkspaceContext::open("w", &ws_dir, &ws_dir, &[], home.join("a"), "pg").unwrap();
        let store = Store::open(&home, StoreOptions::default()).unwrap();
        let (w, _) = store
            .import_node_transcript(
                &src,
                NewSession {
                    id: Some("imported".into()),
                    workspace: &ws,
                    model_config_id: "p/m".into(),
                    permission_mode: synthesis_core::PermissionMode::Default,
                    permission_policy_id: "pol".into(),
                    budget: Budget::default(),
                    parent: None,
                    title: None,
                },
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(&src).unwrap(),
            text,
            "{id}: import never modifies the source"
        );
        let out = dir.path().join("out.jsonl");
        w.export_node_transcript(&out).unwrap();
        let back = synthesis_session::node::read_entries(&out, None).unwrap();
        assert_eq!(
            items_from_entries(&back, None),
            items_from_entries(entries, None),
            "round trip {id}"
        );
        n += 1;
    }
    eprintln!("golden round trip: {n} replay cases");
    assert!(n > 40);
}
