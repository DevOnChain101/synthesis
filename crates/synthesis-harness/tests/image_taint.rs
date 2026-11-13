//! An image the conversation carries (the person's attachment or a tool result) taints the actions after it:
//! in auto mode an action with an external effect is asked, never judged. The taint is not washed out by a
//! person's answer while an image block is still in the conversation, and a later task keeps it.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use synthesis_core::PermissionMode;
use synthesis_events::Event;
use synthesis_harness::testing::Turn;
use synthesis_harness::{BoxFut, ImageAttachment, InputProvider, TaskRequest, ToolBox, UserReply};
use synthesis_providers::{Block, Content, ImageSource, Message, Role};
use synthesis_tools::{NeedsUser, Registry, SideEffectClass};
use tokio_util::sync::CancellationToken;

struct Answering;
impl InputProvider for Answering {
    fn ask<'a>(
        &'a self,
        _: &'a NeedsUser,
        _: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async { Some(UserReply::Answers(json!({"Which?": "A"}))) })
    }
}

fn question() -> serde_json::Value {
    json!({"questions": [{"question": "Which?", "header": "h", "multiSelect": false, "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]}]})
}

const ALLOW: &str = r#"{"decision":"allow","rule":"","reason":"in scope"}"#;

fn rig(w: &Arc<Probe>, turns: Vec<Turn>) -> Rig {
    Rig::build(
        Opts {
            mode: PermissionMode::Auto,
            tools: ToolBox::new(Registry::builtin()).with_tool(w.clone()),
            ..Opts::default()
        },
        turns,
        |h, _| h.with_input(Arc::new(Answering)),
    )
}

fn verdicts(r: &Rig) -> Vec<(String, String)> {
    r.find(|e| match e {
        Event::AutoVerdict {
            verdict, source, ..
        } => Some((verdict.clone(), source.clone())),
        _ => None,
    })
}

fn image() -> Message {
    Message {
        role: Role::User,
        content: Content::Blocks(vec![
            Block::Text {
                text: "what is this".into(),
            },
            Block::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "iVBORw0KGgo=".into(),
                },
            },
        ]),
    }
}

fn answer_then_mutate() -> Vec<Turn> {
    vec![
        Turn::tool("q", "AskUserQuestion", question()),
        Turn::tool("m1", "Mutate", json!({})),
        Turn::text(ALLOW),
        Turn::text("done"),
    ]
}

#[tokio::test]
async fn a_persons_answer_does_not_wash_out_an_image_the_conversation_still_carries() {
    let w = Probe::new("Mutate", SideEffectClass::Write, 0, "changed");
    let mut r = rig(&w, answer_then_mutate());
    let mut req = TaskRequest::new("now fix the file");
    req.history = vec![
        image(),
        Message {
            role: Role::Assistant,
            content: Content::Text("a logo".into()),
        },
    ];
    r.run_with(req, CancellationToken::new()).await;
    assert_eq!(w.runs(), 0, "still asked, not judged");
    assert_eq!(verdicts(&r), vec![("ask".into(), "fail_closed".into())]);
}

#[tokio::test]
async fn no_image_anywhere_means_the_judge_decides_as_usual() {
    let w = Probe::new("Mutate", SideEffectClass::Write, 0, "changed");
    let mut r = rig(&w, answer_then_mutate());
    r.run("fix the file").await;
    assert_eq!(verdicts(&r), vec![("allow".into(), "model".into())]);
}

#[tokio::test]
async fn an_image_of_an_earlier_task_still_taints_a_later_one() {
    let w = Probe::new("Mutate", SideEffectClass::Write, 0, "changed");
    // task 1 carries an image; task 2 has none of its own (the harness also keeps a sticky bit for the case where a
    // compaction drops the image block: it is cleared only by a person's answer, see `Harness::image_taint`)
    let turns = vec![
        Turn::text("a logo"),
        Turn::tool("m1", "Mutate", json!({})),
        Turn::text("could not"),
    ];
    let mut r = rig(&w, turns);
    r.harness.start_session(&mut r.session, "startup").unwrap();
    let mut first = TaskRequest::new("what is this");
    first.images = vec![ImageAttachment {
        media_type: "image/png".into(),
        data_base64: "iVBORw0KGgo=".into(),
    }];
    r.harness
        .run_task(&mut r.session, first, CancellationToken::new())
        .await
        .unwrap();
    r.harness.to_idle(&mut r.session).unwrap();
    r.harness
        .run_task(
            &mut r.session,
            TaskRequest::new("fix the file"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        w.runs(),
        0,
        "image-derived text is still untrusted after the compaction"
    );
    assert_eq!(verdicts(&r), vec![("ask".into(), "fail_closed".into())]);
}
