# ADR-017: Orchestration model on top of sessions

Status: Proposed (2026-11-14). **Amends ADR-009** ("Defer complex multi-agent orchestration"), which ADR-012 already amended for messaging only. Needs an owner decision (plan D1).
Related: ADR-012, ADR-016, ADR-018, ADR-019, ADR-020.

## Context

The design separates Outcome, Plan, Task, Attempt, Worker, Agent, Session and Worktree. The repo has Session (14-state machine, `synthesis-core/src/session.rs:17-76`), Task (one prompt-run: `tasks` table, `task.started/ended`), Agent (`AgentDef`), Worktree (`GitInfo`), sub-agents (`harness/src/agents.rs`, `hub.rs`) and peers/delegation (`harness/src/peer.rs`, `MAX_IN_FLIGHT = 4` at `:44`, rounds/threads caps in `synthesis-peers/src/lib.rs:224`, e2e `apps/cli/tests/e2e.rs:2041-2208`). There is no scheduler, lease, heartbeat or watchdog for work (only the desktop's engine-silence watchdog, `apps/desktop/src/remote.rs:691`).

## Options

A. New orchestrator that launches each attempt as an ordinary governed session through the existing run/host code path.
B. Build the orchestrator on peers/delegation (sessions message each other).
C. Run attempts as in-session sub-agents of one long session.

## Decision

Option A. The orchestrator is a host-side program; it is not an LLM and not a session participant.

### Vocabulary (normative)

| Term | Meaning here | Existing or new |
|---|---|---|
| Outcome | User-intended result: contract version, plan, state | new |
| Contract | Versioned, approved intent, criteria, authority envelope; immutable once approved (new version = new row) | new |
| Plan | Versioned DAG of work items for one contract version | new |
| Work item | One schedulable unit (the design's "Task"). Never called task in code or UI | new |
| Attempt | One execution of a work item; owns exactly one session | new |
| Task | One prompt-run inside a session (unchanged) | existing |
| Session | Conversation and tool trace; an attempt's session is a normal child session (`sessions.parent_session_id` = control session) | existing |
| Worker | The process role that hosts an attempt's session: `local-inproc` or `local-engine` (stdio engine). Stored as `attempts.worker_kind`; no separate type in MVP | new column |
| Agent | Model + prompt + tools profile (`AgentDef`) | existing |
| Worktree | Git checkout of an attempt (`GitInfo`) | existing |
| Control session | Hidden session carrying the outcome's events and lock | new (ADR-016) |

### State machines

Pure tables in `synthesis-core`, same style as `SessionStatus::can_transition_to` (`session.rs:48-67`): anything not listed is rejected.

- **Outcome**: `Draft -> ContractPending -> Ready -> Running -> Verifying -> Verified -> Completed`; `Running <-> Blocked`; `Verifying -> ReworkRequired -> Running`; `Running -> Failed`; any non-terminal `-> Cancelled`. `Verified` is set only by the verdict function of ADR-020; `Completed` only by a delivery condition (MVP: patch/branch ready, no merge).
- **Work item**: `Created -> Ready -> Leased -> Running -> Verifying -> Succeeded`; `Running -> WaitingDecision -> Ready`; `Verifying|Running -> RetryableFailure -> Ready` (retry budget); `-> TerminalFailure`; `Leased|Running -> Recovering -> Ready|Blocked`; any non-terminal `-> CancelRequested -> Cancelled`.
- **Attempt** (coarse; the fine-grained state is the session's): `Starting -> Running -> Finished(task status) | Lost | Cancelled`. `Finished` records the session task's `TaskStatus` (`Completed|Unverified|Blocked|Failed|Cancelled`, `synthesis-events/src/types.rs:118`) and the in-attempt verification report id.
- **Decision**: see ADR-019.

Mapping of attempt end to work item: `Completed` + independent evidence (ADR-020) -> `Succeeded`; `Unverified`/`Failed` -> `RetryableFailure` while budget remains, else `TerminalFailure` and the outcome goes `Blocked` with a decision brief; `Blocked` with a policy denial -> `WaitingDecision`.

### Scheduler

In-process, one orchestrator per outcome (the control-session lock). Algorithm: select `Ready` items whose predecessors are `Succeeded`; respect `max_parallel_workers` from the envelope and a global cap; allocate in canonical id order; create the attempt row and outbox `worktree.create` in one transaction; dispatch after commit. Plans are validated by a pure function: unknown dependency, cycle, unknown capability, uncovered mandatory criterion are rejected. Dependency types in MVP: `requires` only (design also lists `produces`, `blocks`, `may_parallelize_with`; deferred). Resource locks (design section 10) start with one lock: serial integration into the outcome branch.

### Leases, liveness, recovery

- **Local liveness is the OS lock** of the attempt's session (`store.rs:1597-1615`, released by the kernel on death; `recover.rs:184-188` already treats `Locked` as live). No heartbeat table in MVP.
- `attempts.fence` is a monotonic counter per work item, passed to the attempt; a late write from a stale attempt (evidence, artifact registration) with an old fence is rejected.
- **Startup recovery** (extends `recover.rs:163`): load outcomes in `Running|Verifying|Blocked`; for each attempt in `Starting|Running` open the session lock: held = alive, else mark session `Interrupted` (existing path), attempt `Lost`, work item `Recovering`; re-check policy version and evidence freshness; replay outbox; produce a visible recovery report (event `outcome.recovered`). Irreversible effects are never replayed: they are not outbox kinds in MVP.
- **Cancel**: set `CancelRequested`, call the existing stop path (`stop_requested_at`, `store.rs:781`; process-tree kill `synthesis-terminal/src/lib.rs:493`), then cleanup only when the process has exited and the worktree has no unsaved work (`worktree.rs:628`).

### Relation to peers, delegation, sub-agents

- Peers stay person-driven messaging (ADR-012). The orchestrator does not use peer sockets; attempts do not message each other. Inter-attempt data flows as work item outputs (artifact ids, commit ids) recorded in the DB, never prompt-to-prompt.
- Sub-agents inside an attempt are unchanged and count against the attempt's budget.
- The delegation rule "a delegate's mode is capped by the sender's" (e8c) is generalised: attempt permission mode = min(user's mode, envelope), and never `bypassPermissions` (ADR-019).

### Where it runs

`synthesis-orchestrator` is a library hosted by (a) `synt outcome ...` (headless, survives the terminal like any process), (b) the desktop in process, (c) `synt engine --stdio` for WSL (protocol v4, ADR-021). A separate daemon is not built (design doc section 13 stays open); closing the desktop window ends an in-process orchestrator exactly as it ends an in-process session today, and recovery resumes it. This is stated in the UI ("runs while the app is open"), per design section 26.

## Consequences

- Positive: reuses the governed run path, caps and sandbox; crash model is the existing one; no new IPC.
- Negative: ADR-009 amended; one more crate; a headless process must stay alive for background progress; the control session borrows `SessionStatus`.
- Risk: scheduler bugs multiply cost. Mitigation: cost cap required per outcome and per attempt (`run.rs:859`), bounded retries, MVP parallelism 2.

## Revisit trigger

- An outcome must survive closing the desktop (-> daemon ADR).
- Remote workers (-> `WorkerTransport`, heartbeats, fencing over the wire).
- More than ~50 work items per plan or fairness across outcomes (-> priority queue).
- Peers prove sufficient for a use case the owner wants (-> merge the models).
