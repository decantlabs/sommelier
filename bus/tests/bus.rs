//! End to end tests: a real hub on a free local port, real WebSocket clients.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::{Kind, Message, Role, format_line};
use sommelier::proto::{ConnectParams, FilterSpec};
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

static N: AtomicU32 = AtomicU32::new(0);

struct TestHub {
    url: String,
    dir: PathBuf,
    tokens: HashMap<&'static str, String>,
    task: tokio::task::JoinHandle<()>,
}

impl TestHub {
    fn opts(&self, who: &str) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.tokens[who].clone(),
        }
    }

    async fn send(&self, who: &str, req: Value) -> Value {
        client::request(&self.opts(who), req)
            .await
            .expect("request")
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fresh_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sommelier-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_tokens(dir: &std::path::Path) -> HashMap<&'static str, String> {
    let mut t = TokenFile::default();
    let mut map = HashMap::new();
    for (name, role) in [
        ("orch", Role::Orchestrator),
        ("alice", Role::Member),
        ("bob", Role::Member),
        ("watcher", Role::Observer),
    ] {
        map.insert(name, t.add(name, role).unwrap());
    }
    t.save(&dir.join("tokens.json")).unwrap();
    map
}

async fn start_in(
    dir: PathBuf,
    tokens: HashMap<&'static str, String>,
    tweak: impl FnOnce(&mut HubConfig),
) -> TestHub {
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    tweak(&mut cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    TestHub {
        url,
        dir,
        tokens,
        task,
    }
}

async fn start(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = fresh_dir();
    let tokens = write_tokens(&dir);
    start_in(dir, tokens, tweak).await
}

/// A bare WebSocket client, the way a non Rust agent would talk to the hub.
struct Raw {
    ws: client::Ws,
}

impl Raw {
    async fn open(url: &str) -> Raw {
        let (ws, _) = tokio_tungstenite::connect_async(url.into_client_request().unwrap())
            .await
            .expect("connect");
        Raw { ws }
    }

    /// Observers put the token into the URL (as a plain monitor must), everyone else into the header.
    async fn with_token(hub: &TestHub, who: &str, query: &str) -> Raw {
        if who == "watcher" {
            let sep = if query.is_empty() { "" } else { "&" };
            return Raw::open(&format!(
                "{}/?token={}{sep}{query}",
                hub.url, hub.tokens[who]
            ))
            .await;
        }
        let mut req = format!("{}/?{query}", hub.url)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", hub.tokens[who]).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("connect");
        Raw { ws }
    }

    async fn send(&mut self, v: Value) {
        self.ws.send(WsMsg::text(v.to_string())).await.unwrap();
    }

    /// Next text frame as JSON, panics after 5 s.
    async fn next(&mut self) -> Value {
        loop {
            let f = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("stream ended")
                .expect("frame error");
            if let WsMsg::Text(t) = f {
                assert!(!t.as_str().contains('\n'), "a frame is one line");
                return serde_json::from_str(t.as_str()).unwrap();
            }
        }
    }

    /// Asserts that nothing arrives for a short while.
    async fn quiet(&mut self) {
        let r = tokio::time::timeout(Duration::from_millis(300), self.ws.next()).await;
        assert!(r.is_err(), "expected no frame, got {r:?}");
    }
}

fn send_req(kind: &str, topic: &str, body: &str, refs: &[&str]) -> Value {
    json!({"op": "send", "kind": kind, "topic": topic, "body": body, "refs": refs})
}

#[tokio::test]
async fn two_clients_talk() {
    let hub = start(|_| {}).await;
    let mut alice = Raw::with_token(&hub, "alice", "").await;
    let r = hub
        .send(
            "bob",
            send_req(
                "READY",
                "issue/189",
                "phi count bounded",
                &["shaders/ssa-phi", "1a2b3c4"],
            ),
        )
        .await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["id"], 1);
    let m: Message = serde_json::from_value(alice.next().await).unwrap();
    assert_eq!(
        (m.id, m.from.as_str(), m.role, m.kind),
        (1, "bob", Role::Member, Kind::Ready)
    );
    let line = format_line(&m);
    assert!(
        line.starts_with("#1 ")
            && line.ends_with(" bob READY issue/189 shaders/ssa-phi 1a2b3c4 : phi count bounded"),
        "{line}"
    );

    // And back: alice answers on her own connection and gets the reply plus her own message.
    alice
        .send(json!({"kind": "CHAT", "body": "thanks", "rid": "a1"}))
        .await;
    let mut got = [alice.next().await, alice.next().await];
    got.sort_by_key(|v| v.get("reply").is_none());
    assert_eq!(got[0]["reply"], "send");
    assert_eq!(got[0]["rid"], "a1");
    assert_eq!(got[1]["from"], "alice");
}

#[tokio::test]
async fn spoofing_refused() {
    let hub = start(|_| {}).await;
    for field in ["from", "id", "ts", "role"] {
        let mut req = send_req("CHAT", "", "hi", &[]);
        req[field] = json!("orch");
        let r = hub.send("alice", req).await;
        assert_eq!(r["ok"], false);
        assert!(
            r["error"].as_str().unwrap().contains("stamped by the hub"),
            "{r}"
        );
    }
    let r = hub.send("alice", send_req("CHAT", "", "hi", &[])).await;
    assert_eq!(r["ok"], true);
    let log = hub.send("watcher", json!({"op": "log"})).await;
    assert_eq!(log["messages"].as_array().unwrap().len(), 1);
    assert_eq!(log["messages"][0]["from"], "alice");
}

#[tokio::test]
async fn bad_token_refused() {
    let hub = start(|_| {}).await;
    let e = client::connect(
        &Opts {
            url: hub.url.clone(),
            token: "sb_nope".into(),
        },
        &ConnectParams::default(),
    )
    .await
    .expect_err("must fail");
    assert!(e.fatal && e.msg.contains("401"), "{}", e.msg);
    // A member token in the URL is refused, an observer token is fine.
    let url = format!("{}/?token={}", hub.url, hub.tokens["alice"]);
    let r = tokio_tungstenite::connect_async(url.into_client_request().unwrap()).await;
    assert!(r.is_err(), "member token in the URL must be refused");
    Raw::with_token(&hub, "watcher", "").await;
}

#[tokio::test]
async fn claim_conflict_and_release() {
    let hub = start(|_| {}).await;
    let r = hub
        .send(
            "alice",
            send_req("CLAIM", "issue/186", "taking the xma clock", &[]),
        )
        .await;
    assert_eq!(r["ok"], true, "{r}");
    let r = hub
        .send("bob", send_req("CLAIM", "issue/186", "me too", &[]))
        .await;
    assert_eq!(r["ok"], false);
    let e = r["error"].as_str().unwrap();
    assert!(e.contains("issue/186 is held by alice since #1"), "{e}");

    let c = hub.send("watcher", json!({"op": "claims"})).await;
    assert_eq!(c["claims"][0]["topic"], "issue/186");
    assert_eq!(c["claims"][0]["holder"], "alice");

    let r = hub
        .send("bob", send_req("RELEASE", "issue/186", "", &[]))
        .await;
    assert_eq!(r["ok"], false);
    let r = hub
        .send("alice", send_req("RELEASE", "issue/186", "done", &[]))
        .await;
    assert_eq!(r["ok"], true);
    let r = hub
        .send("bob", send_req("CLAIM", "issue/186", "", &[]))
        .await;
    assert_eq!(r["ok"], true);
    // The orchestrator may free anything.
    let r = hub
        .send("orch", send_req("RELEASE", "issue/186", "reassigning", &[]))
        .await;
    assert_eq!(r["ok"], true);
}

#[tokio::test]
async fn claim_expires_without_heartbeat() {
    let hub = start(|c| {
        c.claim_ttl = Duration::from_millis(300);
        c.sweep_every = Duration::from_millis(50);
    })
    .await;
    let mut watcher = Raw::with_token(&hub, "watcher", "kinds=RELEASE").await;
    hub.send("alice", send_req("CLAIM", "issue/7", "", &[]))
        .await;
    let m = watcher.next().await;
    assert_eq!(
        (m["from"].as_str(), m["kind"].as_str(), m["topic"].as_str()),
        (Some("hub"), Some("RELEASE"), Some("issue/7"))
    );
    assert_eq!(m["role"], "hub");
    assert!(m["body"].as_str().unwrap().contains("expired"));
    let c = hub.send("watcher", json!({"op": "claims"})).await;
    assert!(c["claims"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn heartbeat_keeps_claim() {
    let hub = start(|c| {
        c.claim_ttl = Duration::from_millis(600);
        c.sweep_every = Duration::from_millis(50);
    })
    .await;
    hub.send("alice", send_req("CLAIM", "issue/8", "", &[]))
        .await;
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let r = hub.send("alice", json!({"op": "heartbeat"})).await;
        assert_eq!(r["claims"], 1, "{r}");
    }
}

#[tokio::test]
async fn replay_after_reconnect_and_restart() {
    let dir = fresh_dir();
    let tokens = write_tokens(&dir);
    let hub = start_in(dir.clone(), tokens.clone(), |_| {}).await;
    let mut alice = Raw::with_token(&hub, "alice", "").await;
    for i in 1..=3 {
        hub.send(
            "bob",
            send_req("FINDING", "issue/1", &format!("finding {i}"), &[]),
        )
        .await;
    }
    for i in 1..=3 {
        assert_eq!(alice.next().await["id"], i);
    }
    drop(alice);
    hub.send("bob", send_req("FIX", "issue/2", "four", &[]))
        .await;
    hub.send("bob", send_req("STUCK", "issue/3", "five", &[]))
        .await;
    hub.send("alice", send_req("CLAIM", "issue/9", "", &[]))
        .await;

    // Reconnect from the last seen id: exactly the missed messages, then live.
    let mut alice = Raw::with_token(&hub, "alice", "since=4").await;
    assert_eq!(alice.next().await["body"], "four");
    assert_eq!(alice.next().await["body"], "five");
    assert_eq!(alice.next().await["kind"], "CLAIM");
    alice.quiet().await;

    // last=N with a filter.
    let mut w = Raw::with_token(&hub, "watcher", "last=2&kinds=FINDING&live=0").await;
    assert_eq!(w.next().await["body"], "finding 2");
    assert_eq!(w.next().await["body"], "finding 3");
    w.quiet().await;

    // Restart the hub on the same folder: ids continue, claims survive.
    drop(alice);
    drop(hub);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let hub = start_in(dir, tokens, |_| {}).await;
    let r = hub
        .send("bob", send_req("CHAT", "", "after restart", &[]))
        .await;
    assert_eq!(r["id"], 7);
    let r = hub.send("bob", send_req("CLAIM", "issue/9", "", &[])).await;
    assert_eq!(r["ok"], false, "claim must survive the restart: {r}");
    let log = hub.send("watcher", json!({"op": "log", "since": 6})).await;
    let ids: Vec<_> = log["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![6, 7]);
}

#[tokio::test]
async fn tail_client_resumes_across_hub_restart() {
    let dir = fresh_dir();
    let tokens = write_tokens(&dir);
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let first = hub::Hub::open(cfg).unwrap();
    let first_task = tokio::spawn(hub::serve(first.clone(), listener));
    let hub = TestHub {
        url: format!("ws://127.0.0.1:{port}"),
        dir: dir.clone(),
        tokens: tokens.clone(),
        task: tokio::spawn(async {}),
    };
    let opts = hub.opts("alice");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tail = tokio::spawn(async move {
        client::tail(&opts, ConnectParams::default(), false, |m, _| {
            tx.send(m.body.clone()).unwrap();
            true
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.send("bob", send_req("CHAT", "", "one", &[])).await;
    assert_eq!(rx.recv().await.unwrap(), "one");
    first.shutdown();
    first_task.await.unwrap().unwrap();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Same port, same folder. A message lands while the tail is still reconnecting.
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let h = hub::Hub::open(cfg).unwrap();
    {
        let bob = TokenFile::load(&dir.join("tokens.json"))
            .unwrap()
            .get("bob")
            .cloned()
            .unwrap();
        h.publish(
            &bob,
            Kind::Chat,
            String::new(),
            "two".into(),
            vec![],
            vec![],
        )
        .unwrap();
    }
    let task = tokio::spawn(hub::serve(h, listener));
    let got = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("tail did not resume")
        .unwrap();
    assert_eq!(got, "two");
    task.abort();
    tail.abort();
    let _ = tokens;
}

#[tokio::test]
async fn observer_cannot_send_but_can_read() {
    let hub = start(|_| {}).await;
    let mut w = Raw::with_token(&hub, "watcher", "").await;
    w.send(send_req("CHAT", "", "hello", &[])).await;
    let r = w.next().await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("read only"), "{r}");
    hub.send("alice", send_req("CHAT", "", "visible", &[]))
        .await;
    assert_eq!(w.next().await["body"], "visible");
}

#[tokio::test]
async fn only_orchestrator_sends_task() {
    let hub = start(|_| {}).await;
    let r = hub
        .send("alice", send_req("TASK", "issue/5", "scope: x", &[]))
        .await;
    assert_eq!(r["ok"], false);
    let r = hub
        .send(
            "orch",
            send_req(
                "TASK",
                "issue/5",
                "scope: x | files: a.rs | done: tests pass",
                &[],
            ),
        )
        .await;
    assert_eq!(r["ok"], true);
}

#[tokio::test]
async fn rate_limit() {
    let hub = start(|c| {
        c.rate_per_sec = 0.5;
        c.burst = 3.0;
    })
    .await;
    let mut a = Raw::with_token(&hub, "alice", "live=0").await;
    let mut oks = 0;
    let mut refused = None;
    for i in 0..5 {
        a.send(send_req("CHAT", "", &format!("{i}"), &[])).await;
        let r = a.next().await;
        if r["ok"] == true {
            oks += 1;
        } else {
            refused = Some(r);
        }
    }
    assert_eq!(oks, 3);
    let r = refused.unwrap();
    assert!(
        r["error"].as_str().unwrap().starts_with("rate limit"),
        "{r}"
    );
    // Another member is not affected.
    assert_eq!(
        hub.send("bob", send_req("CHAT", "", "x", &[])).await["ok"],
        true
    );
}

#[tokio::test]
async fn oversize_and_malformed_refused() {
    let hub = start(|c| c.limits.kind_chars.clear()).await;
    let mut a = Raw::with_token(&hub, "alice", "live=0").await;
    a.send(send_req("CHAT", "", &"x".repeat(5000), &[])).await;
    let r = a.next().await;
    assert!(
        r["error"].as_str().unwrap().contains("body is 5000 bytes"),
        "{r}"
    );
    a.send(send_req("CHAT", "", &"x".repeat(9000), &[])).await;
    let r = a.next().await;
    assert!(r["error"].as_str().unwrap().contains("frame is"), "{r}");
    a.send(send_req("CHAT", "", "two\nlines", &[])).await;
    assert!(
        a.next().await["error"]
            .as_str()
            .unwrap()
            .contains("newline")
    );
    a.ws.send(WsMsg::binary(vec![1u8, 2, 3])).await.unwrap();
    assert!(a.next().await["error"].as_str().unwrap().contains("binary"));
    a.ws.send(WsMsg::text("not json")).await.unwrap();
    assert!(a.next().await["error"].as_str().unwrap().contains("JSON"));
    // Still usable afterwards.
    a.send(send_req("CHAT", "", "fine", &[])).await;
    assert_eq!(a.next().await["ok"], true);
}

#[tokio::test]
async fn revoked_token_is_cut_off() {
    let hub = start(|c| c.token_reload_every = Duration::from_millis(100)).await;
    let mut a = Raw::with_token(&hub, "alice", "").await;
    let path = hub.dir.join("tokens.json");
    let mut t = TokenFile::load(&path).unwrap();
    assert!(t.revoke("alice"));
    t.save(&path).unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(Ok(f)) = a.ws.next().await {
            if let WsMsg::Close(Some(c)) = f {
                return c.reason.as_str().to_owned();
            }
        }
        String::new()
    })
    .await
    .expect("not cut off");
    assert_eq!(closed, "token revoked");
    let e = client::connect(&hub.opts("alice"), &ConnectParams::default())
        .await
        .err()
        .unwrap();
    assert!(e.fatal);
}

#[tokio::test]
async fn rotated_token_replaces_the_old_one() {
    let mut hub = start(|c| c.token_reload_every = Duration::from_millis(100)).await;
    let path = hub.dir.join("tokens.json");
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains(&hub.tokens["bob"])
    );
    let mut t = TokenFile::load(&path).unwrap();
    let (new, _) = t.rotate("bob").unwrap();
    t.save(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let e = client::connect(&hub.opts("bob"), &ConnectParams::default())
        .await
        .expect_err("old token must fail");
    assert!(e.fatal);
    hub.tokens.insert("bob", new);
    let r = hub
        .send("bob", send_req("CHAT", "", "new token", &[]))
        .await;
    assert_eq!(r["ok"], true, "{r}");
}

#[tokio::test]
async fn tail_once_prints_the_replay_and_returns() {
    let hub = start(|_| {}).await;
    for i in 1..=4 {
        hub.send("bob", send_req("CHAT", "", &format!("m{i}"), &[]))
            .await;
    }
    let run = |params: ConnectParams| {
        let opts = hub.opts("alice");
        async move {
            let mut got = Vec::new();
            let r = tokio::time::timeout(
                Duration::from_secs(5),
                client::tail(&opts, params, true, |m, _| {
                    got.push(m.body.clone());
                    true
                }),
            )
            .await;
            (r, got)
        }
    };
    let (r, got) = run(ConnectParams {
        last: Some(2),
        ..Default::default()
    })
    .await;
    assert!(r.expect("--once must not hang").is_ok());
    assert_eq!(got, vec!["m3", "m4"]);
    let (r, got) = run(ConnectParams {
        since: Some(2),
        ..Default::default()
    })
    .await;
    assert!(r.expect("--once must not hang").is_ok());
    assert_eq!(got, vec!["m2", "m3", "m4"]);
    let (r, got) = run(ConnectParams::default()).await;
    assert!(
        r.expect("--once without replay must return at once")
            .is_ok()
    );
    assert!(got.is_empty());
}

#[tokio::test]
async fn addressed_messages_and_to_me() {
    let hub = start(|_| {}).await;
    let mut alice = Raw::with_token(&hub, "alice", "to=me").await;
    let mut bob = Raw::with_token(&hub, "bob", "to=me").await;
    let mut watcher = Raw::with_token(&hub, "watcher", "").await;

    let mut req = send_req("TASK", "issue/5", "scope: x", &[]);
    req["to"] = json!(["Alice"]);
    let r = hub.send("orch", req).await;
    assert_eq!(r["ok"], true, "{r}");
    let m = alice.next().await;
    assert_eq!(m["to"], json!(["alice"]), "stored in lower case");
    let line = format_line(&serde_json::from_value(m).unwrap());
    assert!(
        line.contains(" orch TASK @alice issue/5 : scope: x"),
        "{line}"
    );
    assert_eq!(
        watcher.next().await["kind"],
        "TASK",
        "without a filter everyone sees it"
    );

    hub.send("orch", send_req("CHAT", "", "for everyone", &[]))
        .await;
    assert_eq!(alice.next().await["body"], "for everyone");
    assert_eq!(
        bob.next().await["body"],
        "for everyone",
        "bob skipped the TASK for alice"
    );
    assert!(
        watcher.next().await.get("to").is_none(),
        "no addressee, no key"
    );

    let mut req = send_req("CHAT", "", "typo", &[]);
    req["to"] = json!("alcie");
    let r = hub.send("orch", req).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("unknown addressee `alcie`"),
        "{r}"
    );

    let log = hub.send("bob", json!({"op": "log", "to": "me"})).await;
    let bodies: Vec<_> = log["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].clone())
        .collect();
    assert_eq!(bodies, vec![json!("for everyone")]);
}

#[tokio::test]
async fn size_limit_by_kind() {
    let hub = start(|_| {}).await;
    let cases = [
        ("ACCEPT", 200),
        ("DECLINE", 200),
        ("CHAT", 600),
        ("RUN_REQUEST", 600),
        ("CLAIM", 600),
        ("HELLO", 600),
        ("READY", 1200),
        ("FIX", 1200),
        ("STUCK", 1200),
        ("RUN_RESULT", 1200),
        ("FINDING", 1200),
        ("LANDED", 1200),
    ];
    for (i, (kind, max)) in cases.iter().enumerate() {
        let topic = format!("t/{i}");
        let ok = hub
            .send("alice", send_req(kind, &topic, &"x".repeat(*max), &[]))
            .await;
        assert_eq!(ok["ok"], true, "{kind} at its limit: {ok}");
        let r = hub
            .send("alice", send_req(kind, &topic, &"x".repeat(max + 1), &[]))
            .await;
        let e = r["error"].as_str().unwrap_or_default();
        assert!(
            e.contains(&format!("{} characters", max + 1))
                && e.contains(&format!("limit for {kind} is {max}"))
                && e.contains("commit, the issue or a file"),
            "{kind}: {r}"
        );
    }
    let r = hub
        .send("orch", send_req("TASK", "t/x", &"x".repeat(601), &[]))
        .await;
    assert_eq!(r["ok"], false, "TASK over 600: {r}");
    let r = hub
        .send("orch", send_req("TASK", "t/x", &"x".repeat(600), &[]))
        .await;
    assert_eq!(r["ok"], true, "TASK at 600: {r}");
    let r = hub
        .send(
            "alice",
            send_req("READY", "t/y", &"\u{e9}".repeat(1200), &[]),
        )
        .await;
    assert_eq!(r["ok"], true, "limits count characters: {r}");
}

#[tokio::test]
async fn size_limit_can_be_changed() {
    let hub = start(|c| {
        c.limits.kind_chars.insert(Kind::Chat, 10);
    })
    .await;
    let r = hub
        .send("alice", send_req("CHAT", "", "0123456789", &[]))
        .await;
    assert_eq!(r["ok"], true);
    let r = hub
        .send("alice", send_req("CHAT", "", "01234567890", &[]))
        .await;
    assert_eq!(r["ok"], false);
}

#[tokio::test]
async fn role_addressing_and_worker_filter() {
    let hub = start(|_| {}).await;
    let mut plain = Raw::with_token(&hub, "alice", "to=me").await;
    let mut w1 = Raw::with_token(&hub, "alice", "worker=d3d12-fixes").await;
    let mut w2 = Raw::with_token(&hub, "alice", "worker=other.worker").await;
    let mut bob = Raw::with_token(&hub, "bob", "worker=d3d12-fixes").await;

    let mut req = send_req("CHAT", "", "for the d3d worker", &[]);
    req["to"] = json!(["Alice/d3d12-fixes"]);
    let r = hub.send("orch", req).await;
    assert_eq!(r["ok"], true, "{r}");
    let m = plain.next().await;
    assert_eq!(
        m["to"],
        json!(["alice/d3d12-fixes"]),
        "name in lower case, worker as given"
    );
    let line = format_line(&serde_json::from_value(m).unwrap());
    assert!(line.contains(" CHAT @alice/d3d12-fixes "), "{line}");
    assert_eq!(w1.next().await["body"], "for the d3d worker");

    let mut req = send_req("CHAT", "", "for the whole participant", &[]);
    req["to"] = json!("alice");
    hub.send("orch", req).await;
    hub.send("orch", send_req("CHAT", "", "for everyone", &[]))
        .await;
    assert_eq!(plain.next().await["body"], "for the whole participant");
    assert_eq!(w1.next().await["body"], "for the whole participant");
    assert_eq!(w2.next().await["body"], "for the whole participant");
    assert_eq!(
        w2.next().await["body"],
        "for everyone",
        "w2 skipped the message for the d3d worker"
    );
    assert_eq!(w1.next().await["body"], "for everyone");
    assert_eq!(plain.next().await["body"], "for everyone");
    assert_eq!(
        bob.next().await["body"],
        "for everyone",
        "same worker name, other participant"
    );

    // The history filter works the same way.
    let log = hub
        .send("alice", json!({"op": "log", "worker": "other.worker"}))
        .await;
    let bodies: Vec<_> = log["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].clone())
        .collect();
    assert_eq!(
        bodies,
        vec![json!("for the whole participant"), json!("for everyone")]
    );
    let log = hub.send("alice", json!({"op": "log", "to": "me"})).await;
    assert_eq!(
        log["messages"].as_array().unwrap().len(),
        3,
        "without a worker all three"
    );

    // Bad parts are refused: no token for the name, bad characters in the worker.
    for (to, want) in [
        ("zed/x", "unknown addressee `zed`"),
        ("alice/a b", "worker"),
        ("alice/", "worker"),
        ("alice/a/b", "worker"),
        ("alice/\u{fc}", "worker"),
    ] {
        let mut req = send_req("CHAT", "", "x", &[]);
        req["to"] = json!([to]);
        let r = hub.send("orch", req).await;
        let e = r["error"].as_str().unwrap_or_default();
        assert!(e.contains(want), "{to}: {r}");
    }
}

#[tokio::test]
async fn plain_websocket_with_query_filters() {
    let hub = start(|_| {}).await;
    let mut w = Raw::with_token(&hub, "watcher", "kinds=RUN_RESULT&topic=run/*").await;
    hub.send("alice", send_req("CHAT", "run/1", "noise", &[]))
        .await;
    hub.send(
        "alice",
        send_req("RUN_RESULT", "other/1", "wrong topic", &[]),
    )
    .await;
    hub.send("alice", send_req("RUN_RESULT", "run/1", "fps 31", &[]))
        .await;
    assert_eq!(w.next().await["body"], "fps 31");
    w.quiet().await;
}

/// The first real flow: a test run on a guest machine.
#[tokio::test]
async fn run_request_flow() {
    let hub = start(|_| {}).await;
    let orch_opts = hub.opts("orch");
    let params = ConnectParams {
        filter: FilterSpec {
            topic: Some("run/*".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = tokio::spawn(async move {
        client::tail(&orch_opts, params, false, |m, _| {
            tx.send(m.clone()).unwrap();
            m.kind != Kind::RunResult
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hello = hub
        .send(
            "bob",
            send_req(
                "HELLO",
                "",
                "os=linux gpu=rx7800 cpu=7700x agent=codex",
                &[],
            ),
        )
        .await;
    assert_eq!(hello["ok"], true);
    let req = hub
        .send(
            "orch",
            send_req(
                "RUN_REQUEST",
                "run/42",
                "env: SOMMELIER_LOG=1 | secs: 120 | need: os=linux | report: fps, first error",
                &["06ecbed"],
            ),
        )
        .await;
    let req_id = req["id"].as_u64().unwrap();
    hub.send(
        "bob",
        send_req("ACCEPT", "run/42", "starting", &[&format!("#{req_id}")]),
    )
    .await;
    hub.send(
        "bob",
        send_req(
            "RUN_RESULT",
            "run/42",
            "exit 0 | fps 31 | no errors",
            &["06ecbed"],
        ),
    )
    .await;
    let mut kinds = Vec::new();
    while let Some(m) = rx.recv().await {
        kinds.push(m.kind);
        if m.kind == Kind::RunResult {
            assert_eq!(m.from, "bob");
            break;
        }
    }
    assert_eq!(kinds, vec![Kind::RunRequest, Kind::Accept, Kind::RunResult]);
    assert!(watch.await.unwrap().is_ok());
}

/// A raw HTTP/1.1 request, returns status line plus headers and the body.
async fn http(hub: &TestHub, req: &str) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = hub.url.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .expect("http timeout")
        .unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.to_owned(), body.to_owned())
}

fn get(path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: bus.example.org\r\n{extra}\r\n")
}

#[tokio::test]
async fn serves_dashboard_files_safely() {
    let ui = fresh_dir();
    std::fs::write(ui.join("index.html"), "<h1>bus</h1>").unwrap();
    std::fs::write(ui.join("app.js"), "1").unwrap();
    std::fs::create_dir_all(ui.join("sub")).unwrap();
    let ui2 = ui.clone();
    let hub = start(move |c| c.ui_dir = Some(ui2)).await;

    let (head, body) = http(&hub, &get("/", "")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Type: text/html; charset=utf-8"));
    assert!(
        head.contains("Content-Security-Policy: default-src 'none'; script-src 'self'"),
        "{head}"
    );
    assert!(
        head.contains("connect-src 'self' wss://bus.example.org"),
        "{head}"
    );
    assert_eq!(body, "<h1>bus</h1>");
    let (head, _) = http(&hub, &get("/app.js", "")).await;
    assert!(head.contains("text/javascript"), "{head}");
    for bad in [
        "/sub",
        "/sub/",
        "/../Cargo.toml",
        "/%2e%2e/x",
        "/.git",
        "/nope.css",
    ] {
        let (head, _) = http(&hub, &get(bad, "")).await;
        assert!(head.starts_with("HTTP/1.1 404"), "{bad}: {head}");
    }
    let (head, _) = http(
        &hub,
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 405"), "{head}");
    // The WebSocket on the same port still works.
    let r = hub
        .send("alice", send_req("CHAT", "", "still here", &[]))
        .await;
    assert_eq!(r["ok"], true);
}

#[tokio::test]
async fn api_state_snapshot() {
    let hub = start(|_| {}).await;
    let _alice_online = Raw::with_token(&hub, "alice", "").await;
    hub.send("bob", send_req("HELLO", "", "os=linux gpu=rx7800", &[]))
        .await;
    hub.send("bob", send_req("CLAIM", "issue/9", "", &[])).await;
    let mut st = send_req(
        "STATUS",
        "agent/p3",
        "phi placement",
        &["state:running", "model:sonnet", "branch:b1"],
    );
    hub.send("orch", st.clone()).await;
    st["topic"] = json!("agent/p4");
    hub.send("orch", st).await;
    hub.send("orch", send_req("STATUS", "agent/p4", "", &["state:done"]))
        .await;

    let (head, body) = http(&hub, &get("/api/state", "")).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head} {body}");
    let (head, _) = http(
        &hub,
        &get(&format!("/api/state?token={}", hub.tokens["alice"]), ""),
    )
    .await;
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "member token in the URL: {head}"
    );
    let (head, _) = http(
        &hub,
        &get(&format!("/api/state?token={}", hub.tokens["watcher"]), ""),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let (head, body) = http(
        &hub,
        &get(
            "/api/state",
            &format!("Authorization: Bearer {}\r\n", hub.tokens["alice"]),
        ),
    )
    .await;
    assert!(
        head.starts_with("HTTP/1.1 200") && head.contains("application/json"),
        "{head}"
    );
    let v: Value = serde_json::from_str(&body).unwrap();

    assert_eq!(v["hub"]["id"], 5);
    let member = |n: &str| {
        v["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(member("alice")["online"], true);
    assert_eq!(member("bob")["online"], false);
    assert_eq!(member("bob")["hello"], "os=linux gpu=rx7800");
    let agents = v["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "p4 is done: {agents:?}");
    assert_eq!(
        (
            &agents[0]["owner"],
            &agents[0]["agent"],
            &agents[0]["state"],
            &agents[0]["model"],
            &agents[0]["branch"],
            &agents[0]["text"],
            &agents[0]["stale"]
        ),
        (
            &json!("orch"),
            &json!("p3"),
            &json!("running"),
            &json!("sonnet"),
            &json!("b1"),
            &json!("phi placement"),
            &json!(false)
        )
    );
    assert_eq!(v["claims"][0]["topic"], "issue/9");
    assert_eq!(v["claims"][0]["holder"], "bob");
    let kinds: Vec<_> = v["recent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        vec![json!("HELLO"), json!("CLAIM")],
        "STATUS stays out of recent"
    );
}

#[tokio::test]
async fn status_roster_rules() {
    let hub = start(|_| {}).await;
    let r = hub
        .send("alice", send_req("STATUS", "issue/1", "x", &[]))
        .await;
    assert!(r["error"].as_str().unwrap().contains("agent/"), "{r}");
    let r = hub
        .send(
            "alice",
            send_req("STATUS", "agent/a1", "x", &["state:asleep"]),
        )
        .await;
    assert!(
        r["error"].as_str().unwrap().contains("unknown state"),
        "{r}"
    );
    let r = hub
        .send("watcher", send_req("STATUS", "agent/w", "x", &[]))
        .await;
    assert_eq!(r["ok"], false);
    hub.send("alice", send_req("STATUS", "agent/a1", "first", &[]))
        .await;
    hub.send(
        "alice",
        send_req("STATUS", "agent/a1", "second", &["state:blocked"]),
    )
    .await;
    hub.send("bob", send_req("STATUS", "agent/a1", "bob's own a1", &[]))
        .await;
    let r = hub
        .send("alice", json!({"op": "roster", "owner": "me"}))
        .await;
    let a = r["agents"].as_array().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(
        (&a[0]["text"], &a[0]["state"]),
        (&json!("second"), &json!("blocked"))
    );
    let r = hub.send("watcher", json!({"op": "roster"})).await;
    assert_eq!(r["agents"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn shutdown_closes_connections_with_restart_code() {
    let dir = fresh_dir();
    let tokens = write_tokens(&dir);
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let h = hub::Hub::open(cfg).unwrap();
    let task = tokio::spawn(hub::serve(h.clone(), listener));
    let hub = TestHub {
        url,
        dir,
        tokens,
        task: tokio::spawn(async {}),
    };
    let mut a = Raw::with_token(&hub, "alice", "").await;
    hub.send("bob", send_req("CHAT", "", "before", &[])).await;
    assert_eq!(a.next().await["body"], "before");
    h.shutdown();
    let code = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(f)) = a.ws.next().await {
            if let WsMsg::Close(Some(c)) = f {
                return u16::from(c.code);
            }
        }
        0
    })
    .await
    .unwrap();
    assert_eq!(code, 1012);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("serve returns")
        .unwrap()
        .unwrap();
    let log = std::fs::read_to_string(hub.dir.join("bus.jsonl")).unwrap();
    assert!(log.ends_with("\"body\":\"before\",\"refs\":[]}\n"), "{log}");
}

#[tokio::test]
async fn published_policy_is_announced_and_checks_claims() {
    use sommelier::policy::{Change, DEFAULT_POLICY, STALE, Store};

    let bus = start(|cfg| cfg.token_reload_every = Duration::from_millis(20)).await;
    let mut subscriber = Raw::with_token(&bus, "watcher", "kinds=POLICY").await;
    let initial = client::fetch_policy(&bus.opts("alice")).await.unwrap();
    assert_eq!(initial.policy.version, 1);
    let mut bad_auth = bus.opts("alice");
    bad_auth.token.clear();
    assert!(
        client::fetch_policy(&bad_auth)
            .await
            .unwrap_err()
            .contains("401")
    );

    let path = bus.dir.join("candidate.toml");
    let publish = |version, grace| {
        let source = DEFAULT_POLICY
            .replace("version = 1", &format!("version = {version}"))
            .replace("grace_min = 15", &format!("grace_min = {grace}"))
            .replace("\"off\"", "\"enforce\"");
        std::fs::write(&path, source).unwrap();
        Store::publish(&bus.dir, &bus.tokens["orch"], Change::Publish(&path)).unwrap()
    };

    let next_policy = publish(2, 15);
    let msg: Message = serde_json::from_value(subscriber.next().await).unwrap();
    assert_eq!(msg.kind, Kind::Policy);
    assert_eq!(msg.role, Role::Hub);
    let body: Value = serde_json::from_str(&msg.body).unwrap();
    assert_eq!(body, json!({"version": 2, "hash": next_policy.hash}));

    let claim = json!({"kind": "CLAIM", "topic": "queue/1", "policy_hash": initial.hash});
    let reply = bus.send("alice", claim.clone()).await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["warning"], STALE);
    let next_policy = publish(3, 0);
    let msg: Message = serde_json::from_value(subscriber.next().await).unwrap();
    assert_eq!(msg.kind, Kind::Policy);
    assert_eq!(
        client::fetch_policy(&bus.opts("alice")).await.unwrap().hash,
        next_policy.hash
    );
    let reply = bus.send("alice", claim.clone()).await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], STALE);
    let mut claim = claim;
    claim["policy_hash"] = json!(next_policy.hash);
    assert_eq!(bus.send("alice", claim).await["ok"], true);
    assert_eq!(
        bus.send("alice", json!({"kind": "CLAIM", "topic": "queue/2"}))
            .await["error"],
        STALE
    );
    assert_eq!(
        bus.send("orch", json!({"kind": "POLICY"})).await["ok"],
        false
    );
    assert_eq!(
        bus.send("alice", json!({"kind": "CHAT", "body": "hello"}))
            .await["ok"],
        true
    );
}

#[tokio::test]
async fn idle_alert_once_not_repeated_within_10_min_and_closed_when_working() {
    let dir = fresh_dir();
    let tokens = write_tokens(&dir);
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    cfg.idle_alert_after = Duration::from_secs(0);
    let h = hub::Hub::open(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let h_serve = h.clone();
    let task = tokio::spawn(async move {
        hub::serve(h_serve, listener).await.unwrap();
    });
    let hub = TestHub {
        url,
        dir,
        tokens,
        task,
    };

    let mut coord = Raw::with_token(&hub, "orch", "kinds=ALERT&to=me").await;
    // Alice declares 1 codex slot and is online.
    let _alice = Raw::with_token(&hub, "alice", "").await;
    let r = hub
        .send(
            "alice",
            send_req("HELLO", "", "os=windows class=codex:1 mode=auto", &[]),
        )
        .await;
    assert_eq!(r["ok"], true, "{r}");

    // First tick: emits one IDLE alert for alice/codex/1 addressed to orch.
    h.sweep();
    let alert1 = coord.next().await;
    assert_eq!(alert1["kind"], "ALERT");
    assert_eq!(alert1["topic"], "alert/IDLE/alice/codex/1");
    assert_eq!(alert1["to"], json!(["orch"]));
    assert_eq!(alert1["refs"], json!(["state:open"]));
    let body1: Value = serde_json::from_str(alert1["body"].as_str().unwrap()).unwrap();
    assert_eq!(body1["code"], "IDLE");
    assert_eq!(body1["state"], "open");
    assert_eq!(body1["count"], 1);

    // Immediate second tick (within 10 minutes): no repeat alert.
    h.sweep();
    coord.quiet().await;

    // Alice's slot starts working.
    let r = hub
        .send(
            "alice",
            send_req(
                "STATUS",
                "agent/w1",
                "building shaders",
                &["state:running", "model:codex", "branch:codex/fix"],
            ),
        )
        .await;
    assert_eq!(r["ok"], true, "{r}");

    // Next tick: closes the IDLE alert.
    h.sweep();
    let alert_closed = coord.next().await;
    assert_eq!(alert_closed["kind"], "ALERT");
    assert_eq!(alert_closed["topic"], "alert/IDLE/alice/codex/1");
    assert_eq!(alert_closed["refs"], json!(["state:closed"]));
    let body_closed: Value = serde_json::from_str(alert_closed["body"].as_str().unwrap()).unwrap();
    assert_eq!(body_closed["state"], "closed");
}

#[tokio::test]
async fn brief_http_put_and_get_and_module_op_forwarding() {
    let hub = start(|_| {}).await;
    let brief_body = "# Task t42\nFix views and run tests.\n";
    let sha = sommelier::brief::sha256_hex(brief_body.as_bytes());

    // 1. Member cannot PUT /api/brief/<id> (403)
    let put_alice = format!(
        "PUT /api/brief/t42 HTTP/1.1\r\nHost: bus.example.org\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{}",
        hub.tokens["alice"],
        brief_body.len(),
        brief_body
    );
    let (head, _) = http(&hub, &put_alice).await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");

    // 2. Orchestrator can PUT /api/brief/<id> (200)
    let put_orch = format!(
        "PUT /api/brief/t42?sha256={sha} HTTP/1.1\r\nHost: bus.example.org\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{}",
        hub.tokens["orch"],
        brief_body.len(),
        brief_body
    );
    let (head, body) = http(&hub, &put_orch).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    let put_res: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(put_res["ok"], true);
    assert_eq!(put_res["id"], "t42");
    assert_eq!(put_res["sha256"], sha);

    // 3. Member can GET /api/brief/<id> by id or by sha256 (200)
    let (head, body) = http(
        &hub,
        &get(
            "/api/brief/t42",
            &format!("Authorization: Bearer {}\r\n", hub.tokens["alice"]),
        ),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, brief_body);

    let (head, body) = http(
        &hub,
        &get(
            &format!("/api/brief/{sha}"),
            &format!("Authorization: Bearer {}\r\n", hub.tokens["alice"]),
        ),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, brief_body);

    // 4. Observer cannot GET /api/brief/<id> (403)
    let (head, _) = http(
        &hub,
        &get(
            &format!("/api/brief/t42?token={}", hub.tokens["watcher"]),
            "",
        ),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");

    // 5. ModuleOp forwarding through hooks::on_op (e.g. quota.report)
    let mut sub = Raw::with_token(&hub, "watcher", "kinds=FACT").await;
    let rep = hub
        .send(
            "alice",
            json!({
                "op": "quota.report",
                "owner": "alice",
                "samples": [{
                    "owner": "alice",
                    "vendor": "codex",
                    "pool": "codex.5h",
                    "window": "5h",
                    "observed_at": 1795605600,
                    "used_pct": 33.0,
                    "resets_at": 1795618000,
                    "source": "api",
                    "confidence": 0.9
                }]
            }),
        )
        .await;
    assert_eq!(rep["ok"], true, "{rep}");
    assert_eq!(rep["applied"], 1);
    let fact = sub.next().await;
    assert_eq!(fact["kind"], "FACT");
    assert_eq!(fact["topic"], "quota/alice/codex.5h/5h");
}
