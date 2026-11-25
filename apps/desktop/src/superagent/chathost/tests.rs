use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use serde_json::json;
use synthesis_view::convo::{ApprovalState, Item, ToolState};

use super::ChatHost;
use super::model::{self, Block, Turn};
use crate::live::{Dialog, DialogKind, Live};
use crate::superagent::session::host::tests::fx;
use crate::tui_view::{Embed, TuiView};

fn asst(text: &str) -> Item {
    Item::Assistant {
        text: text.into(),
        streaming: false,
        tainted: false,
    }
}

fn tool(id: &str, output: &str) -> Item {
    Item::Tool {
        id: id.into(),
        name: "Bash".into(),
        summary: "cargo test".into(),
        state: ToolState::Ok,
        preview: String::new(),
        output: output.into(),
        duration_ms: 12,
        file: None,
        diff: None,
        input: Box::new(json!({"command": "cargo test"})),
        status: None,
        exit_code: Some(0),
    }
}

// ------------------------------------------------------------------------------------------ the model

#[test]
fn items_group_into_user_turns_and_assistant_turns() {
    let items = vec![
        Item::User("hi".into()),
        asst("a"),
        tool("t1", "out"),
        asst("b"),
        Item::User("again".into()),
        asst("c"),
    ];
    let t = model::turns(&items, 0, false, false);
    assert_eq!(t.len(), 4);
    assert!(matches!(&t[0], Turn::User { at: 0, text } if text == "hi"));
    match &t[1] {
        Turn::Assistant { blocks } => {
            assert_eq!(blocks.len(), 3);
            assert!(matches!(blocks[1], Block::Tool { at: 2 }));
        }
        _ => panic!("assistant turn"),
    }
    assert!(matches!(&t[3], Turn::Assistant { blocks } if blocks.len() == 1));
    // a window over the end keeps the original indexes (element ids and cards follow them)
    let tail = model::turns(&items, 4, false, false);
    assert!(matches!(&tail[0], Turn::User { at: 4, .. }));
}

#[test]
fn thinking_internal_rows_and_finished_tasks_follow_the_settings() {
    let items = vec![
        Item::Thinking("pondering".into()),
        Item::Internal("memory read".into()),
        Item::TaskEnd {
            ok: true,
            text: "done".into(),
            word: "completed".into(),
            checks: String::new(),
        },
        Item::TaskEnd {
            ok: false,
            text: "budget".into(),
            word: "failed".into(),
            checks: String::new(),
        },
    ];
    let quiet = model::turns(&items, 0, false, false);
    let Turn::Assistant { blocks } = &quiet[0] else {
        panic!()
    };
    assert_eq!(blocks.len(), 1, "only the failed task is a line");
    assert!(matches!(&blocks[0], Block::End { ok: false, text } if text == "failed: budget"));
    let loud = model::turns(&items, 0, true, true);
    let Turn::Assistant { blocks } = &loud[0] else {
        panic!()
    };
    assert_eq!(blocks.len(), 3);
}

#[test]
fn what_the_model_wrote_is_cleaned_before_it_is_a_turn() {
    let t = model::turns(
        &[Item::User("a\u{1b}[31mred\u{202e}".into())],
        0,
        false,
        false,
    );
    let Turn::User { text, .. } = &t[0] else {
        panic!()
    };
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
}

#[test]
fn a_past_ask_has_a_word_and_the_status_line_never_hides_an_open_one() {
    assert_eq!(model::ask_word(ApprovalState::Allowed), "Allowed");
    assert_eq!(model::ask_word(ApprovalState::Denied), "Denied");
    assert_eq!(model::status_line(false, false, 0, 0, ""), None);
    assert_eq!(
        model::status_line(true, true, 1, 0, "").as_deref(),
        Some("Starting the session")
    );
    // a dialog wins over "working": the person is the one being waited for
    assert_eq!(
        model::status_line(false, true, 2, 1, "running").as_deref(),
        Some("Waiting for your answer (2 open requests) \u{b7} 1 queued")
    );
    assert_eq!(
        model::status_line(false, true, 0, 0, "verifying").as_deref(),
        Some("Verifying the result")
    );
    assert_eq!(
        model::status_line(false, true, 0, 0, "").as_deref(),
        Some("Working")
    );
}

// ------------------------------------------------------------------------------------------ the view

struct Root(Entity<ChatHost>);

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

struct Chat<'a> {
    _fx: super::super::session::host::tests::Fx,
    tui: Entity<TuiView>,
    v: &'a mut VisualTestContext,
}

/// A fixture session (no engine): `items` and `dialogs` are what the shared `Live` holds.
fn chat(cx: &mut TestAppContext, items: Vec<Item>, dialogs: Vec<Dialog>, busy: bool) -> Chat<'_> {
    let (f, cfg) = fx(1);
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    let mut tui = None;
    let (_, v) = cx.add_window_view(|window, cx| {
        let t = cx.new(|cx| TuiView::live(window, cx, cfg, Default::default(), 0.));
        t.update(cx, |t, _| {
            t.embed = Some(Embed {
                headless: true,
                folder: Some(std::path::PathBuf::from("/work/proj")),
                ..Embed::default()
            });
            let mut l = Live::new();
            l.sid = "sid-1".into();
            l.starting = false;
            l.busy = busy;
            l.view.status.model = "fake/model".into();
            l.view.status.mode = "default".into();
            l.view.convo.items = items;
            l.dialogs = dialogs;
            t.set_live(l);
        });
        let host = cx.new(|cx| ChatHost::new(t.clone(), window, cx));
        tui = Some(t);
        Root(host)
    });
    v.simulate_resize(size(px(1000.), px(700.)));
    v.run_until_parked();
    Chat {
        _fx: f,
        tui: tui.unwrap(),
        v,
    }
}

fn plan_dialog() -> Dialog {
    Dialog {
        kind: DialogKind::Plan {
            id: "p1".into(),
            plan: "1. do it".into(),
        },
        options: vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
        sel: 0,
        armed_until: 0,
        engaged: false,
    }
}

#[gpui::test]
fn a_session_frame_draws_turns_and_a_composer_and_never_the_tui_view(cx: &mut TestAppContext) {
    let c = chat(
        cx,
        vec![
            Item::User("hello".into()),
            asst("hi there"),
            tool("t1", "ok"),
        ],
        vec![],
        false,
    );
    assert!(
        c.v.debug_bounds("view").is_none(),
        "the TUI view is not drawn"
    );
    let (host, user, text, card, comp) = (
        c.v.debug_bounds("sa-chat-host").unwrap(),
        c.v.debug_bounds("sa-chat-user").unwrap(),
        c.v.debug_bounds("sa-chat-text").unwrap(),
        c.v.debug_bounds("sa-tool-head-2").unwrap(),
        c.v.debug_bounds("sa-chat-composer").unwrap(),
    );
    // the person's line is on the right of a centred reading column, the answer below it, the composer last
    assert!(user.center().x > host.center().x, "{user:?} {host:?}");
    assert!(user.bottom() <= text.top() && text.bottom() <= card.top());
    assert!(card.bottom() <= comp.top() && comp.bottom() <= host.bottom());
    assert!(
        comp.size.width <= px(760.) && comp.size.width > px(300.),
        "{comp:?}"
    );
    assert!(
        c.v.debug_bounds("sa-chat-model").is_some() && c.v.debug_bounds("sa-chat-mode").is_some()
    );
    assert!(
        c.v.debug_bounds("sa-chat-send").is_some() && c.v.debug_bounds("sa-chat-stop").is_none()
    );
    assert!(
        c.v.debug_bounds("sa-chat-dialog").is_none()
            && c.v.debug_bounds("sa-chat-status").is_none()
    );
}

#[gpui::test]
fn a_running_turn_shows_stop_and_a_status_line(cx: &mut TestAppContext) {
    let c = chat(cx, vec![Item::User("go".into())], vec![], true);
    assert!(c.v.debug_bounds("sa-chat-stop").is_some());
    assert!(c.v.debug_bounds("sa-chat-send").is_none());
    assert!(c.v.debug_bounds("sa-chat-status").is_some());
}

#[gpui::test]
fn a_tool_card_opens_and_closes_on_a_click_on_its_header(cx: &mut TestAppContext) {
    let c = chat(
        cx,
        vec![asst("x"), tool("t1", "line one\nline two")],
        vec![],
        false,
    );
    assert!(
        c.v.debug_bounds("sa-tool-body-1").is_none(),
        "closed by default"
    );
    let head = c.v.debug_bounds("sa-tool-head-1").unwrap();
    c.v.simulate_click(head.center(), gpui::Modifiers::none());
    c.v.run_until_parked();
    assert!(c.v.debug_bounds("sa-tool-body-1").is_some());
    let head = c.v.debug_bounds("sa-tool-head-1").unwrap();
    c.v.simulate_click(head.center(), gpui::Modifiers::none());
    c.v.run_until_parked();
    assert!(c.v.debug_bounds("sa-tool-body-1").is_none());
}

#[gpui::test]
fn a_waiting_ask_is_a_card_above_the_composer_and_a_click_answers_it(cx: &mut TestAppContext) {
    let c = chat(cx, vec![Item::User("go".into())], vec![plan_dialog()], true);
    let (dlg, comp) = (
        c.v.debug_bounds("sa-chat-dialog")
            .expect("the ask is drawn"),
        c.v.debug_bounds("sa-chat-composer").unwrap(),
    );
    assert!(dlg.bottom() <= comp.top(), "{dlg:?} {comp:?}");
    assert!(
        c.v.debug_bounds("sa-chat-status").is_some(),
        "the status line says so too"
    );
    // the first option, through the shared dialog's own handler (arming, engagement and the answer are `Live`'s)
    let list = c.v.debug_bounds("dialog-list").unwrap();
    c.v.simulate_click(
        list.origin + point(px(20.), px(10.)),
        gpui::Modifiers::none(),
    );
    c.v.run_until_parked();
    let left = c
        .tui
        .read_with(c.v, |t, _| t.embed_live().unwrap().dialogs.len());
    assert_eq!(left, 0, "the ask was answered, not lost");
    assert!(c.v.debug_bounds("sa-chat-dialog").is_none());
}

#[gpui::test]
fn a_line_typed_while_a_turn_runs_is_queued_like_in_the_tui_prompt(cx: &mut TestAppContext) {
    let c = chat(cx, vec![Item::User("go".into())], vec![], true);
    let prompt = c.tui.read_with(c.v, |t, _| t.embed_prompt()).unwrap();
    // Shift+Enter is a new line of the draft, never a send
    prompt.update_in(c.v, |s, w, cx| s.set_value("one\ntwo".to_string(), w, cx));
    assert_eq!(
        prompt.read_with(c.v, |s, _| s.value().to_string()),
        "one\ntwo"
    );
    c.tui.update_in(c.v, |t, w, cx| t.embed_submit(w, cx));
    c.v.run_until_parked();
    let queued = c
        .tui
        .read_with(c.v, |t, _| t.embed_live().unwrap().queue.len());
    assert_eq!(queued, 1, "Send queues behind the running turn");
    assert_eq!(
        prompt.read_with(c.v, |s, _| s.value().to_string()),
        "",
        "the box is empty again"
    );
    let status = c.v.debug_bounds("sa-chat-status");
    assert!(status.is_some());
}

/// The Shell handoff (Super Agent to TUI) reads the hosted view, not the chat that draws it: the session id, its
/// folder and the scope of its engine are still there.
#[gpui::test]
fn the_handoff_entry_comes_from_the_driving_view_behind_the_chat(cx: &mut TestAppContext) {
    let c = chat(cx, vec![], vec![], false);
    let h = c
        .tui
        .read_with(c.v, |t, _| t.embed_handoff())
        .expect("a session with an id is handed over");
    assert_eq!(h.sid, "sid-1");
    assert_eq!(h.folder, std::path::PathBuf::from("/work/proj"));
    assert_eq!(h.scope, crate::handoff::Scope::Local);
}

// ------------------------------------------------------------------------------------------ images

fn png(w: u32, h: u32, pad: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.resize(v.len() + pad, 0);
    v
}

fn clip(c: &mut Chat<'_>, f: gpui::ImageFormat, bytes: Vec<u8>) {
    c.v.update(|_, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
            f, bytes,
        )))
    });
}

/// A key in the frame; true when the embedded view took it.
fn press(c: &mut Chat<'_>, tui: &Entity<TuiView>, keys: &'static str) -> bool {
    let ks = gpui::Keystroke::parse(keys).unwrap();
    tui.update_in(c.v, |t, w, cx| t.on_key(&ks, w, cx))
}

fn held(c: &Chat<'_>, tui: &Entity<TuiView>) -> usize {
    tui.read_with(c.v, |t, _| t.embed_live().unwrap().images.len())
}

fn last_note(c: &Chat<'_>) -> String {
    c.tui.read_with(c.v, |t, _| {
        match t.embed_live().unwrap().view.convo.items.last() {
            Some(Item::Notice { text, .. }) => text.clone(),
            _ => String::new(),
        }
    })
}

#[gpui::test]
fn a_pasted_image_is_a_chip_in_the_frame_and_the_tui_cap_holds(cx: &mut TestAppContext) {
    let mut c = chat(cx, vec![], vec![], false);
    let tui = c.tui.clone();
    clip(&mut c, gpui::ImageFormat::Png, png(4, 4, 8));
    for _ in 0..5 {
        assert!(press(&mut c, &tui, "cmd-v"));
    }
    assert_eq!(held(&c, &tui), 5);
    c.v.run_until_parked();
    assert!(c.v.debug_bounds("sa-chat-images").is_some());
    assert!(
        press(&mut c, &tui, "cmd-v"),
        "an image is consumed even when refused"
    );
    assert_eq!(held(&c, &tui), 5);
    assert!(last_note(&c).contains("at most 5"), "{}", last_note(&c));
    tui.update(c.v, |t, cx| t.embed_unattach(0, cx));
    assert_eq!(held(&c, &tui), 4);
}

#[gpui::test]
fn a_non_image_and_an_oversize_image_are_refused_in_a_frame(cx: &mut TestAppContext) {
    let mut c = chat(cx, vec![], vec![], false);
    let tui = c.tui.clone();
    // text on the clipboard: Cmd+V is left to the box, Alt+V says there is no image
    c.v.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("hi".into())));
    assert!(!press(&mut c, &tui, "cmd-v"));
    assert!(press(&mut c, &tui, "alt-v"));
    assert!(last_note(&c).contains("no image"));
    clip(
        &mut c,
        gpui::ImageFormat::Png,
        b"not a png at all, just text bytes".to_vec(),
    );
    assert!(press(&mut c, &tui, "cmd-v"));
    assert!(last_note(&c).contains("not a PNG"), "{}", last_note(&c));
    clip(&mut c, gpui::ImageFormat::Gif, vec![1; 32]);
    assert!(press(&mut c, &tui, "cmd-v"));
    assert!(last_note(&c).contains("not supported"));
    clip(&mut c, gpui::ImageFormat::Png, png(4, 4, 5 << 20));
    assert!(press(&mut c, &tui, "cmd-v"));
    assert!(last_note(&c).contains("4 MB"), "{}", last_note(&c));
    assert_eq!(held(&c, &tui), 0);
}

#[gpui::test]
fn a_paste_attaches_to_the_frame_that_took_the_key_only(cx: &mut TestAppContext) {
    let mut a = chat(cx, vec![], vec![], false);
    let (_f2, cfg) = fx(1);
    let other = a.v.update(|w, cx| {
        cx.new(|cx| {
            let mut t = TuiView::live(w, cx, cfg, Default::default(), 0.);
            t.embed = Some(Embed {
                headless: true,
                ..Embed::default()
            });
            let mut l = Live::new();
            l.starting = false;
            t.set_live(l);
            t.shown = false; // a frame behind the one on screen
            t
        })
    });
    let tui = a.tui.clone();
    clip(&mut a, gpui::ImageFormat::Png, png(4, 4, 8));
    assert!(press(&mut a, &tui, "cmd-v"));
    assert_eq!((held(&a, &tui), held(&a, &other)), (1, 0));
    assert!(
        !press(&mut a, &other, "cmd-v"),
        "a frame that is not on screen takes no keys"
    );
    assert_eq!(held(&a, &other), 0);
}
