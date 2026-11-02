//! Images the person attached to the request travel as image blocks next to the text, and are
//! dropped with a notice for a model that does not take images.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod common;

use common::*;
use synthesis_events::Event;
use synthesis_harness::testing::Turn;
use synthesis_harness::{ImageAttachment, TaskRequest};
use synthesis_providers::{Block, Content, ImageSource, Role};
use tokio_util::sync::CancellationToken;

fn req() -> TaskRequest {
    let mut r = TaskRequest::new("what is in this picture?");
    r.images = vec![ImageAttachment {
        media_type: "image/png".into(),
        data_base64: "iVBORw0KGgo=".into(),
    }];
    r
}

#[tokio::test]
async fn an_attached_image_reaches_the_model_as_a_block_after_the_text() {
    let mut rig = Rig::new(Opts::default(), vec![Turn::text("a cat")]);
    rig.run_with(req(), CancellationToken::new()).await;
    let sent = rig.provider.requests();
    let m = sent[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    let Content::Blocks(b) = &m.content else {
        panic!("{:?}", m.content)
    };
    assert!(matches!(&b[0], Block::Text { text } if text == "what is in this picture?"));
    assert!(matches!(
        &b[1],
        Block::Image { source: ImageSource::Base64 { media_type, data } }
            if media_type == "image/png" && data == "iVBORw0KGgo="
    ));
}

#[tokio::test]
async fn a_model_without_vision_gets_the_text_and_the_person_a_notice() {
    let o = Opts {
        no_vision: true,
        ..Opts::default()
    };
    let mut rig = Rig::new(o, vec![Turn::text("ok")]);
    rig.run_with(req(), CancellationToken::new()).await;
    let sent = rig.provider.requests();
    let m = sent[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert!(matches!(&m.content, Content::Text(t) if t == "what is in this picture?"));
    let events = rig.events();
    assert!(events.iter().any(|e| matches!(&e.event,
        Event::Notice { text, .. } if text.contains("does not take images"))));
}

#[tokio::test]
async fn no_image_keeps_the_plain_text_message() {
    let mut rig = Rig::new(Opts::default(), vec![Turn::text("hi")]);
    rig.run("hello").await;
    let sent = rig.provider.requests();
    assert!(
        sent[0]
            .messages
            .iter()
            .any(|m| matches!(&m.content, Content::Text(t) if t == "hello"))
    );
}
