# TUI view UI audit, 2026-11-10

Offline audit with the shot harness (`--features shot,test-provider`, temp `SYNT_HOME`, scripted fake model). Shots in
`/private/tmp/claude-501/audit-shots/ (not committed, regenerate with the shot harness) <scene>-<WxH>.png`, each at 1400x900, 1100x700, 900x600. Scenes: `reply`, `tools`, `approval`, `diff`,
`terminal`, `browser`, `files`, `files-untrusted`, `hideall`, `cjk`, `picker` (`/model`), `help`, `settings`.
Inspected by eye: reply, tools (1400), approval (1100), cjk (900), hideall (1100), files-untrusted (900), picker (900),
settings (900). The other shots were rendered without error but not inspected one by one.

Note: the prompt marquee in a still always shows its phase 0 (the text enters from the right edge), so "L" at the right of
the slot is the animation start, not a tiny slot.

## Findings

| # | Finding | Severity | Status | Shot |
|---|---------|----------|--------|------|
| 1 | Auth failure shown twice (`Invalid API key`, then `task blocked: model_error:auth: ...`) | high | fixed: one line `model error (auth): Invalid API key - check your API key env` (same for rate_limit, network: `model error (<kind>): msg`); test in `synthesis-view` convo.rs | n/a (unit test) |
| 2 | Header prompt slot shrinks to nothing when the window is narrow (path and stats kept at full width) | high | fixed: `chrome::header_tail` drops `N files \| LOC`, then the path, to keep the slot >= 150 px (about 20 chars); unit test | reply-900x600, approval-1100x700 |
| 3 | Top bar chips are clipped mid-glyph (`session: Sh`, `run t`, `effo`) with no ellipsis or whole-chip drop | medium | fixed: `chrome::fit_chips` drops whole chips and ends with a `...` chip (unit test); deviation from `cli.html` below its measured widths, in `docs/specs/desktop.md`. Verified with shots at 1400x900, 1100x700, 900x600 (`/private/tmp/claude-501/p0-shots`, not committed), by eye: 900x600 and 1100x700 show `workspace: ws | ...`, no partial glyph | cjk-900x600, files-untrusted-900x600, hideall-1100x700 |
| 4 | Trust dialog body is cut mid-line at its fold at 900x600 (needs scroll, no cue) | medium | fixed: the dock shrinks (its text box and option list scroll) and shows `down-arrow more text: scroll`; at 900x600 the cut is in the scrolling box, with the cue. Verified with shots at 1400x900, 1100x700, 900x600 (`/private/tmp/claude-501/p0-shots`, not committed), by eye |
| 5 | With the Trust dialog open at 900x600 the prompt box is pushed under the dialog and clipped at the bottom | medium | fixed: dock is shrinkable (`min_h 0`, card scrolls as last resort), prompt box fully visible at 900x600 for Trust and approval. Verified with shots at 1400x900, 1100x700, 900x600 (`/private/tmp/claude-501/p0-shots`, not committed), by eye |
| 6 | `approval needed` attention text is half cut at the very top edge (title-bar strip), dim red on near black | medium | fixed: fixed line box in the strip, only the dot blinks, text steady `ERROR` (5.6:1). Verified with shots at 1400x900, 1100x700, 900x600 (`/private/tmp/claude-501/p0-shots`, not committed), by eye (whole `approval needed` at all three sizes) | approval-1100x700, files-untrusted-900x600 |
| 7 | Long unbroken tokens in chat wrap at the column edge (no overflow); CJK and emoji wrap and align correctly | info | ok | cjk-900x600 |
| 8 | Tools & Skills panel last row is clipped half a line at the panel bottom with no ellipsis or scroll cue | low | fixed: a cut panel body (Tools & Skills, Recent Activity, Workspace, Running) fades out at its bottom edge, no half row. Verified with shots at 1400x900, 1100x700, 900x600 (`/private/tmp/claude-501/p0-shots`, not committed), by eye (tools-1100x700) | tools-1400x900, approval-1100x700 |
| 9 | `--hide` of left panels (`workspace`, `running`) in the hide-all shot left them visible (keys accepted only in the Hidden table for the right column?) | low | explained, not a bug: Workspace and Running are never hideable (owner decision 2026-11-09), `--hide` says so; `--hide-all` added and shot (hideall-1100x700 keeps exactly Workspace, Running, chat) | hideall-1100x700 |
| 10 | Picker rows with unknown metadata read `ctx ? . out ? . vision ? . effort ? . ?` (noisy) | low | fixed: `models::detail` lists only known fields (unit test); shot picker-900x600 has no `?` (the fake model has no metadata) | picker-900x600 |
| 11 | Empty chat (no task) cannot be shot: `--live` needs a prompt (an empty one times out) | tooling | fixed: `--live SCRIPT` without `--prompt` sends nothing; also `--hide-all` and `--bare`; shots emptychat-*, hideall-* | n/a |

## Plan

P0
- Top bar: drop whole chips that do not fit, ending with an ellipsis, instead of clipping mid-glyph. Check: at 900x600 no chip shows a partial glyph.
- Docked dialogs (Trust, approval): reserve the prompt box and make an overflowing body scroll with a visible cue. Check: at 900x600 the prompt is fully visible and the last body line is not cut.
- Attention banner: render inside the title bar strip, fully visible. Check: shot with a pending approval shows the whole `approval needed` text.

P1
- Panel bodies that clip (Tools & Skills, Recent Activity): ellipsis or fade at the clip edge. Check: no half line at a panel bottom.
- Picker: hide `?` fields; show only known metadata. Check: `/model` for a provider without metadata has no `?`.
- Shot harness: allow `--live` with no prompt (empty chat) and a `--hide-all` flag matching `/panels hide-all`. Check: both produce a PNG.
- Contrast pass on dim text (`dim()`) and the red attention colour against the panel background. Check: WCAG AA (4.5:1) for body text.
- Hover, empty and loading states for the file explorer, browser and terminal tabs. Check: a shot of each tab with no data shows a sentence, not a blank.

P2
- CLI parity features not yet in the TUI view (see `docs/tui-feature-port.md`): list there is the source; pick by owner priority. Check: each ported command has a `--cmd` shot test.
- Per-widget min-width table for all header items, tested in one place. Check: one unit test covers every header item at 900, 1100, 1400.
- Real-window pass on Linux and Windows (not run here).

## Status of the plan (2026-11-10, branch `feat/desktop-ui-polish-p0`)

P0 all three and P1 all items done, verified by shots (above) and tests. Details not in the table:
- Contrast: `MUTED` is 4.84:1 on SURFACE but 4.06:1 on ELEVATED (selected rows, open tab), so `chrome::dim` is now `#8E8F93` (4.57:1 on ELEVATED); the terminal TUI keeps `MUTED`. The red `ERROR` is 5.6:1 on SURFACE; the earlier unreadable look was the blink's 0.25-alpha phase, now only the dot blinks. Test `contrast_tests` (dim, TEXT, TEXT_STRONG, ERROR, WARN, ACCENT on BG, SURFACE, ELEVATED), `docs/tui-palette.md` updated.
- Empty states: terminal ("No command ran yet...") and browser ("no page opened yet", "No screenshot saved yet.") already had sentences (shots empty-terminal-*, empty-browser-* with `--bare`). The file explorer in a trusted folder that is not a git repo showed `not a git repository` but rescanned on every frame (the stale check treated any note as the untrusted one): fixed to compare with the untrusted note only. No hover states were added.
- Not done: the Trust dialog at 900x600 still cuts its text and option list mid-line inside their scroll boxes (that is what scrolling is; the cue says so); the chat stage above the dock is nearly empty while it is open at that height.

## Status of P2 (2026-11-10, branch `feat/desktop-p2-polish`)

- Perf, TAIL_REMEASURE: done. `sync_rows` keeps a height signature per chat row (`item_sig`: kind, state flags and text lengths) and re-measures only the rows whose signature moved, wherever they are (no fixed 12-row tail, so a slow parallel tool far up no longer keeps a stale height). Test `only_the_rows_whose_height_inputs_moved_are_measured_again`. Numbers (`--perf 100`, debug build, noisy +-20%, machine busy): before streaming/1500 rows avg 44.8 ms, after 66.6 ms in the same noise band as idle (45 vs 57); the debug harness cannot resolve the difference, no release number was taken (`test-provider` is refused in release).
- Perf, marquee in its own entity: NOT done, and an entity alone would not help. In `gpui-pre 0.3.10` `request_animation_frame` notifies the view that holds the animated element and `mark_view_dirty` marks all its ancestors dirty, and a dirty (even `cached`) ancestor renders again. The marquee sits inside TuiView's tree, so TuiView renders at 30 fps whatever entity wraps it. A real fix needs the animated slot outside TuiView's subtree (a Shell-level overlay positioned from the measured header slot, with the dialog/scrim layering to solve) or splitting TuiView's heavy children (chat list, panels) into `cached` entities. The shot harness does `refresh()` every frame, so it cannot measure either.
- Trust dialog at 900x600 / 1100x700 / 1400x900: done. The text box is cut at a multiple of the line height (paragraph gap is one line, line height `LINE`), the option list at a row edge (`whole_rows` from the previous frame's row bounds), both sized from the window height so the options and the prompt stay visible; the cues stay. Verified by eye on shots in `/private/tmp/claude-501/p2-shots` (trust1-*): no line is cut inside either box at the three sizes. At 900x600 the stage above the dock is a thin strip and shows 3 text lines and 2 options (scroll for the rest); at 1100x700 the explorer note above is still clipped mid-line inside its own box (not touched).
- Header min-width table: done. `HEADER_MIN` / `HeaderItem` / `header_min` / `top_fixed` in `widgets/chrome.rs`; `top_bar` and `chat_head` read it. Test `every_header_item_fits_its_row_at_900_1100_1400`.
- Hover: done in code (tabs, Workspace and Running rows, panel header fold labels, the two "new ..." rows, same white 5% fill as the existing file/diff rows; picker options and chat cards already had it). NOT verified visually: the offscreen harness has no pointer. Empty file explorer now says "No files to list yet: the project has no tracked or new files." (loading and not-loaded sentences already existed).
- CLI parity (from `docs/tui-feature-port.md` and `synthesis_tui::local_commands`; every other local command is already wired): `/quit` = `/exit`, `/perms` = `/permissions` (the terminal TUI's two aliases, previously "unknown command"), `/agents --all|-a` also prints the other sessions' table. Tests in `live.rs` (`slash_commands_are_never_dropped_silently`) and `tests/commands_fake.rs`; shot `--cmd "/perms;;/agents --all"` at 1100x700 shows the permissions report, the agents report and the "peers are off" line.
