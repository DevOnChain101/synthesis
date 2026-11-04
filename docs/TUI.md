# The interactive app (`synt`, `synt tui`)

Branch `restore/ui-classic`. `synt` with no subcommand (on a terminal) or `synt tui` opens it;
without a terminal it prints the usage and exits 2. It runs the same wiring as `synt run`
(`synthesis-app::run::prepare`, `Front::Tui`); it is a client of the harness event stream.

Flags: `--cwd DIR` (default: the current directory), `--resume ID` / `--continue` (the stored
session continues in place), `--once` (print one frame and exit), plus the `synt run` flags
(`--model`, `--sandbox`, `--allow`, `--worktree`, `--peers`, ...). `--fork-session` and
`--permission-mode bypassPermissions` are for `synt run` only (bypass: Shift+Tab, confirm).

## Commands

| command | does |
|---|---|
| /help | keys and commands, plus the skills and MCP prompts of this session |
| /clear | new conversation in the same session |
| /compact [instructions] | summarise the conversation (`Harness::request_compact` / compact now) |
| /new | pick a folder; a fresh session starts there. Type a path (`/abs`, `~/x`, `./rel`, `a/b`) to get an `open <path>` row, or the reason it cannot be a folder (`no such folder`, `not a folder`, `permission denied`). Symlinked folders are listed with `(link)`. A folder that cannot be opened is a notice; the current session stays. Choosing a folder while a task runs cancels the task and says so |
| /resume [id] | continue a saved session (picker: this folder first); an unknown id is a notice, the app stays |
| /model [name] | pick or set a model; restarts on the same session |
| /effort [auto\|low\|...] | reasoning effort, only when the model declares `effort_tiers` |
| /mcp | MCP servers: state, tools, last error |
| /images [clear] | the attached images, or drop them |
| /config | the `synt.tui` settings as a list; enter flips a row, applies it and saves it |
| /files | the project tree with git status letters (read-only git, only in a trusted folder: `--trust-project`; see "Git in `/files` and `/diff`" below) |
| /diff [file] | uncommitted changes: a line per file with its full `+added -deleted` count (from `git diff --numstat`), then the diff text (of one file), cut at 600 lines with a note at the top; trusted folder only |
| /terminal | the shell commands this session ran, with their status |
| /browser | the browser and web calls this session made |
| /peers | other sessions on this machine (needs `--peers` or `synt.peers.enabled`) |
| /agents --all | the session's agents, then the other sessions' table |
| /name | the name other sessions address this one by (fixed for the run: `--name`) |
| /memory | instruction files loaded, long-term memory on/off, where `#` notes go |
| /agents | agents of this session and the definitions `@agent:name` can start |
| /permissions [allow\|remove RULE] | mode and rules; allow / remove a rule |
| /status | session, model, sandbox, budgets, verification |
| /focus [on\|off] | prompt, summary and response only |
| /exit | quit |
| /skill-name [args] | runs a skill (the model is told to load it with the Skill tool) |
| /mcp__server__prompt [args] | fetches the MCP prompt and sends it as the request |

Input: `!cmd` runs a shell command (`!!` only records it), `#note` saves a note to an instruction
file (you pick the file), `@file`, `@agent:name`, `@server:scheme://uri` (an MCP resource),
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

## Keys in dialogs

A picker (`/new`, `/resume`, `/model`, `/config`, ...) closes with Esc; an approval dialog is answered with Enter / 1-3, and Esc or Ctrl+C rejects it. Ctrl+D does nothing inside a dialog (on the prompt it quits when the prompt is empty); close the dialog first.

## Settings (`synt.tui`, user settings.json only)

`panels.{session,mcp,tools,activity}` (Workspace, Running sessions and the chat are always shown; the
other panels take the rows of a hidden one), `logo` (`art` | `off`), `show_thinking`, `diffs_expanded`
(long output starts whole, like ctrl+o), `queue_indicators`, `motion` (spinners drawn still when off),
`mouse_capture` (off: the terminal's own selection; applies at start), `copy_on_select`. Defaults:
everything on, `diffs_expanded` off. A project, local or managed settings file that sets `synt.tui`
is ignored with a diagnostic; a wrong value keeps the default and the diagnostic names the key.
`/config` edits these and writes the USER file atomically, keeping every other key; it can write
nothing else, so never a credential. First run without a configured model shows a setup card
(`r` checks again, `q` quits) instead of an error. What came from the removed new TUI and what did
not: `docs/tui-feature-port.md`.

## Git in `/files` and `/diff`

Only in a folder started with `--trust-project`, one scan at a time, 5 s limit, output bounded and cut
on a line. `git` is the one found by absolute path on PATH; it gets a minimal environment, `core.hooksPath`
at the null device, no network protocols, no submodules. For each call these programs named by the
repository's own config are switched off: `core.fsmonitor`, textconv and external diff drivers, and
every `filter.<name>.clean|smudge|process` found in the config (a filter whose name cannot be passed
safely makes the scan refuse to run git). Not guaranteed: other config keys git might honour; that
is why the folder must be trusted.

## Events

Every event type of `synthesis-events` has a row or a reason not to: conversation, tool, approval,
verification, budget, compaction, sub-agent and message events have their own views;
peers, memory, processes, mode changes, non-successful hooks and any future kind get one sanitised
`type: key=value` row; bookkeeping (tool.started, session lifecycle, todo/turn/delta events) is
shown elsewhere (header, todo list, streaming text). An approval made for another session says
`for <origin>:`.

## Per task, like `synt run`

MCP mentions and prompts are expanded, the memory block is retrieved for the typed prompt, the
memory episode and (opt-in) reflection run after it, usage reaches the session store, and the
`SessionStart`/`SessionEnd` hooks run per session. Not done in the app: `--json-schema`, limits
flags (`--max-turns`, ...), `--criteria-file`.

## Tests

`cargo test -p synthesis-tui-base -p synthesis-tui-render -p synthesis-tui` (render goldens in
`tests/golden/{render,keys,sgr,selection,welcome}`), and `cargo test -p synthesis-cli --features
test-provider --test tui_pty` (the real binary on a pty with a scripted model; needs python3).
