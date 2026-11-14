# Spec: Desktop app (native GPUI): draft 0.2, phase 0

Replaces the archived spec (tag `archive/ui-before-removal`, `docs/specs/desktop.md`: Mode 1/Mode 2,
onboarding, OS credential store). Owner request 2026-11-06; CLAUDE.md "Status" has the one-line note.

## 1. Goals and non-goals

A native GPUI app (not Tauri, no webview) for macOS, Linux and Windows that is the desktop version
of the TUI design, in this repo, running the Rust engine in process.

- One interface, plus a placeholder and Settings: **TUI** (the terminal-style tab, drawn natively from engine
  data, `cli.html` exactly), **Super Agent** (a different design, later: the page says "Coming soon") and **Settings**
  (a centred popup over the TUI view, section 6). The design reference is `~/Vibe/synthesis-cli-ui/cli.html` with the platinum
  palette (`docs/cli-palette.md`), not its blue/green. The first Super Agent view (`agent_view/`, built on `#view2`) was
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
  `InputProvider` (`ChanInput`: plan and question dialogs, answered by the window). `Front::Native` behaves like `Front::Cli` otherwise.
- `crates/synthesis-view` (`synthesis-view`): UI-agnostic view models and reducers, no terminal, no
  GPUI, no drawing. The CLI and the desktop both depend on it. One `state::ViewState` per
  session: `apply(&Envelope)` folds an event into every panel; `load_from_store(&Reader,
  &WorkspaceContext, &StoreLoad)` fills the store panels (and replays one session). Panels:
  `convo` (messages, tool cards, pending approvals, sub-agents, todo plan; moved from
  `apps/desktop/src/convo.rs`), `status` (`StatusModel`: context used/window, tokens in/out/cache and
  hit rate, cost, turns, tools, model/effort/mode, cwd, uptime; `SandboxStatus`, `ContextStatus` moved
  from `synthesis-cli-ui`), `usage` (`UsageHistory`, per-day history; moved), `workspaces`
  (`WorkspaceList`: root, label, session count, `+N -N` when the host marks the folder trusted;
  `SessionRow` with title or `(untitled)`, status, `updated_at`, `is_live`; empty and closed
  sessions hidden by `Reader::list_used_sessions`), `running` (`RunningList`: live sessions and
  `PeerRow`s from the `/peers` listing), `activity` (`ActivityModel`, `ToolsSkillsModel`),
  `topology` (`Topology`: stage pipeline Plan, Guard, Orch, Exec, Verify, Learn, and the
  user/core/guards/skills/agents/hooks/mcp/exec/memory nodes with state and counters), `model`
  (the transcript-shaped `Session` all of those read; moved from `synthesis-cli-ui-render`),
  `project` (git tree/diff scan with the trust gate; moved from `synthesis-cli-ui`), `sessions`
  (`list_saved`; moved). `synthesis-cli-ui` and `synthesis-cli-ui-render` re-export the moved items under
  their old paths and keep colours, layout and ANSI. Frontends decide `is_live` and the trust gate
  by closures in `LoadOpts`; the crate never reads the environment itself.
  Dependency note: `synthesis-view` still depends on `synthesis-cli-ui-base` (only for `is_js_space` and
  the bounded process runner `imgclip::RealExec` that `project` runs git with) and on
  `synthesis-harness` (only `sanitize_for_display`); moving both into a small base crate is follow-up.
- `theme.rs` reads the CLI's own tokens (`synthesis-cli-ui-render::theme`), so palette changes land in
  both UIs at once. Fonts: Inter (UI sans), Fira Code (TUI and Super Agent views, the design's `--mono`) and JetBrains Mono (settings font choice), SIL OFL, licences in
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
  (`--shot ... --view settings --check`: size is the asked one times a whole scale, more than 16 distinct colours, and a text edge density of at least 0.7%: a frame with layout and icons but no glyphs fails; a no-op text system gave 0.5%, real frames 1.1% to 3.2%); macOS uses headless Metal,
  Linux wgpu on lavapipe (`mesa-vulkan-drivers`). gpui-pre-windows has no headless renderer, so Windows launches the real app on `--sample` with
  `--exit-after-first-frame` and requires the "first frame drawn" line (log in the artifact; no PNG). Not proven: real window on macOS/Linux, a real GPU,
  HiDPI beyond the runner's scale, IME, input events, Windows pixels and Windows text (first frame only proves a draw). `gpui_platform` needs the `wayland` and `x11` features on Linux (off by default in gpui-pre-linux: without them the Linux build has a no-op text system and no window backend).
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

0. Shell (done): window, title bar (TUI, Super Agent, Settings icons), placeholders, palette, fonts, `Front::Native`,
   engine and reducer restored and green against the current crates.
1. Super Agent view on `engine` + `synthesis_view::convo` (send, stream, approve/deny), then TUI view panels from the same state.
2. Settings from `synthesis-settings` (model, permissions, appearance), stored-session resume.
3. Plan/question dialogs, browser/verification panels, Linux and Windows builds and CI jobs.

## 5. Known gaps

Wired: the input provider (plan and question dialogs, with "Other" free text), stored-session resume and the
model/resume pickers, every built-in slash command, image paste, and peer polling (`tui_view/driver.rs` `tick`
calls `Engine::peer_rows` every fifth second into `ViewState::set_peers`). Linux/Windows run in CI but no
maintainer has used them on a real machine (no ConPTY need any more).

Chat rows follow the CLI's quiet-chat rules (`tui_view/chat.rs` `visible`, reducer in
`synthesis-view/src/convo.rs`, wording from `synthesis-cli-ui` `chat.rs`). Always shown: budget exceeded, failed
hooks, a browser login to confirm, a refused peer request, work a peer asked for, peer replies and agent
messages, a memory note waiting for review, a failed memory reflection. Internal rows (successful hooks,
memory retrieved/written/quarantined, peer listening/requests/threads/tool calls) are `Item::Internal`: drawn
only with `synt.tui.verbose`, never in `/focus`. A task that did not complete ends with a warning line
(`task unverified|blocked|cancelled`, `interrupted`) or an error line (`task failed`), followed by the checks of its
last verification report; `Done` is the only green end. An exceeded budget also shows as a status chip until the
next task. With `--worktree`, a chat notice and the Session panel (`worktree`, `branch`) give the path and branch;
the finish report (changes, cleanup) is a notice of the next session's chat (stderr on quit).

TUI view is LIVE (phase 2b-1): `engine.rs` mirrors `synt` (prepare + `round` + `Host`), `live.rs` holds the
dialogs/queue/history/focus rules, `tui_view/snapshot.rs` builds the painted `Snapshot` from `ViewState`,
`driver.rs` pumps engine messages (batched, one redraw per 16 ms), handles keys, completion, resume and
store refresh, `chat.rs`/`dialog.rs` paint the conversation and the permission/plan/question dialogs.
`--sample` keeps the sample data; `--shot ... --live SCRIPT --prompt T [--answer once|deny|always]` renders a
scripted (fake provider, `--features shot,test-provider`) conversation on the real engine. Not yet: Linux/Windows/IME
verification.

Slash commands (all built-ins are wired; `live.rs` `WIRED`, the CLI's semantics): `/files` and `/diff [file]` switch
the centre panel to its file explorer or diff tab (`/diff FILE` opens that file's diff) and post nothing in the chat;
the explorer needs no trust. `/peers` posts the CLI's table.
`/send` with no arguments opens a picker docked above the prompt (`send_pick.rs`, the `/model` dialog family): the
live peers of the polled listing (main agents, not this session, not unreachable) grouped by workspace, typing filters
(session, workspace, short id), each row with the Running panel's activity dot, the short id and the last activity
(the registry carries no time, so it is the window's own: now while running, else the poll that saw the state change;
none seen = gray). A pick sets a one-shot chip over the prompt (`to <workspace> > <session>`) holding the `ws:<key>/sess:<id>`
address; the next plain line goes there and a note echoes `sent to ws:abcd1234../sess:12345678..`. Esc (or Ctrl+T) clears
the chip, a command leaves it, a peer that left the listing holds the line with a notice. Peers off or none listed: the
picker's only row says how to enable them. `/send <address> <text>` is unchanged.
`/name` shows the name peers address the session by; it is fixed for the run (`--name`), as in the CLI, so it
does not rename the stored title. `/rename <title>` does (`synthesis_app::rename`, one validator for both frontends):
the title is trimmed, newlines collapse to a space, 1-120 characters, control/bidi/zero-width characters and
secret-shaped text are refused; it is stored with `Store::set_title` and shows at once in the Workspace and
Running lists; without an argument it shows the current title and the usage. `/focus [on|off]` hides tools, thinking, approvals and agent rows (user,
assistant, result and notices stay). `/permissions allow|remove RULE` edits the rules of this session only
(`is_well_formed_rule`, nothing is written to settings, as in the CLI; `remove` also drops deny/ask entries).

TUI view (`src/tui_view/`), rebuilt 1:1 on the owner's design `~/Vibe/synthesis-cli-ui/cli.html` (owner decision
2026-11-07, "dung lai dung cli.html"): geometry and typography are the design's (Fira Code 500, 12.5px / 1.55,
panels radius 8, the height tiers (column breakpoints: see the column rule below) 750/610/860, the `fitRight` rules, the
topology modes A, B and T), only the colours are the platinum tokens. Panels: top bar; left Workspace, Running,
Session, Tools & Skills, Recent Activity, User Prompt; centre header (status, latest prompt, context bar, folder,
files | LOC), tabs chat | diff | terminal | browser | file explorer, conversation, prompt, transcript switch, mode
bar; right Synthesis Topology, Mission (only with a task list) + Context, Usage & Metrics; key hint footer. Shared chrome
(`widgets/chrome.rs`: top bar, panel, tabs, footer, chat header); the diff, terminal (command history, read-only, no
PTY) and browser panes are `widgets/panes.rs`, the file explorer `tui_view/tabs.rs`.

`synt.tui.panels.*` keys (unchanged, so no settings.json breaks): `session` = Session, `tools` = Tools & Skills,
`activity` = Recent Activity (prompts and agent activity merged), `tokens` = Context, `topology` = Synthesis
Topology, `usage` = Usage & Metrics; `mcp` has no panel any more (MCP is in the Topology card and the Tools &
Skills row) and is ignored. Workspace, Running, the chat (with the sub-agent transcript beside it), and the sub-agents
cannot be hidden (owner decision 2026-11-09). Mission (was Agent Status, the agent's task list; key `status`, also
`/panels mission`) is hideable, and shows only while the agent has a task list; `Convo::mission(now)` hides it 5
minutes after all tasks are done, a new list replaces the old, a resumed session clears it. The `* main` switch under
the prompt shows only while sub-agents exist: a row each, `name - state` left and `online - tokens` right; done ->
idle after 1 minute -> removed (with its transcript) after 5 (`synthesis_view::lifecycle`; online time freezes when it
finished; docs/CLI-APP.md has the details). The sub-agents are the Subagents card of Topology; while Topology is hidden a compact Subagents block
(`p-subagents`) takes its place, so the right column always exists and the chat no longer spans to the right edge.
`/panels` lists the panels with their state, `/panels hide-all` hides every hideable one, `/panels show-all`
shows them, `/panels <name> on|off` switches one; it applies at once and saves to `synt.tui.panels` in the user file
(atomic, via `SettingsStore`). The labels in
`synthesis-settings::UI_PANELS` name both the CLI and the desktop panel.

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
Column rule (owner decision 2026-11-07, an approved DEVIATION from the proportions of `cli.html`, whose 250|1fr|500 gave
the chat 43% of a 1400px window): the chat column (tabs, conversation, prompt, mode bar) is at least 55% of the window
width at every width and fills the content height. `layout::cols`: left column 250 from 1400, else 220; right column
clamp(300, 24% of the width, 420), so the side columns stop growing at 670px and the chat takes the rest (63% at 1920,
79% at 3440). The right column is inline from 1280 and below that a slide-over drawer (button with the topology icon at
the right end of the top bar, Cmd/Ctrl+I, click on the dimmed chat closes it); the left column is inline from 1000 and
below that a drawer too (folder icon, Cmd/Ctrl+B). Drawers are closed by default, one at a time, drawn over the chat
(`--drawer left|right` for `--shot`). Hidden `synt.tui.panels.*` panels give their space to the chat. All other
metrics are the design's; `design_tests.rs` keeps the Chrome rects as the reference for everything except column
x/width (and the wrapping of the right cards in the narrower column), and `chat_share`, `drawers` and
`topology_fits_the_narrow_column` encode the rule. Known: at 300-336px the Context card clips a long `tokens` row.
Narrow-window polish (audit `docs/ui-audit-2026-11.md`, deviations from `cli.html`, only below the widths where it
fits): the top bar drops whole chips that do not fit and ends with a `...` chip (`chrome::fit_chips`; `cli.html` clips them
mid-glyph); a docked dialog (Trust, approval, pickers) is not taller than its column, so the prompt stays on screen:
its text box and option list shrink and scroll, and a line `down-arrow more ...: scroll` shows while text is below the
fold; "approval needed" in the title bar has a fixed line box and only its dot blinks; a side panel body that is cut
(Tools & Skills, Recent Activity, Workspace, Running) fades out at its bottom edge; the `/model` picker lists only the
metadata the provider gave; `chrome::dim` is `#8E8F93` (docs/cli-palette.md). At the reference sizes of `design_tests`
nothing of this changes the rects. Shot flags: `--live SCRIPT` without `--prompt` (empty chat), `--hide-all`, `--bare`
(Files and Browser tabs unseeded).
Chat copy and menu (`tui_view/chat_sel.rs`): Cmd/Ctrl+A in the chat tab (input empty, or a chat selection exists)
selects the whole conversation. The list is virtual, so this is a flag: the rows built tint like a selection, a pill says
so, and Cmd/Ctrl+C copies the transcript assembled from the same items and `toolview` cards the rows paint (same
redaction and sanitizing; rows the quiet-chat rules hide are not in it; a collapsed card copies what it shows), capped at
2 MB with a notice. Esc, typing or a click in the chat drops it. Right-click on a row opens the explorer's menu widget
(`quote::menu_overlay`) with Copy (pointer selection, or all when select-all is on), Copy message, Copy code block (all
fenced blocks of that assistant message) and Select all; unavailable rows are dimmed. Thinking, plan and check cards,
approval prompts and the quote blocks of user messages are selectable. With the header hidden, a strip over the input
carries a handle per slide-over column that has no other opener (with Cmd/Ctrl+B and Cmd/Ctrl+I) and the `N need you`
count the header chip would show; with the header shown nothing changes (cli-rects untouched). Shot flags: `--attach-image
FILE.png`, `--focus chat|input|panel|none` (a bare `--focus` is still the quiet-chat switch), `--active-window true|false`.
Not ported: topology mode L (tall and narrow falls back to T). The "Latest user prompt" scrolls (16s, repeating,
`synt.tui.motion`) only when it does not fit.

Window shell (`ui.rs`): a 30px title bar over the content, no side rail. The bar follows the Tauri app's chrome
(`#chrome`: `.cb` buttons 26x22, radius 6, hover and pressed backgrounds): the app mark and drag region on the left,
the TUI and Super Agent icons in a group, then the Settings gear; each has a tooltip with its shortcut (Cmd/Ctrl+1,
Cmd/Ctrl+2, Cmd/Ctrl+,). The content below is the design's 1400x900 etc. exactly (the bound tests lay the view out
without the bar; `ui::tests` checks that the bar adds 30px above it and nothing beside it). macOS: transparent
native title bar with the traffic lights at (14, 9) and 78px reserved for them. Windows and Linux: client-side
decorations, the bar is the drag region (`WindowControlArea::Drag`) and draws minimise / maximise / close
(`WindowControlArea::Min|Max|Close`, close hovers red). Not verified without a real window on each OS.

Session switch (instant preview, owner request 2026-11-10; `tui_view/driver.rs` `preview_row`, `Live::preview`): moving the Running selection (arrows, Tab into the panel, a click) shows that session's STORED conversation in the chat at once and starts nothing. It is read-only: `Live::convo()` returns the preview, the engine and its pump keep running on the open session. The conversation is replayed from the store off the UI thread (`load_convo`: events only, no session listing, no messages, no engine) into an LRU of 8 (key: session id + the row's last-activity time, so a changed session is read again); the rows next to the selection are read ahead. Until a read ends the chat keeps what it shows (no spinner, no loading text); a cached session shows in the same frame. The engine starts lazily, through the unchanged `switch()` (so `engine_switch_blocker` and the 5 s `switch_armed` rule for a running task still decide): on Enter on the row, on a click followed by Enter, or on the first line typed (`hold_for_engine`: the line is held in `after_start`, the box is cleared, and the line is handled by `dispatch` once the engine is attached; a refused start leaves the text in the box; a failed start returns it to the box). A line typed while any engine start is in flight is held the same way (it was dropped before). Moving the selection never tears down a running session. The Recent Activity click does nothing while a preview is shown. Only the chat is previewed: the side panels keep showing the open session. The `new workspace...` and `new session...` rows are the last row of their lists (`Live::ws_new`, `run_sel` past the sessions): selectable and lit, they show the empty chat (welcome) and create nothing until Enter or a click.

Centre tabs (`TabState`, kept per session id, so a session left and resumed finds its tabs as they were; the strip
shows counters; Ctrl+1..5 switch, Esc returns to the chat, PageUp/PageDown scroll the pane):
- diff: the session's changed files (`AgentPanes.files`) with `+N -N`; the newest opens by itself, a click expands a
  file's syntax-coloured diff; `/diff FILE` opens that file.
- terminal: the command history (`AgentPanes.commands`, from `process.*` and Bash tool events): command, exit code,
  duration, output tail, running state. Read-only, not a terminal.
- browser: the page, the selected saved screenshot (`browser.frame`; the latest by default, history rows select
  another; a PNG inside the session's artifact folder only, <= 8 MiB) and the browser calls with state and duration.
- file explorer (user scope, no trust needed: the agent never reads it): `project::scan` (git's hardened file list and
  status) in a trusted folder, `project::scan_plain` (a plain directory walk, no git, no links, no `.git`) in an untrusted
  one, so nothing a repository's config names can run; the Trust dialog is `/trust ask` (a session trust is kept for
  every session of the same folder while the app runs). The diff tab reads the session's own change events, no git. Directories open on demand (the parents of
  changed files open first), changed files marked M/A/D/?, a click previews a text file read-only (<= 256 KB and 2000
  lines; refuses paths outside the folder, links out of it, secret files and binary content).

### Feature gaps against the CLI (view-model status)

Done in `synthesis-view`: workspaces and sessions, running/peers, usage, status, topology, activity,
tools and skills, conversation. Not done, with where the data would come from:

- Browser screenshots: done as history, not a live stream. `browser.frame` (path, origin, w, h, bytes) feeds
  `AgentPanes.frames` and `.screenshot`; the file is opened only through `viewable_screenshot`.
  `Event::BrowserLoginRequired` is already in the stream and can be shown. Page title changes are not an event.
- Commands tab: `process.spawned|output|exited` (Bash, terminal_spawn) give the real exit code and background
  rows; `[exit code N]` parsing stays as the fallback for old sessions. A background process nobody polls
  again never gets `process.exited`. Verification: `verification.check.started|finished` give per-check progress (a card in the chat with one row per check, 5-line clamp for the output tail of a check that did not pass; a line in Recent Activity); sessions recorded before them show the one-line `verification.completed` as before.
- `/rewind`: done (`apps/desktop/src/rewind.rs`, `tui_view/rewind.rs`). It is `synt rewind` with a dock in place of flags, and
- `/close-workspace` (desktop only; `apps/desktop/src/close_ws.rs`, `Store::close_workspace`): closes every listed session of a workspace (`sessions.closed_at`; nothing deleted, `/resume <id>` opens one) after a dock confirmation (Cancel selected, Esc cancels, needs engaging like the bypass confirmation) that names the workspace and path, the count, the running sessions with their age, and that running tasks are cancelled. Sessions whose lock another live process holds are never closed and are listed as such. The open folder goes through the engine restart (the confirmation stands for the second press that cancels a running task) and lands on a fresh session in the same folder; the Workspace panel's `x` button (every row but the open folder) closes another folder in the store only. Refused while another dialog is open and, for the open folder, while a rewind is open (`engine_switch_blocker`). At confirm time the ask-time checks run again (`Live::confirm_close_workspace`): the workspace gone or a blocker now applies refuses with a note; a different open/not-open state or session list (id and kind) re-opens the dialog instead of closing. The open folder's dialog names the queued prompts and open dialogs the restart drops. `Store::open` (used to write) creates or tightens the home directory (0700), the database file and migrates the schema to the latest; it prunes nothing (empty-session pruning runs only at the first start of an engine). Folders that differ only by case on a case-sensitive volume are merged by `fold_dir`, as in the existing lists. Not in the CLI.
  it uses the same code (`synthesis_app::rewind::{load, plan, apply_confirmed}`), so the containment is the CLI's: only files
  inside the session's workspace roots (parent resolved, `..` and relative paths refused), never a symlink or non-regular file,
  never a secret-masked snapshot, never a file changed since the run (the desktop has no `--force`). Steps: (1) a picker of the
  session's checkpoints (name, time, message count, file), a dry run is always next; (2) the exact files as CLI rows
  (`restore`/`remove`/`skip`, path, size), grouped, 5 rows per group with Show more; (3) a confirmation docked above the prompt
  that says how many files are overwritten and deleted and that it cannot be undone beyond the checkpoint. Cancel is the
  selected default and Esc; Enter and digits are ignored until the person engages the dock (arrows, Tab, a click) and for 400 ms
  after it appears, so type-ahead cannot approve it. Refused with a reason while a task runs, compaction runs, a dialog or queued
  prompt is pending, or another `/rewind` is open; checked again at apply. Apply plans again right before writing and writes
  nothing if the plan is not the one shown. The result row gives counts and each failed file; the Files tab is scanned again and
  the Diff tab drops the files that are back. Like `synt rewind` it puts back files only: the conversation history stays.
  Every path in a checkpoint is untrusted: a snapshot is written back only if it hashes to the sha256 stored with the
  checkpoint, a file is deleted only if the checkpoint recorded it as absent before the run (older checkpoints without
  either are listed under "Will NOT be touched"), no directory on the path may be a symlink or junction, a session whose
  workspace root is `/`, a drive root or the home directory is refused (the desktop has no `--allow-wide-root`), and each
  file is checked again right before its own write. One apply per session at a time (lock file). While a rewind is open
  or writing, nothing switches the engine (`/resume`, `/model`, `/new`, `/close`, `/open`, restarts), and `/rewind` is
  refused while a background command or sub-agent runs. A partial apply says PARTIAL and lists the failed files.
  A rewind step that does not answer within 2 minutes frees the prompt and engine switches again (with a note to check the
  Files tab). Checkpoints come from the file tools only: Bash moves, renames and deletes are not tracked and are not
  restored. `Live::engine_switch_blocker` is the one check every engine switch or restart must call.
  Limits: the hashes and the proof live in the session store next to the rows, so a program that can write the store can
  forge them (whether the store is outside the agent's write scope is a sandbox decision, R7); a write by a process outside
  Synthesis between the last check and the rename (a directory swapped for a link) is the CLI's documented race.
- `/clean` (desktop only): permanently deletes stored sessions. (1) A dock picker of three scopes with live counts computed off the UI
  thread (sessions, workspaces that lose their last session, event rows, bytes on disk with screenshots / checkpoint snapshots / logs):
  "Closed sessions, and empty ones older than 1 hour" (`closed_at` set, whatever its history, and a closed-then-resumed session still qualifies because `closed_at` is never cleared; or no message and no task and older than 1 hour), sessions not updated for 30 days, and ALL
  sessions. Cancel is the first row and the selection; Esc cancels; Enter and digits are ignored until the person engages the dock and for
  400 ms after it appears. (2) A confirmation that lists what is DELETED PERMANENTLY (session rows and their events, messages, tasks, tool
  calls, approvals, processes, checkpoints and rewind snapshots, screenshots, logs, and `$SYNT_HOME/sessions/<id>/`) and what is NOT touched
  (settings, auth and env, memory store, trust records and `trust.key`, model cache, logs outside sessions, the Node transcript copies
  under `projects/`, project files, the current and running sessions). Rows: Cancel, a "Back up synt.db first" toggle (default on),
  Delete. Scope "all" also needs `delete` or `clean` typed into the field: letters are taken only when the dock is engaged and armed, so
  type-ahead cannot spell it; the scope row shows messages and events that go; the confirmation says copies remain in `projects/*.jsonl` and in the backup (which holds ALL sessions, tool output included, and is never auto-deleted); the Delete row is disabled until it matches. (3) `Store::clean` (crates/synthesis-session/src/clean.rs):
  protected are the open session, every session whose `lock` is held (this process or another) and the whole parent/child tree of those;
  a session with a child that stays is kept; every session's lock is probed (a running parent or child outside the scope protects its tree), the absence of a directory or lock is re-probed inside the transaction, `backups/` is checked for ownership and set to 0700, and `updated_at` is always the store's own fixed-width UTC text (an import goes through `create_session`; NULL never matches); a `sessions/<id>` that is a link or has a non-regular lock is skipped, never followed. Only
  ids shown in the confirmation are deleted, each in its own transaction taken under its lock and re-checked (scope, no children), the
  directory removed only after the rows are gone (`remove_dir_all` unlinks links inside, never follows them). The backup is
  `VACUUM INTO $SYNT_HOME/backups/clean-<time>/synt.db` (0600, dir 0700, `quick_check`ed; a failed backup deletes nothing); artifact
  directories are not copied and the backup holds the deleted conversations. The result row gives deleted / skipped running / kept / failed
  counts, bytes freed and the backup path; the Workspace / Running / Recent lists are read again. While a `/clean` is open or running no
  prompt starts and no engine switch happens (`Live::engine_switch_blocker`). The 10-minute timeout frees the flag only for the counting step; a deletion keeps prompts and engine switches blocked until it reports (a note says it is still working); it is refused while a task, dialog, queued prompt or
  `/rewind` is pending. Not done: directories under `sessions/` with no row (left by a crash) are not swept; `synt.db` is not VACUUMed (the
  file does not shrink; freed pages are zeroed and the WAL truncated); the CLI and `synt` have no `/clean`.
- Settings: done. Write API in `crates/synthesis-settings-ui`, the popup in `apps/desktop/src/settings_view` (section 6).
- Multi-session tabs: TODO. `ViewState` is one session; tabs are a `Vec<ViewState>` keyed by session id
  in the desktop, fed by one event channel per `engine` run. Needs the engine to host several sessions.
- Peers refresh: done. `ViewState::set_peers` takes the harness rows; the desktop polls the `peer` listing every
  fifth second (`driver.rs` `tick`, `Engine::peer_rows`).
- Uncommitted `+N -N` needs the host's trust decision per folder (`LoadOpts::trusted`) and runs git
  with a 5 s limit per call, so load off the UI thread.
- Interactive PTY: not wanted (owner decision).

### Super Agent view (removed)

The first Super Agent view (phase 3) is gone; the Super Agent page is "Coming soon". What it built on `synthesis_view::agent`
(`AgentPanes`: sub-agents, commands, verification, browser calls and frames, changed files) is used by the TUI view's
tabs and its sub-agent transcript (the `* main / developer` switch under the prompt opens a transcript beside the chat).
Known gaps: browser events carry no image bytes (screenshots are the saved artifacts, no live stream); concurrent
engines share process-global state (`set_learned_skills`), which no longer matters with one session on screen.

## 6. Settings (backend: `crates/synthesis-settings-ui`)

**`synt.tui` is the desktop TUI tab's section, `synt.cli` the CLI's** (owner decision 2026-11-11). Both hold the same keys (`UiSettings`) and
are read and written separately, so the Settings popup never changes how the CLI looks and `/config` never changes the desktop. The
keys in the table below are `synt.tui.*`. `synt doctor` notes CLI-only keys still sitting under `synt.tui` and `SYNT_TUI_*` variables.

**What the design has.** `cli.html` has no settings form with these sections. USER & PROMPT, MEMORY,
EXECUTION, GUARDS, HOOKS, MCP, SKILLS and SUB-AGENTS are the status cards of the topology (`topology1`,
`topology2`); its only settings hook is the `synthesis-settings` message (theme, font, font size, opacity), which is
appearance and has no backend key (the desktop owns it). The sections below are the same titles, filled with the real keys.

**Crate.** Pure data, no UI dependency. Separate from `synthesis-settings` because it reuses the validators of
`synthesis-harness` (`AutoCompact`, `ToolSearchMode`, `ShellSettings`), and the harness depends on `synthesis-settings`.
- `SettingsSchema::new()`: `Field { key (dotted path), section, label, kind (Bool | Enum | Int{min,max} | Float{min,max} |
  Percent{min,max} (a slider) | List | ReadOnly), default, help, scope (UserOnly | Layered | Tighten), restart, edit (Yes | Confirm(reason) | No(reason)) }`.
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
- The CLI `/config` still writes `synt.cli` through `persist_user_ui(home, "cli", ..)`; switching it was not zero-risk (it writes the whole subtree and the
  CLI would gain a dependency on this crate). The desktop reads and writes `synt.tui` (the TUI tab's own section; same keys, `UiSettings`).

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
| `synt.tui.opacity` | APPEARANCE | user file only | no | yes (slider 30 to 100 %, default 100) |
| `synt.tui.blur` | APPEARANCE | user file only | no | yes where the platform blurs (default off) |
| `synt.tui.mouse_capture` | INTERFACE | user file only | no | yes |
| `synt.tui.copy_on_select` | INTERFACE | user file only | no | yes |
| `synt.tui.panels.{session,mcp,tools,activity,status,tokens,topology,usage}` | INTERFACE | user file only | no | yes |

Why the "no" keys: an `allow` rule, an extra directory, a project MCP approval, `env_passthrough` and `read_tools` widen what the agent
may do or leak; hooks, verification checks and `autoMode` run or steer commands; browser secrets/profiles/pre-approvals are credential
adjacent (ADR-011: only `{env:}`/`{file:}` references, written by hand); `deny` and `ask` would be safe to add but their rule syntax is
validated in `synthesis-permissions`, not wired here yet (follow-up). A managed file that sets a key locks it (`Locked`).

**Providers tab (custom providers).** Not a `settings.json` key: it edits `providers.<id>` of the USER `synt.json` (`synthesis_app::provider_cfg`, atomic `edit_json_atomic`, 0600 when new, `$schema` added to a new file, other keys, their order and the entry's unmanaged fields kept; the result must pass `validate_versioned_config` or nothing is written, and an unreadable file is shown and kept). Fields: name (a-z 0-9 - _ ., 40; `anthropic` and `openai` are refused), protocol, base URL (https; http only for localhost, 127.0.0.0/8, [::1]; no userinfo, query or fragment; a private, link-local or metadata IP gets a warning, not a refusal), models (id, display name, context, max output, effort levels), optional headers (Host, Authorization, Proxy-Authorization, Cookie, Content-Length, Transfer-Encoding, Connection, TE, Upgrade and duplicates are refused; values are references). **Credentials are references only (ADR-011):** the key and every header value is an environment variable NAME (`UPPER_SNAKE`, 64 at most) or a file path, saved as `{env:NAME}` / `{file:path}`. `SYNT_*` names are free; any other name, and every file, needs a click-through that names the variable (or file) and the destination host, kept in memory only and asked again when either changes; variables of other tools (AWS_, GITHUB_, GH_, NPM_, SSH_, GOOGLE_, AZURE_, KUBE, DOCKER_ ...) are refused. A file path must be absolute, `~/` or `./`, without `..`, UNC or device paths, outside the secret stores (~/.ssh, ~/.aws, ~/.gnupg, ~/.config/gcloud, ~/.kube, ~/.docker) and system folders, and under the synt home only in `keys/`; an existing file must be a regular file of at most 4 KB. Text that looks like a key is refused and no error, warning or list repeats what was typed; a literal already in the file is never shown. The loader drops a referenced value that has a control character, and `SYNT_API_KEY` / `SYNT_AUTH_TOKEN` reach the implicit default provider (`anthropic`) only while it points at the vendor, `SYNT_BASE_URL` or no URL (an explicit `SYNT_PROVIDER` is the user's choice). Per provider: Test connection (shows what it sends and where, waits for a click unless every reference is `SYNT_*`; then the `synt models --refresh` call of `synthesis-providers` on a worker thread, 30 s bound; it shows the result only). Provider calls do NOT go through the `synthesis-egress` proxy (neither do `synt models --refresh` and the sessions' model calls): a provider's host is whatever the user typed, so the confirmation above is the guard. No price field (`synt.pricing` is edited by hand). In WSL mode the tab only says the engine reads its own `~/.synt/synt.json`. There is no managed provider layer in the config today (the managed file is `settings.json` only), so nothing is shown as managed. Open item: a managed provider allowlist (which hosts and variables an organisation permits). New sessions pick the models up; an open session keeps what it loaded.

**Design controls with no backend key** (shown read-only or not at all): the user name and task text (USER & PROMPT: session state);
model and effort (`synt.json` / `state.json`, `/model` `/effort`); safety, policy, sandbox and network toggles (GUARDS: `--sandbox` and
`--network` flags of the run); approval "granular" (only `permissions.defaultMode` and rules); proxy (standard `HTTP(S)_PROXY` variables);
per-skill on/off and planner/coder/reviewer toggles (SKILLS, SUB-AGENTS: files in `skills/` and `agents/`); per-event hook toggles
(pre-tool, post-tool, on-error, on-done: hooks are defined per event, only `disableAllHooks` exists); MEMORY learned/feedback counters
(data, not settings); EXECUTION tool-call, error and success counters (live status); theme and font (desktop appearance; opacity
and blur are `synt.tui.opacity` / `synt.tui.blur`).

**The popup** (`settings_view`). Cmd/Ctrl+`,`, the gear in the title bar and `/config` open it; Esc (the open confirmation first), the x
and a click on the backdrop close it. It is a centred panel over the TUI view (no sidebar, no view of its own): at most 720 x 640, a
16 px margin to the window, scrolling inside. Tabs group the sections (Appearance, Interface, Memory, Execution, Guards, Hooks, MCP,
Skills, Sub-agents, User & prompt); a search shows the matching rows of every tab. Each row shows the value, where it comes from
(default / user / project / local / flag / managed), its default, a reset link (removes the key from the USER file), the validation
error of a refused edit, and when it applies: `applies now` (panels, thinking, verbose rows, motion, opacity, blur), `applies to the next
session` with a banner and a working **Restart session** button (a fresh session in the same folder, which reads the files again), or
`CLI only` (logo, expanded diffs, queue lines, mouse capture, copy on select: the desktop view has nothing to apply). Keys that
widen what the agent may do stay read-only with the reason; there is no field for an API key (ADR-011). Every write goes through
`SettingsStore` (atomic, USER `settings.json`, every other key kept). While the popup is open the TUI view is not `shown`, so it never
answers a pending approval (the title bar keeps the red "approval needed", a click on it returns to the TUI view).

**Window opacity and blur.** `synt.tui.opacity` (whole percent, 30 to 100, default 100; under 30 the window is too faint to read, so
the loader and the slider refuse it) and `synt.tui.blur` (default off). Opacity scales the alpha of the window's background fills
only (`theme::fill`: the shell, the title bar, the TUI view, the panels); text, borders and the Settings popup stay opaque. The window
asks GPUI for `Opaque` at 100 %, else `Blurred` (blur on and supported) or `Transparent` (`WindowBackgroundAppearance`, set at open
and on every change). Blur: macOS (NSVisualEffectView) and Windows (acrylic) support it; on Linux GPUI blurs only on some Wayland
compositors and never on X11, so the control is disabled there with that reason. Blur shows only under 100 % opacity. Dragging the
slider previews at once; the file is written when the thumb is released. Only a real window shows the result (headless shots draw no
desktop behind the window).

## 7. Project trust (desktop)

- Launch flag `--trust-project` still works. Without it a folder is untrusted: project settings, MCP servers, agents, skills, hooks and memory stay inert and no git runs.
- Opening the Files tab in an untrusted folder raises the Trust dialog in the dock: it shows the canonical ROOT that would be trusted (the nearest folder with `.git` or `.synt`, which can be above the current folder; said so when it differs), what trust enables and risks, and offers Not now (default, Esc), Trust this folder (remember), Trust for this session only and, only for a real project parent, Trust parent folder (all its subfolders, remembered). A filesystem or drive root, the home folder (or a parent of it) and the config folder are refused for both. Arming delay and "engaged" rules are those of approvals.
- Accepting restarts the same session (conversation reloaded from the store, `--approve-mcp` of the launch dropped, grants and mode reset); a failed trusted restart continues untrusted.
- Remembered trust (owner decision 2026-11-09, hardened after the round-3 review) lives in the USER session store, `synt.db`, table `trusted_roots` (root, scope `folder|parent|child`, date, manifest, MAC), created lazily outside the versioned migrations so `user_version` does not move and older binaries still open the store. It is read at every engine start (resumed sessions of the folder included), never from a project file and never settable by a model, peer or tool output.
  - **Integrity:** anything that can run a shell as the user can write the database, so each row carries HMAC-SHA256(key, root|scope|date|manifest). The key is 32 random bytes in `<SYNT_HOME>/run/trust.key` (0600, `O_EXCL`, never a link, never printed; `run/` is in `synthesis_state_paths` and the permission deny list, and `*.key` is a secret path). Rows whose MAC fails are ignored and counted (`/trust`: "unverified rows ignored: N"). A missing key is made only when no row exists; if rows exist without it they are invalid and dropped by the next grant. `synt.db-journal` is in the protected state paths too.
  - **Sandbox gate:** remembered trust is honoured only while the sandbox is `enforce` (the default; `best-effort` and `off` can run a command unconfined, which could have written the store). `/trust` and a start-up notice say so; session-only trust and `--trust-project` are unaffected. The desktop never starts in `bypassPermissions` (the engine forces `default`).
  - **Scope:** a folder grant covers exactly that root; a parent grant covers it and what lies inside (components compared, case-sensitive, project path resolved with `dunce` first). Records are skipped when their root is empty, relative, a filesystem or drive root, `$HOME` or an ancestor, the config folder; a parent grant is also refused (and not offered) for temp folders, `~/Downloads|Desktop|Documents`, world-writable and root-owned system folders. At most 200 rows, roots up to 4096 bytes.
  - **Changed configuration (stricter than VS Code):** the row keeps a manifest (SHA-256 per file). It covers: `.synt/settings.json`, `.synt/settings.local.json`, `.synt/config.json` and `.mcp.json` of the project root and (as `cwd/...`) of the session's folder when that is another one; `AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md`, `.synt/AGENTS.md` and `CLAUDE.local.md` of every folder from the root down to the session's folder, and what they `@import` inside the project (4 hops); every file under `.synt/agents|skills|commands|rules` and `.claude/rules`; and the in-project files that hooks and MCP servers name as `command` or `args`. The code those scripts run in turn is NOT covered (the dialog says so). Hashing streams (depth 8, at most 500 files, 8 MiB each, 64 MiB in all). A link is followed when its target is inside the project and the target's CONTENT is hashed (a linked directory is walked); a link that leaves the project, or any cap exceeded, refuses trust-by-fingerprint. If the manifest no longer matches, remembered trust is NOT honoured: the project starts untrusted with a notice naming the changed files, and the Files tab asks again listing them; trusting again records the new manifest. The set is a hand-kept list (`CONFIG_FILES`, `CONFIG_DIRS`, `INSTRUCTION_NAMES` in `trust.rs`, one test file per kind): it is not derived from the loaders, so a new project-config kind must be added there. A parent grant records each child project's manifest at its first start (scope `child`, an internal row removed with its parent) and checks it afterwards.
  - **A remembered trust applies the project's DATA only (owner decision, round 7; deliberately narrower than VS Code):** a trust that comes from the store (and the run that follows "Trust this folder (remember)") enables the file explorer's git listing and the project's agents, skills, memory and instruction files. The project's settings and provider config load with the UNTRUSTED filter (`Common::project_exec_inert`; `prepare` loads both with `ProjectTrust::Untrusted`): no hooks, no MCP servers, no permission allow rules, `defaultMode`, `additionalDirectories`, `env`, verification checks, or provider URLs, keys and headers, and `disableAllHooks` cannot switch off the user's own hooks (project deny/ask rules, which only narrow, still apply). "Allow always" is not persisted into the project. Everything that widens permissions, sets env, changes providers or runs commands needs "Trust for this session only" or `--trust-project` in THIS run. A notice at every start says when project settings were present but ignored and lists what was left out (hooks, allow rules, additionalDirectories, defaultMode, env, verification checks, provider/config, MCP servers). The integrity gate stays sound without parsing commands: the manifest hashes declarative configuration for change detection; the script content it also hashes is best effort only (words of hook/MCP `command`/`args`, `$SYNT_PROJECT_DIR`/`$CLAUDE_PROJECT_DIR` expanded; shell operators, `--opt=path`, directories and what a script runs in turn are not followed; more than 2000 words, a word over 4096 bytes, or a link that dangles or leaves the project make the manifest unfit, so no remember is offered).
  - **Checked around the load:** the approved manifest (the stored one, or the one taken at the answer for a session-only trust) is compared (1) before the restarted engine reads any project file, (2) again before the worktree and anything else a project's files could cause (`Native::pre_start`, called by `prepare`), and (3) once more before any hook runs (`StartOpts::verify`, after `prepare`, before `session_hook`). A change in between aborts that start, which is repeated untrusted with the changed files in a notice (check (2) precedes the loading of agents, skills and memory, which are content only, never executed); a test shows no hook or MCP server ran when it failed at (1) or (2) and no hook at (3). Limit: a point-in-time check, not a hash of the bytes the loaders read, so a file changed after check (2) is not caught (it only matters for a trust of this run, which the person has just granted). A session that could not be fingerprinted runs without change detection, and the dialog says so before it is chosen.
  - **Walk limits:** every entry's real path, whether it or a folder on the way is a link, must stay inside the project; at most 5000 entries are visited and a directory is walked once (a web of links costs visits, so it ends as too big); files are opened without blocking and checked on the open handle (a FIFO swapped in cannot hang it).
  - **Git in the file explorer** (after trust) is the hardened read-only `git` of `synthesis-view::project`: absolute exe, closed environment (`GIT_CONFIG_NOSYSTEM`, `GIT_PAGER=cat`, no askpass, no API keys), `core.fsmonitor`, `core.hooksPath`, `core.pager`, `core.sshCommand`, `core.askPass`, `credential.helper`, `diff.external` and every `filter.*` driver neutralised with `-c`, `protocol.allow=never`, bounded output and a wall-clock limit with a process-group kill.
  - If the restart after "remember" fails, the rows that answer wrote are removed.
- `/trust` lists the remembered folders (date, fingerprint state); `/trust revoke <n|path>` confirms in the dock (Keep is the default); Settings > Guards lists them with a Revoke button. Revoking takes effect at the next engine start.

## 8. Engine over stdio (`synt engine --stdio`, INTERNAL)

Hidden subcommand, not a public interface: it exists so the desktop app can drive an engine that runs somewhere else (WSL2 on Windows, increment PR2) through a child process. It takes the same flags as `synt run`'s `Common` (`--model`, `--provider`, `--sandbox`, `--trust-project`, ...) and the process environment as every other command. Code: `crates/synthesis-app/src/{proto,engine_server,host,asks}.rs` (protocol, server, the session host shared with the desktop window, the approval/question dialogs), client `apps/desktop/src/remote.rs`, `Engine::start_remote`.

**Transport.** stdin/stdout, one JSON object per line, UTF-8, `\n` (a `\r` before it is dropped). stdout carries protocol lines only; diagnostics go to stderr, printable and short. A line over 8 MiB is a broken stream: the engine ends the session, the client kills the child. stdin EOF ends the session like `close` (hooks, stored usage, MCP, worktree; the engine kills its own process groups as in every session).

**Engine to client.**
- First line `{"msg":"hello","version":"<synt version>","protocol":3}`. The match is exact on both fields; the client refuses and kills the engine on any difference (no negotiation: install the matching version). `pgid` is not sent yet.
- Event lines: the ordinary `Envelope` of events.md (`v`, `seq`, `session_id`, ..., `type`, `payload`); they have no `msg` key.
- `{"msg":"result","id":N,"ok":{...}}` or `{"msg":"result","id":N,"error":"text"}`, the answer to request N.
- `{"msg":"ask.approval","id","agent_id","tool","input","dialog","prompt","reason","origin","suggestions":[...]}`: a permission ask. `suggestions` are `{"kind":"rules","behavior","rules":[{"tool","content"}]}` or `{"kind":"mode","mode"}`; bypassPermissions is never offered or accepted.
- `{"msg":"ask.input","id","req":<NeedsUser>}`: a plan or question dialog.
- `{"msg":"withdraw","id"}`: the ask is gone (answered, cancelled, expired).
- `{"msg":"task_done","error":null|"text"}`: the task started by `send` ended.
- `{"msg":"notice","text"}`: something to show (an event over the line limit was dropped on the engine side).
- `{"msg":"restarted"}`: the session was ended for a restart the client asked for (`trust.set`, `model.set`, `session.*`, `workspace.close` of the own folder); the lines after it belong to the new session. The client drops the old session's trailing events and treats a `restarted` it did not ask for as a hostile engine.
- `{"msg":"compacted","error":null|"text"}`: a `compact` that ran between tasks ended.
Asks are answered by a person only: nothing on either side answers one; an ask nobody can answer is denied (cancel withdraws it).

**Client to engine.** `{"id":N,"method":"...","params":{...}}`; `id` is a number the client chooses; unit methods carry no `params`. A line that does not parse is refused (an error result when it has a numeric `id`, else a note on stderr) and the engine keeps serving.

| method | params | result `ok` |
|---|---|---|
| `start` | `{cwd, resume?, model?}`: an absolute folder path on the engine's own OS, once per process; `resume` is a stored session id of THIS folder (checked), `model` a `provider/model` | `{sid,mode,label,models,effort_tiers,context_window,notices,cwd,root,stale_model,trusted}` (`trusted`: the engine's own verdict, see Trust) |
| `send` | `{text}`: starts a task (`task_done` follows; an error result: one is running, or not started) | `{}` |
| `answer` | `{id,choice}`: `once` \| `always` \| `deny` for an `ask.approval` | `{accepted}` (false: no such ask waiting) |
| `answer_input` | `{id,reply}`: `{"answers":<json>}`, `{"plan":{"approved","message","mode"}}` or `null` | `{accepted}` |
| `set_mode` | `{mode}`: any mode except bypassPermissions | `{mode}` |
| `cancel` | none | `{}` |
| `close` (alias `shutdown`) | none; the engine ends the session, answers `{worktree}`, and exits | `{worktree}` |

Store-backed and trust methods (PR3a; protocol 2). The engine owns the store, the trust rows and their MAC key (all on its own machine: in WSL, the distro's `~/.synt`); the client sends ids and the person's choice, never a path, a fingerprint or a flag. Every parameter is validated in `engine_methods.rs` (ids `[A-Za-z0-9-]` up to 64, keys and titles bounded and printable, models up to 200 characters not starting with `-`, instructions up to 8 KiB, `tz_min` within a day); a request that does not parse but has a numeric `id` is answered with an error (no timeout to wait out); errors are fixed texts (the detail goes to stderr, no path of the machine travels); every answer is bounded (200 workspaces, 200 sessions each, a preview of at most 4 MiB and 50 000 events: the newest, with `truncated`); the store-backed methods are limited to 50 requests per second (`too many requests`).

| method | params | result `ok` |
|---|---|---|
| `workspaces.list` | none | `{workspaces:[{root,key,label,sessions:[{id,title,status,updated_at,mtime,is_live}],available,changes}],truncated}`; `key` is the engine's workspace key, only ever compared |
| `sessions.list` | `{workspace: key}` | `{sessions}` (unknown key: error) |
| `session.preview` | `{id}` | `{events:[Envelope],truncated}`: the stored conversation, read-only |
| `usage.summary` | `{tz_min}` | `{days:[HistoryDay]}` |
| `session.rename` | `{id,title}`: only the engine's own session | `{title}` |
| `session.new` | none | the start report of a fresh session in the same folder |
| `session.resume` | `{id}`: a stored session of the same folder (checked before anything is ended) | the start report |
| `session.close` | `{id}`: only the engine's own; hides it, then a fresh session | the start report |
| `workspace.close` | `{workspace: key}`: hides every session of it that no other process holds; the own folder also continues in a fresh session | `{closed,held,failed}` (own folder: the start report plus `closed_report`) |
| `model.set` | `{model}` | the start report |
| `trust.state` | none | what the Trust dialog and `/trust` show (see Trust) |
| `trust.set` | `{mode: session\|remember\|parent\|deny}` | the start report of the restarted session; `deny` changes nothing: `{trusted,restarted:false}` |
| `trust.revoke` | `{root}`: a root `trust.state` listed (or this process's own grant) | `{revoked,session_grant_ended}` |
| `compact` | `{instructions?}` | `{started}` (false: between tasks, `compacted` follows) |
| `clear` | none | `{}` |

**Out of band.** A reader thread answers `cancel`, `answer` and `answer_input` at once; every other request is queued (64) for the worker and a full queue is refused (`too many requests`), so a slow store call never delays a cancel or an answer. `start` is one-shot: a second one is refused whatever happened to the session. A `cancel` that reaches the reader while its `send` still waits in the queue is applied to that send when the worker starts it (not to sends queued after the cancel). The methods work under `--host desktop` (the desktop's launch) and under the governed headless defaults (a worktree per session, the cost cap): a restart builds the next session with the same limits and `synt stop` governance, so each session has its own stop watcher (the ended session's runtime takes its watcher with it, and a stop of the old id does nothing to the new session), its own `gate-baseline.json` under `sessions/<id>/`, and a resumed session continues its event hash chain; `session.resume` accepts a session of a worktree when the repository it was made from is the engine's folder. A `send` during a `compact` is refused (`refused: busy`): the compaction replaces the history it copied. In the engine the refused/fatal distinction is a typed error decided where the error is made (the wire text is unchanged).

A restart (`trust.set`, `model.set`, `session.new|resume|close`, `workspace.close` of the own folder) is refused while a task runs, a dialog waits or a compaction runs, and at most once a second (the error starts with `refused: `, and means nothing was ended); it ends the session like `close` (hooks, usage), writes `restarted`, then starts the next. If the next session cannot start, the engine resumes the previous one and says why in `notices` (a trusted restart that fails continues untrusted, and the trust rows written for it are put back); only if that fails too is there no session: the error starts with `fatal: ` and the engine exits. An error of a restart request that starts with neither prefix, a timeout or a dead process make the client drop (kill) the engine: only a marked refusal keeps a session. A rollback that failed because the trusted configuration changed (`trust-changed`), and the rollback of a `trust.set`, start UNTRUSTED (no grant, no remembered lookup) and the notice says so; `session.close` and `workspace.close` hide sessions only after the new session runs; a restart that changed the permission mode says so. The client drops every line of the ending session (events, task ends, asks, notices) between the request and `restarted`. One folder per process: another folder, or a session of another folder, is a new process (`start` with `cwd` and `resume`). A remote `Engine` never touches the host's store (`synthesis_home` is empty in remote mode, and the commands that would use it are guarded): files, images, rewind, clean, memory and settings go over protocol 3 (next paragraph).

**Files, images, rewind, clean, memory and settings (PR3b; protocol 3).** The handshake is exact, so a protocol 2 client and a protocol 3 engine refuse each other cleanly (install the matching version). Everything below runs IN the engine, on its own machine; paths in answers are display text for the client and are never opened by it (the Windows host reads nothing of a WSL folder). Code: `crates/synthesis-app/src/{engine_fs,engine_ops}.rs`, client `remote.rs`. Parameters are validated in the engine (a path is relative with plain components only: no `..`, no `.`, no absolute, drive or root, no control characters, nothing in `.git`, at most 1 KiB); errors are fixed texts or the engine's own text with its folders (as given and as the system resolves them; also in a clean result's `failed` and `backup`) replaced by `<folder>` and made printable; every answer is bounded; `fs.list` (first page), `fs.git_*` and `doctor.summary` and `clean.plan` are limited to one per 250 ms (`too many requests`); the rest share the 50 per second limit. A call before `start` is refused.

| method | params | result `ok` |
|---|---|---|
| `fs.list` | `{dir?, offset?}`: `dir` "" is the scan (first level of the folder plus the parents of changed files; the first page scans, later pages cut the same scan); a `dir` that is a folder of the project is read from the disk NOW, `offset` entries in (lazy: a folder is listed when the client opens it, so a huge `target/` never uses up the listing) | `{rows:[[depth,name,dir,status,link,generated,more,notice]],total,next,truncated,note,trusted}`; 200 rows a page; `link` (symbolic link: listed, never entered), `generated` (`target`, `node_modules`, `.venv`, `dist`, `build` untracked or ignored, or a valid `CACHEDIR.TAG`: one closed row) and `more` and `notice` (the four are additive; `partial` in the answer says entries are missing; the client builds its own "... N more" row from `total`); git (hardened wrapper, `status --ignored`) only when the engine trusts the folder, else the disk alone; the folder is read from a directory handle with one NOFOLLOW `openat` per component on unix, and the pages after the first come from the folder read at offset 0; a link on the way to `dir` is refused (`no such folder`); no protocol bump (the handshake is exact on version and protocol, and the shape only grew) | Listing shows the NAMES of secret files (`.env`, keys): `fs.read` still refuses their content. Entries `git` ignores are hidden (an ignored build directory is one closed row). The `generated` label can come from a `CACHEDIR.TAG` the repository holds: it only collapses the row. A name that is not printable text (the display would rewrite it, so it could not be opened again) or entries past 200 000 in one folder are left out and said so by a note row. An older engine that answers `dir` with a whole subtree is tolerated: the window keeps only the rows one level below the folder.
| `fs.read` | `{path}` | `{lines,size,cut}`: at most 256 KB and 2000 lines, made printable; refused: outside the folder (real paths compared with `PathPolicy`), a link that leaves it, `.git` or a secret file by the name given AND by where the file really is (a link inside the project to `.git/config` or `.env` is refused), not a regular file, a file with other hard links, a file on a filesystem that cannot count hard links (`fstatfs` type 9p, CIFS/SMB, FUSE: a Windows drive in WSL2 is 9p; ext4, xfs, btrfs, tmpfs, overlayfs and NFS are fine, so a Linux project is never refused by its path), binary. The file is opened by its canonical relative path beneath a handle of the folder with no link followed (Linux: `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` through rustix, `O_NONBLOCK|O_NOCTTY`, so a parent or the file swapped for a link meanwhile is refused, a pipe never blocks and nothing becomes a terminal; macOS and other unix: cap-std). It must be the very file that was looked at with `lstat` before the open (device and inode), and on Linux the path of the open descriptor (`/proc/self/fd/N`) goes through the `.git`/secret checks once more; a kernel without `openat2` (before 5.6) refuses every preview. An engine on Windows serves no previews at all. On macOS and other non-Linux unix the open is cap-std and the `lstat`/`fstat` identity and link-count checks apply, but there is NO file-system type refusal and NO descriptor-path readback: both need an `unsafe` libc call (`fstatfs`/`F_GETPATH`), which this codebase forbids. The engine is meant to run in Linux (WSL2). A file name with control or bidi characters is listed escaped and so cannot be read (the escaped name is not a file) |
| `fs.git_status`, `fs.git_diff` | none | `{files:[[path,letter]]}`, `{diff,stats,truncated,note}`; only through the engine's git wrapper (no filters, textconv, fsmonitor, hooks, pager or network) and only when trusted (else `the folder is not trusted: git is not run here`) |
| `image.put` | `{upload?,seq,mime?,size?,data,last}`: base64 chunks of at most 512 K characters; the first names `mime` (png, jpeg, gif, webp) and `size` (at most 5 MB) | `{upload}` then, for the last chunk, `{id,bytes}`; the bytes are sniffed and must be the declared type, and the width and height read from the header must be at most 8000 pixels a side and 40 megapixels (a truncated or malformed header is refused; the dimensions come from the header ONLY, the pixel data is not decoded or checked against them: the cap bounds what is sent to the model, not what a decoder would do); one upload at a time, idle 60 s drops it, a wrong sequence or id ends it; at most 5 images and 6 MB waiting |
| `image.drop` | `{id}` | `{}` |
| `send` | `{text, images?:[id]}` | ids the engine issued, each once; an unknown id refuses the send. An attached image is untrusted pixels: for that task and every later one whose history still carries an image block (until a compaction drops it; the harness also keeps a session-wide bit that outlives such a compaction and is cleared only by a person's answer to a question, and a person's answer never clears the taint while an image block is still in the conversation; the bit is not stored, so a restart of the session recomputes it from the history only), in auto mode, actions with an external effect are asked, never judged by the model (the `after_image` rule of tool-result images; test `an_image_the_person_attached_taints_the_actions_that_follow`) |
| `rewind.list` | none | `{checkpoints:[{name,when,after_message,files}]}` of THIS session only |
| `rewind.dry_run` | `{checkpoint?}` | `{token,entries:[{shown,kind,size,reason}]}` |
| `rewind.apply` | `{token}` | `{restored,removed,skipped,failed,changed}`; a protected path (the union of what the tools ask about and what they refuse outright, folded and alias-reduced on every OS: `.git` and hooks, shell, CI and package-manager configuration, `.env*`, see permissions.md, `synthesis_permissions::is_protected_path`) is never restored or removed, in the plan and again when applied, and its skip reason lists the set and says to revert by hand; the token is single use, valid 10 minutes, for this session and exactly the dry run it was issued for (the apply plans again and compares); any attempt with another token spends it; refused while a task runs, a dialog waits or a compaction runs |
| `clean.plan` | none | `{plans:[{scope,token,counts}]}` for the three scopes; the own session is never in a plan |
| `clean.apply` | `{token,backup}` | the result counts (also limited to one per 250 ms; the window's confirmation for the `All` scope lists the counts and says it deletes the sessions stored inside WSL, and needs the typed word); same token rules as the rewind (a plan of another scope, an old plan or a wrong token deletes nothing); the store checks every session again |
| `memory.list`, `memory.retrieve {query}`, `memory.show {id}` | | `{records,note}` / `{text}`; project scope only in a trusted folder with memory on, else the user scope |
| `memory.add {text,user}`, `memory.forget {id}` | | `{text}`; trust `owner` (the person typed and confirmed it in the window), secret-shaped text, over 2000 characters or 40 lines refused |
| `memory.note {text,user}` | | `{text}`: a `#` note appended to an instruction file by the engine's own `Notes::save` (contained in the project, never through a link); a project note (`user:false`) is refused unless the engine trusts the folder. The engine cannot tell whether the person confirmed a note: it trusts its client (the window) for that, as for every protocol call |
| `config.effort` | `{effort}` | a tier the model declares, or null (auto); saved in the engine's state and applied |
| `context.set` | `{value}` | `1m`, `200k`, `128000` or `unset`: writes `limit.context` of the model in use into the engine's own user `synt.json` (a provider declared there, no link); applies from the next start |
| `doctor.summary` | none | `{rows:[{level,text}]}`, 20 s limit |
| `mcp.list`, `mcp.add`, `mcp.remove` | `{name,url,transport}` / `{name}` | `add` takes a REMOTE server only: an `https` URL (no `$`, no spaces, 2 KiB; `http` is refused), `http` or `sse` transport, into the engine's USER `synt.json`, with `synt mcp add`'s name checks and no more than 32 servers added this way, serialized with a lock file. The URL is parsed and judged by `synthesis_mcp::check_remote_url` at add time AND the connect path of every transport applies the same address blocklist: no user:password, no loopback host (not even `localhost`), no private, link-local (the cloud metadata address `169.254.169.254`), CGNAT or multicast address, and a name that does not resolve or resolves to one is refused; the streamable-HTTP transport follows a redirect only for 307/308 within the same origin (a redirect to another host, a private one included, stops with its own status). At connect every transport (streamable HTTP, SSE, WebSocket) resolves the name itself through a guard resolver (`synthesis-mcp/src/guard.rs`) that filters EVERY answer with `synthesis_platform::net::ip_blocked` and hands only the validated addresses to the connector, so the address that was checked is the address connected to (DNS rebinding cannot lead to a private host; an IP literal is checked directly). The connect-time check is strict for every host except an explicit loopback name or address in a hand-written config (`localhost`, `127.0.0.1`, `[::1]`: `http` allowed there, as before): `https` only, a failed lookup fails closed. A protocol-added server is never loopback (refused at add), so it is always strict. Redirects stop at another origin. A `command` (stdio), `env` (`LD_PRELOAD`, `PATH`, ...) or `header` (`${VAR}` would send an engine secret to a client-chosen URL) is refused, and so is replacing a server (no `force`): running code or carrying secrets is never done by a protocol call; if a stdio server is ever needed here it goes through the engine's approval dialogs, never automatically. A server name never contains `__` (see permissions.md). `remove` opens the user file only (a project-scope entry is not in it) and refuses a name a project server shadows in the live session |

Images are held in the engine's memory until the `send` that names them (the local window does the same: the bytes travel inside the session's event); they are dropped when the session restarts. Known limits: `/memory edit` and `/memory review` are not available in WSL mode; the rewind result does not tell the Diff tab which files came back (the events refresh it); `fs.git_status` and `fs.git_diff` exist in the engine and `Remote` but the window does not call them yet (the Diff tab is built from events and the tree marks changed files from `fs.list`); `mcp.add` and `mcp.remove` have no window command yet; the local `read_preview` of the window does not refuse hard links (the engine's does).

**Trust (PR3a).** The engine decides; this is the same code as the local engine (`crates/synthesis-app/src/trust.rs`: `launch_common`, `remembered_trust`, `TrustCheck`, the refusal rules and the MAC-signed store of `synthesis_session::trust`). The start report carries the same snapshot as `trust` (the dialog needs no call; `/trust` refreshes it in the background). `trust.set` refuses every mode when the configuration cannot be fingerprinted (a session grant without a manifest would run unchecked). `trust.state` reports the root the grant would cover, the refusal (filesystem or drive root, home or a parent of it, the config folder), whether the configuration can be fingerprinted (`unfit`), the remembered state (`no` | `trusted` | `changed` with the changed file names), the parent that may be offered, whether the sandbox enforces and the listed rows. `trust.set`: `session` grants this process only (kept for the root and the manifest, checked again before anything of the project runs, carried across this process's restarts, gone with it); `remember` and `parent` write the MAC-signed row (and for `parent` the project's baseline) with the engine's key, and the session restarts trusted-inert (data only: no hooks, MCP, allow rules, env, providers); `deny` is "Not now". The manifest is computed at the answer and compared again after the configuration is loaded and before any hook or MCP server runs (`pre_start`); a change ends the start (and the restart falls back untrusted). A remembered trust is also honoured at every later start, by the engine, when the rows verify, the configuration is unchanged and the sandbox enforces. The client cannot mark anything trusted without `trust.set`, cannot name a root to trust or revoke (only one the engine listed), and `set_mode` still refuses bypassPermissions, while a restart returns to the default mode. `start` still refuses `--trust-project` and `--approve-mcp`.

**Trust boundary (PR1 hardening).**
- The engine starts untrusted for project configuration: `synt engine --stdio` refuses `--trust-project` and `--approve-mcp`; trust comes only from `trust.set` or a verified remembered row (see Trust above).
- A remote `Engine` takes `cwd` and `root` from the folder the person chose on the host, never from the engine; what the engine reports is kept as display text (`remote_cwd`, `remote_root`). `/memory`, `#` notes, `/doctor`, `/effort`, `/context` and `/mcp` are calls to the engine (protocol 3); the Trust dialog is built from `trust.state`. `Engine::mode()` is the mode the engine reported (in `start`, and after each accepted `set_mode`); bypassPermissions is never entered or offered remotely, and a plan reply may continue only in default or acceptEdits.
- The protocol channel is private (unix): the engine dups fd 0 and fd 1 to close-on-exec handles, then points fd 0 at /dev/null and fd 1 at stderr, so hooks, `$EDITOR` and other descendants cannot read requests or forge lines. Windows has no equivalent without unsafe code (not used there today; the engine runs inside WSL).
- Client limits: at most 5000 lines and 32 MiB per second, 64 open asks, 100 unreadable lines or repeated hellos, a 256-request writer queue; past a limit the engine is killed. A watchdog kills an engine that owes a line (a task runs, a call waits) and is silent for 10 minutes. `close` waits up to 30 s (SessionEnd hooks, MCP), then stdin EOF with a 3 s grace, then kill. Known limit: the window's own queue has no readable length, so the rate limits bound its growth, not its size.
- The child gets only an allowlisted environment (`remote::allowed_env`: paths, locale, proxy, TLS roots, temp, `XDG_*`, `LC_*`, `SYNT_*`, `SystemRoot`); stderr is piped, capped (200 lines of 2 KiB), made printable and logged, never forwarded raw.
- Decided (owner, ADR-015): the provider API key lives where the engine runs and is never copied; trust is the engine's (above). A key is never passed in argv.

## 8.1 Sandbox on WSL (PR4)

The engine inside WSL2 runs commands only under bubblewrap plus a seccomp filter (Windows interop, Windows drives, WSLg and AF_VSOCK are closed or hidden), best-effort is enforce there, and a project on a Windows drive (`/mnt/c/...`) is ALWAYS refused because `.git/hooks` and `.synt` cannot be protected on drvfs: keep projects in the Linux filesystem. Consequences for the launcher and the ADR on WSL: the desktop must tell the person that bubblewrap is required (`sudo apt install bubblewrap`), that `--sandbox off` is the only unconfined mode there, and that interop stays open for user-owned MCP servers, the browser worker and `!` commands. Details, limits and tests: `docs/specs/sandbox.md`. When the WSL launcher section and its ADR merge, they must cite this text.

## 9. Agent inside WSL2 (Windows, ADR-015)

Setting (the user AND the managed settings files, managed wins; never a project, local or `--settings` file; Settings > Execution > WSL): `synt.desktop.wsl.enabled` (bool, default false) and `synt.desktop.wsl.distro` (text; empty is WSL's default distro). The engine is started as `synt engine --stdio --host desktop --max-cost-usd N`: `N` is `synt.desktop.max_cost_usd` (number > 0, default 10, per task) and `synt.desktop.no_cost_cap: true` is the explicit way to run without one (docs/UNATTENDED.md); `--host desktop` keeps the interactive behaviour (edits land in the opened folder, no worktree, no default wall cap). The toggle exists on every OS and is shown disabled with "Windows only" on macOS and Linux; on Windows it is disabled with the reason when `wsl.exe --status` fails. Code: `apps/desktop/src/wsl.rs` (pure logic over an injected `wsl.exe` runner), started from `tui_view/driver.rs` through `Engine::start_remote` (the same seam the Super Agent tab will use).

- **Command.** `C:\Windows\System32\wsl.exe -d <distro> --cd <linux cwd> --exec <synt> engine --stdio` (the path comes from `SystemRoot`, never from `PATH`). wsl.exe gets the allowlisted environment without any `SYNT_*`, and nothing is exported into the distro (no `WSLENV`). Hello may take 60 s (cold distro, "starting distro"); every other call is bounded.
- **Distro.** `wsl.exe -l -v` (UTF-16LE, BOM or none, CRLF; `*` marks the default; Running/Stopped; VERSION). WSL1 is refused with the `--set-version` hint. Stopped is not an error: wsl.exe boots it.
- **Engine binary.** `$HOME/.synt/bin/synt`, else `/usr/local/bin/synt`, found by `/bin/sh -c` (no login shell, fixed `PATH=/usr/local/bin:/usr/bin:/bin`, so neither a profile nor the Windows `PATH` WSL appends can supply it); a path under `/mnt/` is refused. Folders use `/usr/bin/wslpath`. `synt --version` must show exactly this app's version. If it is missing or differs the window shows the install command text (the versioned release's `install.sh` (`releases/download/v<version>/`) with `SYNT_VERSION` and `SYNT_INSTALL_DIR=$HOME/.synt/bin`) and runs nothing. No auto-install; an Install button is PR4.
- **Folders.** The agent works on Linux paths. `\\wsl.localhost\<D>\x` and `\\wsl$\<D>\x` become `/x` only when `<D>` is the selected distro (another distro is refused); `C:\x` goes through `wslpath -u` run in the distro, never a guess. The host-side session folder stays the Windows path the person chose (file tree, Reveal: `wsl::to_display`).
- **Errors shown.** wsl.exe missing or `--status` failing (toggle disabled), no default distro, WSL1, distro did not start in 60 s, binary missing, version mismatch, engine crashed (the last 5 sanitized stderr lines are appended; wsl.exe's UTF-16 messages are readable).
- **Settings and policy.** Schema scope `UserManaged`: the keys are read from the user file and the managed file only (never project, local or `--settings`), managed wins per key, like the core loader's last layer. `wsl::read_setting` reads both with the core's `read_json` (BOM tolerated). A managed `enabled=true` forces WSL mode and the user cannot switch it off; a managed `enabled=false` cannot be overridden by the user file; a managed `distro` wins. The Settings view shows the origin and locks a managed key ("set by the managed settings file"). Absent: off. Present but unreadable, over 1 MiB, not JSON, or a wrong type: the start is refused with the reason, never the local engine. Project and local files are never read. Folder components `.`, `..`, or ending in `.` or a space are refused; error text with names from wsl.exe or the distro is made printable.
- **Fail closed.** While WSL mode is on (or forced by policy) no launch may start the Windows engine: `driver::build` returns through `wsl::start` before the only local `Engine::start_with`; `wsl_plan` classifies every `Target` (an exhaustive match; a new `Target` must decide): a first start and `/new` are new engine processes, a model change, trust, close and close-workspace are calls the running engine answers by restarting its session, a resume is in place for a session of the engine's folder and else a new process in that session's folder (the folder comes from the engine's own list, turned into the distro's UNC path and checked like any chosen folder). `wsl_build` returns before anything local: the host's `synt.db` is never opened for a WSL workspace (lists, previews, close, usage come from the engine; `/context`, `/effort`, `/doctor`, `/mcp`, `/memory`, rewind, clean, the file tree and the preview are engine calls), and after a restart the reported folder is checked again with `check_reported`. `Engine::start` and `start_with` are `pub(crate)` (integration tests use `engine::for_tests`, feature `local-engine-for-tests`, a dev-dependency only), and a test checks all targets and that no source but `driver.rs`, `shot.rs` and `engine.rs` names them (whitespace and aliases included). The settings key shape must be the full path `synt.desktop.wsl.*`: a top-level, `synt.` or `synt.desktop.` `enabled`/`distro` is ignored, and a wrong type on the path refuses the start. The Settings view asks `wsl.exe --status` again each time it opens, on a worker thread with a 4 s limit (the toggle shows "Checking WSL..." and stays locked meanwhile). `scripts/check-release-features.mjs` (CI desktop job and the release build) fails if `local-engine-for-tests` could reach a release build. Reveal paths: no `.`/`..` component, none ending in `.` or a space (also the distro), no `%`; a trailing `\` is dropped before Explorer's quoted argument. A settings file that cannot be read or validated refuses the start. The engine's reported folder must equal the Linux path asked for and its root be that folder or an ancestor (`wsl::check_reported`), else the start is refused (open the real path, no links).
- **UI.** While the distro boots the working line says "starting distro". `/reveal [path]` shows the session folder, or a Linux path the agent printed, in the system file manager (`/mnt/<d>/x` becomes `D:\x`, other paths `\\wsl.localhost\<distro>\...`); only absolute local paths are accepted (a drive path, or a share of a WSL distro; never a URL, another UNC share or a relative path), paths with quotes, `| < > * ?` or control characters are refused, and Explorer gets the documented `/select,"path"` as one raw argument; code in `reveal.rs` and `wsl::reveal_path`.
- **WSL mode is NOT isolation.** (PR4, section 8.1 and `docs/specs/sandbox.md`, closes interop and the drives inside the sandbox; the text below is the state before it.) Until the sandbox work of PR4, the agent in the distro can run Windows programs (interop) and write the Windows drives (`/mnt/c`); the Linux sandbox does not stop that. Treat it as the same trust as the local engine.
- **Keys and settings.** They live inside WSL (`~/.synt`). Settings edits the Windows file; its notice says engine settings and keys are WSL-side. Prefer `{file:}` provider keys: `wsl.exe --exec` does not load the shell profile, so `export` lines in `.bashrc` do not apply.
- **Not blocking, not silent.** Store calls the window makes after start (`/trust`, `/trust revoke`, `/rename`, `/compact`, `/clear`) run on worker threads and answer as messages (a visible "working" note, a 15 s bound); `--resume`, `--continue`, `--fork-session`, `--trust-project` and `--approve-mcp` are refused in WSL mode (never ignored); Settings does not read or revoke the Windows trust store in WSL mode; workspace keys of the engine are compared as issued (never canonicalized or case-folded on the host); `#` notes, `/memory` and `/mcp` never touch the host.
- **Store and trust (PR3a).** Workspaces, sessions, previews, usage, resume, new, close, close-workspace, rename, model, compact, clear and trust go over the protocol (section 8); trust is decided by the engine and stored WSL-side. A session-only trust ends with its engine process (a session in another folder, or `/new` elsewhere, asks again). **Files and settings (PR3b).** The file explorer and its preview, pasted and dropped images, `/rewind`, `/clean`, `/memory` and `#` notes, `/effort`, `/context`, `/doctor` and `/mcp` are protocol 3 calls run on background threads (a visible working note, bounded timeouts: 15 s, 30 s for scans, doctor and plans, 5 minutes for a rewind or a deletion, which the engine finishes); a failure is a note, never a fallback to the Windows engine, the host's store or the host's file system. Interop (running Windows programs from the distro) is to be blocked by the sandbox in PR4.
- **Tests.** Parsing, path table, probe, launcher and settings are unit tests in `wsl.rs`; `apps/desktop/tests/remote_engine.rs` runs the whole path against a fake `wsl.exe` (the test binary) on every OS. Real WSL2 on Windows is UNVERIFIED.



## Command pickers and session-scoped tabs (2026-11)

- A slash command with options opens a picker when typed WITHOUT an argument (`apps/desktop/src/menu.rs`, `PickerKind::Menu`): `/effort`, `/verbose`, `/focus`, `/images` (when attached), `/agents`, `/context`, `/permissions` (list, allow, remove), `/trust` (list, ask, revoke), `/memory`, `/panels` (every panel with its on/off state; a pick flips it and the picker stays open). `/model`, `/resume`, `/send`, `/rewind`, `/clean`, `/close-workspace` already had pickers. The current value is marked `*` and holds the highlight; arrows, click, Enter, Esc and type-to-filter work as in the model picker (digits are filter text). Free-text arguments (allow RULE, memory add, another context size) close the picker and put the command in the input box. Typed arguments still work. Not pickers: `/rename`, `/name`, `/open`, `/reveal`, `/compact` (free text), `/diff` (its tab lists the files), and commands with no options.
- The tabs follow the open session: the file explorer tree is keyed by an epoch that is bumped on a session switch, so a scan or file read begun for the previous session is dropped; the selection and expanded folders are kept per session id; `attach` now replaces the diff, command and browser panes with those of the session being opened. While a session that is not open is previewed from the store, the Diff, Terminal, Browser and Files tabs say "Open this session (Enter) to ..." (the previewed session's folder and data are not known without starting it).
