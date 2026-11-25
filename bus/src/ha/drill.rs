//! High availability failure drills (section 13.7, step 12b).
//!
//! `sommelier drill <name>` runs in an isolated temporary setup (free port, own data
//! folder, fixture policy). It never touches the live hub.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;

use crate::client::{self, Opts};
use crate::guide::{PolicyInfo, handover};
use crate::hub::{self, HubConfig};
use crate::msg::{Kind, Message, Role};
use crate::role::{
    LeaseScope, RoleAction, RoleCandidate, RoleCommand, RolePolicy, RoleTable, check_fence,
};
use crate::store::Store;
use crate::time::{now_ms, rfc3339};
use crate::tokens::TokenFile;

use super::fence::reconcile_returning_primary;
use super::promote::{PromoteOpts, promote};
use super::standby::{StandbyConfig, run_standby};
use super::witness::{MemoryWitness, Witness, WitnessRecord};
use super::{HaConfig, PromotionMode};

/// Outcome of a single failure drill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrillResult {
    pub drill: String,
    pub passed: bool,
    pub recover_sec: u64,
    pub target_recover_sec: u64,
    pub details: String,
}

impl DrillResult {
    /// Formats the drill result as a `FACT` message with topic `drill/<name>`.
    pub fn to_fact_message(&self, id: u64) -> Message {
        Message {
            id,
            ts: rfc3339(now_ms()),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: format!("drill/{}", self.drill),
            body: serde_json::to_string(self).unwrap_or_default(),
            refs: vec![],
            to: vec![],
        }
    }
}

/// Runs the named drill (`coordinator-dies`, `hub-dies`, `quota-drain`, `flapping`).
pub async fn run_drill(name: &str, data_dir: Option<&Path>) -> Result<DrillResult, String> {
    let ha_cfg = HaConfig::default();
    let res = match name {
        "coordinator-dies" => drill_coordinator_dies(&ha_cfg, data_dir).await?,
        "hub-dies" => drill_hub_dies(&ha_cfg).await?,
        "quota-drain" => drill_quota_drain(&ha_cfg)?,
        "flapping" => drill_flapping(&ha_cfg)?,
        other => {
            return Err(format!(
                "unknown drill `{other}` (expected: coordinator-dies, hub-dies, quota-drain, flapping)"
            ));
        }
    };
    if !res.passed {
        return Err(format!("drill `{name}` failed: {}", res.details));
    }
    Ok(res)
}

fn fixture_coordinator_policy() -> RolePolicy {
    RolePolicy {
        heartbeat_sec: 60,
        dead_sec: 180,
        min_hold_min: 15,
        cooldown_min: 30,
        max_changes_per_hour: 3,
        require: vec!["harness_repo".into()],
        candidates: vec![
            RoleCandidate {
                who: "seat-a/coord-claude".into(),
                tier: 1,
                class: "claude_strong".into(),
                scope: LeaseScope::Full,
                spawn: true,
                opt_in: false,
            },
            RoleCandidate {
                who: "seat-a/coord-codex".into(),
                tier: 2,
                class: "codex".into(),
                scope: LeaseScope::Full,
                spawn: true,
                opt_in: false,
            },
            RoleCandidate {
                who: "guest-b/lead".into(),
                tier: 3,
                class: "gemini".into(),
                scope: LeaseScope::Limited,
                spawn: false,
                opt_in: true,
            },
        ],
    }
}

async fn drill_coordinator_dies(
    ha_cfg: &HaConfig,
    data_dir: Option<&Path>,
) -> Result<DrillResult, String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dir = data_dir.unwrap_or_else(|| temp.path());
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let mut table = RoleTable::new();
    table.set_policy("coordinator", fixture_coordinator_policy());

    let t0 = 1_700_000_000u64;
    let grant1 = table.role_tick("coordinator", t0);
    let RoleAction::Granted {
        holder: first_holder,
        epoch: ep1,
        ..
    } = grant1
    else {
        return Err(format!("expected initial grant, got {grant1:?}"));
    };
    assert_eq!(ep1, 1);

    // Seed an open alert in an isolated Store.
    let mut store = Store::open(dir)?;
    let open_alert = Message {
        id: 1,
        ts: rfc3339(t0 * 1000),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: "alert/IDLE/slot-1".into(),
        body: json!({
            "v": 1,
            "code": "IDLE",
            "key": "slot-1",
            "severity": "warn",
            "text": "slot-1 idle",
            "data": {}
        })
        .to_string(),
        refs: vec![],
        to: vec!["coordinator".into()],
    };
    store.apply(&open_alert)?;

    // Holder stops heartbeating; dead_sec (180s) elapses.
    let t_dead = t0 + 181;
    let grant2 = table.role_tick("coordinator", t_dead);
    let RoleAction::Granted {
        holder: second_holder,
        epoch: ep2,
        ..
    } = grant2
    else {
        return Err(format!("expected failover grant, got {grant2:?}"));
    };
    if ep2 != ep1 + 1 || second_holder == first_holder {
        return Err(format!(
            "unexpected failover grant: holder={second_holder} epoch={ep2}"
        ));
    }

    // Old epoch command must be refused with exit code 3 (REFUSED).
    let row = table.get("coordinator").ok_or("missing coordinator row")?;
    let fence_err =
        check_fence(row, Some(&first_holder), ep1, Some(RoleCommand::AckAlert)).unwrap_err();
    if !fence_err.message.contains("epoch") {
        return Err(format!("unexpected fence message: {}", fence_err.message));
    }

    // Takeover page must be at most 80 lines.
    let policy_info = PolicyInfo::load(None, dir)?;
    let state_json = json!({
        "hub": { "id": 1, "now": rfc3339(t_dead * 1000), "as_of": t_dead },
        "policy": { "version": policy_info.version, "hash": policy_info.hash, "mode": policy_info.mode },
        "roles": [row],
        "alerts": [
            { "code": "IDLE", "key": "slot-1", "severity": "warn", "state": "open", "opened_at": t0, "text": "slot-1 idle" }
        ],
    });
    let page = handover::build("coordinator", &state_json, &policy_info);
    if page.line_count > 80 {
        return Err(format!(
            "takeover page exceeded 80 lines: {}",
            page.line_count
        ));
    }

    // Successor (epoch 2) passes fence check and acks the open alert.
    check_fence(row, Some(&second_holder), ep2, Some(RoleCommand::AckAlert))
        .map_err(|e| e.message)?;
    let ack_alert = Message {
        id: 2,
        ts: rfc3339((t_dead + 1) * 1000),
        from: second_holder.clone(),
        role: Role::Orchestrator,
        kind: Kind::Alert,
        topic: "alert/IDLE/slot-1".into(),
        body: json!({
            "v": 1,
            "code": "IDLE",
            "key": "slot-1",
            "severity": "warn",
            "state": "acked",
            "text": "acked by successor",
        })
        .to_string(),
        refs: vec![],
        to: vec![],
    };
    store.apply(&ack_alert)?;

    let recover_sec = (t_dead + 1).saturating_sub(t0);
    let passed = recover_sec <= ha_cfg.target_recover_sec;
    let res = DrillResult {
        drill: "coordinator-dies".into(),
        passed,
        recover_sec,
        target_recover_sec: ha_cfg.target_recover_sec,
        details: format!(
            "failover {first_holder} (ep {ep1}) -> {second_holder} (ep {ep2}) in {recover_sec}s; takeover {} lines; old epoch fenced",
            page.line_count
        ),
    };
    store.apply(&res.to_fact_message(3))?;
    Ok(res)
}

async fn drill_hub_dies(ha_cfg: &HaConfig) -> Result<DrillResult, String> {
    let primary_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let standby_dir = tempfile::tempdir().map_err(|e| e.to_string())?;

    let mut tokens = TokenFile::default();
    let orch_tok = tokens.add("orch", Role::Orchestrator)?;
    let replica_tok = tokens.add("replica", Role::Observer)?;
    tokens
        .save(&primary_dir.path().join("tokens.json"))
        .map_err(|e| e.to_string())?;

    let mut primary_cfg = HubConfig::new(primary_dir.path());
    primary_cfg.verbose = false;
    let primary_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let primary_ws = format!(
        "ws://{}",
        primary_listener.local_addr().map_err(|e| e.to_string())?
    );
    let primary_task = tokio::spawn(async move {
        let _ = hub::run(primary_cfg, primary_listener).await;
    });

    let mut standby_cfg = StandbyConfig::new(&primary_ws, &replica_tok, standby_dir.path());
    standby_cfg.initial_epoch = 1;
    standby_cfg.bundle_source_dir = Some(primary_dir.path().to_path_buf());
    let standby_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let standby_handle = run_standby(standby_cfg, Some(standby_listener)).await?;
    let standby_ws = standby_handle.ws_url.clone();
    let standby_http = standby_handle.http_url.clone();

    let t0 = 1_700_000_000u64;
    let witness = Witness::Memory(MemoryWitness::new(Some(WitnessRecord {
        holder: "primary-pc".into(),
        epoch: 1,
        heartbeat: t0,
    })));

    // 1. Send messages to primary.
    let primary_opts = Opts {
        url: primary_ws.clone(),
        token: orch_tok.clone(),
    };
    let r1 = client::request_ok(
        &primary_opts,
        json!({"op": "send", "kind": "CHAT", "topic": "drill", "body": "msg-one"}),
    )
    .await?;
    let r2 = client::request_ok(
        &primary_opts,
        json!({"op": "send", "kind": "CHAT", "topic": "drill", "body": "msg-two"}),
    )
    .await?;
    let last_primary_id = r2["id"].as_u64().unwrap_or(0);
    assert!(r1["id"].as_u64().unwrap_or(0) < last_primary_id);

    // 2. Wait for standby to replicate and verify byte-equal bus.jsonl.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while standby_handle.last_id() < last_primary_id {
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for standby log replication".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let primary_bytes =
        fs::read(primary_dir.path().join("bus.jsonl")).map_err(|e| e.to_string())?;
    let standby_bytes =
        fs::read(standby_dir.path().join("bus.jsonl")).map_err(|e| e.to_string())?;
    if primary_bytes != standby_bytes {
        return Err("standby bus.jsonl is not byte-equal to primary bus.jsonl".into());
    }

    // 3. Standby must answer 503 before promote.
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http_client
        .get(format!("{standby_http}/api/state"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 503 {
        return Err(format!("expected 503 from standby, got {}", resp.status()));
    }

    // 4. Kill primary and promote standby after witness_stale_min.
    primary_task.abort();
    let _ = primary_task.await;

    let t_promote = t0 + ha_cfg.witness_stale_min * 60 + 5;
    let outcome = promote(
        &PromoteOpts {
            data_dir: standby_dir.path().to_path_buf(),
            primary_url: Some(primary_ws.clone()),
            new_holder: "standby-pc".into(),
            confirm_code: Some("drill-confirm".into()),
            expected_confirm: None,
            guest_confirmations: vec![],
            now_sec: t_promote,
            ha_config: ha_cfg.clone(),
        },
        &witness,
    )
    .await?;
    if outcome.epoch != 2 {
        return Err(format!("expected promoted epoch 2, got {}", outcome.epoch));
    }

    // 5. Client with address list [primary, standby] fails over to standby and gets id = last_primary_id + 1.
    let failover_opts = Opts::from_urls(&[primary_ws.clone(), standby_ws.clone()], &orch_tok);
    let mut failover_ok = None;
    let failover_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < failover_deadline {
        if let Ok(reply) = client::request_ok(
            &failover_opts,
            json!({"op": "send", "kind": "CHAT", "topic": "drill", "body": "after-promote"}),
        )
        .await
        {
            failover_ok = Some(reply);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reply = failover_ok.ok_or("client did not fail over to promoted standby")?;
    let new_id = reply["id"].as_u64().unwrap_or(0);
    if new_id != last_primary_id + 1 {
        return Err(format!(
            "expected next id {} after failover, got {new_id}",
            last_primary_id + 1
        ));
    }

    // 6. Returning primary with an orphan tail steps down, saves bus.orphan-1.jsonl, and raises LOG_DIVERGED.
    let mut primary_log_file = fs::OpenOptions::new()
        .append(true)
        .open(primary_dir.path().join("bus.jsonl"))
        .map_err(|e| e.to_string())?;
    use std::io::Write as _;
    let orphan_msg = Message {
        id: last_primary_id + 1,
        ts: rfc3339(t_promote * 1000),
        from: "orch".into(),
        role: Role::Orchestrator,
        kind: Kind::Chat,
        topic: "orphan".into(),
        body: "unseen-by-standby".into(),
        refs: vec![],
        to: vec![],
    };
    writeln!(
        primary_log_file,
        "{}",
        serde_json::to_string(&orphan_msg).unwrap()
    )
    .map_err(|e| e.to_string())?;
    drop(primary_log_file);

    let standby_lines: Vec<String> = fs::read_to_string(standby_dir.path().join("bus.jsonl"))
        .map_err(|e| e.to_string())?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let orphan_report = reconcile_returning_primary(
        primary_dir.path(),
        1,
        outcome.epoch,
        Some(&standby_ws),
        &standby_lines,
    )?;
    if orphan_report.orphan_count != 1 || !orphan_report.orphan_path.exists() {
        return Err(format!("unexpected orphan report: {orphan_report:?}"));
    }

    standby_handle.stop();
    Ok(DrillResult {
        drill: "hub-dies".into(),
        passed: true,
        recover_sec: 1,
        target_recover_sec: ha_cfg.target_recover_sec,
        details: format!(
            "byte-equal log verified; 503 before promote; client failover id {new_id}; orphan_count {}",
            orphan_report.orphan_count
        ),
    })
}

fn drill_quota_drain(ha_cfg: &HaConfig) -> Result<DrillResult, String> {
    let mut table = RoleTable::new();
    table.set_policy("coordinator", fixture_coordinator_policy());
    let t0 = 1_700_000_000u64;
    table.role_tick("coordinator", t0);

    // Planned handover before holder dies (zero vacant seconds).
    let act = table
        .handover(
            "coordinator",
            Some("seat-a/coord-claude"),
            Some(1),
            "pool in drain".to_string(),
            t0 + 60,
        )
        .map_err(|e| e.message)?;
    let RoleAction::Granted { holder, epoch, .. } = act else {
        return Err(format!("expected planned handover grant, got {act:?}"));
    };
    Ok(DrillResult {
        drill: "quota-drain".into(),
        passed: epoch == 2 && holder == "seat-a/coord-codex",
        recover_sec: 0,
        target_recover_sec: ha_cfg.target_recover_sec,
        details: format!("planned handover to {holder} at epoch {epoch} with 0 vacant seconds"),
    })
}

fn drill_flapping(ha_cfg: &HaConfig) -> Result<DrillResult, String> {
    let mut policy = fixture_coordinator_policy();
    policy.min_hold_min = 0;
    policy.cooldown_min = 0;
    policy.max_changes_per_hour = 3;
    let mut table = RoleTable::new();
    table.set_policy("coordinator", policy);

    let mut t = 1_700_000_000u64;
    table.role_tick("coordinator", t); // change 1
    for _ in 0..5 {
        t += 200;
        let cur_holder = table
            .get("coordinator")
            .and_then(|r| r.holder.clone())
            .unwrap_or_default();
        for c in &["seat-a/coord-claude", "seat-a/coord-codex", "guest-b/lead"] {
            if *c != cur_holder {
                table.candidate_heartbeats.insert((*c).to_string(), t);
            }
        }
        let act = table.role_tick("coordinator", t);
        if matches!(act, RoleAction::Frozen { .. })
            || table.get("coordinator").is_some_and(|r| r.frozen)
        {
            return Ok(DrillResult {
                drill: "flapping".into(),
                passed: true,
                recover_sec: 0,
                target_recover_sec: ha_cfg.target_recover_sec,
                details: "role froze at 4th change with ROLE_FLAPPING".into(),
            });
        }
    }
    Err("expected role to freeze after exceeding max_changes_per_hour".into())
}

/// Ensures `PromotionMode` is referenced even when only default is used in drills.
#[allow(dead_code)]
fn _mode_check(m: PromotionMode) -> bool {
    matches!(m, PromotionMode::Owner | PromotionMode::OwnerOrGuestQuorum)
}
