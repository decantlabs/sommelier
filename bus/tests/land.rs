//! Tests for `land` module (LANDING, BATCH, BRANCH, RUN, READY enqueue, branch_sweep 4.3, run_sweep, and ops).

use std::collections::BTreeSet;

use serde_json::{Value, json};
use tempfile::TempDir;

use sommelier::hooks::{OpCaller, OpContext, on_op};
use sommelier::land::{
    BranchRow, LandModule, RunFolderInfo, RunRow, SweepConfig, branch_sweep, query_branches,
    query_landings, query_runs, run_sweep, scan_licence_markers,
};
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::{Store, StoreModule};
use sommelier::task::{TaskRow, query_tasks};

fn make_msg(
    id: u64,
    from: &str,
    role: Role,
    kind: Kind,
    topic: &str,
    body: &str,
    refs: Vec<&str>,
) -> Message {
    Message {
        id,
        ts: "2026-11-25T12:00:00Z".into(),
        from: from.into(),
        role,
        kind,
        topic: topic.into(),
        body: body.into(),
        refs: refs.into_iter().map(str::to_owned).collect(),
        to: Vec::new(),
    }
}

#[test]
fn ready_from_trusted_sender_enqueues_once_and_ignores_untrusted() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();

    // Guest READY must NOT enqueue.
    let guest_ready = make_msg(
        1,
        "guest-user",
        Role::Member,
        Kind::Ready,
        "status",
        "ready",
        vec!["branch:origin/guest/feat", "sha:aaaa1111"],
    );
    store.apply(&guest_ready).unwrap();
    assert!(query_landings(store.connection()).unwrap().is_empty());

    // Trusted orchestrator READY enqueues into LANDING.
    let orch_ready = make_msg(
        2,
        "coord",
        Role::Orchestrator,
        Kind::Ready,
        "status",
        "ready",
        vec!["branch:origin/gemini/feat", "sha:bbbb2222", "task:t-1"],
    );
    store.apply(&orch_ready).unwrap();

    // Duplicate READY for same (ref, sha) keeps queue unique.
    let dup_ready = make_msg(
        3,
        "coord",
        Role::Orchestrator,
        Kind::Ready,
        "status",
        "ready again",
        vec!["branch:origin/gemini/feat", "sha:bbbb2222"],
    );
    store.apply(&dup_ready).unwrap();

    let rows = query_landings(store.connection()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ref_name, "origin/gemini/feat");
    assert_eq!(rows[0].sha, "bbbb2222");
    assert_eq!(rows[0].task_id, "t-1");
    assert_eq!(rows[0].state, "queued");
}

#[test]
fn branch_sweep_orphan_and_deletion_rules() {
    let now = 1_764_000_000_u64;
    let cfg = SweepConfig {
        orphan_hours: 24,
        keep_days: 7,
        our_namespaces: vec!["codex/".into(), "gemini/".into(), "wip/".into()],
    };

    let branches = vec![
        // Orphan branch: > 24h old, not in LANDING, no live lease or delivered task.
        BranchRow {
            remote: "origin".into(),
            ref_name: "gemini/orphan-x".into(),
            head_sha: "dead0001".into(),
            ahead: 2,
            behind: 0,
            last_commit_at: now - 30 * 3600,
            task_id: String::new(),
            state: "active".into(),
            seen_at: now - 30 * 3600,
            msg_id: 1,
        },
        // Our landed branch older than 7 days -> must be deleted.
        BranchRow {
            remote: "origin".into(),
            ref_name: "codex/old-landed".into(),
            head_sha: "cafe0002".into(),
            ahead: 0,
            behind: 0,
            last_commit_at: now - 10 * 86_400,
            task_id: String::new(),
            state: "landed".into(),
            seen_at: now - 10 * 86_400,
            msg_id: 2,
        },
        // Guest landed branch older than 7 days -> NEVER deleted.
        BranchRow {
            remote: "upstream".into(),
            ref_name: "codex/guest-fork".into(),
            head_sha: "beef0003".into(),
            ahead: 0,
            behind: 0,
            last_commit_at: now - 30 * 86_400,
            task_id: String::new(),
            state: "landed".into(),
            seen_at: now - 30 * 86_400,
            msg_id: 3,
        },
        // Guest branch in non-our namespace on origin -> NEVER deleted.
        BranchRow {
            remote: "origin".into(),
            ref_name: "guest/shader-fix".into(),
            head_sha: "feed0004".into(),
            ahead: 0,
            behind: 0,
            last_commit_at: now - 30 * 86_400,
            task_id: String::new(),
            state: "landed".into(),
            seen_at: now - 30 * 86_400,
            msg_id: 4,
        },
    ];

    let res = branch_sweep(
        &branches,
        &[],
        &[],
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        now,
        &cfg,
    );

    assert_eq!(res.orphan_alerts.len(), 1);
    assert_eq!(res.orphan_alerts[0].0, "origin/gemini/orphan-x");
    assert_eq!(res.triage_tasks.len(), 1);
    assert_eq!(res.triage_tasks[0].kind, "triage");
    assert_eq!(res.triage_tasks[0].scope, "read-only");
    assert_eq!(res.triage_tasks[0].priority, "filler");

    assert_eq!(res.delete_branches.len(), 1);
    assert_eq!(res.delete_branches[0].remote, "origin");
    assert_eq!(res.delete_branches[0].ref_name, "codex/old-landed");

    // Second sweep with existing alert and triage task raises no duplicates.
    let open_alerts: BTreeSet<String> = res.orphan_alerts.iter().map(|(k, _)| k.clone()).collect();
    let triage_ids: BTreeSet<String> = res.triage_tasks.iter().map(|t| t.id.clone()).collect();
    let res2 = branch_sweep(
        &branches,
        &[],
        &res.triage_tasks,
        &[],
        &open_alerts,
        &triage_ids,
        now,
        &cfg,
    );
    assert!(res2.orphan_alerts.is_empty());
    assert!(res2.triage_tasks.is_empty());
}

#[test]
fn run_sweep_flags_unregistered_runs_once() {
    let folders = vec![
        RunFolderInfo {
            run_id: "20261125-120000-check".into(),
            folder: "runs/20261125-120000-check".into(),
            machine: "pc1".into(),
            sha: "abc123".into(),
            label: "check".into(),
        },
        RunFolderInfo {
            run_id: "20261125-121000-rogue".into(),
            folder: "runs/20261125-121000-rogue".into(),
            machine: "pc1".into(),
            sha: "def456".into(),
            label: "rogue".into(),
        },
    ];
    let existing = vec![RunRow {
        run_id: "20261125-120000-check".into(),
        registered: true,
        ..RunRow::default()
    }];

    let res = run_sweep(&folders, &existing, &BTreeSet::new());
    assert_eq!(res.unregistered_runs.len(), 1);
    assert_eq!(res.unregistered_runs[0].run_id, "20261125-121000-rogue");
    assert!(!res.unregistered_runs[0].registered);
    assert_eq!(res.alerts.len(), 1);
    assert_eq!(res.alerts[0].0, "20261125-121000-rogue");
}

#[test]
fn land_and_run_ops_and_api_projection() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut next_id = 10_u64;
    let mut emit = |s: &mut Store, mut msg: Message| -> Result<u64, String> {
        next_id += 1;
        msg.id = next_id;
        if msg.ts.is_empty() {
            msg.ts = "2026-11-25T12:00:00Z".into();
        }
        s.apply(&msg)?;
        Ok(msg.id)
    };
    let caller = OpCaller {
        name: "coord".into(),
        role: Role::Orchestrator,
        trust: "owner",
    };

    {
        let mut ctx = OpContext::new(caller, &mut store, 1_764_000_000_000, &mut emit);

        let r1 = on_op(
            &mut ctx,
            "land.enqueue",
            &json!({ "ref": "origin/gemini/a", "sha": "1111aaaa", "priority": "high" }),
        )
        .unwrap();
        assert_eq!(r1.get("duplicate"), Some(&json!(false)));

        let r2 = on_op(
            &mut ctx,
            "land.enqueue",
            &json!({ "ref": "origin/gemini/a", "sha": "1111aaaa" }),
        )
        .unwrap();
        assert_eq!(r2.get("duplicate"), Some(&json!(true)));

        let next = on_op(
            &mut ctx,
            "land.next",
            &json!({ "claim": true, "batch_id": "b-1", "base_sha": "0000" }),
        )
        .unwrap();
        assert_eq!(next["entries"].as_array().unwrap().len(), 1);

        on_op(
            &mut ctx,
            "land.result",
            &json!({ "batch_id": "b-1", "result": "landed", "verifications": 1 }),
        )
        .unwrap();

        on_op(
            &mut ctx,
            "run.register",
            &json!({ "run_id": "r-1", "sha": "1111aaaa", "label": "check", "machine": "pc1" }),
        )
        .unwrap();
        on_op(
            &mut ctx,
            "run.result",
            &json!({ "run_id": "r-1", "outcome": "pass" }),
        )
        .unwrap();
    }

    let mut api_state = serde_json::Map::new();
    LandModule
        .api(store.connection(), 1_764_000_000, &mut api_state)
        .unwrap();
    assert_eq!(api_state["landing"].as_array().unwrap().len(), 1);
    assert_eq!(api_state["landing"][0]["state"], json!("landed"));
    assert_eq!(api_state["batches"].as_array().unwrap().len(), 1);
    assert_eq!(api_state["runs"].as_array().unwrap().len(), 1);
    assert_eq!(api_state["runs"][0]["outcome"], json!("pass"));
    assert_eq!(query_runs(store.connection()).unwrap().len(), 1);
    assert!(query_branches(store.connection()).unwrap().is_empty());
    let _ = query_tasks(store.connection()).unwrap();
    let _: Value = json!(TaskRow::default());
}

#[test]
fn licence_marker_scan_detects_added_markers() {
    let clean_diff = "+// MIT or Apache-2.0\n-GPL-3.0\n";
    assert!(scan_licence_markers(clean_diff).is_empty());

    let bad_diff = "+// SPDX-License-Identifier: GPL-2.0-only\n";
    assert_eq!(scan_licence_markers(bad_diff).len(), 1);
}

#[test]
fn land_approve_and_approved_by_metadata_require_orchestrator_and_per_ref_result_updates_only_target()
 {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut next_id = 1_u64;
    let mut emit = |s: &mut Store, mut msg: Message| -> Result<u64, String> {
        next_id += 1;
        msg.id = next_id;
        if msg.ts.is_empty() {
            msg.ts = "2026-11-25T12:00:00Z".into();
        }
        s.apply(&msg)?;
        Ok(msg.id)
    };

    let member = OpCaller {
        name: "worker1".into(),
        role: Role::Member,
        trust: "guest",
    };
    let orch = OpCaller {
        name: "coord".into(),
        role: Role::Orchestrator,
        trust: "owner",
    };

    // 1. Member supplying `approved_by` on `land.enqueue` must be rejected
    {
        let mut ctx = OpContext::new(member.clone(), &mut store, 1_764_000_000_000, &mut emit);
        let err = on_op(
            &mut ctx,
            "land.enqueue",
            &json!({
                "ref": "origin/gemini/guarded",
                "sha": "11112222",
                "approved_by": "coord",
            }),
        )
        .unwrap_err();
        assert!(err.contains("only an orchestrator"), "{err}");

        // Member calling `land.approve` directly must also be rejected
        let err2 = on_op(
            &mut ctx,
            "land.approve",
            &json!({
                "ref": "origin/gemini/guarded",
                "sha": "11112222",
            }),
        )
        .unwrap_err();
        assert!(err2.contains("only an orchestrator"), "{err2}");

        // Member enqueuing without `approved_by` succeeds
        on_op(
            &mut ctx,
            "land.enqueue",
            &json!({
                "ref": "origin/gemini/guarded",
                "sha": "11112222",
            }),
        )
        .unwrap();
        on_op(
            &mut ctx,
            "land.enqueue",
            &json!({
                "ref": "origin/gemini/clean",
                "sha": "33334444",
            }),
        )
        .unwrap();
    }

    // 2. Orchestrator can approve and per-ref `land.result` only updates the specified ref
    {
        let mut ctx = OpContext::new(orch, &mut store, 1_764_000_010_000, &mut emit);
        on_op(
            &mut ctx,
            "land.result",
            &json!({
                "batch_id": "batch-100",
                "result": "held",
                "ref": "origin/gemini/guarded",
                "sha": "11112222",
            }),
        )
        .unwrap();
        on_op(
            &mut ctx,
            "land.result",
            &json!({
                "batch_id": "batch-100",
                "result": "landed",
                "ref": "origin/gemini/clean",
                "sha": "33334444",
            }),
        )
        .unwrap();
    }

    let landings = query_landings(store.connection()).unwrap();
    let guarded = landings
        .iter()
        .find(|l| l.ref_name == "origin/gemini/guarded")
        .unwrap();
    let clean = landings
        .iter()
        .find(|l| l.ref_name == "origin/gemini/clean")
        .unwrap();
    assert_eq!(
        guarded.state, "held",
        "guarded ref must stay held when sibling ref in same batch lands"
    );
    assert_eq!(clean.state, "landed");
}

#[test]
fn swarm_scripts_omit_local_run_folder_and_claim_no_blind_batch() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let run_game = std::fs::read_to_string(manifest_dir.join("../swarm/run-title.sh")).unwrap();
    assert!(
        !run_game.contains("--folder"),
        "run-title.sh must not publish local filesystem --folder paths to shared bus records"
    );
    let lander = std::fs::read_to_string(manifest_dir.join("../swarm/lander.sh")).unwrap();
    assert!(
        !lander.contains("land next --claim"),
        "lander.sh must not blindly claim bus items unrelated to the local file queue"
    );
}
