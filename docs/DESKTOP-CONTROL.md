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

## Owner policy (2026-11-03) and where it is enforced

| Rule | Enforced in |
|---|---|
| (a) Apps launched by Synthesis from inside the workspace are allowed in EVERY permission mode, no per-call dialog. An explicit `ask` rule on a desktop tool still prompts (dontAsk: deny); a `deny` rule still wins. | `synthesis-permissions/src/lib.rs` `decide_desktop` (before bypass/plan/allow rules); `desktop.rs::gate`; repeated in `synthesis-tools/src/desktop.rs::mode_ok` |
| (b) Any other app (path outside the workspace) needs a dialog per launch, in the default mode only. In acceptEdits, plan, auto, dontAsk and bypassPermissions it is refused with a message. No allow rule, "always allow" or mode pre-approves it (`Verdict.hard`, no suggestions). The approved app is cut off again if the mode leaves `default`. | same functions; tool layer re-checks `ctx.permission_mode` on every call |
| (c) Peers and sub-agents never get desktop tools. | tool list filters in `synthesis-harness/src/comm.rs` and `peer.rs`; refusal before any rule or dialog in `exec.rs::authorize`; tool layer refuses `ctx.sub_agent` |
| (d) Disabled by default; `synt.desktop.*` is read from the user settings file only; project, local, managed and `--settings` layers get a diagnostic. | `synthesis-settings/src/lib.rs`; `synthesis-app/src/desktop.rs`; tools not registered otherwise |
| (e) Hard-deny list, exact case-folded bundle id / executable name (and every `.app` path component), `keepass*` by prefix: 1Password, Bitwarden, KeePass*, Keychain Access, Dashlane, LastPass; System Settings/Preferences, `control`, `credwiz`; Finder, Explorer, Dock, loginwindow; web browsers (Chrome, Chromium, Firefox, Safari, Edge, Brave, Arc, Opera, Vivaldi, IE). No approval lifts it. The terminal/IDE hosting synt (process ancestry) is never offered as a window. | `synthesis-permissions/src/desktop.rs::hard_denied` (permission layer, on the target), `synthesis-tools/src/desktop.rs` (on the target AND its real path, so a link cannot launder a name), `synthesis-desktop/src/runtime.rs` (`register_app`, and every window's `app_name`; ancestors of the synt process are hidden) |
| (f) UI text is untrusted data; secret-shaped strings are redacted; screenshots are one window; artifacts are 0700 and not uploaded. | `untrusted.rs`, `text.rs` (uses `synthesis_browser::redact_text`), `snapshot.rs`, `runtime.rs::private_dir`; password-role values are replaced |
| (g) Audit events `desktop.launch`, `desktop.act` (no value), `desktop.screenshot`, `desktop.denied`. | `synthesis-events`, `synthesis-app/src/desktop.rs::AuditHost`, `exec.rs::desktop_denied`; spec in `docs/specs/events.md` |
| (h) Kill switch: session end and Ctrl-C kill the launched app's process tree (terminal runtime `close_session`) and any in-flight `agent-desktop` call (process group / job object, kill on drop). | `native.rs::Shared::close_session`, `runtime.rs::run` |

Notes on the classification: "inside the workspace" is `WorkspaceContext::resolve` (working
directory and additional directories, after resolving links). The artifact directory is not treated
as project code.

## Settings (user `settings.json` only)

```json
{ "synt": { "desktop": {
    "enabled": true,
    "binary": "/abs/path/to/agent-desktop",
    "allow_unpinned": false,
    "actions_per_second": 2,
    "max_actions": 300
} } }
```

- `enabled`: exactly `true` turns the tools on. Default off.
- `binary`: absolute path. Default: `agent-desktop` (`.exe`) next to the `synt` executable.
- `allow_unpinned`: run a binary that does not match the pin. At your own risk; `synt doctor` says so.
- `actions_per_second` (0.1-10, default 2) paces `desktop_act`; `max_actions` (1-5000, default 300) is a hard cap per session.

## The pin

`crates/synthesis-desktop/src/pin.rs` holds the one pinned version and one sha256 per `os-arch`.
**All values are UNVERIFIED placeholders.** The binary is hashed (sha256) and its `version` command
must report the pinned version. A placeholder never matches. Hashing is done in place just before
the first run; the window between hash and exec is not closed (the binary's directory must be
writable only by you).

## Child process contract

- Environment cleared; only: `HOME USERPROFILE PATH TMPDIR TEMP TMP LANG LC_ALL XDG_RUNTIME_DIR XDG_SESSION_TYPE DISPLAY WAYLAND_DISPLAY XAUTHORITY DBUS_SESSION_BUS_ADDRESS SystemRoot SYSTEMROOT windir USERNAME APPDATA LOCALAPPDATA`, from the injected `Env`, plus `AGENT_DESKTOP_HOME=<artifacts>/desktop/state`. No API key or `SYNT_*` secret reaches it.
- 30 s per call (configurable in code), 4 MiB stdout cap (the process is killed past it), stderr discarded.
- Envelope: major version `2` required; unknown fields ignored; the command in the answer must match the one asked; a missing `disposition` is never "safe".
- Every string from the UI tree, every window title and every agent-desktop error message is cleaned (control characters, length), redacted, and wrapped as untrusted text; the wrapper cannot be closed from inside.
- Refs the model did not receive from a snapshot of that app are refused before any process starts. A new whole-window snapshot supersedes older refs of that window; an error `STALE_REF` poisons the snapshot it came from.

## Threat model

| Threat | Mitigation | Residual |
|---|---|---|
| Prompt injection through on-screen text (labels, titles, logs) | Wrapped as untrusted; never a permission grant; refs only from our snapshots; sub-agents/peers cannot be steered into desktop tools | The model still reads the text. Same as web pages. |
| Agent controls a password manager, system settings, browser, shell | Hard-deny list at three layers, windows of denied apps hidden even if inside the tree | Name-based: an unlisted app with the same function is not covered. Only exact names; a project binary named like a denied app (`arc`, `control`...) is refused too. |
| Agent drives another app than the one it launched | Window ids come from `list-windows` filtered by the launched process tree; ids outside it are refused; no `--app` | An app that re-parents itself away from the tree (daemonises, `open`-style launchers, XPC services) has windows that are simply not visible: fail closed, but it also means such apps cannot be tested yet. |
| Keystrokes land in the wrong window (terminal hosting synt) | `press` is sent with `--window-id`; headless only; the host terminal is never offered | Depends on agent-desktop honouring `--window-id` for `press` (D0 item). |
| Untrusted or tampered `agent-desktop` | Pin + sha256 + version; env cleared; own process group; output cap; timeout | Pin is empty until the owner fills it. TOCTOU between hash and exec. |
| Secret exposure on screen | Pattern redaction of text; password roles replaced; typed values never in events or prompts (`desktop_act.value` is redacted in `redact_input`) | **Screenshots cannot be redacted** (pixels). Window-only, 0700 directory, never uploaded by Synthesis, but a vision model that receives it sees it. The app wiring currently never attaches the image to the model (path only), like the browser. |
| macOS grants Accessibility to the terminal app, so every command the agent runs inherits it | `synt doctor` warns | A signed helper `.app` that holds the grant instead is the proper fix; not built. |
| Runaway agent | 2 actions/s, 300 per session, one command at a time, kill switch | |
| Leftover processes | Process group kill on session end; per-call children die with the call | The macOS command host `agent-desktop` starts (it idles out after 15 minutes) is NOT killed: no safe way to identify it was found (D0 item). |
| Artifacts | `<artifacts>/desktop/{state,shots}` mode 0700 (Unix); not uploaded | Windows ACLs not hardened. |

## Limits

- GUI sessions only. No display (Linux without `DISPLAY`/`WAYLAND_DISPLAY`, macOS ssh shell) or WSL: tools return `desktop_unavailable`. macOS/Windows cannot be told apart from CI without an OS call, so a headless macOS/Windows runner fails at the first `agent-desktop` call instead.
- Launching goes through the terminal runtime's sandbox. A GUI app under the macOS Seatbelt profile (mach-lookup is restricted) or the Linux sandboxes may not start or may not show a window; this is unverified (D0). The sandbox is not weakened for it.
- Process table: `ps -A -o pid=,ppid=` (macOS, Linux), PowerShell `Get-CimInstance Win32_Process` by absolute `System32` path (Windows; unverified).
- `list-windows` is accepted as a JSON array or `{windows: [...]}` (the reference shows an array; confirm at D0).
- Screenshot PNG is validated (magic, size 8 B-25 MiB) before it is reported.
- Tests use the fake CLI `crates/synthesis-desktop/src/bin/fake_agent_desktop.rs` (never shipped) and, in `synthesis-tools`, a POSIX shell script (Unix only).

## D0 checklist for the owner (real machine, per OS)

Do these on a throwaway account with nothing private open. Use a disposable test app (any small GUI program).

1. Get a release of agent-desktop you trust (README: `npm install -g agent-desktop`, or a GitHub release archive). Note the version from `agent-desktop version`. Keep any helper beside the executable.
2. Compute the sha256 of the executable and put `(os-arch, sha256)` and the version in `crates/synthesis-desktop/src/pin.rs`. Then `synt doctor` must say `desktop pin: sha256 matches`.
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
- Verified by tests (fake CLI only): the whole launch -> snapshot -> act -> wait -> screenshot ->
  logs -> close loop, every permission mode for project and external apps, hard-deny list, settings
  rejection, rate limit, timeout/oversize kill, process-tree kill, redaction.
- Unverified: everything that needs the real binary, real OS permissions or real windows (see D0).
