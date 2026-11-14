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
- Decisions are **durable data, not live asks** (owner point 5). Today `ask.approval` is live: it exists only while a process waits, has no expiry and no staleness check (`synthesis-app/src/asks.rs:55-59`, `proto.rs:308`; the in-attempt ask is denied when nobody can answer, `lib.rs:2455`). A Decision is a stored row any surface can list and answer, also after a restart or when the surface that was open has closed.
- Resolution is by a person only, through the command `decision.resolve(id, expected_revision, choice)` (ADR-016 item 11, ADR-021). **Exactly one answerer**: the owner applies the first valid command by compare-and-swap on `decisions.revision`; every other command for the same decision receives `already_resolved` (with the stored choice and surface) or `stale`. The choice set is alternatives plus `reject`, `defer`, `request_evidence`, `edit_scope`; never a bare approve; no default selection; no bulk resolve for kinds above R1.
- **Approval executes as a scoped one-shot grant.** The attempt had been denied (non-interactive); on approval the orchestrator starts a new attempt (new session) with exactly one extra allow rule whose text equals the approved action (the existing mechanism that lifts the irreversible table is a USER-level rule or CLI flag, `docs/SECURITY.md:21`). The grant row records `decision_id`, `action_hash`, expiry, and is consumed on use. A changed commit, plan, contract or policy hash invalidates the Decision and its unused grant (`Invalidated`, event `decision.invalidated`).
- The existing per-tool `approvals` table (`schema.rs:57`) stays for interactive sessions; outcome Decisions are separate.

### 5. Races (normative)

| Race | Outcome |
|---|---|
| Two surfaces resolve the same decision at once | CAS on `revision`: one `applied`, the other `already_resolved`; one `decision_approvals` row; the loser's UI refetches and shows who and what |
| Resolve vs invalidation (new commit, plan, contract or policy hash) | Invalidation bumps the revision: the resolve gets `stale`; nothing is granted |
| Resolve vs expiry | Expiry is an owner transition, also CAS: whichever commits first wins; an expired decision is never approved |
| Resolve while no owner runs | The command waits in `outcome_commands`; the owner re-checks bindings when it starts (stale -> `stale`); the surface shows "queued" |
| Same command sent twice (retry, two clients behind one idempotency key) | Same stored result, one effect |
| Resolve from an attempt, a hook or tool output | Impossible: attempts have no channel and the sandbox denies the endpoint and the DB; the command carries `from_surface` and only the protocol transport of a surface may set it |
| Decision answered while its attempt was already restarted | Binding includes the attempt ordinal and `subject_commit`; mismatch is `stale` |

### 6. One gateway for every attempt, no surface side paths (owner point 8)

The gateway, the launch profile and the closed bypass paths above apply identically whatever surface created the outcome: the surface only names a workspace, a contract and a person's commands. Surfaces create no side paths: no surface-side `!cmd`, `/permissions allow` or `Always` choice affects an orchestrated attempt (those act on the surface's own chat session, not on the contract's compiled rules), and no surface applies decisions by editing settings.

**Gate.** `synt.orchestrator.enabled` cannot be turned on, and PR-05's runner is not enabled in any surface, until the bypass table in section 2 is implemented and its tests pass: harness-runtime tools (`AskUserQuestion` -> decision), `bypassPermissions` never granted, no `!cmd` channel, hooks tighten-only, MCP stdio and browser **excluded** from attempts by the tool allow-list (so their lack of sandboxing, `docs/specs/sandbox-hardening.md` section 4, is not reachable), git aliases and repo scripts closed by not granting `auto` or `bypassPermissions` (the only modes where the red-team suite records them as known gaps, `docs/SECURITY.md:102`). This is a release gate, not a follow-up.

## Consequences

- Positive: no new engine; deny-first semantics and golden tests are inherited; restart-safe (no live ask); approvals are auditable and revision-bound.
- Negative: an approved action costs a new attempt (one re-run) instead of resuming mid-turn; envelope compilation is new code that must be fuzzed against the "no allow for irreversible actions" rule; the auto judge is unavailable in attempts.
- Residual (stated, not hidden): sandbox backends are Weak on macOS/Linux filesystem; MCP and browser are outside the sandbox; a program the agent builds can still act inside the sandbox's limits (`docs/SECURITY.md:22`).

## Revisit trigger

- A feature needs mid-turn human input (interactive pair mode inside an outcome).
- A second approver role (RBAC, separation of duties) is required.
- Evidence from nightly evals shows the auto judge is reliable enough to use in attempts.
- A formal policy language is requested (re-open Cedar).

## Integrity of approvals (PR-02 review)

A decision approval is a row with an HMAC over the decision, outcome, kind, revision, binding hash, expiry, brief hash, approver, choice and time; the resolved state is derived from the MAC'd choice, and a mismatch is `Tampered`. Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. PR-10 must re-check the binding against the current state and tie every approval to its chained `decision.resolved` event (which carries `outcome_id` and `binding_hash`) before it grants anything. `from_surface` and the approver are self-asserted until the orchestrator endpoint authenticates its clients (PR-12) and, for irreversible decisions, a person-present signature exists (post-MVP).

User and managed hooks still run on the host, outside the sandbox, in an attempt (only project hooks are excluded). This sentence is part of what is shown at enable time.

## Implementation notes (PR-08: envelope compiler, gateway, non-interactive attempts)

- **Compiler** (`synthesis_orchestrator::compile_envelope`, pure): the envelope becomes `--allow`/`--deny` rules in the CLI layer (a managed deny still wins; deny beats every allow), `--network deny|hosts` and permission mode **`default`**. The mode is never `acceptEdits`: it would allow every edit in the worktree whatever `allowed_paths` says, so writes are allowed by explicit `Edit(/<path>)` rules (`repo.write: allow`) and every other edit asks, which in an attempt becomes a Decision. `denied_paths` become `Read/Grep/Glob/Edit(/<path>)` denies (the engine also applies them to shell commands it can parse). An unlisted capability is denied. `git.push`, `secrets.read`, `deploy.production`: `allow` is refused at compile time; `deny` emits explicit deny rules; `approval_required` emits nothing and leaves the engine's irreversible table, which asks. `network.outbound`: `deny` denies `WebFetch`/`WebSearch`/`curl`/`wget` and the OS sandbox network; `allow` is `--network hosts` (hosts only from user or managed settings). `tests.run: allow` allows only the commands the user's settings list as tests, and only plain invocations of known runners (anything else, any shell syntax, is refused: such a rule could lift the irreversible table). The policy hash in a Decision's binding is a hash of the compiled rules (not of the user's test commands).
- **Gateway for attempts.** Every tool call of an attempt goes through `Permissions::check` with these rules inside the worker; the profile adds no other way in. The orchestrator-side `OrchGateway` (authorize table for orchestrator effects) is not built in this PR: its first user is PR-10/12.
- **Non-interactive attempts.** A tool call that needs approval is refused to the model ("requires approval; no approver available"), the task ends `blocked` (exit 4, reason `approval_unavailable` or `needs_user_input`, the rules it needed in `task.ended.needed_permissions`). When the runner settles a blocked attempt it reads that from the attempt's session and stores a **Decision** (`record_blocked_decision`): kind `approval_needed` (rules and the refused calls), `ambiguity` (the question and its options) or `blocked`; state `pending`, expiry within the settings limit, binding = contract version, plan version, subject commit, policy hash, action hash, attempt ordinal; no pre-selected choice, safe default HOLD. Text from the model and tools is data: truncated and stripped of control characters. The work item goes `waiting_decision`. Answering it and the one-shot grant are PR-10.

| Path | Decision (PR-08) | Test |
|---|---|---|
| `!cmd` | No input channel: a task text starting with `!` is plain text; the model call is the only way in | `the_bypass_paths_are_closed_for_an_attempt` |
| `AskUserQuestion` | Ends the attempt `blocked`; the question and its options become an `ambiguity` Decision | `ask_user_question_becomes_a_decision_not_a_prompt` |
| `ExitPlanMode` | **Not a no-op** (changed from the table above): an attempt never runs in plan mode and nobody can approve a plan, so the call blocks the attempt with a Decision | same test as `!cmd` |
| `ToolSearch` | Read-only; there are no deferred (MCP) tools to load; it makes no decision | same test |
| `bypassPermissions`, `auto` | Refused by the binary (`--unattended` and `--attempt-session`); the profile never emits them | `an_attempt_refuses_auto_and_bypass_modes_whatever_the_caller_says` |
| Hooks | Project hooks are untrusted and not run; user and managed hooks run on the host (stated at enable time) | `an_attempt_starts_no_mcp_server_and_runs_no_project_hook` |
| MCP | No server is started in an attempt | same test |
| Browser | The browser tools are not registered in an attempt | `the_bypass_paths_are_closed_for_an_attempt` |
| Auto judge | Off: mode is `default`, `auto` refused | mode test above |
| `Always` allow | No approver, no dialog; the settings files are unchanged after a blocked attempt | `the_bypass_paths_are_closed_for_an_attempt` |
| Git aliases, repo scripts | Run only with approval, which nobody can give: refused and recorded | `the_bypass_paths_are_closed_for_an_attempt` |

- **Red-team** (`tests/redteam/cases.mjs`, run in all four modes where the rule is a deny, `default` where allowed paths matter): `orch-attempt-ctl-write-inside` (positive control), `orch-attempt-write-outside-allowed`, `orch-attempt-write-denied-path`, `orch-attempt-push`, `orch-attempt-secrets-read`, `orch-attempt-network`, `orch-attempt-readme-injection`. The flags in the file are what the compiler emits; the test `the_red_team_cases_use_what_the_compiler_emits` keeps them in step. Real-binary attempt tests are in `apps/cli/tests/outcome_attempt.rs`.
- **Residuals, stated.** `allowed_paths` is enforced for the file tools and for shell commands only as far as the engine parses them; a program the worker builds and runs inside the sandbox can write anywhere in the worktree (the OS sandbox confines the worktree, not the path list). PR-09's verification must check the changed files of the commit against the envelope. Denied path rules are matched against paths relative to the worktree. The enable gate in section 6 is satisfied by these tests, but `outcome enable` itself arrives with the CLI (PR-13).

### PR-08 review fixes

- **Intersection is implemented, not just stated.** In an attempt (`--attempt-session`) the allow rules and the user-allow set are the compiled flags only. `permissions.allow` of the user and managed settings is ignored (it could allow edits outside `allowed_paths`, or lift the irreversible table for an `approval_required` capability); deny and ask rules still come from every layer, so a managed deny wins. `permissions.additionalDirectories` and `--add-dir` are ignored or refused (no extra writable root), a managed `defaultMode: acceptEdits` is ignored and `acceptEdits` is refused (it allows edits outside the contract), and reads outside the working directories are blocked. Tests: `user_settings_cannot_widen_the_compiled_envelope_of_an_attempt` (fails if the filtering is removed) and the 648-combination sweep, which now sets `user_allow` and `worktree` like the binary and probes `Write`, `Edit`, `MultiEdit`, `NotebookEdit`, `WebFetch`, an MCP tool and `Task` with one explicit expectation per tool (edits outside the paths: Ask, or Deny when writing or reading is denied; WebFetch: Deny when the network is denied, never Allow; MCP: Ask; sub-agents run under the same rules: Allow by design).
- **Path patterns** are `/`-separated, non-empty segments (`*` within a segment, `**` across): no empty, `.` or `..` segment, no leading `/`, `~`, drive, backslash, parenthesis, or path-separator lookalike and invisible characters (`.//x`, `a//b`, `x/` are refused by the envelope validator and again by the compiler: `.//etc/x` used to compile to the absolute rule `Edit(//etc/x)`). A `denied_paths` entry that does not end in `**` denies the path and everything under it. `allowed_paths: src` allows only a file named `src`; write `src/**`.
- **Decisions.** Text from the model is stripped of control, bidi, zero-width, line/paragraph separator and tag characters and shortened; the **action hash is over the full action** (the needed rules and the refused calls untruncated) and the brief shows the digest and whether text was shortened. **PR-10 must create a grant from the stored full action (the worker's `task.ended` and call events), never from the brief text.** A Decision binds to the contract version and policy hash stored on the attempt at start (`attempts.bound_contract_version`, `bound_policy_hash`, DDL revision 1005), not to the outcome's current contract. `grant_once` is offered only when the contract does not deny the capability and the tool can exist in an attempt (no `browser_*`, no `mcp__*`); otherwise the only alternative is a retry, and a changed contract is a new contract version. PR-10 must re-check this before granting anything.
- **ExitPlanMode** ends the attempt `blocked` with a `blocked` Decision whose brief says it asked to leave plan mode (test `bypass_exit_plan_mode_blocks_with_a_blocked_decision`; one test and one script per bypass path, each asserting the Decision kind and the refused calls).
- **Residuals, stated.** `repo.read: deny` also denies `Read`, `Grep`, `Glob` and `LS`, but read-only shell commands (`cat`, `ls`) stay auto-allowed by the engine; reads of a denied path through an interpreter (`python -c`, `node -e`) are not parsed; the engine denies listing a directory that contains a denied path; `secrets.read: deny` covers five path patterns (`.env`, `.env.*`, `~/.ssh`, `~/.aws`, `~/.gnupg`), not every secret store. The OS sandbox is the boundary for all of these.
- **Disclosure at enable time** must say: user and managed hooks run on the host outside the sandbox in an attempt; user and managed `permissions.allow` rules are ignored for attempts (the contract's envelope is the whole allow list); sandbox backends are Weak on macOS/Linux filesystem reads.
- **Red-team note.** The exploit tests for `.//` and for user-settings allow rules are Rust tests (`hostile_path_patterns_are_refused_twice`, `user_settings_cannot_widen_the_compiled_envelope_of_an_attempt`): `tests/redteam/run.mjs` runs plain `synt run`, where user allow rules legitimately apply and no compiler runs, so it cannot express them.
