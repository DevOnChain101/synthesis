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
async fn a_model_without_vision_gets_image_placeholders_and_one_notice() {
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
    let Content::Blocks(b) = &m.content else {
        panic!("{:?}", m.content)
    };
    assert!(
        matches!(&b[1], Block::Text { text } if text == "[image]"),
        "{b:?}"
    );
    assert!(
        !format!("{b:?}").contains("iVBOR"),
        "no image bytes for a text model"
    );
    let notes = rig
        .events()
        .iter()
        .filter(|e| matches!(&e.event, Event::Notice { text, .. } if text.contains("does not take images")))
        .count();
    assert_eq!(notes, 1);
}

/// Images already in the history (pasted for one model) are checked again for each request: a
/// switch to a text-only model replaces them in the request copy, the history keeps them.
#[tokio::test]
async fn images_in_history_are_re_checked_for_every_request() {
    let o = Opts {
        no_vision: true,
        ..Opts::default()
    };
    let mut rig = Rig::new(o, vec![Turn::text("ok")]);
    let history = vec![synthesis_providers::Message {
        role: Role::User,
        content: Content::Blocks(vec![
            Block::Text {
                text: "earlier".into(),
            },
            Block::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "SECRETPIXELS".into(),
                },
            },
        ]),
    }];
    let mut r = TaskRequest::new("next");
    r.history = history;
    let out = rig.run_with(r, CancellationToken::new()).await;
    let sent = format!("{:?}", rig.provider.requests()[0].messages);
    assert!(
        sent.contains("[image]") && !sent.contains("SECRETPIXELS"),
        "{sent}"
    );
    assert!(
        format!("{:?}", out.messages).contains("SECRETPIXELS"),
        "the history keeps the image for a later model"
    );
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
