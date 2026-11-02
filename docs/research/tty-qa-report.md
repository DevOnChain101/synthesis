# TTY QA report: driving the real Synthesis TUI with the real provider

Date 2026-10-28. Driver: `tests/tty-qa/` (see its README). Provider: `omniroute` (OpenAI-compatible gateway),
`cc/claude-haiku-5-5` for nearly everything, one run of `cx/gpt-5.6-luna`. Terminal model: pyte, 120x40 unless
stated, `TERM=xterm-256color COLORTERM=truecolor`, macOS 15 / arm-or-x86 seatbelt backend.

Binaries: most scenarios ran on the `target/debug/synt` built 21:47; the items marked **[re-verified]** were re-run
on the 22:37 build (the other developer's working tree at that time, no later source changes). One earlier "FAIL"
(digit keys in the approval dialog) turned out to be fixed in the newer build and is not listed as a bug.
Nothing under `crates/`, `apps/` or `~/Vibe/nemo-cli` was modified; no commit or push.

Limits of this method: no real terminal emulator (no IME, no Shift/Option+drag, no ConPTY, macOS only), one
provider, one model family for tools, haiku returned no thinking text, auto-compaction needs ~200k tokens and was
not triggered.

## 1. Coverage table

PASS = works as designed. PARTIAL = works with a defect. FAIL = broken or missing. NT = not testable here.

| # | Area | Result | Evidence / note |
|---|------|--------|-----------------|
| 1 | Startup, first frame, footer (model, mode, ctx) | PASS | First interactive frame 1.2 s. `◆ omniroute/cc/claude-haiku-5-5 ... ctx ▱▱▱▱▱▱ 0%`, footer `⏸ manual mode on (shift+tab to cycle)   cc/claude-haiku-5-5 · effort: auto`. Welcome box shows an empty `cwd:` (B-23). |
| 2 | Typing, Alt+Backspace, Ctrl+A/E/K/W/U | PASS | `hello world foo` -> alt-bksp -> `hello world` -> ctrl-w -> `hello`; ctrl-a then `X` -> `Xhello`; ctrl-k kills to end. Ctrl+U clears to line start only (multiline keeps the rest). |
| 3 | Multiline (Shift+Enter, Alt+Enter, `\`+Enter) | PASS | CSI-u Shift+Enter, Alt+Enter, LF and `\`+Enter all give `> line1 / line2 / line3`. xterm modifyOtherKeys form `ESC[27;2;13~` is dropped (B-33). |
| 4 | History Up/Down, Ctrl+R | PASS | Up -> `q two`, Up -> `q one`, Down -> `q two`, Down -> empty. Ctrl+R shows `reverse-i-search` with `› first prompt`. |
| 5 | Paste: small, 300-line bracketed, right-click | PARTIAL | Small multi-line paste OK, right-click pasted clipboard text into the prompt OK. 300 lines are shown expanded (last 6 rows visible) instead of a `[Pasted text +300 lines]` chip (B-31). |
| 6 | Slash picker, `/help`, unknown command | PASS | `/` lists 10 commands with `1/10`; `/mo` filters to /mode,/model; Tab completes; `⎿ unknown command: /bogus (try /help)`. |
| 7 | `/model`, `/mode` pickers | PASS | Model list with `✓` on current; `/mode bypass` opens `Bypass permissions?` defaulting to `No, keep auto`. |
| 8 | `/clear` | PARTIAL | `※ new conversation` but header stays `ctx ▰▱▱▱▱▱ 9%` and the Status panel keeps 20.4k tokens **[re-verified]** (B-9). |
| 9 | `/compact` (manual) | FAIL | `※ /compact failed: NoProgress { before: 3, after: 11 }` **[re-verified]**; in a larger run 20 s with no progress indicator, then two warning lines (B-8). |
| 10 | `/resume` | PARTIAL | Picker works and `--resume <prefix>` restores the transcript and context (model answered from it). Rows show only 8 hex chars, folder and raw ISO time, 96 entries from every folder (B-15). |
| 11 | `/agents`, `/mcp`, `/permissions` | FAIL | `⎿ unknown command: /permissions (try /help)`; same for /agents and /mcp. Only 10 commands exist (B-14). |
| 12 | `@` file mention | PASS | `@src/` -> `❯ @src/lib.rs  162 B · just now`; `@READ` + Enter completes to `@README.md` without sending; no match shows nothing; Esc closes. |
| 13 | `!cmd`, `!!cmd` | PASS | `!!printf ...` runs with no model turn, `[exit code 1]` for `false`, unicode output intact, `!cmd` is followed by a model turn as designed (docs/CLI.md). |
| 14 | Streaming rendering | PASS | 2 to 3 s turns, 0 bytes written while idle, ~97 KB per 3.4 s of streaming (full repaint per tick, no visible flicker in the model). |
| 15 | Thinking block layout | NT | Neither haiku nor gpt-5.6-luna returned thinking text through the gateway. Footer shows `effort: auto` but no way to change it (B-30). |
| 16 | Markdown, code, tables | PARTIAL | Heading, bullets, bold/italic/inline code colouring, numbered list, blockquote `▎`, table with box drawing, CJK/emoji cells aligned. Fenced code is drawn with a diff gutter `│ + fn main() {` (B-22). |
| 17 | Tool rows Read/Write/Edit | PASS | `● Read(lib.rs) ⎿ read 12 lines` with numbered lines and `… +6 more lines (ctrl+o to expand)`; after approval `● Update(src/lib.rs) ⎿ Updated src/lib.rs — +9 −0 (lines 2–21)` with `@@ -2,6 +2,10 @@` and `2  2` dual line numbers. Running row says `Edit(lib.rs)`, finished row `Update(src/lib.rs)` (B-27). |
| 18 | Bash running tail + Ctrl+O | PASS | While running: `line 7 ... line 12` then `… +6 more lines (ctrl+o to expand)`; finished `seq 1 40` shows the first 6 lines + `… +34 more lines`; Ctrl+O expands everything and ends with `(ctrl+o to collapse)`; Ctrl+O again collapses. |
| 19 | Approval dialog, keyboard | PASS | Up/Down/Enter, digits `1-3` **[re-verified]**, Esc = `rejected by you` and the model recovers. 0.4 s input lock (`wait…`). |
| 20 | Approval dialog, mouse | FAIL | Click on `2. Yes, allow all edits...` leaves `❯ 1. Yes` and the dialog open **[re-verified]** (B-7). |
| 21 | Edit/Write approval preview | FAIL | `│ - b\nc` / `│ + B\nc` literal escapes, 400-char cap, no hunk view **[re-verified]** (B-5). |
| 22 | Todo / task panel | PASS | `TodoWrite(3 todos)` plus live panel `■ Reading README.md / ☐ Read notes.txt / ☐ Summarize`. Every TodoWrite call re-prints the full list in the transcript (B-26). |
| 23 | Esc / Ctrl+C interrupt, stream | PARTIAL | Both stop the turn (`※ task cancelled: cancelled`, app stays alive, 2nd Ctrl+C on idle quits with `※ press ctrl+c again to quit`). The partial answer is discarded (B-12). |
| 24 | Esc / Ctrl+C interrupt, long Bash | PASS | `pgrep -f "sleep 311"` 3 pids before, none 2 s after Esc; same for Ctrl+C and for a model-run `for ...; sleep 313`. Output so far is replaced by `⎿ interrupted` (B-12). |
| 25 | Queue while running | FAIL | Enter during a turn keeps the text in the prompt, nothing is queued or sent, no hint **[re-verified]** (B-4). |
| 26 | Shift+Tab cycle, bypass confirm | PARTIAL | Observed order `manual -> accept edits -> plan -> auto -> manual`, but with repeats/stalls: `manual, accept, accept, plan, auto, manual, accept, auto` and, on the newer build, `auto, auto, auto` **[re-verified]** (B-10). Bypass is only reachable via `/mode bypass` (confirmation defaults to No; Shift+Tab leaves it). `--permission-mode bypassPermissions` is refused in the TUI with a clear message. |
| 27 | Esc Esc edit last prompt | FAIL | `/help` documents it; two Esc 50 ms or 150 ms apart after a finished turn leave the prompt empty **[re-verified]** (B-11). |
| 28 | Copy: drag, double, triple click | FAIL (P0) | Highlight appears (bg `3c4869`), notice `✓ copied 10 characters` shows, clipboard unchanged **[re-verified]**. `pbcopy` stays alive (same pid for 8 s) because stdin is never closed (B-1). Ctrl+C / Alt+C / Enter / `y` on a selection also do nothing visible. |
| 29 | Shift+drag native selection | NT | Handled by the host terminal. Mouse modes 1000/1002/1006 are on, which is the precondition. |
| 30 | Scrollback, wheel, PgUp | PASS | 300-line output: bottom 280-300, PgUp 270-290, PgDn back, wheel x3 = 3 rows per notch, indicator `↓ 40 more rows · pgdn`. Typing while scrolled does not jump to the bottom (B-24). |
| 31 | Resize / narrow / <60 cols | PASS | 200, 120, 100, 80, 60 cols all redraw; below 60x17 `terminal too small · need 60x17`; recovery without artefacts (an early blank screen was a driver artefact and is fixed in the driver). At 60-80 cols the 20-col sidebar stays (B-25). |
| 32 | Sidebar / sessions | PARTIAL | `/new` and Ctrl+N open `New session in folder`. The Workspace box lists every stored session of the folder as identical `repo-rust` rows (15 after a day of QA) (B-25). |
| 33 | Context status | PARTIAL | `ctx 9% 18.6k/200k` in the Status panel vs `ctx 5% 9k/200k` in the footer for the same turn; cache reads counted in one and not the other (B-9). |
| 34 | Auto-compact | NT | Needs ~200k tokens; not triggered. |
| 35 | Sub-agent (Task) | PASS | `Task(general-purpose: Read first lines...)`, `※ agent 1cd743bb-b6fd-43bc-b9bb-73f61bdd6b54 started`, child rows prefixed `@1cd743:Read(notes.txt)`, `... stopped`, parent relays the answer. Full UUID is noisy (B-28). |
| 36 | MCP stdio (echo_server) | PASS | `※ MCP server "echo" (project) approved`, dialog `Tool use · echo:echo {"text":"hello mcp"}`, row `● echo:echo(hello mcp) ⎿ hello mcp`. No status view (B-14); wording `unknown tool requires confirmation` (B-29). |
| 37 | Verification gate | FAIL | See B-3 and B-6: 3 to 4 extra dialogs per turn, `※ invalid session transition Verifying -> WaitingApproval`, ends `※ task unverified: unverified` although `4 passed, 0 failed`. |
| 38 | End-to-end coding task | PARTIAL | Rust: edit + test correct in ~19 s once `--sandbox-read ~/.rustup ~/.cargo` was passed; without it cargo cannot run at all (B-2). Node (nvm) hit the same wall. 9 to 11 dialogs for a 3-line change. |
| 39 | Error: bad key | PASS | `※ model error: Invalid API key` + `※ task blocked: model_error:auth: Invalid API key` (duplicated, B-19). |
| 40 | Error: key missing | PARTIAL | TUI refuses to start, exit 2: `synt: error: No credential for provider "omniroute". Set SYNTHESIS_API_KEY / SYNTHESIS_AUTH_TOKEN or the provider's apiKey in config.` (B-20). |
| 41 | Error: gateway down | PARTIAL | After 6.1 s: `model error: error sending request: client error (Connect): tcp connect error: Connection refused (os error 61)`; no URL or hint, duplicated (B-19). |
| 42 | Error: network deny | PASS | Default `--network deny`: tool commands have no network (not exercised against a real host). |
| 43 | Non-TTY, `--once` | PARTIAL | Piped stdin/stdout: `synt: the interactive app needs a terminal on stdin and stdout.` with usage; no `--cwd` on a tty: `synt: error: pass --cwd DIR`. `--once` prints one frame but with 24-bit/256-colour SGR even when stdout is a pipe (B-21). |
| 44 | Exit paths, terminal restore | PASS | Ctrl+D, `/exit`, double Ctrl+C, SIGTERM, SIGHUP: 1049/1000/1002/1006/2004 off, cursor shown, ICANON/ECHO/ISIG restored, exit code 0 (SIGTERM -15 after restore). SIGINT is ignored in raw mode (by design). |
| 45 | Colour / no-colour | PASS | `NO_COLOR=1`: zero 48;/38; colour sequences except one stray; `TERM=xterm` without COLORTERM: no 24-bit sequences; `TERM=dumb` still draws the full UI (acceptable). |
| 46 | Wide Unicode / emoji | PASS | `日本語テキスト 😀 emoji ✅ café Việt Nam` echoed intact, markdown table with `名前`/`言語`/`😀` keeps its borders aligned. |
| 47 | Vietnamese input | PARTIAL | Precomposed and NFD (`Việt` -> `Việt`) render correctly; Backspace removes one combining mark, not the grapheme (`Việt` -> `Viê`) (B-32). Hardware cursor is hidden, so an IME candidate window cannot anchor at the caret (B-34, not verifiable here). |

## 2. Bugs

Ranking: P0 = blocks a core promise (copy, running the user's toolchain, finishing a normal task); P1 = clearly
wrong or missing behaviour users will hit in the first session; P2 = polish.
Counts: **P0 3, P1 12, P2 19 (34 total)**.

### P0

**B-1 Mouse selection never reaches the clipboard (pipe tools never get EOF).**
Repro: run a command that prints text, drag over it, release. Notice `✓ copied 10 characters` appears but
`pbpaste` still returns the old content. `pgrep -x pbcopy` shows one `pbcopy` child that lives until the TUI is
killed (8 s observed). Cause: `crates/synthesis-tui-base/src/clipboard.rs:160-163`: the thread does
`stdin.write_all(&data); child.wait();` and `stdin` is only dropped when the closure ends, i.e. after `wait()`, so
the tool blocks on read forever; the comment "closes stdin on drop" is wrong. Same for `clip.exe`, `wl-copy`, `xclip`.
Fix: `drop(stdin)` before `child.wait()`; add a test with a real `cat > file` sink that asserts content. The success
toast should also reflect a completed write. (This area is being edited by the other developer; this is the root
cause to hand over.)

**B-2 Default sandbox makes toolchains in $HOME unusable.**
Repro: `synt --cwd <rust project>`; ask it to run `cargo test`. Result:
`error: could not read settings file: '/Users/<u>/.rustup/settings.toml': Operation not permitted (os error 1) [exit code 1]`.
Same for nvm-installed node/npm (`/Users/<u>/.nvm/versions/node/...`). The model then burns turns on `ls -la ~/.rustup`
and tells the user to grant Full Disk Access, which is wrong. Passing `--sandbox-read ~/.rustup --sandbox-read ~/.cargo`
fixes it. Cause: the read allow-list in `crates/synthesis-sandbox/src/lib.rs` (policy built near `deny_paths`/`credential_paths`,
l.105-190 and 402) has no toolchain roots, and the denial is not translated into a "blocked by sandbox, use
--sandbox-read X" tool result. Suggested: auto-allow read-only toolchain dirs found via `rustup show`/`$CARGO_HOME`/
`$RUSTUP_HOME`/nvm/volta/pyenv/asdf/Homebrew prefixes, and make the Bash tool report sandbox denials explicitly.

**B-3 The verification gate turns every task into a dialog storm and ends "unverified".**
Repro: Rust repo, `Add a function mul ... and run cargo test`: dialogs for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test` (each `verification check kN: Bash requires confirmation`), then the model calls `verify_run` (another
dialog), then the gate again: 9 to 11 dialogs total, ~20 s, and the end state is `※ verification unverified: unverified: 4 passed, 0 failed ...` /
`※ task unverified: unverified` because "no explicit acceptance criteria; inferred checks only". The model then tells
the user "the task is not confirmed complete". It also fires on read-only turns (asked for line counts of two files:
`verification check k3 ... cargo test` dialog) and on a trivial `Create hello.txt` turn (`cargo fmt --check` in a
folder with no Rust). A `> verification feedback` line is rendered as if the user had typed it.
Cause (suspected): gate checks go through the normal permission path without inheriting the user's earlier approval for the
same command (`crates/synthesis-harness/src/harness.rs:985-1010`), inferred checks are not filtered by touched files, and
"inferred only" is mapped to `unverified` rather than `passed (inferred)`.

### P1

**B-4 Enter while a turn runs is swallowed.** Type a second message and press Enter during streaming: the text stays in
the prompt, nothing is queued or sent after the turn ends, no hint (`crates/synthesis-tui/src/app.rs:760`, `submit` at :895).
`!cmd` in bash mode behaves the same (text accumulates: `! !false!!echo ...`).

**B-5 Edit/Write approval dialog shows an escaped one-line "diff".** `│ - b\nc` / `│ + B\nc`, 536-char input cut to 400 with a
sha256 note, instead of the numbered hunk that is shown after approval. Cause: the preview is built from the raw JSON
string (`crates/synthesis-permissions/src/lib.rs` Ask path / `crates/synthesis-tui-render/src/dialogs.rs`); reuse the post-edit diff renderer.

**B-6 Internal state-machine error leaks into the transcript.** `※ invalid session transition Verifying -> WaitingApproval`
every time a verification check needs approval (`crates/synthesis-core/src/session.rs:44`; raised from `harness.rs:588-604`). Either allow
the transition or do not surface it.

**B-7 Mouse clicks do nothing in the approval dialog** (all options inert; wheel and Tab too). Keyboard only.

**B-8 `/compact` fails loudly on short conversations and gives no progress.** `※ /compact failed: NoProgress { before: 3, after: 11 }`
(Rust Debug format of `CompactError`, `crates/synthesis-tui/src/chat.rs:239` + `synthesis-context/src/compact.rs:224`), emitted twice
(`context compaction failed (manual): ...` and `/compact failed: ...`). In one run the command took 20 s with the screen showing
`✓ Done` and no spinner. Should say "Nothing to compact yet" and show `Compacting...`.

**B-9 Context meter is stale or inconsistent.** (a) `/clear` leaves `ctx 9%` in the header and 20.4k in the Status panel;
(b) same turn shows `ctx 9% 18.6k/200k` (Status) and `ctx 5% 9k/200k` (footer): the first adds cache-read tokens to input
(`total 18.6k = input 9.3k + cache 9.2k`).

**B-10 Shift+Tab cycle is irregular.** Ten presses 0.7 to 0.8 s apart: `manual, accept, accept, plan, auto, manual, accept, auto, manual`
(old build) and `manual, accept, plan, auto, auto, auto, ...` (new build). Repeats and skips instead of a fixed ring.
Likely the cycle is computed from a pending vs applied mode (`runtime/app.rs` mode handling around `app.rs:1036`). The other developer is on mode switching.

**B-11 Esc Esc does not edit the last prompt** although `/help` lists "esc esc edit the last prompt" (`app.rs:792` Escape handling).

**B-12 Interrupts drop partial output.** Esc during streaming removes the partially streamed answer; Esc during Bash replaces
captured lines by `⎿ interrupted`. Keep what was received (Claude Code keeps both). Also `※ task cancelled: cancelled` repeats itself.

**B-13 "Don't ask again" suggestions for compound commands are wrong.** `whoami; id; ls ...` offers `Bash(whoami;:*)`;
`ls -la ...; cargo test 2>&1 | tail -3` offers `Bash(ls:*)`; a `cd "<path>" && test ... && echo ...` offers the whole literal command.
Cause: `suggestions()` in `crates/synthesis-permissions/src/lib.rs:1386` splits on whitespace without parsing `;`, `&&`, pipes. Should offer
nothing, or one rule per sub-command; a prefix rule derived from the first word of a compound command is misleading.

**B-14 Missing surface for features that exist.** Only `/clear /compact /exit /focus /help /mode /model /new /resume /status`.
No `/mcp` (no way to see connected servers/tools or failures), `/agents`, `/permissions` (rules, mode), `/cost`, `/diff`, `/undo`,
`/init`, `/config`, `/doctor`. The Skill tool exists but no skills appear in the picker in an empty project (expected).

**B-15 `/resume` picker is not usable at scale.** Rows are `3b617251 /private/tmp/.../repo-rust · 2026-10-28T15:12:12.874Z`: no title or first
prompt, raw UTC timestamp, path wraps over 2 lines, 96 sessions from every folder (including `~/Vibe/nemo-cli`). Needs first-prompt
snippet, relative time, current-folder filter first.

### P2

**B-16** The two-option `Bypass permissions?` dialog (and other short pickers) shows a meaningless `filter: ▍` line (reuses the picker widget).
**B-17** `/status` prints Rust Debug: `sandbox SandboxStatus { mode: "enforce", backend: "seatbelt", filesystem: "weak", network: "weak", unsandboxed_by_request: false }` (`app.rs:1101` `format!("{:?}", ...)`), and says `mode default` while the footer says `manual`.
**B-18** Completed Bash rows show `Bash(Run cargo test)` (the description) instead of the command; edit rows change name/path between running and done (`Edit(lib.rs)` -> `Update(src/lib.rs)`). Spinner says `Pondering` while a tool runs; elapsed shows `0s` for a user `!!sleep 3`.
**B-19** Model errors are printed twice and with internal codes: `※ model error: Invalid API key` + `※ task blocked: model_error:auth: Invalid API key`; the network error has no gateway URL and takes 6.1 s.
**B-20** A missing API key makes the TUI exit with code 2 instead of opening and explaining/offering `/model`.
**B-21** `synt --once > file` embeds colour escapes (`ESC[38;5;147m...`) although stdout is not a tty.
**B-22** Fenced code blocks render with a diff gutter (`│ + fn main() {`), implying additions.
**B-23** Welcome box prints `cwd:` with no value (also at 100 cols).
**B-24** After scrolling up, typing a character does not return the view to the bottom.
**B-25** At 60 to 80 columns the 20-column sidebar stays and leaves ~38 columns for chat; the Workspace box shows identical `repo-rust` rows for every stored session, and `Running · re…` titles are truncated.
**B-26** Each `TodoWrite` call re-prints the full list in the transcript (three lists for a 3-step task) in addition to the live panel.
**B-27** Footer/status truncation: `verify unve…`, `terminal too small · need 60x…` at 30 cols.
**B-28** Sub-agent notices print the full UUID (`agent 1cd743bb-b6fd-43bc-b9bb-73f61bdd6b54 started`) instead of the short id used in row prefixes.
**B-29** MCP approval says `unknown tool requires confirmation` (`synthesis-permissions/src/lib.rs:1380`); `synt mcp get echo` says `no such server` while `synt mcp test echo` finds the project server.
**B-30** Footer shows `effort: auto` but there is no control or documentation to change effort tiers; thinking display could not be verified.
**B-31** A 300-line paste is inserted verbatim into the prompt box (no collapsed `[Pasted text #1 +300 lines]`).
**B-32** Backspace on `Việt` typed as NFD removes one combining codepoint (`ệ` -> `ê`) instead of the grapheme cluster.
**B-33** Shift+Enter in the xterm `modifyOtherKeys` encoding (`ESC[27;2;13~`) is ignored (inserts nothing). Terminals without CSI-u cannot distinguish Shift+Enter; Alt+Enter and `\`+Enter work.
**B-34** The hardware cursor is always hidden and parked bottom-right; macOS IME (Vietnamese Telex, CJK) places its candidate window at the hardware cursor, so composition will appear at the wrong place. Needs a real-terminal check; consider showing and positioning the hardware cursor at the caret.

## 2a. Status after fixes (re-checked 2026-10-29 on a fresh `target/debug/synt`)

fixed = behaviour verified (unit/e2e test or the TTY driver); partly = improved, a part remains; open = not changed.
"Ev" says how it was verified: T = test in the repo, D = tty-qa driver on the real binary, C = code reading only.

| Bug | Pri | Status after fixes | Note / evidence |
|-----|-----|--------------------|-----------------|
| B-1 | P0 | fixed | stdin of the clipboard tool is closed before `wait()`; `pipe_clipboard_closes_stdin_so_tool_receives_text`, `pbcopy_really_receives_text` (T). |
| B-2 | P0 | fixed | toolchain roots are read-only allowed plus a "blocked by sandbox, use --sandbox-read" hint; `cargo test` ran inside the default sandbox in the driver run with no flags (D). |
| B-3 | P0 | fixed | one gate dialog, inferred-only pass is `passed` (spec 3 rule 4), unrelated files skip checks; driver run ended `verification passed: 3 passed ...` / `Done` (D). A task still shows the model's own Bash/Edit approvals (4 + 1 gate dialog in the driver run, was 9 to 11). |
| B-4 | P1 | fixed | Enter during a turn queues in order (`⧗ queued:`); `typing_and_submit_starts_a_task_and_enter_while_busy_queues_in_order` (T). |
| B-5 | P1 | fixed | numbered hunk in the approval dialog; `edit_approval_shows_a_numbered_hunk_not_an_escaped_one_liner` (T). |
| B-6 | P1 | fixed | no `invalid session transition` in the driver run with a gate dialog (D). |
| B-7 | P1 | fixed | `clicking_an_option_in_an_approval_dialog_answers_it_once_armed` (T). |
| B-8 | P1 | fixed | `compacting_shows_a_working_line_and_errors_are_sentences_not_debug` (T). |
| B-9 | P1 | fixed | header, Status panel and footer share one meter and `/clear` resets it; driver run showed `ctx 5% 11k/200k` in the footer and `5% 10.9k/200.0k` in Status for the same turn (T, D). |
| B-10 | P1 | fixed | `shift_tab_visits_every_mode_once_per_press_including_past_the_bypass_dialog` (T). |
| B-11 | P1 | fixed | `esc_esc_on_an_idle_empty_prompt_edits_the_last_prompt` (T). |
| B-12 | P1 | fixed | partial output is kept and the row says `interrupted · partial output kept`; `interrupted_bash_keeps_its_output_...` (T). |
| B-13 | P1 | fixed | one prefix rule per simple command; `compound_commands_get_one_prefix_rule_per_simple_command` (T). |
| B-14 | P1 | partly | `/mcp`, `/agents`, `/permissions`, `/status`, `/effort` exist now; `/cost`, `/diff`, `/undo`, `/init`, `/config`, `/doctor` still missing. |
| B-15 | P1 | fixed | current folder first, first prompt, relative age; `resume_picker_lists_this_folder_first_with_the_prompt_and_age` (T). |
| B-16 | P2 | fixed | `bypass_confirmation_has_no_filter_line` (T). |
| B-17 | P2 | fixed | `/status` prints no Rust Debug and uses the footer's mode names; `new_slash_commands_ask_the_host_and_status_has_no_debug_output` (T). |
| B-18 | P2 | partly | edit rows are `Update(<path>)` while running and when done (was `Edit(lib.rs)` then `Update(src/lib.rs)`); Bash rows show the command, not the model's description; spinner verbs are Working/Thinking/Reasoning/Planning (was 16 whimsical ones). Still open: the spinner verb does not name the running tool, and a user `!!sleep 3` shows `0s` elapsed. Goldens: 4 divergence reasons in `golden_divergences.txt`. |
| B-19 | P2 | fixed | one `※ model error:` line plus `※ task failed` (no code repeat); connection errors read `cannot reach 127.0.0.1:1 (Connection refused (os error 61)). Check that the gateway is running and the provider baseURL is right, then retry`, host:port only, no retries, 0.1 s instead of 6.1 s (D, `network_error_text_has_no_credentials` T). |
| B-20 | P2 | fixed | missing key: `synt: error: no API key for provider "gw": set GW_KEY and restart`, exit 2, immediately, no Debug/stack (D). The TUI still does not open to offer `/model`: by design a session needs a credential. |
| B-21 | P2 | fixed | `synt --once` to a pipe emits no ESC bytes (D). |
| B-22 | P2 | fixed | fenced code uses a plain `│ ` gutter (documented divergence). |
| B-23 | P2 | fixed | `welcome_box_shows_the_folder` (T); the driver shows `cwd: /private/...` (D). |
| B-24 | P2 | fixed | typing brings the view back to the prompt (`app.rs`, scroll reset on text) (C). |
| B-25 | P2 | partly | below 80 columns the chat uses the whole width (no sidebar), the Workspace count is the number of stored sessions and the Running list shows one row per other folder instead of one per stored session (D at 70 and 100 cols). Still open: `Running · re…` title truncation in the 20-column sidebar at 80+ columns. 14 golden cases moved by this and B-27 are listed as divergences. |
| B-26 | P2 | fixed | TodoWrite calls share one row (`interrupted_bash_keeps_its_output_todo_calls_share_a_row_...`) (T). |
| B-27 | P2 | fixed | status line: mode > model > effort, whole items only; key-hint footer and bottom status line drop whole hints (`fit_items`), prompt placeholder is cut at ` · `; header chips drop cwd before context % before model; `terminal too small` picks a shorter wording that fits (D, T). Goldens: divergences with reasons. |
| B-28 | P2 | fixed | sub-agent notices use the short id (`short_id`) (C). |
| B-29 | P2 | open | `unknown tool requires confirmation` for MCP tools is the permission engine's reason string (`synthesis-permissions/src/lib.rs`, golden-covered); not touched (permissions area). `synt mcp get` vs `test` not re-checked. |
| B-30 | P2 | fixed | `/effort [auto\|low\|medium\|high]` and Alt+T cycle the tiers declared in `capabilities.effort_tiers`; the footer shows `effort: <tier>`; the harness sends `reasoning_effort` on the following requests (OpenAI-compatible protocol only). Thinking text display still not verifiable (haiku returns none). `effort_cycles_with_alt_t_and_shows_in_the_footer` (T). |
| B-31 | P2 | fixed | `[Pasted text #1 +300 lines]` chip; `a_big_paste_is_a_chip_that_expands_when_sent` (T). |
| B-32 | P2 | fixed | `backspace_removes_a_decomposed_letter_whole` (T). |
| B-33 | P2 | fixed | `modify_other_keys_shift_enter_makes_a_newline` (T). |
| B-34 | P2 | partly | the real cursor is shown and parked at the prompt caret whenever the prompt is on screen and hidden under dialogs/pickers (`hardware_caret_follows_the_prompt_caret`, `caret_is_parked_after_each_repaint_...`; driver: cursor at (26,34) = the prompt caret). Whether macOS IME actually anchors its candidate window there still needs a manual check in a real terminal. |

Totals after fixes: fixed 29, partly 4 (B-14, B-18, B-25, B-34), open 1 (B-29). (B-1 to B-13, B-15 to B-17, B-21 to B-24, B-26, B-28, B-31 to B-33 were fixed by the other developer's uncommitted work in this tree; B-18, B-19, B-20, B-25, B-27, B-30, B-34 are this round.)

### End-to-end run after the fixes

`zsh -ic`, real provider `omniroute` / `cc/claude-haiku-5-5`, fresh Rust repo, prompt "add a function with a unit test to this tiny Rust project and make tests pass", default sandbox and manual mode, every dialog answered with Enter on "Yes":
20.2 s from Enter to `✓ Done · 6 turns · 19s`, 5 approval dialogs (read-only Bash, Bash, edit, and the single verification-gate dialog; one of the Edit calls was a model mistake with path `/dev/null` which was rejected), `※ verification passed: passed: 3 passed, 0 failed, 0 blocked, 0 unverified`, `git diff` = `src/lib.rs | 12 +`, `cargo test` in the repo passes (2 tests). The first, buggy driver run (dialog detection) showed the same flow. Glitches: the doubled `verification passed: passed:` wording; the Status panel said `IDLE last activity 3m39s ago` while the gate dialog was open in the earlier run. No Debug output, no `invalid session transition`, no sandbox error.
`qa_smoke.py` via `zsh -ic`: offline 7/7 PASS, `--live` 8/8 PASS (one real turn).

## 3. UX improvement suggestions

1. One approval per command ever: verification checks, `verify_run` and the earlier identical Bash call should share the user's decision; show "Verification: 3 checks, all passed (inferred)" as a single line.
2. Make inferred-only verification a pass with a footnote; reserve `unverified` for "could not run".
3. Sandbox: detect and mount toolchains read-only, and turn `Operation not permitted` under seatbelt into "blocked by sandbox: add --sandbox-read PATH". Surface the sandbox mode as a one-time hint in the first frame.
4. Show a spinner and the verb ("Compacting...") for any slash command that takes more than 300 ms; never print Rust Debug.
5. Queue messages typed during a turn (`⧗ queued: ...` above the prompt) and send them in order; keep Esc to drop the queue.
6. Keep partial output on interrupt; print `⎿ interrupted after 4.2s (12 lines kept)`.
7. Replace the three-pane dashboard below ~100 columns by a single chat column; keep the dashboard for wide terminals.
8. `/resume`: show first prompt, relative time, current folder first, `/` to search.
9. Add `/mcp`, `/agents`, `/permissions`, `/cost`, `/diff`, `/undo`, `/doctor`; show MCP server state in the footer or Status panel.
10. In the Edit dialog render the same diff as the final row, with path and `+N −M`.
11. Collapse repeated TodoWrite output into one updating block; collapse the sub-agent start/stop lines into the Task row.
12. Make the prompt chip for large pastes `[Pasted text #1 +300 lines]`, expandable with Ctrl+O.
13. Do not auto-open "Workspace" rows for every stored session; group as `repo-rust (15 sessions)`.
14. Show the hardware cursor at the caret (or at least when the input has focus) for IME users and screen readers.
15. Consistent naming for modes (manual/default, auto, accept edits) across footer, `/status`, `/mode` and docs.

## 4. Driver usage

See `tests/tty-qa/README.md`. Smoke: `<venv>/bin/python tests/tty-qa/qa_smoke.py [--live]` (8 checks, all PASS on the 22:37 build).
Ad-hoc scenario scripts used for this report lived in the session scratchpad and are not committed; every check above can be
rebuilt from the README API in a few lines (`Session`, `key`, `s.wait`, `s.drag`, `s.dump`).

Spend: roughly 90 short haiku turns, one 3 s gpt-5.6-luna turn.
