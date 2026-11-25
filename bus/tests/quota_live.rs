//! Integration and table tests for step 9 (`quota-live`):
//! - Supervisor quota poller (`src/agent/quota_poll.rs`) on fixture folders:
//!   reports each sample once, also after a restart (offsets kept).
//! - Spend deduplication by `(file_key, byte_offset)` so session files read twice never double count.
//! - Op `quota.report` ownership check: a report for another owner's meter is refused.
//! - Probe fault tolerance: a probe that fails three times in a row turns itself off,
//!   falls back to `inferred`, and raises exactly one `PROBE_BROKEN` alert.
//! - Store projection (`QUOTA_SAMPLE`, `METER`, `SPEND`, `ESTIMATE`) and `/api/state` `"pools"` JSON matching the golden fixture.
//! - `COORD_BUDGET` alert at 70 percent (section 13.6), reporting whether an eligible successor exists.
//! - `ui/views/pools.js` view contract verification.
//!
//! No test uses the network, a vendor login, or the live hub.
//! Fixtures hold no account id, email, token, or user folder.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};
use sommelier::agent::children::{ChildRecord, ChildrenStore};
use sommelier::agent::quota_poll::{
    CODEX_APPSERVER_STALE_SECS, QuotaPoller, QuotaPollerConfig, is_report_empty,
};
use sommelier::msg::{Kind, Message, Role};
use sommelier::quota::api::{PoolApiRow, WaitingPoolTask, build_pool_row};
use sommelier::quota::apply::{
    DEFAULT_COORD_BUDGET_ALARM_PCT, QUOTA_TABLES, QuotaModule, QuotaReportOp, ReportedSpend,
    check_coord_budget_alerts, handle_quota_report, query_estimates, query_meter_rows,
    query_quota_samples, query_spend, validate_and_build_report_messages,
};
use sommelier::quota::estimate::{Estimate, EstimateSource};
use sommelier::quota::forecast::ActiveLease;
use sommelier::quota::meter::{
    ConfidenceBand, Meter, QuotaConfig, QuotaSample, Source, SpendRecord, StateEvent,
    contains_sensitive_text,
};
use sommelier::role::{LeaseScope, RoleCandidate, RolePolicy};
use sommelier::store::{Store, query_alerts};

static TEST_SEQ: AtomicU32 = AtomicU32::new(1);

fn temp_test_dir(label: &str) -> PathBuf {
    let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "sommelier-quota-live-test-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

const ROLLOUT_LINE_1: &str = r#"{"timestamp":"2026-11-25T10:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"cached_input_tokens":5000,"output_tokens":200,"total_tokens":10200},"last_token_usage":{"input_tokens":10000,"cached_input_tokens":5000,"output_tokens":200,"total_tokens":10200}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1795618000},"secondary":{"used_percent":40.0,"window_minutes":10080,"resets_at":1796158000}}}}"#;
const ROLLOUT_LINE_2: &str = r#"{"timestamp":"2026-11-25T10:10:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30000,"cached_input_tokens":15000,"output_tokens":600,"total_tokens":30600},"last_token_usage":{"input_tokens":20000,"cached_input_tokens":10000,"output_tokens":400,"total_tokens":20400}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":18.0,"window_minutes":300,"resets_at":1795618000},"secondary":{"used_percent":42.0,"window_minutes":10080,"resets_at":1796158000}}}}"#;
const ROLLOUT_LINE_3: &str = r#"{"timestamp":"2026-11-25T10:20:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":25000,"output_tokens":1000,"total_tokens":51000},"last_token_usage":{"input_tokens":20000,"cached_input_tokens":10000,"output_tokens":400,"total_tokens":20400}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":25.0,"window_minutes":300,"resets_at":1795618000},"secondary":{"used_percent":44.0,"window_minutes":10080,"resets_at":1796158000}}}}"#;

const CLAUDE_STATUSLINE_1: &str = r#"{"cost":{"total_api_duration_ms":12500},"rate_limits":{"five_hour":{"used_percentage":42.0,"resets_at":1795618000},"seven_day":{"used_percentage":65.0,"resets_at":1796158000}}}"#;

const GOLDEN_POOLS_FIXTURE: &str = r#"[
  {
    "owner": "owner1",
    "vendor": "gemini",
    "pool": "agy.window",
    "window": "5h",
    "state": "paused",
    "drain": false,
    "used_pct": 50.0,
    "committed_pct": 0.0,
    "line_pct": 90.0,
    "stop_at_pct": 90.0,
    "reserve_pct": 10.0,
    "resets_at": 1795613087,
    "burn_pct_per_min": 0.0,
    "burn_pct_per_hour": 0.0,
    "t_line": null,
    "t_80": null,
    "resets_first": false,
    "forecast_label": "paused, resets in 2 h 4 min",
    "source": "inferred",
    "confidence": 0.2,
    "band": "low",
    "observed_at": 1795605600,
    "age_secs": 0,
    "fresh_min": 5,
    "stale": false,
    "probe_enabled": true,
    "active_leases": 0,
    "waiting_tasks": []
  },
  {
    "owner": "owner1",
    "vendor": "claude",
    "pool": "claude.week",
    "window": "week",
    "state": "open",
    "drain": false,
    "used_pct": 72.0,
    "committed_pct": 0.0,
    "line_pct": 80.0,
    "stop_at_pct": 80.0,
    "reserve_pct": 20.0,
    "resets_at": 1796158000,
    "burn_pct_per_min": 0.0,
    "burn_pct_per_hour": 0.0,
    "t_line": null,
    "t_80": null,
    "resets_first": false,
    "forecast_label": "resets first, in 153 h 26 min",
    "source": "api",
    "confidence": 0.9,
    "band": "high",
    "observed_at": 1795605600,
    "age_secs": 0,
    "fresh_min": 30,
    "stale": false,
    "probe_enabled": true,
    "active_leases": 0,
    "waiting_tasks": []
  },
  {
    "owner": "owner1",
    "vendor": "codex",
    "pool": "codex.5h",
    "window": "5h",
    "state": "drain",
    "drain": true,
    "used_pct": 86.0,
    "committed_pct": 0.0,
    "line_pct": 90.0,
    "stop_at_pct": 90.0,
    "reserve_pct": 10.0,
    "resets_at": 1795618000,
    "burn_pct_per_min": 0.2,
    "burn_pct_per_hour": 12.0,
    "t_line": 1795606800,
    "t_80": 1795605600,
    "resets_first": false,
    "forecast_label": "reaches the line in 20 min",
    "source": "api",
    "confidence": 0.9,
    "band": "high",
    "observed_at": 1795605600,
    "age_secs": 0,
    "fresh_min": 5,
    "stale": false,
    "probe_enabled": true,
    "active_leases": 0,
    "waiting_tasks": []
  }
]"#;

fn open_store_with_quota(dir: &std::path::Path) -> Store {
    let mut store = Store::open(dir).unwrap();
    store.register_module(Box::new(QuotaModule)).unwrap();
    store
}

#[test]
fn poller_reports_each_sample_once_and_keeps_offsets_across_restart() {
    let dir = temp_test_dir("poller-restart");
    let data_dir = dir.join("data");
    let sessions_dir = dir.join("sessions").join("2026").join("11").join("25");
    fs::create_dir_all(&sessions_dir).unwrap();
    let rollout_path = sessions_dir.join("rollout-100000-thread42.jsonl");

    // Write the first two rollout lines
    fs::write(
        &rollout_path,
        format!("{ROLLOUT_LINE_1}\n{ROLLOUT_LINE_2}\n"),
    )
    .unwrap();

    // Write claude drop file
    let claude_drop_path = data_dir.join("quota").join("claude.json");
    fs::create_dir_all(claude_drop_path.parent().unwrap()).unwrap();
    fs::write(&claude_drop_path, CLAUDE_STATUSLINE_1).unwrap();

    let cfg = QuotaPollerConfig::new("owner1", &data_dir)
        .with_codex_sessions_dir(dir.join("sessions"))
        .with_claude_drop_path(&claude_drop_path);

    let mut store = open_store_with_quota(&data_dir);
    let t0 = 1_795_605_000;

    // 1. First poller instance reads lines 1 & 2 and claude drop
    {
        let mut poller = QuotaPoller::open(cfg.clone()).unwrap();
        let rep1 = poller.poll_tick(t0).unwrap();

        // 2 lines * 2 windows (codex.5h, codex.week) = 4 codex samples + 2 claude samples = 6 samples
        assert_eq!(rep1.samples.len(), 6);
        // 2 codex spend records keyed by file and byte offset
        assert_eq!(rep1.spend.len(), 2);
        assert_eq!(
            rep1.spend[0].file_key.as_deref(),
            Some("2026/11/25/rollout-100000-thread42.jsonl")
        );
        assert_eq!(rep1.spend[0].byte_offset, Some(0));
        assert_eq!(
            rep1.spend[0].record.worker.as_deref(),
            Some("thread42"),
            "thread id from filename joins spend to worker/thread"
        );
        assert!(rep1.spend[1].byte_offset.unwrap() > 0);

        handle_quota_report(&mut store, "owner1", Role::Orchestrator, &rep1, None, t0).unwrap();

        // Immediate second tick with no new bytes in files reports nothing!
        let rep2 = poller.poll_tick(t0 + 10).unwrap();
        assert!(
            rep2.samples.is_empty(),
            "unchanged files must not re-report samples: {:?}",
            rep2.samples
        );
        assert!(
            rep2.spend.is_empty(),
            "unchanged files must not re-report spend: {:?}",
            rep2.spend
        );
    }

    // 2. Simulate supervisor restart: open a fresh QuotaPoller from disk at t0 + 20
    {
        let mut poller_restarted = QuotaPoller::open(cfg.clone()).unwrap();
        let rep_after_restart = poller_restarted.poll_tick(t0 + 20).unwrap();
        assert!(
            rep_after_restart.samples.is_empty(),
            "after restart, already-read lines must not be reported again: {:?}",
            rep_after_restart.samples
        );
        assert!(
            rep_after_restart.spend.is_empty(),
            "after restart, already-read spend must not be reported again"
        );

        // Append a 3rd rollout line and verify only line 3 is reported
        let mut f = OpenOptions::new().append(true).open(&rollout_path).unwrap();
        writeln!(f, "{ROLLOUT_LINE_3}").unwrap();
        drop(f);

        let rep3 = poller_restarted.poll_tick(t0 + 30).unwrap();
        assert_eq!(
            rep3.samples.len(),
            2,
            "only the 2 new samples from line 3 should be reported"
        );
        assert_eq!(rep3.samples[0].pool, "codex.5h");
        assert_eq!(rep3.samples[0].used_pct, 25.0);
        assert_eq!(rep3.samples[1].pool, "codex.week");
        assert_eq!(rep3.samples[1].used_pct, 44.0);
        assert_eq!(
            rep3.spend.len(),
            1,
            "only the 1 new spend record from line 3 should be reported"
        );

        handle_quota_report(
            &mut store,
            "owner1",
            Role::Orchestrator,
            &rep3,
            None,
            t0 + 30,
        )
        .unwrap();
    }

    // 3. Verify store tables have exact counts and no double-counted spend even if the same report is re-applied
    let spend_rows = query_spend(store.connection()).unwrap();
    assert_eq!(spend_rows.len(), 3);
    let total_units: f64 = spend_rows.iter().map(|s| s.units).sum();

    // Re-sending a duplicate spend report with the same (file_key, byte_offset) must not double-count
    let dup_rep = QuotaReportOp {
        op: "quota.report".into(),
        owner: "owner1".into(),
        spend: vec![ReportedSpend {
            record: SpendRecord {
                owner: "owner1".into(),
                pool: "codex.5h".into(),
                task_id: Some("t-dup".into()),
                at: t0 + 999,
                worker: Some("thread42".into()),
                model: None,
                units: 99999.0,
                input_tokens: 10000,
                cached_tokens: 5000,
                output_tokens: 200,
            },
            file_key: Some("2026/11/25/rollout-100000-thread42.jsonl".into()),
            byte_offset: Some(0),
        }],
        ..Default::default()
    };
    handle_quota_report(
        &mut store,
        "owner1",
        Role::Orchestrator,
        &dup_rep,
        None,
        t0 + 999,
    )
    .unwrap();
    let spend_after_dup = query_spend(store.connection()).unwrap();
    assert_eq!(
        spend_after_dup.len(),
        3,
        "duplicate (file_key, byte_offset) must be ignored"
    );
    let total_units_after: f64 = spend_after_dup.iter().map(|s| s.units).sum();
    assert!((total_units - total_units_after).abs() < 1e-9);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn report_for_another_owners_meter_is_refused() {
    let dir = temp_test_dir("owner-refusal");
    let mut store = open_store_with_quota(&dir);
    let now = 1_795_605_600;

    // 1. Report where top-level owner differs from sender
    let bad_top = QuotaReportOp {
        op: "quota.report".into(),
        owner: "owner1".into(),
        samples: vec![QuotaSample {
            owner: "owner1".into(),
            vendor: "codex".into(),
            pool: "codex.5h".into(),
            window: "5h".into(),
            observed_at: now,
            used_pct: 10.0,
            resets_at: now + 18000,
            source: Source::Api,
            confidence: 0.9,
        }],
        ..Default::default()
    };
    let err1 = handle_quota_report(
        &mut store,
        "guest_mallory",
        Role::Member,
        &bad_top,
        None,
        now,
    )
    .expect_err("guest_mallory reporting for owner1 must be refused");
    assert_eq!(err1.code, 3);
    assert!(
        err1.reason
            .contains("cannot report quota or spend for another owner"),
        "{}",
        err1.reason
    );

    // 2. Report where top-level owner matches sender, but an inner sample targets another owner's meter
    let bad_sample = QuotaReportOp {
        op: "quota.report".into(),
        owner: "guest_mallory".into(),
        samples: vec![QuotaSample {
            owner: "owner1".into(),
            vendor: "codex".into(),
            pool: "codex.5h".into(),
            window: "5h".into(),
            observed_at: now,
            used_pct: 99.0,
            resets_at: now + 18000,
            source: Source::Api,
            confidence: 0.9,
        }],
        ..Default::default()
    };
    let err2 =
        validate_and_build_report_messages("guest_mallory", Role::Member, &bad_sample, now * 1000)
            .expect_err("inner sample for another owner must be refused");
    assert_eq!(err2.code, 3);
    assert_eq!(err2.target_owner, "owner1");

    // 3. Report where an inner spend record targets another owner
    let bad_spend = QuotaReportOp {
        op: "quota.report".into(),
        owner: "guest_mallory".into(),
        spend: vec![ReportedSpend {
            record: SpendRecord {
                owner: "owner1".into(),
                pool: "codex.5h".into(),
                task_id: None,
                at: now,
                worker: None,
                model: None,
                units: 5000.0,
                input_tokens: 1000,
                cached_tokens: 0,
                output_tokens: 1000,
            },
            file_key: None,
            byte_offset: None,
        }],
        ..Default::default()
    };
    let err3 =
        validate_and_build_report_messages("guest_mallory", Role::Member, &bad_spend, now * 1000)
            .expect_err("inner spend for another owner must be refused");
    assert_eq!(err3.code, 3);

    // 4. Direct log replay defense: even if a forged FACT message from `guest_mallory` on topic `quota/owner1/codex.5h/5h`
    // appeared in a log, `apply_quota_message` refuses to touch `owner1`'s tables.
    let forged = Message {
        id: 1,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: "guest_mallory".into(),
        role: Role::Member,
        kind: Kind::Fact,
        topic: "quota/owner1/codex.5h/5h".into(),
        body: json!({
            "v": 1,
            "owner": "owner1",
            "vendor": "codex",
            "pool": "codex.5h",
            "window": "5h",
            "used_pct": 99.0,
            "resets_at": now + 18000,
            "source": "api",
            "confidence": 0.9,
            "observed_at": now,
        })
        .to_string(),
        refs: vec![],
        to: vec![],
    };
    store.apply(&forged).unwrap();
    assert!(
        query_meter_rows(store.connection()).unwrap().is_empty(),
        "forged FACT for another owner must not create or modify METER rows"
    );
    assert!(
        query_quota_samples(store.connection()).unwrap().is_empty(),
        "forged FACT for another owner must not create QUOTA_SAMPLE rows"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn probe_failing_three_times_turns_off_falls_back_to_inferred_and_raises_one_alert() {
    let dir = temp_test_dir("probe-broken");
    let data_dir = dir.join("data");
    let claude_drop_path = data_dir.join("quota").join("claude.json");
    fs::create_dir_all(claude_drop_path.parent().unwrap()).unwrap();

    let cfg = QuotaPollerConfig::new("owner1", &data_dir).with_claude_drop_path(&claude_drop_path);
    let mut poller = QuotaPoller::open(cfg).unwrap();
    let mut store = open_store_with_quota(&data_dir);
    let t0 = 1_795_605_000;

    // Seed a valid claude sample first so the meter exists with source=api
    fs::write(&claude_drop_path, CLAUDE_STATUSLINE_1).unwrap();
    let rep_ok = poller.poll_claude_drop(t0).unwrap();
    assert_eq!(rep_ok.samples.len(), 2);
    assert!(rep_ok.broken_probes.is_empty());
    handle_quota_report(&mut store, "owner1", Role::Orchestrator, &rep_ok, None, t0).unwrap();

    // Now write corrupt/unparseable JSON to claude.json and poll 3 times in a row
    fs::write(&claude_drop_path, "{broken json line 1").unwrap();
    let fail1 = poller.poll_claude_drop(t0 + 60).unwrap();
    assert!(
        fail1.broken_probes.is_empty(),
        "1st failure must not disable probe yet"
    );
    assert!(poller.is_probe_active("claude_drop"));

    fs::write(&claude_drop_path, "{broken json line 2").unwrap();
    let fail2 = poller.poll_claude_drop(t0 + 120).unwrap();
    assert!(
        fail2.broken_probes.is_empty(),
        "2nd failure must not disable probe yet"
    );
    assert!(poller.is_probe_active("claude_drop"));

    fs::write(&claude_drop_path, "{broken json line 3").unwrap();
    let fail3 = poller.poll_claude_drop(t0 + 180).unwrap();
    assert!(
        !poller.is_probe_active("claude_drop"),
        "3rd consecutive failure must turn the probe off"
    );
    assert_eq!(
        fail3.broken_probes.len(),
        1,
        "3rd consecutive failure must raise exactly one PROBE_BROKEN alert"
    );
    assert_eq!(fail3.broken_probes[0].probe, "claude_drop");
    assert_eq!(fail3.broken_probes[0].failures, 3);
    // Meters fall back to `inferred`
    assert!(
        fail3.samples.iter().all(|s| s.source == Source::Inferred),
        "fallback samples must have source=inferred"
    );

    handle_quota_report(
        &mut store,
        "owner1",
        Role::Orchestrator,
        &fail3,
        None,
        t0 + 180,
    )
    .unwrap();

    // 4th poll while probe is off does nothing and raises no second alert
    let fail4 = poller.poll_claude_drop(t0 + 240).unwrap();
    assert!(is_report_empty(&fail4));
    assert!(fail4.broken_probes.is_empty());

    // Verify in store: exactly one PROBE_BROKEN alert row with count 1, and claude meters have probe_enabled = false and source = inferred
    let alerts = query_alerts(store.connection()).unwrap();
    let probe_alerts: Vec<_> = alerts.iter().filter(|a| a.code == "PROBE_BROKEN").collect();
    assert_eq!(probe_alerts.len(), 1);
    assert_eq!(probe_alerts[0].key, "owner1/claude_drop");
    assert_eq!(probe_alerts[0].state, "open");
    assert_eq!(probe_alerts[0].count, 1);

    let meters = query_meter_rows(store.connection()).unwrap();
    for m in meters.iter().filter(|m| m.vendor == "claude") {
        assert!(
            !m.probe_enabled,
            "meter {} must have probe_enabled=false",
            m.pool
        );
        assert_eq!(
            m.latest_sample.as_ref().unwrap().source,
            Source::Inferred,
            "meter {} must fall back to inferred",
            m.pool
        );
    }

    // Also verify codex_appserver 3-failure cutoff
    let mut app_poller =
        QuotaPoller::open(QuotaPollerConfig::new("owner1", dir.join("data2"))).unwrap();
    for i in 1..=3 {
        let rep = app_poller
            .poll_codex_appserver_with(t0 + i * 300, CODEX_APPSERVER_STALE_SECS, || {
                Err("app-server timed out".into())
            })
            .unwrap();
        if i < 3 {
            assert!(rep.broken_probes.is_empty());
            assert!(app_poller.is_probe_active("codex_appserver"));
        } else {
            assert_eq!(rep.broken_probes.len(), 1);
            assert!(!app_poller.is_probe_active("codex_appserver"));
        }
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pools_json_matches_golden_fixture_and_coord_budget_alert_fires_at_70_percent() {
    let dir = temp_test_dir("pools-fixture");
    let mut store = open_store_with_quota(&dir);
    let now = 1_795_605_600_u64;

    let coord_policy = RolePolicy {
        heartbeat_sec: 60,
        dead_sec: 180,
        min_hold_min: 15,
        cooldown_min: 30,
        max_changes_per_hour: 3,
        require: vec![],
        candidates: vec![
            RoleCandidate {
                who: "owner1/coord-claude".into(),
                tier: 1,
                class: "claude_strong".into(),
                scope: LeaseScope::Full,
                spawn: true,
                opt_in: false,
            },
            RoleCandidate {
                who: "owner1/coord-codex".into(),
                tier: 2,
                class: "codex".into(),
                scope: LeaseScope::Full,
                spawn: true,
                opt_in: false,
            },
        ],
    };

    // Build report that populates:
    // 1. codex.5h with 3 samples over 10 minutes (84% -> 85% -> 86%, burn = 0.2%/min = 12%/h, drain state)
    // 2. claude.week at 72% (>= 70% COORD_BUDGET threshold)
    // 3. agy.window at 50% (inferred) with a pause state_event for 2h4m47s (7487s -> resets_at = 1795613087)
    // 4. ESTIMATE row for ("fix", "codex")
    let report = QuotaReportOp {
        op: "quota.report".into(),
        owner: "owner1".into(),
        samples: vec![
            QuotaSample {
                owner: "owner1".into(),
                vendor: "codex".into(),
                pool: "codex.5h".into(),
                window: "5h".into(),
                observed_at: now - 600,
                used_pct: 84.0,
                resets_at: 1_795_618_000,
                source: Source::Api,
                confidence: 0.9,
            },
            QuotaSample {
                owner: "owner1".into(),
                vendor: "codex".into(),
                pool: "codex.5h".into(),
                window: "5h".into(),
                observed_at: now - 300,
                used_pct: 85.0,
                resets_at: 1_795_618_000,
                source: Source::Api,
                confidence: 0.9,
            },
            QuotaSample {
                owner: "owner1".into(),
                vendor: "codex".into(),
                pool: "codex.5h".into(),
                window: "5h".into(),
                observed_at: now,
                used_pct: 86.0,
                resets_at: 1_795_618_000,
                source: Source::Api,
                confidence: 0.9,
            },
            QuotaSample {
                owner: "owner1".into(),
                vendor: "claude".into(),
                pool: "claude.week".into(),
                window: "week".into(),
                observed_at: now,
                used_pct: 72.0,
                resets_at: 1_796_158_000,
                source: Source::Api,
                confidence: 0.9,
            },
            QuotaSample {
                owner: "owner1".into(),
                vendor: "gemini".into(),
                pool: "agy.window".into(),
                window: "5h".into(),
                observed_at: now,
                used_pct: 50.0,
                resets_at: 1_795_613_087,
                source: Source::Inferred,
                confidence: 0.2,
            },
        ],
        state_events: vec![sommelier::quota::apply::ReportedStateEvent {
            owner: "owner1".into(),
            vendor: "gemini".into(),
            pool: "agy.window".into(),
            window: "5h".into(),
            event: StateEvent {
                resets_at: 1_795_613_087,
                observed_at: now,
                message: "agy quota reached, resets_at 1795613087".into(),
                family: Some("carbon".into()),
            },
        }],
        estimates: vec![Estimate {
            kind: "fix".into(),
            class: "codex".into(),
            n: 8,
            minutes_p50: 25.0,
            minutes_p90: 60.0,
            units_p50: 350_000.0,
            units_p90: 1_100_000.0,
            source: EstimateSource::Learned,
        }],
        ..Default::default()
    };

    handle_quota_report(
        &mut store,
        "owner1",
        Role::Orchestrator,
        &report,
        Some(&coord_policy),
        now,
    )
    .unwrap();

    // 1. Check that all 4 quota tables exist in state.db and have rows
    let hashes = store.table_hashes().unwrap();
    for table in QUOTA_TABLES {
        assert!(hashes.contains_key(table), "missing quota table {table}");
    }
    assert_eq!(query_estimates(store.connection()).unwrap().len(), 1);

    // 2. Check `/api/state` `"pools"` JSON against the golden fixture
    let api_state = store.api_state(now).unwrap();
    let actual_pools = &api_state["pools"];
    let expected_pools: Value = serde_json::from_str(GOLDEN_POOLS_FIXTURE).unwrap();
    assert_eq!(
        actual_pools,
        &expected_pools,
        "pools JSON mismatch:\nactual: {}\nexpected: {}",
        serde_json::to_string_pretty(actual_pools).unwrap(),
        serde_json::to_string_pretty(&expected_pools).unwrap()
    );
    assert!(!contains_sensitive_text(&actual_pools.to_string()));

    // 3. Check COORD_BUDGET alert at 70%:
    // Since claude.week is at 72% (>= 70%) and codex.5h is in drain (86%), no successor is eligible.
    let alerts = query_alerts(store.connection()).unwrap();
    let coord_alerts: Vec<_> = alerts.iter().filter(|a| a.code == "COORD_BUDGET").collect();
    assert_eq!(coord_alerts.len(), 1);
    assert_eq!(coord_alerts[0].key, "owner1/claude.week");
    assert_eq!(coord_alerts[0].state, "open");
    assert!(
        coord_alerts[0].text.contains("no eligible successor"),
        "{}",
        coord_alerts[0].text
    );
    assert_eq!(coord_alerts[0].data["successor_eligible"], false);

    // Not repeated if checked again while still open
    let repeat = check_coord_budget_alerts(
        store.connection(),
        Some(&coord_policy),
        DEFAULT_COORD_BUDGET_ALARM_PCT,
        now + 60,
    )
    .unwrap();
    assert!(
        repeat.is_empty(),
        "open COORD_BUDGET alert must not repeat on every tick"
    );

    // 4. Now test COORD_BUDGET when a successor IS eligible (e.g. codex.5h is open at 20%)
    let dir2 = temp_test_dir("coord-budget-with-successor");
    let mut store2 = open_store_with_quota(&dir2);
    let rep_with_succ = QuotaReportOp {
        op: "quota.report".into(),
        owner: "owner1".into(),
        samples: vec![
            QuotaSample {
                owner: "owner1".into(),
                vendor: "codex".into(),
                pool: "codex.5h".into(),
                window: "5h".into(),
                observed_at: now,
                used_pct: 20.0,
                resets_at: 1_795_618_000,
                source: Source::Api,
                confidence: 0.9,
            },
            QuotaSample {
                owner: "owner1".into(),
                vendor: "claude".into(),
                pool: "claude.5h".into(),
                window: "5h".into(),
                observed_at: now,
                used_pct: 75.0,
                resets_at: 1_795_618_000,
                source: Source::Api,
                confidence: 0.9,
            },
        ],
        ..Default::default()
    };
    handle_quota_report(
        &mut store2,
        "owner1",
        Role::Orchestrator,
        &rep_with_succ,
        Some(&coord_policy),
        now,
    )
    .unwrap();
    let alerts2 = query_alerts(store2.connection()).unwrap();
    let ca2 = alerts2
        .iter()
        .find(|a| a.code == "COORD_BUDGET")
        .expect("COORD_BUDGET must open at 75%");
    assert_eq!(ca2.data["successor_eligible"], true);
    assert_eq!(ca2.data["successor"], "owner1/coord-codex");
    assert!(
        ca2.text.contains("eligible successor: owner1/coord-codex"),
        "{}",
        ca2.text
    );

    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&dir2);
}

#[test]
fn poller_agy_live_child_spend_and_child_output_events_and_ui_view() {
    let dir = temp_test_dir("agy-and-ui");
    let data_dir = dir.join("data");

    // Register a live child process (using current test process PID so sysinfo sees it alive)
    let children = ChildrenStore::new(&data_dir);
    let my_pid = std::process::id();
    let t0 = 1_795_600_000;
    children
        .save(&ChildRecord {
            name: "w-gemini".into(),
            class: "gemini_build".into(),
            model: "gemini-4-carbon-medium".into(),
            branch: "gemini/quota-live".into(),
            task_id: Some("t-quota-live".into()),
            pid: my_pid,
            started_at_sec: t0,
            heartbeat_at_sec: t0,
            out_dir: None,
            worktree: None,
            state: "running".into(),
            text: "building".into(),
        })
        .unwrap();

    let mut poller = QuotaPoller::open(QuotaPollerConfig::new("owner1", &data_dir)).unwrap();
    // Poll at t0 + 120s (2 minutes of medium effort = 2 * 2.0 = 4.0 units)
    let rep_agy = poller.poll_agy_spend(t0 + 120).unwrap();
    assert_eq!(rep_agy.spend.len(), 1);
    assert!((rep_agy.spend[0].record.units - 4.0).abs() < 1e-6);
    assert_eq!(rep_agy.samples.len(), 1);
    assert_eq!(rep_agy.samples[0].pool, "agy.window");
    assert_eq!(rep_agy.samples[0].source, Source::Inferred);

    // Poll child output: two Gemini family hits within 10 minutes pause the whole agy.window pool
    let hit1 = poller
        .on_child_output(
            "gemini",
            Some("carbon"),
            r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 2h4m47s."}"#,
            t0 + 180,
        )
        .unwrap();
    assert_eq!(hit1.state_events.len(), 1);
    assert_eq!(hit1.state_events[0].pool, "agy.family.carbon");

    let hit2 = poller
        .on_child_output(
            "gemini",
            Some("hydrogen"),
            r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 2h10m0s."}"#,
            t0 + 300,
        )
        .unwrap();
    assert!(
        hit2.state_events.iter().any(|e| e.pool == "agy.window"),
        "second family hit within 10 minutes must pause agy.window"
    );

    // Verify build_pool_row with active leases and waiting tasks (section 6.4 & 7.7)
    let mut m = Meter::new("owner1", "codex", "codex.5h", "5h");
    sommelier::quota::forecast::on_sample(
        &mut m,
        QuotaSample {
            owner: "owner1".into(),
            vendor: "codex".into(),
            pool: "codex.5h".into(),
            window: "5h".into(),
            observed_at: t0,
            used_pct: 50.0,
            resets_at: t0 + 18000,
            source: Source::Api,
            confidence: 0.9,
        },
    );
    let row: PoolApiRow = build_pool_row(
        &m,
        t0 + 600, // 10 min old > fresh_min (5) -> stale = true
        &QuotaConfig::default(),
        &[ActiveLease {
            pools: vec!["codex.5h".into()],
            remaining_cost_pct: 12.5,
            remaining_minutes: 25.0,
        }],
        vec![WaitingPoolTask {
            id: "t99".into(),
            title: "fix views".into(),
            kind: "fix".into(),
            priority: "blocker".into(),
            reason: "no headroom on codex.5h".into(),
        }],
    );
    assert!(
        row.stale,
        "sample older than fresh_min must be marked stale"
    );
    assert_eq!(row.committed_pct, 12.5);
    assert_eq!(row.active_leases, 1);
    assert_eq!(row.waiting_tasks.len(), 1);
    assert_eq!(row.band, ConfidenceBand::Low);

    // Verify ui/views/pools.js exports renderPools and renders all required 6.4 & 7.7 elements
    let pools_js_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/views/pools.js");
    let pools_js = fs::read_to_string(pools_js_path).unwrap();
    assert!(pools_js.contains("export function renderPools"));
    assert!(pools_js.contains("pool-bar-used"));
    assert!(pools_js.contains("pool-bar-committed"));
    assert!(pools_js.contains("pool-bar-line"));
    assert!(pools_js.contains("pool-bar-reserve"));
    assert!(pools_js.contains("waiting_tasks"));
    assert!(pools_js.contains("is-greyed"));

    let _ = fs::remove_dir_all(&dir);
}
