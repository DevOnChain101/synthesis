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
  paints; `convo`, `engine`, `theme` are plain Rust, unit-tested without a window.
- `engine.rs`: `synthesis_app::run::prepare(.., Front::Native(Native { out, approver, input }))` builds
  the harness in process. The host supplies the JSONL event writer (parsed to typed `Envelope`s and sent
  to the window over a channel), the `Approver` (pending map answered by the UI) and the
  `InputProvider` (questions; `NoInput` for now). `Front::Native` behaves like `Front::Tui` otherwise.
- `convo.rs`: reducer over the typed event stream (conversation items, tool rows, approvals, plan,
  agents, cost, changed files). Phase 1 makes it the shared view-model of both views: the TUI view
  maps it to the TUI's panels (`synthesis-tui-render` is the source of the palette and later of the
  panel model), the Agent view to a conversation layout.
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
1. Agent view on `engine` + `convo` (send, stream, approve/deny), then TUI view panels from the same state.
2. Settings from `synthesis-settings` (model, permissions, appearance), stored-session resume.
3. Plan/question dialogs, browser/verification panels, Linux and Windows builds and CI jobs.

## 5. Known gaps

Views are placeholders; `InputProvider` answers nothing; no stored-session resume; no
plan/question dialogs, browser or verification UI; Linux/Windows unbuilt (no ConPTY need any more);
no CI job for the desktop crate; no screenshot tooling (the archived offscreen `--shot` feature was dropped).
