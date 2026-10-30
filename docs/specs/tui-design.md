# Spec: TUI redesign (brand theme, panel registry, /config) — draft 0.1

Date 2026-10-29. Status: **accepted: the owner took all recommendations D1–D11 on 2026-10-30** (see §13). P1 and P2 are implemented (P2: 2026-10-30, goldens pending owner review, see §15); P3 and P4 are not started. **Later the same day the owner supplied two reference mockups; §14 is authoritative and overrides this document where they disagree** (default theme, default panel visibility, the title, the greeting). Tags: `[fact file:line]` = read in the repo, `[assumption]` = not verified, `[derived]` = a value this spec adds that is not in the kit.

**Inputs:** `design/02-design-tokens/tokens.css`, the desktop and landing previews, and `design/01-brand/logo/png/synthesis-symbol-transparent-256.png`. The brand boards (`04`) were never copied into the repo (`design/README.md` "Not included"), so they were not used.

**Current UI:** read from `crates/synthesis-tui-render/src/{dash,theme,agent}.rs` and `crates/synthesis-tui-base/src/sgr.rs`. Real frames were taken from the goldens `frame-agent-chat-{80x24,120x40,200x50}` and `frame-agent-permission-120x40`.

**Not run:** `synth --once` and `tests/tty-qa` captures (the design session had no shell). Frames at 100x30 and 140x40 were not seen. Every mockup here is hand-drawn. The contact sheet from §8.4 becomes the reference once the code exists.

## 0. Problem and non-negotiables

**Problem.** The TUI does not look like the Synthesis brand. It has heavy `┏━┓` boxes, an indigo/cyan gradient title pill, eight saturated hues and shimmer animations (`theme.rs:10-65`, `dash.rs:301-345`, `dash.rs:2398-2423`). The layout cannot be configured.

**Current structure** `[fact dash.rs:1462-3307]`:
- **Left column** (`side_column`): Workspace, Running, Session, MCP, Tools & Skills, Activity.
- **Centre** (`agent_detail`): a row with Status, Tokens and Tasks, then Usage, then the chat.
- **Right column** (`topology_column`): Topology, a sub-agent dock that appears only while a sub-agent is being viewed, then Tasks and Usage.
- **Layout choice:** `layout_for` picks one of three layouts (Right, Bottom, None).
- **Bottom:** two rows, the key-hint footer and the status line from `app.rs`.
- **Colour:** about 250 call sites read RGB constants. All output is 24-bit, and `sgr::encode` downgrades it for smaller palettes.
- **The app always passes an agent** (`app.rs` `render`). The dashboard layout without an agent is only exercised by the `dash-*` goldens.

**Non-negotiables (owner rules):**
- Workspace, Running and Chat are never hidden.
- Every other panel can be hidden from `/config`.
- When all optional left panels are hidden, Workspace and Running split the left column 50/50.
- The sub-agent transcript always lives in the right sidebar.
- Panels reflow with no holes.
- Below 100 columns the sidebars become a switcher. Below 60 columns the screen is chat only.
- Settings persist in the USER file only.
- The design works on macOS, Linux and Windows, and degrades through truecolor, 256, 16 and `NO_COLOR`.
- Credentials and other products' environment variables are untouched (ADR-011).

## 1. Options and choice

| Axis | Options | Pick |
|---|---|---|
| **Theme mechanism** | **A1** Pass `&Theme` through every render function. Explicit, but about 250 call sites and many signatures change.<br>**A2** Semantic token functions (`t().body`) read a theme that `render_frame`, `render_agent` and `transcript_rows` set for the duration of one call, through a thread-local scope guard.<br>**A3** Renderers keep fixed "sentinel" RGB values and `sgr::encode` swaps them for theme colours. Smallest diff, but colours mixed by `lerp` match no table entry and the mapping becomes magic. | **A2** |
| **Layout** | **B1** Add visibility flags to the three existing layouts. The sizing logic is spread across `side_column`, `topology_column` and `agent_detail` with ad-hoc constants, and the owner's rules do not fit it.<br>**B2** A pure planner (`layout.rs`) plus a static panel registry. Each panel renders by id and reuses the existing panel functions. | **B2** |
| **Panel chrome** | **C1** Rounded light boxes. Each costs 2 rows and 2 columns.<br>**C2** Borderless sections: a label row, one blank row between sections, and one `│` rule between columns. Frames are kept for floating elements only (prompt, dialogs, pickers, `/config`). | **C2**. It is closer to the kit (1 px lines, generous space) and saves about 2 rows per panel. |
| **Logo** | **L1** Braille line art generated offline from the PNG, with per-cell truecolor sheen.<br>**L2** Half-block art (`▀▄`). Chunkier.<br>**L3** Inline image (iTerm2, kitty, sixel). Exact, but only some terminals support it, tmux breaks it, and the diff renderer can erase it. | **L1** as the default. L3 is optional, off by default, and limited to the welcome screen. |

**Strongest reason against this choice.** It moves Status, Tokens and Tasks out of the centre column, which is a visible change for current users. It also rebaselines nearly every render golden while another developer is editing `terminal.rs`, `e2e.rs` and `app.rs`. The `app.rs` line numbers moved while this spec was being written. The phase gates in §8.4 contain this risk.

**Reason against A2 specifically.** It adds hidden per-thread state to a crate that is documented as pure. Each entry point sets the scope explicitly, tests set it explicitly, and the default is Synthesis (§14.7).

## 2. Design principles

1. **Graphite, silver, one pearl.** Grey carries the hierarchy. The warm pearl marks focus and the logo. Status colours appear only for status.
2. **Space instead of lines.** One gap row between panels, a 2-column chat gutter, light rules, never heavy borders.
3. **One focus.** The focused element gets a pearl `▍` bar and an accent label.
4. **Never colour alone.** Every state has a glyph or a word, so the UI still reads in Mono and in 16 colours.
5. **Quiet motion.** One braille spinner per active item. The shimmer, gradient borders and pulses go (`pulse`, `cycle`, the shimmer in `bar`, `BORDER_GRAD`, `LOGO_GRAD`). With the Motion setting off, spinners become static glyphs.
6. **Respect the terminal.** The global background is never painted `[assumption: users who choose Graphite run a dark terminal]`. Only the header band, overlays, selection and diff lines are painted.

## 3. Palette and token mapping

Contrast is the WCAG ratio against `#0B0C0F` (the kit background). Each 256-colour index is what the existing `to256` produces automatically, so no override table is needed. In 16-colour mode a dedicated ANSI theme is used, and its RGB values match the ANSI16 table exactly, so `to16` maps them one to one.

| Token | Graphite (truecolor) | Source | Contrast | 256 | 16-colour | Mono |
|---|---|---|---|---|---|---|
| `bg_ref` (not painted) | `#0B0C0F` | kit bg | — | 232 | default | default |
| `surface` (header band, toasts) | `#15171B` | kit | — | 234 | none | none |
| `elevated` (dialogs, selection) | `#242830` | kit | — | 235 | none | reverse (selection) |
| `border` (rules, frames) | `#3A3F4A` | kit | 1.84 (decorative) | 238 | 8 | dim |
| `label` (uppercase section labels) | `#8C929B` | desktop.html `.micro` | 6.20 | 246 | 7 | dim |
| `muted` (times, hints, folders) | `#7A808C` | [derived] | 4.90 (4.53 on surface) | 244 | 8 | dim |
| `body` (chat text, rows) | `#C8CCD2` | [derived] | 12.05 | 252 | 7 | default |
| `strong` (values, user turn) | `#E5E7EB` | kit text-strong | 15.69 | 254 | 15 | bold |
| `accent` (active label, wordmark) | `#D9D9E0` | kit accent | 13.82 | 253 | 15 | bold |
| `accent_light` (logo highlight) | `#F7F8FA` | kit | 18.29 | 231 | 15 | bold |
| `pearl` (focus bar, caret, spinner) | `#D7CEC7` | kit soft-beige | 12.53 | 252 | 15 | bold |
| `ok` | `#A8C5B2` | desktop.html `.tick` | 10.45 | 151 | 2 | glyph ✓ |
| `warn` | `#D8B46A` | [derived] | 9.84 | 179 | 3 | glyph ! |
| `err` | `#E08A8A` | [derived] | 7.56 | 174 | 1 | glyph ✗ + bold |
| `info` (MCP, permission, links) | `#8EA4C8` | [derived] | 7.67 | 110 | 6 | underline (links) |
| `diff_add_bg` | `#14241B` | [derived] | body on it 10.03 | 234 | none | `+` glyph |
| `diff_del_bg` | `#2A1618` | [derived] | body on it 10.62 | 234 | none | `−` glyph |

Text on painted surfaces: `strong` on `elevated` is 11.93, `body` on `surface` is 11.1 (computed), and the kit's own `text` `#9CA3AF` on `elevated` is 5.82.

**Theme variants.** `/config` offers the first four. `ansi16` is chosen automatically.
- **Graphite**: the table above, with the accent, body and strong values revised on 2026-10-30 (§14.7); no longer the default.
- **Midnight:** the header band and overlays use kit muted-blue `#0B1322` and elevated `#16213A`. Border `#2C3A55`, body `#C3CBD9`, muted `#7E8AA3` (5.60), accent unchanged, pearl becomes ice `#BFD3F2`. In 256-colour mode the blue collapses to grey 233, so Midnight looks almost like Graphite there.
- **High contrast:** body `#F2F3F5`, muted `#B8BDC6`, border `#8C929B` (6.2, so it meets the 3:1 rule for UI components), ok `#9BE7B0`, warn `#FFD479`, err `#FF9C9C`. `strong` is bold and selection is bold on `elevated`.
- **Mono:** no colour at all. Strong is bold, muted and label are dim (SGR 2), selection is reverse, and focus is `▍` plus bold. `sgr` Mono mode already keeps attributes `[fact sgr.rs:17]`.
- **ansi16** (automatic): the slots in the 16-colour column above.

**Light terminals.** Graphite on a white background falls to 1.6–2.5:1, which is unreadable. There is no detection in P1. The user has two options:
- **Mono:** works on any background.
- **Pearl (light):** proposed in decision D5. Text `#3A3F4A` (9.9 on `#F7F8FA`), strong `#15171B` (16.9), muted `#5B616E` (5.85).

Automatic detection would need an OSC 11 query at startup in `terminal.rs`. That is a P4 option and needs a decision.

**Selection rule:**

```
mode  = ColorMode::detect(env)                 // existing, sgr.rs:28
theme = Mono            if mode == Mono        // NO_COLOR always wins; /config shows "NO_COLOR is set"
        ansi16          if mode == Ansi16
        user.theme      otherwise              // default synthesis
glyphs = user.glyphs, else ascii when TERM in {linux, vt100, vt220, cons25}, else unicode
```

**Contrast test.** A unit test in `theme.rs` computes the WCAG ratio of every theme against its reference background and asserts these floors:

| Pair | Floor |
|---|---|
| body, strong | ≥ 7 |
| label, muted, ok, warn, err, info | ≥ 4.5 |
| body on diff backgrounds | ≥ 7 |
| border | ≥ 1.5 (≥ 3 for High contrast) |

**Mapping from today's constants:**

| Today | Becomes |
|---|---|
| `T_TEXT` | strong |
| `T_BODY` | body |
| `T_INACTIVE` | label |
| `T_SUBTLE` | muted |
| `T_BORDER` | border |
| `T_CLAUDE`, `ACCENT_CORE` | accent |
| `ACCENT_LIGHT`, `ACCENT_GLOW` | accent_light |
| `T_SUCCESS` | ok |
| `T_WARNING` | warn |
| `T_ERROR` | err |
| `T_PERMISSION`, `T_MCP` | info |
| mode colours (`T_AUTO_ACCEPT`, `T_PLAN_MODE`, `T_BASH`) | label; the mode is named in text |
| `AGENT_COLORS` (8 hues) | 4 tints: ok, info, sand `#CDBB9A` [derived], pearl |
| `HEAT` (green ramp) | silver ramp `#1B1E24 #3A3F4A #6B717C #9CA3AF #E5E7EB` |
| `BORDER_GRAD`, `LOGO_GRAD` | removed |

**Unrelated finding.** `sgr.rs:30` reads `CLAUDE_MONITOR_COLOR`. That breaks the environment-variable rule (only `SYNTHESIS_*` plus standard variables may be read). It should be renamed to `SYNTHESIS_COLOR` or removed; see follow-up F1.

## 4. Typography, spacing, glyphs

**Wordmark.**
- 80 columns and wider: `S Y N T H E S I S` in `accent_light`.
- 60–79 columns: `SYNTHESIS`.
- Below 60: the mark only.
- Welcome screen: the art, the spaced wordmark, and "Autonomous development environment" in `label`.

**Text.**
- Section labels are UPPERCASE in `label`, without letter spacing. Counts sit right-aligned in `strong`.
- Graphite and Midnight use no bold, following the Node rule `BOLD_ON = false`. Only High contrast and Mono use attributes.
- Tokens become `Style { fg, bg, bold, dim, reverse, italic }`, and `paint_style` replaces `paint` where attributes matter.

**Spacing.**
- 1 column of padding inside sidebar sections.
- 2-column chat gutter.
- Inside frames: 1 column of padding, plus 1 blank row above and below dialog content.
- 1 blank row between chat turns.

**Lines.**
- Frames: rounded light `╭─╮│╰╯` in `border`.
- Column rules: `│`.
- Separators inside a panel: `┄`.
- No `┏━┓` and no `━` rule anywhere.

**Key hints.** The key is in `body` and the action in `muted`, three spaces apart. Hints that do not fit are dropped whole by priority, using the existing `fit_items` (`theme.rs:208`).

| Use | Unicode | ASCII |
|---|---|---|
| mark | `⢎⡱` (braille ring) | none |
| spinner | `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` | `\| / - \` |
| live / on | `●` | `*` |
| off / idle | `○` | `o` |
| needs you (approval, question) | `◆` | `!` |
| success / failure | `✓` / `✗` | `+` / `x` |
| tool row | `◇` | `>` |
| user prompt | `›` | `>` |
| thinking | `∴` | `:` |
| focus / selection | `▍` | `>` |
| locked | `▣` | `#` |
| meter / bar | `▰▱` / `▇` | `#-` / `#` |
| diff | `+` `−` | `+` `-` |
| branch | `⎇` | `git:` |
| separators | `·` `│` `┄` | `-` `\|` `-` |

**Wide-character rule.** Every glyph must have `unicode-width` 1. Emoji-presentation code points (U+1F300 and above, or anything with VS16) are banned, and a unit test enumerates the set. Ambiguous-width glyphs such as `●○◆▣·▍` are already used today, but they take two cells in terminals set to treat ambiguous width as wide (common with CJK locales). The ASCII set (`/config` → Glyphs) is the fallback for those terminals.

**Logo.**
- The header mark is `⢎⡱`: the left cell in `accent`, the right in `pearl`.
- The welcome art is braille, about 18×6 cells, with per-cell colour running from accent through accent_light to pearl to imitate the sheen.
- The art is generated offline by `tools/logo-art/gen.py` (Python standard library only, `zlib` for PNG) from the 256 px PNG, and committed as a `const` table in `logo.rs`. Nothing is decoded at runtime.
- In 16-colour and Mono the same dots are drawn in one colour. With the ASCII glyph set, only the wordmark is shown.
- Inline image (optional, L3): only when `TERM_PROGRAM` is `iTerm.app` or `WezTerm` (iTerm2 protocol), or `TERM` is `xterm-kitty` or `xterm-ghostty` (kitty protocol). Never inside tmux or screen (`TERM` starting with `screen` or `tmux`). Sixel is left out because it needs a DA1 round-trip. A 64 px PNG is embedded with `include_bytes!`.

## 5. Layout

### 5.1 Regions

| Region | Rule |
|---|---|
| Header | 1 row, or 2 when H ≥ 30 (adds a thin `─` rule). Left: mark, wordmark, project, branch. Right chips, in drop order: cost, waiting count, live/busy sessions, clock. |
| Tab strip | Tabs mode only. 1 row: `Chat · Sessions n · Agents n · Panels n`. |
| Bottom bar | 1 row. Merges the current footer and the status line: key hints on the left, sandbox · budget · verification on the right. Toasts replace it, as today. |
| Columns mode (W ≥ 100) | Left sidebar, chat, right sidebar (§6). |
| Tabs mode (60 ≤ W < 100) | One view at a time. **Sessions** = Workspace + Running (always split 50/50). **Agents** = the sub-agent panel at full height. **Panels** = the visible optional panels (left ones, then right ones) stacked with the same fit rules; the tab disappears when none are visible. **Chat** is the default view, and a pending approval switches back to it. **Tab** cycles the views (it reuses today's focus cycle); a mouse click on a tab selects it. |
| Chat only (40 ≤ W < 60) | No tab strip. `/config` still works and notes that panels appear at 60 columns and wider. The new minimum size is 40×12, down from 60×16 (`dash.rs:2730`). |

### 5.2 Mockups

80×24 (Tabs mode, Chat view):

```
 ⢎⡱  S Y N T H E S I S   proj ⎇ main                         ● 2 live  │  12:00
  ▍Chat    Sessions 2    Agents 1    Panels 3                      tab switches
  › fix the flaky login test
  ∴ thought for 2s · ctrl+o to show
  I will check login.ts and retry():
    • first
    • second
  ◇ Bash   npm test                                            ✗ exit 1 · 1.2s
    │ FAIL login.test.ts
    │ expected 2
  ◇ Read   src/login.ts                                                running

  ⠹ Conjuring…  4s · ↓ 0 tokens · esc to interrupt






  ╭──────────────────────────────────────────────────────────────────────────╮
  │ › hello▏                                                                 │
  ╰──────────────────────────────────────────────────────────────────────────╯
  manual · ⇧tab to cycle                     sonnet-4.5 · effort auto · ctx 3%
 enter send   esc interrupt   ⇧tab mode   /config   ctrl+c ×2 quit
```

120×36 (Columns mode: L=22, chat 68, R=30; default panels):

```
 ⢎⡱  S Y N T H E S I S    proj  ⎇ main                                                      ● 2 live · 1 busy  │  12:00
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 WORKSPACE         1 │  › fix the flaky login test                                        │ STATUS
 ▍proj             2 │                                                                    │ ⠹ running tool        9m54s
                     │  ∴ thought for 2s · ctrl+o to show                                 │   explorer · find files
 RUNNING           2 │                                                                    │
 ▍⠹ proj-0       10% │  I will check login.ts and retry():                                │ CONTEXT                  3%
    project-0        │    • first                                                         │ ▱▱▱▱▱▱▱▱▱▱▱▱  5.9k / 200k
  ● proj-1       10% │    • second                                                        │ in 9.0k · out 1.5k · $ –
    project-1        │                                                                    │
                     │  ◇ Bash   npm test                                ✗ exit 1 · 1.2s  │ TASKS                   1/3
 SESSION             │    │ FAIL login.test.ts                                            │ ✓ Read
 model  sonnet-4.5   │    │ expected 2                                                    │ ⠹ Fixing
 mode   manual       │  ◇ Read   src/login.ts                                    running  │ ○ Test
 effort auto         │  ◇ Edit   src/login.ts                                      +2 −1  │
 branch main         │     12     const r = await retry(fn, {                             │ SUB-AGENTS         1 active
                     │     13 −   tries: 2,                                               │ ▍⠹ explorer  find files  9m
 MCP               1 │     13 +   tries: 3,                                               │ ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄
 ● github   12 tools │     14 +   backoff: 50,                                            │ › find files matching auth
                     │     15     })                                                      │ ◇ Grep  "login"     14 hits
                     │                                                                    │ ◇ Read  src/login.ts
                     │  ⠹ Conjuring…  4s · ↓ 0 tokens · esc to interrupt                  │ Two candidates: login.ts,
                     │                                                                    │ retry.ts. Checking retry…
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │                                                                    │
                     │  ╭──────────────────────────────────────────────────────────────╮  │
                     │  │ › hello▏                                                     │  │
                     │  ╰──────────────────────────────────────────────────────────────╯  │
                     │  manual · ⇧tab to cycle         sonnet-4.5 · effort auto · ctx 3%  │
 enter send   esc interrupt   ⇧tab mode   /config panels   ctrl+c ×2 quit              budget 3/50 turns  ·  unverified
```

200×50 (L=32, chat 120, R=48). All left panels and Status, Context, Tasks, Activity and Usage are on. Rows 25–30 are cut.

```
 ⢎⡱  S Y N T H E S I S    proj  ⎇ main                                                                                                            ● 2 live · 1 busy  │  ! 1 waiting  │  $0.42  │  12:00
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 WORKSPACE                   1 │  › fix the flaky login test                                                                                            │ STATUS
 ▍proj                       4 │                                                                                                                        │ ⠹ running tool                          9m54s
                               │  ∴ thought for 2s · ctrl+o to show                                                                                     │   explorer · find files
 RUNNING                     4 │                                                                                                                        │ 5h quota  ▰▰▰▱▱▱▱▱▱▱▱▱  23%  ↻ 2h10m
 ▍⠹ proj-0                 10% │  I will check login.ts and retry():                                                                                    │
    project-0 · main           │    • first                                                                                                             │ CONTEXT                                    3%
  ● proj-1                 10% │    • second                                                                                                            │ ▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱  5.9k / 200k
    project-1 · parser         │                                                                                                                        │ in 9.0k · out 1.5k · cache 20.0k · $0.42
  ● proj-2                 48% │  ◇ Bash   npm test                                                                                    ✗ exit 1 · 1.2s  │
    project-2 · main           │    │ FAIL login.test.ts                                                                                                │ TASKS                                     1/3
  ○ proj-3                     │    │ expected 2                                                                                                        │ ✓ Read
    project-3 · main           │  ◇ Read   src/login.ts                                                                                     ✓ 42 lines  │ ⠹ Fixing
                               │                                                                                                                        │ ○ Test
 SESSION                       │  ◆ explorer   find files matching auth                                                             running in sidebar  │
 model   sonnet-4.5            │                                                                                                                        │ SUB-AGENTS                           1 active
 mode    manual                │                                                                                                                        │ ▍⠹ explorer   find files                   9m
 effort  auto                  │                                                                                                                        │ ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄
 branch  main                  │                                                                                                                        │ › find files matching auth
 uptime  1m00s                 │                                                                                                                        │ ◇ Grep  "login"                       14 hits
 turns   1 prompts · 5 tools   │                                                                                                                        │ ◇ Read  src/login.ts
                               │                                                                                                                        │ Two candidates: login.ts, retry.ts.
 MCP                         2 │                                                                                                                        │ Checking retry…
 ● github             12 tools │                                                                                                                        │
 ⠹ linear           connecting │                                                                                                                        │
┆ body rows 25–30 cut: TOOLS & SKILLS starts on row 26 (left); the sub-agent transcript continues to row 29 (right)
 TodoWrite  ▇▇▇▇▇▇▇▇▇▇  1      │  ◇ Edit   src/login.ts                                                                                          +2 −1  │ ACTIVITY
 skills  scout ×1              │     12     const r = await retry(fn, {                                                                                 │ 11:50:00  › refactor the parser
                               │     13 −   tries: 2,                                                                                                   │ 11:50:01  ◇ Read src/parser.rs
                               │     13 +   tries: 3,                                                                                                   │ 11:50:04  ◇ Bash npm test
                               │     14 +   backoff: 50,                                                                                                │ 11:50:06  ✗ exit 1
                               │     15     })                                                                                                          │ 11:52:10  ◆ explorer started
                               │                                                                                                                        │
                               │  The retry now waits 50 ms between attempts; running the suite again.                                                  │
                               │                                                                                                                        │ USAGE                         Oct 2026 · 1.0k
                               │  ⠹ Conjuring…  4s · ↓ 0 tokens · esc to interrupt                                                                      │   M T W T F S S
                               │                                                                                                                        │   · · · · ▪ ■ ·
                               │                                                                                                                        │   ■ ▪ ■ ■ ■ · ·
                               │                                                                                                                        │   ■ ■ ■ ▪ ■ · ·
                               │  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮  │   ▪ ■ ■ ■ ■ · ·
                               │  │ › hello▏                                                                                                         │  │   ■ ■ ▪ ■ · · ·
                               │  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯  │
                               │  manual · ⇧tab to cycle                                                             sonnet-4.5 · effort auto · ctx 3%  │   less ▪ ■ more
 enter send   esc interrupt   ⇧tab mode   /config panels   ctrl+r history   ctrl+o expand   ctrl+c ×2 quit                                              sandbox on  ·  budget 3/50 turns  ·  unverified
```

### 5.3 Chat details

| Element | Style |
|---|---|
| User turn | `›` and the text in `strong`. No box. |
| Assistant text | `body`, rendered with the existing markdown and `highlight`. |
| Thinking | Collapsed: `∴ thought for 2s · ctrl+o to show` in `muted`. Expanded: italic `muted` lines behind a `┆` gutter. |
| Tool row | `◇ Name   target`, with Name in `label` and target in `body`. A right-aligned outcome: spinner while running, then `✓ summary` (`ok`) or `✗ exit n` (`err`). Output sits behind a `│` gutter in `muted`, 3 lines when collapsed (ctrl+o, as today). |
| Sub-agent launch | `◆ name   task   running in sidebar`, with `◆` in `pearl`. |
| Diff | Line numbers in `muted`, then the sign. Added lines on `diff_add_bg` with the sign in `ok`; removed lines on `diff_del_bg` with the sign in `err`. Collapsed unless "Expand diffs" is on. |
| Working line | Spinner, verb, elapsed time, tokens, `esc to interrupt`. |
| Queue | `+2 queued` in `muted`, above the prompt. |
| Prompt | Rounded frame in `border`, caret `▏` in `pearl`. In bash mode the frame turns `info` and the prefix is `!`. |
| Footer (inside the chat) | Left: the mode. `bypass` is the only place a mode is shown in `err`. Right: model · effort · context %. |

### 5.4 Overlays

All overlays sit in the chat column in Columns mode (so the sidebars stay visible) and take the full width in Tabs and chat-only modes. They are framed in `border` with `elevated` text colours. The background is painted only in High contrast.

```
╭─ PERMISSION ────────────────────────────────── expires 4:52 ─╮
│                                                              │
│  Edit  src/login.ts                                   +2 −1  │
│                                                              │
│   12     const r = await retry(fn, {                         │
│   13 −   tries: 2,                                           │
│   13 +   tries: 3,                                           │
│   14 +   backoff: 50,                                        │
│                                                              │
│  Make this edit to src/login.ts?                             │
│ ▍1  Yes                                                      │
│  2  Yes, and allow edits in src/ for this session            │
│  3  No, tell Synthesis what to do instead                    │
│                                                              │
│  ↑↓ choose  enter confirm  esc reject                        │
╰──────────────────────────────────────────────────────────────╯

╭─ BYPASS PERMISSIONS ─────────────────────────────────────────╮   (the only frame drawn in warn)
│  Bypass skips permission prompts for this session.           │
│  Deny rules and protected paths still apply.                 │
│                                                              │
│  enter enable bypass        esc keep auto mode               │
╰──────────────────────────────────────────────────────────────╯

╭─ RESUME ─────────────────────────────────────────── 12 saved ─╮
│  filter  log▏                                                │
│                                                              │
│ ▍fix the flaky login test      proj · main     2h ago        │
│  refactor the log parser       proj · parser   1d ago        │
│  add login rate limiting       api · main      3d ago        │
│                                                              │
│  ↑↓ choose  enter resume  esc cancel                         │
╰──────────────────────────────────────────────────────────────╯
```

The welcome screen (chat column, empty session) shows hand-drawn braille standing in for the generated art:

```
            ⢀⣀⣤⣤⣤⣤⣀⡀
         ⢀⣴⡿⠛⠉⠁  ⠈⠉⠻⣷⣄
        ⣰⡿⠋   ⣠⣶⣦⡀   ⠙⢿⣆
        ⣿⡇  ⢠⡾⠋ ⠙⢷⡄  ⢸⣿
        ⠹⣷⣄⣠⡿⠁   ⠈⢿⣄⣠⣾⠏
          ⠉⠛⠛⠋      ⠙⠛⠛⠉
        S Y N T H E S I S
  Autonomous development environment
       1.0.0 · /work/proj

  Ask Synthesis to explain, fix or build something here.
  /  commands     @  files     !  shell     /config  panels
```

## 6. Panel registry and reflow

### 6.1 Registry

The registry is a static table in `layout.rs`. **Drop** is the order in which panels are dropped when space runs out: 1 goes first; "never" means the panel is locked.

| id | Title | Side | Default | Hideable | min h | want h | max h | flex (weight) | Drop | Content source (existing function) |
|---|---|---|---|---|---|---|---|---|---|---|
| `workspace` | WORKSPACE | L | on | **locked** | 2 | 1+n | max(2, H/4) | 50/50 rule | never | `workspace_panel` |
| `running` | RUNNING | L | on | **locked** | 3 | 1+2n (+arrows) | ∞ | yes (1) | never | `sessions_panel` |
| `session` | SESSION | L | on | yes | 4 | 1+rows | 9 | no | 3 | `session_info_panel` / `_essential` |
| `mcp` | MCP | L | on | yes | 2 | 1+servers | 8 | no | 2 | `mcp_panel` / `mcp_summary_line` |
| `tools` | TOOLS & SKILLS | L | off | yes | 3 | 1+tools(+2) | 10 | no | 1 | `tools_panel` |
| `chat` | — | C | on | **locked** | — | — | — | the whole centre | never | `agent_panel` |
| `status` | STATUS | R | on | yes | 3 | 1+2(+quotas) | 6 | no | 6 | `status_panel`, without its context bar |
| `context` | CONTEXT | R | on | yes | 3 | 3–4 | 4 | no | 5 | context bar + `token_panel` (this replaces Tokens) |
| `tasks` | TASKS | R | on | yes | 3 | 1+todos | 12 | no | 4 | `tasks_panel` |
| `subagents` | SUB-AGENTS | R | on | **locked** | 6 | ∞ | ∞ | yes (2) | never | sub-agent list (≤3 rows) + `transcript_rows`. Empty state: "No sub-agents yet · /agents" |
| `activity` | ACTIVITY | R | off | yes | 4 | ∞ | ∞ | yes (1) | 3 | `feed_panel` |
| `usage` | USAGE | R | off | yes | 9 | 9 | 9 | no | 2 | `usage_panel` (month view only) |
| `topology` | TOPOLOGY | R | off | yes | 12, and inner width ≥ 46 | natural | 32 | no | 1 | `topology_panel` |

The right column's order, top to bottom, is status, context, tasks, subagents, activity, usage, topology. Users cannot reorder panels.

### 6.2 Algorithm

```
plan(W, H, vis, demand) -> Plan                    // pure, no I/O
  if W < 40 or H < 12: return TooSmall
  header = 1 + (H >= 30); bottom = 1
  mode   = Columns if W >= 100 else Tabs if W >= 60 else ChatOnly
  body   = H - header - bottom - (mode == Tabs)
  Columns:
    L = clamp(W*16/100, 22, 34); R = clamp(W*24/100, 30, 56)      // each includes its 1-column rule
    if W-L-R < 48: R = max(30, W-L-48); L = max(22, W-R-48)       // chat keeps at least 48 columns
    left  = fit(LEFT,  L-1, body, left_rule=true)
    right = fit(RIGHT, R-1, body, left_rule=false)
    chat  = Rect(L, header, W-L-R, body)
  Tabs:  Chat → chat=full; Sessions → fit([workspace,running], W, body, true)
         Agents → subagents=full; Panels → fit(visible optional L ++ R, W, body, false)
  ChatOnly: chat = full

fit(spec, colW, H, left_rule) -> [(id, y, h)], starved
  P = [p in spec where p.locked or vis[p]]; starved = []
  move p from P to starved where p.min_width > colW              // topology on a narrow sidebar
  gap = 1
  if Σmin(P) + gap*(|P|-1) > H: gap = 0                           // gaps go first
  while Σmin(P) > H and P has an optional panel:
      drop the optional panel with the lowest Drop value; add it to starved
  if Σmin(P) > H:                                                // only locked panels left, very short column
      h_i = H / |P|, remainder to the flex panel; return
  if left_rule and P == [workspace, running]:                   // owner rule: 50/50 split
      ws = (H-gap)/2; run = H-gap-ws; return
  h = min for each p; spare = H - Σh - gap*(|P|-1)
  for p in P where not flex (in registry order):                // fixed panels grow to their want
      t = min(spare, min(want_p, max_p) - h_p); h_p += t; spare -= t
  while spare > 0 and there are flex panels below their want:   // flex panels share by weight
      share spare by weight, capping each at its want; take back what a capped panel left over
  h[last(P)] += spare                                            // leftover goes to the bottom panel: never a hole between panels
  lay out top to bottom with gap rows
```

**Inside a panel.** When content is taller than `h-1` rows, the panel shows a window with `↑ n more` / `↓ n more` rows and keeps the selection visible (the existing `sessions_window`). The mouse wheel scrolls the panel under the pointer. The sub-agent transcript follows the tail unless the user has scrolled it (the existing `dock_scroll`).

**Resize.** The plan has no state. Scroll offsets are clamped. When the focused panel gets dropped, focus moves to the chat.

**Worked examples** (body = 33 at 120×36; 47 at 200×50):

| Case | Allocation |
|---|---|
| Default left, 120×36 | ws 2, running 5, session 5, mcp 2+16 (the trailing rows). Total 33 = 2+1+5+1+5+1+18. |
| Left with every optional panel hidden, 120×36 | ws 16, gap 1, running 16 |
| Right with every optional panel hidden | subagents 33 (the whole column) |
| Right at 200×50 with status, context, tasks, activity and usage | status 4, context 3, tasks 4, subagents 15, activity 7, usage 9, plus 5 gaps = 47 |
| 100×16 (body 14), default left | ws 2, running 3, session 4, mcp 2, plus 3 gaps = 14. At 15 rows or fewer, gaps are dropped before panels. |

### 6.3 Invariants (property tests, no new dependency)

The tests sweep W from 40 to 260 and H from 12 to 80 with 8 representative visibility masks, then all 512 masks at 6 sizes. They assert:

1. The rectangles of each column tile the body exactly: no overlap, and nothing between panels except gap rows.
2. Locked panels are always present with h ≥ 1. The chat is at least 48 columns wide in Columns mode.
3. The 50/50 rule holds exactly, and the sub-agent panel gets the whole right column when it is the only visible right panel.
4. Hiding an optional panel never makes a locked panel shorter, unless a panel that had been dropped for lack of room comes back.
5. The plan is deterministic, and a resize back to the previous size restores the previous plan.

## 7. `/config`

`/config` opens a frame like the overlays in §5.4: in the chat column in Columns mode, so changes preview live in the sidebars, and full screen in the other modes.

```
╭─ C O N F I G ──────────────────────────────────── esc close ─╮
│  / filter                                                    │
│                                                              │
│  PANELS   ·   Appearance   ·   Behaviour   ·   Mouse         │
│                                                              │
│  LEFT SIDEBAR                                                │
│  ▣ Workspace          locked · always visible                │
│  ▣ Running sessions   locked · always visible                │
│ ▍● Session            model, mode, effort, branch            │
│  ● MCP                servers and tool counts                │
│  ○ Tools & skills     calls this session                     │
│                                                              │
│  CHAT                                                        │
│  ▣ Chat               locked · always visible                │
│                                                              │
│  RIGHT SIDEBAR                                               │
│  ▣ Sub-agents         locked · always on the right           │
│  ● Status             phase, running tool, quotas            │
│  ● Context            meter, tokens, cost                    │
│  ● Tasks              todo list                              │
│  ○ Activity           event feed                             │
│  ○ Usage              daily heatmap                          │
│  ○ Topology           needs a 46-column sidebar              │
│                                                              │
│  ↑↓ move  space toggle  tab group  / filter  r reset  esc    │
╰──────────────────────────────────────────────────────────────╯
```

**The other groups:**
- **Appearance:**
  - Theme: Graphite / Midnight / High contrast / Mono (+ Pearl if D5 is accepted).
  - Glyphs: Unicode / ASCII.
  - Logo: Line art / Inline image (shown only when a protocol is detected) / Off.
- **Behaviour:** Show thinking, Expand diffs, Queue indicators, Motion.
- **Mouse & clipboard:** Mouse capture (off means the terminal's own selection) and Copy on select. Both are new switches in `runtime.rs` and `terminal.rs`; see D9.

**Keys:**

| Key | Action |
|---|---|
| ↑ ↓ | Move between rows. Locked rows can be focused too. |
| Space, Enter | Toggle, or cycle a choice. |
| ← → | Choose within a choice row. |
| Tab, Shift+Tab | Next or previous group. Inside `/config` this replaces the mode-cycling meaning of Shift+Tab. |
| `/` | Filter across all groups with `picker::matches`. |
| Backspace | Edit the filter. |
| `r` | Reset the focused group to its defaults. |
| Esc | Clear the filter first; a second Esc closes. |
| PgUp, PgDn | Scroll. |
| Mouse | A click toggles, the wheel scrolls. |

**Locked rows.**
- Space on a locked row changes nothing. The hint line shows the reason instead: "Workspace is always visible: it switches workspaces." "Sub-agents always live in the right sidebar."
- A starved panel shows `on · no room at this size`.
- While `NO_COLOR` is set, the Theme row is locked to Mono with the reason "NO_COLOR is set".

**Live preview.** Every change updates the app's in-memory `TuiSettings`, and the next frame uses it. Nothing is written until the screen closes.

**Persistence.** On close, and only when something changed, `synthesis_settings::persist_user_tui(home, tui: &Value)`:
1. Re-reads `$SYNTHESIS_HOME/settings.json`.
2. Stops if the file cannot be parsed. It shows the toast "settings.json is unreadable; changes kept for this session" and never overwrites the file, following the pattern in `lib.rs:284`.
3. Otherwise replaces only the `synthesis.tui` subtree and writes through a temp file plus rename. Key order is kept because `preserve_order` is on.

`synthesis.tui` in the project, local or managed files is ignored, with the diagnostic "synthesis.tui is read from user settings only".

**Schema and defaults:**

```json
{"synthesis": {"tui": {
  "theme": "graphite",
  "glyphs": "auto",
  "logo": "art",
  "panels": {"session": true, "mcp": true, "tools": false, "status": true, "context": true,
             "tasks": true, "activity": false, "usage": false, "topology": false},
  "chat": {"show_thinking": true, "diffs_expanded": false, "queue_indicators": true},
  "motion": true,
  "mouse": {"capture": true, "copy_on_select": true}
}}}
```

- **Allowed values:** `theme` is `graphite|midnight|high-contrast|mono` (plus `pearl` if D5). `glyphs` is `auto|unicode|ascii`. `logo` is `art|image|off`.
- **Panel ids:** an unknown id is ignored with a diagnostic. A locked id (`workspace`, `running`, `chat`, `subagents`) is ignored with the diagnostic "<id> is always visible".
- **Bad values:** a wrong type keeps the default, with a diagnostic that names the key but never the value. The parser is `TuiSettings::from_settings(&Value) -> (Self, Vec<String>)`, following the `AutoCompact` pattern (`harness.rs:65`).
- **Where diagnostics appear:** the `/config` hint line and `synth doctor`.

**Test hooks (no new CLI flag):**
- `config_view::render(&ConfigModel, w, h)` is pure, so it gets goldens.
- `App` unit tests drive `/config` through keys, as in the existing `app/tests.rs` `screen(&mut a)`.
- `synth --once` with `SYNTHESIS_HOME` pointing at a temp directory that contains `settings.json` renders any panel set. Whether `--once` respects `COLUMNS`/`LINES` is `[assumption]` and needs checking.
- `tests/tty-qa/qa_config.py` toggles rows, asserts the reflow and checks the written file.

## 8. Implementation within the current architecture

1. **Theme layer** (`theme.rs`):
   - `struct Theme` holds the tokens as `Style`.
   - `const GRAPHITE, MIDNIGHT, HIGH_CONTRAST, MONO, ANSI16` (and `PEARL`).
   - `thread_local! CURRENT`, with `pub fn t() -> &'static Theme` and a `ThemeScope` guard set at the entry points.
   - A `glyphs()` table with Unicode and ASCII sets.
   - `contrast(a, b)` and the contrast tests.
   - The old constants are replaced site by site through the §3 mapping. `fit`, `fit_items`, `lerp` and `spaces` stay.
2. **Layout engine:** a new `layout.rs` holding the registry, `plan`, `fit` and the property tests. `render_frame` becomes: build the plan, render each panel id into its rectangle, assemble the frame. `boxed` becomes `section` (a label row and content, no frame).
   - The `Rects` fields stay (`session_rows`, `workspace_rows`, `activity_rect`, `dock_rect`, `chat_rect`, `caret`) but are filled from the plan, so the `app.rs` hit tests change very little.
   - `layout_for`, `topology_column`, `agent_detail` and `tasks_usage_row` are deleted once nothing calls them.
3. **Panels:** reuse the existing panel functions; only their chrome and colours change.
4. **Goldens.** The 95 render goldens are rebaselined on purpose, behind a gate:
   - `SYNTHESIS_UPDATE_GOLDEN=redesign-p1|p2|p3` rewrites only the cases on that phase's allow-list.
   - **P1:** only `expected.ansi.json`, plus the header rows of frame cases. The tool fails if any other text changes.
   - **P2:** frame and dash text and rectangles.
   - **P3:** agent, dialog and picker cases.
   - Each rewrite sets `meta.json.reviewed_by` to `null`. A new assertion in `golden.rs` fails on `null`, so CI stays red until the owner writes `reviewed_by: owner:DevOnChain101 <date> (redesign Pn)` after reviewing the contact sheet. That is the approval gate.
   - `tools/golden-contact/contact.py` (standard library only) converts `expected.ansi.json` from two git revisions (`git show`) into one HTML page with before/after side by side, a text diff and a phase filter, written to `target/golden-contact/`. It runs locally, not in CI.
   - New cases: plans at 80×24, 100×30, 120×36, 140×40 and 200×50 with three visibility sets each (defaults, all optional off, all on); `/config` in each group; a locked-row hint; one frame per theme, plus `NO_COLOR` and 16-colour.
5. **tty-qa:** `qa_visual.py` captures the four owner sizes for each theme, for `NO_COLOR=1` and for `TERM=xterm`. It asserts:
   - no row is wider than the terminal;
   - no glyph is two cells wide;
   - Mono output carries no colour;
   - the bottom bar is present;
   - `/config` toggles reflow the screen.

   It runs on macOS and Linux. On Windows the code is checked in CI, but ConPTY rendering stays `unverified` until someone checks it on a real machine.
6. **Accessibility:**
   - `NO_COLOR` gives Mono.
   - The High contrast theme.
   - The ASCII glyph set.
   - The Motion toggle.
   - No meaning carried by colour alone.
   - Focus is shown by a glyph as well as colour.
   - Screen-reader users still have `synth run` headless; the alternate screen is not made accessible.
7. **Performance:** the planner is O(panels) per frame. `tests/perf.rs` must stay within its current budget.

## 9. Phases (files to touch)

| Phase | Work | Files | Exit criteria |
|---|---|---|---|
| **P1** Tokens, header, logo | Themes and tokens, colour sweep, header mark and wordmark, logo art generator, theme read from user settings (no `/config` yet), contrast and glyph-width tests | render: `theme.rs`, new `logo.rs`, `dash.rs` (`header`, colours), `agent.rs`, `flow.rs`, `dialogs.rs`, `picker.rs`; tui: `app.rs` (theme choice), new `tui_settings.rs`, `status.rs` (colours); `tools/logo-art/gen.py` | Only ANSI files and header rows changed (enforced by the tool); contact sheet approved; CI green on 3 OSes |
| **P2** Layout and `/config` | Planner and registry, sections, sub-agent panel always on, Tabs and chat-only modes, centre panels moved to the right, merged bottom bar, `/config` with persistence | render: new `layout.rs`, new `config_view.rs`, `dash.rs` (`render_frame` rewritten); tui: `app.rs` (`/config` command and keys, Tab view cycling), `status.rs`; settings: `lib.rs` (`persist_user_tui`); goldens, `golden.rs` | Property tests green; new layout goldens; frame/dash rebaseline approved; `qa_config.py` green on macOS and Linux |
| **P3** Chat, diffs, dialogs | Tool rows, thinking, diff gutter, prompt, footer, welcome with art, approval with diff, bypass, pickers and resume | render: `agent.rs`, `agent/diffview.rs`, `agent/thinking.rs`, `dialogs.rs`, `picker.rs` | agent/picker/dialog rebaseline approved |
| **P4** Polish and docs | Motion, ASCII audit, Mouse group (coordinated with whoever owns `terminal.rs`), optional inline image, optional OSC 11 light-background detection, docs, final contact sheet, 3-OS tty-qa | `runtime.rs`, `terminal.rs`; docs: `design/README.md` (TUI line), `docs/specs/README.md`, `tests/golden/README.md` | Every golden reviewed, nothing in an unreviewed state |

P2 and P3 change `app.rs`. Start them after the current `terminal.rs`, `e2e.rs` and `app.rs` work has merged.

## 10. Risks

| Risk | Mitigation |
|---|---|
| Golden churn hides real regressions | Phase allow-lists (P1 may change colour only), the contact sheet, and the `reviewed_by` gate |
| Ambiguous-width glyphs misalign in CJK locales | Glyph-width test, ASCII set, the `auto` rule |
| A light terminal makes Graphite unreadable | Mono or Pearl (D5); OSC 11 detection later |
| Thread-local theme state | Set only by the entry points; tests set it explicitly; Synthesis is the default |
| A user edits `settings.json` while the TUI is open | Re-read before writing, never overwrite an unparsable file, atomic rename, write only on close |
| 256 and 16 colours flatten the palette (Midnight's blue, the pearl's warmth, the diff tints) | Meaning also rides on glyphs; this loss is accepted |
| Moving the Mission-Control panels changes muscle memory | Defaults keep Status, Context and Tasks visible on the right |
| Inline images under tmux or in diff redraws | Off by default, welcome screen only, never inside tmux or screen |
| Concurrent edits to `app.rs` and `terminal.rs` | Sequencing in §9 |

## 11. Owner decisions

- **D1 Panel list.** Merge Tokens into a new Context panel. Move Activity to the right. Keep Topology as optional and off by default, or drop it.
- **D2 Defaults.** On: Session, MCP, Status, Context, Tasks. Off: Tools, Activity, Usage, Topology.
- **D3 Logo.** Braille art (recommended) or half-block. Inline image yes/no (recommended: optional, off by default, welcome screen only).
- **D4 Palette.** Adopt the kit fully plus the `[derived]` tokens (body `#C8CCD2`, muted, status colours, diff tints), or use the kit's colours only, which makes all body text `#9CA3AF`.
- **D5 Light terminals.** Add a Pearl theme (recommended). Later, automatic detection through OSC 11.
- **D6 Background.** Never paint the terminal background (recommended), or paint `#0B0C0F` everywhere.
- **D7 Chrome.** Borderless sections (recommended) or rounded boxes.
- **D8 Misc layout.** One merged bottom bar; minimum size 40×12; Tab cycles the views in Tabs mode.
- **D9 Scope of the Behaviour and Mouse groups.** Motion, mouse capture and copy-on-select are new runtime switches. Ship them in P4, or drop them.
- **D10 No-agent dashboard.** Retire the no-agent dashboard path and its `dash-*` goldens (only goldens reach it), or restyle it as well.
- **D11 `CLAUDE_MONITOR_COLOR`** (`sgr.rs:30`). Rename it to `SYNTHESIS_COLOR` or remove it (see F1).

## 12. Proposed changes to the design doc (for the lead to apply)

- **Revision history:** add `| 1.5 | 2026-10-29 | TUI redesign: FR-16, ADR-014 (proposed); spec docs/specs/tui-design.md |`. The header says Version 1.3 while the history already lists 1.4.
- **§3.1:** add `| FR-16 | Branded TUI: semantic theme tokens (kit palette) with truecolor/256/16/NO_COLOR fallbacks, logo mark and wordmark, a fixed panel registry with locked Workspace/Running/Chat/Sub-agents panels, reflow on hide/show, and a /config screen persisted in user settings only | P1 |`.
- **§9:** add `| ADR-014 | **Status: Proposed.** TUI theme is a set of semantic tokens chosen per render call (scoped thread-local); layout is a pure planner over a static panel registry; TUI preferences live only under `synthesis.tui` in the user settings file | Keeps the renderer deterministic and golden-testable, reuses the existing panel functions, needs no new dependency. Cost: one deliberate rebaseline of the render goldens gated by owner review, hidden per-call theme state, and a layout change for current users |`.

## Follow-ups for the planner

- **F1:** `CLAUDE_MONITOR_COLOR` in `sgr.rs:30` breaks the environment-variable rule.
- **F2:** Check that `--once` respects `COLUMNS` and `LINES`.
- **F3:** Check how 16-colour terminals render braille, and the Windows conhost font.
- **F4:** Golden `agent-welcome-empty-70x24/expected.txt` row 11 is wider than 70 columns. It looks like a leftover from the rename. Check whether `golden_divergences.txt` lists it.
- **F5:** Apply the §12 changes and add this spec to `docs/specs/README.md`.

## 13. Owner decisions (2026-10-30)

The owner accepted every recommendation of §11. Recorded as decided:

| | Decision |
|---|---|
| **D1** | Tokens merge into a new Context panel. Activity moves to the right. Topology stays, optional and off by default. |
| **D2** | Defaults as in §11: on Session, MCP, Status, Context, Tasks; off Tools, Activity, Usage, Topology. |
| **D3** | Braille logo art, generated offline and committed as a table. The inline image is optional, off by default, welcome screen only. |
| **D4** | The kit palette plus the `[derived]` tokens of §3. |
| **D5** | Add the light **Pearl** theme. No automatic detection yet (OSC 11 stays a P4 option). |
| **D6** | The terminal background is never painted. |
| **D7** | Borderless sections. Rounded frames only for the prompt, dialogs, pickers and `/config`. |
| **D8** | One merged bottom bar, minimum size 40x12, Tab switches the views in narrow mode. |
| **D9** | Motion, mouse capture and copy-on-select switches ship in P4. |
| **D10** | The no-agent dashboard is restyled minimally (colours only) or retired as §11 recommends; P1 only recolours it. |
| **D11** | `CLAUDE_MONITOR_COLOR` (`sgr.rs:30`) is renamed `SYNTHESIS_COLOR`. Only `SYNTHESIS_*` variables and the standard ones are read. |

### P1 notes (2026-10-30)

Choices made while implementing P1 that the spec left open:
- Themes: see §14.7 (Synthesis is the default; Classic, Graphite, Midnight, High contrast, Pearl, Mono, plus the automatic `ansi16`). Selection follows the rule of §3 (Mono and Ansi16 colour modes override the user's theme).
- Attributes (bold, dim, reverse) ride on the token colour: a theme lists the attribute of each token colour that needs one (Mono, High contrast), looked up when a colour is painted. Synthesis, Classic, Graphite, Midnight and Pearl use none.
- P1 changes only colours and the header rows (mark, spaced wordmark, chips). Layout, panel chrome and chat text are P2 and P3. The welcome art shows only when the pane has the spare rows, so goldens with a small pane keep their text.
- The theme comes from `synthesis.tui.theme` and `synthesis.tui.glyphs` in the USER settings file (read by the CLI), `NO_COLOR` and the colour mode always win.
- Golden rebaseline: `SYNTHESIS_UPDATE_GOLDEN=redesign-p1` rewrites only `expected.ansi.json` and header rows, refuses anything else, and marks the case `reviewed_by = "pending-owner-review (P1 2026-10-30)"`. `node tools/golden-review-status.mjs` lists pending cases. Unlike §8.4 CI does not fail on pending cases; it only prints them.

### D12 to D16 (owner, 2026-10-30, after seeing the P1 result and the two mockups)

| | Decision |
|---|---|
| **D12** | The P1 palette looked pale and washed out; the owner prefers the pre-P1 colours. The pre-P1 palette is kept value for value as the `classic` theme, and the default theme (`synthesis`) reproduces the look of the two reference mockups (§14.7). This reverses D4 (kit palette as the default look). |
| **D13** | `graphite` (kit silver) stays as an optional theme, with a more saturated accent and brighter body and strong text. |
| **D14** | The old second title of the header (the two words after "Synthesis") is removed everywhere: header, terminal title, tests, goldens, docs. The title is "Synthesis" only. |
| **D15** | The welcome greeting "Hello there! I'm Synthesis." is replaced: a time-of-day line ("Good evening.") over "What shall we build?", without first person. The strings live in one constant (`greeting.rs`, `GREETING`), so a translation is one more constant of the same type. |
| **D16** | The owner's HTML prototype `~/Vibe/synthesis-cli-ui` (it replaced the two PNG mockups, 2026-10-31) is the authoritative visual target for P2 and P3 (§14). |

### Theme setting (P1 rework, 2026-10-30)

`synthesis.tui.theme` takes `synthesis` (default), `classic`, `graphite`, `midnight`, `high-contrast`, `pearl`, `mono` or `auto`. `auto` is the default theme on 256-colour and truecolor terminals; 16 colours and `NO_COLOR` always pick `ansi16` and `mono` whatever the value (the same rule as before), and there is no light-background detection yet. The goldens run in the default theme; the other themes have their own small fixtures in `tests/golden/render-themes/` (`<theme>__<case>`), rewritten with `SYNTHESIS_UPDATE_GOLDEN=themes`.

## 14. Owner reference mockups (authoritative)

**Files (removed 2026-10-31; superseded by the HTML prototype `~/Vibe/synthesis-cli-ui`).** `design/tui-reference/mockup-A-chat.png` (A, chat-centred) and `mockup-B-dashboard.png` (B, dashboard-centred), 1672x941 each, supplied by the owner on 2026-10-30. They are the owner-approved visual reference for the TUI. Both still show the old second title after "Synthesis" in the header; the owner removed it (D14), everything else is the target. P2 and P3 acceptance means **matches mockups A/B within terminal limits** (§14.8).

### 14.1 What is in mockup A (chat-centred)

| Area | Elements |
|---|---|
| Header | Logo mark, "Synthesis". Chips: workspace (`<project>`), session (`1`), model (`claude-3.5-sonnet`), tokens (`42.1k / 200k`), health (green dot, "healthy"), clock (`23:07:05`). |
| Left sidebar | **Workspace**: current folder with its path and a collapsible tree (`src/ tests/ docs/ package.json tsconfig.json .env`). **Session**: `<project> #1`, a green live dot, rows mode, effort, model, cwd, tokens, tools ("8 enabled"). **Tools & Skills**: filesystem, shell, search, edit, browser, mcp with a green dot and a check (browser shows a hollow circle). **Activity**: read, search, run, edit, run, verify with target and "2m ago". |
| Centre top | Strip "Synthesis · ready | planning · coding · verifying" and, on the right, "Context: <project> | 12 files | 3.2k LOC". |
| Centre, chat | User card (avatar, name, time, text). Synthesis card (avatar, name, time, intro text). Rich tool cards inside the answer, each with a glyph, a title, an argument and a right-hand meta: **Reading file** (path, line range `1-240`, numbered code), **Searching** (query, `12 results`, path / line / match rows), **Running** (command, elapsed `18.4s`, output lines, `14 passed`, `0 failed`), **Applying patch** (path, `+12 -8`, numbered diff with red and green rows), **Verifying** (title, `6.1s`, three check rows with per-file results `5 passed`). Then closing prose and "Next steps" list. |
| Centre bottom | Tab strip: chat, diff, terminal, browser, tests, files (active one underlined and boxed). Prompt `> explain the fix and open the diff` with caret and `Cmd+Enter send`. |
| Right sidebar | **Capability Topology** with an "online" status: coloured cards in two columns joined by arrows: User/Project (teal; active session, project, goal), Guards (red/orange; safety, policy, sandbox on; approvals auto), Skills (green; code, search, edit, test, browser, analyze with ticks), Sub-agents (purple; "no sub-agents, single agent mode", planner, coder, reviewer), Synthesis Core (grey; "plan > act > verify > learn", "reasoning, tool use, code, evaluation"), Execution (teal; tool calls 8, running 0, errors 0, success 100%), Hooks (amber; pre-tool, post-tool, on-error, on-done), External/MCP (blue; context, github, supabase, filesystem with state), Memory/Feedback (red; session tokens, project indexed, learned items, feedback positive). **Usage**: tokens bar `42.1k / 200k (21%)`, sessions `1 today / 7 this week`, runtime, cost `$0.12 (est)`. |
| Footer | Key hints: Tab switch panel, up/down navigate, Cmd/Ctrl+K clear, Alt+V paste image, Ctrl+L history, /shell /memory /files /browser, Shift+Enter newline. Right side: `<project> | main | 3 files changed`. |

### 14.2 What is in mockup B (dashboard-centred)

| Area | Elements |
|---|---|
| Header | Mark, "SYNTHESIS" (spaced). Chips separated by thin rules: `1 live`, `workspace: synthesis`, `session: feature/auth-flow`, `model: claude-3.5-sonnet`, `context: 128k`; right: green "system healthy", date and clock (`Mon Oct 27 23:07:05`). |
| Left sidebar | **Workspace** with count: `synthesis` (selected row), `platform`, `web-app`, `infra`, `+ new workspace...`. **Session** with "running": mode agent, model, context `128,000 tokens`, tools `12 enabled`, cwd, uptime. **Tools & Skills** with count: file system, shell, browser, search, git, mcp (`2 servers`, each `on`), memory, "+ 5 more...". **Recent activity**: Edited auth.ts, Ran tests, Searched codebase, Read architecture.md, Updated plan, Used browser, Committed changes, Checked diagnostics, each with age. **Session stats** (`42m`): files read 28, files edited 6, commands run 14, tokens used 48.2k, tool calls 37, success rate 100%. |
| Centre top | **Agent Status** card: "Working on: Implementing secure authentication flow with session management", "Next: Run tests and verify edge cases"; beside it a checklist (Plan updated 2m ago, Implementing changes..., Running tests..., Verifying results...). **Context** card: tokens `48.2k / 128k 38%`, input `42.1k` with a bar, output `6.1k`, cache `12.3k (hit 92%)`. |
| Centre tabs | Strip: TERMINAL (active), agent, workspace, diff (underlined), `+`, and a close button. |
| Centre body | Terminal view: prompt line `synthesis ~/dev/synthesis git:(feature/auth-flow) >`, the user request, then "Synthesis" with a step list (circle, text, duration: 3.2s, 2.1s, 1.8s, 24.3s, 18.7s, 4.1s). Below, a split: **Project** tree (`src/ app/ api/ auth/ route.ts session.ts middleware.ts lib/ ... tests/ README.md package.json` with `M` and `A` markers) beside a **diff** view (`src/lib/auth.ts`, hunk header, numbered lines, green added rows), and under both a **Terminal** pane with a `test` tab (pnpm test output, green ticks, `Test Files 3 passed (3)`, `Tests 18 passed (18)`, `Duration 101ms`). |
| Prompt | Box with `>` and placeholder "Ask Synthesis anything... (Ctrl+L for history)", a model chip (`claude-3.5-sonnet`) and a send button; under it a slash-command bar: `/tools access tools | /memory manage memory | /shell run commands | /browser web research | /diff view changes`. |
| Right sidebar | **Synthesis Topology**: a hub diagram with the core in the middle (logo, "SYNTHESIS CORE", "Reason, Act, Verify") and cards around it joined by lines: User/Project (goals and context, requirements, feedback), Guards (safety checks, policy enforcement, risk analysis), Sub-agents (task specialists, domain experts, on-demand), Skills (tasks and capabilities, code, search, web, custom skills), External/MCP (MCP servers, external APIs, data sources), Execution (tool execution, code changes, environment access), Memory/Feedback (context storage, learn from outcomes, improve over time). **Usage and Metrics** with a "7 days" selector: bars for Tokens `48.2k / 128k`, Tool calls 37, Files modified 6, Commands run 14, Browser requests 3, MCP calls 12. **System Health**: "All systems operational"; Core agent, Tool runtime, MCP servers (`2/2 online`), Memory store, Browser sandbox, each with a status and a latency (`42ms`, `28ms`, `36ms`, `31ms`, `47ms`). |

### 14.3 How the two merge (where they differ)

| Point | A | B | Merged target |
|---|---|---|---|
| Header chips | workspace, session, model, tokens, health, clock | live, workspace, session, model, context, health, date + clock | live, workspace, session, model, context (`used / limit`), health, clock; the date only from 140 columns. Drop order from the right of the list: date, health text (dot stays), context, session, live, model, workspace; the clock stays. |
| Left | Workspace as a folder tree, Session, Tools & Skills, Activity | Workspace as a list of workspaces, Session, Tools & Skills, Recent activity, Session stats | Workspace stays the **list of workspaces** (owner rule: it is locked and splits the column with Running). The folder tree is its own optional panel (`project_tree`). Running sessions (not drawn in either mockup, locked by the owner) sit under Workspace. |
| Centre top | A thin "ready | planning · coding · verifying" strip with project facts | Two cards: Agent Status (+ plan checklist) and Context | The two cards of B, with A's phase words as the Agent Status headline (`ready`, `planning`, `coding`, `verifying`) and A's `files | LOC` facts in the Context card. |
| Centre tabs | chat, diff, terminal, browser, tests, files | TERMINAL, agent, workspace, diff, + | One strip: `chat` (default, B's `agent`), `diff`, `terminal`, `browser`, `tests`, `files` (B's `workspace`), `+` for more; active tab boxed and underlined. |
| Centre body | Chat with rich tool cards | Terminal log, project tree + diff + terminal split | The default tab is **chat with A's tool cards**. B's split is the layout of the `files` and `diff` tabs from 160 columns (tree left, diff right, terminal underneath); narrower, one pane at a time. |
| Prompt | `> text` with `Cmd+Enter send` | box, model chip, send button, slash-command bar | B's box (rounded frame) with the model chip and `Enter send` on the right, and B's slash-command bar below it when the height allows. |
| Right | Topology as a two-column grid with arrows, Usage | Topology as a hub around the core, Usage and Metrics, System Health | Topology as a **two-column card grid with the core row in the middle** (a terminal cannot draw the hub), cards with coloured thin borders as in A and B; Usage and Metrics bars (B) with A's cost and runtime lines; System Health (B). The sub-agent transcript is always in the right sidebar (owner rule), under the Sub-agents card of the topology or in the Sub-agents panel. |
| Footer | key hints, plus `project | branch | files changed` | none visible (the slash bar instead) | Key hints on the left, `project | branch | n files changed` on the right (A), the slash bar above it (B). |

### 14.4 Mockup element to panel id

New ids are in bold. **Default** is the visibility before the user changes it in `/config`; panels that do not fit are dropped by the planner (lowest drop number first), so on a small terminal the defaults shrink to the locked panels. Locked panels (Workspace, Running sessions, Chat) are never hidden.

| Mockup element | Panel id | Side | Default | Drop | Content source | Phase |
|---|---|---|---|---|---|---|
| Header chips, health, clock | (header, not a panel) | top | on | n/a | `header` | P2 |
| Workspace list | `workspace` | L | on, locked | never | `workspace_panel` | P2 |
| Running sessions | `running` | L | on, locked | never | `sessions_panel` | P2 |
| Session | `session` | L | on | 4 | `session_info_panel` | P2 |
| Tools & Skills (with `mcp  2 servers`) | **`tools_skills`** (replaces `tools` and absorbs `mcp`) | L | on | 3 | `tools_panel` + `mcp_summary_line` | P2 |
| Recent activity / Activity | **`recent_activity`** (replaces `activity`, moves to the left) | L | on | 2 | `feed_panel`, one row per event with an age | P2 |
| Session stats | **`session_stats`** | L | on | 1 | counters of the model (files read and edited, commands, tokens, tool calls, success rate) | P2 |
| Folder tree (A) / Project (B) | **`project_tree`** | C tab `files` | off | n/a | `folders.rs` plus the git status letters | P3 |
| Chat with tool cards | `chat` | C | on, locked | never | `agent_panel` (tool cards: P3) | P3 |
| Agent Status card | **`agent_status`** (replaces `status`) | C top | on | 6 | `status_panel` plus the todo list (replaces `tasks`) | P2 |
| Context card | `context` | C top | on | 5 | context bar and `token_panel`, with input, output, cache hit | P2 |
| Diff view | **`diff`** | C tab `diff` | off | n/a | `agent/diffview.rs` over the stored file changes | P3 |
| Terminal view and test pane | **`terminal`** | C tab `terminal` | off | n/a | the bash tool outputs of the session (read only) | P3 |
| browser, tests tabs | (tabs of the same strip; content is the Browser and verification results) | C | off | n/a | later phases (browser: R6, tests: Verification engine) | after P3 |
| Prompt, model chip, slash bar | part of `chat` | C | on | n/a | `agent_panel` input rows, `suggestions` | P3 |
| Synthesis Topology | **`topology`** | R | on | 1 | `topology_panel`, two-column card grid (needs inner width 46; hidden on a narrower sidebar) | P2 |
| Usage and Metrics | **`usage_metrics`** (replaces `usage`) | R | on | 2 | bars from the session counters, a "7 days" switch reading the usage history | P2 |
| System Health | **`system_health`** | R | on | 3 | core agent, tool runtime, MCP servers, memory store, browser sandbox: state from the runtime, latency only where measured | P2 |
| Sub-agent transcript | `subagents` | R | on, locked | never | sub-agent list and `transcript_rows` | P2 |
| Footer key hints, branch, files changed | (bottom bar) | bottom | on | n/a | `status.rs` | P2 |

Removed ids (merged): `status` into `agent_status`, `tasks` into `agent_status`, `tools` and `mcp` into `tools_skills`, `activity` into `recent_activity`, `usage` into `usage_metrics`. The registry of §6.1 is updated accordingly in P2; the invariants of §6.3 are unchanged. The default visibilities of D2 (Tools, Activity, Usage, Topology off) are superseded: the mockups show them all, so they are on, and the planner drops them when there is no room.

### 14.5 Merged target layout

Sketches are not to scale; a letter is the panel (W workspace, R running, S session, T tools & skills, A recent activity, X session stats; AS agent status, C context; TP topology, U usage and metrics, H system health, SA sub-agents, CH chat). The owner's rules hold in every size: Workspace, Running and Chat locked; left column splits equally between Workspace and Running when no optional left panel is visible; the sub-agent transcript is in the right sidebar; remaining panels reflow into freed space.

**200x50** (Columns, left 32, right 56, centre 112; body 47):

```
 mark SYNTHESIS  1 live | workspace | session | model | context 128k        healthy | Mon Oct 27 23:07
 WORKSPACE (locked)   | AGENT STATUS + plan checklist        | CONTEXT   | TOPOLOGY (card grid, 2 columns)
 RUNNING (locked)     |--------------------------------------+-----------| User/Project | Guards
 SESSION              | chat | diff | terminal | browser | ... | Skills       | Sub-agents
 TOOLS & SKILLS       |                                      | Synthesis Core (full width)
 RECENT ACTIVITY      |   CHAT: user card, answer, tool      | Execution    | Hooks
 SESSION STATS        |   cards (Reading, Searching,         | External/MCP | Memory/Feedback
                      |   Running, Applying patch,           |-------------------------------
                      |   Verifying), closing prose          | USAGE AND METRICS (bars)
                      |                                      | SYSTEM HEALTH
                      |  > prompt box [model chip] Enter send| SUB-AGENT TRANSCRIPT (when viewing)
                      |  /tools /memory /shell /browser /diff|
 key hints ........................................  project | branch | n files changed
```

On the `files` and `diff` tabs at this size the centre splits as in B: project tree left, diff right, terminal underneath.

**120x36** (Columns, left 22 to 34, right 30 to 56; body 33): left W, R, S, T and as many of A and X as fit (X goes first, then A, then T); centre: agent status and context as one row of two cards (3 to 4 rows), the tab strip, chat, the prompt box, the slash bar only when 4 spare rows exist; right: topology (compact: cards without their detail rows), usage and metrics, system health; sub-agents when a sub-agent is being viewed (it then takes the topology's place: drop order 1 is the topology). Header chips: live, workspace, session, model, context, health dot, clock.

**80x24** (Tabs mode; below 100 columns the sidebars are a switcher): tab strip `Chat | Sessions n | Agents n | Panels n`. Chat view: one header row (live, model, context, clock), the compact agent status line, the chat with tool cards in one-line form, the prompt, key hints. Sessions view: Workspace and Running split 50/50. Panels view: the visible optional panels stacked (session, tools & skills, recent activity, session stats, then the right ones). Agents view: the sub-agent transcript.

### 14.6 Out of scope for a terminal

- Images and avatars: the logo is braille art (D3), the user and agent avatars are the glyphs `›` and `✻`.
- Shadows, glow, blur, gradients on card edges, rounded 12 to 24 px corners: cards are 1-cell borders (`─ │` with square or `╭ ╮` corners), coloured by token. The hub diagram's radial lines are replaced by the two-column card grid with `│ ▾` connectors.
- Font choice, letter-spacing outside the wordmark, per-pixel sparkline bars (bars are `█░` cells), hover states, animated caret, window chrome and the close button.
- The terminal background is never painted (D6): the mockup's navy page is the theme's `bg_ref`, assumed, and only the header band, selected rows, diff rows and overlays are painted.

### 14.7 Palette: the default theme reproduces the mockups

Method: `tools/sample-mockup-palette.py` (standard library only: PNG decoder, colour histogram, vivid pixels grouped by hue). Values are dominant pixel colours of the scaled, anti-aliased PNGs, so they are approximate (hue is reliable, saturation of thin lines is lowered by the scaling):

| Role | Sampled in the mockups | Classic (git 05063f4) | Decision |
|---|---|---|---|
| Page background | `#080E12` (43 %), `#0A0E12` | none painted | `bg_ref` of `synthesis`: `#080E12` |
| Header band | `#0E141A` | none | `surface`: `#0E141A` |
| Selected row | `#161E26` | `60;72;105` selection | `synthesis` selection `#1A2433` (a little lighter than sampled so it shows on 256 colours too) |
| Diff-added row | `#122220` | none | `diff_add_bg`: `#122220` |
| Blue (focus, links, selected) | `#6888D8` (hue 222) | accent `#818CF8` (hue 234), light `#A5B4FC` | same family; keep Classic's exact values |
| Green (ok, checks) | `#58A878`, `#78D898` (hue 140 to 150) | `#4ADE80` (hue 142) | same family; keep Classic |
| Teal (cards, execution) | `#489878`, `#5898A8` (hue 160 to 190) | teal `#2DD4BF`, cyan `#22D3EE` | same family; keep Classic |
| Amber / orange (warnings, hooks) | `#B88848` (hue 38) | yellow `#FACC15`, orange `#FB923C` | same family; keep Classic |
| Red (errors, guards, memory) | `#C86868` (hue 0 to 359) | `#F87171` | same family; keep Classic |
| Purple (sub-agents) | `#9848A8` (hue 285) | `#C084FC` | same family; keep Classic |
| Soft white text | `#CACED6` | body `#D7D7DC`, strong `#F5F5F5` | keep Classic |

Where the mockups and Classic agree on hue (all of the above), Classic's exact value is kept; the mockups add only the navy surfaces. The theme set:

| Theme | What it is |
|---|---|
| `synthesis` (default) | Classic's colours on the mockups' navy surfaces. |
| `classic` | The pre-P1 palette, value for value (`theme-color-fns` golden and the unchanged colours of every other golden pin it). |
| `graphite` | Kit silver and pearl, accent revised to `#4D8BFF` (hue 219, the hue of the kit's Muted Blue `#0B1322`: the kit itself has no saturated accent token), light accent `#A3C3FF`, body `#DADDE3`, strong `#F5F6F8`, selection `#264173`. |
| `midnight`, `high-contrast`, `pearl`, `mono`, `ansi16` | Unchanged. |

Contrast (WCAG, against each theme's `bg_ref`): Synthesis body 13.53, strong 17.80, label 7.93, **muted 3.95**, ok 11.14, warn 12.68, err 7.02, info 10.74, border 3.21. Classic (against `#0B0C0F`): body 13.64, strong 17.94, label 7.99, **muted 3.98**, ok 11.22, warn 12.77, err 7.07, info 10.82, border 3.24. Graphite: body 14.37, strong 18.09, label 6.24, muted 4.93, ok 10.52, warn 9.92, err 7.61, info 7.72, accent 6.01. **Exception:** the muted text of Classic and Synthesis is the old `#707078`, which is below the 4.5 floor (3.95 and 3.98); the test allows 3.5 for those two themes only, and Classic's colour is not changed to satisfy the floor (owner instruction).

### 14.8 Acceptance criteria for P2 and P3: "matches mockups A/B within terminal limits"

Checked on 80x24, 120x36 and 200x50 with `synth --once` and tty-qa, against the mockups in `design/tui-reference/`:

1. **Header:** every chip of §14.3 present in the order of B, dropped by priority when narrow; no second title after "Synthesis" anywhere.
2. **Left:** the panels of §14.4 in the order Workspace, Running, Session, Tools & Skills, Recent activity, Session stats; the 50/50 rule when the optional ones are hidden.
3. **Centre:** Agent Status and Context cards (B), the tab strip with the tabs of §14.3, the prompt box with model chip and send hint, the slash bar when the height allows.
4. **Chat (P3):** user and Synthesis cards; tool cards with glyph, title, argument and right-hand meta for Reading file, Searching, Running, Applying patch (`+n −m`, numbered diff rows with green and red backgrounds) and Verifying (ticks and per-file results).
5. **Right:** Topology as the card grid with the nine cards of A and B (User/Project, Guards, Skills, Sub-agents, Synthesis Core, Execution, Hooks, External/MCP, Memory/Feedback) with thin borders in teal, orange, purple, blue; Usage and Metrics bars; System Health with states; the sub-agent transcript always in the right sidebar.
6. **Look:** the default theme's surfaces, accents and ticks are those of §14.7; green check marks, amber and orange warnings, soft white text; the greeting of D15 on the empty chat.
7. **Rules:** Workspace, Running sessions and Chat cannot be hidden; every other panel can be hidden in `/config`; hiding a panel gives its rows to the others with no hole (the §6.3 invariants).
8. **Goldens and contact sheet:** frames per size per theme, reviewed by the owner against the mockups; deviations are listed with the reason (terminal limit or data not available).

## 15. Phase P2 status (2026-10-30): layout engine, panel registry, `/config`

**Implemented.** `layout.rs` (registry of 16 ids, pure planner, 8 property tests over sizes 40..260 x 11..80 and all 512 visibility masks), `panels.rs` (agent status, context, tools & skills, recent activity, session stats, topology cards, usage & metrics, system health, sub-agents), `config_view.rs` and `tui/src/config_screen.rs` (`/config`), `dash.rs` rewritten around the plan (borderless sections, header chips of §14.3, Tabs-mode strip, footer with `project | branch | n files changed`), `synthesis_settings::persist_user_tui` and the extended `synthesis.tui` schema (`tui_settings.rs`). User docs: `docs/CLI.md` "Layout, panels and /config".

**Decisions.**
- Registry ids follow §14.4 (the merged ids `status`, `tasks`, `tools`, `mcp`, `activity`, `usage` do not exist). Defaults: every panel of the mockups is on; the three centre tabs (`project_tree`, `diff`, `terminal`) are off and have no content yet.
- Planner priority when a column has spare rows: locked panels reach their ideal height first, then fixed optional panels in registry order, then flex optional panels, then the leftover goes to flex optional panels, then to Workspace and Running up to half of the column each (so hiding a panel never shortens a locked one and the 50/50 rule is the limit case), then to the last panel. Gaps are dropped before panels.
- Right sidebar width is 28% of the width (30 to 56); it grows to 47 columns (the topology cards need 46 of panel width) when the chat keeps at least 70 columns, so the topology is drawn from about 140 columns instead of §14.5's 200. With less room it is "starved": on, but not drawn; `/config` says so.
- The frame height the planner gets is the rows above the app's status line, so the smallest terminal is 40x12 (frame 40x11). The status line stays a second bottom row: merging it into the key-hint bar (§5.1) is **not done** (the `status_line_with` row is pinned by the e2e tests and `--once`); the right end of the key-hint row carries `project | branch | n files changed` instead.
- Tab: in Tabs mode it cycles the views; the old Tab meaning (cycle the message target among sub-agents) is unreachable below 100 columns (`@name` still routes a line). In Columns mode nothing changed.
- No-agent dashboard (D10): kept; the chat pane then reads "no conversation attached". The `dash-*` goldens exercise it.
- Removed with the old layout: `layout_for`, `Layout`, `topology_column`, `agent_detail`, `detail_panels`, `tasks_usage_row`, `side_column`, the Status/Tokens/Tasks panels and the heat-map Usage panel (year and month views). Their 14 goldens (`layout-for-table`, `panel-tasks-*`, `panel-usage-*`) were deleted with them. `flow.rs` (the old topology diagram) is no longer called; it is left in place and can go in a follow-up.

**Data-backed vs placeholder.** Backed by events or the host: session counters (files read and edited, commands, tool calls and errors, success rate, tokens), the context meter and cost, phase and the running tool, todos (plan checklist), activity feed with ages, sub-agents and their transcripts, MCP state (live, through `Options.mcp_status`), sandbox mode, memory-store availability, hooks seen in the transcript, last verification. Honest "no data yet": quotas, latencies (none measured), the browser sandbox, the memory feedback beyond writes, `7 days` totals (the app keeps no usage history, so the panel says `this session`; the history view is wired for hosts that pass one). Placeholder text, not data: the Synthesis core card (`plan -> act -> verify -> learn`).

**Deviations from the mockups.** Tool cards (Reading file, Searching, Running, Applying patch, Verifying), user and Synthesis cards, the prompt box with model chip and `Enter send`, and the slash bar are chat content: phase P3 (the P2 row of the phase table does not include them). The centre tab strip (chat, diff, terminal, browser, tests, files) and the files/diff/terminal split are P3 as well, so `project_tree`, `diff` and `terminal` only exist as registry ids. The topology is a two-column card grid with connectors, not a hub; cards show 0 to 4 detail rows by height. Behaviour and Mouse rows of `/config` are stored but take effect in P3 and P4.

**Goldens.** `SYNTHESIS_UPDATE_GOLDEN=redesign-p2` rewrites only `frame` cases (and creates the files of a new case directory), after checking the frame size, the header and, from 100 columns, the locked panels; it refuses every other op. 29 existing cases were rewritten and 26 `layout-*` cases added, all stamped `pending-owner-review (P2 2026-10-30)`; `node tools/golden-contact-p2.mjs` writes `docs/research/tui-p2-contact-sheet.html`. The nine `dash-*`/`frame-*` entries of `golden_divergences.txt` were dropped (those cases are now Rust-authored). Live frames: `docs/research/tui-p2-frames/` (`tests/tty-qa/qa_config.py --frames`).

**Unverified.** Windows ConPTY, legacy conhost and any terminal other than the macOS pty used by tty-qa; Linux tty-qa; the clipboard and mouse paths on `/config` (unit-tested only, not run in a real terminal).
