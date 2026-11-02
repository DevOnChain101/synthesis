# Releasing

Everything here is UNVERIFIED on GitHub: the workflows were validated locally only for YAML syntax and the individual commands (see the end of this file).

## Steps

1. Make sure `main` is green (`ci.yml`, including cargo-deny and cargo-audit).
2. Bump `[workspace.package] version` in `Cargo.toml`, update `Cargo.lock` (`cargo update -w`), move CHANGELOG entries from Unreleased, commit, merge.
3. Tag and push as described in "How tag verification works" below: `git tag -s vX.Y.Z -m "Synthesis vX.Y.Z" && git push origin vX.Y.Z`. The `gate` job refuses anything that is not an annotated, GitHub-verified tag by a maintainer listed in `.github/release-maintainers.txt`, on `main`, named `vX.Y.Z` and equal to the workspace version. `scripts/test-check-release-tag.sh` covers these refusals.
4. `release.yml` first runs `gate` (ancestor of main, signed annotated tag, `scripts/check-action-pins.mjs`, `scripts/check-placeholders.mjs --expect <this repo>`), then re-runs the whole CI on the tagged commit (any red job blocks the build), builds 7 targets (musl x86_64 and windows-arm64 are `experimental`: may fail without blocking), proves `test-provider` is absent (`scripts/check-no-test-provider.sh`), smoke-tests (`--version`, `doctor`), packages, makes SBOM, npm tarballs, SHA256SUMS (covering `scripts/install.sh`/`install.ps1`, which are copied into the release as they are), a provenance attestation covering archives, npm tarballs, SHA256SUMS and the installers (job `attest`), and creates a DRAFT release (job `release`, the only one with `contents: write`). The `release` job re-runs `scripts/check-release-tag.sh` right before `gh release create ... --verify-tag --target "$GITHUB_SHA"` (approval can take days, the tag may have been moved meanwhile) and once more after creation; if the tag moved, it deletes the draft and fails. Jobs `attest`, `release`, `publish` and `npm-publish` run in the `release` environment and wait for its reviewers. The draft is NOT public until the small final `publish` job (separate approval) re-runs `scripts/check-release-tag.sh`, checks the release is still a draft whose `targetCommitish` equals `$GITHUB_SHA` (`gh release view --json isDraft,targetCommitish`), and only then runs `gh release edit --draft=false`. The `release` and `publish` checkouts use `persist-credentials: false`; the script passes `GH_TOKEN` as a one-off `http.extraheader` for its tag fetch only.
5. Review the draft: 5 required archives present, SHA256SUMS, SBOM. Download one archive per OS, run `gh attestation verify <file> --repo DevOnChain101/nemo-acd` (also for `SHA256SUMS`), run `synt --version`/`synt doctor`.
6. Approve the `publish` job (it publishes the draft after re-verifying tag and target; do not publish by hand unless that job is unavailable). Then `npm-publish` runs (separate approval): it publishes the platform packages and then `@devonchain101/synthesis` with `npm publish --provenance --access public`, or logs a skip when no npm credential is configured (section npm). Rerunning it after a partial failure fails on versions npm already has; publish the rest by hand with the same command. Homebrew/winget stay manual: fill the `@VERSION@`/`@SHA256_*@` tokens in `packaging/*.tmpl` from SHA256SUMS and submit.

### How tag verification works (plain language)

Only the owner can release. A release starts only when the owner pushes a tag such as `v0.1.0` that is signed with the owner's own signing key. The first job, `gate`, then checks all of the following and stops the whole release at the first failure, with an error message:

1. The tag name is exactly `vX.Y.Z` (optionally `-prerelease`), and `X.Y.Z` equals the workspace version in `Cargo.toml` (the `version` job).
2. The tag is annotated (created with `git tag -s`), not a lightweight tag.
3. GitHub reports the tag signature as `verified: true` (read from the `git/tags` and `git/ref` API). An unsigned tag, or one signed with a key not registered on GitHub, is refused.
4. The tagger identity (the email in the signed tag, which GitHub verified) is listed in `.github/release-maintainers.txt`. It lists only the owner (`43412094+DevOnChain101@users.noreply.github.com`, the noreply identity of account `DevOnChain101`). Anyone else with push access is refused.
5. The tagged commit is reachable from `main`, and the tag still points at that commit when the release is created and published (`release` and `publish` repeat the check).

An empty or missing list refuses every release ("fail closed"). There is no rule about commit signatures beyond this: the repo has no existing commit-signature policy, so reachability from `main` (protected, see checklist) is what is required. Escape hatch, disabled by default and not recommended: the repository variable `SYNTHESIS_ALLOW_ANY_VERIFIED_TAGGER=true` makes an empty list accept any verified tagger; leave it unset. A non-empty list always wins over it, and every other check above still applies.

#### Owner steps to create a release tag

1. Configure signing, once. SSH: `git config --global gpg.format ssh && git config --global user.signingkey ~/.ssh/id_ed25519.pub`. Or GPG: `gpg --full-generate-key`, `gpg --list-secret-keys --keyid-format=long`, `git config --global user.signingkey <KEYID>`. Then `git config --global user.email 43412094+DevOnChain101@users.noreply.github.com` (or add the email you prefer to `.github/release-maintainers.txt` in a reviewed merge to `main`, and use it).
2. Register the key on GitHub as a **Signing Key**: github.com, Settings, SSH and GPG keys, New SSH key, Key type "Signing Key" (for GPG: New GPG key). An SSH key registered only as an "Authentication Key" does not make tags verified.
3. Make sure `main` is green and holds the version bump (steps 1 and 2 above).
4. `git tag -s vX.Y.Z -m "Synthesis vX.Y.Z"` then `git push origin vX.Y.Z`.
5. Check github.com, tags: the tag shows "Verified". Then Actions, Release: approve the `release` environment prompts.
6. To rehearse without any risk, use the dry run below first; it needs no tag.

### Dry run (test installable artifacts without publishing)

Actions, Release, "Run workflow" (`workflow_dispatch`, any branch). It runs the CI, builds every target, packs archives, npm tarballs, SBOM and `SHA256SUMS`, and uploads them as the artifact `release-assets`. The tag checks, attestation, GitHub Release and npm steps are skipped, so nothing is published and no approval is needed. Download `release-assets`, unzip it, and test: unzip `synthesis-<version>-x86_64-pc-windows-msvc.zip` and run `synt.exe doctor`, or install the npm tarballs: `npm install -g ./devonchain101-synthesis-win32-x64-<version>.tgz ./devonchain101-synthesis-<version>.tgz` (platform tarball first). Dry-run artifacts have no attestation, so `install.sh`/`install.ps1` cannot verify them; use `SYNTHESIS_DOWNLOAD_BASE=file:///path SYNTHESIS_INSECURE_DOWNLOAD_BASE_I_KNOW=1 SYNTHESIS_SKIP_ATTESTATION=1 SYNTHESIS_VERSION=<version> sh scripts/install.sh` on macOS/Linux.

## Secrets and variables

No private signing secrets are used. Releases are signed through GitHub: Sigstore build-provenance attestations (`actions/attest-build-provenance`, verified by the installers with `gh attestation verify`) and npm provenance. Binaries are not Apple-notarized or Authenticode-signed (macOS Gatekeeper and Windows SmartScreen may warn on first run).

## npm

Packages: `@devonchain101/synthesis` (command `synt`) plus `@devonchain101/synthesis-<darwin-arm64|darwin-x64|linux-x64|linux-arm64|linux-x64-musl|win32-x64|win32-arm64>` as `optionalDependencies`, generated by `scripts/package-npm.mjs` (license, `publishConfig` access public + provenance true included). `npm-publish` publishes only when one of these exists, otherwise it logs a skip and the release still succeeds:

- Preferred, no stored secret: npm trusted publishing. For each of the 8 packages on npmjs.com (Settings, Trusted Publisher): GitHub Actions, organization/user `DevOnChain101`, repository `nemo-acd`, workflow `release.yml`, environment `release`. Then set the repository variable `SYNTHESIS_NPM_TRUSTED_PUBLISHING=true`. npm requires the package to exist first: publish a first version once by hand with a token, or use the token route for the first release.
- Fallback: environment secret `NPM_TOKEN` (granular access token, publish-only, scope `@devonchain101`) in the `release` environment. Provenance is still generated.

## Repository slug and the rename

The product is Synthesis but the GitHub repository is still `DevOnChain101/nemo-acd` and CI/release keep running there. When the new repository exists, change the slug in: `packaging/REPO` (single source), `scripts/install.sh` and `scripts/install.ps1` (built-in default, must equal `packaging/REPO`), `packaging/homebrew/synthesis.rb.tmpl`, `packaging/winget/Synthesis.installer.yaml.tmpl`, `packaging/winget/Synthesis.locale.en-US.yaml.tmpl`, `packaging/npm/bin/synt.js` (error text), `README.md`, `docs/INSTALL.md`, `docs/RELEASE.md`, `docs/STATUS.md`, `CLAUDE.md`; then run `node scripts/check-placeholders.mjs --expect <owner>/<repo>` and `node scripts/check-no-legacy-name.mjs` (the slug is allowed by `scripts/nemo-leftovers.allow`; delete those entries afterwards). Re-register npm trusted publishers for the new repository and the new package names (`@devonchain101/synthesis`, `@devonchain101/synthesis-<os>-<arch>[-musl]`).

Repository variables were renamed with the product: set `SYNTHESIS_ALLOW_ANY_VERIFIED_TAGGER` / `SYNTHESIS_NPM_TRUSTED_PUBLISHING` (the old `NEMO_*` names are no longer read by the workflows). App icons for a future installer or Start menu entry (the Windows `.ico`; a macOS `.icns` still has to be generated from the 1024 px PNG) are in `design/01-brand/app-icons/`; the current zip/portable installers have no icon.

## One-time owner setup (only the repository owner can do these; no script does them for you)

Decisions already in the repo: slug `DevOnChain101/nemo-acd` (`packaging/REPO`, checked by `node scripts/check-placeholders.mjs`), license Apache-2.0, npm scope `@devonchain101`.

Checklist, Settings of `DevOnChain101/nemo-acd` on github.com:

- [ ] Account, SSH and GPG keys: register your tag-signing key as a **Signing key** and configure git (exact commands: "Owner steps to create a release tag").
- [ ] Settings, Environments, New environment `release`: tick "Required reviewers" and add yourself (sole owner: do NOT tick "Prevent self-review", or nobody can approve); "Deployment branches and tags": select "Selected branches and tags", add tag pattern `v*`. Add the `NPM_TOKEN` environment secret only if you take the token route.
- [ ] Settings, Rules, Rulesets, New tag ruleset: target tags matching `v*`, enforcement Active, rules "Restrict creations" (bypass: yourself only), "Restrict updates", "Restrict deletions".
- [ ] Settings, Rules, Rulesets (or Branches), protect `main`: "Require a pull request" (optional for a sole owner), "Require status checks to pass" with `ci-ok`, "Block force pushes".
- [ ] Settings, General, Releases: tick "Enable release immutability" (published release and tag can no longer be changed).
- [ ] Settings, Actions, General: "Workflow permissions" = read repository contents (read-only); require approval for workflows from outside collaborators.
- [ ] Settings, Code security: enable private vulnerability reporting; keep artifact attestations available (public repo, or a plan that supports them).
- [ ] Settings, Secrets and variables, Actions, Variables: leave `SYNTHESIS_ALLOW_ANY_VERIFIED_TAGGER` unset. Set `SYNTHESIS_NPM_TRUSTED_PUBLISHING=true` only after the npm item below.
- [ ] npmjs.com: create/own the `@devonchain101` scope, then enable trusted publishing for the 8 packages (section npm), or create the `NPM_TOKEN`.
- [ ] Run the Release workflow once by hand (Actions, Release, Run workflow) as a dry run and test the Windows package from `release-assets` before the first tag.
- [ ] Runner labels used: `ubuntu-22.04-arm` and `windows-11-arm` (free for public repos only); `macos-latest` builds both darwin targets. Private repos need paid or self-hosted runners.
- [ ] Linux bwrap on `ubuntu-24.04`: the CI sandbox job runs `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`; without it bubblewrap fails with "setting up uid map: Permission denied". The job also fails (instead of silently skipping) if the runner kernel lacks Landlock.

## Action pinning

Every third-party `uses:` in `ci.yml` and `release.yml` is pinned to a full commit SHA with the tag in a trailing comment. Each SHA was resolved on 2026-10-27 from the GitHub API (`GET /repos/<owner>/<repo>/git/ref/tags/<major-tag>`, annotated tags dereferenced), not from memory; I did NOT audit the action source at those commits. The major tags move, so the SHAs are whatever the tag pointed at that day.

| Action | Tag | Pinned SHA |
|---|---|---|
| actions/checkout | v7.0.1 | 3d3c42e5aac5ba805825da76410c181273ba90b1 |
| actions/setup-node | v7.1.0 | 949feb2413d6458794dcd2491c4babbbce0c15c1 |
| actions/setup-python | v7.1.0 | 6d4fd22b20465ccc0f7354476588e8df7c9ca0e0 |
| actions/upload-artifact | v7.0.2 | cf430e030ddbb5b0abf93d22962f4752f3646cd9 |
| actions/download-artifact | v8.0.2 | 9000827ccba6bdab643e8b6fd33ac0654aef8333 |
| actions/attest-build-provenance | v2 | e8998f949152b193b063cb0ec769d69d929409be |
| Swatinem/rust-cache | v2 | d1527c8f67ef9af357290475d4af4b0fab8f12e0 (CI only; removed from release builds) |
| browser-actions/setup-chrome | v2.2.0 | 48ad923757ca74d66703209fe939badbdf80f2f4 (`chrome-version: stable`; pin a concrete version for reproducibility) |
| EmbarkStudios/cargo-deny-action | v2 | 3c6349835b2b7b196a839186cb8b78e02f7b5f25 |
| taiki-e/install-action | v2 | 5488ac5a951645ce95062214902ac4b6bc1a1037 |
| raven-actions/actionlint | v2 | 3d39aea434753780c3b3d4a1a31c854b4dbf49d7 |

Node 20 deprecation (bumped 2026-11-01): `checkout`, `setup-node`, `setup-python`, `upload-artifact`, `download-artifact` and `setup-chrome` were bumped to the latest release (see the table); the SHAs came from `releases/latest` and `git/ref/tags/<tag>` (annotated tags dereferenced) of the public GitHub API, and for each the `action.yml` at that SHA says `using: node24` (checked; the inputs the workflows pass exist there). `Swatinem/rust-cache` v2.9.3 was already pinned and is node24; `cargo-deny-action` (docker), `install-action` and `actionlint` (composite) run no Node. NOT bumped, still open: `actions/attest-build-provenance` v2 is composite but calls `actions/attest` v2.4.0, whose `action.yml` says `using: node20`; v4.2.2 (`4d101475d8b20a2381f78447822ac1eab6504dd8`) calls `actions/attest` v4.2.1 on node24, but it was left alone because bumping it touches the release attestation step (decide, then bump together with a dry-run of Release). Not verified: no CI run on the new majors yet and `actionlint` was not run locally (not installed; the CI `actionlint` job covers it). Watch one CI run on all three OSes and one Release dry run; majors can need a newer runner version.

None are left unpinned. If one ever is (a new action, a manual edit), `release.yml` fails in `gate` via `scripts/check-action-pins.mjs`; ordinary CI does not. To resolve: `gh api repos/<owner>/<repo>/git/ref/tags/<tag> --jq .object` (if `.type` is `tag`, follow `.url` and take `.object.sha`), put `@<sha> # <tag>` in the workflow, rerun the script. `.github/dependabot.yml` (github-actions, cargo) proposes bumps weekly; review each. Dependabot has no npm entry because no npm manifest with dependencies exists.

Also unpinned by design: `rust-toolchain.toml` uses `channel = "stable"`; MSRV is checked separately at 1.89.

## Rollback

- Unpublished draft: delete the draft release and the tag (`git push --delete origin vX.Y.Z`).
- Published release with a bad binary: mark it pre-release or delete the release, publish a fixed patch version; do not move tags. Homebrew/winget: revert the PR or ship the fix version.
- npm: `npm deprecate @devonchain101/synthesis@X.Y.Z "reason"` (unpublish is only possible within 72 hours and discouraged); publish X.Y.Z+1. Users pin back with `npm i -g @devonchain101/synthesis@<good>`.
- Installers always take `SYNTHESIS_VERSION=<good>`.

## Validated locally (macOS x86_64) vs not

Validated: `scripts/test-check-release-tag.sh` (also run in ci.yml `shell-scripts`: empty list fails, missing file fails, flag `true` passes, flag not exactly `true` fails, listed email passes, unlisted fails even with the flag, token-header fetch); both workflow files parse as YAML; `cargo build --release --locked -p synthesis-cli`; `scripts/check-no-test-provider.sh` passes on the release binary and fails on a `test-provider` debug build; `scripts/package-npm.mjs` + `npm pack` + install into a temp dir + `nemo-acd --version` / `doctor`; `scripts/install.sh` against a local archive (success and `synt doctor`; refusals for bad checksum, http base, bad version, malformed repo; a stub `gh` for pass, wrong `--source-ref`, no `gh` (proceeds with a warning) and `SYNTHESIS_SKIP_ATTESTATION=1`); `scripts/check-release-tag.sh` against a local origin with a stub `gh` (pass; lightweight tag; tag/GITHUB_SHA mismatch; unverified; wrong tag name; API object sha differing; API ref differing; allowlist empty/match/miss); `check-action-pins.mjs` and `check-placeholders.mjs` pass and fail cases; both workflows parse as YAML (ruby); the release stamping loop; `cargo deny check` (after fixing the policy) and `cargo audit`; `cargo cyclonedx` output. Not validated: everything that runs on GitHub, including the new `workflow_dispatch` dry run, the removed signing steps and the `npm-publish` job (npm trusted publishing, `--provenance` from a tarball) (CI, release, the new gate job and its tag-signature API check, environment approvals, `permissions`, the split attest/release jobs, attestation, real `gh attestation verify` and its `--signer-workflow`/`--source-ref` flags, the real API fields `scripts/check-release-tag.sh` reads (`verification.verified`, `tag`, `object.*`, `tagger.email`), `gh release create --target` with an existing tag, SBOM job, drafting), all signing and notarization, Linux/Windows builds and the cross targets, musl, `install.ps1` (no pwsh available; reviewed only; it now requires PowerShell 7+), the `publish` job and `gh release view --json targetCommitish` for a draft created with `--target <sha>`, the `persist-credentials: false` + extraheader fetch on GitHub, shellcheck and actionlint (not installed here; ci.yml runs actionlint), Homebrew/winget templates, the Chrome action.
