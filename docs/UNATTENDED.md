# Unattended runs

Status: implemented on branch `feat/harness-governance`; owner decisions below were delegated with safe defaults and are to be reviewed. Terms: CLI = the terminal app, TUI = the desktop tab.

`synt run` (headless) and `synt engine --stdio` are treated as runs nobody watches. The interactive CLI and the desktop's in-process sessions are unchanged unless you set a limit yourself.

## Defaults of `synt run` and `synt engine --stdio`

| What | Default | Opt out |
|---|---|---|
| Wall clock | 1800 s per task | `--max-wall-secs N` |
| Cost cap | REQUIRED: the run is refused (exit 2) without one | `--max-cost-usd X`, or `--no-cost-cap` |
| Tokens | no cap | `--max-tokens N` (input + output + cache tokens) |
| Worktree | used when the directory is a git repository with a commit; it starts from the tree as it is (uncommitted and untracked work included, detached HEAD works, submodules are NOT checked out and a notice says so). A notice names the worktree path and branch. If it cannot be made, a plain headless run falls back to the directory with a loud warning; `--unattended` refuses | `--no-worktree` |
| Inferred-only checks | a task whose only checks were inferred stays `unverified` (exit 3) | an explicit `"inferred_only": "allow_passed"` in your settings |
| Session store | a message that cannot be stored ends the task `failed` (`context_error: session_store: ...`) | `--no-session-store` |
| Sandbox | enforce. Native Windows has no confinement: the run is refused unless `--sandbox off` is explicit | `--sandbox off` (loud warning) |

`synt engine --stdio` takes `--max-cost-usd`, `--no-cost-cap`, `--max-wall-secs`, `--max-tokens` and the common flags. A desktop host launches it with `--host desktop`: a person is at the window, so there is no default worktree, no default wall cap and no required cap; the interactive store behaviour applies. The desktop passes the cap itself (below).

### Desktop (WSL) cost cap

The Windows desktop runs `synt engine --stdio --host desktop --max-cost-usd N` in WSL. `N` is `synt.desktop.max_cost_usd` (user or managed settings, managed wins; default 10 USD per task). `synt.desktop.no_cost_cap: true` is the explicit way to run without one (the flag is then not passed). The cap needs a price for the model: built in for Anthropic and OpenAI models, otherwise `synt.pricing` inside WSL, or the session does not start and says so.

## `--unattended`

One flag for all of the above, stricter: worktree is mandatory, the cost cap cannot be waived, inferred-only is forced to `cap_unverified` even if your settings say `allow_passed`, network must be `deny`, peers are off, the session store cannot be disabled. It REFUSES to start (exit 2) if the sandbox is not enforceable here, the directory is not a git repository with a commit, a resumed session had no worktree, or one of `--sandbox off|best-effort`, `--network allow|loopback`, `--permission-mode bypassPermissions`, `--trust-project`, `--approve-mcp`, `--no-worktree`, `--no-session-store`, `--peers`, `--no-cost-cap` is given. Out of scope (still allowed): `--allow` rules (an allow rule for an irreversible command is yours to judge), MCP servers and hooks from your own user settings, `--add-dir`, `--sandbox-read`, and anything the model does inside the sandbox and the worktree (a pushed branch, a network-less but destructive command).

## Verification gate and unattended runs

The gate (docs/specs/verification.md) freezes the checks at the task start, keeps a baseline file (`gate-baseline.json` under the session's artifact dir, no database change), and caps a pass at `unverified` for planned test checks that did not pass, changed definitions, modified test files and model-declared checks. On top of that, a headless run or engine treats `inferred_only` as `cap_unverified`: with no user criteria, inferred checks alone end the task `unverified` (exit 3) even when they passed, which main's gate would otherwise accept as `passed`. The unattended default is stricter than the gate default; your own `"inferred_only": "allow_passed"` in the USER settings lifts it for a plain headless run (never under `--unattended`). Every `unverified` outcome of the gate keeps exit 3.

## Cost accounting

Every response is priced on input, output, cache read and cache creation tokens, and the summarizer calls of compaction are charged to the task counters. Prices: `synt.pricing` of the user settings, else a built-in table for first-party models (approximate; the dearer price of a family so a cap does not under-count). See docs/specs/config.md section 6. Granularity: the cap is checked between model responses. One response can overshoot it by that response's cost (its input plus up to `max_tokens` of output); there is no pre-turn estimate that refuses a turn whose worst case would cross the cap (NOT_FIXED; plan: estimate input from the last `usage`, add the request's `max_tokens` times the output price, refuse when it exceeds the remainder). Each response is priced by the model that answered it: the run's model, a sub-agent's model or a failover target; under a cap every fallback and sub-agent model must have a price or the run is refused at start. Built-in prices match exact ids (and a date suffix) only: `gpt-5-pro`, `o3-pro`, `o4-mini-deep-research` and newer Claude ids have no built-in price, so a capped run is refused until `synt.pricing` prices them. Not modelled: tiers by prompt length (Sonnet above 200k input tokens costs more); price those models yourself at the dearer tier. Known over-count: OpenAI-compatible providers report cached tokens inside `prompt_tokens`, so cached tokens are counted at full input price and again at the cache-read price. Not yet priced: the auto-mode judge's cache tokens and memory reflection use their own simpler pricing.

## Stopping a run

`synt stop SESSION` from any terminal sets a stop request in the session store. It works only on a session that a live process holds (`synt run`, `synt engine --stdio`, the CLI); for anything else it prints "no live process holds this session" and exits 1, never 0. The process polls once a second, cancels the task (graceful, exit 130 / `cancelled`), and after 15 s kills the process groups of its commands, marks the row `interrupted` (best effort, bounded to 3 s) and exits 130. Only a request made after the process started counts, so an old request does not stop a resumed session, and a request made while a resume is still preparing is not lost. If the stop flag cannot be read five times in a row the session is stopped anyway (fail closed) and stderr says why. The second Ctrl-C / SIGTERM of `synt run` runs the same bounded kill (on macOS and Linux commands live in their own process groups, so a bare `exit` used to orphan them). Needs the same `SYNT_HOME`. NOT reached: a desktop window's in-process session (the stop would have to exit the whole app).

## Audit trail

Each stored event carries `prev`, an HMAC-SHA256 (key: the per-user file `$SYNT_HOME/chain.key`, 0600, outside the database) over the previous link and the event row; the session row keeps the head and a `chained` flag set at creation. `synt sessions verify ID` recomputes it: exit 0 verified, 1 TAMPERED (an event changed, removed, cut off at the end, a link or the head removed on a chained session), 3 NOT VERIFIED (a session created before the chain existed, or the key file is gone). SCOPE: the `events` table only. Tasks, verification reports, approvals, messages and tool_calls are NOT chained; `verification.completed` carries the sha256 of the full report so the stored report can be compared with the event. Streaming `model.delta` bodies are not covered (they are compacted in place; the final `assistant.message` is). Event payloads moved to artifacts are covered by their artifact id, not their bytes. LIMIT: tamper evidence, not prevention. Someone who can write only `synt.db` cannot forge the chain; a process of the same user outside the sandbox can read the key and rewrite everything. The sandbox denies `$SYNT_HOME` to the agent's commands.

`synt sessions show ID [--events] [--report]` prints the timeline, budget use, the active model (after a failover) and the stored verification reports (redacted again, control characters made harmless). The report row now holds the full report as `synthesis-verification` serialises it; older rows hold the one-line summary.

## Resume of a worktree session

The worktree's pins (`GitInfo`) are stored in the session row. `--resume ID` re-pins them only if the record names one of this tool's worktrees (`$SYNT_HOME/worktrees/<session id>` of the recorded repository, git pointers matching) and continues in it; otherwise or if it is gone it refuses. `--resume` / `--continue` find a session that ran exactly in this directory or in the worktree this exact directory got (the repository top, or the same subfolder). `--fork-session` starts a new session in a new worktree.

## Provider failover

`"fallback": ["provider/model", ...]` in the user `config.json` (docs/specs/config.md). Only before the first byte and only for rate limit, overload and server errors; emits `model.failover`. The switch holds for `fallbackReprobeMinutes` (default 5), then the primary is asked again and a `model.failover` event with reason `primary_recovered` says when it answers. `synt sessions show` names the active model. DATA EGRESS: a failover sends the conversation (prompts, file contents the model read, tool output) to the fallback provider, under that provider's data policy and key; only list providers you accept for the same work. A fallback that cannot be built stops an unattended run and is warned about in the CLI.

## Red-team suite and nightly evals

`tests/redteam` (CI job `redteam`, all three OSes) runs about 50 scripted attacks against the real binary in default, acceptEdits, auto (always-allow fake judge) and bypassPermissions, and `evals-nightly.yml` runs `tests/agent-evals` with a real model when the secret `SYNT_API_KEY` exists (variables `SYNT_EVAL_PROVIDER`, `SYNT_EVAL_MODEL`, `SYNT_EVAL_MAX_COST_USD`, default 1 USD per run; the summary shows status-ok, false-success and cost; the key reaches only the adapter, never the graded checks). What an unattended run does NOT stop is listed in docs/SECURITY.md ("Red-team suite"): a repo git alias or script in `auto`/`bypassPermissions`, no disk quota, no process limit. The nightly secret belongs in the Environment `evals-nightly` (docs/SECURITY.md).

## Not fixed / residual risks

- NOT_FIXED notifications on blocked/done (V9): use `--events-out` plus your own watcher or `synt sessions show`; a built-in hook is a later feature.
- The session-store failure ends the task as `context_error: session_store: ...`, not a bare `session_store` reason.
- The hash chain is tamper evidence only and covers events only (see above). Artifact bytes are not chained.
- Failover keeps the primary's context window and output limit; a smaller fallback can fail with context overflow.
- Windows: the Job Object cannot confine, so a headless run does not start unless `--sandbox off` is explicit (the refusal says `--sandbox off (explicit, unconfined)`; best-effort is never offered to an unattended run). Unverified on a real Windows machine; the logic is unit-tested only.
- Submodules are not checked out in the implicit worktree.
- The pre-turn cost estimate (see Cost accounting) is not built.
- The auto-mode classifier and memory reflection still price input/output only and with the run's price.
- A wall cap bounds a task, not a long-lived engine session of many tasks.
- No disk or process quota for commands (red-team known gaps `large-write`, `fork-fanout`); a git alias or repo script runs in `bypassPermissions` and in `auto` when the judge allows it.
