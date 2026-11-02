# Synthesis conformance gaps vs the official checklist

Audit date 2026-10-28. Input: `docs/research/official-conformance.md` (140 numbered statements). Code audited: working tree of `<repo>` (uncommitted edits by other developers included) and `target/debug/synt`. No `.rs`/`.js`/`.yml` file was changed. Items tagged `not verified` in the checklist are UNKNOWN, never requirements. Codex statements (23-29, 37, 47, 59, 91, 128-131) are treated as secondary references, since Synthesis follows the Claude Code model.

Evidence kinds: **experiment** = I ran a scratch program (outside the repo) against `synthesis-permissions` or ran `target/debug/synt`; **golden** = repo golden case; **code** = file:line read, not executed. Paths are relative to the repo root; `P` = `crates/synthesis-permissions/src/`.

## 1. Summary counts (140 items)

| Status | Count |
|---|---|
| MATCH | 60 |
| GAP | 34 |
| DIVERGES-BY-DESIGN | 23 |
| UNKNOWN | 19 |
| N/A | 4 |

GAP by priority: P0 = 3, P1 = 9, P2 = 22.

- GAP P0 items: 12, 33, 96
- GAP P1 items: 15, 19, 30, 31, 46, 53, 61, 74, 118
- GAP P2 items: 4, 7, 8, 39, 42, 44, 45, 49, 50, 51, 52, 54, 66, 69, 71, 80, 108, 114, 115, 121, 124, 125
- DIVERGES-BY-DESIGN items: 11, 23, 24, 26, 27, 34, 36, 41, 43, 47, 56, 70, 78, 84, 85, 86, 87, 88, 93, 94, 98, 104, 120
- UNKNOWN items: 35, 62, 64, 72, 75, 76, 79, 81, 82, 89, 103, 105, 111, 117, 119, 126, 129, 134, 140
- N/A (guidance, not testable): 16, 109, 131, 135

Note: several items are partly MATCH and partly GAP (e.g. 8, 42, 54, 69); they are counted once, as GAP, and the MATCH part is in the line note of section 5.


## 2. Prioritized GAP list

### P0 (security / safety: a protection silently does nothing)

**P0-1. Read-deny rules do not cover Bash file commands (item 12).**
- Now: experiment with `deny: ["Read(**/.env)"]` and `deny: ["Read(sec.txt)"]`: `cat sec.txt`, `cat .env`, `head -n1 .env`, `tail .env`, `grep k .env`, `cat < .env` all return Allow ("read-only command"). Only Read/Grep/Glob are bound to a `Read(..)` rule (`crates/synthesis-permissions/src/lib.rs:490 `read_family`). Write/Edit of a Read-denied path only ask (bypass: Allow).
- Official: a `Read(path)` deny also blocks Edit/Write and recognized file commands (`cat`, `head`, `tail`, `sed`, `tee`) and redirection targets.
- Minimal fix: in `decide_tool`, for `Bash` collect read operands of the read-only command set (the table in `crates/synthesis-permissions/src/bash.rs:251-742 already knows operand positions; `fs_operands` bash.rs:1051 covers rm/cp/mv) and test them with `paths::match_path_rule_real` against `Read(..)` deny rules; add `Read(..)` deny -> Edit tools in `matches_rule` (`read_family` also for EDIT_TOOLS when `any`). Crate `synthesis-permissions`; tests: new cases in `tests.rs` next to `recursive_grep_glob_checked_against_read_deny_rules` (line 2334).
- Mitigation today: the sandbox denies credential dirs (`crates/synthesis-sandbox/src/lib.rs:404`) but a workspace `.env` is readable.

**P0-2. `mcp__*` and `mcp__server__*` deny rules never match (item 96).**
- Now: experiment, deny `mcp__*` and deny `mcp__srv__*` vs tool `mcp__srv__do` -> decision Ask ("unknown tool requires confirmation"), not Deny. `mcp__srv` and exact names work (`crates/synthesis-permissions/src/lib.rs:491-492). A deny rule that fails open is the worst kind.
- Official: `mcp__<server>__*` globs work after the literal prefix; `mcp__*` as deny removes all MCP tools; as allow it is skipped.
- Minimal fix: in `matches_rule`, treat a trailing `*` on an `mcp__` rule as a prefix glob (`tool.starts_with(prefix)`) before the exact/`__` test; keep `mcp__*` out of allow (is_broad_allow). Crate `synthesis-permissions`, tests.rs.

**P0-3. Malformed settings.json silently drops deny rules (item 33).**
- Now: experiment, `SYNTHESIS_HOME/settings.json` = `{"permissions":{"deny":["Bash(rm:*)"],}}` (trailing comma): `synt run` prints only `synt: warning: malformed JSON in user settings` and carries on with no deny rules (`crates/synthesis-settings/src/lib.rs:196-199`, `apps/cli/src/run.rs:405-411`).
- Official: a syntax error is a Settings Error at startup.
- Minimal fix: make a malformed or unreadable existing user/project settings file a hard error for `synt run` (exit 2) and the TUI start, or at minimum fail closed to `dontAsk` plus a visible notice. Crate `synthesis-settings` (return an error flag) and `apps/cli/src/run.rs`; test in `crates/synthesis-settings` and `apps/cli/tests`. Owner decision D5.

### P1 (behaviour users will notice)

**P1-1. Ask rules ignored in bypassPermissions (item 19).** Experiment: `ask: ["Bash(git push:*)"]` + bypass -> Allow (`crates/synthesis-permissions/src/lib.rs:1157 returns before the `asked` branch; banner at `crates/synthesis-harness/src/harness.rs:520` documents it; golden `permissions/rules-ask-rule-beats-allow-and-readonly` encodes it). Official: ask-rule matches prompt in every mode. Fix: move the `bypass` return below the ask check (ask -> Ask, rule=true; headless -> denied). Change golden-divergence entry. Owner decision D8.

**P1-2. Read has no token/byte window (item 61).** Default 2000 lines x 2000 chars/line, files to 10 MiB (`crates/synthesis-tools/src/read.rs:8-9`, `util.rs:15`): one call can put megabytes in context. Official: 25,000 tokens per call, whole-file over 256 KB refused (offset/limit instead). Fix: cap output bytes in `read.rs` (e.g. refuse whole-file over 256 KiB, cap result by estimated tokens), with a hint to use offset/limit. Tests: `crates/synthesis-tools/tests/golden.rs` (case `read-default-limit-2000-lines` needs a divergence entry). Owner decision D4.

**P1-3. AGENTS.md vs CLAUDE.md precedence (items 46, 47).** Now both are loaded in the same directory unless byte-identical (`crates/synthesis-context/src/instructions.rs:244-252`), 120 KB total. Official Claude: CLAUDE.md wins, AGENTS.md only as fallback or via import; Codex: one file per directory, 32 KiB. Owner decision D2.

**P1-4. Hook defaults (item 56, DIVERGES-BY-DESIGN, listed because users notice).** Default timeout 60 s (`crates/synthesis-hooks/src/lib.rs:240`) vs official 600 s; a slow legitimate PreToolUse hook (e.g. a lint) is killed and BLOCKS the tool. Fail-closed itself is deliberate and safer. Owner decision D3.

**P1-5. No managed/local settings layers (items 30, 31, 15).** No gitignored local layer and no managed layer, so "allow always" cannot persist and an org cannot force deny rules. Fix: add `.synthesis/settings.local.json` (highest non-managed, same trust rules as project) and persist `Permissions::apply` suggestions there (cap 5 rules). Crate `synthesis-settings`; harness approval path. Larger than the others; plan it.

**P1-6. Enter while a task runs is refused, not queued (item 118).** `crates/synthesis-tui/src/app.rs:898`. Users type ahead in Claude Code. Fix: queue in `AppState`, send on `task.ended`. Test in `crates/synthesis-tui/src/app/tests.rs`.

**P1-7. Bash timeout kills instead of backgrounding (items 74, 75).** `crates/synthesis-tools/src/terminal.rs:139` ("[timed out after ...]", process killed); official moves to background and keeps it. Also no documented 30 min/2 h background caps. Owner decision D6.

**P1-8. Hook stdin payload is thin (item 53; also 49, 52, 80 at P2).** 9 hook events of 33, no `if`, stdin lacks `permission_mode`/`tool_use_id`/`prompt_id`, subagent frontmatter only `name/description/tools`. Existing Claude Code hook scripts that read `tool_use_id` or `permission_mode` break. Cheapest: add `permission_mode` and `tool_use_id` to the stdin payload in `crates/synthesis-harness/src/exec.rs:533` and `PermissionRequest`/`PostToolUseFailure` events later.

### P2 (polish)

- 4: bare tool deny should hide the tool from the model (`toolbox.rs`).
- 7: also strip `time`, `stdbuf`, `command`, `builtin`, `noglob`, bare `xargs` for allow matching (`crates/synthesis-permissions/src/bash.rs:942). Only costs prompts today.
- 8: commands over 10,000 chars always ask; a trailing `&&`/`|` should make the compound unparseable (experiment: `ls &&` is allowed by `Bash(ls:*)`). Fix in `bash.rs:149`/`end_seg`.
- 12 (rest): `Read(..)` deny should deny Write/Edit (see P0-1).
- 36/39/42/44/45: memory loader: all ancestors up to `/`, depth 4, skip inline code spans, `.claude/rules/*.md` with `paths`, auto memory, subdirectory on-demand.
- 50/51: hook matcher: plain name or `A|B` list is exact, else unanchored regex; ignore matcher on UserPromptSubmit/Stop; an invalid regex should warn loudly instead of making the hook inert.
- 54: PreCompact exit 2 should block (`crates/synthesis-harness/src/agent.rs:323`); Stop exit 2 should continue the turn.
- 66: a partial Read (offset/limit) must not mark the file fresh for Edit (`read.rs:205`); at least when the window does not cover the `old_string` area.
- 69/71: Grep should honour `.gitignore` and take `offset` (`grep.rs:67`).
- 108: re-read up to 5 recent files after compaction.
- 114/115: double-Esc rewind, Ctrl+B background, Ctrl+T checklist.
- 121/124/125: `--output-format text|json` and `-o` final message, `--bare`, 10 MB stdin cap (`apps/cli/src/run.rs:1102`).

## 3. Decisions needed from the owner

D1. **Read-before-edit enforcement.** Synthesis enforces for every model, mtime-checked (edit.rs:59, write.rs:35); official varies by model. Recommended: keep (checklist 68). Open sub-question: should a partial read count (66)?
D2. **AGENTS.md vs CLAUDE.md.** Options: (a) official Claude: CLAUDE.md, AGENTS.md only when none; (b) Codex: AGENTS.md first, one file per dir; (c) status quo, both. Recommended: SYNTHESIS.md > AGENTS.md > CLAUDE.md, first non-empty per directory, 32 KiB combined cap, since Synthesis is model-agnostic.
D3. **Hook timeout / fail-closed.** Keep fail-closed for PreToolUse and UserPromptSubmit (documented deviation, safer). Choose the default timeout: 60 s (now) or 600 s (official). Recommended: raise to 600 s for PostToolUse and other fail-open events, keep 60 s for gating events with per-hook override.
D4. **Read limits.** Adopt 25k-token and 256 KB caps (official) or keep 2000 lines? The 2000-line default is `not verified` as an official number (item 64). Recommended: byte/token cap.
D5. **Settings parse errors.** Hard error (official) vs warning. Recommended: hard error in `synt run`, visible blocking dialog in the TUI.
D6. **Bash timeouts/background.** Keep 120 s/600 s; decide on auto-background at timeout (official) vs kill (now, deterministic for headless). Recommended: kill in headless, background in the TUI.
D7. **MCP output cap.** 28,000 chars with the full text saved as artifact (now) vs 25k tokens / 50k chars. Recommended: keep (equivalent intent, already saves the full result).
D8. **Ask rules in bypassPermissions.** Official: ask rules still prompt. Synthesis says bypass skips them. Recommended: follow official (P1-1) so `ask: ["Bash(git push:*)"]` stays a real tripwire.
D9. **Protected paths list.** Synthesis's list is a superset (adds `.synthesis`, `.ssh`, `.aws`); protection under bypass is "Allow" (matches official 18), but `synt run --help` says bypass keeps protected paths as hard stops, which is wrong for plain writes (experiment: bypass + Write `.git/config` -> Allow). Decide: keep official behaviour and fix the help text, or make protected paths ask even in bypass (Codex-like, safer). Recommended: ask even in bypass.
D10. **Sandbox failure.** Fail-closed Enforce by default (better than official). On Windows nothing is confined, so the default refuses all commands there. Decide the Windows default (BestEffort with loud notice vs refuse).
D11. **Settings location / ADR-011.** `.synthesis/` and `SYNTHESIS_HOME` only, `.claude/settings.json` never read. Stay (ADR-011) but document it as a conscious difference from items 30, 34, 41.
D12. **Subagent depth, auto compaction at 80%, sandbox on by default, JSONL-only headless output.** All stricter or different by design; confirm they stay.

## 4. Golden cases that conflict with the official checklist

The Node prototype is not authoritative (owner decision 2026-10-28). 717 cases = 696 in 11 areas + 21 in `schemas`. Areas with no overlap with the checklist: config 86, providers 46, render 95, keys 49, sgr 52, selection 56, transcripts 55, schemas 21 (no conflicts found; keys/render not compared to official TUI shortcuts beyond item list).

| Area / id | Encodes | Conflicts with item | Note |
|---|---|---|---|
| permissions / `rules-ask-rule-beats-allow-and-readonly` | ask rule + bypassPermissions = allow | 19 | the only one; fix with P1-1 |
| permissions / `rules-path-globs-edit` | `Write(/abs-never/**)` accepted as a rule | 11 | harmless; official says never matches |
| permissions / the 48 `bash-*` and 10 `write-*` table cases | decisions per mode | none found for 2, 8, 13, 17, 18; `time`/`stdbuf` wrappers (7) not covered | |
| hooks / `match-is-anchored-full-match`, `match-alternation-hits`, `match-regex-prefix-wildcard`, `match-mcp-tool-name-regex`, `match-invalid-regex-never-matches`, `match-case-sensitive`, `match-exact-name-no-match` | always full-match regex | 50 | consistent for simple names and `A|B`; differs only for metachar patterns (unanchored officially) |
| hooks / `match-target-undefined-matches-only-wildcards` | non-wildcard matcher with no target never fires | 51 | official ignores matcher on UserPromptSubmit/Stop |
| hooks / `stdin-json-payload-contract`, `stdin-payload-keys-overridden-by-run` | stdin has 4 keys | 53 | no permission_mode/tool_use_id |
| hooks / `nonblocking-failure-exit1-ignored`, `json-invalid-json-ignored` | fail-open | 56 (Rust differs from golden, in the safe direction) | Rust diverges by design on PreToolUse/UserPromptSubmit |
| settings / 5 cases: `claude-user-settings-not-read-by-default`, `read-user-config-*` (5), `agents-claude-dir-only-with-read-user-config`, `mcp-project-mcp-json-ignored-without-read-user-config`, `memory-auto-dir-claude-home-when-read-user-config` | Claude files read only behind an opt-in flag | 30, 34, 41, 94 | settled by ADR-011, not a bug |
| settings / `memory-user-then-project-ancestors-claude-md-ignored`, `memory-imports-relative-cycle-fence-missing` | root-to-cwd memory walk, import depth | 39, 42 | |
| settings / `mcp-project-mcp-json-needs-approval`, `mcp-project-mcp-json-enable-all`, `mcp-approval-lists-and-flag` | project MCP needs approval (also headless) | 94 | stricter than official `-p`; Rust is stricter still (hash-bound) |
| tools / `read-default-limit-2000-lines`, `read-offset-and-limit`, `read-line-longer-than-2000-truncated` | 2000-line window, no token cap | 61, 64 | |
| tools / `read-binary-file-error` | NUL-byte detection | 63 | official: extension list |
| tools / `grep-skips-binary-node_modules-and-git`, `glob-excludes-node_modules-and-git` | skips node_modules/.git, not .gitignore | 69 | Grep conflicts; Glob unknown |
| tools / `grep-head-limit`, `grep-count-mode` | head_limit on lines | 71, 72 | official count semantics partly `not verified` |

Count: about 30 of 717 (permissions 2, hooks 12, settings about 14, tools 6; some overlap), none of them blocks the Rust port; 1 (permissions ask rule under bypass) is a real safety conflict. Rust-side divergences are already recorded in `crates/synthesis-permissions/tests/golden-divergence.json` (3 cases), `crates/synthesis-tools/tests/golden-divergences.json`, `crates/synthesis-config/tests/golden_divergences.txt`.

## 5. Per-item audit (status, priority, evidence)

Legend: M = MATCH, G = GAP, D = DIVERGES-BY-DESIGN, U = UNKNOWN, N = N/A.

| # | St | Pri | Evidence |
|---|---|---|---|
| 1 | M |  | Mode enum + `manual` alias, crates/synthesis-permissions/src/lib.rs:167-205 |
| 2 | M |  | deny, dialog, critical rm, ask, bypass, plan, protected, allow order, lib.rs:3-6, 1081-1157 (but bypass precedes ask, see 19) |
| 3 | M |  | golden rules-ask-rule-beats-allow-and-readonly; experiment: deny beats allow |
| 4 | G | P2 | no tool-list filtering by deny rule found in synthesis-harness (toolbox.rs, agent.rs); a bare deny only blocks at call time |
| 5 | M |  | bash.rs:1471-1538 bash_specs/bash_all_match; golden rules-allow-bash-exact-and-star |
| 6 | M |  | `Bash(ls:*)` form used throughout tests.rs; golden rules-allow-bash-prefix |
| 7 | G | P2 | allow matching strips only timeout/nice/nohup + 7 safe env vars (bash.rs:942-975); `time`, `stdbuf`, `command`, `builtin`, `noglob`, bare `xargs` not stripped, so allow rules miss them (experiment: `time git status` asks). Fails safe (extra prompt). Deny/ask side strips more (loose mode): MATCH |
| 8 | G | P2 | split on ; / & newline and deep_segments for $()/backticks (bash.rs:149-226, 1289) MATCH; no 10,000-char always-prompt rule (experiment: 12k-char `echo` allowed by `Bash(echo:*)`); trailing `ls &&` still matches allow `Bash(ls:*)` (experiment) |
| 9 | M |  | basename/`r\m` candidates (bash.rs:1429-1455); `sh -c`, `/usr/bin/curl` still hit deny (experiment); sandbox on by default |
| 10 | M |  | paths.rs:152-175 `//abs`, `~/`, `/x` = project, bare = cwd; `/x` resolves against cwd not settings-file dir (minor) |
| 11 | D |  | `Write(..)` rules DO match Write (lib.rs:495-498; experiment: `Write(//p)` deny -> Deny); an `Edit(..)` rule also governs Write/MultiEdit/NotebookEdit. Safer than official |
| 12 | G | P0 | experiment: deny `Read(**/.env)` and `Read(sec.txt)` do NOT stop Bash `cat`, `head`, `tail`, `grep`, `cat <` (Allow, read-only command); Write/Edit on a Read-denied path only Ask (bypass: Allow). Only Read/Grep/Glob are covered (lib.rs:490) |
| 13 | M |  | read-only Bash set allowed in all 6 modes, tests.rs:101-110 table (`ls -la` = AAAAAA) |
| 14 | M |  | strict parse -> ask, lib.rs:23-24, bash.rs:149 returns None on backticks/$(); experiment `curl` asks |
| 15 | G | P1 | Permissions::apply mutates in-memory rules only (lib.rs:1366); no settings.local.json layer, no persistence, no 5-rule cap |
| 16 | N |  | engine enforces, memory files are context (synthesis-context) |
| 17 | M |  | PROTECTED_DIRS/FILES lib.rs:111-160 superset (adds .synthesis, .ssh, .aws); `.config/git` asks (experiment). `.claude` has no exceptions |
| 18 | M |  | protected_path_decision lib.rs:735-748: classify/deny/allow/ask per mode; plan denies edits earlier |
| 19 | G | P1 | experiment: ask rule `Bash(git push:*)` in bypassPermissions -> Allow (lib.rs:1157 bypass returns before the ask check, harness.rs:520 banner states it). AskUserQuestion/ExitPlanMode and critical rm still ask: MATCH. Golden rules-ask-rule-beats-allow-and-readonly encodes the opposite |
| 20 | M |  | MAX_CONSECUTIVE_BLOCKS=3, MAX_TOTAL_BLOCKS=20, automode.rs:13-14 |
| 21 | M |  | CYCLE default, acceptEdits, plan, auto; bypass and dontAsk excluded, synthesis-tui/src/app.rs:66, 860 |
| 22 | M |  | untrusted project: allow/additionalDirectories dropped, deny/ask kept, auto/bypass refused, synthesis-settings/src/lib.rs:5-13, 30-31 |
| 23 | D |  | Synthesis follows the Claude 6-mode model; no Codex policies |
| 24 | D |  | sandbox is an independent axis (--sandbox enforce/best-effort/off), one workspace-write-like policy; no read-only preset except plan mode |
| 25 | M |  | --network deny default, apps/cli --help; NetworkPolicy::Deny, sandbox lib.rs:42 |
| 26 | D |  | sandbox readonly_paths = .git/hooks, .git/config (sandbox lib.rs:160-161); ~/.ssh, ~/.aws credentials denied outright (lib.rs:404). Less than Codex's whole `.git`, deliberate so commits work |
| 27 | D |  | no Codex presets; equivalent is `--permission-mode bypassPermissions --sandbox off` |
| 28 | M |  | `synt run` default mode with asks auto-denied (events.md:90) plus enforce sandbox is effectively read-only |
| 29 | M |  | ProjectTrust::Untrusted strips project config/hooks/allow rules, synthesis-config config.rs:35-80, settings lib.rs:315-335 |
| 30 | G | P1 | Synthesis files: `<synthesis_home>/settings.json` and `<root>/.synthesis/settings.json` (DIVERGES-BY-DESIGN, ADR-011). Missing: a gitignored local layer and a managed layer (settings lib.rs:3-6) |
| 31 | G | P1 | only user < project; no managed layer, no `--settings` override |
| 32 | M |  | lists are unions, scalars last wins, settings lib.rs:3-5 |
| 33 | G | P0 | experiment: user settings.json with a trailing comma holding `deny: ["Bash(rm:*)"]` -> only `warning: malformed JSON in user settings`, the run proceeds with NO deny rules (settings lib.rs:196-199 returns None; run.rs:408 only prints). Official stops with a Settings Error |
| 34 | D |  | SYNTHESIS_HOME, never CLAUDE_CONFIG_DIR (ADR-011) |
| 35 | U |  | no live reload found; not verified |
| 36 | D |  | config.json takes SYNTHESIS_* env as top layer (config.rs:3-5); settings.json has no env precedence |
| 37 | M |  | project config cannot redirect provider baseURL, golden_divergences.txt |
| 38 | M |  | SYNTHESIS_* only, synthesis-config env.rs; hook children still get CLAUDE_PROJECT_DIR as output (hooks lib.rs:226) |
| 39 | G | P2 | walks workspace root -> cwd only (instructions.rs:233-244), not every ancestor; no on-demand subdirectory loading |
| 40 | M |  | outermost first, CLAUDE.local.md last in DEFAULT_NAMES, instructions.rs:42-48, 244 |
| 41 | D |  | names SYNTHESIS.md, CLAUDE.md, .claude/CLAUDE.md, AGENTS.md, CLAUDE.local.md; user files passed by caller; no managed-policy path |
| 42 | G | P2 | max_depth 5 (official 4), instructions.rs:33; inline code spans not skipped (fences are); no `\ ` escaped spaces. Project imports must stay inside the workspace (stricter than official one-time approval: DIVERGES-BY-DESIGN) |
| 43 | D |  | caps 40,000 bytes/file, 120,000 total (instructions.rs:31-34); no 200-line guidance |
| 44 | G | P2 | `.claude/rules/*.md` not loaded |
| 45 | G | P2 | no auto-memory loader found in synthesis-context/harness |
| 46 | G | P1 | owner decision: loads CLAUDE.md AND AGENTS.md in the same dir; dedupes only when contents are identical (instructions.rs:244-252) |
| 47 | D |  | no AGENTS.override.md; every name per dir, 120 KB total vs Codex 32 KiB |
| 48 | M |  | AGENTS.md is a default name, instructions.rs:42-48 |
| 49 | G | P2 | 9 events (hooks lib.rs:30-40) vs 33: no PermissionRequest, PostToolUseFailure, PostCompact, InstructionsLoaded, SubagentStart, ConfigChange ... |
| 50 | G | P2 | always `^(?:p)$` regex (hooks lib.rs:150-155); official: plain name or list is exact, anything else is an UNanchored regex. An invalid regex silently never matches, so the hook is inert |
| 51 | G | P2 | golden match-target-undefined: with no matcher target only wildcard matchers fire; official ignores the matcher on UserPromptSubmit/Stop |
| 52 | G | P2 | per-handler `if` not implemented (HookCommand has kind/command/timeout/fail_open only) |
| 53 | G | P1 | stdin has session_id, transcript_path, cwd, hook_event_name (hooks lib.rs:219-222), tool_name/tool_input (exec.rs:533); no permission_mode, prompt_id, tool_use_id, effort |
| 54 | G | P2 | exit 2 blocks, stderr is the reason, JSON ignored on 2: MATCH (lib.rs:291-296). PreCompact exit 2 is ignored (harness agent.rs:323-325); Stop exit 2 not checked |
| 55 | M |  | allow/deny/ask, reason, updatedInput (lib.rs:312-330); `defer` and PermissionRequest absent; hook allow is advisory only (stricter, by design) |
| 56 | D |  | default 60 s (lib.rs:240) vs 600; PreToolUse/UserPromptSubmit timeout FAILS CLOSED unless `fail_open` (lib.rs:11-14) vs official fail-open |
| 57 | M |  | JoinSet parallel + dedupe, hooks lib.rs:196-211, 235 |
| 58 | M |  | untrusted project hooks dropped, settings lib.rs:315-335 |
| 59 | M |  | only `command` handlers run (lib.rs:204) |
| 60 | M |  | `{:>6}\t{line}` read.rs:200-206; empty-file golden read-empty-file-note |
| 61 | G | P1 | no token cap: default 2000 lines x up to 2000 chars/line, files up to 10 MiB (read.rs:8-9, util.rs:15). Official: 25k tokens, 256 KB whole-file |
| 62 | U |  | png/jpeg/gif/webp and PDF `pages` exist (read.rs:11-16, 176); size/page limits not checked |
| 63 | M |  | notebook cells rendered (read.rs:38); NUL-byte binary refusal (util.rs:165) not extension list |
| 64 | U |  | not verified in checklist; Synthesis uses 2000 (read.rs:8) |
| 65 | M |  | edit.rs:93-101 |
| 66 | G | P2 | Edit needs an mtime-fresh read for ALL models (edit.rs:59-64), stricter than official; but a partial Read (offset/limit) also marks the file fresh (read.rs:205, ledger_set), official does not count PARTIAL |
| 67 | M |  | write.rs:35-40; new files need no read |
| 68 | M |  | unconditional, model-independent, mtime-checked |
| 69 | G | P2 | Glob mtime sort + 100 cap, ignores .gitignore: MATCH (glob.rs:12, 67). Grep walks with standard_filters(false), skipping only node_modules/.git (grep.rs:67-74), so gitignored files are searched |
| 70 | D |  | native Glob/Grep on every OS, so they pass the permission engine |
| 71 | G | P2 | Grep has head_limit but no `offset` (grep.rs:283-300) |
| 72 | U |  | not verified in checklist |
| 73 | M |  | 120,000 / 600,000 ms, terminal.rs:20-21 |
| 74 | G | P1 | timeout kills the command ([timed out], terminal.rs:139) instead of moving it to background; `run_in_background` exists; no 30 min / 2 h background cap found |
| 75 | U |  | 30,000-char inline cap MATCH (terminal.rs:19); save-to-file-with-preview for foreground output not checked |
| 76 | U |  | not checked |
| 77 | M |  | child gets own history, exec.rs:1426 |
| 78 | D |  | DEPTH_LIMIT 2 (exec.rs:1435) vs 3; stricter |
| 79 | U |  | no Explore/Plan agent types found; not checked |
| 80 | G | P2 | agents.rs:4-5, 64 reads name, description, tools only; no disallowedTools, model, permissionMode, maxTurns, hooks, isolation |
| 81 | U |  | not verified in checklist; Synthesis has TodoWrite (native.rs:120) |
| 82 | U |  | not checked (web.rs) |
| 83 | M |  | WebFetch/WebSearch always ask outside bypass (lib.rs:1215); sandbox network flag does not cover them |
| 84 | D |  | Seatbelt, bwrap, Landlock; ON by default (official off); Windows has no confinement (windows.rs:1-6) and the default Enforce mode refuses to run there (terminal lib.rs:22-25) |
| 85 | D |  | writable = workspace + private per-command temp; readable = allow-list (narrower than official); ~/.ssh etc. denied, sandbox lib.rs:60-70, 404 |
| 86 | D |  | only deny/loopback/allow; AllowHosts rejected on every backend (lib.rs:50-52); no domain allowlist |
| 87 | D |  | no per-host prompts |
| 88 | D |  | Enforce (fail closed) is the default; BestEffort is opt-in (terminal lib.rs:105-111, 700-715) |
| 89 | U |  | per-call `unconfined` spec exists (terminal.rs:132); no excludedCommands setting found |
| 90 | M |  | MCP servers not sandboxed (mcp.rs:9-10); file tools run in-process |
| 91 | M |  | Seatbelt + bwrap + Landlock fallback |
| 92 | M |  | stdio, http, sse (mcp config.rs:24-25); no ws |
| 93 | D |  | Synthesis's own scope order, apps/cli/src/mcp.rs:3-8 |
| 94 | D |  | project servers need --trust-project AND a hash-bound approval, also in headless (mcp.rs:5-17); stricter than official -p |
| 95 | M |  | mcp__server__tool, 64-char cap with hash, synthesis-mcp naming.rs:5-47 |
| 96 | G | P0 | experiment: deny `mcp__*` and deny `mcp__srv__*` do NOT match `mcp__srv__do` (decision Ask, lib.rs:491-492 only handles `mcp__server` and exact names). Official: those deny rules remove tools. `mcp__srv` works |
| 97 | M |  | tool 60 s, connect 30 s, per-server override (manager.rs:77-78, config.rs:168-177); matches Codex-ish/Claude -p |
| 98 | D |  | MAX_TEXT_LEN 28,000 chars with full text saved as artifact (mcp result.rs:12, 187-194); official 25k tokens / 50k chars |
| 99 | M |  | on_tool_list_changed manager.rs:751 |
| 100 | M |  | list_all_tools paginates, manager.rs:291 |
| 101 | M |  | isError normalised, result.rs:200 |
| 102 | M |  | annotations kept as hints, manager.rs:112-134; destructive MCP asks (harness.rs:520) |
| 103 | U |  | not verified in checklist |
| 104 | D |  | Threshold::Auto = 80% of window (compact.rs:30-32, 89) vs at the limit |
| 105 | U |  | partial in checklist |
| 106 | M |  | Threshold::Tokens(n); no 100k-1M range check |
| 107 | M |  | `/compact [instructions]`, harness lib.rs:15, compact.rs:158 |
| 108 | G | P2 | instructions rebuilt each turn and TaskState lives outside history (compact.rs:2); recent files are not re-read, keep_recent_tokens 4000 raw messages instead |
| 109 | N |  | provider cache behaviour |
| 110 | M |  | Task subagent returns a summary |
| 111 | U |  | not researched |
| 112 | M |  | see 21 |
| 113 | M |  | Ctrl+C: clear input, cancel task, press again to quit, app.rs:801-818 |
| 114 | G | P2 | Esc cancels task (app.rs:783); no double-Esc rewind/clear |
| 115 | G | P2 | Ctrl+O expand and Ctrl+R history search exist (app.rs:629, 789); no Ctrl+B background, Ctrl+T checklist |
| 116 | M |  | `!cmd` shell mode, app.rs:674, 930, events `origin:user` |
| 117 | U |  | `/` commands and Ctrl+P picker exist; `@` autocomplete not found in the TUI |
| 118 | G | P1 | Enter while a task runs is refused with a toast (app.rs:898), not queued |
| 119 | U |  | not verified in checklist |
| 120 | D |  | own table 0/1/2/3/4/70/130/141 (events.md:92-101); SIGTERM = 130 not 143 (run.rs:1258-1265) |
| 121 | G | P2 | only JSONL events on stdout; no text/json final-result mode, no --json-schema, no `-o` |
| 122 | M |  | asks auto-denied, `needed_permissions` in task.ended (events.md:36, 90) |
| 123 | M |  | `--allow 'Bash(git diff:*)'` rule syntax (run.rs help); broad allows suspended in auto |
| 124 | G | P2 | no --bare equivalent |
| 125 | G | P2 | stdin read with read_to_string, uncapped (run.rs:1102-1105) |
| 126 | U |  | skipped MCP servers are reported (mcp.rs:8-9) but a system/init equivalent was not checked |
| 127 | M |  | --max-cost-usd/--max-turns/--max-wall-secs (run help); --resume on the main command |
| 128 | M |  | events to stdout, diagnostics to stderr (events.md:89); `-o` missing (see 121) |
| 129 | U |  | not documented officially |
| 130 | M |  | --no-session-store ~ --ephemeral; stdin prompt supported (run.rs:1096-1108) |
| 131 | N |  | workflow guidance |
| 132 | M |  | synthesis-verification CompletionGate in run.rs |
| 133 | M |  | plan mode |
| 134 | U |  | Esc exists; /rewind not checked |
| 135 | N |  | prompting guidance |
| 136 | M |  | untrusted-data wrapping, synthesis-harness context.rs:8-9 |
| 137 | M |  | experiment: `curl` asks; unparseable asks |
| 138 | M |  | rules enforced in engine; hooks fail closed |
| 139 | M |  | model.retry/model.error events, events.md:42-43 |
| 140 | U |  | not verified in checklist |

## 6. Not checked / limits of this audit

- Experiments exercised the permission engine only (scratch crate against `synthesis-permissions`, with an in-memory `Env`; not the full harness). Hook, sandbox, MCP, TUI, compaction and tool findings are code reads plus repo tests, not executed here.
- I did not run the repo test suite: other developers are editing code and the task forbade code changes.
- `P0-2` and `P0-3` should each get a failing regression test before the fix.
- Items 4, 35, 45, 89 are "not found in the code" negatives from grep and may hide an implementation elsewhere.
