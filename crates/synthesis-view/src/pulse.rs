//! Live activity of the Topology nodes, from event timestamps and a `now` argument (pure, no clock).
//! Every event either touches a node (a one-shot pulse, or a call that stays open until its end event)
//! or is explicitly ignored: [`touch_of`] has no wildcard arm, so a new event type will not compile
//! until it is placed. A node is `Request` while a call is in flight (data flows core -> node; the
//! prompt flows user -> core) and `Result` for [`HOLD_MS`] after the last event (the opposite way, fading).

use std::collections::BTreeMap;

use serde_json::Value;
use synthesis_events::{Envelope, Event};

use crate::model::{classify_tool, parse_ts};

/// The nodes around the core, and the core itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Node {
    Core,
    User,
    Hooks,
    Guards,
    Subagents,
    Skills,
    Mcp,
    Tools,
    Memory,
}

/// An activity fades this long after the last event of a node.
pub const HOLD_MS: i64 = 1200;
/// A one-shot event shows as a request this long before it turns into a result.
pub const REQUEST_MS: i64 = 450;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Request,
    Result,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Act {
    pub phase: Phase,
    /// 1.0 while a call is open or just started, down to 0.0 at the end of the hold.
    pub strength: f32,
}

impl Act {
    pub const IDLE: Act = Act {
        phase: Phase::Idle,
        strength: 0.,
    };
    pub fn on(&self) -> bool {
        self.phase != Phase::Idle
    }
}

/// Does data flow toward the core in this phase? The prompt goes user -> core; every other node is
/// asked by the core (request) and answers it (result).
pub fn toward_core(n: Node, p: Phase) -> bool {
    (n == Node::User) == (p == Phase::Request)
}

/// What one event does to the nodes.
#[derive(Debug, PartialEq)]
pub enum Touch {
    /// A call starts and stays open until the `Close` of the same key.
    Open(Node, String),
    Close(String),
    Pulse(Node),
    /// The task or session ended: nothing is in flight any more.
    Reset,
    Ignore,
}

fn tool_node(name: &str, input: &Value) -> Node {
    if name == "Agent" || name == "Task" {
        Node::Subagents
    } else if name == "Skill" {
        Node::Skills
    } else if name.starts_with("mcp__") {
        Node::Mcp
    } else if classify_tool(name, input) == "learn" {
        Node::Memory
    } else {
        Node::Tools
    }
}

fn is_guard_hook(event: &str) -> bool {
    ["PreToolUse", "UserPromptSubmit", "PermissionRequest"]
        .iter()
        .any(|p| event.starts_with(p))
}

/// The node(s) an event touches. Exhaustive on purpose.
pub fn touch_of(e: &Event) -> Touch {
    use Event::*;
    use Node::*;
    let open = |n: Node, k: &str, id: &str| Touch::Open(n, format!("{k}:{id}"));
    let close = |k: &str, id: &str| Touch::Close(format!("{k}:{id}"));
    match e {
        UserMessage { .. } => Touch::Pulse(User),
        TurnStarted { .. } => open(Core, "turn", ""),
        TurnCompleted { .. } => close("turn", ""),
        ModelDelta { .. }
        | AssistantMessage { .. }
        | ModelRetry { .. }
        | ModelError { .. }
        | TodoUpdated { .. } => Touch::Pulse(Core),
        ContextCompacting { .. } => open(Core, "compact", ""),
        ContextCompacted { .. } | ContextCompactionFailed { .. } => close("compact", ""),
        ToolRequested {
            tool_call_id,
            name,
            input,
            ..
        } => open(tool_node(name, input), "tool", tool_call_id),
        // `tool.requested` normally came first; a call seen only from its start is classified by name
        ToolStarted {
            tool_call_id, name, ..
        } => open(tool_node(name, &Value::Null), "tool", tool_call_id),
        ToolCompleted { tool_call_id, .. } => close("tool", tool_call_id),
        ToolOutput { .. }
        | ProcessSpawned(_)
        | ProcessReady(_)
        | ProcessExited(_)
        | ProcessOutput { .. }
        | BrowserLoginRequired { .. }
        | BrowserAction { .. }
        | BrowserFrame { .. } => Touch::Pulse(Tools),
        ToolApprovalRequired { approval_id, .. } => open(Guards, "approval", approval_id),
        ToolApprovalResolved { approval_id, .. } => close("approval", approval_id),
        AutoVerdict { .. } | SessionModeChanged { .. } | BudgetExceeded { .. } => {
            Touch::Pulse(Guards)
        }
        HookRan {
            event, hook_name, ..
        } => Touch::Pulse(if is_guard_hook(event) || is_guard_hook(hook_name) {
            Guards
        } else {
            Hooks
        }),
        VerificationCheckStarted { check_id, .. } => open(Memory, "check", check_id),
        VerificationCheckFinished { check_id, .. } => close("check", check_id),
        VerificationCompleted { .. }
        | MemoryRetrieved { .. }
        | MemoryWritten { .. }
        | MemoryQuarantined { .. }
        | MemoryProposalCreated { .. } => Touch::Pulse(Memory),
        MemoryReflectionStarted { job_id } => open(Memory, "job", job_id),
        MemoryReflectionCompleted { job_id, .. } | MemoryReflectionFailed { job_id, .. } => {
            close("job", job_id)
        }
        SubagentStarted(i) => open(Subagents, "agent", &i.agent_id),
        SubagentStopped(i) => close("agent", &i.agent_id),
        SubagentStatusChanged { .. }
        | MessageSent { .. }
        | MessageDelivered { .. }
        | MessageDropped { .. }
        | PeerListening { .. }
        | PeerStopped { .. }
        | PeerRequestSent { .. }
        | PeerRequestReceived { .. }
        | PeerReplyPartial { .. }
        | PeerReply { .. }
        | PeerThreadClosed { .. }
        | PeerDelegationAccepted { .. }
        | PeerDelegationTool { .. }
        | PeerDelegationDenied { .. } => Touch::Pulse(Subagents),
        PeerDelegationStarted { delegation_id, .. } => open(Subagents, "deleg", delegation_id),
        PeerDelegationCompleted { delegation_id, .. } => close("deleg", delegation_id),
        TaskEnded { .. } | SessionCompleted(_) | SessionFailed(_) | SessionCancelled(_) => {
            Touch::Reset
        }
        // bookkeeping with no component behind it
        SessionStarted { .. }
        | SessionStatusChanged { .. }
        | TaskStarted { .. }
        | ContextUsage { .. }
        | ArtifactCreated { .. }
        | ModelChanged { .. }
        | Notice { .. } => Touch::Ignore,
    }
}

#[derive(Clone, Debug, Default)]
pub struct Pulses {
    /// Calls in flight: key -> node.
    open: BTreeMap<String, Node>,
    /// Per node: first and last event (ms).
    seen: BTreeMap<Node, (i64, i64)>,
}

impl Pulses {
    pub fn apply(&mut self, env: &Envelope) {
        let Some(ts) = parse_ts(&env.ts) else { return };
        match touch_of(&env.event) {
            Touch::Open(n, k) => {
                // `tool.started` after `tool.requested` keeps the first classification
                self.open.entry(k).or_insert(n);
                self.seen.insert(n, (ts, ts));
            }
            Touch::Close(k) => {
                if let Some(n) = self.open.remove(&k) {
                    self.seen.entry(n).and_modify(|s| s.1 = ts);
                }
            }
            Touch::Pulse(n) => {
                self.seen.insert(n, (ts, ts));
            }
            Touch::Reset => {
                for (_, n) in std::mem::take(&mut self.open) {
                    self.seen.entry(n).and_modify(|s| s.1 = ts);
                }
            }
            Touch::Ignore => {}
        }
    }

    /// Nothing in flight any more: events replayed from a store ended without their end event.
    pub fn settle(&mut self) {
        self.open.clear();
    }

    /// Holds `nodes` open (the `--active` shot).
    pub fn forced(nodes: &[Node]) -> Self {
        let mut p = Pulses::default();
        for n in nodes {
            p.open.insert(format!("forced:{n:?}"), *n);
            p.seen.insert(*n, (0, 0));
        }
        p
    }

    pub fn act(&self, n: Node, now: i64) -> Act {
        if self.open.values().any(|o| *o == n) {
            return Act {
                phase: Phase::Request,
                strength: 1.,
            };
        }
        let Some(&(start, end)) = self.seen.get(&n) else {
            return Act::IDLE;
        };
        let age = (now - end).max(0);
        if age >= HOLD_MS {
            return Act::IDLE;
        }
        Act {
            phase: if start == end && now - start < REQUEST_MS {
                Phase::Request
            } else {
                Phase::Result
            },
            strength: 1. - age as f32 / HOLD_MS as f32,
        }
    }

    pub fn acts(&self, now: i64) -> BTreeMap<Node, Act> {
        self.seen
            .keys()
            .chain(self.open.values())
            .map(|n| (*n, self.act(*n, now)))
            .filter(|(_, a)| a.on())
            .collect()
    }

    /// When every node is idle again, `i64::MAX` while a call is open.
    pub fn active_until(&self) -> i64 {
        if !self.open.is_empty() {
            return i64::MAX;
        }
        self.seen.values().map(|s| s.1 + HOLD_MS).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_700_000_000_000;

    fn ts(ms: i64) -> String {
        // RFC 3339 with milliseconds, as the event log writes it
        let secs = ms.div_euclid(1000);
        let d = chrono_free(secs);
        format!("{d}.{:03}Z", ms.rem_euclid(1000))
    }

    /// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SS` (civil-from-days).
    fn chrono_free(secs: i64) -> String {
        let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    fn env(at: i64, typ: &str, payload: Value) -> Envelope {
        serde_json::from_value(
            json!({"v":1,"seq":1,"ts":ts(at),"session_id":"s","type":typ,"payload":payload}),
        )
        .unwrap()
    }

    fn requested(at: i64, id: &str, name: &str) -> Envelope {
        env(
            at,
            "tool.requested",
            json!({"tool_call_id":id,"name":name,"input":{},"input_truncated":false}),
        )
    }

    fn completed(at: i64, id: &str, name: &str) -> Envelope {
        env(
            at,
            "tool.completed",
            json!({"tool_call_id":id,"name":name,"status":"ok","is_error":false,"not_run":false,
                   "duration_ms":1,"preview":"","bytes":0,"truncated":false}),
        )
    }

    fn on(p: &Pulses, now: i64) -> Vec<Node> {
        p.acts(now).keys().copied().collect()
    }

    #[test]
    fn a_call_is_a_request_while_open_then_a_result_that_fades() {
        let mut p = Pulses::default();
        p.apply(&requested(T0, "a", "mcp__fs__read"));
        assert_eq!(
            on(&p, T0 + 60_000),
            vec![Node::Mcp],
            "a long call stays active"
        );
        assert_eq!(p.act(Node::Mcp, T0 + 60_000).phase, Phase::Request);
        p.apply(&completed(T0 + 90_000, "a", "mcp__fs__read"));
        let end = T0 + 90_000;
        assert_eq!(p.act(Node::Mcp, end).phase, Phase::Result);
        assert!(p.act(Node::Mcp, end + 1000).on(), "t+1.0 s still on");
        assert!(p.act(Node::Mcp, end + 1000).strength < p.act(Node::Mcp, end + 100).strength);
        assert!(on(&p, end + 1500).is_empty(), "t+1.5 s idle");
        assert_eq!(p.active_until(), end + HOLD_MS);
    }

    #[test]
    fn concurrent_calls_keep_the_node_active_until_the_last_one_ends() {
        let mut p = Pulses::default();
        p.apply(&requested(T0, "a", "Bash"));
        p.apply(&requested(T0 + 100, "b", "Read"));
        p.apply(&completed(T0 + 200, "a", "Bash"));
        assert_eq!(p.act(Node::Tools, T0 + 10_000).phase, Phase::Request);
        p.apply(&completed(T0 + 10_000, "b", "Read"));
        assert_eq!(p.act(Node::Tools, T0 + 10_100).phase, Phase::Result);
    }

    #[test]
    fn events_light_their_own_component() {
        let cases = [
            (
                env(T0, "user.message", json!({"text":"hi","images":[]})),
                Node::User,
            ),
            (
                env(
                    T0,
                    "hook.ran",
                    json!({"event":"PostToolUse","hook_name":"fmt","outcome":"ok","duration_ms":1}),
                ),
                Node::Hooks,
            ),
            (
                env(
                    T0,
                    "hook.ran",
                    json!({"event":"PreToolUse","hook_name":"g","outcome":"ok","duration_ms":1}),
                ),
                Node::Guards,
            ),
            (
                env(
                    T0,
                    "permission.auto_verdict",
                    json!({"tool_call_id":"x","name":"Bash","verdict":"allow","reason":"","source":"model"}),
                ),
                Node::Guards,
            ),
            (requested(T0, "x", "Skill"), Node::Skills),
            (requested(T0, "x", "Agent"), Node::Subagents),
            (requested(T0, "x", "mcp__s__t"), Node::Mcp),
            (requested(T0, "x", "Edit"), Node::Tools),
            (
                env(
                    T0,
                    "memory.retrieved",
                    json!({"ids":[],"tokens":0,"candidates":0}),
                ),
                Node::Memory,
            ),
            (
                env(
                    T0,
                    "turn.started",
                    json!({"model":"m","provider":"p","iteration":1}),
                ),
                Node::Core,
            ),
            (
                env(
                    T0,
                    "model.delta",
                    json!({"block_index":0,"kind":"text","text":"x"}),
                ),
                Node::Core,
            ),
        ];
        for (e, n) in cases {
            let mut p = Pulses::default();
            p.apply(&e);
            assert_eq!(on(&p, T0 + 10), vec![n], "{:?}", e.event);
        }
    }

    #[test]
    fn approval_holds_the_guards_until_resolved_and_a_reset_closes_everything() {
        let mut p = Pulses::default();
        p.apply(&env(T0, "tool.approval_required", json!({"approval_id":"p","tool_call_id":"x","dialog":"d","prompt":"","options":[],"suggestions":[]})));
        assert_eq!(p.act(Node::Guards, T0 + 30_000).phase, Phase::Request);
        p.apply(&env(
            T0 + 30_000,
            "tool.approval_resolved",
            json!({"approval_id":"p","decision":"allow","reviewer":"user","input_changed":false}),
        ));
        assert_eq!(p.act(Node::Guards, T0 + 30_100).phase, Phase::Result);
        p.apply(&requested(T0 + 31_000, "a", "Bash"));
        p.apply(&env(
            T0 + 32_000,
            "session.cancelled",
            json!({"reason":"x"}),
        ));
        assert!(on(&p, T0 + 40_000).is_empty());
    }

    #[test]
    fn one_shot_flows_user_to_core_and_the_rest_core_to_node() {
        assert!(toward_core(Node::User, Phase::Request));
        assert!(!toward_core(Node::User, Phase::Result));
        assert!(!toward_core(Node::Tools, Phase::Request));
        assert!(toward_core(Node::Tools, Phase::Result));
        let mut p = Pulses::default();
        p.apply(&env(T0, "user.message", json!({"text":"hi","images":[]})));
        assert_eq!(p.act(Node::User, T0 + 100).phase, Phase::Request);
        assert_eq!(p.act(Node::User, T0 + 600).phase, Phase::Result);
    }

    #[test]
    fn replayed_open_calls_settle_and_forced_nodes_stay_open() {
        let mut p = Pulses::default();
        p.apply(&requested(T0, "a", "Bash"));
        p.settle();
        assert!(on(&p, T0 + 99_999).is_empty());
        let f = Pulses::forced(&[Node::Tools, Node::Mcp]);
        assert_eq!(on(&f, 250), vec![Node::Mcp, Node::Tools]);
    }
}
