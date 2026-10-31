# Spec: Event stream (JSONL) — v1 reviewed

Status: reviewed 0.1 (R1). Normative words: MUST / SHOULD / MAY. Tags: `[from-node file:line]` = derived from the frozen Node reference (`~/Vibe/nemo-cli`), `[new]` = new design per the design doc §5.12 / §7.3.

## 1. Purpose
One typed, append-only event stream per session. It is the only interface between the core and every client (TUI, headless CLI, eval harness, future GPUI). It replaces Node's untyped `EventEmitter` + `'update'` repaint signal [from-node agent.js:450-4027]. The same records are persisted (session-store.md `events` table) and streamed on stdout in headless mode.

## 2. Envelope [new]
One JSON object per line, UTF-8, `\n` terminated, no pretty printing. Max line size 64 KiB (see §5).

| Field | Type | Rule |
|---|---|---|
| `v` | u16 | Schema major version. `1` for this spec. |
| `seq` | u64 | Starts at 1 per session, +1 per event, no gaps, never reused. A gap seen by a client means loss: re-read from the store with cursor = last seq. |
| `ts` | string | RFC 3339 UTC, millisecond precision, `Z` suffix. Producer wall clock; ordering is by `seq`, never by `ts`. |
| `session_id` | string | UUIDv4 lowercase [from-node agent.js:929 `crypto.randomUUID()`]. |
| `task_id` | string \| null | UUIDv4 of the active task; null before the first task and for session-level events between tasks. |
| `turn_id` | string \| null | UUIDv4 of the current model turn (one provider request/response cycle; Node counts these as `turns` [from-node agent.js:3360]). Null outside a turn. |
| `agent_id` | string \| null | Null for the main agent; sub-agent id for `Task` delegations [from-node agent.js:879]. |
| `type` | string | Dotted lowercase `area.event`. |
| `payload` | object | Type-specific; schema per §3. Always an object (possibly `{}`). |

```json
{"v":1,"seq":17,"ts":"2026-10-26T09:12:03.481Z","session_id":"6f1c…","task_id":"a2b9…","turn_id":"0d44…","agent_id":null,"type":"tool.completed","payload":{"tool_call_id":"toolu_01X","name":"Bash","status":"error","is_error":true,"duration_ms":2310,"preview":"FAIL src/login.test.js…[exit code 1]","bytes":48211,"truncated":true,"result_ref":{"artifact_id":"9c7e…","path":"artifacts/9c7e….txt","size":48211,"sha256":"…","mime":"text/plain"}}}
```

## 3. Taxonomy
Doc §5.12 types are kept verbatim; additions are marked. Payload field types: `str`, `int` (i64), `u64`, `bool`, `obj`, `[T]`, `?` = optional/nullable. `ArtifactRef` defined in §4.

| Type | Payload | Source |
|---|---|---|
| `session.started` | `workspace:{workspace_id:str, workspace_key:str? (additive: stable 12-hex id of the root, shared by all sessions of that workspace; absent in older logs), root:str, cwd:str, additional_dirs:[str], git_worktree:str?}, model:str, provider:str? (additive: id of the provider serving the model; absent in older logs), permission_mode:str, source:"startup"\|"resume"\|"clear", resumed_from_seq:u64?` | doc §5.12; `source` [from-node agent.js:1229 SessionStart] |
| `session.status_changed` | `from:SessionStatus, to:SessionStatus, reason:str?` | [new] (state machine, workspace-context.md §3) |
| `session.completed` / `session.failed` / `session.cancelled` | `reason:str, error:str?` — exactly one per session, last event of the stream | doc §5.12 |
| `task.started` | `request:str, acceptance_criteria:[{id:str,text:str,check:str?}], budget:Budget` | [new] |
| `task.ended` | `status:"completed"\|"unverified"\|"blocked"\|"failed"\|"cancelled", reason:str, verification_report_id:str?, usage:Usage, changed_files:[str], artifacts_root:str, needed_permissions:[str]?` | [new]; `needed_permissions` = rules the task needed but nobody could approve (headless); carries the §6 exit code mapping |
| `user.message` | `text:str, display_text:str?, images:[ArtifactRef]` | [from-node agent.js:1256 transcript `user` + `displayText`] |
| `turn.started` | `model:str, provider:str, iteration:int` | doc §5.12 |
| `model.delta` | `block_index:int, kind:"text"\|"thinking"\|"tool_input", text:str` | [from-node agent.js:3401-3418 text_delta/thinking_delta/input_json_delta] |
| `assistant.message` | `message_id:str?, content:[Block], stop_reason:str, usage:Usage?` (final folded turn; thinking `signature` stripped) | [from-node agent.js:3365] |
| `turn.completed` | `stop_reason:"end_turn"\|"tool_use"\|"max_tokens"\|str, usage:Usage?, duration_ms:u64` | [from-node agent.js:3436 message_delta] |
| `model.retry` | `attempt:int, max:int, error:str, delay_ms:u64` | [from-node agent.js:3184-3195, MAX_MODEL_RETRIES=3 agent.js:50] |
| `model.error` | `status:int?, message:str, retryable:bool, context_overflow:bool` | [from-node agent.js:225-237] |
| `context.compacted` | `trigger:"auto"\|"manual", summary_ref:ArtifactRef, tokens_before:u64?, reason:"auto"\|"overflow"\|"manual", before_tokens:u64, after_tokens:u64, window_tokens:u64, messages_before:u64, messages_after:u64, summary_chars:u64, duration_ms:u64` | [from-node agent.js:1953,3158 + compact_boundary agent.js:2018]; `trigger` is legacy (`overflow` reads `auto`); the fields from `reason` on are additive and default to ""/0 when reading older logs; `after_tokens` is an estimate |
| `context.usage` | `used_tokens:u64, window_tokens:u64, percent:f64 (used/window*100), source:"provider"\|"estimate", stale:bool` | [new] context size. Emitted after EVERY model response (`provider`: the response's input tokens, Anthropic including cache read/creation; `estimate`+`stale:true` when the provider reported none, the estimate then covers the request sent, not the reply) and right after every compaction (`estimate`, `stale:false`, the post-compaction size: consumers MUST replace any earlier number with it). `agent_id` set = sub-agent (not the main status). `window_tokens` is the model's context window, 200000 when unknown |
| `context.compacting` | `reason:"auto"\|"overflow"\|"manual", used_tokens:u64, window_tokens:u64` | [new] compaction started (after the PreCompact hook) |
| `context.compaction_failed` | `reason:"auto"\|"overflow"\|"manual", error:str` (redacted, <= 500 chars) | [new] history is unchanged; auto failures back off (pause of 2, 4, 8, 16 requests) |
| `tool.requested` | `tool_call_id:str, name:str, input:obj, input_truncated:bool, origin:"user"?` | doc §5.12; truncated input [from-node agent.js:3514] |
| `tool.approval_required` | `approval_id:str, tool_call_id:str, dialog:"permission"\|"plan"\|"question"\|"outside_read", prompt:str, options:[str], suggestions:[obj], deadline:str?` | doc §5.12; dialogs [from-node agent.js:3654-3696, 3793, 3828] |
| `tool.approval_resolved` | `approval_id:str, decision:"allow"\|"deny"\|"expired"\|"auto_denied", reviewer:"user"\|"policy"\|"hook"\|"classifier"\|"headless", input_changed:bool, message:str?` | [new]; expiry [from-node agent.js:3706] |
| `tool.started` | `tool_call_id:str, name:str, side_effect:"read"\|"write"\|"execute"\|"external", cwd:str` | doc §5.12 |
| `tool.completed` | `tool_call_id:str, name:str, status:ToolStatus, is_error:bool, not_run:bool, duration_ms:u64, preview:str, bytes:u64, truncated:bool, result_ref:ArtifactRef?, origin:\"user\"?, output:str?, output_truncated:bool?, output_bytes:u64?` | doc §5.12; statuses [from-node agent.js:3510-3651] |
| `tool.output` | `tool_call_id:str, stream:"stdout"\|"stderr"\|"merged", text:str, seq:u64, origin:"user"?` | [new] live output of a running `Bash` command (also `!cmd`): complete lines coalesced to >= 4 KiB or 50 ms, `seq` 1,2,.. per call, at most 256 KiB of text per call (then one cut marker; the full log is the artifact). Redacted per event, so an unfinished line is held back until its newline. Emitted between `tool.started` and `tool.completed` |
| `hook.ran` | `event:str, hook_name:str, exit_code:int?, outcome:"success"\|"non_blocking_error"\|"blocking_error", duration_ms:u64` | [from-node agent.js:1032-1041; never stdout/stderr] |
| `process.spawned` / `process.ready` / `process.exited` | `process_id:str, name:str?, pid:int?, exit_code:int?, signal:str?` | [new] TerminalRuntime |
| `artifact.created` | `ArtifactRef + kind:str, retention:str` | doc §5.12 |
| `verification.completed` | `report_id:str, status:VerificationStatus, attempt:int, summary:str` | doc §5.12; see verification.md |
| `todo.updated` | `todos:[{id:str?, content:str, status:"pending"\|"in_progress"\|"completed", active_form:str}]` | [from-node agent.js:3646, taskcreate.js] |
| `subagent.started` / `subagent.stopped` | `agent_id:str, agent_type:str, tool_call_id:str?, reason:str?, name:str?, background:bool?` | [from-node agent.js:3913 'subagent-start', 874 'subagent-evicted', 3017 SubagentStop] |
| `subagent.status_changed` | `agent_id:str, name:str, from:str, to:str, reason:str?` | [new] `running\|waiting_approval\|idle\|cancelled\|evicted` (messaging.md A2); `agent_id` in the scope too |
| `message.sent` | `message_id:str, from:str, to:str, origin:"user"\|"agent"\|"system", kind:"message"\|"result"\|"notice", summary:str?, text:str (redacted, <= 4 KiB), bytes:u64, notify_when_idle:bool?` | [new] a message entered an inbox; `from`/`to` are instance names (`main`, `user`, `system`). Messaging.md calls the sub-agent name `instance`; the event field is `name` |
| `message.delivered` | `message_id:str, at:"tool_round"\|"turn_end"\|"wake"` | [new] the text reached the recipient's history; scope `agent_id` = recipient |
| `message.dropped` | `message_id:str, reason:str` | [new] recipient cancelled or the task ended before delivery; an agent sender gets a notice, a user line returns to the prompt |
| `peer.listening` / `peer.stopped` | `endpoint_kind:"unix"\|"pipe"` | [new] the session's peer endpoint (messaging.md B4); never the path or pipe name |
| `peer.request_sent` | `request_id:str, from:PeerAddr, to:PeerAddr, summary:str, bytes:u64, chain_len:u64` | [new] this session asked a peer. `PeerAddr` = `workspace_key, workspace_label, workspace_root?, session_id, session_name?, agent_id?, instance, kind` |
| `peer.request_received` | `request_id:str, from:PeerAddr, to:PeerAddr, summary:str, bytes:u64, decision:"accepted"\|"rejected", reason:str?` | [new] a peer asked this session (`from` is self-asserted); `reason` = `disabled\|denied\|busy\|rate_limited\|cycle\|too_large\|no_agent` |
| `peer.reply_partial` | `request_id:str, seq:u64, text:str` | [new] a partial reply received (redacted by the sender's session) |
| `peer.reply` | `request_id:str, status:"answered"\|"declined"\|"failed"\|"timeout"\|"cancelled"\|"offline", text:str (<= 4 KiB preview), bytes:u64, duration_ms:u64` | [new] emitted in both sessions with the same `request_id`; no ArtifactRef yet |
| `session.mode_changed` | `from:str, to:str, reason:str?` | [new] permission mode switched while the session runs (TUI Shift+Tab, a future GUI via `Harness::set_mode`); not emitted for the starting mode. Entering `bypassPermissions` also emits a `notice` |
| `permission.auto_verdict` | `tool_call_id:str, name:str, verdict:"allow"\|"deny"\|"ask", reason:str, source:"model"\|"cache"\|"fail_closed"` | [new] auto mode: the judge's decision on one call before it runs (audit trail; `fail_closed` = judge unavailable, unparseable, timed out or over the per-task cap, so the call is an ask) |
| `model.changed` | `model:str, effort:str?` | [from-node agent.js:1539 'model-changed'] |
| `budget.exceeded` | `budget:str, limit:number, used:number` | [new] |
| `memory.retrieved` | `ids:[str], tokens:u64, candidates:u64` | [new] the notes injected at task start (ids only, never text); memory.md §11 |
| `memory.written` | `id:str, op:"added"\|"deduped"\|"superseded"\|"proposed"\|"quarantined"\|"updated"\|"archived", kind:str, scope:str, trust:str, status:str` | [new] a record was written by the episode writer, a `Memory*` tool or a reflection |
| `memory.quarantined` | `id:str, rule:str` | [new] the poisoning scanner parked a record (also sent as `memory.written` with `op:"quarantined"`) |
| `memory.reflection.started` / `memory.reflection.failed` | `job_id:str` / `job_id:str, error:str` | [new] one reflection job (a capped model call) began / failed (`error` redacted) |
| `memory.reflection.completed` | `job_id:str, ops:u64, tokens:u64?, cost_usd:f64?` | [new] `ops` = records saved or queued for review; tokens and cost are absent while the summarizer reports no usage |
| `memory.proposal.created` | `id:str` | [new] a skill or instruction-file proposal was written (nothing is applied without the person) |
| `notice` | `level:"info"\|"warn"\|"error", text:str` | [from-node agent.js:919-925 note()/toast()] |

`origin:"user"` marks a command the person typed (`!cmd`); absent = the model's call. `tool.completed.output` is the model-facing result text, whole up to 64 KiB (redacted); when the result is longer (or the tool cut it) `output` holds head and tail, `output_truncated:true`, `output_bytes` the whole size and `result_ref` the artifact with the whole (the command's own log for Bash). Lines of `tool.completed`/`tool.output` may exceed the 64 KiB line guideline. All additions are optional fields/new types (v1 unchanged).

`ToolStatus` = `"ok"|"error"|"soft_error"|"denied"|"interrupted"|"timeout"|"budget_exceeded"` [from-node: Node item statuses done/error/interrupted + `soft` flag, soft.js:8-21]. `Usage` = `{input_tokens:u64, output_tokens:u64, cache_read_input_tokens:u64?, cache_creation_input_tokens:u64?, cost_usd:f64?}` [from-node providers/openai.js:78 mapUsage, Anthropic usage passthrough]. `Block` = Anthropic content block shape (`text`, `thinking`, `tool_use`, `tool_result`, `image` with `source` replaced by an `ArtifactRef`).

Node emitter names NOT carried into the stream (they are UI commands handled by the interface layer, not runtime facts) [from-node]: `update`, `view-main`, `view-subagent`, `picker-request`, `attach-request`, `resume-request`, `new-session-request`, `close-request`, `exit-request`, `fork-request`. Mapping of the rest: `initialized`+`session` → `session.started`; `permission` → `tool.approval_required`; `toast` → `notice`.

`tool.completed.diff` [new, additive, optional; absent for other tools and in older logs]: the structured line diff of a file change, copied from the tool's `meta.diff` (shape in tools.md §4a): `{path, kind:"update"|"create"|"overwrite"|"delete", added:u64, removed:u64, hunks:[{old_start, old_lines, new_start, new_lines, lines:[{tag:" "|"+"|"-", old_no?, new_no?, text}]}], truncated:bool, hidden?:bool}`. Line text is capped (2000 diff lines / 64 KiB; `added`/`removed` stay exact, `truncated:true`) and redacted like every payload. Readers without it fall back to a diff built from `tool.requested.input`, numbered from 1.

Ordering invariants: `tool.requested` < (`tool.approval_*`)* < `tool.started`? < `tool.completed`, all with the same `tool_call_id`; a denied call has no `tool.started` and `not_run:true`. Every `tool.requested` gets exactly one `tool.completed` (Node rule: every tool_use needs a tool_result [from-node agent.js:3455]). `task.ended` precedes `session.*` terminal event.

## 4. Artifact references [new]
`ArtifactRef = {artifact_id:str(UUIDv4), path:str, size:u64, sha256:str(hex), mime:str}`; `path` is relative to the session artifact root (`$SYNTHESIS_HOME/sessions/<session_id>/`, doc §5.7). Events never inline binary data. Clients read content via `artifact.read` or the file path.

## 5. Redaction and size rules [new unless tagged]
Applied by the producer before persistence AND before streaming; an unredacted copy is never written anywhere.
1. Never emitted: credential values; env values whose name matches `^SYNTHESIS_(?:[A-Z0-9]+_)*(?:API_KEY|AUTH_TOKEN|TOKEN)$` [from-node auth.js:46]; hook stdout/stderr [from-node agent.js:1030]; HTTP `Authorization`/`x-api-key` headers; thinking `signature`.
2. String values anywhere in `payload` matching built-in patterns are replaced by `"[REDACTED:<kind>]"`: `sk-ant-…`, `sk-…` (≥20 chars), `ghp_/gho_/github_pat_…`, `AKIA[0-9A-Z]{16}`, `-----BEGIN [A-Z ]*PRIVATE KEY-----…`, `Bearer <token>`. Browser `fill` values marked `sensitive` are always redacted.
3. Any string > 8 KiB becomes a preview (head 60 % / tail 40 %, marker `… [N chars truncated] …`, same algorithm as [from-node bash.js:18-26]) plus an `ArtifactRef`; images always become artifacts.
4. Redaction is lossy by design; tool results seen by the model are NOT redacted (the model must see real output), only their persisted/streamed copies.

## 6. Headless CLI contract [new]
`synth run --headless --cwd <dir> [--model <provider/model>] [--mode <permission-mode>] [--criteria <file.json>] [--max-turns N] [--max-tool-calls N] [--max-wall-ms N] (--prompt <text> | --prompt-file <f> | stdin)`
- `--cwd` is REQUIRED (no implicit cwd, workspace-context.md §4).
- stdout: events only, one per line. stderr: human diagnostics, unstructured.
- No interactive approver: every `ask` resolves `auto_denied` (`reviewer:"policy:non-interactive"` in `synth run`, which emits `tool.approval_required` + `tool.approval_resolved` for the audit trail; was `"headless"`) and the denial text goes back to the model as a tool error.

| Exit | Meaning (`task.ended.status`) |
|---|---|
| 0 | `completed` (verification passed, or not applicable — verification.md §4) |
| 1 | `failed` |
| 2 | usage or config error, no session started (Node uses 1 for this [from-node bin/synth.js:269-685]; deliberately changed) |
| 3 | `unverified` |
| 4 | `blocked` |
| 70 | internal error (crash after the session started; `session.failed` emitted if possible) |
| 130 | `cancelled` by SIGINT/Ctrl-C [from-node bin/synth.js:116] |
| 141 | stdout was closed by the reader (EPIPE, e.g. `synth run ... \| head`): the task is cancelled cleanly, every event including the final ones is still persisted in the session store; `synth doctor/models/auth` just exit 0 quietly [new] |

## 7. Versioning and compatibility [new]
- Additive changes (new `type`, new optional payload field, new enum value in a field documented as open) keep `v`. Consumers MUST ignore unknown types and unknown fields, and MUST treat unknown enum values as "other".
- Removing/renaming a field, changing a type or meaning, or making a field required bumps `v`. A producer emits one `v` per session; a store may hold sessions of different `v`.
- The JSON Schema for each payload lives in `crates/synthesis-events/schema/v1/*.json` (to be generated from the Rust types); golden stream fixtures in `tests/golden/events/` pin it.

## Open
- Whether `model.delta` for `thinking` is streamed at all to non-TUI clients (doc §5.4 "never leak hidden internals").
- Live output streaming for long `Bash`/`terminal.exec` (no `tool.output` event in v1; use `terminal.logs`).
