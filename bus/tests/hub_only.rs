//! Only the hub writes the FACTs of the factory: a client FACT on any of the listed prefixes is refused.

use serde_json::json;
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HUB_ONLY_TOPICS, HubConfig};
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_client_fact_on_a_factory_prefix_is_refused_for_every_role() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let member = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(hub::run(cfg, listener));

    for prefix in [
        "key/",
        "submission/",
        "gate/",
        "verify/",
        "review/",
        "report/",
        "runorder/",
        "stamp/",
        "tier/",
    ] {
        assert!(HUB_ONLY_TOPICS.contains(&prefix), "{prefix} is not listed");
    }
    for token in [&orch, &member] {
        let opts = Opts {
            url: url.clone(),
            token: token.clone(),
        };
        for prefix in HUB_ONLY_TOPICS {
            let topic = format!("{prefix}x1");
            let err = client::request_ok(
                &opts,
                json!({"op": "send", "kind": "FACT", "topic": topic, "body": "{}"}),
            )
            .await
            .unwrap_err();
            assert!(err.contains("written by the hub only"), "{topic}: {err}");
        }
    }
    task.abort();
}
