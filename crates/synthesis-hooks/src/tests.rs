use super::*;
use std::fs;
use std::path::PathBuf;
use synthesis_terminal::SandboxMode;

struct Fx {
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(base.join("w")).unwrap();
    Fx {
        dir: base,
        _dir: dir,
    }
}

impl Fx {
    fn hooks(&self, cfg: Value) -> Hooks {
        let ws = Arc::new(
            WorkspaceContext::open(
                "w",
                &self.dir.join("w"),
                &self.dir.join("w"),
                &[],
                self.dir.join("art"),
                "pg",
            )
            .unwrap(),
        );
        let rt = Arc::new(TerminalRuntime::new(SandboxMode::Off));
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        rt.open_session("s1", ws.clone(), env);
        Hooks::new(rt, "s1", ws, serde_json::from_value(cfg).unwrap())
    }
}

fn cfg(event: &str, matcher: Option<&str>, cmds: &[&str]) -> Value {
    let hooks: Vec<Value> = cmds
        .iter()
        .map(|c| json!({"type": "command", "command": c}))
        .collect();
    let mut g = json!({ "hooks": hooks });
    if let Some(m) = matcher {
        g["matcher"] = json!(m);
    }
    json!({ event: [g] })
}

/// Like `cfg` but one hook with `fail_open` and optional timeout.
fn cfg_open(event: &str, cmd: &str, fail_open: bool) -> Value {
    json!({ event: [{"hooks": [{"type": "command", "command": cmd, "fail_open": fail_open}]}] })
}

async fn run(h: &Hooks, ev: HookEvent, target: Option<&str>) -> HookOutcome {
    h.run(
        ev,
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        target,
        &CancellationToken::new(),
    )
    .await
}

#[test]
fn matcher_semantics() {
    assert!(matcher_matches(None, Some("x")));
    assert!(matcher_matches(Some(""), None));
    assert!(matcher_matches(Some("*"), Some("x")));
    assert!(matcher_matches(Some("Write|Edit"), Some("Edit")));
    assert!(
        !matcher_matches(Some("Write|Edit"), Some("MultiEdit")),
        "full match, not substring"
    );
    assert!(matcher_matches(Some("mcp__.*"), Some("mcp__srv__tool")));
    assert!(
        matcher_matches(Some("("), Some("x")),
        "invalid regex fails closed: matches"
    );
    assert!(!matcher_matches(Some("Bash"), None));
}

#[test]
fn config_parses_node_settings_shape() {
    let c: HooksConfig = serde_json::from_value(json!({
        "PreToolUse": [{"matcher": "Write|Edit", "source": "/x", "hooks": [{"type": "command", "command": "c", "timeout": 5, "source": "/x"}]}]
    }))
    .unwrap();
    assert_eq!(c["PreToolUse"][0].hooks[0].timeout, Some(5));
}

#[tokio::test]
async fn stdin_payload_shape_and_env() {
    let f = fx();
    let out = f.dir.join("got.json");
    let cmd = format!(
        "cat > '{0}'; echo \"$SYNT_PROJECT_DIR|$CLAUDE_PROJECT_DIR\" >> '{0}.env'",
        out.display()
    );
    let mut h = f.hooks(cfg("PreToolUse", Some("Bash"), &[&cmd]));
    h.transcript_path = "/t.jsonl".into();
    let r = run(&h, HookEvent::PreToolUse, Some("Bash")).await;
    assert_eq!(r.results[0].exit_code, Some(0));
    let got: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(got["tool_name"], "Bash");
    assert_eq!(got["hook_event_name"], "PreToolUse");
    assert_eq!(got["session_id"], "s1");
    assert_eq!(got["transcript_path"], "/t.jsonl");
    let cwd = f.dir.join("w");
    assert_eq!(got["cwd"], cwd.to_str().unwrap());
    let env = fs::read_to_string(format!("{}.env", out.display())).unwrap();
    assert_eq!(env.trim(), format!("{0}|{0}", cwd.display()));
}

#[tokio::test]
async fn matcher_filters_and_unmatched_is_empty() {
    let f = fx();
    let h = f.hooks(cfg("PreToolUse", Some("Write|Edit"), &["exit 2"]));
    let r = run(&h, HookEvent::PreToolUse, Some("Bash")).await;
    assert!(!r.blocked && r.results.is_empty());
    let r = run(&h, HookEvent::PostToolUse, Some("Write")).await;
    assert!(r.results.is_empty(), "other event");
    let r = run(&h, HookEvent::PreToolUse, Some("Edit")).await;
    assert!(r.blocked);
}

#[tokio::test]
async fn exit_code_semantics() {
    let f = fx();
    let h = f.hooks(cfg("PreToolUse", None, &["echo nope >&2; exit 2"]));
    let r = run(&h, HookEvent::PreToolUse, None).await;
    assert!(r.blocked);
    assert_eq!(r.reason.as_deref(), Some("nope"));
    assert_eq!(r.hook_decision, Some(Decision::Deny));

    let h = f.hooks(cfg("PreToolUse", None, &["exit 2"]));
    assert_eq!(
        run(&h, HookEvent::PreToolUse, None).await.reason.as_deref(),
        Some("blocked by hook")
    );

    // other non-zero with fail_open: non-blocking, stdout ignored
    let h = f.hooks(cfg_open(
        "PreToolUse",
        "echo '{\"decision\":\"block\",\"reason\":\"x\"}'; exit 1",
        true,
    ));
    let r = run(&h, HookEvent::PreToolUse, None).await;
    assert!(!r.blocked && r.hook_decision.is_none());
    assert_eq!(r.results[0].exit_code, Some(1));
}

#[tokio::test]
async fn json_output_merge() {
    let f = fx();
    let allow =
        r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"a"}}'"#;
    let ask = r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"why"},"additionalContext":"b"}'"#;
    let h = f.hooks(cfg("PreToolUse", None, &[allow, ask]));
    let r = run(&h, HookEvent::PreToolUse, None).await;
    assert_eq!(r.hook_decision, Some(Decision::Ask));
    assert_eq!(r.reason.as_deref(), Some("why"));
    assert_eq!(r.additional_context, ["a", "b"]);
    assert!(!r.blocked);

    let deny = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no"}}'"#;
    let h = f.hooks(cfg("PreToolUse", None, &[ask, deny, allow]));
    let r = run(&h, HookEvent::PreToolUse, None).await;
    assert_eq!(
        (r.hook_decision, r.reason.as_deref()),
        (Some(Decision::Deny), Some("no"))
    );

    let blk = r#"echo '{"decision":"block","reason":"stop it"}'"#;
    let r = run(
        &f.hooks(cfg("PreToolUse", None, &[blk])),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(
        r.blocked
            && r.reason.as_deref() == Some("stop it")
            && r.hook_decision == Some(Decision::Deny)
    );

    let appr = r#"echo '{"decision":"approve"}'"#;
    let r = run(
        &f.hooks(cfg("PreToolUse", None, &[appr])),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert_eq!(r.hook_decision, Some(Decision::Allow));

    let cont = r#"echo '{"continue":false,"stopReason":"enough"}'"#;
    let r = run(&f.hooks(cfg("Stop", None, &[cont])), HookEvent::Stop, None).await;
    assert!(r.stop && r.stop_reason.as_deref() == Some("enough"));

    let bad = "echo '{not json'";
    let r = run(
        &f.hooks(cfg_open("PreToolUse", bad, true)),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(!r.blocked && r.hook_decision.is_none());
}

#[tokio::test]
async fn plain_stdout_is_context_only_for_prompt_and_session_start() {
    let f = fx();
    for (name, ev, want) in [
        ("UserPromptSubmit", HookEvent::UserPromptSubmit, 1),
        ("SessionStart", HookEvent::SessionStart, 1),
        ("PreToolUse", HookEvent::PreToolUse, 0),
    ] {
        let r = run(&f.hooks(cfg(name, None, &["echo hello"])), ev, None).await;
        assert_eq!(r.additional_context.len(), want, "{name}");
    }
}

#[tokio::test]
async fn input_rewrite_sets_flag() {
    let f = fx();
    let c = r#"echo '{"hookSpecificOutput":{"updatedInput":{"command":"echo safe"}}}'"#;
    let r = run(
        &f.hooks(cfg("PreToolUse", None, &[c])),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(r.input_rewritten);
    assert_eq!(r.updated_input, Some(json!({"command": "echo safe"})));
    let r = run(
        &f.hooks(cfg("PreToolUse", None, &["true"])),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(!r.input_rewritten && r.updated_input.is_none());
}

#[tokio::test]
async fn duplicates_run_once_and_disabled_runs_none() {
    let f = fx();
    let mut c = cfg("PreToolUse", None, &["true", "true"]);
    let g = c["PreToolUse"][0].clone();
    c["PreToolUse"].as_array_mut().unwrap().push(g);
    let mut h = f.hooks(c);
    assert_eq!(run(&h, HookEvent::PreToolUse, None).await.results.len(), 1);
    h.disabled = true;
    assert!(
        run(&h, HookEvent::PreToolUse, None)
            .await
            .results
            .is_empty()
    );
}

#[tokio::test]
async fn timeout_with_fail_open_is_non_blocking_and_kills_grandchild() {
    let f = fx();
    let pidfile = f.dir.join("pid");
    let cmd = format!("sleep 60 & echo $! > '{}'; wait", pidfile.display());
    let c = json!({"PreToolUse": [{"hooks": [{"type": "command", "command": cmd, "timeout": 1, "fail_open": true}]}]});
    let t0 = Instant::now();
    let r = run(&f.hooks(c), HookEvent::PreToolUse, None).await;
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert!(r.results[0].timed_out && r.results[0].exit_code.is_none());
    assert!(!r.blocked && r.hook_decision.is_none());
    let pid = fs::read_to_string(&pidfile).unwrap().trim().to_string();
    for _ in 0..300 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("grandchild {pid} survived");
}

#[tokio::test]
async fn cancel_stops_hook_without_blocking() {
    let f = fx();
    let h = f.hooks(cfg("PreToolUse", None, &["sleep 30"]));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let t0 = Instant::now();
    let r = h.run(HookEvent::PreToolUse, json!({}), None, &cancel).await;
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert!(!r.blocked && r.results[0].exit_code.is_none());
}

#[tokio::test]
async fn hooks_run_in_parallel() {
    let f = fx();
    let h = f.hooks(cfg(
        "PreToolUse",
        None,
        &["sleep 1", "sleep 1.01", "sleep 1.02"],
    ));
    let t0 = Instant::now();
    let r = run(&h, HookEvent::PreToolUse, None).await;
    assert_eq!(r.results.len(), 3);
    assert!(t0.elapsed() < Duration::from_millis(2500));
}

#[tokio::test]
async fn multiline_command_and_env_filter() {
    let f = fx();
    let h = f
        .hooks(cfg(
            "Stop",
            None,
            &["x=1\nif [ \"$x\" = 1 ]; then exit 2; fi"],
        ))
        .with_env(BTreeMap::from([
            ("HOME".into(), "/x".into()),
            ("SYNT_A".into(), "1".into()),
        ]));
    assert!(run(&h, HookEvent::Stop, None).await.blocked);
    assert_eq!(h.env.len(), 1);
}

// F14: fail closed on PreToolUse / UserPromptSubmit unless `fail_open: true`.
#[tokio::test]
async fn failures_block_by_default_for_pre_tool_use_and_prompt_submit() {
    let f = fx();
    let slow = json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "sleep 30", "timeout": 1}]}]});
    let cases = [
        (cfg("PreToolUse", None, &["exit 1"]), "exit code 1"),
        (cfg("PreToolUse", None, &["echo boom >&2; exit 7"]), "boom"),
        (
            cfg("PreToolUse", None, &["echo '{not json'"]),
            "invalid JSON",
        ),
        (cfg("PreToolUse", None, &["kill -9 $$"]), "no exit code"),
        (slow, "timed out"),
    ];
    for (c, want) in cases {
        let r = run(&f.hooks(c), HookEvent::PreToolUse, None).await;
        assert!(r.blocked && r.failed_closed, "{want}");
        assert_eq!(r.hook_decision, Some(Decision::Deny));
        assert!(
            r.reason.as_deref().unwrap().contains(want),
            "{:?}",
            r.reason
        );
    }
    let r = run(
        &f.hooks(cfg("UserPromptSubmit", None, &["exit 1"])),
        HookEvent::UserPromptSubmit,
        None,
    )
    .await;
    assert!(r.blocked && r.failed_closed);
    // spawn error (unknown session) also blocks
    let mut h = f.hooks(cfg("PreToolUse", None, &["true"]));
    h.session_id = "nope".into();
    assert!(run(&h, HookEvent::PreToolUse, None).await.blocked);
}

#[tokio::test]
async fn fail_open_opt_out_and_other_events_stay_open() {
    let f = fx();
    let r = run(
        &f.hooks(cfg_open("PreToolUse", "exit 1", true)),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(!r.blocked && !r.failed_closed);
    // fail_open: false is the default, explicit works too
    let r = run(
        &f.hooks(cfg_open("PreToolUse", "exit 1", false)),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    assert!(r.blocked);
    for (name, ev) in [
        ("PostToolUse", HookEvent::PostToolUse),
        ("Stop", HookEvent::Stop),
    ] {
        let r = run(&f.hooks(cfg(name, None, &["exit 1"])), ev, None).await;
        assert!(!r.blocked, "{name}");
    }
}

#[tokio::test]
async fn success_and_plain_stdout_do_not_trip_fail_closed() {
    let f = fx();
    for cmd in ["true", "echo hello", "echo '{}'"] {
        let r = run(
            &f.hooks(cfg("PreToolUse", None, &[cmd])),
            HookEvent::PreToolUse,
            None,
        )
        .await;
        assert!(!r.blocked, "{cmd}");
    }
}

#[tokio::test]
async fn allow_is_advisory_and_rewrite_still_surfaced() {
    let f = fx();
    let c = r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"command":"rm -rf x"}}}'"#;
    let r = run(
        &f.hooks(cfg("PreToolUse", None, &[c])),
        HookEvent::PreToolUse,
        None,
    )
    .await;
    // The harness must still call Permissions::check/recheck: allow never skips policy, and the
    // rewritten input must be re-checked.
    assert_eq!(r.hook_decision, Some(Decision::Allow));
    assert!(!r.blocked && r.input_rewritten);
    assert_eq!(r.updated_input, Some(json!({"command": "rm -rf x"})));
}

#[test]
fn default_timeout_is_60s_for_gating_events_and_600s_for_the_rest() {
    use crate::{HookEvent::*, default_timeout_secs};
    for e in [PreToolUse, UserPromptSubmit] {
        assert_eq!(default_timeout_secs(e), 60, "{e:?}");
    }
    for e in [
        PostToolUse,
        Stop,
        SubagentStop,
        SessionStart,
        SessionEnd,
        Notification,
        PreCompact,
    ] {
        assert_eq!(default_timeout_secs(e), 600, "{e:?}");
    }
}

#[test]
fn matcher_is_an_exact_list_or_an_unanchored_regex() {
    assert!(matcher_matches(Some("Edit|Write"), Some("Write")));
    assert!(
        !matcher_matches(Some("Writ"), Some("Write")),
        "a plain list is exact"
    );
    assert!(matcher_matches(Some("Wri.*"), Some("Write")));
    assert!(
        !matcher_matches(Some("rit"), Some("Write")),
        "plain names are exact"
    );
    assert!(matcher_matches(Some("^W.*e$|zzz"), Some("Write")));
    assert!(matcher_matches(Some("mcp__.*"), Some("mcp__a__b")));
    assert!(
        matcher_matches(Some("rit."), Some("Write")),
        "regex is unanchored"
    );
}

#[tokio::test]
async fn matcher_is_ignored_on_stop_and_user_prompt_submit() {
    let f = fx();
    let h = f.hooks(cfg("Stop", Some("zzz"), &["exit 0"]));
    let r = run(&h, HookEvent::Stop, None).await;
    assert_eq!(r.results.len(), 1, "a Stop hook with any matcher fires");
}

#[tokio::test]
async fn per_hook_if_filters_by_tool_call() {
    let f = fx();
    let c = json!({"PreToolUse": [{"hooks": [
        {"type": "command", "command": "exit 0", "if": "Bash(ls *)"},
        {"type": "command", "command": "true", "if": "Bash(git *)"},
        {"type": "command", "command": "echo any", "if": "Bash"},
        {"type": "command", "command": "echo edit", "if": "Edit"}
    ]}]});
    let h = f.hooks(c);
    // `run` sends {"tool_name":"Bash","tool_input":{"command":"ls"}}
    let r = run(&h, HookEvent::PreToolUse, Some("Bash")).await;
    let cmds: Vec<_> = r.results.iter().map(|x| x.command.as_str()).collect();
    assert_eq!(cmds, ["exit 0", "echo any"], "{cmds:?}");
}

#[tokio::test]
async fn bash_if_sees_through_wrappers_and_unparsable_commands() {
    let f = fx();
    let h = f.hooks(json!({"PreToolUse": [{"hooks": [
        {"type": "command", "command": "exit 0", "if": "Bash(git push *)"}
    ]}]}));
    for cmd in [
        "cd r && git push -f",
        "time git push",
        "FOO=1 git push",
        "sh -c 'git push'",
        "git push '",
    ] {
        let r = h
            .run(
                HookEvent::PreToolUse,
                json!({"tool_name": "Bash", "tool_input": {"command": cmd}}),
                Some("Bash"),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(r.results.len(), 1, "{cmd}");
    }
    let r = run(&h, HookEvent::PreToolUse, Some("Bash")).await;
    assert!(r.results.is_empty(), "ls does not trigger it");
}

#[tokio::test]
async fn invalid_matcher_runs_the_hook_and_warns() {
    let f = fx();
    let h = f.hooks(cfg("PreToolUse", Some("("), &["exit 0"]));
    let r = run(&h, HookEvent::PreToolUse, Some("Bash")).await;
    assert_eq!(r.results.len(), 1);
    assert_eq!(r.warnings.len(), 1);
}

#[tokio::test]
async fn pre_compact_exit_2_is_reported_as_blocked() {
    let f = fx();
    let h = f.hooks(cfg("PreCompact", None, &["echo no >&2; exit 2"]));
    let r = run(&h, HookEvent::PreCompact, None).await;
    assert!(r.blocked);
}
