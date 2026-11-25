//! Step 11 scheduler, simulator, invariants, safe mode, CLI, and full role election tests.
//!
//! Covers:
//! - 20 golden scenarios in `tests/fixtures/plan/*.json`
//! - Determinism: byte-equal actions across 100 shuffled input orders
//! - Invariant guard: any violation discards all actions and returns `[Alert(INVARIANT)]` only
//! - Safe mode: invariant violation and 10-minute stall drop enforce to shadow with `SAFE_MODE`
//! - Shadow comparison and `sim` reproducing recorded `DECISION` rows from a fixture log
//! - CLI: `plan --dry`, `plan --explain`, `sim`, `pause`, `resume`
//! - Full role election (13.3): quota runway skip, planned handover with zero vacant gap,
//!   min_hold hysteresis, stickiness, guest opt-in + HELLO roles, and 4th-change flapping freeze

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use sommelier::msg::{Kind, Message, Role};
use sommelier::plan::{
    PauseScope, PlanAction, PlanCapacity, PlanMember, PlanMode, PlanOffer, PlanPolicy, PlanState,
    PlanStateInput, PlanTask, SafeModeState, check_invariants, compare_shadow, evaluate_safe_mode,
    guard_actions, next_decision_tick_id, output_to_messages, plan, plan_explain, query_decisions,
    query_pauses, simulate_log_file,
};
use sommelier::quota::meter::{Meter, MeterPolicy, QuotaSample, Source, StateEvent};
use sommelier::role::eligible::{
    EligibilityContext, FullRolePolicy, HandoverRequest, build_context_from_store, role_tick_full,
};
use sommelier::role::{LeaseScope, RoleAction, RoleCandidate, RolePolicy, RoleState, RoleTable};
use sommelier::store::Store;

#[derive(Deserialize)]
struct GoldenScenario {
    name: String,
    now: u64,
    #[serde(default)]
    policy: PlanPolicy,
    state: PlanStateInput,
    expected_actions: Vec<PlanAction>,
    #[serde(default)]
    expected_wait_reasons: BTreeMap<String, String>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("plan")
}

fn load_golden_scenarios() -> Vec<GoldenScenario> {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read plan fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            serde_json::from_str::<GoldenScenario>(&text)
                .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", p.display()))
        })
        .collect()
}

#[test]
fn twenty_golden_scenarios_match_expected_actions_and_wait_reasons() {
    let scenarios = load_golden_scenarios();
    assert_eq!(
        scenarios.len(),
        20,
        "expected exactly 20 golden scenarios in tests/fixtures/plan"
    );

    for sc in &scenarios {
        let state = PlanState::from_input(sc.state.clone());
        let out = plan(&state, &sc.policy, sc.now);
        assert!(
            out.invariant_violations.is_empty(),
            "scenario {} had invariant violations: {:?}",
            sc.name,
            out.invariant_violations
        );
        assert_eq!(
            out.actions, sc.expected_actions,
            "scenario {} actions mismatch",
            sc.name
        );
        assert_eq!(
            out.wait_reasons, sc.expected_wait_reasons,
            "scenario {} wait_reasons mismatch",
            sc.name
        );
    }
}

#[test]
fn drain_under_five_minutes_admits_nothing() {
    let now = 1_795_600_000;
    let policy = PlanPolicy {
        backlog_min_ready: 0,
        ..PlanPolicy::default()
    };
    let mut meter = Meter::new("hub-owner", "codex", "codex.5h", "5h");
    meter.policy = MeterPolicy {
        vendor: "codex".into(),
        window: "5h".into(),
        stop_at: 90.0,
        reserve_pct: 10.0,
        pace: false,
        drain_min: 30,
        drain_band_pct: 5.0,
        fresh_min: 5,
        burn_window_min: 30,
        window_duration_sec: 18000,
    };
    meter.pct_per_unit = 0.0000001;
    meter.resets_at = now + 14_400;
    // Burn = 0.5 %/min; used = 88.5%, line = 90.0% => 3 minutes left (< 5 min).
    let s1 = QuotaSample {
        owner: "hub-owner".into(),
        vendor: "codex".into(),
        pool: "codex.5h".into(),
        window: "5h".into(),
        observed_at: now - 600,
        used_pct: 83.5,
        resets_at: now + 14_400,
        source: Source::Api,
        confidence: 0.9,
    };
    let s2 = QuotaSample {
        observed_at: now - 300,
        used_pct: 86.0,
        ..s1.clone()
    };
    let s3 = QuotaSample {
        observed_at: now,
        used_pct: 88.5,
        ..s1.clone()
    };
    meter.series = vec![s1, s2, s3.clone()];
    meter.latest = Some(s3);

    let input = PlanStateInput {
        members: vec![PlanMember {
            name: "hub-owner".into(),
            trust: "owner".into(),
            online: true,
            tier: None,
        }],
        capacities: vec![PlanCapacity {
            owner: "hub-owner".into(),
            machine: "box1".into(),
            class: "codex".into(),
            mode: "auto".into(),
            useful_parallel: 1,
            accepts: ["review".to_owned()].into_iter().collect(),
            pools: vec!["codex.5h".into()],
            family: None,
            free_since: None,
        }],
        tasks: vec![PlanTask {
            id: "t-tiny".into(),
            title: "Tiny check".into(),
            kind: "review".into(),
            priority: "high".into(),
            state: "ready".into(),
            track: String::new(),
            scope: "src/a/*".into(),
            accepts: vec!["codex".into()],
            min_tier: String::new(),
            branch: String::new(),
            done_when: "ok".into(),
            deps: vec![],
            size: 0.02, // 1.5 min p90 * 1.2 = 1.8 min < 3 min t_line, so reaches drain <5m check
            checkpointable: true,
            attempt: 1,
            max_attempts: 3,
            resume_note: String::new(),
            exclude_pools_until: BTreeMap::new(),
            created_at: now,
            ready_at: now,
            closed_at: 0,
            risk: "normal".into(),
            min_trust: 2,
        }],
        meters: vec![meter],
        ..PlanStateInput::default()
    };

    let out = plan(&PlanState::from_input(input), &policy, now);
    assert!(out.actions.is_empty());
    assert_eq!(
        out.wait_reasons.get("t-tiny").map(String::as_str),
        Some("drain, nothing more on codex.5h")
    );
}

/// Simple deterministic Fisher-Yates shuffle using xorshift64 so the test needs no extra crates.
fn shuffle<T>(slice: &mut [T], seed: &mut u64) {
    let mut next_u64 = || {
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        x
    };
    for i in (1..slice.len()).rev() {
        let j = (next_u64() as usize) % (i + 1);
        slice.swap(i, j);
    }
}

#[test]
fn determinism_byte_equal_across_100_shuffled_input_orders() {
    let now = 1_795_605_000;
    let policy = PlanPolicy {
        mode: PlanMode::Enforce,
        backlog_min_ready: 3,
        ..PlanPolicy::default()
    };

    let base_input = PlanStateInput {
        hub_started_at: now - 1000,
        members: vec![
            PlanMember {
                name: "alpha".into(),
                trust: "owner".into(),
                online: true,
                tier: None,
            },
            PlanMember {
                name: "beta".into(),
                trust: "owner".into(),
                online: true,
                tier: None,
            },
            PlanMember {
                name: "guest-c".into(),
                trust: "guest".into(),
                online: true,
                tier: None,
            },
        ],
        capacities: vec![
            PlanCapacity {
                owner: "alpha".into(),
                machine: "m1".into(),
                class: "codex".into(),
                mode: "auto".into(),
                useful_parallel: 2,
                accepts: ["fix".into(), "review".into()].into_iter().collect(),
                pools: vec!["codex.5h".into(), "codex.week".into()],
                family: None,
                free_since: Some(now - 900),
            },
            PlanCapacity {
                owner: "beta".into(),
                machine: "m2".into(),
                class: "gemini_build".into(),
                mode: "auto".into(),
                useful_parallel: 2,
                accepts: ["fix".into(), "review".into()].into_iter().collect(),
                pools: vec!["agy.window".into()],
                family: Some("agy.family".into()),
                free_since: Some(now - 900),
            },
            PlanCapacity {
                owner: "guest-c".into(),
                machine: "m3".into(),
                class: "codex".into(),
                mode: "own".into(),
                useful_parallel: 1,
                accepts: ["fix".into(), "review".into()].into_iter().collect(),
                pools: vec!["codex.5h".into()],
                family: None,
                free_since: Some(now - 900),
            },
        ],
        tasks: (1..=8)
            .map(|i| PlanTask {
                id: format!("t{i:02}"),
                title: format!("Task {i}"),
                kind: if i % 3 == 0 { "review" } else { "fix" }.into(),
                priority: match i % 4 {
                    0 => "blocker",
                    1 => "high",
                    2 => "normal",
                    _ => "filler",
                }
                .into(),
                state: if i == 8 { "offered" } else { "ready" }.into(),
                track: if i % 2 == 0 { "game" } else { "shaders" }.into(),
                scope: if i == 4 {
                    "src/t02/sub/*".into()
                } else {
                    format!("src/t{i:02}/*")
                },
                accepts: vec!["codex".into(), "gemini_build".into()],
                min_tier: String::new(),
                branch: format!("codex/t{i:02}"),
                done_when: "tests pass".into(),
                deps: vec![],
                size: 0.5,
                checkpointable: true,
                attempt: if i == 5 { 2 } else { 1 },
                max_attempts: 3,
                resume_note: String::new(),
                exclude_pools_until: BTreeMap::new(),
                created_at: now - (i * 600),
                ready_at: now - (i * 600),
                closed_at: 0,
                risk: "normal".into(),
                min_trust: 2,
            })
            .collect(),
        offers: vec![PlanOffer {
            id: "offer-t08-old".into(),
            task_id: "t08".into(),
            to_owner: "guest-c".into(),
            class: "codex".into(),
            sent_at: now - 400,
            expires_at: now - 10,
            state: "open".into(),
        }],
        ..PlanStateInput::default()
    };

    let baseline = plan(&PlanState::from_input(base_input.clone()), &policy, now);
    let baseline_bytes = serde_json::to_vec(&baseline.actions).unwrap();
    let baseline_waits = serde_json::to_vec(&baseline.wait_reasons).unwrap();
    let baseline_exp = serde_json::to_vec(&baseline.explanations).unwrap();

    for run in 1..=100u64 {
        let mut seed = run.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A_F0F0_0F0F;
        let mut shuffled = base_input.clone();
        shuffle(&mut shuffled.members, &mut seed);
        shuffle(&mut shuffled.capacities, &mut seed);
        shuffle(&mut shuffled.tasks, &mut seed);
        shuffle(&mut shuffled.leases, &mut seed);
        shuffle(&mut shuffled.offers, &mut seed);
        for t in &mut shuffled.tasks {
            shuffle(&mut t.accepts, &mut seed);
            shuffle(&mut t.deps, &mut seed);
        }
        for c in &mut shuffled.capacities {
            shuffle(&mut c.pools, &mut seed);
        }

        let out = plan(&PlanState::from_input(shuffled), &policy, now);
        assert_eq!(
            serde_json::to_vec(&out.actions).unwrap(),
            baseline_bytes,
            "actions differed on shuffle {run}"
        );
        assert_eq!(
            serde_json::to_vec(&out.wait_reasons).unwrap(),
            baseline_waits,
            "wait_reasons differed on shuffle {run}"
        );
        assert_eq!(
            serde_json::to_vec(&out.explanations).unwrap(),
            baseline_exp,
            "explanations differed on shuffle {run}"
        );
    }
}

#[test]
fn invariant_violation_discards_all_actions_and_returns_only_alert() {
    let now = 1_795_600_000;
    let policy = PlanPolicy::default();
    let state = PlanState::from_input(PlanStateInput {
        members: vec![PlanMember {
            name: "hub-owner".into(),
            trust: "owner".into(),
            online: true,
            tier: None,
        }],
        capacities: vec![PlanCapacity {
            owner: "hub-owner".into(),
            machine: "box1".into(),
            class: "codex".into(),
            mode: "auto".into(),
            useful_parallel: 2,
            accepts: ["fix".into()].into_iter().collect(),
            pools: vec!["codex.5h".into()],
            family: None,
            free_since: None,
        }],
        tasks: vec![
            PlanTask {
                id: "t1".into(),
                title: "Task 1".into(),
                kind: "fix".into(),
                priority: "high".into(),
                state: "ready".into(),
                track: String::new(),
                scope: "src/shared/*".into(),
                accepts: vec!["codex".into()],
                min_tier: String::new(),
                branch: String::new(),
                done_when: "ok".into(),
                deps: vec![],
                size: 1.0,
                checkpointable: true,
                attempt: 1,
                max_attempts: 3,
                resume_note: String::new(),
                exclude_pools_until: BTreeMap::new(),
                created_at: now,
                ready_at: now,
                closed_at: 0,
                risk: "normal".into(),
                min_trust: 2,
            },
            PlanTask {
                id: "t2".into(),
                title: "Task 2 overlapping".into(),
                kind: "fix".into(),
                priority: "high".into(),
                state: "ready".into(),
                track: String::new(),
                scope: "src/shared/a.rs".into(),
                accepts: vec!["codex".into()],
                min_tier: String::new(),
                branch: String::new(),
                done_when: "ok".into(),
                deps: vec![],
                size: 1.0,
                checkpointable: true,
                attempt: 1,
                max_attempts: 3,
                resume_note: String::new(),
                exclude_pools_until: BTreeMap::new(),
                created_at: now,
                ready_at: now,
                closed_at: 0,
                risk: "normal".into(),
                min_trust: 2,
            },
        ],
        ..PlanStateInput::default()
    });

    // Inject a bad plan that leases two overlapping tasks at once (plus a valid alert).
    let bad_actions = vec![
        PlanAction::Lease {
            task_id: "t1".into(),
            attempt: 1,
            owner: "hub-owner".into(),
            machine: "box1".into(),
            class: "codex".into(),
            slot_key: "hub-owner/box1/codex/1".into(),
            pools: vec!["codex.5h".into()],
            est_minutes_p90: 75.0,
            est_cost: 1500000.0,
            est_end_at: now + 4500,
            expires_at: now + 2700,
        },
        PlanAction::Lease {
            task_id: "t2".into(),
            attempt: 1,
            owner: "hub-owner".into(),
            machine: "box1".into(),
            class: "codex".into(),
            slot_key: "hub-owner/box1/codex/2".into(),
            pools: vec!["codex.5h".into()],
            est_minutes_p90: 75.0,
            est_cost: 1500000.0,
            est_end_at: now + 4500,
            expires_at: now + 2700,
        },
        PlanAction::Alert {
            code: "BACKLOG_LOW".into(),
            key: "codex".into(),
            severity: "info".into(),
            to: "brain".into(),
            text: "low".into(),
            data: json!({}),
        },
    ];

    assert!(check_invariants(&bad_actions, &state, &policy, now).is_err());
    let (guarded, violations) = guard_actions(bad_actions, &state, &policy, now);
    assert!(violations.is_some());
    assert_eq!(guarded.len(), 1, "only Alert(INVARIANT) may remain");
    match &guarded[0] {
        PlanAction::Alert { code, to, .. } => {
            assert_eq!(code, "INVARIANT");
            assert_eq!(to, "coordinator");
        }
        other => panic!("expected Alert(INVARIANT), got {other:?}"),
    }
}

#[test]
fn safe_mode_triggers_on_invariant_or_ten_minute_stall() {
    let policy = PlanPolicy::default();
    let t0 = 1_795_600_000;

    // 1. Invariant violation immediately drops enforce to safe mode.
    let (sm1, al1) = evaluate_safe_mode(&SafeModeState::default(), &policy, true, 1, 1, 0, t0);
    assert!(sm1.active);
    assert!(matches!(
        al1,
        Some(PlanAction::Alert { ref code, .. }) if code == "SAFE_MODE"
    ));

    // 2. Stall with ready tasks + free admissible slots + 0 leases for 10 min (600 s).
    let (sm_start, al_none) =
        evaluate_safe_mode(&SafeModeState::default(), &policy, false, 2, 1, 0, t0);
    assert!(!sm_start.active);
    assert!(al_none.is_none());
    assert_eq!(sm_start.stall_since, Some(t0));

    let (sm_mid, al_mid) = evaluate_safe_mode(&sm_start, &policy, false, 2, 1, 0, t0 + 599);
    assert!(!sm_mid.active);
    assert!(al_mid.is_none());

    let (sm_trip, al_trip) = evaluate_safe_mode(&sm_mid, &policy, false, 2, 1, 0, t0 + 600);
    assert!(sm_trip.active);
    assert!(matches!(
        al_trip,
        Some(PlanAction::Alert { ref code, .. }) if code == "SAFE_MODE"
    ));
}

#[test]
fn shadow_comparison_and_sim_reproduce_recorded_decision_rows() {
    let log_path = fixtures_dir().join("sim_log.txt");
    let pol_path = fixtures_dir().join("sim_policy.toml");
    let report = simulate_log_file(&log_path, Some(&pol_path)).unwrap();

    assert_eq!(report.messages_replayed, 6);
    assert_eq!(report.recorded_decisions, 1);
    assert_eq!(
        report.exact_decision_matches, 1,
        "sim must reproduce recorded DECISION rows exactly; diff: {:?}",
        report.diffs
    );
    assert_eq!(report.total_leases_changed, 0);
    assert_eq!(report.shadow_comparison.matched, 2);
    assert_eq!(report.shadow_comparison.agreement_ratio, 1.0);
    assert!(report.shadow_comparison.mismatches.is_empty());

    // Direct shadow comparison with one agreement and one mismatch.
    let planned = vec![
        PlanAction::Lease {
            task_id: "t1".into(),
            attempt: 1,
            owner: "owner1".into(),
            machine: "m1".into(),
            class: "gemini_build".into(),
            slot_key: "owner1/m1/gemini_build/1".into(),
            pools: vec![],
            est_minutes_p90: 30.0,
            est_cost: 50.0,
            est_end_at: 1000,
            expires_at: 2000,
        },
        PlanAction::Lease {
            task_id: "t2".into(),
            attempt: 1,
            owner: "owner1".into(),
            machine: "m1".into(),
            class: "codex".into(),
            slot_key: "owner1/m1/codex/1".into(),
            pools: vec![],
            est_minutes_p90: 30.0,
            est_cost: 50.0,
            est_end_at: 1000,
            expires_at: 2000,
        },
    ];
    let actual = vec![
        ("t1".into(), "owner1".into(), "gemini_build".into()),
        ("t2".into(), "owner2".into(), "codex".into()),
    ];
    let cmp = compare_shadow(&planned, &actual);
    assert_eq!(cmp.matched, 1);
    assert_eq!(cmp.agreement_ratio, 0.5);
    assert_eq!(cmp.mismatches.len(), 1);
    assert_eq!(cmp.mismatches[0].task_id, "t2");
}

#[test]
fn cli_plan_dry_explain_sim_pause_resume_and_decision_store_projection() {
    let dir = tempfile::tempdir().unwrap();
    let log_src = fs::read_to_string(fixtures_dir().join("sim_log.txt")).unwrap();
    // Seed only the first 3 messages (HELLO + two ready tasks) so plan --dry places both.
    let seed_lines: Vec<&str> = log_src.lines().take(3).collect();
    fs::write(dir.path().join("bus.jsonl"), seed_lines.join("\n") + "\n").unwrap();

    let pol_path = fixtures_dir().join("sim_policy.toml");
    let bin = env!("CARGO_BIN_EXE_sommelier");

    // 1. plan --dry --json
    let out = Command::new(bin)
        .args([
            "plan",
            "--dry",
            "--data",
            dir.path().to_str().unwrap(),
            "--policy",
            pol_path.to_str().unwrap(),
            "--now",
            "1795600920",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["actions"].as_array().unwrap().len(), 2);

    // 2. plan --explain t1 --json
    let out = Command::new(bin)
        .args([
            "plan",
            "--explain",
            "t1",
            "--data",
            dir.path().to_str().unwrap(),
            "--policy",
            pol_path.to_str().unwrap(),
            "--now",
            "1795600920",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["explanation"]["task_id"], "t1");
    assert_eq!(
        v["explanation"]["assigned_slot"],
        "hub-owner/box1/gemini_build/1"
    );

    // 3. pause class gemini_build -> plan --dry now routes t1 to codex!
    let out = Command::new(bin)
        .args([
            "pause",
            "class",
            "gemini_build",
            "--data",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["scope"], "class/gemini_build");
    assert_eq!(v["paused"], true);

    let out = Command::new(bin)
        .args([
            "plan",
            "--explain",
            "t1",
            "--data",
            dir.path().to_str().unwrap(),
            "--policy",
            pol_path.to_str().unwrap(),
            "--now",
            "1795600920",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["explanation"]["assigned_slot"], "hub-owner/box1/codex/1");

    // 4. resume class gemini_build -> t1 returns to gemini_build!
    let out = Command::new(bin)
        .args([
            "resume",
            "class",
            "gemini_build",
            "--data",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["paused"], false);

    // 5. sim CLI on fixture log
    let out = Command::new(bin)
        .args([
            "sim",
            fixtures_dir().join("sim_log.txt").to_str().unwrap(),
            "--policy",
            pol_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["report"]["exact_decision_matches"], 1);
    assert_eq!(v["report"]["recorded_decisions"], 1);

    // 6. Store DECISION projection and replay consistency
    let mut store = Store::open(dir.path()).unwrap();
    assert!(query_pauses(store.connection()).unwrap().is_empty());
    let policy = PlanPolicy::parse_toml(&fs::read_to_string(&pol_path).unwrap()).unwrap();
    let st = sommelier::plan::load_plan_state_from_store(store.connection(), &policy, 0).unwrap();
    let pout = plan(&st, &policy, 1795600920);
    let mut msgs = output_to_messages(&pout, &st, &policy, 7, "statehash", 1795600920);
    let base_id = store.cursor().unwrap() + 1;
    for (i, m) in msgs.iter_mut().enumerate() {
        m.id = base_id + i as u64;
    }
    store.apply_batch(&msgs).unwrap();
    let decisions = query_decisions(store.connection()).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].tick_id, 7);
    assert!(decisions[0].applied);
    assert_eq!(decisions[0].actions.len(), 2);

    // Ensure plan_explain returns None for unknown task.
    assert!(plan_explain(&st, &policy, "nonexistent", 1795600920).is_none());
    assert_eq!(
        PauseScope::parse("pool codex.5h").unwrap(),
        PauseScope::Pool("codex.5h".into())
    );
}

fn make_meter_with_runway(
    owner: &str,
    vendor: &str,
    pool: &str,
    used_pct: f64,
    burn_per_min: f64,
    now_sec: u64,
    state_event: Option<StateEvent>,
) -> Meter {
    let mut m = Meter::new(owner, vendor, pool, "5h");
    m.policy = MeterPolicy {
        vendor: vendor.into(),
        window: "5h".into(),
        stop_at: 90.0,
        reserve_pct: 10.0,
        pace: false,
        drain_min: 30,
        drain_band_pct: 5.0,
        fresh_min: 10,
        burn_window_min: 30,
        window_duration_sec: 18000,
    };
    m.resets_at = now_sec + 36_000;
    let s1 = QuotaSample {
        owner: owner.into(),
        vendor: vendor.into(),
        pool: pool.into(),
        window: "5h".into(),
        observed_at: now_sec - 600,
        used_pct: (used_pct - burn_per_min * 10.0).max(0.0),
        resets_at: now_sec + 36_000,
        source: Source::Api,
        confidence: 0.9,
    };
    let s2 = QuotaSample {
        observed_at: now_sec - 300,
        used_pct: (used_pct - burn_per_min * 5.0).max(0.0),
        ..s1.clone()
    };
    let s3 = QuotaSample {
        observed_at: now_sec,
        used_pct,
        ..s1.clone()
    };
    m.series = vec![s1, s2, s3.clone()];
    m.latest = Some(s3);
    m.state_event = state_event;
    m
}

fn full_coordinator_policy() -> FullRolePolicy {
    FullRolePolicy {
        base: RolePolicy {
            heartbeat_sec: 60,
            dead_sec: 180,
            min_hold_min: 15,
            cooldown_min: 30,
            max_changes_per_hour: 3,
            require: vec!["harness_repo".into()],
            candidates: vec![
                RoleCandidate {
                    who: "owner/coord-claude".into(),
                    tier: 1,
                    class: "claude_strong".into(),
                    scope: LeaseScope::Full,
                    spawn: true,
                    opt_in: false,
                },
                RoleCandidate {
                    who: "owner/coord-codex".into(),
                    tier: 2,
                    class: "codex".into(),
                    scope: LeaseScope::Full,
                    spawn: true,
                    opt_in: false,
                },
                RoleCandidate {
                    who: "owner/coord-gemini".into(),
                    tier: 2,
                    class: "gemini_build".into(),
                    scope: LeaseScope::Full,
                    spawn: true,
                    opt_in: false,
                },
                RoleCandidate {
                    who: "guest-x/lead".into(),
                    tier: 3,
                    class: "codex".into(),
                    scope: LeaseScope::Limited,
                    spawn: false,
                    opt_in: true,
                },
            ],
        },
        min_runway_min: 120,
        handover_runway_min: 45,
        budget_alarm_pct: 70.0,
    }
}

fn base_role_ctx(now_sec: u64) -> EligibilityContext {
    let mut ctx = EligibilityContext::default();
    ctx.valid_tokens.insert("owner".into());
    ctx.valid_tokens.insert("guest-x".into());
    ctx.online_members.insert("owner/coord-claude".into());
    ctx.online_members.insert("owner/coord-codex".into());
    ctx.online_members.insert("owner/coord-gemini".into());
    ctx.online_members.insert("guest-x/lead".into());
    ctx.supervisor_heartbeats.insert("owner".into(), now_sec);
    ctx.capabilities.insert(
        "owner".into(),
        ["harness_repo".to_owned()].into_iter().collect(),
    );
    ctx.capabilities.insert(
        "guest-x".into(),
        ["harness_repo".to_owned()].into_iter().collect(),
    );
    ctx.hello_roles.insert(
        "guest-x".into(),
        ["coordinator".to_owned()].into_iter().collect(),
    );
    ctx.class_pools
        .insert("claude_strong".into(), vec!["claude.5h".into()]);
    ctx.class_pools
        .insert("codex".into(), vec!["codex.5h".into()]);
    ctx.class_pools
        .insert("gemini_build".into(), vec!["agy.window".into()]);
    ctx
}

#[test]
fn role_election_skips_candidate_short_of_runway_and_picks_longest_in_tier() {
    let now = 1_795_600_000;
    let pol = full_coordinator_policy();
    let mut table = RoleTable::new();
    table.set_policy("coordinator", pol.base.clone());

    let mut ctx = base_role_ctx(now);
    // Tier 1 claude: used 50%, burn 0.5%/min => 80 min to line 90%, / 1.2 = 66.7 min < 120 min -> SKIPPED!
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 50.0, 0.5, now, None),
    );
    // Tier 2 codex: used 40%, burn 0.2%/min => 250 min to line, / 1.2 = 208 min >= 120 min (runway 225)
    ctx.meters.insert(
        ("owner".into(), "codex.5h".into()),
        make_meter_with_runway("owner", "codex", "codex.5h", 40.0, 0.2, now, None),
    );
    // Tier 2 gemini: used 20%, burn 0.2%/min => 350 min to line, / 1.2 = 291 min >= 120 min (runway 315 > 225)
    ctx.meters.insert(
        ("owner".into(), "agy.window".into()),
        make_meter_with_runway("owner", "gemini", "agy.window", 20.0, 0.2, now, None),
    );

    let out = role_tick_full(&mut table, "coordinator", &pol, &ctx, now);
    assert_eq!(
        out.action,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "owner/coord-gemini".into(),
            lease_scope: LeaseScope::Full,
            epoch: 1,
            reason: "vacant".into(),
        }
    );
    let claude_eval = out
        .evaluated_candidates
        .iter()
        .find(|c| c.candidate.who == "owner/coord-claude")
        .unwrap();
    assert!(!claude_eval.eligible);
    assert!(
        claude_eval
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("min_runway_min")
    );
}

#[test]
fn role_holder_in_drain_hands_over_with_no_vacant_gap_and_respects_hysteresis() {
    let t0 = 1_795_600_000;
    let pol = full_coordinator_policy();
    let mut table = RoleTable::new();
    table.set_policy("coordinator", pol.base.clone());

    let mut ctx = base_role_ctx(t0);
    // Both tier 1 claude and tier 2 codex healthy initially.
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 20.0, 0.1, t0, None),
    );
    ctx.meters.insert(
        ("owner".into(), "codex.5h".into()),
        make_meter_with_runway("owner", "codex", "codex.5h", 20.0, 0.1, t0, None),
    );
    ctx.meters.insert(
        ("owner".into(), "agy.window".into()),
        make_meter_with_runway("owner", "gemini", "agy.window", 25.0, 0.1, t0, None),
    );

    // Grant epoch 1 to tier 1 claude.
    let out1 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t0);
    assert_eq!(
        out1.action,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "owner/coord-claude".into(),
            lease_scope: LeaseScope::Full,
            epoch: 1,
            reason: "vacant".into(),
        }
    );

    // At t0 + 100 (< min_hold_until = t0 + 900): claude enters drain (used 86%, line 90%, band 5%),
    // still not out of quota -> min_hold_until hysteresis keeps holder for now.
    let t_early = t0 + 100;
    table.heartbeat("coordinator", "owner/coord-claude", t_early);
    ctx.supervisor_heartbeats.insert("owner".into(), t_early);
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 86.0, 0.1, t_early, None),
    );
    ctx.meters.insert(
        ("owner".into(), "codex.5h".into()),
        make_meter_with_runway("owner", "codex", "codex.5h", 20.0, 0.1, t_early, None),
    );
    ctx.meters.insert(
        ("owner".into(), "agy.window".into()),
        make_meter_with_runway("owner", "gemini", "agy.window", 25.0, 0.1, t_early, None),
    );
    let out_hold = role_tick_full(&mut table, "coordinator", &pol, &ctx, t_early);
    assert_eq!(
        out_hold.action,
        RoleAction::Renewed {
            role: "coordinator".into(),
            holder: "owner/coord-claude".into(),
            epoch: 1,
        }
    );
    assert!(
        out_hold.alerts.iter().any(|a| a.code == "COORD_BUDGET"),
        "expected COORD_BUDGET alarm above 70%"
    );

    // At t0 + 960 (>= min_hold_until): claude is still alive and in drain -> planned handover
    // directly to owner/coord-codex at epoch 2 with ZERO vacant gap!
    let t_handover = t0 + 960;
    table.heartbeat("coordinator", "owner/coord-claude", t_handover - 30);
    ctx.supervisor_heartbeats.insert("owner".into(), t_handover);
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 86.0, 0.1, t_handover, None),
    );
    ctx.meters.insert(
        ("owner".into(), "codex.5h".into()),
        make_meter_with_runway("owner", "codex", "codex.5h", 20.0, 0.1, t_handover, None),
    );
    ctx.meters.insert(
        ("owner".into(), "agy.window".into()),
        make_meter_with_runway("owner", "gemini", "agy.window", 25.0, 0.1, t_handover, None),
    );
    let out2 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t_handover);
    assert!(out2.planned_handover);
    assert_eq!(out2.vacant_gap_secs, 0);
    assert!(matches!(
        out2.action,
        RoleAction::Granted {
            ref holder,
            epoch: 2,
            ..
        } if holder == "owner/coord-codex"
    ));
    let row = table.get("coordinator").unwrap();
    assert_eq!(row.state, RoleState::Held);
    assert_eq!(row.holder.as_deref(), Some("owner/coord-codex"));
    assert_eq!(row.epoch, 2);

    // Stickiness: even when tier 1 claude recovers full quota, healthy codex keeps the role!
    let t_sticky = t_handover + 60;
    table.heartbeat("coordinator", "owner/coord-codex", t_sticky);
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 10.0, 0.1, t_sticky, None),
    );
    let out_sticky = role_tick_full(&mut table, "coordinator", &pol, &ctx, t_sticky);
    assert_eq!(
        out_sticky.action,
        RoleAction::Renewed {
            role: "coordinator".into(),
            holder: "owner/coord-codex".into(),
            epoch: 2,
        }
    );

    // Context-full planned handover allows a fresh session of the same seat even before min_hold_until.
    let mut ctx_full = ctx.clone();
    ctx_full.handover_requests.insert(
        "coordinator".into(),
        HandoverRequest {
            holder: "owner/coord-codex".into(),
            reason: "context".into(),
            note: "context nearly full".into(),
        },
    );
    let out_ctx = role_tick_full(&mut table, "coordinator", &pol, &ctx_full, t_sticky + 10);
    assert!(out_ctx.planned_handover);
    assert!(matches!(
        out_ctx.action,
        RoleAction::Granted { epoch: 3, .. }
    ));
}

#[test]
fn role_fourth_change_in_one_hour_freezes_role_with_flapping_alert() {
    let t0 = 1_795_600_000;
    let pol = full_coordinator_policy();
    let mut table = RoleTable::new();
    table.set_policy("coordinator", pol.base.clone());

    let mut ctx = base_role_ctx(t0);
    ctx.meters.insert(
        ("owner".into(), "claude.5h".into()),
        make_meter_with_runway("owner", "claude", "claude.5h", 10.0, 0.05, t0, None),
    );
    ctx.meters.insert(
        ("owner".into(), "codex.5h".into()),
        make_meter_with_runway("owner", "codex", "codex.5h", 10.0, 0.05, t0, None),
    );
    ctx.meters.insert(
        ("owner".into(), "agy.window".into()),
        make_meter_with_runway("owner", "gemini", "agy.window", 10.0, 0.05, t0, None),
    );
    ctx.meters.insert(
        ("guest-x".into(), "codex.5h".into()),
        make_meter_with_runway("guest-x", "codex", "codex.5h", 10.0, 0.05, t0, None),
    );

    // Change 1 (epoch 1): claude
    let a1 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t0);
    assert!(matches!(a1.action, RoleAction::Granted { epoch: 1, .. }));

    // Change 2 (epoch 2): claude dies past dead_sec (t0 + 200) -> codex
    let t1 = t0 + 200;
    ctx.supervisor_heartbeats.insert("owner".into(), t1);
    let a2 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t1);
    assert!(matches!(a2.action, RoleAction::Granted { epoch: 2, .. }));

    // Change 3 (epoch 3): codex dies past dead_sec (t1 + 200) -> gemini
    let t2 = t1 + 200;
    ctx.supervisor_heartbeats.insert("owner".into(), t2);
    let a3 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t2);
    assert!(matches!(a3.action, RoleAction::Granted { epoch: 3, .. }));

    // 4th change within one hour (t2 + 200): gemini dies -> trying to grant guest-x hits
    // changes_last_hour = 4 > max_changes_per_hour (3) -> FREEZES role and raises ROLE_FLAPPING!
    let t3 = t2 + 200;
    ctx.supervisor_heartbeats.insert("owner".into(), t3);
    let a4 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t3);
    assert_eq!(
        a4.action,
        RoleAction::Frozen {
            role: "coordinator".into(),
            epoch: 3,
            changes_last_hour: 4,
        }
    );
    assert!(table.get("coordinator").unwrap().frozen);
    assert!(
        a4.alerts
            .iter()
            .any(|a| a.code == "ROLE_FLAPPING" && a.to == "owner")
    );

    // Subsequent ticks stay frozen until the owner intervenes.
    let a5 = role_tick_full(&mut table, "coordinator", &pol, &ctx, t3 + 60);
    assert!(matches!(a5.action, RoleAction::Frozen { .. }));
}

#[test]
fn guest_coordinator_requires_both_policy_opt_in_and_hello_roles() {
    let now = 1_795_600_000;
    let mut pol = full_coordinator_policy();
    // Keep only the guest candidate to test guest eligibility directly.
    pol.base
        .candidates
        .retain(|c| c.scope == LeaseScope::Limited);
    let mut table = RoleTable::new();
    table.set_policy("coordinator", pol.base.clone());

    let mut ctx = base_role_ctx(now);
    ctx.meters.insert(
        ("guest-x".into(), "codex.5h".into()),
        make_meter_with_runway("guest-x", "codex", "codex.5h", 10.0, 0.05, now, None),
    );
    // Guest did not declare roles=coordinator in HELLO -> ineligible -> ROLE_VACANT!
    ctx.hello_roles.insert("guest-x".into(), BTreeSet::new());
    let out_vacant = role_tick_full(&mut table, "coordinator", &pol, &ctx, now);
    assert!(matches!(out_vacant.action, RoleAction::Vacant { .. }));
    assert!(out_vacant.alerts.iter().any(|a| a.code == "ROLE_VACANT"));

    // Once guest declares roles=coordinator in HELLO, it receives a limited lease.
    ctx.hello_roles.insert(
        "guest-x".into(),
        ["coordinator".to_owned()].into_iter().collect(),
    );
    let out_granted = role_tick_full(&mut table, "coordinator", &pol, &ctx, now + 10);
    assert_eq!(
        out_granted.action,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "guest-x/lead".into(),
            lease_scope: LeaseScope::Limited,
            epoch: 1,
            reason: "vacant".into(),
        }
    );

    // Ensure store projection of ROLE works through ModuleRegistry.
    let mut store = Store::open_in_memory().unwrap();
    let mut fact = out_granted.fact_message.unwrap();
    fact.id = 1;
    fact.ts = "2026-11-25T10:00:00.000Z".into();
    store.apply(&fact).unwrap();
    let api = store.api_state(now + 10).unwrap();
    assert_eq!(api["roles"][0]["holder"], "guest-x/lead");
    assert_eq!(api["roles"][0]["lease_scope"], "limited");
}

#[test]
fn role_store_module_projects_decisions_and_roles_via_store_replay() {
    let mut store = Store::open_in_memory().unwrap();
    let msgs = vec![
        Message {
            id: 1,
            ts: "2026-11-25T10:00:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: "pause/class/codex".into(),
            body: json!({"v": 1, "scope": "class/codex", "paused": true, "by": "owner", "at": 1795600000}).to_string(),
            refs: vec!["pause:on".into()],
            to: vec![],
        },
        Message {
            id: 2,
            ts: "2026-11-25T10:00:10.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: "decision/1".into(),
            body: json!({
                "v": 1,
                "tick_id": 1,
                "at": 1795600010,
                "policy_hash": "abc",
                "state_hash": "def",
                "mode": "shadow",
                "actions": [],
                "applied": false
            })
            .to_string(),
            refs: vec!["mode:shadow".into()],
            to: vec![],
        },
    ];
    store.apply_batch(&msgs).unwrap();
    let api = store.api_state(1795600010).unwrap();
    assert_eq!(api["pauses"][0]["scope"], "class/codex");
    assert_eq!(api["decisions"][0]["tick_id"], 1);
    assert_eq!(api["decisions"][0]["mode"], "shadow");
    assert_eq!(api["scheduler"]["decision_count"], 1);
}

#[test]
fn build_context_from_store_loads_hello_capabilities_and_roles() {
    let mut store = Store::open_in_memory().unwrap();
    let msgs = vec![
        Message {
            id: 1,
            ts: "2026-11-25T10:00:00.000Z".into(),
            from: "guest-x".into(),
            role: Role::Member,
            kind: Kind::Hello,
            topic: String::new(),
            body: "capabilities=harness_repo,gpu roles=coordinator".into(),
            refs: vec![],
            to: vec![],
        },
        Message {
            id: 2,
            ts: "2026-11-25T10:00:01.000Z".into(),
            from: "guest-y".into(),
            role: Role::Member,
            kind: Kind::Hello,
            topic: String::new(),
            body: "os=windows gpu=rtx4090".into(),
            refs: vec![],
            to: vec![],
        },
    ];
    store.apply_batch(&msgs).unwrap();

    let mut ctx = build_context_from_store(store.connection());
    assert!(ctx.capabilities["guest-x"].contains("harness_repo"));
    assert!(ctx.capabilities["guest-x"].contains("gpu"));
    assert!(ctx.hello_roles["guest-x"].contains("coordinator"));
    assert!(ctx.capabilities["guest-y"].is_empty());
    assert!(ctx.hello_roles["guest-y"].is_empty());

    let now = 1_795_600_000;
    ctx.class_pools
        .insert("codex".into(), vec!["codex.5h".into()]);
    ctx.meters.insert(
        ("guest-x".into(), "codex.5h".into()),
        make_meter_with_runway("guest-x", "codex", "codex.5h", 10.0, 0.05, now, None),
    );
    ctx.meters.insert(
        ("guest-y".into(), "codex.5h".into()),
        make_meter_with_runway("guest-y", "codex", "codex.5h", 10.0, 0.05, now, None),
    );

    let mut pol = full_coordinator_policy();
    pol.base.candidates = vec![
        RoleCandidate {
            who: "guest-y/lead".into(),
            tier: 1,
            class: "codex".into(),
            scope: LeaseScope::Limited,
            spawn: false,
            opt_in: true,
        },
        RoleCandidate {
            who: "guest-x/lead".into(),
            tier: 2,
            class: "codex".into(),
            scope: LeaseScope::Limited,
            spawn: false,
            opt_in: true,
        },
    ];
    let mut table = RoleTable::new();
    table.set_policy("coordinator", pol.base.clone());

    let out = role_tick_full(&mut table, "coordinator", &pol, &ctx, now);
    assert_eq!(
        out.action,
        RoleAction::Granted {
            role: "coordinator".into(),
            holder: "guest-x/lead".into(),
            lease_scope: LeaseScope::Limited,
            epoch: 1,
            reason: "vacant".into(),
        }
    );
    let y_eval = out
        .evaluated_candidates
        .iter()
        .find(|e| e.candidate.who == "guest-y/lead")
        .unwrap();
    assert!(!y_eval.eligible);
    assert_eq!(
        y_eval.reason.as_deref(),
        Some("missing required capability harness_repo")
    );
}

#[test]
fn hooks_on_tick_projects_task_reaper_before_plan_enforce_avoids_duplicates() {
    use sommelier::hub::{Hub, HubConfig};
    use sommelier::policy::{Change, Store as PolicyStore};
    use sommelier::store::read_log_messages;
    use sommelier::tokens::TokenFile;

    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenFile::default();
    let owner_tok = tokens.add("owner", Role::Orchestrator).unwrap();
    let _worker_tok = tokens.add("worker-a", Role::Member).unwrap();
    tokens.save(&dir.path().join("tokens.json")).unwrap();
    let owner_member = tokens.lookup(&owner_tok).unwrap().clone();

    let pol_path = dir.path().join("enforce.toml");
    let source = sommelier::policy::DEFAULT_POLICY.replace("version = 1", "version = 2");
    fs::write(&pol_path, source).unwrap();
    drop(Hub::open(HubConfig::new(dir.path())).unwrap());
    PolicyStore::publish(dir.path(), &owner_tok, Change::Publish(&pol_path)).unwrap();

    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();

    // Create a task and an open offer that expires at t = 100s.
    hub.publish(
        &owner_member,
        Kind::TaskUpdate,
        "task/t-dup".into(),
        json!({
            "v": 1,
            "task": "t-dup",
            "title": "Test task",
            "kind": "fix",
            "scope": "bus",
            "done_when": "passes",
            "state": "ready",
        })
        .to_string(),
        vec!["state:ready".into()],
        vec![],
    )
    .unwrap();
    hub.publish(
        &owner_member,
        Kind::Offer,
        "task/t-dup".into(),
        json!({
            "v": 1,
            "offer_id": "off-1",
            "task": "t-dup",
            "to_owner": "worker-a",
            "class": "codex",
            "sent_at": 10,
            "expires": 100,
            "state": "open",
        })
        .to_string(),
        vec!["offer:open".into()],
        vec![],
    )
    .unwrap();

    // Run hooks::on_tick at t = 500s (> expires=100s).
    let remaining = sommelier::hooks::on_tick(&hub, 500_000);
    let logged = read_log_messages(&dir.path().join("bus.jsonl")).unwrap();
    let all_msgs: Vec<Message> = logged.into_iter().chain(remaining).collect();

    let expired_offers: Vec<&Message> = all_msgs
        .iter()
        .filter(|m| {
            m.kind == Kind::Offer
                && serde_json::from_str::<Value>(&m.body)
                    .ok()
                    .and_then(|v| v["state"].as_str().map(str::to_owned))
                    .as_deref()
                    == Some("expired")
        })
        .collect();
    let requeued_updates: Vec<&Message> = all_msgs
        .iter()
        .filter(|m| {
            m.kind == Kind::TaskUpdate
                && serde_json::from_str::<Value>(&m.body)
                    .ok()
                    .and_then(|v| v["reason"].as_str().map(str::to_owned))
                    .as_deref()
                    == Some("offer_expired")
        })
        .collect();

    assert_eq!(expired_offers.len(), 1, "expected exactly 1 expired offer");
    assert_eq!(
        requeued_updates.len(),
        1,
        "expected exactly 1 offer_expired task update"
    );
}

#[test]
fn next_decision_tick_id_queries_max_without_deserializing_history() {
    let store = Store::open_in_memory().unwrap();
    let conn = store.connection();
    assert_eq!(next_decision_tick_id(conn).unwrap(), 1);

    // Insert historical rows including malformed actions JSON to ensure it is not deserialized.
    conn.execute(
        "INSERT INTO DECISION (tick_id, at, policy_hash, state_hash, mode, actions, applied, msg_id)
         VALUES (1, 100, 'p1', 's1', 'shadow', '{invalid-json', 0, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO DECISION (tick_id, at, policy_hash, state_hash, mode, actions, applied, msg_id)
         VALUES (5, 200, 'p1', 's2', 'enforce', '[]', 1, 2)",
        [],
    )
    .unwrap();

    assert_eq!(next_decision_tick_id(conn).unwrap(), 6);
}

#[test]
fn role_hello_is_backfilled_from_existing_log_and_tables_on_schema_migration() {
    use rusqlite::Connection;
    use sommelier::store::ModuleRegistry;

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("bus.jsonl");
    let db_path = dir.path().join("state.db");

    let hello_x = Message {
        id: 1,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: "guest-x".into(),
        role: Role::Member,
        kind: Kind::Hello,
        topic: String::new(),
        body:
            "machine=box-x os=linux capabilities=harness_repo,gpu roles=coordinator class=codex:1"
                .into(),
        refs: vec![],
        to: vec![],
    };
    let hello_y = Message {
        id: 2,
        ts: "2026-11-25T10:00:05.000Z".into(),
        from: "guest-y".into(),
        role: Role::Member,
        kind: Kind::Hello,
        topic: String::new(),
        body: "machine=box-y os=windows class=codex:1".into(),
        refs: vec![],
        to: vec![],
    };
    fs::write(
        &log_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&hello_x).unwrap(),
            serde_json::to_string(&hello_y).unwrap()
        ),
    )
    .unwrap();

    // Simulate a pre-migration state.db that already applied the HELLO messages (cursor = 2)
    // before ROLE_HELLO existed.
    {
        let mut store = Store::open(dir.path()).unwrap();
        store.apply_batch(&[hello_x, hello_y]).unwrap();
        store
            .connection()
            .execute("DROP TABLE ROLE_HELLO", [])
            .unwrap();
    }

    // Reopen Store on the existing state.db: init_schemas must create and backfill ROLE_HELLO.
    let reopened = Store::open(dir.path()).unwrap();
    let ctx = build_context_from_store(reopened.connection());
    assert!(
        ctx.capabilities["guest-x"].contains("harness_repo"),
        "expected backfilled capabilities for guest-x, got {:?}",
        ctx.capabilities
    );
    assert!(ctx.capabilities["guest-x"].contains("gpu"));
    assert!(ctx.hello_roles["guest-x"].contains("coordinator"));
    assert!(ctx.capabilities["guest-y"].is_empty());
    assert!(ctx.hello_roles["guest-y"].is_empty());

    // Also verify table-level fallback when an in-memory DB has pre-existing MEMBER/CAPACITY rows.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(sommelier::store::CORE_SCHEMA_SQL)
        .unwrap();
    conn.execute(
        "INSERT INTO META (name, value, msg_id) VALUES ('cursor', '3', 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO MEMBER (name, role, trust, online, last_seen_at, msg_id)
         VALUES ('guest-z', 'member', 'guest', 1, 100, 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO CAPACITY (owner, machine, class, mode, useful_parallel, accepts, pools, declared_at, msg_id)
         VALUES ('guest-z', 'box-z', 'codex', 'own', 1, '[\"fix\",\"cap:harness_repo\",\"role:coordinator\"]', '[\"codex.5h\"]', 100, 3)",
        [],
    )
    .unwrap();
    ModuleRegistry::new().init_schemas(&conn).unwrap();
    let mem_ctx = build_context_from_store(&conn);
    assert!(mem_ctx.capabilities["guest-z"].contains("harness_repo"));
    assert!(mem_ctx.hello_roles["guest-z"].contains("coordinator"));
    drop(db_path);
}

#[test]
fn per_tick_plan_state_hash_excludes_append_only_decision_history() {
    let mut store = Store::open_in_memory().unwrap();
    let policy = PlanPolicy::default();

    let hello = Message {
        id: 1,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: "hub-owner".into(),
        role: Role::Orchestrator,
        kind: Kind::Hello,
        topic: String::new(),
        body: "machine=box1 os=windows gpu=rtx4090 class=codex:2 mode=auto accepts=fix,review"
            .into(),
        refs: vec![],
        to: vec![],
    };
    store.apply(&hello).unwrap();

    let state_before =
        sommelier::plan::load_plan_state_from_store(store.connection(), &policy, 1000).unwrap();
    let hash_before = state_before.state_hash();
    assert!(!hash_before.is_empty());

    // Append DECISION history rows (including corrupted JSON that would trip full row deserialization)
    // and verify that per-tick `PlanState::state_hash()` is unchanged and does not scan `DECISION`.
    let decision_msg = Message {
        id: 2,
        ts: "2026-11-25T10:00:15.000Z".into(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic: "decision/1".into(),
        body: json!({
            "v": 1,
            "tick_id": 1,
            "at": 1015,
            "policy_hash": policy.policy_hash(),
            "state_hash": hash_before,
            "mode": "shadow",
            "actions": [],
            "applied": false,
        })
        .to_string(),
        refs: vec!["mode:shadow".into()],
        to: vec![],
    };
    store.apply(&decision_msg).unwrap();

    let state_after =
        sommelier::plan::load_plan_state_from_store(store.connection(), &policy, 1000).unwrap();
    assert_eq!(
        state_after.state_hash(),
        hash_before,
        "append-only DECISION rows must not change per-tick PlanState::state_hash()"
    );
}

#[test]
fn empty_decision_spam_is_suppressed_within_300s_and_emitted_on_change_action_or_heartbeat() {
    use sommelier::hub::{Hub, HubConfig};
    use sommelier::policy::{Change, Store as PolicyStore};
    use sommelier::tokens::TokenFile;

    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenFile::default();
    let owner_tok = tokens.add("owner", Role::Orchestrator).unwrap();
    tokens.save(&dir.path().join("tokens.json")).unwrap();
    let owner_member = tokens.lookup(&owner_tok).unwrap().clone();

    // Publish an enforce policy with no declared capacity slots so idle ticks produce empty actions.
    let pol_path = dir.path().join("enforce_no_spam.toml");
    let source = sommelier::policy::DEFAULT_POLICY
        .replace("version = 1", "version = 2")
        .replace("mode = \"off\"", "mode = \"enforce\"");
    fs::write(&pol_path, source).unwrap();
    drop(Hub::open(HubConfig::new(dir.path())).unwrap());
    PolicyStore::publish(dir.path(), &owner_tok, Change::Publish(&pol_path)).unwrap();

    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let apply_tick = |now_sec: u64| -> Vec<Message> {
        let msgs = sommelier::plan::on_tick(&hub, now_sec * 1000);
        for m in &msgs {
            hub.publish(
                &owner_member,
                m.kind,
                m.topic.clone(),
                m.body.clone(),
                m.refs.clone(),
                m.to.clone(),
            )
            .unwrap();
        }
        msgs
    };

    // Seed owner online before initial tick so hub-emitted FACT messages do not change MEMBER.online.
    hub.publish(
        &owner_member,
        Kind::Chat,
        String::new(),
        "ready".into(),
        vec![],
        vec![],
    )
    .unwrap();

    // 1. Initial empty tick at t = 1000s emits FACT decision/1.
    let m1 = apply_tick(1000);
    assert_eq!(m1.len(), 1);
    assert_eq!(m1[0].topic, "decision/1");

    // 2. Subsequent empty ticks at 1010s, 1020s, and 1299s with identical mode + policy_hash + state_hash are suppressed!
    assert!(
        apply_tick(1010).is_empty(),
        "10s duplicate empty decision must be suppressed"
    );
    assert!(
        apply_tick(1020).is_empty(),
        "20s duplicate empty decision must be suppressed"
    );
    assert!(
        apply_tick(1299).is_empty(),
        "299s duplicate empty decision must be suppressed"
    );

    // 3. At t = 1300s (300s elapsed), heartbeat emits FACT decision/2.
    let m_hb = apply_tick(1300);
    assert_eq!(m_hb.len(), 1);
    assert_eq!(m_hb[0].topic, "decision/2");
    assert!(apply_tick(1310).is_empty());

    // 4. State change (new task added to state.db) emits immediately at t = 1320s even when actions are empty.
    hub.publish(
        &owner_member,
        Kind::TaskUpdate,
        "task/t-state-change".into(),
        json!({
            "v": 1,
            "task": "t-state-change",
            "title": "State change task",
            "kind": "fix",
            "scope": "bus",
            "done_when": "ok",
            "state": "draft",
        })
        .to_string(),
        vec!["state:draft".into()],
        vec![],
    )
    .unwrap();
    let m_state = apply_tick(1320);
    assert_eq!(m_state.len(), 1, "state_hash change must emit immediately");
    assert_eq!(m_state[0].topic, "decision/3");
    assert!(apply_tick(1330).is_empty());

    // 5. Policy / mode change emits immediately at t = 1340s.
    let shadow_source = sommelier::policy::DEFAULT_POLICY
        .replace("version = 1", "version = 3")
        .replace("mode = \"off\"", "mode = \"shadow\"");
    fs::write(&pol_path, shadow_source).unwrap();
    PolicyStore::publish(dir.path(), &owner_tok, Change::Publish(&pol_path)).unwrap();
    let m_pol = apply_tick(1340);
    assert_eq!(m_pol.len(), 1, "policy/mode change must emit immediately");
    assert_eq!(m_pol[0].topic, "decision/4");
    assert!(apply_tick(1350).is_empty());

    // 6. Non-empty actions (!actions.is_empty()) emit immediately.
    hub.publish(
        &owner_member,
        Kind::Hello,
        String::new(),
        "os=windows class=gemini:1 mode=auto".into(),
        vec![],
        vec![],
    )
    .unwrap();
    let m_act1 = apply_tick(1360);
    assert!(
        !m_act1.is_empty(),
        "non-empty actions must emit immediately"
    );
    let m_act2 = apply_tick(1370);
    assert!(
        !m_act2.is_empty(),
        "non-empty actions must emit even within 300s"
    );
}
