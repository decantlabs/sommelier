//! Key members: challenge, join, signing in, tier 0, rotation and revocation, against a real
//! hub on a free local port. No network beyond localhost, no vendor login.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, member_id, pow_ok, sign_request, solve_pow};
use sommelier::key::{JoinRules, tier0_allows};
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::Store;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BITS: u32 = 8;

struct TestHub {
    url: String,
    dir: tempfile::TempDir,
    orch: String,
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

    fn key_facts(&self) -> Vec<Message> {
        self.log_text()
            .lines()
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .filter(|m| m.kind == Kind::Fact && m.topic.starts_with("key/"))
            .collect()
    }
}

async fn start(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: BITS,
    };
    tweak(&mut cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    TestHub {
        url,
        dir,
        orch,
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

async fn post_join(url: &str, body: &Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(kc::http_url(url, "/api/join").unwrap())
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

/// A join body for `key` with a solved proof of work for the nonce.
fn join_body(key: &MemberKey, nonce: &str, handle: &str, bits: u32) -> Value {
    let counter = solve_pow(nonce, &key.public(), bits);
    let mut body =
        json!({"pub": key.public_b64(), "handle": handle, "nonce": nonce, "counter": counter});
    body["sig"] = json!(sign_request(key, &body));
    body
}

async fn nonce(url: &str) -> String {
    get(url, "/api/challenge", None).await.1["nonce"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn client_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn opts_of(hub: &TestHub, dir: &Path) -> Opts {
    kc::opts_for(&hub.url, dir)
}

async fn whoami(opts: &Opts) -> Result<Value, String> {
    client::request_ok(opts, json!({"op": "whoami"})).await
}

async fn refused(opts: &Opts, req: Value) -> String {
    let v = client::request(opts, req).await.expect("request");
    assert_eq!(v["ok"], false, "{v}");
    v["error"].as_str().unwrap().to_owned()
}

async fn connect_with(url: &str, header: &str) -> Result<client::Ws, String> {
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", header.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn joining_is_refused_while_it_is_disabled() {
    let hub = start(|c| c.join = JoinRules::default()).await;
    assert!(!JoinRules::default().enabled);
    let key = MemberKey::generate().unwrap();
    let n = nonce(&hub.url).await;
    let (code, v) = post_join(&hub.url, &join_body(&key, &n, "bob", 20)).await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("switched off"), "{v}");
    assert!(hub.key_facts().is_empty());
    // A hub that never turned it on behaves as before: tokens work.
    assert_eq!(whoami(&hub.orch()).await.unwrap()["name"], "orch");
}

#[tokio::test]
async fn the_challenge_has_a_hub_id_and_a_fresh_nonce() {
    let hub = start(|_| {}).await;
    let (code, a) = get(&hub.url, "/api/challenge", None).await;
    assert_eq!(code, 200);
    let (_, b) = get(&hub.url, "/api/challenge", None).await;
    assert_eq!(a["hub_id"], b["hub_id"]);
    assert_ne!(a["nonce"], b["nonce"]);
    assert!(a["expires"].as_str().unwrap().ends_with('Z'));
    let id = std::fs::read_to_string(hub.dir.path().join("hub_id")).unwrap();
    assert_eq!(id.trim(), a["hub_id"].as_str().unwrap());
}

#[tokio::test]
async fn bad_joins_are_refused() {
    let hub = start(|_| {}).await;
    let key = MemberKey::generate().unwrap();

    // A signature of another key.
    let n = nonce(&hub.url).await;
    let mut body = join_body(&key, &n, "bob", BITS);
    body["sig"] = json!(sign_request(&MemberKey::generate().unwrap(), &body));
    let (code, v) = post_join(&hub.url, &body).await;
    assert_eq!(code, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("signature"), "{v}");

    // A field changed after signing.
    let n = nonce(&hub.url).await;
    let mut body = join_body(&key, &n, "bob", BITS);
    body["handle"] = json!("eve");
    assert_eq!(post_join(&hub.url, &body).await.0, 400);

    // A nonce that was used.
    let n = nonce(&hub.url).await;
    let body = join_body(&key, &n, "bob", BITS);
    assert_eq!(post_join(&hub.url, &body).await.0, 200);
    let other = MemberKey::generate().unwrap();
    let (code, v) = post_join(&hub.url, &join_body(&other, &n, "bob", BITS)).await;
    assert_eq!(code, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("nonce"), "{v}");

    // A nonce nobody handed out.
    let (code, _) = post_join(&hub.url, &join_body(&other, "feedface", "bob", BITS)).await;
    assert_eq!(code, 400);

    // A handle that is not allowed.
    let n = nonce(&hub.url).await;
    let (code, v) = post_join(&hub.url, &join_body(&other, &n, "no spaces", BITS)).await;
    assert_eq!(code, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("handle"), "{v}");

    // Nothing but the one good join was written.
    assert_eq!(hub.key_facts().len(), 1);
}

#[tokio::test]
async fn a_missing_proof_of_work_is_refused() {
    let hub = start(|c| c.join.proof_of_work_bits = 14).await;
    let key = MemberKey::generate().unwrap();
    let n = nonce(&hub.url).await;
    let wrong = (0u64..)
        .find(|c| !pow_ok(&n, &key.public(), *c, 14))
        .unwrap();
    let mut body = json!({"pub": key.public_b64(), "handle": "bob", "nonce": n, "counter": wrong});
    body["sig"] = json!(sign_request(&key, &body));
    let (code, v) = post_join(&hub.url, &body).await;
    assert_eq!(code, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("proof of work"),
        "{v}"
    );
    assert!(hub.key_facts().is_empty());
}

#[tokio::test]
async fn an_expired_nonce_is_refused() {
    let hub = start(|c| c.nonce_ttl = Duration::from_millis(1)).await;
    let key = MemberKey::generate().unwrap();
    let n = nonce(&hub.url).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let (code, v) = post_join(&hub.url, &join_body(&key, &n, "bob", BITS)).await;
    assert_eq!(code, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("expired"), "{v}");
}

#[tokio::test]
async fn a_good_join_writes_one_fact_and_a_second_join_writes_none() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let out = kc::join(&hub.url, dir.path(), Some("bob")).await.unwrap();
    assert!(out.new_key && out.registered);
    let file = kc::load(&kc::key_path(dir.path())).unwrap();
    assert_eq!(out.member, member_id(&file.key.public()));
    assert_eq!(out.member, file.member);

    let facts = hub.key_facts();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].topic, format!("key/{}", out.member));
    assert_eq!(facts[0].from, "hub");
    assert!(!facts[0].body.contains('\n'));
    let body: Value = serde_json::from_str(&facts[0].body).unwrap();
    for field in ["v", "member", "pub", "handle", "state", "joined_at"] {
        assert!(body.get(field).is_some(), "{field} in {body}");
    }
    assert_eq!(body["state"], "active");
    assert_eq!(body["handle"], "bob");

    let again = kc::join(&hub.url, dir.path(), Some("bob")).await.unwrap();
    assert!(!again.new_key && !again.registered);
    assert_eq!(again.member, out.member);
    assert_eq!(hub.key_facts().len(), 1);

    // The table is part of /api/state.
    let (code, state) = get(
        &hub.url,
        "/api/state",
        Some(&format!("Bearer {}", hub.orch)),
    )
    .await;
    assert_eq!(code, 200);
    let keys = state["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["member"], out.member.as_str());
    assert_eq!(keys[0]["handle"], "bob");
    assert_eq!(keys[0]["state"], "active");
    assert!(keys[0]["joined_at"].as_str().is_some());
}

#[tokio::test]
async fn a_key_member_connects_as_tier_0() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let out = kc::join(&hub.url, dir.path(), None).await.unwrap();
    let who = whoami(&opts_of(&hub, dir.path())).await.unwrap();
    assert_eq!(who["name"], out.member.as_str());
    assert_eq!(who["tier"], 0);
    assert_eq!(who["role"], "member");
    // A token member has no tier field and keeps today's answer.
    let w = whoami(&hub.orch()).await.unwrap();
    assert!(w.get("tier").is_none(), "{w}");

    // /api/state takes the key too.
    let header = kc::auth_header(&hub.url, &opts_of(&hub, dir.path()).token)
        .await
        .unwrap();
    let (code, state) = get(&hub.url, "/api/state", Some(&header)).await;
    assert_eq!(code, 200, "{state}");
    // The same header a second time is a replay.
    let (code, _) = get(&hub.url, "/api/state", Some(&header)).await;
    assert_eq!(code, 401);
}

#[tokio::test]
async fn tier_0_may_say_hello_and_nothing_more() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = opts_of(&hub, dir.path());

    let ok = client::request_ok(
        &opts,
        json!({"op": "send", "kind": "HELLO", "body": "machine=box os=linux"}),
    )
    .await
    .unwrap();
    assert!(ok["id"].as_u64().is_some());
    for op in ["heartbeat", "claims", "who", "roster"] {
        client::request_ok(&opts, json!({"op": op})).await.unwrap();
    }
    client::request_ok(&opts, json!({"op": "log", "last": 3}))
        .await
        .unwrap();

    for kind in ["CHAT", "READY", "CLAIM", "TASK"] {
        let e = refused(
            &opts,
            json!({"op": "send", "kind": kind, "topic": "t/1", "body": "x"}),
        )
        .await;
        assert!(e.contains("tier 0") && e.contains(kind), "{kind}: {e}");
    }
    let e = refused(
        &opts,
        json!({"op": "task.add", "kind": "fix", "title": "t", "scope": "s", "done_when": "d"}),
    )
    .await;
    assert!(e.contains("tier 0") && e.contains("task.add"), "{e}");
    let e = refused(&opts, json!({"op": "idle"})).await;
    assert!(e.contains("tier 0"), "{e}");
    assert!(tier0_allows("whoami") && !tier0_allows("task.add"));
    // Token members keep their rights: the same CHAT goes through for a member token.
    let mut alice = TokenFile::load(&hub.dir.path().join("tokens.json")).unwrap();
    let token = alice.rotate("alice").unwrap().0;
    alice.save(&hub.dir.path().join("tokens.json")).unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let a = Opts {
        url: hub.url.clone(),
        token,
    };
    client::request_ok(&a, json!({"op": "send", "kind": "CHAT", "body": "hi"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_member_cannot_forge_a_key_fact() {
    let hub = start(|_| {}).await;
    let e = refused(
        &hub.orch(),
        json!({"op": "send", "kind": "FACT", "topic": "key/mabcdefghijklmnop", "body": "{}"}),
    )
    .await;
    assert!(e.contains("hub only"), "{e}");
    assert!(hub.key_facts().is_empty());
}

#[tokio::test]
async fn rotate_keeps_the_member_and_refuses_the_old_key() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let out = kc::join(&hub.url, dir.path(), Some("bob")).await.unwrap();
    let old_copy: PathBuf = dir.path().join("old.key");
    std::fs::copy(kc::key_path(dir.path()), &old_copy).unwrap();
    let old_opts = kc::opts_for(&hub.url, dir.path());
    let old_opts = Opts {
        url: old_opts.url,
        token: kc::marker_for(&old_copy),
    };
    assert_eq!(
        whoami(&old_opts).await.unwrap()["name"],
        out.member.as_str()
    );

    let new_pub = kc::rotate(&hub.url, dir.path()).await.unwrap();
    let file = kc::load(&kc::key_path(dir.path())).unwrap();
    assert_eq!(file.key.public_b64(), new_pub);
    assert_eq!(file.member, out.member, "the id does not change");

    let err = whoami(&old_opts).await.unwrap_err();
    assert!(err.contains("401"), "{err}");
    let who = whoami(&opts_of(&hub, dir.path())).await.unwrap();
    assert_eq!(who["name"], out.member.as_str());
    assert_eq!(who["tier"], 0);

    // The rotation is one more fact, the signature of the old key is in its body.
    let facts = hub.key_facts();
    assert_eq!(facts.len(), 2);
    let body: Value = serde_json::from_str(&facts[1].body).unwrap();
    assert_eq!(body["op"], "rotate");
    assert!(body["sig"].as_str().is_some());

    // Joining again with the new key gives the same member and writes nothing.
    let again = kc::join(&hub.url, dir.path(), None).await.unwrap();
    assert_eq!(again.member, out.member);
    assert!(!again.registered);
    assert_eq!(hub.key_facts().len(), 2);

    // A rotation that the old key did not sign is refused.
    let intruder = MemberKey::generate().unwrap();
    let mut req =
        json!({"op": "key.rotate", "new_pub": MemberKey::generate().unwrap().public_b64()});
    req["sig"] = json!(sign_request(&intruder, &req));
    let e = refused(&opts_of(&hub, dir.path()), req).await;
    assert!(e.contains("signature"), "{e}");
}

#[tokio::test]
async fn revoke_refuses_the_member_and_closes_its_connection() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let out = kc::join(&hub.url, dir.path(), None).await.unwrap();
    let opts = opts_of(&hub, dir.path());
    let header = kc::auth_header(&hub.url, &opts.token).await.unwrap();
    let mut ws = connect_with(&hub.url, &header).await.expect("connect");

    kc::revoke(&hub.url, dir.path(), "lost my laptop")
        .await
        .unwrap();

    // Refused at connect from now on.
    let err = whoami(&opts).await.unwrap_err();
    assert!(err.contains("revoked"), "{err}");
    // The open connection is closed within the periodic check.
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(frame) = ws.next().await {
            match frame {
                Ok(WsMsg::Close(_)) | Err(_) => return true,
                Ok(_) => {}
            }
        }
        true
    })
    .await;
    assert_eq!(closed, Ok(true), "the connection stays open");

    let facts = hub.key_facts();
    let body: Value = serde_json::from_str(&facts.last().unwrap().body).unwrap();
    assert_eq!(body["state"], "revoked");
    assert_eq!(body["reason"], "lost my laptop");
    assert!(body["sig"].as_str().is_some());

    let (_, state) = get(
        &hub.url,
        "/api/state",
        Some(&format!("Bearer {}", hub.orch)),
    )
    .await;
    assert_eq!(state["keys"][0]["state"], "revoked");
    assert_eq!(state["keys"][0]["member"], out.member.as_str());
}

#[tokio::test]
async fn an_orchestrator_revokes_with_a_reason_and_a_member_cannot_revoke_others() {
    let hub = start(|_| {}).await;
    let a = client_dir();
    let b = client_dir();
    let ma = kc::join(&hub.url, a.path(), None).await.unwrap().member;
    let mb = kc::join(&hub.url, b.path(), None).await.unwrap().member;

    // Member a tries to revoke member b with its own signature: refused.
    let key_a = kc::load(&kc::key_path(a.path())).unwrap().key;
    let mut req = json!({"op": "key.revoke", "member": mb, "reason": "x"});
    req["sig"] = json!(sign_request(&key_a, &req));
    let e = refused(&opts_of(&hub, a.path()), req).await;
    assert!(e.contains("own key"), "{e}");

    // An orchestrator needs a reason.
    let e = refused(&hub.orch(), json!({"op": "key.revoke", "member": ma})).await;
    assert!(e.contains("reason"), "{e}");
    client::request_ok(
        &hub.orch(),
        json!({"op": "key.revoke", "member": ma, "reason": "abuse"}),
    )
    .await
    .unwrap();
    assert!(whoami(&opts_of(&hub, a.path())).await.is_err());
    assert!(whoami(&opts_of(&hub, b.path())).await.is_ok());
    let last: Value = serde_json::from_str(&hub.key_facts().last().unwrap().body).unwrap();
    assert_eq!(last["by"], "orch");
    assert_eq!(last["reason"], "abuse");
}

#[tokio::test]
async fn a_replayed_authorization_header_is_refused() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    kc::join(&hub.url, dir.path(), None).await.unwrap();
    let header = kc::auth_header(&hub.url, &opts_of(&hub, dir.path()).token)
        .await
        .unwrap();
    connect_with(&hub.url, &header).await.expect("first use");
    let err = connect_with(&hub.url, &header).await.unwrap_err();
    assert!(err.contains("401"), "{err}");
    // A made up header and an unknown member are refused too.
    for bad in [
        "SommelierKey mabcdefghijklmnop.abc.def",
        "SommelierKey nonsense",
    ] {
        assert!(connect_with(&hub.url, bad).await.is_err(), "{bad}");
    }
}

#[tokio::test]
async fn no_network_address_reaches_the_log_or_the_state() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let out = kc::join(&hub.url, dir.path(), Some("bob")).await.unwrap();
    whoami(&opts_of(&hub, dir.path())).await.unwrap();
    kc::rotate(&hub.url, dir.path()).await.unwrap();
    let opts = opts_of(&hub, dir.path());
    client::request_ok(
        &opts,
        json!({"op": "send", "kind": "HELLO", "body": "machine=box"}),
    )
    .await
    .unwrap();

    let (_, state) = get(
        &hub.url,
        "/api/state",
        Some(&format!("Bearer {}", hub.orch)),
    )
    .await;
    let ip = regex::Regex::new(r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}").unwrap();
    let port = hub.url.rsplit(':').next().unwrap().to_owned();
    for (what, text) in [("log", hub.log_text()), ("state", state.to_string())] {
        assert!(!ip.is_match(&text), "{what} holds an address: {text}");
        assert!(
            !text.contains("::1") && !text.contains("localhost"),
            "{what}"
        );
        assert!(!text.contains(&format!(":{port}")), "{what} holds the port");
    }
    assert!(state.to_string().contains(&out.member));
}

fn replay_keys(
    log: &str,
) -> (
    std::collections::BTreeMap<String, String>,
    Vec<(String, String)>,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bus.jsonl"), log).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .catch_up_from_log(&dir.path().join("bus.jsonl"))
        .unwrap();
    let hashes = store.table_hashes().unwrap();
    let mut stmt = store
        .connection()
        .prepare(r#"SELECT member || ':' || public_key, state FROM "KEY" ORDER BY msg_id, member"#)
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (hashes, rows)
}

#[tokio::test]
async fn replaying_a_log_with_join_rotate_and_revoke_gives_the_same_key_table() {
    let hub = start(|_| {}).await;
    let dir = client_dir();
    let other = client_dir();
    kc::join(&hub.url, dir.path(), Some("bob")).await.unwrap();
    kc::join(&hub.url, other.path(), Some("eve")).await.unwrap();
    kc::rotate(&hub.url, dir.path()).await.unwrap();
    kc::revoke(&hub.url, dir.path(), "done").await.unwrap();
    // A forged fact in the log changes nothing: a member line, not from the hub.
    let mut log = hub.log_text();
    let forged = Message {
        id: 999,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: "alice".into(),
        role: Role::Member,
        kind: Kind::Fact,
        topic: format!(
            "key/{}",
            kc::load(&kc::key_path(other.path())).unwrap().member
        ),
        body: json!({"v": 1, "member": kc::load(&kc::key_path(other.path())).unwrap().member,
                     "pub": "x", "handle": "eve", "state": "revoked", "joined_at": "x",
                     "op": "revoke", "reason": "forged", "by": "alice"})
        .to_string(),
        refs: vec![],
        to: vec![],
    };
    log.push_str(&serde_json::to_string(&forged).unwrap());
    log.push('\n');

    let (h1, rows1) = replay_keys(&log);
    let (h2, rows2) = replay_keys(&log);
    assert_eq!(h1["KEY"], h2["KEY"]);
    assert_eq!(rows1, rows2);
    // bob's first key (rotated), eve (active, the forged revoke changed nothing), bob's new key (revoked).
    let states: Vec<&str> = rows1.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(states, ["rotated", "active", "revoked"]);
}
