# Outcome orchestrator: Windows and WSL2 manual run (evidence for B8)

Status: runbook, written 2026-11-18 from the code at `dd12257a`. Nobody has run it yet. Related: [remote-backend spec, section 4 "Gate mechanism"](../specs/orchestrator-remaining-backend.md), [plan 3b and PR-12](../architecture/outcome-orchestration-plan.md), [smoke](outcome-orchestrator-smoke.md), [acceptance](outcome-orchestration-acceptance.md), [ADR-017](../architecture/adr/ADR-017-outcome-orchestration.md), [ADR-021](../architecture/adr/ADR-021-outcome-ui-protocol.md), [desktop spec, section 9 and the Outcomes view states](../specs/desktop.md).

## 1. What this verifies and why

Outcome orchestration was written and tested on macOS (and Linux in CI). The Windows named-pipe endpoint, the singleton lock, the HMAC handshake over a pipe, the "native Windows refuses writable attempts" rule, WSL2 reach and the Linux sandbox inside WSL2 are all marked unverified, and the process and endpoint tests are `cfg(unix)` only, so CI cannot catch a Windows defect there. B8 (flipping `ENABLE_GATE_OPEN`) waits for this run. What the run proves is an ownership and correctness boundary: one orchestrator per home, a client must hold `run/orch.key`, the stdio engine cannot queue a command, and an attempt inside the sandbox cannot read the key or the store. It is **not** a security boundary against a program that runs as you outside the sandbox (that program can read the key; the enable notice says so). A pass here means "behaves as documented on these machines", not "safe against the same user".

Do not run any of this against a repository you care about. Use scratch repos and a scratch home only.

## 2. Prerequisites

- Windows 10 22H2 or Windows 11, PowerShell 5.1 or 7, Git for Windows (Git Bash is optional, used in A9). WSL2 with one Ubuntu-like distro (`wsl.exe -l -v` shows VERSION 2), `sudo apt install bubblewrap git build-essential nodejs`.
- Two binaries from the same commit (the desktop's WSL link needs the exact same version in the distro):
  - Windows `synt.exe`: either `cargo build --release -p synthesis-cli` in a clone on Windows (output `target\release\synt.exe`), or the dry-run artifact `release-assets` from Actions, Release, "Run workflow" (unzip `synthesis-<version>-x86_64-pc-windows-msvc.zip`; see [RELEASE.md](../RELEASE.md), Dry run).
  - Linux `synt` for WSL: clone the repo inside the distro on the Linux filesystem (`~/synthesis`, NOT `/mnt/c`) and run `cargo build --release -p synthesis-cli` (output `target/release/synt`; put it on `PATH`).
  - Desktop app for Windows: the `desktop-x86_64-pc-windows-msvc` zip from the same CI run (`Synthesis.exe`), or `cargo build -p synthesis-desktop` on Windows.
- Rust toolchain on both sides (`rustup`), Node 20+ in WSL (red-team step B8).
- Scratch home variables. PowerShell (the home MUST be under `%USERPROFILE%`, see A3):

  ```powershell
  $env:SYNT_HOME = "$env:USERPROFILE\synt-smoke"
  New-Item -ItemType Directory -Force $env:SYNT_HOME | Out-Null
  ```

  WSL2 (short Linux path; a unix socket path is limited to about 100 bytes):

  ```sh
  export SYNT_HOME=/tmp/synt-smoke; mkdir -p $SYNT_HOME
  ```

- Every command prints something; "paste this back" means the full text (redact nothing needed: no secrets are involved; do not paste `orch.key` bytes, ever).

## 3. Checklist

Mark each step PASS, FAIL (differs from Expected) or UNEXPECTED (the docs do not say what should happen: record it). Where it says "record what you see" there is no documented expectation; do not judge it, copy it. Steps tagged `[G3]` etc. map to section 5.

### A. Native Windows (PowerShell, `synt.exe` on PATH)

**A1. Version and doctor.**

```powershell
synt --version; "exit=$LASTEXITCODE"
synt doctor; "exit=$LASTEXITCODE"
```

Expect: version line, exit 0. `doctor` prints an `orchestrator: off (default), no session store yet` line (the flag is off), and a `sandbox:` line. Record the `sandbox:` line and every `wsl:` line: Windows has no enforcing sandbox, so what you see is the documented-as-unverified part. Paste back: both outputs.

**A2. Off by default.**

```powershell
synt orchestrator --serve; "exit=$LASTEXITCODE"
Test-Path "$env:SYNT_HOME\run\orch.json"
```

Expect: `synt: error: outcomes are off (synt.orchestrator.enabled is false): not starting`, exit 2, `False`. Paste back: output. `[G3]`

**A3. Home outside the profile is refused.** (`platform_ok` on Windows is this check, not the writable-attempt refusal: the home must be under `%USERPROFILE%` so `run/` and `orch.key` inherit the profile ACL.) First turn the flag on:

```powershell
Set-Content -Encoding ascii "$env:SYNT_HOME\settings.json" '{"synt":{"orchestrator":{"enabled":true}}}'
$keep = $env:SYNT_HOME
$env:SYNT_HOME = "$env:SystemDrive\synt-outside"; New-Item -ItemType Directory -Force $env:SYNT_HOME | Out-Null
Set-Content -Encoding ascii "$env:SYNT_HOME\settings.json" '{"synt":{"orchestrator":{"enabled":true}}}'
synt orchestrator --serve; "exit=$LASTEXITCODE"
$env:SYNT_HOME = $keep
```

Expect: `synt: error: SYNT_HOME must be under your user profile on Windows`, exit 2. (If your profile is itself on the `C:` root-level path this still holds; if `%USERPROFILE%` is on another drive, adapt the outside path.) Paste back: output. `[G3]`

**A4. Start with the flag on.** In a second PowerShell window with the same `$env:SYNT_HOME` (or `Start-Process`), run:

```powershell
$p = Start-Process synt -ArgumentList 'orchestrator','--serve' -PassThru -NoNewWindow -RedirectStandardError "$env:USERPROFILE\orch-serve.err" -RedirectStandardOutput "$env:USERPROFILE\orch-serve.out"
Start-Sleep 2
Get-ChildItem "$env:SYNT_HOME\run"
Get-Content "$env:SYNT_HOME\run\orch.json"
Get-Content "$env:USERPROFILE\orch-serve.err"
```

Expect: the process keeps running (it stops by itself only after `idle_exit_minutes`, default 10, with no client). `run\` holds `orch.key`, `orch.lock`, `orch.json`. Per the code, `orch.json` is `{"endpoint":"\\\\.\\pipe\\synthesis-peer-<32 hex>","pid":<n>,"protocol":5}`: the endpoint must be a named pipe starting `\\.\pipe\synthesis-peer-` (a planted entry with any other name is ignored). Per the code `serve` does NOT refuse on native Windows; the WSL2 refusal lives in `synt outcome <mutating command>` (A7) and in attempt allocation. Record whether the process started or refused, and the stderr text (the "cycle: outcome orchestration is not enabled for this store" line is expected, as in the smoke). Also record who can read `run\`: `icacls "$env:SYNT_HOME\run"` and `icacls "$env:SYNT_HOME\run\orch.key"` (the docs say the profile ACL protects them; there is no documented expected ACL text). Paste back: all output. `[G4]`

**A5. A second orchestrator on the same home.**

```powershell
synt orchestrator --serve; "exit=$LASTEXITCODE"
Get-Content "$env:SYNT_HOME\run\orch.json"
```

Expect: `another orchestrator already serves this home`, exit 0, `orch.json` unchanged from A4 (same pipe name and pid). Paste back: output. `[G4]`

**A6. The stdio engine is read-only.**

```powershell
$req = '{"id":1,"method":"orchestrator.status"}','{"id":2,"method":"outcome.start","params":{"id":"11111111-1111-4111-8111-111111111111","idempotency_key":"k","surface":"cli"}}'
Set-Content -Encoding ascii "$env:USERPROFILE\req.jsonl" $req
Get-Content "$env:USERPROFILE\req.jsonl" | synt engine --stdio --no-cost-cap
```

Expect (same as smoke section 4): a `hello` line with `protocol` 5, a result for id 1 (`"enabled":true`, `schema` null or 4), and an error for id 2 saying commands are accepted only on the orchestrator endpoint. If the engine does not start on native Windows at all, record that text. Paste back: output. `[G4]`

**A7. `synt outcome` on native Windows.**

```powershell
synt outcome list; "exit=$LASTEXITCODE"
synt outcome enable --yes; "exit=$LASTEXITCODE"
synt outcome pause 11111111-1111-4111-8111-111111111111; "exit=$LASTEXITCODE"
```

Expect for the two mutating commands (`enable`, `pause`; everything except `contract`, `list`, `status`, `decisions`, `inspect`, `events` mutates): `synt: error: outcome attempts write files and need an OS sandbox: they run on macOS, Linux and WSL2, not on native Windows; open the folder in WSL2`, exit 2, nothing sent to the pipe. For `list` (a read): the docs do not say whether it connects over the pipe and how it ends before the store is enabled (it may start or attach to the orchestrator): record what you see, its exit code, and whether `$env:SYNT_HOME\run\orch.log` appeared. Paste back: output. `[G3]`

**A8. Kill it and let it come back.**

```powershell
$old = Get-Content "$env:SYNT_HOME\run\orch.json" -Raw
Stop-Process -Id $p.Id -Force
Start-Sleep 1
$p = Start-Process synt -ArgumentList 'orchestrator','--serve' -PassThru -NoNewWindow -RedirectStandardError "$env:USERPROFILE\orch-serve2.err"
Start-Sleep 2
$old; Get-Content "$env:SYNT_HOME\run\orch.json"; Get-Content "$env:USERPROFILE\orch-serve2.err"
```

Expect (smoke section 5, pipe form): the second start succeeds (the OS released the lock with the dead process), `orch.json` names a new pipe name and the new pid. Named pipes vanish with the process, so there is no socket file to delete. If the second start says "another orchestrator already serves this home", the lock was not released: FAIL, record. Paste back: both `orch.json` bodies and the stderr. `[G4]`

**A9. Git Bash quoting (only if you use Git Bash).** In Git Bash (MSYS), with `SYNT_HOME` set to the same folder written as `/c/Users/<you>/synt-smoke`:

```sh
printf '%s\n' '{"id":1,"method":"orchestrator.status"}' | synt engine --stdio --no-cost-cap
```

Expect the same `hello` and a result for id 1 as A6. Record whether MSYS rewrote the path or the JSON quoting broke anything. No documented expectation beyond that. Paste back: output. `[G5]`

**A10. Turn it off.** Set `"enabled": false` in `$env:SYNT_HOME\settings.json`:

```powershell
Set-Content -Encoding ascii "$env:SYNT_HOME\settings.json" '{"synt":{"orchestrator":{"enabled":false}}}'
Start-Sleep 3
$p.HasExited; Test-Path "$env:SYNT_HOME\run\orch.json"; Get-Content "$env:USERPROFILE\orch-serve2.err" -Tail 3
```

Expect: within about a second the process logs `outcomes were turned off: stopping`, exits (`True`), and `orch.json` is gone (`False`). If the process lingers, `Stop-Process -Id $p.Id -Force` and record. Paste back: output. `[G4]`

**A11. Desktop app, Windows native.** Start `Synthesis.exe` from a PowerShell where `$env:SYNT_HOME` is set. Set the flag back to true first. Open the second view (title bar icon "Outcomes" or key `2`) and the Settings view, then do the same with the flag false. Expect per the code (`gate()` checks the platform before the setting): the Outcomes tab shows the title "Outcomes need WSL2 on Windows" and the `WSL2_REFUSAL` text in both cases, never the "Outcomes are off" screen, and it never starts a process or touches `run\` (confirm `run\orch.json` is not created by the app). There is NO Settings row for `synt.orchestrator.enabled`: the only related row is Settings, Execution, WSL (`synt.desktop.wsl.enabled`). Record whether that toggle is enabled or locked and with which text ("Checking WSL...", "Windows only", "WSL check failed" or none), and a screenshot or the text of each state. Paste back: the texts and screenshots. `[G3]`

**A12. (Optional, 5 minutes) Pipe unit tests on Windows.** In the Windows clone: `cargo test -p synthesis-peers`. Expect green (this is what CI already runs; it exercises the same pipe handshake the orchestrator uses). Paste back: the last 5 lines. `[G4]`

### B. WSL2 (bash inside the distro, Linux `synt`, repo cloned under `~`)

**B1. Which kind of WSL.**

```sh
uname -r; cat /proc/version | head -1; wsl.exe -l -v 2>/dev/null | tr -d '\0'
df -T $SYNT_HOME $PWD | tail -2
```

Expect: kernel string mentions `microsoft` and `WSL2`; VERSION 2; `df -T` shows `ext4` (or another Linux type) for both, NOT `9p`/`drvfs`. Paste back: output.

**B2. Version and doctor, sandbox lines.**

```sh
synt --version; synt doctor; echo "exit=$?"
```

Expect: exit 0 (or the exit of a doctor with warnings: record it), a `wsl:` line `bubblewrap works (the only sandbox used on WSL; Landlock ... is not used ...)` and a `sandbox: ... enforce ready` line. If instead you see `bubblewrap does not work (...)` or `enforce NOT ready`, install or fix bubblewrap (`sudo apt install bubblewrap`; on Ubuntu 24.04 you may need `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`) and record both outputs: that is a result, not a failure of the runbook. Also record the `wsl:` lines about interop and `/mnt/c`. `[G1]`

**B3. Smoke sections 1 to 6** from [outcome-orchestrator-smoke.md](outcome-orchestrator-smoke.md), verbatim, with `SYNT_HOME=/tmp/synt-smoke`: (1) off refusal, exit 2; (2) start with the flag (`orch.key` mode 0600, `orch.lock`, `orch.json` naming a `.sock`, `run/` mode 0700); (3) second `--serve`: "another orchestrator already serves this home", exit 0; (4) stdio engine read-only; (5) `kill -9` the pid in `orch.json`, restart, new socket and pid, old socket file gone; (6) set `enabled` false: it stops and `orch.json` goes. Expected output is in the smoke doc. Add `stat -c '%a %n' $SYNT_HOME/run $SYNT_HOME/run/*` after step 2. Paste back: all outputs. `[G4]`

**B4. Smoke section 7** (drive an outcome by hand) in a scratch git repo under `~` (never `/mnt/c`), flag on:

Follow section 7 of the smoke doc exactly (`synt outcome enable` then `enable --yes`, `create`, `contract`, `approve`, `start`, `status`, `decisions`, `pause`, `resume`, `cancel`, `inspect`, `events`). Expected: as written there. Without a configured provider the worker attempt fails and the outcome blocks: expected. If `enable --yes` answers "the release gate is not passed", the build you run already contains the B1 gate const: stop here, record it, and see open question 1. Paste back: the terminal transcript. `[G3] [G4]`

**B5. Sandbox: an attempt cannot write outside its worktree or read the key or the store.** A real model is not needed: the red-team suite scripts a "worker" that obeys the attack and the real `synt` binary must contain it. In the repo clone in WSL:

```sh
cargo build --locked -p synthesis-cli --features synthesis-cli/test-provider
EVAL_SANDBOX=enforce node tests/redteam/run.mjs --out $HOME/redteam-out \
  --case evid-chain-key-read,evid-backups-read,orch-db-write-from-attempt,inj-readme-approve-decision,path-traversal-write,outside-abs-write
```

Expect: every selected case ends `blocked` (never `harm`, never `inconclusive`, never `skipped`) in every mode it runs; the process exits 0. The first four are the plan 3b item 1 cases (chain key, backups, store write, approve/resolve plus reading `orch.key`); the last two are writes outside the workspace. `inconclusive` means the attack did not run (usually bubblewrap is not working: go back to B2). Do NOT use this build for anything else (`test-provider` is not in release builds). Paste back: the summary table at the end of the output and `redteam-out` file list. `[G1] [G2]`

**B6. Automated process, cancel, kill and recovery tests on real WSL2** (smoke "automated tests" list; these are `cfg(unix)` and never ran on WSL):

```sh
cargo test -p synthesis-app --test orch_endpoint
cargo test -p synthesis-cli --features test-provider --test orchestrator_process
cargo test -p synthesis-cli --features test-provider --test outcome_cmds
cargo test -p synthesis-cli --features test-provider --test outcome_e2e   # 2 to 3 minutes
```

Expect: all green. A failure is a result: paste the failing test name and the first error. Paste back: the `test result:` line of each. `[G1] [G4]`

**B7. Cancel, kill and recovery by hand (with the scripted worker).** In B6 the e2e covers cancel (`outcome cancel` ends the whole attempt tree) and a `kill -9` matrix. By hand, only if B6 failed or you want to see it: start an outcome (B4), `pause`, `kill -9` the orchestrator pid from `orch.json`, then `synt outcome status ID` (a client starts a new orchestrator) and expect the outcome still `paused` with the same revision and no second worktree under the repo's worktree folder (`git -C <repo> worktree list`). Record what you see.

**B8. Desktop on Windows driving a WSL2 engine.** On Windows set (user settings, PowerShell, same scratch home as before; use WSL only here):

```powershell
$env:SYNT_HOME = "$env:USERPROFILE\synt-smoke"
Set-Content -Encoding ascii "$env:SYNT_HOME\settings.json" '{"synt":{"desktop":{"wsl":{"enabled":true,"distro":""}}}}'
& .\Synthesis.exe
```

In the app: Settings, Execution, WSL (record the toggle state), open a Linux-folder workspace (UNC `\\wsl.localhost\<distro>\home\<you>\repo`), send a trivial prompt, then run `/doctor`. Expect (desktop spec section 9): the engine starts through `C:\Windows\System32\wsl.exe -d <distro> --cd <linux path> --exec synt engine --stdio`; the `/doctor` output is the Linux one (bubblewrap works, `sandbox ... enforce ready`); nothing is created under `%USERPROFILE%\synt-smoke\synt.db`. Also run in PowerShell: `wsl.exe -l -v` and `wsl.exe --status` and record whether the output is UTF-16 garbage in your console (the app decodes it; the console is not under test). Record what happens with a workspace under `\\wsl.localhost` containing spaces or non-ASCII characters in the folder name (wsl.exe quoting). The Outcomes tab on Windows still shows the WSL2 text (A11); an in-WSL Outcomes link does not exist yet (open question 3). Paste back: screenshots of Settings, the `/doctor` text, the notes above. `[G3] [G5]`

### C. Cleanup, and what NOT to run

```powershell
Stop-Process -Name synt -Force -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force "$env:USERPROFILE\synt-smoke","$env:SystemDrive\synt-outside","$env:USERPROFILE\req.jsonl","$env:USERPROFILE\orch-serve*.err","$env:USERPROFILE\orch-serve*.out" -ErrorAction SilentlyContinue
```

```sh
pkill -x synt; rm -rf /tmp/synt-smoke ~/redteam-out
```

Do NOT: use your real `~/.synt` or `%USERPROFILE%\.synt` (always the scratch `SYNT_HOME`); run `synt outcome enable --yes` against a home with sessions you want to keep (it migrates the store to schema v5 after a backup, and older binaries then refuse that store); point `outcome create --cwd` at a real repository; run the test-provider build in daily use; put the scratch home or repo on `/mnt/c` in WSL (drvfs cannot protect `.git/hooks` or `.synt`; sessions refuse it); paste `orch.key`.

## 4. Results table and what to send back

Copy this table; fill it in.

| Step | Pass / Fail / Unexpected | Pasted output (or file name) |
|---|---|---|
| A1 | | |
| A2 | | |
| A3 | | |
| A4 | | |
| A5 | | |
| A6 | | |
| A7 | | |
| A8 | | |
| A9 (optional) | | |
| A10 | | |
| A11 | | |
| A12 (optional) | | |
| B1 | | |
| B2 | | |
| B3 | | |
| B4 | | |
| B5 | | |
| B6 | | |
| B7 (optional) | | |
| B8 | | |

Send this block with the table:

```
commit (git rev-parse HEAD of the build):
synt --version (Windows / WSL):
Windows build number (winver), WSL version (wsl.exe --version), distro, kernel (uname -r):
bwrap version (bwrap --version) and the doctor sandbox line:
steps FAIL or UNEXPECTED (ids):
anything that hung, crashed or needed a manual kill:
did the Windows app ever create run\orch.json by itself (A11): yes / no
```

## 5. How the results feed B8

The gate rows are those of [spec section 4](../specs/orchestrator-remaining-backend.md); "Remaining" there is what a row still needs.

| Gate evidence | Satisfied by | Pass means |
|---|---|---|
| `[G1]` Row 1: red-team cases `evid-chain-key-read`, `evid-backups-read`, `orch-db-write-from-attempt` are `enforced` and blocked, plus the SEC-01 case, on Linux/WSL2 | B5, B6, B2 (sandbox lines) | All six cases `blocked` on a real WSL2 distro; bubblewrap reported working; automated Linux tests green on WSL2 |
| `[G2]` Writes outside the worktree and key reads are denied (acceptance SEC-02/T-21 area; part of row 1) | B5 | `path-traversal-write`, `outside-abs-write` and the key-read cases blocked |
| `[G3]` Row 3: native Windows refuses writable attempts, every surface shows the WSL2 text | A2, A3, A7, A11, B4 (the same commands work in WSL2), B8 | `synt outcome <mutating>` exits 2 with the exact `WSL2_REFUSAL` text and nothing sent; the desktop shows it; the same commands run on WSL2 |
| `[G4]` Risk line "Windows named pipe and WSL reach remain unverified": pipe endpoint, `orch.lock`, handshake, kill and restart, read-only stdio engine, turn-off | A4 to A8, A10, A12, B3, B4, B6, B7 | Pipe named `\\.\pipe\synthesis-peer-...`, second serve detected, restart after `Stop-Process -Force`, stdio engine read-only, WSL2 smoke 1 to 7 as documented |
| `[G5]` `wsl.exe` output decoding and quoting, Git Bash quoting, the desktop WSL launcher | A9, B8 | Distro listed and engine started via `System32\wsl.exe`; no path or quoting defect recorded |

Rows 2, 4, 5, 6 and 7 and the SEC-01 `cases.mjs` entry are not touched by this run. B8 still needs, besides a clean run of this checklist: the B1 PR (the `ENABLE_GATE_OPEN` const, the verbatim enable disclosure and confirmation, the `inj-readme-approve-decision` case, the explicit "plain-SQL evidence never passes" test); the `invisible()` set merged (HANDOFF item, `feat/orchestrator-followups`); the macOS and Linux `redteam` and `sandbox-linux` jobs green with those three cases `enforced` on the release commit; owner sign-off; and a PR body that lists the evidence per row (supervisor review, spec section 5 table). Failures or unexpected results here go back as bugs before B8; they do not get waived in the PR body.

## 6. Open questions (answer from what you record)

1. `ENABLE_GATE_OPEN` is not in the code at `dd12257a` (B1 is not merged), so `outcome enable --yes` is not gated in this build and B4 can run. A build with B1 refuses with "the release gate is not passed" and makes B4 impossible; which build is the gate evidence run on (pre-B1 for the run, or B1 with a local `true`)?
2. The brief of this run expected native Windows `synt orchestrator --serve` with the flag on to refuse writable attempts. In the code `serve` only checks that `SYNT_HOME` is under `%USERPROFILE%`; the WSL2 refusal is in `synt outcome` mutating commands, in attempt allocation (`AllocError::NativeWindows`) and in the desktop view. Is a running orchestrator on native Windows (reads only, never an attempt) intended, or should `serve` refuse?
3. The desktop's Outcomes tab on Windows always shows the WSL2 screen, even with the WSL setting on; there is no in-WSL Outcomes link yet (`Engine::start_remote` is the named future seam). Confirm the target for B8: is "Windows app shows the refusal" enough, or must the app drive a WSL2 orchestrator?
4. No `synt.orchestrator.enabled` toggle exists in the desktop Settings; A11 and B8 only record the WSL toggle. Is one planned before B8 (the checklist item "settings toggle")?
5. What `synt outcome list` (a read) should do on native Windows with the flag on and an un-migrated store (A7), and what ACL `run\` and `orch.key` should show (A4): not defined in the docs.
6. No Windows pass criterion exists for `synt doctor`'s `sandbox:` line (A1); Windows is documented as "no confinement, best-effort".
