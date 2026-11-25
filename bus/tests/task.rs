//! Integration and unit tests for task queue, leases, offers, checkpoints, brief hash check,
//! lease reaper, `done` validation, `BOARD.tsv` import, and replay determinism (step 8).

use serde_json::{Map, Value, json};
use sommelier::brief::{self, sha256_hex};
use sommelier::hooks::{self, OpCaller, OpContext};
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::{Store, all_table_hashes, combined_state_hash};
use sommelier::task::{
    LeaseTimers, TASK_TABLES, TaskState, board_row_to_task_payload, parse_board_tsv,
    query_active_lease_for_task, query_checkpoints, query_leases, query_offers, query_task,
    reap_leases_and_offers, select_next_ready,
};
use sommelier::time::rfc3339;
use sommelier::tokens::Member;
use tempfile::TempDir;

fn make_ctx_emit(
    caller: OpCaller,
    store: &mut Store,
    next_id: &mut u64,
    now_ms: u64,
    op: &str,
    body: &Value,
) -> Result<(Map<String, Value>, Vec<Message>), String> {
    let mut emitted = Vec::new();
    let mut emit_fn = |st: &mut Store, mut msg: Message| -> Result<u64, String> {
        msg.id = *next_id;
        *next_id += 1;
        if msg.ts.is_empty() {
            msg.ts = rfc3339(now_ms);
        }
        st.apply(&msg)?;
        emitted.push(msg);
        Ok(*next_id - 1)
    };
    let mut ctx = OpContext::new(caller, store, now_ms, &mut emit_fn);
    let res = hooks::on_op(&mut ctx, op, body)?;
    Ok((res, emitted))
}

fn orch_caller() -> OpCaller {
    OpCaller {
        name: "coord".into(),
        role: Role::Orchestrator,
        trust: "owner",
    }
}

fn worker_caller(name: &str) -> OpCaller {
    OpCaller {
        name: name.into(),
        role: Role::Member,
        trust: "guest",
    }
}

#[test]
fn state_machine_table_test() {
    use TaskState::*;
    let cases = [
        (Draft, Ready, true),
        (Draft, Cancelled, true),
        (Draft, Leased, false),
        (Ready, Offered, true),
        (Ready, Leased, true),
        (Ready, Closed, false),
        (Offered, Leased, true),
        (Offered, Ready, true),
        (Leased, Running, true),
        (Leased, Delivered, true),
        (Leased, Ready, true),
        (Running, Delivered, true),
        (Running, Stuck, true),
        (Running, Ready, true),
        (Delivered, Landing, true),
        (Delivered, Closed, true),
        (Landing, Closed, true),
        (Stuck, Ready, true),
        (Stuck, Cancelled, true),
        (Closed, Ready, false),
        (Cancelled, Ready, false),
    ];
    for (from, to, expected) in cases {
        assert_eq!(
            from.can_transition(to),
            expected,
            "transition {:?} -> {:?} expected {expected}",
            from,
            to
        );
    }
}

#[test]
fn silent_lease_is_requeued_with_last_checkpoint_and_attempt_plus_one() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;

    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-reap",
            "title": "fix shader bounds",
            "kind": "fix",
            "priority": "high",
            "scope": "src/shaders/*",
            "branch": "codex/t-reap",
            "done_when": "cargo test passes",
        }),
    )
    .unwrap();

    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 10_000,
        "task.work",
        &json!({
            "task": "t-reap",
            "worker": "w1-codex",
            "class": "codex",
            "supervised": true,
        }),
    )
    .unwrap();

    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 30_000,
        "task.checkpoint",
        &json!({
            "task": "t-reap",
            "branch": "codex/t-reap",
            "sha": "abc1234",
            "pushed": true,
            "note": "STATE: halfway\nDONE: parsed IR\nNEXT: emit bounds check",
        }),
    )
    .unwrap();

    let timers = LeaseTimers::default();
    let hub_started_at = t0_ms / 1000;

    // After 130s from last heartbeat (t0 + 30s): suspect, not yet dead.
    let msgs_suspect = reap_leases_and_offers(
        store.connection(),
        (t0_ms / 1000) + 30 + 130,
        hub_started_at,
        timers,
    )
    .unwrap();
    assert_eq!(msgs_suspect.len(), 1);
    assert_eq!(msgs_suspect[0].kind, Kind::TaskUpdate);
    for mut m in msgs_suspect {
        m.id = next_id;
        next_id += 1;
        store.apply(&m).unwrap();
    }
    let lease = query_active_lease_for_task(store.connection(), "t-reap")
        .unwrap()
        .unwrap();
    assert_eq!(lease.state, "suspect");

    // After 200s from last heartbeat (> 180s supervised limit, and > 120s restart grace): released + requeued.
    let msgs_dead = reap_leases_and_offers(
        store.connection(),
        (t0_ms / 1000) + 30 + 200,
        hub_started_at,
        timers,
    )
    .unwrap();
    assert_eq!(msgs_dead.len(), 2);
    assert_eq!(msgs_dead[0].kind, Kind::TaskUpdate);
    assert_eq!(msgs_dead[1].kind, Kind::Alert);
    for mut m in msgs_dead {
        m.id = next_id;
        next_id += 1;
        store.apply(&m).unwrap();
    }

    let task = query_task(store.connection(), "t-reap").unwrap().unwrap();
    assert_eq!(task.state, "ready");
    assert_eq!(task.attempt, 2);
    assert!(task.resume_note.contains("emit bounds check"));
    assert_eq!(task.resume_sha, "abc1234");
    assert!(
        query_active_lease_for_task(store.connection(), "t-reap")
            .unwrap()
            .is_none()
    );
}

#[test]
fn second_lease_on_same_task_is_refused() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;

    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-dup",
            "title": "single holder",
            "kind": "fix",
            "scope": "src/a.rs",
            "done_when": "tests pass",
        }),
    )
    .unwrap();

    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 1_000,
        "task.work",
        &json!({ "task": "t-dup" }),
    )
    .unwrap();

    let err = make_ctx_emit(
        worker_caller("w2"),
        &mut store,
        &mut next_id,
        t0_ms + 2_000,
        "task.work",
        &json!({ "task": "t-dup" }),
    )
    .unwrap_err();
    assert!(err.contains("already has an active lease"), "{err}");
}

#[test]
fn brief_with_wrong_hash_is_refused() {
    let dir = TempDir::new().unwrap();
    let content = b"TASK t-hash BRANCH codex/t-hash\nDO: fix bug\n";
    let real_sha = sha256_hex(content);
    let wrong_sha = "0".repeat(64);

    let orch = Member {
        name: "coord".into(),
        role: Role::Orchestrator,
        salt: String::new(),
        hash: "x".into(),
        created: "2026-11-25T10:00:00Z".into(),
        token: None,
    };

    // PUT with wrong sha256 query parameter is refused with 400.
    let bad = brief::on_http(
        dir.path(),
        "PUT",
        "/api/brief/t-hash",
        Some(&format!("sha256={wrong_sha}")),
        content,
        Some(&orch),
    )
    .unwrap();
    assert_eq!(bad.status, 400);

    // PUT with matching sha256 succeeds and GET by hash returns the brief.
    let ok = brief::on_http(
        dir.path(),
        "PUT",
        "/api/brief/t-hash",
        Some(&format!("sha256={real_sha}")),
        content,
        Some(&orch),
    )
    .unwrap();
    assert_eq!(ok.status, 200);

    let fetched = brief::on_http(
        dir.path(),
        "GET",
        &format!("/api/brief/{real_sha}"),
        None,
        &[],
        Some(&orch),
    )
    .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.body, content);

    // `task.work` with wrong brief_sha256 is also refused.
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;
    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-hash",
            "title": "verify brief hash",
            "kind": "fix",
            "scope": "src/b.rs",
            "done_when": "hash checked",
            "brief_sha256": real_sha,
        }),
    )
    .unwrap();

    let err = make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 1_000,
        "task.work",
        &json!({ "task": "t-hash", "brief_sha256": wrong_sha }),
    )
    .unwrap_err();
    assert!(err.contains("brief sha256 mismatch"), "{err}");
}

#[test]
fn done_without_prior_art_verdict_is_refused_when_required() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;

    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-pa",
            "title": "implement decoder",
            "kind": "implement",
            "scope": "src/dec.rs",
            "branch": "codex/t-pa",
            "prior_art": true,
            "done_when": "tests pass",
        }),
    )
    .unwrap();

    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 1_000,
        "task.work",
        &json!({ "task": "t-pa" }),
    )
    .unwrap();

    // Missing PRIOR ART line -> refused.
    let err = make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 2_000,
        "task.done",
        &json!({
            "task": "t-pa",
            "ref": "codex/t-pa",
            "sha": "deadbeef1234",
            "result_text": "READY codex/t-pa deadbeef1234 : cargo test passed",
        }),
    )
    .unwrap_err();
    assert!(err.contains("prior art verdict"), "{err}");

    // With PRIOR ART verdict line -> accepted.
    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 3_000,
        "task.done",
        &json!({
            "task": "t-pa",
            "ref": "codex/t-pa",
            "sha": "deadbeef1234",
            "result_text": "READY codex/t-pa deadbeef1234 : cargo test passed\nPRIOR ART: clean",
        }),
    )
    .unwrap();

    let task = query_task(store.connection(), "t-pa").unwrap().unwrap();
    assert_eq!(task.state, "delivered");
    assert_eq!(task.result_sha, "deadbeef1234");
}

#[test]
fn next_selects_by_score_order_and_import_board_parses_tsv() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;

    for (id, prio, scope) in [
        ("t-filler", "filler", "src/f.rs"),
        ("t-normal", "normal", "src/n.rs"),
        ("t-blocker", "blocker", "src/b.rs"),
    ] {
        make_ctx_emit(
            orch_caller(),
            &mut store,
            &mut next_id,
            t0_ms,
            "task.add",
            &json!({
                "id": id,
                "title": id,
                "kind": "fix",
                "priority": prio,
                "scope": scope,
                "done_when": "ok",
            }),
        )
        .unwrap();
    }

    let (top, score, count) = select_next_ready(store.connection(), t0_ms / 1000, None)
        .unwrap()
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(top.id, "t-blocker");
    assert_eq!(score, 100);

    let tsv = "\u{feff}id\ttopic\towner\tclass\tbranch\tfiles\tstate\n\
               10\tissue/10\tw1\tcodex\tcodex/i10\tsrc/a.rs\trunning\n\
               11\tissue/11\t-\tgemini\t-\tsrc/b.rs\tdone\n";
    let rows = parse_board_tsv(tsv).unwrap();
    assert_eq!(rows.len(), 2);
    let p0 = board_row_to_task_payload(&rows[0]);
    let p1 = board_row_to_task_payload(&rows[1]);
    assert_eq!(p0["id"], "10");
    assert_eq!(p0["state"], "ready");
    assert_eq!(p1["id"], "11");
    assert_eq!(p1["state"], "closed");
}

#[test]
fn replay_rebuilds_identical_task_tables_and_api_state() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("bus.jsonl");

    let mut store1 = Store::open_in_memory().unwrap();
    let t0_ms = 1_795_600_000_000u64;
    let mut all_msgs = Vec::new();
    let mut next_id = 1u64;

    let mut run_op = |caller: OpCaller, now_ms: u64, op: &str, body: Value| {
        let mut emitted = Vec::new();
        let mut emit_fn = |st: &mut Store, mut msg: Message| -> Result<u64, String> {
            msg.id = next_id;
            next_id += 1;
            if msg.ts.is_empty() {
                msg.ts = rfc3339(now_ms);
            }
            st.apply(&msg)?;
            emitted.push(msg);
            Ok(next_id - 1)
        };
        let mut ctx = OpContext::new(caller, &mut store1, now_ms, &mut emit_fn);
        hooks::on_op(&mut ctx, op, &body).unwrap();
        all_msgs.extend(emitted);
    };

    run_op(
        orch_caller(),
        t0_ms,
        "task.add",
        json!({
            "id": "t-rep",
            "title": "replay test",
            "kind": "fix",
            "scope": "src/r.rs",
            "branch": "codex/t-rep",
            "done_when": "ok",
        }),
    );
    run_op(
        worker_caller("w1"),
        t0_ms + 1_000,
        "task.work",
        json!({ "task": "t-rep", "worker": "w1" }),
    );
    run_op(
        worker_caller("w1"),
        t0_ms + 2_000,
        "task.checkpoint",
        json!({ "task": "t-rep", "note": "step 1", "sha": "1111222" }),
    );
    run_op(
        worker_caller("w1"),
        t0_ms + 3_000,
        "task.done",
        json!({
            "task": "t-rep",
            "ref": "codex/t-rep",
            "sha": "1111222",
            "result_text": "READY codex/t-rep 1111222 : ok",
        }),
    );

    let log_text: String = all_msgs
        .iter()
        .map(|m| format!("{}\n", serde_json::to_string(m).unwrap()))
        .collect();
    std::fs::write(&log_path, log_text).unwrap();

    let mut replay_a = Store::open_in_memory().unwrap();
    replay_a.rebuild_from_log(&log_path).unwrap();
    let mut replay_b = Store::open_in_memory().unwrap();
    replay_b.rebuild_from_log(&log_path).unwrap();

    let hashes_a = all_table_hashes(replay_a.connection()).unwrap();
    let hashes_b = all_table_hashes(replay_b.connection()).unwrap();
    assert_eq!(hashes_a, hashes_b);
    assert_eq!(
        combined_state_hash(&hashes_a),
        combined_state_hash(&hashes_b)
    );
    for table in TASK_TABLES {
        assert!(hashes_a.contains_key(table), "missing table {table}");
    }

    assert_eq!(query_leases(replay_a.connection()).unwrap().len(), 1);
    assert_eq!(query_checkpoints(replay_a.connection()).unwrap().len(), 1);
    assert!(query_offers(replay_a.connection()).unwrap().is_empty());

    let api = replay_a.api_state(t0_ms / 1000).unwrap();
    assert_eq!(api["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(api["leases"].as_array().unwrap().len(), 1);
    assert_eq!(api["checkpoints"].as_array().unwrap().len(), 1);
}

#[test]
fn offered_task_verifies_recipient_and_accepts_only_matching_offer() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;

    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-offer",
            "title": "offered task",
            "kind": "fix",
            "scope": "src/o.rs",
            "done_when": "tests pass",
        }),
    )
    .unwrap();

    // Emit two open offers on t-offer: one to w1, one to w2
    for (oid, owner) in [("off-w1", "w1"), ("off-w2", "w2")] {
        let msg = Message {
            id: next_id,
            ts: rfc3339(t0_ms + 1_000),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Offer,
            topic: "task/t-offer".into(),
            body: json!({
                "v": 1,
                "offer_id": oid,
                "task": "t-offer",
                "to_owner": owner,
                "class": "codex",
                "sent_at": t0_ms / 1000,
                "expires": (t0_ms / 1000) + 300,
                "state": "open",
            })
            .to_string(),
            refs: vec!["offer:open".into()],
            to: vec![owner.into()],
        };
        next_id += 1;
        store.apply(&msg).unwrap();
    }

    // Non-recipient w3 must be refused when trying to claim the offered task
    let err = make_ctx_emit(
        worker_caller("w3"),
        &mut store,
        &mut next_id,
        t0_ms + 2_000,
        "task.work",
        &json!({ "task": "t-offer" }),
    )
    .unwrap_err();
    assert!(
        err.contains("offered to"),
        "expected offer recipient mismatch error, got: {err}"
    );

    // Recipient w1 succeeds and ONLY off-w1 is marked accepted (off-w2 stays open)
    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        t0_ms + 3_000,
        "task.work",
        &json!({ "task": "t-offer" }),
    )
    .unwrap();

    let offers = query_offers(store.connection()).unwrap();
    let off1 = offers.iter().find(|o| o.id == "off-w1").unwrap();
    let off2 = offers.iter().find(|o| o.id == "off-w2").unwrap();
    assert_eq!(off1.state, "accepted");
    assert_eq!(off2.state, "open");
}

#[test]
fn short_advertised_lease_expires_at_expires_at_even_when_heartbeat_is_recent() {
    let mut store = Store::open_in_memory().unwrap();
    let mut next_id = 1u64;
    let t0_ms = 1_795_600_000_000u64;
    let t0_sec = t0_ms / 1000;

    make_ctx_emit(
        orch_caller(),
        &mut store,
        &mut next_id,
        t0_ms,
        "task.add",
        &json!({
            "id": "t-short",
            "title": "short lease task",
            "kind": "fix",
            "scope": "src/s.rs",
            "done_when": "tests pass",
        }),
    )
    .unwrap();

    // Unsupervised worker takes a 60-second lease at t0 + 130s (past restart grace)
    let grant_ms = t0_ms + 130_000;
    make_ctx_emit(
        worker_caller("w1"),
        &mut store,
        &mut next_id,
        grant_ms,
        "task.work",
        &json!({
            "task": "t-short",
            "lease_sec": 60,
            "supervised": false,
        }),
    )
    .unwrap();

    let timers = LeaseTimers::default();

    // Before expires_at (grant + 30s): not reaped
    let before = reap_leases_and_offers(store.connection(), t0_sec + 160, t0_sec, timers).unwrap();
    assert!(before.is_empty());

    // At expires_at (grant + 65s, well below the 45-minute default silence limit): reaped!
    let after = reap_leases_and_offers(store.connection(), t0_sec + 195, t0_sec, timers).unwrap();
    assert_eq!(
        after.len(),
        2,
        "short lease must expire once expires_at passes"
    );
    for mut m in after {
        m.id = next_id;
        next_id += 1;
        store.apply(&m).unwrap();
    }
    let task = query_task(store.connection(), "t-short").unwrap().unwrap();
    assert_eq!(task.state, "ready");
    assert_eq!(task.attempt, 2);
}
