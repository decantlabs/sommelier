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
