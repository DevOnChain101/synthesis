# CLI notes: approvals, permission flags

Naming: text that identifies the product to a reader (version headline, titles, notices) says Synthesis; technical names are short: command `synt`, `~/.synt`, `.synt/`, `synt.json`, `synt.db`, `AGENTS.md`, env `SYNT_*`. The names used before 2026-11-02 are never read; `synt doctor` detects them and prints how to migrate (docs/MIGRATION.md).

## Workspace directory (`--cwd`)
`--cwd DIR` is optional for `synt run`: without it the workspace is the directory `synt` was started in (read once at the CLI boundary; the libraries never read the process cwd). `synt doctor` needs no workspace.

`synt` with no subcommand opens the interactive terminal app (see `docs/TUI.md`); without a terminal on stdin and stdout it prints the usage and exits 2. `synt tui` is the same, `synt run` is headless. The native GPUI desktop app lives at `apps/desktop` (`docs/specs/desktop.md`, a separate binary `synthesis-desktop`); the redesigned (N1-N5) TUI stays removed (archived at tag `archive/ui-before-removal`).

## Commands the agent runs (tests, builds)
The agent runs tests itself. A call that needs permission is denied unless a rule allows it. Verification-gate checks (`cargo test`, `npm test`, ...) go through the same permission check as a `Bash` call. Without an approver (`synt run`) the call is not run, the model is told which permission is missing and the task ends `blocked` (`approval_unavailable`); `task.ended.needed_permissions` lists the rules and `synt run` prints, e.g.:

    synt: blocked: approval needed for Bash(cargo test:*) — rerun with --allow 'Bash(cargo test:*)'

## Permission modes (`--permission-mode`)
`synt run --permission-mode default|acceptEdits|plan|dontAsk|auto|bypassPermissions`. `synt run` never prompts (no stdin): asks are decided by the mode and the `--allow` / settings allowlist; an ask nobody can grant is denied.

| mode | `synt run` behaviour |
|---|---|
| `default` | asks are auto-denied unless allowed by `--allow`/settings (`SendMessage` between agents asks; `ListAgents` is read-only) |
| `acceptEdits` | edits inside the workspace are allowed, and so is `SendMessage`; other asks (Bash, MCP, ...) are auto-denied |
| `plan` | read-only; writes are denied, whatever `--allow` says; `SendMessage` is denied (it can steer another agent) |
| `dontAsk` | an ask becomes a deny |
| `auto` | like `acceptEdits`, edits inside the workspace are allowed without the judge (no `permission.auto_verdict` event for them); a judge model call decides the other open calls (Bash, MCP, edits outside the workspace) as allow / deny / ask; ask, unclear answer, error or timeout = denied |
| `bypassPermissions` | skips approval prompts; still stops: deny rules, ask rules (they still ask, so an unattended run denies them), critical `rm`, destructive MCP asks. Protected paths (`.git`, `.synt`, `.ssh`, ...) are NOT stopped by plain writes, as in Claude Code (work delegated by a peer session is the exception: it asks); the sandbox still confines commands. Loud notice at start. Only from the CLI flag: never from project settings, `.mcp.json` or the environment |

**Browser login (`browser_login`, `browser_fill_secret`).** Exception to "never prompts": when a terminal exists (`/dev/tty`, `CONIN$` on Windows), `synt run` asks `[y/N]` there before opening a visible browser for `browser_login` and before the first use of a stored secret on an origin; the person then logs in and presses Enter (or runs `synt browser done PROFILE` from another terminal and types the one-time code that the waiting terminal printed). A saved login (`browser_navigate` with `profile`) asks the same way once per session. Without a terminal all three are refused unless pre-approved in the user settings (`synt.browser.approved`). The credential tools also need `--sandbox enforce` (or `synt.browser.allow_unsandboxed`). Every other ask stays auto-denied. Details, settings (`synt.browser.secrets`) and the threat model: docs/BROWSER-LOGIN.md. `synt doctor` prints the profiles directory and whether a display is available.

**Default limits.** `synt run` ends a task after 3600 s of wall time unless `--max-wall-secs N` says otherwise; turns (50) and tool calls (200) are bounded by default as well. `--max-cost-usd` has no default because it needs a configured price.

**Inferred verification checks.** In `default` and `acceptEdits` the checks the harness infers from the repo (`cargo test`, `npm run test`, `pytest`, `go test`, `make test`, ... see `docs/specs/verification.md` section 4) run without an `--allow`, so `synt run --permission-mode acceptEdits` no longer ends `blocked` only because such a check asked. They stay sandboxed and deny rules still win; any other check command (user-declared or model-registered, or not a recognised test/lint/build invocation) still needs an allow.

A write or command (Write, Edit, Bash, MCP write tools, ...) whose ASK is auto-denied because nobody can answer makes the task end `blocked` / `approval_unavailable` with `needed_permissions`, never `completed`. A denial by policy (`plan`, `dontAsk`, or a `--deny` / deny rule) is not a missing approval: the model is told it is a policy decision and the task can still end `completed` (see `docs/specs/tools.md`). Exception: a denied model-registered `verify_run` check (a command the model chose, not one the harness inferred) does not block the task: the edits were authorised, so the task ends `unverified` (exit 3) and `needed_permissions` still names the rule. Typical CI use:

    synt run --permission-mode acceptEdits --allow 'Bash(cargo test:*)' --prompt "fix the failing test"

**Auto mode judge.** The judge uses the session provider and model; set another model of the same provider with `autoMode.model` in the user `settings.json` (with `autoMode.environment|allow|soft_deny|hard_deny` to steer it). It sees the task, earlier user messages, the workspace and the pending call, all as data (tool output and repo text are never sent). At most 50 judge calls per task (identical calls are cached); their cost counts in the task. Every decision is logged as a `permission.auto_verdict` event. Hard stops (deny rules, critical `rm`, protected paths, destructive MCP asks) are decided before it and it cannot relax them.

## `--allow RULE` / `--deny RULE` (repeatable, `synt run`)
Settings rule syntax (`Bash(cargo test:*)`, `Edit`, `mcp__server__tool`). Flags are typed by you, so trusted.
- `deny: ["mcp__*"]` removes every MCP tool, `mcp__server__*` (or `mcp__server`) one server; `mcp__*` is never honoured as an allow rule.
- A `Read(path)` deny/ask rule also covers Grep, Glob, Write/Edit of that path (denied, not asked), and Bash readers of it (`cat`, `head`, `tail`, `less`, `grep`, `sed`, `awk`, `cp`, `< file` ...). It is checked on the command's own operands only; a command that builds the path in the shell (`cat "$F"`) cannot be matched, which is what the sandbox and the other rules are for.
- An ask rule prompts in every mode but `dontAsk` (denied there), `bypassPermissions` included; `synt run` has nobody to ask, so it is denied.
- A `settings.json` that exists but is malformed (a trailing comma, a missing brace) or unreadable stops `synt run` with exit 2 and `settings error: ...`, because the deny rules in it could not apply. Fix or remove the file.
- Memory files: per directory, the first non-empty of `AGENTS.md`, `CLAUDE.md` (then `.claude/CLAUDE.md`) is loaded, root to cwd, 32 KiB combined; `CLAUDE.local.md` is added next to it.
- Hooks: `PreToolUse` and `UserPromptSubmit` fail closed and time out after 60 s by default; every other event defaults to 600 s. A per-hook `timeout` (seconds) overrides either.
- Read: a whole-file read over 256 KB, or a result over about 25k tokens, is refused with the offset/limit to use instead.
- Bash timeout: `synt run` kills the command.
- Windows: no OS backend can confine commands, so without `--sandbox` the default is `best-effort` with a notice at start (commands run unconfined); pass `--sandbox enforce` to refuse them instead. macOS and Linux default to `enforce`. Deny wins over allow. `--allow` cannot lift critical-path or protected-path protections. A malformed rule is a usage error (exit 2).

## `--peers` / `--name NAME` / `--about TEXT` (cross-session messaging, `synt run`)
Off by default. Another live session of the same OS user on the same machine can ask this one a question, and this one can ask it, with no human in the loop (docs/specs/messaging.md Part B).
- Turn it on with `"synt": {"peers": {"enabled": true}}` in the USER `settings.json`, or `--peers` for one run. A project `settings.json` is ignored for `synt.peers` (with a diagnostic).
- `--name NAME` (`[a-z0-9_-]`, at most 32, starting with a letter or digit): the session answers to `sess:NAME`. A name already used by a live session in the same workspace is a usage error (exit 2); it is never suffixed. Without peers on, the name is unused.
- `--about TEXT` (at most 120 characters): what this session is for. Default: the first non-empty line of `AGENTS.md` at the workspace root. It is redacted and shown to allowed peers in the `session.about` field of `ListSessions` / `ListAgents` rows.
- `synt run --resume ID --resume-any-workspace` continues a session of another workspace in the CURRENT `--cwd`: the work happens there, and the stored session's `cwd` becomes this directory from then on (the TUI picker instead reopens a session in its own folder). `synt mcp add NAME` refuses a name that already exists in that file unless `--force` is given.

**`synt mcp add` credential guard (best effort).** `mcp add` refuses to write a literal credential into the config file. It matches names by whole words (split on `_ - .`, camelCase and digits): `--api-key`, `API_KEY`, `x-api-key`, `accessToken`, `client_secret`, `--auth-token`, `Authorization` are caught, while `--author`, `--keyboard`, `--design` and `--key-file /path` are not. It also checks the shape of values (known token prefixes, `Bearer <token>`, `user:password@` in a URL) and, in a `--url`, user info, query parameters (split on `&` and `;`, bare keys too), path parameters (`/mcp;key=...`) and the fragment. It cannot recognise every secret: a credential with an unusual name and no known prefix can still get through, so do not rely on it. The supported way is a `${VAR}` reference (for example `--header 'Authorization: Bearer ${MY_TOKEN}'`), which is stored as written and resolved from the environment at start.
- `synt.peers` keys: `enabled` (false), `allow` (default: this workspace and the other worktrees of the same repository, `["workspace:<root>", "repo:<repo_key>"]`; a list you write is used as written), `deny`, `initiate` (true), `answer` (true), `confirm_replies` (false, not built), `max_hops` (2), `max_cost_usd` (1.0, what peer answers may cost this session), `resume_offline` (false, not built). Rules: `workspace:<path>` (a trailing `*` is a wildcard that stops at a path boundary: `/work/api*` is `/work/api` and below, never `/work/api-evil`), `ws:<label|key-prefix>`, `ws:<label>/sess:<name>`, `sess:<name|id-prefix>`, `repo:<repo_key>`; deny beats allow. The same list says whom this session may ask and who may ask it.
- Model tools: `ListSessions` (only with peers on), `ListAgents {scope:"all"}`, and `SendMessage {to:"ws:<label>/sess:<name>[/agent:<instance>]"}`. A peer answers once, through a read-only responder (Read, Glob, Grep, LS; mode `stricter(current, plan)`; asks are denied; 8 turns, 20 tool calls); the answer arrives as a fenced `<peer_reply trust="peer-data">` at the asker's next safe point and the task waits for it. A reply is information: it never approves anything. A `SendMessage` to a permitted peer needs no prompt when `initiate` is true; with `initiate` false, or to anything else, it asks (`synt run`: denied). `plan` and `dontAsk` still deny every `SendMessage`.
- Threads: `SendMessage {thread:true}` starts a multi-round conversation (the result and the `<peer_reply>` carry its `thread_id`); `thread_id:"<id>"` continues it, and the peer's responder resumes its own transcript instead of starting fresh. Caps (`synt.peers`): `max_rounds` 6 (round 7 is `thread_closed`), `thread_idle_min` 30, `max_threads` 4 open per receiver (the 5th is `busy`). A thread belongs to the session that opened it. Memory only: threads end with the receiver's process.
- Handoff: `SendMessage {notify_on_end:true}` is not a question: the peer answers when its current task ends (`kind="result"`: status, final text, changed files, at most 8 KiB, redacted), or at once with `idle` when none runs. The asking task waits for it up to `notify_wait_max_s` (3600). One subscription per asker session, 8 in all; a session that waits on X refuses X's own subscription (`cycle`). Same gates as a reply (`allow`, `answer`, `confirm_replies`), and the receiver must set `accept_notify: true` in its user settings (default off, else `notify_off`): the result is the main task's final text and changed files (files naming a secret, Synthesis state or an outside path are left out). At most 2 subscriptions per asker workspace.
- Shared command output: responders never see Bash results, except those of commands you list in the USER setting `synt.peers.share_commands` (default empty), e.g. `["cargo test*", "npm test*"]`. A trailing `*` is a prefix that ends at a word (`cargo test*` is not `cargo testx`, `ls*` not `lsof`); flags and arguments after it are not constrained; the whole command must match and may contain none of `; | & $ ( ) < >`, a backtick or a newline, and name no secret, Synthesis state or outside path. The output is redacted and cut to 8 KiB.
- Delegation (off by default): `SendMessage {delegate:true}` asks a peer to DO a task, not only to answer. The peer must opt in in its USER `settings.json` (`synt.peers.accept_delegation: true`) and name who may ask in `delegation_allow` (same rules as `allow`; empty = nobody). The task runs as a sub-agent inside the peer's own session, with the peer's tools, rules, hooks, sandbox and approver, in the peer's permission mode capped by the SENDER's mode (`stricter(peer mode, sender mode)`; unknown = `plan`). In `default`/manual the peer's user approves each call (the dialog starts with `[peer request from ... (unverified sender)]` and offers no "allow always"; the asker waits up to 10 minutes); a headless peer denies every ask; in `acceptEdits`/`auto`/`bypassPermissions` it acts. Deny rules stay blocked in every mode. Delegated work is held stricter than this session's own agent: its file tools never read secret files or Synthesis state; its Bash is denied when a word names a secret or Synthesis state (reads and writes) and asks (headless: denied) when a word could build one (`$`, backtick, glob, `~user`); protected paths ask even under `bypassPermissions` (headless: denied); its `SendMessage` follows your normal approval. This is a lexical scan, the sandbox is the real boundary. `delegation_allow` is policy against honest peers, not authentication (`from` and the sender's mode are self-asserted), and a bare `sess:<name>` is ignored there: write `ws:<label>/sess:<name>`. Bounds: one delegation at a time, `delegation_max_tool_calls` (40) and `delegation_wall_s` (900) per round, the `max_cost_usd` pool, rounds as threads, `delegation_max_hops` (2: the peer's worker may delegate one further hop; a session already on the way is a `cycle`). The sender's own mode decides whether it may send: `default` asks its user, `plan`/`dontAsk` deny, a headless `default` run is denied. The result comes back like a reply (fenced `<peer_reply>`, redacted, 32 KiB). A delegation to a session that has ended waits as a read-only question. Audit events: `peer.delegation.accepted|started|tool|completed|denied`. Unverified: `from` and the sender's mode are self-asserted under the shared key (messaging.md J6).
- Transport: a unix socket (0600, in `$SYNT_HOME/run`, 0700) or a Windows named pipe, mutual HMAC-SHA256 handshake with `run/peer.key`. No TCP port. `$SYNT_HOME/run` and the other Synthesis state are hard-denied for reading by file tools (every mode), refused in Bash commands that name them, and denied inside the sandbox. A peer's question is answered by a responder that cannot read secret-shaped files, anything under `$SYNT_HOME`, or anything outside the workspace. A socket path over 100 bytes turns peers off for the session with a notice. On Windows peers stay off unless `SYNT_HOME` is under `%USERPROFILE%`.
- A session answers peers only when its model has a price (`synt.pricing` in user `settings.json`); without one it declines with `no_price`, because the cost cap needs a price.
- `synt doctor` prints a `peers` block (enabled, platform, socket path length, limits).

## Long-term memory (`synt memory`, `--no-memory`)
Memory is data the model may read, never instructions or permissions. It lives in `$SYNT_HOME/memory/memory.db` (local, 0600), shared by all worktrees of a repository, and is on by default. Turn it off for one run with `--no-memory` (`synt run`), for everything with `"synt": {"memory": {"enabled": false}}` in the user `settings.json`, or for one repository with `synt memory optout on`. In a project that is not trusted (no `--trust-project`), only your own user-scope notes are injected. A repository can only turn memory off (`.synt/settings.json` `"synt": {"memory": {"enabled": false}}`); the user `settings.json` can list repository roots to skip with `"synt": {"memory": {"disabled_workspaces": ["**/client-*"]}}`.

In a run the model has four tools while memory is on: `MemorySearch` (read), `MemoryWrite` (workspace scope allowed; user scope asks, so it is denied headless), `MemoryUpdate` and `MemoryForget` (only notes the agent saved itself for this workspace). Sub-agents get `MemorySearch` only. `synt doctor` prints a `memory:` block: store path, schema, size, `quick_check`, records by trust, jobs, masking and whether reflection is on.

| Command | Does |
|---|---|
| `synt memory add TEXT [--user] [--kind fact\|preference\|lesson] [--slot cmd.test] [--pin] [--title T]` | Save a note you typed (trust `owner`). A slotted note replaces the old value of that slot. |
| `synt memory list [--kind K] [--status S\|any] [--all]`, `search QUERY`, `show ID` | Read. Defaults: this repository plus user scope, active records. Search is full text (BM25), ranked. |
| `synt memory edit ID` | Open the note in `$VISUAL`/`$EDITOR` (first line the title, then a blank line and the body); saves a new `owner` version through the normal write path (secrets masked, scanner). Needs a terminal and an editor; a refused save keeps the old note and prints where your text is kept; that temporary directory is not deleted for you, remove it yourself. |
| `synt memory forget ID --reason R`, `delete ID`, `delete --workspace` | Archive a wrong note (keeps provenance), hard-delete one, or purge this repository. |
| `synt memory review`, `approve ID` | Proposed and quarantined records wait for you. Quarantine means the text carried a tag, hidden Unicode or an override phrase. |
| `synt memory export [--format jsonl\|md]`, `import FILE`, `import --claude [--cwd D]` | Secrets are masked again on export. Imported records never keep `owner` or `verified` trust. `--claude` reads the auto-memory that `synt import claude` staged for this repository; every note arrives `untrusted` and `proposed`, so it waits in `review`. |
| `synt memory stats [--json]`, `prune` | Counts, size, injected tokens (p50/p95), hit rate, reflection cost (7 d, 30 d). `prune` deletes episodes older than 365 days, archives decayed, unused notes and merges near-duplicate agent notes (also automatic after every 20th task); facts, lessons and skills are never deleted automatically. |

At the start of a task the harness retrieves up to 8 notes for the request (about 2,000 tokens at most) and adds them as a `<memory trust="data">` block that stays fixed for the task. After a task it saves a short episode (request, status, changed files, verification report id). Secrets are masked before anything is stored. Design: docs/specs/memory.md.


After a task that went wrong and recovered, was corrected by you, hit a denial or used 8 or more tool calls, `synt run` queues a reflection job and, once the task result is final, makes one capped model call (60 s; at most 20 per day) that proposes up to 8 notes. The harness decides what is kept: trust is computed from the evidence the model cites (never above `agent`; `untrusted` when it cites web, browser or MCP output), secrets are masked, poisoned text is quarantined, and notes for all your repositories (user scope) wait in `synt memory review`. `synt memory reflect [--retry] [--limit N] [--provider P --model M]` runs queued or failed jobs by hand. Reflection stops for the day at `synt.memory.daily_budget_usd` (default 0.50; needs a price in `synt.pricing` for the reflection model, otherwise only the 20-calls-per-day cap applies). Reflection sends the task digest (request, tool calls, your turns) to the model provider, so it is **off by default**: opt in with `"synt": {"memory": {"reflection": true}}` in the user `settings.json` (a repository cannot turn it on). Without it no job is queued and no model call is made; the deterministic episode is still saved locally.

Proposals: once a lesson or note proves useful across runs, `synt memory proposals` lists a skill or an instruction-file addition. `synt memory apply ID` only shows the diff; `apply ID --yes` writes it (a skill becomes `learned:NAME`, available to the Skill tool). `reject ID` declines; `promote` looks for candidates now. Memory never edits an instruction file or skill by itself.

Retrieval quality and the poisoned-record checks run offline in `cargo test -p synthesis-memory --test quality`.

Commands that change memory (add, delete, forget, approve, reject, apply --yes, import, optout, prune, promote, reflect) need a terminal and refuse inside the agent's own commands; the agent cannot run them (a permission rule denies them). Imported records arrive `untrusted` and `proposed`; notes learned in an untrusted project or in a task that used the web, MCP or network commands are stored `untrusted` and never injected.

## `synt models`
Lists the configured models, one `provider/model` per line. `--json` adds `context_window`, `max_output` and `vision` per model (`null` = unknown) from the model-info cache (docs/specs/config.md §6.2). `--refresh` asks every provider with a credential for its model list (network, ignores the 24 h freshness), rewrites the cache, adds the discovered models, and prints tab-separated columns: `provider/model`, context window, max output, vision (`yes`/`no`); `-` = the provider did not say.

## Flag and command reference

Checked against `--help` by `apps/cli/tests/docs_flags.rs`: a flag that `synt --help`, `synt run --help` or `synt tui --help` lists and this file does not mention fails the test.

Flags of `synt run` and of the interactive app (`synt`, `synt tui`):

| Flag | Does |
|---|---|
| `--cwd DIR` | Start folder / workspace directory (default: the current directory). |
| `--sandbox MODE` | Command confinement: `enforce` (default), `best-effort` or `off` (loud warning). On Windows the default is `best-effort` with a notice. |
| `--network MODE` | Network for tool commands: `deny` (default), `loopback` or `allow` (loud warning). |
| `--sandbox-read PATH` | Extra read-only path for commands, e.g. a toolchain dir. Must exist; `$HOME`, filesystem roots and credential-store directories are refused (repeatable). |
| `--provider ID` | Provider id (with `--model NAME`); `fake:<script.json>` in test-provider builds. |
| `--model NAME` | `provider/model`, an alias or a bare model name. |
| `--permission-mode MODE` | `default`, `acceptEdits`, `plan`, `dontAsk`, `auto` or `bypassPermissions` (see above). |
| `--trust-project` | Trust `<cwd>/.synt/config.json`, `.synt/settings.json`, agents and (with approval) MCP servers. |
| `--approve-mcp NAME` | Start this project-scope MCP server (needs `--trust-project`; repeatable). |
| `--no-session-store` | Do not write the session to `synt.db` (no resume, no rewind). |
| `--no-memory` | No long-term memory: nothing retrieved, injected or saved. |
| `--allow RULE` | Allow a tool call without asking (repeatable; see above). |
| `--deny RULE` | Deny a tool call; deny wins over every allow (repeatable). |
| `--worktree` | Work in an isolated git worktree on branch `synthesis/<session>`; left in place for review. |
| `--cleanup-worktree` | With `--worktree`: remove the worktree at the end when nothing changed. |
| `--peers` | Turn cross-session messaging on for this run (see above). |
| `--name NAME` | Session name other sessions can address (`sess:<name>`). |
| `--about TEXT` | What this session is for, shown to peers. |
| `--resume ID` | Continue a stored session of this workspace by id (or unique prefix) with the new prompt. |
| `--resume-any-workspace` | With `--resume`: also accept a session of another workspace. |
| `-c, --continue` | Continue the most recent session of this workspace. |
| `--fork-session` | With `--resume`/`--continue`: start a new session seeded with the stored conversation. |
| `--system-prompt TEXT` | Replace the default system rules (the untrusted-data rule stays). |
| `--system-prompt-file FILE` | Same, read from a file. |
| `--append-system-prompt TEXT` | Add text after the system rules. |
| `--append-system-prompt-file FILE` | Same, read from a file. |
| `--add-dir DIR` | Extra working directory the agent may access (repeatable). |
| `--settings FILE` | Extra `settings.json` layered above local settings (managed settings still win). |
| `--require-mcp NAME` | Fail (exit 2) unless this MCP server starts (repeatable). |
| `--bare` | Minimal run: no hooks, no instruction files, no long-term memory. |

Only `synt run`:

| Flag | Does |
|---|---|
| `-p, --prompt TEXT` | The task. Without it the prompt is read from `--prompt-file`, `--prompt-command` or stdin. |
| `--prompt-file FILE` | Read the task from a file. |
| `--prompt-command CMD` | An MCP prompt as the task: `"/mcp__<server>__<prompt> arg1 arg2"` (`synt mcp prompts` lists them). |
| `--max-turns N` | Turn budget (default 50). |
| `--max-tool-calls N` | Tool-call budget (default 200). |
| `--max-cost-usd X` | Cost cap; needs a price for the model (`synt.pricing` in the user `settings.json`). |
| `--max-wall-secs N` | Wall-clock cap in seconds (default 3600). |
| `--criteria-file FILE` | JSON array of `{id?, text, check?}` (or plain strings): the task is not done until they pass. |
| `--events-out FILE` | Also write the event stream to this file. |
| `--output-format F` | What stdout carries: `text` (final message), `json` (one result object) or `stream-json` (the JSONL events, default). |
| `--json-schema SCHEMA` | JSON Schema (inline or a file) the final answer must satisfy; exit 1 when it does not. |

Only the interactive app:

| Flag | Does |
|---|---|
| `--once` | Print one frame to stdout and exit (no terminal needed). |

Other commands:

| Command | Does |
|---|---|
| `synt auth status` | Credential status per provider (variable names and where a key comes from, never values). |
| `synt rewind SESSION` | Put the files the agent changed with its file tools (Write, Edit, MultiEdit, NotebookEdit) back to their state at the checkpoint taken before its first such write to each. Moves, renames and deletes done through Bash are not tracked by checkpoints, so a rewind neither undoes nor lists them. A dry run unless `--apply`; a file changed since the run is refused unless `--force`; `--checkpoint NAME` limits it to one checkpoint. A snapshot is restored only if it still matches the hash stored with the checkpoint, and a file is removed only if the checkpoint recorded it as absent; checkpoints of older builds lack both and need `--force`. A session whose workspace root is a filesystem root or the home directory is refused unless `--allow-wide-root` (that also covers a directory that contains the home directory, such as `/Users` or `/home`, and a home directory that is not known: it fails closed). `--force` never overrides a creation proof or a snapshot hash that was recorded, only their absence in older checkpoints. `--apply` takes the per-session lock first and prints each line after its write; a failure prints `FAILED path: reason` and ends with a PARTIAL or FAILED line. Options: `--apply`, `--force`, `--checkpoint`, `--allow-wide-root`. A checkpoint is taken before the first write attempt, so a dry run can list a file whose write was refused. |
| `synt models [--json] [--refresh]` | List configured models (see below). |
| `synt doctor` | Diagnose the installation. |
| `synt browser done PROFILE` | Finish a `browser_login` wait from another terminal. |
| `synt import claude [--apply]` | Copy/convert a Claude Code setup into Synthesis's own files (dry run unless `--apply`). |
| `synt mcp ...` | Manage MCP server entries in your own config files (never credentials). |
