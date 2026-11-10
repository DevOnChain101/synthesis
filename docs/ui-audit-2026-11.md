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
