# ADR-020: Evidence model, trust classes and independent verification

Status: Proposed (2026-11-14). Touches anti-forgery and the sandbox deny list: security review required before acceptance.
Related: ADR-006 (verification evidence is first-class), ADR-016, ADR-017, ADR-019. Spec: `docs/specs/verification.md`.

## Context

The design requires: every mandatory criterion maps to current, trustworthy, provenance-linked evidence; model assessments and worker claims never count as proof; evidence names the exact code revision and environment; an independent checker outside the worker's control produces high-trust evidence; checks re-run after integration.

What exists (all in `synthesis-verification`): `CheckResult` with `declared_by`, `exit_code`, `criteria_ids`, `artifacts`, parsed test counts (`types.rs:143-165`); `Declarer { User, Harness, Model }` and the rule that model-declared checks never satisfy a pass (`gate.rs:134,155,412`; `plan.rs:192-195`); per-criterion `status` and `evidence` ids set by `aggregate` (`gate.rs:78-95`); `CompletionGate` with checks frozen at task start, a workspace baseline and anti-weakening scans (`gate.rs:230,276`, `weaken.rs:86,115`); the full report stored and its sha256 in `verification.completed` (`events/types.rs:492-495`); exit code 3 for `unverified` (`run.rs:3071-3079`). Weaknesses recorded by the owner (HANDOFF 7p, summarised in `docs/SECURITY.md:23`): weakening in non-test source edited only by shell is detected by size and mtime; honest work with assertion edits ends `unverified`. The in-session gate runs the checks in the **same worktree the worker can modify**, so it is observed evidence, not independent evidence.

## Options

A. Keep the in-attempt gate as one class of evidence; add a host-side **independent runner** that re-runs the frozen checks on a clean checkout of the attempt's commit, and a pure verdict function over evidence rows.
B. Trust the in-attempt gate only.
C. A separate verifier service.

## Decision

Option A.

### 1. Evidence record

`evidence` row (ADR-016): `id, outcome_id, work_item_id, attempt_id, kind, trust_class, subject_commit, subject_tree, check_id, command_hash, criteria_ids (JSON), result, exit_code, producer ('gate'|'independent_runner'|'human'|'model'|'worker'), env_fingerprint, artifact_session_id, artifact_id, sha256, captured_at, fence`. `result`, `exit_code`, `criteria_ids` come from `CheckResult`; nothing is re-parsed from worker-written files.

### 2. Trust classes (ordered, lowest first)

| Class | Source | May satisfy a mandatory criterion |
|---|---|---|
| `claimed_by_worker` | worker text, report files in the worktree | never |
| `model_assessment` | reviewer or judge model output, `Declarer::Model` checks | never (informative; shown labelled) |
| `observed_in_attempt` | the in-session `CompletionGate` report (`Declarer` User/Harness), same worktree | no, on its own |
| `independent_check` | checks run by the host-side runner on a clean checkout of `subject_commit`, sandboxed, from the **frozen** check list | yes |
| `human_review` | a Decision resolution by a person naming the evidence it saw | yes, only for criteria whose contract says `manual` |

### 3. Independent runner

After an attempt ends and the host commits its tree (ADR-018 item 3), the orchestrator (outbox `verifier.run`, idempotent on `(attempt_id, check_id, subject_commit)`):

1. creates a detached, read-only checkout of `subject_commit` in a fresh directory under `$SYNT_HOME/verify/<attempt_id>/` (git worktree add `--detach`; no agent-writable `.git` pointer is trusted, same pinning as `GitInfo`);
2. runs each check of the **frozen plan** (checks frozen at outcome approval from the contract and `detect_checks`, not re-read from the attempt's tree; `gate.rs:230`) through the existing runner and sandbox policy (`enforce`, network deny, caps), as a separate runtime session with a scrubbed environment;
3. records the exit code, parsed counts, duration, stdout/stderr artifacts (raw, like all artifacts) and the environment fingerprint (OS, arch, sandbox backend and guarantee, toolchain `--version` lines of the check's tools, definitions-baseline hash from `synthesis_core::definitions`);
4. refuses to produce `independent_check` evidence when the sandbox is not `enforce` (evidence class degrades to `observed_in_attempt` with `blocked_reason`).

Definition files (`package.json`, `Makefile`, test configs, scripts: `synthesis_core::definitions`) that differ between the base commit and `subject_commit` make the runner mark the evidence `definitions_changed` and the verdict `unverified` unless the contract lists them as in scope. This reuses the gate's rule (`docs/SECURITY.md:23`) at outcome level.

### 4. Criterion mapping and verdict

Contract criteria extend `Criterion` (`types.rs:70`) with `mandatory: bool`, `required_trust` (default `independent_check`), and `check_ids`. A plan is invalid if a mandatory criterion has no work item or check that can cover it (design section 12). The verdict is a pure function:

`verdict(criteria, evidence, candidate) -> { per-criterion: Passed | Failed | Unverified(reason) | Stale, outcome: Verified | NotVerified }`

A criterion is `Passed` only if some evidence has trust class >= `required_trust`, `result = passed`, `subject_commit = candidate.commit` (freshness), a fence equal to the attempt's final fence, and no `definitions_changed` flag. Any `Failed` evidence for the same commit wins over a `Passed` of the same class (worst-of, as `aggregate` does at `gate.rs:78-95`). Outcome is `Verified` only if every mandatory criterion is `Passed`; `Unverified` or `Stale` criteria keep it `NotVerified` and are listed as known gaps. Skipped tests, flaky checks and unverifiable requirements are recorded explicitly as limitations (existing `SKIPPED_TESTS` mechanism, `gate.rs:199-215`).

### 5. Anti-forgery

- Evidence rows are written only by the orchestrator process (control-session lock holder). The attempt's sandbox denies `synt.db*` (`synthesis-sandbox/src/lib.rs:484-487`), and file tools deny the state paths (`synthesis-app/tests/state_files_protected.rs`).
- **`chain.key` is added to `synthesis_state_paths`** (`lib.rs:480-497`). Today it is absent: on Linux bubblewrap, `/` is bound read-only so a sandboxed command can read `<home>/chain.key`; macOS Seatbelt's read allow-list does not include `$SYNT_HOME`. Read in code, **not executed**; the red-team case `evid-chain-key-read` (threat model) proves or refutes it on CI before the line is merged, and the same PR corrects `docs/UNATTENDED.md:49`. Whether other state (`sessions/`, `config.json`, `state.json`, `worktrees/`) must also be denied is **unverified** (sandboxed commands may need `worktrees/` itself); decide per path with a test.
- Test-reporter forgery: the independent runner derives results from the exit code and the runner's own parse of output it produced; a report file in the worktree is an artifact of class `claimed_by_worker`.
- Evidence bound to an old `fence` or an old commit is rejected at insert.
- The verification report hash chain is extended to evidence: `evidence.sha256` plus the `evidence.recorded` event (chained, ADR-016).

### 6. Re-verification

Any change to the candidate commit (new attempt, integration, conflict resolution) makes earlier evidence `Stale` (not deleted) and enqueues `verifier.run` again. Merge itself is out of MVP.

## Consequences

- Positive: reuses the runner, parser and gates; the only genuinely new code is the checkout, the table, and a pure verdict function with exhaustive tests.
- Negative: checks run twice (in-attempt and independent), roughly doubling verification time and token-free compute; environment drift between attempt and runner can fail honest work (reported as a limitation, not hidden); the runner executes agent-written code (tests) on the host under the sandbox, with the same weak guarantees as the attempt.
- Residual: tests that pass on a clean checkout can still not exercise the criterion (semantic coverage is a human or review question; model review stays `model_assessment`).

## Revisit trigger

- A criterion kind that cannot be a command (UI, performance): add `observed_runtime` evidence with a measured producer.
- The runner needs a stronger boundary than the host sandbox (container or remote).
- Evidence volume makes per-commit re-runs too slow: incremental/affected-test selection.
- A second independent signer is required (signed evidence attestation).

## Integrity of evidence (PR-02 review)

Evidence rows carry no MAC today. Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. A writer without the key can INSERT a passing `independent_check` row with plain SQL, so the MVP verdict in the store is **not** to be trusted before PR-09: it must compute the verdict only from signed `evidence.recorded` events and verify that each row matches its event. Open points recorded for PR-09: nothing produces `definitions_changed` yet; evidence `command_hash` is not bound to a frozen command (criterion `check_ids` are); "the attempt that ended last" as candidate commit is unsound for parallel branches.

## Implementation notes (PR-09: evidence, independent runner, verdict)

- **Pure verdict** (`synthesis_core::outcome::verdict`, no I/O): contract + evidence facts + candidate commit give a verdict per criterion (`Passed`, `Failed`, `Unverified(reason)`, `Stale`) and `verified` when every mandatory criterion is `Passed`. Rules as section 4: the class must be `independent_check` (`human_review` for a manual criterion), the commit must be the candidate, the row from the newest attempt, no flag; a `failed` or `error` row of that class for the commit wins over any pass; a pass for another commit or a superseded attempt is `Stale`; a criterion that names checks is satisfied only by them. The matrix is the test `only_current_independent_evidence_passes_a_mandatory_criterion` (AC-08).
- **The store only counts event-backed rows.** `evidence_facts` takes a row only if the control session holds a chained `evidence.recorded` event with the same evidence id, work item, check, trust class, result, commit, fence and hash (also for the item `succeeded` guard). A row written by plain SQL has no event and counts for nothing (test `a_passing_row_without_its_chained_event_does_not_verify`). Flags (`definitions_changed`, `tests_weakened`, `outside_envelope`) are added with `flag_evidence` and never removed; any flag disqualifies the row. Residual: a same-user writer who can also forge the chained event and its HMAC (needs `chain.key`, denied to the sandbox) is outside this protection.
- **Frozen checks are part of the contract**: `Contract.checks = [{id, command}]` (absent from the JSON when empty, so older contracts keep their hash), at most 32, one-line commands up to 512 characters; once a contract has checks a criterion may only name those. The evidence `command_hash` is the sha256 of the contract's command. The runner runs nothing else.
- **Independent runner** (`IndependentVerifier`, `SandboxRunner`): a FRESH checkout of the commit under `$SYNT_HOME/verify/<item>-<commit>/src` made from the object database (`read-tree` into a temporary index and `checkout-index`: no `.git`, no hooks, no `export-ignore`, no repository config), never the attempt's worktree. Each check runs `sh -c <command>` under the OS sandbox with the source read-only (no writable root), a private temp dir (`HOME`, `TMPDIR`, `CARGO_TARGET_DIR` point into it), no network, Synthesis's state paths denied (`chain.key`, `synt.db`...) and an environment of `PATH`, `LANG`, `LC_ALL` only. The runner refuses (no evidence at all) when the backend cannot confine the filesystem and deny the network (`NullSandbox`, native Windows). A check that fails to start, times out (process group killed), is stopped, or dies by a signal is an `error` row, never a pass. The output tail (64 KiB) is stored as an artifact of the control session and its hash is on the evidence row; the environment fingerprint is OS, architecture, backend and guarantees.
- **Two kinds of verification.** An `agent` item: its commit's changed files (`base..commit`) against the envelope (`outside_envelope`: write allowed, an allowed pattern matches, no denied entry matches; case folded; odd spellings refused), recorded as the `envelope-scope` check. A write outside `allowed_paths` fails the item (it retries) and none of the violating attempt's code is run: this closes the shell-redirect/built-program residual of ADR-019. Changes to definition files or weakened tests (`synthesis-verification` `weakened_tests`, `removed_prerequisite_guards`, `is_definition`) flag the evidence and fail the item (a change to what the checks are made of needs a person; there is no exemption yet). A `verify` item: the contract's checks on the candidate commit.
- **Off-thread.** The checks run on a background thread; `IndependentVerifier::verify` returns `Pending` until it ends, `Verifier::busy` keeps the walk ticking instead of stopping as `WaitingVerification`, and the tick never waits for a check (test `verification_runs_off_thread_and_the_walk_keeps_ticking`). Dropping the verifier stops running checks.
- **Residuals, stated.** The candidate commit is still "the newest finished attempt of the plan" (unsound for parallel branches until integration/merge exists); the fingerprint has no toolchain versions or definitions-baseline hash; a check that needs to write into its source tree fails (writes go to the temp dir); `.gitattributes` filters named by the user's own repository config would run at checkout (the agent cannot write that config); the Verifier is wired by the caller (`Scheduler::new(host, IndependentVerifier::new(..), cfg)`): the orchestrator process that does it for real outcomes arrives with PR-12. Tests: `ac08_an_outcome_is_verified_only_through_independent_evidence`, `a_write_outside_the_envelope_fails_verification_even_if_the_check_passes`, `changed_definitions_are_flagged_and_the_outcome_is_not_verified`, `a_failing_or_stuck_check_and_a_forged_report_never_verify`, `without_an_enforcing_sandbox_no_independent_evidence_is_produced`, `a_check_runs_sandboxed_with_a_read_only_source_and_a_private_tmp`, `a_fresh_checkout_is_the_commits_tree_and_nothing_else`.
