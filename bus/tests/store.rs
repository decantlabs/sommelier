//! Integration and property tests for `state.db`, core tables, module registry, cursor,
//! `snapshot`, `replay --verify`, and `/api/state` (step 4).

use std::path::PathBuf;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::{
    CORE_TABLES, SCHEMA_VERSION, Store, StoreModule, combined_state_hash, query_agents,
    query_alerts, query_capacities, query_machines, query_members, query_meta, query_policies,
    read_log_messages,
};

fn fixture_messages() -> Vec<Message> {
    vec![
        Message {
            id: 1,
            ts: "2026-11-25T10:00:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Policy,
            topic: "policy".into(),
            body: json!({"version": 1, "hash": "a1b2c3d4e5f60001", "mode": "off", "published_at": 1795600800}).to_string(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 2,
            ts: "2026-11-25T10:01:00.000Z".into(),
            from: "coord".into(),
            role: Role::Orchestrator,
            kind: Kind::Hello,
            topic: String::new(),
            body: "machine=hub-box os=windows gpu=rtx4090 class=claude:2,codex:4 mode=auto gpu_slots=1 disk_free_gb=120 agent_version=0.3.0 policy_hash=a1b2c3d4e5f60001 accepts=fix,review,run,scout".into(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 3,
            ts: "2026-11-25T10:02:00.000Z".into(),
            from: "guest1".into(),
            role: Role::Member,
            kind: Kind::Hello,
            topic: String::new(),
            body: "machine=linux-box os=linux gpu=rx7800 class=gemini:2 mode=own gpu_slots=1 accepts=fix,review".into(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 4,
            ts: "2026-11-25T10:03:00.000Z".into(),
            from: "coord".into(),
            role: Role::Orchestrator,
            kind: Kind::Status,
            topic: "agent/w1".into(),
            body: "building d3d12 views".into(),
            refs: vec![
                "state:running".into(),
                "model:codex".into(),
                "branch:codex/fix-views".into(),
                "task:t101".into(),
                "machine:hub-box".into(),
            ],
            to: vec![],
        },
        Message {
            id: 5,
            ts: "2026-11-25T10:04:00.000Z".into(),
            from: "guest1".into(),
            role: Role::Member,
            kind: Kind::Status,
            topic: "agent/main".into(),
            body: "reviewing shaders".into(),
            refs: vec![
                "state:running".into(),
                "model:gemini".into(),
                "branch:gemini/shader-review".into(),
            ],
            to: vec![],
        },
        Message {
            id: 6,
            ts: "2026-11-25T10:05:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Alert,
            topic: "alert/IDLE/coord/codex/2".into(),
            body: json!({
                "v": 1,
                "code": "IDLE",
                "key": "coord/codex/2",
                "severity": "warn",
                "state": "open",
                "opened_at": 1795601100,
                "last_at": 1795601100,
                "count": 1,
                "text": "slot coord/codex/2 (codex) idle for 600 s",
                "data": {"owner": "coord", "class": "codex", "idle_secs": 600}
            })
            .to_string(),
            refs: vec!["state:open".into()],
            to: vec!["coord".into()],
        },
        Message {
            id: 7,
            ts: "2026-11-25T10:06:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: "capacity/guest2/rig2".into(),
            body: json!({
                "v": 1,
                "owner": "guest2",
                "machine": "rig2",
                "os": "linux",
                "gpu": "rtx3080",
                "machine_class": "linux-rtx3080",
                "gpu_slots": 1,
                "disk_free_gb": 85,
                "agent_version": "0.3.0",
                "policy_hash": "a1b2c3d4e5f60001",
                "mode": "auto",
                "class": "codex",
                "useful_parallel": 3,
                "accepts": ["fix", "scout"],
                "pools": ["codex.5h", "codex.week"],
                "declared_at": 1795601160,
                "heartbeat_at": 1795601160
            })
            .to_string(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 8,
            ts: "2026-11-25T10:07:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Policy,
            topic: "policy".into(),
            body: json!({"version": 2, "hash": "b2c3d4e5f6000002", "mode": "shadow", "published_at": 1795601220}).to_string(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 9,
            ts: "2026-11-25T10:08:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Alert,
            topic: "alert/IDLE/coord/codex/2".into(),
            body: json!({
                "v": 1,
                "code": "IDLE",
                "key": "coord/codex/2",
                "severity": "info",
                "state": "closed",
                "opened_at": 1795601100,
                "last_at": 1795601280,
                "count": 1,
                "text": "slot coord/codex/2 (codex) is now working",
                "data": {"owner": "coord", "class": "codex"}
            })
            .to_string(),
            refs: vec!["state:closed".into()],
            to: vec!["coord".into()],
        },
        Message {
            id: 10,
            ts: "2026-11-25T10:09:00.000Z".into(),
            from: "coord".into(),
            role: Role::Orchestrator,
            kind: Kind::Ready,
            topic: "issue/101".into(),
            body: "d3d12 views fixed".into(),
            refs: vec!["codex/fix-views".into(), "0123456789abcdef".into()],
            to: vec![],
        },
    ]
}

fn write_log_file(dir: &std::path::Path, msgs: &[Message]) -> PathBuf {
    let path = dir.join("bus.jsonl");
    let mut lines = String::new();
    for m in msgs {
        lines.push_str(&serde_json::to_string(m).unwrap());
        lines.push('\n');
    }
    std::fs::write(&path, lines).unwrap();
    path
}

#[test]
fn replay_fixture_log_twice_gives_equal_table_hashes() {
    let msgs = fixture_messages();

    let dir1 = tempfile::tempdir().unwrap();
    let log1 = write_log_file(dir1.path(), &msgs);
    let mut store1 = Store::open(dir1.path()).unwrap();
    let rep1 = store1.rebuild_from_log(&log1).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let log2 = write_log_file(dir2.path(), &msgs);
    let mut store2 = Store::open(dir2.path()).unwrap();
    let rep2 = store2.rebuild_from_log(&log2).unwrap();

    assert_eq!(rep1.messages_applied, msgs.len());
    assert_eq!(rep1.cursor, 10);
    assert_eq!(rep1.table_hashes, rep2.table_hashes);
    assert_eq!(rep1.state_hash, rep2.state_hash);

    // Replaying the same log a second time into store1 produces the exact same table hashes
    let rep1_again = store1.rebuild_from_log(&log1).unwrap();
    assert_eq!(rep1.table_hashes, rep1_again.table_hashes);
    assert_eq!(rep1.state_hash, rep1_again.state_hash);

    // Every core table must be present in the hash map
    for table in CORE_TABLES {
        assert!(
            rep1.table_hashes.contains_key(table),
            "missing core table {table}"
        );
    }

    // Verify projected rows in the core tables
    let members = query_members(store1.connection()).unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].name, "coord");
    assert_eq!(members[0].role, "orchestrator");
    assert_eq!(members[0].trust, "owner");
    assert_eq!(members[0].msg_id, 10);
    assert_eq!(members[1].name, "guest1");
    assert_eq!(members[1].role, "member");
    assert_eq!(members[1].trust, "guest");
    assert_eq!(members[1].msg_id, 5);

    let machines = query_machines(store1.connection()).unwrap();
    assert_eq!(machines.len(), 3);
    assert_eq!(machines[0].owner, "coord");
    assert_eq!(machines[0].machine, "hub-box");
    assert_eq!(machines[0].os, "windows");
    assert_eq!(machines[0].gpu, "rtx4090");
    assert_eq!(machines[0].disk_free_gb, 120);
    assert_eq!(machines[2].owner, "guest2");
    assert_eq!(machines[2].machine, "rig2");
    assert_eq!(machines[2].gpu_slots, 1);

    let caps = query_capacities(store1.connection()).unwrap();
    assert_eq!(caps.len(), 4);
    let coord_codex = caps
        .iter()
        .find(|c| c.owner == "coord" && c.class == "codex")
        .unwrap();
    assert_eq!(coord_codex.useful_parallel, 4);
    assert_eq!(coord_codex.mode, "auto");
    assert_eq!(coord_codex.accepts, vec!["fix", "review", "run", "scout"]);

    let agents = query_agents(store1.connection()).unwrap();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].owner, "coord");
    assert_eq!(agents[0].name, "w1");
    assert_eq!(agents[0].class, "codex");
    assert_eq!(agents[0].task_id, "t101");
    assert!(agents[0].pid_alive);
    assert_eq!(agents[0].source, "process");

    let alerts = query_alerts(store1.connection()).unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].code, "IDLE");
    assert_eq!(alerts[0].key, "coord/codex/2");
    assert_eq!(alerts[0].state, "closed");
    assert_eq!(alerts[0].msg_id, 9);

    let policies = query_policies(store1.connection()).unwrap();
    assert_eq!(policies.len(), 2);
    assert_eq!(policies[0].version, 1);
    assert_eq!(policies[1].version, 2);
    assert_eq!(policies[1].mode, "shadow");

    let meta = query_meta(store1.connection()).unwrap();
    assert!(
        meta.iter()
            .any(|m| m.name == "cursor" && m.value == "10" && m.msg_id == 10)
    );
    assert!(
        meta.iter()
            .any(|m| m.name == "schema_version" && m.value == SCHEMA_VERSION.to_string())
    );
}

#[test]
fn kill_in_middle_of_transaction_leaves_cursor_and_tables_consistent() {
    let msgs = fixture_messages();
    let dir = tempfile::tempdir().unwrap();
    let log_path = write_log_file(dir.path(), &msgs);
    let mut store = Store::open(dir.path()).unwrap();

    // Apply first 4 messages cleanly
    store.apply_batch(&msgs[..4]).unwrap();
    assert_eq!(store.cursor().unwrap(), 4);
    let hashes_at_4 = store.table_hashes().unwrap();
    let hash_at_4 = combined_state_hash(&hashes_at_4);

    // Simulate a kill in the middle of applying messages 5..10 (after 3 messages inside the tx)
    let err = store.apply_batch_fault_after(&msgs[4..], 3).unwrap_err();
    assert!(err.contains("simulated kill"));

    // Cursor and every table hash must still match message 4 exactly
    assert_eq!(store.cursor().unwrap(), 4);
    assert_eq!(store.table_hashes().unwrap(), hashes_at_4);
    assert_eq!(store.state_hash().unwrap(), hash_at_4);

    // Reopen the store from disk (as after a process kill) and verify cursor is still 4
    drop(store);
    let mut reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.cursor().unwrap(), 4);
    assert_eq!(reopened.table_hashes().unwrap(), hashes_at_4);

    // Catching up from the log applies messages 5..=10 and reaches identical final state as a clean replay
    let (applied, end_cursor) = reopened.catch_up_from_log(&log_path).unwrap();
    assert_eq!(applied, 6);
    assert_eq!(end_cursor, 10);

    let ver = reopened.replay_verify(&log_path).unwrap();
    assert!(ver.verified);
    assert_eq!(ver.cursor, 10);
}

struct DummyTaskModule;

impl StoreModule for DummyTaskModule {
    fn name(&self) -> &'static str {
        "dummy_task"
    }

    fn schema(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS DUMMY_TASK (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            msg_id INTEGER NOT NULL
        );"
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        if msg.kind == Kind::Task {
            tx.execute(
                "INSERT OR REPLACE INTO DUMMY_TASK (id, title, msg_id) VALUES (?1, ?2, ?3)",
                params![msg.topic, msg.body, msg.id as i64],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let mut stmt = conn
            .prepare("SELECT id, title, msg_id FROM DUMMY_TASK ORDER BY id ASC")
            .map_err(|e| e.to_string())?;
        let rows: Vec<Value> = stmt
            .query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "title": r.get::<_, String>(1)?,
                    "msg_id": r.get::<_, i64>(2)?,
                }))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        state.insert("dummy_tasks".into(), Value::Array(rows));
        Ok(())
    }
}

#[test]
fn module_registry_calls_schema_apply_and_api_and_serves_api_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut msgs = fixture_messages();
    msgs.push(Message {
        id: 11,
        ts: "2026-11-25T10:10:00.000Z".into(),
        from: "coord".into(),
        role: Role::Orchestrator,
        kind: Kind::Task,
        topic: "task/t200".into(),
        body: "fix shader compiler".into(),
        refs: vec![],
        to: vec![],
    });
    let log_path = write_log_file(dir.path(), &msgs);

    let mut store = Store::open(dir.path()).unwrap();
    store.register_module(Box::new(DummyTaskModule)).unwrap();
    store.catch_up_from_log(&log_path).unwrap();

    let api = store.api_state(1795601500).unwrap();
    assert_eq!(api["hub"]["id"], 11);
    assert_eq!(api["as_of"], 1795601500);
    assert_eq!(api["store"]["cursor"], 11);
    assert_eq!(api["dummy_tasks"][0]["id"], "task/t200");
    assert_eq!(api["dummy_tasks"][0]["title"], "fix shader compiler");
    assert_eq!(api["dummy_tasks"][0]["msg_id"], 11);
    assert_eq!(api["members"].as_array().unwrap().len(), 2);
    assert_eq!(api["agents"].as_array().unwrap().len(), 2);

    // Test snapshot creation and metadata update
    let (snap_rep, snap_val) = store.write_snapshot(dir.path(), None, 1795601500).unwrap();
    assert_eq!(snap_rep.cursor, 11);
    assert!(snap_rep.snapshot_id.starts_with("snap-11-"));
    assert_eq!(store.snapshot_id().unwrap(), snap_rep.snapshot_id);
    assert_eq!(snap_val["cursor"], 11);
    assert!(dir.path().join("state.snapshot.json").is_file());
}

#[test]
fn damaged_state_db_is_renamed_and_rebuilt_from_log() {
    let dir = tempfile::tempdir().unwrap();
    let msgs = fixture_messages();
    write_log_file(dir.path(), &msgs);
    let db_path = dir.path().join("state.db");
    std::fs::write(&db_path, b"not a valid sqlite database file header").unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.cursor().unwrap(), 10);
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|n| n.starts_with("state.db.damaged-")),
        "expected damaged db backup in {entries:?}"
    );
}

#[test]
#[ignore = "local check on a copy of a real log, ignored in CI (section 10.3 step 4)"]
fn replay_verify_on_copy_of_real_log_ends_without_error() {
    let default_dir = sommelier::default_data_dir();
    let candidates = [
        default_dir.join("bus.jsonl"),
        PathBuf::from(r"C:/runs/bus.jsonl"),
    ];
    let Some(real_log) = candidates.into_iter().find(|p| p.is_file()) else {
        // If no local live log exists on this machine, verify on a copy of our fixture log
        let tmp = tempfile::tempdir().unwrap();
        let log_copy = write_log_file(tmp.path(), &fixture_messages());
        let mut store = Store::open(tmp.path()).unwrap();
        let report = store.replay_verify(&log_copy).unwrap();
        assert!(report.verified);
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let log_copy = tmp.path().join("bus.jsonl");
    std::fs::copy(&real_log, &log_copy).unwrap();

    let msgs = read_log_messages(&log_copy).unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    let report = store.replay_verify(&log_copy).unwrap();
    assert!(report.verified);
    assert_eq!(report.messages_applied, msgs.len());
}

#[tokio::test]
async fn cli_snapshot_and_replay_verify_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let msgs = fixture_messages();
    let log_path = write_log_file(dir.path(), &msgs);
    let data_arg = dir.path().to_string_lossy().into_owned();
    let log_arg = log_path.to_string_lossy().into_owned();

    let code = sommelier::cli::state::run(
        "replay",
        &["--data".into(), data_arg.clone(), "--json".into()],
    )
    .await
    .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    let code = sommelier::cli::state::run(
        "replay",
        &[
            "--verify".into(),
            "--data".into(),
            data_arg.clone(),
            "--log".into(),
            log_arg,
            "--json".into(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    let code =
        sommelier::cli::state::run("snapshot", &["--data".into(), data_arg, "--json".into()])
            .await
            .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(dir.path().join("state.snapshot.json").is_file());
}
