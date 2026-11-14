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
```

`sessions.title` holds only a title a person set (`/rename`, `SessionWriter::set_title`: the row, `updated_at`, and a Node `summary` line; the value is redacted). It is `NULL` otherwise: the first prompt that names an untitled session in the lists is derived when they are read (`synthesis-view`) and never written, so it cannot overwrite a set title. No schema change (no `session.renamed` event: the lists read the store).

```sql
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

Migration v2 (addition): `ALTER TABLE sessions ADD COLUMN closed_at TEXT`. `/close` sets it; `Reader::list_used_sessions` (Running/Workspace panels, `/resume` picker) skips closed sessions, `list_sessions` and resume by id still see them. A v2 store makes older binaries fail with `SchemaTooNew`.

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

6. **Schema v5, outcome orchestration (opt-in, ADR-016).** Migration 5 adds `sessions.kind` and the outcome tables (`outcomes`, `contracts`, `plans`, `work_items`, `work_item_deps`, `attempts`, `evidence`, `decisions`, `decision_approvals`, `grants`, `outcome_commands`, `outbox`).
   - **A default open never applies it.** `StoreOptions::schema_target` defaults to 4, so with `synt.orchestrator.enabled` off the schema, the SQL of every existing command and the files on disk are exactly those of v4 (`tests/outcome_store.rs` pins the v4 `sqlite_master` digest). Only an open with `schema_target = 5` (the explicit enable step) migrates, and only after `backups/pre-v5-<time>/synt.db` was written and passed `quick_check` (mode 0600 in a 0700 directory; no backup, no migration). The copy's version is re-read after it is written: a copy that is not at the version being upgraded (another process enabled in between) is deleted and the open starts over, so a backup is never labelled with the wrong version. A v5 database is read and written by this build whatever the flag says; the listers and cleaners add `kind = 'chat'` only when the database is v5, and the cleaners never touch an attempt's session. `/resume` and `/close` refuse control and attempt sessions.
   - **Older binaries.** A released binary that supports v4 refuses a v5 database when it opens the store for writing (`Store::open`) with its existing `SchemaTooNew` text, `database schema v5 is newer than this build supports (v4)`. Its read-only `Reader::open` does not check the version (released behaviour): it opens a v5 database, `list_used_sessions` and `get_session` work, and `list_sessions` also lists the hidden control sessions (rows with no messages). Nothing in a released binary writes through a `Reader`.
   - **Rollback.** (1) Stop every surface (CLI, desktop) and any `synt orchestrator` process, so nothing holds the database. (2) Delete `synt.db-wal` and `synt.db-shm` next to `synt.db`: a stale write-ahead log from the v5 database must not be replayed over the restored file. (3) Copy `backups/pre-v5-<time>/synt.db` over `synt.db` and start the older binary. **Everything written after enabling is lost**, outcome data and ordinary chat sessions alike (the backup holds rows only, not artifact files). There is no downgrade tool.
   - **Outcome events** are untyped additive events (`outcome.*`, `plan.*`, `work_item.*`, `attempt.*`, `decision.*`, `evidence.*`, `v` stays 1) of the outcome's hidden control session, so they are hash-chained and verified by `synt sessions verify`.
   - **Integrity claim (precise).** Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. The same-user, outside-the-sandbox cases and the cut-off tail are accepted residuals, not defects of this schema (`chain_head` is a plain column in the same database: deleting the tail events and setting it to the previous link passes `verify_chain`).
   - **What is and is not covered.** MACs cover only contract approvals (`contracts.approved_mac`: outcome, version, body hash, approver, time) and decision approvals (`decision_approvals.mac`: decision, outcome, kind, revision, binding hash, expiry, brief hash, approver, choice, time). Reading a resolved decision verifies the MAC, recomputes the binding hash, and derives the state from the MAC'd choice; a mismatch is `Error::Tampered`, never a quiet "not approved". **Not covered by any MAC or cross-check against the chained events:** evidence rows (a writer without the key can INSERT a passing `independent_check` row), `attempts.subject_commit` and `fence`, `work_items.spec`, `grants.consumed_at`, `evidence.flags`, `outbox`, `outcome_commands`. Triggers make several of these one-way for old or careless writers (`attempts_commit_once`, `work_items_spec_immutable`, `grants_consume_once`, evidence immutable except `flags`), nothing more.
   - **Prerequisites before outcomes run real attempts.** The sandbox must deny the attempt both `synt.db*` (state paths) and `chain.key` and `backups/` (PR-S1 / PR #117, merged; red-team cases `evid-chain-key-read`, `evid-backups-read` and `orch-db-write-from-attempt` must be green on Linux and macOS CI). The MVP verdict in the store reads unsigned rows and is not to be relied on before PR-09 (signed `evidence.recorded` events) and PR-08 (bypass gate); see the Enable gate in the plan.
   - **Settings gate.** `create_outcome` and `open_outcome` take an `OutcomePolicy` built from `synt.orchestrator.*` (`synthesis_app::outcome_policy`): they refuse when `enabled` is off (a managed `false`, or a managed value that failed to parse, also stops an outcome that already exists), they reject a contract whose budgets exceed the ceilings (never silently clamped: what the person approves is what runs), and the attempt budget is `min(contract, settings)` at use time.

## Checkpoints and `synt rewind`
Each file entry of a checkpoint may carry `post_sha256`: the state of the file after the run's last write to it (`file_state_hash`: sha256, or `absent` / `not-a-regular-file`). `synt rewind` refuses a file whose state differs or has no recorded hash unless `--force`, never touches symlinks or non-regular files, restores through a temp file and rename, and removes a file the run created only while it still has the recorded hash. A checkpoint file entry also carries `artifact_sha256` (sha256 of the stored snapshot bytes) and `pre_state` (`file_state_hash` when the checkpoint was taken; `absent` = the run is about to create the file). Both are additive JSON fields (older binaries ignore them). A rewind writes a snapshot back only if it still hashes to `artifact_sha256`, and deletes a file only with `pre_state: absent`; entries of older builds have neither and need `synt rewind --force` (the desktop refuses them). The hashes sit in the same store as the rows, so they detect damage and a replaced artifact file, not a forged row: whoever can write `synt.db` can forge both. Keeping the session store outside the agent's write scope is a sandbox decision (R7). Checkpoints are taken by the file tools only: moves, renames and deletes through Bash are not tracked and a rewind cannot restore them. `synt rewind` refuses a session whose workspace root is a filesystem root, the home directory or a directory containing it (and any session when the home directory is unknown) unless `--allow-wide-root`; `--force` does not override a recorded `pre_state` or hash; the lock file is opened without following links, takes a per-session lock (`sessions/ID/.rewind.lock`) to apply, and checks every path again (inside the roots, no symlink or junction directory, same file state, same snapshot hash) right before its own write. `synt run --resume ID` only finds sessions of the current directory unless `--resume-any-workspace`.

## Open
- Empty sessions (no `messages` and no `tasks` row): `Reader::list_used_sessions` leaves them out of listings (TUI panels, `/resume`); `Store::prune_empty_sessions` deletes those older than 1 hour at CLI start (all dependent rows and `sessions/<id>/`), skipping the session being resumed, any session with a child session, and any whose `lock` is held by a live process.
- `Store::clean_plan` / `clean` / `backup_db` (desktop `/clean`, see desktop.md): permanent deletion of sessions by scope (closed or empty-and-older-than-1-hour, inactive 30 days, all) with the same row list as `prune_empty_sessions`; never the kept ids, lock-held sessions or their parent/child tree; directories only under `sessions/<id>`, links never followed; optional `VACUUM INTO backups/clean-<time>/synt.db`.
- Artifact retention default and size cap (proposal: keep for the session's lifetime, `synt gc --older-than 30d`).
- Store full raw provider responses for debugging? Not in v1 (cost, secrets).
