//! Lost-update scenario of Confluence/Jira style MCP servers, end to end through the real binary
//! with a scripted model and `synthesis-mcp/examples/atlassian_like_server` (feature `test-provider`).
//!
//! The server mimics the Atlassian semantics: `updatePage` REPLACES the whole body and only checks
//! that `version.number` is current+1. A model that never fetches first therefore destroys the
//! rest of the page, either by guessing the version or by learning it from the 409 error.
#![cfg(all(feature = "test-provider", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

/// `target/<profile>/examples/atlassian_like_server`, built on first use.
fn server() -> PathBuf {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let exe = Path::new(env!("CARGO_BIN_EXE_synt"));
        let profile = exe.parent().unwrap();
        let p = profile.join("examples").join("atlassian_like_server");
        if !p.exists() {
            let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
            c.args([
                "build",
                "-p",
                "synthesis-mcp",
                "--example",
                "atlassian_like_server",
            ]);
            if profile.file_name().is_some_and(|n| n == "release") {
                c.arg("--release");
            }
            assert!(c.status().unwrap().success(), "cannot build the example");
        }
        p
    })
    .clone()
}

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    /// `flags` go to the server, `settings` is the user settings.json.
    fn new(flags: &[&str], settings: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(dir.path().join("synthesis")).unwrap();
        let cfg = json!({"$schema": "config.json/v1",
            "mcpServers": {"atl": {"command": server(), "args": flags}}});
        std::fs::write(dir.path().join("synthesis/synt.json"), cfg.to_string()).unwrap();
        std::fs::write(
            dir.path().join("synthesis/settings.json"),
            settings.to_string(),
        )
        .unwrap();
        Rig { dir }
    }

    fn run(&self, script: Value) -> (Output, Vec<Value>, String) {
        let s = self.dir.path().join("script.json");
        std::fs::write(&s, script.to_string()).unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(["run", "--no-cost-cap", "--cwd"])
            .arg(self.dir.path().join("ws"))
            .arg("--provider")
            .arg(format!("fake:{}", s.display()))
            .args(["--prompt", "update the page", "--sandbox", "best-effort"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("SYNT_HOME", self.dir.path().join("synthesis"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let ev = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let mut mirror = String::new();
        for d in std::fs::read_dir(self.dir.path().join("synthesis/projects"))
            .unwrap()
            .flatten()
        {
            for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                mirror += &std::fs::read_to_string(f.path()).unwrap();
            }
        }
        // JSON-escaped inside the transcript: compare on the unescaped text
        (o, ev, mirror.replace("\\\"", "\"").replace("\\n", "\n"))
    }
}

fn allow() -> Value {
    json!({"permissions": {"allow": ["mcp__atl"]}})
}

fn call(name: &str, input: Value) -> Value {
    json!({"tools": [{"name": format!("mcp__atl__{name}"), "input": input}]})
}

fn completed(ev: &[Value]) -> Vec<(&str, bool, &str)> {
    ev.iter()
        .filter(|e| e["type"] == "tool.completed")
        .map(|e| {
            let p = &e["payload"];
            (
                p["name"].as_str().unwrap(),
                p["is_error"].as_bool().unwrap(),
                p["preview"].as_str().unwrap(),
            )
        })
        .collect()
}

/// Text of the approval requests (what a person would be asked).
fn prompt(ev: &[Value]) -> String {
    ev.iter()
        .filter(|e| e["type"] == "tool.approval_required")
        .map(|e| e["payload"]["prompt"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn update(version: i64, body: &str) -> Value {
    call(
        "updatePage",
        json!({"id": "100", "title": "Runbook", "body": body, "version": {"number": version}}),
    )
}

const SNIPPET: &str = "# Runbook\nNEW INTRO ONLY";

#[test]
fn update_without_fetching_is_rejected_and_the_page_survives() {
    let rig = Rig::new(&[], allow());
    // 1) guessed version, 2) the model learns "4" from the 409 and retries, 3) check the page
    let (o, ev, mirror) = rig.run(json!([
        update(9, SNIPPET),
        update(4, SNIPPET),
        call("getPage", json!({"id": "100"})),
        {"text": "done"}
    ]));
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let c = completed(&ev);
    eprintln!("{c:#?}");
    assert!(
        c[0].1 && c[1].1,
        "both blind updates are tool errors: {c:?}"
    );
    assert!(
        !c[0].2.contains("409") && !c[1].2.contains("409"),
        "rejected by the client, never sent to the server: {c:?}"
    );
    assert!(
        mirror.contains("getPage") && mirror.contains("MERGE"),
        "the error says what to do: {mirror}"
    );
    assert!(!c[2].1, "{c:?}");
    assert!(
        mirror.contains("## Rollback") && mirror.contains("## Contacts"),
        "{mirror}"
    );
}

#[test]
fn fetch_then_merged_update_succeeds_and_a_second_blind_update_does_not() {
    // no annotations: the write is recognised by its name and needs no approval (rule allows)
    let rig = Rig::new(&["--no-annotations"], allow());
    let merged = "# Runbook\n\n## Rollback\nStep 1: stop the deploy.\nStep 2: restore the snapshot.\nStep 3: page the owner.\n\n## Contacts\nOn call: alice@example.com\nEscalation: bob@example.com\n\n## History\n2024-01-05 first version\n2024-02-11 added contacts\n";
    let (o, ev, mirror) = rig.run(json!([
        call("getPage", json!({"id": "100"})),
        update(4, merged),
        // no re-read in between: the write above made the fetched copy stale
        update(5, merged),
        call("getPage", json!({"id": "100"})),
        {"text": "done"}
    ]));
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let c = completed(&ev);
    eprintln!("{c:#?}");
    assert_eq!(
        c.iter().map(|x| x.1).collect::<Vec<_>>(),
        [false, false, true, false],
        "{c:?}"
    );
    assert!(mirror.contains("Step 3: page the owner."), "{mirror}");
    assert!(mirror.contains("## Contacts"), "{mirror}");
}

#[test]
fn destructive_shrinkage_asks_even_when_the_tool_is_allowed() {
    let rig = Rig::new(&["--no-annotations"], allow());
    let (_, ev, mirror) = rig.run(json!([
        call("getPage", json!({"id": "100"})),
        update(4, SNIPPET),
        call("getPage", json!({"id": "100"})),
        {"text": "done"}
    ]));
    let c = completed(&ev);
    eprintln!("{c:#?}");
    assert_eq!(c[1].0, "mcp__atl__updatePage");
    assert!(c[1].1, "not run: headless ask is a denial: {c:?}");
    assert!(
        prompt(&ev).contains("not present in your update"),
        "the approval prompt carries the reason and the diff: {}",
        prompt(&ev)
    );
    assert!(mirror.contains("## Rollback"), "page untouched");
}

#[test]
fn big_pages_keep_their_version_field() {
    let rig = Rig::new(&["--big-body"], allow());
    let (_, ev, mirror) = rig.run(json!([call("getPage", json!({"id": "100"})), {"text": "done"}]));
    eprintln!("{:#?}", completed(&ev));
    let compact = mirror.replace(": ", ":");
    assert!(
        compact.contains("\"version\":{\"number\":3}") && compact.contains("\"id\":\"100\""),
        "id and version survive truncation"
    );
    assert!(compact.contains("omitted"), "the cut is explicit");
}

#[test]
fn jira_edit_needs_the_issue_to_be_read_first_and_search_does_not_count_for_other_issues() {
    let rig = Rig::new(&["--no-annotations"], allow());
    let edit = |k: &str| {
        call(
            "editIssue",
            json!({"issueIdOrKey": k, "fields": {"description": "x"}}),
        )
    };
    let (_, ev, _) = rig.run(json!([
        edit("PROJ-1"),
        call("getIssue", json!({"issueIdOrKey": "PROJ-1"})),
        edit("PROJ-1"),
        edit("PROJ-2"),
        {"text": "done"}
    ]));
    let c = completed(&ev);
    eprintln!("{c:#?}");
    assert_eq!(
        c.iter().map(|x| x.1).collect::<Vec<_>>(),
        [true, false, false, true],
        "{c:?}"
    );
}

#[test]
fn a_tool_annotated_destructive_always_asks_even_after_a_fetch_and_a_rule() {
    let rig = Rig::new(&[], allow());
    let (_, ev, _) = rig.run(json!([
        call("getPage", json!({"id": "100"})),
        update(4, "# Runbook\n\n## Rollback\nStep 1: stop the deploy.\n"),
        {"text": "done"}
    ]));
    let c = completed(&ev);
    assert!(!c[0].1 && c[1].1, "{c:?}");
    assert!(prompt(&ev).contains("destructive"), "{}", prompt(&ev));
}

#[test]
fn v2_tools_and_execute_wrappers_follow_the_same_rule() {
    let rig = Rig::new(&["--no-annotations"], allow());
    let exec =
        |name: &str, tool: &str, args: Value| call(name, json!({"tool": tool, "arguments": args}));
    let edit = json!({"pageId": "100", "version": {"number": 4},
        "edits": [{"find": "stop the deploy", "replace": "stop the deploy and page the owner"}]});
    let (_, ev, mirror) = rig.run(json!([
        exec("executeWrite", "updateConfluenceContent", edit.clone()),
        exec("executeRead", "getConfluenceContent", json!({"pageId": "100"})),
        exec("executeWrite", "updateConfluenceContent", edit),
        call("getConfluenceContent", json!({"pageId": "100"})),
        {"text": "done"}
    ]));
    let c = completed(&ev);
    assert_eq!(
        c.iter().map(|x| x.1).collect::<Vec<_>>(),
        [true, false, false, false],
        "{c:?}"
    );
    assert!(
        mirror.contains("and page the owner") && mirror.contains("## Contacts"),
        "{mirror}"
    );
}
