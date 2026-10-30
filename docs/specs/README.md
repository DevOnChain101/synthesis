# Synthesis R1 contract specs

Phase R1 deliverable (design doc §10 Phase 0, §14). Base: `../Synthesis_Solution_Concept_and_Technical_Design.md` v1.1 and the frozen Node reference `~/Vibe/nemo-cli` (read-only). Tags in every spec: `[from-node file:line]` = derived from Node, `[new]` = new design. Line numbers refer to the Node tree as read on 2026-10-26; golden fixtures pin the exact commit.

| Spec | Covers | Status |
|---|---|---|
| [events.md](events.md) | JSONL envelope, event taxonomy, redaction, artifact refs, versioning, headless exit codes | reviewed 0.1 |
| [workspace-context.md](workspace-context.md) | `WorkspaceContext`, `AgentSession`, `ToolContext`, state machine, path invariants | reviewed 0.1 |
| [tools.md](tools.md) | registry contract, side-effect classes, pipeline, 20 ported schemas, new tool drafts | reviewed 0.1 |
| [session-store.md](session-store.md) | Node transcript format, SQLite schema, two-way compatibility plan | reviewed 0.1 |
| [config.md](config.md) | files, layering, env vars, `$schema` rules, credential policy (user-managed API keys, ADR-011) | reviewed 0.1 |
| [verification.md](verification.md) | `VerificationReport`, status rules, completion gate, self-correction, budgets | reviewed 0.1 |
| [eval-and-golden.md](eval-and-golden.md) | eval harness, task manifest, 6+2 scenarios, scoring, golden capture plan | reviewed 0.1 |
| [tui-design.md](tui-design.md) | TUI redesign: theme tokens, layout planner and panel registry, `/config`, logo; phases P1–P4 | accepted (owner decisions D1–D11, 2026-10-30); P1 implemented |

Status values: draft → reviewed (product owner + one engineer) → frozen (changes need a `v` bump or an ADR).

Review record: all seven specs were reviewed and approved by the product owner on 2026-10-28 (chat approval; no second engineer reviewer). They stay `reviewed`, not `frozen`. The product owner is the sole maintainer, so this spec review is final (there is no second reviewer to wait for). Golden cases are a separate review: the owner approved all golden cases on 2026-10-29 as regression snapshots (the Node prototype is not authoritative); every golden `meta.json` carries that `reviewed_by`.

## Decisions taken in these drafts (challenge them here)
1. Ported tools keep their Node/Claude names (`Read`, `Bash`, …). Doc §5.6 ids map onto them; new tools use `_` instead of `.` on the wire (dots are not valid in provider tool names). tools.md §1.
2. `Idle` and `Recovering` are added to `SessionStatus`; terminal statuses are per task, and the session ends with `session.completed|failed|cancelled`. workspace-context.md §3.
3. `--cwd DIR` is optional: without it the CLI boundary (`launch_dir()` in `apps/cli/src/main.rs`) reads the launch directory once; the libraries never read the process cwd (clippy bars `std::env::current_dir`) and always take an explicit cwd. Owner fix 2026-10-31 (plain `synth` used to fail with `pass --cwd DIR`). workspace-context.md §4.1.
4. Headless `ask` is never prompted (no stdin approval). What happens depends on the permission MODE (`synth run --permission-mode`, tools.md §3) plus a pre-declared allowlist (`--allow` / settings); an ask nobody can answer is auto-denied, and a denied write/execute call ends the task `blocked` / `approval_unavailable`. Node's "outside read is allowed when nobody can answer" is not carried over. Owner decision 2026-10-28. tools.md §3.
5. Policy is re-checked after a hook or approver rewrites tool input (Node does not). tools.md §3.
6. Rust adds nothing to `config.json/v1` during the parallel run; Rust-only settings live under `"synthesis"` in `settings.json`. config.md §6.
7. Rust mirror-writes Node-shaped transcript JSONL until cutover; SQLite is the source of truth. session-store.md §C.
8. Pure Q&A tasks with no side effects skip the gate and end `completed`. verification.md §4.

## Open questions for the product owner
1. Can inferred checks alone yield `passed`/exit 0 when the user gave no acceptance criteria, or is the ceiling `unverified`? (verification.md §3 rule 4; doc §13.7)
2. ~~Approval in headless/CI~~ Resolved by the product owner (2026-10-28): approval depends on the permission mode, plus option 2 (a pre-declared allowlist via `--allow` / settings); no stdin prompting. See decision #4 and tools.md §3.
3. ~~Credential migration~~ Resolved by the product owner (ADR-011): API keys only, user-managed (env vars / provider config); no keychain, Rust never writes credentials.
4. Is the Node eval baseline allowed to run through an in-repo adapter that drives `NativeAgent` programmatically (Node has no headless mode)? Without it there is no baseline before Rust exists.
5. Is `Idle` acceptable in the doc's state machine, and is the doc §7.2 schema allowed to grow (`messages`, `parent_session_id`, `legacy_transcript_path`)?
Also open, lower stakes: thinking deltas in the stream; artifact retention default; whether the model may call `verify_run`; keeping `TodoWrite` and `TaskCreate/*` both; case-sensitive macOS volumes.

## Found in the Node reference that the doc or the task brief do not reflect
- Node transcripts live in `$SYNTHESIS_HOME/projects/…` (`~/.synthesis`), not `~/.claude/projects` (transcript.js:32, watcher.js:11-12); the `~/.claude/projects` comment in history.js:3 is stale. Claude Code's tree is only read to import memory.
- Node has no headless/JSONL mode and no exit-code contract (bin/synth.js uses 1 for every error, 130 for SIGINT).
- Node has no JSON Schema validation of tool input; each tool checks fields by hand.
- Node tool paths have no containment: `resolvePath` accepts any absolute path (tools/util.js:10). Containment exists only in the permission layer (ask/deny), with realpath and case folding (permissions.js:323-353, 380-386).
- Node agent cwd defaults to `process.cwd()` (agent.js:303): the implicit inheritance the doc forbids.
- Node stacks retries: SDK `maxRetries: 4` (providers/anthropic.js:51, openai.js:88) under the agent's own 3 retries (agent.js:50). Rust should keep one retry layer.
- Doc §10 and Appendix A put the Cargo workspace in `~/Vibe/Coding-Agent/nemo-cli`; CLAUDE.md and this repo are `~/Vibe/Nemo-ACD`.
- Doc §5.6 lists `terminal.exec` etc. as new tools, but `Bash`/`Read`/`Edit`/`Grep` already cover `terminal.exec`/`file.*`.
- Project config is read from `<cwd>/.synthesis/config.json` while settings come from `<projectRoot>/.synthesis/settings.json` (config.js:278 vs claudecfg.js:800).

## R1 progress
- `tests/agent-evals/` holds the eval harness (Node ESM, no deps), the 8 v1 scenarios, and oracle/noop/node/rust adapters; see its README. Self-test: `node tests/agent-evals/harness/selftest.js`. Open question 4 above is answered in practice: `NativeAgent` runs headless through the in-repo adapter. Windows/Linux and real-provider runs are unverified.

## Branding and TUI theme (2026-10-30)

The owner's brand kit is in `design/` (README there). The kit palette is silver on near-black (`design/02-design-tokens/tokens.css`); the TUI theme in `crates/synthesis-tui-render/src/theme.rs` (`ACCENT_*`, `BORDER_GRAD`, `LOGO_GRAD`) is still indigo. Not changed with the rename: switching the accents rewrites the SGR snapshots of nearly every render golden, which is a larger, reviewable change of its own. Wordmark: plain `Synthesis` in the dashboard header; the spaced uppercase `S Y N T H E S I S` of the kit is not used by the TUI.
