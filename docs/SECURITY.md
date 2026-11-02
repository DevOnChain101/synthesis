# Security

Status: pre-1.0, not independently audited. This document states what Synthesis tries to guarantee and, deliberately, what it does not.

## Reporting a vulnerability

Do not open a public issue. Use GitHub private vulnerability reporting ("Security" tab, "Report a vulnerability") on the repository. TODO(owner): enable it and add a fallback contact address here before the first release. Include version (`synt --version`), OS, and a reproduction. Expect acknowledgement on a best-effort basis; there is no bounty.

## Threat model

Assets: your files outside the workspace, your credentials, your network position. Adversaries considered:
1. **Prompt injection** from repository files, web pages, tool output, MCP servers, or the model itself. All of it is data, never instructions or permission grants.
2. **Malicious repository** (config, hooks, skills, MCP servers, build scripts) opened by a trusting user.
3. **Buggy or over-eager model** performing destructive actions.

Out of scope: a malicious local user or root, a compromised model provider, a hostile process that has already escaped the sandbox, kernel exploits, side channels. Synthesis is not a container or VM.

## Trust model

- A permission gate sits before every tool call (ADR-007); model proposals do not grant privileges. Modes: `default`, `acceptEdits`, `plan`, `dontAsk`. Hooks fail closed. `bypassPermissions` (CLI flag, or the Shift+Tab confirmation dialog) skips prompts but still stops deny rules, ask rules, critical `rm` and destructive MCP asks; it does NOT stop plain writes to protected paths (`.git`, `.synt`, `.ssh` ...), as in Claude Code, so rely on the sandbox there.
- **Project-level config, hooks, skills and MCP servers are untrusted by default**: files inside a repository can ask for execution, so they are not honoured as if you had written them. User-level config in `$SYNT_HOME` is trusted. See docs/specs/config.md for the exact layering.
- Child process environments (shell, hooks, MCP stdio) are an **allowlist** (PATH, HOME, locale, TERM, proxy and `SSL_CERT_*` variables, non-secret-shaped `SYNT_*`). API keys and tokens are never passed on. Only `{env:NAME}` references you write yourself, or explicit per-server env maps, add variables.
- Only `SYNT_*` variables are read for Synthesis's own settings (`ANTHROPIC_*`/`OPENAI_*` are never read). Secrets are redacted from events, logs, transcripts and artifacts, and Debug output lists names only.
- Browser: isolated profile, URL policy, Chrome sandbox on unless a test hook disables it.
- Browser logins (docs/BROWSER-LOGIN.md): `browser_login` opens a visible browser on a dedicated profile under `$SYNT_HOME/browser-profiles` and always needs the person (every mode, no allow rule, never for peers or sub-agents); `browser_fill_secret` types a secret declared in USER settings only into the exact origins it is bound to, and the value never reaches the model, events, transcript, database or tool output. Neither tool returns cookies or page content. The saved profiles and `{file:}` secret files are hidden from the agent's commands and file tools like the rest of Synthesis's state, and the tools refuse unless the sandbox is `enforce`; an env-var secret is readable through `/proc` by the same user.

## Sandbox guarantees (commands run by the agent)

`Strong` = kernel-enforced boundary for that dimension. `Weak` = defence in depth against accidents and lazy attacks, **not** a boundary against a hostile process. `None` = not enforced. Taken from the backends' `capabilities()` in `crates/synthesis-sandbox`; `synt doctor` prints what your machine actually selected.

| OS / backend | Filesystem | Network (deny) | Process tree | Notes |
|---|---|---|---|---|
| macOS, Seatbelt (`sandbox-exec`) | Weak | Weak | Strong | Deprecated by Apple, may break on any update. `--network loopback` exposes the HOST loopback (databases, Redis, docker TCP) and a 0.0.0.0 listener is LAN-reachable. |
| Linux, Landlock (helper `synthesis-sandbox-exec`) | Strong with ABI >= 3, else Weak | Weak with ABI >= 4, else None | Strong | `--network loopback` unsupported. File metadata (stat) stays readable. |
| Linux, bubblewrap | Weak | Strong | Strong | Fresh network namespace; `loopback` is private to the sandbox. Needs unprivileged user namespaces. |
| Windows, Job Object | None | None | Weak | Only resource/process-tree containment. Treat as unsandboxed. |

`--network allow` restricts nothing. Per-host allowlists are unsupported on every backend and rejected. With `--sandbox enforce` (default) Synthesis refuses to run a command it cannot confine to at least `Weak`; `best-effort` and `off` run unconfined and warn loudly. On Windows nothing can confine a command, so when `--sandbox` is NOT given the default is `best-effort` with a loud one-line notice at start (`sandbox: this OS has no command confinement ... UNCONFINED`) instead of refusing every command; `--sandbox enforce` still refuses them. macOS and Linux keep `enforce` as the default (owner decision D10, 2026-10-28); check `synt doctor`.

**Read-only inside the writable workspace.** Besides `.git/hooks` and `.git/config`, the standard policy makes `.git/modules`, `.gitmodules`, `.synt/` (project settings: hooks, MCP, agents, skills), `AGENTS.md` and `AGENTS.md` read-only, so a sandboxed command cannot plant configuration or instructions that the next trusted session would load. The Write/Edit tools judge the same names as protected paths. Seatbelt enforces these even when the path does not exist yet; bubblewrap only for paths that exist (a missing `.synt/` can still be created by a command on Linux); the Windows backend has no filesystem confinement. Side effect: `git submodule update` and similar writes to `.git/modules` fail inside the sandbox.

**Linux requirement for `--sandbox enforce`.** The standard policy makes `.git/hooks` and `.git/config` read-only inside the writable workspace. Landlock cannot express a read-only path inside a writable root and rejects that policy, so on Linux `enforce` needs **bubblewrap** (`bwrap`) with unprivileged user namespaces. With Landlock alone `synt doctor` prints `enforce_ready: no (...)` and `synt run` exits 2; Enforce is not weakened. On Ubuntu 24.04+ AppArmor restricts unprivileged user namespaces: either `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, or an AppArmor profile that lets `/usr/bin/bwrap` create them. Otherwise run with `--sandbox best-effort` (commands run unconfined). `synt doctor` reports `enforce_ready: yes|no (reason)` by dry-running the same preflight as `synt run`. On Linux the bubblewrap check is a real probe: `bwrap` runs `/bin/true` with the exact argument set used for commands (user/pid/ipc/net namespaces, `--proc /proc`, `--dev /dev`, tmpfs, read-only root bind), once per process, so `enforce_ready: yes` means a command will start. When it fails, doctor and the `synt run` refusal print bwrap's own error and a remedy (inside Docker, where `/proc` is masked: `--security-opt systempaths=unconfined`; blocked user namespaces: the sysctl/AppArmor fixes above). Backends are tried bubblewrap, then Landlock, then none. `enforce` uses the first and refuses with its reason; `best-effort` degrades to the next backend that accepts the policy, finally to unconfined, printing one `WARNING ... UNCONFINED (unsandboxed_by_request)` line to stderr and flagging every such result `sandbox: null, unsandboxed_by_request: true`. Limit: only the policy-independent argument set is probed; a failure specific to one policy (for example a bind of a path that disappears) still surfaces as that command's own error, not as a fallback.

## Data retention

Everything is local under `$SYNT_HOME` (default `~/.synt`): settings, session transcripts (`projects/<cwd>/<session>.jsonl`, written while `compat.write_node_transcripts` is on), the SQLite store `synt.db`, and artifacts under `sessions/<id>/artifacts/`. Transcripts contain your prompts, file contents the agent read, and command output, with known secret shapes redacted (best effort, not a guarantee). There is no telemetry and no retention cleanup yet: delete the files to delete the data. Synthesis writes no credentials to disk; the legacy plaintext `auth.json` of the Node CLI is not carried over. Prompts and tool results are sent to the model provider you configure.

## Supply chain

CI runs cargo-deny (licenses, bans, sources, advisories) and cargo-audit. Release archives, `SHA256SUMS` and the installers carry GitHub build provenance attestations, plus a CycloneDX SBOM. The `test-provider` feature (scripted fake model) is a compile error in release profiles and is checked absent from every release binary.

Release pipeline controls (all UNVERIFIED on GitHub until the first release runs; details and owner setup in docs/RELEASE.md):
- Every `uses:` is pinned to a full commit SHA; the release `gate` job runs `scripts/check-action-pins.mjs` and fails otherwise. Dependabot proposes updates.
- The tagged commit must be an ancestor of `origin/main`, and the tag must be annotated with a signature GitHub reports as verified, named exactly as the pushed tag, and pointing at the commit being built (`scripts/check-release-tag.sh`, run in `gate` and again in `release` right before the draft is created with `--target $GITHUB_SHA`; a draft is deleted if the tag moved during creation). Caveat: "Verified" means signed by a key of any GitHub account, so any account allowed to push tags passes. `.github/release-maintainers.txt` is an allowlist of tagger emails and the check FAILS CLOSED: a missing or empty list refuses the release unless the repository variable `SYNT_ALLOW_ANY_VERIFIED_TAGGER` is exactly `true` (explicit owner risk acceptance). Also restrict tag creation and block tag updates with a ruleset, and enable immutable releases. Publishing is a separate `publish` job in the `release` environment that re-runs the tag check and verifies the draft's `targetCommitish` equals the built commit before `gh release edit --draft=false`. The `release`/`publish` checkouts do not persist the token.
- Signing certificates are ENVIRONMENT secrets of the `release` environment (required reviewers, tag-restricted), never repository secrets. Build, attest and release jobs run in that environment. `id-token`/`attestations` write is granted to the `attest` job only; `contents: write` to the `release` job only. Release builds use no build cache.
- Workflows trigger on `push` (branches/tags), `pull_request` (no secrets, read-only token) and `schedule`; there is no `pull_request_target` and no `workflow_run`. User-controlled values (tag, ref names) reach shell only through `env:`.
- Installers: https only (`--proto '=https' --tlsv1.2`), version allow-list, sha256 check, and `gh attestation verify` of `SHA256SUMS` by default; they fail closed when `gh` is missing (`SYNT_SKIP_ATTESTATION=1` accepts checksum-only with a loud warning), pin the attestation to the `release.yml` signer workflow and to `refs/tags/v<version>`, and refuse to run with the `OWNER` placeholder repository (`scripts/check-placeholders.mjs` also gates the release).

Limits: a pinned SHA fixes which code runs, not that it is good; review Dependabot bumps. With `SYNT_SKIP_ATTESTATION=1` the installers authenticate nothing beyond TLS to GitHub. The attestation proves the artifacts were built by `release.yml` of the repository, not that the source is benign.

## Artifacts are raw

Command logs and `tool-results/` artifacts hold the tool's output UNREDACTED (redaction applies to events, not to these files). The `tool-results` directory is created 0700 and its files 0600 on unix; on Windows rely on the profile ACLs. Treat the artifact root like the working tree: do not share or upload it unreviewed.

## Known gaps

No independent security audit; Windows has no filesystem/network confinement; Seatbelt is deprecated; secret redaction is pattern-based; macOS/Windows signing depends on owner-provided certificates (docs/RELEASE.md).
