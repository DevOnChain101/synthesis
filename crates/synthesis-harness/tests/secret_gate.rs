//! Secrets never reach the provider: the last step before every send masks the request, and a
//! registered value also withholds images.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod common;

use common::*;
use synthesis_events::Event;
use synthesis_harness::testing::Turn;
use synthesis_harness::{ImageAttachment, TaskRequest};
use tokio_util::sync::CancellationToken;

// built at runtime: no secret-looking literal in the source
fn fake_token() -> String {
    format!("{}_{}", "ghp", "Q".repeat(36))
}

#[tokio::test]
async fn a_registered_secret_and_a_token_shaped_value_never_reach_the_provider() {
    let registered = format!("{}-{}", "hunter", "98765432");
    let o = Opts {
        secrets: vec![registered.clone()],
        ..Opts::default()
    };
    let mut rig = Rig::new(o, vec![Turn::text("ok")]);
    let mut r = TaskRequest::new(format!("use {registered} and {}", fake_token()));
    r.images = vec![ImageAttachment {
        media_type: "image/png".into(),
        data_base64: "iVBORw0KGgo=".into(),
    }];
    rig.run_with(r, CancellationToken::new()).await;
    let sent = format!("{:?}", rig.provider.requests());
    assert!(!sent.contains(&registered), "{sent}");
    assert!(!sent.contains(&fake_token()), "{sent}");
    assert!(!sent.contains("iVBOR"), "image withheld: {sent}");
    assert!(sent.contains("[REDACTED:secret]") && sent.contains("[REDACTED:github]"));
    let note = rig.find(|e| match e {
        Event::Notice { text, .. } if text.starts_with("secret gate") => Some(text.clone()),
        _ => None,
    });
    assert_eq!(note.len(), 1, "{note:?}");
    assert!(
        !note[0].contains(&registered) && note[0].contains("github:1"),
        "{note:?}"
    );
}
