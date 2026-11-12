# Permissions: harness audit hardening

Status: draft. Scope: what the permission engine (`crates/synthesis-permissions`) and the harness enforce in code for autonomous operation, fail closed. User-facing summary: docs/SECURITY.md.

## Order of decision (Bash and edit tools)
deny rules, hard read denials, critical `rm`, **irreversible-action table**, secret files, hard links, plan, protected paths, allow rules, mode defaults. For the edit tools the **read-only set** check comes before the bypass shortcut and before every allow rule.

## P1 read-only set (`is_tool_readonly`)
The whole `.git` tree and pointer file, `.gitmodules`, `.gitattributes`, `.synt/`, `.husky/`, `AGENTS.md`, `.envrc`, `.mcp.json`, `.cargo/`, `.devcontainer/`, `.npmrc`, `.yarnrc*`, `.pre-commit-config.yaml`, `lefthook*`, `.vscode/{tasks,settings,launch}.json`. Segments are root-relative and case-folded, taken from the lexical path, the realpath and a by-hand link resolution (`paths::resolve_links`, dangling links and chains included), with Windows aliases reduced (trailing dots/spaces, streams, 8.3). Hard deny in every mode. Lifted only by `Env.user_allow` rules whose literal text (no wildcard) is the path or its tail.

## P2 irreversible table (`irreversible.rs`)
`scan(cmd, worktree) -> Vec<Hit{key, reason, force}>` over `deep_segments` + loose `unwrap_words`. Verdict: hard Ask (headless Deny), Deny in plan/dontAsk and for `force`. A hit is lifted when a user-level Bash rule contains `hit.key` and covers the whole command (`bash_all_match_or`, read-only parts and `cd` pass). Unparsable commands that name a table tool, and command names built from `$`/backticks, ask.

## P3 auto judge
- Engine: in `auto`, a command running a file in `Env.written` asks (not judged) unless `Env.sandbox_enforcing` and `bash::is_test_runner`.
- Harness (`classifier.rs`): action over `INPUT_CAP` asks; `TaskRun.after_image` asks for non-edit actions; both before the cache; prompt carries `files_written_by_the_agent_this_task`.
- Defaults: `Read-Only HTTP` and `Push To Working Repo` removed from `ALLOW`; `HARD_DENY` has credential exfiltration, secrets in URLs, credential paths.
- Judge model: user setting `autoMode.model`.

## P4 project code
`Permissions::project_code_trusted()` = `Env.sandbox_enforcing` && no definition file (`synthesis_core::definitions`, the one shared list; the verification-gate crate `synthesis-verification/src/plan.rs` should use it too) in `Env.written` && the hashes taken by `Env::snapshot_definitions()` at session start still match. Used by `seg_safe` (auto-safe list) and the gate's `trusted_inferred`. The harness refreshes `Env.written` from `TaskRun.changed` before each check.

## P5 file commands
`risky_delete` asks in acceptEdits/auto. `bash::mutation_targets` feeds a `before-first-bash-N` checkpoint in `synthesis-app/src/events.rs`.

## P6 redaction
`Redactor` patterns in `synthesis-events/src/io.rs`; `Redactor::add_secret` registers exact values; the harness registers `KEY=value` / `key: value` values of a successful `Read` of a secret-shaped file.

## Known gaps
See the NOT_FIXED list in the commit message / SECURITY.md limits: Bash writes to the read-only set rely on the sandbox; MCP tools are not covered by P1; the table is not exhaustive; the image rule has no setting to relax it.

## Shared list for the verification gate
`synthesis_core::definitions::{FILES, PREFIXES, DIRS, is_definition}` is the single list of files that define what a build/test command runs. `feat/harness-gate` defines its own list in `crates/synthesis-verification/src/plan.rs`; replace it with this one.
