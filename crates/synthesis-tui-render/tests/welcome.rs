//! Goldens of the welcome (brand art, greeting by the local hour, "What shall we build?"):
//! `tests/golden/welcome/<case>/{expected.txt, expected.ansi}`. `expected.txt` is the plain rows,
//! `expected.ansi` the rows as the terminal gets them in that colour mode. Rewrite with
//! `SYNTHESIS_UPDATE_GOLDEN=1 cargo test -p synthesis-tui-render --test welcome`.

use std::path::PathBuf;
use synthesis_tui_base::sgr::{ColorMode, encode};
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{Agent, AgentOpts, EditorView, Item, render_agent};

/// 09:00 UTC, 2026-01-01.
const NOW: i64 = 1_767_258_000_000;

fn frame(cols: usize, rows: usize, mode: ColorMode, tz: i32) -> (Vec<String>, Vec<String>) {
    frame_with(cols, rows, mode, tz, &[])
}

/// `notes`: notices that arrived before the first message (kind `note`, tone `warn`).
fn frame_with(
    cols: usize,
    rows: usize,
    mode: ColorMode,
    tz: i32,
    notes: &[&str],
) -> (Vec<String>, Vec<String>) {
    let agent = Agent {
        items: notes
            .iter()
            .map(|t| {
                serde_json::from_value::<Item>(
                    serde_json::json!({"kind": "note", "text": t, "tone": "warn"}),
                )
                .unwrap()
            })
            .collect(),
        cwd: "/work/proj".into(),
        version: "0.1.0".into(),
        mode: "default".into(),
        status: "idle".into(),
        model: "fake/model".into(),
        ..Agent::default()
    };
    let opts = AgentOpts {
        hero: true,
        tz_offset_min: tz,
        plain_logo: matches!(mode, ColorMode::Ansi16 | ColorMode::Mono),
        ..AgentOpts::default()
    };
    let out = render_agent(
        &agent,
        &[],
        &EditorView { chars: &[], cur: 0 },
        cols,
        rows,
        0,
        NOW,
        &opts,
    );
    let plain = out.lines.iter().map(|l| strip_ansi(l)).collect();
    let ansi = out.lines.iter().map(|l| encode(l, mode)).collect();
    (plain, ansi)
}

fn check(
    case: &str,
    cols: usize,
    rows: usize,
    mode: ColorMode,
    tz: i32,
) -> (Vec<String>, Vec<String>) {
    check_with(case, cols, rows, mode, tz, &[])
}

fn check_with(
    case: &str,
    cols: usize,
    rows: usize,
    mode: ColorMode,
    tz: i32,
    notes: &[&str],
) -> (Vec<String>, Vec<String>) {
    let (plain, ansi) = frame_with(cols, rows, mode, tz, notes);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/welcome")
        .join(case);
    let (txt, raw) = (
        format!("{}\n", plain.join("\n")),
        format!("{}\n", ansi.join("\n")),
    );
    if std::env::var_os("SYNTHESIS_UPDATE_GOLDEN").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("expected.txt"), &txt).unwrap();
        std::fs::write(dir.join("expected.ansi"), &raw).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("expected.txt")).unwrap(),
        txt,
        "{case} text"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("expected.ansi")).unwrap(),
        raw,
        "{case} ansi"
    );
    (plain, ansi)
}

fn has_art(rows: &[String]) -> bool {
    rows.iter()
        .any(|r| r.contains('\u{28ff}') || r.contains('⣿'))
}

#[test]
fn welcome_80x24_shows_the_art_without_the_tips() {
    let (plain, _) = check("80x24", 80, 24, ColorMode::TrueColor, 0);
    assert!(has_art(&plain));
    assert!(plain.iter().any(|r| r.contains("Good morning.")));
    assert!(plain.iter().any(|r| r.contains("What shall we build?")));
}

#[test]
fn welcome_120x40_shows_art_wordmark_tagline_box_and_tips() {
    let (plain, _) = check("120x40", 120, 40, ColorMode::TrueColor, 0);
    assert!(has_art(&plain));
    for want in [
        "S Y N T H E S I S",
        "Autonomous development environment",
        "Tips for getting started",
    ] {
        assert!(plain.iter().any(|r| r.contains(want)), "{want}");
    }
}

#[test]
fn welcome_tiny_40x10_degrades_to_the_wordmark() {
    let (plain, _) = check("tiny-40x10", 40, 10, ColorMode::TrueColor, 0);
    assert!(!has_art(&plain));
    assert!(plain.iter().any(|r| r.contains("S Y N T H E S I S")));
}

#[test]
fn greeting_follows_the_zone() {
    // 09:00 UTC is 23:00 in UTC-10: night
    let (plain, _) = frame(80, 24, ColorMode::TrueColor, -600);
    assert!(plain.iter().any(|r| r.contains("Working late.")));
}

#[test]
fn mono_has_no_colour_and_ansi16_draws_the_art_in_one_colour() {
    let (plain, ansi) = check("mono-80x24", 80, 24, ColorMode::Mono, 0);
    assert!(has_art(&plain));
    assert!(
        ansi.iter()
            .all(|l| !l.contains("38;2;") && !l.contains("38;5;"))
    );
    let (_, ansi16) = check("ansi16-80x24", 80, 24, ColorMode::Ansi16, 0);
    // the art rows (the first rows of the pane) use one foreground code
    let art: Vec<&String> = ansi16
        .iter()
        .filter(|l| strip_ansi(l).contains('⣿'))
        .collect();
    assert!(!art.is_empty());
    let mut codes = std::collections::BTreeSet::new();
    for l in art {
        for part in l.split("\x1b[").skip(1) {
            let code = part.split('m').next().unwrap_or("");
            if code.starts_with("3") || code.starts_with("9") {
                codes.insert(code.to_string());
            }
        }
    }
    assert_eq!(codes.len(), 1, "{codes:?}");
}

const SANDBOX_NOTE: &str =
    "unsandboxed_by_request: --sandbox off, commands run with the agent's full privileges";

#[test]
fn a_startup_notice_does_not_hide_the_welcome_it_comes_after_it() {
    let (plain, _) = check_with(
        "notice-120x40",
        120,
        40,
        ColorMode::TrueColor,
        0,
        &[SANDBOX_NOTE],
    );
    let at = |t: &str| plain.iter().position(|r| r.contains(t));
    let (hello, note) = (at("Good morning."), at("unsandboxed_by_request"));
    assert!(hello.is_some() && note.is_some(), "{plain:#?}");
    assert!(hello < note, "the notice is below the welcome");
    assert!(has_art(&plain));
}

#[test]
fn a_short_welcome_drops_whole_tips_never_half_of_one() {
    let tips = |plain: &[String]| {
        ["1. Ask Synthesis", "2. Type /", "3. Drop or paste"]
            .iter()
            .filter(|t| plain.iter().any(|r| r.contains(*t)))
            .count()
    };
    let mut seen = std::collections::BTreeSet::new();
    for rows in 14..=34 {
        let (plain, _) = frame_with(80, rows, ColorMode::TrueColor, 0, &[SANDBOX_NOTE]);
        let n = tips(&plain);
        // tips are shown from the first on, whole
        for (i, full) in [
            "Ask Synthesis to explain, fix or build something in this folder",
            "Type / for commands and skills, @ to mention a file",
            "Drop or paste an image path to attach a screenshot",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                plain.iter().any(|r| r.contains(full)),
                i < n,
                "rows {rows} tip {i}"
            );
        }
        seen.insert(n);
    }
    assert!(seen.contains(&0) && seen.contains(&3), "{seen:?}");
}

#[test]
fn the_wordmark_never_touches_the_pane_top() {
    // a pane too short for the box: the wordmark has a blank row above it
    let (plain, _) = frame(80, 12, ColorMode::TrueColor, 0);
    let w = plain
        .iter()
        .position(|r| r.contains("S Y N T H E S I S"))
        .unwrap();
    assert!(w > 0 && plain[w - 1].trim().is_empty(), "{plain:#?}");
}
