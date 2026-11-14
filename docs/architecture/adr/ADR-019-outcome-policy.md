# ADR-019: Policy gateway, grants, authority envelope and decisions

Status: Proposed (2026-11-14). Touches the permission model: security review required before acceptance (CLAUDE.md, global rules).
Related: ADR-007 (policy gate before tools), ADR-011, ADR-017, ADR-020. Specs: `docs/specs/permissions.md`, `docs/SECURITY.md`.

## Context

The design wants one deterministic, default-deny gateway before every privileged effect, an LLM that cannot grant itself anything, a human authority envelope, scoped grants, and approvals bound to a revision.

What exists: `Permissions::check` (`synthesis-permissions/src/lib.rs:2453`) and `decide_tool` (`:2509`) called before execution (`synthesis-harness/src/exec.rs:744-751`); six modes (`lib.rs:183-190`); deny wins over every allow; a read-only hard deny that no mode lifts (`:2678`); an irreversible table that asks/denies in every mode including bypass and is lifted only by a USER-level rule (`irreversible.rs`, `docs/SECURITY.md:21-22`); settings layering managed > CLI > local > project > user, lists unioned, project file can only tighten unless trusted (`synthesis-settings/src/lib.rs:1-14`); the egress proxy and sandbox; asks answered by a person only (`docs/specs/desktop.md` section 8). Bypass paths and their resolution are tabulated in the [gap analysis, section 4](../outcome-orchestration-gap-analysis.md). Today `Choice { Once, Always, Deny }` (`synthesis-app/src/asks.rs:55-59`) has no expiry and no binding to a revision, and a non-interactive environment turns every ask into a deny (`lib.rs:2455`).

## Options

A. Keep the permission engine as the single gateway; compile the contract's authority envelope into its inputs (rules, mode, sandbox policy, budgets); add a thin host gateway for orchestrator effects; make attempts non-interactive and turn denials into durable Decisions.
B. New policy engine (Cedar or custom) beside the permission engine.
C. Let attempts ask live (interactive asks over the stdio protocol).

## Decision

Option A. B duplicates a hardened, golden-tested engine (`synthesis-permissions/tests/golden.rs`) and adds a dependency; C does not survive a restart and blocks a worker on a person.

### 1. Gateway

- **Attempt effects** keep going through `Permissions::check` inside the attempt's harness. The envelope is compiled into: `--allow/--deny` rule list (CLI layer, so a managed deny still wins and an envelope allow cannot override a deny), permission mode (never `bypassPermissions`, never `auto` unless the user's setting already allows it for unattended use), sandbox policy (writable root = worktree, network deny/hosts), budgets (`Budget`, cost cap).
- **Orchestrator effects** (worktree create/cleanup, evidence runs, decision resolution, later merge/push) go through `OrchGateway::authorize(principal, action, resource, context) -> Allow | Deny(reason) | Escalate(decision_kind)`. Default deny for an unknown action. Implementation is a pure table plus the envelope; it does not call a model. Principals: `user`, `orchestrator`, `attempt:<id>`. A principal never authorizes itself: `orchestrator` cannot resolve a decision, `attempt` cannot call the gateway at all (it has no channel to it).
- Every call writes a `policy.decision` event (allow and deny) with the policy version hash; denials and escalations are never subject to attention-budget suppression.

### 2. Closing the bypass paths for attempts

| Path | Rule inside an orchestrated attempt |
|---|---|
| `!cmd` (`user_cmd.rs:195`) | No input channel; not reachable. |
| Harness-runtime tools (`exec.rs:566-574`) | `AskUserQuestion` returns a structured "needs decision" result: the attempt ends `Blocked` with reason `needs_decision` and the question becomes a Decision. `ExitPlanMode` is a no-op. `ToolSearch` is read-only. |
| `bypassPermissions` (`lib.rs:1821`) | Mode never granted; the launch profile refuses it (already true for `--unattended`, `docs/UNATTENDED.md:27`). |
| Hooks (`exec.rs:596-660`) | User/managed hooks only; project hooks stay untrusted (project trust is never granted to attempts). Hook `allow` stays ignored (`:648`). |
| MCP | Only servers from managed or user settings; project MCP refused; stdio servers run outside the sandbox today (`docs/specs/sandbox-hardening.md` section 4), so MVP attempts run with **no MCP servers** unless the user lists them in the contract and accepts the stated weaker isolation. Destructive MCP asks (`mcp_guard`) escalate. |
| Auto judge | Off for attempts in MVP (a model must not be the authority); revisit with evidence from the nightly evals. |
| `Always` allow | Disabled: the dialog offers no rule suggestions in an attempt; a recurring need becomes a *policy change proposal* (a Decision of kind `policy_change`), never a silent widening (design section 16). |

### 3. Authority envelope

A contract carries an envelope (design section 13 example): `allowed_paths`, `denied_paths`, `capabilities` (`repo.read`, `repo.write`, `tests.run`, `network.outbound`, `git.push`, `secrets.read`, `deploy.production`, ...), `budgets`, `quality_gates`. Compilation to existing machinery:

| Envelope field | Becomes |
|---|---|
| allowed/denied paths | `Edit/Write/Read` allow/deny rules; denied paths also added to the sandbox `deny_paths` |
| `repo.write: allow` | write root = worktree (always), `acceptEdits`-class behaviour |
| `network.outbound` | `NetworkPolicy::Deny` or `Proxied` with hosts from user/managed settings only (project and `--allow-host` are refused, `docs/UNATTENDED.md:27`) |
| `git.push`, `secrets.read`, `deploy.*` | must be `deny` or `approval_required`; `allow` is **refused at compile time** (never emit an allow for an irreversible-table action) |
| budgets | `Budget` (`synthesis-events/src/types.rs:76-85`), cost cap (required), `max_parallel_workers`, `max_task_attempts` (work item retry) |

**Precedence.** Effective policy = managed settings (final) intersected with user settings intersected with envelope. The envelope can only restrict what the user and managed layers already permit; it can never grant a capability a layer above denies. The envelope itself is human-approved content, hashed (`contract_hash`), and immutable once approved; a change is a new contract version and re-approval. Risk tiers R0-R4 map as: R0/R1 inside the envelope and sandbox = automatic; R2 (irreversible table: push, publish, deploy) = deny unless the contract says `approval_required` and then Decision; R3/R4 = deny, no MVP path.

### 4. Decisions and approvals

- A Decision is created only by the orchestrator, from a recorded denial, a failed budget, an unresolved ambiguity raised by `AskUserQuestion`, or a `policy_change` proposal. Table `decisions` (ADR-016) holds kind, dedupe key, brief (JSON: why a human, delta from contract, alternatives with tradeoffs, evidence refs, unknowns, safe default = `HOLD`), and the **binding**: `contract_version`, `plan_version`, `subject_commit`, `policy_hash`, `action_hash` (sha256 of the canonical action), `expires_at`.
- States: `Preparing -> Pending -> Approved | Rejected | Expired | Invalidated`, `NeedsMoreInfo -> Pending (new revision)`. A timer moves `Pending` to `Expired` (safe default HOLD; never an auto-approve).
- Resolution is by a person only, through `decision.resolve(id, revision, choice)` (ADR-021). The choice set is alternatives plus `reject`, `defer`, `request_evidence`, `edit_scope`; never a bare approve; no default selection; no bulk resolve for kinds above R1.
- **Approval executes as a scoped one-shot grant.** The attempt had been denied (non-interactive); on approval the orchestrator starts a new attempt (new session) with exactly one extra allow rule whose text equals the approved action (the existing mechanism that lifts the irreversible table is a USER-level rule or CLI flag, `docs/SECURITY.md:21`). The grant row records `decision_id`, `action_hash`, expiry, and is consumed on use. A changed commit, plan, contract or policy hash invalidates the Decision and its unused grant (`Invalidated`, event `decision.invalidated`).
- The existing per-tool `approvals` table (`schema.rs:57`) stays for interactive sessions; outcome Decisions are separate.

## Consequences

- Positive: no new engine; deny-first semantics and golden tests are inherited; restart-safe (no live ask); approvals are auditable and revision-bound.
- Negative: an approved action costs a new attempt (one re-run) instead of resuming mid-turn; envelope compilation is new code that must be fuzzed against the "no allow for irreversible actions" rule; the auto judge is unavailable in attempts.
- Residual (stated, not hidden): sandbox backends are Weak on macOS/Linux filesystem; MCP and browser are outside the sandbox; a program the agent builds can still act inside the sandbox's limits (`docs/SECURITY.md:22`).

## Revisit trigger

- A feature needs mid-turn human input (interactive pair mode inside an outcome).
- A second approver role (RBAC, separation of duties) is required.
- Evidence from nightly evals shows the auto judge is reliable enough to use in attempts.
- A formal policy language is requested (re-open Cedar).
