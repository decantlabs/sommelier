//! Integration tests for high availability standby, witness, promotion, self-fencing,
//! client address list failover, and drills (step 12b, sections 13.5, 13.7).

use std::fs;
use std::io::Write;
use std::time::Duration;

use serde_json::json;
use sommelier::client::{self, Opts};
use sommelier::ha::{
    HaConfig, HaRoleState, HubHaRole, MemoryWitness, PromoteOpts, SelfFenceMonitor, SelfFenceState,
    StandbyConfig, Witness, WitnessBackend, WitnessError, WitnessRecord, promote,
    reconcile_returning_primary, run_drill, run_standby, sync_bundle,
};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::Store;
use sommelier::time::rfc3339;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

#[test]
fn self_fencing_requires_standby_configured_and_witness_outage() {
    let witness = MemoryWitness::new(Some(WitnessRecord {
        holder: "primary".into(),
        epoch: 1,
        heartbeat: 1000,
    }));

    // No standby configured: self-fencing stays inactive even if witness is offline.
    let cfg_no_standby = HaConfig {
        standby: String::new(),
        self_fence_min: 8,
        ..Default::default()
    };
    let mut mon = SelfFenceMonitor::new("primary", 1, &cfg_no_standby, 1000);
    witness.set_offline(true);
    let st = mon.tick(&witness, 1000 + 600).unwrap();
    assert_eq!(st, SelfFenceState::Active);
    assert!(st.may_grant_leases_and_offers());

    // Standby configured: witness offline for >= 8 min (480s) triggers self-fencing.
    let cfg_with_standby = HaConfig {
        standby: "wss://standby.example.org".into(),
        self_fence_min: 8,
        ..Default::default()
    };
    let mut mon2 = SelfFenceMonitor::new("primary", 1, &cfg_with_standby, 1000);
    assert_eq!(
        mon2.tick(&witness, 1000 + 479).unwrap(),
        SelfFenceState::Active
    );
    let fenced = mon2.tick(&witness, 1000 + 480).unwrap();
    assert!(matches!(fenced, SelfFenceState::Fenced { .. }));
    assert!(!fenced.may_grant_leases_and_offers());

    // Witness returns at same epoch -> un-fences.
    witness.set_offline(false);
    assert_eq!(
        mon2.tick(&witness, 1000 + 500).unwrap(),
        SelfFenceState::Active
    );

    // Witness moved to higher epoch -> tick returns Err(higher_epoch) so primary steps down.
    let promoted = witness.promote("standby", 1, 1600).unwrap();
    assert_eq!(promoted.epoch, 2);
    assert_eq!(mon2.tick(&witness, 1610), Err(2));
}

#[test]
fn witness_cas_rejects_stale_epoch() {
    let witness = MemoryWitness::new(Some(WitnessRecord {
        holder: "primary".into(),
        epoch: 1,
        heartbeat: 100,
    }));
    assert_eq!(
        witness.promote("standby", 0, 200),
        Err(WitnessError::StaleEpoch {
            current: 1,
            sent: 0
        })
    );
    let rec = witness.promote("standby", 1, 200).unwrap();
    assert_eq!(rec.epoch, 2);
    assert_eq!(
        witness.heartbeat("primary", 1, 210),
        Err(WitnessError::StaleEpoch {
            current: 2,
            sent: 1
        })
    );
}

#[test]
fn bundle_sync_copies_salted_token_hashes_policy_and_briefs() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let mut tokens = TokenFile::default();
    let _tok = tokens.add("alice", Role::Member).unwrap();
    tokens.save(&src.path().join("tokens.json")).unwrap();
    fs::create_dir_all(src.path().join("policy/versions")).unwrap();
    fs::write(src.path().join("policy/versions/1.toml"), "version = 1\n").unwrap();
    fs::create_dir_all(src.path().join("briefs")).unwrap();
    fs::write(src.path().join("briefs/t-1.md"), "# brief\n").unwrap();

    let stats = sync_bundle(src.path(), dst.path()).unwrap();
    assert!(stats.tokens_copied);
    assert_eq!(stats.policy_files, 1);
    assert_eq!(stats.brief_files, 1);
    assert_eq!(
        fs::read(src.path().join("tokens.json")).unwrap(),
        fs::read(dst.path().join("tokens.json")).unwrap()
    );
}

#[tokio::test]
async fn promote_checks_stale_witness_unreachable_primary_and_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let t0 = 1_700_000_000u64;
    let witness = Witness::Memory(MemoryWitness::new(Some(WitnessRecord {
        holder: "primary".into(),
        epoch: 1,
        heartbeat: t0,
    })));

    // Fresh witness is refused.
    let err_fresh = promote(
        &PromoteOpts {
            data_dir: dir.path().to_path_buf(),
            primary_url: None,
            new_holder: "standby".into(),
            confirm_code: Some("secret".into()),
            expected_confirm: Some("secret".into()),
            guest_confirmations: vec![],
            now_sec: t0 + 100,
            ha_config: HaConfig::default(),
        },
        &witness,
    )
    .await
    .unwrap_err();
    assert!(err_fresh.contains("only 100s old"), "{err_fresh}");

    // Missing confirm code is refused.
    let err_confirm = promote(
        &PromoteOpts {
            data_dir: dir.path().to_path_buf(),
            primary_url: None,
            new_holder: "standby".into(),
            confirm_code: None,
            expected_confirm: Some("secret".into()),
            guest_confirmations: vec![],
            now_sec: t0 + 700,
            ha_config: HaConfig::default(),
        },
        &witness,
    )
    .await
    .unwrap_err();
    assert!(err_confirm.contains("--confirm"), "{err_confirm}");

    // Valid stale witness + confirm code succeeds.
    let ok = promote(
        &PromoteOpts {
            data_dir: dir.path().to_path_buf(),
            primary_url: None,
            new_holder: "standby".into(),
            confirm_code: Some("secret".into()),
            expected_confirm: Some("secret".into()),
            guest_confirmations: vec![],
            now_sec: t0 + 700,
            ha_config: HaConfig::default(),
        },
        &witness,
    )
    .await
    .unwrap();
    assert_eq!(ok.epoch, 2);
    assert_eq!(
        HaRoleState::load(dir.path()).unwrap().role,
        HubHaRole::Primary
    );
}

#[tokio::test]
async fn standby_log_byte_equal_503_until_promoted_and_client_failover() {
    let primary_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let mut tokens = TokenFile::default();
    let orch_tok = tokens.add("orch", Role::Orchestrator).unwrap();
    let replica_tok = tokens.add("replica", Role::Observer).unwrap();
    tokens
        .save(&primary_dir.path().join("tokens.json"))
        .unwrap();

    let mut p_cfg = HubConfig::new(primary_dir.path());
    p_cfg.verbose = false;
    let p_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p_ws = format!("ws://{}", p_listener.local_addr().unwrap());
    let p_task = tokio::spawn(async move {
        let _ = hub::run(p_cfg, p_listener).await;
    });

    let mut s_cfg = StandbyConfig::new(&p_ws, &replica_tok, standby_dir.path());
    s_cfg.bundle_source_dir = Some(primary_dir.path().to_path_buf());
    let s_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let standby = run_standby(s_cfg, Some(s_listener)).await.unwrap();
    let s_ws = standby.ws_url.clone();
    let s_http = standby.http_url.clone();

    let p_opts = Opts {
        url: p_ws.clone(),
        token: orch_tok.clone(),
    };
    let sent = client::request_ok(
        &p_opts,
        json!({"op": "send", "kind": "CHAT", "topic": "ha", "body": "hello-ha"}),
    )
    .await
    .unwrap();
    let last_id = sent["id"].as_u64().unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while standby.last_id() < last_id {
        assert!(tokio::time::Instant::now() < deadline, "standby timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        fs::read(primary_dir.path().join("bus.jsonl")).unwrap(),
        fs::read(standby_dir.path().join("bus.jsonl")).unwrap()
    );

    // Standby refuses HTTP and WS until promoted.
    let http = reqwest::Client::new();
    let res = http
        .get(format!("{s_http}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 503);
    let body = res.text().await.unwrap();
    assert!(body.contains("not primary, epoch 1"), "{body}");

    // Kill primary and promote standby.
    p_task.abort();
    let _ = p_task.await;

    let t0 = 1_700_000_000u64;
    let witness = Witness::Memory(MemoryWitness::new(Some(WitnessRecord {
        holder: "primary".into(),
        epoch: 1,
        heartbeat: t0,
    })));
    promote(
        &PromoteOpts {
            data_dir: standby_dir.path().to_path_buf(),
            primary_url: Some(p_ws.clone()),
            new_holder: "standby".into(),
            confirm_code: Some("ok".into()),
            expected_confirm: None,
            guest_confirmations: vec![],
            now_sec: t0 + 700,
            ha_config: HaConfig::default(),
        },
        &witness,
    )
    .await
    .unwrap();

    // Client with address list [primary, standby] moves to standby automatically.
    let list_opts = Opts::from_urls(&[p_ws, s_ws], &orch_tok);
    let mut moved = None;
    for _ in 0..40 {
        if let Ok(r) = client::request_ok(
            &list_opts,
            json!({"op": "send", "kind": "CHAT", "topic": "ha", "body": "after-failover"}),
        )
        .await
        {
            moved = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let moved = moved.expect("client failed over to standby");
    assert_eq!(moved["id"].as_u64().unwrap(), last_id + 1);

    standby.stop();
}

#[test]
fn returning_primary_saves_orphan_tail_and_raises_log_diverged() {
    let dir = tempfile::tempdir().unwrap();
    let mut primary_lines = Vec::new();
    for id in 1..=2u64 {
        let msg = Message {
            id,
            ts: rfc3339(1_700_000_000_000 + id * 1000),
            from: "orch".into(),
            role: Role::Orchestrator,
            kind: Kind::Chat,
            topic: "t".into(),
            body: format!("m{id}"),
            refs: vec![],
            to: vec![],
        };
        primary_lines.push(serde_json::to_string(&msg).unwrap());
    }

    // Returning primary had one extra unseen orphan line at id 3.
    let orphan = Message {
        id: 3,
        ts: rfc3339(1_700_000_005_000),
        from: "orch".into(),
        role: Role::Orchestrator,
        kind: Kind::Chat,
        topic: "t".into(),
        body: "orphan-line".into(),
        refs: vec![],
        to: vec![],
    };
    let mut f = fs::File::create(dir.path().join("bus.jsonl")).unwrap();
    for l in &primary_lines {
        writeln!(f, "{l}").unwrap();
    }
    writeln!(f, "{}", serde_json::to_string(&orphan).unwrap()).unwrap();
    drop(f);

    let report =
        reconcile_returning_primary(dir.path(), 1, 2, Some("ws://standby:7301"), &primary_lines)
            .unwrap();
    assert_eq!(report.orphan_count, 1);
    assert!(report.orphan_path.exists());
    assert!(report.alert_id.is_some());
    assert_eq!(
        HaRoleState::load(dir.path()).unwrap().role,
        HubHaRole::Standby
    );

    let store = Store::open(dir.path()).unwrap();
    let alerts = sommelier::store::query_alerts(store.connection()).unwrap();
    assert!(alerts.iter().any(|a| a.code == "LOG_DIVERGED"));
}

#[tokio::test]
async fn drills_pass() {
    let c = run_drill("coordinator-dies", None).await.unwrap();
    assert!(c.passed && c.recover_sec <= c.target_recover_sec);

    let h = run_drill("hub-dies", None).await.unwrap();
    assert!(h.passed);

    let q = run_drill("quota-drain", None).await.unwrap();
    assert!(q.passed);

    let f = run_drill("flapping", None).await.unwrap();
    assert!(f.passed);
}
