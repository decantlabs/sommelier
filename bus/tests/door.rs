//! The front door: address blocks, limits and public read, against a real hub on a free local port.
//! Client addresses are simulated with a trusted header from the loopback peer. The addresses below
//! are from the documentation ranges. No network beyond localhost.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::door::{block_id, block_of, client_ip, hidden_from_public, redact_state};
use sommelier::hub::{self, Hub, HubConfig};
use sommelier::key::JoinRules;
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, sign_request, solve_pow};
use sommelier::msg::{Kind, Message, Role};
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BITS: u32 = 8;
const HEADER: &str = "CF-Connecting-IP";
const ADDR_A: &str = "203.0.113.77";
const ADDR_A2: &str = "203.0.113.200";
const ADDR_B: &str = "198.51.100.9";
const ADDR_V6: &str = "2001:db8:abcd:1::5";

struct TestHub {
    url: String,
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

impl TestHub {
    fn orch(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.orch.clone(),
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("bus.jsonl")).unwrap_or_default()
    }

    fn messages(&self) -> Vec<Message> {
        self.log_text()
            .lines()
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect()
    }

    fn host(&self) -> String {
        self.url.trim_start_matches("ws://").to_owned()
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
        proof_of_work_bits: BITS,
    };
    tweak(&mut cfg);
    let hub = Hub::open(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let h = hub.clone();
    let task = tokio::spawn(async move {
        hub::serve(h, listener).await.unwrap();
    });
    TestHub {
        url,
        dir,
        hub,
        orch,
        alice,
        task,
    }
}

fn with_door(c: &mut HubConfig) {
    c.door.trust_header = Some(HEADER.into());
    c.door.keep_days = 7;
}

async fn http_get(
    hub: &TestHub,
    path: &str,
    auth: Option<&str>,
    addr: Option<&str>,
) -> (u16, String) {
    let mut r = reqwest::Client::new().get(format!("http://{}{path}", hub.host()));
    if let Some(a) = auth {
        r = r.header("authorization", a);
    }
    if let Some(a) = addr {
        r = r.header(HEADER, a);
    }
    let resp = r.send().await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

async fn post_join(hub: &TestHub, body: &Value, addr: &str) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("http://{}/api/join", hub.host()))
        .header(HEADER, addr)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn join_body(key: &MemberKey, nonce: &str, handle: &str) -> Value {
    let counter = solve_pow(nonce, &key.public(), BITS);
    let mut body =
        json!({"pub": key.public_b64(), "handle": handle, "nonce": nonce, "counter": counter});
    body["sig"] = json!(sign_request(key, &body));
    body
}

async fn nonce(hub: &TestHub) -> String {
    let (_, text) = http_get(hub, "/api/challenge", None, None).await;
    serde_json::from_str::<Value>(&text).unwrap()["nonce"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Joins a fresh key from `addr`, returns the member id.
async fn join_from(hub: &TestHub, addr: &str) -> (u16, Value, String) {
    let key = MemberKey::generate().unwrap();
    let n = nonce(hub).await;
    let (code, v) = post_join(hub, &join_body(&key, &n, "visitor"), addr).await;
    let member = v["member"].as_str().unwrap_or_default().to_owned();
    (code, v, member)
}

/// Opens a WebSocket. `Err` holds the status and the body of a refusal.
async fn ws_open(
    hub: &TestHub,
    query: &str,
    auth: Option<&str>,
) -> Result<client::Ws, (u16, String)> {
    let mut req = format!("{}/?{query}", hub.url)
        .into_client_request()
        .unwrap();
    if let Some(a) = auth {
        req.headers_mut()
            .insert("authorization", format!("Bearer {a}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err((
            r.status().as_u16(),
            String::from_utf8_lossy(r.body().as_deref().unwrap_or_default()).into_owned(),
        )),
        Err(e) => panic!("{e}"),
    }
}

async fn send_as(hub: &TestHub, who: &Opts, kind: &str, topic: &str, body: &str) {
    let v = client::request(
        who,
        json!({"op": "send", "kind": kind, "topic": topic, "body": body}),
    )
    .await
    .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    let _ = hub;
}

async fn next_text(ws: &mut client::Ws) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_millis(700), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => return Some(t.as_str().to_owned()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn the_block_is_stable_per_slash_24_and_slash_48() {
    let secret = [7u8; 32];
    let a = block_id(&secret, ip("203.0.113.5"));
    assert_eq!(a.len(), 16);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(a, block_id(&secret, ip("203.0.113.5")));
    assert_eq!(a, block_id(&secret, ip("203.0.113.250")), "same /24");
    assert_ne!(a, block_id(&secret, ip("203.0.114.5")), "other /24");
    assert_ne!(a, block_id(&secret, ip("198.51.100.5")));
    // The same IPv4 address as an IPv4 mapped IPv6 address is the same block.
    assert_eq!(a, block_id(&secret, ip("::ffff:203.0.113.9")));

    let v = block_id(&secret, ip("2001:db8:abcd:1::5"));
    assert_eq!(
        v,
        block_id(&secret, ip("2001:db8:abcd:ffff::9")),
        "same /48"
    );
    assert_ne!(v, block_id(&secret, ip("2001:db8:abce:1::5")), "other /48");
    assert_ne!(v, a);

    // Another hub secret gives another value.
    assert_ne!(a, block_id(&[8u8; 32], ip("203.0.113.5")));
    // The block holds no part of the address as text.
    assert!(!a.contains("203") || !a.contains("113"));
}

#[test]
fn the_header_counts_only_from_loopback() {
    let from_tunnel = ip("127.0.0.1");
    let from_tunnel6 = ip("::1");
    let stranger = ip("192.0.2.44");
    let h = Some(ADDR_A);
    assert_eq!(client_ip(from_tunnel, Some(HEADER), h), ip(ADDR_A));
    assert_eq!(client_ip(from_tunnel6, Some(HEADER), h), ip(ADDR_A));
    assert_eq!(
        client_ip(ip("::ffff:127.0.0.1"), Some(HEADER), h),
        ip(ADDR_A)
    );
    // A peer that is not loopback cannot name its own address.
    assert_eq!(client_ip(stranger, Some(HEADER), h), stranger);
    // No trusted header configured: the header is ignored also from loopback.
    assert_eq!(client_ip(from_tunnel, None, h), from_tunnel);
    // Missing or damaged header: the peer counts.
    assert_eq!(client_ip(from_tunnel, Some(HEADER), None), from_tunnel);
    assert_eq!(
        client_ip(from_tunnel, Some(HEADER), Some("not an address")),
        from_tunnel
    );
    // A proxy chain: the address the nearest proxy added counts.
    assert_eq!(
        client_ip(from_tunnel, Some(HEADER), Some("10.9.9.9, 203.0.113.77")),
        ip(ADDR_A)
    );
}

#[test]
fn the_door_is_off_by_default() {
    let c = sommelier::door::DoorConfig::default();
    assert!(c.trust_header.is_none());
    assert_eq!(c.joins_per_block_per_hour, 0);
    assert_eq!(c.conns_per_member, 0);
    assert_eq!(c.max_conns, 0);
    assert!(!c.public_read);
    assert_eq!(c.keep_days, 0);
}

#[tokio::test]
async fn joins_over_the_limit_are_refused_with_a_retry_time() {
    let hub = start(|c| {
        with_door(c);
        c.door.joins_per_block_per_hour = 2;
        c.door.join_flood_refusals = 3;
    })
    .await;
    // Two joins from one /24 pass, from two addresses of it.
    assert_eq!(join_from(&hub, ADDR_A).await.0, 200);
    assert_eq!(join_from(&hub, ADDR_A2).await.0, 200);
    // More are refused, with the limit and the time to retry.
    for _ in 0..4 {
        let (code, v, _) = join_from(&hub, ADDR_A).await;
        assert_eq!(code, 429, "{v}");
        let e = v["error"].as_str().unwrap();
        assert!(e.contains("join limit: 2 joins per hour"), "{e}");
        assert!(e.contains("try again in") && e.contains(" s"), "{e}");
    }
    // Another block still joins.
    assert_eq!(join_from(&hub, ADDR_B).await.0, 200);

    // JOIN_FLOOD is raised once, with counts and no address.
    let mut alerts = vec![];
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        alerts = hub
            .messages()
            .into_iter()
            .filter(|m| m.kind == Kind::Alert && m.topic.starts_with("alert/JOIN_FLOOD"))
            .collect();
        if !alerts.is_empty() {
            break;
        }
    }
    assert_eq!(alerts.len(), 1, "{}", hub.log_text());
    let body: Value = serde_json::from_str(&alerts[0].body).unwrap();
    assert_eq!(body["code"], "JOIN_FLOOD");
    assert!(body["count"].as_u64().unwrap() >= 3);
    for _ in 0..3 {
        let _ = join_from(&hub, ADDR_A).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let n = hub
        .messages()
        .iter()
        .filter(|m| m.kind == Kind::Alert && m.topic.starts_with("alert/JOIN_FLOOD"))
        .count();
    assert_eq!(n, 1, "one alert per window");
}

#[tokio::test]
async fn a_hub_without_limits_joins_as_before() {
    let hub = start(with_door).await;
    for _ in 0..4 {
        assert_eq!(join_from(&hub, ADDR_A).await.0, 200);
    }
}

#[tokio::test]
async fn token_members_connect_when_tier_0_and_anonymous_fill_the_hub() {
    let hub = start(|c| {
        with_door(c);
        c.door.public_read = true;
        c.door.max_conns = 6;
        c.door.reserved_share = 0.5;
        c.door.conns_per_member = 2;
        c.door.public_reads_per_block_per_min = 0;
    })
    .await;
    // Three anonymous readers fill the part that is not reserved.
    let mut readers = vec![];
    for _ in 0..3 {
        readers.push(ws_open(&hub, "live=1", None).await.ok().unwrap());
    }
    let (code, text) = ws_open(&hub, "live=1", None).await.err().unwrap();
    assert_eq!(code, 429);
    assert!(
        text.contains("connection limit") && text.contains("try again in"),
        "{text}"
    );

    // A tier 0 key member is refused for the same reason.
    let dir = tempfile::tempdir().unwrap();
    let joined = kc::join(&hub.url, dir.path(), None).await.unwrap();
    let key_opts = kc::opts_for(&hub.url, dir.path());
    let v = client::request(&key_opts, json!({"op": "whoami"})).await;
    assert!(
        v.as_ref().is_err() || v.as_ref().unwrap()["ok"] != true,
        "{v:?} for {}",
        joined.member
    );

    // Token members still connect, up to their own limit per member.
    let _a1 = ws_open(&hub, "live=1", Some(&hub.alice))
        .await
        .ok()
        .unwrap();
    let _a2 = ws_open(&hub, "live=1", Some(&hub.alice))
        .await
        .ok()
        .unwrap();
    let (code, text) = ws_open(&hub, "live=1", Some(&hub.alice))
        .await
        .err()
        .unwrap();
    assert_eq!(code, 429);
    assert!(text.contains("per member"), "{text}");
    let _o1 = ws_open(&hub, "live=1", Some(&hub.orch)).await.ok().unwrap();
    // The hub is full now: six connections.
    let (code, text) = ws_open(&hub, "live=1", Some(&hub.orch))
        .await
        .err()
        .unwrap();
    assert_eq!(code, 429);
    assert!(text.contains("connection limit"), "{text}");

    // A closed connection gives its place back.
    drop(readers.pop());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(ws_open(&hub, "live=1", Some(&hub.orch)).await.is_ok());
}

#[tokio::test]
async fn public_read_off_refuses_as_today() {
    let hub = start(|_| {}).await;
    let (code, text) = ws_open(&hub, "last=5", None).await.err().unwrap();
    assert_eq!(code, 401, "{text}");
    let (code, _) = http_get(&hub, "/api/state", None, None).await;
    assert_eq!(code, 401);
    // A wrong token is refused also when public read is on.
    let on = start(|c| c.door.public_read = true).await;
    let (code, _) = ws_open(&on, "last=5", Some("sb_wrong"))
        .await
        .err()
        .unwrap();
    assert_eq!(code, 401);
}

#[tokio::test]
async fn public_read_is_read_only_and_without_meters() {
    let hub = start(|c| c.door.public_read = true).await;
    let reset = 4_000_000_000u64;
    let quota = json!({"owner": "orch", "pool": "claude.5h", "vendor": "claude", "window": "5h",
        "used_pct": 42.5, "resets_at": reset})
    .to_string();
    send_as(&hub, &hub.orch(), "FACT", "quota/orch/claude.5h/5h", &quota).await;
    send_as(
        &hub,
        &hub.orch(),
        "FACT",
        "spend/orch/2026-11",
        "{\"usd\": 12}",
    )
    .await;
    send_as(&hub, &hub.orch(), "CHAT", "talk", "hello everybody").await;

    // The backlog and the live messages, without the meters.
    let mut ws = ws_open(&hub, "last=50&live=1", None).await.ok().unwrap();
    let mut seen = vec![];
    while let Some(t) = next_text(&mut ws).await {
        seen.push(t);
    }
    assert!(
        seen.iter().any(|t| t.contains("hello everybody")),
        "{seen:?}"
    );
    send_as(&hub, &hub.orch(), "FACT", "quota/orch/claude.5h/5h", &quota).await;
    send_as(&hub, &hub.orch(), "CHAT", "talk", "second line").await;
    while let Some(t) = next_text(&mut ws).await {
        seen.push(t);
    }
    assert!(seen.iter().any(|t| t.contains("second line")), "{seen:?}");
    for t in &seen {
        assert!(!t.contains("quota/") && !t.contains("spend/"), "{t}");
    }

    // It can send nothing.
    ws.send(WsMsg::text(
        json!({"op": "send", "kind": "CHAT", "topic": "talk", "body": "spam"}).to_string(),
    ))
    .await
    .unwrap();
    let reply: Value = serde_json::from_str(&next_text(&mut ws).await.unwrap()).unwrap();
    assert_eq!(reply["ok"], false, "{reply}");
    assert!(!hub.log_text().contains("spam"));

    // It does not show as a connection of the hub.
    let who = client::request_ok(&hub.orch(), json!({"op": "who"}))
        .await
        .unwrap();
    assert!(
        who["connections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "public"),
        "{who}"
    );

    // /api/state: pools show their state and reset time only.
    let (code, text) = http_get(&hub, "/api/state", None, None).await;
    assert_eq!(code, 200, "{text}");
    let state: Value = serde_json::from_str(&text).unwrap();
    let pools = state["pools"].as_array().unwrap();
    assert_eq!(pools.len(), 1, "{state}");
    let pool = pools[0].as_object().unwrap();
    assert_eq!(pool["pool"], "claude.5h");
    assert!(pool.contains_key("state") && pool["resets_at"] == reset);
    assert!(pool.len() == 4, "{pool:?}");
    assert!(state.get("quota").is_none());
    for needle in ["used_pct", "42.5", "spend/", "quota/", "burn_pct"] {
        assert!(!text.contains(needle), "{needle} in {text}");
    }

    // A member with a token sees what it saw before.
    let (code, text) = http_get(
        &hub,
        "/api/state",
        Some(&format!("Bearer {}", hub.alice)),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(
        text.contains("used_pct") && text.contains("quota/orch"),
        "{text}"
    );
}

#[tokio::test]
async fn the_public_reader_has_a_rate_limit_per_block() {
    let hub = start(|c| {
        with_door(c);
        c.door.public_read = true;
        c.door.public_reads_per_block_per_min = 3;
    })
    .await;
    for _ in 0..3 {
        let (code, _) = http_get(&hub, "/api/state", None, Some(ADDR_A)).await;
        assert_eq!(code, 200);
    }
    let (code, text) = http_get(&hub, "/api/state", None, Some(ADDR_A2)).await;
    assert_eq!(code, 429, "same block, other address: {text}");
    assert!(
        text.contains("public read limit") && text.contains("try again in"),
        "{text}"
    );
    let (code, _) = http_get(&hub, "/api/state", None, Some(ADDR_B)).await;
    assert_eq!(code, 200, "another block");
    // A token member is not limited by it.
    for _ in 0..5 {
        let (code, _) = http_get(
            &hub,
            "/api/state",
            Some(&format!("Bearer {}", hub.alice)),
            Some(ADDR_A),
        )
        .await;
        assert_eq!(code, 200);
    }
}

#[tokio::test]
async fn block_of_answers_for_members_that_connected() {
    let hub = start(with_door).await;
    let (_, _, m1) = join_from(&hub, ADDR_A).await;
    let (_, _, m2) = join_from(&hub, ADDR_A2).await;
    let (_, _, m3) = join_from(&hub, ADDR_B).await;
    let b1 = block_of(&hub.hub, &m1).unwrap();
    assert_eq!(b1.len(), 16);
    assert_eq!(Some(b1.clone()), block_of(&hub.hub, &m2), "one owner");
    assert_ne!(Some(b1.clone()), block_of(&hub.hub, &m3));
    assert_eq!(b1, hub.hub.door().block(ip(ADDR_A)));
    assert_eq!(block_of(&hub.hub, "mnobody"), None);

    // A connection notes the block too: a key member signing in over loopback without a header.
    let dir = tempfile::tempdir().unwrap();
    let out = kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    let who = client::request_ok(&opts, json!({"op": "whoami"}))
        .await
        .unwrap();
    assert_eq!(who["name"], out.member.as_str());
    assert_eq!(
        block_of(&hub.hub, &out.member),
        Some(hub.hub.door().block(ip("127.0.0.1")))
    );
    // A token member too.
    client::request_ok(&hub.orch(), json!({"op": "whoami"}))
        .await
        .unwrap();
    assert!(block_of(&hub.hub, "orch").is_some());
}

#[tokio::test]
async fn without_keep_days_no_block_is_kept() {
    let hub = start(|c| c.door.trust_header = Some(HEADER.into())).await;
    let (_, _, m) = join_from(&hub, ADDR_A).await;
    assert_eq!(block_of(&hub.hub, &m), None);
    assert!(!hub.dir.path().join("door.db").exists());
}

fn contains_address(text: &str) -> Option<&'static str> {
    [
        ADDR_A,
        ADDR_A2,
        ADDR_B,
        ADDR_V6,
        "203.0.113",
        "198.51.100",
        "2001:db8",
        "2001db8",
        "127.0.0.1",
    ]
    .into_iter()
    .find(|a| text.contains(a))
}

fn file_text(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
}

#[tokio::test]
async fn no_address_is_stored_logged_or_served() {
    let hub = start(|c| {
        with_door(c);
        c.door.joins_per_block_per_hour = 1;
        c.door.public_read = true;
        c.door.max_conns = 3;
        c.door.reserved_share = 0.34;
    })
    .await;
    let mut refusals = vec![];
    for addr in [ADDR_A, ADDR_A2, ADDR_B, ADDR_V6, ADDR_V6] {
        let (code, v, _) = join_from(&hub, addr).await;
        if code != 200 {
            refusals.push(v.to_string());
        }
    }
    assert!(!refusals.is_empty());
    // Connections from several addresses, a refused one among them.
    let mut held = vec![];
    for _ in 0..3 {
        match ws_open(&hub, "live=1", None).await {
            Ok(w) => held.push(w),
            Err((_, text)) => refusals.push(text),
        }
    }
    assert!(held.len() < 3, "the cap refuses the last reader");
    let _ = ws_open(&hub, "live=1", Some(&hub.alice)).await.ok();
    let (_, state_public) = http_get(&hub, "/api/state", None, Some(ADDR_V6)).await;
    let (_, state_member) = http_get(
        &hub,
        "/api/state",
        Some(&format!("Bearer {}", hub.alice)),
        Some(ADDR_B),
    )
    .await;
    let (_, rate_text) = http_get(&hub, "/api/policy", None, Some(ADDR_A)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut texts: Vec<(&str, String)> = vec![
        ("log", hub.log_text()),
        ("public state", state_public),
        ("member state", state_member),
        ("refusals", refusals.join("\n")),
        ("policy refusal", rate_text),
    ];
    for name in [
        "state.db",
        "state.db-wal",
        "state.db-shm",
        "door.db",
        "door.db-wal",
    ] {
        texts.push((name, file_text(&hub.dir.path().join(name))));
    }
    // Also the other files of the data folder.
    for entry in std::fs::read_dir(hub.dir.path()).unwrap().flatten() {
        if entry.path().is_file() && entry.file_name() != "bus.jsonl" {
            texts.push(("data file", file_text(&entry.path())));
        }
    }
    for (what, text) in &texts {
        if let Some(a) = contains_address(text) {
            panic!("{what} holds the address text {a}");
        }
    }
    // door.db holds blocks and nothing that parses as an address.
    let db = rusqlite::Connection::open(hub.dir.path().join("door.db")).unwrap();
    let rows: Vec<(String, String)> = db
        .prepare("SELECT member, block FROM SEEN")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    assert!(!rows.is_empty());
    for (member, block) in rows {
        assert!(block.parse::<IpAddr>().is_err(), "{block}");
        assert_eq!(block.len(), 16);
        assert!(!member.is_empty());
    }
}

#[tokio::test]
async fn old_rows_are_dropped_on_the_tick() {
    let hub = start(|c| {
        with_door(c);
    })
    .await;
    let (_, _, m) = join_from(&hub, ADDR_A).await;
    assert!(block_of(&hub.hub, &m).is_some());
    // Age the row by hand in the side store, then let the tick run.
    let db = rusqlite::Connection::open(hub.dir.path().join("door.db")).unwrap();
    db.execute("UPDATE SEEN SET last_seen = 1000, first_seen = 1000", [])
        .unwrap();
    drop(db);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if block_of(&hub.hub, &m).is_none() {
            return;
        }
    }
    panic!("the old row is still there");
}

#[test]
fn redaction_keeps_what_the_public_may_see() {
    let mut state = json!({
        "hub": {"id": 3},
        "quota": {"spend": [1]},
        "pools": [{"owner": "o", "pool": "p", "window": "5h", "state": "open", "used_pct": 3.0,
                    "resets_at": 5, "waiting_tasks": []}],
        "recent": [
            {"kind": "FACT", "topic": "quota/o/p/5h", "body": "x"},
            {"kind": "FACT", "topic": "spend/o", "body": "x"},
            {"kind": "CHAT", "topic": "quota/talk", "body": "x"},
            {"kind": "FACT", "topic": "key/m1", "body": "x"}
        ]
    });
    redact_state(&mut state);
    assert!(state.get("quota").is_none());
    assert_eq!(
        state["pools"][0],
        json!({"pool": "p", "window": "5h", "state": "open", "resets_at": 5})
    );
    assert_eq!(state["recent"].as_array().unwrap().len(), 2);
    let m = |kind, topic: &str| Message {
        id: 1,
        ts: String::new(),
        from: "x".into(),
        role: Role::Member,
        kind,
        topic: topic.into(),
        body: String::new(),
        refs: vec![],
        to: vec![],
    };
    assert!(hidden_from_public(&m(Kind::Fact, "quota/a")));
    assert!(hidden_from_public(&m(Kind::Fact, "spend/a")));
    assert!(!hidden_from_public(&m(Kind::Chat, "quota/a")));
}
