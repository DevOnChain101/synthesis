# ADR-021: Outcomes and Decisions UI surface; engine protocol v4

Status: Proposed (2026-11-14). The UI placement needs an owner decision (plan D5, D6).
Related: ADR-015 (WSL engine over stdio), ADR-016, ADR-017, ADR-019. Specs: `docs/specs/desktop.md`, `docs/specs/events.md`.

## Context

Owner direction (design section 22): keep Workspace with Sessions nested, chat primary, composer controls under the chat, and exactly one right-side tabbed popup for inspection; elevate Outcomes and Decisions above low-level sessions; do not stack competing popups.

What exists: `View { Tui, SuperAgent, Settings }` with Super Agent shown as "Coming soon" (`apps/desktop/src/ui.rs:29-33`; the owner said the Agent entry will be a different desktop design later, HANDOFF 1b). The TUI view has left panels (Workspace, Running, Session, Tools & Skills, Recent Activity, Prompt), centre tabs `ChatTab { Chat, Diff, Terminal, Browser, Files }` (`apps/desktop/src/tui_view/mod.rs:89-96`), right panels (Topology, Agent Status, Context, Usage) that collapse to a drawer, and a layout rule that chat is always the majority (`docs/specs/desktop.md`). View models are UI-agnostic in `synthesis-view` (`ViewState::apply(&Envelope)`). The CLI app renders the same models (`synthesis-cli-ui`). Engine protocol 3: hello with exact version and protocol match, requests `{"id","method","params"}`, events as ordinary envelopes, asks as `ask.approval`/`ask.input` (`synthesis-app/src/proto.rs:19,71-231,308`).

There is **no** single right-side tabbed inspector today; the right side is a panel stack. Whether the owner's design files define one is unverified.

## Options

A. **Inside the TUI view.** The Workspace panel gets an "Outcomes" group above "Sessions" (same nested list); selecting an outcome shows its control-session chat (milestones only) in the existing chat area; a Decisions list is a panel/tab; the inspector is the right drawer region re-organised as one tabbed container.
B. A new top-level `View::Outcomes`.
C. Use the Super Agent tab.

## Decision

Option A for MVP, with the surface kept behind view models so B or C is a re-skin, not a re-write. Reasons: it keeps the owner's navigation and chat-first layout, adds no new top-level mode, and the Super Agent design is explicitly undecided. Default for owner decision D5.

1. **CLI first.** MVP ships `synt outcome create|approve|start|status|decisions|resolve|cancel|inspect` (headless JSONL and a plain-text summary). The desktop surface follows in a later PR and consumes the same protocol, so the engine is testable without GPUI (`design_tests` and CI desktop job are heavy and flaky-prone, `docs/STATUS.md`).
2. **Outcome chat is not worker chat.** The control session's conversation holds user intent, contract summaries, milestone events (`outcome.*`, `work_item.succeeded`, `decision.created`, `outcome.verified`) and material notices. Worker messages stay in the attempt's own session, reachable through the inspector (design section 22 "important UX distinction"; AC-11).
3. **One inspector.** A single container with tabs: `Overview` (plan/DAG and criteria), `Workers` (attempts, session link, sandbox backend and guarantee, cost), `Activity` (tool timeline, policy decisions), `Diff` (commit/diff of the attempt), `Evidence` (evidence rows, trust class, subject commit, freshness, policy and audit). Existing Diff/Terminal/Browser/Files tabs keep their place for session view; the inspector for an outcome reuses their panes through `synthesis-view::agent::AgentPanes` instead of copying them. A second popup is never opened: dialogs (trust, rewind, clean) stay as they are and are not inspectors.
4. **Decision cards** (design sections 23-24): first level is one question, why a human, delta from the approved contract; second level alternatives, tradeoffs, evidence, unknowns, consequence of deferring; third level diff/logs in the inspector. Options are the alternatives plus Reject, Defer, Request evidence, Edit scope, Open inspector. No preselected option, no accent colour on any option, no bulk approve above R1, status shown with text labels as well as colour. A stale decision is shown struck as "Invalidated: <reason>" and cannot be answered.
5. **Protocol v4** (exact match like v3; `PROTOCOL = 4`, desktop and engine ship together, ADR-015). Additive methods:
   - `outcome.create {workspace, goal}`, `outcome.contract.get`, `outcome.contract.approve {version, hash}`, `outcome.start`, `outcome.pause|resume|cancel`, `outcome.get`, `outcome.list {workspace}`;
   - `plan.get {outcome, version}`; `work_item.get`; `attempt.inspect {id}` (links the attempt session for the existing `session.preview`);
   - `decision.list {outcome}`, `decision.get`, `decision.resolve {id, revision, choice, note}`; `evidence.get {id}`;
   - `events.subscribe {outcome, after_seq}` returning the control session's envelopes after a cursor (resume after reconnect; pagination and redaction as `session.preview`).
   - Outcome events are ordinary envelopes of the control session, so existing consumers ignore them (events spec section 7); `decision.created` is also pushed as a `{"msg":"decision","id"}` line so a client need not poll.
   - Decisions are answered by a person only: `decision.resolve` is accepted only from the host's UI/CLI transport, never from an attempt; the hostile-engine rule of v3 (a `restarted` the client did not ask for) extends to unsolicited `decision.resolve` results.
6. **State is a projection.** Views never infer authorization from cached state: `decision.resolve` re-checks the revision and bindings server-side (ADR-019); a stale client gets `error: stale` and refetches.
7. **No new listener.** Same transports as today (in-process, stdio). The design's local HTTP option is rejected (ADR-016 context, gap analysis section 8).
8. **Notifications.** Reuse the `notify` hook for `decision.created` and `outcome.verified` (`docs/UNATTENDED.md` Notifications; the desktop notification kind is still a stub there).

## Consequences

- Positive: minimal change to navigation; protocol additive on a tested transport; testable without a window.
- Negative: protocol bump forces lock-step desktop and engine (already the rule); the inspector re-organisation touches `tui_view` layout tests (`design_tests` bound to browser-measured rects) and needs a design pass by the owner; Windows desktop reaches outcomes only through the WSL engine.
- Unverified: pixel design, keyboard model, and whether the right drawer can host tabs without breaking the chat >= 55% rule.

## Revisit trigger

- The owner delivers the Super Agent design (then option C may replace A).
- More than ~5 open outcomes per workspace make the nested list unusable.
- A non-desktop client (web, mobile) needs the protocol (then a versioned, negotiated API replaces the exact match).
