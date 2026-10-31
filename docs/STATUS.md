# Synthesis status

UI removed 2026-10-31: the figures below that mention the TUI, its goldens or tty-qa describe the archived state.

Goal: R1–R9, hand over the full Synthesis autonomous coding agent. Not complete: see "Open".

## Verified
- CI run #22 on commit c6c6a29 (GitHub Actions, public repo `DevOnChain101/nemo-acd`): `ci-ok` green. test (macOS, Ubuntu, Windows), msrv 1.89, sandbox (linux), docs, cargo-deny, cargo-audit, shell-scripts and the three eval selftests passed. Only `browser (windows-latest)` is red (non-blocking, `continue-on-error`).
- macOS (local, tester + developers): `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (about 884 tests, 0 failed), `cargo test -p synthesis-cli --features test-provider` (88), eval selftest 6/6, rustdoc `-D warnings`, `cargo +1.89 check`.
- Counting method (to compare future runs): `grep -E '^test result' log | awk '{p+=$4; f+=$6; i+=$8}'`.
- Real-model eval after the rename (2026-10-30, 40 runs, acceptEdits, sandbox enforce; docs/research/eval-baseline-rust.md): status-ok haiku 79% (19/24), luna 94% (15/16); false-success 0/40; code tasks completed without model approval haiku 44%, luna 100%. Cutover thresholds: false-success met; status-ok >= 85% not met for haiku; approvals >= 50% not met for haiku alone (pooled 67%).
- Real provider smoke (omniroute, OpenAI-compatible gateway): `cc/claude-haiku-5-5` and `cx/gpt-5.6-luna` stream, call tools and finish a task.
- Security: five review passes plus a sixth on the permission/MCP/`!cmd`/streaming changes (fixes in); residual risks in docs/SECURITY.md.
- Golden: 717 snapshot cases from the Node prototype are regression snapshots only (the prototype is not authoritative); documented divergences are listed next to each golden test.

## Built (R1–R9)
- R1 contracts, specs (reviewed by the owner 2026-10-28), eval harness, golden snapshots.
- R2 core: config, terminal, hooks, tools, permissions (all six modes incl. the auto-mode LLM judge; bypass needs confirmation).
- R3 providers (Anthropic, OpenAI-compatible) and MCP (stdio, streamable HTTP, SSE; read-before-write guard; credential propagation checks; `synth mcp test`).
- R4 harness, verification gate (one approval per check set; edits with no check end `unverified`), headless `synth run` with `--permission-mode`, `--allow/--deny`.
- R5 TUI: built, then removed on 2026-10-31 together with the GPUI desktop app (archive tag `archive/ui-before-removal`); the product is a headless CLI plus libraries until the owner asks for a new UI built from the HTML design.
- R6 browser (own CDP client). R7 sandbox (Seatbelt tested; Landlock/bwrap; Windows Job Object). R8 sessions (SQLite) and worktrees. R9a CI + release pipeline.

## Decisions on record
Product rename to Synthesis (owner, 2026-10-30; see CHANGELOG): command `synth`, crates `synthesis-*`, env `SYNTHESIS_*`, home `~/.synthesis`; the old names are detected by `synth doctor` and never read (docs/MIGRATION.md). Not verified yet after the rename: Linux and Windows CI (run it on the next push); GitHub repository variables must be recreated under the new names (docs/RELEASE.md).

ADR-010 Rust core; ADR-011 API keys only (SYNTHESIS_*); sandbox enforce by default (Windows: best-effort with a notice); network deny by default; the Node prototype is a frozen, non-authoritative reference (design doc plus official Claude Code/Codex practice decide behaviour, see docs/research/official-conformance.md).

## Open
- Owner: R9 cutover decision on the two haiku threshold misses (accept pooled, or product change; see the eval doc); golden snapshot review; release setup (maintainer emails or `SYNTHESIS_ALLOW_ANY_VERIFIED_TAGGER`, `release` environment reviewers, tag ruleset, signing, npm names, OWNER placeholders); Dependabot PRs; remote-MCP OAuth decision (default: not supported); owner email in public commit history.
- Engineering: `browser (windows)` job hangs (cause unconfirmed); Windows/Linux behaviour of ConPTY, Git Bash quoting, clipboard, Landlock/bwrap enforcement is verified only by CI, not on real machines; remaining conformance gaps in docs/research/conformance-gaps.md (P2); QA items B-14, B-18, B-25, B-29, B-34.
