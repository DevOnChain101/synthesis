# ADR-016: Outcome persistence in `synt.db`

Status: Proposed (2026-11-14). Not yet in the registry of section 9 of `docs/Synthesis_Solution_Concept_and_Technical_Design.md`; add a row there on acceptance.
Related: ADR-005, ADR-006, ADR-007, ADR-017, ADR-020. Gap analysis: [section 5](../outcome-orchestration-gap-analysis.md).

## Context

Outcome-level state (contract, plan, work items, attempts, decisions, evidence, outbox) must be authoritative and survive crashes. Today `synt.db` stores sessions only: 11 tables, 4 migrations (`synthesis-session/src/schema.rs:9-81`), WAL and foreign keys (`store.rs:243-247`), events appended in an IMMEDIATE transaction under a per-session advisory lock (`store.rs:1863-1900`, `:1597-1615`), HMAC-chained events (`schema.rs:68-80`, key `<home>/chain.key`, `trust.rs:250`), per-session artifacts with sha256 (`schema.rs:49-54`). The event envelope is `SCHEMA_VERSION = 1`; additive types keep it (`synthesis-events/src/lib.rs:13`, `docs/specs/events.md:135-138`). The design asked for revisions, an outbox, an inbox, an artifact store and migration/rollback.

## Options

A. **Extend `synt.db`**: migration v5 adds outcome tables and a `sessions.kind` column; outcome events live in a hidden per-outcome *control session* so they reuse append, chain, redaction, streaming and the single-writer lock.
B. **New `orchestrator.db`** with its own ladder and event table.
C. **New `synthesis-store` crate** owning all persistence, session crate becomes a client.

## Decision

Option A.

1. **One DB, one ladder.** Migration v5 (and later) in `schema.rs`; code in a new `synthesis-session/src/outcome.rs`. Domain types and transition tables are pure code in `synthesis-core` (precedent `session.rs`). The orchestrator crate depends on both.
2. **Control session.** Each outcome owns a `sessions` row with `kind = 'outcome'` (default `'chat'`). All outcome events are ordinary events of that session (`outcome.*`, `work_item.*`, `attempt.*`, `decision.*`, `evidence.*`, additive, `v` stays 1). Holding the control session's lock is what makes an orchestrator the single writer of the outcome. Every lister (`list_used_sessions`, `store.rs:1298`) and every cleaner (`prune_empty_sessions`, `clean`) filters `kind = 'chat'` unless asked.
3. **Revisions.** Mutable aggregate rows (`outcomes`, `work_items`, `attempts`, `decisions`) carry `revision INTEGER NOT NULL`. A transition is one IMMEDIATE transaction: `UPDATE ... SET state=?, revision=revision+1 WHERE id=? AND revision=?` (0 rows = conflict, caller reloads), state-table check in code, event insert (same transaction, through the existing append), outbox insert. The event payload carries `aggregate`, `aggregate_id`, `revision`.
4. **Idempotency.** Natural unique keys, not a command table: `attempts(work_item_id, ordinal)`, `decisions(outcome_id, dedupe_key, subject_revision)`, `outbox(idempotency_key)`, `evidence(attempt_id, check_id, subject_commit)`.
5. **Outbox.** `outbox(id, outcome_id, idempotency_key UNIQUE, kind, payload, state, tries, next_at, receipt)`. MVP effect kinds are local only: `worktree.create`, `worktree.cleanup`, `verifier.run`. The dispatcher runs after commit, records a receipt, and on restart replays `pending` and reconciles `dispatching` by asking the effect's own probe (does the worktree exist, is the verifier artifact present). No claim of exactly-once; at-least-once with idempotent receivers.
6. **Inbox.** Not built in MVP (no external events). The `(source, external_id)` unique table is specified for the GitHub adapter phase and added then.
7. **Artifacts.** Reuse `artifacts` and `sessions/<id>/artifacts/` (write temp, fsync, rename, insert; `session-store.md` B.3). Evidence stores `(session_id, artifact_id, sha256)`. A global content-addressed store is deferred (revisit trigger below). Artifact bytes are raw, not redacted (`docs/SECURITY.md` "Artifacts are raw"); evidence shown in UI is redacted again on read.
8. **Ids.** UUIDv4 TEXT like every other id (`docs/specs/workspace-context.md` section 1); ordering by `created_at` and per-session `seq`.
9. **Migration and rollback.** Additive only (`CREATE TABLE`, `ALTER TABLE ADD COLUMN` with defaults). Before applying v5 the opener copies the DB with the existing consistent-backup path (`clean.rs:431` `backup_db`, extended to a `pre-v5` directory name). A binary older than v5 refuses the DB (`SchemaTooNew`, `schema.rs:88`); rollback is: install the old binary and restore the backup (outcome data created after the upgrade is lost) or keep both. This is documented in the release notes. A downgrade tool is not planned.
10. **Tests.** A migration test from a v4 fixture DB, a CAS-conflict test, an outbox replay test with a crash between commit and dispatch (reuse `synthesis-session/tests/crash.rs` helper), a chain-verify test over outcome events (reuse `tests/governance.rs`).

## Consequences

- Positive: no second database, no second chain, `synt sessions verify` covers outcome events for free, recovery scan (`recover.rs:163`) already finds interrupted control sessions.
- Negative: `synthesis-session` grows; a bad migration is user-visible; the `sessions.status` machine (14 states) is borrowed for the control session and must not be stretched to model outcome states (outcome state lives in `outcomes.state`).
- Neutral: events of different sessions are not ordered globally; an outcome view merges by `(ts, session, seq)` and never assumes a total order across sessions.

## Revisit trigger

- Writer contention: more than one orchestrator or UI writer per outcome, or measured `SQLITE_BUSY` retries above the existing 15 s budget (`store.rs` `update_status`).
- Remote workers: artifact transfer needs a content-addressed store.
- The v5 table count or row volume makes `synt.db` backups slow (> a few seconds on a developer machine).
- An external consumer needs a stable cross-session event order.
