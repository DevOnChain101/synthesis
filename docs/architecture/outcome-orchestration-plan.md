# Outcome orchestration: phased plan and PR slicing

Changelog: 2026-11-14 rev 2 (owner): the orchestrator serves CLI, TUI and Super Agent through one core and one protocol; standalone orchestrator process; durable decisions and commands; explicit v5 migration; delivery order core, protocol, CLI, TUI, Super Agent; D4, D5, D6 revised and D17-D21 added. Rev 1 (same day): first version.

Status: Proposed (2026-11-14, rev 2). Nothing here is scheduled until the owner answers the decisions in section 7.
Remaining backend work after the implemented PRs: [orchestrator remaining backend spec](../specs/orchestrator-remaining-backend.md) (proposed; owner questions O1-O16).
Inputs: [gap analysis](./outcome-orchestration-gap-analysis.md), [ADRs](./adr/README.md). Test specs: [acceptance](../testing/outcome-orchestration-acceptance.md). Threat model: [threat model](./outcome-orchestration-threat-model.md). First slice: [MVP slice](./outcome-orchestration-mvp-slice.md).

Estimates are rough guesses for one engineer plus review (S <= 2 days, M 3-5, L 6-10), not measurements. The repository rule applies to every PR: `cargo fmt --all --check`, `cargo clippy --workspace --exclude synthesis-desktop --all-targets -- -D warnings`, `cargo test --workspace --exclude synthesis-desktop` (add `-p synthesis-desktop` for desktop PRs), `node scripts/check-no-legacy-name.mjs`, and the CI job set (`ci-ok`). Every PR that touches permissions, sandbox, secrets, input validation or the store goes through the read-only security reviewer before merge (owner rule, `HANDOFF.md` 7k). Work happens in worktrees under `.claude/worktrees/`.

## 1. Phases

Delivery order (owner, multi-surface rule 13): **core, protocol, CLI, TUI, Super Agent.** The CLI is the cheapest full lifecycle proof; the TUI and Super Agent are added as thin clients of the same protocol. Terminology: CLI = `synt` binary and terminal app; TUI = desktop terminal-style tab; Super Agent = desktop Agent tab (see the [gap analysis, section 9](./outcome-orchestration-gap-analysis.md)).

| Phase | Content | Exit gate |
|---|---|---|
| P0 Decisions | Owner answers section 7; ADRs move to Accepted; registry rows added | ADR-016..021 Accepted |
| P1 Core foundations | Domain types, migration v5 with revisions and commands, cwd audit, chain-key fix | Pure tests green on 3 OS; migration test from a v4 fixture; CAS and idempotency tests |
| P2 Core execution | Attempt allocation, runner, cancel, recovery, scheduler | AC-02, AC-03, AC-04, AC-13 at protocol level on macOS and Linux |
| P3 Core control | Contract, envelope, gateway, durable decisions, bypass gate | AC-05, AC-06, AC-09, AC-10 (data), policy adversarial suite, the ADR-019 section 6 gate |
| P4 Core evidence | Independent runner, verdict | AC-07, AC-08, forged-evidence red-team cases |
| P5 Protocol | Protocol v4, client library (InProc, Stdio), view models, dependency test; then the standalone orchestrator process (LocalSocket) | Whole lifecycle provable headless through the client (acceptance document, section 1) |
| P6 CLI | `synt outcome ...`, `outcome enable`, notify kinds | MVP definition of done |
| P7 TUI | Outcomes group, decision picker, inspector tabs | AC-11 rendering tests; multi-surface tests with the CLI as the second client |
| P8 Super Agent | Surface per the owner's design (specified in ADR-021; implementation blocked on the design) | Same protocol tests, plus its rendering tests |
| Parallel hardening | Linux read allow-list, MCP/browser under sandbox, rlimits | Gap analysis R-8 closed |
| Later | Inline+Spill context, watchdog, external events, remote workers | Separate designs |

## 2. Dependency graph

```mermaid
flowchart LR
  S1[PR-S1 chain.key deny + redteam case]
  P01[PR-01 domain types + tables]
  P02[PR-02 migration v5 + revisions + commands + outbox]
  P03[PR-03 cwd audit: CLI, TUI, WSL]
  P04[PR-04 attempt allocation + base commit]
  P05[PR-05 attempt runner + cancel + recovery]
  P06[PR-06 scheduler + retry budgets]
  P07[PR-07 contract + planner schema]
  P08[PR-08 envelope + gateway + bypass gate]
  P09[PR-09 evidence + independent runner + verdict]
  P10[PR-10 durable decisions + grants + audit]
  P11[PR-11 protocol v4 + client lib + view models + dep test]
  P12[PR-12 standalone orchestrator process + endpoint + notify kinds]
  P13[PR-13 CLI surface]
  P14[PR-14 TUI surface]
  P15[PR-15 Super Agent surface]
  H1[PR-H1 Linux read allow-list]
  H2[PR-H2 MCP and browser under sandbox]
  S1 --> P09
  P01 --> P02
  P02 --> P04
  P02 --> P05
  P04 --> P05
  P05 --> P06
  P01 --> P07
  P06 --> P07
  P01 --> P08
  P05 --> P08
  P02 --> P09
  P04 --> P09
  P05 --> P09
  P08 --> P10
  P02 --> P10
  P07 --> P11
  P09 --> P11
  P10 --> P11
  P08 --> P11
  P11 --> P12
  P12 --> P13
  P13 --> P14
  P13 --> P15
  P03 -.independent.-> P04
  H1 -.raises guarantee.-> P08
  H2 -.enables MCP in attempts.-> P08
```

Critical path to the MVP (CLI): PR-01, PR-02, PR-04, PR-05, PR-06, PR-07, PR-08, PR-10, PR-11, PR-12, PR-13 (PR-09 in parallel after PR-05). The feature cannot be enabled in any surface before PR-08's bypass gate passes. PR-S1 and PR-03 can start immediately and need only the decisions on naming.

## 3. PR slices

Mapping to the design's list: design PR-01 -> PR-01, PR-02 -> PR-02, PR-03 -> PR-03 (plus audit scope), PR-04 -> PR-04, PR-05 -> PR-05, PR-06 -> PR-06, PR-07 -> PR-07, PR-08 -> PR-08, PR-09 -> PR-09, PR-10 -> PR-10, PR-11 -> PR-11 to PR-15 (protocol, orchestrator process, then one PR per surface), the design's PR-12..15 (Inline+Spill, watchdog, GitHub, remote) -> deferred (section 6). PR-S1 and PR-H* are new because the repo shows gaps the design does not know about.

### PR-S1 Chain key off the sandbox read path
- Scope: add `chain.key` to `synthesis_state_paths` (`synthesis-sandbox/src/lib.rs:480-497`); decide per path whether `sessions/`, `config.json`, `state.json` follow (test first); correct `docs/UNATTENDED.md:49`.
- Crates: `synthesis-sandbox`, tests in `synthesis-app/tests/state_files_protected.rs`, `tests/redteam`.
- Tests: red-team `evid-chain-key-read` (Linux bubblewrap must not read it; macOS must not read it); unit test that the path list contains it.
- Covers: ADR-020 anti-forgery, threat T-E3.
- Risk: Low; a sandboxed toolchain that wants the home dir could break (state paths are exact names, `chain.key` is not a tool path). Rollback: revert one line.
- Size: S. Security review: yes.

### PR-01 Domain types and transition tables
- Scope: `synthesis-core::outcome`: ids (UUIDv4 strings), enums `OutcomeState`, `WorkItemState`, `AttemptState`, `DecisionState`, transition functions in the style of `session.rs:48-67`, contract/plan structs (serde), plan validator (unknown dependency, cycle, unknown capability, uncovered mandatory criterion), `Envelope` type and its compile-time refusals (no allow for irreversible actions).
- Crates: `synthesis-core`.
- Tests: table-driven transition tests (every pair), property-style cycle tests, serde round trips; runs on all OSes.
- Covers: AC-01 (validation half), AC-09 (state half).
- Risk: Low (pure). Rollback: revert. Size: M.

### PR-02 Migration v5, outcome store, outbox
- **Prerequisite for merging:** PR-S1 / PR #117 (chain key and `backups/` read deny, red-team cases `evid-chain-key-read`, `evid-backups-read`) is merged into main; this branch contains it. The store's MACs are only as good as the key's confinement.
- Scope: `schema.rs` v5 (DDL in the [MVP slice](./outcome-orchestration-mvp-slice.md) section 3), `sessions.kind`, `synthesis-session::outcome` (CAS transitions, **`outcome_commands` with idempotency keys and result write-back, optimistic revisions: Phase 1**, outbox, evidence insert API without runner), kind filters in listers and cleaners, `StoreOptions` migration target so a default open stays at v4 (`store.rs:585`) and v5 is applied only by the explicit `enable` step with the automatic pre-migration backup (`clean.rs:431` pattern).
- Crates: `synthesis-session`, `synthesis-events` (additive event types and payload structs).
- Tests: v4 -> v5 migration fixture; CAS conflict; outbox replay after a killed writer (reuse `synthesis-session/tests/crash.rs` helpers); chain verification over outcome events (`tests/governance.rs` style); control session hidden from `list_used_sessions` and `clean`; `SchemaTooNew` for a v4-target open on a v5 DB (assert error text); a default open leaves v4 at v4; command idempotency and stale-revision results.
- Covers: AC-04 (store half), HARD-4, HARD-8, HARD-9.
- Risk: Medium (migration, user data). Rollback: restore the pre-v5 backup; documented. Size: L.

### PR-03 cwd scoping audit and tests (independent)
- **Audit result (implemented):** the one real defect was the desktop `/new` fallback to the running engine's folder when the selected workspace was missing from the list (D7); it is now `new_session_root`, a refusal with a visible note. A store-level check was absent and is added: `create_session` refuses a context whose cwd is outside its root and added folders (`OutsideWorkspace`). Verified by test: desktop relaunch targets (`New`, `Model`, `Close`, `Resume` while another folder was active) in `apps/desktop/tests/commands_fake.rs`; the `/new` rule; engine `start` (relative, missing, file cwd refused; `session.new` keeps the folder; another folder's session never resumed) in `apps/cli/tests/engine_stdio.rs`; store containment and own-cwd in `synthesis-session/tests/store.rs`; `check_resumable` and `check_reported` (WSL) and the CLI `/new` test already existed. Verified by reading only: `launch_dir` (one `current_dir()` read, `--cwd` wins), the desktop `main.rs` start folder, `is_here` for `--continue`/`--resume`.
- Scope: tests and, only if a defect is found, fixes for the three entry points (CLI launch, desktop TUI, WSL; the Super Agent has none yet); store-level check that a session's `cwd` lies in `workspace_root` or an additional dir if absent (**unverified** whether it exists); desktop `Action::New` fallback becomes a refusal (decision D7).
- Crates: `apps/desktop`, `synthesis-cli-ui`, `synthesis-session`, `synthesis-app`.
- Tests: desktop `commands_fake.rs`-style test for every `Target` (`New`, `Resume`, `Model`, `Trust`, `Close`, `CloseWorkspace`), WSL `check_reported` cases, existing CLI test `synthesis-cli-ui/src/app/tests.rs:2958` kept, new store test for cwd containment.
- Covers: AC-03, HARD-3.
- Risk: Low-Medium (desktop behaviour change on D7). Rollback: revert. Size: M. Needs the desktop job (heavy); no new window tests.

### PR-04 Attempt allocation and base commit
- **Status (implemented):** `WorktreeManager::create_at`, `allocate_unstarted`, `head`; new crate `synthesis-orchestrator` (`Allocator`, `finish_attempt`, `cleanup_attempt`, `retention_due`); store additions `set_attempt_retained` and `Store::set_git_info_if_unset`. See ADR-018 implementation notes.
- Scope: orchestrator-side allocation (session id first, worktree, record `GitInfo`) with crash reconcile; `WorktreeManager` option to start at a given commit (only needed after MVP; implement the API, test it, do not use it in MVP); host `commit_all` at attempt end; `retained_reason` on refused cleanup.
- Crates: `synthesis-workspace`, `synthesis-orchestrator` (new, first code), `synthesis-session`.
- Tests: crash between worktree creation and DB commit (adopt/redo); two attempts get distinct dirs, branches and sessions (extends `workspace.rs:376`); cleanup refusal keeps the tree.
- Covers: AC-02 (isolation), HARD-3.
- Risk: Medium (git on three OSes, long paths on Windows). Rollback: revert; worktrees are plain git. Size: M.

### PR-05 Attempt runner, cancel and recovery
- **Status (implemented):** `Profile`, `Runner`, `recover_attempts`, `CancelFlag`, `SyntSpawn` in `synthesis-orchestrator`; `--attempt-session` in `synt run`; schema additions `attempts.worker_pid/worker_start/heartbeat_at`, `Store::resume_attempt_session`, fence checks (`record_worker`, `heartbeat`). See ADR-017 implementation notes. Not done here: envelope-to-rules compile and the OrchGateway (PR-08), the scheduler that decides when to run (PR-06), the independent verifier (PR-09), the standalone process (PR-12).
- Scope: start a governed session for an attempt through `run::prepare`/host with the unattended profile; map `TaskStatus` to attempt state; cancel via stop path and process-tree kill; orchestrator startup recovery (lost attempts, outbox replay) with a recovery report event.
- Crates: `synthesis-orchestrator`, `synthesis-app`.
- Tests: fake provider (`test-provider` feature) attempt end-to-end; kill -9 the orchestrator mid-attempt and recover (new e2e `outcome_recovery`); cancel kills a spawning tree within a bound (AC-13).
- Covers: AC-04, AC-13, AC-15, HARD-8.
- Risk: High (concurrency, process lifetime, Windows flake pattern at `docs/STATUS.md` SQLite lock). Rollback: feature flag `synt.orchestrator.enabled` default off. Size: L.

### PR-06 Scheduler and retry budgets
- **Status (implemented):** `Scheduler`, `SchedConfig`, `Control`, `WorkerHost`/`ProcessHost`, `Verifier`/`HandOff`, `Runner::start/poll`. See ADR-017 implementation notes. The gate "1,000 queued items" is exercised at the plan limit of 256 items (250 agent items plus a verify item): a plan cannot hold 1,000.
- **Review fixes (PR-06):** reservation-based outcome budget (`attempts.reserved_usd`, DDL 1004), failed starts and verifier errors no longer end the tick, `Scheduler::shutdown`/`run_until` deadline, handle drop kills the tree. **Note for PR-09:** verification must run off-thread, or tick the workers while it runs: heartbeat, cancel and the wall cap advance only in `tick`.
- Scope: bounded-parallel DAG walk, canonical ordering, retry budget per work item, blocked state with reason; no resource locks yet.
- Crates: `synthesis-orchestrator`.
- Tests: synthetic DAG with injected failures (design Phase 3 gate): no duplicate assignment, no cycle, retries bounded, 1,000 queued items do not spawn more than the cap (stub attempts).
- Covers: AC-02 (parallelism), AC-07.
- Risk: Medium. Rollback: flag. Size: M.

### PR-07 Contract and planner schema
- **Status (implemented):** `new_contract_version` (store), `parse_plan`/`diff_plans`/`PlanDiff` (core), `Planner`/`StubPlanner`/`make_plan`/`replan` (orchestrator). Approval hash and immutability were PR-02's. The model planner is not built (D13). See ADR-017 implementation notes.
- Scope: contract model (goal, criteria with optional check commands, envelope), approval records the contract hash (surface-independent; the CLI form comes in PR-13); deterministic stub planner (inspect -> implement -> test); planner JSON schema validation; model planner behind the same schema later.
- Crates: `synthesis-orchestrator`.
- Tests: schema rejections; approval hash stability; contract immutability.
- Covers: AC-01.
- Risk: Low-Medium. Size: M.

### PR-08 Envelope compiler, gateway, non-interactive attempts
- **Status (implemented):** `compile_envelope`, `Profile` adds the compiled rules and mode `default`, `record_blocked_decision` (blocked attempts become Decisions), bypass-path tests and the `orch-attempt-*` red-team cases. Not done: the orchestrator-side `OrchGateway`, answering decisions and the one-shot grant (PR-10), checking the commit's files against the envelope (PR-09). See ADR-019 implementation notes.
- Scope: envelope -> rules/mode/sandbox/budget compile; `OrchGateway`; attempt launch profile (no bypass, no auto judge, no Always, no project trust, no MCP unless listed); `AskUserQuestion` and denials recorded as `needs_decision` (ADR-019 section 2).
- Crates: `synthesis-orchestrator`, `synthesis-permissions` (only if a hook is missing; prefer none), `synthesis-app`.
- Tests: golden compile tests; fuzz: no generated allow for any `irreversible.rs` action; red-team cases (threat model) in all four modes; AC-05, AC-06.
- Gate: includes the ADR-019 section 6 bypass-closure tests; until they pass, `synt.orchestrator.enabled` is refused by `outcome enable`.
- Risk: High (security). Rollback: flag. Size: L. Security review: mandatory, two rounds expected.

### PR-09 Evidence, independent runner, verdict
- **Status (implemented):** pure `verdict` and `outside_envelope` (core), `Contract.checks`, event-backed evidence facts and `flag_evidence` (store), `IndependentVerifier`/`SandboxRunner` (orchestrator), `Verifier::busy`. See ADR-020 implementation notes. Not done: the process that wires the verifier for real outcomes (PR-12), merge/integration of parallel branches, the definitions exemption.
- Scope: `evidence` writes, host-side runner on a detached checkout, trust classes, pure `verdict`, stale marking, environment fingerprint.
- Crates: `synthesis-verification`, `synthesis-orchestrator`, `synthesis-session`.
- Tests: exhaustive verdict table; forged report file does not clear the gate; `definitions_changed` blocks; stale commit rejected; fence rejection; runner refuses without enforce.
- Covers: AC-08, forged-evidence cases.
- Risk: High (runs agent-written tests on the host under the sandbox). Rollback: flag. Size: L. Security review: mandatory.

### PR-10 Decisions, grants, audit
- **Status (implemented):** `DecisionDriver` (expiry, invalidation, grants, answered decisions) on the scheduler's `Decisions` seam, the store's `create_grant`/`usable_grants`/`consume_grant`/`revoke_grant`/`audit_policy`, `decisions.action`, MAC'd `grants`, `policy.decision` and `grant.*` events, grant rules as `--allow` flags of one attempt. See ADR-019 PR-10 notes. Not done: the protocol commands and surfaces (PR-11), the orchestrator process that runs the driver (PR-12), `edit_scope` re-briefing (B3 did `request_evidence`), a live `OrchGateway`.
- Scope: durable decision lifecycle (rows, not live asks), single-answerer CAS and the race table of ADR-019 section 5, brief builder (structured, no recommended/selected field), dedupe, bindings, expiry timer, invalidation on contract/plan/commit/policy change, one-shot grant that starts a new attempt, `policy.decision` events.
- Crates: `synthesis-orchestrator`, `synthesis-session`.
- Tests: stale decision replay after a commit change is rejected; revoked grant; expiry moves to HOLD, never approve; dedupe; no default choice in the brief schema; AC-09, AC-10 (data shape).
- Risk: Medium-High. Size: L. Security review: yes.

### PR-11 Protocol v4, client library, view models
- **Status (implemented):** `PROTOCOL = 4` with the read methods, the command methods and `events.subscribe` (a cursor page) of ADR-021 section 1 (`synthesis-app::{proto, outcome_service}`, wired into `synt engine --stdio`, off unless `synt.orchestrator.enabled`); `synthesis_app::outcome_client` (`Stdio` with the hostile-engine rules, `InProc` behind the `inproc-test` feature, typed calls); `synthesis_orchestrator::apply_queued` (the owner's step for `decision.resolve` and `outcome.contract.approve`); `synthesis-view::outcomes` (models, brief rows, `uncovered`, `Picker` with no cursor, the inspector tab set); later: plan, criteria, attempt, evidence and budget models and `InspectorContent` with an explicit `Unavailable` (ADR-021 notes; the reads `outcome.budget`, `attempt.diff` and `event_backed` on evidence rows were added afterwards, read-only and behind the same switch, see ADR-021 notes; the protocol still has no sandbox facts because the store does not record them at launch); the dependency test (`crates/synthesis-app/tests/surface_deps.rs`). Not done: `outcome.create` and `orchestrator.enable` (PR-12), applying `outcome.start|pause|resume|cancel|ack` (they stay `queued`; PR-12), the `{"msg":"decision"}` push line and notify kinds (PR-12), the `LocalSocket` transport (PR-12), `critical` in `needs_you`, the whole acceptance suite driven through the client (needs a live orchestrator, PR-12), reconnect with a push stream (the cursor page is there), re-briefing after `request_evidence`/`edit_scope` (B3 re-briefs `request_evidence`; `edit_scope` still not).
- Scope: `PROTOCOL = 4`; methods of ADR-021 section 1; `synthesis_app::outcome_client` with transports `InProc` (tests and headless CI only) and `Stdio` (WSL engine link); `synthesis-view::outcomes` models and the shared inspector tab set; **dependency test**: `apps/*`, `synthesis-cli-ui*`, `synthesis-view` must not depend on `synthesis-orchestrator`; renderer field-coverage helper for decision briefs.
- Crates: `synthesis-app`, `synthesis-view`, `synthesis-orchestrator`.
- Tests: protocol golden lines; the whole acceptance suite driven through the client (this is the headless lifecycle proof); hostile-engine cases (unsolicited resolve); reconnect with `after_event_id`; two clients racing `decision.resolve`.
- Covers: AC-01..AC-15 at protocol level, multi-surface rules 1, 2, 4, 5, 6, 14.
- Risk: Medium (lock-step version). Size: L.

### PR-12 Standalone orchestrator process
- Scope: hidden `synt orchestrator --serve` (detached, idle exit `synt.orchestrator.idle_exit_minutes`); `LocalSocket` transport (unix socket or named pipe under `$SYNT_HOME/run/`, peers' framing and HMAC handshake, key `run/orch.key`); control-session lock ownership and takeover; auto-start by clients and `outcome resume`; notify kinds `decision.created`, `outcome.blocked`, `outcome.verified` and the `needs_you` derivation; `orchestrator.enable` (explicit migration with backup and compatibility notice); add the endpoint and key to `synthesis_state_paths` (they live under the already-denied `run/`; test it).
- Crates: `synthesis-app`, `synthesis-orchestrator`, `synthesis-peers` (reuse `net.rs`, extract a shared module only if needed), `synthesis-sandbox` (test only), `apps/cli` (the hidden subcommand).
- Tests: kill the surface, outcome continues; kill the orchestrator, another client restarts it and recovery runs; two orchestrators race for one outcome (lock decides); sandboxed attempt cannot connect (red-team `orch-endpoint-from-attempt`); idle exit; notification fired with no client attached.
- Covers: multi-surface rules 3, 7; AC-04 (process level), threat T-32, T-36.
- Risk: High (process lifecycle on three OSes, Windows named pipes **unverified**; WSL reach unverified). Rollback: flag; fall back to L1 (in-process plus `outcome resume`) as a degraded mode. Size: L.
- **Status (implemented):** hidden `synt orchestrator --serve` (`synthesis_app::orch_owner`): process singleton lock `run/orch.lock`, per-outcome ownership by the control-session lock (a second owner gets `Locked` and leaves the outcome), adoption of running outcomes and outcomes with queued commands, `recover_attempts` plus an `outcome.recovered` event on adoption, durable pause (events), idle exit (D20), stop on SIGINT/SIGTERM or when `synt.orchestrator.enabled` turns false (workers stopped). `LocalSocket` transport and endpoint (`synthesis_app::orch_endpoint`): unix socket or named pipe under `run/`, peers' framing and HMAC handshake with `run/orch.key`, endpoint registry `run/orch.json`, an `attach {surface}` frame after the handshake; a mutating call must name the attached surface; the stdio engine is read-only (`Service::read_only`). `outcome.create` and `orchestrator.enable` (served by the owner only; enable is the store migration with its automatic `backups/pre-v5-*` copy and requires the setting to be on already). `apply_lifecycle` for `outcome.start|pause|resume|cancel|ack` and `conclude` (running -> verifying -> verified or rework_required by the store's own verdict), client auto-start (`LocalSocket::connect_or_start`), the real `ProcessHost`, `IndependentVerifier` and `DecisionDriver` wired per running outcome, notify payloads (`decision.created`, `outcome.blocked`, `outcome.verified`, ids and codes only) sent through a confined `Notifier`. See ADR-017/019/020/021 PR-12 notes and the manual smoke in `docs/testing/outcome-orchestrator-smoke.md`.
- **Not done in PR-12 (and why):** a live `OrchGateway` (not assigned to PR-12 by the scope above; it follows the bypass gate, PR-08); a model planner (D13: `start` uses the deterministic `StubPlanner`); the `{"msg":"decision"}` push line (clients poll `events.subscribe` or `decision.list`); `critical` in `needs_you`; writing the `synt.orchestrator.enabled` setting from `orchestrator.enable` (the person sets it; PR-13/PR-14 add the confirmation step); WSL forwarding of mutating calls through the stdio engine (refused on purpose, PR-14); durable notification dedupe (in memory); **Windows named pipe and WSL reach are unverified** (the process and endpoint tests are unix-only; the Windows code is not built or run by this PR's author, CI will compile it); merge/integration of parallel branches. The endpoint authenticates a process that holds `run/orch.key`, which any program of the same OS user outside the sandbox can read: ownership and correctness, not security.

- **Status (end-to-end and recovery matrix):** `apps/cli/tests/outcome_e2e.rs` drives create, approve, start, a scripted worker that edits a worktree, independent verification, verdict and ack through the real `synt outcome` CLI and owner process, a decision produced by a real blocked worker, and kill -9 of the owner at six lifecycle points (AC-04, partial). Test-only injection: `SYNT_TEST_ORCH_SCRIPT` / `SYNT_TEST_ORCH_HOLD`, read in `orch_owner::serve_with` under feature `test-provider` only. Covered and not covered, and three behaviours found on the way (live worker adopted then stalled, lost attempt's reservation blocks the outcome, a decision answer does not resume a blocked outcome): acceptance doc section 3c.

### PR-13 CLI surface (MVP)
- **Status (implemented):** `synt outcome create|contract|approve|start|pause|resume|cancel|ack|list|status|decisions|resolve|inspect|events|enable` (`synthesis_app::outcome_cmd`, text and `--json` with `"schema": 1`), over `OutcomeClient` on `LocalSocket::connect_or_start` as surface `cli`; new typed reads on the client (`contract`, `plan`, `work_item`, `attempt`, `evidence`); `resolve` needs `--choice` plus the revision and brief hash of the card shown (or a numbered picker without default on a terminal), refuses a changed card before sending; commands are followed until the owner applied them (10 s); exit codes 0/1/2; off or native Windows (`create`, `start`, `resume`) refuse with the text of the setting or `WSL2_REFUSAL`. Tests: `outcome_cmd` unit tests (goldens per brief field, no preselection, JSON schema keys, refusals), `apps/cli/tests/outcome_cmds.rs` (real binary and owner process: enable, create, contract, approve, start, status, inspect, events, cancel; a seeded decision shown, refused without a choice or with a stale card, answered once). **Not done:** terminal-app `/outcomes` and `synt.cli.outcomes.*` keys; a numbered picker is tested with scripted input only (no pty test); `--base-commit` is required (the CLI does not run git); `inspect --tab diff` says it is not available (no protocol method); no sandbox backend and guarantee per attempt in `inspect` (the protocol does not carry them); `enable` does not check the 3b gate items beyond the setting being on and `--yes` (the gate is the owner's decision); no `plan` print command; the e2e does not run a worker (no scripted model reaches the owner process); Windows named pipe path unverified (the e2e is unix-only).
- Scope: `synt outcome create|approve|start|status|list|decisions|resolve|pause|resume|cancel|inspect|enable` (text and `--json`), thin over the client library with the `LocalSocket` transport; picker without default for `resolve`; terminal app `/outcomes` (read and resolve) after the subcommands; `synt.cli.outcomes.*` rendering keys.
- Crates: `apps/cli`, `synthesis-cli-ui*`, `synthesis-view`.
- Tests: command dispatch and rendering only (output goldens for each brief field, no preselection, platform refusal text); behaviour is already proven by PR-11 tests.
- Covers: MVP definition of done; AC-10 (CLI rendering).
- Risk: Low-Medium. Size: M.

### PR-14 TUI surface
- Scope: Outcomes group in the Workspace panel, control-session chat, decision picker, inspector tab container in the right drawer region while an outcome is selected; `synt.tui.outcomes.*`; Settings toggle for `synt.orchestrator.enabled` that calls `orchestrator.enable`.
- Crates: `apps/desktop` (`tui_view/`), `synthesis-view`.
- Tests: `design_tests` bounds for new rows, snapshot of a decision card, no-preselection rule test, multi-surface test with the CLI as the second client (race on one decision).
- Covers: AC-10, AC-11 (rendering), HARD-11 (TUI).
- Risk: High (UI regression, desktop CI job, `design_tests`). Size: L (probably two PRs). Blocked on D5, D6.

### PR-15 Super Agent surface
- Scope: per the owner's Super Agent design (not yet available): outcome list and goal conversation, Decision/Task inbox, one right-side inspector with the shared tab set, decision cards from the structured brief; replaces the "Coming soon" page behind the flag. `synt.superagent.*` if the owner wants a section (D17).
- Crates: `apps/desktop`, `synthesis-view`.
- Tests: rendering and command dispatch only, plus the shared field-coverage helper.
- Risk: High (new design). Size: L. **Blocked** until the design exists; this PR is specified, not scheduled.

### PR-H1, H2, H3 Known hardening (already listed by the owner, `docs/specs/sandbox-hardening.md` sections 2-4)
- H1 Linux read allow-list; H2 MCP stdio and browser worker under the sandbox; H3 rlimits and disk quota. They are not prerequisites of the MVP but each one upgrades what the UI may claim (ADR-018 item 9) and closes risks R-8. H4 (Windows AppContainer) needs an ADR-010 exception (decision D3).

## 3b. Enable gate (before PR-13 turns the feature on for real attempts)

`synt outcome enable` must refuse, and no surface may offer the toggle, until every item holds:

1. **PR-S1 merged** and the red-team cases `evid-chain-key-read`, `evid-backups-read` and `orch-db-write-from-attempt` are enforced (green) on Linux and macOS CI.
2. **PR-08 bypass gate** passed (ADR-019 section 6).
3. **Native Windows refuses writable attempts** (D3); the refusal text is shown by every surface.
4. **Authority is consumed only from verified data.** PR-10 re-checks a decision's binding against the current state and ties each approval to its signed `decision.resolved` event; PR-09 computes the verdict only from signed `evidence.recorded` events (today a writer without the key can INSERT a passing `independent_check` row with plain SQL, and the MVP verdict in the store must not be relied on).
5. **Attempts run with no MCP servers and no project hooks by default** (T-23 `pol-mcp-not-in-attempt-by-default`). Highest priority.
6. **The integrity claim and the per-attempt sandbox guarantee are shown to the user at enable time.** The claim: "Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. User and managed hooks still run on the host, outside the sandbox. There is no person-present signature yet: a program running as you outside the sandbox can read the key and forge an approval, an event or evidence." The same text is `INTEGRITY_DISCLOSURE` in `synthesis_app::outcome_policy` (a test fails if the two differ); the owner's `orchestrator.enable` reply carries it as `disclosure: {id, text}` and every surface shows that text, none words it itself.

7. **PR-09 verifies a FRESH CHECKOUT of the subject commit, never the attempt's worktree.** On macOS a sandboxed command that calls `setsid` survives the attempt and can keep writing its worktree after `subject_commit` was recorded (Linux bubblewrap has `--unshare-pid` and `--die-with-parent`); nothing read from the attempt's tree after the commit may count as evidence.

Acceptance criteria for PR-09 and PR-10: **every read that grants authority verifies the control-session chain and that the row matches its signed event.** PR-12's standalone process is an ownership boundary, not a security boundary; separate-OS-user daemons and person-present signatures for irreversible decisions are post-MVP owner decisions.

## 4. Cross-platform requirements

- Pure logic (domain, validator, scheduler with stub attempts, verdict, brief builder, envelope compiler, store) must pass on macOS, Linux and Windows CI from the first PR; tests may not depend on `$HOME`, `TERM`/`COLORTERM` or a non-symlinked temp dir (macOS `/var` -> `/private/var`, CLAUDE.md).
- Paths: canonicalise with `synthesis_core::canonicalize`; case-fold on win32 and darwin (`PathPolicy`); spawn Windows system tools by absolute `System32` path; kill whole process trees (group on POSIX, `taskkill /T` on Windows); never rewrite `\` to `/` on POSIX.
- Surfaces and platforms: writable attempts only on macOS, Linux and WSL2 for the CLI and the desktop alike; elsewhere (native Windows) the orchestrator returns one explicit refusal naming WSL2 and every surface renders that text (test per surface). The orchestrator process and its endpoint are new per-OS code (unix socket, Windows named pipe); Windows named pipes and Windows-to-WSL reach are **unverified**.
- Execution tests (attempt runner, independent runner, red-team) run on macOS and Linux (`EVAL_SANDBOX=enforce` as in the `redteam` job); on Windows they are skipped with an explicit reason and the orchestrator refuses writable attempts natively (ADR-018 item 5). Windows users reach outcomes through the WSL engine (ADR-015); the WSL path is verified only by CI plus the owner's manual `#[ignore]` test (`docs/SECURITY.md:44`), **unverified** on real hardware.
- SQLite on Windows showed lock contention on event append (`docs/STATUS.md` Verified, retried since PR #55): orchestrator tests must not hammer one DB from many threads.
- Environment: orchestrator code takes `&Env` (injectable); child environments stay the existing allowlist.
- New CI jobs: none for the MVP. Extend `test`, `redteam` (new case ids), `sandbox-linux` (chain-key read) and the desktop job (PR-14 and PR-15 only). Add the new test files to the existing jobs rather than creating jobs, to keep `ci-ok` stable.

## 5. Rollback, feature flag and compatibility

All orchestration code is behind `synt.orchestrator.enabled` (user or managed setting, default false; a project file can never enable it). With it off: no schema change (the default store target stays v4, ADR-016 item 9), no new commands in `--help` (hidden), no UI row in any surface. Turning it on is an explicit step (`synt outcome enable`, protocol `orchestrator.enable`, TUI Settings toggle) that prints a notice, makes a backup, then migrates. Compatibility between binaries and database versions, and what happens when the CLI and the desktop update at different times, is the matrix in [ADR-016](./adr/ADR-016-outcome-persistence.md). Release notes must say: update every surface before enabling; an older binary refuses a v5 database; rollback = old binary plus the pre-v5 backup.

### Settings split (shared vs per surface)

| Key | Section | Notes |
|---|---|---|
| `enabled`, `max_parallel_workers`, `max_attempts_per_work_item`, `max_cost_usd` (outcome), `attempt.max_cost_usd`, `attempt.max_wall_secs`, `worktree_retention_days`, `decision_expiry_hours`, `idle_exit_minutes`, `network.allowed_hosts` (reuses `synt.sandbox.network.allowedHosts`, not duplicated) | **`synt.orchestrator.*`** | Shared by all surfaces; user or managed only; project file ignored with a diagnostic; managed wins (`synthesis-settings/src/lib.rs:6-9`). Security- and money-relevant, so never per surface. |
| Notification command | existing `synt.notify` | Unchanged (`docs/UNATTENDED.md` Notifications). |
| Panel visibility, density, picker style | `synt.cli.outcomes.*`, `synt.tui.outcomes.*`, proposed `synt.superagent.*` | Rendering only; no effect on behaviour. |
| Any orchestrator key found under `synt.cli` or `synt.tui` | n/a | `synt doctor` reports it and it is ignored (precedent: CLI-only keys under `synt.tui`, `CLAUDE.md`). |

The 10 USD per-attempt default of the desktop WSL cap (`docs/UNATTENDED.md:23`) is that surface's existing setting and is not changed; orchestrated attempts take their cap from `synt.orchestrator.attempt.max_cost_usd`.

## 6. Deliberately deferred

External event adapters and inbox (the design's GitHub PR, AC-14); remote workers and `WorkerTransport` (the design's remote-worker PR); Inline+Spill context and scoped retrieval (the design's context PR; the existing memory store, ADR-013, covers part of it); watchdog and adaptive recovery ladder (the design's watchdog PR; local liveness is the OS lock); model planner and replanner (stub planner first); merge and push (D8); resource locks beyond the integration key; Cedar; multi-approver roles; telemetry export and the design's metric names (events provide the counters locally); MCP in attempts; auto judge in attempts; native Windows execution; a daemon beyond the idle-exit orchestrator process; the Super Agent implementation (specified in ADR-021, needs the owner's design).

## 7. Open owner decisions (recommended default)

| Id | Decision | Recommended default | Blocks |
|---|---|---|---|
| D1 | Amend ADR-009 to allow an orchestrator (scoped to outcome mode, opt-in) | Yes, behind `synt.orchestrator.enabled`, off by default | Everything |
| D2 | Outcome state in `synt.db` via a hidden control session (vs separate DB) | Control session | PR-02 |
| D3 | Native Windows: refuse writable attempts, WSL2 only, for CLI and desktop; AppContainer later via an ADR-010 exception | Refuse | PR-05 |
| D4 (revised) | Orchestrator lifetime. Options: L1 in-process per surface; L2 standalone orchestrator process with durable-state recovery; L3 engine per surface (wrong lifetime). Lifetime must be independent of any surface | **L2** with `outcome resume` as the degraded fallback; justification and costs in ADR-017 | PR-12 |
| D5 (revised) | Outcomes placement per surface | CLI: `synt outcome ...` plus `/outcomes`; TUI: "Outcomes" group in the Workspace panel plus decision picker; Super Agent: primary object in its own design (specified, built later) | PR-13, PR-14, PR-15 |
| D6 (revised) | Inspector | One tabbed container, same tab set (`Overview`, `Workers`, `Activity`, `Diff`, `Evidence`) defined once in `synthesis-view`; Super Agent: its single right-side inspector; TUI: right drawer region while an outcome is selected; CLI: `synt outcome inspect --tab`. Needs the owner's design for TUI and Super Agent | PR-14, PR-15 |
| D7 | Desktop `/new` with no selected workspace | Refuse with a visible note | PR-03 |
| D8 | Merge and push in the first release | Neither: branch and patch only | PR-10 scope |
| D9 | Worktree retention after an outcome closes | 7 days, never auto-delete dirty trees | PR-04 |
| D10 | Budgets | Cost cap required per outcome (no default amount); `max_parallel_workers` 2; retries per work item 2; per-attempt cap from `synt.orchestrator.attempt.max_cost_usd` | PR-06 |
| D11 | Auto judge in attempts | Off | PR-08 |
| D12 | MCP and browser tools in attempts | Excluded by the tool allow-list in the first release | PR-08 |
| D13 | Planner | Deterministic stub first, model planner after the gate | PR-07 |
| D14 | Approver model | Single local user | PR-10 |
| D15 | Extra state paths to deny beyond `chain.key` | Decide per path with a test | PR-S1 |
| D16 | Outcome data retention and export | Same as sessions (no cleanup yet); `synt outcome export` after the MVP | later |
| D17 (new) | Settings section for the Super Agent | `synt.superagent.*`, rendering only, created when that surface exists | PR-15 |
| D18 (new) | Which settings are shared | Exactly the `synt.orchestrator.*` table above; nothing security- or budget-related under a surface section | PR-02 |
| D19 (new) | Migration policy | Explicit opt-in step with backup and notice; a default open never migrates | PR-02 |
| D20 (new) | Orchestrator idle exit | 10 minutes with no active outcome, command or client | PR-12 |
| D21 (new) | Notification content when no surface is open | ids and codes only through the existing notify hook; no brief text | PR-12 |
| D22 (new) | Remove the `InProc` transport from production builds | Keep it for tests and headless CI only (test feature or `cfg(test)`), never a surface default | PR-11 |
| D23 (new) | Sandbox of an attempt on the wire | The owner records the backend and filesystem guarantee it resolved at launch (`attempts.sandbox_*`, DDL revision 1009, set once, no MAC, never from the worker); `attempt.inspect` sends `sandbox{backend,guarantee}`, omitted for older attempts; surfaces show it as server-reported | PR-11 follow-up |

## 8. Risks specific to the plan

- The standalone orchestrator process (D4) is the largest new surface area; if the owner rejects it, outcomes become resume-driven (L1) and multi-surface rule 3 weakens to "survives via resume".
- The control-session idea is the least conventional choice; if the owner prefers a separate store (D2), PR-02 grows (own chain code) by roughly one size step.
- PR-05 and PR-08 together carry most of the risk; do not merge PR-05 without the flag.
- The desktop job and `design_tests` are the slowest and flakiest gates; the plan keeps the engine testable without them.
- Estimates do not include owner review latency or CI reruns of known flaky jobs (`docs/STATUS.md`).

## Owner decisions recorded during PR-09 review (not decided silently)

- **User-configured git filters (git-lfs and similar) at verification time: DISABLED.** The independent checkout is made through a scratch repository that never reads the user's repository config, so no filter driver runs on the host outside the sandbox. A repository that needs LFS content for its checks verifies those files as pointers (the check will usually fail or be flagged); enabling filters would need an explicit, per-contract opt-in decided by the owner.
- **Wording of "independent".** "Independent" = the repository's own frozen checks exit 0 on a clean checkout of attacker-writable code, under a sandbox, run by the host. It is not "the requirement was tested". Use this wording in UIs and docs; a flagged change escalates to a person.

### Recovery follow-up (after the e2e kill-9 matrix)
- **Status (implemented):** adopted live attempts are reconciled on every tick (`WorkerHost::reconcile_adopted`, `recover_item`): alive and renewed, settled from the session store, or lost; a live attempt's reservation no longer blocks the outcome; a lost attempt keeps its reservation as spent and the item gets a new attempt while budget remains, else `blocked` with the numbers; an answered decision moves a `blocked` outcome back to `running` by itself (`answered_waiting`). See ADR-017/019/020 recovery notes and section 3c of the acceptance document. **Residuals:** a sole attempt that held the whole cap and was lost leaves no budget (a person raises the cap or sets `attempt_max_cost_usd`); the once-per-decision unblock guard is in memory; the adopted worker's lease is renewed by the owner only within its wall cap; Linux and Windows are not run.

## B1 + B2 status (enable disclosure, decision push, durable notifications)

Design `docs/specs/orchestrator-remaining-backend.md`; notes in ADR-021 ("B1 + B2"). Implemented: the 3b item 6 disclosure at enable time (CLI notice, `orchestrator.enable` reply `disclosure`, client refuses a reply without it); red-team case `inj-readme-approve-decision`; test `sql_inserted_evidence_never_passes` (3b item 4); `attach` push of decision hints (protocol stays 4); `critical` dropped from `needs_you`; durable at-most-once notice dedupe in `outbox`; `synt outcome decisions --watch`. Not implemented (other PRs of the design): `ENABLE_GATE_OPEN` and the refusal while it is closed (B1 gate const, B8), the OrchGateway, re-briefing, `edit_scope`, the model planner. 3b item 6 is therefore met in code but `enable` still does not refuse on the other 3b items; opening the gate stays the owner's decision.

B3 (`feat/orchestrator-b3`): implemented the `request_evidence` re-brief (new decision with `brief.supersedes`, same stored action and binding hash, old row `invalidated`, evidence summary from stored rows with no model, at most 3 re-briefs, expiry carried over, owner keep-alive while a re-brief is due) and the lows of #148 (enable disclosure id and text check, pinned hash test, EVENT_BACKED isolated in `sql_inserted_evidence_never_passes`, attack-output check in `inj-readme-approve-decision`). Not implemented: running the independent verifier for the blocked commit, `edit_scope` and `outcome.contract.revise` (B4), the OrchGateway, the model planner. Notes: ADR-019 (B3), ADR-021 (B3).

B4 (`feat/orchestrator-b4`): implemented `edit_scope` follow-up: `outcome.contract.revise` (next contract version, unapproved, only while `blocked` with no live attempt), `outcome.contract.diff` (server-computed diff with `widens` and `weakens`), `synt outcome revise`, rebind on the person's hash-bound approval (decisions of the old version invalidated, unused grants revoked, evidence of the old plan not counted, replan and `running`). Not implemented: revising a paused or running outcome, a separate approval of the stub replan, the OrchGateway, the model planner. Notes: ADR-019 (B4), ADR-021 (B4).

B5 (`feat/orchestrator-b5`): implemented the `OrchGateway` table (`synthesis_orchestrator::gateway`, ADR-019 B5 notes): pure default-deny `(principal, action)`, principal only from the attached link, exhaustive `Call::action()`, every owner command routed and audited in its transaction. Not done: per-effect authorization of `plan.install`/`notify` inside the owner loop, `outcome.contract.revise` (B4), `planner.spawn` (model planner, B6/B7). Not a security boundary against a same-user process holding the key.

B6 (`feat/orchestrator-b6`): implemented the planner foundations, with NO model call and nothing enabled by default: `PlanItem.title/instructions` (capped, hidden characters refused, not stored when empty so the stub plan hash is unchanged); the pure `validate_model_plan` (`ReasonCode`, size, item, depth caps); the `plan_approval` decision kind (stored action = the validated plan plus the stub plan as the alternative, brief with the item table and the diff against the stub and the current plan, `approve_model_plan`/`use_stub_plan`/`reject`, installed only through `install_plan_from_decision` from the stored action, invalidated when the contract version, plan version or policy hash moves); `plan_runs` (DDL revision 1010: MAC'd identity, set-once settlement, reservation before the run counted in `outcome_spend`, at most 32 runs, one at a time); `synt.orchestrator.planner.{mode,model,max_cost_usd,max_items}` (user/managed only, default `stub`, 1 USD, 16 items, ceilings 10 USD and 256) and a doctor row; `planner.spawn` allowed for the orchestrator principal only. Not implemented (B7): the planning session, prompt, repair round, read-only profile, fallback decision, recovery of a lost run, `outcome.budget.planner_spent_usd` on the wire, a surface render of `plan_approval`. Notes: ADR-019 (B6), ADR-021 (B6).

B7 (`feat/orchestrator-b7`): implemented the model planning run behind `synt.orchestrator.enabled` and `planner.mode = model` (default `stub`: nothing changes for anyone who does not set it). Owner side: `planrun` (read-only sandboxed `synt run --plan-session` on a fresh checkout of the base commit, fixed profile, prompt that quotes the contract as data, ONE repair round with a fixed sentence per `ReasonCode`, validation by `validate_model_plan`, a `plan_approval` decision for a valid plan, a stub-only decision for every failure and never an automatic switch, settlement from the session's real usage with unknown usage keeping the reservation, recovery of a run an owner died on as `lost`, one run per contract version, an unpriced model refused before anything is reserved); gateway `planner.spawn` called and audited; `outcome.start` waits while the plan is made and approved; `plan_text_block` (B6 review item a); the cap check inside `reserve_budget` (item b); `install_approved_plan` takes the user's `max_items` (item d); `outcome.budget.planner_spent_usd` on the wire (no surface renders it). Tests: `crates/synthesis-orchestrator/tests/planrun.rs`, `apps/cli/tests/outcome_e2e.rs` (B7 section). Not implemented: the model replanner (B9), killing a planner an owner left behind (bounded by its caps), a replan after an expired decision without a revised contract, a surface render of `plan_approval` and `planner_spent_usd`, `prompt_sha256` in an event. Unverified here: Linux and Windows (the e2e needs an enforcing sandbox and is unix only; native Windows has no enforcing sandbox, so a planning run there fails closed into the stub-only decision). Notes: ADR-017 (B7), ADR-019 (B7), ADR-021, threat model T-38 to T-40.

B9 (`feat/orchestrator-b9`): implemented the model replanner, retry from the blocked attempt's commit and the B7 start dead-end fixes, all behind `synt.orchestrator.enabled` and `planner.mode = model` (default `stub`). Replanner: `replan_trigger` (an outcome `blocked` or `rework_required` with a plan of the current contract and an item in `terminal_failure`, `blocked`, or waiting for a decision that is closed without an answer that lets it run, or a revised contract whose plan is the old one) makes `needs_model_plan` true; the same `start_plan_run` runs a planning session of the SAME fixed read-only profile and reservation, with a prompt that quotes the approved contract as data and the state of the plan being replaced as fixed codes only (item keys, kinds, `requires`, capabilities, `covers`, item and attempt states, ordinals, retries, the verdict code of each check by contract check id, budgets): never a transcript, diff, decision brief, `task_status`, plan `title`/`instructions` or any worker text. The answer goes through `validate_replan` (= `validate_model_plan` plus: a kept key keeps its kind, no capability that no item of the current plan has; the second rule is waived when the current plan is of an older contract); a refusal is a fixed `ReasonCode` (`replan_edit` is new). The result is a `plan_approval` whose brief carries `diff_vs_current_plan` with every field of the added, removed and changed items, `trigger`, and the choices `approve_model_plan`, `use_stub_plan` (the stub replan) and the new `keep_current_plan` (offered only when the current plan is of the current contract; it installs nothing and the outcome stays as it is); any failure creates the same decision without the model's plan. Nothing is applied without the person; `succeeded` items are not carried over (O8); replay never runs the model (one decision or run per `(contract version, plan version)`). Retry from commit: `carried_commit` gives the new attempt's start commit (the blocked attempt's `subject_commit`, only when a person's MAC-verified answer that means "continue" (a question answer or `grant_once`, never `retry`) to a decision bound to that attempt and commit exists), passed as the worktree's start; the scope check still diffs against the BASE commit, and `verify_agent` no longer reuses a scope row of an earlier attempt (the new attempt is inspected again). Dead ends: `plan_wait` now also reads `outcome.planner_failed` (written when the stub plan is invalid too, and by the owner when `start_plan_run` fails) and `outcome.plan_install_refused` (an approved decision that cannot install), so `outcome.start` is denied with the "revise the contract" message instead of staying queued. `answered_plans` only counts the decision that installed the current plan and never `keep_current_plan`; the owner also moves a `rework_required` outcome back to `running` after an installed replan. Tests: `crates/synthesis-orchestrator/tests/{planrun,carry,model_plan}.rs`, `apps/cli/tests/outcome_e2e.rs` (B9 section). Not implemented: a surface render of the replan diff; carry-over of succeeded items (O8); a replan after an expired decision of the same plan state (the decision is the answer); a kill of a replanning run an owner left behind (as B7). Unverified here: Linux and Windows (e2e is unix only and needs an enforcing sandbox).
