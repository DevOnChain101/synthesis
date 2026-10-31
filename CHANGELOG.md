# Changelog

Format: Keep a Changelog. Versioning: SemVer (pre-1.0: minor versions may break).

## [Unreleased]

### Removed (owner decision 2026-10-31)
- All interactive UI: the TUI (`synthesis-tui`, `-tui-base`, `-tui-render`, `synth tui`, `synthesis.tui.*` settings, `SYNTHESIS_ASCII`/`SYNTHESIS_COLOR`/`SYNTHESIS_ONCE_SCREEN`), the GPUI desktop app (`apps/desktop`) and their goldens, tty-qa and frame tooling. `synth` with no subcommand prints help. Archived at tag `archive/ui-before-removal`; a new UI will be built later from the HTML design.

### Changed (rename Nemo to Synthesis, owner decision 2026-10-30)
- The product is **Synthesis**; the command is `synth` (`synth run|tui|doctor|auth|models|mcp|import`). Crates `nemo-*` are `synthesis-*` (package `synthesis-cli`, bin `synth`), env `NEMO_*` is `SYNTHESIS_*`, `~/.nemo` is `~/.synthesis`, `.nemo/` is `.synthesis/`, `nemo.json` is `synthesis.json`, `NEMO.md` is `SYNTHESIS.md`, `nemo.db` is `synthesis.db`, settings section `"nemo"` is `"synthesis"`, npm `@devonchain101/synthesis`, archives `synthesis-<version>-<target>`, bundled skill `nemocode-config` is `synthesis-config`. The GitHub slug stays `DevOnChain101/nemo-acd` until the new repository exists (docs/RELEASE.md). Repository variables `NEMO_ALLOW_ANY_VERIFIED_TAGGER` and `NEMO_NPM_TRUSTED_PUBLISHING` are now `SYNTHESIS_*`.
- No automatic migration: the old names are not read. `synth doctor` prints the manual migration steps when `~/.nemo` exists and `~/.synthesis` does not, names any set `NEMO_*` variable (never values) and warns when another `synth` on PATH shadows this one (docs/MIGRATION.md).
- Golden snapshots follow the same mapping (owner-approved divergence from the Node captures; regression-only). 29 render cases were regenerated on 2026-10-30 with `SYNTHESIS_UPDATE_GOLDEN=1` (longer wordmark, wrapping and header chips); `tools/golden-capture` is untouched and can no longer regenerate them.
- New `design/` (brand kit, tokens, landing and desktop design references) and CI check `scripts/check-no-legacy-name.mjs`.

### Changed (release engineering)
- License is Apache-2.0 (LICENSE, NOTICE). npm packages are `@devonchain101/nemo` and `@devonchain101/nemo-<os>-<arch>[-musl]` (command `nemo`, `publishConfig` access public + provenance). Installers moved to `scripts/install.sh` / `scripts/install.ps1` (default repo built in, run `nemo doctor` after install, attestation verified whenever `gh` exists). `release.yml`: `workflow_dispatch` dry run, no private signing secrets, optional `npm-publish` job with provenance.

### Changed
- TUI: drag-select may start on any cell, double-click selects a word, triple-click a line, Ctrl+C copies a selection (else keeps its interrupt meaning), copies leave out gutters and join wrapped rows, notice "Copied N chars". `/mode` removed: Shift+Tab cycles default, acceptEdits, plan, auto, bypassPermissions (confirmation dialog), default.
- Permissions: `Read(..)` deny now also blocks Bash file readers and Write/Edit of that path; `mcp__*` / `mcp__server__*` deny rules match; ask rules still prompt under bypassPermissions; `--help` and docs state what bypass still stops.
- A malformed or unreadable `settings.json` is a hard error (`nemo run` exit 2, TUI does not open) instead of a warning that dropped its deny rules.
- Memory files: first non-empty of NEMO.md > AGENTS.md > CLAUDE.md per directory, 32 KiB combined. Hook default timeout 600 s except PreToolUse/UserPromptSubmit (60 s). Read refuses whole-file reads over 256 KB and results over ~25k tokens. Interactive Bash timeouts move the command to the background. Windows defaults to `--sandbox best-effort` with a notice.

### Security
- Release tag allowlist now fails closed: `scripts/check-release-tag.sh` exits non-zero when `.github/release-maintainers.txt` is missing or has no entries, unless repository variable `NEMO_ALLOW_ANY_VERIFIED_TAGGER` is exactly `true`. Tests: `scripts/test-check-release-tag.sh` (run in CI). Owner must add maintainer emails before the first tag.
- release.yml: `release` checkout no longer persists credentials; publishing moved to a separate `publish` job (environment approval) that re-checks the tag and the draft's `targetCommitish` before un-drafting.
- install.ps1 now requires PowerShell 7+ (it used `-SkipHttpErrorCheck`) and fails early with an install hint; docs no longer claim 5.1.
- Release gate: `scripts/check-release-tag.sh` also requires the tag to point at the built commit, the signed tag object to carry the tag's name and target that commit, and the API ref to match; it is re-run in the `release` job before and after creating the draft, which is now created with `--target $GITHUB_SHA`. Optional maintainer allowlist `.github/release-maintainers.txt` (empty = off); `Verified by GitHub` trust caveat documented.
- Installers fail closed when `gh` is missing (`NEMO_SKIP_ATTESTATION=1` to accept checksum-only; `NEMO_VERIFY` removed, now the default) and verify the attestation with `--source-ref refs/tags/v<version>`.
- ci.yml: checkouts no longer persist the token (`persist-credentials: false`).
- Release pipeline hardening: actions pinned to commit SHAs (enforced by `scripts/check-action-pins.mjs`), Dependabot, `release` environment, main-ancestor and signed-tag gate, per-job least-privilege permissions, no `${{ }}` of user data in `run:`, no build cache in release builds, SHA256SUMS and installers attested.
- Installers: https-only downloads, version allow-list, attestation verification of SHA256SUMS by default; repository slug from `packaging/REPO` with hard failure on the `OWNER` placeholder (`scripts/check-placeholders.mjs`).

## [0.1.0] - DRAFT, not released

### Added
- Rust core engine workspace: config, permissions, tools, terminal runtime, hooks, providers (Anthropic-compatible and OpenAI-compatible), MCP client, sessions, verification engine, browser runtime, OS sandbox backends (Seatbelt, Landlock, bubblewrap, Windows Job Object).
- Headless CLI `nemo` (`run`, `auth`, `models`, `doctor`) emitting JSONL events.
- API-key-only model access via `NEMO_*` environment variables (ADR-011).
- Sandbox controls `--sandbox enforce|best-effort|off`, `--network deny|loopback|allow`, `--sandbox-read`.
- Release engineering: 3-OS CI, MSRV, docs, cargo-deny/audit, Linux sandbox and browser jobs; tag-triggered release pipeline with checksums, provenance, SBOM, optional signing; npm platform packages (`nemo-acd`), install scripts, Homebrew/winget templates.

### Known limitations
- CI and the release pipeline have not run on GitHub; only macOS has been exercised.
- Windows has no filesystem or network sandbox; macOS Seatbelt is deprecated.
- Eval parity with the Node CLI is unmeasured; TUI, durable sessions and worktrees incomplete.
- `cargo audit` currently reports RUSTSEC-2026-0009 in `time 0.3.45` (fixed in >= 0.3.47); update `Cargo.lock` before release.
