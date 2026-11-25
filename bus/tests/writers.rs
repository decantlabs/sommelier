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
