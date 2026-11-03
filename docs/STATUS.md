# Synthesis status

The classic TUI is back (restored 2026-11, `docs/tui-feature-port.md`); the GPUI desktop and the redesigned TUI stay removed (tag `archive/ui-before-removal`). Figures below that mention the TUI's goldens or tty-qa describe the archived state, not the restored crates.

Goal: R1–R9, hand over the full Synthesis autonomous coding agent. Not complete: see "Open".

## Verified
- CI run #22 on commit c6c6a29 (GitHub Actions, public repo `DevOnChain101/synthesis`): `ci-ok` green. test (macOS, Ubuntu, Windows), msrv 1.89, sandbox (linux), docs, cargo-deny, cargo-audit, shell-scripts and the three eval selftests passed. Only `browser (windows-latest)` is red (non-blocking, `continue-on-error`).
- macOS (local, tester + developers): `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (about 884 tests, 0 failed), `cargo test -p synthesis-cli --features test-provider` (88), eval selftest 6/6, rustdoc `-D warnings`, `cargo +1.89 check`.
- Counting method (to compare future runs): `grep -E '^test result' log | awk '{p+=$4; f+=$6; i+=$8}'`.
- Real-model eval after the rename (2026-10-30, 40 runs, acceptEdits, sandbox enforce; docs/research/eval-baseline-rust.md): status-ok haiku 79% (19/24), luna 94% (15/16); false-success 0/40; code tasks completed without model approval haiku 44%, luna 100%. Cutover thresholds: false-success met; status-ok >= 85% not met for haiku; approvals >= 50% not met for haiku alone (pooled 67%).
- Real provider smoke (omniroute, OpenAI-compatible gateway): `cc/claude-haiku-5-5` and `cx/gpt-5.6-luna` stream, call tools and finish a task.
- Security: five review passes plus a sixth on the permission/MCP/`!cmd`/streaming changes (fixes in); residual risks in docs/SECURITY.md.
- Golden: 717 snapshot cases from the Node prototype are regression snapshots only (the prototype is not authoritative); documented divergences are listed next to each golden test.

## Built (R1–R9)
- R1 contracts, specs (reviewed by the owner 2026-10-28), eval harness, golden snapshots.
- R2 core: config, terminal, hooks, tools, permissions (all six modes incl. the auto-mode LLM judge; bypass needs confirmation).
- R3 providers (Anthropic, OpenAI-compatible) and MCP (stdio, streamable HTTP, SSE; read-before-write guard; credential propagation checks; `synt mcp test`).
- R4 harness, verification gate (one approval per check set; edits with no check end `unverified`), headless `synt run` with `--permission-mode`, `--allow/--deny`.
- R5 TUI: the classic TUI was restored on the current core (crates `synthesis-tui*`, `apps/cli/src/tui.rs`; palette, logo welcome, `/config`, peers, image paste); the GPUI desktop app and the redesigned TUI remain removed (archive tag `archive/ui-before-removal`).
- R6 browser (own CDP client); browser login hand-off (`browser_login`, headed, persistent profile) and secret fill by reference (`browser_fill_secret`): implemented on branch feat/browser-login, headless paths tested with real Chrome, headed window unverified on a real desktop (docs/BROWSER-LOGIN.md). Desktop control (`desktop_*` on a pinned `agent-desktop` CLI, off by default): implemented on branch feat/desktop-control and tested with a fake CLI only; the real binary, OS permissions and windows are unverified and the pin is a placeholder (docs/DESKTOP-CONTROL.md, D0 checklist). R7 sandbox (Seatbelt tested; Landlock/bwrap; Windows Job Object). R8 sessions (SQLite) and worktrees. R9a CI + release pipeline.

## Decisions on record
Product rename to Synthesis (owner, 2026-10-30; see CHANGELOG): command `synt`, crates `synthesis-*`, env `SYNT_*`, home `~/.synt`; the old names are detected by `synt doctor` and never read (docs/MIGRATION.md). Not verified yet after the rename: Linux and Windows CI (run it on the next push); GitHub repository variables must be recreated under the new names (docs/RELEASE.md).

ADR-010 Rust core; ADR-011 API keys only (SYNT_*); sandbox enforce by default (Windows: best-effort with a notice); network deny by default; the Node prototype is a frozen, non-authoritative reference (design doc plus official Claude Code/Codex practice decide behaviour, see docs/research/official-conformance.md).

## Open
- Owner: R9 cutover decision on the two haiku threshold misses (accept pooled, or product change; see the eval doc); golden snapshot review; release setup (maintainer emails or `SYNT_ALLOW_ANY_VERIFIED_TAGGER`, `release` environment reviewers, tag ruleset, signing, npm names, OWNER placeholders); Dependabot PRs; remote-MCP OAuth decision (default: not supported); owner email in public commit history.
- Engineering: `browser (windows)` job hangs (cause unconfirmed); Windows/Linux behaviour of ConPTY, Git Bash quoting, clipboard, Landlock/bwrap enforcement is verified only by CI, not on real machines; remaining conformance gaps in docs/research/conformance-gaps.md (P2); QA items B-14, B-18, B-25, B-29, B-34.
