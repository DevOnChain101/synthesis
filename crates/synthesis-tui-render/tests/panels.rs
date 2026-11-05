//! `synt.tui.panels`: a sidebar panel switched off disappears and the panels that stay take
//! its rows (no hole). Text goldens in `tests/golden/panels/<case>/expected.txt` (colour-agnostic);
//! rewrite with `SYNT_UPDATE_GOLDEN=1 cargo test -p synthesis-tui-render --test panels`.

use std::path::PathBuf;
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{Agent, EditorView};
use synthesis_tui_render::dash::{
    DashState, DashView, Focus, Hidden, HistoryDay, Live, SessionRow, Usage, Workspace,
    render_frame, workspace_key,
};
use synthesis_tui_render::model::{FeedItem, session_from_events};
use synthesis_tui_render::util::basename;

const NOW: i64 = 1_800_000_000_000;

fn frame(cols: usize, rows: usize, hidden: Hidden) -> Vec<String> {
    frame_with(cols, rows, hidden, &[], false)
}

fn frame_with(
    cols: usize,
    rows: usize,
    hidden: Hidden,
    history: &[HistoryDay],
    history_loading: bool,
) -> Vec<String> {
    raw_frame(
        cols,
        rows,
        hidden,
        history,
        history_loading,
        Focus::Agent,
        0,
        0,
    )
    .lines
    .iter()
    .map(|l| strip_ansi(l).trim_end().to_string())
    .collect()
}

/// The frame as the renderer made it (colours, hit rects); `n_prompts` rows in Activity.
#[allow(clippy::too_many_arguments)]
fn raw_frame(
    cols: usize,
    rows: usize,
    hidden: Hidden,
    history: &[HistoryDay],
    history_loading: bool,
    focus: Focus,
    activity_scroll: usize,
    n_prompts: usize,
) -> synthesis_tui_render::dash::Frame {
    let cwd = "/work/proj".to_string();
    let mut session = session_from_events("s1", []);
    session.cwd = cwd.clone();
    session.project = basename(&cwd);
    session.model = "fake/model".into();
    for i in 0..n_prompts {
        session.activity.push(FeedItem {
            ts: Some(NOW),
            kind: "prompt",
            text: format!("prompt {i}"),
            anchor: Some(i),
        });
    }
    let agent = Agent {
        id: "s1".into(),
        kind: "main".into(),
        cwd: cwd.clone(),
        model: "fake/model".into(),
        mode: "default".into(),
        version: "0.1".into(),
        status: "idle".into(),
        ..Agent::default()
    };
    let srows = [SessionRow {
        id: "s1".into(),
        project: basename(&cwd),
        cwd: cwd.clone(),
        mtime: NOW,
        live: Some(Live {
            name: "synthesis".into(),
            cwd: cwd.clone(),
            status: "idle".into(),
        }),
        phase: "idle".into(),
        tokens: 0.0,
        ctx_pct: None,
        waiting: false,
        mode: Some("default".into()),
        title: String::new(),
    }];
    let key = workspace_key(&cwd);
    let ws = [Workspace {
        key: key.clone(),
        name: basename(&cwd),
        cwd: cwd.clone(),
        count: 1,
        waiting: false,
        busy: false,
    }];
    let view = DashView {
        hidden,
        activity_scroll,
        ..DashView::default()
    };
    let usage = Usage::default();
    let editor: [char; 0] = [];
    let state = DashState {
        session: &session,
        sessions: &srows,
        index: 0,
        usage: &usage,
        follow: false,
        history,
        history_loading,
        usage_metric: "tokens",
        mcp_servers: &[],
        mcp_live: None,
        workspaces: Some(&ws),
        active_workspace: &key,
        agent: Some(&agent),
        chat_agent: None,
        editor: Some(EditorView {
            chars: &editor,
            cur: 0,
        }),
        focus,
        view: &view,
        agent_session_id: Some("s1"),
        tz_offset_min: 0,
    };
    render_frame(&state, cols, rows, 0, NOW)
}

fn case(name: &str, cols: usize, rows: usize, hidden: Hidden) -> Vec<String> {
    let lines = frame(cols, rows, hidden);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/panels")
        .join(name);
    let text = format!("{}\n", lines.join("\n"));
    if std::env::var_os("SYNT_UPDATE_GOLDEN").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("expected.txt"), &text).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("expected.txt")).unwrap(),
        text,
        "{name}"
    );
    lines
}

fn has(lines: &[String], t: &str) -> bool {
    lines.iter().any(|l| l.contains(t))
}

#[test]
fn every_panel_on_is_the_classic_frame() {
    let lines = case("all-on-160x47", 160, 47, Hidden::default());
    for t in [
        "Workspace",
        "Running",
        "Session",
        "Tools & Skil",
        "Activity",
        "Status",
    ] {
        assert!(has(&lines, t), "{t}");
    }
}

#[test]
fn a_hidden_panel_disappears_and_the_others_take_its_rows() {
    let on = frame(160, 47, Hidden::default());
    let no_act = case(
        "no-activity-160x47",
        160,
        47,
        Hidden {
            activity: true,
            ..Hidden::default()
        },
    );
    assert!(!has(&no_act, "Activity") && has(&no_act, "Tools & Skil"));
    let no_tools = case(
        "no-tools-160x47",
        160,
        47,
        Hidden {
            tools: true,
            ..Hidden::default()
        },
    );
    assert!(!has(&no_tools, "Tools & Skil") && has(&no_tools, "Activity"));
    let no_session = case(
        "no-session-160x47",
        160,
        47,
        Hidden {
            session: true,
            ..Hidden::default()
        },
    );
    assert!(!has(&no_session, "Session") && has(&no_session, "Tools & Skil"));
    let none = case(
        "no-optional-160x47",
        160,
        47,
        Hidden {
            session: true,
            mcp: true,
            tools: true,
            activity: true,
        },
    );
    for t in ["Session", "Tools & Skil", "Activity"] {
        assert!(!has(&none, t), "{t}");
    }
    // Workspace, Running and the chat are locked: always there
    for l in [&on, &no_act, &no_tools, &no_session, &none] {
        assert!(has(l, "Workspace") && has(l, "Running") && has(l, "SYNTHESIS"));
        assert_eq!(l.len(), 47, "the frame keeps its height");
    }
    // no hole: with the optional panels off the left column ends in blank rows only after the
    // locked panels, never between panels (every non-blank sidebar row is above every blank one)
    let side = |lines: &[String]| -> Vec<bool> {
        lines[2..46]
            .iter()
            .map(|l| l.chars().take(18).all(char::is_whitespace))
            .collect()
    };
    let blank = side(&none);
    let first_blank = blank.iter().position(|b| *b).unwrap_or(blank.len());
    assert!(
        blank[first_blank..].iter().all(|b| *b),
        "a hole in the sidebar: {blank:?}"
    );
}

#[test]
fn usage_panel_says_loading_then_no_usage_then_shows_the_data() {
    let (loading, empty) = (
        frame_with(160, 47, Hidden::default(), &[], true),
        frame_with(160, 47, Hidden::default(), &[], false),
    );
    assert!(has(&loading, "loading usage…") && !has(&loading, "no usage yet"));
    assert!(has(&empty, "no usage yet") && !has(&empty, "loading usage…"));
    // three days ending today (NOW is 2027-01-15 08:00Z)
    let day = |ago: i64, tokens: f64| HistoryDay {
        date_ms: NOW - ago * 86_400_000,
        tokens,
        ..HistoryDay::default()
    };
    let data = [day(2, 1200.0), day(1, 0.0), day(0, 34_500.0)];
    let loaded = frame_with(160, 47, Hidden::default(), &data, false);
    assert!(!has(&loaded, "no usage yet") && !has(&loaded, "loading usage…"));
    assert!(has(&loaded, "35.7k"), "{}", loaded.join("\n"));
}

#[test]
fn the_focused_panel_has_the_accent_border_and_the_hit_rects_cover_the_panels() {
    use synthesis_tui_render::theme::{T_CLAUDE, paint};
    let accent = paint(T_CLAUDE, "┏━");
    // (line of the Workspace top border, line of the Running top border)
    let lit = |focus| {
        let f = raw_frame(160, 47, Hidden::default(), &[], false, focus, 0, 0);
        let at = |t: &str| {
            f.lines
                .iter()
                .position(|l| strip_ansi(l).contains(t))
                .unwrap()
        };
        (
            f.lines[at("Workspace")].contains(&accent),
            f.lines[at("Running")].contains(&accent),
        )
    };
    assert_eq!(lit(Focus::Agent), (false, false));
    assert_eq!(lit(Focus::Workspace), (true, false));
    assert_eq!(lit(Focus::Dashboard), (false, true));
    let r = raw_frame(160, 47, Hidden::default(), &[], false, Focus::Agent, 0, 0).rects;
    let (w, q) = (r.workspace_rect.unwrap(), r.session_rect.unwrap());
    assert_eq!(q.y, w.y + w.h, "Running starts under Workspace");
    assert_eq!(r.workspace_rows, [(w.y + 1, workspace_key("/work/proj"))]);
    assert_eq!(r.session_rows[0], (q.y + 1, "s1".to_string()));
}

#[test]
fn activity_scrolls_clamped_and_says_how_much_is_cut() {
    let at = |scroll| {
        raw_frame(
            160,
            47,
            Hidden::default(),
            &[],
            false,
            Focus::Agent,
            scroll,
            60,
        )
    };
    let f = at(0);
    let title = |f: &synthesis_tui_render::dash::Frame| {
        f.lines
            .iter()
            .map(|l| strip_ansi(l))
            .find(|l| l.contains("Activity"))
            .unwrap()
    };
    let max = f.rects.activity_max_scroll;
    assert!(max > 0 && title(&f).contains('↑') && !title(&f).contains('↓'));
    // newest at the bottom: the last row is the newest prompt
    assert_eq!(f.rects.activity_hits.last().unwrap().1, 59);
    // past the end is clamped to the oldest row
    let old = at(10_000);
    assert_eq!(old.rects.activity_scroll, max);
    assert_eq!(old.rects.activity_hits[0].1, 0);
    assert!(title(&old).contains('↓') && !title(&old).contains('↑'));
}
