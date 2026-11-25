//! Who may write what: the table in `role/writers.rs` seen from the wire. A token member may still send the
//! plain kinds of the swarm, and nobody but the hub writes what the hub owns.

use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

struct Wire {
    orch: Opts,
    alice: Opts,
    bob: Opts,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start() -> Wire {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    let bob = t.add("bob", Role::Member).unwrap();
    // The alert of a parked order is addressed to the coordinator, which needs a token.
    t.add("coordinator", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    let opts = |token: String| Opts {
        url: url.clone(),
        token,
    };
    Wire {
        orch: opts(orch),
        alice: opts(alice),
        bob: opts(bob),
        _dir: dir,
        task,
    }
}

async fn send(o: &Opts, kind: &str, topic: &str, body: Value) -> Result<Value, String> {
    client::request_ok(
        o,
        json!({"op": "send", "kind": kind, "topic": topic, "body": body.to_string()}),
    )
    .await
}

#[tokio::test]
async fn nobody_but_the_hub_writes_task_updates_offers_and_alerts() {
    let w = start().await;
    let forged = json!({"task": "T1", "task_row": {"min_trust": 0, "risk": "low", "scope": "**"},
                        "lease": {"owner": "alice", "attempt": 1, "state": "active"}});
    for who in [&w.alice, &w.orch] {
        for (kind, topic) in [
            ("TASK_UPDATE", "task/T1"),
            ("OFFER", "task/T1"),
            ("ALERT", "alert/STUCK/T1"),
        ] {
            let err = send(who, kind, topic, forged.clone()).await.unwrap_err();
            assert!(err.contains("only the hub may send"), "{kind}: {err}");
        }
    }
}

#[tokio::test]
async fn a_fact_on_a_topic_of_the_hub_is_refused_for_every_prefix() {
    let w = start().await;
    for prefix in hub::HUB_ONLY_TOPICS {
        for who in [&w.alice, &w.orch] {
            let err = send(who, "FACT", &format!("{prefix}x1"), json!({}))
                .await
                .unwrap_err();
            assert!(err.contains("written by the hub only"), "{prefix}: {err}");
        }
    }
    // The topics the findings name, one by one.
    for prefix in [
        "task/",
        "land/",
        "batch/",
        "branch/",
        "run/",
        "role/",
        "decision/",
        "pause/",
        "drill/",
        "metric/",
        "stamp/",
        "tier/",
    ] {
        assert!(hub::HUB_ONLY_TOPICS.contains(&prefix), "{prefix}");
    }
}

#[tokio::test]
async fn a_member_declares_capacity_and_reports_only_for_itself() {
    let w = start().await;
    let own = json!({"class": "codex", "useful_parallel": 2, "machine": "m1"});
    send(&w.alice, "FACT", "capacity/alice/m1", own.clone())
        .await
        .unwrap();
    let err = send(&w.alice, "FACT", "capacity/bob/m1", own.clone())
        .await
        .unwrap_err();
    assert!(err.contains("only for itself"), "{err}");
    // The orchestrator is a member too: it does not declare for others.
    let err = send(&w.orch, "FACT", "capacity/bob/m1", own.clone())
        .await
        .unwrap_err();
    assert!(err.contains("only for itself"), "{err}");
    let err = send(&w.bob, "FACT", "quota/alice/p/w", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("only for itself"), "{err}");
    let err = send(&w.bob, "FACT", "spend/alice/p", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("only for itself"), "{err}");
}

#[tokio::test]
async fn a_token_member_still_sends_the_plain_kinds() {
    let w = start().await;
    for (kind, topic) in [
        ("HELLO", ""),
        ("CHAT", ""),
        ("READY", "issue/1"),
        ("STUCK", "issue/1"),
        ("CLAIM", "issue/2"),
        ("FIX", "issue/3"),
        ("FINDING", "issue/3"),
        ("STATUS", "agent/alice"),
        ("FACT", "issue/9"),
        ("FACT", "exit/alice"),
    ] {
        send(&w.alice, kind, topic, json!("x"))
            .await
            .unwrap_or_else(|e| panic!("{kind} {topic}: {e}"));
    }
    // TASK stays with the orchestrator.
    assert!(
        send(&w.alice, "TASK", "issue/50", json!("x"))
            .await
            .is_err()
    );
    send(&w.orch, "TASK", "issue/50", json!("x")).await.unwrap();
}

// ---- the second guard: a projection takes the line only from the hub ----

use sommelier::msg::{Kind, Message};
use sommelier::store::Store;

fn line(id: u64, role: Role, from: &str, kind: Kind, topic: &str, body: Value) -> Message {
    Message {
        id,
        ts: "2026-11-26T10:00:00Z".into(),
        from: from.into(),
        role,
        kind,
        topic: topic.into(),
        body: body.to_string(),
        refs: vec![],
        to: vec![],
    }
}

/// The same line as a member and as the hub: only the hub's line changes a table.
fn apply_both(
    store: &mut Store,
    kind: Kind,
    topic: &str,
    body: Value,
    check: impl Fn(&Store) -> bool,
) {
    let forged = line(1, Role::Member, "alice", kind, topic, body.clone());
    store.apply(&forged).unwrap();
    assert!(
        !check(store),
        "a line of a member changed the table: {topic}"
    );
    let forged = line(2, Role::Orchestrator, "orch", kind, topic, body.clone());
    store.apply(&forged).unwrap();
    assert!(
        !check(store),
        "a line of the orchestrator changed the table: {topic}"
    );
    let real = line(3, Role::Hub, "hub", kind, topic, body);
    store.apply(&real).unwrap();
    assert!(check(store), "the line of the hub changed nothing: {topic}");
}

#[test]
fn a_forged_task_update_or_offer_in_the_log_gives_no_lease_and_no_offer() {
    let mut store = Store::open_in_memory().unwrap();
    let body = json!({"task": "T1", "task_row": {"min_trust": 0, "risk": "low", "scope": "**"},
                      "lease": {"owner": "alice", "attempt": 1, "state": "active"}});
    apply_both(&mut store, Kind::TaskUpdate, "task/T1", body, |s| {
        sommelier::task::query_task(s.connection(), "T1")
            .unwrap()
            .is_some()
    });
    let mut store = Store::open_in_memory().unwrap();
    let offer = json!({"offer_id": "o1", "task": "T1", "to_owner": "alice", "state": "open"});
    apply_both(&mut store, Kind::Offer, "task/T1", offer, |s| {
        !sommelier::task::query_offers(s.connection())
            .unwrap()
            .is_empty()
    });
}

#[test]
fn a_forged_landing_batch_branch_or_run_row_in_the_log_changes_nothing() {
    let sha = "a".repeat(40);
    let row = json!(sommelier::land::LandingRow {
        ref_name: "fx/x".into(),
        sha: sha.clone(),
        state: "landed".into(),
        ..Default::default()
    });
    let mut store = Store::open_in_memory().unwrap();
    apply_both(&mut store, Kind::Fact, "land/fx/x", row, |s| {
        !sommelier::land::query_landings(s.connection())
            .unwrap()
            .is_empty()
    });
    let mut store = Store::open_in_memory().unwrap();
    apply_both(
        &mut store,
        Kind::Fact,
        "batch/b1",
        json!(sommelier::land::BatchRow {
            batch_id: "b1".into(),
            result: "landed".into(),
            ..Default::default()
        }),
        |s| {
            !sommelier::land::query_batches(s.connection())
                .unwrap()
                .is_empty()
        },
    );
    let mut store = Store::open_in_memory().unwrap();
    apply_both(
        &mut store,
        Kind::Fact,
        "run/r1",
        json!(sommelier::land::RunRow {
            run_id: "r1".into(),
            sha: sha.clone(),
            outcome: "pass".into(),
            ..Default::default()
        }),
        |s| {
            !sommelier::land::query_runs(s.connection())
                .unwrap()
                .is_empty()
        },
    );
}

#[test]
fn a_forged_stamp_in_the_log_gives_no_stamp() {
    let mut store = Store::open_in_memory().unwrap();
    let body = json!({"v": 1, "subject": "m1", "issuer": "m2", "kind": "landed",
                      "evidence": {"msg": 7, "commit": "abc"}, "at": 100});
    apply_both(&mut store, Kind::Fact, "stamp/m1", body, |s| {
        s.connection()
            .query_row("SELECT COUNT(*) FROM STAMP", [], |r| r.get::<_, i64>(0))
            .unwrap()
            > 0
    });
}

#[test]
fn a_forged_alert_or_role_row_in_the_log_changes_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let alert =
        json!({"code": "STUCK", "key": "T1", "severity": "warn", "state": "open", "text": "x"});
    apply_both(&mut store, Kind::Alert, "alert/STUCK/T1", alert, |s| {
        !sommelier::store::core::query_alerts(s.connection())
            .unwrap()
            .is_empty()
    });
    let mut store = Store::open_in_memory().unwrap();
    let role = json!({"role": "lander", "scope": "global", "holder": "alice", "state": "held", "epoch": 1});
    apply_both(&mut store, Kind::Fact, "role/lander", role, |s| {
        sommelier::role::query_row(s.connection(), "lander", "global")
            .ok()
            .flatten()
            .is_some()
    });
}

#[test]
fn the_owner_of_a_capacity_fact_is_the_sender_never_the_topic() {
    let mut store = Store::open_in_memory().unwrap();
    let decl = json!({"class": "codex", "useful_parallel": 3, "machine": "m1"});
    // alice writes the owner bob: nothing.
    store
        .apply(&line(
            1,
            Role::Member,
            "alice",
            Kind::Fact,
            "capacity/bob/m1",
            decl.clone(),
        ))
        .unwrap();
    assert!(
        sommelier::store::core::query_capacities(store.connection())
            .unwrap()
            .is_empty()
    );
    // alice for herself: one row.
    store
        .apply(&line(
            2,
            Role::Member,
            "alice",
            Kind::Fact,
            "capacity/alice/m1",
            decl,
        ))
        .unwrap();
    let rows = sommelier::store::core::query_capacities(store.connection()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "alice");
}

#[test]
fn capacity_is_capped_per_owner_in_total() {
    let mut store = Store::open_in_memory().unwrap();
    let mut id = 0;
    // Many machines, many classes, each at the single declaration cap.
    for m in 0..40 {
        for c in 0..6 {
            id += 1;
            let decl = json!({"class": format!("c{c}"), "useful_parallel": 64, "machine": format!("m{m}")});
            store
                .apply(&line(
                    id,
                    Role::Member,
                    "alice",
                    Kind::Fact,
                    &format!("capacity/alice/m{m}"),
                    decl,
                ))
                .unwrap();
        }
    }
    let rows = sommelier::store::core::query_capacities(store.connection()).unwrap();
    let total: u32 = rows.iter().map(|r| r.useful_parallel).sum();
    assert!(
        total <= sommelier::derive::capacity::MAX_TOTAL_SLOTS,
        "total {total}"
    );
    let machines: std::collections::BTreeSet<_> = rows.iter().map(|r| r.machine.clone()).collect();
    assert!(machines.len() as i64 <= sommelier::store::core::MAX_MACHINES_PER_OWNER);
    // Another owner is not touched by the cap of the first.
    store
        .apply(&line(
            id + 1,
            Role::Member,
            "bob",
            Kind::Fact,
            "capacity/bob/m1",
            json!({"class": "codex", "useful_parallel": 5, "machine": "m1"}),
        ))
        .unwrap();
    let rows = sommelier::store::core::query_capacities(store.connection()).unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.owner == "bob")
            .map(|r| r.useful_parallel),
        Some(5)
    );
}

#[tokio::test]
async fn only_the_holder_or_the_orchestrator_marks_an_order_stuck() {
    let w = start().await;
    for id in ["t1", "t2"] {
        client::request_ok(
            &w.orch,
            json!({"op": "task.add", "id": id, "title": format!("order {id}"), "kind": "fix",
                   "scope": format!("src/{id}/**"), "done_when": "cargo test"}),
        )
        .await
        .unwrap();
    }
    // An order that nobody holds: a member may not park it.
    let err = client::request_ok(
        &w.bob,
        json!({"op": "task.stuck", "task": "t1", "why": "x"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("no lease"), "{err}");
    client::request_ok(&w.alice, json!({"op": "task.work", "task": "t1"}))
        .await
        .unwrap();
    // Another member may not park the order of alice, and its lease stays.
    let err = client::request_ok(
        &w.bob,
        json!({"op": "task.stuck", "task": "t1", "why": "x"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("leased to alice"), "{err}");
    let shown = client::request_ok(&w.alice, json!({"op": "task.show", "task": "t1"}))
        .await
        .unwrap();
    assert_eq!(shown["task"]["state"], "leased", "{shown}");
    // The holder parks it, and the orchestrator may park an order with no lease.
    client::request_ok(
        &w.alice,
        json!({"op": "task.stuck", "task": "t1", "why": "x"}),
    )
    .await
    .unwrap();
    client::request_ok(
        &w.orch,
        json!({"op": "task.stuck", "task": "t2", "why": "x"}),
    )
    .await
    .unwrap();
}
