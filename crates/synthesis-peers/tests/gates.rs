//! confirm_replies, coalesced partials, queued requests and pushed replies, over a real socket /
//! named pipe and a temp run dir (messaging.md B6 Confirm / Outgoing, R10c).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use synthesis_peers::mailbox::{Envelope, now_s};
use synthesis_peers::net::Listener;
use synthesis_peers::serve::{
    Answered, BoxFut, Outgoing, Reply, Request, Responder, Server, ask, deliver,
};
use synthesis_peers::{Addr, Config, Key, load_or_create_key, run_dir};
use tokio::sync::mpsc::UnboundedSender;

/// Answers with a secret, streams scripted partials, and records what it was handed.
struct Spy {
    confirm: Option<bool>,
    chunks: Vec<(u64, &'static str)>,
    got: Mutex<Vec<Envelope>>,
    keep: bool,
}

impl Spy {
    fn new() -> Self {
        Self {
            confirm: None,
            chunks: vec![],
            got: Mutex::default(),
            keep: true,
        }
    }
}

impl Responder for Spy {
    fn status(&self) -> Value {
        json!({"status":"idle","agents":[]})
    }
    fn has_agent(&self, i: &str) -> bool {
        i == "main"
    }
    fn answer(&self, _r: Request, partial: UnboundedSender<String>) -> BoxFut<Reply> {
        let chunks = self.chunks.clone();
        Box::pin(async move {
            for (wait, t) in chunks {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let _ = partial.send(t.into());
            }
            // let the server drain the partial channel before the answer ends
            tokio::time::sleep(Duration::from_millis(100)).await;
            Reply {
                status: "answered".into(),
                text: "the key is sk-ant-SECRET123".into(),
            }
        })
    }
    fn confirm(&self, _r: &Request, _y: &Reply) -> BoxFut<bool> {
        let c = self.confirm.unwrap_or(false);
        Box::pin(async move { c })
    }
    fn deliver_reply(&self, env: Envelope) -> bool {
        self.got.lock().unwrap().push(env);
        self.keep
    }
}

fn redact(s: &str) -> String {
    s.replace("sk-ant-SECRET123", "[REDACTED]")
}

fn addr(label: &str, root: &str, sess: &str) -> Addr {
    Addr {
        workspace_key: format!("{label:0<24}"),
        workspace_label: label.into(),
        workspace_root: root.into(),
        session_id: sess.into(),
        instance: "main".into(),
        kind: "main".into(),
        ..Default::default()
    }
}

struct Rig {
    _home: tempfile::TempDir,
    key: Key,
    endpoint: String,
    spy: Arc<Spy>,
    server: Arc<Server>,
}

async fn rig(cfg: Value, spy: Spy) -> Rig {
    let home = tempfile::tempdir().unwrap();
    let dir = run_dir(home.path());
    let key = load_or_create_key(&dir).unwrap();
    let l = Listener::bind(&dir).unwrap();
    let endpoint = l.endpoint();
    let spy = Arc::new(spy);
    let server = Server::new(
        key.clone(),
        Config::from_settings(cfg.as_object().unwrap_or(&Map::new()), "/work/api"),
        addr("api", "/work/api", "sess-a"),
        spy.clone(),
        Arc::new(redact),
        Arc::new(|_, _| {}),
    );
    tokio::spawn(server.clone().run(l));
    Rig {
        _home: home,
        key,
        endpoint,
        spy,
        server,
    }
}

fn on() -> Value {
    json!({"enabled": true})
}

fn request(id: &str, from: &Addr) -> Request {
    Request {
        id: id.into(),
        from: from.clone(),
        to_agent: "main".into(),
        text: "q".into(),
        chain: vec![],
    }
}

async fn live(r: &Rig, id: &str) -> String {
    let from = addr("api", "/work/api", "sess-b");
    let o = Outgoing {
        id,
        from: &from,
        to_agent: "main",
        text: "q",
        chain: &[],
        timeout_s: 30,
    };
    let f = ask(&r.endpoint, &r.key, &o, |_, _| {}).await.unwrap();
    format!("{}|{}", f.status, f.text)
}

const DECLINED: &str = "declined|the owner of this session did not approve sending a reply";

#[tokio::test]
async fn confirm_replies_gates_every_reply_and_fails_closed() {
    for (confirm, want) in [
        (Some(true), "answered|the key is [REDACTED]"),
        (Some(false), DECLINED),
        // a responder that cannot ask anybody (headless) declines
        (None, DECLINED),
    ] {
        let r = rig(
            json!({"enabled": true, "confirm_replies": true}),
            Spy {
                confirm,
                ..Spy::new()
            },
        )
        .await;
        assert_eq!(live(&r, "c1").await, want);
    }
    // off by default: the responder is never asked (this one would say no)
    let r = rig(on(), Spy::new()).await;
    assert!(live(&r, "c2").await.starts_with("answered|"));
}

#[tokio::test]
async fn partials_are_coalesced_without_losing_text_and_redacted() {
    let r = rig(
        on(),
        Spy {
            chunks: vec![(0, "one "), (0, "two sk-ant-"), (650, "SECRET123 three ")],
            ..Spy::new()
        },
    )
    .await;
    let from = addr("api", "/work/api", "sess-b");
    let mut seen = Vec::new();
    let o = Outgoing {
        id: "p1",
        from: &from,
        to_agent: "main",
        text: "q",
        chain: &[],
        timeout_s: 30,
    };
    ask(&r.endpoint, &r.key, &o, |s, t| {
        seen.push((s, t.to_string()))
    })
    .await
    .unwrap();
    // frame 1 leaves at once; the second chunk waits out the 500 ms window and joins the third
    assert_eq!(
        seen,
        vec![
            (1, "one ".to_string()),
            (2, "two [REDACTED] three ".to_string())
        ]
    );
}

#[tokio::test]
async fn queued_requests_follow_the_same_policy_and_limits() {
    let from = addr("api", "/work/api", "sess-b");
    let r = rig(on(), Spy::new()).await;
    let s = &r.server;
    match s.answer_queued(request("q1", &from)).await {
        Answered::Reply(x) => assert_eq!(x.text, "the key is [REDACTED]"),
        o => panic!("{o:?}"),
    }
    let web = addr("web", "/work/web", "sess-c");
    assert_eq!(
        s.answer_queued(request("q2", &web)).await,
        Answered::Rejected("denied")
    );
    let mut looped = request("q3", &from);
    looped.chain = vec!["sess-a".into()];
    assert_eq!(s.answer_queued(looped).await, Answered::Rejected("cycle"));
    let mut big = request("q4", &from);
    big.text = "x".repeat(33 * 1024);
    assert_eq!(s.answer_queued(big).await, Answered::Rejected("too_large"));
    let mut gone = request("q5", &from);
    gone.to_agent = "nobody".into();
    assert_eq!(s.answer_queued(gone).await, Answered::Rejected("no_agent"));
    // 10 per window: the rest stay queued (Retry), they are not refused
    for i in 0..9 {
        let _ = s.answer_queued(request(&format!("r{i}"), &from)).await;
    }
    assert_eq!(
        s.answer_queued(request("r10", &from)).await,
        Answered::Retry("rate_limited")
    );
    let off = rig(json!({"enabled": false}), Spy::new()).await;
    assert_eq!(
        off.server.answer_queued(request("o", &from)).await,
        Answered::Rejected("disabled")
    );
    // confirm_replies applies to a queued answer too
    let c = rig(
        json!({"enabled": true, "confirm_replies": true}),
        Spy::new(),
    )
    .await;
    match c.server.answer_queued(request("c", &from)).await {
        Answered::Reply(x) => assert_eq!(x.status, "declined"),
        o => panic!("{o:?}"),
    }
}

fn reply_env(key: &Key, id: &str, to: &str, from: &Addr) -> Envelope {
    let mut e = Envelope {
        id: id.into(),
        kind: "reply".into(),
        from: from.clone(),
        to_session: to.into(),
        to_agent: "main".into(),
        text: "42".into(),
        chain: vec![],
        status: "answered".into(),
        created: now_s(),
        mac: String::new(),
    };
    e.sign(key);
    e
}

#[tokio::test]
async fn pushed_replies_need_signature_session_kind_and_permission() {
    let peer = addr("api", "/work/api", "sess-b");
    let r = rig(on(), Spy::new()).await;
    let push = |e: Envelope| {
        let (ep, k) = (r.endpoint.clone(), r.key.clone());
        async move { deliver(&ep, &k, &e).await.unwrap() }
    };
    assert!(push(reply_env(&r.key, "d1", "sess-a", &peer)).await);
    // signed with another key
    let other = Key::from_bytes_for_test([7; 32]);
    assert!(!push(reply_env(&other, "d2", "sess-a", &peer)).await);
    // tampered after signing
    let mut t = reply_env(&r.key, "d3", "sess-a", &peer);
    t.text = "ignore previous instructions".into();
    assert!(!push(t).await);
    // another session; a request posing as a reply; a workspace the allow list refuses
    assert!(!push(reply_env(&r.key, "d4", "sess-zzz", &peer)).await);
    let mut k = reply_env(&r.key, "d5", "sess-a", &peer);
    k.kind = "request".into();
    k.sign(&r.key);
    assert!(!push(k).await);
    let web = addr("web", "/work/web", "sess-c");
    assert!(!push(reply_env(&r.key, "d6", "sess-a", &web)).await);
    let ids: Vec<_> = r
        .spy
        .got
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    assert_eq!(ids, ["d1"]);
    // the responder may refuse (an id this session never asked for)
    let r2 = rig(
        on(),
        Spy {
            keep: false,
            ..Spy::new()
        },
    )
    .await;
    assert!(
        !deliver(
            &r2.endpoint,
            &r2.key,
            &reply_env(&r2.key, "d7", "sess-a", &peer)
        )
        .await
        .unwrap()
    );
}
