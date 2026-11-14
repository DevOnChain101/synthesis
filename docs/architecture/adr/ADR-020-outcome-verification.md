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

Evidence rows carry a MAC since PR-09 (see the implementation notes): a writer who cannot read `chain.key` can no longer INSERT a passing `independent_check` row, alter a column, or add or drop a flag. Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals, evidence and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. Rows from before the MAC (or from an old build) have none and count for nothing. Residual open points: the candidate commit "the attempt that ended last" is unsound for parallel branches.

## Implementation notes (PR-09: evidence, independent runner, verdict)

- **Pure verdict** (`synthesis_core::outcome::verdict`, no I/O): contract + evidence facts + candidate commit give a verdict per criterion (`Passed`, `Failed`, `Unverified(reason)`, `Stale`) and `verified` when every mandatory criterion is `Passed`. Rules as section 4: the class must be `independent_check` (`human_review` for a manual criterion), the commit must be the candidate, the row from the newest attempt, no flag; a `failed` or `error` row of that class for the commit wins over any pass; a pass for another commit or a superseded attempt is `Stale`; a criterion that names checks is satisfied only by them. The matrix is the test `only_current_independent_evidence_passes_a_mandatory_criterion` (AC-08).
- **The store only counts authentic rows.** `authentic_evidence` takes a row only if (1) the control session holds a chained `evidence.recorded` event with the same content (evidence id, work item, check, trust class, result, commit, fence, hash, command hash, exit code, environment fingerprint, criteria and flags) AND (2) the row's `mac` verifies: an HMAC under a domain-separated subkey of `chain.key` over every column, flags included. Flags are part of the row from the insert (`NewEvidence.flags`) and immutable with it: there is no `flag_evidence`. Either check alone is not enough; a writer without the key can forge neither (tests `a_forged_row_and_event_without_the_mac_do_not_verify`, `rewritten_criteria_and_removed_flags_break_the_mac`). For a check the contract froze, the verdict also requires `command_hash` to be the sha256 of the contract's command (`evidence_must_carry_the_hash_of_the_frozen_command`); the verifier writes the hash of the contract's command, not the runner's. Residual: a same-user writer who also reads `chain.key` (denied to the sandbox) can forge everything.
- **Frozen checks are part of the contract**: `Contract.checks = [{id, command}]` (absent from the JSON when empty, so older contracts keep their hash), at most 32, one-line commands up to 512 characters; once a contract has checks a criterion may only name those. The evidence `command_hash` is the sha256 of the contract's command. The runner runs nothing else.
- **Independent runner** (`IndependentVerifier`, `SandboxRunner`, `gitsafe`): the verifier is only made for the APPROVED current contract (`for_outcome` checks the approval MAC and re-validates the contract). A FRESH checkout of the commit is made through a SCRATCH bare repository (empty config, no refs, no attributes files) whose object store is the user's repository's by `alternates`, with `GIT_NO_REPLACE_OBJECTS`, system and global config off, hooks and attributes files off, a timeout and a stop flag on every git command, and `.git` entries refused. The user's repository config is never read, so a filter driver (git-lfs or other) defined there does NOT run at verification time: running user-configured git filters at verification is DISABLED, an owner decision recorded in the plan (tests `a_filter_in_the_repos_config_does_not_run_at_checkout_and_replace_refs_are_ignored`, `an_entry_inside_a_git_directory_is_refused`). Each check runs `sh -c <command>` under the OS sandbox with the source read-only (no writable root), a private temp dir (`HOME`, `TMPDIR`, `CARGO_TARGET_DIR` point into it), no network, Synthesis's state paths denied, `ulimit -f` 64 MiB and an environment of `PATH`, `LANG`, `LC_ALL` only. The runner refuses (no evidence) when the backend cannot confine the filesystem and deny the network. The guarantee is the backend's (`Weak` on macOS Seatbelt and the bubblewrap filesystem): defence in depth for code the attempt could write, not a boundary against a determined program. A run with no exit status (timed out, stopped, could not start, killed, zero tests) writes NO row (an error is not evidence and the same commit can be verified again); the item fails with the reason. The process group is killed BEFORE the leader is reaped, never by pid once the kernel reaped it (state seam, test `a_check_reaped_behind_our_back_is_never_signalled_and_a_normal_exit_kills_its_group`); descendants that left the group (a `setsid` daemon on macOS) are out of reach by this and only the sandbox confines them. Scratch directories are removed on every exit path; dropping or cancelling the verifier stops running checks and git commands and joins their threads for at most 5 seconds. The output tail (64 KiB) is stored sanitised (no control, ANSI or direction characters) as an artifact of the control session; the fingerprint is OS, architecture, backend and guarantees.
- **Two kinds of verification.** An `agent` item: its commit's changes (`base..commit`, paths and modes from `git diff --raw -z`) against the envelope (`outside_envelope`: write allowed, an allowed pattern matches, no denied entry matches, case folded, odd spellings refused; denied patterns are read broadly, a superset of the permission engine's: `**/` is zero or more segments and a prefix glob, so `**/.env` matches a root-level `.env`), plus symlinks and submodule links (never inside an envelope), recorded as the `envelope-scope` check. A write outside `allowed_paths` fails the item and none of the violating attempt's code is run. Flags (`definitions_changed`, `tests_weakened`, `mode_changed`, `outside_envelope`) come from: definition files and build/test configuration (`synthesis-core` definitions plus `build.rs`, `conftest.py`, `.cargo/`, toolchain pins, `pytest.ini`...), the weakening scan of `synthesis-verification` over line diffs parsed from the hunks by their counts (a content line that looks like a file header cannot re-attribute anything; paths come from the raw listing; the user's `diff.noprefix` is overridden), removed `#[test]` and assertions in any `.rs` file (inline tests), and, where a runner's summary can be read (`cargo test`), fewer tests than the base commit has (the check is also run at the base commit) or zero tests (not evidence). A flag fails the item and escalates to a person; there is no exemption yet. A `verify` item: the contract's checks on the candidate commit.
- **Off-thread.** The checks run on a background thread; `IndependentVerifier::verify` returns `Pending` until it ends, `Verifier::busy` keeps the walk ticking instead of stopping as `WaitingVerification`, and the tick never waits for a check (test `verification_runs_off_thread_and_the_walk_keeps_ticking`). Dropping the verifier stops running checks.
- **What "independent" means, honestly.** The evidence says: the repository's OWN checks (the contract's frozen commands) exited 0 on a clean checkout of code the attempt could write, under a sandbox, run by a process the attempt cannot influence. It does not say the checks test the requirement. `build.rs`, `conftest.py`, `.cargo/config.toml`, a Makefile, test configuration and toolchain pins can make a check pass without testing anything: they are flagged where they can be seen, and a flag fails the item and goes to a person. Other residuals: the candidate commit is still "the newest finished attempt of the plan" (unsound for parallel branches until integration/merge exists); the fingerprint has no toolchain versions or definitions-baseline hash; a check that needs to write into its source tree fails (writes go to the temp dir); a test-count comparison needs a runner whose summary can be parsed; the Verifier is wired by the caller (`Scheduler::new(host, IndependentVerifier::for_outcome(..)?, cfg)`): the orchestrator process that does it for real outcomes arrives with PR-12.

### PR-09 final fixes

- **Heuristics, not controls.** The weakening scan, the removed-inline-test check and the test-count comparison read text an attempt can influence (a `harness = false` binary can print a `test result:` line); they raise flags for a person and never prove a test ran. Per-binary `Running ...` lines would not change that, so the count is documented as a heuristic rather than hardened.
- **Large files and dependencies.** A check cannot write a file larger than `synt.orchestrator.verify.max_file_mb` (default 512 MiB; `SandboxRunner::max_file_mb`): a larger one (a big debug test binary) stops with SIGXFSZ and fails closed. The limit bounds one file, not the total the check writes. The sandbox has no registry cache and no network, so only projects whose dependencies are vendored (or already in the toolchain) can be verified.
- **Jobs.** `agent` items are inspected on a job thread like `verify` items (up to hundreds of git commands, each with a timeout and the per-job stop flag, so the tick never waits and cancel reaches them); every job has its own scratch directory (a nonce in the name), so a stale job's cleanup can never delete a newer job's checkout. A retry runs and records only the checks that have no AUTHENTIC row for the commit, so a passing check is never recorded twice; `decide` and the "already done" shortcut read authentic rows (chained event and MAC), not raw ones. A non-UTF-8 changed path is an error (the commit is not verified), `git diff` runs with `-a` (a NUL byte cannot turn a diff into "Binary files differ"), and the scratch repository is created with an empty template (no `info/attributes`, no hooks).
- **MAC part list** now includes `kind` and `subject_tree` (DDL revision 1007; the feature is opt-in and unreleased, so rows MAC'd under revision 1006 simply do not count).
- **Residuals, stated.** Allow patterns are case-folded (lenient: `Src/x` is `src/x` on a case-insensitive file system); `kill_unreaped_tree` lists descendants before killing, so a process forked in between survives (the sandbox is the boundary).
