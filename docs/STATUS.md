# Synthesis status

The CLI is back (restored 2026-11, `docs/cli-feature-port.md`) and the native GPUI desktop app is rebuilt at `apps/desktop` (owner request 2026-11-06, `docs/specs/desktop.md`: TUI view on the owner's `cli.html`, Super Agent = coming soon, Settings, no launchpad, no terminal emulator); the redesigned (N1-N5) TUI stays removed (tag `archive/ui-before-removal`). Figures below that mention the old CLI's goldens or tty-qa describe the archived state, not the restored crates.

Goal: R1–R9, hand over the full Synthesis autonomous coding agent. Not complete: see "Open".

## Verified
- CI on `main` at PR #55 (2026-11-07, GitHub Actions, public repo `DevOnChain101/synthesis`): `ci-ok` green with 19 jobs: test (macOS, Ubuntu, Windows), the `desktop` job (build, clippy, tests, and a runtime smoke: offscreen PNG render on Linux and macOS, real window first frame on Windows; the desktop job is required), browser jobs, msrv 1.89, sandbox (linux), docs, cargo-deny, cargo-audit, shell-scripts and the eval selftests. Known flaky tests that pass on re-run: `cli_pty` on Ubuntu (a different test times out each time), `synthesis-tools/tests/browser.rs` on macOS, `apps/cli/tests/e2e.rs` peers tests on Windows (SQLite lock on event append, retried since PR #55).
- macOS (local, tester + developers): `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --features synthesis-cli/test-provider` (about 1740 tests, 0 failed at PR #54), `cargo test -p synthesis-cli --features test-provider` (88), eval selftest 6/6, rustdoc `-D warnings`, `cargo +1.89 check`.
- Counting method (to compare future runs): `grep -E '^test result' log | awk '{p+=$4; f+=$6; i+=$8}'`.
- Real-model eval after the rename (2026-10-30, 40 runs, acceptEdits, sandbox enforce; docs/research/eval-baseline-rust.md): status-ok haiku 79% (19/24), luna 94% (15/16); false-success 0/40; code tasks completed without model approval haiku 44%, luna 100%. Cutover thresholds: false-success met; status-ok >= 85% not met for haiku; approvals >= 50% not met for haiku alone (pooled 67%).
- Real provider smoke (omniroute, OpenAI-compatible gateway): `cc/claude-haiku-5-5` and `cx/gpt-5.6-luna` stream, call tools and finish a task.
- Security: five review passes plus a sixth on the permission/MCP/`!cmd`/streaming changes (fixes in); residual risks in docs/SECURITY.md.
- Golden: 717 snapshot cases from the Node prototype are regression snapshots only (the prototype is not authoritative); documented divergences are listed next to each golden test.

## Built (R1–R9)
- R1 contracts, specs (reviewed by the owner 2026-10-28), eval harness, golden snapshots.
- R2 core: config, terminal, hooks, tools, permissions (all six modes incl. the auto-mode LLM judge; bypass needs confirmation).
- R3 providers (Anthropic, OpenAI-compatible) and MCP (stdio, streamable HTTP, SSE; read-before-write guard; credential propagation checks; `synt mcp test`).
- R4 harness, verification gate (one approval per check set; edits with no check end `unverified`; hardened 2026-11-12: planned test checks must pass, checks frozen at task start, baseline snapshot sees shell edits, no-op/flaky checks and A,B,A,B loops, gaming eval scenarios; docs/specs/verification.md §3b), headless `synt run` with `--permission-mode`, `--allow/--deny`.
- R5 CLI: the CLI was restored on the current core (crates `synthesis-cli-ui*`, `apps/cli/src/cli_app.rs`; platinum palette, logo welcome, `/config`, peers, image paste, `/close`, hideable panels). The GPUI desktop app is rebuilt at `apps/desktop` on the shared `synthesis-view` models and `synthesis-settings-ui` (PRs #40-#54; packaged for macOS, Windows and Linux by `release.yml`); the redesigned TUI remains removed (archive tag `archive/ui-before-removal`).
- R6 browser (own CDP client); browser login hand-off (`browser_login`, headed, persistent profile) and secret fill by reference (`browser_fill_secret`): implemented on branch feat/browser-login, headless paths tested with real Chrome, headed window unverified on a real desktop (docs/BROWSER-LOGIN.md). R7 sandbox (Seatbelt tested; Landlock/bwrap; Windows Job Object). R8 sessions (SQLite) and worktrees. R9a CI + release pipeline.

- Harness audit hardening (branch feat/harness-perms, 2026-11-12): read-only set for the edit tools, irreversible-action table, auto-judge fail-closed rules, project-code trust, rm asks and Bash checkpoints, wider redaction (docs/specs/permissions.md, docs/SECURITY.md). Unverified on Linux/Windows CI; behaviours that now ask or deny are listed in the commit message.

## Governance leftovers (branch feat/governance-leftovers)
Pre-turn worst-case cost estimate (`budget_exceeded:max_cost_usd_estimate`); judge and reflection calls priced to the counters; long-context price tiers (built in, `synt.pricing` override); OpenAI cached tokens no longer double counted; optional `synt.notify` hook (user/managed settings); opt-in worktree submodules (`synt.worktree.submodules`, hardened, https/ssh only); `synt stop` for the desktop's in-process session (no app exit, mechanism printed); terminal restored on the CLI's hard stop. Details and limits: docs/UNATTENDED.md. Desktop system notification is a stub (the app has no notifier). Verified on macOS only.

## Governance of unattended runs (branch feat/harness-governance)
Cost accounting prices cache tokens and compaction calls, `--max-tokens`, built-in prices for first-party models; `synt run`/`engine --stdio` default to a 30 min wall cap, a required cost cap, a worktree, inferred-only = unverified; `--unattended` preset; fatal session-store failure when unattended; resume of worktree sessions; `synt stop`; hash-chained events with `synt sessions verify`/`show`; full verification report stored; provider `fallback`. Details, limits and NOT_FIXED items: docs/UNATTENDED.md. Verified on macOS only; Windows/Linux CI not run for this branch.

## Browser child targets and red-team suite (branch feat/browser-redteam, 2026-11-13)
Browser: the DNS-rebinding check (`Network.responseReceived`) now covers child targets (popups, iframes) with `Network.enable` on every auto-attached session, kept out of the network log; workers are proven closed unresumed; a socket bypass matrix is tested and the former sandboxed-srcdoc WebSocket leak is closed by an injected connect-src CSP (docs/SECURITY.md). `tests/redteam` (CI job `redteam`) and `evals-nightly.yml` (real model, skipped without `SYNT_API_KEY`) added; known gaps are listed in docs/SECURITY.md. Verified on macOS only; Linux and Windows results of the new job are unknown until CI runs.

## Decisions on record
Product rename to Synthesis (owner, 2026-10-30; see CHANGELOG): command `synt`, crates `synthesis-*`, env `SYNT_*`, home `~/.synt`; the old names are detected by `synt doctor` and never read (docs/MIGRATION.md). Not verified yet after the rename: Linux and Windows CI (run it on the next push); GitHub repository variables must be recreated under the new names (docs/RELEASE.md).

ADR-010 Rust core; ADR-011 API keys only (SYNT_*); sandbox enforce by default (Windows: best-effort with a notice); network deny by default; the Node prototype is a frozen, non-authoritative reference (design doc plus official Claude Code/Codex practice decide behaviour, see docs/research/official-conformance.md).

## Open
- Owner: R9 cutover decision on the two haiku threshold misses (accept pooled, or product change; see the eval doc); golden snapshot review; release setup (maintainer emails or `SYNT_ALLOW_ANY_VERIFIED_TAGGER`, `release` environment reviewers, tag ruleset, signing, npm names, OWNER placeholders); Dependabot PRs; remote-MCP OAuth decision (default: not supported); owner email in public commit history.
- Desktop: never run by a human in a real window (1:1 look, title bar, drawers); Linux/Windows pixels and real GPU unverified (CI proves a Linux/macOS offscreen render and a Windows first frame only); packages unsigned and not notarized; `/name` rename of the stored session title undecided.
- Engineering: `browser (windows)` job hangs (cause unconfirmed); Windows/Linux behaviour of ConPTY, Git Bash quoting, clipboard, Landlock/bwrap enforcement is verified only by CI, not on real machines; remaining conformance gaps in docs/research/conformance-gaps.md (P2); QA items B-14, B-18, B-25, B-29, B-34.
