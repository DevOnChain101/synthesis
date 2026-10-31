use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::Registry;

fn ctx(dir: &std::path::Path) -> ToolContext {
    let root = synthesis_core::canonicalize(dir).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], root.join("art"), "pg").unwrap();
    ToolContext {
        session_id: "s".into(),
        task_id: "t".into(),
        turn_id: "u".into(),
        tool_call_id: "c".into(),
        workspace: Arc::new(ws),
        env: Arc::new(BTreeMap::new()),
        read_ledger: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: PermissionMode::Default,
        skip_secrets: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

async fn run(reg: &Registry, c: &ToolContext, tool: &str, input: Value) -> Value {
    let out = reg.execute(tool, input, c).await;
    assert!(!out.is_error, "{tool}: {:?}", out.content);
    out.meta["diff"].clone()
}

/// Every `-` line's old number holds that text in the old file, every `+` line's new number in
/// the new file, every context line in both.
fn check(diff: &Value, old: &str, new: &str) {
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    for h in diff["hunks"].as_array().unwrap() {
        for l in h["lines"].as_array().unwrap() {
            let t = l["text"].as_str().unwrap();
            let at = |k: &str, f: &[&str]| f[l[k].as_u64().unwrap() as usize - 1].to_string();
            match l["tag"].as_str().unwrap() {
                "-" => assert_eq!(at("old_no", &o), t),
                "+" => assert_eq!(at("new_no", &n), t),
                _ => assert_eq!((at("old_no", &o), at("new_no", &n)), (t.into(), t.into())),
            }
        }
    }
}

#[tokio::test]
async fn edit_diff_numbers_match_the_files() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let f = d.path().join("f.txt");
    let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&f, &old).unwrap();
    reg.execute("Read", json!({"file_path": "f.txt"}), &c).await;
    let diff = run(
        &reg,
        &c,
        "Edit",
        json!({"file_path": "f.txt", "old_string": "line 20\n", "new_string": "twenty\ntwenty b\n"}),
    )
    .await;
    let new = std::fs::read_to_string(&f).unwrap();
    check(&diff, &old, &new);
    assert_eq!(
        (
            diff["kind"].clone(),
            diff["added"].clone(),
            diff["removed"].clone()
        ),
        (json!("update"), json!(2), json!(1))
    );
    assert_eq!(diff["hunks"][0]["old_start"], 17);
    assert_eq!(new.lines().nth(19), Some("twenty"));
}

#[tokio::test]
async fn write_create_overwrite_and_secret() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let diff = run(
        &reg,
        &c,
        "Write",
        json!({"file_path": "n.txt", "content": "a\nb\n"}),
    )
    .await;
    assert_eq!(
        (
            diff["kind"].clone(),
            diff["added"].clone(),
            diff["removed"].clone()
        ),
        (json!("create"), json!(2), json!(0))
    );
    reg.execute("Read", json!({"file_path": "n.txt"}), &c).await;
    let diff = run(
        &reg,
        &c,
        "Write",
        json!({"file_path": "n.txt", "content": "a\nc\nd\n"}),
    )
    .await;
    assert_eq!(diff["kind"], "overwrite");
    check(&diff, "a\nb\n", "a\nc\nd\n");
    let diff = run(
        &reg,
        &c,
        "Write",
        json!({"file_path": ".env", "content": "TOKEN=abc\n"}),
    )
    .await;
    assert_eq!(
        (diff["hidden"].clone(), diff["added"].clone()),
        (json!(true), json!(1))
    );
    assert!(!diff.to_string().contains("abc"));
}

#[tokio::test]
async fn notebook_diff_is_relative_to_the_cell() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let nb = json!({"nbformat": 4, "nbformat_minor": 5, "metadata": {}, "cells": [
        {"id": "a1", "cell_type": "code", "metadata": {}, "source": ["x = 1\n", "y = 2"], "outputs": [], "execution_count": null}]});
    std::fs::write(d.path().join("n.ipynb"), nb.to_string()).unwrap();
    reg.execute("Read", json!({"file_path": "n.ipynb"}), &c)
        .await;
    let diff = run(
        &reg,
        &c,
        "NotebookEdit",
        json!({"notebook_path": "n.ipynb", "cell_id": "a1", "new_source": "x = 1\ny = 3"}),
    )
    .await;
    check(&diff, "x = 1\ny = 2", "x = 1\ny = 3");
    assert_eq!(
        (diff["added"].clone(), diff["removed"].clone()),
        (json!(1), json!(1))
    );
}
