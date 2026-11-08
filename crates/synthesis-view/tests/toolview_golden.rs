//! Golden cards: every tool kind, collapsed and open, shaped from real event JSONL
//! (`tests/fixtures/tools.jsonl`). `UPDATE_GOLDEN=1 cargo test -p synthesis-view --test toolview_golden`
//! rewrites `tests/golden/tools.txt`; review the diff.

use synthesis_events::Envelope;
use synthesis_view::convo::{Convo, Item};
use synthesis_view::toolview::{Glyph, dump, shape};

fn convo() -> Convo {
    let events: Vec<Envelope> = include_str!("fixtures/tools.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).expect("fixture line"))
        .collect();
    Convo::from_events(&events)
}

#[test]
fn every_kind_collapsed_and_open() {
    let c = convo();
    let mut out = String::new();
    for it in &c.items {
        let Item::Tool { id, .. } = it else { continue };
        for open in [false, true] {
            out += &format!("== {id} {}\n", if open { "open" } else { "collapsed" });
            out += &dump(&shape(it, open, 60).unwrap());
        }
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/tools.txt");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden")).unwrap();
        std::fs::write(path, &out).unwrap();
    }
    let want = std::fs::read_to_string(path).expect("golden file (UPDATE_GOLDEN=1 writes it)");
    assert_eq!(out, want);
}

#[test]
fn the_fixture_has_no_secret_or_home_path() {
    let f = include_str!("fixtures/tools.jsonl");
    for bad in ["/Users/", "/home/", "sk-", "Bearer ", "api_key"] {
        assert!(!f.contains(bad), "{bad}");
    }
}

#[test]
fn states_and_badges_come_from_the_events() {
    let c = convo();
    let card = |id: &str| {
        c.items
            .iter()
            .find(|i| matches!(i, Item::Tool { id: t, .. } if t == id))
            .and_then(|i| shape(i, false, 60))
            .unwrap()
    };
    let badges = |id: &str| {
        card(id)
            .badges
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(card("bash_ok").glyph, Glyph::Ok);
    assert_eq!(badges("bash_ok"), ["exit 0", "2.3s"]);
    assert_eq!(card("bash_fail").glyph, Glyph::Error);
    assert!(badges("bash_fail").contains(&"exit 1".to_string()));
    assert!(badges("bash_bg").contains(&"background".to_string()));
    assert_eq!(card("bash_run").glyph, Glyph::Running);
    assert_eq!(card("den1").glyph, Glyph::Denied);
    assert_eq!(card("int1").glyph, Glyph::Interrupted);
    assert_eq!(badges("edit1")[..2], ["+2", "\u{2212}1"]);
    assert_eq!(badges("read1")[0], "lines 10\u{2013}17");
    assert_eq!(card("agent1").link.as_deref(), Some("rev-1"));
    assert_eq!(card("mcp1").head, "github:get_issue");
}
