//! One agent's model loop (main agent or sub-agent): build context, stream, fold, run tools.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use futures_util::StreamExt;
use serde_json::{Value, json};
use synthesis_context::{CharsPerToken, fixed_tokens, messages_tokens};
use synthesis_events::{Event, ToolStatus};
use synthesis_hooks::HookEvent;
use synthesis_providers::{
    Block, Content, Delta, ErrorKind, Message, MessageBuilder, ProviderError, Request, Role,
    StartBlock, StopReason, StreamEvent, System, SystemBlock, ToolDef,
};

use crate::harness::{AgentCtx, AgentEnd, AgentState, Harness, TaskRun};
use crate::util::{check_workspace, event_usage, lock, uuid};

/// Window assumed when the model's metadata has none (Node default).
const DEFAULT_WINDOW: u64 = 200_000;
/// A proactive compaction must free at least this share, else auto-compact pauses.
const MIN_GAIN_PERCENT: u64 = 15;
const GAIN_PAUSE_REQUESTS: u64 = 5;

/// Input size of a request as the provider counts it. Anthropic reports cached input apart from
/// `input_tokens`; OpenAI includes it.
/// Providers report `input_tokens` without the cache tokens (the OpenAI adapter subtracts them).
fn provider_used(u: &synthesis_providers::Usage) -> Option<u64> {
    let n = |t: Option<u64>| t.unwrap_or(0);
    let all = n(u.input_tokens) + n(u.cache_read_input_tokens) + n(u.cache_creation_input_tokens);
    (all > 0).then_some(all)
}

pub(crate) enum StreamFail {
    Cancelled,
    /// The task wall budget ran out during the model call.
    Budget(&'static str),
    Provider(ProviderError),
}

struct Streamed {
    msg: synthesis_providers::AssistantMessage,
    /// Tool calls whose block never got a stop event (cut off).
    unstopped: HashSet<String>,
}

pub(crate) enum Problem {
    Truncated,
    Malformed(String),
}

pub(crate) struct Call {
    pub id: String,
    pub name: String,
    pub input: Option<Value>,
    pub problem: Option<Problem>,
}

fn delta_event(index: usize, d: &Delta) -> Option<Event> {
    let (kind, text) = match d {
        Delta::TextDelta { text } => ("text", text),
        Delta::ThinkingDelta { thinking } => ("thinking", thinking),
        Delta::InputJsonDelta { partial_json } => ("tool_input", partial_json),
        Delta::SignatureDelta { .. } | Delta::Unknown => return None,
    };
    Some(Event::ModelDelta {
        block_index: index as i64,
        kind: kind.into(),
        text: text.clone(),
    })
}

/// Event form of a content block: the thinking signature is provider-issued, never emitted.
fn block_value(b: &Block, tools: &crate::ToolBox) -> Value {
    let mut v = serde_json::to_value(b).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("signature");
    }
    if let Block::ToolUse {
        name,
        input: Some(i),
        ..
    } = b
    {
        v["input"] = tools.redact_input(name, i);
    }
    v
}

/// The worst-case USD a call holds in the task counters until its response is accounted. Dropping it
/// (an early return, an aborted sub-agent future) gives the money back, so nothing leaks.
struct Reservation<'a> {
    task: &'a TaskRun,
    amount: f64,
}

impl Reservation<'_> {
    fn release(&mut self) {
        if self.amount > 0.0 {
            let mut c = lock(&self.task.counters);
            c.reserved = (c.reserved - self.amount).max(0.0);
            self.amount = 0.0;
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Smallest output limit a capped call is lowered to; below it the call is refused.
const MIN_OUTPUT: u64 = 4096;
/// Consecutive batches of only cut-off tool calls, under a lowered output limit, that end the task.
const MAX_CUT_STREAK: u32 = 3;

/// `(output limit, USD to reserve)` for a call of `prompt` tokens that wants `want` output tokens
/// when `left` USD of the cap remain (after the calls in flight); `None` when even the floor does
/// not fit.
fn plan_call(
    price: &crate::harness::Pricing,
    prompt: u64,
    want: u64,
    left: f64,
) -> Option<(u64, f64)> {
    let per_token = price.worst_case(prompt, 1) - price.worst_case(prompt, 0);
    let afford = if per_token > 0.0 {
        ((left - price.worst_case(prompt, 0)) / per_token)
            .floor()
            .max(0.0) as u64
    } else {
        u64::MAX
    };
    if afford < want.min(MIN_OUTPUT) {
        return None;
    }
    let out = want.min(afford);
    Some((out, price.worst_case(prompt, out)))
}

#[cfg(test)]
mod estimate_tests {
    use super::*;
    use crate::harness::Pricing;

    // Opus-class: $15 in, $75 out per million
    const OPUS: Pricing = Pricing {
        input_per_mtok: 15.0,
        output_per_mtok: 75.0,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        long: None,
    };

    #[test]
    fn a_dear_model_under_a_one_dollar_cap_gets_a_clamped_output_not_a_refusal() {
        // 32000 output tokens alone would be $2.4
        let (out, reserve) = plan_call(&OPUS, 5_000, 32_000, 1.0).expect("fits clamped");
        assert!((4096..32_000).contains(&out), "{out}");
        assert!(reserve <= 1.0 && reserve > 0.5, "{reserve}");
        // a request that wanted less than the floor is not raised
        assert_eq!(plan_call(&OPUS, 5_000, 100, 1.0).map(|p| p.0), Some(100));
        // too little left for the floor: refused
        assert!(plan_call(&OPUS, 5_000, 32_000, 0.2).is_none());
    }

    /// Concurrent calls see each other's reservation: the second gets less (or none) of the same cap.
    #[test]
    fn a_reservation_shrinks_what_a_parallel_call_can_have() {
        let (_, r1) = plan_call(&OPUS, 5_000, 32_000, 1.0).unwrap();
        let second = plan_call(&OPUS, 5_000, 32_000, 1.0 - r1);
        assert!(second.is_none(), "{second:?}");
        // and with room for both they both fit in full
        let (_, r) = plan_call(&OPUS, 1_000, 4_096, 5.0).unwrap();
        assert!(plan_call(&OPUS, 1_000, 4_096, 5.0 - r).is_some());
    }
}

impl Harness {
    pub(crate) fn budget_hit(&self, task: &TaskRun, agent: &AgentCtx) -> Option<&'static str> {
        if let Some(k) = *lock(&task.hit) {
            return Some(k);
        }
        let found = {
            let c = lock(&task.counters);
            let b = &task.budget;
            let wall = task.started.elapsed().as_millis() as u64;
            if c.turns >= b.max_turns {
                Some(("max_turns", f64::from(b.max_turns), f64::from(c.turns)))
            } else if b.max_wall_ms.is_some_and(|m| wall >= m) {
                Some((
                    "max_wall_ms",
                    b.max_wall_ms.unwrap_or(0) as f64,
                    wall as f64,
                ))
            } else if let Some(t) = [self.config.max_total_tokens, b.max_tokens_total]
                .into_iter()
                .flatten()
                .min()
                .filter(|t| c.input + c.output + c.cache_read + c.cache_create >= *t)
            {
                Some((
                    "max_tokens",
                    t as f64,
                    (c.input + c.output + c.cache_read + c.cache_create) as f64,
                ))
            } else if b.max_cost_usd.is_some_and(|m| c.cost >= m) {
                Some(("max_cost_usd", b.max_cost_usd.unwrap_or(0.0), c.cost))
            } else {
                None
            }
        };
        found.map(|(k, limit, used)| {
            self.mark_budget(task, agent, k, limit, used, None);
            k
        })
    }

    /// Pre-turn cost check under a cost cap. The worst case of the call is the estimated prompt
    /// (20% margin until the provider has reported a size) at the dearer of the input and
    /// cache-write prices plus `max_tokens` of output at the output price. When that does not fit
    /// in what is left (cap - spend - worst cases of calls in flight), `max_tokens` is lowered to
    /// what the remainder affords, but never below [`MIN_OUTPUT`] (or the request's own limit if
    /// that is smaller); only below that floor is the call refused. A model with no price is
    /// refused. The worst case of the call that goes ahead is RESERVED in the task counters until
    /// the response is accounted, so concurrent sub-agents see each other. `Ok` is the reservation guard.
    fn estimate_stop<'a>(
        &self,
        task: &'a TaskRun,
        agent: &AgentCtx,
        req: &mut Request,
        st: &mut AgentState,
        raw: u64,
    ) -> Result<Reservation<'a>, &'static str> {
        let Some(cap) = task.budget.max_cost_usd else {
            return Ok(Reservation { task, amount: 0.0 });
        };
        let Some(price) = self.price_of(agent) else {
            let spent = lock(&task.counters).cost;
            self.mark_budget(task, agent, "max_cost_usd_unpriced", cap, spent, None);
            return Err("max_cost_usd_unpriced");
        };
        let est = st.meter.estimate(raw);
        let prompt = if st.meter.provider_used().is_some() {
            est
        } else {
            est + est / 5
        };
        // decide and reserve in ONE critical section: what is left, the plan and the reservation
        // cannot be separated by a sibling agent
        let planned = {
            let mut c = lock(&task.counters);
            let (spent, left) = (c.cost, cap - c.cost - c.reserved);
            let want = u64::from(req.max_tokens);
            match plan_call(&price, prompt, want, left) {
                Some((out, reserve)) => {
                    c.reserved += reserve;
                    Ok((want, out, reserve))
                }
                None => Err((spent, price.worst_case(prompt, want.min(MIN_OUTPUT)))),
            }
        };
        match planned {
            Err((spent, worst)) => {
                self.mark_budget(
                    task,
                    agent,
                    "max_cost_usd_estimate",
                    cap,
                    spent,
                    Some(worst),
                );
                Err("max_cost_usd_estimate")
            }
            Ok((want, out, reserve)) => {
                if out < want {
                    req.max_tokens = out as u32;
                    if !st.clamp_note {
                        st.clamp_note = true;
                        self.notice(
                            task,
                            "warn",
                            format!(
                                "output limit lowered to {out} tokens to stay within the cost cap"
                            ),
                        );
                    }
                }
                Ok(Reservation {
                    task,
                    amount: reserve,
                })
            }
        }
    }

    /// Records the first exceeded budget and emits `budget.exceeded` once.
    pub(crate) fn mark_budget(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        kind: &'static str,
        limit: f64,
        used: f64,
        estimate_usd: Option<f64>,
    ) {
        let first = {
            let mut h = lock(&task.hit);
            h.is_none() && h.replace(kind).is_none()
        };
        if first {
            self.emit(
                task,
                agent,
                None,
                Event::BudgetExceeded {
                    budget: kind.into(),
                    limit,
                    used,
                    estimate: estimate_usd.is_some(),
                    estimate_usd,
                },
            );
        }
    }

    async fn stream_once(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        req: &Request,
    ) -> Result<Streamed, StreamFail> {
        let mut stream = self.provider.stream(req, agent.cancel.clone());
        let mut builder = MessageBuilder::new();
        let mut open: BTreeMap<usize, Option<String>> = BTreeMap::new();
        // Tool-call blocks whose streamed arguments must not be emitted (they may hold secrets
        // and cannot be redacted before they are complete; `tool.requested` carries the redacted input).
        let mut masked: HashSet<usize> = HashSet::new();
        loop {
            let next = tokio::select! {
                biased;
                () = agent.cancel.cancelled() => return Err(StreamFail::Cancelled),
                // dropping the stream ends the request; a stalled or slow provider cannot outlive `max_wall_ms`
                () = async {
                    match task.deadline {
                        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
                        None => std::future::pending().await,
                    }
                } => match self.budget_hit(task, agent) {
                    Some(k) => return Err(StreamFail::Budget(k)),
                    None => continue,
                },
                n = stream.next() => n,
            };
            let Some(item) = next else { break };
            let ev = item.map_err(StreamFail::Provider)?;
            match &ev {
                StreamEvent::ContentBlockStart {
                    index,
                    content_block: StartBlock::ToolUse { id, name, .. },
                } => {
                    open.insert(*index, id.clone());
                    // An unnamed block is unknown, so treated as masked.
                    if name.as_deref().is_none_or(|n| self.tools.masks_input(n)) {
                        masked.insert(*index);
                    }
                }
                StreamEvent::ContentBlockStop { index } => {
                    open.remove(index);
                    masked.remove(index);
                }
                StreamEvent::ContentBlockDelta { index, delta } => {
                    let hide =
                        masked.contains(index) && matches!(delta, Delta::InputJsonDelta { .. });
                    if let Delta::TextDelta { text } = delta {
                        self.peer_partial(&agent.name, text);
                    }
                    if let Some(e) = delta_event(*index, delta).filter(|_| !hide) {
                        self.emit(task, agent, Some(turn), e);
                    }
                }
                _ => {}
            }
            builder.push(&ev).map_err(StreamFail::Provider)?;
        }
        let msg = builder.finish().map_err(StreamFail::Provider)?;
        Ok(Streamed {
            msg,
            unstopped: open.into_values().flatten().collect(),
        })
    }

    /// Adds usage of a call that is not a turn (compaction summary) to the task counters.
    /// Priced as `model` (its row of `pricing_for`, else the run's price).
    pub(crate) fn charge_usage(&self, task: &TaskRun, model: &str, u: &synthesis_providers::Usage) {
        let price = self.price_for(model);
        let mut c = lock(&task.counters);
        c.input += u.input_tokens.unwrap_or(0);
        c.output += u.output_tokens.unwrap_or(0);
        c.cache_read += u.cache_read_input_tokens.unwrap_or(0);
        c.cache_create += u.cache_creation_input_tokens.unwrap_or(0);
        c.cost += price.map_or(0.0, |p| p.cost(u));
    }

    /// Adds the response usage to the task counters; returns the event form.
    /// The price of the model that answered: the failover target if the provider switched, else the
    /// agent's own model; the run's price when no row exists.
    fn price_of(&self, agent: &AgentCtx) -> Option<crate::harness::Pricing> {
        let model = self
            .provider
            .answered_by()
            .unwrap_or_else(|| self.model_of(agent));
        self.price_for(&model)
    }

    /// The price of `model`: its row of `pricing_for` when the host priced models one by one (an
    /// unlisted model has NO price: it never borrows the run's), else the run's price.
    fn price_for(&self, model: &str) -> Option<crate::harness::Pricing> {
        match &self.config.pricing_for {
            Some(f) => f(model),
            None => self.config.pricing,
        }
    }

    fn account(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        u: &synthesis_providers::Usage,
    ) -> synthesis_events::Usage {
        let mut c = lock(&task.counters);
        c.turns += 1;
        let (i, o) = (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0));
        c.input += i;
        c.output += o;
        c.cache_read += u.cache_read_input_tokens.unwrap_or(0);
        c.cache_create += u.cache_creation_input_tokens.unwrap_or(0);
        let cost = self.price_of(agent).map(|p| p.cost(u));
        c.cost += cost.unwrap_or(0.0);
        event_usage(u, cost)
    }

    /// The tools `agent` may see: a bare deny rule (`Bash`, `mcp__server`) hides the tool, the
    /// model never sees it. `ToolSearch` is listed here; `tool_defs` drops it when unused.
    pub(crate) fn visible_specs(&self, agent: &AgentCtx) -> Vec<synthesis_tools::ToolSpec> {
        let denied = lock(&agent.perms).rules.deny.clone();
        let hidden = |n: &str| {
            denied
                .iter()
                .any(|r| r == n || (r.starts_with("mcp__") && n.starts_with(&format!("{r}__"))))
        };
        let mut all = self.tools.specs();
        all.push(crate::toolsearch::spec());
        all.into_iter()
            .filter(|s| !hidden(&s.name))
            .filter(|s| agent.allowed.as_ref().is_none_or(|a| a.contains(&s.name)))
            .filter(|s| s.name != "ListSessions" || self.peers_on())
            .collect()
    }

    /// Tool definitions of the next request and the names of the deferred MCP tools (listed in
    /// the system prompt instead of their schemas).
    fn tool_defs(&self, task: &TaskRun, agent: &AgentCtx) -> (Vec<ToolDef>, BTreeSet<String>) {
        let visible = self.visible_specs(agent);
        let (active, deferred) = self.deferred(task, &visible);
        let defs = visible
            .into_iter()
            .filter(|s| !deferred.contains(&s.name))
            .filter(|s| s.name != crate::toolsearch::NAME || active)
            .map(|s| ToolDef {
                name: s.name,
                description: Some(s.description),
                input_schema: s.input_schema,
            })
            .collect();
        (defs, deferred)
    }

    /// Builds the next request from `st.messages` and refreshes `st.fixed_raw`.
    pub(crate) fn make_request(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        st: &mut AgentState,
    ) -> Result<Request, String> {
        if task.caps.context_window.is_none()
            && agent.agent_id.is_none()
            && !self
                .window_noted
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.notice(
                task,
                "warning",
                format!(
                    "context window unknown for {}: assuming 200k; set limit.context",
                    self.config.model
                ),
            );
        }
        let built = self.context.build(&st.messages, agent.prompt.as_deref())?;
        let max_output = task
            .caps
            .max_output
            .map_or(self.config.max_output_tokens, |m| {
                self.config
                    .max_output_tokens
                    .min(m.min(u64::from(u32::MAX)) as u32)
            });
        let mut messages = built.messages;
        if !task.caps.images {
            let n = strip_images(&mut messages);
            if n > 0 && !st.image_note {
                st.image_note = true;
                self.notice(
                    task,
                    "warning",
                    format!("the model does not take images: {n} image(s) are sent as \"[image]\""),
                );
            }
        }
        // older screenshots go out as placeholders; the history keeps them
        let (n, bytes) = synthesis_context::omit_old_images(&mut messages, 2, REQUEST_IMAGE_BYTES);
        if n > st.images_omitted {
            st.images_omitted = n;
            self.notice(
                task,
                "info",
                format!(
                    "older screenshots omitted from the model request ({n} images, {:.1} MB); they stay in the session",
                    bytes as f64 / 1e6
                ),
            );
        }
        let (tools, deferred) = self.tool_defs(task, agent);
        let mut system = built.system;
        if !deferred.is_empty() {
            let note = format!(
                "\n\nThese MCP tools are listed by name only. Before calling one, load its \
                 definition with the ToolSearch tool (query \"select:<name>\" or keywords): {}",
                deferred.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            match &mut system {
                System::Text(t) => t.push_str(&note),
                System::Blocks(b) => b.push(SystemBlock {
                    kind: Some("text".into()),
                    text: Some(note.trim_start().into()),
                }),
            }
        }
        let req = Request {
            model: self.model_of(agent),
            max_tokens: max_output,
            system: Some(system),
            messages,
            tools,
            temperature: None,
            thinking: None,
            // never a tier the model does not declare (it may have changed with /model)
            reasoning_effort: lock(&self.reasoning_effort)
                .clone()
                .filter(|e| task.caps.effort_tiers.contains(e)),
        };
        st.fixed_raw = fixed_tokens(&req);
        Ok(req)
    }

    /// The model of the next request: the sub-agent's own (frontmatter `model`) or the session's.
    fn model_of(&self, agent: &AgentCtx) -> String {
        agent
            .limits
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone())
    }

    fn messages_raw(&self, st: &AgentState) -> u64 {
        messages_tokens(&CharsPerToken::default(), &st.messages)
    }

    /// The window compaction is measured against: the model's, else the 200k fallback.
    pub(crate) fn window(task: &TaskRun) -> u64 {
        task.caps.context_window.unwrap_or(DEFAULT_WINDOW).max(1)
    }

    /// The window shown to people (`window_tokens` of the events): 0 = unknown.
    fn reported_window(task: &TaskRun) -> u64 {
        task.caps.context_window.map_or(0, |w| w.max(1))
    }

    fn auto_due(&self, task: &TaskRun, st: &AgentState, used: u64) -> bool {
        let c = &self.config.auto_compact;
        c.enabled
            && self.context.can_compact()
            && st.requests > st.auto_pause_until
            && used as f64 >= c.threshold * Self::window(task) as f64
    }

    fn emit_usage(&self, task: &TaskRun, agent: &AgentCtx, used: u64, source: &str, stale: bool) {
        let window = Self::reported_window(task);
        self.emit(
            task,
            agent,
            None,
            Event::ContextUsage {
                used_tokens: used,
                window_tokens: window,
                percent: if window == 0 {
                    0.0
                } else {
                    used as f64 * 100.0 / window as f64
                },
                source: source.into(),
                stale,
            },
        );
    }

    /// `context.usage` after a response: the provider's input size when it reported one (and the
    /// estimator is recalibrated against it), else the calibrated estimate of the request just
    /// sent, marked stale because the reply is not counted in it.
    fn report_usage(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        st: &mut AgentState,
        raw: u64,
        u: &synthesis_providers::Usage,
    ) {
        match provider_used(u) {
            Some(used) => {
                st.meter.observe(raw, used);
                self.emit_usage(task, agent, used, "provider", false);
            }
            None => self.emit_usage(task, agent, st.meter.estimate(raw), "estimate", true),
        }
    }

    /// Compacts `st.messages` and emits `context.compacting`, then `context.compacted` and
    /// `context.usage` (post-compaction estimate), or `context.compaction_failed`. A PreCompact
    /// hook runs first; its output is added to the summary instructions, and exit 2 (`blocked`)
    /// cancels the compaction like Claude Code. Returns the
    /// estimated tokens `(before, after)`. History is untouched on error.
    pub(crate) async fn compact_history(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        st: &mut AgentState,
        reason: &str,
        instructions: &str,
    ) -> Result<(u64, u64), String> {
        let started = std::time::Instant::now();
        let window = Self::reported_window(task);
        let before = st.meter.estimate(st.fixed_raw + self.messages_raw(st));
        let hook = self
            .run_hook(
                task,
                agent,
                None,
                HookEvent::PreCompact,
                json!({
                    "trigger": if reason == "manual" { "manual" } else { "auto" },
                    "session_id": task.session_id,
                    "custom_instructions": instructions,
                }),
                None,
            )
            .await;
        if hook.as_ref().is_some_and(|h| h.blocked) {
            let why = "blocked by a PreCompact hook".to_string();
            self.emit(
                task,
                agent,
                None,
                Event::ContextCompactionFailed {
                    reason: reason.into(),
                    error: why.clone(),
                },
            );
            return Err(why);
        }
        let mut instr = instructions.to_string();
        for extra in hook.iter().flat_map(|h| &h.additional_context) {
            instr.push('\n');
            instr.push_str(extra);
        }
        self.emit(
            task,
            agent,
            None,
            Event::ContextCompacting {
                reason: reason.into(),
                used_tokens: before,
                window_tokens: window,
            },
        );
        let result = tokio::select! {
            biased;
            () = agent.cancel.cancelled() => Err("cancelled".to_string()),
            r = self.context.compact(&st.messages, instr.trim()) => r,
        };
        if let Some(u) = self.context.take_usage() {
            self.charge_usage(task, &self.config.model, &u);
        }
        let c = match result {
            Ok(c) => c,
            Err(e) => {
                self.emit(
                    task,
                    agent,
                    None,
                    Event::ContextCompactionFailed {
                        reason: reason.into(),
                        error: e.chars().take(500).collect(),
                    },
                );
                return Err(e);
            }
        };
        let messages_before = st.messages.len() as u64;
        st.messages = c.messages;
        st.meter.forget_anchor();
        let after = st.meter.estimate(st.fixed_raw + self.messages_raw(st));
        // Emitted before the next `build`: the transcript mirror learns of the rewrite from it.
        self.emit(
            task,
            agent,
            None,
            Event::ContextCompacted {
                trigger: if reason == "manual" { "manual" } else { "auto" }.into(),
                summary_ref: c.summary_ref,
                tokens_before: Some(before),
                reason: reason.into(),
                before_tokens: before,
                after_tokens: after,
                window_tokens: window,
                messages_before,
                messages_after: st.messages.len() as u64,
                summary_chars: c.summary_chars,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
        self.emit_usage(task, agent, after, "estimate", false);
        Ok((before, after))
    }

    pub(crate) async fn run_agent(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        st: &mut AgentState,
    ) -> AgentEnd {
        let mut iteration = 0i64;
        let mut recoveries = 0u32;
        let mut cut_streak = 0u32;
        loop {
            if agent.cancel.is_cancelled() {
                return AgentEnd::Cancelled;
            }
            if self.sink.is_broken() {
                return AgentEnd::Internal("event_sink_failed".into());
            }
            if let Err(why) = check_workspace(&task.workspace) {
                return AgentEnd::UnsafeWorkspace(why);
            }
            if let Some(kind) = self.budget_hit(task, agent) {
                return AgentEnd::Budget(kind);
            }
            // frontmatter `maxTurns`: ends this agent only; the task budget above stays the outer bound
            if agent
                .limits
                .max_turns
                .is_some_and(|n| iteration >= i64::from(n))
            {
                return AgentEnd::Budget("agent_max_turns");
            }
            st.requests += 1;
            let mut req = match self.make_request(task, agent, st) {
                Ok(r) => r,
                Err(e) => return AgentEnd::Internal(format!("context_error: {e}")),
            };
            // Safe point: the previous batch's results are in history, so pairing is complete.
            let manual = if agent.agent_id.is_none() {
                lock(&self.compact_wanted).take()
            } else {
                None
            };
            let used = st.meter.estimate(st.fixed_raw + self.messages_raw(st));
            let reason = if manual.is_some() {
                Some("manual")
            } else if self.auto_due(task, st, used) {
                Some("auto")
            } else {
                None
            };
            if let Some(reason) = reason {
                let instructions = manual.flatten().unwrap_or_default();
                match self
                    .compact_history(task, agent, st, reason, &instructions)
                    .await
                {
                    Ok((before, after)) => {
                        if reason == "auto" {
                            st.auto_failures = 0;
                            if after * 100 > before * (100 - MIN_GAIN_PERCENT) {
                                st.auto_pause_until = st.requests + GAIN_PAUSE_REQUESTS;
                                self.notice(
                                    task,
                                    "warn",
                                    format!(
                                        "auto-compact freed under {MIN_GAIN_PERCENT}% ({before} -> {after} tokens); paused for {GAIN_PAUSE_REQUESTS} requests"
                                    ),
                                );
                            }
                        }
                        req = match self.make_request(task, agent, st) {
                            Ok(r) => r,
                            Err(e) => return AgentEnd::Internal(format!("context_error: {e}")),
                        };
                    }
                    Err(_) if agent.cancel.is_cancelled() => return AgentEnd::Cancelled,
                    Err(_) if reason == "auto" => {
                        st.auto_failures += 1;
                        st.auto_pause_until = st.requests + (1u64 << st.auto_failures.min(4));
                    }
                    Err(_) => {}
                }
            }
            // Drain point: the round's results are in history (and no compaction is running).
            if self.deliver(task, agent, st) {
                req = match self.make_request(task, agent, st) {
                    Ok(r) => r,
                    Err(e) => return AgentEnd::Internal(format!("context_error: {e}")),
                };
            }
            let raw = st.fixed_raw + self.messages_raw(st);
            let mut reservation = match self.estimate_stop(task, agent, &mut req, st, raw) {
                Ok(r) => r,
                Err(kind) => return AgentEnd::Budget(kind),
            };
            let turn = uuid();
            iteration += 1;
            let t0 = std::time::Instant::now();
            self.emit(
                task,
                agent,
                Some(&turn),
                Event::TurnStarted {
                    model: self.model_of(agent),
                    provider: self.provider.id().to_string(),
                    iteration,
                },
            );

            let streamed = self.stream_once(task, agent, &turn, &req).await;
            if streamed.is_err() {
                reservation.release();
            }
            let streamed = match streamed {
                Ok(s) => s,
                Err(StreamFail::Cancelled) => return AgentEnd::Cancelled,
                Err(StreamFail::Budget(k)) => return AgentEnd::Budget(k),
                Err(StreamFail::Provider(e)) => {
                    if agent.cancel.is_cancelled() || e.kind == ErrorKind::Cancelled {
                        return AgentEnd::Cancelled;
                    }
                    self.emit(
                        task,
                        agent,
                        Some(&turn),
                        Event::ModelError {
                            status: e.status.map(i64::from),
                            message: e.message().to_string(),
                            retryable: e.retryable(),
                            context_overflow: e.context_overflow(),
                        },
                    );
                    if e.context_overflow() && st.overflow_compactions < MAX_OVERFLOW_COMPACTIONS {
                        st.overflow_compactions += 1;
                        match self.compact_history(task, agent, st, "overflow", "").await {
                            Ok(_) => continue,
                            Err(why) => {
                                return AgentEnd::ModelError {
                                    blocked: false,
                                    reason: format!("context_overflow: compaction failed: {why}"),
                                };
                            }
                        }
                    }
                    // Only a failure after the first byte is retried here: before that the
                    // provider layer already used its retry budget.
                    let transient = matches!(
                        e.kind,
                        ErrorKind::RateLimit
                            | ErrorKind::Overloaded
                            | ErrorKind::Server
                            | ErrorKind::Network
                            | ErrorKind::InvalidResponse
                    );
                    if e.mid_stream && transient && recoveries < self.config.stream_recoveries {
                        recoveries += 1;
                        let delay = self.config.recovery_delay;
                        self.emit(
                            task,
                            agent,
                            Some(&turn),
                            Event::ModelRetry {
                                attempt: i64::from(recoveries),
                                max: i64::from(self.config.stream_recoveries),
                                error: e.message().to_string(),
                                delay_ms: delay.as_millis() as u64,
                            },
                        );
                        tokio::select! {
                            () = tokio::time::sleep(delay) => {}
                            () = agent.cancel.cancelled() => return AgentEnd::Cancelled,
                        }
                        continue;
                    }
                    return AgentEnd::ModelError {
                        blocked: e.kind == ErrorKind::Auth,
                        reason: format!("model_error:{}: {}", e.kind.as_str(), e.message()),
                    };
                }
            };
            recoveries = 0;

            let Streamed { mut msg, unstopped } = streamed;
            let usage = self.account(task, agent, &msg.usage);
            reservation.release();
            self.report_usage(task, agent, st, raw, &msg.usage);
            let has_tools = msg
                .content
                .iter()
                .any(|b| matches!(b, Block::ToolUse { .. }));
            let stop_str = msg.stop_reason.clone().map_or_else(
                || {
                    if has_tools {
                        "tool_use".to_string()
                    } else {
                        "end_turn".to_string()
                    }
                },
                String::from,
            );
            self.emit(
                task,
                agent,
                Some(&turn),
                Event::AssistantMessage {
                    message_id: msg.id.clone(),
                    content: msg
                        .content
                        .iter()
                        .map(|b| block_value(b, &self.tools))
                        .collect(),
                    stop_reason: stop_str.clone(),
                    usage: Some(usage.clone()),
                },
            );
            self.emit(
                task,
                agent,
                Some(&turn),
                Event::TurnCompleted {
                    stop_reason: stop_str,
                    usage: Some(usage),
                    duration_ms: t0.elapsed().as_millis() as u64,
                },
            );

            // Fold into history and collect the calls. A call without valid arguments stays in
            // history with `{}` (providers reject a tool_use without input) and gets an error result.
            let max_tokens_hit = msg.stop_reason == Some(StopReason::MaxTokens);
            if max_tokens_hit {
                self.notice(
                    task,
                    "warn",
                    format!(
                        "the response was cut at the output limit ({} tokens)",
                        req.max_tokens
                    ),
                );
            }
            let mut calls = vec![];
            let mut final_text = String::new();
            for b in &mut msg.content {
                match b {
                    Block::Text { text } => final_text.push_str(text),
                    Block::ToolUse { id, name, input } => {
                        let bad = msg
                            .invalid_tool_inputs
                            .iter()
                            .find(|i| i.tool_use_id == *id);
                        let problem = if unstopped.contains(id.as_str())
                            || (bad.is_some() && max_tokens_hit)
                        {
                            Some(Problem::Truncated)
                        } else {
                            bad.map(|i| Problem::Malformed(i.error.clone()))
                        };
                        calls.push(Call {
                            id: id.clone(),
                            name: name.clone(),
                            input: if problem.is_some() {
                                None
                            } else {
                                input.clone()
                            },
                            problem,
                        });
                        // History (returned to the caller, possibly persisted) keeps no secrets.
                        *input = Some(match input.take() {
                            Some(i) => self.tools.redact_input(name, &i),
                            None => json!({}),
                        });
                    }
                    _ => {}
                }
            }
            if !msg.content.is_empty() {
                st.messages.push(Message {
                    role: Role::Assistant,
                    content: Content::Blocks(msg.content.clone()),
                });
            }

            if calls.is_empty() {
                match msg.stop_reason {
                    Some(StopReason::Refusal) => return AgentEnd::Refusal,
                    Some(StopReason::PauseTurn) => continue,
                    _ => {
                        // a cut answer is not presented as a finished one
                        if max_tokens_hit {
                            final_text.push_str("\n\n[answer cut at the output limit]");
                        }
                        return AgentEnd::Finished(final_text);
                    }
                }
            }

            let all_cut = calls
                .iter()
                .all(|c| matches!(c.problem, Some(Problem::Truncated)));
            let outcome = self.run_batch(task, agent, &turn, calls).await;
            st.messages.push(Message {
                role: Role::User,
                content: Content::Blocks(outcome.blocks),
            });
            if agent.cancel.is_cancelled() {
                return AgentEnd::Cancelled;
            }
            if let Some(kind) = *lock(&task.hit) {
                return AgentEnd::Budget(kind);
            }
            // calls that keep being cut off under a lowered output limit cannot succeed
            cut_streak = if all_cut { cut_streak + 1 } else { 0 };
            if cut_streak >= MAX_CUT_STREAK && st.clamp_note {
                self.mark_budget(
                    task,
                    agent,
                    "max_tokens_clamp",
                    f64::from(req.max_tokens),
                    0.0,
                    None,
                );
                return AgentEnd::Budget("max_tokens_clamp");
            }
            st.recent.push_back(outcome.fingerprint);
            let window = self.config.no_progress_window.max(1);
            while st.recent.len() > window.max(OSCILLATION_WINDOW) {
                st.recent.pop_front();
            }
            if stalled(&st.recent, window) {
                return AgentEnd::NoProgress {
                    approval_unavailable: outcome.all_unavailable,
                };
            }
        }
    }
}

/// Batches remembered to spot an A,B,A,B (period 2) or A,B,C,A,B,C (period 3) loop.
const OSCILLATION_WINDOW: usize = 6;

/// No progress: the last `window` batches are identical, or the last 4 / 6 repeat with period
/// 2 / 3 (each batch fingerprint covers the calls and their results).
pub(crate) fn stalled(recent: &VecDeque<u64>, window: usize) -> bool {
    let r: Vec<u64> = recent.iter().copied().collect();
    let tail = |n: usize| r.len().checked_sub(n).map(|i| &r[i..]);
    tail(window).is_some_and(|t| t.iter().all(|f| *f == t[0]))
        || tail(4).is_some_and(|t| t[0] == t[2] && t[1] == t[3])
        || tail(6).is_some_and(|t| (0..3).all(|i| t[i] == t[i + 3]))
}

pub(crate) fn tool_status_for(is_error: bool, soft: bool) -> ToolStatus {
    match (is_error, soft) {
        (false, _) => ToolStatus::Ok,
        (true, true) => ToolStatus::SoftError,
        (true, false) => ToolStatus::Error,
    }
}

/// Reactive compactions allowed per task.
const MAX_OVERFLOW_COMPACTIONS: u8 = 3;
/// Decoded image bytes one request may carry (newest first); well under common gateway limits.
const REQUEST_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Replaces images (attached by the person or returned in a tool result) by the text `[image]` in a
/// request copy (the history keeps them); returns how many.
fn strip_images(messages: &mut [synthesis_providers::Message]) -> usize {
    messages.iter_mut().map(|m| m.content.strip_images()).sum()
}

#[cfg(test)]
mod stalled_tests {
    use super::stalled;

    fn s(v: &[u64]) -> bool {
        stalled(&v.iter().copied().collect(), 3)
    }

    #[test]
    fn detects_constant_and_periodic_loops_only() {
        assert!(s(&[7, 7, 7]));
        assert!(s(&[1, 2, 1, 2])); // A,B,A,B
        assert!(s(&[1, 2, 3, 1, 2, 3])); // A,B,C,A,B,C
        assert!(!s(&[1, 2, 3, 4, 5, 6]));
        assert!(!s(&[1, 2, 1])); // too short to call
        assert!(!s(&[1, 2, 1, 3, 1, 2]));
    }
}
