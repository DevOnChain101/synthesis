# Spec: Approvals A1/A4 (risk tiers, Needs you queue, one-tap, batch, Undo, auto-decide, blocks-wN)

Status: Proposed (2026-11-26); P1 built (section 15 records where the code differs from the first draft). Implementation spec for backlog item F ("Approvals A1/A4") and the owner direction of 2026-11-26 ("allow several approval mechanisms").
Builds on `docs/specs/approvals-concept.md` (the concept; its tier idea, threat list and classifier rules stay) and replaces its delivery rows A1, A4 (the mechanism part) and A6 with the slices of section 12. The concept's A2 (brief fields), A3 (non-blocking asks inside an attempt), A4-provisional (T1 `assumption_review` with checkpoints) and A7 (deferred asks in own sessions) are NOT in this spec; section 14 says how they plug in later.
Owner principle: HANDOFF 7bl (2026-11-18); owner Q1 2026-11-18 (HANDOFF 7bq); home decision D10 2026-11-21 (`super-agent-s1.md` 5.5); 2026-11-26 "several mechanisms".
Related: ADR-016, ADR-019, ADR-021 (protocol, `needs_you`, brief rules), `docs/specs/super-agent-s1.md` 5.5 and 6, `docs/specs/permissions.md`, `docs/specs/conductor.md` 3.2, `docs/specs/config.md` 6.1, `docs/specs/events.md` 7.

Markers: **[E]** exists in code (path:line from main 5b2328a6), **[N]** new here, **[A]** assumption, **[V]** verify while implementing (I read around it but did not prove it).

---

## 0. One-line rules

1. **Tiers decide attention, never authority** (concept rule 0). No tier, policy, batch or Undo creates a grant, widens a scope, raises a budget or answers a T2/T3 item.
2. **The tier and the recommended answer are computed by the owner's code from stored typed facts.** Never by the model, never by a surface, never from brief text. An agent proposal can only raise a tier.
3. **Undo cancels an answer before its effect; it never reverses an effect.** An answer from Super Agent is held a few seconds by the owner; inside that window Undo wins, after it Undo says "too late" and changes nothing.

---

## 1. Goals, non-goals, requirements

### 1.1 Goals (owner principle mapped)

| Principle (7bl / 2026-11-26) | Here |
|---|---|
| (a) do not block the agent | A pending decision already parks only its work item [E `sched.rs:446-470`, ready items keep leasing]; P6 makes the scheduler prefer branches that unblock more work and names what blocks what |
| (b) do not flood the person | One queue, sorted by tier then impact; T3 never folded; batch for low-risk rows; notifications unchanged (one per decision revision, ADR-021 B2) |
| (c) easy to accept | One-tap on a store-recommended answer (T1), batch "accept recommended" (T1), Undo on every Super Agent answer |
| (d) the agent decides small things, recorded | Auto-decide by explicit user/managed policy for T1 kinds only, default OFF, chained record, visible in "Answered, recorded" |
| (e) never lost | The queue is derived from store rows (decisions, contracts pending, expired results); a restart loses nothing; snooze never hides a row |
| (f) blocked branch: reprioritise, show blocked-by | `blocks` on every queue row, `blocked_by` on every work item, lease order by downstream impact (P6) |
| Several mechanisms (2026-11-26) | Card (existing), one-tap, batch, Undo, auto-decide, snooze |

### 1.2 Non-goals

- No change to the TUI view (`apps/desktop/src/tui_view/**`): owner direction "TUI unchanged"; its dialogs keep blocking; `design_tests` stay byte-identical.
- No one-tap, batch, auto or Undo on live tool-permission asks (sessions, orchestrator chat): the agent's turn waits on that answer; delaying or bulking it adds risk and latency. They get a tier chip and a blocked-by line only.
- No change to what a T2/T3 card asks (owner Q1: an agent suggestion on T2 is a label, never preselected, choose then Send; nothing on T3).
- No new database schema (no DDL, `user_version` 5 and marker 1010 unchanged, section 5).
- No LLM judge for tiers (ADR-019). No multi-person approval. No spot audit or weekly review (concept 7.3): not asked for now.
- Not here: non-blocking asks inside one attempt (concept A3), `assumption_review` T1 (concept A4-provisional, needs V1a caveats), read-only folder consent (owner question open, section 13 Q3), conductor `message_worker` card (S1 C3 slice 3).

### 1.3 Requirements (each has a test in section 11)

| # | Requirement |
|---|---|
| R1 | Every `pending` outcome decision, every outcome in `contract_pending`, and every decision that expired or was invalidated in the last 24 h appears in `approvals.queue` until answered or aged out; owner and desktop restarts lose none (the queue is a read of store rows). |
| R2 | Every queue row carries `tier`, `kind`, `created_at` (server time), `expires_at`, `blocks` (work items) and `waiting_on_it` (transitive dependents in the current plan). `plan.get` and `work_item.get` items carry `blocked_by` (open decision ids). |
| R3 | `tier` is the output of one pure function `synthesis_orchestrator::tier::classify`; an unknown kind is T3; no input lowers it below the kind's floor; a hint can only raise it (property test). |
| R4 | Ordering is tier (T3 first), then `waiting_on_it + 1` descending, then age; T3 rows are never folded behind "Show more"; counts per tier and per outcome are shown. |
| R5 | One-tap exists only where the server sets `one_tap: true` (tier <= T1, a server `recommended` exists, `synt.approvals.one_tap` on). The owner re-checks at apply: `mechanism: one_tap` with a choice other than its own recommendation, or a tier above T1, is `denied`. |
| R6 | Batch (`decision.resolve_many`) accepts 1..=`batch_max` items (default 10, ceiling 20), every item T1 with `choice == recommended` at apply time; otherwise the whole command is `denied`; an item that went stale is reported per item while the rest apply. |
| R7 | An answer sent from Super Agent (card, one-tap, batch, contract approval) is held `undo_seconds` (default 10, 0..=30) from the command's server `created_at`. `command.revert` succeeds iff the target is still queued; it never changes an applied effect (`too_late`). |
| R8 | Auto-decide is off by default; it applies only to kinds in the code constant `AUTO_ALLOWED` (T1) that the user or managed `synt.approvals.auto` lists; a project file never counts; each auto-decision writes a chained `approval.auto_decided` event with the policy hash and never writes a `decision_approvals` row. |
| R9 | Every person answer's chained `decision.resolved` event carries `audit {mechanism, tier, classifier_version, recommended, batch_id, held_until}`. |
| R10 | A pending decision parks only its work item; with P6 the lease order prefers ready items with more downstream items; a blocked outcome's reason names the decision ids and tiers that hold it. |
| R11 | No new colours or tokens; new Super Agent elements use `superagent/kit.rs` parts; the TUI view and its shots are unchanged. |
| R12 | Old databases, old clients and old engines keep working: PROTOCOL stays 5, each slice adds a `features` string, a client uses a mechanism only when the server lists it. |
| R13 | Nothing in this spec makes an answer without a person's per-item act, except R8's T1 auto-decisions, which grant nothing and stay within the contract and its retry budget. |

---

## 2. Options considered

| Option | Data flow | Failure modes | Security | Operability | Cost signal |
|---|---|---|---|---|---|
| **O1. Server-derived queue + held commands + chained records (chosen)** | Tier and recommendation computed on read (`approvals.queue`) and again at apply; answers stay `outcome_commands` rows; Undo = cancel a held queued row; records = chained events | A held row waits for an owner tick (500 ms, `orch_owner.rs:60`); audit is per outcome chain | Reuses CAS, binding, approval MAC, gateway, single writer; no new authority path | `command.status`, `approvals.history`, `orch.log` codes | S per slice, no migration |
| O2. Store tier and recommendation in the brief (concept A2) | Classifier at decision creation, inside `brief_sha256` | Old open decisions lack fields; every creator must call it; `DecisionCard::from_json` refuses `recommended`/`default` in a brief (`synthesis-view/src/outcomes.rs:98-104`), so an old surface would refuse every new card | Strong binding, but fights the PRESELECT rule | Same | M |
| O3. Side table `approval_records` created like `trusted_roots` (`synthesis-session/src/trust.rs:3`, `CREATE TABLE IF NOT EXISTS`, no version bump) | One MAC'd append-only row per decision | New triggers, MAC domain, backup and doctor rules | Strong | Easy cross-outcome queries | M |
| O4. Undo by reversing applied decisions | Revoke grants, re-open decisions | Fights `decisions_terminal`, `decision_approvals_no_update` (`schema.rs:222-257`); races with leasing | Weakens "a finished decision does not change" | Hard | L |

**Choice: O1.** Smallest change that meets every requirement; the hardened decision path stays the only way an answer lands.

**Strongest reason against O1:** the record of who decided what is spread over each outcome's control-session chain, so "Answered, recorded" across many outcomes costs one bounded read per outcome, and a classifier change (new `CLASSIFIER_VERSION`) can show a different tier for an old open row than the one recorded when it was answered. Mitigation: the applied tier is recorded in the event (R9); revisit (O3) if more than ~20 open outcomes make the history read slow.

---

## 3. Inventory and risk tiers

### 3.1 What exists today

| # | Kind | Lives in | How it is answered today | Durable? |
|---|---|---|---|---|
| 1 | Decision `blocked` (worker ended blocked; plan-mode exit) | `synthesis-orchestrator/src/decision.rs:359-397` (`record_blocked_decision`, alternatives `[retry]`) | Card: choose, Tab for full brief, Enter (`apps/desktop/src/approvals.rs:134-243` `Popup`) | yes, `decisions` |
| 2 | Decision `approval_needed`, grantable (`grant_once`) | `decision.rs:366-389`; grant from stored action (`grants.rs`) | Card | yes |
| 3 | Decision `approval_needed`, not grantable (only `retry`) | same | Card | yes |
| 4 | Decision `ambiguity` (worker's AskUserQuestion) | `decision.rs:344-358` | Card; options are the worker's text | yes |
| 5 | Decision `plan_approval` (model plan, stub fallback, replan) | `model_plan.rs:461`, `planrun.rs`, `grants.rs:297` | Card | yes |
| 6 | Decision `report_review` (read-only goal) | `report.rs:392-420` | Card (`accept_report`) | yes |
| 7 | Contract approval (`outcome.contract.approve`, outcome `contract_pending`) | `superagent/goal/review.rs:290-345` dialog (`approve_ok` arming, `goal/model.rs:90`), CLI `synt outcome approve` | Dialog: version and hash shown, deliberate Approve | yes, `contracts.approved_mac` |
| 8 | Contract revision (`edit_scope` follow-up; diff `widens`) | `outcome_service.rs:155-174`, `commands.rs:9-13` | Same dialog with WARNING blocks | yes |
| 9 | Tool permission ask, Super Agent own/hosted session | `apps/desktop/src/live.rs:49` `DialogKind::Approval`, `synthesis-app/src/asks.rs:34-70` (`Once/Always/Deny`, dialog `permission` or `outside_read`) | In-session dialog; Needs you row "Open" | live only; history in v4 `approvals` table (`schema.rs:57`) |
| 10 | Orchestrator chat permission ask | `superagent/chat/asks.rs` (one FIFO queue, modal + card + Needs you, `MAX_ASKS` 32, `ARM`, Esc = later) | Buttons | live only |
| 11 | Conductor proposal cards (`send_message`, `interrupt`, `create_session`, `close`, `message_worker`) | `synthesis-app/src/conductor/{proposal,config}.rs`, `superagent/chat/cards.rs`; `Confirm::Auto` only marks `auto_eligible` (`conductor/config.rs:36-38`) | Card click (`PersonClick`) | proposal expiry |
| 12 | Run in terminal (person's own action on a chat code block) | `tui_view/run_block.rs:1-4`, `widgets/codeblock.rs` Gate (#241: always a click; risky/multiline/tainted a second click) | Click | no |
| 13 | Browser payment/consent | `BrowserHost::confirm_payment` (terminal-browser.md) | Host dialog, one-shot | no |
| 14 | Project trust, MCP approval, permission-mode raise, orchestrator enable | desktop dialogs, `orchestrator.enable` disclosure | Dialog | settings / store |
| 15 | Verification waiver / integrity ack | not built (concept row 13, V1c) | - | - |
| 16 | Read-only folder consent | not built (owner question open, HANDOFF 7bu) | - | - |
| 17 | `assumption_review` (agent default, provisional) | not built (concept A4) | - | - |

The current Needs you (`superagent/home/needs.rs`, `home/data.rs`) has one row per outcome with decisions pending plus one per asking session, oldest first by first-seen time (`approvals.rs:6-7`: "the server sends no creation time"; it does send `created_at` in `decision_json`, `outcome_service.rs:557-559`, which this spec uses). `Gates` (`home/data.rs:16-29`) already reserves `one_tap`, `batch`, `undo` (all false), `NeedRow.blocks`/`risk`/`recommended` are `None` ("until A1").

### 3.2 Tier definitions

| Tier | Name | Meaning | Mechanisms allowed |
|---|---|---|---|
| **T0** | Info | Nothing to decide: an auto-decision record, an expired decision (HOLD happened), an invalidated one (re-asked) | Shown in "Answered, recorded" / "Lapsed"; dismiss only |
| **T1** | Low | The answer stays inside the approved contract and policy: no grant, no scope, plan, bar or money change; worst case is spending turns within the retry budget | Card, one-tap, batch, Undo, auto-decide by explicit policy |
| **T2** | Review | Needs a person's informed choice: a permission outside the contract, a design or plan choice, accepting a report, a live tool ask | Card only (nothing preselected, choose then Send, brief expanded once); Undo when sent from Super Agent; owner Q1 label on `ambiguity` when built |
| **T3** | Confirm | Security, money, scope widening, destructive or irreversible, trust | Its existing deliberate dialog; Undo while held; never one-tap, batch or auto |

Deviation from the brief I was given: it described T2 as "needs one tap". One-tap needs a recommendation the owner can stand behind; for every T2 kind today the only candidate is the agent's own suggestion, which the owner ruled must never be preselected (Q1, 2026-11-18). So one-tap is T1 only (the structure would allow `one_tap_max_tier = T2` later; it is a constant, not a setting, until the owner asks). Section 13 Q1.

### 3.3 Tier and recommendation per kind (the classifier table)

| # | Kind | Floor | Raised to | Recommended (server) | Notes |
|---|---|---|---|---|---|
| 1 | `blocked` | T1 | T2 if the item's `retries_used` already equals the contract's retry budget (a retry would be refused) [V store rule `retry budget`, `runner.rs:279-286`] | `retry` when it is one of the brief's alternatives | The only T1 decision kind today |
| 2 | `approval_needed` grantable | T2 | T3 if any stored call (`action.calls[].tool,input`) hits `synthesis_permissions::irreversible::scan`/`text_hits` (`irreversible.rs:1057,1081`), names a secret-shaped path (`synthesis_core::secrets::is_secret_shaped`), or any `needed` rule is a network tool [V exact API] | none | a grant widens the contract for one attempt |
| 3 | `approval_needed` not grantable | T2 | as 2 | none (a retry meets the same refusal) | |
| 4 | `ambiguity` | T2 | - | none (Q1 label only, not one-tap) | worker free text |
| 5 | `plan_approval` | T2 | - | none | design authority |
| 6 | `report_review` | T2 | - | none | the bar for done |
| 7 | Contract approval (pending) | T3 | - | none | queue row opens the goal review |
| 8 | Contract revision | T3 | - | none | the diff's `widens` is shown as today |
| 9-10 | Live tool asks | T2 (fixed, desktop constant) | - | none | not server rows; merged by the desktop |
| 11 | Conductor proposals | unchanged (conductor.md 3.2) | - | - | not queue rows (they live in the chat), counted as today |
| 12-14 | Run, payment, trust/MCP/mode/enable | unchanged dialogs | - | - | not queue rows |
| 15, 16 | Waiver, folder consent | T3 when built | | none | |
| 17 | `assumption_review` | T1 when built | | the stored default | plugs into one-tap/batch unchanged |
| - | Any other decision kind | T3 | | none | fail closed: an unknown kind gets the highest tier |
| - | Expired / invalidated (last 24 h) | T0 | | - | "Lapsed" rows |

Rules:
- `classify(kind, action, item) -> Tier` and `recommended(kind, brief, tier) -> Option<(AltId, label)>` live in `crates/synthesis-orchestrator/src/tier.rs` [N], pure, with `CLASSIFIER_VERSION: u32 = 1`. They read typed fields only: `kind`, the brief's `alternatives[].id`, the stored `action` JSON, the item's `retries_used` and the contract's retry budget. Labels, `why`, `delta` and every other free text are never read.
- `tier = max(floor(kind), raises(action, item))`. No tier or hint an agent wrote is read; if a `tier_hint` field is ever added it may only participate through `max`.
- `recommended` is `None` unless `tier <= T1`. It must be an id the brief offers (`offers` rule, `outcome.rs:4081`).
- The function runs twice: in `outcome_service` for `approvals.queue` (what the surface shows) and in `commands.rs` when the owner applies (what counts). A surface never computes either (ADR-021 dependency rule, `surface_deps.rs`).
- Unit table test per row; property test: for random briefs, actions and hints, `classify >= floor(kind)` and `recommended.is_some() => tier <= T1`.

---

## 4. Mechanisms

### 4.1 Card (existing, unchanged)

The #167 rules stay for every tier: nothing preselected, choose then Send, the whole brief shown once before Send (`Popup::send_blocked`), Enter inert for `ARM_MS` after the card takes the keys, a new revision or hash re-arms and clears the choice. P2 adds only the one-tap button beside it and P3 the `hold_ms` of 4.3 to what it sends.

### 4.2 One-tap

- Shown on a Needs you row only when the server row says `one_tap: true` (R5). Label: the recommended alternative's cleaned label from the brief, else its id ("Run again" for `retry`); a `keycap("A")` beside it. Nothing else on the row changes.
- Activation: a real mouse click (`superagent/chat/asks.rs:311` `real()`: mouse, standard, one click), or `a` on the keyboard cursor row when that row has been the cursor for at least `ARM_MS`, the key is not held, and no card is open. Keyboard activation of a focused button follows `approve_ok` (`goal/model.rs:90`).
- Sends `decision.resolve {choice: recommended, mechanism: "one_tap", expected_revision, brief_sha256, hold_ms}` with the revision and hash from the same queue row. The row's brief was not necessarily read: that is why one-tap is T1 only.
- Owner check at apply (`commands.rs::resolve`): recompute tier and recommendation from the stored row; `mechanism == one_tap` and (`tier > T1` or `choice != recommended` or `synt.approvals.one_tap == false`) => `denied` with code `not_one_tap`.

### 4.3 Undo (hold window)

| Step | Rule |
|---|---|
| Send | A Super Agent answer carries `hold_ms = undo_seconds * 1000` (0..=30000; the server refuses other values with `hold_out_of_range`). The CLI sends none (0): scripts keep today's behaviour. |
| Hold | The owner skips a queued `decision.resolve`, `decision.resolve_many` or `outcome.contract.approve` while `now < created_at + hold_ms` (`created_at` is the server's insert time, `outcome.rs:1404-1416`; a client cannot backdate it). It stays `queued`; `command.status` says `queued` with `held_until`. |
| Revert | `command.revert {target_key}` is its own command. In every owner cycle `apply_queued_with` applies reverts BEFORE any other command (after the decision driver). A revert finds the target: still `queued` => in one transaction the target is finished `denied` with result `{"code": "reverted", "revert_key": K}` and the revert `applied`; already finished => the revert is `stale` with `{"code": "too_late", "target_state": S}`; already reverted => `already_resolved`. |
| Apply | When the hold has passed and no revert is queued for it, the target applies exactly as today (driver first: an expiry during the hold makes it `stale`; a moved binding during the hold makes it `stale`). |
| Late Undo | A revert that arrives after `held_until` but before the owner applied the target still wins (the effect has not happened). The window shown is a guide; the server's answer is the truth. |

What Undo means per kind (an applied effect is never reversed):

| Kind | While held | After apply |
|---|---|---|
| `blocked` -> `retry` | Undo: nothing happened | The item is `ready`, may lease at the next tick: not revertible; pause or cancel the outcome |
| `approval_needed` -> `grant_once` | Undo: no grant | A one-shot grant exists; not revertible here (the existing invalidation still revokes it if the binding moves) |
| `ambiguity` answer | Undo | The answer is in the next attempt's prompt: not revertible |
| `plan_approval` | Undo | The plan is installed: not revertible (replan or cancel) |
| `report_review` accept | Undo | Evidence written (immutable): not revertible |
| `reject` / `request_evidence` / `edit_scope` (any kind) | Undo | Final (rejected), re-brief started, or scope edit opened: not revertible |
| Contract approval | Undo: the contract stays unapproved | `contracts_approval_once` (`schema.rs:198-200`): not revertible; the outcome does not run before `outcome.start`, so the recourse is not to start, or cancel |
| Batch | Undo reverts the whole command | Per item as above |
| Auto-decision (4.5) | no Undo | Retry within budget; recourse: pause, cancel, turn the policy off |
| Live tool asks | no hold, no Undo | The agent waits on the answer |

Rejected: Undo after apply by revoking an unconsumed grant. It would leave an `approved` decision whose grant is gone, and races the lease at the next tick; hold-only is exact and needs no state move the store forbids.

### 4.4 Batch ("accept recommended")

- Offered when at least 2 rows have `batchable: true` (server: `one_tap` and `synt.approvals.batch_max > 0`). Bar above the Needs you rows: "N low-risk answers recommended" and a ghost button "Review N". T2/T3 rows are never in it and never counted in it.
- "Review N" opens the digest card in place of the bar: one line per item (outcome goal, kind, `blocks w3`, the recommended label), each with a checkbox ticked (per-item exceptions: untick to leave it in the queue), a summary line "Accept K of N: K x Run again in M outcomes", and the primary button "Accept K" armed `ARM_MS` after the digest is drawn (real click, or Enter on the focused button per `approve_ok`). No plain-key shortcut accepts a batch.
- Sending: one `decision.resolve_many` per outcome (commands are per outcome, `outcome_commands.outcome_id`), all with the same `batch_id` [N, random 128-bit hex] and `hold_ms`. Items: `{id, expected_revision, brief_sha256, choice}` from the digest's rows. Max `batch_max` items in the whole batch (more than the cap is refused by the surface before sending and by the owner on apply).
- Owner apply: (1) validate every item against the stored row: distinct ids, decision of this outcome, `pending`, tier recomputed `<= T1`, `choice == recommended`, count <= `batch_max` (setting) <= `BATCH_CEILING` = 20 (constant). Any failure of these => the whole command `denied` with code `batch_tier` / `batch_not_recommended` / `batch_too_large` / `batch_duplicate`; nothing applied. (2) Then each item goes through the existing `resolve` path in its own transaction (`commands.rs:147-210`), collecting `{id, state}` (`applied`, `stale`, `already_resolved`). (3) The command finishes `applied` with `{"items": [...], "batch_id": B}`. A crash between items leaves the command queued; the replay sees `already_resolved` for done items and applies the rest (idempotent, no second effect).
- Undo of a batch: the surface sends `command.revert` for each per-outcome command of the batch (same `batch_id`), each answered on its own.

### 4.5 Auto-decide with recorded decision

- Policy `synt.approvals.auto` (section 9): a list of kinds; only kinds in the code constant `AUTO_ALLOWED = ["blocked"]` (T1) are honoured; default `[]`. Read from user and managed settings only; managed wins key by key; project, local and `--settings` files are ignored with a diagnostic (pattern of `conductor/config.rs:57-75`).
- Where: `runner.rs:672-688` (a `Blocked` end). When the kind that `record_blocked_decision` would create is listed, the classifier says T1, and the item's retry budget is not used up, the owner does NOT create a decision. It settles the item as a retryable failure (`settle_item` path `RetryableFailure`, `runner.rs:279-286`), so the existing retry with backoff and the contract's retry budget bound it, and writes in the same transaction the chained event `approval.auto_decided` (5.2). Otherwise the decision is created as today.
- This never writes `decision_approvals` and never moves a decision to `approved` (the "only a person resolves" guard, `state.rs:276-359`, stays untouched).
- Visible: "Answered, recorded" shows "Super-Agent ran w3 again (your setting)" with the time and the policy hash prefix; `synt outcome queue --history` lists it.

### 4.6 Snooze and later

- Esc on a card = later (existing). New: "Snooze 1 h" on the card footer: the row moves to the end of its tier group, shows "snoozed until 14:30", still counts in every badge and count, and is never folded if T3. Maximum: 1 h for T1/T2, 4 h for T3, never past `expires_at` (concept 7.4 numbers). An expired snooze reverts silently.
- Stored per surface in `desktop-state.json` (`apps/desktop/src/last_session.rs` writer) as `approvals.snoozed: {decision_id: until_ms}`, pruned when the decision is no longer in the queue. A lost write only shows the row again (fail visible). No server state: the queue itself is the durable part.

### 4.7 Non-blocking and blocks-wN

- Today: a decision parks its work item in `waiting_decision` (`runner.rs:274`); the scheduler keeps leasing other `ready` items (`sched.rs:446-470`), so other branches already continue; an attempt that hits an ask ends (concept A3 would change that; not here).
- `blocks` of a decision = `binding.work_item_id` (`grants.rs:41-59`, every non-plan decision has it); for `plan_approval` and contract rows: every item of the current plan (they hold the whole outcome). `waiting_on_it` = items of the current plan that transitively require a blocked item (reverse of `work_item_deps`, at most 1000 items, linear walk). `blocked_by` on an item = ids of open decisions whose `blocks` contains it.
- Shown: on the queue row "blocks w3 · 2 waiting on it"; on the outcome frame's work item rows and the home Running row note "w3 waits for your decision" (existing note slot, `home/data.rs:185-196`).
- P6 lease order (`sched.rs:269` `pick_in_order` and its caller at `:456`): among `ready` items, larger `downstream(item)` (count of items that transitively require it) first, then key (deterministic, today's order as the tie-break). Effect: while w3 waits, the slot goes to the ready item that unblocks the most other work. No starvation: every ready item is leased when slots allow; the order only changes who goes first.
- P6 blocked reason (`sched.rs:685-760` `stop_of`): when nothing can run and decisions hold items, the reason names them: "no work item can run: w3 waits for decision D (T2), w5 requires w3" (ids and tier words only, no brief text).

### 4.8 Data flow and failure modes

```
 Super Agent home                 owner endpoint (outcome_service)        owner loop (orch_owner, 500 ms)
 ----------------                 --------------------------------        --------------------------------
 approvals.queue  ───────────────► read store rows, tier::classify ──┐
 rows (tier, recommended, blocks) ◄──────────────────────────────────┘
 one-tap / card / digest
   decision.resolve{hold_ms} ─────► enqueue_command (created_at = now) ──► driver.drive (expiry, invalidation)
   decision.resolve_many ─────────►   (outcome_commands, queued)          reverts first
 Undo toast                                                               skip held (now < created_at+hold)
   command.revert{target_key} ────► enqueue_command ─────────────────────► re-classify, CAS, binding, MAC
 command.status / decision hint ◄──────────────────────────────────────── decision.resolved{audit} event
```

| Failure | Result |
|---|---|
| Owner not running when the answer is queued | The row waits `queued`; the owner applies it when it starts (the hold is already over); a revert queued in the meantime still wins (applied first) |
| Owner crash during the hold or mid-batch | The command stays queued; on restart the hold is recomputed from `created_at`; batch replay as 4.4 |
| Link lost after sending | The surface keeps the idempotency key and polls `command.status` on reconnect; the toast says "Sent, waiting for Super-Agent" until it knows; Undo still sends `command.revert` with a new key |
| Decision expires or its binding moves during the hold | Driver runs first: the answer is `stale`; the toast shows the server's code |
| Surface and owner disagree on tier (stale queue read, classifier upgrade) | The owner's apply-time tier wins: one-tap or batch is `denied`; the surface refetches the queue |
| Desktop state file write lost | A snoozed row shows again |
| Old owner without the feature strings | No mechanism is offered (6.5) |

HA/DR: single owner per outcome by design (ADR-016/017); nothing here adds a second writer. Everything durable is in `synt.db` (backups and `pre-v5` copies as today); the queue and the hold are recomputed from it.

---

## 5. Data model and persistence

### 5.1 No DDL

Nothing is added to `schema.rs`. Old databases (v4, v5 rev 1010) work unchanged; an older binary reads the same store and sees only extra event types it already treats as `Record::Unknown` (events.md 7).

| Need | Where it lives |
|---|---|
| The queue | Derived on read from `decisions` (pending, and expired/invalidated within 24 h), `outcomes.state = contract_pending`, `work_items`, `work_item_deps` |
| A person's answer | `outcome_commands` row (params gain `hold_ms`, `mechanism`; `resolve_many` params `items[]`, `batch_id`), then `decision_approvals` row with its MAC and the state change, as today |
| The decision record | Chained events in the outcome's control session (5.2), written in the same transaction as the effect |
| Undo | The target command finished `denied` with `{"code":"reverted","revert_key"}` (a state the CHECK already allows, `schema.rs:174`) plus the `approval.reverted` event |
| Auto-decision | `approval.auto_decided` event only (no decision row, no approval row) |
| Snooze | `desktop-state.json` (surface preference) |

### 5.2 Decision record (event payloads; ids, codes and hashes only, never brief text)

| Field | `decision.resolved` (existing event, additive `audit` object) | `approval.reverted` [N] | `approval.auto_decided` [N] |
|---|---|---|---|
| id | event seq in the control session; `decision_id` | `revert_key`, `target_key` | event seq |
| kind | decision kind (existing payload has `decision_id`, `choice`, `approver`, `brief_sha256`, `binding_hash`, `revision`, `state`, `outcome.rs:4150-4155`) | target command kind | would-be decision kind |
| tier | `audit.tier` (`T0`..`T3`) + `audit.classifier_version` | `tiers[]` of the target's items | `tier`, `classifier_version` |
| item hash | `brief_sha256` (existing) | `items[{decision_id, brief_sha256}]` | `facts_sha256` = sha256 of `(work_item_id, attempt_ordinal, attempt_session, reason code)` |
| default | `audit.recommended` (null when none) | - | `choice` (`retry`) |
| decided_by | `approver` (existing, `user:<surface>` from the command, `outcome.rs:1362-1369`) + `audit.mechanism` (`card`, `one_tap`, `batch`, `cli`) | `by` = `user:<surface>` of the revert | `policy:<sha256 of the effective synt.approvals object, 12 hex>` |
| decided_at | the approval row's `decided_at` (MAC'd) | applied time | event time |
| undo_until | `audit.held_until` (`created_at + hold_ms`, null when 0) | - | null |
| revert_of | - | `target_key` (the reverted command's idempotency key) | - |
| batch | `audit.batch_id` | `batch_id` | - |

Binding to the existing integrity:
- The approval MAC (`approval_mac`, `outcome.rs:1079-1092`) is unchanged and still covers decision, outcome, kind, revision, binding hash, expiry, brief hash, approver, choice and time; one-tap and batch items each get their own row and MAC through the same `resolve_decision`.
- The audit object is inside the chained event (HMAC chain under the chain key), so it is tamper-evident like every other event; it is not inside the approval MAC (mechanism is metadata, not authority).
- Fencing: a held answer is checked against `current_binding_for` (`grants.rs:92-132`) and expiry when it applies, not when it was sent; an attempt fence or contract move during the hold makes it `stale`.
- `Resolution` (`outcome.rs`, from `Resolution::from_command`) gains `audit: serde_json::Value` (default `Null`, set by `commands.rs` after classifying); `resolve_decision` copies it into the event. No other caller changes.

### 5.3 Idempotency

| Call | Key | Replay |
|---|---|---|
| `decision.resolve` | `idempotency_key` per outcome (existing `UNIQUE(outcome_id, idempotency_key)`) | same row returned; one effect (`apply_command` returns a finished row as stored, `outcome.rs:2113-2143`) |
| `decision.resolve_many` | same | finished: stored result; crash mid-batch: done items are `already_resolved`, rest apply |
| `command.revert` | same; params `{target_key}` | finished: stored result; a second revert key on the same target: `already_resolved` |
| Hold | none needed | the hold is a function of the stored row (`created_at`, `hold_ms`) |

---

## 6. Protocol and IPC

PROTOCOL stays 5 (`proto.rs:22`, exact match). Everything is additive and announced in `orchestrator.status.features` (`proto.rs:32`). A surface uses a mechanism only when its feature string is present; otherwise `Gates` stay false and today's UI shows (R12).

### 6.1 Reads (served by the owner endpoint and the read-only stdio engine, refused while `synt.orchestrator.enabled` is false)

`approvals.queue {workspace?}` [N, feature `approvals_queue`, P1] ->
```json
{"classifier_version": 1,
 "policy": {"one_tap": true, "batch_max": 10, "undo_seconds": 10},
 "counts": {"t0": 0, "t1": 2, "t2": 3, "t3": 1},
 "per_outcome": [{"outcome": "o1", "t1": 1, "t2": 2, "t3": 0, "oldest": "2026-11-26T09:00:00Z"}],
 "rows": [{"outcome": "o1", "goal": "...", "source": "decision|contract|lapsed",
           "decision_id": "d1", "kind": "blocked", "state": "pending", "tier": "T1",
           "revision": 2, "brief_sha256": "<64 hex>", "created_at": "...", "expires_at": "...",
           "recommended": {"id": "retry", "label": "run the work item again"},
           "one_tap": true, "batchable": true,
           "blocks": [{"id": "<work item id>", "key": "w3"}], "waiting_on_it": 2}]}
```
Rows are server-built and ordered (R4); at most 1000; strings cleaned and capped like the other view models. `goal` and `label` are cleaned text (shown as data, never as instructions). A contract row has `source: contract`, `version`, `hash`, no `decision_id`. `policy` is the effective setting the owner applies (so a surface never reads `synt.approvals` itself).

`approvals.history {outcome, after_event_id}` [N, P1] -> `{"records": [{"event_id", "ts", "type", "payload"}], "next_event_id": N}`: the control session's `decision.resolved`, `approval.reverted`, `approval.auto_decided` events after the cursor, 200 per page (one call reads at most 20000 events), payloads as 5.2. `next_event_id` is the last event read: a cursor that did not move is the end.

`plan.get`, `work_item.get`: each item gains `blocked_by: [decision ids]` [N, P1]. `outcome.get` gains `repo_root` [N, P2] (for the approve dialog's Folder row, 7.4).

### 6.2 Commands (owner endpoint only)

The stdio engine refuses them before the store is touched, like `decision.resolve`: `Service::read_only` sets `may_queue: false` (`outcome_service.rs:94-97`), so anything sent through `self.queue` is refused there.

| Call | Params | Feature | Slice |
|---|---|---|---|
| `decision.resolve` (existing) | + `mechanism?: "card"|"one_tap"` (default `card`; the CLI sends `cli`), + `hold_ms?` (0..=30000) | `approvals_one_tap` (mechanism), `approvals_undo` (hold) | P2, P3 |
| `outcome.contract.approve` (existing) | + `hold_ms?` | `approvals_undo` | P3 |
| `decision.resolve_many` [N] | `{outcome, items: [{id, expected_revision, brief_sha256, choice}], batch_id, hold_ms?, idempotency_key, surface}` | `approvals_batch` | P4 |
| `command.revert` [N] | `{outcome, target_key, idempotency_key, surface}`; target kinds: `decision.resolve`, `decision.resolve_many`, `outcome.contract.approve` | `approvals_undo` | P3 |

Code to touch for each new call: `Call` variant (`proto.rs`), `Call::is_outcome()` (`:474-503`), `Call::surface()` (`:507-523`), `Call::action()` (`:528-555`, exhaustive on purpose), `Service::handle` (`outcome_service.rs:379-428` pattern: validate cheaply, then `self.queue(...)`), `Store::enqueue_command` validation (`outcome.rs:1395-1397` pattern for `decision.resolve`), gateway `Action::ResolveMany` (`decision.resolve_many`) and `Action::Revert` (`command.revert`), both `by_person` (`gateway.rs:42-112`; `ALL` grows to 16), `apply_queued_with` known kinds (`commands.rs:46-52`), owner `KINDS` (`orch_owner.rs:68-77`), `OutcomeClient` typed calls (`outcome_client.rs`).

### 6.3 Results and codes

Results keep the five states (`applied | stale | already_resolved | denied | queued`). Codes (fixed strings in `result.code`, never request text):

| Code | State | When |
|---|---|---|
| `held` | queued | inside the hold (`held_until` in the result of `command.status`) |
| `reverted` | denied (target) | Undo won |
| `too_late` | stale (revert) | the target was already applied, stale or denied |
| `not_revertible` | denied (revert) | target kind not in the list, or another outcome's key |
| `hold_out_of_range` | refused at the call | `hold_ms` outside 0..=30000 |
| `not_one_tap` | denied | one-tap on a row the owner does not rate T1-with-recommendation, or one-tap off |
| `batch_tier`, `batch_not_recommended`, `batch_too_large`, `batch_duplicate`, `batch_off` | denied (whole batch) | 4.4 |
| existing `reason` texts | as today | resolve path |

### 6.4 ADR-021 impact (amend in P2)

- Section 2: "No field marks a recommended or selected option" becomes: no field **inside the brief** does (the store and `DecisionCard::from_json` keep refusing `recommended`/`default`/... there, `outcomes.rs:97-104`, ADR-021 PR-11 notes). A server-derived `recommended` exists **outside the brief**, only on T1 queue rows, computed by the owner's classifier from typed fields, re-checked at apply. It is shown as a separate one-tap button; the card's picker still starts with no cursor.
- Threat T-34 note: an agent cannot make a recommendation; the owner's code does, for T1 only.
- Section 4: `needs_you` unchanged; `approvals.queue` is the per-row read model.
- Hints (B2): unchanged; a surface refetches `approvals.queue` on any decision hint.

### 6.5 Compatibility

| Pair | Behaviour |
|---|---|
| New surface, old owner | features absent: no queue call (surface falls back to `outcome.list` + `decision.list`, today's rows), no one-tap, batch or Undo. A surface MUST NOT send `hold_ms`/`mechanism` without the feature: `Call` has no `deny_unknown_fields` (checked), so an old owner would ignore them and apply at once, and Undo would then fail. |
| Old surface, new owner | never calls the new methods; `decision.resolve` without `hold_ms` applies at once; extra row fields are ignored by `DecisionCard::from_json` (it reads named keys; the PRESELECT scan is on the brief only). |
| Old database | no change. |
| Older binary on a store with new events | unknown event types are kept as `Record::Unknown` and shown raw by `events.subscribe` consumers [V the CLI's event printer]. |
| WSL engine link (stdio, read-only) | reads work; commands refused as today; mutations go to the owner endpoint. |

---

## 7. UI

### 7.1 Super Agent home, Needs you (`apps/desktop/src/superagent/home/*`)

Rows become one per decision (and one per contract pending, one per asking session, one per orchestrator chat ask, as today for the last two). Layout reuses `row(RowSpec)` (`home/needs.rs:84-97`):

```
◆  Fix flaky test in sched                                   [T1 low] [Run again A] [Review]
   Decision · blocked · 12m · expires in 23h
   blocks w3 · 2 waiting on it
◆  Ship release notes                                        [T3 confirm]          [Review]
   Contract v2 waits for approval · 1h
```

| Element | Built from | Style (no new colour) |
|---|---|---|
| Risk chip | `row.tier`: text "T1 low", "T2 review", "T3 confirm" (T0 rows say "info") | `kit::composer_chip` shape (pill, `t::hover()` fill, `TEXT_STRONG`); tier is text, never colour only |
| Meta | kind word, age from `created_at`, "expires in" from `expires_at` | existing meta line (`T2`, `TEXT`) |
| blocked-by line | `blocks` + `waiting_on_it` | existing `notes` slot (`SOFT_BEIGE`) |
| One-tap | `one_tap` + `recommended.label` | `kit::button(Primary)` + `kit::keycap("A")`, in the reserved slot (`home/needs.rs:64-67`) |
| Review / Open | as today | `kit::button(Ghost)` |
| Batch bar | `batchable` rows >= 2 | `kit::list_row` with a Ghost "Review N"; the digest is `kit::card(id, false)` with checkbox rows and a Primary "Accept K" |
| Undo toast | after any held send | `kit::toast(message, Some(Ghost "Undo  U"))` (`kit.rs:407-432`, built for this), bottom of the home, shows the seconds left; then "Applied", "Undone" or the server's code in words ("Too late: it already ran") for 3 s |
| Snoozed | local snooze | meta adds "snoozed until 14:30" |
| Sections | server order | "Needs you" (T3, T2, T1 rows), then "Lapsed" (T0, collapsed, count), then "Answered, recorded" (from `approvals.history`, collapsed, last 20) |

Rules:
- `FIRST` (5) rows show, the rest behind "Show N more", except that every T3 row is always shown (R4).
- The section header count is all rows needing an answer (T1-T3), never reduced by snooze; the rail badge and the title-bar number keep using `OutcomesView::waiting_count()` (`popup.rs:147-149`), which counts these rows. Per-decision rows can raise the number for an outcome with several decisions: that is the "per-outcome counts" item, intended.
- The card (`home/card.rs`) opens for the row's own decision id (today: the first resolvable card of the outcome, `home/mod.rs:233`): `Open` gains `decision_id`.
- Minimal look for Super Agent: no borders added, highlight only on rows that need the person (existing borderless rule), one primary button per row at most.

### 7.2 Keyboard

| Key | Where | Action |
|---|---|---|
| J/K, Up/Down | home | move the cursor (existing) |
| Enter | home row | open card / session (existing) |
| A | cursor row with `one_tap`, cursor there >= `ARM_MS`, not held | one-tap |
| U | while an Undo toast is shown | Undo the newest held answer |
| Alt+A | anywhere in Super Agent | next waiting row (existing) |
| Esc | card | later (existing); digest: close without sending |
| Tab / Enter | digest | focus "Accept K", Enter on the focused, armed button |

No key accepts a batch without the digest; no key acts on a T2/T3 row except opening its card. `a` and `u` join the pure `act()` map (`home/mod.rs:81-90`) so they are unit-tested like J/K.

### 7.3 Accessibility

Tier, blocked-by, snooze and hold state are text. Every button has a `debug_selector` id (`home-one-tap`, `home-batch`, `home-digest-accept`, `home-undo`, `home-risk`) used by shots and tests. Focus order: row, one-tap, Review. The toast does not steal focus. Motion: the toast countdown is text, no animation.

### 7.4 Approve dialog budget row (backlog F)

The dialog already shows "Turns" from the stored contract and "Cost cap (legacy)" only when the stored contract carries one (`goal/review.rs:317-331`). Left over: the "Folder" row prints the typed form field (`review.rs:312-315`, `typed.workspace`), not the stored outcome. P2: show `outcome.get.repo_root` (6.1) and refuse to arm Approve while it is unknown [V whether the contract view already carries the folder]. The dialog stays T3 (its arming rules unchanged) and gains the Undo hold of 4.3 in P3.

### 7.5 TUI view and CLI

- TUI view: unchanged. It has no Needs you list (it has its red "needs you" signal and its own dialogs, `tui_view/dialog.rs:56-62`); no row, style or key is added; `design_tests` and TUI shots must stay identical (checked in every slice).
- CLI: `synt outcome queue [--workspace W] [--json] [--history OUTCOME]` [N, P1] prints rows (tier, kind, outcome, decision id, blocks, age, expires) and the exact `synt outcome resolve` line to answer; `--json` is `"schema": 1` with the server's fields. `resolve` is unchanged (no hold, `mechanism: cli`).

---

## 8. Security analysis

| # | Threat | Mitigation | Invariant | Test |
|---|---|---|---|---|
| S1 | Rubber-stamping (one-tap or batch used without reading) | Only T1 (no authority change) can be one-tapped or batched; the digest lists every item with a count in the button; arming; real click; Undo | No T2/T3 answer without its card | `ap-one-tap-refused-on-t2`, `ap-batch-refuses-t2` |
| S2 | Batch smuggling a T3 (a hostile or stale surface lists a T3 id) | Owner recomputes the tier of every item at apply; any item above T1 denies the whole command | `resolve_many` effects only T1 decisions | `ap-batch-with-one-t3-applies-nothing` |
| S3 | Race between Undo and apply | One writer; reverts applied first in each cycle; the target applies only when due and no revert queued; both in transactions | An applied effect is never reverted; a won revert means no effect | `ap-undo-race` (fake clock, revert at hold-1 ms, hold, hold+1 ms, after apply) |
| S4 | Replay of an answer or a revert | Idempotency keys, CAS on revision, brief hash, approval MAC, `apply_command` returns finished rows unchanged | One effect per key | `ap-resolve-many-replay-one-effect`, `ap-revert-replay` |
| S5 | Injected model proposes a low tier or a recommendation | The classifier reads typed fields only; `tier_hint` only raises; recommendations are the owner's; a brief with `recommended` is still refused (`outcomes.rs:98`) | `classify >= floor` | property test; `ap-inj-label-cannot-lower` (brief text "naming: safe, one tap" on an `approval_needed` stays T2/T3) |
| S6 | Spoofed recommended default (hostile engine or forged queue row) | The surface sends what it showed; the owner re-derives and denies a mismatch; view model refuses a `recommended` on a T2/T3 row (fail closed: no button) | Owner check is authoritative | `ap-forged-recommended-denied`, view-model unit test |
| S7 | Flooding pushes the real ask out of view | Order by tier; T3 never folded; per-tier counts in the header; chat asks capped at 32 (`asks.rs:29`); commands capped at 1000 per outcome; one notification per decision revision | A T3 row is always on screen when the home shows Needs you | `ap-flood-keeps-t3-visible` (200 T1 + 1 T3 rows) |
| S8 | Approval through untrusted content (repo file, web page, worker report telling the person or the conductor to "accept all") | Text is cleaned and shown as data; the conductor cannot call `decision.*` (gateway principal; `PersonClick` allowlist); batch needs a real click on the digest | No model path reaches `resolve`/`resolve_many`/`revert` | existing `inj-readme-approve-decision`, `inj-forged-card-text`; new `ap-conductor-cannot-batch` |
| S9 | Auto-decide widened by a planted settings file | Project files ignored; only `AUTO_ALLOWED` (T1) kinds; bounded by the retry budget; policy hash recorded; `synt doctor` lists it | Auto never grants, never writes an approval row | `ap-project-auto-ignored`, `ap-auto-only-t1`, `ap-auto-respects-retry-budget` |
| S10 | Hold used to delay a security-relevant refusal | A hold delays only the person's own answer; expiry and invalidation run first (driver) and win | A held answer to an expired or moved decision is `stale` | `ap-held-answer-expires-stale` |
| S11 | Revert of another surface's or outcome's command | Revert is per outcome; target must be a person command kind of that outcome | | `ap-revert-other-outcome-refused` |
| S12 | Classifier version drift | `classifier_version` in queue and records; the apply-time tier is the one recorded | | unit test |

Residuals (stated plainly, as in ADR-021 PR-12): any same-user process holding `run/orch.key` can attach as a surface and answer, batch or revert; `user:superagent` means "a key holder said so". Mechanism and audit fields are self-asserted metadata inside the chain, not proof of a person. Auto-decision makes the owner act without a person for T1 `blocked` retries only.

What the security reviewer must check (P1 classifier, P2-P5):
1. `tier.rs` table against 3.3, the property test, and that no free-text field is read.
2. Every path into `resolve_decision` goes through the owner's re-check (one-tap, batch), and `resolve_many` denies the whole command on any tier/choice failure.
3. Revert ordering and the single-transaction finish of the target; the fake-clock race tests.
4. `Call::action()`, `surface()` and `is_outcome()` classify the new calls; the stdio engine refuses them; gateway table tests extended.
5. Auto path: no decision, no approval row, retry budget enforced, event written in the same transaction, project settings ignored.
6. View model: refuses `recommended` on T2/T3 rows and on any brief; the one-tap and digest buttons need a real click or the armed focused-button rule.

---

## 9. Settings (`synt.approvals`, user or managed only)

```json
"synt": { "approvals": { "one_tap": true, "batch_max": 10, "undo_seconds": 10, "auto": [] } }
```

| Key | Type, range | Default | Effect |
|---|---|---|---|
| `one_tap` | bool | `true` | false: no one-tap anywhere |
| `batch_max` | int 0..=20 | `10` | 0: no batch; items per batch |
| `undo_seconds` | int 0..=30 | `10` | hold of every Super Agent answer; 0: no hold, no Undo |
| `auto` | list of kinds from `AUTO_ALLOWED` (`["blocked"]`) | `[]` | kinds the owner decides by itself (4.5) |

- Read by the owner (`synthesis-app/src/outcome_policy.rs`, next to `synt.orchestrator`) from user and managed settings only; managed wins key by key; project, local and `--settings` files are ignored with a diagnostic naming the key. A wrong value keeps the default with a diagnostic naming the key (never the value); in the managed file a wrong value takes the most conservative value (`one_tap: false`, `batch_max: 0`, `undo_seconds: 10`, `auto: []`), the `fail_closed` rule of `synt.orchestrator`.
- The owner sends the effective values in `approvals.queue.policy`; surfaces never read them.
- `synt doctor` prints the effective values, names any ignored key, and says "auto-decide is ON for: blocked" when set.
- Desktop Settings: one "Approvals" group with the four controls (P5); writes the user file like the other settings.

`docs/specs/config.md` 6.1, new bullet (P2 adds the first two keys, P3 `undo_seconds`, P5 `auto`):
- `synt.approvals` (USER or MANAGED settings only; project, local and `--settings` files are ignored with a diagnostic; managed wins key by key and a wrong managed value takes the conservative value): Needs you mechanisms (docs/specs/approvals.md). `one_tap` bool (default `true`), `batch_max` 0..=20 (default 10, 0 = no batch), `undo_seconds` 0..=30 (default 10, 0 = no Undo), `auto` list of kinds the owner may decide alone, only `"blocked"` is accepted (default `[]`). These change attention, never authority: T2/T3 items always need the person's own answer.

---

## 10. Operations

| Concern | What |
|---|---|
| Logging | `run/orch.log` one line per held, reverted, too-late, denied-batch and auto-decided command: codes and ids only (`approval held key=.. until=..`, `approval reverted key=..`, `approval auto kind=blocked item=..`) |
| Metrics (from events, no new infra) | answers per mechanism and tier; Undo used / too late; auto-decisions per kind; median wait per tier (`created_at` -> `decided_at`); batch size |
| Alerts | none new; notifications unchanged (ADR-021 section 4) |
| Doctor | `synt.approvals` values, ignored keys, auto ON |
| Runbook (`docs/testing/outcome-orchestrator-smoke.md`, P3) | "Undo says too late": `synt outcome queue --history O` shows the applied record; pause or cancel. "An answer did not apply": `command.status` (held / stale code). "Unexpected retries": history shows `policy:` records; set `synt.approvals.auto` to `[]`. "A row is missing": `synt outcome queue --json` (server truth) vs the home (surface). |

---

## 11. Test plan

| Layer | Test | Slice |
|---|---|---|
| Unit | `tier.rs`: one case per row of 3.3; unknown kind T3; table over every kind, action and item fact: `tier >= floor`, `recommended => tier <= T1` | P1 |
| Unit | `blocks`/`waiting_on_it`/`blocked_by` on a diamond and a chain plan; 1000-item bound | P1 |
| Unit | view model `queue.rs`: cleans text, caps, refuses `recommended` on T2/T3 (no button), keeps server order | P1 |
| Store | `resolve_decision` writes `audit` into `decision.resolved`; approval MAC unchanged (old vector test still passes) | P1 |
| Protocol golden | `approvals.queue`, `approvals.history` request/reply pairs in `crates/synthesis-app/tests/outcome_proto.rs`; stdio engine serves reads; features list per slice | P1-P5 |
| Protocol golden | `decision.resolve` with `mechanism`/`hold_ms`; `resolve_many`; `command.revert`; stdio engine refuses the three mutations; gateway table rows | P2-P4 |
| Owner | `ap-one-tap-refused-on-t2`, `ap-forged-recommended-denied`, `not_one_tap` when `one_tap=false` | P2 |
| Owner, fake clock | `ap-undo-race`: revert before hold, at hold-1 ms, at hold (same cycle: revert wins), after apply (`too_late`); held answer whose decision expires -> `stale`; owner restart during hold keeps it held | P3 |
| Owner | `ap-batch-with-one-t3-applies-nothing`, `batch_too_large` (21 and `batch_max+1`), duplicate id, per-item stale while others apply, crash after item 1 then replay (`already_resolved`, one effect each) | P4 |
| Owner | auto: off by default; listed + T1 + budget left -> no decision, retry, `approval.auto_decided`; budget used -> decision created; project-file `auto` ignored; managed `[]` wins | P5 |
| Scheduler | lease order by downstream count, key tie-break, deterministic; `stop_of` names decision ids and tiers | P6 |
| E2E (scripted provider, `apps/cli/tests` outcome e2e pattern) | an outcome with two items, one blocks (`blocked`): queue shows T1 + blocks; one-tap with hold; Undo; one-tap again; applied; the other branch kept running | P3 |
| Flood | 200 T1 decisions + 1 T3 contract row: T3 first and visible, header counts right, batch caps at 10, home render under the existing perf budget [V budget] | P4 |
| UI shots (`superagent/home/shot.rs`) | row with chip, one-tap and blocked-by; batch bar; digest; Undo toast; Lapsed and Answered sections; T3 never folded | P2-P4 |
| UI unit (`home/tests.rs`) | `act()` maps `a`/`u`; one-tap inert before `ARM_MS` and on a held key; digest Accept inert before arming | P2-P4 |
| TUI | `design_tests` and TUI shots unchanged (run, not assumed) | every slice |
| Red team (`tests/redteam/cases.mjs`) | `ap-inj-label-cannot-lower`, `ap-conductor-cannot-batch` | P4 |

CI on macOS, Linux, Windows for every slice (outcome owner tests that are unix-only today stay so; the view model, classifier and protocol tests are portable).

---

## 12. Implementation plan (one PR per slice)

Order change against the suggestion I was given: Undo (P3) comes before batch (P4), because batch is the riskiest mechanism and Undo is what makes a mistaken batch cheap; Undo also helps every tier from the start.

| Slice | Scope | Files (read and confirmed to exist) | Depends on | Acceptance | SR |
|---|---|---|---|---|---|
| **P1** Queue, tier, record, blocked-by (no behaviour change) | `tier.rs` classifier; `approvals.queue`, `approvals.history`; `blocked_by` on items; `audit` in `decision.resolved` (mechanism `card`/`cli`, tier, version); home rows per decision with chip, blocked-by, real ages, Lapsed/Answered sections; `synt outcome queue` | [N] `crates/synthesis-orchestrator/src/tier.rs`, `lib.rs`; `crates/synthesis-orchestrator/src/commands.rs` (audit at apply); `crates/synthesis-session/src/outcome.rs` (`Resolution.audit`, event payload); `crates/synthesis-events/src/outcome.rs` (constants); `crates/synthesis-app/src/{proto.rs, outcome_service.rs, outcome_client.rs, outcome_cmd.rs}`; [N] `crates/synthesis-view/src/outcomes/queue.rs`; `apps/desktop/src/approvals.rs`, `apps/desktop/src/superagent/home/{data,needs,mod,card,shot,tests}.rs`, `superagent/outcomes/{popup,link,model}.rs`; docs: this spec, `docs/specs/events.md` 7, `docs/specs/README.md` | none | R1-R4, R9, R11, R12 tests green; Gates stay false; TUI shots unchanged | yes (classifier table and `Call` classification only; small) |
| **P2** One-tap + recommended | `mechanism` on `decision.resolve` and the owner re-check; `one_tap`/`batch_max` settings read; ADR-021 amendment; one-tap button and `A` key; snooze; approve dialog Folder row from `repo_root` | `commands.rs`, `crates/synthesis-app/src/{proto.rs, outcome_service.rs, outcome_policy.rs, doctor.rs}`; `apps/desktop/src/superagent/home/*`, `apps/desktop/src/last_session.rs` (snooze), `superagent/goal/review.rs`; `docs/architecture/adr/ADR-021-outcome-ui-protocol.md`, `docs/specs/config.md` | P1 | R5; `ap-one-tap-*`; shots | yes |
| **P3** Undo | `hold_ms` on `decision.resolve` and `outcome.contract.approve`; `command.revert`; revert-first ordering and hold skip in `apply_queued_with`; store method that finishes target + revert in one transaction; `approval.reverted`; toast + `U`; `undo_seconds` setting; runbook | `crates/synthesis-orchestrator/src/{commands.rs, gateway.rs}`; `crates/synthesis-session/src/outcome.rs` (`revert_command`, enqueue validation); `crates/synthesis-app/src/{proto.rs, outcome_service.rs, outcome_client.rs, orch_owner.rs, outcome_policy.rs}`; `apps/desktop/src/superagent/{home/*, goal/{mod,review}.rs}`; `docs/testing/outcome-orchestrator-smoke.md` | P1 (P2 for the one-tap toast) | R7; `ap-undo-race`, held-expiry, restart-during-hold; e2e | yes + supervisor (changes when the owner applies person commands) |
| **P4** Batch | `decision.resolve_many` end to end; gateway `ResolveMany`; batch bar, digest, per-item exceptions, per-outcome fan-out with `batch_id`; red-team cases | `crates/synthesis-orchestrator/src/{commands.rs, gateway.rs}`; `crates/synthesis-session/src/outcome.rs` (enqueue validation); `crates/synthesis-app/src/{proto.rs, outcome_service.rs, outcome_client.rs, orch_owner.rs}`; `apps/desktop/src/superagent/home/*`; `tests/redteam/cases.mjs` | P2, P3 | R6; S1, S2, S7 tests; flood test | yes |
| **P5** Auto-decide | `synt.approvals.auto`, `AUTO_ALLOWED`; auto path at a `Blocked` end; `approval.auto_decided`; doctor line; Settings group | `crates/synthesis-orchestrator/src/{runner.rs, decision.rs, tier.rs}`; `crates/synthesis-app/src/{outcome_policy.rs, doctor.rs, orch_owner.rs}`; desktop Settings view [V file name]; `docs/specs/config.md` | P1 | R8, R13; S9 tests | yes |
| **P6** Reprioritisation | lease order by downstream count; `stop_of` names decisions | `crates/synthesis-orchestrator/src/sched.rs` (`pick_in_order`, `:456`, `stop_of`) | P1 | R10; scheduler determinism tests; existing sched tests unchanged except order where downstream differs | no (no authority change) |

P1 is small and valuable alone: real ages, the blocked-by relation and tiers on every row, a CLI queue that cannot lose an outcome ask, and the audit record. P5 and P6 are independent of P2-P4 and can run in parallel with them (disjoint files except `commands.rs`/`orch_owner.rs`: rebase the later one).

Rollback per slice: P1 is reads plus an additive event field (revert the PR; old events keep the extra field, harmless). P2-P4: turn the mechanism off by settings (`one_tap: false`, `undo_seconds: 0`, `batch_max: 0`) or remove the feature string; queued held commands then apply on their own after the hold. P5: `auto: []`. P6: one function back to key order.

---

## 13. Open questions for the owner (defaults apply if unanswered)

| Q | Question | Recommended default |
|---|---|---|
| Q1 | One-tap is T1 only, because every T2 kind today has no recommendation except the agent's own, which you ruled is a label, never preselected (2026-11-18). Keep that? | **Yes, T1 only.** Revisit when a T2 kind gets a recommendation the code can derive. |
| Q2 | Auto-decide: the only safe kind today is "a worker ended blocked: run it again" (bounded by the retry budget). Ship the setting with that one kind, off by default? | **Yes, off by default; `blocked` only.** More kinds come with `assumption_review` (concept A4), which needs the verification caveats first. |
| Q3 | Read-only folder consent (backlog F, open since 2026-11-19): when a read-only goal targets a broad folder or one with secret-shaped files, require an explicit consent? | **Yes, a T3 consent in the goal review before approval** (data goes to the model provider); not part of these slices. |

---

## 14. Assumptions and follow-ups for the planner

1. [A] The owner loop's 500 ms tick (`orch_owner.rs:60`) is short enough for a 10 s hold; a held command does not need a wake-up job. Verify in P3 that the loop keeps running while only held commands exist (an idle owner exits after `idle_exit_minutes`; a queued command must keep it, or restart it) [V `orch_owner.rs` idle rule].
2. [V] `irreversible::scan`, `text_hits` and `is_secret_shaped` are callable from `tier.rs` with the stored call inputs (the orchestrator already depends on `synthesis-permissions`, `Cargo.toml:18`).
3. [V] The `blocked` retry-budget raise reads the same rule the store uses (`retry budget` refusal in `runner.rs:283`); if the budget is not cheaply readable, drop the raise and let the retry be refused as today.
4. [V] `desktop-state.json` has two background writers that can drop an update (DEBT 1); snooze loss is acceptable (fail visible).
5. [A] Rows per decision make `waiting_count()` larger for outcomes with several decisions; the TUI view shows that number only through the mode button: run `design_tests`; if a sample changes, keep the sample at one decision per outcome.
6. [A] The numbers (10 s Undo, 30 s ceiling, batch 10/20, snooze 1 h/4 h, 24 h Lapsed) are proposals, not measured; P1-P3 events give the first data.
7. Later, outside this spec: concept A3 (attempts continue after a refused call) for true in-attempt non-blocking; concept A4-provisional `assumption_review` (T1 with checkpoints; it plugs into one-tap, batch and Undo unchanged); A7 deferred asks in own sessions; `message_worker` card; read-only consent (Q3).
8. Docs to update in the slices: `docs/specs/README.md` (list this spec), ADR-021 (P2), `docs/specs/events.md` (P1, P3, P5), `docs/specs/config.md` (P2, P3, P5), `super-agent-s1.md` 5.5 (gate lifted per slice), `approvals-concept.md` section 11 (point A1/A4/A6 here).

---

## 15. P1 as built (where the code differed from the draft)

| Draft | Code |
|---|---|
| Unknown kind T2 | **T3** (fail closed, P1 instruction). `assumption_review` is unknown until built |
| `classify(kind, brief, action, item)` plus a `hint` | `classify(kind, action, item)`: the brief decides no tier; no hint exists |
| [V] retry budget: `retries_used` equals the budget | A `blocked` retry is refused by the attempt ordinal (`new_attempt`: next ordinal <= `max_attempts_per_work_item`); `retries_used` counts only retryable failures (limit budget - 1). T2 when either is spent, or the facts cannot be read. The contract's budget is used, not the settings ceiling |
| [V] `irreversible::scan` on the stored calls | `scan` reads recipe files of `make`, `just`, `npm`, `pnpm`, `yarn`, `bun` from folders the command names. The classifier reads no such file: those runners are T3 without scanning; commands over 16 KiB, a shell call without a command, and any bare, prefix or wildcard shell rule (`Bash`, `Bash(*)`, `Bash(git:*)`) are T3. It uses `irreversible::scan_no_fs`, where reaching a recipe runner is itself a hit |
| T3 raises | also: a missing or unverifiable stored action, an unreadable shape; tools `SendMessage`, `browser_*`, `mcp__*` count as network; `Write`/`Edit` content is checked with `text_hits` |
| `Resolution.audit`: no other caller changes | every `Resolution { .. }` literal in tests gained the field |
| Lapsed: expired or invalidated in the last 24 h | an invalidated row has no time of its own: both are listed while `expires_at` is within the last 24 h or later |
| `policy` = effective settings, `one_tap`/`batchable` per row | P1 offers no mechanism: `{"one_tap": false, "batch_max": 0, "undo_seconds": 0}`, both row flags false (the settings come in P2) |
| Queue row of a contract revision (row 8) | not listed: R1 covers `contract_pending` only; a revision of a blocked outcome is reached through the outcome as before |
| T0 "dismiss only" | no dismiss in P1; Lapsed rows age out |
| 4.7 outcome frame work item rows say "waits for your decision" | not in P1: the home's Running row says it; `blocked_by` is served on every item |
| Contract row opens the goal review | opens the outcome |
| Tool asks get a blocked-by line (1.2) | they get the T2 chip only: a session ask holds no work item |
| [V] 6.5 the CLI event printer and unknown event types | holds: `synt outcome events` prints seq, type and the whole record |
| Command text judged as written | also judged with line continuations, quotes and backslashes removed (and lowercased); either one raises. Network programs (`curl`, `wget`, `ssh`, `gh`, cloud CLIs, `npx`, `pip`, PowerShell `iwr`/`irm` ...), recipe runners anywhere in the command, secret wildcards (`.env*`) and `git push/fetch/pull/clone/ls-remote/remote/submodule` are T3 whatever the case |
| T3 raises name Write/Edit content | also `MultiEdit` `edits[].new_string`, `NotebookEdit` `new_source`, `Grep`/`Glob` `pattern` and `glob` naming a secret, and an action with no call and no needed rule |
| `blocked` is T1 unless the budget is spent | also T2 when the stored action is absent or holds anything but harmless `Read`, `Grep`, `Glob` or `ExitPlanMode` calls (P5 would retry a T1 without a person) |
| 6.1 rows capped at 1000; counts cover every row | waiting asks are kept before lapsed ones, newest first, and cut before the per-row reads; `counts.t0..t3` count listed rows, `counts.waiting` every ask, `counts.omitted` the cut ones. `blocks` names at most 20 items, `blocks_total` counts them. An outcome that cannot be read is listed in `unreadable` and the rest of the queue is served (a row that makes `list_outcomes` itself fail still fails the read) |
