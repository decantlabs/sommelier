//! The test network end to end against a real hub on a free local port: run orders, `test.next`,
//! `report.send`, settling, MATRIX_REGRESSION and the CLI round with fakes for every outside effect.
//! No network beyond localhost, no title, no vendor login.
//!
//! The front door answers no address block in this build, so the tests name blocks through the
//! hook `TestConfig.blocks`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, sign_request};
use sommelier::msg::{Kind, Message, Role};
use sommelier::report::orders::{OrderRow, query_orders};
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::report::{ReportRow, query_reports};
use sommelier::store::Store;
use sommelier::tester::profile::Profile;
use sommelier::tester::round::{
    Effects, RoundConfig, RoundOutcome, ScenarioOutput, ScenarioRun, run_loop, run_round,
};
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

const BITS: u32 = 8;
const B1: &str = "aaaaaaa1111";
const B2: &str = "bbbbbbb2222";

type Blocks = Arc<Mutex<BTreeMap<String, String>>>;

struct TestHub {
    url: String,
    dir: tempfile::TempDir,
    orch: String,
    alice: String,
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
            token: self.orch.clone(),
        }
    }

    fn alice(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.alice.clone(),
        }
    }

    fn messages(&self) -> Vec<Message> {
        std::fs::read_to_string(self.dir.path().join("bus.jsonl"))
            .unwrap_or_default()
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

    fn alerts(&self) -> Vec<Message> {
        self.messages()
            .into_iter()
            .filter(|m| m.kind == Kind::Alert && m.topic.starts_with("alert/MATRIX_REGRESSION"))
            .collect()
    }

    /// The two tables as a replay of the log gives them.
    fn tables(&self) -> (Vec<OrderRow>, Vec<ReportRow>) {
        replay(&self.dir.path().join("bus.jsonl"))
    }

    fn order(&self, id: &str) -> OrderRow {
        self.tables()
            .0
            .into_iter()
            .find(|o| o.id == id)
            .expect("order")
    }
}

fn replay(log: &Path) -> (Vec<OrderRow>, Vec<ReportRow>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(log, dir.path().join("bus.jsonl")).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .catch_up_from_log(&dir.path().join("bus.jsonl"))
        .unwrap();
    (
        query_orders(store.connection()).unwrap(),
        query_reports(store.connection()).unwrap(),
    )
}

async fn start() -> TestHub {
    start_with(|_| {}).await
}

async fn start_with(tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let blocks: Blocks = Arc::default();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.join = sommelier::key::JoinRules {
        enabled: true,
        proof_of_work_bits: BITS,
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
        dir,
        orch,
        alice,
        blocks,
        task,
    };
    // The first tick files the settings that the requests read.
    for _ in 0..100 {
        let v = client::request(
            &hub.orch(),
            json!({"op": "test.next", "machine": machine("linux", "x"), "profiles": []}),
        )
        .await
        .unwrap();
        if v["ok"] == true {
            return hub;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the test network did not come up");
}

struct Tester {
    _dir: tempfile::TempDir,
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
        _dir: dir,
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

async fn next(opts: &Opts, os: &str, gpu: &str, profiles: &[&str]) -> Option<String> {
    let v = client::request_ok(
        opts,
        json!({"op": "test.next", "machine": machine(os, gpu), "profiles": profiles}),
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

/// Asks `test.next` as the tester does, so the order of the report is handed to this member. A
/// report counts only for an order that `test.next` handed out.
async fn handed(opts: &Opts, report: &Value) {
    let got = client::request_ok(
        opts,
        json!({"op": "test.next", "machine": report["machine"], "profiles": [report["profile"]]}),
    )
    .await
    .unwrap();
    assert_eq!(
        got["order"]["id"], report["run"],
        "test.next handed another order: {got}"
    );
}

async fn send(opts: &Opts, report: Value) -> Result<Value, String> {
    handed(opts, &report).await;
    client::request_ok(opts, json!({"op": "report.send", "report": report})).await
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

// ---------------------------------------------------------------- orders and test.next

#[tokio::test]
async fn orders_are_for_the_orchestrator_and_close() {
    let hub = start().await;
    let t = tester(&hub, None).await;
    let req = json!({"op": "test.order", "build": B1, "channel": "main", "profile": "p1", "scenario": "boot"});
    assert!(refused(&t.opts, req.clone()).await.contains("tier 0"));
    assert!(
        refused(&hub.alice(), req.clone())
            .await
            .contains("orchestrator")
    );
    let id = new_order(&hub, B1, json!({}), 3).await;
    assert!(hub.facts("runorder/").len() == 1);
    // The same order twice while it is open.
    assert!(refused(&hub.orch(), req).await.contains("open already"));
    // Bad fields.
    let bad = json!({"op": "test.order", "build": "xyz", "channel": "main", "profile": "p1", "scenario": "boot"});
    assert!(refused(&hub.orch(), bad).await.contains("build"));
    let bad = json!({"op": "test.order", "build": B2, "channel": "nightly", "profile": "p1", "scenario": "boot"});
    assert!(refused(&hub.orch(), bad).await.contains("channel"));
    let bad = json!({"op": "test.order", "build": B2, "channel": "main", "profile": "p1", "scenario": "boot", "needs": {"cpu": "x"}});
    assert!(refused(&hub.orch(), bad).await.contains("needs.cpu"));

    let state = state(&hub).await;
    assert_eq!(state["run_orders"].as_array().unwrap().len(), 1);
    assert_eq!(state["run_orders"][0]["id"], id.as_str());
    assert_eq!(state["run_orders"][0]["reports"], 0);

    assert!(
        refused(&hub.alice(), json!({"op": "test.close", "id": id}))
            .await
            .contains("orchestrator")
    );
    client::request_ok(&hub.orch(), json!({"op": "test.close", "id": id}))
        .await
        .unwrap();
    assert_eq!(hub.order(&id).state, "closed");
    assert!(
        refused(&hub.orch(), json!({"op": "test.close", "id": id}))
            .await
            .contains("closed already")
    );
    assert!(
        refused(&hub.orch(), json!({"op": "test.close", "id": "nope"}))
            .await
            .contains("no run order")
    );
    assert!(state_open(&hub).await.is_empty());
}

async fn state(hub: &TestHub) -> Value {
    let url = kc::http_url(&hub.url, "/api/state").unwrap();
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", format!("Bearer {}", hub.orch))
        .send()
        .await
        .unwrap();
    serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap()
}

async fn state_open(hub: &TestHub) -> Vec<Value> {
    state(hub).await["run_orders"].as_array().cloned().unwrap()
}

#[tokio::test]
async fn test_next_matches_needs_profiles_and_never_repeats() {
    let hub = start().await;
    let t = tester(&hub, None).await;
    // Nothing yet. A tier 0 member may ask.
    let v = client::request_ok(
        &t.opts,
        json!({"op": "test.next", "machine": machine("windows", "RTX 4080"), "profiles": ["p1"]}),
    )
    .await
    .unwrap();
    assert!(v["order"].is_null());
    assert_eq!(v["reason"], "nothing to test now");

    let linux = new_order(&hub, B1, json!({"os": "linux"}), 3).await;
    let amd = new_order(
        &hub,
        B1.replace('a', "c").as_str(),
        json!({"gpu": "rx 7800"}),
        3,
    )
    .await;
    // Nothing fits a windows RTX machine.
    assert_eq!(
        next(&t.opts, "windows", "NVIDIA GeForce RTX 4080", &["p1"]).await,
        None
    );
    // The os and the gpu need match.
    assert_eq!(
        next(&t.opts, "linux", "NVIDIA GeForce RTX 4080", &["p1"]).await,
        Some(linux.clone())
    );
    assert_eq!(
        next(&t.opts, "windows", "AMD Radeon RX 7800 XT", &["p1"]).await,
        Some(amd)
    );
    // The profile filter.
    assert_eq!(next(&t.opts, "linux", "RTX 4080", &["other"]).await, None);
    assert_eq!(next(&t.opts, "linux", "RTX 4080", &[]).await, None);

    // The answer holds the order and nothing about other testers.
    let v = client::request_ok(
        &t.opts,
        json!({"op": "test.next", "machine": machine("linux", "RTX 4080"), "profiles": ["p1"]}),
    )
    .await
    .unwrap();
    let text = v["order"].to_string();
    assert!(
        !text.contains("member") && !text.contains("reports"),
        "{text}"
    );
    assert_eq!(v["order"]["build"], B1);
    assert_eq!(v["order"]["wanted"], 3);

    // Never the same order twice for one member: after a report it is gone.
    let order = hub.order(&linux);
    let mut r = report_for(&order, &t.member, Outcome::Works);
    r.machine = serde_json::from_value(machine("linux", "RTX 4080")).unwrap();
    send(&t.opts, signed(&t, &r)).await.unwrap();
    assert_eq!(next(&t.opts, "linux", "RTX 4080", &["p1"]).await, None);
}

#[tokio::test]
async fn test_next_gives_the_order_with_the_fewest_reports_first() {
    let hub = start().await;
    let a = tester(&hub, Some("blk-a")).await;
    let b = tester(&hub, Some("blk-b")).await;
    let first = new_order(&hub, B1, json!({}), 3).await;
    let second = new_order(&hub, B2, json!({}), 3).await;
    // Equal counts: the older order first.
    assert_eq!(
        next(&a.opts, "windows", "RTX 4080", &["p1"]).await,
        Some(first.clone())
    );
    // A report on the first order moves the second to the front for the others.
    let r = report_for(&hub.order(&first), &a.member, Outcome::Works);
    send(&a.opts, signed(&a, &r)).await.unwrap();
    assert_eq!(
        next(&b.opts, "windows", "RTX 4080", &["p1"]).await,
        Some(second.clone())
    );
    // A token member gets one too.
    assert_eq!(
        next(&hub.alice(), "windows", "RTX 4080", &["p1"]).await,
        Some(second)
    );
}

#[tokio::test]
async fn a_member_holds_only_so_many_handed_orders() {
    let hub = start_with(|c| c.test.max_handed_per_member = 1).await;
    let a = tester(&hub, Some("blk-a")).await;
    let b = tester(&hub, Some("blk-b")).await;
    let o1 = new_order(&hub, B1, json!({}), 3).await;
    let o2 = new_order(&hub, B2, json!({}), 3).await;
    assert_eq!(
        next(&a.opts, "windows", "RTX 4080", &["p1"]).await,
        Some(o1.clone())
    );
    // Another tester answers o1, so o2 has the fewest reports now.
    let r = report_for(&hub.order(&o1), &b.member, Outcome::Works);
    send(&b.opts, signed(&b, &r)).await.unwrap();
    // Tester b holds nothing and gets o2. Tester a holds o1 and is held to it.
    assert_eq!(
        next(&b.opts, "windows", "RTX 4080", &["p1"]).await,
        Some(o2)
    );
    assert_eq!(
        next(&a.opts, "windows", "RTX 4080", &["p1"]).await,
        Some(o1)
    );
}

// ---------------------------------------------------------------- report.send

#[tokio::test]
async fn report_send_refuses_what_it_must_and_takes_a_good_one() {
    let hub = start().await;
    let t = tester(&hub, Some("blk-a")).await;
    let other = tester(&hub, Some("blk-b")).await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let closed = new_order(&hub, B2, json!({}), 3).await;
    client::request_ok(&hub.orch(), json!({"op": "test.close", "id": closed}))
        .await
        .unwrap();
    let order = hub.order(&id);
    let good = report_for(&order, &t.member, Outcome::Works);

    // Unsigned by a key member.
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": serde_json::to_value(&good).unwrap()}),
    )
    .await;
    assert!(e.contains("report.sig"), "{e}");
    // Signed by another key.
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&other, &good)}),
    )
    .await;
    assert!(e.contains("report.sig is wrong"), "{e}");
    // A field changed after signing.
    let mut changed = signed(&t, &good);
    changed["secs_alive"] = json!(61);
    let e = refused(&t.opts, json!({"op": "report.send", "report": changed})).await;
    assert!(e.contains("report.sig is wrong"), "{e}");
    // For another member.
    let theirs = report_for(&order, &other.member, Outcome::Works);
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&other, &theirs)}),
    )
    .await;
    assert!(e.contains("report.member"), "{e}");
    // An unknown order and a closed one.
    let mut unknown = good.clone();
    unknown.run = "ro999".into();
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &unknown)}),
    )
    .await;
    assert!(e.contains("no run order"), "{e}");
    let mut late = good.clone();
    late.run = closed;
    late.build = B2.into();
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &late)}),
    )
    .await;
    assert!(e.contains("is closed"), "{e}");
    // A report that does not answer the order.
    let mut wrong = good.clone();
    wrong.build = "dddddddd".into();
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &wrong)}),
    )
    .await;
    assert!(e.contains("does not answer"), "{e}");
    // A private path in the excerpt.
    let mut private = good.clone();
    private.log_excerpt = r"opened C:\Users\someone\game.log".into();
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &private)}),
    )
    .await;
    assert!(e.contains("report.log_excerpt"), "{e}");
    // An excerpt over the limit of the hub.
    let mut big = good.clone();
    big.log_excerpt = "x".repeat(3000);
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &big)}),
    )
    .await;
    assert!(e.contains("log_excerpt"), "{e}");
    assert!(hub.facts("report/").is_empty());
    assert!(hub.tables().1.is_empty());

    // The good one: one FACT and one row.
    let v = send(&t.opts, signed(&t, &good)).await.unwrap();
    assert_eq!(v["run"], id.as_str());
    let facts = hub.facts("report/");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].from, "hub");
    assert_eq!(facts[0].role, Role::Hub);
    assert_eq!(facts[0].topic, format!("report/{id}"));
    let rows = hub.tables().1;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].member, t.member);
    assert_eq!(rows[0].outcome, "works");
    // Twice.
    let e = refused(
        &t.opts,
        json!({"op": "report.send", "report": signed(&t, &good)}),
    )
    .await;
    assert!(e.contains("already"), "{e}");
    assert_eq!(hub.facts("report/").len(), 1);
    // The log and the state show no address.
    let log = std::fs::read_to_string(hub.dir.path().join("bus.jsonl")).unwrap();
    assert!(!log.contains("127.0.0.1"));
}

#[tokio::test]
async fn a_token_member_needs_no_signature_but_must_be_the_sender() {
    let hub = start().await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let mine = report_for(&order, "alice", Outcome::Works);
    let e = refused(&hub.alice(), json!({"op": "report.send", "report": serde_json::to_value(report_for(&order, "orch", Outcome::Works)).unwrap()})).await;
    assert!(e.contains("report.member"), "{e}");
    send(&hub.alice(), serde_json::to_value(&mine).unwrap())
        .await
        .unwrap();
    assert_eq!(hub.tables().1.len(), 1);
}

#[tokio::test]
async fn the_network_is_off_by_default() {
    assert!(!sommelier::report::ops::TestConfig::default().enabled);
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(100);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    let opts = Opts { url, token: orch };
    let mut said = String::new();
    for _ in 0..50 {
        said = refused(
            &opts,
            json!({"op": "test.next", "machine": machine("linux", "x"), "profiles": []}),
        )
        .await;
        if said.contains("switched off") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(said.contains("switched off"), "{said}");
    task.abort();
}

// ---------------------------------------------------------------- settling and regressions

#[tokio::test]
async fn three_testers_in_three_blocks_settle_an_order() {
    let hub = start().await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    for n in 0..3 {
        let t = tester(&hub, Some(&format!("blk-{n}"))).await;
        send(
            &t.opts,
            signed(&t, &report_for(&order, &t.member, Outcome::Works)),
        )
        .await
        .unwrap();
    }
    until("the order to settle", || hub.order(&id).state == "settled").await;
    assert_eq!(hub.facts("runorder/").len(), 2);
    // A settled order takes no more reports.
    let late = tester(&hub, Some("blk-9")).await;
    let e = refused(&late.opts, json!({"op": "report.send", "report": signed(&late, &report_for(&order, &late.member, Outcome::Works))})).await;
    assert!(e.contains("settled"), "{e}");
}

#[tokio::test]
async fn two_keys_in_one_block_do_not_settle_an_order() {
    let hub = start().await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    for block in ["blk-same", "blk-same", "blk-other"] {
        let t = tester(&hub, Some(block)).await;
        send(
            &t.opts,
            signed(&t, &report_for(&order, &t.member, Outcome::Works)),
        )
        .await
        .unwrap();
    }
    // Several ticks go by.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(hub.order(&id).state, "open");
    // A third owner settles it.
    let t = tester(&hub, Some("blk-third")).await;
    send(
        &t.opts,
        signed(&t, &report_for(&order, &t.member, Outcome::Works)),
    )
    .await
    .unwrap();
    until("the order to settle", || hub.order(&id).state == "settled").await;
}

#[tokio::test]
async fn a_newer_build_that_fails_raises_one_matrix_regression() {
    let hub = start().await;
    let old = new_order(&hub, B1, json!({}), 3).await;
    let old_order = hub.order(&old);
    for n in 0..3 {
        let t = tester(&hub, Some(&format!("blk-{n}"))).await;
        send(
            &t.opts,
            signed(&t, &report_for(&old_order, &t.member, Outcome::Works)),
        )
        .await
        .unwrap();
    }
    until("the old order to settle", || {
        hub.order(&old).state == "settled"
    })
    .await;
    assert!(hub.alerts().is_empty());

    let new = new_order(&hub, B2, json!({}), 3).await;
    let new_order_row = hub.order(&new);
    for n in 3..6 {
        let t = tester(&hub, Some(&format!("blk-{n}"))).await;
        send(
            &t.opts,
            signed(&t, &report_for(&new_order_row, &t.member, Outcome::Fails)),
        )
        .await
        .unwrap();
    }
    until("the alert", || !hub.alerts().is_empty()).await;
    // More ticks do not raise it again.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let alerts = hub.alerts();
    assert_eq!(alerts.len(), 1);
    let body: Value = serde_json::from_str(&alerts[0].body).unwrap();
    assert_eq!(body["code"], "MATRIX_REGRESSION");
    assert_eq!(body["data"]["last_good"], B1);
    assert_eq!(body["data"]["first_bad"], B2);
    assert_eq!(body["data"]["profile"], "p1");
}

#[tokio::test]
async fn an_order_that_stays_open_too_long_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(100);
    cfg.test.enabled = true;
    cfg.test.order_ttl_secs = 0;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    let opts = Opts { url, token: orch };
    let req = json!({"op": "test.order", "build": B1, "channel": "main", "profile": "p1", "scenario": "boot"});
    let mut id = None;
    for _ in 0..50 {
        if let Ok(v) = client::request_ok(&opts, req.clone()).await {
            id = v["id"].as_str().map(str::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let id = id.expect("order");
    for _ in 0..80 {
        let (orders, _) = replay(&dir.path().join("bus.jsonl"));
        if orders.iter().any(|o| o.id == id && o.state == "closed") {
            task.abort();
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    task.abort();
    panic!("the old order stayed open");
}

// ---------------------------------------------------------------- facts and replay

#[tokio::test]
async fn a_client_fact_on_report_or_runorder_changes_nothing_and_replay_agrees() {
    let hub = start().await;
    let id = new_order(&hub, B1, json!({}), 3).await;
    let order = hub.order(&id);
    let t = tester(&hub, Some("blk-a")).await;
    send(
        &t.opts,
        signed(&t, &report_for(&order, &t.member, Outcome::Works)),
    )
    .await
    .unwrap();
    let before = hub.tables();

    // Members with a token try to write the facts themselves.
    let forged_order = OrderRow {
        id: "ro77".into(),
        ..order.clone()
    };
    let forged_report = report_for(&order, "alice", Outcome::Fails);
    for opts in [hub.alice(), hub.orch()] {
        for (topic, body) in [
            ("runorder/ro77", forged_order.fact_body("open")),
            (&format!("runorder/{id}") as &str, order.fact_body("closed")),
            (
                &format!("report/{id}") as &str,
                serde_json::to_string(&forged_report).unwrap(),
            ),
        ] {
            // The hub may refuse the message or may keep it in the log. Either way no table moves.
            let _ = client::request(
                &opts,
                json!({"op": "send", "kind": "FACT", "topic": topic, "body": body}),
            )
            .await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = hub.tables();
    assert_eq!(before, after);
    assert_eq!(after.0.len(), 1);
    assert_eq!(after.0[0].state, "open");
    assert_eq!(after.1.len(), 1);

    // A replay of the same log twice gives the same tables.
    let log = hub.dir.path().join("bus.jsonl");
    assert_eq!(replay(&log), replay(&log));
    assert_eq!(replay(&log), after);
}

// ---------------------------------------------------------------- the CLI round

/// Fakes for every outside effect of a round.
struct Fake {
    verify: Result<(), String>,
    scenario: Result<ScenarioOutput, String>,
    ran: Cell<usize>,
    verified: Cell<usize>,
    paused: Cell<bool>,
    sleeps: Cell<usize>,
    pause_after_sleeps: usize,
    seen: RefCell<Vec<String>>,
    commits: RefCell<Vec<String>>,
    released: RefCell<Vec<PathBuf>>,
    labels: RefCell<Vec<String>>,
}

impl Fake {
    fn new(verify: Result<(), String>, scenario: Result<ScenarioOutput, String>) -> Fake {
        Fake {
            verify,
            scenario,
            ran: Cell::new(0),
            verified: Cell::new(0),
            paused: Cell::new(false),
            sleeps: Cell::new(0),
            pause_after_sleeps: usize::MAX,
            seen: RefCell::new(vec![]),
            commits: RefCell::new(vec![]),
            released: RefCell::new(vec![]),
            labels: RefCell::new(vec![]),
        }
    }
}

impl Effects for Fake {
    fn now(&self) -> u64 {
        1_700_000_000
    }
    fn paused(&self) -> bool {
        self.paused.get()
    }
    fn machine(&self, gpu: Option<&str>) -> MachineClass {
        serde_json::from_value(machine("windows", gpu.unwrap_or("rtx 4080"))).unwrap()
    }
    fn prepare_build(&self, archive: &Path, repo: &str, commit: &str) -> Result<PathBuf, String> {
        self.verified.set(self.verified.get() + 1);
        self.seen.borrow_mut().push(repo.to_owned());
        self.commits.borrow_mut().push(commit.to_owned());
        self.verify.clone()?;
        Ok(archive.with_file_name("unpacked"))
    }
    fn release_build(&self, folder: &Path) {
        self.released.borrow_mut().push(folder.to_path_buf());
    }
    fn run_scenario(&self, run: &ScenarioRun<'_>) -> Result<ScenarioOutput, String> {
        assert!(
            run.build.ends_with("unpacked"),
            "the run uses the unpacked folder"
        );
        self.ran.set(self.ran.get() + 1);
        self.labels.borrow_mut().push(run.label.to_owned());
        self.scenario.clone()
    }
    fn sleep(&self, _secs: u64) {
        self.sleeps.set(self.sleeps.get() + 1);
        if self.sleeps.get() >= self.pause_after_sleeps {
            self.paused.set(true);
        }
    }
}

fn works_output() -> ScenarioOutput {
    ScenarioOutput {
        summary: vec!["fps_median: 58.5".into(), "frames: 1200 at 55 s".into()],
        log: "1 swapchain created\n10 present probe: frame 5 (thread 1)\n20 menu ready\n".into(),
        secs_alive: 60,
        ended_by_itself: false,
    }
}

fn fails_output() -> ScenarioOutput {
    ScenarioOutput {
        summary: vec![],
        log: "1 swapchain created\n5 Removing Device\n".into(),
        secs_alive: 8,
        ended_by_itself: true,
    }
}

/// A title file, a profile that fits it and a build file.
struct Files {
    dir: tempfile::TempDir,
    profile: Profile,
}

impl Files {
    fn new() -> Files {
        let dir = tempfile::tempdir().unwrap();
        let title = b"a title that is not one";
        std::fs::write(dir.path().join("title.exe"), title).unwrap();
        std::fs::write(dir.path().join("decant.bin"), b"a build").unwrap();
        let sha: String = Sha256::digest(title)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let text = format!(
            "id = \"p1\"\nname = \"Example\"\n[executable]\nfile = \"title.exe\"\nsha256 = \"{sha}\"\nsize = {}\n\
             [[scenarios]]\nid = \"boot\"\nseconds = 60\nmetrics = [\"fps_median\", \"frames\"]\n\
             [[scenarios.milestones]]\nid = \"window_up\"\npattern = \"swapchain created\"\nby = 20\n\
             [[scenarios.milestones]]\nid = \"menu\"\npattern = \"menu ready\"\nby = 45\n",
            title.len()
        );
        Files {
            profile: Profile::parse(&text).unwrap(),
            dir,
        }
    }

    fn title(&self) -> PathBuf {
        self.dir.path().join("title.exe")
    }

    fn config(&self, t: &Tester, hub: &TestHub) -> RoundConfig {
        RoundConfig {
            opts: Opts {
                url: hub.url.clone(),
                token: t.opts.token.clone(),
            },
            key: Some(MemberKey::from_pkcs8(t.key.pkcs8()).unwrap()),
            profile: self.profile.clone(),
            title: self.title(),
            build: self.dir.path().join("decant.bin"),
            repo: "example/decant".into(),
            gpu: None,
            interval_secs: 1,
        }
    }
}

fn outcome_of(hub: &TestHub, member: &str, run: &str) -> String {
    hub.tables()
        .1
        .into_iter()
        .find(|r| r.member == member && r.run == run)
        .map(|r| r.outcome)
        .unwrap_or_default()
}

#[tokio::test]
async fn a_round_with_a_working_run_sends_a_signed_report() {
    let hub = start().await;
    let files = Files::new();
    let t = tester(&hub, Some("blk-a")).await;
    let cfg = files.config(&t, &hub);
    let fake = Fake::new(Ok(()), Ok(works_output()));

    // No order yet.
    assert_eq!(
        run_round(&cfg, &fake).await.unwrap(),
        RoundOutcome::NothingToTest
    );
    assert_eq!(fake.verified.get(), 0);

    let id = new_order(&hub, B1, json!({}), 3).await;
    let o = run_round(&cfg, &fake).await.unwrap();
    assert_eq!(
        o,
        RoundOutcome::Reported {
            run: id.clone(),
            outcome: Outcome::Works,
            first_error: None,
            note: None
        }
    );
    assert!(o.line().contains("works") && o.line().contains(&id));
    assert_eq!((fake.verified.get(), fake.ran.get()), (1, 1));
    assert_eq!(fake.seen.borrow()[0], "example/decant");
    // The build is checked against the commit of the order, and the folder is given back.
    assert_eq!(fake.commits.borrow()[0], B1);
    assert_eq!(fake.released.borrow().len(), 1);
    assert_eq!(outcome_of(&hub, &t.member, &id), "works");
    let row = hub.tables().1.remove(0);
    assert!(row.metrics.contains("fps_median"));
    // The same order is not handed again.
    assert_eq!(
        run_round(&cfg, &fake).await.unwrap(),
        RoundOutcome::NothingToTest
    );
}

#[tokio::test]
async fn a_round_with_a_failing_run_reports_fails() {
    let hub = start().await;
    let files = Files::new();
    let t = tester(&hub, None).await;
    let cfg = files.config(&t, &hub);
    let id = new_order(&hub, B1, json!({}), 3).await;
    let fake = Fake::new(Ok(()), Ok(fails_output()));
    match run_round(&cfg, &fake).await.unwrap() {
        RoundOutcome::Reported {
            outcome,
            first_error,
            ..
        } => {
            assert_eq!(outcome, Outcome::Fails);
            assert_eq!(first_error.as_deref(), Some("device_removed"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(outcome_of(&hub, &t.member, &id), "fails");
}

#[tokio::test]
async fn a_failed_build_check_runs_nothing_and_sends_nothing() {
    let hub = start().await;
    let files = Files::new();
    let t = tester(&hub, None).await;
    let cfg = files.config(&t, &hub);
    let id = new_order(&hub, B1, json!({}), 3).await;
    let fake = Fake::new(
        Err("the attestation of the build does not verify".into()),
        Ok(works_output()),
    );
    let e = run_round(&cfg, &fake).await.unwrap_err();
    assert!(e.contains("does not verify"), "{e}");
    assert_eq!(fake.ran.get(), 0, "the scenario must not run");
    assert!(
        hub.facts("report/").is_empty() && hub.tables().1.is_empty(),
        "a failed check sends nothing"
    );
    assert_eq!(hub.order(&id).state, "open");
}

#[tokio::test]
async fn a_failed_title_check_runs_nothing_and_sends_inconclusive() {
    let hub = start().await;
    let files = Files::new();
    let t = tester(&hub, None).await;
    let mut cfg = files.config(&t, &hub);
    std::fs::write(files.dir.path().join("other.exe"), b"not the title").unwrap();
    cfg.title = files.dir.path().join("other.exe");
    let id = new_order(&hub, B1, json!({}), 3).await;
    let fake = Fake::new(Ok(()), Ok(works_output()));
    let o = run_round(&cfg, &fake).await.unwrap();
    assert!(matches!(
        o,
        RoundOutcome::Reported {
            outcome: Outcome::Inconclusive,
            ..
        }
    ));
    assert!(o.line().contains("does not match"));
    assert_eq!(fake.ran.get(), 0);
    assert_eq!(outcome_of(&hub, &t.member, &id), "inconclusive");
}

#[tokio::test]
async fn the_loop_goes_on_until_the_pause_file_appears() {
    let hub = start().await;
    let files = Files::new();
    let t = tester(&hub, None).await;
    let cfg = files.config(&t, &hub);
    let id = new_order(&hub, B1, json!({}), 3).await;
    let mut fake = Fake::new(Ok(()), Ok(works_output()));
    fake.pause_after_sleeps = 2;
    let mut lines = Vec::new();
    let rounds = run_loop(&cfg, &fake, &mut |o| lines.push(o.line()))
        .await
        .unwrap();
    // Round one reports, round two finds nothing, then the pause file stops it.
    assert_eq!(rounds, 2);
    assert_eq!(fake.sleeps.get(), 2);
    assert_eq!(lines.len(), 3);
    assert!(lines[2].starts_with("paused"));
    assert_eq!(fake.ran.get(), 1);
    assert_eq!(outcome_of(&hub, &t.member, &id), "works");
    // A round with the pause file there asks nothing.
    assert_eq!(run_round(&cfg, &fake).await.unwrap(), RoundOutcome::Paused);
}

#[tokio::test]
async fn the_command_checks_its_arguments_before_it_does_anything() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let e = sommelier::cli::test::run(&args(&["--title", "x"]))
        .await
        .unwrap_err();
    assert!(e.contains("--profile"), "{e}");
    let e = sommelier::cli::test::run(&args(&["--bogus"]))
        .await
        .unwrap_err();
    assert!(e.contains("unknown argument"), "{e}");
    assert!(sommelier::cli::test::help().contains("--repo"));
}
