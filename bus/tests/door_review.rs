//! Fixes of the second review at the front door: the request head, plain HTTP connections, uploads,
//! the replay on connect, the public view, the join limit and the key sign in. Real hub on a free
//! local port, no network beyond localhost.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sommelier::hub::{self, Hub, HubConfig};
use sommelier::key::JoinRules;
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct TestHub {
    addr: std::net::SocketAddr,
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
    orch: String,
    alice: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(200);
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    tweak(&mut cfg);
    let hub = Hub::open(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = hub.clone();
    let task = tokio::spawn(async move {
        hub::serve(h, listener).await.unwrap();
    });
    TestHub {
        addr,
        dir,
        hub,
        orch,
        alice,
        task,
    }
}

/// True when the peer closed the connection within `within`.
async fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 256];
    matches!(
        tokio::time::timeout(within, s.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

async fn raw_status(addr: std::net::SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed_after_the_head_timeout() {
    let hub = start(|c| c.door.head_timeout_secs = 1).await;
    let t0 = Instant::now();
    let mut silent = TcpStream::connect(hub.addr).await.unwrap();
    assert!(closed_within(&mut silent, Duration::from_secs(4)).await);
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
    // A half request line is cut off at the same deadline, one that trickles does not extend it.
    let mut partial = TcpStream::connect(hub.addr).await.unwrap();
    partial.write_all(b"GET /api/sta").await.unwrap();
    let t1 = Instant::now();
    let mut buf = [0u8; 16];
    for _ in 0..4 {
        let _ = partial.write_all(b"t").await;
        if tokio::time::timeout(Duration::from_millis(400), partial.read(&mut buf))
            .await
            .is_ok()
        {
            break;
        }
    }
    assert!(t1.elapsed() < Duration::from_secs(3), "{:?}", t1.elapsed());
}

#[tokio::test]
async fn plain_http_connections_are_counted_in_total() {
    let hub = start(|c| {
        c.door.max_http_conns = 3;
        c.door.head_timeout_secs = 2;
    })
    .await;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(TcpStream::connect(hub.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut fourth = TcpStream::connect(hub.addr).await.unwrap();
    assert!(
        closed_within(&mut fourth, Duration::from_millis(1500)).await,
        "the fourth connection is refused while three are held"
    );
    // The held ones time out, then a real request is answered again.
    for h in &mut held {
        assert!(closed_within(h, Duration::from_secs(5)).await);
    }
    let answer = raw_status(
        hub.addr,
        "GET /api/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
}

#[tokio::test]
async fn plain_http_connections_are_counted_per_address_block() {
    let hub = start(|c| {
        c.door.trust_header = Some("CF-Connecting-IP".into());
        c.door.http_conns_per_block = 2;
    })
    .await;
    // Two connections that sent a head and hold the body back occupy their block.
    let head = |ip: &str| {
        format!(
            "PUT /api/join HTTP/1.1\r\nHost: x\r\nCF-Connecting-IP: {ip}\r\nContent-Length: 10\r\n\r\n"
        )
    };
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut s = TcpStream::connect(hub.addr).await.unwrap();
        s.write_all(head("203.0.113.5").as_bytes()).await.unwrap();
        held.push(s);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = raw_status(hub.addr, &format!("{}0123456789", head("203.0.113.9"))).await;
    assert!(refused.starts_with("HTTP/1.1 429"), "{refused}");
    // Another block is not affected.
    let other = raw_status(hub.addr, &format!("{}0123456789", head("198.51.100.4"))).await;
    assert!(!other.starts_with("HTTP/1.1 429"), "{other}");
}

#[tokio::test]
async fn an_upload_is_refused_from_its_head_before_any_body_is_read() {
    let hub = start(|c| c.intake.enabled = true).await;
    let big = 4 * 1024 * 1024;
    // No credentials: refused at once, the body never arrives.
    let t0 = Instant::now();
    let a = raw_status(
        hub.addr,
        &format!(
            "PUT /api/submit/x/1?sha256={} HTTP/1.1\r\nHost: x\r\nContent-Length: {big}\r\n\r\n",
            "a".repeat(64)
        ),
    )
    .await;
    assert!(a.starts_with("HTTP/1.1 401"), "{a}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    // A token member that holds no lease: refused as well.
    let b = raw_status(
        hub.addr,
        &format!(
            "PUT /api/submit/x/1?sha256={} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: {big}\r\n\r\n",
            "a".repeat(64),
            hub.alice
        ),
    )
    .await;
    assert!(b.starts_with("HTTP/1.1 403"), "{b}");
    assert!(b.contains("no active lease"), "{b}");
    let _ = (&hub.orch, &hub.dir, &hub.hub);
}

#[tokio::test]
async fn upload_bytes_in_flight_are_limited() {
    let door = sommelier::door::Door::open(
        sommelier::door::DoorConfig {
            max_upload_bytes_in_flight: 1000,
            ..Default::default()
        },
        [1u8; 32],
        std::path::Path::new("."),
    );
    let a = door.reserve_upload(600).unwrap();
    assert!(door.reserve_upload(600).is_err());
    let b = door.reserve_upload(400).unwrap();
    drop(a);
    let c = door.reserve_upload(600).unwrap();
    drop((b, c));
    assert!(door.reserve_upload(1000).is_ok());
}

const HEADER: &str = "CF-Connecting-IP";

async fn post_join_from(hub: &TestHub, body: &str, addr: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("http://{}/api/join", hub.addr))
        .header(HEADER, addr)
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn fresh_nonce(hub: &TestHub) -> String {
    let bytes = reqwest::get(format!("http://{}/api/challenge", hub.addr))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["nonce"].as_str().unwrap().to_owned()
}

fn signed_join(key: &sommelier::key::crypto::MemberKey, nonce: &str) -> String {
    use sommelier::key::crypto::{sign_request, solve_pow};
    let counter = solve_pow(nonce, &key.public(), 8);
    let mut body = serde_json::json!({"pub": key.public_b64(), "handle": "h",
                                      "nonce": nonce, "counter": counter});
    body["sig"] = serde_json::json!(sign_request(key, &body));
    body.to_string()
}

#[tokio::test]
async fn garbage_posts_do_not_use_up_the_join_limit_of_a_block() {
    let hub = start(|c| {
        c.door.trust_header = Some(HEADER.into());
        c.door.joins_per_block_per_hour = 2;
        c.door.bad_joins_per_block_per_hour = 8;
    })
    .await;
    let addr = "203.0.113.50";
    for _ in 0..6 {
        let (code, _) = post_join_from(&hub, "not json", addr).await;
        assert_eq!(code, 400);
    }
    // A valid join of the same block still goes through, twice, then the join limit applies.
    for i in 0..2 {
        let key = sommelier::key::crypto::MemberKey::generate().unwrap();
        let nonce = fresh_nonce(&hub).await;
        let (code, v) = post_join_from(&hub, &signed_join(&key, &nonce), addr).await;
        assert_eq!(code, 200, "join {i}: {v}");
    }
    let key = sommelier::key::crypto::MemberKey::generate().unwrap();
    let nonce = fresh_nonce(&hub).await;
    let (code, _) = post_join_from(&hub, &signed_join(&key, &nonce), addr).await;
    assert_eq!(code, 429);
    // Garbage has its own, cheap limit: after 8 failures the block is told to wait.
    let other = "203.0.113.99";
    let mut last = 0;
    for _ in 0..10 {
        last = post_join_from(&hub, "not json", other).await.0;
    }
    assert_eq!(last, 429);
}

// Replay, the public view and the connection cap

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::key::client as kc;
use sommelier::msg::{Kind, Message};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn url(hub: &TestHub) -> String {
    format!("ws://{}", hub.addr)
}

fn opts(hub: &TestHub, token: &str) -> Opts {
    Opts {
        url: url(hub),
        token: token.to_owned(),
    }
}

async fn ws_open(hub: &TestHub, query: &str, bearer: Option<&str>) -> Result<client::Ws, u16> {
    let mut req = format!("{}/?{query}", url(hub))
        .into_client_request()
        .unwrap();
    if let Some(a) = bearer {
        req.headers_mut()
            .insert("authorization", format!("Bearer {a}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("{e}"),
    }
}

/// Reads texts until the stream is quiet for `quiet`.
async fn drain_texts(ws: &mut client::Ws, quiet: Duration) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(m))) = tokio::time::timeout(quiet, ws.next()).await {
        if let WsMsg::Text(t) = m {
            out.push(t.as_str().to_owned());
        }
    }
    out
}

fn chat_line(id: u64) -> String {
    serde_json::to_string(&Message {
        id,
        ts: "2026-01-01T00:00:00.000Z".into(),
        from: "alice".into(),
        role: Role::Member,
        kind: Kind::Chat,
        topic: "talk".into(),
        body: format!("line {id}"),
        refs: vec![],
        to: vec![],
    })
    .unwrap()
}

async fn start_with_log(lines: u64, tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (1..=lines).map(|i| chat_line(i) + "\n").collect();
    std::fs::write(dir.path().join("bus.jsonl"), text).unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.add("watch", Role::Observer).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    tweak(&mut cfg);
    let hub = Hub::open(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = hub.clone();
    let task = tokio::spawn(async move {
        hub::serve(h, listener).await.unwrap();
    });
    TestHub {
        addr,
        dir,
        hub,
        orch,
        alice,
        task,
    }
}

#[tokio::test]
async fn a_replay_comes_in_pages_and_in_order() {
    let hub = start_with_log(2600, |c| c.door.public_read = true).await;
    // `since` replays everything from the first line, in order, in pages of at most 1000 lines.
    let mut ws = ws_open(&hub, "since=1&live=0", Some(&hub.alice))
        .await
        .unwrap();
    let all = drain_texts(&mut ws, Duration::from_secs(2)).await;
    let ids: Vec<u64> = all
        .iter()
        .filter_map(|t| serde_json::from_str::<Message>(t).ok())
        .map(|m| m.id)
        .collect();
    assert!(ids.len() >= 2600, "{}", ids.len());
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    // `last` is cut at one page, however large the number.
    let mut ws = ws_open(&hub, "last=999999999&live=0", None).await.unwrap();
    let got = drain_texts(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(got.len(), 1000, "{}", got.len());
    assert!(got.last().unwrap().contains("line 2600"));
}

#[tokio::test]
async fn a_client_that_does_not_read_cannot_make_the_hub_build_the_log_in_memory() {
    let hub = start_with_log(5000, |c| c.door.max_conns = 0).await;
    // Many connections ask for everything and never read. The hub stays answerable.
    let mut held = Vec::new();
    for _ in 0..20 {
        held.push(ws_open(&hub, "since=1", Some(&hub.alice)).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let v = client::request_ok(&opts(&hub, &hub.orch), json!({"op": "whoami"}))
        .await
        .unwrap();
    assert_eq!(v["name"], "orch");
}

#[tokio::test]
async fn observer_tokens_are_never_counted_as_low() {
    let hub = start_with_log(0, |c| {
        c.door.public_read = true;
        c.door.max_conns = 4;
        c.door.reserved_share = 0.5;
        c.door.public_reads_per_block_per_min = 0;
    })
    .await;
    // Anonymous readers fill the low part: two of four.
    let _a = ws_open(&hub, "live=1", None).await.unwrap();
    let _b = ws_open(&hub, "live=1", None).await.unwrap();
    assert_eq!(ws_open(&hub, "live=1", None).await.err(), Some(429));
    // Tokens of every role still get in, there are 2 places kept for them.
    let _m1 = ws_open(&hub, "live=1", Some(&hub.alice)).await.unwrap();
    let _m2 = ws_open(&hub, "live=1", Some(&hub.orch)).await.unwrap();
    assert!(!sommelier::door::is_low(
        &hub.hub,
        "watch",
        Some(Role::Observer)
    ));
    assert!(!sommelier::door::is_low(
        &hub.hub,
        "alice",
        Some(Role::Member)
    ));
    assert!(sommelier::door::is_low(&hub.hub, "m0123", None));
}

fn msg(kind: Kind, topic: &str, body: &str) -> Message {
    Message {
        id: 5,
        ts: String::new(),
        from: "hub".into(),
        role: Role::Hub,
        kind,
        topic: topic.into(),
        body: body.into(),
        refs: vec![],
        to: vec![],
    }
}

#[test]
fn the_public_view_is_an_allow_list() {
    use sommelier::door::{public_line, public_message};
    // FACTs of other topics than the allowed ones are hidden, also new ones nobody listed.
    for topic in [
        "quota/o/p/5h",
        "spend/o/2026-11",
        "estimate/x",
        "capacity/o/m",
        "pool/p",
        "owner/o",
        "something/new",
    ] {
        assert!(
            public_message(&msg(Kind::Fact, topic, "{}")).is_none(),
            "{topic}"
        );
    }
    for topic in [
        "key/m1",
        "submission/abc",
        "report/r1",
        "stamp/m1",
        "gate/x",
    ] {
        assert!(
            public_message(&msg(Kind::Fact, topic, "{}")).is_some(),
            "{topic}"
        );
    }
    // The budget alert never reaches the public view, in no form.
    let budget = json!({"v": 1, "code": "COORD_BUDGET", "key": "o.p", "text": "pool p of o is at 91.5% used",
        "data": {"owner": "o", "pool": "p", "used_pct": 91.5}})
    .to_string();
    assert!(public_message(&msg(Kind::Alert, "alert/COORD_BUDGET/o.p", &budget)).is_none());
    assert!(public_message(&msg(Kind::Alert, "alert/UNKNOWN_CODE/k", "{}")).is_none());
    assert!(public_message(&msg(Kind::Alert, "alert/IDLE/s", "not json")).is_none());
    // An allowed alert loses percent values and meter keys.
    let idle = json!({"v": 1, "code": "IDLE", "key": "s", "text": "slot idle, pool at 40% of 5h",
        "data": {"slot": "s", "used_pct": 40.0, "quota": {"x": 1}}})
    .to_string();
    let m = msg(Kind::Alert, "alert/IDLE/s", &idle);
    let shown = public_message(&m).unwrap();
    assert!(
        !shown.body.contains("used_pct") && !shown.body.contains("40%"),
        "{}",
        shown.body
    );
    assert!(shown.body.contains("slot idle"));
    let line = public_line(&m, "RAW").unwrap();
    assert_ne!(line, "RAW");
    assert_eq!(
        public_line(&msg(Kind::Chat, "t", "hi"), "RAW").as_deref(),
        Some("RAW")
    );
}

#[test]
fn the_public_state_keeps_only_listed_keys() {
    let mut state = json!({
        "hub": {"id": 1}, "as_of": 1, "members": [], "tasks": [],
        "quota": {"spend": [1]}, "store": {"alerts": [{"code": "COORD_BUDGET"}]},
        "capacities": [], "machines": [], "future_key": {"x": 1},
        "alerts": [{"code": "COORD_BUDGET", "text": "91%"}, {"code": "IDLE", "text": "x 3%", "used_pct": 3}],
        "derived": {"alerts": [{"code": "COORD_BUDGET"}], "workers": []},
        "recent": [msg(Kind::Alert, "alert/COORD_BUDGET/k", "{}"), msg(Kind::Chat, "t", "hi")],
    });
    sommelier::door::redact_state(&mut state);
    let keys: Vec<&String> = state.as_object().unwrap().keys().collect();
    for gone in ["quota", "store", "capacities", "machines", "future_key"] {
        assert!(state.get(gone).is_none(), "{gone} in {keys:?}");
    }
    assert!(state["tasks"].is_array());
    assert_eq!(state["alerts"].as_array().unwrap().len(), 1);
    assert!(!state["alerts"].to_string().contains("used_pct"));
    assert!(!state["alerts"].to_string().contains("3%"));
    assert_eq!(state["derived"]["alerts"].as_array().unwrap().len(), 0);
    assert_eq!(state["recent"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_key_below_tier_2_gets_the_public_view_everywhere() {
    let hub = start_with_log(0, |c| c.door.public_read = true).await;
    let orch = opts(&hub, &hub.orch);
    let quota = json!({"owner": "orch", "pool": "claude.5h", "vendor": "claude", "window": "5h",
        "used_pct": 42.5, "resets_at": 4_000_000_000u64})
    .to_string();
    for (kind, topic, body) in [
        ("FACT", "quota/orch/claude.5h/5h", quota.as_str()),
        ("CHAT", "talk", "hello all"),
    ] {
        let v = client::request(
            &orch,
            json!({"op": "send", "kind": kind, "topic": topic, "body": body}),
        )
        .await
        .unwrap();
        assert_eq!(v["ok"], true, "{v}");
    }
    let dir = tempfile::tempdir().unwrap();
    kc::join(&url(&hub), dir.path(), None).await.unwrap();
    let key = kc::opts_for(&url(&hub), dir.path());
    // The `log` request.
    let v = client::request_ok(&key, json!({"op": "log", "last": 50}))
        .await
        .unwrap();
    let text = v.to_string();
    assert!(text.contains("hello all"), "{text}");
    assert!(
        !text.contains("quota/orch") && !text.contains("42.5"),
        "{text}"
    );
    let v = client::request_ok(&orch, json!({"op": "log", "last": 50}))
        .await
        .unwrap();
    assert!(v.to_string().contains("quota/orch"));
    // The stream with a replay.
    let mut ws = client::connect(
        &key,
        &sommelier::proto::ConnectParams {
            last: Some(50),
            live: false,
            ..Default::default()
        },
    )
    .await
    .ok()
    .unwrap();
    let seen = drain_texts(&mut ws, Duration::from_millis(700))
        .await
        .join("\n");
    assert!(
        seen.contains("hello all") && !seen.contains("quota/orch"),
        "{seen}"
    );
    // /api/state.
    let header = kc::auth_header(&url(&hub), &kc::marker_for(&kc::key_path(dir.path())))
        .await
        .ok()
        .unwrap();
    let resp = reqwest::Client::new()
        .get(format!("http://{}/api/state", hub.addr))
        .header("authorization", header)
        .send()
        .await
        .unwrap();
    let state = resp.text().await.unwrap();
    assert!(
        !state.contains("used_pct") && !state.contains("\"quota\""),
        "{state}"
    );
    // The token of a member still sees the meters.
    let resp = reqwest::Client::new()
        .get(format!("http://{}/api/state", hub.addr))
        .header("authorization", format!("Bearer {}", hub.orch))
        .send()
        .await
        .unwrap();
    assert!(resp.text().await.unwrap().contains("used_pct"));
    let _ = ws.close(None).await;
    let _: Option<Value> = None;
    let _ = WsMsg::Close(None);
    let _ = ws.send(WsMsg::Close(None)).await;
}

#[tokio::test]
async fn the_block_secret_can_live_in_a_file_outside_the_data_folder() {
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("door-secret");
    let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let f = file.clone();
    let a = start_with_log(0, move |c| c.door.secret_file = Some(f)).await;
    assert!(file.is_file(), "made at the first start");
    let block_a = a.hub.door().block(ip);
    // Another data folder with the same file gives the same blocks, another file gives other ones.
    let f = file.clone();
    let b = start_with_log(0, move |c| c.door.secret_file = Some(f)).await;
    assert_eq!(b.hub.door().block(ip), block_a);
    let other = outside.path().join("other-secret");
    let o = other.clone();
    let c = start_with_log(0, move |c| c.door.secret_file = Some(o)).await;
    assert_ne!(c.hub.door().block(ip), block_a);
    // The data folder holds no copy of that secret.
    let secret = std::fs::read_to_string(&file).unwrap();
    let challenge = std::fs::read_to_string(a.dir.path().join("challenge_secret")).unwrap();
    assert_ne!(secret.trim(), challenge.trim());
    // A file that holds something else stops the start, it is not replaced.
    std::fs::write(&other, "not a secret").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.door.secret_file = Some(other.clone());
    assert!(Hub::open(cfg).is_err());
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "not a secret");
}
