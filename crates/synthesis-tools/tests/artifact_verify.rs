//! artifact_read containment and caps, and verify_run on a tiny fixture repo. Unix only (POSIX sh).
#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::symlink;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::{Content, Registry, ToolOutput};

struct Fx {
    _d: tempfile::TempDir,
    base: std::path::PathBuf,
    reg: Registry,
    ctx: ToolContext,
}

fn fx() -> Fx {
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let (root, art) = (base.join("w"), base.join("art"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(art.join("terminal")).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], art, "pg").unwrap();
    let ctx = ToolContext {
        session_id: "s".into(),
        task_id: "t".into(),
        turn_id: "u".into(),
        tool_call_id: "c".into(),
        workspace: Arc::new(ws),
        env: Arc::new(BTreeMap::from([(
            "PATH".to_string(),
            "/usr/bin:/bin".to_string(),
        )])),
        read_ledger: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: PermissionMode::Default,
        skip_secrets: false,
        sub_agent: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    Fx {
        _d: d,
        base,
        reg: Registry::unsandboxed_for_tests().with_new_tools(),
        ctx,
    }
}

impl Fx {
    async fn run(&self, t: &str, i: Value) -> ToolOutput {
        self.reg.execute(t, i, &self.ctx).await
    }
}

fn text(o: &ToolOutput) -> &str {
    match &o.content {
        Content::Text(t) => t,
        Content::Blocks(_) => "<blocks>",
    }
}

#[tokio::test]
async fn artifact_read_containment_caps_and_binary() {
    let f = fx();
    let art = f.base.join("art");
    std::fs::write(art.join("terminal/p1-00ab.log"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(art.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    std::fs::write(art.join("big.txt"), "x".repeat(5000)).unwrap();
    std::fs::write(f.base.join("secret.txt"), "TOP-SECRET").unwrap();
    std::fs::create_dir(f.base.join("outdir")).unwrap();
    std::fs::write(f.base.join("outdir/s.txt"), "TOP-SECRET").unwrap();
    symlink(f.base.join("secret.txt"), art.join("link.txt")).unwrap();
    symlink(f.base.join("outdir"), art.join("linkdir")).unwrap();

    // by id (stem) and by relative path
    for id in ["p1-00ab", "terminal/p1-00ab.log"] {
        let o = f.run("artifact_read", json!({"artifact_id": id})).await;
        assert_eq!(text(&o), "alpha\nbeta\ngamma\n", "{id}: {o:?}");
    }
    // absolute path inside the root is fine
    let abs = art.join("terminal/p1-00ab.log");
    let o = f
        .run(
            "artifact_read",
            json!({"artifact_id": abs.to_str().unwrap()}),
        )
        .await;
    assert!(!o.is_error, "{o:?}");
    // filter
    let o = f
        .run(
            "artifact_read",
            json!({"artifact_id": "p1-00ab", "filter": "a$"}),
        )
        .await;
    assert_eq!(text(&o), "alpha\nbeta\ngamma");
    // size cap + paging
    let o = f
        .run(
            "artifact_read",
            json!({"artifact_id": "big.txt", "limit": 100}),
        )
        .await;
    assert!(
        text(&o).starts_with(&"x".repeat(100)) && text(&o).contains("continue with offset 100")
    );
    assert_eq!(o.meta["next_offset"], 100);
    let o = f
        .run(
            "artifact_read",
            json!({"artifact_id": "big.txt", "offset": 4990}),
        )
        .await;
    assert_eq!(text(&o), "x".repeat(10));
    // binary is described, not dumped
    let o = f.run("artifact_read", json!({"artifact_id": "blob"})).await;
    assert!(
        text(&o).contains("binary") && o.meta["binary"] == true,
        "{o:?}"
    );

    // containment
    let outside = f.base.join("secret.txt");
    for id in [
        "../secret.txt",
        "terminal/../../secret.txt",
        outside.to_str().unwrap(),
        "link.txt",
        "linkdir/s.txt",
        "/etc/hosts",
    ] {
        let o = f.run("artifact_read", json!({"artifact_id": id})).await;
        assert!(
            o.is_error && !text(&o).contains("TOP-SECRET"),
            "{id}: {o:?}"
        );
    }
    let o = f.run("artifact_read", json!({"artifact_id": "nope"})).await;
    assert!(o.is_error);
    let o = f
        .run("artifact_read", json!({"artifact_id": "x", "extra": 1}))
        .await;
    assert_eq!(o.meta["error"], "invalid_input");
}

#[tokio::test]
async fn verify_run_runs_planned_and_model_checks() {
    let f = fx();
    let root = f.base.join("w");
    std::fs::write(root.join("Makefile"), "test:\n\t@echo all-good\n").unwrap();

    // whole plan: the Makefile `test` check
    let o = f.run("verify_run", json!({})).await;
    assert!(!o.is_error, "{o:?}");
    assert!(
        text(&o).contains("passed")
            && text(&o).contains("make test")
            && text(&o).contains("Advisory"),
        "{}",
        text(&o)
    );
    let log_id = text(&o)
        .split("artifact_read \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let log = f.run("artifact_read", json!({"artifact_id": log_id})).await;
    assert!(text(&log).contains("all-good"), "{log:?}");

    // a failing model command
    let o = f
        .run(
            "verify_run",
            json!({"command": "echo boom; exit 3", "criteria_ids": ["c1"]}),
        )
        .await;
    assert_eq!(o.meta["status"], "failed", "{o:?}");
    assert_eq!(o.meta["checks"].as_array().unwrap().len(), 1);
    assert_eq!(o.meta["checks"][0]["exit_code"], 3);

    // unknown check id lists the plan
    let o = f.run("verify_run", json!({"check_ids": ["zzz"]})).await;
    assert!(o.is_error && text(&o).contains("planned:"), "{o:?}");
}
