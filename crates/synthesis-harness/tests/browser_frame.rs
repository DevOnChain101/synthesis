//! `meta.frame` of `browser_screenshot` becomes a `browser.frame` event (metadata only).
mod common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};
use synthesis_core::PermissionMode;
use synthesis_events::Event;
use synthesis_harness::ToolBox;
use synthesis_harness::testing::Turn;
use synthesis_tools::{
    Block, BoxFuture, Content, ImageSource, SideEffectClass, Tool, ToolOutput, ToolRuntime,
    ToolSpec,
};

struct FakeShot(ToolSpec);

impl Tool for FakeShot {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a synthesis_core::ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async {
            let path = std::env::temp_dir().join("screenshot-1.png");
            ToolOutput::text("Screenshot saved as artifact x").with_meta(json!({
                "artifact_path": path.display().to_string(),
                "frame": {"url": "https://example.com", "w": 800, "h": 600, "bytes": 4242},
            }))
        })
    }
}

#[tokio::test]
async fn a_screenshot_emits_a_frame_event_without_image_bytes() {
    let spec = ToolSpec {
        name: "browser_screenshot".into(),
        description: "d".into(),
        input_schema: json!({"type": "object"}),
        side_effect: SideEffectClass::Read,
        runtime: ToolRuntime::Builtin,
    };
    let mut rig = Rig::new(
        Opts {
            mode: PermissionMode::BypassPermissions,
            tools: ToolBox::empty().with_tool(Arc::new(FakeShot(spec))),
            ..Opts::default()
        },
        vec![
            Turn::tool("s1", "browser_screenshot", json!({})),
            Turn::text("done"),
        ],
    );
    rig.run("x").await;
    let frames = rig.find(|e| match e {
        Event::BrowserFrame {
            path,
            url,
            w,
            h,
            bytes,
        } => Some((path.clone(), url.clone(), *w, *h, *bytes)),
        _ => None,
    });
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(frames[0].0.ends_with("screenshot-1.png"));
    assert_eq!(
        (frames[0].1.as_str(), frames[0].2, frames[0].3, frames[0].4),
        ("https://example.com", 800, 600, 4242)
    );
}

struct ImageTool(ToolSpec, &'static str);

impl Tool for ImageTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a synthesis_core::ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async {
            let mut o = ToolOutput::text("");
            o.content = Content::Blocks(vec![Block::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: self.1.into(),
                },
            }]);
            o
        })
    }
}

#[tokio::test]
async fn with_a_secret_registered_only_the_browser_screenshot_reaches_the_model_when_allowed() {
    let tool = |name: &str, data| {
        Arc::new(ImageTool(
            ToolSpec {
                name: name.into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
                side_effect: SideEffectClass::Read,
                runtime: ToolRuntime::Builtin,
            },
            data,
        ))
    };
    for (allow, sent) in [(true, true), (false, false)] {
        let mut rig = Rig::new(
            Opts {
                mode: PermissionMode::BypassPermissions,
                tools: ToolBox::empty()
                    .with_tool(tool("browser_screenshot", "U0hPVA=="))
                    .with_tool(tool("mcp__x__shot", "T1RIRVI=")),
                secrets: vec!["hunter-42424242".into()],
                config: if allow {
                    |c| c.screenshots_allowed = true
                } else {
                    |_| {}
                },
                ..Opts::default()
            },
            vec![
                Turn::tool("s1", "browser_screenshot", json!({})),
                Turn::tool("m1", "mcp__x__shot", json!({})),
                Turn::text("done"),
            ],
        );
        rig.run("x").await;
        let last = serde_json::to_string(&rig.requests().last().unwrap().messages).unwrap();
        assert_eq!(last.contains("U0hPVA=="), sent, "allow={allow}: {last}");
        assert!(!last.contains("T1RIRVI="), "allow={allow}: {last}");
    }
}
