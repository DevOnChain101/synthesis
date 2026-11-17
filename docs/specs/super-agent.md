# Spec: Super Agent (desktop mode: chat with the orchestrator, outcomes, read-only session detail)

Status: Proposed design (2026-11-17). Docs only, no product code. Layout choice A or B is an open owner decision (section 14).
Inputs: owner mode normalisation 2026-11-17 (binding); wireframes `synthesis-compare/mockups/super-agent-v6-a.svg` (variant A) and `super-agent-v6-b.svg` (variant B); [conductor.md](conductor.md) (the backend and safety design of the chat); [desktop.md](desktop.md) ("Outcomes tab", "TUI view: outcomes (step 3)"); `docs/architecture/outcome-orchestration-plan.md` and ADR-016..021; the GPUI fork `~/Vibe/Legacy/synthesis-gpui-desktop-agent` (read-only reference).

## 1. Purpose and modes

The desktop app has **only two top-level modes**:

| Mode | What it is | Title-bar tab, shortcut |
|---|---|---|
| **TUI** | The current terminal-style view (`cli.html`), unchanged. | TUI, Cmd/Ctrl+1 |
| **Super Agent** | The mode with the orchestrator and outcomes: a chat with the orchestrator, the outcomes and their work items, and a read-only view of any session. | Super Agent, Cmd/Ctrl+2 |

Settings stays a popup over the TUI view (gear, Cmd/Ctrl+,), not a mode.

- The desktop tab currently called **Outcomes** is renamed back to **Super Agent** (title-bar tab label, icon tooltip). The shortcut Cmd/Ctrl+2 does not change. "Outcomes" stays as the name of the thing (`synt outcome`, ADR-021, protocol, code `outcomes_view`), not of a tab.
- **Conductor is not a mode.** The orchestrator chat is the Super Agent's chat frame. `conductor.md` is the backend and safety design of that chat (section 4); there is no Chat/Conductor switch in the TUI and no grid of session frames.
- Terminology: "orchestrator" is the outcome orchestrator (ADR-017); the chat's agent is the orchestrator's conversational face. Code and settings keep the `conductor` names from `conductor.md` (`synt.conductor.*`, tools `conductor_*`); the UI says "orchestrator".
- Off, Windows native, not enabled and unreachable states: section 9.

## 2. Layout

Shared parts (both variants), top to bottom:

- **Title bar** (shell, unchanged): TUI and Super Agent icons and the gear. The Super Agent tab carries a badge with the number of things waiting for the person (section 8).
- **Header strip**: `orchestrator: on . <project> . <branch>` (status of the orchestrator link).
- **Left rail**: `OUTCOMES` with `+ new`; each outcome expands to its work items (a work item is a worker session of the plan, with state glyphs done / running / waiting / blocked) and shows "N waiting" for needs-you; then `OWN SESSIONS` (sessions the window hosts that belong to no outcome; "needs approval" tag); footer with today's cost and the outcome cap. An amber dot (never colour alone: also a label) marks what needs the person.
- **Orchestrator chat frame**: transcript, proposal cards (section 4), decision cards (section 5), and the one composer ("Message the orchestrator, or @w3 to message a session"; the target chip says who gets the line).
- **Session detail** (section 6).
- **Hint line**: `Ctrl+T new session . Alt+A next decision . Esc back to the input . ? keys`.

### 2.1 Variant A: rail | chat | detail pane (three columns)

```
┌ title bar: TUI | Super Agent [2] | Settings ─────────────────────────────┐
│ rail (outcomes > work items,  │ orchestrator chat          │ session detail │
│ own sessions, cost)           │ cards, decisions, composer │ (read-only)    │
└────────────────────────────────────────────────────────────────────────────┘
```

Selecting a rail row fills the right pane (Transcript, Changes, Activity, Evidence tabs; "Read-only, message through the orchestrator (@w3)"; "Open in TUI"). The chat stays visible and keeps its composer.

- Pro: chat and the session are visible together; a card that mentions w3 can be read next to w3's transcript; no mode change to look at something.
- Con: three columns need width. A diff or a long transcript in a pane of about a third of the window is cramped; below 1100 px the pane overlays the chat (section 10). More chrome on screen at once.

### 2.2 Variant B: chat is home, the detail replaces the centre

```
chat home:                          session selected:
│ rail │ orchestrator chat │        │ rail │ detail (centre, wide)  │ context │
│      │ composer (bottom) │        │      │ ◂ Chat > outcome > w3  │ column  │
                                    │      │ composer docked bottom │         │
```

Selecting a session replaces the centre with the detail (breadcrumb `Chat > outcome > w3`, "◂ Chat" or Esc returns). The detail has tabs Transcript, Changes, Activity, Evidence, **Contract**, a right context column (this work item in its outcome: criteria, checks, dependencies, write scope, cost, sandbox "server-reported, not verified", the outcome's plan) and the orchestrator composer docked at the bottom with the target pre-filled `@w3`. A pending decision shows as a thin strip "1 decision waiting . Alt+A"; the answer is still given in the card in the chat.

- Pro: the full width for diffs and long transcripts; one thing in focus; simpler small-window behaviour (the rail is the only drawer); room for the Contract tab and the context column.
- Con: the chat is hidden while a session is open, so a decision card is one key away (Alt+A returns to the chat) rather than visible; two centre states to test; a person can forget there is a waiting decision (mitigated by the strip, the badge and the rail dot).

**The choice between A and B is an open owner decision (section 14, Q1).** Everything below is written to hold for both; where they differ it says so. The models, the cards and the chat are the same; only the layout code of S2 differs.

## 3. Who can do what (summary of the rules)

- The **person** creates, approves, answers, and confirms. The **orchestrator only proposes** (cards) and reports; it never approves a contract, never answers a decision, a session's approval or a question for the person. It has no tool or field for it (`conductor.md` N1, RT2).
- Targets are resolved **only among sessions this window hosts**; CLI sessions, other windows, peers and outcome attempt sessions are not addressable (`conductor.md` O3, RT7). Resolution is **fail closed** on ambiguity (exactly one match, else `ambiguous` with the candidates; no best-match tie-break).
- **Confirm by default**: every create, send, interrupt and close needs a card (`synt.conductor.confirm` is `"all"` unless the user or managed settings say otherwise; taint and described-target overrides force a card, `conductor.md` 3.2).
- **Caps** as in `conductor.md` 3.5 and O8: orchestrator chat 2 USD per conductor session, 5 USD per created session, 6 live sessions, at most 3 sends per turn, unpriced model refused. Spend appears in Usage and on the rail footer.

## 4. The orchestrator chat frame

Messages: the person's lines, the orchestrator's text, host rows (`s2 needs you` style notes, direct-send notes as fenced `<host_note>`), and cards. One composer; a first token `@w3`, `@s2` or `@<sid8+>` goes straight to that session as the person's own line (`conductor.md` 2.6; for a work item `@wN` is resolved through the plan, among hosted sessions only). A mid-text `@w3` is text for the orchestrator. The target chip is set by selecting a row (it is shown, and a click on it clears it); it never changes by itself.

### 4.1 Proposal cards

Every action the orchestrator wants appears as a **PROPOSAL** card the person confirms: create outcome, create session, send a message to a session, interrupt, close. A card is bound to bytes (RT8): later model text cannot change what Send delivers, a target that ended becomes `stale`, nothing is sent. Buttons are Confirm / Edit / Cancel; nothing is preselected for decisions (section 5), proposals have a primary button that does nothing until clicked.

### 4.2 Creating an outcome (owner decision 2026-11-17)

1. The person talks to the orchestrator ("move auth to OIDC, keep the old API working").
2. The orchestrator replies with a **draft outcome card** (a proposal: nothing exists yet): goal, criteria, checks, write scope, budget (with capabilities), base commit. Fields are from the same model as the form (`synthesis_core::outcome::Contract`, checked by `validate_contract`, errors inline, typed text never cleared).
3. Buttons: **Create draft** (creates the outcome in `draft`; it runs nothing), **Edit in form...** (opens the existing form of step 2, prefilled), **Cancel**.
4. **Contract approval is a separate, explicit step**: the Goal screen's "Review and approve" with the server's diff and brief hash, as today. Creating a draft never approves, and the orchestrator has no way to approve (`outcome.contract.approve` is never a conductor tool).
5. The manual path stays: `+ new` in the rail (and `n`) opens the form directly.

A card that would exceed a validation rule is shown with the error and without the Create button; the orchestrator is told, the person is not asked to fix JSON.

### 4.3 Backend

The chat runs on the Conductor backend plan (`conductor.md` sections 2-3 and PRs C0-C7): the host registry and resolver, the confirmation policy, limits, the restricted tool set. This spec changes only the UI contract: no grid, no per-session frames with docks, the session view is read-only (section 6). Outcome access for the orchestrator is the new `conductor_*` surface for draft creation only (propose outcome), not for approval; this widens `conductor.md` O4 and needs the owner's confirmation (Q4).

## 5. Decision cards inline

The #167 pieces in the TUI (centre tab "outcomes N", Approvals panel, auto-open decision popup) **move here**. A pending outcome decision is a card in the chat, at its place in time, using the shared logic (`outcomes_view::Ask`, `approvals::Popup`, ages) and the same safety rules:

- **Nothing preselected.** Choosing (arrows, 1-9, click) and sending (Enter or Send) are two steps.
- **Brief expanded once before Send** when sections are hidden or the first field is cut ("5 more items not shown: Tab shows the whole brief (required before sending)"; the Send button reads "Send (opens the brief first)").
- **Engagement guard**: Enter and digits do nothing until the person has engaged the card and for `ARM_MS` after it appears; nothing for a card that is not `pending`; a new revision or brief hash is a new card (cursor on nothing, armed again, a notice).
- **Focus model**: the card opens without taking the keys; the composer keeps focus; plain keys go to the composer; Alt+A or a click gives the card the keys; Esc or Alt+A returns them. While the composer has text, digits, Enter and Up/Down never reach the card; Shift chords always go to the composer (the #167 security review).
- The answer carries the card's own revision and brief hash; the server checks both again and the worker re-reads the decision before sending.
- Only a person's input answers. The orchestrator's text next to a card cannot choose or send (chat never shows an answer path of its own).
- Tool approvals, questions and plans of a **session** are not answered here: they are answered in the session itself (TUI, `Open in TUI`); the session view shows that it waits and since when (section 6).

## 6. Session detail (read-only follow)

The detail follows the owner's GPUI fork layout (MonoCode Native): a transcript with turns and tool-call cards, a diff view, changes, activity, a composer region. In Synthesis the composer region is **not** a writer into the worker:

- Tabs: **Transcript** (turns, tool-call cards, markdown), **Changes** (files changed with counts, "open full diff" to the diff view), **Activity** (tool and process timeline), **Evidence** (verifier facts, `event_backed`, "server-reported, not verified" for sandbox), and in B **Contract**.
- **Read-only follow**: live tail while running (updates throttled to 4 Hz, 30 fps animation cap), no input, no approval dock. For an attempt or a worker session the data comes from the store and events of the orchestrator (`attempt.diff`, `InspectorContent`), for an own session from its view state.
- **Messaging a worker only through the orchestrator**: the composer target is the orchestrator with `@w3` prefilled; the line goes to the orchestrator as a message (it may propose a send card). Typing directly into a worker is not offered (attempts are unattended, `conductor.md` 4.4).
- **Open in TUI**: continues the session as a normal TUI session (resume mode, read-only first: the TUI opens it viewing, then the person can continue). Only for sessions the person may resume (own sessions and finished worker sessions); an attempt that is running is not resumable (S3).
- A session that waits for a tool approval or question shows a banner "waiting for you since 4 min: Open in TUI".

### 6.1 What is reused from the fork (layout only)

Read: `README.md` (self-contained GPUI app, `src/conversation`, `src/transcript`, `src/synts`, `src/shell`, vendored `gpui-pre*`) and the `src` module list.

| Fork module | Use here |
|---|---|
| `transcript/` (virtualised layout) and `conversation/` (turn rendering, response menu, composer shelf) | transcript pane and chat frame layout; turns, tool-call cards, markdown (`markdown*`) |
| `diff_view.rs`, `changes.rs`, `changes_shelf.rs`, `session_review.rs`, `review.rs` | the Changes tab and "open full diff" |
| `activity.rs`, `synts/` (activity feeds) | the Activity tab |
| `explorer.rs`, `file_icons.rs`, `right_panel.rs` | optional file list of a work item; right pane / context column container |
| `shell/` (`title_tabs.rs`, `project_rail.rs`, `rails.rs`, `layout.rs`, `sidebar_list.rs`) | rail, tabs and layout ideas only (the title bar already exists in `ui.rs`) |
| `composer_*.rs`, `quick_composer*` | the one orchestrator composer, `@` completion |
| `context_meter.rs`, `usage_footer/` | cost line on the rail footer |

Not reused: mobile (`mobile*`, `mobile-link`), remote/ssh (`remote*`), updater, `accounts/` (ADR-011: API keys only), `inbox` provider adapters, `legacy_backend.rs`, `arcade.rs`, particle and 3D `visuals`, `terminal`. The palette stays ours (platinum, no blue accent).

### 6.2 Licence and attribution rule (owner decision 2026-11-16)

- Port the **layout** and widget structure into our own code on `synthesis-view` models; do not copy whole files.
- **Nothing Tauri-derived** (code, deps, assets, naming, comments): the fork has files that mention it (`src/shell.rs`, `src/transcript.rs`, `src/changes.rs`, `src/quick_composer.rs`, `src/pickers.rs`, `src/app_menu.rs`, `src/traffic_lights.rs`, `src/shell/window_chrome.rs`, `src/shell/project_rail.rs`, `src/editor_analysis.rs`, the `inbox` adapters); every ported piece is read for it and `grep -ri tauri` over the ported code must be empty before a commit; no `tauri` crate or `src-tauri`.
- **Attribution**: the fork's `Cargo.toml` says `license = "MIT"` but its root has no LICENSE file. Before S2 the upstream licence is checked (open question Q6); copied or closely derived code keeps the MIT notice (a `NOTICES`/attribution entry and a header), and until the check is done S2 takes only structure and no copied source.
- GPUI itself is already a dependency of `apps/desktop`; the fork's vendored `gpui-pre*` is not used.
- Licence status is recorded here; `desktop.md` has no separate rule text, only the pointer added in this change.

## 7. Keyboard map

Keys already accepted in `conductor.md` (owner clarification b): Tab/Shift+Tab focus panes, Enter zooms, Esc goes back, 1-9 jump to a session, Alt+A next approval. Mapped to this design:

| Key | Action | Note |
|---|---|---|
| Cmd/Ctrl+2 | Open the Super Agent tab | unchanged shortcut |
| Alt+A | Next pending decision (oldest first), focuses its card | from #167; works in the chat; in A and B it jumps to the chat if a session is open |
| Ctrl+T | New session (opens a create-session proposal flow) | new, to check (below) |
| Esc | Back: card -> composer; detail -> chat (B); close pane (A, narrow) | |
| Tab / Shift+Tab | Next / previous pane (rail, chat, detail) | |
| Up/Down, Enter | In the rail: move, open a row; in a card: move the cursor, Enter only after choosing | |
| 1-9 | In a card with focus: choose an alternative. In the rail with focus: jump to the Nth row | not while the composer has text |
| Cmd/Ctrl+L | Focus the orchestrator composer | from `conductor.md` 4.3 |
| `[` `]` | Previous / next decision card | from the Outcomes tab (step 1) |
| `r` | Refresh (in an unreachable state) | |
| `?` | Key help | |

Dropped from `conductor.md` because there is no Conductor mode: Cmd/Ctrl+Shift+M (toggle); Alt+0 and the TUI centre tab "outcomes N". Cmd/Ctrl+J ("frame that waited longest") duplicates Alt+A and is not taken; Alt+A is the one key.

**Conflicts to check before S0/S1 (flagged, not decided):**
1. **Ctrl+2 on Windows/Linux**: the TUI centre tabs use Ctrl+1..5, and Cmd/Ctrl+2 is the title-bar tab shortcut. On macOS Cmd and Ctrl differ; on Windows and Linux the two collide. Which wins in the TUI view (the existing wiring decides today) needs a check and, if it collides, an owner choice (Q5).
2. **Ctrl+T** may be taken in the TUI view or by the terminal; it is read in `driver.rs` before S1 and a fallback key is chosen if used. In Super Agent only; the TUI keeps its own map.
3. **Alt+A** keeps working in the TUI view as the "go to the Super Agent decision" key (it switches tab), see section 8.
4. Cmd/Ctrl+L and `?` are unverified against the existing map (`conductor.md` [A]).

## 8. The TUI keeps only a badge

After S0 the TUI view has no outcomes tab, no Approvals panel and no decision popup. It keeps one thing: on the **Super Agent tab in the title bar**, a badge with the count of things waiting for the person (decisions, plus orchestrator-reported waits; the same number the old "outcomes N" showed). A click on the tab or on the badge, or Cmd/Ctrl+2, opens Super Agent (with the oldest decision's card focused if the click was on the badge). When outcomes are off the badge and the tab's content follow section 9; nothing is shown in the TUI. The gate (`approvals::gate`) stays the single check; the TUI worker stays inert when off.

## 9. States

| State | Screen |
|---|---|
| Off (`synt.orchestrator.enabled` false) | Says how to enable (the setting, `synt outcome enable`); the window never enables anything. No badge. |
| Windows native | `WSL2_REFUSAL`, with the WSL way (desktop.md section on WSL). |
| Not ready (store not migrated) | The migration message. |
| Not enabled for the chat (`synt.conductor.enabled` false while outcomes are on) | Rail and outcomes work; the chat frame says the orchestrator chat is not enabled and the manual `+ new` path still works. |
| Unreachable / failed link | The error text, `r` retries; the rail keeps its last list marked stale. |
| Loading | Skeleton rows; the UI thread never waits on a socket. |

Nothing connects at launch: the first open of the tab runs the check (setting and platform), then connects, as today.

## 10. Small windows

Below 1100 px the rail becomes a **drawer** (button in the header strip, Esc closes). Variant A: the detail pane slides over the chat (Esc returns); variant B: no change besides the rail. Shots and `design_tests` at 1400x900 and 1100x700, plus 900x600 for the drawer. Minimum composer height is kept; cards scroll inside the chat, never clip their buttons.

## 11. What is reused from the existing code

- `apps/desktop/src/outcomes_view/` (model, link worker thread, render, form, `ask.rs` answer rules, shots): rename the tab and strings, move the left list into the rail, keep the form as "Edit in form".
- `tui_view/approvals_ui.rs`, `approvals.rs` (ages, merge and order, popup key machine, the gate): the popup UI is deleted from the TUI; the key machine and ages feed the card; the gate stays.
- `crates/synthesis-view`: `outcomes` (models, brief rows, `uncovered`, `Picker`, inspector tab set), `running`, `convo`, `state`, `toolview`, `activity`; the planned `synthesis-view::conductor` (`FrameModel`, `Attention`, `ProposalCard`; no `Grid`) from `conductor.md`.
- The orchestrator client (`OutcomeClient` over `LocalSocket`, surface `superagent`), unchanged; `apps/desktop` still does not depend on `synthesis-orchestrator`.
- The GPUI fork: layout of transcript, diff, changes, activity (section 6.1).
- Setting section for the Super Agent: `synt.superagent.*` rendering only (D17), created when first needed.

## 12. Phased implementation plan

Each phase is one PR, shippable alone, with CI (fmt, clippy, tests on 3 OSes); a security review for S0 (moved safety rules) and S1.

| PR | Scope | Acceptance tests | Red-team items |
|---|---|---|---|
| **S0** | Rename Outcomes -> Super Agent (title-bar tab, tooltip, UI strings, shot names `--view`, docs). TUI: delete the outcomes centre tab, the Approvals panel and the popup; add the badge. Move the #167 pieces: decision cards inline in the Super Agent centre (a simple card list until the chat exists), the same Ask/Popup logic and rules. | Rename shot and design tests; TUI layout rects unchanged when outcomes are off; badge count equals decisions; click and Cmd/Ctrl+2 open the tab; every #167 safety test is ported and green (nothing preselected, choose then send, brief expanded before Send, engagement guard, focus model, held Enter, Shift chord) | A held Enter reaches no card; a card appearing under typing never takes a digit; a new revision is a new card; the TUI has no answer path any more (grep test) |
| **S1** | The chat frame with proposal cards on the existing C0-C7 conductor backend plan (C0-C3 host and harness, C4 models without the grid, the chat part of C5, C6 WSL, C7 red-team); the draft outcome card with "Create draft" / "Edit in form"; `@w3` direct path; keyboard map. | `conductor.md` section 7 tests for C1-C4; a draft card creates nothing until Create; Create does not approve the contract; ambiguity returns candidates and no card; every card field rendered | `conductor.md` RT1-RT8 plus: the orchestrator cannot call contract approve; a forged decision card in model text is plain text; a draft card with an invalid contract has no Create |
| **S2** | Session detail from the fork: Transcript, Changes, Activity, Evidence (and Contract, context column in B), read-only; layout A or B per Q1; drawer below 1100 px. | Shots at 1400x900, 1100x700, 900x600; transcript with 5,000 rows scrolls within the frame budget (unverified until measured); no input widget exists in the worker view (test) | A worker line cannot be typed; tool output in the transcript cannot trigger a card; `grep -ri tauri` of the ported code is empty; licence entry exists |
| **S3** | Read-only resume: "Open in TUI" opens the session viewing, then continues as a normal TUI session. | A running attempt is not resumable; a resumed session lands in the TUI with its history; the person's first line is the first write | Resuming never changes the session's mode or workspace; no sandbox widening |

S1 depends on the orchestrator backend (B3, B8) and, for the live link, the owner's real Windows/WSL run (memory: B8 waits).

## 13. Risks

- Chat-first creation widens `conductor.md` O4 (outcome access) in one narrow way (propose a draft). Mitigation: draft only, the contract approval path is untouched (Q4).
- Variant A at 1100 px is tight; B depends on the strip and the badge for decisions out of sight.
- GPUI cost of a live transcript plus rail plus chat (base frame 7-10 ms, `ui-audit-2026-11.md`): throttle, cache entities; unverified until shots with a streaming session.
- Moving #167 removes a shipped surface; S0 keeps the badge so a waiting decision stays visible from the TUI.

## 14. Open questions for the owner

1. **Layout A (three columns) or B (chat is home, detail replaces the centre)?** (Section 2; not chosen.)
2. Is the badge in the TUI title bar enough, or should the TUI also show a one-line "N decisions" strip above the prompt? (Recommended: badge only, as decided.)
3. Confirm the orchestrator's name in the UI ("orchestrator") and that `synt.conductor.*` settings and `conductor_*` tools keep their names.
4. May the orchestrator propose a **draft outcome** (a new narrow tool), given `conductor.md` O4 said no outcome access in v1? Contract approval stays out of reach regardless.
5. Ctrl+2 collision on Windows and Linux (TUI centre tabs vs the title-bar tab): which wins? Ctrl+T for "new session": acceptable?
6. Upstream licence of the fork (MIT per its `Cargo.toml`, no LICENSE file): who confirms, and where is the attribution kept (`NOTICES` file)?
7. "Open in TUI" for a running worker: refuse (recommended) or allow after pausing the worker?
8. Does the orchestrator chat transcript show in the TUI Workspace list as a "conductor" session (`conductor.md` O10)?
