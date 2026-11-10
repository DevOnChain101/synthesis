# The interactive app (`synt`, `synt tui`)

Branch `restore/ui-classic`. `synt` with no subcommand (on a terminal) or `synt tui` opens it;
without a terminal it prints the usage and exits 2. It runs the same wiring as `synt run`
(`synthesis-app::run::prepare`, `Front::Tui`); it is a client of the harness event stream.

Flags: `--cwd DIR` (default: the current directory), `--resume ID` / `--continue` (the stored
session continues in place), `--once` (print one frame and exit), plus the `synt run` flags
(`--model`, `--sandbox`, `--allow`, `--worktree`, `--peers`, ...; the full list is the table in
docs/CLI.md). `--fork-session` and `--permission-mode bypassPermissions` are for `synt run` only
(bypass: Shift+Tab, confirm). Related commands: `synt auth status` (credential status per provider)
and `synt rewind SESSION` (put a run's files back; dry run unless `--apply`).

Resuming a session whose process died mid-task (kill, closed window): nothing runs in the new
process, so the dashboard is IDLE and tools/agents without an end in the stored events are shown
as ended (their chat rows read `interrupted`, with no spinner; agents as `interrupted`); the chat gets one line, `previous run was interrupted`. The
store moves the stale `running` status to `interrupted` on resume (`Store::resume_session`), but
never for a session another live process holds (writer lock). The phase is derived from replay by
`Session::settle_replayed`; the later `session.started` (source `resume`) in the event log marks the
cut for `synt run --resume` consumers.

## Commands

| command | does |
|---|---|
| /help | keys and commands, plus the skills and MCP prompts of this session |
| /clear | new conversation in the same session |
| /compact [instructions] | summarise the conversation (`Harness::request_compact` / compact now) |
| /new | a fresh session in the SELECTED workspace (the one highlighted in the Workspace panel; no picker). When that is not the live workspace the app switches to it. A running task needs a second `/new` within 5 s (it cancels the task and says so). For a different folder use Ctrl+N |
| /close | closes THIS session: it is marked closed (`sessions.closed_at`, schema v2; nothing is deleted) and leaves the Running / Workspace / `/resume` lists, but `/resume <id>` still opens it. The app then starts a fresh session in the same folder. A running task needs a second `/close` within 5 s (it cancels the task and says so) |
| /clean | desktop app only (`docs/specs/desktop.md`): permanently deletes stored sessions by scope, with a confirmation; this terminal app has no `/clean` |
| /close-workspace | DESKTOP APP ONLY (the terminal TUI has no such command: its dialogs would need a new confirm kind and a host round trip; close sessions there with `/close` one by one). Closes ALL sessions of the open workspace, then the app starts a fresh session in the same folder (the open folder always shows). Always asks first, in the dock: workspace name and path, the number of sessions to close, the running ones by name and age (their tasks are cancelled the way a second `/close` does), and that nothing is deleted. Cancel is the selected option, Esc cancels, Enter and digits wait until the dialog is engaged (arrows, Tab, a click). A session another process holds (its lock) is never touched: it is listed as `running elsewhere` / `open in another process`, `will NOT be closed`. One `UPDATE` per session (`Store::close_workspace`); the result note counts closed, left open and failed. Workspaces are matched by their canonical folder key, never by text. `/resume <id>` still opens a closed session; there is no `/reopen-workspace`. The Workspace panel shows an `x` button on every row except the open folder, which runs the same flow for that row. For the open folder the dialog also says the restart drops queued prompts (their count) and any open dialog. At confirm time the checks run again: a workspace that left the list or a blocker that applies now refuses with a note, and a changed state (open folder or not, other sessions, a task that started or ended) asks again instead of closing. Workspaces are keyed by `fold_dir` (canonical path, case-folded on macOS and Windows), so two folders that differ only by case on a case-sensitive volume are one row, as the lists already show them |
| /resume [id] | continue a saved session (picker: this folder first); an unknown id is a notice, the app stays |
| /model [name] | pick or set a model; restarts on the same session (like every relaunch, /new, /close, /resume and the Running panel, without leaving the alternate screen: no blank frame, the Usage history is read before the first frame); the choice is stored (`model.changed`, `sessions.model_config`) and `--resume` continues on it unless `--model` is given |
| /effort [auto\|low\|...] | reasoning effort, only when the model declares `effort_tiers` (in `synt.json` or in its `/v1/models` entry, cached) |
| /mcp | MCP servers: state, tools, last error |
| /images [clear] | the attached images, or drop them |
| /config | the `synt.tui` settings as a list; enter flips a row, applies it and saves it |
| /files | the project tree with git status letters (read-only git, only in a trusted folder: `--trust-project`; see "Git in `/files` and `/diff`" below) |
| /diff [file] | uncommitted changes: a line per file with its full `+added -deleted` count (from `git diff --numstat`), then the diff text (of one file), cut at 600 lines with a note at the top; trusted folder only |
| /terminal | the shell commands this session ran, with their status |
| /browser | the browser and web calls this session made |
| /peers | other sessions on this machine (needs `--peers` or `synt.peers.enabled`) |
| /agents --all (`-a`) | the session's agents, then the other sessions' table (the `/peers` table; in the desktop app "peers are off" when peers are not enabled) |
| /name | the name other sessions address this one by (fixed for the run: `--name`) |
| /rename <title> | rename this session (its stored title, max 120 characters; no control, bidi or zero-width characters, nothing that looks like a secret); without a title it shows the current one |
| /memory | instruction files loaded, long-term memory on/off, where `#` notes go |
| /agents | agents of this session and the definitions `@agent:name` can start |
| /permissions [allow\|remove RULE] | mode and rules; allow / remove a rule (also `/perms`) |
| /status | session, model, sandbox, budgets, verification |
| /focus [on\|off] | prompt, summary and response only |
| /exit | quit (also `/quit`) |
| /skill-name [args] | runs a skill (the model is told to load it with the Skill tool) |
| /mcp__server__prompt [args] | fetches the MCP prompt and sends it as the request |

Input: `!cmd` runs a shell command (`!!` only records it), `#note` saves a note to an instruction
file (you pick the file), `@file` (also `@dir/`; a name with spaces is written `@"my notes.txt"`, and Tab inserts that form;
the file is added to the message as data, inside the workspace only, never a secret file, capped;
an `@x` that is no file stays as typed with a notice `@x: no such file`), `@agent:name`, `@server:scheme://uri` (an MCP resource),
Shift+Tab cycles the permission mode.
Ctrl+V and Alt+V paste a clipboard image: Ctrl+V tries an image first and falls back to the text
(an EMPTY bracketed paste, which is what Windows Terminal and WSL send when the clipboard holds only
an image, is the same attempt); Alt+V is image only. Up to 5 images per message, shown as chips above
the prompt; `/images` lists them, `/images clear` or Backspace on an empty prompt drops them. They go
with the next message to the main agent as image blocks (a model without vision gets `[image]` in
its request instead, with a notice; the history keeps the image). Works on WSL (the Windows
clipboard, read with `powershell.exe` by absolute path; Linux `wl-paste`/`xclip` as the fallback;
interop off gives a one-line notice) and on Windows; not on macOS/Linux yet. WSL is detected from
`/proc`, not from environment variables. Limits: 5 s (the helper and its children are killed), 4 MB
PNG per image, 6 MB of base64 per message (the session store refuses a message over 8 MiB and that
limit is not raised), 8000x8000 px.

Stored images: a pasted screenshot is kept UNREDACTED in `synt.db` (mode 0600, like the rest
of the session) and in the Node-format transcript mirror. `synt.images.retentionDays` (user
settings, default 30, `0` = forever) rewrites images of sessions untouched for that long to
`[image removed after N days]` in the session store when the app starts (see docs/specs/config.md for exactly what is and is not rewritten: the Node-format mirror, backups and browser screenshot files are not). A persistence problem (a message the
store refuses, a failing store) is a visible `notice`, and a message over the limit is stored with
its images replaced by `[image]` instead of stopping the later messages.

Images and the context: the stored history keeps every image, but a model request carries base64
images only for the 2 newest messages that have any and at most 8 MiB of them (newest first); older
ones go out as `[image: n, removed to save space]` with one notice (`older screenshots omitted from
the model request ...`). The context estimate counts an image by size (about 1 token per 600 bytes,
between 1500 and 8000), so many screenshots start auto-compaction early. `/compact` and auto-compact
never put images in the summary request; the verbatim tail keeps the newest message in full, with
its images only if they are at most 4 MiB, else as placeholders. A request the provider rejects as too
large (HTTP 413, `request_too_large`, `too many images`, ...) is handled like a context overflow:
compact, up to 3 times per task. If the summary request itself is too large, the oldest half of its
input is dropped and it is retried (up to 5 times); if that fails the message says to use
`/images clear` or `/new`. Pasted images are not downscaled (no image-decoding crate in the tree).

## Context meter
The status line shows `ctx 62% 124k/200k`; the Context panel, the header chip and the topology cards use the same number. The window is the one the harness resolved for the model in use (docs/specs/config.md §6.2), so `/model` shows the new model's window from its first response. When the window is unknown the status line shows `ctx 124k/?` and the panels show `?` instead of a percentage; nothing is guessed from the model name. Set `limit.context` for such a model.

## Workspaces, sessions and the sidebar

The sidebar has three controls. **Workspace** lists the folders that have sessions in the store (the
live one first, then by newest session) with the session count. **Running** lists the sessions of the
SELECTED workspace: the live one (`●`, marked `⌘`) and the stored ones (`○`, named by their first
prompt), in a STABLE order: newest CREATED first (`sessions.created_at`, id as tie-break), never by last activity, so selecting, opening or running a session, or a status change, never moves a row; a new session joins at the top and the others keep their relative order. On a tall terminal each panel lists 5 rows (a session is 2 lines) before it scrolls (`↑ n more` / `↓ n more`); a shorter terminal gives them fewer, and the panels below shrink or drop out instead of overlapping. Sessions with no message and no task (a launch where nothing was typed) are not listed, in these panels or in `/resume`; the open session always shows, and a folder whose sessions are all hidden is not a workspace row (the live folder stays). At start the app deletes such sessions when they are older than 1 hour, are not the one being resumed and are not locked by a running `synt` (with their rows and directory); anything doubtful is kept. Selecting a workspace only changes that list; nothing restarts. Opening a
stored session is `/resume` (the app restarts on it, in its own folder; a running task is cancelled
after a second Enter / click within 5 s). **Activity** is the feed of the live session; a `❯` row is
a prompt.

| input | does |
|---|---|
| Tab (empty prompt, no dialog) | chat -> Workspace -> Running -> chat. The focused panel's border takes the accent colour (warm sand #C7AE90, see `docs/tui-palette.md`) and the footer shows its keys. Skipped when the sidebar is not on screen (terminal under 80 columns) |
| Up/Down, PageUp/PageDown, Home/End | in a focused panel: move the selection (the panel scrolls to keep it visible) |
| Enter | Workspace: its sessions get the keys. Running: open the selected session and give the keys to its chat |
| Esc, typing, Ctrl+C ... | back to the chat (a typed character is inserted) |
| click a workspace row | select it; Running lists its sessions |
| click a session row | open it and focus its chat input (the live session: only the focus moves) |
| click a `❯` row in Activity | scrolls the chat so that prompt is the first row of the view |
| wheel over Workspace / Running | focuses that panel and moves its selection one row per notch (the panel scrolls with it) |
| wheel over Activity | scrolls it (3 rows per notch, clamped); its title shows `↑n` older / `↓n` newer rows out of view |
| wheel over the chat | scrolls the chat; over anything else nothing happens |
| Ctrl+N | NEW WORKSPACE: the folder picker; the chosen folder gets a fresh session. Typed paths (`/x`, `~/x`, `./x`, `a/b`): a readable folder gets an `open <path>` row, and Enter on anything else keeps the picker open with one line saying why (`no such folder: <path>`, `not a folder`, `permission denied`); a word without a separator is only a filter. Rows marked `(link)` are symlinked folders and open on Enter like the others |
| /new | new session in the selected workspace |
| /close | close this session (hide it) and start a new one |
| Ctrl+T (empty prompt, sub-agents exist) | cycles where the prompt goes: main, then each sub-agent (this was Tab before) |
| Shift+Tab | cycles the permission mode (unchanged) |

Workspaces are told apart by folder (symlinks resolved; case folded on macOS and Windows). The Activity
panel is only drawn when the terminal is tall enough (about 160x48); `↑n more` / `↓n more` in
Workspace and Running are clickable steps.

## Secret files and hard links
Reading a secret file (`.env`, keys, cloud credentials), by name, by a `glob`/wildcard that could match one, or with a read-only Bash command such as `cat .env`, asks first in `default`, `acceptEdits` and `auto`, is denied in `plan` and `dontAsk`, and runs in `bypassPermissions`; Grep and Glob skip such files in directory walks, and `@mentions` skip them. A write tool refuses a file with more than one hard link (it may be a name for a file outside the workspace). Reads through a hard link are allowed, and Bash writes through one ask (or are denied in `plan`/`dontAsk`), but not in `bypassPermissions`, and the sandbox cannot see link counts (docs/SECURITY.md); wildcard and variable operands, a link made on the same command line and writers outside the lists (`sort -o`, `curl -o`, `perl -pi`, ...) are not checked. A directory-wide allow rule (`Read(sub/**)`) lifts the secret-file question for what it names; use file-level rules.

## Keys in dialogs

A picker (Ctrl+N, `/resume`, `/model`, `/config`, ...) closes with Esc; an approval dialog is answered with Enter / 1-3, and Esc or Ctrl+C rejects it. Ctrl+D does nothing inside a dialog (on the prompt it quits when the prompt is empty); close the dialog first. `/help` is not a dialog: it prints its text into the conversation like any other output, so Esc does not dismiss it (it scrolls away with the chat).

## Settings (`synt.tui`, user settings.json only)

`panels.{session,mcp,tools,activity,status,tokens,topology,usage}` (Workspace, Running sessions and
the chat are always shown; the other panels take the rows of a hidden one. With session, mcp, tools
and activity all hidden, Workspace and Running split the whole left column in halves. `status` and
`tokens` are the boxes above the chat: with both hidden the chat takes their rows. (The desktop app maps the same
keys onto the panels of its `cli.html` layout: `session` Session, `tools` Tools & Skills, `activity` Recent Activity,
`status` Mission (the agent's task list), `tokens` Context, `topology`, `usage` Usage & Metrics, `mcp` none; see
`docs/specs/desktop.md`.) `topology` is
the right-hand graph and `usage` the history panel: hiding one gives its space to the other. In the desktop app
the sub-agents are never hidden, so its right column always exists, and `status` (Mission) is hideable like the others (an
old settings file that has `status: false` now hides it; `/panels mission off` is the same key); the desktop `/panels`
command lists the panels, `/panels hide-all|show-all` and `/panels <name> on|off` apply at once and save here), `logo` (`art` | `off`), `show_thinking`, `verbose`
(internal rows in the chat, see Events), `diffs_expanded`
(long output starts whole, like ctrl+o), `queue_indicators`, `motion` (spinners drawn still when off),
`mouse_capture` (off: the terminal's own selection; applies at start), `copy_on_select`, and two the
desktop app reads (the terminal ignores them): `opacity` (window background opacity, whole percent 30 to 100) and `blur`
(blur behind the window; macOS and Windows). Defaults: everything on, `diffs_expanded` and `verbose` off, `opacity` 100,
`blur` off. A project, local or managed settings file that sets `synt.tui`
is ignored with a diagnostic; a wrong value keeps the default and the diagnostic names the key.
`/config` edits these and writes the USER file atomically, keeping every other key; it can write
nothing else, so never a credential. First run without a configured model shows a setup card
(`r` checks again, `q` quits) instead of an error. What came from the removed new TUI and what did
not: `docs/tui-feature-port.md`.

## Desktop: sub-agent roster and Mission (2026-11-10)

The `* main` switch under the chat input shows only while the session has sub-agents. Below it, one row per
sub-agent: `name - state` at the left, `online - tokens` (`5m - 900k tokens`) at the far right of the same row.
Lifecycle (pure functions in `crates/synthesis-view/src/lifecycle.rs`, tested with an injected `now`): working = the
hub's status (`running`); finished = `done`, transcript still viewable; 1 minute after it finished = `idle`; 5 minutes
after = row and transcript removed. Online time ticks while it works and is frozen when it finished. Tokens are the
input plus output of its `turn.completed` events. A resumed session's sub-agents that were still working count as
finished (they were interrupted).

Mission (was Agent Status) shows only while the agent has a task list (`todo.updated`): tasks in order, a done task
with a tick, the next open one running. 5 minutes after every task is done the panel disappears; a new list replaces
the old one at once; a resumed session starts without a task list, even one that was still running.
Panel values never wrap: in Session and Context a value is one line cut with `…` when the panel is narrow (cwd and worktree keep their end, `…/folders/ws`; the model name and the rest keep their start), and hovering a Session value shows it whole. Tools & Skills lists every row in a body five rows high that scrolls. The chat header shows the folder as `~/a/b` or its last two segments (`…/code/ws`), at most 20 characters.

## Usage panel

The sidebar's Usage panel (wide terminals) is a per-day history of every session in the store
(`<SYNT_HOME>/synt.db`, all folders, last 53 weeks), read-only and off the UI thread: once at
start (`loading usage…` until it ends) and again when a task ends. Per local day (the terminal's UTC
offset): tokens (`turn.completed` input + output), messages (`user.message`), tool calls
(`tool.started`), agents (`subagent.started`) and sessions with activity. An empty store says
`no usage yet`. The metric is always tokens (the render can show the others; no key switches it
yet, so the `u: metric` hint in the title does nothing). There is no provider quota source, so the
5-hour and 7-day quota rows are absent, and no cost is shown.

## Git in `/files` and `/diff`

Only in a folder started with `--trust-project`, one scan at a time, 5 s limit, output bounded and cut
on a line. `git` is the one found by absolute path on PATH; it gets a minimal environment, `core.hooksPath`
at the null device, no network protocols, no submodules. For each call these programs named by the
repository's own config are switched off: `core.fsmonitor`, textconv and external diff drivers, and
every `filter.<name>.clean|smudge|process` found in the config (a filter whose name cannot be passed
safely makes the scan refuse to run git). Not guaranteed: other config keys git might honour; that
is why the folder must be trusted.

## Events

The chat shows what a person needs and nothing else. The full event stream is always kept in the
session store (`synt.db`; `synt run --output-format stream-json`, the default, prints it), whatever the chat draws.

| Shown | Hidden by default (one readable line each with `verbose`) |
|---|---|
| your prompt; the assistant's text and thinking (`show_thinking`); tool calls and results (collapsed) | memory retrieved / written / quarantined / reflection |
| a permission that was denied, expired or refused by a policy or the auto judge | approval granted, mode changes (the footer shows the mode), `model.changed` |
| model errors and the first retry; failed compaction; the compaction result (one line) | later model retries, `context.usage`, hook runs that succeeded |
| failed hooks, failed verification and `verification passed`, budget exceeded, notices | unverified/blocked verification (the task end line says it), the harness notes to the model (`verification feedback`, `stop hook feedback`) |
| `✓ Done · n turns · t`, `task failed/cancelled/blocked/unverified`, `interrupted` | foreground sub-agent start/stop (its Task row shows it), process, peer and session bookkeeping |
| background sub-agents, peer messages, a browser login to confirm, a memory note waiting for review | any event kind the chat has no sentence for: nothing (verbose: its name only) |

Never a raw `type: key=value` row. `synt.tui.verbose` (default false; `/config` row `verbose`) turns
the right column on for the whole app; `/verbose [on|off]` flips it for this session (like `/focus`,
it applies to rows from then on, earlier rows stay as they were drawn).

## Per task, like `synt run`

MCP mentions and prompts are expanded, the memory block is retrieved for the typed prompt, the
memory episode and (opt-in) reflection run after it, usage reaches the session store, and the
`SessionStart`/`SessionEnd` hooks run per session. Not done in the app: `--json-schema`, limits
flags (`--max-turns`, ...), `--criteria-file`.

## Tests

`cargo test -p synthesis-tui-base -p synthesis-tui-render -p synthesis-tui` (render goldens in
`tests/golden/{render,keys,sgr,selection,welcome}`), and `cargo test -p synthesis-cli --features
test-provider --test tui_pty` (the real binary on a pty with a scripted model; needs python3).
