# Spec: Approvals concept (risk tiers, non-blocking asks, the "Needs you" queue)

Status: Proposed (2026-11-18). Touches the permission model and the decision rules of ADR-019/ADR-021: security review required before any phase that changes behaviour (A2 onwards).
Owner principle: HANDOFF 7bl (2026-11-18). Defaults queue pattern: HANDOFF 7bm.
Related: ADR-016 (store), ADR-017 (scheduler), ADR-019 (gateway, decisions, grants), ADR-020 (verification), ADR-021 (UI protocol, `needs_you`), `docs/specs/super-agent.md` section 5, `docs/specs/conductor.md` 3.2, `docs/specs/super-agent-s1.md` (on `docs/super-agent-s1-plan`), `docs/specs/verification-concept.md` (on `docs/verification-concept`), `docs/specs/permissions.md`, `docs/UNATTENDED.md`.

Markers: **[E]** exists in code today (path given), **[N]** new in this spec, **[A]** assumption, not verified.

---

## 0. One-line rule

**Tiers decide attention, never authority.** A tier says who must look and when. What may run is still decided only by the permission engine, the compiled envelope, the sandbox and a person's MAC'd answer. No tier, policy or classifier result can create a grant, widen a scope, raise a budget or lift the irreversible table.

---

## 1. Problem, goals, non-goals

### 1.1 Problem

Agents that run unattended raise many questions. They are buried in transcripts, the owner stops reading them, and turns on auto-run, which removes the safety value of asking at all. Synthesis must keep the safety and remove the noise.

### 1.2 Goals (owner principle a-f)

| # | Goal | Pass criterion (measured in section 10) |
|---|---|---|
| a | Do not interrupt or stop the agent | An ask never ends a work item that still has work that does not depend on the answer; a T1 ask never stops anything |
| b | Do not flood the person | At most one notification per digest window per outcome; T0 never notifies; open asks capped |
| c | Easy to accept good proposals | Every T0/T1 ask has a default; one tap keeps it; a digest keeps all listed T1 rows in one tap |
| d | The agent decides small things itself, recorded | T0 asks become chained `ask.auto_decided` records with the policy hash; nothing T0 grants anything |
| e | Unanswered asks are never lost | Every T1/T2/T3 ask is a stored row in the queue until answered, expired (with a visible result) or invalidated |
| f | A block parks one branch, the rest keeps going | The scheduler leases unblocked items first; the UI shows "blocked by" per item and per ask |

### 1.3 Non-goals

- No multi-person approval, roles or delegation to other people (ADR-019 revisit trigger: "a second approver role").
- No model (LLM) that sets a tier or answers an ask (ADR-019: the auto judge is not an authority in attempts).
- No change to the TUI's own dialogs (owner direction: the TUI view stays as it is; it keeps its badge).
- No new storage schema (no DDL; store stays `user_version` 5, marker 1010).
- No change to what `bypassPermissions`, `auto` or `dontAsk` do in plain sessions.

### 1.4 Never auto-decided (code floor, not configurable)

| Never auto | Why | Existing control |
|---|---|---|
| Anything the permission engine answers `Ask` or `Deny` | It needs a permission the policy does not already give | `Permissions::check` [E] `synthesis-permissions/src/lib.rs` |
| Irreversible external actions (push, publish, deploy, remote shells, cloud writes) | Irreversible | `irreversible.rs` [E] |
| Destructive actions (critical `rm`, deletes outside the worktree, force push) | Irreversible | `Verdict.critical`, `hard` [E] |
| Secrets and credentials (reading secret paths, anything about API keys) | Security, ADR-011 | protected paths, `secrets.read` capability [E] |
| Money beyond a cap; changing a cap | Money | cost cap, contract revise [E] |
| Scope, contract, plan, verification bar | Design authority belongs to the person | `outcome.contract.approve`, `plan_approval`, `verification_review` [E]/[N in V1c] |
| Project trust, MCP approval, enabling the orchestrator, permission-mode raise | Trust boundary | existing dialogs [E] |
| Browser actions that pay or commit money or consent (checkout, place order, subscribe, donate, authorize, card fields) | Money, consent; owner decision 2026-11-20 | `BrowserHost::confirm_payment` [E] (terminal-browser.md, payment guard) |
| Messages to a worker proposed by the orchestrator | S1 4.6: never on the auto list | conductor confirm policy [planned S1 P3b] |

---

## 2. Options considered

| Option | Data flow | Failure modes | Security | Operability | Cost signal |
|---|---|---|---|---|---|
| **O1. Prompt only** ("ask less" in RULES) | none | agent ignores it; injection makes it ask more | no enforcement | nothing to measure | ~0 |
| **O2. One new durable `asks` table for every ask (sessions and outcomes)** | every surface writes one table | needs a schema step: v6 for outcome users, a first durable table for v4-only users; older releases refuse the store | strong (one MAC'd place) | one queue, easy stats | L-XL; migration risk |
| **O3. Layer on what exists (chosen)**: outcome asks are `decisions` rows (new kinds, store-computed brief fields); live session asks stay on `Approver`; one pure classifier; one merged queue view model | as today, plus classifier and view model | session asks are still bound to their process (see "against") | inherits ADR-019 (CAS, MAC, binding, one-shot grant) | events give the stats | M per phase, no migration |
| **O4. LLM judge sets tiers** | model reads the ask, picks a tier | injection lowers tiers; non-deterministic | a model as authority: refused by ADR-019 | hard to audit | tokens per ask |

**Choice: O3.** It reuses the hardened decision machinery (revision + brief-hash binding, CAS, MAC, one-shot grants, expiry to HOLD) and needs no migration.

**Strongest reason against O3:** two backends. An own-session ask (`Approver`, `synthesis-app/src/asks.rs`) lives only while its process lives; the queue merges it with durable decisions, but a closed session's ask is gone (it ends `Expired`/`AutoDenied` and is recorded in the v4 `approvals` table, which the queue shows as "closed unanswered"). The "never lost" goal (e) is therefore fully true for orchestrated work and for live sessions, and true only as a history row for closed sessions. O2 fixes this at the price of a schema step; revisit if the owner reports lost session asks.

---

## 3. What exists today (code audit, this branch from origin/main d6830915)

| Piece | Where | What it does | Gap for this spec |
|---|---|---|---|
| `Approver` trait, `ApprovalRequest`, `ApprovalDecision {Allow, Deny, Expired, AutoDenied}` | `crates/synthesis-harness/src/traits.rs:22-122` | The harness awaits one answer per tool call; `HeadlessApprover` auto-denies | The agent's turn waits on the answer (blocks the session) |
| `Asks`, `Choice {Once, Always, Deny}`, `Notify` | `crates/synthesis-app/src/asks.rs` | Live dialogs of the desktop and `synt engine` | No tier, no expiry, no binding (ADR-019 notes) |
| Permission modes, `Decision {Allow, Ask, Deny, Classify}`, `Verdict.{critical, hard, rule}` | `crates/synthesis-permissions/src/lib.rs:182-250` | Deterministic policy | none (it is the authority this spec keeps) |
| Irreversible table | `crates/synthesis-permissions/src/irreversible.rs` | Asks/denies in every mode | none |
| `DecisionState`, `Actor`, "only a person resolves" | `crates/synthesis-core/src/outcome/state.rs:276-359` | `Approved/Rejected/NeedsMoreInfo` only by `Actor::User` on a current binding before expiry; expiry by orchestrator only | T0/T1 must not use `Approved` (section 8.2) |
| `WorkItemState::{WaitingDecision, Blocked}` | `state.rs:145-219` | An item waits; others keep running | no "blocked by" read model |
| Scheduler | `crates/synthesis-orchestrator/src/sched.rs:303-445, 685-800` | Leases `Ready` items sorted by key; blocks the outcome only when nothing can move, naming the stuck items | no priority by blocking impact; no provisional depth |
| Decisions table | `crates/synthesis-session/src/schema.rs:140-151` | `kind TEXT` (no CHECK), `brief`, `binding`, `binding_hash`, `expires_at`, `action`; one open row per `(outcome, dedupe_key, binding_hash)` | tier and blocks are not stored |
| Decision driver, grants, re-brief, plan approval | ADR-019 PR-10, B3, B6, B9 notes | Expiry = HOLD; grants one-shot from the stored action | reused unchanged |
| `needs_you {decisions_pending, blocked, verified_awaiting_ack}` | ADR-021 section 4 | Derived counts | no per-ask rows across outcomes and sessions |
| Approvals list and popup | `apps/desktop/src/approvals.rs` (`Ages`, `Kind {Tool, Decision}`, `merge` oldest first, `next`, `Popup` with `seen_all`, `send_blocked`), `outcomes_view/ask.rs` (`ARM_MS` inert Enter) | The person's side of decisions | no tier, no digest, no blocked-by |
| Conductor confirm policy | `docs/specs/conductor.md` 3.2: `synt.conductor.confirm` = `"all"` or an auto list; taint and described-target force a card | The existing pattern of "owner may widen auto, code forces a card" | this spec generalises it |
| Non-interactive attempts | ADR-019 PR-08: a call that needs approval is refused; the attempt ends `blocked` (exit 4); `record_blocked_decision` | The FIRST ask ends the attempt (goal a not met) | section 4 |
| Verification tri-state | verification-concept 6.2 [planned V1a]: Verified / Verified with caveats / Failed | the place where unreviewed T1 assumptions become caveats | section 4.4 |

---

## 4. Risk tiers

### 4.1 Tier definitions

| Tier | Name | Meaning | Agent behaviour while open | Person action | One tap? | Batch? | Expiry result |
|---|---|---|---|---|---|---|---|
| **T0** | Auto-decide and record | Choice among actions the policy already allows; reversible inside the worktree; no design, scope, security or money effect; class listed by owner policy (within the floor) | Applies its default at once | none; may review later | n/a | n/a | n/a (already decided) |
| **T1** | Digest | Same as T0 (already allowed, reversible) but it shapes the design or user-visible behaviour | Proceeds on the default, tagged **provisional** | Keep / Change, any time until the outcome concludes | yes (Keep) | yes ("Keep all N listed") | Default stands, permanent caveat "unreviewed assumption" |
| **T2** | Must ask | Needs something not already allowed, or changes plan, scope or the verification bar | Parks only the affected branch; continues elsewhere | Choose, then Send (two steps), brief expanded once | no | no | HOLD; branch stays parked, row stays in "Held" |
| **T3** | Never auto, extra friction | Security, money, destructive, irreversible, credentials, trust | Parks the branch | Two steps + typed confirmation (kind-specific, below) | no | no | HOLD; never becomes yes |

ADR-019 mapping: T0/T1 exist only inside R0/R1 (inside the envelope and sandbox). T2 = an `Ask` verdict outside the irreversible table, and plan/scope/verification decisions. T3 = R2 (irreversible table, `approval_required`). R3/R4 are denied and cannot be asked at all.

### 4.2 Tier of every ask kind

"Floor" = the lowest tier code allows. "Policy" = what owner policy may do. Policy can always raise.

| # | Ask kind | Exists as | Floor | Policy may lower to | Default if unanswered | Phase (S1 / V) |
|---|---|---|---|---|---|---|
| 1 | Tool permission prompt, own session (engine `Ask`) | `Approver` dialog [E] | T2 | no (an allow RULE in user settings removes the ask; that is the permission system, not this spec) | deny (`Expired`/`AutoDenied`) | A7 with S1 P5 |
| 2 | Tool permission in an attempt (`approval_needed`) | decision [E] | T2 | no | HOLD | A3 (existing PR-10) |
| 3 | Write outside allowed paths | engine `Ask` → `approval_needed` [E] | T2; protected path / `hard` verdict: T3 | no | HOLD | A3 |
| 4 | Network egress to a new host; agent loopback host:port (B6) | engine `Ask` / B6 approval [E] | T2; private network ranges: T3 | no | deny | A7 (sessions); attempts have no browser |
| 5 | Irreversible external action (push, publish, deploy, remote shell) | `irreversible.rs` [E] | T3 | no | deny / HOLD | existing |
| 6 | Destructive (critical rm, delete outside worktree, force push) | `Verdict.critical`, deny [E] | T3 (force push: deny) | no | deny | existing |
| 7 | Secrets, credentials, secret-path reads | protected paths, `secrets.read` [E] | T3 | no | deny | existing |
| 8 | Spend over the cap / raise a cap | contract revise [E] | T3 | no | HOLD (outcome blocked, existing) | existing |
| 9 | Plan change (`plan_approval`, replan) | decision [E] B6/B9 | T2 | no | HOLD (`keep_current_plan` is the safe end) | existing; card in S1 P4 |
| 10 | Scope change (`edit_scope`, contract revise) | [E] B4 | T2 when the diff only narrows; T3 when `widens` is non-empty | no | HOLD | existing |
| 11 | Outcome creation (draft card); contract approval; orchestrator enable | S1 4.2 card; approve [E]; enable [E] | draft card T2; contract approval T3; enable T3 | no | nothing created | S1 P3b/P4 |
| 12 | Ambiguity in an attempt (`AskUserQuestion` → `ambiguity`, no preselection) | decision [E] | T1 if the ask carries a default and every option is "already allowed + reversible" (structured effects, 5.2); otherwise T2 | T1 → T0 only for listed classes | T1: default stands; T2: HOLD | A3 (T2), A4 (T1) |
| 13 | Verification waiver, integrity ack, quarantine (`verification_review`) | [planned V1c] | T2 + typed reason >= 20 chars (verification-concept 6.6) | no | HOLD; outcome cannot be Verified | V1c |
| 14 | Person's own line to a worker | S1 4.6 (NQ1: no card) | not an ask | n/a | n/a | S1 P2c/P5 |
| 14b | Orchestrator-proposed worker message | S1 4.6 card | T2, never auto | no (S1: refused on the auto list) | not sent (proposal expires 30 min) | S1 P3b |
| 15 | Conductor proposals: send to own session, interrupt, create session | conductor.md 3.2 | T2 | T0 for `send_message`, `interrupt`, `create_session` via the existing `synt.conductor.confirm` auto list; forced T2 on taint, described target, mode above `default`; `close` never | not done | S1 P3b (existing knob) |
| 16 | Report review (`accept_report`, read-only goal) | [planned S1 P2b] | T2 | no | HOLD; goal not done | S1 P2b |
| 17 | Clarifying question about requirements (sessions and attempts) | `AskUserQuestion` [E] | T1 with a default and no effects; T2 without a default | T0 for listed classes | T1: default stands + assumption recorded | A4 |
| 18 | Style, naming, formatting, small local design choice | does not exist as an ask (agents ask in prose) | T0 when the class is listed; else T1 | T0 (default list: Q2) | default applied | A5 |
| 19 | Project trust, MCP server approval, permission-mode raise to bypass | desktop dialogs [E] (`live.rs` `TrustGrant`, `DialogKind`) | T3 | no | not granted | unchanged (shown in queue only) |
| 20 | Policy change proposal (`policy_change`, ADR-019 section 2) | [planned] | T3 (widens authority) | no | HOLD | later |
| 21 | Browser payment or consent action (`browser_click`, `browser_mouse`, `browser_press_key` Enter/Space, `browser_fill`/`browser_fill_secret` into a card field on a payment-like page, label or origin) | `BrowserHost::confirm_payment` [E] (a dialog of the host, not a harness approval) | T3: one-shot per action, shows origin, label, action, amount; never covered by an allow rule, "always allow", mode (also bypass), sub-agent or conductor | no | refused (no host, sub-agent, headless `synt run`, a changed target, the person's no); never becomes yes | [E] |

### 4.3 The classifier (deterministic; last word)

`tier = max(agent_hint, floor(kind), managed_min(kind), user_min(kind), raise_taint, raise_anomaly, raise_effects)` and then, only if the result is T1 and the class is in the confirmed T0 list and no raise applied, `T0`.

| Input | Source | Effect |
|---|---|---|
| `kind` | the seam that created the ask (engine verdict, decision kind, tool name) | floor table 4.2; an unknown kind is T2 |
| `agent_hint` | the agent's proposed tier | can only RAISE; a lower hint is ignored and counted (anomaly metric) |
| `effects` (structured, 5.2) | the ask's options | any option with `needs_permission`, `outside_paths`, `capability`, `cost_usd > 0`, `irreversible`, `external` makes the ask >= T2; missing or unparseable effects = T2 |
| `class` | agent-declared label (`naming`, `formatting`, ...) | used ONLY to allow T1 → T0; never to lower anything else |
| Taint | the turn read web, MCP or browser content since the person's last line (same taint set as conductor.md 3.2 and memory H1) | T0 → T1 |
| Anomaly | first ask of a class in this outcome; ask text matched by the existing hidden-character / instruction patterns; agent over its ask budget | T0 → T1 |
| Owner policy | user/managed `synt.approvals` (5.5) | raise any kind; list T0 classes from the allowed set only |

Rules:
- Free text (question, option labels, rationale) is **never** an input. The classifier reads typed fields only.
- The classifier lives in one pure function `synthesis_core::ask::classify` [N], versioned (`CLASSIFIER_VERSION`), with a table test per row of 4.2 and a property test `classify(x) >= floor(kind(x))` for random inputs and policies.
- Who sets what: code sets the floor; managed settings can raise (final); user settings can raise and list T0 classes from `T0_ALLOWED_CLASSES` (code constant); project settings are ignored with a diagnostic (like `synt.conductor.confirm`).

---

## 5. Non-blocking asks

### 5.1 The ask object (view model; storage per section 8)

| Field | Meaning | Set by |
|---|---|---|
| `id` | decision id (outcome) or approval id (session) | store / `Asks` |
| `kind` | row of 4.2 | seam |
| `tier` | classifier result + `classifier_version` + `policy_hash` | store (never the agent, never a surface) |
| `blocks` | work item ids (outcome) or `session:<id>` that cannot continue without it | store, from the attempt that raised it and the plan DAG |
| `default` | index of the default option (T0/T1); `HOLD` for T2/T3 | agent proposes (T0/T1 only), store validates it is an "already allowed" option |
| `suggested` | T2 only, if Q1 is accepted: an option the agent suggests, labelled "agent suggests (untrusted)", never preselected | agent, store checks it is one of the options |
| `alternatives[]` | options with structured `effects` and labels | agent; labels cleaned |
| `impact` | server-derived: what each option changes (paths, capabilities, cost estimate, items that rerun if changed later) | store |
| `evidence` | refs (event ids, diff, evidence rows), never inline model text beyond capped cleaned fields | store |
| `created_at`, `expires_at`, `remind_at[]` | timing (section 6.4) | store |
| `revision`, `brief_sha256`, `binding_hash` | same binding as decisions (ADR-019 section 4) | store |
| `status` | `auto_decided`, `provisional`, `parked`, `answered(keep, change, reject, choice)`, `expired`, `invalidated`, `withdrawn` | store |

### 5.2 Structured effects (what the classifier reads)

`effects = {needs_permission: bool, paths: [..], outside_paths: bool, capability: [..], external: bool, irreversible: bool, cost_usd: f64, reruns: [item keys]}`. For engine asks they come from the `Verdict` and the call (already known). For agent asks (`AskUserQuestion` extended additively with `default` and per-option `effects`), the store **recomputes** what it can (paths against the envelope, capability names against the irreversible table); a declared `effects` that cannot be recomputed and is not empty pushes the ask to T2.

### 5.3 Agent behaviour while an ask is open

| Context | T0 | T1 | T2 / T3 |
|---|---|---|---|
| Outcome attempt (worker) | tool result: "decided: <default>, recorded as A-n" | tool result: "proceed with <default>; provisional; recorded as A-n" | tool result: "queued as ask Q-n; NOT run; continue with work that does not need it; do not reach the same effect another way; end your turn when nothing else is left" [N]. Today the first such call ends the attempt (ADR-019 PR-08); new: the attempt continues, and at settle every queued ask becomes one decision (grouped by `dedupe_key`), the item goes `waiting_decision` |
| Super Agent own session (Q5) | same | same | same tool result; the session keeps working; on answer a host note at the next drain point says "Q-n allowed: re-issue the exact call" and a one-shot, input-hash-bound allow applies to that one call (the session analogue of ADR-019 grants) [N] |
| TUI view session | unchanged: dialogs block as today (owner direction) | unchanged | unchanged |
| Headless `synt run` | default applied, listed in the report | default applied, listed as provisional in the report | fail closed (existing `AutoDenied`), run continues on other parts, ends exit 4 if it needed it, the report lists the asks (section 7.5) |

Rules for the "queued, continue" result:
- It never says "approved". The permission engine still judges every later call, so a workaround must pass the same policy.
- A work item "depends on" an ask when the ask's `blocks` names it; inside one attempt the boundary is the attempt (a worker that has nothing left ends its turn).
- At most 3 T2/T3 asks per attempt [A: number is a proposal]; the 4th ends the attempt `blocked` as today.

### 5.4 T1 default execution rules

| Rule | How |
|---|---|
| Only "already allowed" options can be defaults | store checks the option's effects (5.2); the engine judges the calls anyway |
| Checkpointed | the worker commits before applying a T1 default; the commit id is stored on the assumption (`checkpoint_commit`) |
| Tagged provisional | evidence rows of items whose subject commit descends from a provisional checkpoint carry the flag `provisional:<ask id>` [N, existing `flags` column] |
| Visible in verification | the outcome verdict is at best "Verified with caveats" while any T1 assumption is unreviewed (verification-concept 6.2 caveat list) |
| Rejection invalidates dependants | "Change" moves the outcome to `rework_required` with trigger `assumption_changed`; the existing replanner (B9) proposes a `plan_approval` (T2) with the diff; the person's choice reaches the next prompt as quoted data (like `decided_choices`) |
| Bounded waste | provisional spend (attempts that start on a provisional commit) at most 25% of the outcome cap and at most 2 levels of items stacked on unreviewed assumptions [A: proposals]; past that, the next item parks as T2 "needs review of A-n first" |
| Never irreversible | an irreversible-table call during provisional work still asks (T3); test `ap-t1-default-cannot-push` |

---

## 6. Scheduling and reprioritisation

### 6.1 The blocked-by graph

Nodes: work items (existing), asks [N]. Edges: `requires` (existing `work_item_deps`), `ask blocks item` (from `blocks`), `item built on assumption` (provisional, 5.4). The plan DAG is already acyclic (`validate_model_plan`); asks are leaves, so no cycle can form.

```
 ask Q2 (T2, parked) ──blocks──► w3 ──requires──► w5
 ask A1 (T1, provisional) ─built on─► w2 ──requires──► w4
 w1 (ready, unblocked) ─────────────────────────────► leased first
```

### 6.2 Lease order (replaces "sorted by key" in `sched.rs` step 6) [N]

| Rank | Rule |
|---|---|
| 1 | Items with no pending ask upstream and no provisional ancestor |
| 2 | Within rank 1: more downstream items unblocked first (longest path to a sink) |
| 3 | Items on a provisional (T1) branch, while the provisional caps hold |
| 4 | Ties: age in `Ready` (aging boost: +1 rank step per 30 min waiting), then key (deterministic, existing) |

### 6.3 Starvation and deadlock

| Case | Rule |
|---|---|
| A ready item never leased | aging boost (6.2 rank 4) |
| Every remaining item is behind a parked ask | outcome `blocked` (existing `stop_of`), reason names the asks: "no work item can run: w3 (blocked by Q2), w5 (requires w3)" |
| An answer arrives for a blocked outcome | existing auto-unblock (ADR-019 recovery note) |
| Two asks block each other | impossible: asks are leaves |

### 6.4 Expiry and reminders

| Tier | Default expiry | Reminders | At expiry |
|---|---|---|---|
| T1 | 48 h (Q3) | in the next digest only | default stands; caveat "unreviewed assumption" stays on the verdict; row moves to "Reviewed later" |
| T2 | existing `decision_expiry_hours` | at 25%, 75% of the window, each inside a digest, never in quiet hours | HOLD (existing); item stays `waiting_decision`; row moves to "Held" with the existing actions (retry via replanner, cancel) |
| T3 | existing `decision_expiry_hours` | as T2, plus once at creation even outside a digest (not in quiet hours) | HOLD; never yes |

Parked branches: hold no reservation (the attempt ended) and spend nothing. An outcome whose only remaining work is parked releases its owner under the existing keep-alive rules (B3), and is re-adopted on any answer.

---

## 7. No flooding, easy acceptance, the queue

### 7.1 Batching and caps

| Rule | Value [A: proposals] |
|---|---|
| Digest window | one digest per outcome every 2 h of the person's active time, or as soon as 5 T1 rows are open (Q4) |
| Grouping | by outcome, then branch (work item), then kind |
| Dedup | same `dedupe_key` and same `binding_hash` = one row (existing unique index); equal effects across items of one outcome = one row listing every item |
| Max visible rows | 7 per section; the rest behind "N more" (never hidden from the count) |
| Notifications | T0 none; T1 one per digest; T2 one per new ask, folded into the digest if one is due within 10 min; T3 immediately; quiet hours suppress notifications only (queue and badge still update); existing notify caps and payload rule (ids and codes only, T-33) |
| Open asks cap | 50 open rows per outcome; beyond, new asks are refused and the item parks with reason `ask_cap` |
| Ask budget per agent | 3 T2/T3 per attempt (5.3); 10 asks per outcome per hour; beyond, further asks are collapsed into one row "w3 asked 14 times; collapsed" and the agent is told "ask budget used: proceed only with already-allowed defaults or end your turn" |
| Clarifying-question policy (RULES text) | ask only when the answer changes the plan, scope or a permission; otherwise decide, and record the assumption with `default` and `class` |

### 7.2 "Same answer for the rest" scopes

| Property | Rule |
|---|---|
| Allowed for | T1 classes; T0 widening for a class |
| Never for | T2, T3, any ask that would create a grant |
| Bound | one outcome, at most 24 h, at most 5 active scopes per outcome |
| Explicit | the person picks it on a row ("keep this for the rest of outcome X"); the row shows it |
| Revocable | from the queue; revoking does not undo earlier decisions, it flags them for review |

### 7.3 Easy to accept, without rubber-stamping

| Mechanism | T0 | T1 | T2 | T3 |
|---|---|---|---|---|
| Default always shown | applied | applied, provisional | HOLD | HOLD |
| Agent suggestion | n/a | is the default | label only if Q1 = yes; never preselected | none |
| One tap | n/a | Keep | no | no |
| "Keep all N listed" | n/a | yes, only rows visible on the page, count in the button | no (ADR-019: no bulk resolve above R1) | no |
| Impact preview and diff | in the log | yes (`impact`, diff since checkpoint) | yes (existing brief + `attempt.diff`) | yes |
| Undo | revert = "Change" (5.4) until the outcome concludes | 10 s undo toast after Keep / Keep all; then "Change" until conclude | no (answers are final, existing) | no |
| Engagement guard (Enter inert `ARM_MS`, focus stays in composer, Alt+A / click to engage) | n/a | yes for single rows; the digest button needs a click | yes (existing) | yes |
| Nothing preselected; choose then Send | n/a | the default is pre-applied, not preselected in a picker | yes (existing) | yes |
| Brief expanded once (`seen_all`, `send_blocked`) | n/a | no | yes (existing `approvals::Popup`) | yes |
| Extra friction | n/a | n/a | kind-specific (waiver: typed reason) | type the target name (repo/branch/host/cap) to confirm [N]; person-present signature is post-MVP (ADR-019 residual 7) |

Anti-habituation:
- **Spot audit**: 1 in 20 T0 decisions appears in the digest as "spot check: right?" (Keep / Wrong). "Wrong" reverts like "Change" and moves the class to T1 for 7 days (automatic tightening).
- **Weekly review row**: "you kept N, auto-decided M, changed K this week; top classes"; it lists any policy widening since last week.
- **Anomaly highlight**: first-of-class, tainted, over budget, or text with instruction-like or hidden characters. Highlight uses the attention style reserved for owner action (borderless Super Agent rule: highlight only on rows that need the owner); T0 log rows are never highlighted.
- Accessibility: tier and "blocks" are text, not colour only ("T2 · blocks w3, w5"); every action has a key; screen-reader names include tier, kind, outcome, age; motion follows `synt.tui.motion` / reduce-motion.

### 7.4 The "Needs you" queue (first-class surface)

| Surface | What |
|---|---|
| Super Agent rail | badge with the count of T2+T3 open + T1 digest due; click opens the queue in the centre (a frame of the S1 frame stack) |
| Orchestrator chat | each T2/T3 is a decision card at its place in time (super-agent.md 5); a digest card for T1; chat text cannot answer (existing) |
| TUI view | the existing title-bar badge only (super-agent.md 8); same count |
| Notifications | existing notifier, ids and codes only |
| CLI | `synt outcome decisions` gains `--queue` (sections below) and `--export md` (a TODO list with ids, tier, blocks, age, the exact `resolve` command; never brief text beyond cleaned titles) [N] |

Sections and order: **Blocking work** (T3 then T2; inside, more blocked items first, then oldest) · **To review** (T1 digest, oldest first) · **Held** (expired T2/T3) · **Decided for you** (T0 log, collapsed). The existing `approvals::merge` (oldest first) becomes the tie-break inside a section.

Row: tier label, kind, outcome or session, subject (cleaned, capped), "blocks w3, w5" or "blocks nothing", default or HOLD, age, expires in, provisional spend if any. Filters: outcome, kind, tier. Snooze: T1/T2 up to 1 h before expiry, T3 up to 4 h; snooze hides notifications, never the row. Delegate: "ask the orchestrator to research" = the existing `request_evidence` answer (B3 re-brief, max 3).

### 7.5 Headless and CLI when nobody can be asked

| Tier | `synt run` (plain / `--unattended`) | Report |
|---|---|---|
| T0 | apply default | "decided for you" list with policy hash |
| T1 | apply default, provisional | "assumptions to review" list; exit code unchanged |
| T2 / T3 | fail closed (existing `AutoDenied`); continue on other parts; exit 4 if it was needed | "needs you" list = existing `needed_permissions` + agent asks, with ids |

---

## 8. Data model, protocol and seams

### 8.1 Storage (no DDL; `user_version` 5, marker 1010 unchanged)

| What | Where | Integrity |
|---|---|---|
| T1/T2/T3 asks of outcomes | `decisions` rows; new kind value `assumption_review` (T1); existing kinds for T2/T3 | existing approval MAC, CAS on `revision`, binding hash |
| `tier`, `classifier_version`, `policy_hash`, `blocks`, `default`, `suggested`, `effects`, `impact`, `remind_at` | inside `brief` (store-built, so inside `brief_sha256` and the MAC'd binding) | as briefs |
| T0 decisions | chained event `ask.auto_decided {ask, kind, class, choice_sha256, policy_hash, classifier_version, checkpoint_commit}` in the outcome's control session; for own sessions in the session's chain | HMAC chain (events) |
| T1 provisional tag | `evidence.flags` value `provisional:<id>` | evidence MAC |
| Person's Keep / Change on T1 | `decision_approvals` row (person only, as today) | MAC |
| Policy | user/managed settings `synt.approvals` (5.5) | hash recorded per decision; widening needs the confirmation MAC |
| Own-session asks | live `Asks` + v4 `approvals` table (history) | unchanged |

### 8.2 Lifecycle mapping (keeps "only a person resolves")

| Ask status | Decision row | Who moves it |
|---|---|---|
| `auto_decided` (T0) | none (event only) | owner process, from the classifier |
| `provisional` (T1) | `assumption_review` in `pending` | orchestrator creates it (`Preparing → Pending`, existing guard) |
| answered Keep / Change | `Approved` with choice `keep` or `change:<alt>` | `Actor::User` only (existing guard) |
| expired T1 | `Expired`; driver writes caveat, not approval | `Actor::Orchestrator` (existing guard) |
| `parked` (T2/T3) | existing kinds, `pending` | as today |
| `invalidated` | `Invalidated` | as today (binding moved) |

T0/T1 never use `Approved` without a person: the auto record is an event and a caveat, not a resolved decision.

### 8.3 Policy object `synt.approvals` [N]

```json
"synt": { "approvals": {
  "t0_classes": ["naming", "formatting", "test_layout", "comment_wording"],
  "raise": { "ambiguity": "T2" },
  "digest": { "every_minutes": 120, "max_open_t1": 5 },
  "quiet_hours": null,
  "confirmed": "<hmac>"
} }
```

- Read from user and managed settings only; project ignored with a diagnostic. Managed raises are final.
- `t0_classes` outside `T0_ALLOWED_CLASSES` and any lowering below a floor are ignored with a `synt doctor` diagnostic (names only).
- **Widening needs a confirmation**: a change that adds a T0 class takes effect only when `confirmed` = HMAC(chain-key subkey, policy hash), written by the desktop Settings or `synt approvals confirm` after the person sees the diff. A hand edit without it is treated as the previous (stricter) policy. Tightening needs nothing. Residual: a same-user process holding the key can forge it (ADR-019 residual 6).

### 8.4 Events (chained, ids and codes only)

`ask.created {id, kind, tier, blocks, policy_hash, classifier_version}`, `ask.auto_decided` (8.1), `ask.provisional {id, checkpoint_commit}`, `ask.kept|changed {id, by}`, `ask.expired {id, result: default_stands|hold}`, `ask.collapsed {agent, count}`, `ask.tier_hint_ignored {id, hint, tier}`, `approvals.policy_changed {from_hash, to_hash, widened: bool}`. Every one also feeds `policy.decision` audit where the gateway is involved.

### 8.5 Protocol (PROTOCOL stays 4, additive)

- Read `asks.list {outcome?, section?}` → rows of 5.1 (server-built, never computed by a surface; dependency rule of ADR-021 stands).
- Command `decision.resolve` unchanged; T1 adds choice values `keep`, `change:<alt>`; batch `decision.resolve_many {ids[], expected_revisions[], choice: keep}` accepted ONLY when every id is `assumption_review` (store refuses otherwise, whole batch).
- Push hints unchanged (ids, revision, state word).

### 8.6 Seams (file paths)

| Seam | File | Change |
|---|---|---|
| Classifier | `crates/synthesis-core/src/ask.rs` [N] | pure `classify`, floor table, `T0_ALLOWED_CLASSES`, version |
| Approver (sessions) | `crates/synthesis-harness/src/traits.rs`, `exec.rs` | `ApprovalDecision::Deferred(ask_id)` [N] returning the "queued" tool result; input-hash-bound one-shot allow on re-issue |
| Live asks | `crates/synthesis-app/src/asks.rs` | tier and blocks on `AskApproval`; deferred path |
| Attempt asks | `crates/synthesis-app/src/run.rs` (attempt profile), `crates/synthesis-orchestrator/src/decision.rs` (`record_blocked_decision`) | continue after a refused call; batch at settle |
| Decisions store | `crates/synthesis-session/src/outcome.rs` | brief fields 8.1, `assumption_review`, `resolve_many` check |
| Driver | `crates/synthesis-orchestrator/src/decision.rs` (`DecisionDriver`) | T1 expiry = caveat; reminders; `assumption_changed` → `rework_required` |
| Scheduler | `crates/synthesis-orchestrator/src/sched.rs` step 6 | lease order 6.2, provisional caps |
| Gateway | `crates/synthesis-orchestrator/src/gateway.rs` | `asks.list` read, `decision.resolve_many` (user only) |
| Conductor confirm | `synthesis-app/src/conductor/**` [planned S1 P3b] | its auto list becomes the T0 policy for kind 15 (same rules) |
| Verification | `synthesis-core/src/outcome/verdict.rs` [planned V1a] | caveat `unreviewed_assumption` |
| View models | `crates/synthesis-view/src/outcomes.rs` | queue rows, sections, blocked-by |
| Desktop | `apps/desktop/src/approvals.rs` (`Kind`, `Row`, `merge`), `outcomes_view/ask.rs`, Super Agent queue frame | tier, blocks, sections; `Popup` rules unchanged for T2/T3 |
| CLI | `crates/synthesis-app/src/outcome_cmd.rs` | `--queue`, `--export md`, `approvals confirm` |

Downgrade: an older v5 binary meets a decision of kind `assumption_review` or unknown brief fields [A: behaviour to verify; it must refuse that decision, never treat it as approved]. Release note: conclude or cancel outcomes before downgrading (same as S1 and verification).

---

## 9. Threat model

| # | Threat | Control | Test [N] |
|---|---|---|---|
| AP-1 | Prompt injection makes the agent label a security ask as low tier (`class: naming`, hint T0) | Classifier reads typed effects only; hints only raise; `class` only lifts T1 → T0; the engine judges the call anyway | `ap-inj-label-cannot-lower`: a README tells the worker to ask "naming: push to main?"; expected tier T3, nothing pushed |
| AP-2 | Ask spam to induce habituation | Ask budget, collapse, no one-tap above T1, weekly review | `ap-ask-flood-collapses`: 40 asks → 1 collapsed row, 3 T2 decisions, agent told |
| AP-3 | Harmful recommended default | T1 default must be "already allowed + reversible" (store recomputes); T2 suggestion is a label, never preselected; T3 has none | `ap-default-outside-paths-refused`, `ap-t2-suggestion-not-preselected` |
| AP-4 | TOCTOU between ask and apply | Binding (contract, plan, commit, policy, attempt, action hash); one-shot grant from the stored action; session re-issue must match the input hash | `ap-session-reissue-other-input-asks-again`; existing `a_grant_is_refused_and_unusable_once_the_binding_moved_*` |
| AP-5 | Replay of an old approval | CAS on revision, MAC, consumed-once grants (existing) | existing `proto_command_idempotent_*`; `ap-resolve-many-replay-one-effect` |
| AP-6 | Approving an ask whose brief changed | `brief_sha256` in the answer; new revision re-arms the card (existing `Popup::update`) | existing popup tests; `ap-t1-brief-change-rearms` |
| AP-7 | T1 provisional default causes an irreversible side effect | T1 only covers already-allowed reversible actions; irreversible table still asks | `ap-t1-default-cannot-push` |
| AP-8 | Tier downgrade via policy (user, project, hand edit) | Floor in code; project ignored; widening needs the confirmation MAC; managed final | `ap-policy-cannot-lower-floor`, `ap-project-policy-ignored`, `ap-unconfirmed-widening-ignored` |
| AP-9 | Queue flooding / DoS | Open-ask cap, per-attempt and per-hour budgets, notify caps, dedup | `ap-open-ask-cap-parks-item` |
| AP-10 | Social engineering in ask text ("URGENT: approve or data loss") | All text cleaned (hidden chars, bidi), capped, rendered as data; tier, blocks and impact are server-derived; no "critical" badge (ADR-021) | `ap-hidden-chars-stripped`, existing `inj-forged-card-text` |
| AP-11 | "Keep all" used to approve a T2 | `resolve_many` refused unless every id is `assumption_review` | `ap-resolve-many-refuses-t2` |
| AP-12 | Agent works around a queued T2 by another tool | Every call goes through the engine; "do not reach the same effect" rule; red-team | `ap-queued-ask-workaround-still-asks` |
| AP-13 | Silent wrong T0 decisions accumulate | Spot audit 1/20, "Wrong" auto-tightens the class, weekly review | `ap-wrong-t0-tightens-class` |

---

## 10. Measurement

| Metric | Source | Target [A: proposals] |
|---|---|---|
| Median time to answer, per tier | `ask.created` → `ask.kept/changed` / `decision.resolved` | T2 < 4 h active time |
| % asks auto-decided (T0) | events | report only (no target: a high number is not a goal) |
| % T0/T1 later changed or marked wrong | events | < 5%; a class above 10% auto-tightens |
| Blocked time per branch | `ask.created` → item leaves `waiting_decision` | report; compare with idle workers |
| Idle-worker time while an ask is open | scheduler | trend down vs today |
| Wrong auto-decision on a T2/T3 kind | eval + red-team | **0** (any non-zero is a release blocker) |
| Asks per attempt; collapsed rows | events | report |
| Owner satisfaction | the weekly review row "useful? yes/no" | report |

**Classifier eval** (`crates/synthesis-core/tests/ask_corpus.rs` + `tests/approvals-corpus/*.json`) [N]: a seeded corpus of asks (every row of 4.2, injected labels, missing effects, hostile hints, random policies) with the expected tier. Pass: **zero false-lowering** (tier below expected); false-raising is reported. Runs in CI on 3 OSes (pure Rust).

Operations: `synt doctor` checks `synt.approvals` (unknown classes, lowered floors, unconfirmed widening); runbook entries in `docs/testing/outcome-orchestrator-smoke.md`: "queue flooded" (check `ask.collapsed`, lower the agent's budget), "wrong auto-decision" (mark Wrong; class tightens; `synt outcome events --kind ask.*`).

---

## 11. Delivery plan

Each phase is one PR, CI on 3 OSes. SR = security-reviewer. Sup = supervisor.

| Phase | Scope | Files owned | Depends on | Fits | SR | Owner decision |
|---|---|---|---|---|---|---|
| **A0** | This spec; ADR-019 amendment (tiers = attention; T1 `assumption_review`; deferred asks); ADR-021 amendment (queue read; Q1 if yes) | `docs/**` | none | S1 wave 1, V0 | no | Q1-Q5 |
| **A1** (first slice) | Queue view model + "blocked by" from existing data: decisions, `work_item_deps`, item states, session asks; sections, ordering, age, expiry shown; CLI `--queue`, `--export md` | `synthesis-view/src/outcomes.rs` (queue part), `apps/desktop/src/approvals.rs`, `synthesis-app/src/outcome_cmd.rs` | none | S1 wave 1 (render later mounted by P4) | no (read-only) | none |
| **A2** (first slice) | Classifier `synthesis-core/src/ask.rs` + corpus eval; store writes `tier`, `blocks`, `remind_at`, safe default into new briefs; reminders in `DecisionDriver`; `asks.list` read | `synthesis-core/src/ask.rs`, `synthesis-session/src/outcome.rs` (brief build), `synthesis-orchestrator/src/decision.rs`, `gateway.rs`, `proto.rs` (read variant) | A1 | S1 wave 2 (with P2b: both touch `outcome.rs`, P2b first) | yes | none |
| **A3** (first slice) | Non-blocking T2 in attempts: refused call → "queued, continue"; batch at settle; per-attempt budget; lease order 6.2 | `synthesis-app/src/run.rs` (attempt profile), `synthesis-harness/src/exec.rs` (deferred result), `synthesis-orchestrator/src/{decision,sched}.rs` | A2 | S1 wave 2-3 | yes + Sup (changes PR-08 bypass behaviour) | none |
| **A4** | T1: `assumption_review`, provisional checkpoint and flag, Keep/Change, `resolve_many`, `assumption_changed` → B9 replan, caveat in verdict | `synthesis-session/src/outcome.rs`, `synthesis-orchestrator/src/{decision,sched,planrun}.rs`, `verdict.rs` (one caveat line) | A3, **V1a** (caveats) | S1 wave 3, V wave 2 | yes + Sup | Q3 |
| **A5** | T0: policy `synt.approvals`, confirmation MAC, `ask.auto_decided`, spot audit, weekly review, auto-tightening; conductor auto list folded in | `synthesis-core/src/ask.rs`, `synthesis-settings`, `synthesis-app/src/doctor.rs`, conductor policy module | A4, **S1 P3b** | S1 wave 4 | yes | Q2 |
| **A6** | Super Agent UI: queue frame, digest card, one-tap Keep, Keep all, undo toast, snooze, highlight rules, accessibility; T2/T3 cards keep the #167 rules | `apps/desktop/src/superagent/queue/**` [N], `outcomes_view/{ask,render}.rs` | A1, A4, **S1 P4** | S1 wave 4 (after P4) | yes | Q1, Q4 |
| **A7** | Deferred asks in Super Agent own sessions (not TUI): `ApprovalDecision::Deferred`, input-hash one-shot allow on re-issue, host note at drain point | `synthesis-harness/src/{traits,exec,comm}.rs`, `synthesis-app/src/asks.rs` | A2, **S1 P5** | S1 wave 5 | yes + Sup | Q5 |
| **ART** | `ap-*` red-team cases, `docs/SECURITY.md` residuals | `tests/redteam/cases.mjs`, `docs/SECURITY.md` | each phase | S1 P6, VRT | yes + Sup | none |

Which existing phase each ask kind belongs to: kinds 2, 3, 5-8, 10 (existing, gain tier/blocks in A2); 9 plan cards (S1 P4); 11 (S1 P3b/P4); 12 and 17 (A3 for T2, A4 for T1); 13 waivers (V1c, shown in queue by A1, cards by V6); 14/14b worker messages (S1 P2c, P3b); 15 conductor proposals (S1 P3b, policy in A5); 16 report review (S1 P2b); 18 (A5); 1 and 4 in sessions (A7); 19, 20 (unchanged; listed in the queue by A1).

Parallelism: A1 runs now beside S1 wave 1 (disjoint files except `synthesis-view/src/outcomes.rs`: A1 adds a new section; P2b and P2c rebase). A2 waits for P2b's `outcome.rs` change. A3 and V1b are disjoint. A6 and V6 both touch `outcomes_view/render.rs`: V6 first, A6 rebases.

**Minimal first slice (A1 + A2 + A3)** already gives: a visible queue that cannot lose an outcome ask, "blocked by" on every item and ask, a tier and safe default on every new decision, reminders and expiry rules, and workers that keep working after a refused call while other branches run first. No auto-decision exists yet, so no new authority path exists.

**Rollback**: A1 is read-only. A2-A5 write only new brief fields, a new kind value and events; an older binary must refuse unknown kinds [A]; cancel or conclude outcomes before downgrading. A3 can be switched back to "first refusal ends the attempt" by one constant (kept for one release).

---

## 12. Open questions for the owner (defaults apply if unanswered)

| Q | Question | Default (applied if unanswered) |
|---|---|---|
| Q1 | **DECIDED by the owner 2026-11-18: yes.** May a T2 card show the agent's suggested option (amends ADR-021 / threat T-34 "no recommended field")? | **Yes**, labelled "agent suggests (untrusted)", never preselected, still choose then Send; never on T3 |
| Q2 | Which classes are T0 out of the box? | `naming`, `formatting`, `test_layout`, `comment_wording`; everything else starts at T1 |
| Q3 | T1 expiry and caps | 48 h, then the default stands with a permanent caveat; provisional spend at most 25% of the outcome cap, at most 2 stacked levels |
| Q4 | Digest cadence and quiet hours | Every 2 h of active use or at 5 open T1 rows; quiet hours off until set; T3 notifies at once outside quiet hours |
| Q5 | Non-blocking (deferred) asks in Super Agent own sessions | **Yes** in Super Agent sessions; the TUI view keeps its blocking dialogs unchanged |

---

## 13. Assumptions [A] and follow-ups for the planner

1. Older v5 binaries refuse a decision of an unknown kind or with unknown brief fields rather than mis-reading it: verify in `synthesis-session` before A2 ships (same open item as S1 and verification).
2. The decision → work item link used for `blocks` is derivable from the binding's attempt (no `work_item_id` column on `decisions`): confirm in `outcome.rs` before A1.
3. All numbers (3 asks per attempt, 10 per hour, 50 open, 25%, 2 levels, 48 h, 2 h digest, 1/20 spot audit, 10 s undo) are proposals, not measured; A1-A3 events give the first data.
4. `synthesis-core` can host `ask.rs` without new dependencies; if the permission `Verdict` must be read, the mapping lives at the seam, not in core.
5. HMAC under a chain-key subkey for the policy confirmation reuses the existing key handling (`chain.key`); verify no new key file is needed.
6. Follow-ups: add this spec to `docs/specs/README.md`; ADR-019 and ADR-021 amendments (A0); a decision whether `close` and `message_worker` stay outside every policy (they do in this spec); revisit O2 (one durable `asks` table) if lost session asks are reported.
