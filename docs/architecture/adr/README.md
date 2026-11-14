# Architecture decision records: outcome orchestration

These records are **Proposed** (2026-11-14). Nothing here is accepted until the owner says so; on acceptance a row is added to the registry in section 9 of `docs/Synthesis_Solution_Concept_and_Technical_Design.md` (not edited by this change) and the status line is updated. Numbers continue the registry (last used: ADR-015).

| ADR | Title | Status | Touches security | Depends on |
|---|---|---|---|---|
| [ADR-016](./ADR-016-outcome-persistence.md) | Outcome persistence in `synt.db` (revisions, commands, outbox, control session, explicit migration, compatibility matrix) | Proposed | no | none |
| [ADR-017](./ADR-017-outcome-orchestration.md) | Orchestration model on top of sessions, standalone orchestrator process; amends ADR-009 | Proposed | no | ADR-016 |
| [ADR-018](./ADR-018-execution-isolation.md) | Execution isolation per attempt (worktree, sandbox, cwd, locks, cleanup) | Proposed | yes | ADR-017 |
| [ADR-019](./ADR-019-outcome-policy.md) | Policy gateway, authority envelope, durable decisions, races, bypass gate | Proposed | yes (review required) | ADR-016, ADR-017 |
| [ADR-020](./ADR-020-outcome-verification.md) | Evidence model, trust classes, independent verification, anti-forgery | Proposed | yes (review required) | ADR-016, ADR-018 |
| [ADR-021](./ADR-021-outcome-ui-protocol.md) | One protocol (v4) and three thin surfaces: CLI, TUI, Super Agent | Proposed | no | ADR-016..020 |

Existing records that constrain these: ADR-005 (session-scoped workspace), ADR-006 (verification evidence), ADR-007 (policy gate before tools), ADR-009 (amended here), ADR-010 (Rust), ADR-011 (API keys only), ADR-012 (messaging), ADR-015 (WSL engine).

Scope (revised 2026-11-14): the orchestrator serves all three surfaces (CLI, TUI, Super Agent) through one core and one protocol; terminology follows `CLAUDE.md` (CLI = `synt` binary and terminal app, TUI = the desktop's terminal-style tab, Super Agent = the desktop's Agent tab). Settings: `synt.cli.*`, `synt.tui.*`, shared `synt.orchestrator.*`.

Format of every record: Status, Context (with file:line evidence), Options, Decision, Consequences, Revisit trigger. A statement that could not be verified in code is marked "unverified". Companion documents: [gap analysis](../outcome-orchestration-gap-analysis.md), [plan](../outcome-orchestration-plan.md), [threat model](../outcome-orchestration-threat-model.md), [MVP slice](../outcome-orchestration-mvp-slice.md), [acceptance tests](../../testing/outcome-orchestration-acceptance.md).
