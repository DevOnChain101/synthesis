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

fn decide_with(p: &Permissions, to: &str, resolved: &[&str]) -> Decision {
    p.check(
        "SendMessage",
        &json!({"to": to, "message": "x", "_resolved_targets": resolved}),
        Mode::Default,
    )
    .decision
}

#[test]
fn a_leading_space_does_not_slip_past_a_deny_or_ask_rule() {
    let p = perms(&["SendMessage"], &["SendMessage(ws:secret/*)"]);
    assert_eq!(decide(&p, " ws:secret/sess:x"), Decision::Deny);
    assert_eq!(decide(&p, "ws:secret/sess:x\n"), Decision::Deny);
    let rules = PermissionRules {
        allow: vec!["SendMessage".into()],
        ask: vec!["SendMessage(ws:api/*)".into()],
        ..Default::default()
    };
    let root = std::env::temp_dir();
    let mut p = Permissions::new(rules, Env::new(root.clone(), root, Mode::Default));
    p.env.interactive = true;
    assert_eq!(decide(&p, "\tws:api/sess:x"), Decision::Ask);
}

#[test]
fn every_spelling_of_the_resolved_target_meets_deny_and_ask_rules() {
    let p = perms(
        &["SendMessage"],
        &["SendMessage(ws:secret/*)", "SendMessage(sess:vault)"],
    );
    // `sess:` alone names the session; the harness adds the spellings it resolved to
    assert_eq!(
        decide(&p, "sess:3c9e01aa"),
        Decision::Allow,
        "no hint, no match"
    );
    assert_eq!(
        decide_with(&p, "sess:3c9e01aa", &["ws:secret/sess:3c9e01aa"]),
        Decision::Deny
    );
    assert_eq!(
        decide_with(&p, "ws:x/sess:y", &["sess:vault"]),
        Decision::Deny
    );
    // a hint can only add denials: it never makes an allow rule match
    let p = perms(&["SendMessage(ws:api/sess:backend)"], &[]);
    assert_eq!(
        decide_with(&p, "sess:b1", &["ws:api/sess:backend"]),
        Decision::Ask
    );
}
