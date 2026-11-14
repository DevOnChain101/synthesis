# ADR-021: One protocol (v4) and three thin surfaces: CLI, TUI, Super Agent

Status: Proposed (2026-11-14, revised: the orchestrator serves all three surfaces). Placement of Outcomes per surface needs owner decisions (plan D5, D6).
Related: ADR-015 (WSL engine over stdio), ADR-016, ADR-017, ADR-019. Specs: `docs/specs/desktop.md`, `docs/specs/events.md`, `docs/cli-palette.md`, `docs/CLI-APP.md`.

Terminology (owner, CLAUDE.md): **CLI** = the `synt` binary, whose terminal app opens with no subcommand (there is no `synt tui`); **TUI** = the terminal-style tab of the desktop app (`apps/desktop`, `tui_view/`); **Super Agent** = the desktop's "Agent" entry, "Coming soon" today. Settings: the CLI reads `synt.cli.*`, the TUI `synt.tui.*`; the Super Agent has no section yet (proposal below).

## Context

Owner direction (design section 22): keep Workspace with Sessions nested, chat primary, composer controls under the chat, **one** right-side tabbed popup for inspection, Outcomes and Decisions above low-level sessions.

Facts: `View { Tui, SuperAgent, Settings }` (`apps/desktop/src/ui.rs:29-33`); the owner said the Agent will be a different desktop design later (HANDOFF 1b), so its layout is unknown. The TUI has left panels, centre tabs `ChatTab { Chat, Diff, Terminal, Browser, Files }` (`tui_view/mod.rs:89-96`), a right panel stack that collapses to a drawer; chat is always the majority (`docs/specs/desktop.md`). View models are UI-agnostic in `synthesis-view` (`ViewState::apply(&Envelope)`, shared by the CLI and the desktop). **Today the CLI talks to its engine in process, not through the protocol**; the desktop uses the protocol only for WSL (`apps/desktop/src/remote.rs:20`); protocol 3 has an exact hello match, requests `{"id","method","params"}`, events as envelopes, asks as `ask.approval`/`ask.input` (`synthesis-app/src/proto.rs:19,71-231,308`). `notify.rs` sends ids and codes only, for governed runs only (`synthesis-app/src/notify.rs:5-9,158`), and the desktop has no notifier (`docs/UNATTENDED.md` Notifications).

## Options

A. One protocol, three thin clients, a shared view-model layer (decision below).
B. Per-surface integration (CLI calls library functions, desktop uses the protocol).
C. A new HTTP/JSON API for the Super Agent.

## Decision

Option A. B duplicates logic and hides the real boundary from tests; C adds a listener the repo has avoided (ADR-012, HARD-12).

### 1. One protocol

- **Protocol v4**: `PROTOCOL = 4`, exact match like v3 (no negotiation; surfaces and orchestrator of one install ship together; ADR-015). All surfaces use it, **the CLI included, even when the server is in the same process**: a client library `synthesis_app::outcome_client` offers the same typed calls over three transports: `InProc` (tests and headless CI only, never a production path), `Stdio` (WSL engine link), `LocalSocket` (the standalone orchestrator, ADR-017). There is no per-surface path: no surface imports `synthesis-orchestrator`.
- **Methods** (additive to v3): `orchestrator.status`, `orchestrator.enable` (explicit migration, ADR-016 item 9); `outcome.create {workspace, goal, contract_draft}`, `outcome.contract.get`, `outcome.contract.approve {version, hash}`, `outcome.start|pause|resume|cancel|ack`, `outcome.get`, `outcome.list {workspace}` (each row carries `needs_you`, below); `plan.get`; `work_item.get`; `attempt.inspect {id}`; `decision.list|get|resolve {id, expected_revision, choice, note}`; `evidence.get`; `command.status {idempotency_key}`; `events.subscribe {outcome, after_event_id}` (cursor, reconnect, pagination, redaction as `session.preview`). Every mutating method is a revision-checked command (ADR-016 item 11) and returns `applied | stale | already_resolved | denied | queued`.
- Outcome events are ordinary envelopes of the control session (additive types, `v` stays 1), so v3 consumers ignore them (`docs/specs/events.md` section 7). A `{"msg":"decision","id"}` line is pushed on `decision.created` so a connected client need not poll.
- Decisions are answered by a person only; `decision.resolve` is accepted only from a surface's transport. The hostile-engine rule of v3 (an unsolicited `restarted`) extends to unsolicited command results.
- No TCP and no HTTP: stdio, in-process (tests) and the same-user local socket or named pipe with the mutual HMAC handshake of peers (`synthesis-peers/src/net.rs`).

### 2. Thin surfaces (no logic in UI)

- **Shared view-model layer.** `synthesis-view` gains `outcomes` models (outcome list with `needs_you`, plan/DAG rows, criteria table, attempts, decisions, evidence) built from protocol replies and `outcome.*` envelopes. The **inspector tab set** is defined once there: `Overview`, `Workers`, `Activity`, `Diff`, `Evidence`. Surfaces render; they never compute verdicts, states, bindings or briefs.
- **Decision brief is structured data** (ADR-019): fields `why_human`, `delta`, `alternatives[{id, name, tradeoffs}]`, `evidence_refs`, `unknowns`, `defer_consequence`, `expires_at`, `default_action = HOLD`, `binding`. No field marks a recommended or selected option. Surfaces may lay it out differently but must show every field present, must not reorder alternatives by preference, must not preselect, and must show the stale/invalidated state returned by the server. A shared renderer test ("every non-empty field appears") runs for each surface.

### 3. Per surface

| Surface | Outcomes entry | Decisions | Inspector | Status |
|---|---|---|---|---|
| **CLI** (first) | `synt outcome create|approve|start|status|list|decisions|resolve|pause|resume|cancel|inspect|enable`; text and `--json`; the terminal app gets `/outcomes` (picker) and a start banner "N outcomes need you" later | `synt outcome decisions ID` prints the brief as text; `resolve` uses a numbered picker (no default), `--choice` for scripts | `synt outcome inspect ID --tab overview|workers|activity|diff|evidence` | MVP |
| **TUI** | An "Outcomes" group above "Sessions" in the Workspace panel (nested list kept); selecting one shows the control-session chat (milestones only) in the chat area; badge `needs you` | Decision picker in the same style as `/model`, `/resume` pickers (`docs/specs/desktop.md` "Command pickers") and a Decisions row group; no accent on any option | One tabbed container in the right drawer region replacing the stack **only while an outcome is selected** (tab set above); existing Diff, Terminal, Browser, Files tabs stay for session view | After the CLI |
| **Super Agent** | Own design, not yet made (owner). Specified logically as: outcome list and goal conversation as the primary object, a Decision/Task inbox switchable in the main area, composer with attach/model/effort/permission mode below, **one** right-side tabbed inspector (the same tab set) | Decision cards from the structured brief (why now, delta, alternatives, evidence, unknowns, consequence of defer; options: alternatives, Reject, Defer, Request evidence, Edit scope, Open inspector) | The same inspector | Specified only; implemented after the owner's design |

CLI and TUI settings only affect rendering: `synt.cli.outcomes.*`, `synt.tui.outcomes.*` (panel visible, density). Everything that changes behaviour, budgets or security is under `synt.orchestrator.*` and read from user or managed settings only (plan, "Settings split"); `synt doctor` warns about orchestrator keys found under a surface section, as it already does for CLI-only keys left under `synt.tui` (`CLAUDE.md`). A `synt.superagent.*` section is proposed for when that surface exists (plan D17).

### 4. "Needs you" signal and notifications when no surface is open

- **Durable signal.** `outcome.list` and `outcome.get` return `needs_you: {decisions_pending, critical, blocked, verified_awaiting_ack}` computed from rows (Pending decisions, outcome `Blocked`, `Verified` not yet acknowledged). Any surface shows it on open; the CLI prints it at the start of `synt outcome status` and, later, in the terminal app banner. It survives restarts because it is derived from the store.
- **Push when nothing is open.** The orchestrator process sends the existing notify hook (`synt.notify`, user or managed settings, sandboxed command, ids and codes only, never text by default; `docs/UNATTENDED.md` Notifications) with new event kinds `decision.created`, `outcome.blocked`, `outcome.verified`; payload `{event, outcome_id, decision_id?, kind, reason_code, ts}`. Deduped per `(decision_id, revision)`; the existing caps apply (4 concurrent, 50 per process, `task.ended` reserve). `notify.rs` today starts only for governed runs and the interactive CLI never notifies; the orchestrator process is a governed run, so it qualifies.
- The desktop's own system notification stays a stub (`kind: "desktop"` is skipped today); the orchestrator does not depend on it. When a desktop notifier exists it is a surface feature fed by the same events.
- Payloads never carry brief text, so a notification leaks nothing a lock screen should not show.

### 5. Two surfaces on one outcome

One owner (the orchestrator process), many viewers, actions as revision-checked commands (ADR-016, ADR-019 section 5). A surface that loses a race shows the server's `already_resolved` or `stale` and refetches; it never retries with a guessed revision. Subscriptions are scoped to an outcome, resumable from `after_event_id`, and every surface treats its cache as a projection.

## Consequences

- Positive: lifecycle provable headless through the protocol (acceptance doc); surfaces are small; one change to a rule is one change.
- Negative: the CLI gains a client indirection even in process; the exact version match forces lock-step updates of the CLI, the orchestrator process and the desktop (ADR-016 matrix); TUI layout changes touch `design_tests` (browser-measured rects); Windows desktop reaches outcomes only through WSL.
- Unverified: pixel design, keyboard model, whether the right drawer can host tabs without breaking the chat >= 55% rule, the Super Agent design.

## Revisit trigger

- The owner delivers the Super Agent design (fill in section 3, row 3).
- More than about 5 open outcomes per workspace make the nested list unusable.
- A non-desktop client needs the protocol (then versioned negotiation replaces the exact match).
- Notification needs exceed ids and codes (then a reviewed redaction design).
