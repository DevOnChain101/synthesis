use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::{Content, Registry};

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
        sub_agent: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

fn text(c: &Content) -> &str {
    match c {
        Content::Text(t) => t,
        Content::Blocks(_) => "<blocks>",
    }
}

#[tokio::test]
async fn invalid_input_is_structured_and_does_not_run() {
    let d = tempfile::tempdir().unwrap();
    let out = Registry::unsandboxed_for_tests()
        .execute("Write", json!({"file_path": 5}), &ctx(d.path()))
        .await;
    assert!(out.is_error && !out.soft);
    assert_eq!(out.meta["error"], "invalid_input");
    let v = out.meta["violations"].as_array().unwrap();
    assert!((1..=5).contains(&v.len()), "{v:?}");
    assert!(!d.path().join("5").exists());
}

#[tokio::test]
async fn unknown_and_schema_only_tools_error() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let out = reg.execute("Nope", json!({}), &c).await;
    assert_eq!(text(&out.content), "Unknown tool \"Nope\"");
    let out = reg
        .execute(
            "Task",
            json!({"description": "d", "prompt": "p", "subagent_type": "x"}),
            &c,
        )
        .await;
    assert!(out.is_error);
    assert_eq!(out.meta["error"], "not_implemented");
}

#[tokio::test]
async fn paths_resolve_through_workspace_and_reject_escapes() {
    let d = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("x.txt"), "x").unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let p = other.path().join("x.txt");
    let out = reg.execute("Read", json!({"file_path": p}), &c).await;
    assert!(out.is_error, "{:?}", out.content);
    assert!(text(&out.content).contains("outside workspace"));
    let out = reg
        .execute("Read", json!({"file_path": "../x.txt"}), &c)
        .await;
    assert!(out.is_error);
}

#[tokio::test]
async fn edit_refuses_non_utf8_instead_of_corrupting() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let f = d.path().join("l.txt");
    std::fs::write(&f, b"caf\xe9 x\n").unwrap();
    // Read records the mtime (lossy text is fine); Edit then refuses.
    reg.execute("Read", json!({"file_path": "l.txt"}), &c).await;
    let out = reg
        .execute(
            "Edit",
            json!({"file_path": "l.txt", "old_string": "x", "new_string": "y"}),
            &c,
        )
        .await;
    assert!(out.is_error && text(&out.content).contains("not valid UTF-8"));
    assert_eq!(std::fs::read(&f).unwrap(), b"caf\xe9 x\n");
}

#[tokio::test]
async fn grep_context_with_numbers_off_uses_dash() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "a\nb\nc\n").unwrap();
    let out = Registry::unsandboxed_for_tests()
        .execute("Grep", json!({"pattern": "b", "output_mode": "content", "-n": false, "-A": 1, "path": "a.txt"}), &ctx(d.path()))
        .await;
    let f = synthesis_core::canonicalize(d.path())
        .unwrap()
        .join("a.txt");
    let want = format!("{0}:b\n{0}-c\n--", f.display());
    assert_eq!(text(&out.content), want, "{:?}", Value::Null);
}

/// Documented divergence (golden-divergences.json `validation_stricter`): the fs tools reject
/// unknown properties although their published schemas do not say `additionalProperties:false`.
#[tokio::test]
async fn fs_tools_reject_unknown_properties() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("ok.txt"), "x").unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let cases = [
        (
            "Glob",
            json!({"pattern": "x", "path": ".env", "file_path": "ok.txt"}),
        ),
        ("Grep", json!({"pattern": "x", "bogus": 1})),
        ("Read", json!({"file_path": "ok.txt", "path": "other"})),
        (
            "Write",
            json!({"file_path": "n.txt", "content": "", "mode": "0777"}),
        ),
        (
            "Edit",
            json!({"file_path": "ok.txt", "old_string": "a", "new_string": "b", "x": 1}),
        ),
    ];
    for (tool, input) in cases {
        let out = reg.execute(tool, input, &c).await;
        assert_eq!(out.meta["error"], "invalid_input", "{tool}: {out:?}");
    }
    assert!(!d.path().join("n.txt").exists());
    // The published schema text is untouched.
    assert!(
        reg.specs()
            .all(|s| s.input_schema.get("additionalProperties").is_none())
    );
    // Known properties still pass.
    let out = reg
        .execute("Read", json!({"file_path": "ok.txt"}), &c)
        .await;
    assert!(!out.is_error, "{out:?}");
}

#[tokio::test]
async fn grep_clips_long_lines_and_caps_output() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("long.txt"),
        format!("hit {}\n", "x".repeat(50_000)),
    )
    .unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    let out = reg
        .execute(
            "Grep",
            json!({"pattern": "hit", "output_mode": "content"}),
            &c,
        )
        .await;
    let t = text(&out.content);
    assert!(
        t.contains("[line truncated]") && t.len() < 3000,
        "{}",
        t.len()
    );

    // ~30k matching lines of ~100 bytes: well over the 1 MiB output cap.
    let many = ("needle ".repeat(15) + "\n").repeat(30_000);
    std::fs::write(d.path().join("many.txt"), many).unwrap();
    let out = reg
        .execute(
            "Grep",
            json!({"pattern": "needle", "output_mode": "content", "path": "many.txt"}),
            &c,
        )
        .await;
    assert!(
        text(&out.content).len() < 1024 * 1024 + 200,
        "{}",
        text(&out.content).len()
    );
}

#[tokio::test]
async fn read_refuses_oversized_file() {
    let d = tempfile::tempdir().unwrap();
    let f = std::fs::File::create(d.path().join("big.txt")).unwrap();
    f.set_len(10 * 1024 * 1024 + 1).unwrap();
    let out = Registry::unsandboxed_for_tests()
        .execute("Read", json!({"file_path": "big.txt"}), &ctx(d.path()))
        .await;
    assert!(
        out.is_error && text(&out.content).contains("larger than 10 MiB"),
        "{out:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_refuses_fifo_without_blocking() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&p)
            .status()
            .unwrap()
            .success()
    );
    let out = Registry::unsandboxed_for_tests()
        .execute("Read", json!({"file_path": "pipe"}), &ctx(d.path()))
        .await;
    assert!(
        out.is_error && text(&out.content).contains("not a regular file"),
        "{out:?}"
    );
}

#[tokio::test]
async fn cancelled_walks_stop() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "needle").unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    c.cancel.cancel();
    let g = reg.execute("Glob", json!({"pattern": "*.txt"}), &c).await;
    assert!(
        g.is_error && text(&g.content).contains("cancelled"),
        "{g:?}"
    );
    let g = reg.execute("Grep", json!({"pattern": "needle"}), &c).await;
    assert!(
        g.is_error && text(&g.content).contains("cancelled"),
        "{g:?}"
    );
}

#[tokio::test]
async fn read_refuses_huge_whole_files_and_huge_windows_with_guidance() {
    let d = tempfile::tempdir().unwrap();
    let (reg, c) = (Registry::unsandboxed_for_tests(), ctx(d.path()));
    // 300 KB over 6000 lines: a whole-file read is refused, a window is not
    let big: String = (1..=6000).map(|i| format!("line {i:0>44}\n")).collect();
    assert!(big.len() > 256 * 1024);
    std::fs::write(d.path().join("big.txt"), &big).unwrap();
    let o = reg
        .execute("Read", json!({"file_path": "big.txt"}), &c)
        .await;
    assert!(o.is_error, "{o:?}");
    let msg = text(&o.content);
    assert!(
        msg.contains("offset") && msg.contains("limit") && msg.contains("256 KB"),
        "{msg}"
    );
    let o = reg
        .execute(
            "Read",
            json!({"file_path": "big.txt", "offset": 5000, "limit": 3}),
            &c,
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    // line numbers keep counting from the file start
    assert!(
        text(&o.content).starts_with("  5000\tline "),
        "{}",
        text(&o.content)
    );
    // a small file still reads whole
    std::fs::write(d.path().join("s.txt"), "a\nb\n").unwrap();
    let o = reg.execute("Read", json!({"file_path": "s.txt"}), &c).await;
    assert_eq!(text(&o.content), "     1\ta\n     2\tb");
    // a window of long lines can exceed 25k tokens even under the whole-file cap
    let wide: String = (0..200)
        .map(|_| format!("{}\n", "x".repeat(1900)))
        .collect();
    std::fs::write(d.path().join("wide.txt"), &wide).unwrap();
    let o = reg
        .execute("Read", json!({"file_path": "wide.txt", "limit": 100}), &c)
        .await;
    assert!(o.is_error && text(&o.content).contains("token"), "{o:?}");
    let o = reg
        .execute("Read", json!({"file_path": "wide.txt", "limit": 20}), &c)
        .await;
    assert!(!o.is_error, "{o:?}");
}
