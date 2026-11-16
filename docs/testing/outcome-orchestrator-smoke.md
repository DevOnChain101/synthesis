# Orchestrator process: manual smoke (PR-12)

Status: what exists after PR-12. Related: [plan PR-12](../architecture/outcome-orchestration-plan.md), [ADR-017](../architecture/adr/ADR-017-outcome-orchestration.md), [ADR-021](../architecture/adr/ADR-021-outcome-ui-protocol.md).

**What you cannot do yet.** There is no CLI surface for outcomes (`synt outcome ...` is PR-13), no TUI and no Super Agent surface. So you cannot create an outcome, approve a contract, start one or answer a decision by hand. Those paths are proven by automated tests only (last section). This smoke checks the process, the endpoint, the single-owner rule, the read-only stdio link and the off switch. Do not point it at a repository you care about: nothing here starts a worker.

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

Expect: `orch.key` (mode 0600), `orch.lock`, `orch.json` naming a `.sock` under `run/`, the directory mode 0700, and the process staying in the foreground/background (it exits by itself only after `idle_exit_minutes`, default 10, with no client and no outcome). Its log goes to the terminal here (when a client starts it, to `run/orch.log`). It says once `cycle: outcome orchestration is not enabled for this store`: the store is still at the default schema, and only `orchestrator.enable` (no client for it until PR-13; the automated tests call it) migrates it. That line is expected.

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

Expect: a `hello` line with `protocol` 4, a result for id 1 with `"enabled":true` and a `schema` of `null` or 4, and an error for id 2 saying commands are accepted only on the orchestrator endpoint. (This is the link an agent shell could open: it can read, it cannot queue a command.)

## 5. Kill it and let it come back

```sh
kill -9 $(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' $SYNT_HOME/run/orch.json)
synt orchestrator --serve &
sleep 1; cat $SYNT_HOME/run/orch.json
```

Expect: the second start succeeds (the kernel released the lock), `orch.json` names a new socket and pid, and the dead process's socket file is gone.

## 6. Turn it off

Set `enabled` to `false` in `settings.json`. Expect: within a second the process logs `outcomes were turned off: stopping` and exits, and `orch.json` is removed.

## What is covered by automated tests instead

```sh
cargo test -p synthesis-app --test orch_endpoint          # key, attach, surface mismatch, create, enable, approve, cancel (unix)
cargo test -p synthesis-cli --test orchestrator_process   # real process kill and restart, two owners, recovery, notices (unix)
cargo test -p synthesis-orchestrator --test lifecycle     # start, pause (durable), resume, cancel, ack, verdict
cargo test -p synthesis-cli --features test-provider --test engine_stdio
```

## Not verified

- Windows (named pipe, native or WSL) and Linux with bubblewrap for the process tests: written, not run by the author. CI will compile them; the process and endpoint tests are unix-only.
- A worker run by the orchestrator end to end with a real model (needs the PR-13 surface to create and approve an outcome), and the live policy gateway (not built).
- The notify command through the confined `Notifier` against a real sandbox: the payload and delivery are unit tested without a sandbox.
