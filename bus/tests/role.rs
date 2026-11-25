//! Tests for the `ROLE` table, role leases with epochs, fencing of role commands,
//! and the first election rule of section 13.3 (step 7).
//!
//! Done when (step 7):
//! - A silent holder loses the lease after `dead_sec`
//! - The next candidate gets `epoch + 1`
//! - A command with the old epoch is refused with code 3
//! - A limited lease is refused the full-only commands

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::Connection;
use serde_json::Value;
use sommelier::role::{
    LeaseScope, RoleAction, RoleCandidate, RoleCommand, RolePolicy, RoleState, RoleStore,
    RoleTable, apply, query_all, query_row, schema, upsert_row,
};

static ROLE_TEST_SEQ: AtomicU32 = AtomicU32::new(1);

fn temp_role_dir(label: &str) -> PathBuf {
    let n = ROLE_TEST_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "sommelier-role-test-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn coordinator_policy() -> RolePolicy {
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

#[test]
fn silent_holder_loses_lease_after_dead_sec_and_next_candidate_gets_epoch_plus_one() {
    let mut table = RoleTable::new();
    table.set_policy("coordinator", coordinator_policy());

    let t0 = 1_000_000;
    // Initial election on vacant role picks tier 1 candidate at epoch 1
    let act1 = table.role_tick("coordinator", t0);
    assert_eq!(
        act1,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "seat-a/coord-claude".into(),
            lease_scope: LeaseScope::Full,
            epoch: 1,
            reason: "vacant".into(),
        }
    );

    // At t0 + 60s and t0 + 180s (within dead_sec = 180), without heartbeat, holder still holds lease
    let act_alive = table.role_tick("coordinator", t0 + 180);
    assert_eq!(
        act_alive,
        RoleAction::Renewed {
            role: "coordinator".into(),
            holder: "seat-a/coord-claude".into(),
            epoch: 1,
        }
    );

    // Heartbeat at t0 + 100s extends liveness
    assert!(table.heartbeat("coordinator", "seat-a/coord-claude", t0 + 100));
    assert_eq!(
        table.role_tick("coordinator", t0 + 280),
        RoleAction::Renewed {
            role: "coordinator".into(),
            holder: "seat-a/coord-claude".into(),
            epoch: 1,
        }
    );

    // Silent past dead_sec (t0 + 100 + 181 = t0 + 281): first holder loses lease,
    // next candidate in policy order gets epoch + 1 (epoch 2).
    let act2 = table.role_tick("coordinator", t0 + 281);
    assert_eq!(
        act2,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "seat-a/coord-codex".into(),
            lease_scope: LeaseScope::Full,
            epoch: 2,
            reason: "heartbeat timeout".into(),
        }
    );

    let row = table.get("coordinator").unwrap();
    assert_eq!(row.holder.as_deref(), Some("seat-a/coord-codex"));
    assert_eq!(row.epoch, 2);
    assert_eq!(row.state, RoleState::Held);
    assert_eq!(row.lease_scope, LeaseScope::Full);

    // Even if seat-a/coord-claude sends a heartbeat right after losing the lease,
    // it is on cooldown (30 min) and stickiness keeps seat-a/coord-codex anyway.
    table.heartbeat("coordinator", "seat-a/coord-claude", t0 + 290);
    table.heartbeat("coordinator", "seat-a/coord-codex", t0 + 290);
    assert_eq!(
        table.role_tick("coordinator", t0 + 300),
        RoleAction::Renewed {
            role: "coordinator".into(),
            holder: "seat-a/coord-codex".into(),
            epoch: 2,
        }
    );

    // Now if seat-a/coord-codex also goes silent for > 180s (at t0 + 290 + 181 = t0 + 471),
    // seat-a/coord-claude is still in its 30-min cooldown, so tier 3 guest-b/lead is elected
    // with epoch 3 and limited scope!
    let act3 = table.role_tick("coordinator", t0 + 471);
    assert_eq!(
        act3,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "guest-b/lead".into(),
            lease_scope: LeaseScope::Limited,
            epoch: 3,
            reason: "heartbeat timeout".into(),
        }
    );
}

#[test]
fn stale_epoch_and_limited_lease_full_commands_are_refused_with_code_3() {
    let mut table = RoleTable::new();
    table.set_policy("coordinator", coordinator_policy());

    let t0 = 2_000_000;
    table.role_tick("coordinator", t0);
    // Force failover to epoch 2
    table.role_tick("coordinator", t0 + 181);
    assert_eq!(table.get("coordinator").unwrap().epoch, 2);

    // 1. Old holder sends a command with stale epoch 1 -> refused with code 3
    let err = table
        .check_command(
            "coordinator",
            Some("seat-a/coord-claude"),
            1,
            Some(RoleCommand::AckAlert),
        )
        .expect_err("old epoch must be refused");
    assert_eq!(err.code, 3);
    assert_eq!(
        err.message,
        "you are no longer coordinator (your epoch 1, current 2), stop"
    );

    // 2. Current holder with current epoch 2 and full scope is allowed both limited and full commands
    assert!(
        table
            .check_command(
                "coordinator",
                Some("seat-a/coord-codex"),
                2,
                Some(RoleCommand::AckAlert),
            )
            .is_ok()
    );
    assert!(
        table
            .check_command(
                "coordinator",
                Some("seat-a/coord-codex"),
                2,
                Some(RoleCommand::ApproveHeldBranch),
            )
            .is_ok()
    );

    // 3. Failover to tier 3 limited guest lease (epoch 3)
    table.role_tick("coordinator", t0 + 365);
    let row = table.get("coordinator").unwrap();
    assert_eq!(row.holder.as_deref(), Some("guest-b/lead"));
    assert_eq!(row.epoch, 3);
    assert_eq!(row.lease_scope, LeaseScope::Limited);

    // Allowed commands for limited coordinator (section 13.3 table)
    for allowed in [
        RoleCommand::AckAlert,
        RoleCommand::RequeueTask,
        RoleCommand::WithdrawOffer,
        RoleCommand::AnswerStuck,
        RoleCommand::RouteConsult,
        RoleCommand::PostNudge,
    ] {
        assert!(
            table
                .check_command("coordinator", Some("guest-b/lead"), 3, Some(allowed))
                .is_ok(),
            "limited lease should allow {allowed:?}"
        );
    }

    // Forbidden (full-only) commands for limited coordinator -> refused with code 3
    for forbidden in [
        RoleCommand::CancelTask,
        RoleCommand::ApproveHeldBranch,
        RoleCommand::Land,
        RoleCommand::PauseMachine,
        RoleCommand::ResumeMachine,
        RoleCommand::AskBrain,
        RoleCommand::PostGithub,
        RoleCommand::RestartHub,
        RoleCommand::PublishPolicy,
        RoleCommand::TouchTokens,
        RoleCommand::VetoRole,
        RoleCommand::GrantRole,
    ] {
        let refusal = table
            .check_command("coordinator", Some("guest-b/lead"), 3, Some(forbidden))
            .expect_err("limited lease must refuse full-only command");
        assert_eq!(refusal.code, 3, "command {forbidden:?} must exit 3");
        assert!(
            refusal
                .message
                .contains("requires a full coordinator lease"),
            "{}",
            refusal.message
        );
    }
}

#[test]
fn role_sqlite_schema_and_fact_replay_round_trip() {
    let conn = Connection::open_in_memory().unwrap();
    schema(&conn).unwrap();

    let mut table = RoleTable::new();
    table.set_policy("coordinator", coordinator_policy());
    table.role_tick("coordinator", 5000);

    let row = table.get("coordinator").unwrap().clone();
    upsert_row(&conn, &row).unwrap();

    let loaded = query_row(&conn, "coordinator", "global").unwrap().unwrap();
    assert_eq!(loaded.holder.as_deref(), Some("seat-a/coord-claude"));
    assert_eq!(loaded.epoch, 1);

    // Simulate failover and apply via FACT message
    table.role_tick("coordinator", 5200);
    let mut fact = table.fact_message("coordinator").unwrap();
    fact.id = 42;
    apply(&conn, &fact).unwrap();

    let all = query_all(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].holder.as_deref(), Some("seat-a/coord-codex"));
    assert_eq!(all[0].epoch, 2);
    assert_eq!(all[0].msg_id, 42);
}

#[test]
fn role_cli_check_fencing_and_veto_end_to_end() {
    let dir = temp_role_dir("cli");
    let store = RoleStore::new(&dir);
    let mut table = RoleTable::new();
    table.set_policy("coordinator", coordinator_policy());
    table.role_tick("coordinator", 10_000);
    store.save(&table).unwrap();

    let run_role = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sommelier"))
            .arg("role")
            .args(args)
            .args(["--data", dir.to_str().unwrap(), "--json"])
            .output()
            .unwrap()
    };

    // 1. Check epoch 1 succeeds (code 0)
    let out = run_role(&[
        "check",
        "coordinator",
        "--who",
        "seat-a/coord-claude",
        "--epoch",
        "1",
        "--cmd",
        "approve_held_branch",
    ]);
    assert_eq!(out.status.code(), Some(0));

    // 2. Tick at 10_200 (> dead_sec 180) -> failover to seat-a/coord-codex at epoch 2
    let out = run_role(&["tick", "coordinator", "--now", "10200"]);
    assert_eq!(out.status.code(), Some(0));

    // 3. Old epoch 1 is refused with code 3
    let out = run_role(&[
        "check",
        "coordinator",
        "--who",
        "seat-a/coord-claude",
        "--epoch",
        "1",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], 3);
    assert!(
        v["reason"]
            .as_str()
            .unwrap()
            .contains("you are no longer coordinator (your epoch 1, current 2), stop")
    );

    // 4. Veto seat-a/coord-codex -> elects guest-b/lead (limited, epoch 3)
    let out = run_role(&["veto", "seat-a/coord-codex", "--now", "10210"]);
    assert_eq!(out.status.code(), Some(0));

    // 5. Limited holder allowed `ack_alert` (code 0), refused `approve_held_branch` (code 3)
    let out_ok = run_role(&[
        "check",
        "coordinator",
        "--who",
        "guest-b/lead",
        "--epoch",
        "3",
        "--cmd",
        "ack_alert",
    ]);
    assert_eq!(out_ok.status.code(), Some(0));

    let out_refused = run_role(&[
        "check",
        "coordinator",
        "--who",
        "guest-b/lead",
        "--epoch",
        "3",
        "--cmd",
        "approve_held_branch",
    ]);
    assert_eq!(out_refused.status.code(), Some(3));
    let v_ref: Value = serde_json::from_slice(&out_refused.stdout).unwrap();
    assert_eq!(v_ref["code"], 3);
    assert!(
        v_ref["reason"]
            .as_str()
            .unwrap()
            .contains("requires a full coordinator lease")
    );

    let _ = fs::remove_dir_all(&dir);
}
