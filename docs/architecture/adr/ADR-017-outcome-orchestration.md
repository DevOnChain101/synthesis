# ADR-017: Orchestration model on top of sessions

Status: Proposed (2026-11-14, revised for three surfaces). **Amends ADR-009** ("Defer complex multi-agent orchestration"), which ADR-012 already amended for messaging only. Needs an owner decision (plan D1).
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

### Where it runs (revised 2026-11-14: three surfaces, one core)

Scope: the orchestrator serves **all three surfaces**: the CLI (the `synt` binary: subcommands and the terminal app), the TUI (the desktop's terminal-style tab) and the Super Agent (the desktop's Agent tab, "Coming soon" today, `apps/desktop/src/ui.rs:29-33`). Rules (the full list is the "Multi-surface rules" section of the [gap analysis](../outcome-orchestration-gap-analysis.md)):

1. **One core.** Orchestrator, scheduler, decisions, evidence and policy compilation live only in core crates: `synthesis-core` (types, pure rules), `synthesis-session` (store), the new `synthesis-orchestrator` (logic), `synthesis-app` (protocol server and client library). `apps/*`, `synthesis-cli-ui*` and `synthesis-view` contain rendering, input and command dispatch only. Enforced by a dependency test (plan, PR-11).
2. **Lifetime independent of any surface** (owner point 3, supersedes the earlier "in-process, no daemon" default D4). Closing a surface never cancels an outcome; reopening from another surface shows the same state.

Options for lifetime:

- **L1 in-process** in whichever surface started it: dies with it (closing the TUI window, a terminal) and needs an explicit resume. Fails the rule.
- **L2 standalone orchestrator process**: one process per user and `$SYNT_HOME` that holds the control-session locks, runs attempts and serves protocol v4 on a local endpoint; surfaces are clients.
- **L3 `synt engine --stdio` per surface**: it ends on stdin EOF and owns exactly one session (`synthesis-app/src/engine_server.rs:1-4,1156`), so it is the wrong lifetime.

**Decision: L2, with a guaranteed fallback.**

- **Process.** `synt orchestrator --serve` (hidden, internal, like `engine --stdio`). A surface starts it on demand, detached (own session/process group, no tty), when an outcome is started or an owner is needed; it exits after N idle minutes (default 10, `synt.orchestrator.idle_exit_minutes`) with no active outcome, no pending command and no client. State is durable, so the process may die at any time: any surface can start it again and recovery (below) runs. This is also the "explicit resume path": `synt outcome resume ID` starts the process and attaches.
- **Endpoint.** A unix socket (Windows: named pipe) under `$SYNT_HOME/run/`, NDJSON frames and the mutual HMAC handshake already built for peers (`synthesis-peers/src/net.rs`, key file in `run/`, same OS user; per-OS code exists). A new key `run/orch.key`. No TCP, no HTTP. `run/` is on the sandbox deny list (`synthesis-sandbox/src/lib.rs:490-491`), so attempts cannot connect (to be proven by a red-team case, threat T-32). Same-user processes outside the sandbox remain out of scope (ADR-012 residual).
- **Write-lock ownership.** Today only per-session locks exist (`sessions/<id>/lock`, `store.rs:1597-1615`). The owner of an outcome is the process holding the lock of its **control session**; the orchestrator process holds the locks of all its active outcomes. A second process that wants ownership gets `Locked` (existing error) and connects to the holder instead; if the holder is dead the kernel has released the lock (same property `recover.rs:184-188` relies on). Exactly one owner per outcome at any time; two orchestrator processes may each own different outcomes.
- **Many read-only viewers.** Viewers never need the lock: they use the protocol, or, when no owner runs, open the store read-only (`Reader`, `store.rs:236`) to show last known state labelled "orchestrator not running" with a "resume" action.
- **Commands.** Every mutating request is a revision-checked command (ADR-016 item 11): an idempotent row in `outcome_commands` that the owner applies with compare-and-swap. A surface may enqueue a command when no owner runs; it is applied when an owner starts. Surfaces never write aggregate rows themselves.
- **Platforms.** macOS and Linux: the process runs natively. Windows: the desktop's engine runs inside WSL2 (ADR-015), so the orchestrator process runs in the distro, owned by the engine side, and the TUI and Super Agent reach it through protocol v4 over the existing stdio engine link; a native Windows CLI has no orchestrator (it can only view, and refuses writable attempts, ADR-018 item 5). WSL named-pipe or socket use from Windows is not assumed (**unverified**).
- **Sleep and shutdown.** Closing a laptop stops execution (design section 26): the UI says "orchestrator runs while this computer is awake"; on resume the lock is free or held and recovery handles both.

Relation to the engine protocol: the stdio engine remains one-session-per-engine for chat. Outcome methods (ADR-021) are served by the orchestrator process and, for WSL, forwarded by the engine; in-process hosting of the same server is allowed **only** for tests and headless CI (transport `InProc`), never as a per-surface production path.

### Security boundary of the orchestrator process (added after the PR-02 review)

The standalone process (L2) gives lifetime independence and a single owner per outcome (the control-session lock). It is an **ownership and correctness boundary, not a security boundary**: it runs as the same OS user as every surface, so any program of that user outside the sandbox can read the key and the database. Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. Real boundaries (a daemon under a separate OS user; a person-present signature through FIDO2, Touch ID or Windows Hello for irreversible decisions) are post-MVP owner decisions.

## Consequences

- Positive: reuses the governed run path, caps and sandbox; crash model is the existing one; no new IPC.
- Negative: ADR-009 amended; one more crate and one more long-lived process per user (lifecycle, idle exit, endpoint auth, per-OS code); the control session borrows `SessionStatus`.
- Risk: scheduler bugs multiply cost. Mitigation: cost cap required per outcome and per attempt (`run.rs:859`), bounded retries, MVP parallelism 2.

## Revisit trigger

- Idle-exit or endpoint auth proves fragile on one OS (-> L1 plus explicit resume as a degraded mode).
- Remote workers (-> `WorkerTransport`, heartbeats, fencing over the wire).
- More than ~50 work items per plan or fairness across outcomes (-> priority queue).
- Peers prove sufficient for a use case the owner wants (-> merge the models).

## Implementation notes (PR-05: the attempt runner)

- **Worker = `synt run --unattended --resume <session> --attempt-session ...`**, a child process in its own process group, started by `synthesis-orchestrator::Runner` from a `Profile` that only the profile builder can make: cost cap = the smallest of the contract's and the settings' (the outcome cap bounds it, so a cap always exists), wall cap likewise (default 30 min), `acceptEdits` if the envelope allows writing else `default` (never `auto`, never `bypassPermissions`), and no `--sandbox`, `--network`, `--trust-project`, `--approve-mcp`, `--allow-host`, `--settings`, `--add-dir`, `--no-worktree`: the defaults of `--unattended` stand, so the sandbox is enforced or the worker refuses to start (the attempt then fails and nothing ran). The caller may add only `--provider`, `--model`, `--max-turns`, `--max-tool-calls`.
- **`--attempt-session`** (hidden, internal) lets `run` continue an allocated attempt session, which a plain `/resume` refuses; the store accepts it only for a session of a `starting`/`running` attempt. With it `run` starts no MCP server (D12), registers no browser tools, and refuses `auto` and `bypassPermissions`; project hooks never run because `--unattended` forbids project trust (the repository's `SessionStart` hook is a test case). User and managed hooks remain, as ADR-019 says.
- **Lifecycle.** `mark_started` (the marker that stops any reset of the worktree) is written before the worker is spawned; the attempt goes `starting -> running`, the pid and its start token are recorded (`attempts.worker_pid/worker_start`), and a heartbeat extends the lease while the worker lives. Every worker write (`record_worker`, `heartbeat`, finishing) is refused with `Stale` for a wrong fence, a finished attempt, or when an attempt of the same work item with a higher fence exists.
- **Exit mapping.** 0 completed; 3 unverified; 4 blocked; 1, 2, 70, a signal or the wall cap failed; 130 or a runner-side cancel cancelled. Completed and **unverified** both hand the work item to verification (an unattended worker with no declared check always ends `unverified`; the independent runner of PR-09 is the authority); failed retries within the contract's budget (`retryable_failure`, then `terminal_failure`); blocked waits for a decision; cancelled is cancelled. Deviation from the earlier "unverified retries" reading: it would retry every attempt that has no declared check.
- **Cancel.** A cancel is the store's stop request on the attempt's session (the worker polls it, cancels, kills the commands it started); after the grace period the runner kills the worker's process group and every descendant it listed beforehand. A cancelled attempt's tree is kept uncommitted.
- **Recovery** (`recover_attempts`, at owner start): a live worker with a fresh lease is left alone; a silent one is killed; a dead one whose session ended is settled from the session's status; otherwise the attempt is `lost`, its session interrupted, and the work item freed (`ready` while the attempt budget allows, else `terminal_failure`). Nothing is rerun: the scheduler allocates a new attempt with a new session and worktree.
- **One worker per attempt.** The attempt row is created once per `(item, ordinal)`, a second live attempt of an item is refused, and the attempt session's OS lock admits one writer; there is no separate lock.

## Implementation notes (PR-06: the scheduler)

- `Scheduler` (in `synthesis-orchestrator`) is driven by one owner thread. A **tick** polls the running workers, lets the verifier judge items in `verifying`, promotes retries whose backoff is over, makes `created` items `ready` when every dependency `succeeded`, then leases `ready` agent items in **sorted key order** while fewer than `max_parallel` workers run (the smaller of the contract's `max_parallel_workers` and the settings', default 2; workers an earlier owner left running count too). A `verify` item has no worker: it goes `leased -> running -> verifying` and straight to the verifier. Every change is the store's checked transition, so an item is leased once, and `work_item_deps` plus the validated plan make a cycle impossible; a dependency that can never succeed ends the walk as `Stop::Blocked` naming the stuck items (the outcome goes `blocked`).
- `Runner` is split into `start` and `poll` so one owner can watch several workers; `run_attempt` is both in a loop. `WorkerHost` is the seam between the scheduler and the process world (`ProcessHost` is the real one: allocator, profile with the outcome's remaining budget, `Runner`).
- **Retries.** A failed worker retries within the contract's budget (`max_attempts_per_work_item`, default 3): `retryable_failure`, wait `base * 2^retries` (default 5 s, cap 5 min, kept in memory, so a restart retries at once), `ready`, a NEW attempt. A **failed verification** takes the same path from `verifying`; when the budget is used up the item is `terminal_failure` and the walk ends `Blocked`. Unverified workers hand over to verification (PR-05).
- **Verifier.** `Verifier` is the seam for PR-09. Until then `HandOff` leaves items in `verifying` and the walk stops as `Stop::WaitingVerification`: nothing becomes `succeeded` without the independent evidence the store requires. The outcome moves to `verifying`/`verified` only through the store's own verdict.
- **Budget.** No worker starts once the attempts' recorded spend reached the outcome's cap (`Blocked`), and each profile gets what is left (PR-05 review).
- **Pause and cancel** are flags of `Control` (pause: no new leases, running workers go on; cancel: every worker gets the stop request and then the tree kill, open items are cancelled, the outcome is `cancelled`). They are not stored: the persistent commands are PR-11/12's, and a restart resumes. **Restart:** run `recover_attempts` (PR-05) first; the scheduler then frees items left `leased`/`running` without a live attempt (`recovering -> ready`, or `terminal_failure` when the attempt budget is used) and continues; attempts keep unique ordinals, so no attempt is duplicated.
- A start that can never work (not the outcome's repository, native Windows, no budget) blocks the item with the reason in the tick report instead of retrying in a loop.
- Not in this PR (plan section 6): resource locks beyond one owner, priorities, fairness across outcomes. The plan limit of 256 work items bounds a walk; the tests drive the maximum (250 agent items plus a verify item) and assert the cap never exceeded and the lease order sorted.

