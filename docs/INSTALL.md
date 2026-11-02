# Installing Synthesis

Synthesis is one native binary, `synt`. On Linux a helper, `synthesis-sandbox-exec`, must sit in the same directory (the Landlock sandbox backend looks for it next to `synt`). All methods below keep them together. Nothing needs sudo. UNVERIFIED on GitHub until the first release exists. Exercised locally against locally built artifacts: `install.sh` and npm on macOS x86_64, and on Linux x86_64 in clean `ubuntu:24.04`, `ubuntu:22.04` and `debian:12-slim` containers (binary built on `ubuntu:22.04`, glibc 2.35, packaged like `release.yml`; `install.sh` with a `file://` base, npm from the packed tarballs). Windows has not been run by the maintainers: use [WINDOWS-TEST.md](WINDOWS-TEST.md).

## 1. Install script (macOS, Linux)

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/DevOnChain101/nemo-acd/releases/latest/download/install.sh | sh
```

Needs `curl`, `tar` and `ca-certificates` (HTTPS to GitHub and to your provider); minimal container images such as `ubuntu:24.04` and `debian:12-slim` have none of the three beyond `tar` (`apt-get install -y curl ca-certificates`). Linux binaries need glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+).

Environment: `SYNT_VERSION` (must match `^[0-9A-Za-z.+-]+$`), `SYNT_REPO`, `SYNT_INSTALL_DIR` (default `~/.local/bin`), `SYNT_BIN_NAME` (default `synt`), `SYNT_SKIP_ATTESTATION=1` (accept a checksum-only install; the default requires `gh`), `SYNT_DOWNLOAD_BASE` (must be https; `SYNT_INSECURE_DOWNLOAD_BASE_I_KNOW=1` allows others, tests only).

Verification: all downloads use `curl --proto '=https' --tlsv1.2`. The archive's sha256 is checked against `SHA256SUMS`. When `gh` is installed, `SHA256SUMS` itself is checked with `gh attestation verify` (build provenance from `release.yml`, pinned to that workflow and the `refs/tags/v<version>` source ref; needs `gh auth login`) and a failure aborts. Without `gh` the installer prints a warning and continues with the checksum only (the checksum file then comes from the same server as the archive and proves integrity, not origin); `SYNT_SKIP_ATTESTATION=1` does the same even when `gh` exists. After installing it runs `synt doctor`. The `install.sh` you pipe into `sh` is not covered by this: download it, read it, and verify it with `gh attestation verify install.sh --repo DevOnChain101/nemo-acd`. Prefer downloading `install.sh` first and reading it. musl systems get the x86_64 musl build when published.

## 2. Windows

`install.ps1` from the release (needs PowerShell 7+ (`pwsh`); same variables and the same checksum/attestation behaviour; installs to `%LOCALAPPDATA%\Programs\Synthesis` and runs `synt doctor`). The installer only prints the PATH step; to make `synt` available in new terminals run once `[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';' + "$env:LOCALAPPDATA\Programs\Synthesis", 'User')` and open a new terminal. Binaries are not Authenticode-signed, so SmartScreen may warn ("Windows protected your PC": More info, Run anyway; for a downloaded zip you can `Unblock-File` the extracted `synt.exe`); release provenance is the GitHub attestation. Windows has no filesystem or network sandbox, so `synt run` defaults to `--sandbox best-effort` there (commands run unconfined, with a notice). To test a dry-run build (docs/RELEASE.md, Dry run) without a release, unzip the `synthesis-<version>-x86_64-pc-windows-msvc.zip` from the `release-assets` artifact and run `synt.exe doctor`.

## 3. npm (any OS with Node >= 18)

```sh
npm install -g @devonchain101/synthesis
synt --version
```

`@devonchain101/synthesis` is a small JS shim that resolves the platform package `@devonchain101/synthesis-<os>-<arch>` (an optional dependency holding the binary) and runs it, forwarding the exit code and SIGTERM/SIGHUP/SIGQUIT. SIGINT is not forwarded because the terminal already sends Ctrl-C to the child. Do not install with `--omit=optional`/`--no-optional`; the shim then reports that no native binary was found.

## 4. Manual

Download `synthesis-<version>-<target>.tar.gz|zip` and `SHA256SUMS` from the release, check `sha256sum -c --ignore-missing SHA256SUMS`, `gh attestation verify SHA256SUMS --repo DevOnChain101/nemo-acd` (authenticates the checksum file) and optionally the archive itself, extract, and put the contents of the directory on your PATH. Targets: `x86_64`/`aarch64` `apple-darwin`, `unknown-linux-gnu`, `pc-windows-msvc`; `x86_64-unknown-linux-musl` is experimental.

## 5. Homebrew / winget

Templates only: `packaging/homebrew/synthesis.rb.tmpl`, `packaging/winget/*.tmpl`. Not published. Fill `@VERSION@` and the `@SHA256_*@` values from the release `SHA256SUMS`.

## Requirements and checks

- git on PATH (repository identity, `--worktree`, verification; a plain `synt run` also worked without it in the clean containers); a Chrome-family browser only for browser features (`SYNT_BROWSER_PATH` to choose one; Synthesis never downloads a browser).
- Linux sandbox: the default `--sandbox enforce` needs `bubblewrap` (`bwrap`) with unprivileged user namespaces. Landlock alone (kernel 5.13+) cannot express the standard policy (read-only `.git/hooks` inside the writable workspace), so `synt doctor` shows `enforce_ready: no` and `synt run` refuses unless you pass `--sandbox best-effort`. On Ubuntu 24.04 bubblewrap needs `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` (or an AppArmor profile for bwrap).
- Windows: install Git for Windows. The `Bash` tool runs commands with its `bash.exe` (found in the usual Git install dirs or next to `git.exe` on PATH; `synt doctor` prints `shell: ... kind=Posix`). Without it commands fall back to `cmd.exe`, doctor says so, and they are never auto-approved.
- Containers: `bwrap` cannot create namespaces under Docker's default profile or on Docker Desktop (`bwrap: Creating new namespace failed`, or `loopback: Failed RTM_NEWADDR` with extra capabilities), so `--sandbox enforce` refuses. Start the container with `--privileged` (what the maintainers' Linux test used; doctor then shows `enforce_ready: yes`) or pass `--sandbox best-effort`.
- Run `synt doctor --cwd .` and read the sandbox section. Upgrading from the previous product name: doctor prints the steps (docs/MIGRATION.md).
- Keys: `SYNT_*` environment variables only (README, docs/specs/config.md). For a gateway or any OpenAI-compatible endpoint create `$SYNT_HOME/synt.json` (default `~/.synt/synt.json`; Windows `%USERPROFILE%\.synt\synt.json`). The key stays in an environment variable of your choice, named by `{env:NAME}`, so the file holds no secret:

  ```json
  {
    "$schema": "config.json/v1",
    "providers": {
      "mygw": {
        "npm": "@ai-sdk/openai-compatible",
        "options": { "baseURL": "https://gateway.example.com/v1", "apiKey": "{env:MY_GATEWAY_KEY}" },
        "models": { "my-model": { "capabilities": { "toolCalling": true } } }
      }
    },
    "model": "mygw/my-model"
  }
  ```

  `synt models` then lists `mygw/my-model` and `synt run --cwd . --prompt "..."` uses it (this exact file was run against a real gateway on macOS; the Linux containers used the same fields). `"discover": true` on the provider also lists the models the endpoint reports. A model price (`synt.pricing` in `settings.json`) is needed for `--max-cost-usd` and for answering peers.

## Uninstall

Delete the binary (and `synthesis-sandbox-exec`), or `npm rm -g @devonchain101/synthesis`. State lives in `$SYNT_HOME` (default `~/.synt`), which is shared with the Node CLI; remove it only if you want to drop sessions too.
