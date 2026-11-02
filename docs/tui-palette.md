# TUI palette (theme "synthesis")

One built-in palette, defined as tokens in `crates/synthesis-tui-render/src/theme.rs` (the selection
pair lives in `synthesis-tui-base::selection` and is re-exported there). Neutrals are the brand
kit (`design/02-design-tokens/color-schema.md`); the only accent is teal; no violet, indigo or
magenta anywhere (a test checks the hue of every token and of every colour a golden frame emits).
All colours are 24-bit; `sgr::encode` downgrades to 256/16 colours and drops them for `NO_COLOR`.
`SYNT_COLOR=truecolor|256|16` overrides detection, then `NO_COLOR`, `COLORTERM`, `TERM`.
There is no light variant (the old code had none).

Hue is HSV degrees; contrast is WCAG against BG `#0B0C0F` and SURFACE `#15171B`. Text tokens must
reach 4.5 (tests `palette_tests`). Regenerate with
`cargo test -p synthesis-tui-render palette_table -- --nocapture`.

| token | hex | hue | sat | on BG | on SURFACE | use |
|---|---|---|---|---|---|---|
| BG | #0B0C0F | 225 | 0.27 | 1.00 | 1.09 | brand ground (the terminal's own shows) |
| SURFACE | #15171B | 220 | 0.22 | 1.09 | 1.00 | brand panel ground, heat level 0 |
| ELEVATED | #242830 | 220 | 0.25 | 1.32 | 1.21 | brand elevated ground |
| BORDER | #3A3F4A | 221 | 0.22 | 1.85 | 1.70 | rules, tracks (decorative) |
| RULE | #4B5260 | 220 | 0.22 | 2.49 | 2.29 | frame lines (decorative) |
| TEXT | #9CA3AF | 218 | 0.11 | 7.70 | 7.07 | secondary text |
| MUTED | #7F8797 | 220 | 0.16 | 5.42 | 4.97 | hints, placeholders, comments |
| TEXT_STRONG | #E5E7EB | 220 | 0.03 | 15.80 | 14.49 | primary text |
| SILVER | #D9D9E0 | 240 | 0.03 | 13.93 | 12.78 | body text (brand accent) |
| PEARL | #F7F8FA | 220 | 0.01 | 18.41 | 16.89 | highlights (brand accent-light) |
| BEIGE = THINKING | #D7CEC7 | 26 | 0.07 | 12.61 | 11.57 | thinking, plan/edit modes (brand soft-beige) |
| ACCENT | #5EC8B8 | 171 | 0.53 | 9.71 | 8.91 | prompt, links, focus, selection |
| ACCENT_LIGHT | #9AE3D6 | 169 | 0.32 | 13.35 | 12.25 | strong text, logo sheen |
| ACCENT_GLOW | #CDF3EC | 169 | 0.16 | 16.42 | 15.06 | gradients |
| ACCENT_DEEP | #2E8B7E | 172 | 0.67 | 4.76 | 4.37 | frame gradients (not text) |
| SUCCESS | #7BD88F | 133 | 0.43 | 11.23 | 10.31 | ok, plan mode |
| WARN | #E5B567 | 37 | 0.55 | 10.37 | 9.52 | warnings, strings |
| ERROR | #F26D6D | 0 | 0.55 | 6.69 | 6.14 | errors |
| DIFF_ADD | #9ECE6A | 89 | 0.49 | 10.70 | 9.82 | added lines |
| DIFF_REMOVE | #E8906A | 18 | 0.54 | 8.04 | 7.37 | removed lines, bash mode |
| INFO | #7DB4E6 | 209 | 0.46 | 8.89 | 8.15 | MCP, permission asks |
| CYAN | #6CCFE0 | 189 | 0.52 | 10.84 | 9.95 | inline code, storage keywords |
| SELECT_BG | #1F4D49 | 175 | 0.60 | 2.06 | 1.89 | selected text ground (SELECT_FG = PEARL, 8.9:1 on it) |

Syntax colours (`SYN_*`) and the sub-agent/context colours (`AGENT_COLORS`) are drawn from these
tokens. An agent definition that asks for the colour `purple` gets ACCENT_LIGHT, `pink` BEIGE.

The welcome (`logo.rs`, regenerated from the brand symbol by `tools/logo-art/gen.py`) uses MUTED,
TEXT, ACCENT, ACCENT_LIGHT, PEARL as its sheen; on 16-colour and colourless terminals the art is
drawn in ACCENT only.

Goldens: `SYNT_UPDATE_GOLDEN=colour cargo test -p synthesis-tui-render --test golden` rewrites
colours of a case only when its plain text and structure are identical.
