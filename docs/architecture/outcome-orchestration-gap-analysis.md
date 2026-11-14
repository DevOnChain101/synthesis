# Outcome-centric orchestration: gap analysis

Status: Proposed, for owner review. Baseline: `origin/main` b230f4df (2026-11-14). Input: "Synthesis, Outcome-Centric Agent Orchestration" design v1.0 (2026-11-02, called "the design" below; requirement ids HARD-1..12 are the twelve MUST items of its section 0, AC-01..15 its section 28).

Method: every row below was checked by opening the cited file at this commit. "Read" means verified by reading code; nothing here was executed unless it says "ran". Anything not verifiable in code is marked **unverified**. Line numbers are for this commit and will drift; the function or type name is the stable anchor.

Scope (revised 2026-11-14): the orchestrator serves **all three surfaces**: the CLI (the `synt` binary, subcommands and terminal app), the TUI (the desktop's terminal-style tab) and the Super Agent (the desktop's Agent tab, "Coming soon" today). Terminology follows `CLAUDE.md` strictly; "desktop" alone means the app that hosts TUI and Super Agent.

Related documents: ADRs in [adr/](./adr/README.md), [plan](./outcome-orchestration-plan.md), [threat model](./outcome-orchestration-threat-model.md), [acceptance tests](../testing/outcome-orchestration-acceptance.md), [MVP slice](./outcome-orchestration-mvp-slice.md).

## 1. Summary

The repo already owns most of the execution plane the design needs: a durable per-session store with a tamper-evident event chain, a worktree allocator, an OS sandbox with an egress proxy, a deny-first permission engine, an evidence-producing verification gate, cost/wall caps, stop and crash recovery. What does not exist is everything above one session: no Outcome, Plan, Attempt or Worker type, no scheduler, no revision-checked state transitions, no outbox, no decision objects bound to a revision, no UI surface. The proposed work is therefore an additive layer (new tables in `synt.db`, one new domain crate, one new orchestrator crate, protocol v4) over the existing crates, not a rewrite.

Four conflicts between the design and this repo decide the shape of the work:

1. **"Task" already means something else.** In this repo a Task is one prompt-run inside a session (`tasks` table, `task.started/ended`, `TaskStatus`). The design's Task is a DAG node. This document and the ADRs call the DAG node a **work item**; "Attempt" is new.
2. **ADR-009 defers orchestrators.** ADR-009 ("Defer complex multi-agent orchestration", amended only for messaging by ADR-012) and design doc section 1.2 list multi-agent collaboration as a non-goal. The orchestration ADR must amend ADR-009 explicitly; this needs an owner decision (plan, section 7, D1).
3. **Several guarantees the design assumes are weaker here:** no native Windows sandbox (so no unattended writable attempts on native Windows), the Linux sandbox reads the whole filesystem, `$SYNT_HOME/chain.key` is not on the sandbox deny list, MCP stdio servers and the browser worker run outside the sandbox.
4. **Three surfaces, no shared runtime today.** The CLI runs its engine in process and does not use the engine protocol; the desktop uses the protocol only for WSL (`apps/desktop/src/remote.rs:20`); the only cross-process lock is per session (`store.rs:1597`); notifications exist only for governed runs and carry ids and codes (`synthesis-app/src/notify.rs:5-9`). Serving three surfaces needs one protocol, one owner process per outcome and revision-checked commands (ADR-016, ADR-017, ADR-021, section 9 below).

## 2. Requirement matrix: HARD-1..12

Legend: Have = exists and usable as is; Part = exists, needs extension; Gap = absent.

| Id | Requirement (short) | Current state, with evidence | Status | Gap / action |
|---|---|---|---|---|
| HARD-1 | Rust control plane, local-first, future remote boundary | Core is Rust (ADR-010, CLAUDE.md). The desktop runs the engine in process (`docs/specs/desktop.md` section 2) or as `synt engine --stdio` over JSONL, already the remote boundary used for WSL (`crates/synthesis-app/src/proto.rs:19` `PROTOCOL = 3`; ADR-015). | Part | A `WorkerTransport` trait is not needed for MVP; the stdio engine protocol is the seam. Protocol v4 must also serve the CLI, which does not use it today (ADR-021). Remote workers deferred. |
| HARD-2 | Separate Outcome / Plan / Task / Attempt / Worker / Agent / Session / Worktree | Session: `SessionId` String alias (`synthesis-core/src/session.rs:12`), 14-state machine (`session.rs:17-76`). Task: prompt-run (`synthesis-events/src/types.rs:118` `TaskStatus`; `tasks` table `synthesis-session/src/schema.rs:24`). Agent: `AgentDef` (`synthesis-harness/src/agents.rs:7`). Worktree: `GitInfo` (`synthesis-workspace/src/worktree.rs:43`). Workspace: `WorkspaceContext` (`synthesis-core/src/workspace.rs:22-38`). No Outcome, Plan, Attempt, Worker (read; grep over `crates/` and `apps/`). | Part | New domain types (ADR-017). Rename the design's Task to work item to avoid the collision. |
| HARD-3 | Isolated worktree per concurrent writable task; a new session never inherits another's cwd | Worktree per **session**, branch `synthesis/<session_id>` (`worktree.rs:283`), stored under `$SYNT_HOME/worktrees` (`synthesis-app/src/wt.rs:136`; the crate default `<repo>/../.synt-worktrees` at `worktree.rs:276` is overridden). On by default for `synt run` and the stdio engine, not for the interactive CLI or `--host desktop` (`docs/UNATTENDED.md:14,19`). Distinct worktrees for concurrent sessions are tested (`synthesis-workspace/tests/workspace.rs:376`, `:417`). Process cwd is read in one place only (`synthesis-app/src/lib.rs:84`, enforced by `clippy.toml:1-6`). | Part | Attempt = new session, so it gets a unique worktree for free (ADR-018). cwd inheritance audit: see AC-03 row. |
| HARD-4 | Durable state authoritative; LLM context is disposable | `synt.db` in WAL with foreign keys (`store.rs:243-247`); events append in an IMMEDIATE transaction (`store.rs:1863-1900`); `messages` table holds the exact provider history for resume (`docs/specs/session-store.md` B.2). Nothing above session level is stored. | Part | Outcome/plan/attempt/decision tables (ADR-016). |
| HARD-5 | Policy before every privileged side effect (tools, network, git, merge, secrets, deploy) | `Permissions::check` (`synthesis-permissions/src/lib.rs:2453`) -> `decide_tool` (`:2509`), called before execution in `synthesis-harness/src/exec.rs:744-751`. Irreversible table (`irreversible.rs`), read-only hard deny (`lib.rs:2678`), sandbox, egress proxy. Bypass paths listed in section 4. `WorktreeManager::execute_merge` (`worktree.rs:581`) has no non-test caller (grep), so merge is not reachable today. | Part | Close or route the bypass paths for orchestrated attempts; add a host-side gateway for orchestrator effects (ADR-019). |
| HARD-6 | A worker cannot approve itself, change its policy, or promote a recommendation to evidence | Approvals are answered by a person only (`docs/specs/desktop.md` section 8, "Asks are answered by a person only"); hook `allow` is ignored (`exec.rs:648`); model-declared checks never count (`synthesis-verification/src/gate.rs:134,155,412`, `plan.rs:192-195`); a delegated worker's mode is capped by the sender's (`apps/cli/tests/e2e.rs:2106` e8c) and reply text grants nothing (`:2175` e8f). | Part | Same rules must hold at outcome level: the orchestrator, not the attempt, creates decisions and evidence (ADR-019, ADR-020). |
| HARD-7 | Delta-oriented decision brief, traceable evidence, no default approve, no bulk approve | `ask.approval` carries tool, input, prompt, reason, suggestions (`proto.rs:308`); `Choice { Once, Always, Deny }` (`synthesis-app/src/asks.rs:55-59`); an ask has no expiry or revision binding (read; `asks.rs` has no timer, **unverified** for the desktop dialog layer). | Gap | `decisions` + `decision_approvals` bound to a revision; "Always" disabled inside orchestrated attempts (ADR-019). |
| HARD-8 | Pause/resume/cancel/recovery, idempotency, replay safety, crash restart | Cancel via token; `synt stop` sets `sessions.stop_requested_at` (`store.rs:781`, `synthesis-app/src/sessions.rs:45`); resume (`store.rs:952`); startup recovery marks orphaned sessions and probes processes (`synthesis-session/src/recover.rs:163-190`); crash tests (`synthesis-session/tests/crash.rs:79,141`). No idempotency keys, no outbox (grep `idempot`: only doc comments). | Part | Revision CAS, outbox, unique natural keys (ADR-016); orchestrator recovery (ADR-017). |
| HARD-9 | Audit all decisions and sensitive effects; redact secrets in logs and prompts | HMAC event chain: `events.prev`, `sessions.chain_head`, `chained` (migrations v3, v4, `schema.rs:68-80`; `store.rs:1183` `verify_chain`; key `<home>/chain.key`, `trust.rs:250-263`). Redactor (`synthesis-events/src/io.rs:11`). Chain covers events only (`docs/UNATTENDED.md:49`). | Part | Outcome events go through the same append path (ADR-016) so they are chained and redacted. |
| HARD-10 | Providers pluggable | `synthesis-providers`: Anthropic and OpenAI-compatible adapters, failover (`synthesis-providers/src/failover.rs`; `docs/UNATTENDED.md:57`). | Have | Unchanged. API keys only (ADR-011). |
| HARD-11 | UX keeps Workspace + Sessions, elevates Outcomes/Decisions, ONE right-side tabbed inspector | Desktop views `View { Tui, SuperAgent, Settings }` (`apps/desktop/src/ui.rs:29-33`); the Super Agent is "Coming soon" and the owner says its design comes later (HANDOFF 1b). The TUI has left panels, centre tabs `ChatTab { Chat, Diff, Terminal, Browser, Files }` (`apps/desktop/src/tui_view/mod.rs:89-96`) and a right panel stack as a drawer (`docs/specs/desktop.md`). The CLI app renders the same `synthesis-view` models. There is **no** single right-side tabbed inspector today (read). | Gap | ADR-021 section 3: per-surface Outcomes entry, one inspector tab set defined once in `synthesis-view`; Super Agent specified only. The owner's design files (`~/Vibe/synthesis-cli-ui/cli.html`) were not read: **unverified**. |
| HARD-12 | No mandatory cloud or central server | Local store, no listener in the engine; peers use a per-session unix socket or named pipe, not TCP (ADR-012). | Have | Keep: no TCP/HTTP listener in any new component. |

## 3. Acceptance matrix: AC-01..15

Test specs are in [acceptance](../testing/outcome-orchestration-acceptance.md).

| Id | Current state (evidence) | Reusable today | Gap |
|---|---|---|---|
| AC-01 contract + validated DAG | Verification `Criterion {id, text, source, check, status, evidence}` (`synthesis-verification/src/types.rs:70`); `--criteria-file` JSON on `synt run` (`run.rs:386`). No contract versioning, no plan, no DAG validation. | Criterion shape | Contract, Plan, validator (pure code). |
| AC-02 two parallel writable tasks, distinct dirs/branches/sessions | Distinct worktrees and ports for concurrent sessions are tested (`workspace.rs:376`). | Yes, for the isolation half | Scheduler that starts two attempts; task-scoped context. |
| AC-03 new session never inherits cwd | CLI: `/new` opens the **selected** workspace, test `synthesis-cli-ui/src/app/tests.rs:2958` (`panels.rs:152-157`). Resume matches by exact cwd (`run.rs:498-558` `is_here`). Desktop (TUI) `Action::New` takes the selected workspace root, **falls back to the previous engine's root** when the selection is missing (`driver.rs:2077-2092`); `switch` uses `cfg.project` (the launch folder) for non-`New` targets (`driver.rs:1616-1624`); WSL verifies the engine-reported folder (`apps/desktop/src/wsl.rs:509-520` `check_reported`). Design doc section 5.2 already states the invariant. I did **not** reproduce a failure: the bug the owner refers to is **unverified**. | CLI test; WSL check | Desktop test for each `Target` and a "no fallback to previous root" decision; audit of the three entry points (CLI launch, desktop TUI, WSL; the Super Agent has none yet and must call the protocol) in PR-03; from then on the orchestrator passes cwd (multi-surface rule 8). |
| AC-04 restart without duplicate effects or lost decisions | Session-level recovery (`recover.rs:163`) and crash tests exist. No outcome state, no outbox. | Recovery scan, crash test harness (`synthesis-session/tests/crash.rs`) | Outbox, revision CAS, orchestrator recovery. |
| AC-05 in-scope read/reversible actions proceed, out-of-scope privileged stop | Permission modes, deny rules, irreversible table, read-only set. | Engine | Envelope -> rules compiler; escalation instead of terminal ask. |
| AC-06 worker cannot change permissions via prompt, tool output or forged event | Hook allow ignored (`exec.rs:648`); `.synt/` is read-only to tools and sandbox (`synthesis-sandbox/src/lib.rs:217-226`); red-team cases `inj-*`, `prot-*`. | Red-team suite | Forged **orchestrator** events: the attempt must have no write path to outcome tables (sandbox denies `synt.db`, `lib.rs:484-487`). |
| AC-07 failed test -> bounded retry -> blocked with brief | `max_verify_attempts` (`gate.rs:30`, `events/types.rs:76-85`); gate ends `unverified` (exit 3, `run.rs:3071`). No work-item retry, no brief. | Gate | Retry budget per work item; brief builder. |
| AC-08 every criterion has current, provenance-linked evidence | `CheckResult` has `declared_by`, `exit_code`, `criteria_ids`, `artifacts` (`types.rs:143-165`); `aggregate` sets per-criterion status and `evidence` ids (`gate.rs:78-95`; so a per-criterion verdict **does** exist, correcting an earlier note); `report_sha256` on the event (`events/types.rs:492-495`). Missing: subject commit, environment fingerprint, trust class, freshness. | Gate, report | `evidence` table and independent runner (ADR-020). |
| AC-09 changed commit/plan/policy invalidates decisions | No decision objects. | none | ADR-019 bindings. |
| AC-10 decision brief content, no preselected approve | See HARD-7. | none | Brief schema, UI rules. |
| AC-11 outcome summary without worker chatter | Sub-agent transcript is a separate pane (`docs/specs/desktop.md` Super Agent section); events are per session. | Per-session event streams | Outcome control-session stream with milestone events only. |
| AC-12 local without server; providers pluggable | See HARD-10, HARD-12. | Yes | Keep. |
| AC-13 cancel kills subtree within limits, keeps artifacts | `kill_tree` (`synthesis-terminal/src/lib.rs:493`); stop kills process groups after 15 s (`docs/UNATTENDED.md:45`); worktree cleanup refuses dirty or unmerged work (`worktree.rs:628-660`). | Yes | Wire to attempt cancel; test with a spawning tree. |
| AC-14 replayed external event creates no duplicate | No external event ingress, no inbox. | none | Out of MVP (deferred, plan section 6). |
| AC-15 provider failure never bypasses policy or verification | Provider failure ends the task `model_error`; verification and policy are separate code paths (`docs/UNATTENDED.md` reason codes). The gate treats unchecked work as `unverified`. | Yes (read) | A test at outcome level. |

## 4. Policy bypass paths (HARD-5)

All are read from code. "Closed for attempts" means the ADR-019 decision removes the path inside an orchestrated attempt.

| Path | Evidence | Why it matters | Resolution |
|---|---|---|---|
| `!cmd` person commands | `synthesis-harness/src/user_cmd.rs:195` (runs `Bash` with `UNCONFINED` scoped to `shell.user_commands_unsandboxed`) | Human-initiated; unsandboxed when the setting is on | No input channel exists in an attempt; keep. |
| Harness-runtime tools not policy-checked | `exec.rs:566-574` (`Plan::Special` for runtime-`Harness` tools except `Task`, `SendMessage`, `ListAgents`, `ListSessions`; the interface tools are `AskUserQuestion`, `ExitPlanMode`, `ToolSearch`) | `AskUserQuestion` in an attempt would wait for a person | Closed for attempts: route to a Decision (ADR-019). |
| `bypassPermissions` | `synthesis-permissions/src/lib.rs:1821`; `docs/SECURITY.md:20` | Skips prompts; shell redirects and repo scripts run, sandbox only | Closed for attempts: the mode is never granted (`--unattended` already refuses it, `docs/UNATTENDED.md:27`). |
| Hooks | `exec.rs:596-660` (tighten only; `allow` ignored) | User-level hooks run code | Keep; project hooks stay untrusted. |
| MCP servers | `synthesis-harness/src/mcp_guard.rs:341-351` (read-before-write guard only); stdio servers and browser worker outside the sandbox (`docs/specs/sandbox-hardening.md` section 4, "not built") | A server's own effects are invisible to policy | Closed for attempts: only managed/user-listed servers, destructive tools escalate; sandboxing MCP is a hardening PR (plan, PR-H). |
| Programs that act by themselves (`./deploy`, a built binary) | `docs/SECURITY.md:22` limits | Sandbox and network deny are the only control | Network deny/hosts per envelope; no change. |
| Git alias / repo script in auto and bypass | `docs/SECURITY.md:102` (red-team known gap) | Same | Envelope forbids bypass and auto-allow for attempts; auto judge stays fail-closed. |
| Orchestrator host effects (worktree create/cleanup, merge, decision resolve) | New code | Would be a new unpoliced actor | Gateway with principal `orchestrator` (ADR-019). |

## 5. Persistence facts the design must respect

| Design assumption | Repo fact | Correction |
|---|---|---|
| Event envelope "schema version 4" (carried over in some notes) | `SCHEMA_VERSION = 1` (`synthesis-events/src/lib.rs:13`); additive types and fields keep `v` (`docs/specs/events.md:135-138`). The number 4 is the **DB migration count** (`schema.rs:81-83`, `LATEST`). | New `outcome.*` event types are additive: `v` stays 1. New DB migration is v5. |
| UUIDv7/ULID ids | Ids are UUIDv4 TEXT (`docs/specs/workspace-context.md` section 1) with `created_at` ordering | Keep UUIDv4; do not introduce a second id scheme. |
| `event_log(aggregate_id, aggregate_revision)` unique | `events` PK is `(session_id, seq)` WITHOUT ROWID (`schema.rs:29-33`) | Give each outcome a hidden control session so its events reuse the append path, chain, redaction and single-writer lock (ADR-016). |
| Separate `synthesis-store` crate | Persistence lives in `synthesis-session` (6k lines); migrations are a private const ladder (`schema.rs:9-81`) with `SchemaTooNew` (`schema.rs:88`) | Add migration v5 and an `outcome` module there; no second store. |
| Artifact CAS under app data | Artifacts are per-session files with sha256 and retention in `artifacts` (`schema.rs:49-54`) under `$SYNT_HOME/sessions/<id>/artifacts/` | Evidence references (session, artifact id, sha256). Global CAS deferred. |
| Single-writer SQLite | One writer per session via advisory lock (`store.rs:1597-1615`); WAL; busy timeout 5 s | The outcome control session's lock is the single-orchestrator guard. |
| Heartbeats and fencing for leases | Liveness of a local session is the OS lock, released by the kernel on death (`store.rs:1597`, `recover.rs:184-188` treats `Locked` as live) | Local mode needs no heartbeat; keep a fence counter for stale-write rejection; heartbeats only for remote workers (deferred). |
| Chain key unreachable by workers | Chain key is `<home>/chain.key` (`trust.rs:250`); the trust key is `<home>/run/trust.key` (`trust.rs:172`). `synthesis_state_paths` denies `auth.json, settings.json, synt.db*, memory, projects, run, browser-profiles` (`synthesis-sandbox/src/lib.rs:480-497`) and **not** `chain.key`. Linux bubblewrap binds `/` read-only (`linux.rs:810`), so a sandboxed command can read `chain.key` there; macOS Seatbelt reads from an allow-list (`seatbelt.rs:120-165`) that does not include `$SYNT_HOME`. Read, **not executed**; also `docs/UNATTENDED.md:49` claims the sandbox denies `$SYNT_HOME`, which is broader than the code. | Add `chain.key` to the deny list (one line, ADR-020) and verify on Linux CI (new red-team case `evid-chain-key-read`). Forging a chain still needs write access to `synt.db`, which is denied, so impact is limited to combined attacks. |

## 6. Reusable components

| Need | Reuse | Where |
|---|---|---|
| Durable sessions, events, artifacts, locks, recovery | extend | `synthesis-session` |
| Isolated checkout per attempt, merge plan, cleanup, repo lock | reuse | `synthesis-workspace/src/worktree.rs` (cross-process lock `synt-worktree.lock` exists, `worktree.rs:704-757`; an earlier note saying in-process only was wrong) |
| Unattended preset (wall cap, cost cap, network deny, sandbox enforce) | reuse as the attempt launch profile | `synthesis-app/src/run.rs` (`--unattended`, `docs/UNATTENDED.md:25-27`) |
| Sandbox and egress | reuse | `synthesis-sandbox`, `synthesis-egress` (`NetworkPolicy::Proxied`, `synthesis-sandbox/src/lib.rs:44-68`) |
| Permission engine | reuse, wrap | `synthesis-permissions`, `synthesis-settings` (managed > CLI > local > project > user, `synthesis-settings/src/lib.rs:6-9`) |
| Verification gate, baseline, anti-weakening | reuse as in-attempt evidence | `synthesis-verification` |
| Budgets and cost accounting | reuse | `synthesis-events/src/types.rs:76-85`, `run.rs:859` |
| Process-tree kill | reuse | `synthesis-terminal/src/lib.rs:493` |
| Provider abstraction and failover | reuse | `synthesis-providers` |
| Engine protocol and host | extend (v4) | `synthesis-app/src/{proto,engine_server,host}.rs` |
| View models | extend | `synthesis-view` |
| Red-team harness | extend | `tests/redteam/{cases.mjs,run.mjs}` |
| In-session sub-agents | unchanged, inside an attempt | `synthesis-harness/src/agents.rs`, `hub.rs` |
| Peers and delegation | **not** the scheduler; keep for person-driven messaging | `synthesis-harness/src/peer.rs` (`MAX_IN_FLIGHT = 4`, `:44`), `synthesis-peers` (ADR-012) |

## 7. Module mapping: proposed crate -> repo

| Design module | Decision | Target | Reason |
|---|---|---|---|
| synthesis-core (domain types) | extend | `synthesis-core` (new `outcome.rs`: ids, state enums, transition tables, pure validators) | Precedent: `session.rs` holds the status machine. |
| synthesis-store | extend | `synthesis-session` (migration v5, module `outcome`) | One DB, one migration ladder, existing lock/chain/redaction. |
| synthesis-orchestrator | **new** | `crates/synthesis-orchestrator` (state transitions, scheduler, recovery) | Not session-scoped; must not depend on UI. Depends on core, session, workspace, app host. |
| synthesis-scheduler | merge | inside orchestrator (module `sched`) | Too small to be a crate (MVP is a bounded-parallel DAG walk). |
| synthesis-worker | extend | `synthesis-app` (`run::prepare` + `host`) used as the local worker adapter | Already starts a governed session in a worktree with caps. |
| synthesis-session (context, turns) | reuse | `synthesis-session`, `synthesis-harness`, `synthesis-context` | Unchanged. |
| synthesis-policy | extend | `synthesis-permissions` (+ envelope compiler in orchestrator) | Engine exists; add envelope-to-rules and host gateway. |
| synthesis-verification | extend | `synthesis-verification` (evidence, verdict) | `CheckResult`/`Criterion` are the base. |
| synthesis-decisions | **new** (module first) | `synthesis-orchestrator::decision`; split later if it grows | Avoid a crate for ~500 lines. |
| synthesis-memory | reuse | `synthesis-memory` (ADR-013) | Scoped memory exists; Inline+Spill is deferred. |
| synthesis-events (ingress) | defer | none | No external events in MVP. |
| synthesis-watchdog | defer | `desktop/remote.rs:691` is the only watchdog (engine silence); egress has idle timeouts | Liveness is the OS lock locally; progress watchdog is phase 6. |
| synthesis-provider | reuse | `synthesis-providers` | |
| synthesis-api | extend | `synthesis-app/src/proto.rs` v4, new `outcome_client` (InProc, Stdio, LocalSocket transports), orchestrator server in `engine_methods` style | One protocol for all surfaces. |
| synthesis-observability | defer | events + `synt sessions show` | Metrics export deferred; local counters from events. |
| surfaces | thin | CLI (`apps/cli`, `synthesis-cli-ui*`), TUI and Super Agent (`apps/desktop`), shared models in `synthesis-view` | Rendering, input, dispatch only; no dependency on `synthesis-orchestrator` (checked by a test). |

## 8. Corrections the repo forces on the design

1. **Language and process.** Rust only (ADR-010). No Tokio-external worker runtimes, no sidecars beyond those ADR-010 allows. `unsafe` is forbidden in the workspace (`docs/specs/sandbox-hardening.md:12`), so any Windows AppContainer work lives in a separate helper crate under an owner-approved exception.
2. **Credentials.** API keys only (ADR-011). No OAuth, no keychain. The design's "scoped credentials" for GitHub/MCP are out of scope; MVP has no external adapters.
3. **Environment.** Only `SYNT_*` plus the standard variables listed in CLAUDE.md are read, through the injectable `Env`; orchestrator code takes `&Env`, never `std::env`.
4. **No cloud.** No telemetry, no relay. Metrics stay local.
5. **Naming.** Display name Synthesis, technical `synt`. Commands are `synt outcome ...`; tables and events use `outcome`, `work_item`, `attempt`, `decision`, `evidence`. The design's worktree path under the pre-rename product directory is wrong here: worktrees are `$SYNT_HOME/worktrees/<session id>` (`wt.rs:136`). Pre-rename names are never read and the check script `scripts/check-no-legacy-name.mjs` fails on them.
6. **Windows.** Native Windows has no command confinement (`docs/SECURITY.md:45`); unattended runs refuse unless `--sandbox off` (`docs/UNATTENDED.md:17,88`). Orchestrated writable attempts therefore run on macOS, Linux and WSL2 (ADR-015), for the CLI and the desktop alike; elsewhere the refusal is explicit and names WSL2. State-machine, scheduler and store tests still run on all three OSes.
7. **Remote workers** are out of scope; the design's own phase 7. `WorkerTransport` is not introduced; the stdio engine protocol is the future seam.
8. **IPC (revised).** No TCP, no HTTP (design's "loopback HTTP" is rejected). Channels: stdio (WSL engine link), in-process (tests only) and a same-user unix socket or named pipe for the standalone orchestrator, reusing the peers' endpoint and mutual HMAC handshake (`synthesis-peers/src/net.rs`) with a new key in `run/` (sandbox-denied, `synthesis-sandbox/src/lib.rs:490-491`).
9. **Risk labels.** The design's R0-R4 tiers map onto existing machinery: R0/R1 = read-only / sandboxed writes, R2 = `irreversible.rs` ask class, R3/R4 = hard deny or decision. No new classifier in MVP.
10. **Decision "Always".** The existing "allow always" persists a rule in `settings.local.json`; inside orchestrated attempts it is disabled; policy changes are proposals reviewed by the person (design 16, "approval fatigue").
11. **Worktrees alone are not isolation.** Agrees with the design's note; sandbox enforce is mandatory for attempts and stated per backend (`docs/SECURITY.md` sandbox table).
12. **Terminology and settings.** CLI / TUI / Super Agent as in `CLAUDE.md`; settings `synt.cli.*` and `synt.tui.*` only for rendering, shared behaviour under the new `synt.orchestrator.*` (plan, "Settings split"). The code anchors for the sections are `CLI_SECTION`, `TUI_SECTION` (`synthesis-settings/src/lib.rs:942-944`).
13. **Task naming in the UI.** The desktop "Running" panel lists sessions (`synthesis-view` `RunningList`); work-item rows must not reuse the word "task".

## 9. Multi-surface rules

Normative for every ADR, PR and test of this program. Each rule cites the repo fact that forces it.

1. **One core.** Orchestrator, scheduler, decisions, evidence, policy compilation live only in `synthesis-core`, `synthesis-session`, `synthesis-orchestrator`, `synthesis-app`. Surfaces are thin clients. Test: a dependency check (cargo metadata) that `apps/*`, `synthesis-cli-ui*` and `synthesis-view` do not depend on `synthesis-orchestrator` or `synthesis-verification`'s evidence/verdict modules.
2. **One protocol.** v4 (ADR-021) is the only way a surface reads or changes an outcome, the CLI included, even in process (today the CLI has no protocol path; `synthesis-app/src/proto.rs:19` is used by the desktop for WSL only).
3. **Lifetime independent of surfaces.** A standalone orchestrator process owns outcomes; closing a surface never cancels; reopening elsewhere shows the same state (ADR-017). Today only per-session locks exist (`store.rs:1597-1615`); the control session's lock becomes the outcome owner lock.
4. **Single owner, many viewers, revision-checked commands.** Optimistic revisions and the command table are Phase 1 (PR-02), not later (ADR-016 items 11-12).
5. **Decisions are durable, not live asks.** `ask.approval` is live, has no expiry and no staleness (`proto.rs:308`, `asks.rs:55-59`); outcome decisions are rows, answerable from any surface, exactly one answerer by CAS (ADR-019 section 5).
6. **Structured brief, surface-specific rendering.** Surfaces must show every field, must not reorder, preselect or reinterpret (ADR-021 section 2).
7. **"Needs you" works with no surface open.** Derived signal plus the notify hook; payloads carry ids and codes only (`notify.rs:158`; ADR-021 section 4).
8. **One policy gateway; no side paths.** Same launch profile, same closed bypass paths for every attempt whatever surface created it; the closure is a gate before the feature can be enabled (ADR-019 section 6).
9. **cwd is the orchestrator's.** Surfaces send a workspace identity once; the orchestrator builds every attempt's cwd (ADR-018 item 4); tests for CLI, TUI(desktop) and WSL entry points.
10. **Platform refusal is uniform.** Writable attempts only on macOS, Linux and WSL2; the same refusal text everywhere.
11. **Settings split.** Rendering under `synt.cli.*` / `synt.tui.*` (and a proposed `synt.superagent.*`); behaviour, budgets and security under `synt.orchestrator.*`, user or managed only, never a project file (plan, "Settings split").
12. **Compatibility.** The v5 migration is explicit and opt-in; older binaries refuse a v5 database; matrix in ADR-016.
13. **Delivery order.** core, protocol, CLI, TUI, Super Agent (plan).
14. **Acceptance is headless first.** Protocol-level tests prove lifecycle; per-surface tests cover rendering and command dispatch only (acceptance document).

## 10. Risks

| Id | Risk | Impact | Mitigation |
|---|---|---|---|
| R-1 | ADR-009 not amended by the owner | Whole program blocked | Decision D1 first; ADRs are Proposed only. |
| R-2 | `synthesis-session` grows (6k lines) and a v5 migration makes older binaries refuse the DB (`SchemaTooNew`, `schema.rs:88`) | Rollback of the binary after use is blocked | Additive migration, automatic `VACUUM INTO` backup before v5 (precedent `clean.rs:431`), release note. |
| R-3 | Control-session rows appear in lists or are pruned (`list_used_sessions` and `prune_empty_sessions` key on messages/tasks, `store.rs:1298`; `session-store.md` Open) | Outcome history lost or shown as chat | `sessions.kind` column; every lister and cleaner filters it; test in PR-02. |
| R-4 | Retry loops burn money | Cost | Per-outcome budget derived from `Budget` and `--max-cost-usd`; cap is required (`run.rs:859`). |
| R-5 | Evidence forged by editing report files or reading `chain.key` | False VERIFIED | ADR-020: host-side runner on a clean checkout of the commit; `chain.key` on the deny list. |
| R-6 | Interactive asks inside attempts block forever across a restart | Stuck outcomes | Attempts run non-interactive; a denied ask becomes a Decision and a scoped re-run (ADR-019). |
| R-7 | Desktop flakiness: CI has known flaky tests (`docs/STATUS.md`) | Noise in new gates | New tests are pure or use the fake provider; no new real-browser or pty tests. |
| R-8 | Linux sandbox reads the whole filesystem; MCP/browser unsandboxed | Weaker isolation than the design assumes | Stated per platform; MCP restricted in attempts; hardening PRs tracked, not hidden (plan, PR-H). |
| R-9 | UX direction unclear (inspector, Outcomes placement) | Rework | Owner decisions D5, D6; CLI-first MVP. |
| R-10 | Merge into protected branches | Irreversible | Merge is not wired today; MVP produces a patch/branch only; push stays in `irreversible.rs`. |
| R-11 | Windows CI flakiness (SQLite lock on event append, `docs/STATUS.md`) | Flaky orchestrator tests | Reuse the retry in append; keep tests single-process. |
| R-12 | Surfaces updated at different times; a v5 database locks out older CLI or desktop (`SchemaTooNew`, `schema.rs:88`) | Users blocked | Explicit opt-in migration, backup, compatibility matrix (ADR-016) |
| R-13 | Standalone orchestrator process: lifecycle, endpoint auth, per-OS code, orphaned processes | New failure modes | Idle exit, recovery from durable state, sandbox-denied endpoint, explicit resume path (ADR-017) |
| R-14 | Two surfaces act on one outcome | Double approvals, stale answers | CAS commands, one owner (ADR-016, ADR-019) |
| R-15 | Logic drifts into a surface (a UI computes status or reinterprets a brief) | Three diverging behaviours | Shared view models, dependency test, renderer field-coverage test |

## 11. Unverified in this review

- Any behaviour on Linux or Windows runners (nothing was run; the existing CI is the only evidence and was not consulted).
- The cwd-inheritance bug the owner mentioned: no reproduction found (AC-03).
- Whether the desktop dialog layer ever expires an ask.
- Whether sandboxed commands on Linux need `$SYNT_HOME/sessions/` (relevant to widening the state deny list).
- That a unix socket or named pipe from the Windows desktop into the WSL distro works (the WSL link today is `wsl.exe` stdio).
- The owner's intended inspector layout and the Super Agent design (the design sources under `~/Vibe/synthesis-cli-ui` were not read).
- That `synthesis-peers` and `peer.rs` delegation cannot be reused as a scheduler: this is a design judgement from their interactive, asker-driven model (`peer.rs:732`), not a measured limit.
