//! The findings of the security review of the key identity code, one test each. Every test
//! fails on the code before the fix. Local ports only, no vendor login.

use std::time::Duration;

use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::derive::parse_hello_capacity;
use sommelier::hub::{self, HubConfig};
use sommelier::key::book::{Standing, join_body};
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, member_id};
use sommelier::key::{Challenges, JoinRules, KeyBook, tier0_allows};
use sommelier::msg::{Kind, Message, Role};
use sommelier::tokens::TokenFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const BITS: u32 = 8;

struct TestHub {
    url: String,
    dir: tempfile::TempDir,
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

    fn alice(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.alice.clone(),
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("bus.jsonl")).unwrap_or_default()
    }

    async fn state(&self) -> Value {
        get(
            &self.url,
            "/api/state",
            Some(&format!("Bearer {}", self.orch)),
        )
        .await
        .1
    }
}

/// A hub on a free port. `seed` is the log that the data folder already holds.
async fn start(join_on: bool, seed: Option<&str>) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    if let Some(log) = seed {
        std::fs::write(dir.path().join("bus.jsonl"), log).unwrap();
    }
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.join = JoinRules {
        enabled: join_on,
        proof_of_work_bits: BITS,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    TestHub {
        url,
        dir,
        orch,
        alice,
        task,
    }
}

async fn get(url: &str, path: &str, auth: Option<&str>) -> (u16, Value) {
    let mut r = reqwest::Client::new().get(kc::http_url(url, path).unwrap());
    if let Some(a) = auth {
        r = r.header("authorization", a);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn refused(opts: &Opts, req: Value) -> String {
    let v = client::request(opts, req).await.expect("request");
    assert_eq!(v["ok"], false, "{v}");
    v["error"].as_str().unwrap().to_owned()
}

fn line(id: u64, from: &str, role: Role, kind: Kind, topic: &str, body: &str) -> String {
    let m = Message {
        id,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: from.into(),
        role,
        kind,
        topic: topic.into(),
        body: body.into(),
        refs: vec![],
        to: vec![],
    };
    format!("{}\n", serde_json::to_string(&m).unwrap())
}

/// A log with the join of `key` and a first line, ready to be seeded.
fn join_line(id: u64, key: &MemberKey) -> String {
    let member = member_id(&key.public());
    line(
        id,
        "hub",
        Role::Hub,
        Kind::Fact,
        &format!("key/{member}"),
        &join_body(
            &member,
            &key.public_b64(),
            "bob",
            "2026-11-25T10:00:00.000Z",
        ),
    )
}

fn slots_of(state: &Value) -> usize {
    state["derived"]["slots"].as_array().map_or(0, Vec::len)
}

// Finding 1 and 5: HELLO.

#[test]
fn a_huge_slot_count_is_cut_where_it_is_parsed() {
    let cap = parse_hello_capacity("class=a:4294967295,b:4294967295", &[], 1);
    assert!(cap.classes.values().all(|&n| n <= 64), "{:?}", cap.classes);
    assert!(cap.total_slots() <= 256);
    let cap = parse_hello_capacity("slots=4294967295 gpu_slots=4294967295", &[], 1);
    assert!(cap.classes["default"] <= 64 && cap.gpu_slots <= 64);
    let many: Vec<String> = (0..500).map(|i| format!("c{i}:60")).collect();
    let cap = parse_hello_capacity(&format!("class={}", many.join(",")), &[], 1);
    assert!(cap.classes.len() <= 16 && cap.total_slots() <= 256);
}

#[tokio::test]
async fn tier_0_may_not_send_hello_and_not_talk_to_token_members() {
    assert!(!tier0_allows("HELLO"));
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    for req in [
        json!({"op": "send", "kind": "HELLO", "body": "class=a:4294967295"}),
        json!({"op": "send", "kind": "HELLO", "to": ["alice"], "body": "free text for alice"}),
    ] {
        let e = refused(&opts, req).await;
        assert!(e.contains("tier 0") && e.contains("HELLO"), "{e}");
    }
    assert!(!hub.log_text().contains("4294967295"));
}

#[tokio::test]
async fn a_hello_with_a_huge_count_from_a_token_member_leaves_the_hub_responsive() {
    let hub = start(false, None).await;
    client::request_ok(
        &hub.alice(),
        json!({"op": "send", "kind": "HELLO", "body": "machine=box class=a:4294967295 gpu_slots=4294967295"}),
    )
    .await
    .unwrap();
    let state = tokio::time::timeout(Duration::from_secs(20), hub.state())
        .await
        .expect("the hub answers");
    assert!(slots_of(&state) <= 64, "{} slots", slots_of(&state));
    let idle = tokio::time::timeout(
        Duration::from_secs(20),
        client::request_ok(&hub.orch(), json!({"op": "idle"})),
    )
    .await
    .expect("idle answers")
    .unwrap();
    assert!(idle["slots"].as_array().unwrap().len() <= 64);
}

#[tokio::test]
async fn a_log_that_holds_a_huge_hello_replays_without_harm() {
    let key = MemberKey::generate().unwrap();
    let member = member_id(&key.public());
    let mut log = join_line(1, &key);
    log += &line(
        2,
        "alice",
        Role::Member,
        Kind::Hello,
        "",
        "machine=box class=a:4294967295",
    );
    log += &line(
        3,
        &member,
        Role::Member,
        Kind::Hello,
        "",
        "machine=evil class=b:4294967295 gpu_slots=4294967295",
    );
    log += &line(
        4,
        "alice",
        Role::Member,
        Kind::Fact,
        "capacity/alice/box",
        r#"{"machine":"box","class":"c","useful_parallel":18446744073709551615,"gpu_slots":18446744073709551615}"#,
    );
    let hub = start(false, Some(&log)).await;
    let state = tokio::time::timeout(Duration::from_secs(20), hub.state())
        .await
        .expect("the hub answers after the replay");
    assert!(slots_of(&state) <= 64, "{} slots", slots_of(&state));
    for c in state["store"]["capacities"].as_array().unwrap() {
        assert!(c["useful_parallel"].as_u64().unwrap() <= 64, "{c}");
    }
    // The key member declared no machine and no capacity.
    let owners: Vec<&str> = state["store"]["capacities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["owner"].as_str().unwrap())
        .collect();
    assert!(!owners.contains(&member.as_str()), "{owners:?}");
    let machines = state["store"]["machines"].as_array().unwrap();
    assert!(machines.iter().all(|m| m["owner"] != member.as_str()));
}

// Finding 2: a key member is no token guest.

#[tokio::test]
async fn a_key_member_has_trust_tier0_and_no_capacity_rows() {
    let key = MemberKey::generate().unwrap();
    let member = member_id(&key.public());
    let mut log = join_line(1, &key);
    log += &line(2, &member, Role::Member, Kind::Chat, "", "an old line");
    log += &line(
        3,
        &member,
        Role::Member,
        Kind::Hello,
        "",
        "class=codex:2 accepts=fix",
    );
    let hub = start(false, Some(&log)).await;
    let state = hub.state().await;
    let members = state["store"]["members"].as_array().unwrap();
    let row = members
        .iter()
        .find(|m| m["name"] == member.as_str())
        .unwrap_or_else(|| panic!("no member row in {members:?}"));
    assert_eq!(row["trust"], "tier0", "{row}");
    assert!(state["store"]["capacities"].as_array().unwrap().is_empty());
    assert!(state["store"]["machines"].as_array().unwrap().is_empty());
    // A token member keeps its trust.
    client::request_ok(
        &hub.alice(),
        json!({"op": "send", "kind": "CHAT", "body": "hi"}),
    )
    .await
    .unwrap();
    let state = hub.state().await;
    let alice = state["store"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "alice")
        .unwrap()
        .clone();
    assert_eq!(alice["trust"], "guest");
}

/// A key member at tier 1 has capacity like a token member: its HELLO is kept.
#[tokio::test]
async fn a_key_member_at_tier_1_has_machine_and_capacity_rows() {
    let key = MemberKey::generate().unwrap();
    let member = member_id(&key.public());
    let mut log = join_line(1, &key);
    log += &line(
        2,
        "hub",
        Role::Hub,
        Kind::Fact,
        &format!("tier/{member}"),
        &json!({"v": 1, "member": member, "tier": 1, "reasons": ["test"], "at": 1}).to_string(),
    );
    log += &line(
        3,
        &member,
        Role::Member,
        Kind::Hello,
        "",
        "machine=box class=codex:2 accepts=fix",
    );
    let hub = start(false, Some(&log)).await;
    let state = hub.state().await;
    let owners: Vec<&str> = state["store"]["capacities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["owner"].as_str().unwrap())
        .collect();
    assert!(owners.contains(&member.as_str()), "{owners:?}");
    let machines = state["store"]["machines"].as_array().unwrap();
    assert!(machines.iter().any(|m| m["owner"] == member.as_str()));
}

/// A HELLO of a key member at tier 0 leaves no ROLE_HELLO row, at tier 1 it does.
#[test]
fn a_hello_of_a_key_member_writes_a_role_row_only_from_tier_1() {
    let key = MemberKey::generate().unwrap();
    let member = member_id(&key.public());
    let dir = tempfile::tempdir().unwrap();
    let mut store = sommelier::store::Store::open(dir.path()).unwrap();
    let mut id = 0;
    let mut put = |store: &mut sommelier::store::Store, text: String| {
        id += 1;
        let mut m: Message = serde_json::from_str(text.trim()).unwrap();
        m.id = id;
        store.apply(&m).unwrap();
    };
    put(&mut store, join_line(1, &key));
    let hello = line(
        1,
        &member,
        Role::Member,
        Kind::Hello,
        "",
        "machine=box class=codex:2 accepts=fix",
    );
    put(&mut store, hello.clone());
    let rows = |store: &sommelier::store::Store| -> i64 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM ROLE_HELLO", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(rows(&store), 0);
    put(
        &mut store,
        line(
            1,
            "hub",
            Role::Hub,
            Kind::Fact,
            &format!("tier/{member}"),
            &json!({"v": 1, "member": member, "tier": 1, "reasons": [], "at": 1}).to_string(),
        ),
    );
    put(&mut store, hello);
    assert_eq!(rows(&store), 1);
}

// Finding 3: stateless nonces.

#[test]
fn a_flood_of_challenges_holds_nothing_and_locks_nobody_out() {
    let c = Challenges::default();
    for i in 0..30_000u64 {
        c.issue(1_000 + i, 60_000).unwrap();
    }
    assert!(c.is_empty(), "the hub keeps no nonce it handed out");
    let (nonce, _) = c.issue(2_000, 60_000).unwrap();
    c.consume(&nonce, 3_000).unwrap();
    assert!(c.consume(&nonce, 3_000).is_err(), "used once");
    assert_eq!(c.len(), 1, "only the used nonce is kept");
    // Another hub secret does not accept the nonce.
    let (nonce, _) = c.issue(2_000, 60_000).unwrap();
    assert!(Challenges::default().check(&nonce, 3_000).is_err());
}

#[tokio::test]
async fn the_nonce_secret_stays_in_a_file_and_out_of_the_log() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let secret = std::fs::read_to_string(hub.dir.path().join("challenge_secret")).unwrap();
    let secret = secret.trim();
    assert_eq!(secret.len(), 64);
    assert!(!hub.log_text().contains(secret));
    assert!(!hub.state().await.to_string().contains(secret));
    let (_, ch) = get(&hub.url, "/api/challenge", None).await;
    assert!(!ch.to_string().contains(secret));
}

// Finding 4: the hub id is pinned per address.

#[tokio::test]
async fn a_challenge_with_another_hub_id_is_refused() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    kc::auth_header(&hub.url, &opts.token).await.unwrap();

    let pins = kc::pins_path(&kc::key_path(dir.path()));
    let text = std::fs::read_to_string(&pins).unwrap();
    assert!(text.contains(&hub.url), "the pin names the address");
    std::fs::write(&pins, format!("{} {}\n", hub.url, "0".repeat(32))).unwrap();
    let err = kc::auth_header(&hub.url, &opts.token).await.unwrap_err();
    assert!(err.fatal);
    assert!(
        err.msg.contains("hub id") && err.msg.contains("delete the line"),
        "{}",
        err.msg
    );
    // On purpose: delete the line.
    std::fs::write(&pins, "").unwrap();
    kc::auth_header(&hub.url, &opts.token).await.unwrap();
}

// Finding 6: with joining off the hub serves no key door.

#[tokio::test]
async fn with_joining_off_there_is_no_challenge_and_no_key_sign_in() {
    let on = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&on.url, dir.path(), None).await.unwrap();
    let member = kc::load(&kc::key_path(dir.path())).unwrap().member;
    let log = on.log_text();

    let off = start(false, Some(&log)).await;
    let (code, v) = get(&off.url, "/api/challenge", None).await;
    assert_eq!(code, 403, "{v}");
    assert!(v.get("nonce").is_none());
    // The key is in the log, but the hub accepts no sign in with it.
    let key = kc::load(&kc::key_path(dir.path())).unwrap();
    let nonce = Challenges::default().issue(0, 1).unwrap().0;
    let header = format!(
        "SommelierKey {member}.{nonce}.{}",
        sommelier::key::crypto::b64(&key.key.sign(b"x"))
    );
    let (code, _) = get(&off.url, "/api/state", Some(&header)).await;
    assert_eq!(code, 401);
    let opts = kc::opts_for(&off.url, dir.path());
    assert!(
        client::request_ok(&opts, json!({"op": "whoami"}))
            .await
            .is_err()
    );
    // Tokens still work as before.
    assert_eq!(
        client::request_ok(&off.orch(), json!({"op": "whoami"}))
            .await
            .unwrap()["name"],
        "orch"
    );
}

// Finding 7: a rotation whose reply was lost.

#[tokio::test]
async fn a_lost_rotation_reply_does_not_lose_the_identity() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let path = kc::key_path(dir.path());
    let old = kc::load(&path).unwrap();

    // The client wrote member.key.next and sent the rotation. The hub wrote it, the reply was lost.
    let new = MemberKey::generate().unwrap();
    let hub_id = kc::fetch_challenge(&hub.url).await.unwrap().hub_id;
    let mut req = sommelier::key::book::rotate_request(
        &hub_id,
        &old.member,
        &old.key.public_b64(),
        &new.public_b64(),
    );
    req["sig"] = json!(sommelier::key::crypto::sign_request(&old.key, &req));
    // The client also notes which hub the rotation went to.
    std::fs::write(
        path.with_extension("key.next.hub"),
        format!(
            "{hub_id}
"
        ),
    )
    .unwrap();
    kc::save(
        &path.with_extension("key.next"),
        &kc::KeyFile {
            member: old.member.clone(),
            key: MemberKey::from_pkcs8(new.pkcs8()).unwrap(),
        },
    )
    .unwrap();
    client::request_ok(
        &kc::opts_for(&hub.url, dir.path()),
        json!({"op": "key.rotate", "hub": hub_id, "new_pub": new.public_b64(), "sig": req["sig"]}),
    )
    .await
    .unwrap();

    // The next sign in finds the new key by itself.
    let who = client::request_ok(&kc::opts_for(&hub.url, dir.path()), json!({"op": "whoami"}))
        .await
        .unwrap();
    assert_eq!(who["name"], old.member.as_str());
    assert_eq!(kc::load(&path).unwrap().key.public_b64(), new.public_b64());
    assert!(!path.with_extension("key.next").exists());
}

#[tokio::test]
async fn a_failed_rotation_keeps_the_new_key_file() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    // A hub that answers the challenge and then no request: the rotation was sent, the reply is lost.
    let flaky = flaky_hub().await;
    let err = kc::rotate(&flaky, dir.path()).await.unwrap_err();
    assert!(err.contains("member.key.next"), "{err}");
    assert!(kc::key_path(dir.path()).with_extension("key.next").exists());
    // A hub that cannot be reached at all gets nothing: no new key is made.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("ws://{}", l.local_addr().unwrap())
    };
    std::fs::remove_file(kc::key_path(dir.path()).with_extension("key.next")).unwrap();
    assert!(kc::rotate(&dead, dir.path()).await.is_err());
    assert!(!kc::key_path(dir.path()).with_extension("key.next").exists());
}

/// A hub that answers the challenge and refuses everything else with a 500.
async fn flaky_hub() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let reply = if head.starts_with("GET /api/challenge") {
                let body = json!({"hub_id": "b".repeat(32), "nonce": "n".repeat(8),
                                  "expires": "x", "pow_bits": 0})
                .to_string();
                format!(
                    "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 500 Internal Server Error
Content-Length: 0
Connection: close

"
                .to_owned()
            };
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    url
}

// Finding 8: the client trusts the hub less.

/// A hub that is not one: it answers the challenge and the join with what `challenge` and
/// `member` say.
async fn fake_hub(pow_bits: u64, member: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if head.starts_with("GET /api/challenge") {
                json!({"hub_id": "a".repeat(32), "nonce": "n".repeat(8), "expires": "x",
                       "pow_bits": pow_bits})
            } else {
                json!({"member": member, "joined": true, "id": 1})
            }
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    url
}

#[tokio::test]
async fn a_huge_proof_of_work_request_is_refused_by_the_client() {
    let url = fake_hub(4_000_000_000, "mxxxxxxxxxxxxxxxx").await;
    let dir = tempfile::tempdir().unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), kc::join(&url, dir.path(), None))
        .await
        .expect("the client does not loop");
    let e = r.unwrap_err();
    assert!(e.contains("proof of work"), "{e}");
}

#[tokio::test]
async fn a_member_id_that_the_key_does_not_make_is_refused_by_the_client() {
    let url = fake_hub(0, "mevilevilevilevil").await;
    let dir = tempfile::tempdir().unwrap();
    let e = kc::join(&url, dir.path(), None).await.unwrap_err();
    assert!(e.contains("member id"), "{e}");
    let file = kc::load(&kc::key_path(dir.path())).unwrap();
    assert_eq!(file.member, member_id(&file.key.public()));
}

// Finding 9: a member id that belongs to another key.

#[test]
fn a_member_id_of_another_key_is_not_joined_twice() {
    let a = MemberKey::generate().unwrap();
    let b = MemberKey::generate().unwrap();
    let mut book = KeyBook::default();
    let first = sommelier::key::book::parse_fact(
        &serde_json::from_str::<Message>(join_line(1, &a).trim()).unwrap(),
    )
    .expect("a good fact");
    assert_eq!(first.member, member_id(&a.public()));
    book.apply(&serde_json::from_str::<Message>(join_line(1, &a).trim()).unwrap());
    let member_a = member_id(&a.public());
    assert_eq!(
        book.standing(&member_a, &b.public_b64()),
        Standing::Taken,
        "the id of a belongs to a"
    );
    assert_eq!(
        book.standing(&member_a, &a.public_b64()),
        Standing::Known(member_a.clone())
    );
    assert_eq!(
        book.standing(&member_id(&b.public()), &b.public_b64()),
        Standing::New
    );
}

// The sign in names the address, the pins and the key requests name the hub (door 4, 10, testnet 5).

/// A hub with `public_addresses` set.
async fn start_listed(addresses: &[&str]) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: BITS,
    };
    cfg.door.public_addresses = addresses.iter().map(|a| (*a).to_owned()).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    TestHub {
        url,
        dir,
        orch,
        alice,
        task,
    }
}

/// The header a client would make for a hub it reached at `address`, built by hand.
async fn header_for_address(hub: &TestHub, key: &kc::KeyFile, address: &str) -> String {
    let ch = kc::fetch_challenge(&hub.url).await.unwrap();
    let sig = sommelier::key::crypto::b64(
        &key.key
            .sign(sommelier::key::crypto::auth_text(&ch.hub_id, address, &ch.nonce).as_bytes()),
    );
    format!(
        "SommelierKey {}.{}.{}.{sig}",
        key.member,
        ch.nonce,
        sommelier::key::crypto::b64(address.as_bytes())
    )
}

#[tokio::test]
async fn a_sign_in_made_for_another_address_is_refused() {
    let hub = start_listed(&["hub.example.org"]).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let key = kc::load(&kc::key_path(dir.path())).unwrap();

    // A hostile hub at another address hands the real nonce and hub id on. The signature it gets from
    // the member names the hostile address, so the real hub refuses it.
    let relay = header_for_address(&hub, &key, "relay.example.test:443").await;
    let (code, v) = get(&hub.url, "/api/state", Some(&relay)).await;
    assert_eq!(code, 401, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("not a public address"),
        "{v}"
    );
    // Not even an address that merely contains the name of the hub.
    let lookalike = header_for_address(&hub, &key, "hub.example.org.evil.test:443").await;
    assert_eq!(get(&hub.url, "/api/state", Some(&lookalike)).await.0, 401);
    // The address the owner listed works, with the signature made for it.
    let ok = header_for_address(&hub, &key, "hub.example.org:443").await;
    assert_eq!(get(&hub.url, "/api/state", Some(&ok)).await.0, 200);
    // So does the address of the own machine.
    let own = header_for_address(&hub, &key, "127.0.0.1:9").await;
    assert_eq!(get(&hub.url, "/api/state", Some(&own)).await.0, 200);
    // A signature made for one address does not pass with another address written into the header.
    let ch = kc::fetch_challenge(&hub.url).await.unwrap();
    let sig = sommelier::key::crypto::b64(
        &key.key.sign(
            sommelier::key::crypto::auth_text(&ch.hub_id, "relay.example.test:443", &ch.nonce)
                .as_bytes(),
        ),
    );
    let forged = format!(
        "SommelierKey {}.{}.{}.{sig}",
        key.member,
        ch.nonce,
        sommelier::key::crypto::b64(b"hub.example.org:443")
    );
    let (code, v) = get(&hub.url, "/api/state", Some(&forged)).await;
    assert_eq!(code, 401, "{v}");
    assert!(v["error"].as_str().unwrap().contains("signature"), "{v}");
}

#[tokio::test]
async fn a_hub_without_public_addresses_takes_loopback_only_and_says_why() {
    let hub = start_listed(&[]).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let key = kc::load(&kc::key_path(dir.path())).unwrap();
    let other = header_for_address(&hub, &key, "hub.example.org:443").await;
    let (code, v) = get(&hub.url, "/api/state", Some(&other)).await;
    assert_eq!(code, 401);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("door.public_addresses"),
        "{v}"
    );
    // The client signs the address of its URL: loopback works as before.
    let opts = kc::opts_for(&hub.url, dir.path());
    client::request_ok(&opts, json!({"op": "whoami"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_hub_id_pinned_under_another_address_is_refused() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    let hub_id = kc::fetch_challenge(&hub.url).await.unwrap().hub_id;
    // The id of the real hub was pinned for the real address. A relay at a new address passes it on.
    let pins = kc::pins_path(&kc::key_path(dir.path()));
    std::fs::write(&pins, format!("wss://hub.example.test {hub_id}\n")).unwrap();
    let err = kc::auth_header(&hub.url, &opts.token).await.unwrap_err();
    assert!(err.fatal);
    assert!(
        err.msg.contains("wss://hub.example.test") && err.msg.contains("relay"),
        "{}",
        err.msg
    );
    // Nothing was added to the file.
    assert_eq!(
        std::fs::read_to_string(&pins).unwrap(),
        format!("wss://hub.example.test {hub_id}\n")
    );
}

#[tokio::test]
async fn a_pin_file_that_cannot_be_read_is_not_replaced() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    let pins = kc::pins_path(&kc::key_path(dir.path()));
    std::fs::write(
        &pins,
        format!("wss://other.example.test {}\n", "1".repeat(32)),
    )
    .unwrap();
    // A read error other than "not found" (here: the path is a folder) stops the sign in.
    let saved = std::fs::read(&pins).unwrap();
    std::fs::remove_file(&pins).unwrap();
    std::fs::create_dir(&pins).unwrap();
    let err = kc::auth_header(&hub.url, &opts.token).await.unwrap_err();
    assert!(err.fatal && err.msg.contains("cannot read"), "{}", err.msg);
    assert!(pins.is_dir(), "the path was left alone");
    std::fs::remove_dir(&pins).unwrap();
    std::fs::write(&pins, &saved).unwrap();
    kc::auth_header(&hub.url, &opts.token).await.unwrap();
    let after = std::fs::read_to_string(&pins).unwrap();
    assert!(
        after.contains("wss://other.example.test"),
        "the other pin is kept: {after}"
    );
}

#[tokio::test]
async fn a_new_key_is_promoted_only_at_the_hub_the_rotation_went_to() {
    let dir = tempfile::tempdir().unwrap();
    // A hub that is not the real one says yes to everything.
    let hostile = fake_hub(0, "mabcdefghijklmnop").await;
    let key_path = kc::key_path(dir.path());
    let old = MemberKey::generate().unwrap();
    let member = member_id(&old.public());
    let file = kc::KeyFile {
        member: member.clone(),
        key: MemberKey::from_pkcs8(old.pkcs8()).unwrap(),
    };
    kc::save(&key_path, &file).unwrap();
    let next = MemberKey::generate().unwrap();
    kc::save(
        &key_path.with_extension("key.next"),
        &kc::KeyFile {
            member: member.clone(),
            key: next,
        },
    )
    .unwrap();
    let opts = kc::opts_for(&hostile, dir.path());
    let before = std::fs::read(&key_path).unwrap();
    // No record of where the rotation went: no promotion.
    let _ = kc::auth_header(&hostile, &opts.token).await;
    assert_eq!(std::fs::read(&key_path).unwrap(), before);
    // The rotation went to a hub with another id: no promotion either.
    std::fs::write(
        key_path.with_extension("key.next.hub"),
        format!("{}\n", "c".repeat(32)),
    )
    .unwrap();
    let _ = kc::auth_header(&hostile, &opts.token).await;
    assert_eq!(std::fs::read(&key_path).unwrap(), before);
    assert!(key_path.with_extension("key.next").exists());
    // At the hub the rotation went to, it is promoted.
    std::fs::write(
        key_path.with_extension("key.next.hub"),
        format!("{}\n", "a".repeat(32)),
    )
    .unwrap();
    let _ = kc::auth_header(&hostile, &opts.token).await;
    assert_ne!(std::fs::read(&key_path).unwrap(), before);
    assert!(!key_path.with_extension("key.next").exists());
}

#[tokio::test]
async fn rotate_and_revoke_are_made_for_one_hub() {
    let hub = start(true, None).await;
    let dir = tempfile::tempdir().unwrap();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let key = kc::load(&kc::key_path(dir.path())).unwrap();
    let opts = kc::opts_for(&hub.url, dir.path());
    let other_hub = "0".repeat(32);
    let new = MemberKey::generate().unwrap();

    // A rotation signed for another hub, replayed here: with the hub named and without.
    let mut req = sommelier::key::book::rotate_request(
        &other_hub,
        &key.member,
        &key.key.public_b64(),
        &new.public_b64(),
    );
    req["sig"] = json!(sommelier::key::crypto::sign_request(&key.key, &req));
    let named = json!({"op": "key.rotate", "hub": other_hub, "new_pub": new.public_b64(),
                       "sig": req["sig"]});
    assert!(refused(&opts, named).await.contains("another hub"));
    let silent = json!({"op": "key.rotate", "new_pub": new.public_b64(), "sig": req["sig"]});
    assert!(refused(&opts, silent).await.contains("signature"));

    // The same for a revocation.
    let mut req =
        sommelier::key::book::revoke_request(&other_hub, &key.member, &key.key.public_b64(), "x");
    req["sig"] = json!(sommelier::key::crypto::sign_request(&key.key, &req));
    let silent = json!({"op": "key.revoke", "reason": "x", "sig": req["sig"]});
    assert!(refused(&opts, silent).await.contains("signature"));
    // Nothing changed: the key still works, and the client made for this hub goes through.
    client::request_ok(&opts, json!({"op": "whoami"}))
        .await
        .unwrap();
    kc::rotate(&hub.url, dir.path()).await.unwrap();
    client::request_ok(&kc::opts_for(&hub.url, dir.path()), json!({"op": "whoami"}))
        .await
        .unwrap();
    kc::revoke(&hub.url, dir.path(), "done").await.unwrap();
}

#[test]
fn the_join_output_can_say_how_private_the_key_file_is() {
    // Unix keeps the file at mode 0600 and needs no note, Windows gets the sentence.
    assert_eq!(kc::privacy_note().is_some(), cfg!(windows));
    if let Some(n) = kc::privacy_note() {
        assert!(n.contains("as private as the folder"), "{n}");
    }
}
