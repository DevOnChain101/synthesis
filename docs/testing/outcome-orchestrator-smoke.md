# Orchestrator process: manual smoke (PR-12)

Status: what exists after PR-12. Related: [plan PR-12](../architecture/outcome-orchestration-plan.md), [ADR-017](../architecture/adr/ADR-017-outcome-orchestration.md), [ADR-021](../architecture/adr/ADR-021-outcome-ui-protocol.md).

**What you can do (PR-13).** `synt outcome ...` creates an outcome, shows and approves its contract, starts it, shows status and decisions and answers a decision (section 7). There is still no TUI or Super Agent surface. The orchestrator plans with the deterministic stub planner (D13) and starts workers with whatever provider your settings name; the real policy gateway is not built. Do not point it at a repository you care about.

Use a scratch home so nothing touches `~/.synt`:

```sh
export SYNT_HOME=/tmp/synt-smoke     # short path: a unix socket path is limited to about 100 bytes
mkdir -p $SYNT_HOME
```

## 1. Off by default

```sh
synt orchestrator --serve; echo "exit=$?"
```

Expect: `synt: error: outcomes are off (synt.orchestrator.enabled is false): not starting`, exit 2, and no `$SYNT_HOME/run/orch.json`.

## 2. Turn it on and start it

Put this in `$SYNT_HOME/settings.json` (user settings; a project file cannot turn it on):

```json
{"synt": {"orchestrator": {"enabled": true}}}
```

```sh
synt orchestrator --serve &
sleep 1; ls -l $SYNT_HOME/run; cat $SYNT_HOME/run/orch.json
```

Expect: `orch.key` (mode 0600), `orch.lock`, `orch.json` naming a `.sock` under `run/`, the directory mode 0700, and the process staying in the foreground/background (it exits by itself only after `idle_exit_minutes`, default 10, with no client and no outcome). Its log goes to the terminal here (when a client starts it, to `run/orch.log`). It says once `cycle: outcome orchestration is not enabled for this store`: the store is still at the default schema, and only `synt outcome enable --yes` migrates it. That line is expected.

## 3. One orchestrator per home

```sh
synt orchestrator --serve; echo "exit=$?"
```

Expect: `another orchestrator already serves this home`, exit 0, and `orch.json` unchanged.

## 4. The stdio engine is read-only

```sh
printf '%s\n' \
 '{"id":1,"method":"orchestrator.status"}' \
 '{"id":2,"method":"outcome.start","params":{"id":"11111111-1111-4111-8111-111111111111","idempotency_key":"k","surface":"cli"}}' \
 | synt engine --stdio --no-cost-cap
```

Expect: a `hello` line with `protocol` 5, a result for id 1 with `"enabled":true` and a `schema` of `null` or 4, and an error for id 2 saying commands are accepted only on the orchestrator endpoint. (This is the link an agent shell could open: it can read, it cannot queue a command.)

## 5. Kill it and let it come back

```sh
kill -9 $(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' $SYNT_HOME/run/orch.json)
synt orchestrator --serve &
sleep 1; cat $SYNT_HOME/run/orch.json
```

Expect: the second start succeeds (the kernel released the lock), `orch.json` names a new socket and pid, and the dead process's socket file is gone (it is removed only when it is the orchestrator's own: `<16 hex>.sock`, mode 0600, owned by you, nobody listening). When a client started the process, `run/orch.log` is mode 0600.

## 6. Turn it off

Set `enabled` to `false` in `settings.json`. Expect: within a second the process logs `outcomes were turned off: stopping` and exits, and `orch.json` is removed.

## 7. Drive an outcome by hand (PR-13)

Use a scratch git repository with one commit (`R`) and the scratch home above, with `enabled` true.

The enable gate is open in every build since B8, official releases included; enabling still needs the explicit `synt outcome enable` flow.

```sh
synt outcome enable            # prints the integrity notice and exits 2: nothing changed
synt outcome enable --yes      # backup, then schema v5, then the owner's disclosure text (same first paragraph as the notice)
echo '[{"id":"c1","text":"the endpoint answers","check_ids":["t"]}]' > criteria.json
echo '[{"id":"t","command":"true"}]' > checks.json
echo '{"allowed_paths":["src/**"],"capabilities":{"repo.read":"allow","repo.write":"allow"},"budgets":{"max_cost_usd":1.0}}' > envelope.json
synt outcome create --goal "a small endpoint" --criteria-file criteria.json --checks-file checks.json \
  --envelope-file envelope.json --base-commit $(git -C R rev-parse HEAD) --cwd R
synt outcome contract ID                               # what you are about to approve
synt outcome approve ID --version 1 --hash HASH        # the hash `create` and `contract` print
synt outcome start ID
synt outcome status ID                                 # first line: what needs you
synt outcome decisions ID                              # when a decision is pending: the whole brief, its revision and brief hash
synt outcome decisions ID --watch                      # stays open: shows a card again when it changes (push hint, then refetch); Ctrl-C ends
synt outcome resolve ID DECISION --choice REST --revision N --brief-hash H
synt outcome pause ID; synt outcome resume ID; synt outcome cancel ID
synt outcome revise ID --contract-file revised.json    # after answering a decision with edit_scope: prints the diff and the approve line; approving that version and hash replans
synt outcome inspect ID --tab evidence; synt outcome events ID
```

Expect: `create` prints the id and the approve command; `approve` ends `done`; `status --json` is one object with `"schema": 1`; `resolve` without `--choice` prints the card and exits 2 without sending anything; with a stale revision or hash it exits 2 and sends nothing; a second answer to the same decision exits 1. With outcomes off every command exits 2 saying so and starts no process. `enable --yes` prints the integrity disclosure after the schema line, and `--json` carries `disclosure: {id, text}`. For `--watch`, answer a decision from a second terminal: the first shows the card again as approved (on the push hint; at the latest after 60 s). A worker only does real work with a provider configured; without one the attempt fails and the outcome blocks, which is expected here.

## What is covered by automated tests instead

```sh
cargo test -p synthesis-app --test orch_endpoint          # key, attach, surface mismatch, create, enable, approve, cancel (unix)
cargo test -p synthesis-cli --test orchestrator_process   # real process kill and restart, two owners, recovery, notices (unix)
cargo test -p synthesis-cli --test outcome_cmds           # synt outcome through the real owner process (unix)
cargo test -p synthesis-app --lib outcome_cmd             # dispatch, goldens, no-preselection, JSON schema
cargo test -p synthesis-orchestrator --test lifecycle     # start, pause (durable), resume, cancel, ack, verdict
cargo test -p synthesis-cli --features test-provider --test engine_stdio
cargo test -p synthesis-cli --features test-provider --test outcome_e2e    # real worker, verdict, ack, decision, kill -9 matrix (unix, 2 to 3 min)
```

## Not verified

- Windows (named pipe, native or WSL) and Linux with bubblewrap for the process tests: written, not run by the author. CI will compile them; the process and endpoint tests are unix-only.
- The owner's Windows/WSL run (#177) can use an official release build: the gate is open since B8.
- A worker run by the orchestrator with a real model, and the live policy gateway (not built). `outcome_e2e` runs real workers against a scripted model (`SYNT_TEST_ORCH_SCRIPT`, builds with `test-provider` only), so create, verification, verdict, ack, a real decision and the kill matrix are automated; what it does not cover is listed in the acceptance doc (section 3c). `outcome_cmds` still seeds its decision in the store because it also runs in builds without `test-provider`, where no worker can be given a model.
- The notify command through the confined `Notifier` against a real sandbox: the payload and delivery are unit tested without a sandbox.

## Model planner and replanner (B7, B9): what to know before you turn it on

- With `synt.orchestrator.planner.mode = model` (user or managed settings only) a read-only planning session reads the repository at the outcome's base commit and the model provider receives what it reads. **Anything committed to the repository, including a secret under another name or in a config file, is readable by the planner and can reach the model provider; this is the same exposure as a normal run.** The planner cannot read `.env*`, `~/.ssh`, `~/.aws` and `~/.gnupg`, and it has no network.
- A model plan or replan is only a proposal. The card shows every item (and, for a replan, what was added, removed and changed against the current plan); nothing runs until you choose `approve_model_plan`, `use_stub_plan` or, for a replan, `keep_current_plan`.
- A replan is proposed once per plan state after a work item failed or ended blocked, after a rework verdict, or after a revised contract was approved. Its input is state and reason codes only, never what a worker wrote.
- After you answer a blocked worker's question or allow a grant, the next attempt starts from that worker's commit (the card shows its short id); choosing `retry` starts fresh from the base commit. The final check still compares everything against the base commit.

## In-app path (desktop app, owner request 2026-11-17)

No hand-edited settings and no `synt` commands. Use a scratch home when trying it from a checkout (`SYNT_HOME=/tmp/synt-smoke synthesis-desktop`), or the packaged `Synthesis.app` built with `--synt` (otherwise the app cannot start the orchestrator itself and says to run `synt outcome list`).

1. Settings > Execution (search "Outcomes"): the switch is off. Turn it on: the integrity notice appears; Cancel writes nothing (`$SYNT_HOME/settings.json` unchanged). Or open the Outcomes tab: "Super Agent is off" with "Turn on Super Agent...".
2. Confirm in the dialog (Tab to Cancel, Tab to Confirm, Enter; or click). Expect `settings.json` to hold `{"synt":{"orchestrator":{"enabled":true}}}` with other keys untouched, and the tab to say "Super Agent needs one more step".
3. "Migrate the session store..." shows the same notice with its own Confirm. Confirm: `backups/pre-v5-*` appears, the schema becomes 5, the orchestrator starts (`$SYNT_HOME/run/orch.json`) and the outcome list shows. Nothing migrates before this confirm.
4. With a managed settings file that sets `synt.orchestrator.enabled`, the Settings switch is locked and the tab's confirm reports the refusal.
5. Native Windows: the switch is locked with the WSL2 reason and the tab has no button.

Not covered by automated tests: a real window, a real orchestrator process started from the bundle, and a packaged bundle run (see the PR report).
