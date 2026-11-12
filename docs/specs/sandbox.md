# Sandbox on WSL (draft)

Scope: the Linux engine (`synt run`, `synt engine --stdio`, the desktop TUI tab through WSL2) when it detects it runs inside WSL. Code: `crates/synthesis-platform/src/wsl.rs` (detection), `crates/synthesis-sandbox/src/wsl.rs` (controls), `crates/synthesis-terminal` (mode), `crates/synthesis-app/src/doctor.rs` (`synt doctor`). Owner decision: WSL is NOT isolation, so the sandbox blocks interop itself; users are not required to edit `/etc/wsl.conf`; `synt doctor` only warns.

## Threat
From inside WSL a command can start Windows programs (`cmd.exe`, `powershell.exe`) through the `WSLInterop` binfmt entry and the socket named by `WSL_INTEROP` (`/run/WSL/<pid>_interop`), find them on a `PATH` WSL extends with `/mnt/c/...`, and write every Windows drive under `/mnt/*` (drvfs / 9p).

## Detection
`synthesis_platform::wsl::WslHost::probe(read)` with an injected `read(path) -> Option<String>`: the kernel strings (`/proc/sys/kernel/osrelease`, `/proc/version`) say "microsoft" (WSL1 or WSL2) and "wsl2" (WSL2). Windows drives = `/mnt/*` plus `drvfs` / `9p aname=drvfs` mounts in `/proc/mounts`. The real host is probed once (`WslHost::real()`, Linux only). A WSL1 kernel is treated as WSL too (fail closed), and has no bubblewrap.

## Controls

| Control | Mechanism | Covers | Does not cover |
|---|---|---|---|
| Env scrub | `sandbox::wsl::scrub_env` in `build_env` (every backend) and `Env::child_env` (Bash, hooks, MCP stdio, skill scripts, verification, browser worker) | `WSL*` variables removed (child_env never passed them); `/mnt/*` and drvfs directories removed from `PATH`/`Path` | A program that already knows an absolute path such as `/mnt/c/Windows/System32/cmd.exe`: env is not a boundary |
| Interop socket | bubblewrap `--tmpfs /run` (existing) | `/run/WSL`, the socket and `/var/run` do not exist in the sandbox | A process started outside the sandbox, or one that already holds the socket fd |
| binfmt entry | none possible without root | Without the env var and the socket, executing a PE file fails | The entry stays registered in the kernel; doctor reports it |
| Windows drive writes | bubblewrap: `--ro-bind` of every drvfs mount, before the writable binds (plus the read-only root); `check_writable_roots` | Writes under `/mnt/*` are denied except a writable root strictly inside a drive that contains the cwd (the workspace); a drive root, `/mnt`, a mount point or a temp dir on a drive is refused (`InvalidPolicy`, never degraded) | Windows drives stay READABLE (e.g. `/mnt/c/Users/*/.ssh`); `deny_paths` only lists Linux home paths |
| Backend | Landlock is refused on WSL (`refuse_landlock`, not in `detect_chain`) | Landlock cannot block `connect()` to a unix socket path, so interop would stay reachable from a PE file in the workspace | n/a |
| Fail closed | `TerminalRuntime::sandboxed`: `BestEffort` behaves as `Enforce` on WSL | No bubblewrap (missing, userns blocked, WSL1) means commands are refused, in enforce and best-effort alike | `--sandbox off` (owner opt-in) runs unconfined and prints a loud warning once; a command the person typed with `!` runs unconfined as before |

## `synt doctor`
When WSL is detected: WSL2 vs WSL1; interop in `/etc/wsl.conf` and in the running kernel; `appendWindowsPath` and the number of Windows directories in `PATH` (a count, never the entries); automount root and options (`metadata`); Windows drives mounted read-write; bubblewrap works / Landlock ABI (not used); `synt` binary on `/mnt`. Warnings only, each with the exact `/etc/wsl.conf` line and `wsl --shutdown`. Names of variables only.

## Tests
Pure (all OSes): `synthesis-platform` `wsl::tests`, `synthesis-sandbox` `wsl::tests` and `linux::tests::bwrap_wsl_*`, `synthesis-config` `env::tests`, `synthesis-terminal` `wsl_mode_tests`, `synthesis-app` `doctor::tests::wsl_rows_*`. Linux integration: `crates/synthesis-sandbox/tests/wsl.rs` (real bubblewrap, fake interop env, fake drive; skipped when bubblewrap is unusable). Needs real WSL2: the `#[ignore]` test in the same file.

## Follow-ups
Desktop spec section 9 and ADR-015 (branches feat/engine-stdio, feat/wsl-launcher) must gain this text when they merge: the consequences are "the engine refuses commands on WSL without bubblewrap", "sandbox off on WSL warns", "Landlock is not used on WSL". Not done here: reads of Windows drives are not hidden; an install button for bubblewrap is a separate PR.
