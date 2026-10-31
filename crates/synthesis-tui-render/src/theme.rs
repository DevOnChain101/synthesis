//! Theme tokens (the Synthesis kit palette, spec docs/specs/tui-design.md §3), colour helpers and
//! width-aware `fit` (widths come from `synthesis_tui_base::text`). Every colour is 24-bit: the
//! renderers never consult the colour mode, `synthesis_tui_base::sgr::encode` downgrades at output
//! time. The theme is chosen once per render call (see [`ConfigScope`]).

use std::cell::Cell;
use synthesis_tui_base::sgr::ColorMode;
use synthesis_tui_base::text::{cluster_width, csi_len, strip_ansi, width};
use unicode_segmentation::UnicodeSegmentation;

pub type Rgb = [u8; 3];

/// `#RRGGBB` as a const.
const fn hex(v: u32) -> Rgb {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

// ---------------------------------------------------------------- themes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    /// The palette of the Node prototype and of every release before P1, value for value.
    Classic,
    Graphite,
    /// The owner's reference mockups (design/tui-reference): Classic's colours on deep navy; the default.
    Synthesis,
    Midnight,
    HighContrast,
    Mono,
    Pearl,
    /// Paints nothing: every cell keeps the terminal's own background (surfaces are borders, dim and
    /// underline); the foreground tokens are the Synthesis ones.
    System,
    /// Chosen automatically for 16-colour terminals; never by the user.
    Ansi16,
}

impl ThemeId {
    /// The values `synthesis.tui.theme` accepts.
    pub const CHOICES: [(&'static str, ThemeId); 8] = [
        ("synthesis", ThemeId::Synthesis),
        ("classic", ThemeId::Classic),
        ("graphite", ThemeId::Graphite),
        ("midnight", ThemeId::Midnight),
        ("high-contrast", ThemeId::HighContrast),
        ("mono", ThemeId::Mono),
        ("pearl", ThemeId::Pearl),
        ("system", ThemeId::System),
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::CHOICES.iter().find(|(n, _)| *n == s).map(|(_, i)| *i)
    }
}

/// Code highlighting colours (`highlight.rs`).
#[derive(Clone, Copy)]
pub struct Syntax {
    pub fg: Rgb,
    pub keyword: Rgb,
    pub storage: Rgb,
    pub builtin: Rgb,
    pub literal: Rgb,
    pub string: Rgb,
    pub comment: Rgb,
    pub variable: Rgb,
    pub add: Rgb,
    pub del: Rgb,
}

/// Semantic colour tokens of one theme. A token is a colour; the few that also need an
/// attribute (bold, dim) in Mono and High contrast list it in `fg_attrs`, found by colour when
/// the colour is painted, so tokens stay plain `Rgb` values.
pub struct Theme {
    pub id: ThemeId,
    /// The terminal background this theme is designed for. Painted over the whole frame only when
    /// `paint_bg` is set.
    pub bg_ref: Rgb,
    /// Paint the app background and the card surfaces (Synthesis); the other themes keep the
    /// terminal's own background.
    pub paint_bg: bool,
    /// No `48;` SGR is ever emitted (System): `bg()` is empty and selection is reverse video.
    pub flat: bool,
    /// Header band, toasts.
    pub surface: Rgb,
    /// Dialogs and selection.
    pub elevated: Rgb,
    pub border: Rgb,
    /// Uppercase section labels.
    pub label: Rgb,
    /// Times, hints, folders.
    pub muted: Rgb,
    pub body: Rgb,
    pub strong: Rgb,
    pub accent: Rgb,
    pub accent_light: Rgb,
    /// Focus bar, caret, spinner.
    pub pearl: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub err: Rgb,
    /// MCP, permission, links.
    pub info: Rgb,
    pub sand: Rgb,
    /// Text on an `accent_light` fill.
    pub ink: Rgb,
    pub diff_add_bg: Rgb,
    pub diff_del_bg: Rgb,
    /// Heat map, level 0 (empty) to 4.
    pub heat: [Rgb; 5],
    /// The four agent tints.
    pub tints: [Rgb; 4],
    pub syntax: Syntax,
    /// SGR attribute emitted before a foreground colour.
    pub fg_attrs: &'static [(Rgb, &'static str)],
    /// SGR attribute emitted before a background colour (reverse in Mono).
    pub bg_attrs: &'static [(Rgb, &'static str)],
    /// Start and end sequences of a selected cell (`synthesis_tui_base::selection`).
    pub select: (&'static str, &'static str),
    /// Colours Classic keeps apart that the other themes merge into the tokens above.
    pub extra: Option<&'static Extra>,
}

/// The old palette's finer distinctions (tokens in the other themes: track = border, slate = label,
/// cream = accent_light, blue = info, teal = ok, purple = pearl, pink = sand, orange = warn,
/// code = info). See [`Theme::track`] and friends.
pub struct Extra {
    pub track: Rgb,
    pub slate: Rgb,
    pub cream: Rgb,
    pub blue: Rgb,
    pub teal: Rgb,
    pub purple: Rgb,
    pub pink: Rgb,
    pub orange: Rgb,
    pub code: Rgb,
    /// Context-usage categories.
    pub ctx: [Rgb; 8],
    /// Agent tints, picked by a hash of the agent kind.
    pub agents: [Rgb; 8],
}

const BOLD_SGR: &str = "\x1b[1m";
const DIM_SGR: &str = "\x1b[2m";
const REVERSE_SGR: &str = "\x1b[7m";

/// Monokai: the code colours Graphite and Midnight keep (the goldens pin them).
const MONOKAI: Syntax = Syntax {
    fg: hex(0xF8F8F2),
    keyword: hex(0xF92672),
    storage: hex(0x66D9EF),
    builtin: hex(0xA6E22E),
    literal: hex(0xBE84FF),
    string: hex(0xE6DB74),
    comment: hex(0x75715E),
    variable: hex(0xFFFFFF),
    add: hex(0x50C850),
    del: hex(0xDC5A5A),
};

const BASE: Theme = Theme {
    id: ThemeId::Graphite,
    bg_ref: hex(0x0B0C0F),
    paint_bg: false,
    flat: false,
    surface: hex(0x15171B),
    elevated: hex(0x242830),
    border: hex(0x3A3F4A),
    label: hex(0x8C929B),
    muted: hex(0x7A808C),
    body: hex(0xC8CCD2),
    strong: hex(0xE5E7EB),
    accent: hex(0xD9D9E0),
    accent_light: hex(0xF7F8FA),
    pearl: hex(0xD7CEC7),
    ok: hex(0xA8C5B2),
    warn: hex(0xD8B46A),
    err: hex(0xE08A8A),
    info: hex(0x8EA4C8),
    sand: hex(0xCDBB9A),
    ink: hex(0x0B0C0F),
    diff_add_bg: hex(0x14241B),
    diff_del_bg: hex(0x2A1618),
    heat: [
        hex(0x1B1E24),
        hex(0x3A3F4A),
        hex(0x6B717C),
        hex(0x9CA3AF),
        hex(0xE5E7EB),
    ],
    tints: [hex(0xA8C5B2), hex(0x8EA4C8), hex(0xCDBB9A), hex(0xD7CEC7)],
    syntax: MONOKAI,
    fg_attrs: &[],
    bg_attrs: &[],
    select: (
        "\x1b[38;2;229;231;235m\x1b[48;2;36;40;48m",
        "\x1b[39m\x1b[49m",
    ),
    extra: None,
};

// ---- Classic: the palette before P1, value for value (owner preference 2026-10-30)
const PURPLE: Rgb = [192, 132, 252];
const BLUE: Rgb = [96, 165, 250];
const GREEN: Rgb = [74, 222, 128];
const YELLOW: Rgb = [250, 204, 21];
const ORANGE: Rgb = [251, 146, 60];
const PINK: Rgb = [244, 114, 182];
const CYAN: Rgb = [34, 211, 238];
const TEAL: Rgb = [45, 212, 191];
const RED: Rgb = [248, 113, 113];
const ACCENT_CORE: Rgb = [129, 140, 248];
const ACCENT_LIGHT: Rgb = [165, 180, 252];
const ACCENT_INK: Rgb = [17, 19, 38];
const WARM_DIM: Rgb = [98, 98, 106];
const SELECT_BG: Rgb = [60, 72, 105];

static CLASSIC_EXTRA: Extra = Extra {
    track: [58, 58, 64],
    slate: [176, 190, 210],
    cream: [250, 250, 250],
    blue: BLUE,
    teal: TEAL,
    purple: PURPLE,
    pink: PINK,
    orange: ORANGE,
    code: [125, 211, 252],
    ctx: [BLUE, PURPLE, TEAL, YELLOW, PINK, CYAN, GREEN, RED],
    agents: [PURPLE, BLUE, GREEN, YELLOW, TEAL, PINK, CYAN, RED],
};

const CLASSIC_T: Theme = Theme {
    id: ThemeId::Classic,
    surface: ACCENT_INK,
    elevated: SELECT_BG,
    border: WARM_DIM,
    label: [165, 165, 172],
    muted: [112, 112, 120],
    body: [215, 215, 220],
    strong: [245, 245, 245],
    accent: ACCENT_CORE,
    accent_light: ACCENT_LIGHT,
    // spinner and header mark: the old shimmer ran from the core to the light accent
    pearl: ACCENT_LIGHT,
    ok: GREEN,
    warn: YELLOW,
    err: RED,
    info: CYAN,
    sand: PINK,
    ink: ACCENT_INK,
    diff_add_bg: hex(0x14281C),
    diff_del_bg: hex(0x2E181B),
    heat: [
        [22, 27, 34],
        [14, 68, 41],
        [0, 109, 50],
        [38, 166, 65],
        [57, 211, 83],
    ],
    tints: [PURPLE, BLUE, GREEN, TEAL],
    select: (
        "\x1b[38;2;245;245;245m\x1b[48;2;60;72;105m",
        "\x1b[39m\x1b[49m",
    ),
    extra: Some(&CLASSIC_EXTRA),
    ..BASE
};
pub static CLASSIC: Theme = CLASSIC_T;

/// The owner's HTML prototype (design/tui-prototype/index.html, `:root` tokens value for value):
/// paints the app background (`#0B0C0F`) and the card surfaces (`#15171B`). The default theme.
pub static SYNTHESIS: Theme = Theme {
    id: ThemeId::Synthesis,
    bg_ref: hex(0x0B0C0F),
    paint_bg: true,
    surface: hex(0x15171B),
    elevated: hex(0x242830),
    border: hex(0x606978),
    label: hex(0xB5BECD),
    muted: hex(0x8A93A3),
    body: hex(0xC4CAD6),
    strong: hex(0xF2F4F8),
    accent: hex(0xD9D9E0),
    accent_light: hex(0xF7F8FA),
    pearl: hex(0xD7CEC7),
    ok: hex(0x4ADE80),
    warn: hex(0xE0B252),
    err: hex(0xE5636B),
    info: hex(0x6EA8FF),
    sand: hex(0xF472B6),
    ink: hex(0x0B0C0F),
    diff_add_bg: hex(0x0F2A1D),
    diff_del_bg: hex(0x2B1519),
    heat: [
        hex(0x1B1E24),
        hex(0x3A3F4A),
        hex(0x6B717C),
        hex(0x9CA3AF),
        hex(0xE5E7EB),
    ],
    tints: [hex(0x4ADE80), hex(0x6EA8FF), hex(0xE0B252), hex(0xB794F6)],
    select: (
        "\x1b[38;2;242;244;248m\x1b[48;2;36;40;48m",
        "\x1b[39m\x1b[49m",
    ),
    extra: Some(&SYNTHESIS_EXTRA),
    ..CLASSIC_T
};

static SYNTHESIS_EXTRA: Extra = Extra {
    track: hex(0x3A3F4A),
    slate: hex(0xB5BECD),
    cream: hex(0xF7F8FA),
    blue: hex(0x6EA8FF),
    teal: hex(0x4FD1C5),
    purple: hex(0xB794F6),
    pink: hex(0xF472B6),
    orange: hex(0xF59E42),
    code: hex(0x6EA8FF),
    ctx: [
        hex(0x6EA8FF),
        hex(0xB794F6),
        hex(0x4FD1C5),
        hex(0xE0B252),
        hex(0xF472B6),
        hex(0x4FD1C5),
        hex(0x4ADE80),
        hex(0xE5636B),
    ],
    agents: [
        hex(0xB794F6),
        hex(0x6EA8FF),
        hex(0x4ADE80),
        hex(0xE0B252),
        hex(0x4FD1C5),
        hex(0xF472B6),
        hex(0xF59E42),
        hex(0xE5636B),
    ],
};

/// Same foreground tokens as Synthesis, but nothing is painted: the terminal's own background shows
/// through everywhere. Surfaces are expressed with borders, dim and underline.
pub static SYSTEM: Theme = Theme {
    id: ThemeId::System,
    paint_bg: false,
    flat: true,
    select: (REVERSE_SGR, "\x1b[27m"),
    ..SYNTHESIS
};

/// Silver and pearl surfaces with the kit's blue (hue 219, the hue of the kit's Muted Blue `#0B1322`)
/// where the eye goes (focus, selection, chips, links), and brighter text than the kit's grey so it
/// does not read pale. The kit itself has no saturated accent token.
pub static GRAPHITE: Theme = Theme {
    body: hex(0xDADDE3),
    strong: hex(0xF5F6F8),
    accent: hex(0x4D8BFF),
    accent_light: hex(0xA3C3FF),
    select: (
        "\x1b[38;2;245;246;248m\x1b[48;2;38;65;115m",
        "\x1b[39m\x1b[49m",
    ),
    ..BASE
};

pub static MIDNIGHT: Theme = Theme {
    id: ThemeId::Midnight,
    surface: hex(0x0B1322),
    elevated: hex(0x16213A),
    border: hex(0x2C3A55),
    muted: hex(0x7E8AA3),
    body: hex(0xC3CBD9),
    pearl: hex(0xBFD3F2),
    tints: [hex(0xA8C5B2), hex(0x8EA4C8), hex(0xCDBB9A), hex(0xBFD3F2)],
    heat: [
        hex(0x151C2B),
        hex(0x2C3A55),
        hex(0x5F6B85),
        hex(0x9CA3AF),
        hex(0xE5E7EB),
    ],
    select: (
        "\x1b[38;2;229;231;235m\x1b[48;2;22;33;58m",
        "\x1b[39m\x1b[49m",
    ),
    ..BASE
};

const HC_STRONG: Rgb = hex(0xFFFFFF);
pub static HIGH_CONTRAST: Theme = Theme {
    id: ThemeId::HighContrast,
    border: hex(0x8C929B),
    label: hex(0xD0D4DA),
    muted: hex(0xB8BDC6),
    body: hex(0xF2F3F5),
    strong: HC_STRONG,
    accent: hex(0xF7F8FA),
    accent_light: HC_STRONG,
    pearl: hex(0xE8DDD3),
    ok: hex(0x9BE7B0),
    warn: hex(0xFFD479),
    err: hex(0xFF9C9C),
    info: hex(0xA9C1F0),
    sand: hex(0xE3D3AE),
    heat: [
        hex(0x3A3F4A),
        hex(0x6B717C),
        hex(0x9CA3AF),
        hex(0xD0D4DA),
        HC_STRONG,
    ],
    tints: [hex(0x9BE7B0), hex(0xA9C1F0), hex(0xE3D3AE), hex(0xE8DDD3)],
    fg_attrs: &[(HC_STRONG, BOLD_SGR)],
    select: (
        "\x1b[1m\x1b[38;2;255;255;255m\x1b[48;2;36;40;48m",
        "\x1b[22m\x1b[39m\x1b[49m",
    ),
    ..BASE
};

/// Mono carries no colour: three arbitrary stand-ins (bold, dim, plain) tell the attributes apart.
const M_BOLD: Rgb = hex(0xFFFFFF);
const M_DIM: Rgb = hex(0x808080);
const M_PLAIN: Rgb = hex(0xC8C8C8);
pub static MONO: Theme = Theme {
    id: ThemeId::Mono,
    bg_ref: hex(0x000000),
    paint_bg: false,
    flat: false,
    surface: M_PLAIN,
    elevated: M_PLAIN,
    border: M_DIM,
    label: M_DIM,
    muted: M_DIM,
    body: M_PLAIN,
    strong: M_BOLD,
    accent: M_BOLD,
    accent_light: M_BOLD,
    pearl: M_BOLD,
    ok: M_PLAIN,
    warn: M_PLAIN,
    err: M_BOLD,
    info: M_PLAIN,
    sand: M_PLAIN,
    ink: M_PLAIN,
    diff_add_bg: M_PLAIN,
    diff_del_bg: M_PLAIN,
    heat: [M_DIM, M_DIM, M_PLAIN, M_PLAIN, M_BOLD],
    tints: [M_PLAIN; 4],
    syntax: Syntax {
        fg: M_PLAIN,
        keyword: M_BOLD,
        storage: M_BOLD,
        builtin: M_PLAIN,
        literal: M_PLAIN,
        string: M_PLAIN,
        comment: M_DIM,
        variable: M_PLAIN,
        add: M_PLAIN,
        del: M_PLAIN,
    },
    fg_attrs: &[(M_BOLD, BOLD_SGR), (M_DIM, DIM_SGR)],
    bg_attrs: &[],
    select: (REVERSE_SGR, "\x1b[27m"),
    extra: None,
};

pub static PEARL: Theme = Theme {
    id: ThemeId::Pearl,
    bg_ref: hex(0xF7F8FA),
    paint_bg: false,
    flat: false,
    surface: hex(0xEDEFF3),
    elevated: hex(0xE1E4EA),
    border: hex(0xB7BCC6),
    label: hex(0x4F5560),
    muted: hex(0x5B616E),
    body: hex(0x3A3F4A),
    strong: hex(0x15171B),
    accent: hex(0x2A2E37),
    accent_light: hex(0x0B0C0F),
    pearl: hex(0x7A5C45),
    ok: hex(0x2F6B45),
    warn: hex(0x8A5A00),
    err: hex(0xB03A3A),
    info: hex(0x2F5F9E),
    sand: hex(0x7A6A45),
    ink: hex(0xF7F8FA),
    diff_add_bg: hex(0xDDEFE3),
    diff_del_bg: hex(0xF5DEDE),
    heat: [
        hex(0xE1E4EA),
        hex(0xB7BCC6),
        hex(0x8A909B),
        hex(0x5B616E),
        hex(0x15171B),
    ],
    tints: [hex(0x2F6B45), hex(0x2F5F9E), hex(0x7A6A45), hex(0x7A5C45)],
    syntax: Syntax {
        fg: hex(0x3A3F4A),
        keyword: hex(0xB0245C),
        storage: hex(0x1F6F8B),
        builtin: hex(0x4F7A12),
        literal: hex(0x6B3FB8),
        string: hex(0x8A5A00),
        comment: hex(0x5B616E),
        variable: hex(0x15171B),
        add: hex(0x2F6B45),
        del: hex(0xB03A3A),
    },
    fg_attrs: &[],
    bg_attrs: &[],
    select: (
        "\x1b[38;2;21;23;27m\x1b[48;2;225;228;234m",
        "\x1b[39m\x1b[49m",
    ),
    extra: None,
};

/// 16-colour terminals: every value is exactly one of the ANSI colours that
/// `synthesis_tui_base::sgr::encode` maps one to one (slots in the comments).
const A_BLACK: Rgb = [0, 0, 0]; // 0
const A_RED: Rgb = [255, 0, 0]; // 9 (slot 1, (205,0,0), is only 3.6:1 on black)
const A_GREEN: Rgb = [0, 205, 0]; // 2
const A_YELLOW: Rgb = [205, 205, 0]; // 3
const A_CYAN: Rgb = [0, 205, 205]; // 6
const A_WHITE: Rgb = [229, 229, 229]; // 7
const A_GREY: Rgb = [127, 127, 127]; // 8
const A_BRIGHT: Rgb = [255, 255, 255]; // 15
pub static ANSI16: Theme = Theme {
    id: ThemeId::Ansi16,
    bg_ref: A_BLACK,
    paint_bg: false,
    flat: false,
    surface: A_BLACK,
    elevated: A_BLACK,
    border: A_GREY,
    label: A_WHITE,
    muted: A_GREY,
    body: A_WHITE,
    strong: A_BRIGHT,
    accent: A_BRIGHT,
    accent_light: A_BRIGHT,
    pearl: A_BRIGHT,
    ok: A_GREEN,
    warn: A_YELLOW,
    err: A_RED,
    info: A_CYAN,
    sand: A_YELLOW,
    ink: A_BLACK,
    diff_add_bg: A_BLACK,
    diff_del_bg: A_BLACK,
    heat: [A_GREY, A_GREY, A_WHITE, A_WHITE, A_BRIGHT],
    tints: [A_GREEN, A_CYAN, A_YELLOW, A_BRIGHT],
    syntax: Syntax {
        fg: A_WHITE,
        keyword: A_RED,
        storage: A_CYAN,
        builtin: A_GREEN,
        literal: A_BRIGHT,
        string: A_YELLOW,
        comment: A_GREY,
        variable: A_BRIGHT,
        add: A_GREEN,
        del: A_RED,
    },
    fg_attrs: &[],
    bg_attrs: &[],
    select: (REVERSE_SGR, "\x1b[27m"),
    extra: None,
};

pub fn theme_for(id: ThemeId) -> &'static Theme {
    match id {
        ThemeId::Classic => &CLASSIC,
        ThemeId::Graphite => &GRAPHITE,
        ThemeId::Synthesis => &SYNTHESIS,
        ThemeId::Midnight => &MIDNIGHT,
        ThemeId::HighContrast => &HIGH_CONTRAST,
        ThemeId::Mono => &MONO,
        ThemeId::Pearl => &PEARL,
        ThemeId::System => &SYSTEM,
        ThemeId::Ansi16 => &ANSI16,
    }
}

/// The theme for a terminal: `NO_COLOR` (Mono) and 16 colours override the user's choice.
pub fn select_theme(mode: ColorMode, user: ThemeId) -> ThemeId {
    match mode {
        ColorMode::Mono => ThemeId::Mono,
        ColorMode::Ansi16 => ThemeId::Ansi16,
        ColorMode::TrueColor | ColorMode::Ansi256 => match user {
            ThemeId::Ansi16 => ThemeId::Synthesis,
            u => u,
        },
    }
}

// ---------------------------------------------------------------- glyphs

/// Every symbol the chrome draws, in a Unicode and an ASCII set (spec §4). Each Unicode glyph is
/// one cell wide; the ASCII set is for terminals that draw ambiguous-width characters in two
/// cells and for `TERM=linux`.
pub struct Glyphs {
    /// Braille ring mark (two cells); empty in ASCII.
    pub mark: &'static str,
    pub spinner: &'static [&'static str],
    pub live: &'static str,
    pub idle: &'static str,
    pub attention: &'static str,
    pub ok: &'static str,
    pub fail: &'static str,
    pub tool: &'static str,
    pub user: &'static str,
    pub thinking: &'static str,
    pub focus: &'static str,
    pub locked: &'static str,
    pub meter_full: &'static str,
    pub meter_empty: &'static str,
    pub bar: &'static str,
    pub add: &'static str,
    pub del: &'static str,
    pub branch: &'static str,
    pub dot: &'static str,
    pub vbar: &'static str,
    pub dash: &'static str,
    /// Thin rounded card border.
    pub box_tl: &'static str,
    pub box_tr: &'static str,
    pub box_bl: &'static str,
    pub box_br: &'static str,
    pub box_h: &'static str,
    pub box_v: &'static str,
}

pub static UNICODE: Glyphs = Glyphs {
    mark: "⢎⡱",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    live: "●",
    idle: "○",
    attention: "◆",
    ok: "✓",
    fail: "✗",
    tool: "◇",
    user: "›",
    thinking: "∴",
    focus: "▍",
    locked: "▣",
    meter_full: "▰",
    meter_empty: "▱",
    bar: "▇",
    add: "+",
    del: "−",
    branch: "⎇",
    dot: "·",
    vbar: "│",
    dash: "┄",
    box_tl: "╭",
    box_tr: "╮",
    box_bl: "╰",
    box_br: "╯",
    box_h: "─",
    box_v: "│",
};

pub static ASCII: Glyphs = Glyphs {
    mark: "",
    spinner: &["|", "/", "-", "\\"],
    live: "*",
    idle: "o",
    attention: "!",
    ok: "+",
    fail: "x",
    tool: ">",
    user: ">",
    thinking: ":",
    focus: ">",
    locked: "#",
    meter_full: "#",
    meter_empty: "-",
    bar: "#",
    add: "+",
    del: "-",
    branch: "git:",
    dot: "-",
    vbar: "|",
    dash: "-",
    box_tl: "+",
    box_tr: "+",
    box_bl: "+",
    box_br: "+",
    box_h: "-",
    box_v: "|",
};

impl Glyphs {
    pub fn spin(&self, frame: usize) -> &'static str {
        self.spinner[frame % self.spinner.len()]
    }
}

// ---------------------------------------------------------------- scope

/// What a render call needs besides its arguments. Set once per call by the entry point
/// (`ConfigScope`), so no signature carries it; the default is Synthesis with Unicode glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderConfig {
    pub theme: ThemeId,
    pub ascii: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            theme: ThemeId::Synthesis,
            ascii: false,
        }
    }
}

thread_local! {
    static CURRENT: Cell<RenderConfig> = Cell::new(RenderConfig::default());
}

/// Keeps `cfg` current on this thread until dropped (then the previous one returns).
pub struct ConfigScope(RenderConfig);

impl ConfigScope {
    pub fn new(cfg: RenderConfig) -> Self {
        let prev = CURRENT.with(|c| c.replace(cfg));
        synthesis_tui_base::selection::set_highlight(theme_for(cfg.theme).select);
        Self(prev)
    }
}

impl Drop for ConfigScope {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.0));
        synthesis_tui_base::selection::set_highlight(theme_for(self.0.theme).select);
    }
}

pub fn config() -> RenderConfig {
    CURRENT.with(Cell::get)
}

/// The current theme.
pub fn t() -> &'static Theme {
    theme_for(config().theme)
}

/// The current glyph set.
pub fn glyphs() -> &'static Glyphs {
    if config().ascii { &ASCII } else { &UNICODE }
}

macro_rules! extra_colour {
    ($($(#[$d:meta])* $name:ident => $fallback:ident),*) => {
        impl Theme {$(
            $(#[$d])*
            pub fn $name(&self) -> Rgb {
                self.extra.map_or(self.$fallback, |x| x.$name)
            }
        )*}
    };
}
extra_colour! {
    /// Empty part of a bar.
    track => border,
    slate => label,
    cream => accent_light,
    blue => info,
    teal => ok,
    purple => pearl,
    pink => sand,
    orange => warn,
    code => info
}

impl Theme {
    fn attr(&self, table: &'static [(Rgb, &'static str)], c: Rgb) -> &'static str {
        table.iter().find(|(k, _)| *k == c).map_or("", |(_, a)| a)
    }
}

/// One of the agent tints (eight in Classic and Synthesis, four elsewhere), by a hash of the agent kind.
pub fn agent_color(kind: &str) -> Rgb {
    let mut h: u32 = 0;
    for ch in kind.chars() {
        h = h.wrapping_mul(31).wrapping_add(ch as u32);
    }
    match t().extra {
        Some(x) => x.agents[(h as usize) % 8],
        None => t().tints[(h as usize) % 4],
    }
}

pub const RESET: &str = "\x1b[0m";
pub const ITALIC: &str = "\x1b[3m";
/// Highlights change colour only, never weight (Node `BOLD_ON = false`).
pub const BOLD: &str = "";
pub const BOLD_ON: bool = false;

/// `CLAUDE_SPIN`: forward `· ✢ ✶ ✻ ✽`, then the middle three backwards `✻ ✶ ✢`.
pub fn spin(frame: usize) -> &'static str {
    const S: [&str; 8] = ["·", "✢", "✶", "✻", "✽", "✻", "✶", "✢"];
    S[frame % S.len()]
}

/// `"…"` that moves: 0 to 3 dots padded to 3 cells, driven by the frame counter.
pub fn dots(frame: usize) -> String {
    let n = (frame / 3) % 4;
    format!("{}{}", ".".repeat(n), " ".repeat(3 - n))
}

/// "Checking config…" -> "Checking config" + moving dots.
pub fn waiting(text: &str, frame: usize) -> String {
    let t = text.trim_end_matches(synthesis_tui_base::text::is_js_space);
    let base = t
        .strip_suffix('…')
        .or_else(|| t.strip_suffix("..."))
        .unwrap_or(text);
    format!("{base}{}", dots(frame))
}

pub fn fg(c: Rgb) -> String {
    let t = t();
    format!(
        "{}\x1b[38;2;{};{};{}m",
        t.attr(t.fg_attrs, c),
        c[0],
        c[1],
        c[2]
    )
}
pub fn bg(c: Rgb) -> String {
    let t = t();
    if t.flat {
        return String::new();
    }
    format!(
        "{}\x1b[48;2;{};{};{}m",
        t.attr(t.bg_attrs, c),
        c[0],
        c[1],
        c[2]
    )
}

/// `Math.round`: halves go up.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub fn lerp(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let mut o = [0u8; 3];
    for i in 0..3 {
        let v = f64::from(a[i]) + (f64::from(b[i]) - f64::from(a[i])) * t;
        o[i] = js_round(v).clamp(0.0, 255.0) as u8;
    }
    o
}

/// Position `t` (wraps around) on a multi-stop gradient.
pub fn cycle(stops: &[Rgb], t: f64) -> Rgb {
    let u = t - t.floor();
    let seg = u * (stops.len() - 1) as f64;
    let i = seg.floor() as usize;
    lerp(
        stops[i],
        stops[(i + 1).min(stops.len() - 1)],
        seg - i as f64,
    )
}

/// Triangle-wave gradient between two colours, for pulsing borders.
pub fn pulse(a: Rgb, b: Rgb, frame: u64, period: u64) -> Rgb {
    let p = (frame % period) as f64 / period as f64;
    lerp(a, b, if p < 0.5 { p * 2.0 } else { 2.0 - p * 2.0 })
}

/// `fg(c) + s + RESET`.
pub fn paint(c: Rgb, s: &str) -> String {
    format!("{}{s}{RESET}", fg(c))
}

/// Pad or truncate to exactly `w` cells, keeping colour codes intact and never splitting a
/// grapheme cluster.
pub fn fit(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let stripped = strip_ansi(s);
    let ascii = stripped.bytes().all(|b| (0x20..=0x7e).contains(&b));
    let total = if ascii { stripped.len() } else { width(s) };
    if total <= w {
        let mut o = String::with_capacity(s.len() + w - total);
        o.push_str(s);
        o.extend(std::iter::repeat_n(' ', w - total));
        return o;
    }
    let budget = w - 1; // one cell for the ellipsis
    let mut out = String::with_capacity(s.len());
    let mut used = 0;
    let mut i = 0;
    'outer: while i < s.len() {
        if let Some(n) = csi_len(s, i) {
            out.push_str(&s[i..i + n]);
            i += n;
            continue;
        }
        let mut j = i;
        while j < s.len() {
            if csi_len(s, j).is_some() {
                break;
            }
            j += s[j..].chars().next().map_or(1, char::len_utf8);
        }
        if ascii {
            for ch in s[i..j].chars() {
                if used + 1 > budget {
                    break 'outer;
                }
                out.push(ch);
                used += 1;
            }
        } else {
            for g in s[i..j].graphemes(true) {
                let cw = cluster_width(g);
                if used + cw > budget {
                    break 'outer;
                }
                out.push_str(g);
                used += cw;
            }
        }
        i = j;
    }
    out.push('…');
    out.push_str(RESET);
    out.extend(std::iter::repeat_n(' ', budget.saturating_sub(used)));
    out
}

/// Joins `(priority, text)` items with `sep` and drops whole items (largest priority number first, the
/// later one on ties) until the line fits `w`, so a word is never cut mid-way (QA B-27). One remaining
/// item that is still too long is ellipsized by `fit`.
pub fn fit_items(items: &[(u8, String)], sep: &str, w: usize) -> String {
    let mut keep = vec![true; items.len()];
    let join = |keep: &[bool]| {
        let parts: Vec<&str> = items
            .iter()
            .zip(keep)
            .filter(|(_, k)| **k)
            .map(|(i, _)| i.1.as_str())
            .collect();
        parts.join(sep)
    };
    while width(&join(&keep)) > w && keep.iter().filter(|k| **k).count() > 1 {
        if let Some(i) = (0..items.len())
            .filter(|i| keep[*i])
            .max_by_key(|i| (items[*i].0, *i))
        {
            keep[i] = false;
        }
    }
    fit(&join(&keep), w)
}

/// A zero-based list `0..n` helper for `' '.repeat(n)`.
pub fn spaces(n: usize) -> String {
    " ".repeat(n)
}

#[cfg(test)]
mod fit_items_tests {
    use super::*;

    #[test]
    fn drops_whole_items_by_priority_and_never_cuts_a_word() {
        let items = [
            (1, "mode on".to_string()),
            (3, "ctx 5%".to_string()),
            (2, "model-x".to_string()),
        ];
        assert_eq!(
            fit_items(&items, " · ", 40).trim_end(),
            "mode on · ctx 5% · model-x"
        );
        assert_eq!(fit_items(&items, " · ", 20).trim_end(), "mode on · model-x");
        assert_eq!(fit_items(&items, " · ", 9).trim_end(), "mode on");
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;
    use synthesis_tui_base::sgr::encode;

    const ALL: [ThemeId; 8] = [
        ThemeId::Synthesis,
        ThemeId::Classic,
        ThemeId::Graphite,
        ThemeId::Midnight,
        ThemeId::HighContrast,
        ThemeId::Mono,
        ThemeId::Pearl,
        ThemeId::Ansi16,
    ];

    fn lin(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn lum(c: Rgb) -> f64 {
        0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
    }
    /// WCAG 2.x contrast ratio.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_colour_theme_meets_the_contrast_floors() {
        let mut report = vec![];
        for id in ALL.into_iter().filter(|i| *i != ThemeId::Mono) {
            let t = theme_for(id);
            // text is read on the terminal background and on the header band
            for (bgname, bg) in [("bg", t.bg_ref), ("surface", t.surface)] {
                if id == ThemeId::Ansi16 && bgname == "surface" {
                    continue; // not painted in 16 colours
                }
                let floor = |name: &str, c: Rgb, min: f64| {
                    // Exception: Classic and Synthesis keep the old T_SUBTLE (112,112,120) for muted
                    // text (3.98:1 on Classic's bg, lower on its header band; floor 3.5).
                    let min =
                        if matches!(id, ThemeId::Classic | ThemeId::Synthesis) && name == "muted" {
                            3.5
                        } else {
                            min
                        };
                    let r = contrast(c, bg);
                    assert!(r >= min, "{id:?} {name} on {bgname}: {r:.2} < {min}");
                    r
                };
                for (n, c) in [("body", t.body), ("strong", t.strong)] {
                    floor(n, c, 7.0);
                }
                for (n, c) in [
                    ("label", t.label),
                    ("muted", t.muted),
                    ("accent", t.accent),
                    ("ok", t.ok),
                    ("warn", t.warn),
                    ("err", t.err),
                    ("info", t.info),
                ] {
                    floor(n, c, 4.5);
                }
            }
            if id != ThemeId::Ansi16 {
                for (n, d) in [("diff_add", t.diff_add_bg), ("diff_del", t.diff_del_bg)] {
                    let r = contrast(t.body, d);
                    assert!(r >= 7.0, "{id:?} body on {n}: {r:.2}");
                }
                let bmin = if id == ThemeId::HighContrast {
                    3.0
                } else {
                    1.5
                };
                let r = contrast(t.border, t.bg_ref);
                assert!(r >= bmin, "{id:?} border {r:.2} < {bmin}");
                let r = contrast(t.strong, t.elevated);
                assert!(r >= 7.0, "{id:?} strong on elevated {r:.2}");
            }
            report.push(format!(
                "{id:?}: body {:.2} strong {:.2} label {:.2} muted {:.2} ok {:.2} warn {:.2} err {:.2} info {:.2} border {:.2}",
                contrast(t.body, t.bg_ref), contrast(t.strong, t.bg_ref), contrast(t.label, t.bg_ref), contrast(t.muted, t.bg_ref),
                contrast(t.ok, t.bg_ref), contrast(t.warn, t.bg_ref), contrast(t.err, t.bg_ref), contrast(t.info, t.bg_ref), contrast(t.border, t.bg_ref)
            ));
        }
        println!("{}", report.join("\n"));
    }

    fn tokens(t: &Theme) -> [(&'static str, Rgb); 13] {
        [
            ("border", t.border),
            ("label", t.label),
            ("muted", t.muted),
            ("body", t.body),
            ("strong", t.strong),
            ("accent", t.accent),
            ("accent_light", t.accent_light),
            ("pearl", t.pearl),
            ("ok", t.ok),
            ("warn", t.warn),
            ("err", t.err),
            ("info", t.info),
            ("sand", t.sand),
        ]
    }

    #[test]
    fn graphite_maps_to_the_256_colours_of_the_spec() {
        let want = [
            ("border", 238),
            ("label", 246),
            ("muted", 244),
            ("body", 253),
            ("strong", 255),
            ("accent", 69),
            ("accent_light", 153),
            ("pearl", 252),
            ("ok", 151),
            ("warn", 179),
            ("err", 174),
            ("info", 110),
        ];
        let mut bad = vec![];
        for (n, c) in tokens(&GRAPHITE) {
            let got = encode(
                &format!("\x1b[38;2;{};{};{}mx", c[0], c[1], c[2]),
                ColorMode::Ansi256,
            );
            if let Some((_, w)) = want.iter().find(|(k, _)| *k == n)
                && got != format!("\x1b[38;5;{w}mx\x1b[0m")
            {
                bad.push(format!("{n}: {got:?} want {w}"));
            }
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn every_theme_resolves_in_256_and_16_colours() {
        for id in ALL {
            for (n, c) in tokens(theme_for(id)) {
                let s = format!("\x1b[38;2;{};{};{}mx", c[0], c[1], c[2]);
                assert!(
                    encode(&s, ColorMode::Ansi256).starts_with("\x1b[38;5;"),
                    "{id:?} {n}"
                );
                let e = encode(&s, ColorMode::Ansi16);
                assert!(
                    !e.contains("38;2") && !e.contains("38;5"),
                    "{id:?} {n}: {e:?}"
                );
            }
        }
    }

    #[test]
    fn ansi16_theme_uses_exact_ansi_colours() {
        // ANSI slot: 2 green, 3 yellow, 6 cyan, 7 white, 8 grey, 9 bright red, 15 bright white
        let want = [
            ("ok", ANSI16.ok, "\x1b[32mx\x1b[0m"),
            ("warn", ANSI16.warn, "\x1b[33mx\x1b[0m"),
            ("info", ANSI16.info, "\x1b[36mx\x1b[0m"),
            ("body", ANSI16.body, "\x1b[37mx\x1b[0m"),
            ("muted", ANSI16.muted, "\x1b[90mx\x1b[0m"),
            ("err", ANSI16.err, "\x1b[91mx\x1b[0m"),
            ("strong", ANSI16.strong, "\x1b[97mx\x1b[0m"),
        ];
        for (n, c, code) in want {
            let got = encode(
                &format!("\x1b[38;2;{};{};{}mx", c[0], c[1], c[2]),
                ColorMode::Ansi16,
            );
            assert_eq!(got, code, "{n}");
        }
    }

    #[test]
    fn mono_uses_bold_dim_and_reverse_only() {
        let _s = ConfigScope::new(RenderConfig {
            theme: ThemeId::Mono,
            ascii: false,
        });
        let t = t();
        assert_eq!(
            encode(&paint(t.strong, "x"), ColorMode::Mono),
            "\x1b[1mx\x1b[0m"
        );
        assert_eq!(
            encode(&paint(t.muted, "x"), ColorMode::Mono),
            "\x1b[2mx\x1b[0m"
        );
        assert!(encode(&paint(t.body, "x"), ColorMode::Mono).starts_with('x'));
        assert_eq!(t.select.0, "\x1b[7m");
        // ok/warn/info/body cannot be told apart by attribute: their meaning must ride on glyphs
        assert!([t.ok, t.warn, t.info, t.body].iter().all(|c| *c == t.body));
        for (n, c) in tokens(t) {
            let e = encode(&paint(c, "x"), ColorMode::Mono);
            assert!(!e.contains("38;") && !e.contains("48;"), "{n}: {e:?}");
        }
    }

    #[test]
    fn states_are_never_told_apart_by_colour_alone() {
        // every state has its own mark in both glyph sets
        for g in [&UNICODE, &ASCII] {
            // the state marks; the ASCII set shares `>` between tool, prompt and focus by design (spec §4)
            let marks = [g.ok, g.fail, g.attention, g.live, g.idle];
            for (i, a) in marks.iter().enumerate() {
                for b in &marks[i + 1..] {
                    assert_ne!(a, b, "two states share the mark {a:?}");
                }
            }
        }
        // and the four status colours are four different colours in every colour theme
        for id in [
            ThemeId::Synthesis,
            ThemeId::Classic,
            ThemeId::Graphite,
            ThemeId::Midnight,
            ThemeId::HighContrast,
            ThemeId::Pearl,
        ] {
            let t = theme_for(id);
            assert!(
                t.ok != t.warn && t.warn != t.err && t.ok != t.err && t.info != t.ok,
                "{id:?}"
            );
        }
    }

    #[test]
    fn glyphs_are_one_cell_wide_and_the_ascii_set_is_ascii() {
        let mut all: Vec<&str> = vec![
            UNICODE.mark,
            UNICODE.live,
            UNICODE.idle,
            UNICODE.attention,
            UNICODE.ok,
            UNICODE.fail,
            UNICODE.tool,
            UNICODE.user,
            UNICODE.thinking,
            UNICODE.focus,
            UNICODE.locked,
            UNICODE.meter_full,
            UNICODE.meter_empty,
            UNICODE.bar,
            UNICODE.add,
            UNICODE.del,
            UNICODE.branch,
            UNICODE.dot,
            UNICODE.vbar,
            UNICODE.dash,
        ];
        all.extend(UNICODE.spinner);
        for g in all {
            for c in g.chars() {
                assert_eq!(
                    synthesis_tui_base::text::char_width(c),
                    1,
                    "{c:?} (U+{:04X})",
                    c as u32
                );
                assert!(
                    (c as u32) < 0x1F000 && c != '\u{FE0F}' && c != '\u{200D}',
                    "emoji presentation {c:?}"
                );
            }
        }
        let mut asc: Vec<&str> = vec![
            ASCII.mark,
            ASCII.live,
            ASCII.idle,
            ASCII.attention,
            ASCII.ok,
            ASCII.fail,
            ASCII.tool,
            ASCII.user,
            ASCII.thinking,
            ASCII.focus,
            ASCII.locked,
            ASCII.meter_full,
            ASCII.meter_empty,
            ASCII.bar,
            ASCII.add,
            ASCII.del,
            ASCII.branch,
            ASCII.dot,
            ASCII.vbar,
            ASCII.dash,
        ];
        asc.extend(ASCII.spinner);
        assert!(
            asc.iter()
                .all(|g| g.bytes().all(|b| (0x20..0x7f).contains(&b)))
        );
    }

    #[test]
    fn no_emoji_presentation_characters_in_the_renderer_sources() {
        let files = [
            include_str!("agent.rs"),
            include_str!("dash.rs"),
            include_str!("flow.rs"),
            include_str!("dialogs.rs"),
            include_str!("picker.rs"),
            include_str!("logo_art.rs"),
            include_str!("agent/diffview.rs"),
            include_str!("agent/thinking.rs"),
        ];
        for src in files {
            let code = src.split("#[cfg(test)]").next().unwrap();
            for c in code.chars().filter(|c| !c.is_ascii()) {
                assert!(
                    (c as u32) < 0x1F000 && c != '\u{FE0F}' && c != '\u{200D}',
                    "emoji presentation {c:?}"
                );
                assert_eq!(
                    synthesis_tui_base::text::char_width(c),
                    1,
                    "{c:?} (U+{:04X}) is not one cell wide",
                    c as u32
                );
            }
        }
    }
}

/// Paints the app background under a finished frame (themes with `paint_bg`): every row starts on
/// the background colour and returns to it after each reset, so only cells that name another
/// background (cards, selection, diffs) differ. Other themes: the rows unchanged.
pub fn paint_background(lines: Vec<String>) -> Vec<String> {
    let th = t();
    if !th.paint_bg {
        return lines;
    }
    let b = bg(th.bg_ref);
    lines
        .into_iter()
        .map(|l| {
            format!(
                "{b}{}{RESET}",
                l.replace(RESET, &format!("{RESET}{b}"))
                    .replace("\x1b[49m", &format!("\x1b[49m{b}"))
            )
        })
        .collect()
}
