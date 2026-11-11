//! Proactive / manual / reactive compaction and the `context.*` events (FakeProvider, no network).
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::json;
use synthesis_context::{
    CharsPerToken, CompactConfig, SummarizeError, Summarizer, TaskState, compact, validate_pairing,
};
use synthesis_events::{ArtifactRef, Event, TaskStatus};
use synthesis_harness::testing::Turn;
use synthesis_harness::{BoxFut, Built, Compaction, ContextProvider, StaticContext, ToolBox};
use synthesis_providers::Message;
use synthesis_tools::SideEffectClass;
use tokio_util::sync::CancellationToken;

/// Fake summarizer: counts calls, remembers prompts, optionally fails.
struct Sum {
    calls: AtomicUsize,
    fail: bool,
    text: String,
    prompts: Mutex<Vec<String>>,
}

impl Sum {
    fn new(fail: bool, text: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail,
            text: text.into(),
            prompts: Mutex::default(),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Summarizer for Sum {
    async fn summarize(&self, _: &str, prompt: &str) -> Result<String, SummarizeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.into());
        if self.fail {
            Err(SummarizeError(
                "boom sk-test-TOPSECRETVALUE0123456789".into(),
            ))
        } else {
            Ok(self.text.clone())
        }
    }
}

/// `synthesis-context`'s real `compact` behind the harness trait.
struct Real {
    sum: Arc<Sum>,
    keep: u64,
}

impl ContextProvider for Real {
    fn build(&self, history: &[Message], p: Option<&str>) -> Result<Built, String> {
        StaticContext::default().build(history, p)
    }
    fn compact<'a>(
        &'a self,
        history: &'a [Message],
        instructions: &'a str,
    ) -> BoxFut<'a, Result<Compaction, String>> {
        Box::pin(async move {
            let cfg = CompactConfig {
                keep_recent_tokens: self.keep,
                ..CompactConfig::default()
            };
            let messages = compact(
                &CharsPerToken::default(),
                &*self.sum,
                history,
                &TaskState::default(),
                instructions,
                &cfg,
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
            Ok(Compaction {
                messages,
                summary_ref: ArtifactRef {
                    artifact_id: "a".into(),
                    path: "s.txt".into(),
                    size: 1,
                    sha256: "0".into(),
                    mime: "text/plain".into(),
                },
                tokens_before: None,
                summary_chars: self.sum.text.chars().count() as u64,
            })
        })
    }
}

const BIG: u64 = 190_000; // >= 85% of the default 200k window
const SMALL: u64 = 3_000;

fn tool(i: usize, input: u64) -> Turn {
    Turn::tool(&format!("t{i}"), "Probe", json!({"x": i.to_string()})).with_usage(input, 10)
}

fn opts(p: &Arc<Probe>) -> Opts {
    Opts {
        tools: ToolBox::empty().with_tool(p.clone()),
        allow: vec!["Probe".into()],
        context_window: Some(200_000),
        ..Opts::default()
    }
}

fn rig(turns: Vec<Turn>, sum: &Arc<Sum>, keep: u64, out: &str) -> Rig {
    let p = Probe::new("Probe", SideEffectClass::Read, 0, out);
    let sum = sum.clone();
    Rig::build(opts(&p), turns, move |h, _| {
        h.with_context(Arc::new(Real { sum, keep }))
    })
}

fn long_request() -> String {
    "please refactor the module ".repeat(30)
}

/// `(type, reason)` of the context events, in order.
fn ctx_events(r: &Rig) -> Vec<String> {
    r.find(|e| match e {
        Event::ContextUsage { source, stale, .. } => Some(format!("usage:{source}:{stale}")),
        Event::ContextCompacting { reason, .. } => Some(format!("compacting:{reason}")),
        Event::ContextCompacted { reason, .. } => Some(format!("compacted:{reason}")),
        Event::ContextCompactionFailed { reason, .. } => Some(format!("failed:{reason}")),
        _ => None,
    })
}

#[tokio::test]
async fn threshold_crossing_compacts_once_before_the_request_and_refreshes_usage() {
    let sum = Sum::new(false, "short summary");
    let mut r = rig(
        vec![
            tool(0, BIG),
            tool(1, SMALL),
            Turn::text("done").with_usage(SMALL, 5),
        ],
        &sum,
        20,
        "r",
    );
    let out = r.run(&long_request()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert_eq!(sum.calls(), 1, "exactly one compaction");
    assert_eq!(
        ctx_events(&r),
        [
            "usage:provider:false", // response 1: 190k
            "compacting:auto",
            "compacted:auto",
            "usage:estimate:false", // right after: the POST-compaction number
            "usage:provider:false", // response 2 replaces it
            "usage:provider:false",
        ]
    );
    let reqs = r.requests();
    assert_eq!(reqs.len(), 3);
    assert!(
        user_text(&reqs[1].messages[0]).starts_with("Summary of the conversation so far"),
        "the request that would overflow already carries the summary"
    );
    assert!(user_text(&reqs[0].messages[0]).starts_with("please refactor"));

    let (usage, compacted) = (
        r.find(|e| match e {
            Event::ContextUsage {
                used_tokens,
                window_tokens,
                percent,
                ..
            } => Some((*used_tokens, *window_tokens, *percent)),
            _ => None,
        }),
        r.find(|e| match e {
            Event::ContextCompacted {
                before_tokens,
                after_tokens,
                window_tokens,
                messages_before,
                messages_after,
                summary_chars,
                trigger,
                ..
            } => Some((
                *before_tokens,
                *after_tokens,
                *window_tokens,
                *messages_before,
                *messages_after,
                *summary_chars,
                trigger.clone(),
            )),
            _ => None,
        }),
    );
    let (before, after, window, mb, ma, chars, trigger) = compacted[0].clone();
    assert!(before >= 170_000 && after < before, "{before} -> {after}");
    assert_eq!((window, chars, trigger.as_str()), (200_000, 13, "auto"));
    assert!(ma <= mb && ma >= 1, "{mb} -> {ma} messages");
    assert_eq!(usage[0], (BIG, 200_000, 95.0));
    assert_eq!(
        usage[1].0, after,
        "first usage after compaction is the new estimate"
    );
    assert!(usage[1].0 < usage[0].0);
    assert_eq!(usage[2].0, SMALL);
    check_stream(&r.events());
}

#[tokio::test]
async fn pairing_stays_valid_when_compaction_cuts_inside_a_tool_loop() {
    for keep in [0, 5, 20, 60, 200, 1000] {
        for at in 0..4 {
            let sum = Sum::new(false, "s");
            let mut turns: Vec<Turn> = (0..5)
                .map(|i| tool(i, if i == at { BIG } else { SMALL }))
                .collect();
            turns.push(Turn::text("done"));
            let mut r = rig(turns, &sum, keep, "result text");
            let out = r.run(&long_request()).await;
            assert_eq!(
                out.status,
                TaskStatus::Completed,
                "keep {keep} at {at}: {}",
                out.reason
            );
            for (i, req) in r.requests().iter().enumerate() {
                validate_pairing(&req.messages)
                    .unwrap_or_else(|e| panic!("keep {keep} at {at} request {i}: {e}"));
            }
            validate_pairing(&out.messages).unwrap();
        }
    }
}

#[tokio::test]
async fn summarizer_failure_continues_uncompacted_and_backs_off() {
    let sum = Sum::new(true, "");
    let mut turns: Vec<Turn> = (0..5).map(|i| tool(i, BIG)).collect();
    turns.push(Turn::text("done"));
    let mut r = rig(turns, &sum, 20, "r");
    let out = r.run(&long_request()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    // 6 requests; attempts before #2 and #5 (pause of 2 then the retry)
    assert_eq!(sum.calls(), 2);
    let ev = ctx_events(&r);
    assert_eq!(
        ev.iter().filter(|e| *e == "failed:auto").count(),
        2,
        "{ev:?}"
    );
    assert!(!ev.iter().any(|e| e.starts_with("compacted")));
    let errs = r.find(|e| match e {
        Event::ContextCompactionFailed { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert!(
        errs[0].contains("boom") && !errs[0].contains("TOPSECRET"),
        "redacted: {}",
        errs[0]
    );
    assert!(
        user_text(&r.requests()[1].messages[0]).starts_with("please refactor"),
        "history untouched"
    );
}

#[tokio::test]
async fn low_gain_pauses_auto_compact_and_warns() {
    // Window 10k tokens. The head is the user request; the tail keeps a 10k-token tool result,
    // so a 500-token summary frees ~4%: auto-compact must pause instead of looping.
    let sum = Sum::new(false, &"y".repeat(2000));
    // 1_400 = the input size of request 1: it puts the first compaction at request 2 (the system
    // rules grew by the <agent_message> sentence; the number tracks their size).
    let mut turns = vec![Turn::tool("t0", "Probe", json!({"x": "0"})).with_usage(1_500, 10)];
    turns.extend((1..4).map(|i| tool(i, 10_800 + 10_000 * i as u64)));
    turns.push(Turn::text("done"));
    let p = Probe::new("Probe", SideEffectClass::Read, 0, &"z".repeat(40_000));
    let mut o = opts(&p);
    o.context_window = Some(10_000);
    let s2 = sum.clone();
    let mut r = Rig::build(o, turns, move |h, _| {
        h.with_context(Arc::new(Real {
            sum: s2,
            keep: 10_500,
        }))
    });
    let out = r.run(&"q".repeat(4000)).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert_eq!(
        sum.calls(),
        1,
        "no loop: paused after the first low-gain compaction"
    );
    let warns = r.find(|e| match e {
        Event::Notice { level, text } if level == "warn" && text.contains("auto-compact") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert_eq!(warns.len(), 1, "{:?}", r.types());
}

#[tokio::test]
async fn disabled_setting_and_static_context_never_compact() {
    let sum = Sum::new(false, "s");
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = opts(&p);
    o.config = |c| c.auto_compact.enabled = false;
    let s2 = sum.clone();
    let mut r = Rig::build(
        o,
        vec![tool(0, BIG), tool(1, BIG), Turn::text("done")],
        move |h, _| h.with_context(Arc::new(Real { sum: s2, keep: 20 })),
    );
    r.run(&long_request()).await;
    assert_eq!(sum.calls(), 0);
    assert!(
        ctx_events(&r).iter().all(|e| e.starts_with("usage")),
        "{:?}",
        ctx_events(&r)
    );

    // StaticContext cannot compact: no doomed attempts, no failure events.
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut r = Rig::new(
        opts(&p),
        vec![tool(0, BIG), tool(1, BIG), Turn::text("done")],
    );
    r.run(&long_request()).await;
    assert!(ctx_events(&r).iter().all(|e| e.starts_with("usage")));
}

#[tokio::test]
async fn threshold_setting_moves_the_trigger() {
    let sum = Sum::new(false, "s");
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let mut o = opts(&p);
    o.config = |c| c.auto_compact.threshold = 0.5;
    let s2 = sum.clone();
    let mut r = Rig::build(
        o,
        vec![tool(0, 110_000), Turn::text("done")],
        move |h, _| h.with_context(Arc::new(Real { sum: s2, keep: 20 })),
    );
    r.run(&long_request()).await;
    assert_eq!(sum.calls(), 1, "55% >= 50%");
}

#[tokio::test]
async fn manual_request_runs_at_the_next_safe_point_and_compact_now_between_tasks() {
    let sum = Sum::new(false, "manual summary");
    let mut r = rig(vec![Turn::text("ok"), Turn::text("ok2")], &sum, 20, "r");
    r.harness
        .request_compact(Some("focus on the parser".into()));
    let out = r.run(&long_request()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert_eq!(sum.calls(), 1);
    assert!(sum.prompts.lock().unwrap()[0].contains("focus on the parser"));
    assert_eq!(
        ctx_events(&r)[..3],
        [
            "compacting:manual",
            "compacted:manual",
            "usage:estimate:false"
        ]
    );
    assert!(user_text(&r.requests()[0].messages[0]).starts_with("Summary of the conversation"));

    // between tasks
    let history = vec![
        synthesis_providers::Message {
            role: synthesis_providers::Role::User,
            content: synthesis_providers::Content::Text(long_request()),
        },
        synthesis_providers::Message {
            role: synthesis_providers::Role::Assistant,
            content: synthesis_providers::Content::Text("ok".into()),
        },
    ];
    let n = ctx_events(&r).len();
    let new = r
        .harness
        .compact_now(&r.session, history, None, CancellationToken::new())
        .await
        .unwrap();
    assert!(user_text(&new[0]).starts_with("Summary of the conversation"));
    let ev = ctx_events(&r);
    assert_eq!(
        ev[n..],
        [
            "compacting:manual",
            "compacted:manual",
            "usage:estimate:false"
        ]
    );

    // failure leaves the history alone and says so
    let bad = Sum::new(true, "");
    let r2 = rig(vec![], &bad, 20, "r");
    let h = vec![synthesis_providers::Message {
        role: synthesis_providers::Role::User,
        content: synthesis_providers::Content::Text(long_request()),
    }];
    let e = r2
        .harness
        .compact_now(&r2.session, h, None, CancellationToken::new())
        .await;
    assert!(e.is_err());
    assert_eq!(ctx_events(&r2), ["compacting:manual", "failed:manual"]);
}

#[tokio::test]
async fn reactive_overflow_still_works_when_proactive_did_not_fire() {
    let sum = Sum::new(false, "overflow summary");
    let mut r = rig(
        vec![tool(0, SMALL), Turn::context_overflow(), Turn::text("fits")],
        &sum,
        20,
        "r",
    );
    let out = r.run(&long_request()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert_eq!(sum.calls(), 1);
    let ev = ctx_events(&r);
    assert!(
        ev.windows(3).any(|w| w
            == [
                "compacting:overflow",
                "compacted:overflow",
                "usage:estimate:false"
            ]),
        "{ev:?}"
    );
    let trigger = r.find(|e| match e {
        Event::ContextCompacted { trigger, .. } => Some(trigger.clone()),
        _ => None,
    });
    assert_eq!(trigger, ["auto"], "legacy field keeps its two values");
}

#[tokio::test]
async fn response_without_usage_gives_a_stale_estimate() {
    use synthesis_harness::testing::Step;
    use synthesis_providers::StreamEvent;
    let mut t = Turn::text("hi");
    for s in &mut t.steps {
        if let Step::Event(StreamEvent::MessageStart { message }) = s {
            message.usage.input_tokens = None;
        }
    }
    let mut r = Rig::new(Opts::default(), vec![t]);
    r.run("hello").await;
    assert_eq!(ctx_events(&r), ["usage:estimate:true"]);
}

/// The same 190k request on a 200k model and on a 1M model (what `/model` switches between):
/// the threshold and the reported percent follow the window of the model in use.
#[tokio::test]
async fn the_window_of_the_current_model_sets_the_threshold_and_the_percent() {
    let mut seen = vec![];
    for window in [200_000, 1_000_000] {
        let sum = Sum::new(false, "short summary");
        let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
        let o = Opts {
            context_window: Some(window),
            ..opts(&p)
        };
        let turns = vec![tool(0, BIG), tool(1, SMALL), Turn::text("done")];
        let s2 = sum.clone();
        let mut r = Rig::build(o, turns, move |h, _| {
            h.with_context(Arc::new(Real { sum: s2, keep: 20 }))
        });
        let out = r.run(&long_request()).await;
        assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
        let first = r.find(|e| match e {
            Event::ContextUsage {
                window_tokens,
                percent,
                ..
            } => Some((*window_tokens, *percent)),
            _ => None,
        })[0];
        seen.push((sum.calls(), first));
    }
    assert_eq!(seen[0], (1, (200_000, 95.0)), "190k of 200k compacts");
    assert_eq!(seen[1], (0, (1_000_000, 19.0)), "190k of 1M does not");
}

#[tokio::test]
async fn an_unknown_window_is_reported_as_zero_and_compacts_against_200k_with_one_notice() {
    let sum = Sum::new(false, "short summary");
    let p = Probe::new("Probe", SideEffectClass::Read, 0, "r");
    let o = Opts {
        context_window: None,
        ..opts(&p)
    };
    let turns = vec![tool(0, BIG), tool(1, SMALL), Turn::text("done")];
    let s2 = sum.clone();
    let mut r = Rig::build(o, turns, move |h, _| {
        h.with_context(Arc::new(Real { sum: s2, keep: 20 }))
    });
    let out = r.run(&long_request()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    assert_eq!(sum.calls(), 1, "the 200k fallback still compacts");
    let windows = r.find(|e| match e {
        Event::ContextUsage {
            window_tokens,
            percent,
            ..
        } => Some((*window_tokens, *percent)),
        _ => None,
    });
    assert!(windows.iter().all(|w| *w == (0, 0.0)), "{windows:?}");
    let notes = r.find(|e| match e {
        Event::Notice { text, .. } if text.contains("context window unknown") => Some(text.clone()),
        _ => None,
    });
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("assuming 200k; set limit.context"));
}

fn shot_msg(label: &str, b64_len: usize) -> Message {
    use synthesis_providers::{Block, Content, ImageSource, Role};
    Message {
        role: Role::User,
        content: Content::Blocks(vec![
            Block::Text { text: label.into() },
            Block::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "A".repeat(b64_len),
                },
            },
        ]),
    }
}

fn image_count(m: &Message) -> usize {
    use synthesis_providers::{Block, Content};
    match &m.content {
        Content::Blocks(b) => b
            .iter()
            .filter(|b| matches!(b, Block::Image { .. }))
            .count(),
        Content::Text(_) => 0,
    }
}

#[tokio::test]
async fn older_images_leave_the_request_but_not_the_history() {
    let mut r = Rig::new(Opts::default(), vec![Turn::text("seen")]);
    let mut req = synthesis_harness::TaskRequest::new("and now?");
    // 4 screenshots of 1.5 MB each (decoded): the oldest two exceed the keep-2 policy
    req.history = (0..4)
        .map(|i| shot_msg(&format!("shot{i}"), 2_000_000))
        .collect();
    let out = r.run_with(req, CancellationToken::new()).await;
    assert_eq!(out.status, TaskStatus::Completed, "{}", out.reason);
    let sent = &r.requests()[0].messages;
    assert_eq!(
        sent.iter().map(image_count).collect::<Vec<_>>(),
        [0, 0, 1, 1, 0]
    );
    assert!(user_text(&sent[0]).contains("[image: 1, removed to save space]"));
    assert_eq!(
        out.messages.iter().map(image_count).sum::<usize>(),
        4,
        "the stored history keeps every image"
    );
    let notes = r.find(|e| match e {
        Event::Notice { text, .. } if text.contains("older screenshots omitted") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert_eq!(notes.len(), 1, "{notes:?}");
}

#[tokio::test]
async fn overflow_compacts_again_but_only_a_bounded_number_of_times() {
    let sum = Sum::new(false, "s");
    let turns = |n: usize| {
        let mut t = vec![tool(0, SMALL), tool(1, SMALL), tool(2, SMALL)];
        t.extend((0..n).map(|_| Turn::context_overflow()));
        t.push(Turn::text("fits"));
        t
    };
    let mut r = rig(turns(9), &sum, 20, "r");
    let out = r.run(&long_request()).await;
    assert_ne!(out.status, TaskStatus::Completed);
    let n = ctx_events(&r)
        .iter()
        .filter(|e| *e == "compacting:overflow")
        .count();
    assert!(n <= 3, "{n}");
}
