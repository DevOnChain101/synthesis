# Spec: Desktop app (native GPUI): draft 0.2, phase 0

Replaces the archived spec (tag `archive/ui-before-removal`, `docs/specs/desktop.md`: Mode 1/Mode 2,
onboarding, OS credential store). Owner request 2026-11-06; CLAUDE.md "Status" has the one-line note.

## 1. Goals and non-goals

A native GPUI app (not Tauri, no webview) for macOS, Linux and Windows that is the desktop version
of the classic TUI design, in this repo, running the Rust engine in process.

- One interface, plus a placeholder and Settings: **TUI** (the classic TUI design drawn natively from engine
  data, `cli.html` exactly), **Agent** (a different design, later: the page says "Coming soon") and **Settings**
  (slide-in over the current view). The design reference is `~/Vibe/synthesis-cli-ui/cli.html` with the platinum
  palette (`docs/tui-palette.md`), not its blue/green. The first Agent view (`agent_view/`, built on `#view2`) was
  removed on 2026-11-07 by owner decision; its chat pane, diff/terminal/browser panes and project tree live on in
  the TUI view's centre panel.
- Non-goals (owner decisions): no launchpad/landing/Ask page; no embedded terminal emulator and no
  PTY pane (the TUI view is panels fed by backend events, not alacritty/xterm/vt100); no desktop-control
  feature; no keychain or credential storage (ADR-011: API keys via `SYNT_*` env, `{env:}`/`{file:}`).

## 2. Architecture

- Crate `apps/desktop` (`synthesis-desktop`, bin `synthesis-desktop`). `src/ui.rs` is the only file that
  paints; `engine`, `theme` are plain Rust, unit-tested without a window. All view state lives in
  `crates/synthesis-view` (below).
- `engine.rs`: `synthesis_app::run::prepare(.., Front::Native(Native { out, approver, input }))` builds
  the harness in process. The host supplies the JSONL event writer (parsed to typed `Envelope`s and sent
  to the window over a channel), the `Approver` (pending map answered by the UI) and the
  `InputProvider` (questions; `NoInput` for now). `Front::Native` behaves like `Front::Tui` otherwise.
- `crates/synthesis-view` (`synthesis-view`): UI-agnostic view models and reducers, no terminal, no
  GPUI, no drawing. The classic TUI and the desktop both depend on it. One `state::ViewState` per
  session: `apply(&Envelope)` folds an event into every panel; `load_from_store(&Reader,
  &WorkspaceContext, &StoreLoad)` fills the store panels (and replays one session). Panels:
  `convo` (messages, tool cards, pending approvals, sub-agents, todo plan; moved from
  `apps/desktop/src/convo.rs`), `status` (`StatusModel`: context used/window, tokens in/out/cache and
  hit rate, cost, turns, tools, model/effort/mode, cwd, uptime; `SandboxStatus`, `ContextStatus` moved
  from `synthesis-tui`), `usage` (`UsageHistory`, per-day history; moved), `workspaces`
  (`WorkspaceList`: root, label, session count, `+N -N` when the host marks the folder trusted;
  `SessionRow` with title or `(untitled)`, status, `updated_at`, `is_live`; empty and closed
  sessions hidden by `Reader::list_used_sessions`), `running` (`RunningList`: live sessions and
  `PeerRow`s from the `/peers` listing), `activity` (`ActivityModel`, `ToolsSkillsModel`),
  `topology` (`Topology`: stage pipeline Plan, Guard, Orch, Exec, Verify, Learn, and the
  user/core/guards/skills/agents/hooks/mcp/exec/memory nodes with state and counters), `model`
  (the transcript-shaped `Session` all of those read; moved from `synthesis-tui-render`),
  `project` (git tree/diff scan with the trust gate; moved from `synthesis-tui`), `sessions`
  (`list_saved`; moved). `synthesis-tui` and `synthesis-tui-render` re-export the moved items under
  their old paths and keep colours, layout and ANSI. Frontends decide `is_live` and the trust gate
  by closures in `LoadOpts`; the crate never reads the environment itself.
  Dependency note: `synthesis-view` still depends on `synthesis-tui-base` (only for `is_js_space` and
  the bounded process runner `imgclip::RealExec` that `project` runs git with) and on
  `synthesis-harness` (only `sanitize_for_display`); moving both into a small base crate is follow-up.
- `theme.rs` reads the TUI's own tokens (`synthesis-tui-render::theme`), so palette changes land in
  both UIs at once. Fonts: Inter (UI sans), Fira Code (TUI and Agent views, the design's `--mono`) and JetBrains Mono (settings font choice), SIL OFL, licences in
  `apps/desktop/assets/fonts/*-OFL.txt`; icons: Lucide through `gpui-kit-assets`.
- Pins (exact): `gpui-pre =0.3.10` (the maintainers' snapshot of Zed's gpui), `gpui-component =0.8.1`,
  `gpui-pre-platform =0.3.10` with `font-kit`, `gpui-kit-assets =0.8.1`; macOS adds `gpui-pre-apple`
  with `runtime_shaders` (Command Line Tools only, no Xcode). Never mix with a git `gpui`.

## 3. Build matrix

- `cargo build -p synthesis-desktop` (run `target/debug/synthesis-desktop`); `cargo test -p synthesis-desktop`.
- The root `Cargo.toml` has `default-members` without the desktop crate, so plain `cargo build/test`
  skip it. `--workspace` ignores `default-members`, hence the CI commands carry `--exclude synthesis-desktop`.
- CI (`ci.yml` job `desktop`, required by `ci-ok`): `cargo build`, `clippy -D warnings`, `test` on ubuntu, macos and windows. Linux needs the
  gpui-pre-linux system libraries (xcb, xkbcommon, wayland, fontconfig/freetype, Vulkan loader/headers for the wgpu renderer; the apt list is in
  the job); Windows is gpui-pre-windows (DirectX 11/DirectWrite). Client-side window controls (minimise/maximise/close) are missing for Windows and Linux.
- CI runtime smoke (same job, `scripts/desktop-smoke.sh`, build with `--features shot,test-provider`; artifact `desktop-shots-<OS>`: download it from the
  run's Summary page, unzip, open the PNGs). macOS and Linux render offscreen `tui-1400x900.png`, `tui-1000x700.png`, `settings-1400x900.png`
  (`--shot ... --view settings --check`: size is the asked one times a whole scale, and more than 16 distinct colours); macOS uses headless Metal,
  Linux wgpu on lavapipe (`mesa-vulkan-drivers`). gpui-pre-windows has no headless renderer, so Windows launches the real app on `--sample` with
  `--exit-after-first-frame` and requires the "first frame drawn" line (log in the artifact; no PNG). Not proven: real window on macOS/Linux, a real GPU,
  HiDPI beyond the runner's scale, IME, input events, Windows pixels.
- Release (`release.yml` job `desktop`, `scripts/package-desktop.mjs`): archives `synthesis-desktop-<version>-<target>`; docs/RELEASE.md "Desktop".

  | Target | Runner | Archive | Contents | Status |
  |---|---|---|---|---|
  | aarch64-apple-darwin, x86_64-apple-darwin | macos-latest | zip | `Synthesis.app` (ad-hoc signed) | required |
  | x86_64-unknown-linux-gnu | ubuntu-22.04 | tar.gz | `synthesis-desktop`, `.desktop`, hicolor icons, README.txt | required |
  | x86_64-pc-windows-msvc | windows-latest | zip | `Synthesis.exe` (icon embedded, GUI subsystem in release) | required |
  | aarch64-unknown-linux-gnu, aarch64-pc-windows-msvc | ubuntu-22.04-arm, windows-11-arm | tar.gz, zip | as above | experimental |

- Names: the cargo binary stays `synthesis-desktop` (technical; it is not the `synt` command and there is no alias). Display name "Synthesis"
  (bundle name, window/app name in the menu, `.desktop` Name, `Synthesis.exe`). Bundle identifier `dev.synthesis.desktop`
  (`[package.metadata.bundle]`). Icons: `assets/icons/` (committed .icns/.ico/png, from `design/01-brand/app-icons`); fonts are compiled in.
- No signing or notarization (no secrets in the pipeline). Started from Finder the cwd is `/`; the app then uses the home folder.

## 4. Phases

0. Shell (done): window, title bar (TUI, Agent, Settings icons), placeholders, palette, fonts, `Front::Native`,
   engine and reducer restored and green against the current crates.
1. Agent view on `engine` + `synthesis_view::convo` (send, stream, approve/deny), then TUI view panels from the same state.
2. Settings from `synthesis-settings` (model, permissions, appearance), stored-session resume.
3. Plan/question dialogs, browser/verification panels, Linux and Windows builds and CI jobs.

## 5. Known gaps

Views are placeholders; `InputProvider` answers nothing; no stored-session resume; no
plan/question dialogs, browser or verification UI; Linux/Windows run in CI but no maintainer has used them on a real machine (no ConPTY need any more).

TUI view is LIVE (phase 2b-1): `engine.rs` mirrors `synt tui` (prepare + `round` + `Host`), `live.rs` holds the
dialogs/queue/history/focus rules, `tui_view/snapshot.rs` builds the painted `Snapshot` from `ViewState`,
`driver.rs` pumps engine messages (batched, one redraw per 16 ms), handles keys, completion, resume and
store refresh, `chat.rs`/`dialog.rs` paint the conversation and the permission/plan/question dialogs.
`--sample` keeps the sample data; `--shot ... --live SCRIPT --prompt T [--answer once|deny|always]` renders a
scripted (fake provider, `--features shot,test-provider`) conversation on the real engine. Not yet: model/resume
pickers and slash commands beyond /help and /exit (a notice says so), image paste, "Other" free text in
question dialogs, keyboard scroll-into-view in Workspace/Running, Linux/Windows/IME verification.

Slash commands (all built-ins are wired; `live.rs` `WIRED`, the TUI's semantics): `/files` and `/diff [file]` switch
the centre panel to its file explorer or diff tab (`/diff FILE` opens that file's diff) and post nothing in the chat;
an untrusted folder is said by the file explorer itself. `/peers` posts the TUI's table.
`/name` shows the name peers address the session by; it is fixed for the run (`--name`), as in the TUI, so it
does not rename the stored title. `/focus [on|off]` hides tools, thinking, approvals and agent rows (user,
assistant, result and notices stay). `/permissions allow|remove RULE` edits the rules of this session only
(`is_well_formed_rule`, nothing is written to settings, as in the TUI; `remove` also drops deny/ask entries).

TUI view (`src/tui_view/`), rebuilt 1:1 on the owner's design `~/Vibe/synthesis-cli-ui/cli.html` (owner decision
2026-11-07, "dung lai dung cli.html"): geometry and typography are the design's (Fira Code 500, 12.5px / 1.55,
panels radius 8, the breakpoints 1400/1200/1000 and the height tiers 750/610/860, the `fitRight` rules, the
topology modes A, B and T), only the colours are the platinum tokens. Panels: top bar; left Workspace, Running,
Session, Tools & Skills, Recent Activity, User Prompt; centre header (status, latest prompt, context bar, folder,
files | LOC), tabs chat | diff | terminal | browser | file explorer, conversation, prompt, transcript switch, mode
bar; right Synthesis Topology, Agent Status + Context, Usage & Metrics; key hint footer. Shared chrome
(`widgets/chrome.rs`: top bar, panel, tabs, footer, chat header); the diff, terminal (command history, read-only, no
PTY) and browser panes are `widgets/panes.rs`, the file explorer `tui_view/tabs.rs`.

`synt.tui.panels.*` keys (unchanged, so no settings.json breaks): `session` = Session, `tools` = Tools & Skills,
`activity` = Recent Activity and User Prompt, `status` = Agent Status, `tokens` = Context, `topology` = Synthesis
Topology, `usage` = Usage & Metrics; `mcp` has no panel any more (MCP is in the Topology card and the Tools &
Skills row) and is ignored. Workspace, Running and the chat cannot be hidden. The labels in
`synthesis-settings::TUI_PANELS` name both the terminal and the desktop panel.

Data: `sample.rs` is the design's own sample (`STATE`), `snapshot.rs` builds the same `Snapshot` from the live
`ViewState`. Usage & Metrics rows come from `synthesis-view::usage` (`UsageHistory::metrics`: tokens, tool calls,
files modified, commands run, browser requests, MCP calls over the last 7 days; the bar is that week's share of the
last 30 days), counted per day from `tool.started` in `synt.db` (`synthesis-session::UsageDay`).

Design acceptance: `tests/design/cli-rects.json` holds `getBoundingClientRect` of the design's panels at 900x520,
1000x700, 1200x800, 1400x900 and 1920x1080 (headless Chrome, `cli.html` copied beside its assets); `design_tests.rs`
lays the sample out in GPUI at the same sizes and compares (1px; 1.5px for panels of text rows, because gpui rounds a
19.375px line box up to 19.5px; wider only where Chrome and gpui wrap text differently, listed in `tol`). Run
`cargo test -p synthesis-desktop --lib design_ -- --nocapture` for the table. Offscreen PNG: `cargo run -p
synthesis-desktop --features shot -- --shot out.png --size 1400x900 [--hide session,tools,...]` (headless
Metal/wgpu, no window server); `node scripts/design-compare.mjs <ref-dir> <shots-dir> <out-dir>` writes
[design | app | difference] images from Chrome renders of `cli.html` (`ref-WxH.png`) and the shots.
Not ported: topology mode L (tall and narrow falls back to T). The "Latest user prompt" scrolls (16s, repeating,
`synt.tui.motion`) only when it does not fit.

Window shell (`ui.rs`): a 30px title bar over the content, no side rail. The bar follows the Tauri app's chrome
(`#chrome`: `.cb` buttons 26x22, radius 6, hover and pressed backgrounds): the app mark and drag region on the left,
the TUI and Agent icons in a group, then the Settings gear; each has a tooltip with its shortcut (Cmd/Ctrl+1,
Cmd/Ctrl+2, Cmd/Ctrl+,). The content below is the design's 1400x900 etc. exactly (the bound tests lay the view out
without the bar; `ui::tests` checks that the bar adds 30px above it and nothing beside it). macOS: transparent
native title bar with the traffic lights at (14, 9) and 78px reserved for them. Windows and Linux: client-side
decorations, the bar is the drag region (`WindowControlArea::Drag`) and draws minimise / maximise / close
(`WindowControlArea::Min|Max|Close`, close hovers red). Not verified without a real window on each OS.

Centre tabs (`TabState`, kept per session id, so a session left and resumed finds its tabs as they were; the strip
shows counters; Ctrl+1..5 switch, Esc returns to the chat, PageUp/PageDown scroll the pane):
- diff: the session's changed files (`AgentPanes.files`) with `+N -N`; the newest opens by itself, a click expands a
  file's syntax-coloured diff; `/diff FILE` opens that file.
- terminal: the command history (`AgentPanes.commands`, from `process.*` and Bash tool events): command, exit code,
  duration, output tail, running state. Read-only, not a terminal.
- browser: the page, the selected saved screenshot (`browser.frame`; the latest by default, history rows select
  another; a PNG inside the session's artifact folder only, <= 8 MiB) and the browser calls with state and duration.
- file explorer: `project::scan` (git's file list; trusted folders only), directories open on demand (the parents of
  changed files open first), changed files marked M/A/D/?, a click previews a text file read-only (<= 256 KB and 2000
  lines; refuses paths outside the folder, links out of it, secret files and binary content).

### Feature gaps against the TUI (view-model status)

Done in `synthesis-view`: workspaces and sessions, running/peers, usage, status, topology, activity,
tools and skills, conversation. Not done, with where the data would come from:

- Browser screenshots: done as history, not a live stream. `browser.frame` (path, origin, w, h, bytes) feeds
  `AgentPanes.frames` and `.screenshot`; the file is opened only through `viewable_screenshot`.
  `Event::BrowserLoginRequired` is already in the stream and can be shown. Page title changes are not an event.
- Commands tab: `process.spawned|output|exited` (Bash, terminal_spawn) give the real exit code and background
  rows; `[exit code N]` parsing stays as the fallback for old sessions. A background process nobody polls
  again never gets `process.exited`. Verification has only `verification.completed` (no per-check progress).
- Settings write API: done in `crates/synthesis-settings-ui` (section 6). The GPUI form is not built.
- Multi-session tabs: TODO. `ViewState` is one session; tabs are a `Vec<ViewState>` keyed by session id
  in the desktop, fed by one event channel per `engine` run. Needs the engine to host several sessions.
- Peers refresh: `ViewState::set_peers` takes the harness rows; the desktop must poll
  `synthesis-harness` `peer` listing (not wired).
- Uncommitted `+N -N` needs the host's trust decision per folder (`LoadOpts::trusted`) and runs git
  with a 5 s limit per call, so load off the UI thread.
- Interactive PTY: not wanted (owner decision).

### Agent view (removed)

The first Agent view (phase 3) is gone; the Agent page is "Coming soon". What it built on `synthesis_view::agent`
(`AgentPanes`: sub-agents, commands, verification, browser calls and frames, changed files) is used by the TUI view's
tabs and its sub-agent transcript (the `* main / developer` switch under the prompt opens a transcript beside the chat).
Known gaps: browser events carry no image bytes (screenshots are the saved artifacts, no live stream); concurrent
engines share process-global state (`set_learned_skills`), which no longer matters with one session on screen.

## 6. Settings (backend: `crates/synthesis-settings-ui`)

**What the design has.** `cli.html` has no settings form with these sections. USER & PROMPT, MEMORY,
EXECUTION, GUARDS, HOOKS, MCP, SKILLS and SUB-AGENTS are the status cards of the topology (`topology1`,
`topology2`); its only settings hook is the `synthesis-settings` message (theme, font, font size, opacity), which is
appearance and has no backend key (the desktop owns it). The sections below are the same titles, filled with the real keys.

**Crate.** Pure data, no UI dependency. Separate from `synthesis-settings` because it reuses the validators of
`synthesis-harness` (`AutoCompact`, `ToolSearchMode`, `ShellSettings`), and the harness depends on `synthesis-settings`.
- `SettingsSchema::new()`: `Field { key (dotted path), section, label, kind (Bool | Enum | Int{min,max} | Float{min,max} |
  List | ReadOnly), default, help, scope (UserOnly | Layered | Tighten), restart, edit (Yes | Confirm(reason) | No(reason)) }`.
- `SettingsStore::open(&LoadContext, ProjectTrust, managed_path, --settings file)`: `entries` (value in force, `Origin`
  Default/User/Project/Local/Flag/Managed, the USER file's own value, and the layers that hold the key but may not set it, each
  with the reason), plus the loader's `diagnostics` and `errors`.
- `set(key, value)` / `set_confirmed` (for `Edit::Confirm` keys) / `unset(key)` write the USER `settings.json` only, with
  `synthesis_settings::edit_json_atomic` (temp file and rename, mode kept, 0600 when new, a file that cannot be parsed or a parent that
  is not an object is never overwritten, every other key and the key order stay). Checks, in order: known key, edit policy,
  managed lock (`SetError::Locked`), the kind (type, enum, range), a credential-shaped literal, then the loaders' own validators on the
  document about to be written. Errors name the key and never echo a refused secret. Returns the diagnostics after the write.
- MCP servers are not in the store. Add and remove call `synthesis_app::mcp::{add_server, remove_server}`, the code `synt mcp add|remove`
  runs (name check, URL/header/env/args credential guards, closed-schema validation, atomic 0600 write); `synt mcp` now calls them too.
- The TUI `/config` still writes `synt.tui` through `persist_user_tui`; switching it was not zero-risk (it writes the whole subtree and the
  TUI would gain a dependency on this crate). Both produce the same file.

**Sections and keys.** Scope: `user file only` = project, local, `--settings` and managed copies are ignored (shown refused with that
reason); `layered` = user, `--settings`, managed, and project/local only with a trusted project; `may tighten` = an untrusted project may
also set it. Restart: a running session keeps the old value. Editable = the UI may write it (always to the user file).

| Key | Section | Scope | Restart | Editable from the UI |
|---|---|---|---|---|
| `synt.memory.enabled` | MEMORY | user file only | yes | yes |
| `synt.memory.reflection` | MEMORY | user file only | yes | yes, after confirm: this sends task digests to your model provider |
| `synt.memory.daily_budget_usd` | MEMORY | user file only | yes | yes |
| `synt.memory.disabled_workspaces` | MEMORY | user file only | yes | yes |
| `synt.images.retentionDays` | MEMORY | user file only | yes | yes |
| `synt.budget.max_turns` | EXECUTION | layered (project if trusted) | yes | yes |
| `synt.budget.max_tool_calls` | EXECUTION | layered (project if trusted) | yes | yes |
| `synt.budget.max_verify_attempts` | EXECUTION | layered (project if trusted) | yes | yes |
| `synt.verification.max_attempts` | EXECUTION | layered (project if trusted) | yes | yes |
| `synt.verification.inferred_only` | EXECUTION | layered (project if trusted) | yes | yes, after confirm: allow_passed lets inferred checks alone mark a task passed |
| `synt.verification.checks` | EXECUTION | layered (project if trusted) | yes | no: each check is a shell command the verifier runs; edit settings.json by hand |
| `synt.context.auto_compact` | EXECUTION | user file only | yes | yes |
| `synt.context.auto_compact_threshold` | EXECUTION | user file only | yes | yes |
| `synt.shell.user_commands_model_turn` | EXECUTION | user file only | yes | yes |
| `synt.shell.user_commands_unsandboxed` | EXECUTION | user file only | yes | yes, after confirm: this runs your `!` commands without the OS sandbox |
| `synt.modelInfo.fetch` | EXECUTION | user file only | yes | yes |
| `synt.compat.write_node_transcripts` | EXECUTION | layered (project if trusted) | yes | yes |
| `synt.browser.login_timeout_sec` | EXECUTION | user file only | yes | yes |
| `synt.browser.allow_unsandboxed` | EXECUTION | user file only | yes | no: turns the browser sandbox off; edit settings.json by hand |
| `synt.browser.secrets` | EXECUTION | user file only | yes | no: references to credentials; edit settings.json by hand ({env:} / {file:} only) |
| `synt.browser.profiles` | EXECUTION | user file only | yes | no: decides which domains a login may browse; edit settings.json by hand |
| `synt.browser.approved` | EXECUTION | user file only | yes | no: pre-approves logins for unattended runs; edit settings.json by hand |
| `synt.pricing` | EXECUTION | user file only | yes | no: a table keyed by provider/model; edit settings.json by hand |
| `env` | EXECUTION | layered (project if trusted) | yes | no: variables for Bash, hooks and verification; secret-shaped names are refused by the loader, edit settings.json by hand |
| `permissions.defaultMode` | GUARDS | layered (project may tighten) | yes | yes, after confirm: a mode other than default changes what runs without asking |
| `permissions.blockReadsOutsideWorkingDirectories` | GUARDS | user file only | yes | yes, after confirm: turning this off lets the agent read outside the workspace |
| `permissions.allow` | GUARDS | layered (project if trusted) | yes | no: an allow rule grants a tool without asking; add them through the approval prompt (allow always) or by hand |
| `permissions.deny` | GUARDS | layered (project may tighten) | yes | no: rule syntax is validated by synthesis-permissions, not wired here yet; edit by hand |
| `permissions.ask` | GUARDS | layered (project may tighten) | yes | no: rule syntax is validated by synthesis-permissions, not wired here yet; edit by hand |
| `permissions.additionalDirectories` | GUARDS | layered (project if trusted) | yes | no: widens the filesystem the agent may touch; edit by hand |
| `autoMode` | GUARDS | user file only | yes | no: steers the classifier that approves without asking; edit settings.json by hand |
| `hooks` | HOOKS | layered (project if trusted) | yes | no: hooks are commands run with your privileges; edit settings.json by hand |
| `disableAllHooks` | HOOKS | layered (project if trusted) | yes | yes, after confirm: this also switches off hooks you rely on as safeguards |
| `synt.mcp.toolSearch` | MCP | user file only | yes | yes |
| `synt.mcp.env_passthrough` | MCP | user file only | yes | no: names of variables handed to stdio servers, secrets included; edit settings.json by hand |
| `synt.mcp.servers` | MCP | user file only | yes | no: per-server write_tools/read_tools; read_tools weakens the guard, edit by hand |
| `enabledMcpjsonServers` | MCP | layered (project if trusted) | yes | no: approving a project server lets a repository start a process; use --approve-mcp NAME@HASH |
| `enableAllProjectMcpServers` | MCP | layered (project if trusted) | yes | no: lets any repository start processes; edit by hand |
| `disabledMcpjsonServers` | MCP | layered (project may tighten) | yes | yes |
| `synt.peers` | SUB-AGENTS | user file only | yes | no: messaging between sessions (off by default); edit settings.json by hand |
| `synt.tui.logo` | INTERFACE | user file only | no | yes |
| `synt.tui.show_thinking` | INTERFACE | user file only | no | yes |
| `synt.tui.verbose` | INTERFACE | user file only | no | yes |
| `synt.tui.diffs_expanded` | INTERFACE | user file only | no | yes |
| `synt.tui.queue_indicators` | INTERFACE | user file only | no | yes |
| `synt.tui.motion` | INTERFACE | user file only | no | yes |
| `synt.tui.mouse_capture` | INTERFACE | user file only | no | yes |
| `synt.tui.copy_on_select` | INTERFACE | user file only | no | yes |
| `synt.tui.panels.{session,mcp,tools,activity,status,tokens,topology,usage}` | INTERFACE | user file only | no | yes |

Why the "no" keys: an `allow` rule, an extra directory, a project MCP approval, `env_passthrough` and `read_tools` widen what the agent
may do or leak; hooks, verification checks and `autoMode` run or steer commands; browser secrets/profiles/pre-approvals are credential
adjacent (ADR-011: only `{env:}`/`{file:}` references, written by hand); `deny` and `ask` would be safe to add but their rule syntax is
validated in `synthesis-permissions`, not wired here yet (follow-up). A managed file that sets a key locks it (`Locked`).

**Design controls with no backend key** (shown read-only or not at all): the user name and task text (USER & PROMPT: session state);
model and effort (`synt.json` / `state.json`, `/model` `/effort`); safety, policy, sandbox and network toggles (GUARDS: `--sandbox` and
`--network` flags of the run); approval "granular" (only `permissions.defaultMode` and rules); proxy (standard `HTTP(S)_PROXY` variables);
per-skill on/off and planner/coder/reviewer toggles (SKILLS, SUB-AGENTS: files in `skills/` and `agents/`); per-event hook toggles
(pre-tool, post-tool, on-error, on-done: hooks are defined per event, only `disableAllHooks` exists); MEMORY learned/feedback counters
(data, not settings); EXECUTION tool-call, error and success counters (live status); theme, font, opacity (desktop appearance).
