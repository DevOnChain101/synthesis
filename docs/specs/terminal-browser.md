# Spec: Desktop Terminal and Browser tabs (design, draft 0.1, for owner review)

Status: design only, nothing built. Owner decisions of 2026-11-16 (below) reverse the earlier "no terminal emulator, no PTY"
non-goal of `docs/specs/desktop.md` section 1 and section 5 ("Interactive PTY: not wanted"). Adjacent specs: `desktop.md`
(sections 5, 8 and 9: the tabs, `synt engine --stdio`, WSL), `events.md`, `sandbox.md`, `sandbox-hardening.md`,
`docs/BROWSER-LOGIN.md`, `docs/SECURITY.md`. Tags: **[A]** = assumption, to be checked in the PR that depends on it.

## 0. Owner decisions (2026-11-16) and non-negotiables

1. No webview of any kind, no `wry`, no code, crate or wording from the former webview stack.
2. Terminal tab = a real terminal: `alacritty_terminal` (VT parser and grid) plus a PTY, drawn by GPUI. The person types and
   runs commands; the agent can use the same terminal.
3. Browser tab = a usable browser view (URL bar, back, forward, reload; tabs optional) on the EXISTING Chrome runtime
   (`crates/synthesis-browser`), shown by streaming CDP frames into GPUI and forwarding the person's mouse and keyboard over
   CDP `Input.*`. The agent's browser tools act on the same session.

Non-negotiables carried from the product (CLAUDE.md, ADR-010/011, SECURITY.md):
- N1. Nothing the agent does through these tabs bypasses the sandbox, the permission engine (rules, asks, plan mode,
  auto judge, protected and secret paths), the URL policy or the credential rules. Fail closed when a check cannot run.
- N2. What the person types is the person's own: it is never fed to the model silently, never written to events, the
  store or logs.
- N3. No `SYNT_*` variable and no API key reaches a shell or a page. Rust never writes credentials.
- N4. macOS, Linux, Windows (native and WSL2 mode) are all first-class; CI on all three; `unsafe` stays out of the workspace.
- N5. Simplest design that meets 1-3. No settings key, protocol field or mode that no requirement needs.

Correction of a premise: `browser.frame` today is NOT a screencast. It is one event per `browser_screenshot` PNG artifact
(`events.md`; no image bytes in the stream). There is no `Page.startScreencast` in the runtime. Live frames are new work
(section 2.3) and deliberately do not travel as events.

---

## 1. Terminal

### 1.1 Options

| | Design | For | Against | Verdict |
|---|---|---|---|---|
| T-A | One PTY shell per tab, owned by the desktop; the agent types into it (keystrokes) | Literally "the same terminal" | The shell is unconfined: every agent line escapes the sandbox, permission matching only sees keystrokes, secrets the person typed are on screen for the model | Rejected (violates N1, N2) |
| T-B | Everything owned by the engine, person shells too, all sandboxed | One owner, one protocol | The person's own shell cannot `git push`, `ssh`, use their keys or Docker; in WSL mode a person's shell needs no engine at all | Rejected (the person's terminal must be theirs) |
| T-C | **Two kinds of PTY in one tab.** *Personal* shells: owned by the desktop, unconfined, invisible to the agent. *Shared* shells: owned by the engine, spawned through the sandbox, readable and writable by the agent through gated tools, and typed into by the person in the same view | The agent really shares a terminal with the person, and every agent byte is gated and confined | Two spawn paths; the person must pick "shared" to collaborate | **Chosen** |
| T-D | No PTY for the agent; keep Bash only, show its output in a terminal-looking pane | Smallest | Not what the owner asked (interactive programs, the person and the agent on the same screen) | Rejected |

**Strongest reason against T-C:** a shared shell is sandboxed, so a person who opens it and runs `git push` or `ssh` there
hits the sandbox; they must know which kind of shell they are in. Mitigation: the kind is in the tab title, the colour and
the "agent can read this" banner; personal is the default for a tab the person opens.

### 1.2 Ownership and data flow

```
 desktop process (GPUI)                                 engine (same process, or `synt engine --stdio` in WSL)
 ┌───────────────────────────────────────────┐          ┌──────────────────────────────────────────────────┐
 │ Terminal tab                              │          │ harness: tools pty_open/pty_send/pty_read/pty_close│
 │  ├ Personal pty_p1 ── portable-pty ─ shell│          │   -> permission engine -> auto judge -> plan mode │
 │  │   (desktop owns; agent cannot address) │          │ synthesis-terminal::pty  (owns SHARED PTYs)       │
 │  └ Shared  pty_3 ◄── bytes ────────────────┼── chan / ─┤   SandboxBackend::wrap(shell) -> portable-pty     │
 │       alacritty Term (draw)  ── person keys┼── proto ──►   alacritty Term (agent's screen view)           │
 └───────────────────────────────────────────┘          └──────────────────────────────────────────────────┘
```

- **Personal PTY** (`origin: person`): spawned by `apps/desktop` with `portable-pty`, the person's shell (`$SHELL -l` on
  unix, `resolve_shell` of `synthesis-terminal` on Windows), cwd = the session folder. In WSL mode it is
  `%SystemRoot%\System32\wsl.exe -d <distro> --cd <linux cwd>` under ConPTY (the distro's login shell), with the same
  allowlisted environment as the engine launch (`remote::allowed_env`) minus `SYNT_*`, no `WSLENV`. It never touches the
  engine, so it survives engine restarts and session switches; it dies with its tab or the app.
- **Shared PTY** (`origin: agent`, also when the person opens one): spawned by the engine in `synthesis-terminal` (new
  module `pty`), wrapped exactly like a `Bash` command (`SandboxBackend::wrap`, `SandboxMode` fail-closed rules, the
  session env from `Env::child_env`, egress `build_env`). Session-scoped: an engine restart or session end kills it. Counts
  against `Limits` (live processes, `max_session_log_bytes`). Its output is also appended to a 0600 log under
  `<artifact_root>/terminal/` (same cap and kill rule as today), so the session keeps a record of what the agent saw.
- **Parsing twice is deliberate:** the desktop keeps an `alacritty_terminal::Term` per PTY for drawing; the engine keeps
  one per shared PTY to answer `pty_read` (the screen the agent sees). Bytes are cheap; shipping rendered grids over the
  WSL protocol is not.
- **Bash is unchanged.** One-shot commands still go through `Bash` (exact output, exit code, artifact). The shared PTY is
  for interactive programs (REPLs, prompts, TUIs, watch processes) and for working in front of the person. The tool
  descriptions say so. `!cmd` is unchanged.

### 1.3 Agent tools (main agent only; sub-agents and peer work get "not available")

| Tool | Input | Gate | Result |
|---|---|---|---|
| `pty_open` | `cwd?` (inside the workspace roots) | side effect `execute`; permission like `Bash` running the user's shell (`Bash(<shell>)` rule form); plan mode refuses; auto mode goes to the judge; Windows native with no backend: refused like `Bash` under `enforce` | `{pty: "pty_N", sandbox: Guarantee}` |
| `pty_send` | `pty`, `text` (printable, no control chars, <= 4 KiB), `enter: bool`, `keys?: [named key]` (`ctrl-c`, `ctrl-d`, `tab`, `esc`, arrows, `enter`...) | see 1.4 | `{sent: bytes}`; never echoes `text` |
| `pty_read` | `pty`, `lines?` (<= 2000) | side effect `read`; allowed in plan mode | screen + scrollback tail as text, `UntrustedText`, redacted (1.5), <= 64 KiB, `{cursor, fg: "shell"\|"program"\|"unknown", exited?}` |
| `pty_close` | `pty` | none beyond ownership | `{exit_code?}` |

The agent can only name PTYs the engine created (`pty_N`); personal PTYs have no id in the engine and cannot be addressed.

### 1.4 How an agent line is gated (`pty_send`)

1. Refused when: the PTY is held by the person (1.6); the person has typed since their last Enter/Ctrl-C/Ctrl-U (`busy`);
   the terminal has ECHO off (password prompt, unix `tcgetattr` on the master, see 1.8); the session is in plan mode;
   `text` holds any C0/C1 control, ESC, or a bracketed-paste delimiter (escape smuggling); `keys` names anything but the
   fixed list.
2. When `enter: true` and the foreground process group is the shell's own (`portable-pty` `process_group_leader()` equals
   the shell pid; [A] exposed on unix) or cannot be determined (Windows, failure): `text` is treated as a **Bash command**
   and goes through the same path as a `Bash` call: permission rules (allow/ask/deny on `Bash(...)`), protected and secret
   path scans (`deny_secret_reads`, `browser-profiles` rule), critical-removal checks, the auto judge with the same
   inputs. It is sent only if that path allows it. A denied line is a tool error, nothing is written to the PTY.
3. When another program is in the foreground (a REPL, `vim`, a `[y/N]` prompt): the input is a new permission kind
   `PtyInput(pty_N)`: `ask` in `default`, allowed in `acceptEdits` only by an explicit rule, judged in auto mode. `ctrl-c`
   is always allowed (stopping is never gated).
4. Whatever passes is confined by the PTY's sandbox anyway (network, filesystem, hidden state paths). Gating is the policy,
   the sandbox is the boundary; neither replaces the other.
5. Events: `tool.requested` carries `text` redacted as for `Bash` input; `pty.agent_input {pty_id, tool_call_id, bytes,
   kind}` (no text). The person's keystrokes are never an event.
6. **T3 requirement (security review of T1).** `SYNT_AGENT_SESSION` in the shell's environment is only a speed bump:
   `env -u SYNT_AGENT_SESSION synt memory approve ...` or `unset SYNT_AGENT_SESSION` defeats it. The gate of rule 2 MUST
   therefore deny any line that runs `synt memory` approve/delete (and `synt` subcommands that change approvals) and the
   forms that remove the marker (`env -u`, `unset`, `export -n`, `env -i`), exactly as for `Bash`; the marker is not the
   control.

### 1.5 What terminal output may reach the model

- Only through `pty_read`, a visible tool call (card in the chat). No background feeding, no automatic attach of terminal
  text to a turn, nothing from personal PTYs ever (they are not in the engine).
- `pty_read` text passes the sink redactor (private-key blocks, token patterns, the session's registered secret values)
  BEFORE the model sees it. This deviates from `events.md` section 5.4 (tool results reach the model unredacted) on
  purpose: a shared terminal shows what the person typed and printed, not only what the agent ran.
- **Hold**: the person can hold a shared PTY ("Agent paused" toggle, or automatically while they type, 1.6). Output
  written while held is withheld from the agent for good: the engine records the absolute grid line where the hold began
  and ended; `pty_read` blanks rows in that range (and the whole screen until it has scrolled or been cleared, because a
  full-screen program can redraw old rows). A "Clear and share" button clears the screen and lifts the blanking.
- ECHO-off input (passwords) never appears on screen, and the agent cannot type while ECHO is off. Windows ConPTY has no
  echo state: there, a last screen line matching `(?i)(password|passphrase|pin|token)[^:]*:\s*$` counts as ECHO off.

### 1.6 Control arbitration and indicator

- Each shared PTY has a holder: `shared` (default) or `person`. The person's keystrokes always go through, in any state.
- Typing in a shared PTY marks it `busy` until Enter, Ctrl-C or Ctrl-U; the agent's `pty_send` gets `busy` (it may retry
  later). Ctrl+Shift+H (and the header button) toggles `person` hold; `pty_send` and `pty_read` then return `held by the
  person`. Release returns to `shared`.
- Indicator in the tab header: `Personal` (no agent access), `Shared · agent can read and type` (accent colour), `Shared ·
  agent running <cmd>` while an agent line runs in the foreground, `Held by you`. The agent's last line is shown as a dim
  marker in the gutter so the person sees which commands were the agent's.
- Event `pty.control_changed {pty_id, holder, reason}` (person action only; not every keystroke).

### 1.7 Limits, sizes, cleanup

- Scrollback 5 000 lines per PTY (alacritty cells are about 24 bytes: a 200-column screen is about 24 MB at the cap);
  at most 8 personal and 4 shared PTYs (the shared ones also count in `Limits`). Fixed numbers, no setting (N5).
- Output path: a reader thread per PTY, chunks coalesced to 16 ms or 64 KiB. Backpressure: at most 1 MiB not yet consumed
  by the drawer (local channel) or not acknowledged by the client (WSL, `pty.ack`); past it the reader stops reading the
  master, so the child blocks on write (the normal PTY flow control). No unbounded queue anywhere.
- Kill on close, session end, engine restart and app exit: unix: SIGHUP to the PTY's session via closing the master, then
  after 2 s SIGKILL to the shell's process group and every foreground group seen (`rustix` safe `kill_process_group`). Linux
  shared PTYs: bubblewrap `--die-with-parent` takes the whole PID namespace. Windows: `ClosePseudoConsole`, then
  `%SystemRoot%\System32\taskkill.exe /T /F /PID <pid>`. Known limit (as in `TerminalRuntime`): on macOS and Windows a
  process that leaves its group/job (`setsid`, double fork) survives; the sandbox still confines it on macOS.
- The person closing a tab with a running foreground program asks once ("<cmd> is running. Close?"; default Cancel).

### 1.8 Platforms

| | Personal PTY | Shared PTY | Notes |
|---|---|---|---|
| macOS | `posix_openpt` via portable-pty, `$SHELL -l` | Seatbelt wrapper | ECHO via `tcgetattr` on the master [A] |
| Linux | same | bubblewrap + seccomp helper; Landlock only where `sandbox.md` allows | X11 and Wayland IME through GPUI |
| Windows native | ConPTY (Windows 10 1809+), `resolve_shell` (pwsh, powershell, cmd, Git Bash) | refused under `enforce` (no backend, sandbox-hardening decision 6); with `--sandbox off` it runs, gated per 1.4 with fg = unknown | no fg-group or echo state; heuristics above; legacy conhost not supported |
| Windows, WSL mode | `wsl.exe -d D --cd X` under ConPTY | in the distro by the engine (Linux path, bubblewrap) over the protocol (1.9) | personal shell has interop like any WSL shell: it is the person's |

Environment of a personal shell: the desktop's environment minus every `SYNT_*` name and minus secret-shaped names
(the loader's existing secret-name rule), plus `TERM=xterm-256color`, `COLORTERM=truecolor`. A login shell re-reads the
person's profile, which may export their own keys: that is their shell. Shared shells get the `Bash` environment only.

### 1.9 Protocol (engine methods; in-process calls locally, `synt engine --stdio` protocol 4 in WSL mode)

Client to engine: `pty.open {cols, rows}` (a shared shell the PERSON opened: the click is the approval, so no permission ask; always sandboxed, allowed in every mode), `pty.input {id, data: base64}` (person keys, <= 64 KiB,
500/s), `pty.resize {id, cols, rows}` (1..300 x 1..500), `pty.hold {id, on}`, `pty.ack {id, bytes}`, `pty.close {id}`.
Engine to client (not `Envelope`s, never stored): `{"msg":"pty.data","id","seq","data"}`, `{"msg":"pty.state","id",
"holder","busy","echo_off","fg","exited"}`. Stored events: `pty.opened {pty_id, origin, sandbox, cols, rows}`,
`pty.closed {pty_id, exit_code?, signal?}`, `pty.agent_input`, `pty.control_changed`. The hello's exact protocol match
already forces client and engine to update together. Client limits of section 8 apply (lines, bytes per second).

### 1.10 Rendering, input, IME, clipboard, selection (desktop)

- A GPUI element paints the `Term` grid: per row, runs of equal style become one shaped text run (Fira Code, the TUI
  palette mapped onto the 16 ANSI colours, 256 and truecolor as given), background quads, cursor shapes, selection.
  **Zed's `terminal`/`terminal_view` crates are GPL-3.0: read for ideas only, never copied** (`deny.toml` allows no GPL).
- Keys: GPUI keystrokes encoded per alacritty's `TermMode` (application cursor, keypad, `kitty` keyboard protocol off in
  v1). App shortcuts (Cmd/Ctrl+1..5, Cmd/Ctrl+B/I, Esc-to-chat only via Ctrl+Esc inside a terminal) are taken first.
- IME: the element implements GPUI's input handler: preedit drawn at the cursor, commit written as UTF-8.
- Mouse: selection by drag (Shift+drag when the program enabled mouse reporting, which otherwise receives SGR mouse
  reports). Copy: Cmd+C (macOS) / Ctrl+Shift+C; paste: Cmd+V / Ctrl+Shift+V, bracketed when the program asked for it.
- Program-initiated clipboard (OSC 52) store AND load are ignored for every PTY (a program must not read the person's
  clipboard). OSC 8 links open only on Cmd/Ctrl+click after a dock confirmation that shows the URL. Titles (OSC 0/2) are
  stripped of control/bidi characters and clipped to 80. Bell: ignored.

### 1.11 Dependency vetting (ADR-010: verify before depending)

| Crate | Use | Check in PR T1 (record results in this file) |
|---|---|---|
| `alacritty_terminal` (Apache-2.0, alacritty org; latest seen 0.25.x [A]) | VT parser and grid only (`Term`, `vte`), NOT its `tty`/event loop | licence in `deny.toml` allow list; release cadence (ships with alacritty); default features off; `cargo deny`; build on the 5 release targets; MSRV vs workspace |
| `portable-pty` (MIT, wezterm repo; 0.9.0, last release Feb 2025 [A]) | spawning: `env_clear`, arbitrary argv (the sandbox wrapper), ConPTY, `process_group_leader`, `get_termios` | slow release cadence is the risk: pin exactly; read its Windows code (ConPTY via `CreatePseudoConsole`), check no `openssl`; fallback if abandoned: alacritty's own `tty` module (but it cannot clear the environment: [A] verify) |

**Vetting record (PR T1, 2026-11-17).**

| Crate | Pin | Licence | Maintenance (crates.io) | Result |
|---|---|---|---|---|
| `alacritty_terminal` | `=0.26.0`, `default-features = false` (no `serde`) | Apache-2.0 (allowed) | 0.26.0 on 2026-04-06, 0.25.1 on 2025-10-18: a release about every 3-6 months with alacritty; 2.8 M downloads; MSRV 1.85 (workspace 1.89) | Used for `Term` + `vte` only (`pty` module); its `tty`/`event_loop` modules are compiled (not optional) but never called |
| `portable-pty` | `=0.9.0` | MIT (allowed) | 0.9.0 on 2025-02-11, 0.8.1 on 2023-03-13: slow cadence (R3), 29 M downloads, widely used; no `rust-version` declared | Pinned exactly. No `openssl`/`native-tls` (`cargo deny` bans pass). ConPTY: `CreatePseudoConsole` loaded from `kernel32` (or a side-loaded `conpty.dll`, never shipped by us, loaded by bare name: mitigated by `harden_dll_search`, see "Decided in T1"), spawn with an explicit environment block. `env_clear()` removes the inherited environment (its builder snapshots the process env at `new`; we clear before adding anything) |

New transitive crates in `Cargo.lock` (all licences already on the `deny.toml` allow list): `vte 0.15.0` (Apache-2.0 OR MIT), `cursor-icon 1.2.0` (MIT OR Apache-2.0 OR Zlib), `home 0.5.12` (MIT OR Apache-2.0), `miow 0.6.1` (MIT OR Apache-2.0, Windows), `rustix-openpty 0.2.0` (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT), `serial2 0.2.39` (BSD-2-Clause OR Apache-2.0), `shared_library 0.1.9` (Apache-2.0/MIT, Windows), `shell-words 1.1.1` (MIT/Apache-2.0), `nix 0.28.0` (MIT), `windows-sys 0.59.0` (MIT OR Apache-2.0), `winreg 0.10.1` (MIT, Windows), `cfg_aliases 0.1.1` (MIT). `rustix` (already locked, 1.1.6) is now a direct unix dependency of `synthesis-terminal` with only the `process` feature, for the safe `kill_process_group`. `synthesis-config` is a direct dependency for `is_secret_shaped`.
Checks run: `cargo deny check` passes (advisories, bans, licenses, sources; only `multiple-versions` warnings, `warn` level, a few more than before: `nix`, `winreg`, `windows-sys`, `cfg_aliases`); `cargo audit --deny warnings` passes; no new ignore in `deny.toml` or `.cargo/audit.toml`. `cargo clippy -p synthesis-terminal --all-targets -D warnings` passes on `x86_64-apple-darwin` (host; tests run) and, check only, on `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`. Not run: `aarch64-*` and `x86_64-unknown-linux-musl` (CI release builds), tests on Linux and Windows (CI).

Why not alacritty's `tty`: it adds to the inherited environment and owns the spawn, so the sandbox wrapper and
`env_clear` cannot be guaranteed. `unsafe` lives inside these crates, never in ours (as with `process-wrap` today).

---

### 1.12 Reply path (what the engine writes back to a shared PTY on its own)

Before T1 the property was "nothing writes to a PTY except the person's input and the agent's gated `pty_send`". It is
replaced by this bounded allow-list:

- Only the engine's `Term` replies, and only to two queries: the cursor position report (`ESC[6n` -> `ESC[row;colR`, both
  numbers 1-5 digits; ConPTY asks for it at start-up and passes no output until it is answered) and primary device
  attributes (`ESC[c` -> the fixed `ESC[?6c`). Every other query (status reports, DA2, mode queries, text-area size,
  colour, title, clipboard) is ignored; no reply holds a newline or any program-chosen byte.
- The reply goes to the shell's stdin, so a program that prints these queries can make stray `ESC[..R` bytes appear as
  input of whatever reads the terminal. That is the standard terminal behaviour and carries no command: the bytes contain
  no newline and no text.
- Caps: at most 64 replies per second per PTY (the rest dropped) into a write queue of 16 entries (overflow dropped),
  drained by one writer thread per PTY. Typed input and agent input share that queue and fail with a timeout after 2 s when
  it is full. A child that never reads its input therefore stalls only the writer thread; the reader, the screen lock,
  `screen_text` and `close` never wait on a write. The writer thread ends when the PTY is killed or closed.
- A command line must be sent in ONE `write_input` call (and `pty_send` in T3 must do so): a reply can land between two
  calls as stray text in the middle of the line.
- A straggler that keeps the slave open (a `setsid` daemon, a background job outside the groups we kill) leaves that PTY's
  writer thread blocked in a write until the straggler dies; it is one idle thread, nothing else waits on it.
- T4 requirement: a `Term` that draws a shared PTY on the desktop must NOT reply. Add a type-level guard when T4 builds it
  (the drawing `Term` takes a listener type that cannot write, for example `VoidListener`, and only the engine constructor
  takes `Reply`); until then this is documentation only. A `Term` that draws a shared PTY on the desktop (T4) must NOT reply (the program would get two answers); only the engine
  `Term` is built with the replying listener, and its allow-list is unit tested (`only_cpr_and_fixed_da1_are_answered`).
  T4 must keep that rule when it adds the drawing `Term`.

## 2. Browser

### 2.1 Options

| | Design | For | Against | Verdict |
|---|---|---|---|---|
| B-A | Show Chrome's own headed window (today's `synt.browser.headed`) | Exists | Not inside the app; real clicks reach the page with no arbitration; no window on Windows TCP | Rejected for the desktop |
| B-B | Embedded webview | Native feel | Owner decision 1 | Rejected |
| B-C | **CDP screencast into GPUI, input over CDP `Input.*`, same `BrowserRuntime` as the tools** | One session, one policy, every input passes our code (arbitration, gates) | Frame latency and cost; some browser UI (dialogs, file pickers, IME) must be rebuilt | **Chosen** |
| B-D | Periodic `browser_screenshot` refresh | No new CDP use | 1-2 fps, artifacts on disk per frame | Rejected |

**Strongest reason against B-C:** the person browses inside the agent's browser, under the agent's policy (no private
ranges, no WebSocket, no downloads). Many everyday sites (chat apps, dashboards with live updates) break, and whatever the
person opens is readable by the agent after hand-back. B-C is a view onto the agent's browser, not a personal browser;
the UI says so, and "Open in your browser" hands a refused or personal URL to the OS default browser.

### 2.2 Ownership and data flow

- The engine owns the session's `BrowserRuntime` (as today, lazily created by the first browser tool). New engine method
  `browser.view {on}` starts it if needed (same `BrowserConfig`, same profile rules: a saved profile still needs the
  `browser_navigate {profile}` confirmation by the agent's tool; the person cannot pick one from the URL bar in v1).
- **Headless in the desktop.** When the desktop hosts the view, the runtime starts headless whatever
  `synt.browser.headed` says (a second, headed window would take the person's clicks around the arbitration). The key
  keeps its meaning for the CLI and `synt run`; its Settings help text says so. The agent cursor and field highlight
  become GPUI overlays drawn from `browser.action` (no `OVERLAY_JS` in this mode).
- `browser_login` keeps its separate headed window on its own profile (BROWSER-LOGIN.md is unchanged in v1).

### 2.3 Frames: transport, cost, backpressure

- `Page.startScreencast {format: "jpeg", quality: 70, maxWidth, maxHeight}` on the current tab while the Browser tab is
  visible and the window is focused or visible; `Page.stopScreencast` otherwise (zero cost when nobody looks).
- **Latest frame wins.** The runtime keeps one slot (a `watch` channel) with the newest frame and its metadata
  (`deviceWidth/Height`, `pageScaleFactor`, `offsetTop`, scroll offsets). `Page.screencastFrameAck` is sent only after the
  consumer took the previous frame, so Chrome itself slows down to the consumer: bounded memory, no queue. Local: the
  drawer takes it at the 16 ms tick; WSL: the client sends `browser.frame_ack {seq}`.
- Cost signal [A, to measure in PR B1]: a 1280x800 JPEG at q70 is roughly 60-150 KB; at Chrome's typical 10-30 fps
  that is up to 4.5 MB/s through the WSL pipe, within the client's 32 MiB/s limit. A cap of 30 fps is enforced by acking
  no faster.
- Frames are NEVER events, never written to the store or artifacts, never given to the model. Decoding (JPEG to RGBA)
  happens off the UI thread in the desktop (`image` crate already in the lockfile [A]); dimensions capped at 4096x4096
  before decode.

### 2.4 Input mapping and coordinates

- The viewport follows the pane: on resize (debounced 150 ms) `Emulation.setDeviceMetricsOverride {width, height,
  deviceScaleFactor: min(window scale, 2)}` in CSS pixels; agent tools use element refs, not coordinates, so a resize
  does not break them. Mapping: css_x = (pane_x - letterbox_x) / drawn_scale, css_y likewise plus `offsetTop`; points
  outside the frame are dropped.
- Mouse: `Input.dispatchMouseEvent` (moved, pressed, released with `clickCount`, wheel with deltas). Keys:
  `Input.dispatchKeyEvent` with `key`, `code`, `windowsVirtualKeyCode`, `modifiers`, `text`; macOS editing shortcuts need
  the `commands` field (`selectAll`, `copy`, `paste`, `undo`...), a fixed table. App shortcuts are taken before the pane.
- IME: GPUI input handler -> `Input.imeSetComposition` for preedit, `Input.insertText` on commit.
- Clipboard: paste reads the OS clipboard in the desktop and sends `Input.insertText` (the page never gets clipboard
  API access); copy runs `getSelection().toString()` in an isolated world and writes the OS clipboard. Nothing of either
  is logged or evented.

### 2.5 Control arbitration

- Holder: `agent` (default) or `person`. **Any person input in the pane (click, key, wheel, URL bar) switches to
  `person` at once** (pause on user input). While `person` holds, every browser tool, reads included (`snapshot`,
  `screenshot`, `console`, `network`, `tabs`), returns `browser_paused: the person is using the browser` immediately (no
  hidden wait; the model can ask the person). An agent CDP call already in flight finishes; the next one is refused.
- Hand-back: the "Hand back to agent" button (or the person sending a chat message, which is an explicit act) returns
  `agent`. The button's tooltip says "The agent can read the pages open in this browser". The agent's next tool result
  carries one line `The person used the browser; current page: <origin>` (origin only, no path, no titles). Refs from
  before are invalid (the existing document-id rule) and the agent must snapshot again.
- Indicator: pane border and chip `Agent is browsing` (accent) / `You are browsing · agent paused`; the agent cursor is
  drawn only while `agent` holds. Event `browser.control_changed {holder, reason: "person_input"|"hand_back"}`.

### 2.6 Navigation policy for the person

- The URL bar goes through the SAME `UrlPolicy` (http/https only, `deny_domains`, private ranges, link-local and metadata
  addresses blocked unless the user's `allow_private_network`/loopback ports allow them, a saved profile's
  `document_allow_domains`) and the in-browser `Fetch` interception. Reason: whatever loads is readable by the agent
  after hand-back, so the person cannot widen the agent's reach by typing. A refused URL shows the reason and an "Open in
  your browser" button (http/https only, the person's click only, `open` / `xdg-open` / `explorer.exe` by absolute path),
  never offered for a URL the agent produced.
- Back/forward/reload: `Page.getNavigationHistory` / `navigateToHistoryEntry` / `Page.reload`; each is checked like a
  navigation. Tabs: a strip from the runtime's `Tabs.order` when there is more than one; switching is person input. No
  "new tab" button in v1 (pages open popups; the agent opens tabs).
- WebSocket and WebTransport stay blocked as documented in SECURITY.md (constructor replaced; WebTransport at script level
  only). This is the main UX gap of person browsing and is an owner question (section 5).

### 2.7 Credentials, secrets and what the model can see

- **Person logging in inside the agent's browser** is a credential hand-off without `browser_login`'s gates. Rule: the
  first time the person focuses a password field (`input[type=password]`, checked in an isolated world on focus change)
  on an origin in this session, key events to it are held until a dock dialog is answered: "You are logging in inside
  the agent's browser. The agent will be able to act as you on <origin> in this session." (Cancel default). The dialog
  is refused outright (keys dropped, notice) unless the sandbox is `enforce` or `synt.browser.allow_unsandboxed`, the same
  `require_sandbox` rule (stdio MCP servers included) as the credential tools, because the temp profile with the cookies
  must be out of reach of the agent's commands. [A] Today the temp profile is a `tempfile::TempDir` in the system temp
  dir; PR B4 moves it under the session's state dir (hidden from sandboxed commands, in `synthesis_state_paths`).
- Chrome's password manager, autofill and save prompts are switched off for the temp profile too (today: login profiles
  only [A]).
- **Screenshots and snapshots given to the model**: the existing `fill_secret` capture guard stays. New: fields the
  PERSON typed into since the document loaded, every `input[type=password]`, and fields with `autocomplete` `cc-*`,
  `one-time-code`, `current-password`, `new-password` are masked: in `browser_snapshot` their values become `[masked]`;
  in `browser_screenshot` their rectangles (from `DOM.getBoxModel`, all frames) are filled black in Rust on the decoded
  PNG before it is saved or returned, so page script cannot undo it. If the rectangles cannot be computed, the screenshot
  is refused (`person input on page`). The live screencast is not masked: only the person sees it.
- The person's keystrokes are never events, never logged; `browser.action` stays agent-only.

### 2.8 Dialogs, downloads, uploads, permissions

- JavaScript dialogs (`Page.javascriptDialogOpening`): shown as a dock in the pane, text sanitized and labelled
  "This page says"; the person answers; while `agent` holds, the existing tool behaviour applies [A: today unhandled,
  which blocks the page; PR B5 dismisses them for the agent with a tool note]. `beforeunload` is auto-accepted.
- Downloads stay `deny` (today's `Browser.setDownloadBehavior`); a person-initiated download shows "Downloads are off in
  the agent's browser" with "Open in your browser".
- Uploads: `Page.setInterceptFileChooserDialog`. A chooser the person triggered opens GPUI's native file picker, then
  `DOM.setFileInputFiles` (local engine only; refused in WSL mode in v1: the files are on the Windows side). A chooser the
  agent triggered is cancelled (no upload tool exists).
- Site permissions (camera, mic, geolocation, notifications, clipboard): denied with `Browser.setPermission` for the
  context; no prompt.

### 2.9 Protocol (engine methods; protocol 4 in WSL mode)

Client to engine: `browser.view {on, max_w, max_h}`, `browser.frame_ack {seq}`, `browser.resize {w, h, dpr}`,
`browser.input {kind: mouse|wheel|key|ime|text, ...}` (500/s, each field bounded), `browser.navigate {url}`,
`browser.history {delta: -1|1}`, `browser.reload`, `browser.tab {index}`, `browser.hand_back`, `browser.dialog {accept,
text?}`, `browser.files {paths}` (local only), `browser.login_confirm {origin, ok}`.
Engine to client (never stored): `{"msg":"browser.screencast","seq","mime","data","w","h","css_w","css_h","offset_top"}`,
`{"msg":"browser.state","url","title","loading","can_back","can_fwd","tabs","holder","dialog?"}` (full URL and title go
to the window only; they are untrusted text, sanitized for display). Stored events: `browser.control_changed`; existing
`browser.frame` and `browser.action` unchanged.

### 2.10 Windows, WSL, transports

- Windows native: CDP over TCP on 127.0.0.1 (no pipe without `unsafe`): any local process can reach the port. The view
  works; persistent profiles stay refused (BROWSER-LOGIN.md). Person logins in the view (2.7) are refused on TCP too:
  the same reason (a local process could drive the logged-in browser).
- WSL mode: Chrome runs in the distro (installed there, or `SYNT_BROWSER_PATH`); frames cross the stdio pipe; WSLg is
  not needed because the view is headless.

---

## 3. Settings keys

None new. Every switch needed already exists: permission rules deny or ask `pty_open`, `pty_send`, `PtyInput(...)` and the
browser tools (managed settings can deny them); `synt.browser.allow_unsandboxed` and `allow_private_network`/loopback
ports keep their user-or-managed scope; `synt.browser.headed` keeps its CLI meaning. Fixed limits (scrollback, PTY
counts, fps) are constants until someone needs them configurable.

## 4. Implementation plan (each PR shippable alone; CI = fmt, clippy, tests on ubuntu, macos, windows)

| # | PR | Contents | Tests (red-team marked RT) | Est. |
|---|---|---|---|---|
| D0 | docs | this spec; `desktop.md` non-goals, section 5 terminal/browser bullets, "Interactive PTY" line, wording of the title-bar paragraph | n/a | 0.5 d |
| T1 | `synthesis-terminal::pty` + deps | vetting record (1.11); spawn with `env_clear` and sandbox wrap; reader with 1 MiB window; engine-side `Term`; kill rules; limits | spawn/echo/resize on 3 OSes (ConPTY on windows-latest); RT: flood 200 MB -> bounded memory and log-cap kill; RT: `setsid` child after close (Linux killed via bwrap, macOS documented); RT: env has no `SYNT_*` | 5 d |
| T2 | desktop personal terminal | GPUI grid element, keys, mouse, selection, clipboard, IME, OSC rules, tabs in the Terminal pane, WSL personal shell via `wsl.exe` | offscreen shot of a fixed byte script (`--shot --pty-script`), `design_tests` for the pane; RT: OSC 52 load/store ignored; RT: title with bidi/escapes sanitized; RT: personal shell env without `SYNT_*`/secret names | 8 d |
| T3 | agent tools + gates | `pty_open/send/read/close`, `PtyInput` permission kind, Bash-path matching, hold and blanking, redaction, events | RT: `pty_send` to a personal id -> unknown; RT: `cat ~/.ssh/id_rsa` and `cat .env` lines denied; RT: newline/ESC/`\e[201~` in `text` refused; RT: send while ECHO off, while busy, while held, in plan mode -> refused; RT: sub-agent and peer calls refused; RT: output during hold absent from `pty_read`; RT: `sk-ant-...` printed -> redacted; auto-judge receives the line | 6 d |
| T4 | shared PTY in the desktop + protocol 4 (pty) | person opens shared shell, indicator, hold, gutter markers; `pty.*` methods with limits; fake `wsl.exe` path test | `remote_engine.rs` extended; RT: oversized `pty.input`, rate over 500/s, unknown id; RT: unacked window stops reading | 4 d |
| B1 | screencast in `synthesis-browser` | `startScreencast`, latest-frame slot, ack on consume, metadata; `browser.view` method | real headless Chrome: frames arrive, stop on view off; RT: stalled consumer -> one frame held, Chrome stops sending; frames absent from events/store | 3 d |
| B2 | desktop browser pane (view only) | decode off thread, draw, URL bar (policy-checked navigation), back/forward/reload, tabs strip, agent cursor overlay, headless override | shots with a fixed frame; RT: `file:///etc/passwd`, `http://169.254.169.254`, `http://10.0.0.1` from the URL bar refused; "Open in your browser" absent for agent URLs | 4 d |
| B3 | input + arbitration | mouse/wheel/key/IME/clipboard mapping, viewport follow, holder switch, `browser_paused`, hand-back note | coordinate mapping unit tests (letterbox, dpr, offsetTop); RT: agent click while person holds -> refused; RT: snapshot while person holds -> refused; RT: hand-back note has origin only | 5 d |
| B4 | credentials and masking | password-focus gate with sandbox rule, temp profile moved into state paths, password manager off for temp profiles, person-typed field masking in snapshot and screenshot (Rust-side fill) | RT: person types in a text field, agent screenshot -> rect black; RT: rect computation fails -> refused; RT: password focus without enforce -> keys dropped; RT: agent `Read` of the temp profile denied; RT: masking survives a page that removes overlays | 5 d |
| B5 | dialogs, uploads, downloads, permissions | as 2.8 | RT: `alert` spoofing app chrome is labelled; download denied; agent-triggered chooser cancelled; camera permission denied | 3 d |
| B6 | protocol 4 (browser) | `browser.*` methods and messages, frame ack over the pipe, limits | fake `wsl.exe` end to end; RT: frame flood vs client limits; oversized input fields | 2 d |

Order: D0, T1, T2, T3, T4, B1, B2, B3, B4, B5, B6 (B1-B3 can run in parallel with T2-T4; B4 must land before the view
accepts keystrokes into password fields: until then B3 drops key events to password fields). Total about 45 engineering
days [A: one developer familiar with the codebase; Windows and WSL verification on real machines not included].
Not provable in CI and listed as unverified until run on a real machine: IME on each OS, HiDPI, ConPTY with real
programs (vim, pwsh), WSL2 end to end, Linux Wayland input.

## 5. Risks

- R1 Person confuses personal and shared shells (runs credentials in a shared one). Mitigated by default = personal,
  banner, colour; residual: redaction is lossy (events.md 5.4).
- R2 Foreground detection is unix-only; Windows falls back to treating every line as a shell command (stricter, may
  mis-gate REPL input as Bash).
- R3 `portable-pty` releases are slow; a ConPTY bug may need a fork (allowed registries only: a fork must be published or
  vendored under `[patch]`, an owner call).
- R4 Screencast latency (CDP JPEG) makes the view feel slower than a real browser; acceptable for "quick browsing" [A].
- R5 Masking depends on detecting fields; a page that renders typed text outside an input (canvas, contenteditable
  mirrors) is not masked. contenteditable the person typed into IS covered; canvas is not.
- R6 The person's browsing is under the agent's restrictive policy; owner may find it too strict (section 6, Q2).
- R7 Two `Term` parsers per shared PTY can diverge if bytes are dropped; they are never dropped (backpressure), and the
  engine's view is the one the agent reads.

## 6. Open owner decisions

- Q1. Should the person be able to "share" a personal shell with the agent (read-only, after a confirmation, from that
  moment on)? Proposed: no in v1; a selection "Send to chat" (person-initiated, quoted as user text) covers the need.
- Q2. Person browsing with WebSocket allowed: a separate "personal" browser context the agent can never read (no tools,
  no snapshot), visibly marked? Proposed: not in v1 (doubles the policy surface); "Open in your browser" instead.
- Q3. Move `browser_login` into the embedded view (no separate window) later? Proposed: after B4, once the masking and
  password gate have been used.
- Q4. `synt.browser.headed` ignored by the desktop (2.2): acceptable, or should the desktop offer the separate window too?
- Q5. PTY tools in the CLI (`synt`) and `synt run`: proposed desktop-only in v1 (registered only with `Front::Native` and
  `--host desktop`), because the person cannot see or hold the PTY elsewhere.
- Q6. `tauri-winrt-notification` is in `Cargo.lock` as a transitive Windows dependency of `notify-rust` 4.18.2 (toast notifications); decision 1 says
  no such crate "anywhere": replace the notification dependency, or accept a transitive crate by that name?

### Decided in T1 (PR `synthesis-terminal::pty`; reverse any of these in review)

- Decided in T1: the shared shell is `shell.program -i` (`/bin/sh -i` on unix, Git Bash `-i`, `cmd.exe` with no argument), the same shell the classifier of `Bash` assumes, not `$SHELL`. A login shell and the person's rc files are therefore not loaded in a shared PTY.
- Decided in T1: the child env is the session env (already the `Env::child_env` allowlist) minus every `SYNT_*` name except the output-only marker `SYNT_AGENT_SESSION` (so `synt memory` still refuses approvals typed into a shell the agent can reach), minus secret-shaped names (`is_secret_shaped`), `BASH_ENV` and `ENV`; `TERM=xterm-256color` and `COLORTERM=truecolor` are set. The spec's "no `SYNT_*`" test therefore reads "no `SYNT_*` other than `SYNT_AGENT_SESSION`".
- Decided in T1: the window (1 MiB) applies only while a subscriber exists. With no subscriber the engine `Term` and the log consume everything immediately; a PTY nobody watches must not stall its child. A new subscriber sees output from the moment it subscribes (T4 sends a screen snapshot first).
- Decided in T1: log cap kill = SIGKILL (unix) of the shell group and of the foreground group, `taskkill /T /F` on Windows; afterwards the reader keeps draining and discarding the master until EOF, because a dying tty waits for its output to drain (observed on macOS: the shell stayed in the exiting state while the master was unread). The over-cap bytes are not written to the log.
- Decided in T1: kill = the shell's process group plus the current foreground group (`tcgetpgrp`, via portable-pty `process_group_leader`), not "every foreground group seen". A job started with `&` in an interactive shell has its own group and is not killed unless the shell hangs it up (bash does, dash may not) or bubblewrap's PID namespace takes it; documented limit, tested.
- Decided in T1: `close` = SIGHUP to the group, up to 2 s for the shell to exit, then SIGKILL; drop (session close, runtime drop) = SIGKILL at once. The unix kill uses rustix's safe `kill_process_group`; no `unsafe` in our code.
- Decided in T1: no 16 ms chunk coalescing in the engine (one chunk per `read`, at most 64 KiB), no disk-quota watcher for PTYs, and OSC 52 is `Disabled`. REVISED after CI and review: the engine `Term` DOES answer two terminal queries (cursor position report, fixed DA1; see 1.12, bounded queue and writer thread), because ConPTY asks for the cursor position at start-up and passes no output until it is answered. The drawing `Term` of a shared PTY (T4) must therefore NOT answer, or the program gets two replies.
- Decided in T1 (security review): the shell's pid is never signalled once it was reaped (a recycled pid could be another group leader of the same user; `taskkill /T /F /PID` likewise): the reap flag is checked under the same lock as the kill, `close` kills before it reaps, and a dropped PTY is reaped by a thread that retries for up to 60 s. The foreground-group lookup stays (it fails once the session is gone).
- Decided in T1 (security review): Term memory is bounded by `MAX_COLS = 300` (about 40 MB per PTY at 5000 scrollback + 500 rows x 300 cols x ~24 B), by counting EVERY registered PTY, exited ones included, against `MAX_PTYS` until `close_pty` (checked again under the registry lock), and by clearing the scrollback when the output ends. The wire limit for `pty.resize` is 1..300 x 1..500.
- Decided in T1 (security review): `close`/drop mark the PTY `dropped`; the reader (which holds a dup of the master fd) ends at its next wake-up, also while draining after a cap kill. A missing `cwd` is an error, never a fallback to the home directory. `HISTFILE=/dev/null` and `HISTSIZE=0` are set, and `LD_*`, `DYLD_*`, `PROMPT_COMMAND`, `SHELLOPTS`, `BASHOPTS`, `PS4` are dropped from the env, `spec.env` included. `PtySpec.sandbox`/`env` come from trusted code only, never from model arguments; `unsandboxed_by_request()` is also true for `BestEffort` without a policy, so consumers read `sandbox == None` as "unconfined".
- Decided in T1 (security review): Windows loads `conpty.dll` by bare name inside portable-pty (System32 does not have it, so the search reaches the cwd and `PATH`). `open_pty` calls `synthesis_sandbox_win::harden_dll_search()` once per process: `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)`, process-wide and irreversible, and refuses to open if it fails. This adds one `unsafe` call to the existing unsafe-exception crate; unverified on a real Windows machine.
- Decided in T1: limits: at most `MAX_PTYS = 4` registered PTYs per session, and PTYs plus spawned processes stay under `Limits::max_live_procs`; the raw output counts in `max_log_bytes` and `max_session_log_bytes` like any process log. `pty.input` writes are capped at 64 KiB per call (`write_input` is blocking: call it off an async executor thread).
- Decided in T1: Windows has no PTY flood/kill red-team test (they need a POSIX shell, `yes` and `kill -0`); Windows CI runs spawn, echo, resize (`mode con` under cmd, `stty size` under Git Bash) and the env test. ConPTY with real programs stays unverified (section 4).

### Decided in B1 (PR `synthesis-browser` screencast)

Decided in B1 (screencast in `synthesis-browser`; runtime API `view(on, max_w, max_h)` = the `browser.view` method, `frames()` = the consumer end; protocol messages stay B6):
- Decided in B1: `Page.screencastFrame` events are intercepted in the CDP dispatcher and never enter the event broadcast, so a frame flood cannot lag the policy listener (which fails closed) and frames cannot reach the capture logs.
- Decided in B1: the ack is sent when the consumer takes a frame (`FrameStream::next`), together with the ids of frames the slot replaced meanwhile, so a stalled consumer leaves Chrome with its in-flight frames unacked and it stops sending; a resumed consumer is acked for all of them. The 30 fps cap is a 33 ms minimum between takes.
- Decided in B1: the view is bound to the tab that is current when `view(true)` runs; calling `view(true)` again re-targets it (B2 does so on a tab switch); frames of any other session are dropped. `view(false)` clears the slot and sends `stopScreencast`.
- Decided in B1: `max_w`/`max_h` outside 1..=4096 are refused (the decode cap of 2.3). The headless viewport stays at the launch size; `device_pixel_ratio` in the frame metadata is 1 until B3 resizes the viewport.
- Decided in B1: metadata beyond Chrome's own (`deviceWidth/Height`, `pageScaleFactor`, `offsetTop`, scroll offsets, `timestamp`) is only `device_pixel_ratio`; the frame `seq` counts frames received from Chrome, so a gap shows frames the consumer skipped.
- Decided in B1 (review round 1): a frame whose base64 `data` exceeds 16 MiB is dropped before it is copied and still acked; metadata is sanitized (non-finite, negative sizes or beyond 1e7 become 0, and 0 means unknown, so B3 must not divide by a 0 size); `Frame` has no pixels in its `Debug`; every `view` starts a new generation that accepts no frame until Chrome answered `startScreencast`, so frames and acks of the previous cast of the same tab are never used; `view` calls are serialised; `FrameStream::next()` returns `None` once the runtime is closed or the connection is gone (a view turned off only stops frames).
- Decided in B1: the view deliberately bypasses `guard_capture` (the `fill_secret` capture guard): frames go to the person's own window and never to the model, events, store or artifacts. B2 must call `view(true)` again after `select_tab`, `new_tab`, `close_tab` or a popup; the view never follows a tab by itself.

### Decided in B2 (PR desktop browser pane, view only)

- Decided in B2: the pane attaches to the session's browser once the agent has launched it (`BrowserTap`, a weak handle; the runtime is the session's, so whichever call wins the launch, main agent or sub-agent, registers it) and starts nothing itself; the engine method `browser.view` that launches a browser stays in B6. In WSL mode there is no pane yet and the tab keeps its earlier body (saved screenshots and the call list).
- Decided in B2: the desktop reaches the browser only through `synthesis_browser::PageView` (live view, frames, navigation, reload, back, forward, tabs, policy): no snapshot, screenshot, click, fill, console or network access.
- Decided in B2: `headed` is forced off whenever a view is attached (the desktop hosts the view), which overrides `synt.browser.headed = true`; the key keeps its meaning for the CLI and `synt run`.
- Decided in B2: the URL bar passes the browser's own `UrlPolicy` before anything is sent (`navigate` checks it again); a refusal shows its reason in the bar area and never navigates. "Open in your browser" (gpui `open_url`: NSWorkspace, ShellExecuteW, xdg open without a shell) is offered only for a refused address the person typed that is http(s), has no whitespace and is at most 200 characters; it is shown whole beside the button and disappears as soon as the bar text changes. Never for URLs the agent produced.
- Decided in B2: the bar shows the page's address as `redact_url` made it, so Enter on an unedited bar does nothing (Reload reloads the real page). URL, title and tab titles from the page are cut to 2048 characters and the strip lists at most 32 tabs; a typed address over 2048 characters is refused.
- Decided in B2: view calls are applied by ONE worker task that applies the latest wish (hide, show, tab change); the page poll (1 s) and a view that showed no frame for 5 polls ask the worker to point the view at the current tab again. Frames are decoded off the UI thread, over 4096 px per side refused before the decode, only the newest kept.
