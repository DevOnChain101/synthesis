# Unattended runs

Status: implemented on branch `feat/harness-governance`; owner decisions below were delegated with safe defaults and are to be reviewed. Terms: CLI = the terminal app, TUI = the desktop tab.

`synt run` (headless) and `synt engine --stdio` are treated as runs nobody watches. The interactive CLI and the desktop's in-process sessions are unchanged unless you set a limit yourself.

## Defaults of `synt run` and `synt engine --stdio`

| What | Default | Opt out |
|---|---|---|
| Wall clock | 1800 s per task | `--max-wall-secs N` |
| Cost cap | REQUIRED: the run is refused (exit 2) without one | `--max-cost-usd X`, or `--no-cost-cap` |
| Tokens | no cap | `--max-tokens N` (input + output + cache tokens) |
| Worktree | used when the directory is a git repository with a commit (otherwise a notice and the directory itself) | `--no-worktree` |
| Inferred-only checks | a task whose only checks were inferred stays `unverified` (exit 3) | an explicit `"inferred_only": "allow_passed"` in your settings |
| Session store | a message that cannot be stored ends the task `failed` (`context_error: session_store: ...`) | `--no-session-store` |
| Sandbox | enforce. Native Windows has no confinement: the run is refused unless `--sandbox off` is explicit | `--sandbox off` (loud warning) |

`synt engine --stdio` takes `--max-cost-usd`, `--no-cost-cap`, `--max-wall-secs`, `--max-tokens` and the common flags. The desktop launches its WSL engine with `--no-cost-cap` because a person is at the window.

## `--unattended`

One flag for all of the above, stricter: worktree is mandatory, the cost cap cannot be waived, inferred-only is forced to `cap_unverified` even if your settings say `allow_passed`, network must be `deny`, peers are off, the session store cannot be disabled. It REFUSES to start (exit 2) if the sandbox is not enforceable here, the directory is not a git repository with a commit, a resumed session had no worktree, or one of `--sandbox off|best-effort`, `--network allow|loopback`, `--no-worktree`, `--no-session-store`, `--peers`, `--no-cost-cap` is given.

## Cost accounting

Every response is priced on input, output, cache read and cache creation tokens, and the summarizer calls of compaction are charged to the task counters. Prices: `synt.pricing` of the user settings, else a built-in table for first-party models (approximate; the dearer price of a family so a cap does not under-count). See docs/specs/config.md section 6. Known over-count: OpenAI-compatible providers report cached tokens inside `prompt_tokens`, so cached tokens are counted at full input price and again at the cache-read price. Not yet priced: the auto-mode judge's cache tokens and memory reflection use their own simpler pricing.

## Stopping a run

`synt stop SESSION` from any terminal sets a stop request in the session store. The running process polls it once a second, cancels the task (graceful, exit 130), and after 15 s kills the process groups of its commands and exits 130. The second Ctrl-C / SIGTERM of `synt run` does the same kill before `exit(130)` (on macOS and Linux commands live in their own process groups, so a bare `exit` used to orphan them). Needs the same `SYNT_HOME`; a resume clears the request.

## Audit trail

Each stored event carries `prev`, the hash of the previous event row, and the session row keeps the head. `synt sessions verify ID` recomputes the chain and reports the first event that was changed, removed, or cut off at the end. Streaming `model.delta` bodies are not covered (they are compacted in place; the final `assistant.message` is). Event payloads moved to artifacts are covered by their artifact id, not their bytes. Sessions written before this build are reported as "not covered". LIMIT: this is tamper evidence, not prevention. A process of the same user outside the sandbox can rewrite events and the head together; keep `$SYNT_HOME` out of reach of the agent's commands (it is denied inside the sandbox).

`synt sessions show ID [--events] [--report]` prints the timeline, budget use and the stored verification reports. The report row now holds the full report as `synthesis-verification` serialises it (redacted); older rows hold the one-line summary.

## Resume of a worktree session

The worktree's pins (`GitInfo`) are stored in the session row. `--resume ID` re-pins them (the git pointers must still match what was recorded) and continues in the same worktree; it is refused if the worktree is gone. `--resume` with `--fork-session` starts a new session in a new worktree.

## Provider failover

`"fallback": ["provider/model", ...]` in the user `config.json` (docs/specs/config.md). Only before the first byte and only for rate limit, overload and server errors; emits `model.failover`. A fallback that cannot be built stops an unattended run and is warned about in the CLI.

## Not fixed / residual risks

- NOT_FIXED notifications on blocked/done (V9): use `--events-out` plus your own watcher or `synt sessions show`; a built-in hook is a later feature.
- The session-store failure ends the task as `context_error: session_store: ...`, not a bare `session_store` reason.
- The hash chain is tamper evidence only (see above). Artifact bytes are not chained.
- Failover keeps the primary's context window and output limit; a smaller fallback can fail with context overflow.
- Windows: the Job Object cannot confine, so headless Bash needs `--sandbox off`. Unverified on a real Windows machine; the logic is unit-tested only.
- The auto-mode classifier and memory reflection still price input/output only.
- A wall cap bounds a task, not a long-lived engine session of many tasks.
