# New-TUI features in the classic TUI (stage 7)

Inventory of what the removed "new TUI" (commits after `05063f4` up to `335f590^`: e301eeb, a9a7e68,
b8820c8, 2fd00b0, 3acd7f7, 465d98a, 76cbf05, a8ea4df, 76954af, plus the desktop commit c0dd774) added to
`synthesis-tui*`, and what the classic TUI has now. FUNCTIONAL = behaviour, data, settings, commands;
VISUAL = themes, colours, glyph sets, header art, card and box styling, view chrome, layout geometry.
Rule: every FUNCTIONAL item is ported in the classic look (the "synthesis" palette, classic widgets);
VISUAL items are not brought back. Where the classic had no place for something, the minimum is a
slash command or a settings row: those are marked **new place** for the owner to confirm.

| # | feature | commit | kind | decision | state after the port |
|---|---|---|---|---|---|
| 1 | Theme tokens and the Graphite / Midnight / Pearl / Classic / system themes | e301eeb, a9a7e68, 2fd00b0 | VISUAL | skipped (visual) | classic keeps its renderer; one palette "synthesis" (docs/tui-palette.md) |
| 2 | `/themes` and the saved theme | 2fd00b0 | VISUAL | skipped (visual) | no theme switch; Mono / 16 / 256 colours still follow NO_COLOR, COLORTERM, SYNTHESIS_COLOR |
| 3 | Icon / glyph set setting (`icons`, ascii) | b8820c8, 2fd00b0 | VISUAL | skipped (visual) | not ported |
| 4 | Brand header, topbar art, date in the header | e301eeb, a8ea4df | VISUAL | skipped (visual) | classic title bar kept (`✻ SYNTHESIS`, clock) |
| 5 | Braille logo art and greeting by local hour | e301eeb, a9a7e68 | VISUAL | already present | ported in stage 2 (welcome hero); the setting that turns the art off is row 12 |
| 6 | `SYNTHESIS_COLOR` colour-depth override | e301eeb | FUNCTIONAL | already present | stage 2 (replaced a foreign variable name) |
| 7 | `synthesis.tui` settings in the USER file only | b8820c8, 2fd00b0 | FUNCTIONAL | ported | `synthesis-settings::TuiSettings`: panels, logo, show_thinking, diffs_expanded, queue_indicators, motion, mouse_capture, copy_on_select; project, local and managed files are rejected with a diagnostic; validated with key-only diagnostics (docs/config.md, TUI.md) |
| 8 | `/config` editor | b8820c8 | FUNCTIONAL | ported | classic picker (`/config`): enter flips a row, applies at once and writes `synthesis.tui` of the USER settings.json atomically (other keys and order kept, bad file never overwritten); it can write nothing but those keys, so never a credential |
| 9 | Panel registry, layout planner, geometry rules | b8820c8, 2fd00b0 | VISUAL | skipped (visual) | classic layout kept |
| 10 | Panels on/off with the others re-flowing | b8820c8 | FUNCTIONAL | ported | Session, MCP, Tools & Skills and Activity can be hidden (Workspace, Running and the chat are locked, as in the new TUI); the rest take the rows; text goldens `tests/golden/panels` |
| 11 | Setting: logo art on/off | b8820c8 | FUNCTIONAL | ported | `logo`: the welcome keeps its wordmark and drops the braille art |
| 12 | Setting: show_thinking | b8820c8 | FUNCTIONAL | ported | thinking blocks are not drawn when off |
| 13 | Setting: diffs_expanded | b8820c8 | FUNCTIONAL | ported | long output, code and diffs start in the ctrl+o "whole" state |
| 14 | Setting: queue_indicators | b8820c8 | FUNCTIONAL | ported | the `queued:` lines above the prompt |
| 15 | Setting: motion | b8820c8 | FUNCTIONAL | ported | spinners and the pulse are drawn still when off |
| 16 | Setting: mouse_capture | b8820c8 | FUNCTIONAL | ported | off: no mouse reporting (the terminal's own selection); read at start |
| 17 | Setting: copy_on_select | b8820c8 | FUNCTIONAL | ported | off: drag-select highlights and ctrl+c copies |
| 18 | First-run setup (no model configured) | 2fd00b0 | FUNCTIONAL | ported | `synth` shows a setup card instead of exiting 2: what to put where (key in an environment variable, `{env:NAME}` in synthesis.json, `synth models` / `doctor`), `r` checks again, `q` leaves; nothing is asked for or stored (ADR-011) |
| 19 | Mission centre tab strip (chat / project / diff / terminal), `/tab` | 3acd7f7 | VISUAL | skipped (visual) | the data of each tab is rows 20-23, as commands (**new place**) |
| 20 | Project tree with git status letters | 3acd7f7 | FUNCTIONAL | ported | `/files` (read-only git with the repository config's programs turned off, minimal environment, 5 s limit, one scan at a time, trusted folder only; no git or not a repository is a note) **new place** |
| 21 | `git diff HEAD` view | 3acd7f7 | FUNCTIONAL | ported | `/diff`: a `+a -d path` line per changed file, then the diff **new place** |
| 22 | Terminal tab: the session's shell commands | 3acd7f7 | FUNCTIONAL | ported | `/terminal`: the Bash calls of this session with their status; the output itself stays in the chat rows (ctrl+o) **new place** |
| 23 | Per-file diff cards, per-file navigation | a8ea4df | FUNCTIONAL (cards: VISUAL) | ported (navigation), cards skipped (visual) | `/diff <file>` narrows to one file |
| 24 | File and diff side by side | a8ea4df | VISUAL | skipped (visual) | not ported |
| 25 | Browser request list | a8ea4df | FUNCTIONAL | ported | `/browser`: the Browser*/WebFetch/WebSearch calls of this session; core has no network request log, so that is all there is **new place** |
| 26 | Topology 3x3 cards with links, panel icons, User prompt panel, send button | a8ea4df, 76cbf05 | VISUAL | skipped (visual) | classic topology kept; Enter sends |
| 27 | Bordered USER / reply / tool cards with copy-safe borders | 76cbf05 | VISUAL | already present | classic copy takes the message text, not the frame |
| 28 | Header icon glyphs, footer and prompt aligned with the HTML design | 76cbf05, 465d98a | VISUAL | skipped (visual) | not ported |
| 29 | Effort chip | a8ea4df | FUNCTIONAL | already present | `effort: ...` in the chat footer, `/effort`, alt+t |
| 30 | `/peers` | 76954af | FUNCTIONAL | ported | `WORKSPACE SESSION AGENT KIND STATUS TASK` table from `Harness::peer_rows`; "peers are off" otherwise |
| 31 | `/agents --all` | 76954af | FUNCTIONAL | ported | the session's agents, then the other sessions' table |
| 32 | `/name` | 76954af | FUNCTIONAL | ported | shows the session name peers address; the name is fixed for the run (start with `--name`), as in core |
| 33 | `peers on` footer and peer rows | 76954af | FUNCTIONAL | ported | `peers on · N in flight` chip in the status line |
| 34 | Memory row | 76954af | FUNCTIONAL | ported | `memory on` chip in the status line; the topology Memory card is VISUAL and not ported; `/memory` also names long-term memory |
| 35 | Link and clipboard fixes, right/alt click, OSC 8, relative and wrapped paths | 8e4bc67, df8d05c | FUNCTIONAL | already present | both commits are before 05063f4: in the classic base; hardened in stages 3-4 |
| 36 | Desktop shell: `Front::Native`, `synthesis-app` crate | c0dd774 | FUNCTIONAL (desktop) | already present / out of scope | `synthesis-app` is in main core and in use; the GPUI desktop is not part of this work |
| 37 | Default `--cwd` to the launch directory | ee42760 | FUNCTIONAL | already present | in main core |

Settings of row 7 also have a user-facing description in `docs/TUI.md` and `docs/specs/config.md`.

## Not equal to the new TUI, on purpose

- `/config` applies and saves each flip at once (the new TUI edited a buffer and wrote on close).
- `theme` and `icons` are not accepted in `synthesis.tui` (the diagnostic says "not a setting").
- Tabs, topology cards and every other layout feature of the new TUI stay out; their data is a command.

## Unverified

Only macOS was run. `/files` and `/diff` need `git` on PATH (a note says so when it is missing).
