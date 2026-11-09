//! `meta.action` of a browser tool becomes a `browser.action` event: position and a short target only,
//! never typed text, a key or a secret.
mod common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};
use synthesis_core::PermissionMode;
use synthesis_events::Event;
use synthesis_harness::ToolBox;
use synthesis_harness::testing::Turn;
use synthesis_tools::{BoxFuture, SideEffectClass, Tool, ToolOutput, ToolRuntime, ToolSpec};

struct Fake(ToolSpec, Value);

impl Tool for Fake {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a synthesis_core::ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        let meta = self.1.clone();
        Box::pin(async move { ToolOutput::text("ok").with_meta(meta) })
    }
}

async fn actions(name: &str, meta: Value) -> Vec<String> {
    let spec = ToolSpec {
        name: name.into(),
        description: "d".into(),
        input_schema: json!({"type": "object"}),
        side_effect: SideEffectClass::Read,
        runtime: ToolRuntime::Builtin,
    };
    let mut rig = Rig::new(
        Opts {
            mode: PermissionMode::BypassPermissions,
            tools: ToolBox::empty().with_tool(Arc::new(Fake(spec, meta))),
            ..Opts::default()
        },
        vec![Turn::tool("a1", name, json!({})), Turn::text("done")],
    );
    rig.run("x").await;
    rig.find(|e| match e {
        Event::BrowserAction { .. } => Some(serde_json::to_string(e).unwrap()),
        _ => None,
    })
}

#[tokio::test]
async fn a_click_reports_where_and_on_what() {
    let a = actions(
        "browser_click",
        json!({"action": {"action": "click", "x": 640, "y": 360, "w": 1280, "h": 720, "target": "button \"Send\""}}),
    )
    .await;
    assert_eq!(a.len(), 1, "{a:?}");
    let v: Value = serde_json::from_str(&a[0]).unwrap();
    assert_eq!(v["type"], "browser.action");
    assert_eq!(v["payload"]["x"], 640);
    assert_eq!(v["payload"]["w"], json!(1280));
    assert_eq!(v["payload"]["h"], json!(720));
    assert_eq!(v["payload"]["target"], "button \"Send\"");
}

#[tokio::test]
async fn a_typed_secret_never_reaches_the_event() {
    // even a tool that put the value, the key or the field name in its meta cannot leak it: only the
    // whitelisted fields are copied, and a secret fill reports the word "secret"
    let a = actions(
        "browser_fill_secret",
        json!({"action": {"action": "type", "x": 10, "y": 20, "w": 800, "h": 600,
            "target": "textbox Password", "text": "hunter2-VALUE", "value": "hunter2-VALUE", "key": "hunter2-VALUE"}}),
    )
    .await;
    assert_eq!(a.len(), 1, "{a:?}");
    assert!(!a[0].contains("hunter2"), "{}", a[0]);
    let v: Value = serde_json::from_str(&a[0]).unwrap();
    assert_eq!(v["payload"]["target"], "secret");
    assert_eq!(v["payload"]["x"], 10);
}

#[tokio::test]
async fn a_long_target_is_clipped_and_other_tools_emit_nothing() {
    let long = "n".repeat(500);
    let a = actions(
        "browser_click",
        json!({"action": {"action": "click", "target": long}}),
    )
    .await;
    let v: Value = serde_json::from_str(&a[0]).unwrap();
    assert_eq!(v["payload"]["target"].as_str().unwrap().len(), 80);
    assert!(v["payload"].get("x").is_none());
    // a tool that is not a browser tool cannot make the event
    assert!(
        actions("Read", json!({"action": {"action": "click"}}))
            .await
            .is_empty()
    );
}
