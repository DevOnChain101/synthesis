// Most tests here are unix-only (sandbox/process groups), so their helpers are unused on Windows.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use synthesis_core::{Budget, PermissionMode, SessionStatus};
use synthesis_events::{ArtifactRef, Event, TaskStatus, ToolStatus};
use synthesis_harness::testing::Turn;
use synthesis_harness::{
    AgentDef, AgentTable, Approval, ApprovalRequest, Approver, BoxFut, Built, Compaction,
    ContextProvider, InputProvider, Pricing, StaticContext, TaskRequest, ToolBox, UserReply,
    budget_from_settings,
};
use synthesis_permissions::Mode;
use synthesis_providers::{ErrorKind, Message, ProviderError};
use synthesis_tools::{NeedsUser, Registry, SideEffectClass};
use synthesis_verification::{CompletionGate, Criterion, GateConfig, VerifyRunner};
use tokio_util::sync::CancellationToken;

fn probe_opts(p: &Arc<Probe>) -> Opts {
    Opts {
        tools: ToolBox::empty().with_tool(p.clone()),
        allow: vec!["Probe".into()],
        ..Opts::default()
    }
}

fn many_tool_turns(n: usize) -> Vec<Turn> {
    (0..n)
        .map(|i| Turn::tool(&format!("t{i}"), "Probe", json!({"x": i.to_string()})))
        .collect()
}

fn reason_of(out: &synthesis_harness::TaskOutcome) -> String {
    format!("{:?}: {}", out.status, out.reason)
}

// ---- budgets ------------------------------------------------------------------------------

#[tokio::test]
async fn budget_max_turns() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_turns: 2,
        ..Budget::default()
    };
    let mut rig = Rig::new(o, many_tool_turns(5));
    let out = rig.run("x").await;
    assert_eq!(
        (out.status, out.reason.as_str()),
        (TaskStatus::Failed, "budget_exceeded:max_turns")
    );
    assert_eq!(rig.requests().len(), 2);
    let b = rig.find(|e| match e {
        Event::BudgetExceeded {
            budget,
            limit,
            used,
        } => Some((budget.clone(), *limit, *used)),
        _ => None,
    });
    assert_eq!(b, vec![("max_turns".to_string(), 2.0, 2.0)]);
    assert_eq!(rig.session.status, SessionStatus::Failed);
    check_stream(&rig.events());
}

#[tokio::test]
async fn budget_max_tool_calls() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_tool_calls: 1,
        ..Budget::default()
    };
    o.parallel_tools = false;
    let mut rig = Rig::new(
        o,
        vec![
            Turn::tools(&[
                ("a", "Probe", json!({"x": "1"})),
                ("b", "Probe", json!({"x": "2"})),
            ]),
            Turn::text("never"),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(
        out.reason,
        "budget_exceeded:max_tool_calls",
        "{}",
        reason_of(&out)
    );
    assert_eq!(p.runs(), 1);
    let st = rig.find(|e| match e {
        Event::ToolCompleted {
            tool_call_id,
            status,
            not_run,
            ..
        } => Some((tool_call_id.clone(), *status, *not_run)),
        _ => None,
    });
    assert!(
        st.contains(&("b".to_string(), ToolStatus::BudgetExceeded, true)),
        "{st:?}"
    );
    assert_eq!(
        rig.requests().len(),
        1,
        "the task stops without another model call"
    );
    check_stream(&rig.events());
}

#[tokio::test]
async fn budget_wall_clock_stops_a_slow_tool() {
    let p = Probe::new("Probe", SideEffectClass::Read, 10_000, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_wall_ms: Some(200),
        ..Budget::default()
    };
    let mut rig = Rig::new(
        o,
        vec![Turn::tool("a", "Probe", json!({})), Turn::text("never")],
    );
    let t0 = Instant::now();
    let out = rig.run("x").await;
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(
        out.reason,
        "budget_exceeded:max_wall_ms",
        "{}",
        reason_of(&out)
    );
    let st = rig.find(|e| match e {
        Event::ToolCompleted { status, .. } => Some(*status),
        _ => None,
    });
    assert_eq!(st, vec![ToolStatus::Timeout]);
}

#[tokio::test]
async fn budget_wall_clock_stops_a_stalled_or_slow_model_call() {
    for turn in [
        Turn::text("never").delayed(60_000),
        Turn::text("partial then slow").delayed_mid_stream(60_000),
    ] {
        let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
        let mut o = probe_opts(&p);
        o.budget = Budget {
            max_wall_ms: Some(300),
            ..Budget::default()
        };
        let mut rig = Rig::new(o, vec![turn]);
        let t0 = Instant::now();
        let out = rig.run("x").await;
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        assert_eq!(
            out.reason,
            "budget_exceeded:max_wall_ms",
            "{}",
            reason_of(&out)
        );
        assert!(
            rig.types().iter().any(|t| t == "budget.exceeded"),
            "{:?}",
            rig.types()
        );
    }
}

#[tokio::test]
async fn budget_tokens_and_cost() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.config = |c| c.max_total_tokens = Some(100);
    let turns: Vec<Turn> = many_tool_turns(4)
        .into_iter()
        .map(|t| t.with_usage(80, 40))
        .collect();
    let mut rig = Rig::new(o, turns);
    let out = rig.run("x").await;
    assert_eq!(
        out.reason,
        "budget_exceeded:max_tokens",
        "{}",
        reason_of(&out)
    );
    assert_eq!(rig.requests().len(), 1);
    assert_eq!((out.usage.input_tokens, out.usage.output_tokens), (80, 40));

    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_cost_usd: Some(1.0),
        ..Budget::default()
    };
    o.config = |c| {
        c.pricing = Some(Pricing {
            input_per_mtok: 1000.0,
            output_per_mtok: 1000.0,
            ..Pricing::default()
        });
        c.max_output_tokens = 100; // keeps the pre-turn worst case under the $1 cap
    };
    let turns: Vec<Turn> = many_tool_turns(4)
        .into_iter()
        .map(|t| t.with_usage(1000, 1000))
        .collect();
    let mut rig = Rig::new(o, turns);
    let out = rig.run("x").await;
    assert_eq!(
        out.reason,
        "budget_exceeded:max_cost_usd",
        "{}",
        reason_of(&out)
    );
    assert_eq!(out.usage.cost_usd, Some(2.0));
}

/// The cap is also checked BEFORE a call: a request whose worst case (prompt + all of `max_tokens`
/// at the output price) would cross it is never sent.
#[tokio::test]
async fn pre_turn_estimate_refuses_a_call_that_could_cross_the_cost_cap() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_cost_usd: Some(1.0),
        ..Budget::default()
    };
    // 8192 output tokens at $1000/M = $8.2 > $1
    o.config = |c| {
        c.pricing = Some(Pricing {
            input_per_mtok: 1000.0,
            output_per_mtok: 1000.0,
            ..Pricing::default()
        })
    };
    let mut rig = Rig::new(o, many_tool_turns(2));
    let out = rig.run("x").await;
    assert_eq!(
        out.reason,
        "budget_exceeded:max_cost_usd_estimate",
        "{}",
        reason_of(&out)
    );
    assert_eq!(rig.requests().len(), 0, "no request may be sent");
    assert_ne!(out.status, synthesis_events::TaskStatus::Completed);
    let hits = rig.find(|e| match e {
        synthesis_events::Event::BudgetExceeded {
            budget,
            limit,
            used,
        } => Some((budget.clone(), *limit, *used)),
        _ => None,
    });
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "max_cost_usd_estimate");
    assert!(hits[0].2 > hits[0].1);
}

#[test]
fn long_context_tier_prices_the_whole_request_dearer() {
    use synthesis_harness::LongTier;
    let p = Pricing {
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
        cache_read_per_mtok: Some(0.3),
        cache_write_per_mtok: Some(3.75),
        long: Some(LongTier {
            above_tokens: 200_000,
            input_mult: 2.0,
            output_mult: 1.5,
        }),
    };
    let u = |input, read| synthesis_providers::Usage {
        input_tokens: Some(input),
        output_tokens: Some(1_000_000),
        cache_read_input_tokens: Some(read),
        cache_creation_input_tokens: None,
    };
    // 100k + 100k = 200k is not above the threshold
    assert!((p.cost(&u(100_000, 100_000)) - (0.3 + 0.03 + 15.0)).abs() < 1e-9);
    // above it (cache tokens count towards the prompt): input side x2, output x1.5
    assert!((p.cost(&u(100_001, 100_000)) - ((0.300_003 + 0.03) * 2.0 + 22.5)).abs() < 1e-6);
    // the worst case uses the dearer cache-write price and the tier
    assert!((p.worst_case(1_000_000, 0) - 7.5).abs() < 1e-9);
}

#[tokio::test]
async fn budget_max_tokens_total_counts_cache_and_stops_the_task() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_tokens_total: Some(1000),
        ..Budget::default()
    };
    let turns: Vec<Turn> = many_tool_turns(4)
        .into_iter()
        .map(|t| t.with_usage(10, 10).with_cache(900, 100))
        .collect();
    let mut rig = Rig::new(o, turns);
    let out = rig.run("x").await;
    assert_eq!(
        out.reason,
        "budget_exceeded:max_tokens",
        "{}",
        reason_of(&out)
    );
    assert_eq!(rig.requests().len(), 1);
}

#[tokio::test]
async fn cache_tokens_are_priced() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.config = |c| {
        c.pricing = Some(Pricing {
            input_per_mtok: 10.0,
            output_per_mtok: 20.0,
            cache_read_per_mtok: Some(1.0),
            cache_write_per_mtok: None, // 125% of input = 12.5
            long: None,
        })
    };
    let mut rig = Rig::new(
        o,
        vec![
            Turn::text("done")
                .with_usage(1_000_000, 1_000_000)
                .with_cache(1_000_000, 1_000_000),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(out.usage.cost_usd, Some(10.0 + 20.0 + 1.0 + 12.5));
}

/// A failover target that costs more than the model the run was priced for is charged at ITS
/// price, so a cap cannot be overrun by a switch the cost accounting never saw.
#[tokio::test]
async fn cost_is_priced_by_the_model_that_answered() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_cost_usd: Some(5.0),
        ..Budget::default()
    };
    o.config = |c| {
        let cheap = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            ..Pricing::default()
        };
        c.pricing = Some(cheap);
        c.pricing_for = Some(std::sync::Arc::new(|m: &str| {
            (m == "b/dear").then_some(Pricing {
                input_per_mtok: 100.0,
                output_per_mtok: 100.0,
                ..Pricing::default()
            })
        }));
    };
    let turns: Vec<Turn> = many_tool_turns(4)
        .into_iter()
        .map(|t| t.with_usage(30_000, 30_000).answered_by("b/dear"))
        .collect();
    let mut rig = Rig::new(o, turns);
    let out = rig.run("x").await;
    // 60k tokens at $100/M = $6 per response: the first response already passes the $5 cap
    assert_eq!(
        out.reason,
        "budget_exceeded:max_cost_usd",
        "{}",
        reason_of(&out)
    );
    assert_eq!(rig.requests().len(), 1);
    assert_eq!(out.usage.cost_usd, Some(6.0));
}

#[test]
fn budget_comes_from_settings() {
    let mut s = synthesis_settings::Settings::default();
    s.synthesis.budget.max_turns = Some(7);
    s.synthesis.budget.max_verify_attempts = Some(2);
    let b = budget_from_settings(&s);
    assert_eq!(
        (b.max_turns, b.max_tool_calls, b.max_verify_attempts),
        (7, Budget::default().max_tool_calls, 2)
    );
}

// ---- cancellation -------------------------------------------------------------------------

fn cancel_after(ms: u64) -> CancellationToken {
    let c = CancellationToken::new();
    let c2 = c.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        c2.cancel();
    });
    c
}

#[tokio::test]
async fn cancel_mid_stream() {
    let mut rig = Rig::new(
        Opts::default(),
        vec![Turn::text("hello world").delayed_mid_stream(10_000)],
    );
    let t0 = Instant::now();
    let out = rig.run_with(TaskRequest::new("x"), cancel_after(100)).await;
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(out.status, TaskStatus::Cancelled);
    assert_eq!(rig.session.status, SessionStatus::Cancelled);
    let tr = rig.transitions();
    assert!(
        tr.ends_with(&[
            ("running".into(), "cancelling".into()),
            ("cancelling".into(), "cancelled".into())
        ]),
        "{tr:?}"
    );
    check_stream(&rig.events());
    assert_eq!(
        rig.types().last().map(String::as_str),
        Some("session.cancelled")
    );
}

#[tokio::test]
async fn cancel_mid_tool_keeps_tool_results_paired() {
    let p = Probe::new("Probe", SideEffectClass::Read, 10_000, "r");
    let mut rig = Rig::new(
        probe_opts(&p),
        vec![Turn::tool("a", "Probe", json!({})), Turn::text("never")],
    );
    let out = rig.run_with(TaskRequest::new("x"), cancel_after(150)).await;
    assert_eq!(out.status, TaskStatus::Cancelled);
    let st = rig.find(|e| match e {
        Event::ToolCompleted { status, .. } => Some(*status),
        _ => None,
    });
    assert_eq!(st, vec![ToolStatus::Interrupted]);
    // history stays valid: the tool_use is answered
    let last = out.messages.last().unwrap();
    assert!(
        matches!(&last.content, synthesis_providers::Content::Blocks(b) if matches!(&b[0], synthesis_providers::Block::ToolResult { .. }))
    );
    assert_eq!(rig.requests().len(), 1);
    check_stream(&rig.events());
}

struct Hanging;
impl Approver for Hanging {
    fn approve<'a>(&'a self, _: ApprovalRequest, _: &'a CancellationToken) -> BoxFut<'a, Approval> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn cancel_while_waiting_for_approval() {
    let w = Probe::new("Mutate", SideEffectClass::Write, 0, "x");
    let mut rig = Rig::build(
        Opts {
            tools: ToolBox::empty().with_tool(w.clone()),
            interactive: true,
            ..Opts::default()
        },
        vec![Turn::tool("m", "Mutate", json!({}))],
        |h, _| h.with_approver(Arc::new(Hanging)),
    );
    let out = rig.run_with(TaskRequest::new("x"), cancel_after(100)).await;
    assert_eq!(out.status, TaskStatus::Cancelled);
    assert_eq!(w.runs(), 0);
    let tr = rig.transitions();
    assert!(
        tr.contains(&("waiting_approval".into(), "cancelling".into())),
        "{tr:?}"
    );
}

// ---- provider errors ----------------------------------------------------------------------

#[tokio::test]
async fn auth_error_blocks_with_reason() {
    let mut rig = Rig::new(
        Opts::default(),
        vec![Turn::error(ProviderError::new(ErrorKind::Auth, "bad key"))],
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Blocked);
    assert!(out.reason.starts_with("model_error:auth"), "{}", out.reason);
    let e = rig.find(|e| match e {
        Event::ModelError {
            message,
            retryable,
            context_overflow,
            ..
        } => Some((message.clone(), *retryable, *context_overflow)),
        _ => None,
    });
    assert_eq!(e, vec![("bad key".to_string(), false, false)]);
}

fn mid_stream(kind: ErrorKind) -> ProviderError {
    let mut e = ProviderError::new(kind, "connection reset");
    e.mid_stream = true;
    e
}

#[tokio::test]
async fn mid_stream_failure_is_recovered_once_without_committing_partial_output() {
    let mut rig = Rig::new(
        Opts::default(),
        vec![
            Turn::text("partial").then_error(mid_stream(ErrorKind::Network)),
            Turn::text("complete"),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Completed);
    assert_eq!(out.final_text, "complete");
    let r = rig.requests();
    assert_eq!(
        r[0].messages, r[1].messages,
        "the failed attempt left nothing in history"
    );
    assert!(rig.types().contains(&"model.retry".to_string()));

    // a second failure is final
    let mut rig = Rig::new(
        Opts::default(),
        vec![
            Turn::text("a").then_error(mid_stream(ErrorKind::Network)),
            Turn::text("b").then_error(mid_stream(ErrorKind::Network)),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Failed);
    assert!(out.reason.starts_with("model_error:network"));
}

#[tokio::test]
async fn pre_stream_errors_are_not_retried_again_by_the_harness() {
    let mut rig = Rig::new(
        Opts::default(),
        vec![
            Turn::error(ProviderError::new(ErrorKind::Server, "500")),
            Turn::text("never reached"),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Failed);
    assert_eq!(
        rig.provider.remaining(),
        1,
        "the provider layer owns retries"
    );
}

struct Compacting;
impl ContextProvider for Compacting {
    fn build(&self, history: &[Message], p: Option<&str>) -> Result<Built, String> {
        StaticContext::default().build(history, p)
    }
    fn compact<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a str,
    ) -> BoxFut<'a, Result<Compaction, String>> {
        Box::pin(async {
            Ok(Compaction {
                messages: vec![Message {
                    role: synthesis_providers::Role::User,
                    content: synthesis_providers::Content::Text(
                        "Summary of the conversation so far: ...".into(),
                    ),
                }],
                summary_ref: ArtifactRef {
                    artifact_id: "a".into(),
                    path: "s.txt".into(),
                    size: 1,
                    sha256: "0".into(),
                    mime: "text/plain".into(),
                },
                tokens_before: Some(123),
                summary_chars: 3,
            })
        })
    }
}

#[tokio::test]
async fn context_overflow_compacts_a_bounded_number_of_times() {
    let mut rig = Rig::build(
        Opts::default(),
        vec![Turn::context_overflow(), Turn::text("fits now")],
        |h, _| h.with_context(Arc::new(Compacting)),
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Completed);
    assert!(rig.types().contains(&"context.compacted".to_string()));
    assert!(user_text(&rig.requests()[1].messages[0]).starts_with("Summary of the conversation"));

    // a second overflow compacts again (images can keep a request too big); the fourth is reported
    let mut rig = Rig::build(
        Opts::default(),
        vec![
            Turn::context_overflow(),
            Turn::context_overflow(),
            Turn::text("fits"),
        ],
        |h, _| h.with_context(Arc::new(Compacting)),
    );
    assert_eq!(rig.run("x").await.status, TaskStatus::Completed);
    let mut rig = Rig::build(
        Opts::default(),
        vec![
            Turn::context_overflow(),
            Turn::context_overflow(),
            Turn::context_overflow(),
            Turn::context_overflow(),
            Turn::text("never"),
        ],
        |h, _| h.with_context(Arc::new(Compacting)),
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Failed);

    // no compaction configured
    let mut rig = Rig::new(Opts::default(), vec![Turn::context_overflow()]);
    let out = rig.run("x").await;
    assert!(
        out.reason
            .starts_with("context_overflow: compaction failed"),
        "{}",
        out.reason
    );
}

// ---- stop conditions ----------------------------------------------------------------------

#[tokio::test]
async fn no_progress_is_detected() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "same");
    let same = |i: usize| Turn::tool(&format!("t{i}"), "Probe", json!({"x": "same"}));
    let mut rig = Rig::new(probe_opts(&p), (0..6).map(same).collect());
    let out = rig.run("x").await;
    assert_eq!(
        (out.status, out.reason.as_str()),
        (TaskStatus::Failed, "no_progress")
    );
    assert_eq!(rig.requests().len(), 3);
}

#[tokio::test]
async fn repeated_unapprovable_calls_block_the_task() {
    let w = Probe::new("Mutate", SideEffectClass::Write, 0, "x");
    let same = |i: usize| Turn::tool(&format!("t{i}"), "Mutate", json!({}));
    let mut rig = Rig::new(
        Opts {
            tools: ToolBox::empty().with_tool(w.clone()),
            ..Opts::default()
        },
        (0..6).map(same).collect(),
    );
    let out = rig.run("x").await;
    assert_eq!(
        (out.status, out.reason.as_str()),
        (TaskStatus::Blocked, "approval_unavailable")
    );
    assert_eq!(w.runs(), 0);
}

#[tokio::test]
async fn unsafe_workspace_blocks() {
    let mut rig = Rig::new(Opts::default(), vec![Turn::text("never")]);
    std::fs::remove_dir_all(rig.work()).unwrap();
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Blocked);
    assert!(out.reason.starts_with("unsafe_workspace"), "{}", out.reason);
    assert!(
        rig.requests().is_empty(),
        "no model call in an unsafe workspace"
    );
}

// ---- completion gate ----------------------------------------------------------------------

fn gate_rig(turns: Vec<Turn>) -> Rig {
    Rig::build(
        Opts {
            tools: ToolBox::new(Registry::builtin()),
            mode: PermissionMode::AcceptEdits,
            // the gate's check commands are judged like Bash calls: these two are allowed
            allow: vec!["Bash(test:*)".into(), "Bash(false)".into()],
            ..Opts::default()
        },
        turns,
        |h, r| {
            let rt = r.rt.clone();
            h.with_gate(Box::new(move |s| {
                CompletionGate::new(
                    VerifyRunner::new(rt.clone(), &s.id, Arc::new(s.workspace.clone())),
                    GateConfig::default(),
                )
            }))
        },
    )
}

#[cfg(unix)]
#[tokio::test]
async fn gate_continue_then_fix_then_complete() {
    let mut rig = gate_rig(vec![
        Turn::text("All done!"), // claims done, nothing fixed
        Turn::tool(
            "w1",
            "Write",
            json!({"file_path": "fixed.txt", "content": "ok"}),
        ),
        Turn::text("Now it is fixed."),
    ]);
    let mut req = TaskRequest::new("create fixed.txt");
    req.criteria = vec![Criterion::user(
        "c1",
        "fixed.txt exists",
        Some("test -f fixed.txt"),
    )];
    let out = rig.run_with(req, CancellationToken::new()).await;

    assert_eq!(out.status, TaskStatus::Completed, "{}", reason_of(&out));
    assert_eq!(out.reason, "verified");
    assert!(out.verification_report_id.is_some());
    assert!(rig.work().join("fixed.txt").exists());
    assert_eq!(out.changed_files, vec!["fixed.txt".to_string()]);

    // the first "done" did NOT complete the task: feedback went back to the model
    let reqs = rig.requests();
    assert_eq!(reqs.len(), 3);
    assert!(user_text(reqs[1].messages.last().unwrap()).contains("NOT verified as complete"));
    let v = rig.find(|e| match e {
        Event::VerificationCompleted {
            status, attempt, ..
        } => Some((status.clone(), *attempt)),
        _ => None,
    });
    assert_eq!(
        v,
        vec![("failed".to_string(), 1), ("passed".to_string(), 2)]
    );
    let tr: Vec<String> = rig
        .transitions()
        .into_iter()
        .map(|(a, b)| format!("{a}>{b}"))
        .collect();
    assert_eq!(
        tr,
        [
            "created>preparing",
            "preparing>running",
            "running>verifying",
            "verifying>running",
            "running>verifying",
            "verifying>completed"
        ]
    );
    let ended = rig.find(|e| match e {
        Event::TaskEnded {
            status,
            verification_report_id,
            ..
        } => Some((*status, verification_report_id.clone())),
        _ => None,
    });
    assert_eq!(
        ended,
        vec![(TaskStatus::Completed, out.verification_report_id.clone())]
    );
    assert_eq!(rig.session.usage.verify_attempts, 2);
    check_stream(&rig.events());
}

#[cfg(unix)]
#[tokio::test]
async fn gate_never_lets_a_claim_through() {
    // the model keeps claiming success; the check keeps failing: the gate ends the task
    let mut rig = gate_rig(vec![
        Turn::text("done 1"),
        Turn::text("done 2"),
        Turn::text("done 3"),
        Turn::text("done 4"),
    ]);
    let mut req = TaskRequest::new("x");
    req.criteria = vec![Criterion::user("c1", "never true", Some("false"))];
    let out = rig.run_with(req, CancellationToken::new()).await;
    assert_ne!(out.status, TaskStatus::Completed, "{}", reason_of(&out));
    assert_eq!(out.status, TaskStatus::Failed);
}

#[tokio::test]
async fn budget_stop_without_proof_is_never_completed() {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = probe_opts(&p);
    o.budget = Budget {
        max_turns: 1,
        ..Budget::default()
    };
    let mut rig = Rig::new(o, many_tool_turns(3));
    let out = rig.run("x").await;
    assert_ne!(out.status, TaskStatus::Completed);
}

// ---- sub-agents ---------------------------------------------------------------------------

fn agents() -> AgentTable {
    AgentTable::new([
        AgentDef {
            name: "writer".into(),
            description: "".into(),
            prompt: "You write files.".into(),
            tools: Some(vec!["Write".into(), "Grep".into()]),
            mode: Some(Mode::BypassPermissions),
            model: None,
            max_turns: None,
        },
        AgentDef {
            name: "planner".into(),
            description: "".into(),
            prompt: "".into(),
            tools: Some(vec!["Grep".into()]),
            mode: Some(Mode::Plan),
            model: None,
            max_turns: None,
        },
        AgentDef {
            name: "reader".into(),
            description: "".into(),
            prompt: "".into(),
            tools: Some(vec!["Read".into()]),
            mode: None,
            model: None,
            max_turns: None,
        },
        AgentDef {
            name: "nest".into(),
            description: "".into(),
            prompt: "".into(),
            tools: Some(vec!["Task".into()]),
            mode: None,
            model: None,
            max_turns: None,
        },
    ])
}

fn task_call(id: &str, kind: &str) -> Turn {
    Turn::tool(
        id,
        "Task",
        json!({"description": "d", "prompt": "do it", "subagent_type": kind}),
    )
}

#[tokio::test]
async fn subagent_cannot_be_more_permissive_than_its_parent() {
    // The parent is in plan mode; the definition asks for bypassPermissions.
    let mode_probe = Probe::new("Grep", SideEffectClass::Read, 0, "g");
    let mut rig = Rig::build(
        Opts {
            mode: PermissionMode::Plan,
            tools: ToolBox::new(Registry::builtin()).with_tool(mode_probe.clone()),
            ..Opts::default()
        },
        vec![
            task_call("tk", "writer"),
            Turn::tool("cw", "Write", json!({"file_path": "x.txt", "content": "c"})),
            Turn::text("write was refused"),
            Turn::text("parent done"),
        ],
        |h, _| h.with_agents(agents()),
    );
    let out = rig.run("delegate").await;
    assert_eq!(out.final_text, "parent done");
    assert!(
        !rig.work().join("x.txt").exists(),
        "child must not write under a plan-mode parent"
    );
    let reqs = rig.requests();
    let (child_res, is_err) = tool_result(&reqs[2], "cw").unwrap();
    assert!(is_err && child_res.contains("NOT run"), "{child_res}");
    let (task_res, _) = tool_result(&reqs[3], "tk").unwrap();
    assert!(task_res.contains("write was refused"));
    // child history is separate: its first message is the delegated prompt only
    assert_eq!(reqs[1].messages.len(), 1);
    assert!(user_text(&reqs[1].messages[0]).contains("do it"));

    // events: child events carry agent_id, parent ref is the Task call id
    let envs = rig.events();
    let started: Vec<_> = envs
        .iter()
        .filter(|e| matches!(e.event, Event::SubagentStarted(_)))
        .collect();
    assert_eq!(started.len(), 1);
    let child_id = started[0].agent_id.clone().expect("child agent id");
    let Event::SubagentStarted(info) = &started[0].event else {
        unreachable!()
    };
    assert_eq!(
        (info.tool_call_id.as_deref(), info.agent_type.as_str()),
        (Some("tk"), "writer")
    );
    assert_eq!(info.agent_id, child_id);
    assert!(envs.iter().any(
        |e| matches!(&e.event, Event::ToolRequested { tool_call_id, .. } if tool_call_id == "cw")
            && e.agent_id.as_deref() == Some(&child_id)
    ));
    assert!(envs.iter().any(
        |e| matches!(&e.event, Event::ToolRequested { tool_call_id, .. } if tool_call_id == "tk")
            && e.agent_id.is_none()
    ));
    assert!(envs.iter().any(
        |e| matches!(&e.event, Event::SubagentStopped(i) if i.reason.as_deref() == Some("finished"))
    ));
    check_stream(&envs);
}

#[tokio::test]
async fn subagent_mode_is_the_stricter_of_parent_and_definition() {
    let g = Probe::new("Grep", SideEffectClass::Read, 0, "g");
    let mut rig = Rig::build(
        Opts {
            mode: PermissionMode::Default,
            tools: ToolBox::new(Registry::builtin()).with_tool(g.clone()),
            ..Opts::default()
        },
        vec![
            task_call("t1", "writer"), // asks bypass under a default parent
            Turn::tool("g1", "Grep", json!({})),
            Turn::text("c1"),
            task_call("t2", "planner"), // asks plan: stricter, honoured
            Turn::tool("g2", "Grep", json!({})),
            Turn::text("c2"),
            Turn::text("end"),
        ],
        |h, _| h.with_agents(agents()),
    );
    rig.run("x").await;
    assert_eq!(
        *g.modes.lock().unwrap(),
        vec![PermissionMode::Default, PermissionMode::Plan]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn subagent_tool_list_unknown_type_depth_and_stop_hook() {
    let mut rig = Rig::build(
        Opts {
            mode: PermissionMode::AcceptEdits,
            tools: ToolBox::new(Registry::builtin()),
            ..Opts::default()
        },
        vec![
            task_call("a", "reader"),
            Turn::tool("w", "Write", json!({"file_path": "y.txt", "content": "c"})), // not in the reader's list
            Turn::text("reader done"),
            task_call("b", "does-not-exist"),
            task_call("n0", "nest"),
            task_call("n1", "nest"), // listed in the definition, but a child never gets Task
            Turn::text("n1 refused"),
            Turn::text("n0 done"),
        ],
        |h, r| {
            let hooks = synthesis_hooks::Hooks::new(
                r.rt.clone(),
                "s1",
                r.ws.clone(),
                serde_json::from_value(json!({"SubagentStop": [{"matcher": "reader", "hooks": [{"type": "command", "command": "touch \"$SYNT_PROJECT_DIR/stopped.marker\""}]}]})).unwrap(),
            );
            h.with_agents(agents()).with_hooks(Arc::new(hooks))
        },
    );
    let out = rig.run("x").await;
    assert_eq!(out.final_text, "n0 done");
    let reqs = rig.requests();
    assert!(
        tool_result(&reqs[2], "w")
            .unwrap()
            .0
            .contains("Unknown tool")
    );
    assert!(!rig.work().join("y.txt").exists());
    assert!(
        rig.work().join("stopped.marker").exists(),
        "SubagentStop hook ran"
    );
    assert!(
        tool_result(&reqs[4], "b")
            .unwrap()
            .0
            .contains("unknown subagent type")
    );
    assert!(
        tool_result(&reqs[6], "n1")
            .unwrap()
            .0
            .contains("Unknown tool"),
        "M11: Task is removed from a child even when its definition lists it"
    );
    check_stream(&rig.events());
}

// ---- interface tools ----------------------------------------------------------------------

fn question() -> serde_json::Value {
    json!({"questions": [{"question": "Which?", "header": "h", "multiSelect": false, "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]}]})
}

struct Answering(UserReply);
impl InputProvider for Answering {
    fn ask<'a>(
        &'a self,
        _: &'a NeedsUser,
        _: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        let r = self.0.clone();
        Box::pin(async move { Some(r) })
    }
}

#[tokio::test]
async fn unanswerable_question_blocks_the_task() {
    let mut rig = Rig::new(
        Opts {
            tools: ToolBox::new(Registry::builtin()),
            ..Opts::default()
        },
        vec![
            Turn::tool("q", "AskUserQuestion", question()),
            Turn::text("I cannot continue without your answer."),
        ],
    );
    let out = rig.run("x").await;
    assert_eq!(
        (out.status, out.reason.as_str()),
        (TaskStatus::Blocked, "needs_user_input")
    );
    let (text, is_err) = tool_result(&rig.requests()[1], "q").unwrap();
    assert!(is_err && text.contains("No user is available"), "{text}");
    let ev = rig.find(|e| match e {
        Event::ToolApprovalRequired {
            dialog, options, ..
        } => Some((dialog.clone(), options.clone())),
        _ => None,
    });
    assert_eq!(
        ev,
        vec![(
            "question".to_string(),
            vec!["A".to_string(), "B".to_string()]
        )]
    );
    let r = rig.find(|e| match e {
        Event::ToolApprovalResolved {
            decision, reviewer, ..
        } => Some((decision.clone(), reviewer.clone())),
        _ => None,
    });
    assert_eq!(r, vec![("auto_denied".to_string(), "headless".to_string())]);
}

#[tokio::test]
async fn answered_question_reaches_the_model() {
    let mut rig = Rig::build(
        Opts {
            tools: ToolBox::new(Registry::builtin()),
            ..Opts::default()
        },
        vec![
            Turn::tool("q", "AskUserQuestion", question()),
            Turn::text("Using A."),
        ],
        |h, _| {
            h.with_input(Arc::new(Answering(UserReply::Answers(
                json!({"Which?": "A"}),
            ))))
        },
    );
    let out = rig.run("x").await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", reason_of(&out));
    let (text, is_err) = tool_result(&rig.requests()[1], "q").unwrap();
    assert!(!is_err && text.contains("\"Which?\":\"A\""), "{text}");
    assert!(
        rig.transitions()
            .contains(&("running".into(), "waiting_approval".into()))
    );
}

#[tokio::test]
async fn exit_plan_mode_changes_the_mode_only_when_the_user_approves() {
    for (approved, want) in [
        (true, PermissionMode::Default),
        (false, PermissionMode::Plan),
    ] {
        let g = Probe::new("Grep", SideEffectClass::Read, 0, "g");
        let reply = UserReply::Plan {
            approved,
            message: None,
            mode: None,
        };
        let mut rig = Rig::build(
            Opts {
                mode: PermissionMode::Plan,
                tools: ToolBox::new(Registry::builtin()).with_tool(g.clone()),
                ..Opts::default()
            },
            vec![
                Turn::tool("p", "ExitPlanMode", json!({"plan": "1. do it"})),
                Turn::tool("g", "Grep", json!({})),
                Turn::text("ok"),
            ],
            move |h, _| h.with_input(Arc::new(Answering(reply))),
        );
        rig.run("x").await;
        assert_eq!(*g.modes.lock().unwrap(), vec![want], "approved={approved}");
        assert_eq!(tool_result(&rig.requests()[1], "p").unwrap().1, !approved);
    }
}

// ---- state machine ------------------------------------------------------------------------

#[tokio::test]
async fn session_state_machine_is_enforced() {
    let mut rig = Rig::new(Opts::default(), vec![Turn::text("one"), Turn::text("two")]);
    let first = rig.run("a").await;
    assert_eq!(rig.session.status, SessionStatus::Completed);

    // a finished task leaves the session idle for the next one
    rig.harness.to_idle(&mut rig.session).unwrap();
    assert_eq!(rig.session.status, SessionStatus::Idle);
    let mut req = TaskRequest::new("b");
    req.history = first.messages.clone();
    let second = rig
        .harness
        .run_task(&mut rig.session, req, CancellationToken::new())
        .await
        .unwrap();
    assert_ne!(first.task_id, second.task_id);
    assert_eq!(rig.requests()[1].messages.len(), 3, "history + new prompt");

    // not allowed from a running session
    rig.session.status = SessionStatus::Running;
    assert!(
        rig.harness
            .run_task(
                &mut rig.session,
                TaskRequest::new("c"),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        rig.harness
            .start_session(&mut rig.session, "startup")
            .is_err()
    );
}

#[tokio::test]
async fn bare_deny_hides_the_tool() {
    let mut rig = Rig::new(
        Opts {
            tools: ToolBox::new(Registry::builtin()),
            deny: vec!["Bash".into(), "Write(secret.txt)".into()],
            ..Opts::default()
        },
        vec![Turn::text("done")],
    );
    rig.run("hi").await;
    let names: Vec<_> = rig.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(!names.contains(&"Bash".to_string()), "{names:?}");
    assert!(
        names.contains(&"Write".to_string()),
        "a scoped deny keeps the tool: {names:?}"
    );
    assert!(names.contains(&"Read".to_string()));
}

// ---- sub-agent frontmatter `model` and `maxTurns` -----------------------------------------

fn limited_agents() -> AgentTable {
    let def = |name: &str, model: Option<&str>, max_turns: Option<u32>| AgentDef {
        name: name.into(),
        description: "".into(),
        prompt: "".into(),
        tools: Some(vec!["Grep".into()]),
        mode: None,
        model: model.map(String::from),
        max_turns,
    };
    AgentTable::new([
        def("cheap", Some("small-model"), None),
        def("alien", Some("other/huge"), None),
        def("brief", None, Some(1)),
    ])
}

fn resolver() -> synthesis_harness::ModelResolver {
    Arc::new(|m| match m.split_once('/') {
        Some((p, _)) => Err(format!("it belongs to provider \"{p}\"")),
        None => Ok(m.to_string()),
    })
}

fn limited_rig(script: Vec<Turn>) -> Rig {
    let probe = Probe::new("Grep", SideEffectClass::Read, 0, "g");
    Rig::build(
        Opts {
            tools: ToolBox::new(Registry::builtin()).with_tool(probe),
            ..Opts::default()
        },
        script,
        |h, _| {
            h.with_agents(limited_agents())
                .with_model_resolver(resolver())
        },
    )
}

#[tokio::test]
async fn subagent_model_is_used_when_the_provider_resolves_it() {
    let mut rig = limited_rig(vec![
        task_call("tk", "cheap"),
        Turn::text("child done"),
        Turn::text("parent done"),
    ]);
    let out = rig.run("delegate").await;
    assert_eq!(out.final_text, "parent done");
    let reqs = rig.requests();
    assert_eq!(reqs[0].model, "fake-model");
    assert_eq!(reqs[1].model, "small-model", "the child's own model");
    assert_eq!(reqs[2].model, "fake-model", "the parent is unchanged");
}

#[tokio::test]
async fn subagent_model_of_another_provider_is_refused_with_a_notice() {
    let mut rig = limited_rig(vec![
        task_call("tk", "alien"),
        Turn::text("child done"),
        Turn::text("parent done"),
    ]);
    rig.run("delegate").await;
    let reqs = rig.requests();
    assert_eq!(reqs[1].model, "fake-model");
    assert!(
        rig.events().iter().any(|e| matches!(&e.event,
            Event::Notice { text, .. } if text.contains("other/huge") && text.contains("ignored"))),
        "a notice names the refused model"
    );
}

#[tokio::test]
async fn subagent_max_turns_stops_that_agent_only() {
    // the child would make a second request after its tool round; maxTurns 1 ends it first
    let mut rig = limited_rig(vec![
        task_call("tk", "brief"),
        Turn::tool("g1", "Grep", json!({"pattern": "x"})),
        Turn::text("never reached by the child"),
        Turn::text("parent done"),
    ]);
    let out = rig.run("delegate").await;
    let reqs = rig.requests();
    assert_eq!(reqs.len(), 3, "parent, one child request, parent again");
    let (task_res, _) = tool_result(&reqs[2], "tk").unwrap();
    assert!(task_res.contains("agent_max_turns"), "{task_res}");
    assert_eq!(out.status, TaskStatus::Completed, "the task itself goes on");
}
