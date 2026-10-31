//! Two peers in one process over a real socket / named pipe, temp run dir (messaging.md C15-C24).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};
use synthesis_peers::net::{self, Conn, Listener};
use synthesis_peers::serve::{
    BoxFut, Down, Outgoing, PeerError, Reply, Request, Responder, Server, ask, discover, status,
};
use synthesis_peers::{Addr, Config, Entry, Key, Registration, load_or_create_key, run_dir};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    /// Sleep this long before answering (0 = answer at once).
    delay_ms: u64,
}

struct DropFlag(Arc<AtomicUsize>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Responder for Fake {
    fn status(&self) -> Value {
        json!({"status":"running","task_summary":"fixing auth","accepts":true,"agents":[]})
    }
    fn has_agent(&self, i: &str) -> bool {
        i == "main" || i == "developer-2"
    }
    fn answer(&self, r: Request, partial: UnboundedSender<String>) -> BoxFut<Reply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (delay, flag) = (self.delay_ms, DropFlag(self.dropped.clone()));
        Box::pin(async move {
            let _flag = flag;
            let _ = partial.send("thinking".into());
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Reply {
                status: "answered".into(),
                text: format!("{}: {} key=sk-ant-SECRET123", r.to_agent, r.text),
            }
        })
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

fn config(v: Value) -> Config {
    Config::from_settings(v.as_object().unwrap_or(&Map::new()), "/work/api")
}

struct Rig {
    _home: tempfile::TempDir,
    dir: PathBuf,
    key: Key,
    endpoint: String,
    fake: Arc<Fake>,
}

async fn rig(cfg: Value, fake: Fake) -> Rig {
    let home = tempfile::tempdir().unwrap();
    let dir = run_dir(home.path());
    let key = load_or_create_key(&dir).unwrap();
    let l = Listener::bind(&dir).unwrap();
    let endpoint = l.endpoint();
    let fake = Arc::new(fake);
    let server = Server::new(
        key.clone(),
        config(cfg),
        addr("api", "/work/api", "sess-a"),
        fake.clone(),
        Arc::new(redact),
        Arc::new(|_, _| {}),
    );
    tokio::spawn(server.run(l));
    Rig {
        _home: home,
        dir,
        key,
        endpoint,
        fake,
    }
}

async fn go(r: &Rig, from: &Addr, to: &str, id: &str, chain: &[String]) -> Result<String, String> {
    let o = Outgoing {
        id,
        from,
        to_agent: to,
        text: "which env sets TTL?",
        chain,
        timeout_s: 30,
        extra: Default::default(),
    };
    match ask(&r.endpoint, &r.key, &o, |_, _| {}).await {
        Ok(f) => Ok(format!("{}|{}", f.status, f.text)),
        Err(PeerError::Rejected(x)) => Err(x),
        Err(e) => Err(format!("{e:?}")),
    }
}

const ON: fn() -> Value = || json!({"enabled": true});

#[tokio::test]
async fn answers_with_partial_redaction_and_same_id() {
    let r = rig(ON(), Fake::default()).await;
    let from = addr("api", "/work/api", "sess-b");
    let mut seen = Vec::new();
    let o = Outgoing {
        id: "req-1",
        from: &from,
        to_agent: "developer-2",
        text: "hi",
        chain: &[],
        timeout_s: 30,
        extra: Default::default(),
    };
    let f = ask(&r.endpoint, &r.key, &o, |s, t| {
        seen.push((s, t.to_string()))
    })
    .await
    .unwrap();
    assert_eq!(f.status, "answered");
    assert_eq!(f.text, "developer-2: hi key=[REDACTED]");
    assert_eq!(f.from.unwrap().session_id, "sess-a");
    assert_eq!(seen, vec![(1, "thinking".to_string())]);
    let st = status(&r.endpoint, &r.key, &from).await.unwrap();
    assert_eq!(st["t"], "status");
    assert_eq!(st["task_summary"], "fixing auth");
}

#[tokio::test]
async fn off_by_default_and_policy_rejections() {
    let from = addr("api", "/work/api", "sess-b");
    let r = rig(json!({}), Fake::default()).await;
    assert_eq!(
        go(&r, &from, "main", "1", &[]).await,
        Err("disabled".into())
    );

    let r = rig(ON(), Fake::default()).await;
    let other = addr("web", "/work/web", "sess-c");
    assert_eq!(go(&r, &other, "main", "1", &[]).await, Err("denied".into()));
    assert_eq!(
        go(&r, &from, "nobody", "2", &[]).await,
        Err("no_agent".into())
    );
    assert_eq!(
        go(&r, &from, "main", "3", &["sess-a".into()]).await,
        Err("cycle".into())
    );
    let chain = vec!["x".to_string(), "y".to_string()];
    assert_eq!(
        go(&r, &from, "main", "4", &chain).await,
        Err("cycle".into())
    );
    let big = "a".repeat(40 * 1024);
    let o = Outgoing {
        id: "5",
        from: &from,
        to_agent: "main",
        text: &big,
        chain: &[],
        timeout_s: 30,
        extra: Default::default(),
    };
    assert!(matches!(
        ask(&r.endpoint, &r.key, &o, |_, _| {}).await,
        Err(PeerError::Rejected(x)) if x == "too_large"
    ));
    assert_eq!(
        r.fake.calls.load(Ordering::SeqCst),
        0,
        "rejected requests never reach the responder"
    );
}

#[tokio::test]
async fn rate_limit_per_sender_session() {
    let r = rig(ON(), Fake::default()).await;
    let from = addr("api", "/work/api", "sess-b");
    for i in 0..10 {
        assert!(go(&r, &from, "main", &format!("r{i}"), &[]).await.is_ok());
    }
    assert_eq!(
        go(&r, &from, "main", "r10", &[]).await,
        Err("rate_limited".into())
    );
    // `from` is self-asserted: a new session id does not buy a new allowance
    let other = addr("api", "/work/api", "sess-z");
    assert_eq!(
        go(&r, &other, "main", "z", &[]).await,
        Err("rate_limited".into())
    );
}

#[tokio::test]
async fn busy_timeout_and_cancel() {
    let r = rig(
        ON(),
        Fake {
            delay_ms: 60_000,
            ..Default::default()
        },
    )
    .await;
    let from = addr("api", "/work/api", "sess-b");
    let mut held = Vec::new();
    for i in 0..2 {
        let (e, k, f) = (r.endpoint.clone(), r.key.clone(), from.clone());
        held.push(tokio::spawn(async move {
            let o = Outgoing {
                id: &format!("h{i}"),
                from: &f,
                to_agent: "main",
                text: "t",
                chain: &[],
                timeout_s: 30,
                extra: Default::default(),
            };
            ask(&e, &k, &o, |_, _| {}).await
        }));
    }
    while r.fake.calls.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        go(&r, &from, "main", "third", &[]).await,
        Err("busy".into())
    );
    // Cancel: aborting the client closes the connection; the responder future is dropped.
    for h in &held {
        h.abort();
    }
    for _ in 0..200 {
        if r.fake.dropped.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(r.fake.dropped.load(Ordering::SeqCst), 2);

    let o = Outgoing {
        id: "t",
        from: &from,
        to_agent: "main",
        text: "t",
        chain: &[],
        timeout_s: 1,
        extra: Default::default(),
    };
    let f = ask(&r.endpoint, &r.key, &o, |_, _| {}).await.unwrap();
    assert_eq!((f.status.as_str(), f.text.as_str()), ("timeout", ""));
}

#[tokio::test]
async fn status_applies_the_allow_and_deny_lists() {
    let r = rig(
        json!({"enabled": true, "deny": ["ws:web"]}),
        Fake::default(),
    )
    .await;
    let ok = addr("api", "/work/api", "sess-b");
    assert!(status(&r.endpoint, &r.key, &ok).await.is_ok());
    let denied = addr("web", "/work/web", "sess-c");
    assert!(matches!(
        status(&r.endpoint, &r.key, &denied).await,
        Err(PeerError::Rejected(x)) if x == "denied"
    ));
    let off = rig(json!({"enabled": false}), Fake::default()).await;
    assert!(matches!(
        status(&off.endpoint, &off.key, &ok).await,
        Err(PeerError::Rejected(_))
    ));
}

#[tokio::test]
async fn wrong_key_and_impostor_leak_nothing() {
    let r = rig(ON(), Fake::default()).await;
    let from = addr("api", "/work/api", "sess-b");
    let bad = Key::from_bytes_for_test([7; 32]);
    let o = Outgoing {
        id: "w",
        from: &from,
        to_agent: "main",
        text: "secret question",
        chain: &[],
        timeout_s: 5,
        extra: Default::default(),
    };
    assert!(matches!(
        ask(&r.endpoint, &bad, &o, |_, _| {}).await,
        Err(PeerError::Auth)
    ));
    assert!(matches!(
        status(&r.endpoint, &bad, &from).await,
        Err(PeerError::Auth)
    ));
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 0);

    // An impostor listener holding another key: the client must abort before sending text.
    let dir = r.dir.clone();
    let mut l = Listener::bind(&dir).unwrap();
    let ep = l.endpoint();
    let imp = tokio::spawn(async move {
        let mut c = l.accept().await.unwrap();
        let hs = net::handshake_server(&mut c, &Key::from_bytes_for_test([9; 32])).await;
        (hs.is_err(), c.recv().await.ok().flatten())
    });
    assert!(matches!(
        ask(&ep, &r.key, &o, |_, _| {}).await,
        Err(PeerError::Auth)
    ));
    let (failed, extra) = imp.await.unwrap();
    assert!(
        failed && extra.is_none(),
        "impostor must receive no request text"
    );
}

#[tokio::test]
async fn oversized_frame_is_refused() {
    use tokio::io::AsyncWriteExt;
    let (mut a, b) = tokio::io::duplex(1 << 22);
    let mut c = Conn::new(b);
    let w = tokio::spawn(async move {
        a.write_all(&vec![b'a'; net::MAX_FRAME + 10]).await.unwrap();
        a.write_all(b"\n").await.unwrap();
        a
    });
    assert!(c.recv().await.is_err());
    drop(w.await.unwrap());
}

#[test]
fn long_socket_path_disables_peers() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("x".repeat(120)).join("run");
    net::prepare_run_dir(&dir).unwrap();
    // Windows pipes have no path limit, so only unix refuses.
    assert_eq!(Listener::bind(&dir).is_err(), cfg!(unix));
}

#[tokio::test]
async fn discovery_lists_permitted_live_peers_and_cleans_dead_ones() {
    let r = rig(ON(), Fake::default()).await;
    let entry = |sid: &str, label: &str, root: &str, ep: &str| Entry {
        v: 1,
        session_id: sid.into(),
        session_name: None,
        workspace_key: format!("{label:0<24}"),
        workspace_label: label.into(),
        workspace_root: root.into(),
        cwd: root.into(),
        pid: 1,
        pid_start: None,
        repo_key: String::new(),
        about: None,
        endpoint: ep.into(),
        started_at: 0,
    };
    let dead = r.dir.join("dead.sock").to_string_lossy().into_owned();
    let _regs = [
        Registration::create(&r.dir, &entry("live-1", "api", "/work/api", &r.endpoint)).unwrap(),
        Registration::create(&r.dir, &entry("dead-1", "api", "/work/api", &dead)).unwrap(),
        Registration::create(&r.dir, &entry("stuck-1", "api", "/work/api", &dead)).unwrap(),
        Registration::create(&r.dir, &entry("self-1", "api", "/work/api", &r.endpoint)).unwrap(),
        Registration::create(&r.dir, &entry("other-1", "web", "/work/web", &r.endpoint)).unwrap(),
    ];
    let cfg = config(ON());
    let peers = discover(
        &r.dir,
        &r.key,
        &cfg,
        &addr("api", "/work/api", "self-1"),
        |e| e.session_id == "dead-1",
    )
    .await;
    let got: Vec<_> = peers
        .iter()
        .map(|p| (p.entry.session_id.as_str(), p.state.is_ok()))
        .collect();
    assert_eq!(
        got.len(),
        3,
        "self and not-permitted sessions are not listed: {got:?}"
    );
    assert_eq!(peers[0].state.as_ref().unwrap_err(), &Down::Offline);
    assert!(peers[1].state.is_ok());
    assert_eq!(peers[2].state.as_ref().unwrap_err(), &Down::Unreachable);
    assert!(!r.dir.join("dead-1.json").exists(), "dead entry is deleted");
    assert!(r.dir.join("stuck-1.json").exists());
}

#[tokio::test]
async fn client_caps_a_huge_reply_and_a_partial_flood() {
    let home = tempfile::tempdir().unwrap();
    let dir = run_dir(home.path());
    let key = load_or_create_key(&dir).unwrap();
    let mut l = Listener::bind(&dir).unwrap();
    let ep = l.endpoint();
    let k2 = key.clone();
    // A server holding the key that ignores every limit.
    tokio::spawn(async move {
        let mut c = l.accept().await.unwrap();
        net::handshake_server(&mut c, &k2).await.unwrap();
        let f = c.recv().await.unwrap().unwrap();
        let id = f["id"].as_str().unwrap().to_string();
        c.send(&json!({"t":"accepted","id":id})).await.unwrap();
        let long = "x".repeat(1000);
        for seq in 0..500 {
            let t = "p".repeat(2000);
            c.send(&json!({"t":"partial","id":id,"seq":seq,"text":t}))
                .await
                .unwrap();
        }
        let big = "r".repeat(900 * 1024);
        c.send(&json!({"t":"reply","id":id,"status":long,"text":big,
            "from":{"workspace_key":long,"workspace_label":long,"workspace_root":long,
                    "session_id":long,"instance":long,"kind":long}}))
            .await
            .unwrap();
    });
    let from = addr("api", "/work/api", "sess-b");
    let o = Outgoing {
        id: "x",
        from: &from,
        to_agent: "main",
        text: "q",
        chain: &[],
        timeout_s: 30,
        extra: Default::default(),
    };
    let (mut n, mut bytes) = (0, 0);
    let f = ask(&ep, &key, &o, |_, t| {
        n += 1;
        bytes += t.len();
    })
    .await
    .unwrap();
    assert!(n <= 64 && bytes <= 64 * 1024, "{n} partials, {bytes} bytes");
    assert_eq!(f.text.len(), 32 * 1024);
    assert_eq!(f.status.len(), 256);
    let from = f.from.unwrap();
    assert!(
        [
            &from.kind,
            &from.instance,
            &from.session_id,
            &from.workspace_label
        ]
        .iter()
        .all(|s| s.len() == 256)
    );
}

#[test]
fn endpoints_must_be_ours_and_the_key_must_be_private() {
    let home = tempfile::tempdir().unwrap();
    let dir = run_dir(home.path());
    let ok = dir.join("0123456789abcdef.sock");
    assert!(synthesis_peers::endpoint_ok(&dir, &ok.to_string_lossy()) == cfg!(unix));
    assert!(!synthesis_peers::endpoint_ok(&dir, "/etc/passwd"));
    assert!(!synthesis_peers::endpoint_ok(
        &dir,
        &dir.join("../x.sock").to_string_lossy()
    ));
    assert!(!synthesis_peers::endpoint_ok(
        &dir,
        &dir.join("a.json").to_string_lossy()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        load_or_create_key(&dir).unwrap();
        let k = dir.join("peer.key");
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            load_or_create_key(&dir).is_err(),
            "a group/world readable key is refused"
        );
        std::fs::remove_file(&k).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", &k).unwrap();
        assert!(load_or_create_key(&dir).is_err(), "a linked key is refused");
        // a run dir that is a symlink is refused
        let real = home.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = home.path().join("linked-run");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(net::prepare_run_dir(&link).is_err());
    }
}
