# TUI palette (theme "synthesis")

One built-in palette, defined as tokens in `crates/synthesis-tui-render/src/theme.rs` (the selection
pair lives in `synthesis-tui-base::selection` and is re-exported there). Neutrals are the brand
kit (`design/02-design-tokens/color-schema.md`) as in the owner's platinum design (Background
#0B0D0F, Surface #17191D, Border #393A3E, Platinum #D8D8D8, Silver #A6A6A8, Text #F2F2F2, Muted
#85868A); the only accent is the warm sand #C7AE90 (focused border, selected-row marker, cursor,
ready dot, logo tint); status colours are desaturated; no green/teal/blue dominant, no violet,
indigo or magenta anywhere (a test checks the hue of every token and of every colour a golden frame emits).
All colours are 24-bit; `sgr::encode` downgrades to 256/16 colours and drops them for `NO_COLOR`.
`SYNT_COLOR=truecolor|256|16` overrides detection, then `NO_COLOR`, `COLORTERM`, `TERM`.
There is no light variant (the old code had none).

Hue is HSV degrees; contrast is WCAG against BG `#0B0D0F` and SURFACE `#17191D`. Text tokens must
reach 4.5 (tests `palette_tests`). Regenerate with
`cargo test -p synthesis-tui-render palette_table -- --nocapture`.

| token | hex | hue | sat | on BG | on SURFACE | use |
|---|---|---|---|---|---|---|
| BG | #0B0D0F | 210 | 0.27 | 1.00 | 1.11 | brand ground (the terminal's own shows) |
| SURFACE | #17191D | 220 | 0.21 | 1.11 | 1.00 | brand panel ground, heat level 0 |
| ELEVATED | #26282D | 223 | 0.16 | 1.32 | 1.19 | brand elevated ground |
| BORDER | #393A3E | 228 | 0.08 | 1.71 | 1.55 | rules, tracks (decorative) |
| RULE | #4E4F54 | 230 | 0.07 | 2.38 | 2.15 | frame lines (decorative) |
| TEXT | #A6A6A8 | 240 | 0.01 | 8.01 | 7.24 | secondary text |
| MUTED | #85868A | 228 | 0.04 | 5.35 | 4.84 | hints, placeholders, comments |
| TEXT_STRONG | #F2F2F2 | 0 | 0.00 | 17.39 | 15.72 | primary text |
| SILVER | #D8D8D8 | 0 | 0.00 | 13.66 | 12.35 | body text (brand accent) |
| PEARL | #F8F8F8 | 0 | 0.00 | 18.33 | 16.57 | highlights (brand accent-light) |
| BEIGE / THINKING | #D2CCC6 | 30 | 0.06 | 12.23 | 11.06 | thinking, plan/edit modes (brand soft-beige) |
| ACCENT | #C7AE90 | 33 | 0.28 | 9.16 | 8.28 | prompt, links, focus, selection |
| ACCENT_LIGHT | #E2D2BE | 33 | 0.16 | 13.16 | 11.90 | strong text, logo sheen |
| ACCENT_GLOW | #F2E8DA | 35 | 0.10 | 16.07 | 14.52 | gradients |
| ACCENT_DEEP | #806C54 | 33 | 0.34 | 3.89 | 3.51 | frame gradients (not text) |
| SUCCESS | #5CBF7E | 141 | 0.52 | 8.54 | 7.72 | ok, plan mode |
| WARN | #E4A44A | 35 | 0.68 | 9.00 | 8.14 | warnings, strings |
| ERROR | #E46C68 | 2 | 0.54 | 6.16 | 5.57 | errors |
| DIFF_ADD | #80B884 | 124 | 0.30 | 8.44 | 7.63 | added lines |
| DIFF_REMOVE | #D88070 | 9 | 0.48 | 6.71 | 6.07 | removed lines, bash mode |
| INFO | #82A2C4 | 211 | 0.34 | 7.34 | 6.63 | MCP, permission asks |
| CYAN | #80B6BC | 186 | 0.32 | 8.65 | 7.82 | inline code, storage keywords |
| SELECT_BG | #463C32 | 30 | 0.29 | 1.81 | 1.64 | selected text ground (SELECT_FG = PEARL, 9.1:1 on it) |

The desktop app's TUI view draws its dim text (`chrome::dim`, `apps/desktop/src/widgets/chrome.rs`) in `#8E8F93`,
`MUTED` lifted by about 9 levels, because `MUTED` is 4.06:1 on ELEVATED (the ground of selected rows and the open tab);
`#8E8F93` is 4.57:1 there and 6.03:1 on BG. The terminal TUI keeps `MUTED`. A desktop unit test
(`contrast_tests`) checks dim, TEXT, TEXT_STRONG, ERROR, WARN and ACCENT at 4.5:1 on BG, SURFACE and ELEVATED.

Syntax colours (`SYN_*`) and the sub-agent/context colours (`AGENT_COLORS`) are drawn from these
tokens. An agent definition that asks for the colour `purple` gets ACCENT_LIGHT, `pink` BEIGE.

The welcome (`logo.rs`, regenerated from the brand symbol by `tools/logo-art/gen.py`) uses MUTED,
TEXT, ACCENT, ACCENT_LIGHT, PEARL as its sheen; on 16-colour and colourless terminals the art is
drawn in ACCENT only (warm sand).

Goldens: `SYNT_UPDATE_GOLDEN=colour cargo test -p synthesis-tui-render --test golden` rewrites
colours of a case only when its plain text and structure are identical.
