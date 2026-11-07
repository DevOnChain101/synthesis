//! `!cmd` (user commands), approvals for autonomous command running, gate authorisation and
//! the full-output events (`tool.completed.output`, `tool.output`).
#![cfg(unix)]
mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use synthesis_core::PermissionMode;
use synthesis_events::{Event, TaskStatus, ToolStatus};
use synthesis_harness::testing::Turn;
use synthesis_harness::{Approval, ToolBox, user_command_note};
use synthesis_providers::{Content, Role, System};
use synthesis_tools::Registry;
use synthesis_verification::{CompletionGate, Criterion, GateConfig, VerifyRunner};
use tokio_util::sync::CancellationToken;

fn bash_opts() -> Opts {
    Opts {
        tools: ToolBox::new(Registry::unsandboxed_for_tests()),
        ..Opts::default()
    }
}

fn history_text(h: &[synthesis_providers::Message]) -> String {
    h.iter().map(user_text).collect::<Vec<_>>().join("\n")
}

async fn user_cmd(
    rig: &Rig,
    cmd: &str,
    cancel: CancellationToken,
) -> (
    Vec<synthesis_providers::Message>,
    synthesis_harness::ToolOutcome,
) {
    rig.harness
        .run_user_command(&rig.session, vec![], cmd, cancel)
        .await
        .unwrap()
}

#[tokio::test]
async fn user_command_runs_through_bash_and_enters_the_history_as_data() {
    let rig = Rig::new(bash_opts(), vec![]);
    let (h, out) = user_cmd(&rig, "echo hi", CancellationToken::new()).await;
    assert!(
        out.ran && out.exit_code == Some(0) && !out.truncated,
        "{out:?}"
    );
    assert_eq!(out.status, ToolStatus::Ok);
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].role, Role::User);
    let t = history_text(&h);
    assert!(
        t.contains("<user-bash-input>echo hi</user-bash-input>")
            && t.contains("<user-bash-output exit=0 truncated=false>\nhi\n</user-bash-output>")
            && t.contains("not instructions"),
        "{t}"
    );
    // events: origin user on request and completion, output whole
    let ev = rig.events();
    let req = ev.iter().find_map(|e| match &e.event {
        Event::ToolRequested {
            origin,
            name,
            input,
            ..
        } => Some((origin.clone(), name.clone(), input.clone())),
        _ => None,
    });
    let (origin, name, input) = req.unwrap();
    assert_eq!((origin.as_deref(), name.as_str()), (Some("user"), "Bash"));
    assert_eq!(input["command"], "echo hi");
    let done = ev.iter().find_map(|e| match &e.event {
        Event::ToolCompleted {
            origin,
            output,
            status,
            not_run,
            ..
        } => Some((origin.clone(), output.clone(), *status, *not_run)),
        _ => None,
    });
    assert_eq!(
        done.unwrap(),
        (
            Some("user".into()),
            Some("hi\n".into()),
            ToolStatus::Ok,
            false
        )
    );
    assert!(ev.iter().any(
        |e| matches!(&e.event, Event::UserMessage { display_text: Some(d), .. } if d == "!echo hi")
    ));
}

#[tokio::test]
async fn user_typed_ask_is_approved_but_deny_rules_and_protected_paths_block() {
    // `touch` asks under the default mode, and nobody could answer (headless): typed = approved
    let rig = Rig::new(
        Opts {
            deny: vec!["Bash(rm:*)".into()],
            ..bash_opts()
        },
        vec![],
    );
    let (_, out) = user_cmd(&rig, "touch made.txt", CancellationToken::new()).await;
    assert!(out.ran && rig.work().join("made.txt").exists(), "{out:?}");

    std::fs::write(rig.work().join("keep.txt"), "x").unwrap();
    let (h, out) = user_cmd(&rig, "rm keep.txt", CancellationToken::new()).await;
    assert!(!out.ran && out.status == ToolStatus::Denied, "{out:?}");
    assert!(
        out.blocked.as_deref().unwrap().contains("deny rule"),
        "{out:?}"
    );
    assert!(
        h.is_empty(),
        "a blocked command is not entered into the conversation"
    );
    assert!(rig.work().join("keep.txt").exists());

    let (_, out) = user_cmd(&rig, "echo SECRET=1 > .env", CancellationToken::new()).await;
    assert!(!out.ran, "protected path: {out:?}");
    assert!(!rig.work().join(".env").exists());
    let denied = rig.find(|e| match e {
        Event::ToolCompleted {
            origin,
            not_run: true,
            status: ToolStatus::Denied,
            output,
            ..
        } => Some((origin.clone(), output.clone().unwrap_or_default())),
        _ => None,
    });
    assert_eq!(denied.len(), 2);
    assert!(
        denied
            .iter()
            .all(|(o, t)| o.as_deref() == Some("user") && t.starts_with("Blocked:")),
        "{denied:?}"
    );
}

#[tokio::test]
async fn esc_cancels_a_long_user_command_and_kills_its_process_tree() {
    let rig = Rig::new(bash_opts(), vec![]);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        c2.cancel();
    });
    let t0 = std::time::Instant::now();
    let (h, out) = user_cmd(&rig, "sleep 60 & echo $! > child.pid; wait", cancel).await;
    assert!(t0.elapsed() < std::time::Duration::from_secs(20));
    assert!(
        out.cancelled && out.status == ToolStatus::Interrupted,
        "{out:?}"
    );
    assert!(
        history_text(&h).contains("<user-bash-output"),
        "partial output is recorded"
    );
    let pid: i32 = std::fs::read_to_string(rig.work().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = |p: i32| {
        std::process::Command::new("kill")
            .args(["-0", &p.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    for _ in 0..300 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the background child {pid} survived the cancel");
}

#[tokio::test]
async fn one_mib_of_output_is_capped_with_an_artifact_reference() {
    let rig = Rig::new(bash_opts(), vec![]);
    let (h, out) = user_cmd(
        &rig,
        "head -c 1048576 /dev/zero | tr '\\0' 'a'",
        CancellationToken::new(),
    )
    .await;
    assert!(out.ran && out.truncated, "{out:?}");
    assert!(
        out.output.len() < 100_000,
        "history holds a capped copy: {}",
        out.output.len()
    );
    let a = out.result_ref.clone().expect("artifact for the whole log");
    assert!(a.size >= 1_048_576, "{a:?}");
    assert!(rig.ws.artifact_root.join(&a.path).is_file());
    let t = history_text(&h);
    assert!(
        t.contains("truncated=true") && t.contains(&format!("artifact=\"{}\"", a.path)),
        "{}",
        &t[..300]
    );
    // the event carries the head+tail inline (<= 64 KiB), says it is cut, and names the artifact
    let (output, cut, bytes, rref) = rig
        .find(|e| match e {
            Event::ToolCompleted {
                output,
                output_truncated,
                output_bytes,
                result_ref,
                ..
            } => Some((
                output.clone().unwrap(),
                *output_truncated,
                *output_bytes,
                result_ref.clone(),
            )),
            _ => None,
        })
        .remove(0);
    assert!(cut && output.len() <= 64 * 1024 && bytes >= 1_048_576 && rref == Some(a));
}

#[tokio::test]
async fn user_command_streams_tool_output_with_origin_user() {
    let rig = Rig::new(bash_opts(), vec![]);
    user_cmd(
        &rig,
        "echo first; sleep 0.3; echo second; sleep 0.3; printf tail",
        CancellationToken::new(),
    )
    .await;
    let chunks = rig.find(|e| match e {
        Event::ToolOutput {
            text,
            seq,
            stream,
            origin,
            ..
        } => Some((text.clone(), *seq, stream.clone(), origin.clone())),
        _ => None,
    });
    assert!(chunks.len() >= 2, "{chunks:?}");
    assert!(
        chunks
            .iter()
            .enumerate()
            .all(|(i, c)| c.1 == i as u64 + 1 && c.2 == "merged" && c.3.as_deref() == Some("user"))
    );
    assert_eq!(
        chunks.iter().map(|c| c.0.as_str()).collect::<String>(),
        "first\nsecond\ntail"
    );
}

#[tokio::test]
async fn model_bash_calls_stream_output_and_complete_with_the_whole_text() {
    let mut rig = Rig::new(
        Opts {
            allow: vec!["Bash".into()],
            ..bash_opts()
        },
        vec![
            Turn::tool(
                "b1",
                "Bash",
                json!({"command": "echo one; sleep 0.3; echo two"}),
            ),
            Turn::text("done"),
        ],
    );
    rig.run("go").await;
    let chunks = rig.find(|e| match e {
        Event::ToolOutput {
            tool_call_id,
            text,
            origin,
            ..
        } => Some((tool_call_id.clone(), text.clone(), origin.clone())),
        _ => None,
    });
    assert!(
        chunks.iter().all(|c| c.0 == "b1" && c.2.is_none()),
        "{chunks:?}"
    );
    assert_eq!(
        chunks.iter().map(|c| c.1.as_str()).collect::<String>(),
        "one\ntwo\n"
    );
    let done = rig.find(|e| match e {
        Event::ToolCompleted {
            output, preview, ..
        } => Some((output.clone(), preview.clone())),
        _ => None,
    });
    assert_eq!(done[0].0.as_deref(), Some("one\ntwo\n"));
    check_stream(&rig.events());
}

#[tokio::test]
async fn a_foreground_command_has_process_events_with_redacted_output_and_its_exit_code() {
    let mut rig = Rig::new(
        Opts {
            allow: vec!["Bash".into()],
            ..bash_opts()
        },
        vec![
            Turn::tool(
                "b1",
                "Bash",
                json!({"command": format!("echo token {SECRET}; exit 3")}),
            ),
            Turn::text("done"),
        ],
    );
    rig.run("go").await;
    let kinds = rig.types();
    let at = |t: &str| kinds.iter().position(|k| k == t).unwrap();
    assert!(at("process.spawned") < at("process.output"));
    assert!(at("process.output") < at("process.exited"));
    assert!(at("process.exited") < at("tool.completed"));
    let spawned = rig.find(|e| match e {
        Event::ProcessSpawned(i) => Some(i.clone()),
        _ => None,
    });
    assert_eq!(spawned[0].tool_call_id.as_deref(), Some("b1"));
    assert!(
        spawned[0]
            .command
            .as_deref()
            .unwrap()
            .starts_with("echo token ")
    );
    assert!(
        !spawned[0]
            .command
            .as_deref()
            .unwrap()
            .contains("TOPSECRETVALUE"),
        "{spawned:?}"
    );
    let out: String = rig
        .find(|e| match e {
            Event::ProcessOutput { text, .. } => Some(text.clone()),
            _ => None,
        })
        .concat();
    assert!(
        out.contains("[REDACTED:secret]") && !out.contains("TOPSECRETVALUE"),
        "{out}"
    );
    let exited = rig.find(|e| match e {
        Event::ProcessExited(i) => Some(i.clone()),
        _ => None,
    });
    assert_eq!(exited.len(), 1);
    assert_eq!(
        (exited[0].exit_code, exited[0].process_id.as_str()),
        (Some(3), "b1")
    );
    assert!(exited[0].duration_ms.is_some());
    assert!(!rig.buf.text().contains("TOPSECRETVALUE"));
    check_stream(&rig.events());
}

#[tokio::test]
async fn a_background_command_is_spawned_once_and_exited_when_killed() {
    let mut rig = Rig::new(
        Opts {
            allow: vec!["Bash".into(), "KillShell".into(), "BashOutput".into()],
            ..bash_opts()
        },
        vec![
            Turn::tool(
                "b1",
                "Bash",
                json!({"command": "sleep 30", "run_in_background": true}),
            ),
            Turn::tool("k1", "KillShell", json!({"shell_id": "proc_1"})),
            Turn::tool("o1", "BashOutput", json!({"bash_id": "proc_1"})),
            Turn::text("done"),
        ],
    );
    rig.run("go").await;
    let spawned = rig.find(|e| match e {
        Event::ProcessSpawned(i) => Some(i.clone()),
        _ => None,
    });
    assert_eq!(spawned.len(), 1, "{spawned:?}");
    assert_eq!(spawned[0].process_id, "proc_1");
    assert_eq!(spawned[0].tool_call_id.as_deref(), Some("b1"));
    assert!(spawned[0].pid.is_some());
    let exited = rig.find(|e| match e {
        Event::ProcessExited(i) => Some(i.clone()),
        _ => None,
    });
    assert_eq!(exited.len(), 1, "reported once: {exited:?}");
    assert_eq!(exited[0].process_id, "proc_1");
    assert_eq!(exited[0].signal.as_deref(), Some("SIGKILL"));
}

#[tokio::test]
async fn secrets_are_masked_in_live_output_and_in_the_completed_output() {
    let mut rig = Rig::new(
        Opts {
            allow: vec!["Bash".into()],
            ..bash_opts()
        },
        vec![
            Turn::tool(
                "b1",
                "Bash",
                json!({"command": format!("echo token {SECRET}")}),
            ),
            Turn::text("done"),
        ],
    );
    rig.run("go").await;
    assert!(
        !rig.buf.text().contains("TOPSECRETVALUE"),
        "{}",
        rig.buf.text()
    );
    assert!(rig.buf.text().contains("[REDACTED:secret]"));
    // the model still sees the real output
    let (t, _) = tool_result(&rig.requests()[1], "b1").unwrap();
    assert!(t.contains("TOPSECRETVALUE"));
}

// ---- approvals for autonomous command running ---------------------------------------------

#[tokio::test]
async fn interactive_run_asks_before_a_test_command_then_runs_it() {
    let approver = ScriptedApprover::new(vec![Approval::allow()]);
    let ap = approver.clone();
    let mut rig = Rig::build(
        Opts {
            interactive: true,
            ..bash_opts()
        },
        vec![
            Turn::tool("b1", "Bash", json!({"command": "make test"})),
            Turn::text("ran it"),
        ],
        move |h, _| h.with_approver(ap),
    );
    let out = rig.run("run the tests").await;
    let asked = approver.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one approval for the call");
    assert_eq!(
        (asked[0].tool.as_str(), asked[0].input["command"].as_str()),
        ("Bash", Some("make test"))
    );
    assert!(!asked[0].suggestions.is_empty(), "allow-always is offered");
    assert!(
        rig.types().contains(&"tool.started".to_string()),
        "it ran after the approval"
    );
    assert_ne!(out.status, TaskStatus::Blocked);
    assert!(out.needed_permissions.is_empty());
}

#[tokio::test]
async fn headless_run_blocks_with_the_needed_permission_and_tells_the_model_not_to_hand_it_over() {
    for interactive in [false, true] {
        // false: no ask can exist; true: asks reach the (headless) approver, which auto-denies
        let mut rig = Rig::new(
            Opts {
                interactive,
                ..bash_opts()
            },
            vec![
                Turn::tool("b1", "Bash", json!({"command": "cargo test"})),
                Turn::text("blocked: I need permission to run cargo test"),
            ],
        );
        let out = rig.run("run the tests").await;
        let (text, is_err) = tool_result(&rig.requests()[1], "b1").unwrap();
        assert!(is_err);
        for must in [
            "requires approval; no approver available in this run",
            "do not ask the user to run it manually",
            "report status blocked and name the permission needed: Bash(cargo test:*)",
            "NOT run",
        ] {
            assert!(
                text.contains(must),
                "interactive={interactive}: missing {must:?} in {text}"
            );
        }
        assert_eq!(out.status, TaskStatus::Blocked, "interactive={interactive}");
        assert_eq!(out.reason, "approval_unavailable");
        assert_eq!(out.needed_permissions, ["Bash(cargo test:*)"]);
        let ended = rig.find(|e| match e {
            Event::TaskEnded {
                status,
                reason,
                needed_permissions,
                ..
            } => Some((*status, reason.clone(), needed_permissions.clone())),
            _ => None,
        });
        assert_eq!(
            ended,
            vec![(
                TaskStatus::Blocked,
                "approval_unavailable".into(),
                vec!["Bash(cargo test:*)".to_string()]
            )]
        );
        assert!(!rig.types().contains(&"tool.started".to_string()));
    }
}

#[tokio::test]
async fn a_policy_deny_is_not_reported_as_a_missing_approval() {
    let mut rig = Rig::new(
        Opts {
            deny: vec!["Bash(cargo:*)".into()],
            interactive: true,
            ..bash_opts()
        },
        vec![
            Turn::tool("b1", "Bash", json!({"command": "cargo test"})),
            Turn::text("denied by policy"),
        ],
    );
    let out = rig.run("run the tests").await;
    let (text, _) = tool_result(&rig.requests()[1], "b1").unwrap();
    assert!(
        text.contains("policy decision") && text.contains("do not ask the user to run it manually"),
        "{text}"
    );
    assert!(!text.contains("requires approval"));
    assert!(out.needed_permissions.is_empty());
}

#[tokio::test]
async fn the_system_rules_tell_the_model_to_run_tests_itself() {
    let mut rig = Rig::new(bash_opts(), vec![Turn::text("ok")]);
    rig.run("hi").await;
    let System::Blocks(b) = rig.requests()[0].system.clone().unwrap() else {
        panic!("system blocks")
    };
    let all: String = b.iter().filter_map(|x| x.text.clone()).collect();
    for must in [
        "Run tests, builds and linters yourself",
        "just make the call: the harness asks the user",
        "NEVER answer with a command for the user to run when you can run it yourself",
        "report the task as blocked",
        "never fake, mock or skip around it",
        "needs_user_input",
    ] {
        assert!(all.contains(must), "{must}");
    }
    assert!(matches!(
        user_text(&rig.requests()[0].messages[0]).as_str(),
        "hi"
    ));
    let _ = (Content::Text(String::new()), user_command_note());
}

// ---- the verification gate asks like Bash does --------------------------------------------

fn gated(o: Opts, turns: Vec<Turn>, approver: Option<Arc<ScriptedApprover>>) -> Rig {
    Rig::build(o, turns, move |h, r| {
        let rt = r.rt.clone();
        let h = h.with_gate(Box::new(move |s| {
            CompletionGate::new(
                VerifyRunner::new(rt.clone(), &s.id, Arc::new(s.workspace.clone())),
                GateConfig::default(),
            )
        }));
        match approver {
            Some(a) => h.with_approver(a),
            None => h,
        }
    })
}

fn gate_opts(interactive: bool) -> Opts {
    Opts {
        interactive,
        allow: vec!["Write".into()],
        ..bash_opts()
    }
}

fn with_check(cmd: &str) -> synthesis_harness::TaskRequest {
    let mut req = synthesis_harness::TaskRequest::new("make a file");
    req.criteria = vec![Criterion::user("c1", "file exists", Some(cmd))];
    req
}

fn write_turns() -> Vec<Turn> {
    vec![
        Turn::tool("w1", "Write", json!({"file_path": "a.txt", "content": "x"})),
        Turn::text("done"),
        Turn::text("done again"),
    ]
}

#[tokio::test]
async fn headless_gate_check_that_needs_approval_is_blocked_not_failed_or_skipped() {
    for interactive in [false, true] {
        let mut rig = gated(gate_opts(interactive), write_turns(), None);
        let out = rig
            .run_with(with_check("test -f a.txt"), CancellationToken::new())
            .await;
        assert_eq!(
            out.status,
            TaskStatus::Blocked,
            "interactive={interactive}: {}",
            out.reason
        );
        assert_eq!(out.reason, "approval_unavailable");
        assert_eq!(out.needed_permissions, ["Bash(test:*)"]);
        let v = rig.find(|e| match e {
            Event::VerificationCompleted {
                status, summary, ..
            } => Some((status.clone(), summary.clone())),
            _ => None,
        });
        assert!(
            !v.is_empty() && v.iter().all(|(s, _)| s != "failed" && s != "passed"),
            "{v:?}"
        );
        assert!(v[0].1.contains("1 blocked"), "{v:?}");
    }
}

#[tokio::test]
async fn interactive_gate_check_asks_the_approver_and_runs_when_allowed() {
    let approver = ScriptedApprover::new(vec![Approval::allow()]);
    let mut rig = gated(gate_opts(true), write_turns(), Some(approver.clone()));
    let out = rig
        .run_with(with_check("test -f a.txt"), CancellationToken::new())
        .await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    let asked = approver.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].input["command"], "test -f a.txt");
    assert!(
        asked[0].reason.contains("verification runs"),
        "{}",
        asked[0].reason
    );
    assert!(!asked[0].suggestions.is_empty());
    assert!(out.needed_permissions.is_empty());
}

/// Honesty: a green check after the agent faked an env var in a test file is `unverified`, and
/// the one-line summary says which test files were modified.
#[tokio::test]
async fn a_test_file_that_fakes_an_env_var_ends_unverified_with_a_limitation() {
    let turns = vec![
        Turn::tool(
            "w1",
            "Write",
            json!({"file_path": "test/db.test.js", "content": "process.env.DB_URL ??= 'postgres://fake';\n"}),
        ),
        Turn::text("done"),
        Turn::text("done again"),
    ];
    let approver = ScriptedApprover::new(vec![Approval::allow(), Approval::allow()]);
    let mut rig = gated(gate_opts(true), turns, Some(approver));
    let mut req = synthesis_harness::TaskRequest::new("fix db");
    req.criteria = vec![Criterion::user("c1", "db", Some("echo ok"))];
    let out = rig.run_with(req, CancellationToken::new()).await;
    assert_eq!(out.status, TaskStatus::Unverified, "{}", out.reason);
    let v = rig.find(|e| match e {
        Event::VerificationCompleted { summary, .. } => Some(summary.clone()),
        _ => None,
    });
    assert!(
        !v.is_empty()
            && v.iter()
                .all(|s| s.contains("test files modified: test/db.test.js")),
        "{v:?}"
    );
}

/// D12: a harness-inferred, recognised verification command (`make test` from a Makefile) runs
/// without an explicit allow in `default` and `acceptEdits`, headless or not, so the task is not
/// blocked only because a trusted inferred check asked. A deny rule still wins.
fn makefile_turns() -> Vec<Turn> {
    vec![
        Turn::tool(
            "w1",
            "Write",
            json!({"file_path": "Makefile", "content": "test:\n\ttouch ran-it\n"}),
        ),
        Turn::text("done"),
        Turn::text("done again"),
    ]
}

#[tokio::test]
async fn trusted_inferred_check_runs_without_an_allow_in_default_and_accept_edits() {
    for mode in [PermissionMode::Default, PermissionMode::AcceptEdits] {
        for interactive in [false, true] {
            let approver = ScriptedApprover::new(vec![]);
            let opts = Opts {
                mode,
                ..gate_opts(interactive)
            };
            let mut rig = gated(opts, makefile_turns(), Some(approver.clone()));
            let out = rig.run("add a makefile").await;
            let ctx = format!("{mode:?} interactive={interactive}: {}", out.reason);
            assert_ne!(out.status, TaskStatus::Blocked, "{ctx}");
            assert!(out.needed_permissions.is_empty(), "{ctx}");
            assert!(
                rig.ws.cwd.join("ran-it").exists(),
                "check did not run: {ctx}"
            );
            assert!(approver.asked.lock().unwrap().is_empty(), "{ctx}");
        }
    }
}

#[tokio::test]
async fn trusted_inferred_check_is_still_subject_to_deny_rules() {
    let opts = Opts {
        deny: vec!["Bash(make:*)".into()],
        ..gate_opts(false)
    };
    let mut rig = gated(opts, makefile_turns(), None);
    let out = rig.run("add a makefile").await;
    assert_ne!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert!(!rig.ws.cwd.join("ran-it").exists());
}

/// B-3 / B-6: the checks of one task are one dialog, the approval during `verifying`
/// is a legal transition that never reaches the transcript, and read-only turns skip the gate.
#[tokio::test]
async fn inferred_checks_are_one_dialog_and_verifying_may_wait_for_approval() {
    // Two user-declared checks: inferred ones are trusted now (D12) and would not ask at all.
    let mut req = synthesis_harness::TaskRequest::new("make a file");
    req.criteria = vec![
        Criterion::user("c1", "file exists", Some("test -f a.txt")),
        Criterion::user("c2", "dir exists", Some("test -d .")),
    ];
    let approver = ScriptedApprover::new(vec![Approval::allow()]);
    let mut rig = gated(gate_opts(true), write_turns(), Some(approver.clone()));
    rig.run_with(req, CancellationToken::new()).await;
    let asked = approver.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let cmd = asked[0].input["command"].as_str().unwrap();
    assert!(
        cmd.contains("test -f a.txt") && cmd.contains("test -d ."),
        "{cmd}"
    );
    let notices = rig.find(|e| match e {
        Event::Notice { text, .. } if text.contains("invalid session transition") => Some(()),
        _ => None,
    });
    assert!(notices.is_empty());
    let moves = rig.find(|e| match e {
        Event::SessionStatusChanged { from, to, .. } => Some((from.clone(), to.clone())),
        _ => None,
    });
    assert!(
        moves.contains(&("verifying".into(), "waiting_approval".into())),
        "{moves:?}"
    );
}

#[tokio::test]
async fn a_read_only_bash_turn_does_not_trigger_the_gate() {
    let turns = vec![
        Turn::tool("b1", "Bash", json!({"command": "ls"})),
        Turn::text("done"),
    ];
    let approver = ScriptedApprover::new(vec![]);
    let mut rig = gated(gate_opts(true), turns, Some(approver.clone()));
    let out = rig.run("list files").await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert!(approver.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn gate_check_denied_by_rule_is_blocked_with_the_rule_reason() {
    let mut rig = gated(
        Opts {
            deny: vec!["Bash(test:*)".into()],
            ..gate_opts(true)
        },
        write_turns(),
        None,
    );
    let out = rig
        .run_with(with_check("test -f a.txt"), CancellationToken::new())
        .await;
    assert_ne!(out.status, TaskStatus::Completed);
    assert_ne!(out.status, TaskStatus::Failed);
    assert!(
        out.needed_permissions.is_empty(),
        "a deny is not a missing approval"
    );
}

#[tokio::test]
async fn pre_tool_hook_deny_blocks_a_user_command() {
    let rig = Rig::build(bash_opts(), vec![], |h, r| {
        let cfg = json!({"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "exit 2"}]}]});
        h.with_hooks(Arc::new(synthesis_hooks::Hooks::new(
            r.rt.clone(),
            "s1",
            r.ws.clone(),
            serde_json::from_value(cfg).unwrap(),
        )))
    });
    let (_, out) = user_cmd(&rig, "echo hi", CancellationToken::new()).await;
    assert!(
        !out.ran
            && out
                .blocked
                .as_deref()
                .is_some_and(|b| b.contains("PreToolUse hook")),
        "{out:?}"
    );
}
