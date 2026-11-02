# Spec: WorkspaceContext, AgentSession, ToolContext, session state machine — reviewed 0.1

Rust below is illustrative (serde-derived, not compiled). Extends doc §5.2 / §7.1. Tags as in events.md.

## 1. Types

```rust
pub type SessionId = String;   // UUIDv4 [from-node agent.js:929]
pub type TaskId = String;      // UUIDv4 [new]

#[derive(Serialize, Deserialize, Clone)]
pub struct WorkspaceContext {
    pub workspace_id: String,              // UUIDv4 [new]
    pub root: PathBuf,                     // canonical, immutable after creation (§4.1)
    pub cwd: PathBuf,                      // canonical, inside `root` or an additional dir
    pub additional_dirs: Vec<PathBuf>,     // canonical; /add-dir + settings permissions.additionalDirectories [from-node permissions.js:520, agent.js:2673]
    pub git_worktree: Option<PathBuf>,     // set when a worktree was allocated (R8)
    pub artifact_root: PathBuf,            // $SYNTHESIS_HOME/sessions/<session_id>/
    pub browser_context_id: Option<String>,
    pub process_group_id: String,          // ResourceManager handle, not an OS pgid
    pub path_policy: PathPolicy,           // decided once at creation
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct PathPolicy {
    pub case_insensitive: bool,  // true on windows + macos [from-node permissions.js:384]
    pub windows_separators: bool // true on windows only [from-node permissions.js:382]
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created, Preparing, Idle, Running, WaitingApproval, Verifying,
    Completed, Unverified, Blocked, Failed,
    Cancelling, Cancelled, Interrupted, Recovering,
}   // `Idle` and `Recovering` are additions to doc §7.1 [new], see §3

#[derive(Serialize, Deserialize, Clone)]
pub struct Budget {
    pub max_turns: u32,              // default 50 per task [from-node agent.js:45 MAX_TURN_ITERATIONS]
    pub max_tool_calls: u32,         // default 200 [new]
    pub max_cost_usd: Option<f64>,   // None = unlimited
    pub max_wall_ms: Option<u64>,    // [new]
    pub max_verify_attempts: u32,    // default 3 [new], see verification.md
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct BudgetUsage { pub turns: u32, pub tool_calls: u32, pub cost_usd: f64, pub wall_ms: u64, pub verify_attempts: u32 }

#[derive(Serialize, Deserialize)]
pub struct AgentSession {
    pub id: SessionId,
    pub task_id: Option<TaskId>,      // active task; None while Idle
    pub status: SessionStatus,
    pub workspace: WorkspaceContext,
    pub model_config_id: String,      // "provider/model" + effort snapshot
    pub permission_policy_id: String,
    pub permission_mode: PermissionMode,
    pub event_cursor: u64,            // last persisted seq
    pub budget: Budget,
    pub usage: BudgetUsage,
    pub created_at: String,           // RFC 3339
    pub updated_at: String,
    pub parent: Option<ParentRef>,    // sub-agent: parent session + tool_call_id [from-node agent.js:833 meta.json]
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]   // wire values identical to Node [from-node dialogs.js:9-13]
pub enum PermissionMode { Default, AcceptEdits, Plan, Auto, DontAsk, BypassPermissions }

pub struct ToolContext {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: String,
    pub tool_call_id: String,                     // [from-node agent.js:1068 toolUseId]
    pub workspace: Arc<WorkspaceContext>,
    pub env: Arc<BTreeMap<String, String>>,       // already secret-stripped [from-node auth.js:48 stripSecretEnv]
    pub read_ledger: Arc<Mutex<HashMap<PathBuf, SystemTime>>>, // path -> mtime at last Read [from-node read.js:116, edit.js:86]
    pub permission_mode: PermissionMode,          // mode the turn started with [from-node agent.js:3538]
    pub approval_token: Option<String>,           // approval_id when the call was user-approved
    pub deadline: Option<Instant>,
    pub cancel: tokio_util::sync::CancellationToken,
}
```

## 2. Ownership
- A `WorkspaceContext` is created by `WorkspaceManager::open(selection)` and persisted in `sessions` before the first event after `session.started`.
- Everything a tool needs about location comes from `ToolContext`. No tool, runtime or provider may read process-global cwd or env [new].

## 3. State machine [new; extends doc §5.2]
Transitions not listed are rejected (error + `notice`). Each accepted transition emits `session.status_changed`.

| From | To | Trigger |
|---|---|---|
| created | preparing | workspace/model resolution starts |
| preparing | idle | interactive session ready, no task |
| preparing, idle | running | task submitted (`task.started`) |
| running | waiting_approval | policy decision `ask` |
| waiting_approval | running | approval resolved (allow or deny; deny is evidence fed back) |
| running | verifying | model proposes finish (end_turn without tool calls) and gate applies |
| verifying | waiting_approval | a gate check needs approval (returns to verifying when answered) |
| verifying | running | verification failed/unverified and verify budget left |
| verifying | completed / unverified / blocked / failed | gate result (verification.md §4) |
| running | failed / blocked | budget exhausted, unrecoverable provider error, unsafe workspace |
| running, waiting_approval, verifying | cancelling → cancelled | user cancel |
| running, waiting_approval, verifying | interrupted | crash / process exit detected on reopen |
| interrupted | recovering → running \| blocked | resume (R8) |
| completed, unverified, blocked, failed, cancelled | idle | interactive only: next prompt may follow; headless ends the session instead |

`Idle` is needed because an interactive session outlives a task (Node keeps `status = 'idle'` between prompts [from-node agent.js:350, 3313]). Terminal statuses are per task; the session ends with `session.completed|failed|cancelled`.

## 4. Invariants

### 4.1 No implicit cwd inheritance [new; doc §5.2]
1. A session is created only from an explicit `WorkspaceSelection`: `Path(p)` (CLI `--cwd`, TUI folder picker), `Resume(session_id)` (persisted `root`/`cwd`), or `LaunchDir` (CLI `synt`, `synt run` and the TUI when `--cwd` is absent: the binary's start directory, read once at the CLI boundary by `launch_dir()` and recorded as the explicit selection; `--cwd` always wins).
2. Node defaults the agent cwd to `process.cwd()` [from-node agent.js:303]; Rust MUST NOT. The libraries never read the process cwd; only `launch_dir()` in the CLI boundary does. Enforcement: `clippy.toml` `disallowed-methods = ["std::env::current_dir", "std::env::set_current_dir"]` for all crates except `apps/cli/src/main.rs`.
3. `root` never changes. `cwd` changes only through an explicit session command, and the new value must be inside `root` or `additional_dirs`. `cd` inside a Bash command does not persist (each command runs in a fresh shell with `current_dir(ctx.workspace.cwd)`) [from-node bash.js:173].
4. Every child process is spawned with explicit `current_dir` and an explicit env built from `ctx.env`.

### 4.2 Path resolution `resolve(ctx, input) -> Resolved` [from-node tools/util.js:10 + permissions.js:569-604]
1. Empty input → `cwd`. Absolute → as given. Relative → `cwd.join(input)`. No `~` expansion in tool paths (Node does not expand it [from-node util.js:10]); `~/` is expanded only in permission rule specs [from-node permissions.js:396].
2. Lexical normalisation (`.`/`..`) without touching the filesystem.
3. Classify against roots = `[cwd] + additional_dirs` (Node's roots [from-node permissions.js:574]; `root` is always in this set because `cwd` starts at `root`):
   - `Inside`: lexically inside a root AND the realpath is inside that root's realpath.
   - `OutsideViaLink`: lexically inside, realpath outside (Node asks the user, never the classifier [from-node permissions.js:802,816]).
   - `Outside`: not lexically inside.
   - `Network`: UNC `\\host\share` or `//host/share`, excluding `\\?\` and `\\.\` [from-node permissions.js:413]; never Inside, always `ask`.
4. Realpath rule: realpath of the path, or of its nearest existing ancestor plus the missing tail; a dangling link anywhere on the path makes the result `Outside` [from-node permissions.js:331-353].
5. `root`, `cwd` and `additional_dirs` are stored as realpaths at creation, so macOS `/var` ↔ `/private/var` and Windows 8.3 short names compare stably [new].

### 4.3 Comparison rules (comparison only, stored strings are never rewritten)
- Case folding: compare lowercased on windows and macos, exact on linux [from-node permissions.js:384]. Use full Unicode lowercase, not ASCII-only [new].
- Separators: on windows accept `\` and `/`, compare in `/` form, strip a `\\?\` verbatim prefix and fold the drive letter [from-node permissions.js:382; prefix strip new]. On POSIX `\` is an ordinary filename character and is NEVER rewritten [from-node permissions.js:382].
- Containment is component-wise (`/a/bc` is not inside `/a/b`) [from-node permissions.js:325-329].
- Non-UTF-8 paths: rejected with a tool error in v1 [new].

## Open
- macOS volumes can be case-sensitive; folding there can call `/Proj/x` inside root `/proj`. Node accepts this; keep, or probe the volume once at creation?
- `Idle` / `Recovering` statuses are additions to doc §7.1; product owner to confirm.
