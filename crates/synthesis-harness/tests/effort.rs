//! The effort tier of `/effort` reaches the request only when the model declares it.
mod common;

use common::*;
use synthesis_harness::testing::Turn;

async fn sent_with(effort: Option<&str>) -> Option<String> {
    let o = Opts {
        effort_tiers: vec!["low".into(), "high".into()],
        ..Opts::default()
    };
    let mut rig = Rig::new(o, vec![Turn::text("a")]);
    rig.harness.set_reasoning_effort(effort.map(String::from));
    rig.run("one").await;
    rig.requests()[0].reasoning_effort.clone()
}

#[tokio::test]
async fn an_undeclared_effort_tier_is_never_sent() {
    assert_eq!(sent_with(Some("high")).await.as_deref(), Some("high"));
    // a tier the model does not declare (the model was switched): dropped, not sent
    assert_eq!(sent_with(Some("max")).await, None);
    assert_eq!(sent_with(None).await, None);
}
