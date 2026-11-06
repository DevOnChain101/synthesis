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

TUI view (`src/tui_view/`, phase 2): native panels on a sample `Snapshot` (`sample.rs`; the real view-model
replaces it), metrics from `cli.html` `#view1` (`layout.rs`), topology wires (`topo.rs`). Layout tests use
gpui test support. Offscreen PNG: `cargo run -p synthesis-desktop --features shot -- --shot out.png --size 1400x900
[--hide session,tools,...]` (headless Metal/wgpu, no window server). Topology mode L (tall and narrow) is not ported.

### Feature gaps against the TUI (view-model status)

Done in `synthesis-view`: workspaces and sessions, running/peers, usage, status, topology, activity,
tools and skills, conversation. Not done, with where the data would come from:

- Browser screenshot stream: TODO. There is no screenshot event today; the source would be
  `synthesis-browser` (a new `browser.screenshot` event with an `ArtifactRef`, read back through
  `Reader::artifact_bytes`). `Event::BrowserLoginRequired` is already in the stream and can be shown.
- Settings write API (phase 2): TODO. Reads and `persist_user_tui` live in `synthesis-settings`; a
  `SettingsModel` with typed edits and validation would sit next to it, then be re-exported here.
- Multi-session tabs: TODO. `ViewState` is one session; tabs are a `Vec<ViewState>` keyed by session id
  in the desktop, fed by one event channel per `engine` run. Needs the engine to host several sessions.
- Peers refresh: `ViewState::set_peers` takes the harness rows; the desktop must poll
  `synthesis-harness` `peer` listing (not wired).
- Uncommitted `+N -N` needs the host's trust decision per folder (`LoadOpts::trusted`) and runs git
  with a 5 s limit per call, so load off the UI thread.
- Interactive PTY: not wanted (owner decision).
