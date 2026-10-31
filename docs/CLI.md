# CLI notes: shell prompt, approvals, permission flags

## `!` shell prompt (interactive app)
- Type `!` at the start of the prompt: the prompt shows `! bash mode`. Enter runs the command; Esc (or backspace on the empty prompt) leaves the mode; while a command runs, Esc cancels it and kills its process tree. Only a typed `!` starts a command: pasted text or a burst of characters starting with `!` is plain text.
- `!cmd` runs as a user-initiated `Bash` call: same terminal runtime, sandbox, `--network` mode and artifact log as the model's Bash tool; events carry `origin:"user"`. A plain `ask` is approved (you typed it); deny rules, protected paths and critical removals still block, with the reason shown. `PreToolUse` hooks run for it and only a deny is honoured (ask and allow are moot: you typed it).
- The command and its capped output are appended to the conversation as `<user-bash-input>` / `<user-bash-output exit=N truncated=B>` (data from a command the user ran). Then the model gets a turn (an implicit harness note, not shown as your text) to report success or failure, key lines and next steps. `!!cmd` records without a model turn.
- Settings (user `settings.json`): `synthesis.shell.user_commands_model_turn` (default true), `synthesis.shell.user_commands_unsandboxed` (default false: network is denied by default; use `--network loopback|allow`).

## Commands the agent runs (tests, builds)
The agent runs tests itself. A call that needs permission opens the approval dialog (allow-always is offered). Verification-gate checks (`cargo test`, `npm test`, ...) go through the same permission check as a `Bash` call. Without an approver (`synth run`) the call is not run, the model is told which permission is missing and the task ends `blocked` (`approval_unavailable`); `task.ended.needed_permissions` lists the rules and `synth run` prints, e.g.:

    synth: blocked: approval needed for Bash(cargo test:*) — rerun with --allow 'Bash(cargo test:*)' or in the TUI

## Permission modes (`--permission-mode`, Shift+Tab)
`synth run --permission-mode default|acceptEdits|plan|dontAsk|auto|bypassPermissions`. `synth run` never prompts (no stdin): asks are decided by the mode and the `--allow` / settings allowlist; an ask nobody can grant is denied.

| mode | `synth run` behaviour |
|---|---|
| `default` | asks are auto-denied unless allowed by `--allow`/settings (`SendMessage` between agents asks; `ListAgents` is read-only) |
| `acceptEdits` | edits inside the workspace are allowed, and so is `SendMessage`; other asks (Bash, MCP, ...) are auto-denied |
| `plan` | read-only; writes are denied; `SendMessage` is denied (it can steer another agent) |
| `dontAsk` | an ask becomes a deny |
| `auto` | a judge model call decides each open call (allow / deny / ask); ask, unclear answer, error or timeout = denied |
| `bypassPermissions` | skips approval prompts; still stops: deny rules, ask rules (they still ask, so an unattended run denies them), critical `rm`, destructive MCP asks. Protected paths (`.git`, `.synthesis`, `.ssh`, ...) are NOT stopped by plain writes, as in Claude Code; the sandbox still confines commands. Loud notice at start. Only from the CLI flag: never from project settings, `.mcp.json` or the environment |

**Default limits.** `synth run` ends a task after 3600 s of wall time unless `--max-wall-secs N` says otherwise (the interactive app has no default); turns (50) and tool calls (200) are bounded by default as well. `--max-cost-usd` has no default because it needs a configured price.

**Inferred verification checks.** In `default` and `acceptEdits` the checks the harness infers from the repo (`cargo test`, `npm run test`, `pytest`, `go test`, `make test`, ... see `docs/specs/verification.md` section 4) run without an `--allow`, so `synth run --permission-mode acceptEdits` no longer ends `blocked` only because such a check asked. They stay sandboxed and deny rules still win; any other check command (user-declared or model-registered, or not a recognised test/lint/build invocation) still needs an allow.

A denied write or command (Write, Edit, Bash, MCP write tools, ...) makes the task end `blocked` / `approval_unavailable` with `needed_permissions`, never `completed`. Typical CI use:

    synth run --permission-mode acceptEdits --allow 'Bash(cargo test:*)' --prompt "fix the failing test"

**Auto mode judge.** The judge uses the session provider and model; set another model of the same provider with `autoMode.model` in the user `settings.json` (with `autoMode.environment|allow|soft_deny|hard_deny` to steer it). It sees the task, earlier user messages, the workspace and the pending call, all as data (tool output and repo text are never sent). At most 50 judge calls per task (identical calls are cached); their cost counts in the task. Every decision is logged as a `permission.auto_verdict` event and shown in the TUI. Hard stops (deny rules, critical `rm`, protected paths, destructive MCP asks) are decided before it and it cannot relax them.

**Interactive app.** Shift+Tab is the only way to switch modes (there is no `/mode` command): it cycles `default -> acceptEdits -> plan -> auto -> bypassPermissions -> default`; the footer shows the current mode. Reaching `bypassPermissions` opens the "Bypass permissions?" dialog: it lists what is turned off and what still stops (deny rules, ask rules, critical removals, destructive MCP asks); "No" is the default, and Esc or "No" stays in the previous mode; only "Yes" switches. The next Shift+Tab leaves bypass. `dontAsk` is reachable only with `--permission-mode`. Bypass is never saved, never restored by `--resume`, and `--permission-mode bypassPermissions` is refused by the TUI. A switch applies to tasks started after it (a running task keeps its mode). Every change emits `session.mode_changed`; hosts call `Harness::set_mode(mode, confirmed_bypass)`, which refuses an unconfirmed bypass in the core.

## Reasoning effort (interactive app)

`/effort` (no argument) or `Alt+T` steps through `auto` and the `effort_tiers` the current model declares in its config (`capabilities.effort_tiers`, e.g. `["low","medium","high"]`); `/effort high` or `/effort auto` sets it directly. The footer shows `effort: <tier>`. It is sent as `reasoning_effort` on the following requests (OpenAI-compatible providers) and resets to `auto` when the model is switched. A model without tiers answers "declares no effort tiers". On macOS terminals Alt+T needs "Use Option as Meta"; `/effort` always works.

## Cursor and IME (interactive app)

While the prompt is shown the real terminal cursor is parked at the prompt caret (so an IME candidate window opens there); it is hidden whenever a dialog or picker owns the screen.

## Selecting and copying text (interactive app)
Mouse reporting is on, so the app does the selection itself.
- Drag over the chat to select (the highlight follows the pointer; a drag may start on a blank cell and run in either direction). On release the text is copied to the system clipboard and a short "Copied N chars" notice shows. A plain click starts afresh; Shift/Alt+click extends a kept selection.
- Double-click selects a word, triple-click the whole line (a wrapped paragraph counts as one line); both copy at once.
- Ctrl+C copies a highlighted selection and clears it; with no selection it keeps its meaning (clear the prompt, cancel the task, press again to quit). Terminals keep Cmd+C for themselves and never pass it to the app, so use Ctrl+C or just release the mouse.
- Copied text leaves out gutters and decorations (the `│` of thinking, diff line numbers, bullets, indents) and joins wrapped rows into one line. Wide characters (CJK) are copied whole.
- The clipboard is written with `pbcopy` (macOS), `clip.exe` (Windows), `wl-copy` / `xclip` (Linux, when `WAYLAND_DISPLAY` / `DISPLAY` is set), then OSC 52 (also works over ssh when the terminal allows it). If none works the notice says "copy failed".
- To use the terminal's own selection instead, hold the bypass key while dragging: Shift in most terminals (xterm, GNOME Terminal, kitty, Windows Terminal), Option on macOS (iTerm2: Option; some terminals need Fn or Option+Cmd). Text selected that way is copied by the terminal (Cmd+C / Ctrl+Shift+C) and may include the gutters.
- Terminals without SGR mouse reporting (mode 1006) are not supported for in-app selection; use the native selection above.

## Opening links and files (interactive app)
Click a URL or a file path in the chat to open it. Nothing runs through a shell; only what is drawn on screen can be opened.
- Gestures (any of): Ctrl+click, Alt/Option+click, or a right click (macOS terminals turn Ctrl+click into a secondary click, so the app sees button 2). A plain left click only selects. A right click on the prompt line still pastes. Cmd/Super+click has no bit in SGR mouse reports, so the app cannot see it; terminals that support OSC 8 hyperlinks (below) handle it themselves.
- A `http(s)://` URL opens in the default browser. A path is revealed in the file manager (`open -R` on macOS, Explorer `/select,` on Windows, the containing folder via `xdg-open` on Linux); a directory is revealed too, and a file is never launched, even an executable or `.app`.
- Paths understood: absolute, relative (to the session cwd), `~/`, `C:\...`, `\\server\share`, `file:///...` (percent-decoded), with `:12`, `:12:3` or `(12)` suffixes ignored, in tool rows, `Update(path)` headers and markdown. A URL or path wrapped over several rows is joined. The path is resolved with symlinks followed and must exist: the footer shows `✓ revealed <path>` or `✗ not found <path>`. Paths containing spaces are not recognised.
- When the terminal reports motion without a button (mouse 1003, not enabled by default), hovering a link shows `ctrl+click to open`.
- OSC 8 hyperlinks (`ESC ] 8 ;; URL ST`) are also written around URLs and existing paths (`file://`, percent-encoded, `file:///C:/...` on Windows) when the terminal is known to support them (`TERM_PROGRAM` iTerm.app / WezTerm / vscode / ghostty, `WT_SESSION`, `KITTY_WINDOW_ID`, `VTE_VERSION` >= 5000). `SYNTHESIS_HYPERLINKS=1` forces them on, `=0` off; `NO_COLOR` has no effect. They add no width. Links are made per drawn row, so a wrapped link is only native-clickable on the row where each piece sits; the in-app gesture joins rows.
- Troubleshooting: set `SYNTHESIS_MOUSE_DEBUG=1` and click; the footer shows what the terminal sent, e.g. `mouse Down button=2 ctrl=false alt=false shift=false @40,12`. If a gesture shows nothing, the terminal swallowed it (Terminal.app opens its context menu on Ctrl+click while an app tracks the mouse; try Option+click, a right click, or hold the terminal's mouse-bypass key). Option+click may need "Use Option as Meta" or may be consumed by the terminal. Nothing here has been verified on a real Terminal.app, iTerm2, Ghostty, WezTerm or Windows Terminal yet; use the debug line to report what yours sends.

## Themes, colour and glyphs (interactive app and `--once`)
- Theme: `synthesis.tui.theme` in the USER settings file (`$SYNTHESIS_HOME/settings.json`): `synthesis` (default: the colours of the owner's reference mockups, navy surfaces with blue, teal, green, amber, red and purple accents), `classic` (the exact pre-redesign palette), `graphite` (kit silver with a saturated blue accent), `midnight`, `high-contrast`, `pearl` (light terminals; there is no auto-detection yet), `mono` or `auto` (the default theme on 256-colour and truecolor terminals). `synthesis.tui` in project, local or managed files is ignored with a diagnostic. A wrong value keeps the default and shows a note naming the key. There is no `/config` screen yet. The owner preferred the pre-redesign colours over the first redesign palette, hence the default; see `docs/specs/tui-design.md` sections 13 and 14.
- The terminal decides first: `NO_COLOR` (non-empty) forces the Mono theme (bold, dim and reverse only, no colour); a 16-colour terminal (`TERM` linux, xterm, screen, vt100 ...) gets an automatic 16-colour theme; the chosen theme only applies with 256 colours or truecolor. All themes are truecolor and are mapped to the nearest 256 colours automatically. The background of the terminal is never painted: on a light terminal use `pearl` or `mono`.
- `SYNTHESIS_COLOR=truecolor|24bit|256|16` overrides the detected colour depth (it replaces the old `CLAUDE_MONITOR_COLOR`, which is no longer read). It beats `NO_COLOR`.
- Glyphs: `synthesis.tui.glyphs` is `auto` (default), `unicode` or `ascii`. `auto` picks ASCII when `SYNTHESIS_ASCII=1` (`0` forces Unicode) or `TERM` is `linux`, `vt100`, `vt220`, `cons25` or `dumb`. ASCII is for terminals that draw ambiguous-width characters in two cells. Phase P1 applies the glyph set to the header, the logo and the welcome art; the other panels keep their Unicode symbols until the later redesign phases.
- The header shows the braille ring mark, the spaced wordmark `S Y N T H E S I S` (80 columns and wider; `SYNTHESIS` from 60, the mark alone below) and live status chips. The welcome screen draws the logo art only when the chat pane has enough spare rows and at least 40 columns.

## `--allow RULE` / `--deny RULE` (repeatable, `synth run` and the interactive app)
Settings rule syntax (`Bash(cargo test:*)`, `Edit`, `mcp__server__tool`). Flags are typed by you, so trusted.
- `deny: ["mcp__*"]` removes every MCP tool, `mcp__server__*` (or `mcp__server`) one server; `mcp__*` is never honoured as an allow rule.
- A `Read(path)` deny/ask rule also covers Grep, Glob, Write/Edit of that path (denied, not asked), and Bash readers of it (`cat`, `head`, `tail`, `less`, `grep`, `sed`, `awk`, `cp`, `< file` ...). It is checked on the command's own operands only; a command that builds the path in the shell (`cat "$F"`) cannot be matched, which is what the sandbox and the other rules are for.
- An ask rule prompts in every mode but `dontAsk` (denied there), `bypassPermissions` included; `synth run` has nobody to ask, so it is denied.
- A `settings.json` that exists but is malformed (a trailing comma, a missing brace) or unreadable stops `synth run` with exit 2 and `settings error: ...`, and the interactive app does not start, because the deny rules in it could not apply. Fix or remove the file.
- Memory files: per directory, the first non-empty of `SYNTHESIS.md`, `AGENTS.md`, `CLAUDE.md` (then `.claude/CLAUDE.md`) is loaded, root to cwd, 32 KiB combined; `CLAUDE.local.md` is added next to it.
- Hooks: `PreToolUse` and `UserPromptSubmit` fail closed and time out after 60 s by default; every other event defaults to 600 s. A per-hook `timeout` (seconds) overrides either.
- Read: a whole-file read over 256 KB, or a result over about 25k tokens, is refused with the offset/limit to use instead.
- Bash timeout: `synth run` kills the command; the interactive app keeps it running in the background and tells the model its `bash_id` (BashOutput / KillShell).
- Windows: no OS backend can confine commands, so without `--sandbox` the default is `best-effort` with a notice at start (commands run unconfined); pass `--sandbox enforce` to refuse them instead. macOS and Linux default to `enforce`. Deny wins over allow. `--allow` cannot lift critical-path or protected-path protections. A malformed rule is a usage error (exit 2).

## Long-term memory (`synth memory`, `--no-memory`)
Memory is data the model may read, never instructions or permissions. It lives in `$SYNTHESIS_HOME/memory/memory.db` (local, 0600), shared by all worktrees of a repository, and is on by default. Turn it off for one run with `--no-memory` (`synth run` and the interactive app), for everything with `"synthesis": {"memory": {"enabled": false}}` in the user `settings.json`, or for one repository with `synth memory optout on`. In a project that is not trusted (no `--trust-project`), only your own user-scope notes are injected.

| Command | Does |
|---|---|
| `synth memory add TEXT [--user] [--kind fact\|preference\|lesson] [--slot cmd.test] [--pin] [--title T]` | Save a note you typed (trust `owner`). A slotted note replaces the old value of that slot. |
| `synth memory list [--kind K] [--status S\|any] [--all]`, `search QUERY`, `show ID` | Read. Defaults: this repository plus user scope, active records. Search is full text (BM25), ranked. |
| `synth memory forget ID --reason R`, `delete ID`, `delete --workspace` | Archive a wrong note (keeps provenance), hard-delete one, or purge this repository. |
| `synth memory review`, `approve ID` | Proposed and quarantined records wait for you. Quarantine means the text carried a tag, hidden Unicode or an override phrase. |
| `synth memory export [--format jsonl\|md]`, `import FILE` | Secrets are masked again on export. Imported records never keep `owner` or `verified` trust. |
| `synth memory stats [--json]`, `prune` | Counts and size. `prune` deletes episodes older than 365 days and archives decayed, unused notes; facts, lessons and skills are never deleted automatically. |

At the start of a task the harness retrieves up to 8 notes for the request (about 2,000 tokens at most) and adds them as a `<memory trust="data">` block that stays fixed for the task. After a task it saves a short episode (request, status, changed files, verification report id). Secrets are masked before anything is stored. Design: docs/specs/memory.md.


After a task that went wrong and recovered, was corrected by you, hit a denial or used 8 or more tool calls, `synth run` queues a reflection job and, once the task result is final, makes one capped model call (60 s; at most 20 per day) that proposes up to 8 notes. The harness decides what is kept: trust is computed from the evidence the model cites (never above `agent`; `untrusted` when it cites web, browser or MCP output), secrets are masked, poisoned text is quarantined, and notes for all your repositories (user scope) wait in `synth memory review`. `synth memory reflect [--retry] [--limit N] [--provider P --model M]` runs queued or failed jobs by hand. Turn it off with `"synthesis": {"memory": {"reflection": false}}` in the user `settings.json`.

Proposals: once a lesson or note proves useful across runs, `synth memory proposals` lists a skill or an instruction-file addition. `synth memory apply ID` only shows the diff; `apply ID --yes` writes it (a skill becomes `learned:NAME`, available to the Skill tool). `reject ID` declines; `promote` looks for candidates now. Memory never edits an instruction file or skill by itself.
