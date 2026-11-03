# Desktop control (D1 + D2)

Lets a Synthesis agent drive the **desktop app it is building** so it can debug its own work:
launch the app, read its UI through the OS accessibility tree, act on it, take a window screenshot,
read its logs, fix, and verify again. Off by default; user settings only.

Status: implemented and tested against a **fake** `agent-desktop`. The real binary, real OS
accessibility permissions and real windows have not been run by anyone yet. The owner does that in
the D0 spike below. Until the pin (`crates/synthesis-desktop/src/pin.rs`) is filled with real values
the tools refuse to run unless the user sets `synt.desktop.allow_unpinned: true`.

## How it works

- Crate `crates/synthesis-desktop`: a `DesktopRuntime` per session (created on the first desktop
  call, dropped with the session). It runs the pinned [`agent-desktop`](https://github.com/vercel-labs/agent-desktop)
  CLI (Apache-2.0) one command at a time as a child process and parses its JSON envelope
  (`{version, ok, command, data | error}`).
- Tools in `crates/synthesis-tools/src/desktop.rs`, registered only by `Registry::with_desktop`,
  which `synt` calls only when `synt.desktop.enabled` is true in the user settings.
- Synthesis starts the app itself through the terminal runtime (own process group, stdout and stderr
  in a log artifact, same sandbox policy as `terminal_spawn`), so it knows the app's pid and process
  tree. Every `agent-desktop` call is scoped by `--window-id`, chosen from `list-windows` filtered to
  windows whose owner pid is in that tree. Never `--app <name>`, never a display, never `--headed`.
- Headless semantic actions only: no pointer, no coordinates, no focus stealing, no clipboard.

## Tools

| Tool | Class | What |
|---|---|---|
| `desktop_launch {target, args?, cwd?}` | External | Starts the app. `target` is an ABSOLUTE executable path. Returns `{app_id, pid, origin, log_path}` and the windows. |
| `desktop_snapshot {app_id, window_id?, root_ref?, skeleton?, max_nodes?}` | Read | Compact text tree, refs `@<snapshot>:eN`, wrapped in `<untrusted_ui_content>`. |
| `desktop_screenshot {app_id, window_id?}` | Read | PNG of that one window, saved under `<artifacts>/desktop/shots/`. |
| `desktop_logs {app_id, since?}` | Read | stdout and stderr of the app since a cursor, redacted, wrapped as untrusted. |
| `desktop_close {app_id}` | Read | Kills the app's process group. |
| `desktop_act {app_id, ref?, action, value?, window_id?}` | External | `click double_click right_click set_value type clear select toggle check uncheck expand collapse focus scroll_to press` -> `{delivered, verified, delivery}`. |
| `desktop_wait {app_id, predicate, text?, ref?, window_id?, timeout_ms?}` | Read | `text` (polls the app's window), `element` (agent-desktop `wait --element`), `window_gone`. |

`desktop_act` returns agent-desktop's own `disposition.delivery`. `delivered` is true only for
`delivered_unverified` / `delivered_verified`; `verified` only for `delivered_verified`. On an error
the message says whether the input may already have been delivered (`disposition.retry`); only an
explicit `safe` means a retry cannot repeat the effect.

**Secrets are not supported in `desktop_act`.** `synt.browser.secrets` are bound to web origins; an
app has no origin, so there is no binding that would not weaken that mechanism. Type non-secret
values only. (Decision of this task; revisit only with a per-app binding design.)

Not implemented: attaching to a running app, launching by bundle id or program name, `--headed`,
raw coordinates, clipboard commands, multi-display capture, sessions of agent-desktop (`--session`;
isolation is by a private `AGENT_DESKTOP_HOME`).

## Owner policy (2026-11-03) as tightened by the security review

**Narrowing of owner policy (a).** The owner asked for apps launched from the project to be allowed
in every permission mode. A project app is code the agent may have just written, so launching it is
code execution that must not bypass the Bash controls. Decision (safer option chosen): a project
launch gets exactly the verdict the same command line would get in Bash (deny/ask rules, protected
paths, critical-command checks, the auto-mode classifier, the secret scan) and so asks in `default`
and `acceptEdits`, is classified in `auto`, is denied in `dontAsk` unless a Bash allow rule covers
it, and passes in `bypassPermissions` like any Bash command. **Plan mode refuses it** (plan must not
execute code). A user who wants no dialog adds a Bash allow rule for the binary. Driving an app that is
already running (snapshot, act, wait, screenshot, logs, close) needs no new dialog, except in plan mode.

| Rule | Enforced in |
|---|---|
| (a) Project app: the same verdict as the command line in Bash; plan refuses; interpreter-style arguments, shells, scripts and copies of system interpreters refused. | `synthesis-permissions/src/lib.rs` `decide_desktop` (builds `launch_command_line` and runs the Bash verdict), `desktop.rs` (`gate`, `refused_target`, `refused_arg`); tool layer repeats name, argument, native-executable and content checks in `synthesis-tools/src/desktop.rs` |
| Sandbox: the app runs only when the terminal runtime is in `enforce` mode with a policy for the session, and only if the process really reports a sandbox; best-effort and off refuse. The app has the same confinement as `terminal_spawn`. | `synthesis-tools/src/desktop.rs::launch_body` |
| (b) Any other app (path outside the workspace): dialog per launch, default mode only, every other mode denied; no allow rule, "always" or mode pre-approves it; the dialog shows the WHOLE command line (target, every argument, cwd, environment additions: none), stripped of control, bidi and zero-width characters, with cuts marked. The approved app is cut off again if the mode leaves `default`. | `decide_desktop` (`Verdict.hard`, no suggestions), `desktop::describe_launch` used by `exec.rs::approve`, tool layer `mode_ok` on every call |
| (c) Peers and sub-agents never get desktop tools. | tool filters in `synthesis-harness/src/comm.rs` and `peer.rs`; refusal before any rule or dialog in `exec.rs::authorize`; tool layer refuses `ctx.sub_agent` |
| (d) Disabled by default; `synt.desktop.*` from the user settings file only; the managed file can force `enabled: false` for good (nothing re-enables it); other layers get a diagnostic. | `synthesis-settings/src/lib.rs` (`desktop_managed_off`), `synthesis-app/src/desktop.rs`; the tools are not registered otherwise |
| (e) Hard-deny list (exact case-folded bundle id / executable name with `.exe .app .lnk .appimage .bat .cmd` stripped; for a path only the FILE NAME and `.app` bundle components, never parent directories; `keepass*` by prefix; localized macOS names of Passwords and Keychain Access in de/fr/es/ja/vi are in the list for window titles): password managers, authenticators and crypto wallets (Raivo, 2FAS, Keeper, Exodus, Electrum, Ledger, Trezor, 1Password, Bitwarden, KeePass*, Keychain Access, Passwords, Dashlane, LastPass, Authy, Proton Pass, NordPass, Enpass, Authenticator, seahorse, KWalletManager), system settings and control panel, terminals and shells (Terminal, iTerm2, Windows Terminal, cmd, PowerShell, conhost, Alacritty, kitty, WezTerm, Ghostty, Hyper, Warp, gnome-terminal, Konsole, xterm, tmux ...), script runners (Script Editor, Automator, Shortcuts), task managers, remote-desktop apps and Screen Sharing, Console, Disk Utility, putty/mintty/wsl/regedit/mmc/consent, the desktop shell (Finder, Explorer, Dock, loginwindow, SecurityAgent), and browsers including Tor Browser. The OS-reported app name AND window title of every window are matched too; the terminal/IDE hosting synt (process ancestry) is never offered. | `synthesis-permissions/src/desktop.rs::hard_denied`; tool layer (target and real path); `synthesis-desktop/src/runtime.rs` (`register_app`, `windows`) |
| (f) UI text is untrusted; secret-shaped text redacted; password roles replaced; typed values redacted in events, hooks, prompts and history (`redact_input`); screenshots are one window; artifacts 0700. | `untrusted.rs`, `text.rs`, `snapshot.rs`, `runtime.rs::private_dir`, `synthesis-tools/src/lib.rs::redact_input` |
| (g) Audit events `desktop.launch`, `desktop.act` (no value), `desktop.screenshot`, `desktop.denied`. | `synthesis-events`, `synthesis-app/src/desktop.rs::AuditHost`, `exec.rs::desktop_denied` |
| (h) Kill switch: see "Kill switch" below. | |

Notes on classification: "inside the workspace" is `WorkspaceContext::resolve` (working directory
and additional directories, after resolving links). The artifact directory is not project code.

## Release blocker: the sandbox is not a boundary for GUI apps

Do NOT widen any sandbox profile to make GUI apps work. A GUI-capable profile (WindowServer and
LaunchServices mach services on macOS) is a known blocker until the points below are resolved;
until then GUI launch may simply not work, which is the safer outcome.

- **macOS Accessibility is inherited.** The grant goes to the terminal app that runs `synt`, so every
  child, including an app the agent wrote, inherits it. A GUI-capable profile would let an
  agent-written app drive other apps (Terminal, password managers) through the Accessibility API
  or by posting events. A signed helper `.app` that holds the grant instead is REQUIRED before any
  widening of MACH_SERVICES.
- **Seatbelt is documented as NOT a security boundary** (`Weak`). `desktop_launch` on such a backend
  is best-effort confinement, and `synt doctor` prints that warning whenever desktop control is enabled.
- **X11 has no per-client isolation**: any client that can reach `DISPLAY` can read and drive every
  other window. Wayland-native apps need the runtime directory and bus (not passed).
- D0 must test that a launched app can NOT call Accessibility or post events to another app.

## Kill switch

There is no separate `synt desktop stop` command (a second process would have to find the first
one's app by pid, which is the stale-pid problem this design avoids). The kill path is the session
one: `Registry::close_session` -> `TerminalRuntime::close_session`, called when a `synt run` or the
TUI ends, kills the process group of every app and its children; `desktop_close` does the same for
one app. A test starts an app with a child and checks both are dead after `close_session`. Ctrl-C
reaches the same cleanup in `run.rs` and `tui.rs` by code reading; it has not been exercised on a
real terminal. In-flight `agent-desktop` calls die with their future. NOT handled: the macOS command
host `agent-desktop` may leave running (idles out after 15 minutes).

## Doctor

`synt doctor` prints, when desktop control is enabled, the sandbox warning above. Its staged copy of the binary lives in a random private directory (exclusive create, 0700) that is removed afterwards, and it never runs a binary that is not pin-verified.

## Settings (user `settings.json` only)

```json
{ "synt": { "desktop": {
    "enabled": true,
    "binary": "/abs/path/to/agent-desktop",
    "allow_unpinned": false,
    "allow_press": false,
    "actions_per_second": 2,
    "max_actions": 300
} } }
```

- `enabled`: exactly `true` turns the tools on. Default off. A managed `enabled: false` wins over every user setting.
- `binary`: absolute path. Default: `agent-desktop` (`.exe`) next to the `synt` executable.
- `allow_press`: let `desktop_act` send key combos (`press`). Default off: whether agent-desktop posts keys to the target process or to the frontmost app is unverified (D0). When on, only plain named keys (no function keys) and ctrl/cmd/alt (+shift) with one letter or digit pass; cmd+space/tab/q/h/m, cmd+shift+digit, alt+f4, ctrl+alt+delete, any win/super/meta combination and every two-modifier combination are refused.
- `allow_unpinned`: run a binary that does not match the pin. At your own risk; `synt doctor` says so.
- `actions_per_second` (0.1-10, default 2) paces `desktop_act`; `max_actions` (1-5000, default 300) is a hard cap per session.

## The pin

`crates/synthesis-desktop/src/pin.rs` holds the one pinned version and one sha256 per `os-arch`.
**All values are UNVERIFIED placeholders.** The binary is hashed (sha256) and its `version` command
must report the pinned version. A placeholder never matches. The binary is COPIED into the private state directory (`<artifacts>/desktop/bin`, 0700) and that copy is hashed and run, so nothing can swap the file between check and exec. A helper file the program expects beside itself (macOS) is not copied: D0 must show whether it works without one; if it does not, the owner decides how to ship it.

## Child process contract

- Environment cleared; only: `HOME USERPROFILE PATH TMPDIR TEMP TMP LANG LC_ALL XDG_RUNTIME_DIR XDG_SESSION_TYPE DISPLAY WAYLAND_DISPLAY XAUTHORITY DBUS_SESSION_BUS_ADDRESS SystemRoot` (the CLI only; the launched APP gets `DISPLAY` and nothing else, because the sandbox is not proven to deny the session bus / secret-service sockets, so Wayland-only and cookie-protected X sessions may not open windows: a D0 item), from the injected `Env`, plus `AGENT_DESKTOP_HOME=<artifacts>/desktop/state`. No API key or `SYNT_*` secret reaches it.
- 30 s per call (configurable in code), 4 MiB stdout cap (the process is killed past it), stderr discarded. Rejected screenshots are deleted and still counted. Caps per session: 300 actions at 2/s, 8 observation commands per second and 5000 in total, 40 screenshots and 64 MiB of screenshots.
- A reused pid is not the app: the process-table start time recorded at launch must exist (an app whose start time cannot be read is refused: fail closed) and must still match and the terminal runtime must still report the process alive, else the app's windows are dropped.
- Values for `type`, `set_value` and `select` must not start with `-` (the CLI could read them as options; `--` is not relied on because it is unverified).
- Envelope: major version `2` required; unknown fields ignored; the command in the answer must match the one asked; a missing `disposition` is never "safe".
- Every string from the UI tree, every window title and every agent-desktop error message is cleaned (control characters, length), redacted, and wrapped as untrusted text; the wrapper cannot be closed from inside.
- Refs the model did not receive from a snapshot of that app are refused before any process starts. A new whole-window snapshot supersedes older refs of that window; an error `STALE_REF` poisons the snapshot it came from.

## Threat model

| Threat | Mitigation | Residual |
|---|---|---|
| "Project app" used to run arbitrary code outside the Bash controls (a copied shell, a workspace script, `app -c ...`) | Same verdict as Bash for the whole command line; plan refuses; shells/interpreters refused by name, by arguments (`-c -e --eval -m ...`), by content (byte-for-byte copy of a system interpreter) and by file type (only Mach-O/ELF/PE, no scripts); sandbox must be `enforce` and the process must report a sandbox | A compiled app is still code: it runs confined by the session sandbox but can do what that sandbox allows. A modified shell binary that is not byte-identical and not named like a shell is not recognised. |
| Dialog spoofing (bidi, zero-width, Unicode tag characters, blank fillers, variation selectors, long arguments) | The dialog shows the target AND the executable it resolves to (a bundle's inner file, a link's target), every argument (a launch with more than 20 is refused, so none is hidden), cwd and the real environment (the session's allowlisted variables plus DISPLAY), sanitized, cuts marked | The list of invisible characters is a list |
| A bundle (`X.app`) hiding another program (inner symlink, symlinked `Contents/MacOS`, two executables) | The inner executable is canonicalized; it must be exactly one file that resolves INSIDE the bundle; hard-deny, interpreter, content, file-type and workspace checks run on that result, in the engine and the tool alike; the resolved path is what runs. On macOS, the bundle's `Info.plist` name and id are checked and an unreadable plist is refused. Project hardlinks are refused. | The size, mtime, sha256, device and inode are re-checked before spawn; a path-based exec still has a residual check-to-exec race. |
| A workspace link changing to an external executable after policy classification | The harness records the canonical executable, identity and engine origin before the approval prompt, and marks external approval only after a hard dialog is answered Allow. The tool requires matching call-scoped evidence even when the permission mode would allow an external app. | The executable is spawned from the resolved path; another process can still replace it in the final check-to-exec window. |
| Interpreter and argument tricks (`-lc`, `--require`, `--inspect`, `--remote-debugging-port`, `--user-data-dir`, script-file arguments, wrappers such as `xcrun`, `arch`, `caffeinate`, `nohup`, `sudo`, `flatpak`, `systemd-run`, `open`, `wsl`) | Wrappers and runtimes are refused as targets; the known dangerous argument spellings, script extensions and extensionless path arguments are refused; copies of the real interpreters and installed hard-denied apps found in usual locations are recognised by file identity or content | **Hints, not a guarantee**: a recompiled or modified copy, a nonstandard installation path, or an argument spelling not on the list is not recognised. The sandbox and the Bash verdict are the control. |
| Prompt injection through on-screen text | Wrapped as untrusted; refs only from our snapshots; sub-agents/peers have no tools | The model still reads the text |
| Agent controls a password manager, authenticator, system settings, terminal, browser, remote-desktop app or shell | Hard-deny list at three layers, matched on file name / bundle id / OS-reported app name / window title (never on parent directories) | Name-based: an unlisted app with the same function is not covered; a project binary named like a denied app (`arc`, `control`, `wt`...) is refused too |
| Drives another app than the one it launched | Window ids from `list-windows` filtered by the launched process tree and re-checked against a reused pid; no `--app`; ids that start with `-` or hold odd characters are dropped | An app that re-parents away from the tree has invisible windows: fail closed |
| Global keystrokes / shortcuts (cmd+space, cmd+q, alt+f4, ctrl+alt+delete, screenshot, lock, function keys) | `press` is OFF unless `synt.desktop.allow_press`; when on, only app-local combos pass an allowlist, checked before anything is counted or started | Whether `press --window-id` posts to the target process or the frontmost app is unverified (D0) |
| Option injection through a typed value (`--headed`, `-1`) | Values starting with `-` are refused | A value that must start with `-` cannot be typed |
| Untrusted or tampered `agent-desktop` | Pin + sha256 + version of a private staged copy that is the file that runs; env cleared; own process group; output cap; timeout | Pin is empty until the owner fills it; the macOS helper beside the binary is not copied |
| The launched app reaching the session bus, secret service or X cookie | The app gets `DISPLAY` only | Wayland-only desktops may not open windows until a safer design exists |
| Secret exposure on screen | Pattern redaction, password roles replaced, typed values redacted in events, hooks, prompts and history | **Screenshots cannot be redacted**; capped at 40 files / 64 MiB per session; 0700; never uploaded; never attached to the model by the app wiring |
| macOS grants Accessibility to the terminal app | `synt doctor` warns | A signed helper `.app` is the proper fix; not built |
| Runaway agent | 2 actions/s and 300 per session, 8 observations/s and 5000 per session, one command at a time, kill switch | |
| Leftover processes | Process group kill on session end / `desktop_close`; per-call children die with the call | The macOS command host is NOT killed |
| Organisation wants it off | Managed `synt.desktop.enabled: false` wins | |

## Limits

- GUI sessions only. No display (Linux without `DISPLAY`/`WAYLAND_DISPLAY`, macOS ssh shell) or WSL: tools return `desktop_unavailable`. macOS/Windows cannot be told apart from CI without an OS call, so a headless macOS/Windows runner fails at the first `agent-desktop` call instead.
- Launching requires `--sandbox enforce` and goes through the terminal runtime's sandbox. A GUI app under the macOS Seatbelt profile (mach-lookup is restricted) or the Linux sandboxes may start but not show a window; this is unverified (D0). The sandbox is not weakened for it: if it makes GUI apps unusable the answer is a reviewed, narrower profile, not `best-effort`.
- Windows desktop launch currently refuses both project and external apps: its sandbox cannot enforce this boundary, and stable standard-library file identity is unavailable. Fake-CLI tests still exercise the protocol. A future Windows backend must add an enforceable sandbox and file identity before removing this refusal.
- Process table: `ps -A -o pid=,ppid=` (macOS, Linux), PowerShell `Get-CimInstance Win32_Process` by absolute `System32` path (Windows; unverified).
- `list-windows` is accepted as a JSON array or `{windows: [...]}` (the reference shows an array; confirm at D0).
- Screenshot PNG is validated (magic, size 8 B-25 MiB) before it is reported.
- Tests use the fake CLI `crates/synthesis-desktop/src/bin/fake_agent_desktop.rs` (also built as a bin of `synthesis-tools` from the same source; never shipped, releases build `-p synthesis-cli` only). It doubles as the app under test (`__app`, `__sleep`), so the suites run on every OS; only tests that need Unix links or shell metacharacters are `cfg(unix)`. The Windows paths (PowerShell process table, job-object kill, `tasklist` in the tests) have not been run.

## D0 checklist for the owner (real machine, per OS)

Do these on a throwaway account with nothing private open. Use a disposable test app (any small GUI program).

1. Get a release of agent-desktop you trust (README: `npm install -g agent-desktop`, or a GitHub release archive). Note the version from `agent-desktop version`. Keep any helper beside the executable.
2. Compute the sha256 of the executable and put `(os-arch, sha256)` and the version in `crates/synthesis-desktop/src/pin.rs`. Then `synt doctor` must say `desktop pin: sha256 matches`.
3. Also check the security assumptions: `agent-desktop press --window-id <id> cmd+a` while ANOTHER app is frontmost (does it reach only the target window?); whether a staged COPY of the binary still works without its helper; whether a Seatbelt-confined app opens a window; whether `--` before positionals is accepted.
3. **macOS**: `agent-desktop permissions --guide`, grant Accessibility (and Screen Recording for screenshots) to the terminal app. Expected `agent-desktop permissions`: envelope `ok:true`, `data` with `accessibility`, `screen_recording`, `automation`. **Windows**: no grant for same-integrity apps. **Linux**: X11 or Wayland session with `at-spi2-core`; `DBUS_SESSION_BUS_ADDRESS` must reach the child (Synthesis passes it).
4. Run the app and check the contract Synthesis relies on:
   - `agent-desktop list-windows`: `data` is an array (or `{windows}`) of `{id, title, app_name, pid}`; **`pid` is the process that owns the window** (for the app you started, or a descendant).
   - `agent-desktop snapshot --window-id <id> --compact`: `data.window.id` equals the id, `data.snapshot_id`, `data.tree` with `ref_id` fields like `@s8f3k2p9:e8`.
   - `agent-desktop click @…`, `type @… "x"`, `press return --window-id <id>`: `data.disposition.delivery` present; confirm `press` reaches ONLY that window while another window is focused.
   - `agent-desktop screenshot --window-id <id> /tmp/x.png`: writes a PNG of that window only.
   - `agent-desktop wait --element @… --timeout 2000`: `--timeout` is milliseconds and the flag spelling is right (taken from the source, not run).
   - `agent-desktop version`: `data.version` is a string.
5. With `"synt": {"desktop": {"enabled": true, "binary": "…"}}` in the user settings, run `synt run` on a project that builds a GUI app and ask the agent to launch it, snapshot, click a button, wait, screenshot and read logs. Check `desktop.*` events in the event stream and that no value typed appears there.
6. Policy checks to run once by hand: auto mode launching a path outside the project is refused; default mode shows the dialog; launching a browser or a password manager is refused; a sub-agent has no `desktop_*` tools.
7. macOS only: does the Seatbelt sandbox let the app open a window? Does the command host survive `synt` exit (`ps aux | grep agent-desktop`) and can it be stopped safely?
8. Windows only: confirm the PowerShell process table call works and finds descendants; confirm job-object kill removes the app.
9. Record what differed from this document and fix the pin/flags accordingly.

## Third-party

`agent-desktop` (vercel-labs, Apache-2.0) is **not bundled**. If it is ever shipped with Synthesis,
add its LICENSE and attribution to `NOTICE` (a note is there) and to the release archives.

## Progress

- Done: permission policy and engine rules; `synthesis-desktop` runtime (envelope, pin, process
  tree, window filtering, snapshot and ref table, limiter, kill) with a fake CLI; tools; events;
  user-only settings; peer/sub-agent refusal; app wiring, audit sink, doctor lines; docs.
- Gates (re-run after the security fixes, see the report): fmt, clippy (+ test-provider), `cargo test --workspace` (1403 tests, 0 failed), `cargo test -p synthesis-cli --features test-provider` (172 passed), rustdoc `-D warnings`, `check-no-legacy-name`.
- Verified by tests (fake CLI only): the whole launch -> snapshot -> act -> wait -> screenshot ->
  logs -> close loop, every permission mode for project and external apps, hard-deny list, settings
  rejection, rate limit, timeout/oversize kill, process-tree kill, redaction.
- Unverified: everything that needs the real binary, real OS permissions or real windows (see D0).
