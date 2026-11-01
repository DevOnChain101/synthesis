# The interactive app (`synth`, `synth tui`)

Branch `restore/ui-classic`. `synth` with no subcommand (on a terminal) or `synth tui` opens it;
without a terminal it prints the usage and exits 2. It runs the same wiring as `synth run`
(`synthesis-app::run::prepare`, `Front::Tui`); it is a client of the harness event stream.

Flags: `--cwd DIR` (default: the current directory), `--resume ID` / `--continue` (the stored
session continues in place), `--once` (print one frame and exit), plus the `synth run` flags
(`--model`, `--sandbox`, `--allow`, `--worktree`, `--peers`, ...). `--fork-session` and
`--permission-mode bypassPermissions` are for `synth run` only (bypass: Shift+Tab, confirm).

## Commands

| command | does |
|---|---|
| /help | keys and commands, plus the skills and MCP prompts of this session |
| /clear | new conversation in the same session |
| /compact [instructions] | summarise the conversation (`Harness::request_compact` / compact now) |
| /new | pick a folder; a fresh session starts there |
| /resume [id] | continue a saved session (picker: this folder first) |
| /model [name] | pick or set a model; restarts on the same session |
| /effort [auto\|low\|...] | reasoning effort, only when the model declares `effort_tiers` |
| /mcp | MCP servers: state, tools, last error |
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

## Events

Every event type of `synthesis-events` has a row or a reason not to: conversation, tool, approval,
verification, budget, compaction, sub-agent and message events have their own views;
peers, memory, processes, mode changes, non-successful hooks and any future kind get one sanitised
`type: key=value` row; bookkeeping (tool.started, session lifecycle, todo/turn/delta events) is
shown elsewhere (header, todo list, streaming text). An approval made for another session says
`for <origin>:`.

## Per task, like `synth run`

MCP mentions and prompts are expanded, the memory block is retrieved for the typed prompt, the
memory episode and (opt-in) reflection run after it, usage reaches the session store, and the
`SessionStart`/`SessionEnd` hooks run per session. Not done in the app: `--json-schema`, limits
flags (`--max-turns`, ...), `--criteria-file`.

## Tests

`cargo test -p synthesis-tui-base -p synthesis-tui-render -p synthesis-tui` (render goldens in
`tests/golden/{render,keys,sgr,selection,welcome}`), and `cargo test -p synthesis-cli --features
test-provider --test tui_pty` (the real binary on a pty with a scripted model; needs python3).
