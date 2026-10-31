//! `SendMessage(ws:<label>/sess:<name>)` rules (messaging.md B7) match the address as typed.

use serde_json::json;
use synthesis_permissions::{Decision, Env, Mode, PermissionRules, Permissions};

fn perms(allow: &[&str], deny: &[&str]) -> Permissions {
    let rules = PermissionRules {
        allow: allow.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let root = std::env::temp_dir();
    let mut p = Permissions::new(rules, Env::new(root.clone(), root, Mode::Default));
    p.env.interactive = true;
    p
}

fn decide(p: &Permissions, to: &str) -> Decision {
    p.check(
        "SendMessage",
        &json!({"to": to, "message": "x"}),
        Mode::Default,
    )
    .decision
}

#[test]
fn an_allow_rule_names_its_peer_and_a_deny_rule_beats_it() {
    let p = perms(
        &["SendMessage(ws:api/sess:backend)", "SendMessage(ws:web/*)"],
        &[],
    );
    assert_eq!(decide(&p, "ws:api/sess:backend"), Decision::Allow);
    assert_eq!(decide(&p, "ws:web/sess:anything"), Decision::Allow);
    assert_eq!(decide(&p, "ws:api/sess:other"), Decision::Ask);
    assert_eq!(decide(&p, "reviewer"), Decision::Ask);
    let p = perms(&["SendMessage"], &["SendMessage(ws:secret/*)"]);
    assert_eq!(decide(&p, "ws:secret/sess:x"), Decision::Deny);
    assert_eq!(decide(&p, "ws:api/sess:x"), Decision::Allow);
}
