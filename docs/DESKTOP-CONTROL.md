# Desktop control (D1/D2)

Status: in progress on branch `feat/desktop-control`. Full design, threat model and the D0
checklist are written when the code is complete (see Progress).

## Progress

- Done: `synthesis-permissions::desktop` (policy: origin gate, hard-deny list) and its engine
  rules; `crates/synthesis-desktop` (runtime, envelope, pin, process tree, snapshot/ref table,
  limiter) with the fake `agent-desktop` test double and tests.
- Next: settings/events, harness refusal, tools (`desktop_*`), app wiring + dialog + doctor, docs.
