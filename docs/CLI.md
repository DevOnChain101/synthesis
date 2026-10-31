# CLI notes: approvals, permission flags

## Workspace directory (`--cwd`)
`--cwd DIR` is optional for `synth run`: without it the workspace is the directory `synth` was started in (read once at the CLI boundary; the libraries never read the process cwd). `synth doctor` needs no workspace.

`synth` with no subcommand prints the help. The interactive terminal app and the desktop app were removed on 2026-10-31 (archived at tag `archive/ui-before-removal`; to be rebuilt later from the HTML design).

## Commands the agent runs (tests, builds)
The agent runs tests itself. A call that needs permission is denied unless a rule allows it. Verification-gate checks (`cargo test`, `npm test`, ...) go through the same permission check as a `Bash` call. Without an approver (`synth run`) the call is not run, the model is told which permission is missing and the task ends `blocked` (`approval_unavailable`); `task.ended.needed_permissions` lists the rules and `synth run` prints, e.g.:

    synth: blocked: approval needed for Bash(cargo test:*) — rerun with --allow 'Bash(cargo test:*)'

## Permission modes (`--permission-mode`)
`synth run --permission-mode default|acceptEdits|plan|dontAsk|auto|bypassPermissions`. `synth run` never prompts (no stdin): asks are decided by the mode and the `--allow` / settings allowlist; an ask nobody can grant is denied.

| mode | `synth run` behaviour |
|---|---|
| `default` | asks are auto-denied unless allowed by `--allow`/settings (`SendMessage` between agents asks; `ListAgents` is read-only) |
| `acceptEdits` | edits inside the workspace are allowed, and so is `SendMessage`; other asks (Bash, MCP, ...) are auto-denied |
| `plan` | read-only; writes are denied; `SendMessage` is denied (it can steer another agent) |
| `dontAsk` | an ask becomes a deny |
| `auto` | a judge model call decides each open call (allow / deny / ask); ask, unclear answer, error or timeout = denied |
| `bypassPermissions` | skips approval prompts; still stops: deny rules, ask rules (they still ask, so an unattended run denies them), critical `rm`, destructive MCP asks. Protected paths (`.git`, `.synthesis`, `.ssh`, ...) are NOT stopped by plain writes, as in Claude Code; the sandbox still confines commands. Loud notice at start. Only from the CLI flag: never from project settings, `.mcp.json` or the environment |

**Default limits.** `synth run` ends a task after 3600 s of wall time unless `--max-wall-secs N` says otherwise; turns (50) and tool calls (200) are bounded by default as well. `--max-cost-usd` has no default because it needs a configured price.

**Inferred verification checks.** In `default` and `acceptEdits` the checks the harness infers from the repo (`cargo test`, `npm run test`, `pytest`, `go test`, `make test`, ... see `docs/specs/verification.md` section 4) run without an `--allow`, so `synth run --permission-mode acceptEdits` no longer ends `blocked` only because such a check asked. They stay sandboxed and deny rules still win; any other check command (user-declared or model-registered, or not a recognised test/lint/build invocation) still needs an allow.

A denied write or command (Write, Edit, Bash, MCP write tools, ...) makes the task end `blocked` / `approval_unavailable` with `needed_permissions`, never `completed`. Typical CI use:

    synth run --permission-mode acceptEdits --allow 'Bash(cargo test:*)' --prompt "fix the failing test"

**Auto mode judge.** The judge uses the session provider and model; set another model of the same provider with `autoMode.model` in the user `settings.json` (with `autoMode.environment|allow|soft_deny|hard_deny` to steer it). It sees the task, earlier user messages, the workspace and the pending call, all as data (tool output and repo text are never sent). At most 50 judge calls per task (identical calls are cached); their cost counts in the task. Every decision is logged as a `permission.auto_verdict` event. Hard stops (deny rules, critical `rm`, protected paths, destructive MCP asks) are decided before it and it cannot relax them.

## `--allow RULE` / `--deny RULE` (repeatable, `synth run`)
Settings rule syntax (`Bash(cargo test:*)`, `Edit`, `mcp__server__tool`). Flags are typed by you, so trusted.
- `deny: ["mcp__*"]` removes every MCP tool, `mcp__server__*` (or `mcp__server`) one server; `mcp__*` is never honoured as an allow rule.
- A `Read(path)` deny/ask rule also covers Grep, Glob, Write/Edit of that path (denied, not asked), and Bash readers of it (`cat`, `head`, `tail`, `less`, `grep`, `sed`, `awk`, `cp`, `< file` ...). It is checked on the command's own operands only; a command that builds the path in the shell (`cat "$F"`) cannot be matched, which is what the sandbox and the other rules are for.
- An ask rule prompts in every mode but `dontAsk` (denied there), `bypassPermissions` included; `synth run` has nobody to ask, so it is denied.
- A `settings.json` that exists but is malformed (a trailing comma, a missing brace) or unreadable stops `synth run` with exit 2 and `settings error: ...`, because the deny rules in it could not apply. Fix or remove the file.
- Memory files: per directory, the first non-empty of `SYNTHESIS.md`, `AGENTS.md`, `CLAUDE.md` (then `.claude/CLAUDE.md`) is loaded, root to cwd, 32 KiB combined; `CLAUDE.local.md` is added next to it.
- Hooks: `PreToolUse` and `UserPromptSubmit` fail closed and time out after 60 s by default; every other event defaults to 600 s. A per-hook `timeout` (seconds) overrides either.
- Read: a whole-file read over 256 KB, or a result over about 25k tokens, is refused with the offset/limit to use instead.
- Bash timeout: `synth run` kills the command.
- Windows: no OS backend can confine commands, so without `--sandbox` the default is `best-effort` with a notice at start (commands run unconfined); pass `--sandbox enforce` to refuse them instead. macOS and Linux default to `enforce`. Deny wins over allow. `--allow` cannot lift critical-path or protected-path protections. A malformed rule is a usage error (exit 2).

## `--peers` / `--name NAME` (cross-session messaging, `synth run`)
Off by default. Another live session of the same OS user on the same machine can ask this one a question, and this one can ask it, with no human in the loop (docs/specs/messaging.md Part B).
- Turn it on with `"synthesis": {"peers": {"enabled": true}}` in the USER `settings.json`, or `--peers` for one run. A project `settings.json` is ignored for `synthesis.peers` (with a diagnostic).
- `--name NAME` (`[a-z0-9_-]`, at most 32, starting with a letter or digit): the session answers to `sess:NAME`. A name already used by a live session in the same workspace is a usage error (exit 2); it is never suffixed. Without peers on, the name is unused.
- `synthesis.peers` keys: `enabled` (false), `allow` (default: this workspace and the other worktrees of the same repository, `["workspace:<root>", "repo:<repo_key>"]`; a list you write is used as written), `deny`, `initiate` (true), `answer` (true), `confirm_replies` (false, not built), `max_hops` (2), `max_cost_usd` (1.0, what peer answers may cost this session), `resume_offline` (false, not built). Rules: `workspace:<path>` (a trailing `*` is a prefix wildcard), `ws:<label|key-prefix>`, `sess:<name|id-prefix>`, `repo:<repo_key>`; deny beats allow. The same list says whom this session may ask and who may ask it.
- Model tools: `ListSessions` (only with peers on), `ListAgents {scope:"all"}`, and `SendMessage {to:"ws:<label>/sess:<name>[/agent:<instance>]"}`. A peer answers once, through a read-only responder (Read, Glob, Grep, LS; mode `stricter(current, plan)`; asks are denied; 8 turns, 20 tool calls); the answer arrives as a fenced `<peer_reply trust="peer-data">` at the asker's next safe point and the task waits for it. A reply is information: it never approves anything. A `SendMessage` to a permitted peer needs no prompt when `initiate` is true; to anything else it asks (`synth run`: denied). `plan` and `dontAsk` still deny every `SendMessage`.
- Transport: a unix socket (0600, in `$SYNTHESIS_HOME/run`, 0700) or a Windows named pipe, mutual HMAC-SHA256 handshake with `run/peer.key`. No TCP port. `$SYNTHESIS_HOME/run` and the other Synthesis state are hard-denied for reading by file tools (every mode), refused in Bash commands that name them, and denied inside the sandbox. A peer's question is answered by a responder that cannot read secret-shaped files, anything under `$SYNTHESIS_HOME`, or anything outside the workspace. A socket path over 100 bytes turns peers off for the session with a notice. On Windows peers stay off unless `SYNTHESIS_HOME` is under `%USERPROFILE%`.
- A session answers peers only when its model has a price (`synthesis.pricing` in user `settings.json`); without one it declines with `no_price`, because the cost cap needs a price.
- `synth doctor` prints a `peers` block (enabled, platform, socket path length, limits).

## Long-term memory (`synth memory`, `--no-memory`)
Memory is data the model may read, never instructions or permissions. It lives in `$SYNTHESIS_HOME/memory/memory.db` (local, 0600), shared by all worktrees of a repository, and is on by default. Turn it off for one run with `--no-memory` (`synth run`), for everything with `"synthesis": {"memory": {"enabled": false}}` in the user `settings.json`, or for one repository with `synth memory optout on`. In a project that is not trusted (no `--trust-project`), only your own user-scope notes are injected. A repository can only turn memory off (`.synthesis/settings.json` `"synthesis": {"memory": {"enabled": false}}`); the user `settings.json` can list repository roots to skip with `"synthesis": {"memory": {"disabled_workspaces": ["**/client-*"]}}`.

In a run the model has four tools while memory is on: `MemorySearch` (read), `MemoryWrite` (workspace scope allowed; user scope asks, so it is denied headless), `MemoryUpdate` and `MemoryForget` (only notes the agent saved itself for this workspace). Sub-agents get `MemorySearch` only. `synth doctor` prints a `memory:` block: store path, schema, size, `quick_check`, records by trust, jobs, masking and whether reflection is on.

| Command | Does |
|---|---|
| `synth memory add TEXT [--user] [--kind fact\|preference\|lesson] [--slot cmd.test] [--pin] [--title T]` | Save a note you typed (trust `owner`). A slotted note replaces the old value of that slot. |
| `synth memory list [--kind K] [--status S\|any] [--all]`, `search QUERY`, `show ID` | Read. Defaults: this repository plus user scope, active records. Search is full text (BM25), ranked. |
| `synth memory edit ID` | Open the note in `$VISUAL`/`$EDITOR` (first line the title, then a blank line and the body); saves a new `owner` version through the normal write path (secrets masked, scanner). Needs a terminal and an editor; a refused save keeps the old note and prints where your text is kept. |
| `synth memory forget ID --reason R`, `delete ID`, `delete --workspace` | Archive a wrong note (keeps provenance), hard-delete one, or purge this repository. |
| `synth memory review`, `approve ID` | Proposed and quarantined records wait for you. Quarantine means the text carried a tag, hidden Unicode or an override phrase. |
| `synth memory export [--format jsonl\|md]`, `import FILE`, `import --claude [--cwd D]` | Secrets are masked again on export. Imported records never keep `owner` or `verified` trust. `--claude` reads the auto-memory that `synth import claude` staged for this repository; every note arrives `untrusted` and `proposed`, so it waits in `review`. |
| `synth memory stats [--json]`, `prune` | Counts, size, injected tokens (p50/p95), hit rate, reflection cost (7 d, 30 d). `prune` deletes episodes older than 365 days, archives decayed, unused notes and merges near-duplicate agent notes (also automatic after every 20th task); facts, lessons and skills are never deleted automatically. |

At the start of a task the harness retrieves up to 8 notes for the request (about 2,000 tokens at most) and adds them as a `<memory trust="data">` block that stays fixed for the task. After a task it saves a short episode (request, status, changed files, verification report id). Secrets are masked before anything is stored. Design: docs/specs/memory.md.


After a task that went wrong and recovered, was corrected by you, hit a denial or used 8 or more tool calls, `synth run` queues a reflection job and, once the task result is final, makes one capped model call (60 s; at most 20 per day) that proposes up to 8 notes. The harness decides what is kept: trust is computed from the evidence the model cites (never above `agent`; `untrusted` when it cites web, browser or MCP output), secrets are masked, poisoned text is quarantined, and notes for all your repositories (user scope) wait in `synth memory review`. `synth memory reflect [--retry] [--limit N] [--provider P --model M]` runs queued or failed jobs by hand. Reflection stops for the day at `synthesis.memory.daily_budget_usd` (default 0.50; needs a price in `synthesis.pricing` for the reflection model, otherwise only the 20-calls-per-day cap applies). Reflection sends the task digest (request, tool calls, your turns) to the model provider, so it is **off by default**: opt in with `"synthesis": {"memory": {"reflection": true}}` in the user `settings.json` (a repository cannot turn it on). Without it no job is queued and no model call is made; the deterministic episode is still saved locally.

Proposals: once a lesson or note proves useful across runs, `synth memory proposals` lists a skill or an instruction-file addition. `synth memory apply ID` only shows the diff; `apply ID --yes` writes it (a skill becomes `learned:NAME`, available to the Skill tool). `reject ID` declines; `promote` looks for candidates now. Memory never edits an instruction file or skill by itself.

Retrieval quality and the poisoned-record checks run offline in `cargo test -p synthesis-memory --test quality`.

Commands that change memory (add, delete, forget, approve, reject, apply --yes, import, optout, prune, promote, reflect) need a terminal and refuse inside the agent's own commands; the agent cannot run them (a permission rule denies them). Imported records arrive `untrusted` and `proposed`; notes learned in an untrusted project or in a task that used the web, MCP or network commands are stored `untrusted` and never injected.
