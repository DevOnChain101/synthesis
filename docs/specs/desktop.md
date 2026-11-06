# Spec: Desktop app (native GPUI): draft 0.2, phase 0

Replaces the archived spec (tag `archive/ui-before-removal`, `docs/specs/desktop.md`: Mode 1/Mode 2,
onboarding, OS credential store). Owner request 2026-11-06; CLAUDE.md "Status" has the one-line note.

## 1. Goals and non-goals

A native GPUI app (not Tauri, no webview) for macOS, Linux and Windows that is the desktop version
of the classic TUI design, in this repo, running the Rust engine in process.

- Exactly two views, plus Settings: **TUI** (the classic TUI design drawn natively from engine data),
  **Agent** (conversation, tool calls, approvals, plan/questions) and **Settings** (slide-in in
  `cli.html`; a view in the rail for now). The design reference is `~/Vibe/synthesis-cli-ui/cli.html`
  (`#view1` TUI, `#view2` Agent) with the platinum palette (`docs/tui-palette.md`), not its blue/green.
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
  both UIs at once. Fonts: Inter (UI sans) and JetBrains Mono (TUI view), SIL OFL, licences in
  `apps/desktop/assets/fonts/*-OFL.txt`; icons: Lucide through `gpui-kit-assets`.
- Pins (exact): `gpui-pre =0.3.10` (the maintainers' snapshot of Zed's gpui), `gpui-component =0.8.1`,
  `gpui-pre-platform =0.3.10` with `font-kit`, `gpui-kit-assets =0.8.1`; macOS adds `gpui-pre-apple`
  with `runtime_shaders` (Command Line Tools only, no Xcode). Never mix with a git `gpui`.

## 3. Build matrix

- `cargo build -p synthesis-desktop` (run `target/debug/synthesis-desktop`); `cargo test -p synthesis-desktop`.
- The root `Cargo.toml` has `default-members` without the desktop crate, so plain `cargo build/test`
  skip it. `--workspace` ignores `default-members`, hence the CI commands carry `--exclude synthesis-desktop`.
- macOS: verified (phase 0). Linux: needs the gpui-pre-linux system libraries (X11 and Wayland:
  xcb, xkbcommon, wayland, plus fontconfig/freetype, and Vulkan loader/headers for the wgpu
  renderer), not yet built. Windows: gpui-pre-windows (DirectX 11/DirectWrite), not yet built.
  Client-side window controls (minimise/maximise/close) are missing for Windows and Linux.

## 4. Phases

0. Shell (done): window, rail (TUI, Agent, Settings), placeholders, palette, fonts, `Front::Native`,
   engine and reducer restored and green against the current crates.
1. Agent view on `engine` + `synthesis_view::convo` (send, stream, approve/deny), then TUI view panels from the same state.
2. Settings from `synthesis-settings` (model, permissions, appearance), stored-session resume.
3. Plan/question dialogs, browser/verification panels, Linux and Windows builds and CI jobs.

## 5. Known gaps

Views are placeholders; `InputProvider` answers nothing; no stored-session resume; no
plan/question dialogs, browser or verification UI; Linux/Windows unbuilt (no ConPTY need any more);
no CI job for the desktop crate.

TUI view is LIVE (phase 2b-1): `engine.rs` mirrors `synt tui` (prepare + `round` + `Host`), `live.rs` holds the
dialogs/queue/history/focus rules, `tui_view/snapshot.rs` builds the painted `Snapshot` from `ViewState`,
`driver.rs` pumps engine messages (batched, one redraw per 16 ms), handles keys, completion, resume and
store refresh, `chat.rs`/`dialog.rs` paint the conversation and the permission/plan/question dialogs.
`--sample` keeps the sample data; `--shot ... --live SCRIPT --prompt T [--answer once|deny|always]` renders a
scripted (fake provider, `--features shot,test-provider`) conversation on the real engine. Not yet: model/resume
pickers and slash commands beyond /help and /exit (a notice says so), image paste, "Other" free text in
question dialogs, keyboard scroll-into-view in Workspace/Running, Linux/Windows/IME verification.

TUI view (`src/tui_view/`, phase 2): native panels on a sample `Snapshot` (`sample.rs`; the real view-model
replaces it), metrics from `cli.html` `#view1` (`layout.rs`), topology wires (`topo.rs`). Layout tests use
gpui test support. Offscreen PNG: `cargo run -p synthesis-desktop --features shot -- --shot out.png --size 1400x900
[--hide session,tools,...]` (headless Metal/wgpu, no window server). Topology mode L (tall and narrow) is not ported.

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

### Agent view (phase 3, `src/agent_view/`)

`cli.html` `#view2` (`.parts/p5.js`; the current `cli.html` folded it into view 1 with the bottom tabs): grid
`260 | 1fr | 430`, sub-agent split `2fr | 1fr` (min 220). The design has no breakpoints; `agent_view/layout.rs`
extends them like the TUI view's (230|320|360 below 1400, 210|300|320 below 1200, 180|300|260 below 1000) and
re-uses its header/footer/height-tier tokens. Elements and their data:

| Element | Source |
|---|---|
| Conversation, tool cards, inline approval rows | `Convo` (same reducer as the TUI view) |
| Approval / plan / question dialog | `widgets::dialog` (shared with the TUI view), `Live.dialogs` |
| Sub-agent switch + transcript pane | `AgentPanes.subs` (new): `subagent.*` + events carrying `agent_id` |
| Plan (todo list) | `Convo.plan` |
| Commands tab (read-only history) | `AgentPanes.commands` (new): Bash tool calls (`tool.output` tail, exit code from `[exit code N]`) and `process.*` |
| Verify tab | `AgentPanes.verify` (new) + stored report (`Reader::verification_reports`) read off-thread |
| Browser tab | `AgentPanes.browser` (new): `browser_*` calls, last URL, newest screenshot path |
| Diff tab | `AgentPanes.files` (new): `ToolCompleted.diff` summed per file (+N -N), click expands the diff |
| Files tab | `project::scan`, trusted folders only (`--trust-project`); a changed file shows its `git diff` |
| Topology, Usage, Session, Tools, Activity, status line | `snapshot::build` (shared with the TUI view) |
| Session tabs | one engine + `Live` per tab (max 4), "+" opens a new one, a Workspace session row resumes one |

Backend added: `synthesis_view::agent` (`AgentPanes`, `read_report`) with tests, fed by `ViewState::apply`.

Known gaps / TODO:
- Browser: events do not carry image bytes, only the artifact path in the `browser_screenshot` result; the tab
  shows that PNG (<= 8 MiB) and the call list. TODO: a live page stream needs a browser-runtime event with frames.
- `process.*` events are handled but the terminal runtime does not emit them yet; the commands tab lists Bash calls.
- Concurrent engines share process-global state (`set_learned_skills`): closing one tab resets learned skills for
  the others until their next task. The Agent view ignores `--resume/--continue` (the TUI view owns them).
- The Agent view has no completion popup, history search or sidebar focus (TUI-only keys); Ctrl+C interrupts, it does not quit.
- The sub-agent switch appears only when a sub-agent exists. At 900x520 the conversation keeps little room.
- The key hook of the hidden view is off (`TuiView.active`, `AgentView.shown`), set by the shell.

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
