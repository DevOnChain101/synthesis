# Spec: Tool registry, schemas and execution pipeline — reviewed 0.1

Tags as in events.md. Exact `description` strings are not copied here; they are captured verbatim into `tests/golden/tools/schemas.<os>.json` (eval-and-golden.md §B), because Node varies them per OS (Bash text differs on win32 [from-node tools/bash.js:131]).

## 1. Registry contract

```rust
pub enum SideEffectClass { Read, Write, Execute, External }      // doc §7.1
pub enum ToolRuntime { Builtin, Harness, Mcp { server: String } } // Harness = run by the agent loop itself [from-node tools/index.js:37-40]
pub struct ToolSpec {
    pub name: String,            // wire name sent to providers, ^[A-Za-z0-9_-]{1,64}$ [from-node mcp.js:110]
    pub logical_id: String,      // doc §5.6 id, e.g. "terminal.spawn"
    pub input_schema: serde_json::Value, // JSON Schema object, exactly as sent to providers
    pub side_effect: SideEffectClass,
    pub rule_name: String,       // name used in permission rules, normally == name
    pub runtime: ToolRuntime,
}
pub struct ToolResult {          // [from-node tools/index.js:3-6]
    pub content: Content,        // String | Vec<Block{text|image}>
    pub is_error: bool,
    pub soft: bool,              // refusal/not-applicable, drawn neutral [from-node tools/soft.js:3-7]
    pub meta: serde_json::Value, // e.g. {"replacements":2} [from-node edit.js:124]
    pub artifacts: Vec<ArtifactRef>,
}
```
Rules:
1. Expected failures are returned as `is_error: true`, never as a Rust `Err`; only cancellation propagates [from-node tools/index.js:3-6].
2. Naming [new]: tools ported from Node keep their names (`Read`, `Bash`, …): the transcript parser keys on them [from-node parser.js:225, history.js:108] and two-way compatibility needs them. Doc §5.6 ids map to these instead of being added again: `file.read`=`Read`, `file.write`=`Write`, `file.patch`=`Edit`, `file.search`=`Grep`+`Glob`, `terminal.exec`=`Bash`. New tools use wire name = logical id with `.` → `_` (`terminal_spawn`), because dots are invalid in Anthropic/OpenAI tool names.
3. MCP tools: `mcp__<server>__<tool>`, sanitized, truncated to 64 chars with a sha1-6 suffix [from-node mcp.js:115-140]; two tools that sanitize to the same name get the hash suffix on the later one, so names stay unique and reversible [new]. Default class `External`. See §6.
4. Registry order is stable (it feeds prompt caching): the 17 built-ins in Node order [from-node tools/index.js:26], then Task/AskUserQuestion/ExitPlanMode, then new tools, then MCP.

## 2. Side-effect classes

| Class | Tools |
|---|---|
| Read | Read, Glob, Grep, BashOutput, TodoWrite, TaskCreate/Get/List/Update, Skill, Task¹, AskUserQuestion, ExitPlanMode, terminal_logs, terminal_status, git_status, git_diff, artifact_read, browser_snapshot, browser_screenshot, browser_console, browser_network |
| Write | Write, Edit, NotebookEdit, git_apply_patch |
| Execute | Bash, KillShell, terminal_spawn, terminal_signal, verify_run, git_revert |
| External | WebFetch, WebSearch, browser_navigate, browser_click, browser_fill, mcp__* |

¹ Task runs a sub-agent whose own calls are gated one by one. The class drives defaults; permission rules still decide (Node's READ_ONLY_TOOLS/EDIT_TOOLS sets [from-node permissions.js:11-12] are ported verbatim for the `Permissions::check` port).

## 3. Execution pipeline (per call, calls in one batch run sequentially [from-node agent.js:3489-3508])
0. **Parse** — tool input cut off or not valid JSON → error `Tool input was cut off …` without running [from-node agent.js:3514-3522]. Unknown name → `Unknown tool "<name>"` [from-node agent.js:3528-3535].
1. **Validate** [new] — JSON Schema validation of `input` against `input_schema` (Node only checks by hand in each tool). Failure → `is_error`, `soft:false`, message lists the first 5 violations.
2. **Policy** — `Permissions::check(name, input, turn_mode)` ported verbatim [from-node permissions.js:719-829], order: deny rule → dialog tools → critical `rm` → outside-read (blocked/network) → ask rule → bypassPermissions (an ask rule still prompts there) → plan → protected paths → allow rules → mode defaults. `classify` → auto-mode classifier [from-node agent.js:3723]. `ask` → `tool.approval_required`; headless resolves it `auto_denied`. Then **PreToolUse hooks** (may block or rewrite input) [from-node agent.js:3585-3603].
   - Change [new]: if a hook or the approver returns `updatedInput`, re-run `check` on the new input; `deny` → not run, `ask` → ask again. Node skips this re-check, so a hook can change the input after permission was granted.
   - **Headless ask handling by mode [new, owner decision 2026-10-28]**: nobody is prompted (no stdin). `synt run --permission-mode` accepts all six modes of `synthesis_permissions::Mode`. An `allow` rule from `--allow`/settings always applies first; then, for what is still an `ask`:

     | mode | edits in the workspace | other asks (Bash, MCP, outside paths) |
     |---|---|---|
     | `default` | ask -> auto-denied | auto-denied |
     | `acceptEdits` | allowed | auto-denied |
     | `plan` | denied (read-only) | denied (a read-only shell command may be allowed) |
     | `dontAsk` | an ask becomes a deny (policy) | same |
     | `auto` | the judge model decides (below) | same; an ask or unclear answer is auto-denied |
     | `bypassPermissions` | allowed | allowed, except hard stops |

     `bypassPermissions` is typed on the CLI (trusted), never read from project settings, `.mcp.json` or the environment, and prints a loud notice + `notice` event at start. Not relaxed: deny rules, ask rules (they still prompt; headless denies them), critical `rm` (`rm -rf ~`, `/`, the cwd), destructive MCP asks, outside-read blocks. Plain writes to protected paths (`.env`, `.git/`, `.synt`, `.ssh` ...) ARE allowed under bypass, exactly like Claude Code (owner decision D9, 2026-10-28); the sandbox (workspace-write, credential dirs denied) is what still confines them.
   - **Blocked, not completed [new]**: when an ask is auto-denied for any non-read tool (Write, Edit, NotebookEdit, Bash, terminal_spawn, verify_run, MCP write/external tools), the task cannot end `completed`: it ends `blocked` / `approval_unavailable` with `needed_permissions` (unless a later write/execute call succeeded). A denied pure read stays evidence the model may work around. A `dontAsk`/`plan`/deny-rule denial is a policy decision, not a missing approval, and does not set this.
   - **Auto mode judge [new]**: `Decision::Classify` in `auto` mode calls the session provider (model: `autoMode.model` of the user settings, else the session model) with the `AutoModeRules` prompt and a data-only user prompt (task, earlier user messages, workspace, the pending call; everything in untrusted tags, closing tags defused; tool output is never sent). The answer must be exactly one JSON object `{"decision":"allow|block|ask",...}`; anything else, a provider error, a timeout (30 s) or the per-task cap (50 calls; identical calls are cached) is an `ask`. Protected-path classifications never reach the judge, and `forced` asks (destructive MCP) are applied after it, so it can only decide what the rules left open. Each decision emits `permission.auto_verdict`; judge tokens/cost count in the task usage. In `plan` mode nothing is judged.
   - Change [new]: Node lets a read outside the working dirs through when no prompt can be shown [from-node agent.js:3543]; the Rust core denies it in headless.
3. **Workspace bind** — build `ToolContext`; resolve every path field (`file_path`, `path`, `notebook_path`, `cwd`) via workspace-context.md §4.2.
4. **Budget** — check `max_tool_calls`, `max_wall_ms`, per-call timeout; exceeded → `tool.completed{status:"budget_exceeded", not_run:true}` and the task stops.
5. **Execute** — Write-class tools first snapshot the target for /rewind [from-node agent.js:3607]. Cancellation → `Interrupted.` [from-node agent.js:3610-3615].
6. **Normalize** — model-visible text capped at 30 000 chars, head 60 % + tail 40 % [from-node tools/bash.js:8-26, agent.js:255-259]; the full text goes to an artifact [new]. PostToolUse hooks run; their `additionalContext` is appended [from-node agent.js:3625-3644]. `soft` comes from the tool or from the soft-pattern list [from-node tools/soft.js:8-21].
7. **Persist** — `tool.completed` event, `tool_calls` row, artifacts (session-store.md).

## 4. Ported tool schemas [from-node, verbatim field names; `type:"object"` omitted]
`number` stays `number` (Node never declares `integer`).

| Tool | properties (required in **bold**) | Source |
|---|---|---|
| Read | **file_path**:string, offset:number (1-based), limit:number (default 2000; a whole-file read of a text file over 256 KB is refused and a result over ~25k tokens (chars/4) is refused, both with offset/limit guidance in the error; line numbering is unchanged, owner decision D4), pages:string ("3", "2-5", PDF only) | tools/read.js:35-44 |
| Write | **file_path**:string, **content**:string | tools/write.js:8-21 |
| Edit | **file_path**:string, **old_string**:string, **new_string**:string, replace_all:boolean | tools/edit.js:53-62 |
| NotebookEdit | **notebook_path**:string, cell_id:string, new_source:string, cell_type:enum[code,markdown], edit_mode:enum[replace,insert,delete] | tools/notebook.js:53-62 |
| Glob | **pattern**:string, path:string | tools/glob.js:18-24 |
| Grep | **pattern**:string, path, glob, type:string, output_mode:enum[files_with_matches,content,count], "-i","-n":boolean, "-A","-B","-C":number, multiline:boolean, head_limit:number | tools/grep.js:253-270 |
| Bash | **command**:string, timeout:number (ms, default 120000, max 600000; at the timeout `synt run` kills the command, owner decision D6), description:string, run_in_background:boolean | tools/bash.js:139-148 |
| BashOutput | **bash_id**:string, filter:string (regex) | tools/bash.js:259-266 |
| KillShell | **shell_id**:string | tools/bash.js:277-281 |
| WebFetch | **url**:string, **prompt**:string | tools/webfetch.js:40-47 |
| WebSearch | **query**:string, allowed_domains:string[], blocked_domains:string[] | tools/websearch.js:77-85 |
| Skill | **skill**:string, args:string | tools/skill.js:11-18 |
| TodoWrite | **todos**:[{**content**:string, **status**:enum[pending,in_progress,completed], **activeForm**:string}] | tools/todowrite.js:14-31 |
| TaskCreate | **subject**:string, description:string, activeForm:string, metadata:object [new: metadata] | tools/taskcreate.js:23-31 |
| TaskGet | **taskId**:string | tools/taskget.js:6-10 |
| TaskList | (none) | tools/tasklist.js:6 |
| TaskUpdate | **taskId**:string, status:enum[pending,in_progress,completed,deleted], subject, activeForm, description, owner:string, addBlocks:string[], addBlockedBy:string[], metadata:object [new: deleted, owner, addBlocks, addBlockedBy, metadata] | tools/taskupdate.js:9-18 |
| Task | **description**:string, **prompt**:string, **subagent_type**:string, model, profile, effort:string | tools/index.js:41-59 |
| AskUserQuestion | **questions**:[{**question**:string, header:string, multiSelect:boolean, **options**:[{**label**:string, description:string}]}] | tools/index.js:60-94 |
| ExitPlanMode | **plan**:string (markdown) | tools/index.js:95-107 |

Task* results [new, conformance 81; shapes from the Agent SDK reference, code.claude.com/docs/en/agent-sdk/typescript "Tool Output Types"]: `TaskCreate` -> `{task:{id,subject}}`; `TaskGet` -> `{task:{id,subject,description,status,blocks,blockedBy,activeForm,owner?,metadata?}}` or `{task:null}` for an unknown id; `TaskList` -> `{tasks:[{id,subject,status,blockedBy,owner?}]}`; `TaskUpdate` -> `{success,taskId,updatedFields,statusChange?:{from,to}}`, or `is_error` with `{success:false,error}` for an unknown id or edge target (nothing is changed). `status:"deleted"` removes the task and its edges; `addBlocks`/`addBlockedBy` set both sides of an edge; a `null` metadata value removes the key. `id`/`task_id`/`active_form` are repaired to `taskId`/`activeForm` before validation. `description` stays optional on TaskCreate (the SDK type requires it). All five tools share the session list, are Read class, and emit the unchanged `todo.updated` event. Schema additions are allow-listed in `tests/golden-divergences.json` (`schema_extra_properties`, `schema_enum_extra`).

Behaviour that is part of the contract [from-node]: Read output is `cat -n` style `%6d\t<line>`, lines over 2000 chars truncated [read.js:9-114]; Read records `path → mtime` and Edit/Write refuse unless the file was read and has not changed since [edit.js:86-89, write.js:41-44]; Edit keeps CRLF files CRLF [edit.js:91-105]; Bash merges stdout+stderr, appends `[exit code N]`, `is_error` on non-zero or timeout [bash.js:225-231], strips secret env [bash.js:160-163], kills the whole process tree on timeout/abort [bash.js:204-242].

Change [new]: `Bash` with `run_in_background` and `BashOutput`/`KillShell` are served by `TerminalRuntime` (`spawn`/`read_logs`/`signal`); `bash_id` == `process_id`. Node's 1 MiB in-memory tail [bash.js:34] becomes a log artifact with a cursor.

### 4a. `meta.diff` of Edit / Write / NotebookEdit [new, additive]

The model-facing text and schemas are unchanged. Where the whole file is known the tool also returns `ToolOutput.meta.diff` so the UI can show real line numbers; the harness copies it to `tool.completed.diff` (events.md).

`{path, kind, added, removed, hunks, truncated, hidden?}`: `path` as the call gave it; `kind` is `update` (Edit, NotebookEdit replace/insert), `create` (Write of a new file), `overwrite` (Write over an existing UTF-8 file; no diff when the old file is not UTF-8 or over the read limit) or `delete` (NotebookEdit cell delete). `hunks[]` are unified-diff hunks with 3 lines of context: `old_start/old_lines/new_start/new_lines` (1-based; an empty side names the line before it) and `lines[]` of `{tag:" "|"+"|"-", old_no?, new_no?, text}` (`text` without the line terminator, CRLF stripped; tabs kept). `added`/`removed` always count every changed line; line text stops at 2000 diff lines or 64 KiB (`truncated:true`). For NotebookEdit the numbers are relative to the cell source. Files matching `synthesis_core::secrets::is_secret_path` get `hidden:true`, the counts and no hunks. Edit diffs are taken on LF-normalised text, so a CRLF file diffs like an LF one.

## 5. New tools [new, drafts; schemas in JSON Schema]
```json
{"terminal_spawn":{"properties":{"command":{"type":"string"},"name":{"type":"string"},"cwd":{"type":"string"},"env":{"type":"object","additionalProperties":{"type":"string"}},"tty":{"type":"boolean"},
  "ready":{"type":"object","properties":{"kind":{"enum":["port","http","log"]},"port":{"type":"integer"},"url":{"type":"string"},"pattern":{"type":"string"},"timeout_ms":{"type":"integer"}},"required":["kind"]}},"required":["command"]},
 "terminal_logs":{"properties":{"process_id":{"type":"string"},"cursor":{"type":"string"},"filter":{"type":"string"},"max_bytes":{"type":"integer"}},"required":["process_id"]},
 "terminal_signal":{"properties":{"process_id":{"type":"string"},"signal":{"enum":["int","term","kill"]}},"required":["process_id","signal"]},
 "terminal_status":{"properties":{"process_id":{"type":"string"}}},
 "git_status":{"properties":{}},
 "git_diff":{"properties":{"path":{"type":"string"},"staged":{"type":"boolean"},"base":{"type":"string"},"context":{"type":"integer"}}},
 "git_apply_patch":{"properties":{"patch":{"type":"string"},"check_only":{"type":"boolean"}},"required":["patch"]},
 "git_revert":{"properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"]},
 "verify_run":{"properties":{"check_ids":{"type":"array","items":{"type":"string"}},"command":{"type":"string"},"criteria_ids":{"type":"array","items":{"type":"string"}},"timeout_ms":{"type":"integer"}}},
 "artifact_read":{"properties":{"artifact_id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"filter":{"type":"string"}},"required":["artifact_id"]},
 "browser_navigate":{"properties":{"url":{"type":"string"},"wait_until":{"enum":["load","domcontentloaded","networkidle"]},"timeout_ms":{"type":"integer"}},"required":["url"]},
 "browser_snapshot":{"properties":{"ref":{"type":"string"},"max_nodes":{"type":"integer"}}},
 "browser_click":{"properties":{"ref":{"type":"string"},"role":{"type":"string"},"name":{"type":"string"},"selector":{"type":"string"}}},
 "browser_fill":{"properties":{"ref":{"type":"string"},"selector":{"type":"string"},"value":{"type":"string"},"sensitive":{"type":"boolean"}},"required":["value"]},
 "browser_screenshot":{"properties":{"full_page":{"type":"boolean"},"ref":{"type":"string"}}},
 "browser_console":{"properties":{"cursor":{"type":"string"},"level":{"enum":["error","warning","info","debug"]}}},
 "browser_network":{"properties":{"cursor":{"type":"string"},"failed_only":{"type":"boolean"}}}}
```
Semantics: `terminal_spawn` returns `{process_id, pid, status, ready:bool, log_cursor}` and outlives the tool call but not the session (doc §5.8). `terminal_status` without id lists the session's processes only. `git_revert` restores paths to the session baseline (agent edits only), never runs `git reset`/`checkout` on user changes, and is always `ask` (doc §5.10). `browser_*` use accessibility refs first, selectors as fallback, no coordinates (doc §5.9); backend decided by the R6 spike. Permission rule syntax for new tools follows Node: `terminal_spawn(<cmd prefix>:*)` like Bash, `browser_navigate(domain:<host>)` like WebFetch [from-node permissions.js:442-507].

### 5a. Agent tools [new, R10a; harness-run, `with_new_tools`]
- `Task` gains `name?` (`[a-z0-9_-]{1,32}`, a taken name gets `-2`, `-3`) and `run_in_background?`. Background returns `{agent_id, name, kind, status:"running"}` and the final text arrives later as an `<agent_message type="result">`. The main task does not finish while background agents run (one gate, one budget). The schema replaces the ported one only in `with_new_tools`, so the Node golden is untouched.
- `SendMessage {to, message, summary?, notify_when_idle?}`: `to` = `name`, `agent:<name>`, `main`, `parent` or an agent-id prefix (>= 6 hex); an unknown or ambiguous address fails with the candidate list. Errors: `not_found`, `ambiguous`, `too_large` (32 KiB), `inbox_full` (32 queued). Returns `{status:"queued"|"resumed", to, message_id}`; an idle sub-agent is started again with mode `stricter(its own, sender's)`. Counts as a tool call (loop protection). Policy: not read-only. `default` asks, `acceptEdits`/`auto`/`bypassPermissions` allow, `plan`/`dontAsk` deny; deny/ask rules and PreToolUse hooks apply. A sender without Bash/Write/Edit tools wakes a recipient in `default` mode at most. Names `user`, `system`, `parent`, `main` are reserved. A message is data (`trust="peer-data"`): it never answers an approval or changes a mode.
- `ListAgents {scope?}`: rows `{name, address, kind, status, queued, you}` for this session, `main` first. `scope:"all"` (peers on) returns the cross-session rows of messaging.md B9 instead.
- `ListSessions {}`: only offered when `synt.peers.enabled`; B9 rows (main agent of this session first, then each live session the allow list permits). Read-only.
- `SendMessage` to a peer: `to` = `ws:<label|key>/sess:<name|id>[/agent:<instance>]` (messaging.md section 1). Returns `{request_id, to:<canonical>, status:"accepted"}`; the answer arrives later as a `<peer_reply>` block. Errors: `disabled`, `denied`, `not_found`, `ambiguous` (with candidates), `offline`, `unreachable`, `busy` (4 in flight here, or the receiver's limit), `rate_limited`, `cycle`, `too_large`, `no_agent`. Allowed without a prompt when the target is on the allow list and `initiate` is true; otherwise asks (headless: denied).
- Delivery is at safe points only: before the next model request (after the round's tool results, appended as a text block to the same user message), at turn end before the Stop hook and the gate, and when an idle agent wakes. Never mid-stream, mid-approval or between a `tool_use` and its result.
- Session-qualified addresses (`ws:`, `sess:`) are reserved for R10b and are rejected today.

## 6. MCP [new; user guide: docs/MCP.md]
Spec revision 2025-06-18. Crate `synthesis-mcp` (client manager), `apps/cli/src/mcp.rs` (adapter, config, `synt mcp`), `synthesis-harness/src/mcp_guard.rs` (read-before-write guard).

- **Transports**: stdio (own process group / Job Object with kill-on-close), streamable HTTP (rmcp; `Mcp-Session-Id` 404 => transparent re-initialize), legacy HTTP+SSE (own thin client, `sse.rs`: https or loopback only, no private/metadata addresses, redirects revalidated, the `endpoint` event must stay on the stream's origin). A bare `url` tries streamable HTTP, then SSE. OAuth is out of scope (ADR-011): use `npx mcp-remote <url>` as a stdio server or a Bearer header from `${VAR}`.
- **Capabilities advertised**: none (no sampling, roots, elicitation); such requests are declined by rmcp's defaults. `ping` is answered.
- **Live registry**: `ToolBox::with_source` re-reads the MCP tools every turn; `tools/list_changed`, reconnects and losses change what the model sees (the old list is kept if a refresh fails). Pagination is followed.
- **Tool metadata kept**: `annotations` (hints only: they never relax a rule), `outputSchema` verbatim, `structuredContent` verbatim in `McpToolResult.structured` and in the tool output `meta`, the text form is what the model reads.
- **Results**: text capped at 28 000 chars; the MIDDLE is cut with an explicit `[… N chars omitted …]` marker (JSON keeps every key and short value such as `id`/`version`, only its longest strings are shortened); the full text is saved under `<session artifacts>/mcp/` and named in the result. Images pass through; audio, resource links and unknown blocks become one-line placeholders.
- **Resources and prompts**: servers that declare them get two synthetic read-only tools, `mcp__<server>__resource_read {uri}` and `mcp__<server>__prompt_get {name, arguments}`, whose description lists what the server offers. A real tool of that name wins. The user can also reach them without the model: `@server:protocol://path` in the prompt appends the resource as an untrusted-data block, and `/mcp__<server>__<prompt> args` (or `--prompt-command`) runs a prompt; `synt mcp prompts` lists them (`crates/synthesis-app/src/mcpinput.rs`).
- **Tool search** (`synthesis-harness/src/toolsearch.rs`; Claude Code's "Scale with MCP tool search"): `synt.mcp.toolSearch` (USER settings only) = `auto` (default) | `on` | `off`. When deferral is on (`on`, or `auto` and the MCP definitions, chars/4 of name + description + schema, are 10% or more of the context window; once on it stays on for the task) the request lists those tools by name in the system prompt and sends no schema, and a harness-run `ToolSearch {query, max_results?}` is offered. `query` = `select:name1,name2` (exact `mcp__server__tool` names) or keywords matched against name and description (best matches first, default 5, max 20). The result is the loaded definitions as JSON inside the untrusted-data fence (descriptions are server text); those tools are sent with their schema from the next request and are callable for the rest of the task. A call to a deferred tool that is not loaded is refused before schema validation and policy with a hint to call `ToolSearch`. Permissions are unchanged: a loaded tool is checked under its full name; a bare deny rule (`mcp__server`, `ToolSearch`) hides the tool or the search (without `ToolSearch` nothing is deferred); `ToolSearch` itself is a read tool that is not policy-checked, like the other harness tools. A sub-agent whose `tools` list lacks `ToolSearch` gets every schema. No new event: `tool.requested`/`tool.completed` cover `ToolSearch`. Not done: `auto:N` percentages, per-server `alwaysLoad`, naming failed servers in empty results.
- **Server `instructions`** are appended to the first tool's description inside a label that marks them as untrusted data.
- **Timeouts**: per call 60 s, connect 30 s (`tool_timeout_sec|tool_timeout_ms|timeout`, `startup_timeout_sec|startup_timeout_ms` per server). Cancelling a call sends `notifications/cancelled`. A lost connection is re-established with a bounded backoff (1 s, 4 s, 15 s); the failed call is never retried (it may have been a write).
- **Filters**: `enabled_tools` / `disabled_tools` per server (disabled wins; also enforced at call time).
- **Read-before-write guard** (`McpGuard`, runs in the pipeline before the policy check, state per task):
  - *Write tool* = listed in `synt.mcp.servers.<server>.write_tools`, or (unless listed in `read_tools` / annotated `readOnlyHint: true`) `destructiveHint: true`, or a name (first 80 chars of the description if the name says nothing) with a write verb: update, edit, put, patch, replace, write, set, modify, delete, transition, comment, remove, restore, merge, move, archive. A name that starts with get/read/list/search/find/fetch/view/query/describe/discover/diff/show/lookup/count/check/retrieve is read. `executeRead|Write|Destructive`-style wrappers are judged by the inner `tool` and its `arguments`. `read_tools` can only come from the user's settings; a project settings file can only add `write_tools`.
  - *Freshness proof* for each id-like argument of a write (`id`, `key`, `uri`, `issueIdOrKey`, `pageId`, `*_id`, `*Id`): an earlier successful read of the same server in this task whose arguments or result mention the id, and no successful write to that id since. Otherwise the call is NOT sent: a normal tool error says to fetch with the suggested read tool, MERGE, retry.
  - *Version provenance*: a `version|etag|revision|ifMatch|updated` field of the write must equal a value in the fetched result (numbers may be fetched+1).
  - *Approval*: a tool classed destructive always asks, in every mode and despite allow rules; an update whose `body|content|description|text|...` drops more than 30 % of the fetched text or any heading of it asks with "this update will replace N lines; M lines of the current content are not present in your update" and the changed lines. Headless runs deny these asks.
  - Results only feed the guard's string comparisons; they never reach the permission engine.
- **CLI**: `synt mcp list|get|add|remove` edit `<SYNT_HOME>/synt.json` (user) or `<cwd>/.mcp.json` (project, still needs approval); secret-looking env/header values must be `${VAR}` references.

## Open
- `TodoWrite` vs `TaskCreate/Update` share one store in Node [from-node taskcreate.js:3-7]; both stay on the wire (decided 2026-11-01, conformance 81: Claude Code offers the four Task tools by default and `TodoWrite` only with `CLAUDE_CODE_ENABLE_TASKS=0`).
- Should `verify_run` be callable by the model, or only by the harness at the completion gate?
