# Spec: Session store and transcript compatibility — reviewed 0.1

Tags as in events.md. Two formats: (A) the Node transcript JSONL that Rust must READ and, during the parallel run, also WRITE; (B) the new SQLite store (doc §7.2).

## A. Node transcript format (compat, read + mirror-write) [from-node]

### A.1 Location
- Main: `$SYNT_HOME/projects/<sanitize(cwd)>/<session_id>.jsonl` [transcript.js:32-33]; `SYNT_HOME` defaults to `~/.synt` [auth.js:26-28].
- `sanitize(p)` = every char outside `[A-Za-z0-9]` → `-` [claudecfg.js:16-18]. Lossy (`/a-b` and `/a/b` collide); the real cwd is in every line, so readers trust the line `cwd`, not the folder name.
- Sub-agent: `…/<session_id>/subagents/agent-<agent_id>.jsonl` + `agent-<agent_id>.meta.json` = `{toolUseId:str, agentType:str, route:{source:str, fallback:str?, profile:str, candidate:str?}}` [transcript.js:28-30, agent.js:833-837].
- Note: Node does NOT list `~/.claude/projects` as sessions; that tree is read only to import auto-memory [importclaude.js:297-307]. The comment at history.js:3 saying `~/.claude/projects` is stale; the watcher reads `$SYNT_HOME/projects` [watcher.js:11-12].

### A.2 Common fields (every line) [agent.js:1023]
`timestamp:str (ISO 8601)`, `sessionId:str`, `cwd:str`, `type:str`; sub-agent lines add `agentId:str` [agent.js:879]. Files are append-only, written one line at a time with no locking and no fsync [transcript.js:40-47].

### A.3 Line types written by Node
| `type` | Extra fields | Source |
|---|---|---|
| `user` | `message:{role:"user", content:str\|[Block]}`, `displayText?:str` (what the user typed, without hook context), `isMeta?:bool` | agent.js:1256; tool results: agent.js:3503 (`content` = `[{type:"tool_result", tool_use_id, content, is_error}]`) |
| `assistant` | `message:{id:str?, role:"assistant", content:[text\|thinking{thinking,signature?}\|tool_use{id,name,input}], usage:obj?, stop_reason:str, model?:str}`, `isMeta?`, `synthetic?:"queue-summary"`, `final?:bool` | agent.js:3365, 3248 |
| `system` + `subtype:"compact_boundary"` | `trigger:"auto"\|"manual"`, `summary:str` | agent.js:2018 |
| `system` + `subtype:"handoff"` | `summary:str`, `fallback?:bool`, `bootstrap?:bool` | agent.js:2112-2121, history.js:118 |
| `summary` | `summary:str` (session title from /rename) | agent.js:2336, transcript.js:59-75 |
| `permission-mode` | `permissionMode:str` (one of the 6 modes; `bypassPermissions` never restored) | agent.js:1017, transcript.js:77-91 |
| `attachment` | `attachment:{type:"hook_success"\|"hook_non_blocking_error"\|"hook_blocking_error", hookName:"<event>:<target>", hookEvent:str, exitCode:int?}` | agent.js:1032-1041 |

### A.4 Reader rules (port of parser.js / history.js)
1. Split on `\n`; skip blank and unparsable lines [parser.js:429-440]. Ignore unknown `type`s. Tolerate Claude Code extras (`isSidechain` lines skipped [history.js:60], `version`, `gitBranch`, `uuid`, `parentUuid`).
2. Assistant blocks repeated under the same `message.id` are kept once [history.js:99-103].
3. A `tool_use` with no matching `tool_result` is `interrupted` on load [history.js:120-123].
4. Large files: UI loads read only the last 8 MiB, dropping the partial first line [history.js:14, 40-52]; a full resume/import reads the whole file in streaming 1 MiB blocks [history.js:139-170].
5. Soft errors are recognised by text patterns, since transcripts have no `soft` flag [tools/soft.js:3-7].

### A.5 Other Node state files (read; Rust writes them only when the user changes the value)
`$SYNT_HOME/state.json` = `{model?:str, effort?:str|null, autocompact?:"auto"|"off"|int}` [state.js:3-5, agent.js:370-373]; open tabs are persisted by opentabs.js using the same transcript paths [opentabs.js:20].

## B. New store [new]

### B.1 Location and engine
`$SYNT_HOME/synt.db` (SQLite, `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`); artifacts in `$SYNT_HOME/sessions/<session_id>/artifacts/` (doc §5.7). Timestamps are RFC 3339 UTC TEXT; JSON columns are TEXT holding valid JSON; ids are UUIDv4 TEXT unless noted.

### B.2 Tables (doc §7.2 minimum plus marked additions)
```sql
CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
CREATE TABLE sessions(
  id TEXT PRIMARY KEY, status TEXT NOT NULL, workspace_id TEXT NOT NULL,
  workspace_root TEXT NOT NULL, cwd TEXT NOT NULL, additional_dirs TEXT NOT NULL DEFAULT '[]',
  git_worktree TEXT, model_config TEXT NOT NULL, permission_mode TEXT NOT NULL,
  permission_policy_id TEXT NOT NULL, budget TEXT NOT NULL, usage TEXT NOT NULL DEFAULT '{}',
  event_cursor INTEGER NOT NULL DEFAULT 0, checkpoint_id TEXT, title TEXT,
  parent_session_id TEXT REFERENCES sessions(id), parent_tool_call_id TEXT,  -- sub-agents
  legacy_transcript_path TEXT,            -- Node JSONL this session mirrors/imports
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE tasks(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  request TEXT NOT NULL, acceptance_criteria TEXT NOT NULL DEFAULT '[]', status TEXT NOT NULL,
  reason TEXT, started_at TEXT NOT NULL, ended_at TEXT);
CREATE TABLE events(session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL,
  v INTEGER NOT NULL, ts TEXT NOT NULL, task_id TEXT, turn_id TEXT, agent_id TEXT, type TEXT NOT NULL,
  payload TEXT,            -- inline when the redacted payload is <= 8 KiB
  payload_ref TEXT,        -- artifact id otherwise (exactly one of payload/payload_ref is set)
  PRIMARY KEY(session_id, seq)) WITHOUT ROWID;
CREATE TABLE messages(     -- addition: provider-visible history for resume, exact bytes sent
  session_id TEXT NOT NULL REFERENCES sessions(id), idx INTEGER NOT NULL, role TEXT NOT NULL,
  content TEXT NOT NULL, compacted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(session_id, idx));
CREATE TABLE tool_calls(id TEXT NOT NULL, session_id TEXT NOT NULL REFERENCES sessions(id),
  turn_id TEXT NOT NULL, tool_name TEXT NOT NULL, input_ref TEXT, result_ref TEXT,
  status TEXT NOT NULL, decision TEXT, duration_ms INTEGER, PRIMARY KEY(session_id, id));
CREATE TABLE processes(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  name TEXT, pid INTEGER, cwd TEXT NOT NULL, command TEXT NOT NULL, started_at TEXT NOT NULL,
  ended_at TEXT, status TEXT NOT NULL, exit_code INTEGER, log_artifact_id TEXT, log_cursor TEXT);
CREATE TABLE browser_contexts(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  backend TEXT NOT NULL, profile_ref TEXT, status TEXT NOT NULL);
CREATE TABLE artifacts(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  kind TEXT NOT NULL, path TEXT NOT NULL, mime TEXT NOT NULL, size INTEGER NOT NULL,
  sha256 TEXT NOT NULL, retention_policy TEXT NOT NULL DEFAULT 'session', created_at TEXT NOT NULL);
CREATE TABLE verification_reports(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id),
  attempt INTEGER NOT NULL, status TEXT NOT NULL, report TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE approvals(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  tool_call_id TEXT NOT NULL, dialog TEXT NOT NULL, decision TEXT NOT NULL, reviewer TEXT NOT NULL,
  requested_at TEXT NOT NULL, resolved_at TEXT);
```
`tool_calls.id` is the provider's tool-use id and is unique only within a session, hence the composite key. The `messages` table exists because resume needs the exact history the model saw (Node rebuilds it lossily from UI items [agent.js:3035-3039]); whether events alone are enough is doc §13.4.

### B.3 Write rules
- One writer process per session: advisory lock file `$SYNT_HOME/sessions/<id>/lock` (`fs4`-style OS lock; crate unverified). A second Rust process attaches read-only.
- Each event is inserted in the same transaction that updates `sessions.event_cursor`; streaming to clients happens after commit, so a client never sees a seq that is not durable.
- Artifacts are written to a temp file, fsynced, renamed, then the row is inserted.

## C. Compatibility and migration plan [new]
Requirement: session formats read/write compatible both ways during the parallel run (doc §10 Phase 3).
1. **Node → Rust (import on demand).** Resuming a Node session id, or `synt import node-sessions`, parses A.3 into `sessions`/`messages`/`tasks` and one synthetic `legacy.imported` event `{source_path, lines, skipped}`; `legacy_transcript_path` is set. The original JSONL is never modified.
2. **Rust → Node (mirror).** While `compat.write_node_transcripts` is on (default ON until R9 cutover), Rust also appends A.3-shaped lines to `legacy_transcript_path` (created at A.1's path): `user`, `assistant`, tool-result `user`, `system/compact_boundary`, `summary`, `permission-mode`, hook `attachment`. Rust-only facts (verification, processes, approvals) are not mirrored. Mirror writes are best-effort, as in Node [transcript.js:39].
3. **Concurrency.** Node takes no lock, so the same session must not be open in both binaries at once. Rust refuses to resume a session whose JSONL changed after its last mirror write (size+mtime recorded) until it re-imports it.
4. **Golden gate.** For each fixture in `tests/golden/transcripts/`: Node JSONL → Rust import → Rust mirror export → Node `itemsFromEntries` must equal the original Node `itemsFromEntries` output (timestamps/ids normalised).
5. **Cutover (R9).** The mirror is switched off by default; import stays forever. Schema changes go through numbered `schema_migrations`; the binary refuses to open a DB newer than it knows, and a downgrade needs an explicit export.

## Checkpoints and `synt rewind`
Each file entry of a checkpoint may carry `post_sha256`: the state of the file after the run's last write to it (`file_state_hash`: sha256, or `absent` / `not-a-regular-file`). `synt rewind` refuses a file whose state differs or has no recorded hash unless `--force`, never touches symlinks or non-regular files, restores through a temp file and rename, and removes a file the run created only while it still has the recorded hash. `synt run --resume ID` only finds sessions of the current directory unless `--resume-any-workspace`.

## Open
- Artifact retention default and size cap (proposal: keep for the session's lifetime, `synt gc --older-than 30d`).
- Store full raw provider responses for debugging? Not in v1 (cost, secrets).
