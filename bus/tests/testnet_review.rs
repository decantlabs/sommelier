//! Tests of the second review of the test network: the privacy scan, the side store of blocks and
//! hand outs, reports that count only with a block and a hand out, the client checks of values that
//! come from a hub.

use sommelier::report::{find_private, scrub};

#[test]
fn privacy_scan_finds_what_the_review_named() {
    let samples = [
        (r"cache C:\Users\Jane Roe\AppData\Local\x\y.bin end", "Roe"),
        (
            r"sync C:\Users\jroe\OneDrive - Acme Corp\Games\x end",
            "Acme",
        ),
        (r"sync D:\OneDrive - Acme Corp\Games end", "Acme"),
        (r"wine Z:\home\jroe\.wine\drive_c end", "jroe"),
        (r"wine \home\jroe\.wine end", "jroe"),
        (
            r"owner S-1-5-21-1004336348-1177238915-682003330-1001 end",
            "1004336348",
        ),
        (
            r"dev {123e4567-e89b-12d3-a456-426614174000} end",
            "426614174000",
        ),
        ("adapter 00:1A:2B:3C:4D:5E up", "3C:4D"),
        ("adapter 00-1a-2b-3c-4d-5e up", "3c-4d"),
        ("peer 203.0.113.77 reset", "203.0.113"),
        ("peer 2001:db8:85a3:0:0:8a2e:370:7334 reset", "8a2e"),
        ("peer fe80::1ff:fe23:4567:890a reset", "890a"),
        ("access_token=abcd1234efgh5678ijkl", "abcd1234"),
        (r#"{"refresh_token": "abcd1234efgh5678ijkl"}"#, "abcd1234"),
        ("client_secret: abcd1234efgh5678ijkl", "abcd1234"),
    ];
    for (line, leak) in samples {
        assert!(!find_private(line).is_empty(), "no hit in: {line}");
        let clean = scrub(line);
        assert!(!clean.contains(leak), "`{leak}` left in: {clean}");
        assert!(find_private(&clean).is_empty(), "still private: {clean}");
    }
}

#[test]
fn privacy_scan_keeps_lines_that_only_look_alike() {
    let lines = [
        "listening on 127.0.0.1 port 7300",
        "bind 0.0.0.0 ok",
        "loopback ::1 ok",
        "std::vec::Vec created",
        "frame time 12:34:56 done",
        "driver 31.0.15.4601 loaded",
        "token expired at 12:30",
        "vk::Result::SUCCESS",
        "d3d12: DXGI_ERROR_DEVICE_REMOVED (0x887A0005)",
    ];
    for l in lines {
        assert!(find_private(l).is_empty(), "false hit: {l}");
        assert_eq!(scrub(l), l);
    }
}

// ------------------------------------------------------------------ the hub with a test network

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, sign_request};
use sommelier::msg::{Kind, Message, Role};
use sommelier::report::orders::{OrderRow, check_handed_order, query_orders};
use sommelier::report::side::Side;
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::stamp::ops::StampConfig;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

const B1: &str = "aaaaaaa1111";
const B2: &str = "bbbbbbb2222";

type Blocks = Arc<Mutex<BTreeMap<String, String>>>;

#[derive(Clone)]
struct Tokens {
    orch: String,
    alice: String,
}

struct TestHub {
    url: String,
    dir: PathBuf,
    _keep: Option<tempfile::TempDir>,
    tokens: Tokens,
    blocks: Blocks,
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
            token: self.tokens.orch.clone(),
        }
    }

    fn alice(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.tokens.alice.clone(),
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.dir.join("bus.jsonl")).unwrap_or_default()
    }

    fn messages(&self) -> Vec<Message> {
        self.log_text()
            .lines()
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect()
    }

    fn facts(&self, prefix: &str) -> Vec<Message> {
        self.messages()
            .into_iter()
            .filter(|m| m.kind == Kind::Fact && m.topic.starts_with(prefix))
            .collect()
    }

    fn orders(&self) -> Vec<OrderRow> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(self.dir.join("bus.jsonl"), dir.path().join("bus.jsonl")).unwrap();
        let mut store = sommelier::store::Store::open(dir.path()).unwrap();
        store
            .catch_up_from_log(&dir.path().join("bus.jsonl"))
            .unwrap();
        query_orders(store.connection()).unwrap()
    }

    fn order(&self, id: &str) -> OrderRow {
        self.orders().into_iter().find(|o| o.id == id).unwrap()
    }
}

async fn start_with(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&path.join("tokens.json")).unwrap();
    let mut hub = start_in(&path, Tokens { orch, alice }, tweak).await;
    hub._keep = Some(dir);
    hub
}

async fn start_in(path: &Path, tokens: Tokens, tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let blocks: Blocks = Arc::default();
    let mut cfg = HubConfig::new(path);
    cfg.verbose = false;
    cfg.join = sommelier::key::JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    cfg.sweep_every = Duration::from_millis(100);
    cfg.test.enabled = true;
    cfg.test.blocks = blocks.clone();
    tweak(&mut cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    let hub = TestHub {
        url,
        dir: path.to_path_buf(),
        _keep: None,
        tokens,
        blocks,
        task,
    };
    // The first tick files the settings that the requests read.
    for _ in 0..100 {
        let v = client::request(
            &hub.orch(),
            json!({"op": "test.next", "machine": machine("linux", "x"), "profiles": []}),
        )
        .await;
        if v.is_ok_and(|v| v["ok"] == true) {
            return hub;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the test network did not come up");
}

struct Tester {
    dir: tempfile::TempDir,
    opts: Opts,
    member: String,
    key: MemberKey,
}

async fn tester(hub: &TestHub, block: Option<&str>) -> Tester {
    let dir = tempfile::tempdir().unwrap();
    let out = kc::join(&hub.url, dir.path(), None).await.unwrap();
    let file = kc::load(&kc::key_path(dir.path())).unwrap();
    if let Some(b) = block {
        hub.blocks
            .lock()
            .unwrap()
            .insert(out.member.clone(), b.to_owned());
    }
    Tester {
        opts: kc::opts_for(&hub.url, dir.path()),
        member: out.member,
        key: file.key,
        dir,
    }
}

fn machine(os: &str, gpu: &str) -> Value {
    json!({"os": os, "os_build": "26100", "gpu": gpu, "gpu_memory_mb": 16384,
           "driver": "552.22", "memory_mb": 32768})
}

async fn refused(opts: &Opts, req: Value) -> String {
    let v = client::request(opts, req).await.expect("request");
    assert_eq!(v["ok"], false, "{v}");
    v["error"].as_str().unwrap().to_owned()
}

async fn new_order(hub: &TestHub, build: &str, needs: Value, wanted: u64) -> String {
    let v = client::request_ok(
        &hub.orch(),
        json!({"op": "test.order", "build": build, "channel": "main", "profile": "p1",
               "scenario": "boot", "needs": needs, "wanted": wanted}),
    )
    .await
    .unwrap();
    v["id"].as_str().unwrap().to_owned()
}

async fn next(opts: &Opts, os: &str, gpu: &str) -> Option<String> {
    let v = client::request_ok(
        opts,
        json!({"op": "test.next", "machine": machine(os, gpu), "profiles": ["p1"]}),
    )
    .await
    .unwrap();
    v["order"]["id"].as_str().map(str::to_owned)
}

fn report_for(order: &OrderRow, member: &str, outcome: Outcome) -> Report {
    Report {
        v: 1,
        run: order.id.clone(),
        member: member.into(),
        build: order.build.clone(),
        channel: order.channel.clone(),
        profile: order.profile.clone(),
        scenario: order.scenario.clone(),
        machine: serde_json::from_value::<MachineClass>(machine("windows", "RTX 4080")).unwrap(),
        outcome,
        first_error: (outcome == Outcome::Fails).then(|| "timeout".to_string()),
        metrics: BTreeMap::new(),
        secs_alive: 60,
        log_excerpt: "1 swapchain created\n".into(),
        at: 1_000,
        sig: None,
    }
}

fn signed(t: &Tester, r: &Report) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    let sig = sign_request(&t.key, &v);
    v["sig"] = json!(sig);
    v
}

/// `test.next` for a windows RTX 4080 machine, then `report.send`.
async fn hand_and_send(t: &Tester, order: &OrderRow, outcome: Outcome) -> Result<Value, String> {
    assert_eq!(
        next(&t.opts, "windows", "RTX 4080").await.as_deref(),
        Some(order.id.as_str())
    );
    client::request_ok(
        &t.opts,
        json!({"op": "report.send", "report": signed(t, &report_for(order, &t.member, outcome))}),
    )
    .await
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..80 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn state(hub: &TestHub) -> Value {
    let url = kc::http_url(&hub.url, "/api/state").unwrap();
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", format!("Bearer {}", hub.tokens.orch))
        .send()
        .await
        .unwrap();
    serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap()
}

// ------------------------------------------------------------------ report.send needs a hand out

#[tokio::test]
async fn a_report_for_an_order_that_was_not_handed_out_is_refused() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    let report = signed(&t, &report_for(&order, &t.member, Outcome::Works));
    let e = refused(&t.opts, json!({"op": "report.send", "report": report})).await;
    assert!(e.contains("not handed to you"), "{e}");
    assert!(hub.facts("report/").is_empty());
    // After test.next the same report goes through, once.
    hand_and_send(&t, &order, Outcome::Works).await.unwrap();
    assert_eq!(hub.facts("report/").len(), 1);
    // A token member is held to it too.
    let alice = report_for(&order, "alice", Outcome::Works);
    let e = refused(
        &hub.alice(),
        json!({"op": "report.send", "report": serde_json::to_value(&alice).unwrap()}),
    )
    .await;
    assert!(e.contains("not handed to you"), "{e}");
}

#[tokio::test]
async fn a_report_must_come_from_the_machine_class_the_order_was_handed_to() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    // Handed to a windows RTX 4080, the report claims an invented card for a cell nobody else has.
    assert_eq!(
        next(&t.opts, "windows", "RTX 4080").await.as_deref(),
        Some(id.as_str())
    );
    let mut r = report_for(&order, &t.member, Outcome::Works);
    r.machine = serde_json::from_value(machine("windows", "Imaginary GPU 9000")).unwrap();
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &r)}),
    )
    .await;
    assert!(e.contains("another machine class"), "{e}");
    // The hand out is not used up by the refusal.
    let r = report_for(&order, &t.member, Outcome::Works);
    client::request_ok(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &r)}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_machine_of_a_report_must_fit_the_needs_of_its_order() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({"gpu": "rx 7800"}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    assert_eq!(
        next(&t.opts, "windows", "AMD Radeon RX 7800 XT")
            .await
            .as_deref(),
        Some(id.as_str())
    );
    // The machine in the report is an RTX 4080: it does not fit.
    let r = report_for(&order, &t.member, Outcome::Works);
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &r)}),
    )
    .await;
    assert!(e.contains("does not fit"), "{e}");
    assert!(hub.facts("report/").is_empty());
}

#[tokio::test]
async fn sends_of_one_report_at_once_write_one_fact() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    assert_eq!(
        next(&t.opts, "windows", "RTX 4080").await.as_deref(),
        Some(id.as_str())
    );
    let report = signed(&t, &report_for(&order, &t.member, Outcome::Works));
    let send = || {
        client::request(
            &t.opts,
            json!({"op": "report.send", "report": report.clone()}),
        )
    };
    let (a, b, c) = tokio::join!(send(), send(), send());
    let oks = [a, b, c]
        .into_iter()
        .filter(|v| v.as_ref().is_ok_and(|v| v["ok"] == true))
        .count();
    assert_eq!(oks, 1, "exactly one send may win");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.facts("report/").len(), 1);
}

#[tokio::test]
async fn hand_outs_survive_a_restart_of_the_hub() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    assert_eq!(
        next(&t.opts, "windows", "RTX 4080").await.as_deref(),
        Some(id.as_str())
    );
    // Stop the hub and start another one on the same folder.
    hub.task.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let again = start_in(&hub.dir, hub.tokens.clone(), |_| {}).await;
    again
        .blocks
        .lock()
        .unwrap()
        .insert(t.member.clone(), "blk-a".into());
    let opts = kc::opts_for(&again.url, t.dir.path());
    let r = report_for(&order, &t.member, Outcome::Works);
    client::request_ok(
        &opts,
        json!({"op": "report.send", "report": signed(&t, &r)}),
    )
    .await
    .unwrap();
}

// ------------------------------------------------------------------ reports without a block are hints

#[tokio::test]
async fn reports_without_a_block_settle_nothing_and_the_state_says_so() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    for _ in 0..4 {
        let t = tester(&hub, None).await;
        hand_and_send(&t, &order, Outcome::Works).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(hub.order(&id).state, "open", "four hints settled an order");
    let s = state(&hub).await;
    let row = &s["matrix"].as_array().unwrap()[0];
    assert_eq!(row["state"], "hint_works");
    assert_eq!(
        (row["reports"].as_u64(), row["with_block"].as_u64()),
        (Some(4), Some(0))
    );
    assert_eq!(s["matrix_blocks"]["kept"], false);
    assert!(
        s["matrix_blocks"]["text"]
            .as_str()
            .unwrap()
            .contains("keeps no address blocks")
    );
}

#[tokio::test]
async fn keys_without_a_block_never_cause_a_regression() {
    let hub = start_with(|_| {}).await;
    let old = new_order(&hub, B1, json!({}), 3).await;
    let old_order = hub.order(&old);
    for n in 0..3 {
        let t = tester(&hub, Some(&format!("blk-{n}"))).await;
        hand_and_send(&t, &old_order, Outcome::Works).await.unwrap();
    }
    until("the old order to settle", || {
        hub.order(&old).state == "settled"
    })
    .await;
    // Three fresh keys without a block say the newer build fails.
    let new = new_order(&hub, B2, json!({}), 3).await;
    let new_order_row = hub.order(&new);
    for _ in 0..3 {
        let t = tester(&hub, None).await;
        hand_and_send(&t, &new_order_row, Outcome::Fails)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let alerts: Vec<_> = hub
        .messages()
        .into_iter()
        .filter(|m| m.kind == Kind::Alert && m.topic.starts_with("alert/MATRIX_REGRESSION"))
        .collect();
    assert!(
        alerts.is_empty(),
        "three keys without a block raised an alert"
    );
    assert_eq!(hub.order(&new).state, "open");
}

#[tokio::test]
async fn the_block_is_the_one_of_the_moment_the_report_arrived() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    // Two keys of one owner report from one address block.
    let a = tester(&hub, Some("blk-same")).await;
    let b = tester(&hub, Some("blk-same")).await;
    hand_and_send(&a, &order, Outcome::Works).await.unwrap();
    hand_and_send(&b, &order, Outcome::Works).await.unwrap();
    let c = tester(&hub, Some("blk-c")).await;
    hand_and_send(&c, &order, Outcome::Works).await.unwrap();
    // Afterwards the keys reconnect from new addresses. The reports keep the block they arrived with.
    hub.blocks
        .lock()
        .unwrap()
        .insert(a.member.clone(), "blk-new-a".into());
    hub.blocks
        .lock()
        .unwrap()
        .insert(b.member.clone(), "blk-new-b".into());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        hub.order(&id).state,
        "open",
        "a change of address made two keys look independent"
    );
}

#[tokio::test]
async fn the_block_stays_out_of_the_log_and_the_state() {
    let hub = start_with(|_| {}).await;
    let id = new_order(&hub, B1, json!({}), 1).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-secret-block")).await;
    hand_and_send(&t, &order, Outcome::Works).await.unwrap();
    until("the order to settle", || hub.order(&id).state == "settled").await;
    assert!(!hub.log_text().contains("blk-secret-block"));
    let s = state(&hub).await.to_string();
    assert!(!s.contains("blk-secret-block"));
    // It sits in its own file, not in state.db.
    let side = Side::open_existing(&hub.dir).expect("a side store");
    assert!(
        side.blocks()
            .unwrap()
            .values()
            .any(|b| b == "blk-secret-block")
    );
    let main = std::fs::read(hub.dir.join("state.db")).unwrap();
    assert!(
        !main.windows(16).any(|w| w == &b"blk-secret-block"[..]),
        "the block is in state.db"
    );
}

#[tokio::test]
async fn hand_outs_that_no_longer_count_are_dropped_on_the_tick() {
    let hub = start_with(|c| c.test.hand_hold_secs = 1).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let t = tester(&hub, Some("blk-a")).await;
    assert_eq!(
        next(&t.opts, "windows", "RTX 4080").await.as_deref(),
        Some(id.as_str())
    );
    let side = Side::open_existing(&hub.dir).unwrap();
    assert_eq!(side.handed_to(&t.member).unwrap().len(), 1);
    // The member never asks again. The tick still forgets the hand out.
    until("the hand out to be dropped", || {
        side.handed_to(&t.member).unwrap().is_empty()
    })
    .await;
}

// ------------------------------------------------------------------ run_agreed needs a member of tier 1

fn stamps_of(hub: &TestHub, kind: &str) -> Vec<Value> {
    hub.facts("stamp/")
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.body).ok())
        .filter(|v| v["kind"] == kind)
        .collect()
}

#[tokio::test]
async fn a_ring_of_fresh_keys_gets_no_run_agreed_and_a_member_of_tier_one_lifts_the_cell() {
    let hub = start_with(|c| {
        c.stamp = StampConfig {
            enabled: true,
            ..StampConfig::default()
        };
    })
    .await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    for n in 0..3 {
        let t = tester(&hub, Some(&format!("blk-ring-{n}"))).await;
        hand_and_send(&t, &order, Outcome::Works).await.unwrap();
    }
    until("the order to settle", || hub.order(&id).state == "settled").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        stamps_of(&hub, "run_agreed").is_empty(),
        "three fresh keys lifted themselves"
    );

    // A second order in which the token member alice, tier 2, agrees with two fresh keys.
    let id2 = new_order(&hub, B2, json!({}), 3).await;
    let order2 = hub.order(&id2);
    hub.blocks
        .lock()
        .unwrap()
        .insert("alice".into(), "blk-alice".into());
    assert_eq!(
        next(&hub.alice(), "windows", "RTX 4080").await.as_deref(),
        Some(id2.as_str())
    );
    client::request_ok(
        &hub.alice(),
        json!({"op": "report.send", "report": serde_json::to_value(report_for(&order2, "alice", Outcome::Works)).unwrap()}),
    )
    .await
    .unwrap();
    for n in 0..2 {
        let t = tester(&hub, Some(&format!("blk-fresh-{n}"))).await;
        hand_and_send(&t, &order2, Outcome::Works).await.unwrap();
    }
    until("run_agreed stamps", || {
        stamps_of(&hub, "run_agreed").len() >= 3
    })
    .await;
}

// ------------------------------------------------------------------ values that come from a hub

#[test]
fn every_value_of_an_order_from_a_hub_is_checked_like_the_hub_checks_it() {
    let good = ("ro12", B1, "main", "p1", "boot");
    assert!(check_handed_order(good.0, good.1, good.2, good.3, good.4).is_ok());
    let long = "x".repeat(65);
    let bad_ids = [
        "x/../../../../dir",
        "..",
        "a..b",
        "a/b",
        r"a\b",
        "",
        long.as_str(),
        "a b",
        "a;b",
    ];
    for bad in bad_ids {
        assert!(
            check_handed_order(bad, good.1, good.2, good.3, good.4).is_err(),
            "id {bad}"
        );
        assert!(
            check_handed_order(good.0, good.1, bad, good.3, good.4).is_err(),
            "channel {bad}"
        );
        assert!(
            check_handed_order(good.0, good.1, good.2, bad, good.4).is_err(),
            "profile {bad}"
        );
        assert!(
            check_handed_order(good.0, good.1, good.2, good.3, bad).is_err(),
            "scenario {bad}"
        );
    }
    let long_hex = "a".repeat(65);
    for bad in [
        "",
        "xyz",
        "ABCDEF1",
        "abc12",
        "--help",
        "a/b",
        long_hex.as_str(),
    ] {
        assert!(
            check_handed_order(good.0, bad, good.2, good.3, good.4).is_err(),
            "build {bad}"
        );
    }
}

#[test]
fn the_run_script_refuses_a_label_that_leads_out_of_the_runs_folder() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../swarm/run-title.sh");
    if std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("no bash on this machine: the label check of the script is not run");
        return;
    }
    let runs = tempfile::tempdir().unwrap();
    for label in ["x/../../../../dir", "a/b", r"a\b", "x..y", ".."] {
        let out = std::process::Command::new("bash")
            .arg(&script)
            .args(["/nonexistent", "1", label])
            .env("SOMMELIER_RUNS", runs.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "label {label}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains("label"), "label {label}: {said}");
    }
    assert_eq!(
        std::fs::read_dir(runs.path()).unwrap().count(),
        0,
        "the script made something for a bad label"
    );
}

#[test]
fn sommelier_test_tells_what_a_report_holds_and_that_it_is_public() {
    let text = sommelier::cli::test::NOTICE;
    for word in ["member id", "machine", "log", "public"] {
        assert!(text.contains(word), "{word}");
    }
}
