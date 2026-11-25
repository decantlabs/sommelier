//! Integration and table tests for sommelier quota meters, parsers, forecast, and admission (step 5).

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use sommelier::quota::admit::{AdmitDecision, SlotAdmitInput, TaskAdmitInput, admit};
use sommelier::quota::calibrate::{
    calibrate_codex_from_rollout, calibrate_gemini_on_limit, parse_launcher_log_limits,
    parse_limited_tsv, parse_quotas_tsv,
};
use sommelier::quota::claude::{
    parse_result_spend, parse_statusline, parse_stream_rate_limit_event,
};
use sommelier::quota::codex::{parse_appserver_response, parse_rollout_line};
use sommelier::quota::estimate::{Estimate, EstimateSource, estimate};
use sommelier::quota::forecast::{ActiveLease, forecast, on_sample, pool_state};
use sommelier::quota::gemini::{
    SEED_CAPACITY_UNITS, apply_limit_hit, inferred_sample, parse_agy_json_output,
    parse_limit_message, parse_reset_duration,
};
use sommelier::quota::meter::{
    ConfidenceBand, Meter, PoolState, QuotaConfig, QuotaSample, Source, TokenWeights,
    contains_sensitive_text,
};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/quota")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

fn assert_clean(label: &str, text: &str) {
    assert!(
        !contains_sensitive_text(text),
        "{label} must not contain accountId, @, or token-like strings: {text}"
    );
}

#[test]
fn codex_rollout_and_appserver_parsers_drop_account_id() {
    let weights = TokenWeights::default();
    let rollout = fixture("codex_rollout.txt");
    let parsed: Vec<_> = rollout
        .lines()
        .filter_map(|l| parse_rollout_line(l, "owner1", &weights))
        .collect();
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].samples.len(), 2);
    assert_eq!(parsed[0].samples[0].pool, "codex.5h");
    assert_eq!(parsed[0].samples[0].used_pct, 10.0);
    assert_eq!(parsed[2].samples.len(), 1); // missing secondary tolerated
    assert!(parsed[0].spend.is_some());

    let app_raw = fixture("codex_appserver.json");
    assert!(app_raw.contains("accountId"));
    let app_samples = parse_appserver_response(&app_raw, "owner1", 1_795_605_600).unwrap();
    assert_eq!(app_samples.len(), 2);
    assert_eq!(app_samples[0].pool, "codex.5h");
    assert_eq!(app_samples[0].used_pct, 82.0);
    assert_eq!(app_samples[1].pool, "codex.week");
    assert_eq!(app_samples[1].used_pct, 43.0);

    let serialized = serde_json::to_string(&app_samples).unwrap();
    assert_clean("codex_appserver_samples", &serialized);
}

#[test]
fn claude_parsers_handle_statusline_stream_and_result() {
    let raw = fixture("claude_statusline.json");
    let r1 = parse_statusline(&raw, "owner1", 1_795_600_000, None).unwrap();
    assert!(r1.fresh);
    assert_eq!(r1.state.samples.len(), 2);
    assert_eq!(r1.bar_line, "claude 5h:42% week:18%");
    assert_clean(
        "claude_statusline",
        &serde_json::to_string(&r1.state).unwrap(),
    );

    // Same total_api_duration_ms -> not fresh, keeps previous observed_at.
    let r2 = parse_statusline(&raw, "owner1", 1_795_601_000, Some(&r1.state)).unwrap();
    assert!(!r2.fresh);
    assert_eq!(r2.state.last_observed_at, 1_795_600_000);

    // Stream events
    let stream = fixture("claude_stream.txt");
    let lines: Vec<_> = stream.lines().collect();
    let e0 = parse_stream_rate_limit_event(lines[0], "owner1", 1_795_600_000).unwrap();
    assert_eq!(e0.status, "allowed_warning");
    assert!((e0.sample.unwrap().used_pct - 78.0).abs() < 1e-6);

    let e1 = parse_stream_rate_limit_event(lines[1], "owner1", 1_795_600_000).unwrap();
    assert_eq!(e1.status, "rejected");
    assert!(e1.state_event.is_some());

    // Result JSON spend
    let res_raw = fixture("claude_result.json");
    let spend =
        parse_result_spend(&res_raw, "owner1", 1_795_600_000, &TokenWeights::default()).unwrap();
    assert_eq!(spend.len(), 1);
    assert_eq!(spend[0].model.as_deref(), Some("claude-sonnet"));
    assert_eq!(spend[0].output_tokens, 500);
}

#[test]
fn gemini_parsers_and_pool_pause_rules() {
    let hit_txt = fixture("agy_limit.txt");
    assert_eq!(parse_reset_duration(&hit_txt), Some(2 * 3600 + 4 * 60 + 47));
    let hit = parse_limit_message(&hit_txt, 1_000_000, Some("carbon")).unwrap();
    assert!(!hit.is_weekly);
    assert_eq!(hit.target_pool, "agy.window");
    assert_eq!(hit.resets_at, 1_000_000 + 7487);

    let weekly_txt = fixture("agy_limit_weekly.txt");
    let weekly_hit = parse_limit_message(&weekly_txt, 1_000_000, Some("carbon")).unwrap();
    assert!(weekly_hit.is_weekly);
    assert_eq!(weekly_hit.target_pool, "agy.week");

    // Second family hit within 10 minutes pauses whole pool.
    let mut win_m = Meter::new("owner1", "gemini", "agy.window", "5h");
    let mut wk_m = Meter::new("owner1", "gemini", "agy.week", "week");
    let mut fam_m = Meter::new("owner1", "gemini", "agy.family.hydrogen", "5h");
    apply_limit_hit(&mut win_m, &mut wk_m, Some(&mut fam_m), &hit, Some(999_700));
    assert!(fam_m.state_event.is_some());
    assert!(win_m.state_event.is_some());

    // Source C (agy --print --output-format json)
    let agy_out = fixture("agy_print.json");
    let spend =
        parse_agy_json_output(&agy_out, "owner1", 1_000_000, &TokenWeights::default()).unwrap();
    assert_eq!(spend.input_tokens, 11954);
    assert_eq!(spend.output_tokens, 1212);

    // Inferred sample
    let inf = inferred_sample(
        "owner1",
        "agy.window",
        "5h",
        1500.0,
        SEED_CAPACITY_UNITS,
        1_018_000,
        1_000_000,
        0,
    );
    assert!((inf.used_pct - 50.0).abs() < 1e-6);
    assert_eq!(inf.source, Source::Inferred);
}

#[test]
fn calibration_codex_and_gemini() {
    let rollout = fixture("codex_rollout.txt");
    let slope =
        calibrate_codex_from_rollout(&rollout, "owner1", "codex.5h", &TokenWeights::default())
            .unwrap();
    assert!(slope > 0.0);

    // Gemini: first 3 calibrations take min, after 3 take 0.5*cap + 0.5*obs.
    let (c1, n1) = calibrate_gemini_on_limit(3000.0, 0, 2600.0);
    assert_eq!((c1, n1), (2600.0, 1));
    let (c4, n4) = calibrate_gemini_on_limit(2400.0, 3, 2800.0);
    assert_eq!((c4, n4), (2600.0, 4));

    // History parsers
    let log = "2026-11-25 02:44:06 LIMIT task-a hit a quota message, no new start until 07:45:05\n";
    assert_eq!(parse_launcher_log_limits(log).len(), 1);
    let tsv = "utc\ttask\tused\n2026-11-25T02:00:00Z\ttask-a\t50\n";
    assert_eq!(parse_quotas_tsv(tsv).len(), 1);
    assert_eq!(parse_limited_tsv(tsv).len(), 1);
}

fn make_codex_meter(now: u64, used_pcts: &[(u64, f64)], resets_at: u64) -> Meter {
    let mut m = Meter::new("owner1", "codex", "codex.5h", "5h");
    m.pct_per_unit = 0.00001; // 1M units = 10%
    for (t, pct) in used_pcts {
        on_sample(
            &mut m,
            QuotaSample {
                owner: "owner1".to_owned(),
                vendor: "codex".to_owned(),
                pool: "codex.5h".to_owned(),
                window: "5h".to_owned(),
                observed_at: *t,
                used_pct: *pct,
                resets_at,
                source: Source::Api,
                confidence: 0.9,
            },
        );
    }
    let _ = now;
    m
}

fn base_task(checkpointable: bool) -> TaskAdmitInput {
    TaskAdmitInput {
        id: "t1".to_owned(),
        kind: "fix".to_owned(),
        size: 1.0,
        checkpointable,
        exclude_pools_until: BTreeMap::new(),
    }
}

fn base_slot() -> SlotAdmitInput {
    SlotAdmitInput {
        class: "codex".to_owned(),
        pools: vec!["codex.5h".to_owned()],
        family_pool: None,
    }
}

#[test]
fn forecast_and_admit_table_tests() {
    let cfg = QuotaConfig::default();
    let now = 1_000_000_u64;
    let est = Estimate {
        kind: "fix".to_owned(),
        class: "codex".to_owned(),
        n: 10,
        minutes_p50: 15.0,
        minutes_p90: 30.0,
        units_p50: 500_000.0,
        units_p90: 1_000_000.0, // 10% base cost at pct_per_unit=1e-5
        source: EstimateSource::Learned,
    };

    // 1. Headroom OK: used=30%, line=90%, need=10%*1.2=12% <= 60%, resets in 5h
    let m_ok = make_codex_meter(
        now,
        &[(now - 600, 29.0), (now - 300, 29.5), (now, 30.0)],
        now + 18_000,
    );
    let mut meters = BTreeMap::new();
    meters.insert("codex.5h".to_owned(), m_ok.clone());
    assert_eq!(
        admit(
            &base_task(true),
            &base_slot(),
            &meters,
            &est,
            &cfg,
            &BTreeMap::new(),
            &BTreeMap::new(),
            now
        ),
        AdmitDecision::Ok
    );

    // 2. Limit before the expected end: burn = 1%/min (30% -> 90% in 60m), task end = 36m?
    // Make burn = 2%/min: 50% -> 90% in 20m < end (30*1.2 = 36m)
    let m_limit = make_codex_meter(
        now,
        &[(now - 600, 30.0), (now - 300, 40.0), (now, 50.0)],
        now + 18_000,
    );
    meters.insert("codex.5h".to_owned(), m_limit);
    let dec = admit(
        &base_task(true),
        &base_slot(),
        &meters,
        &est,
        &cfg,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    );
    assert!(
        matches!(dec, AdmitDecision::Deny(ref r) if r.contains("limit before the expected end"))
    );

    // 3. Reset first: low burn 0.1%/min, used=80%, line=90% (100m to line), resets in 40m -> resets_first = true
    let m_reset = make_codex_meter(
        now,
        &[(now - 600, 79.0), (now - 300, 79.5), (now, 80.0)],
        now + 2400,
    );
    let f_reset = forecast(&m_reset, now, &[]);
    assert!(f_reset.resets_first);
    assert_eq!(f_reset.t_line, None);
    // Reset bridge: need = 12%, head = 10%, resets in 35m >= est.minutes_p90 (30m) and < end (36m), checkpointable=true -> Ok!
    let m_bridge = make_codex_meter(
        now,
        &[(now - 600, 79.0), (now - 300, 79.5), (now, 80.0)],
        now + 2000, // 33.3m (< 36m end, >= 30m p90)
    );
    meters.insert("codex.5h".to_owned(), m_bridge);
    assert_eq!(
        admit(
            &base_task(true),
            &base_slot(),
            &meters,
            &est,
            &cfg,
            &BTreeMap::new(),
            &BTreeMap::new(),
            now
        ),
        AdmitDecision::Ok
    );
    // Non-checkpointable cannot bridge reset -> no headroom
    let dec_no_cp = admit(
        &base_task(false),
        &base_slot(),
        &meters,
        &est,
        &cfg,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    );
    assert!(matches!(dec_no_cp, AdmitDecision::Deny(ref r) if r.contains("no headroom")));

    // 4. Drain: within drain_band_pct (5%), non-checkpointable >10m is denied
    let mut m_drain = make_codex_meter(now, &[(now, 86.0)], now + 18_000);
    m_drain.pct_per_unit = 0.000001; // tiny cost so headroom passes
    meters.insert("codex.5h".to_owned(), m_drain.clone());
    assert_eq!(
        pool_state(&m_drain, &forecast(&m_drain, now, &[]), now, &cfg),
        PoolState::Drain
    );
    let dec_drain = admit(
        &base_task(false),
        &base_slot(),
        &meters,
        &est,
        &cfg,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    );
    assert!(matches!(dec_drain, AdmitDecision::Deny(ref r) if r.contains("drain on")));

    // 5. Reserve: stop_at=95, reserve_pct=20 -> worker line = 80; at 80% pool is Closed
    let mut m_res = make_codex_meter(now, &[(now, 80.0)], now + 18_000);
    m_res.policy.stop_at = 95.0;
    m_res.policy.reserve_pct = 20.0;
    let f_res = forecast(&m_res, now, &[]);
    assert_eq!(f_res.line, 80.0);
    assert_eq!(pool_state(&m_res, &f_res, now, &cfg), PoolState::Closed);

    // 6. Low confidence margin: inferred sample (conf 0.4 * 0.5 = 0.2 -> Low -> margin 2.0)
    // head = 15%, base need = 10%: high conf (12%) would pass, low conf (20%) denies!
    let mut m_low = Meter::new("owner1", "codex", "codex.5h", "5h");
    m_low.pct_per_unit = 0.00001;
    on_sample(
        &mut m_low,
        QuotaSample {
            owner: "owner1".to_owned(),
            vendor: "codex".to_owned(),
            pool: "codex.5h".to_owned(),
            window: "5h".to_owned(),
            observed_at: now,
            used_pct: 75.0,
            resets_at: now + 18_000,
            source: Source::Inferred,
            confidence: 0.2,
        },
    );
    assert_eq!(forecast(&m_low, now, &[]).band, ConfidenceBand::Low);
    meters.insert("codex.5h".to_owned(), m_low);
    let dec_low = admit(
        &base_task(true),
        &base_slot(),
        &meters,
        &est,
        &cfg,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    );
    assert!(matches!(dec_low, AdmitDecision::Deny(ref r) if r.contains("no headroom")));

    // 7. Paused
    let mut m_paused = make_codex_meter(now, &[(now, 10.0)], now + 18_000);
    m_paused.state_event = Some(sommelier::quota::meter::StateEvent {
        resets_at: now + 3600,
        observed_at: now,
        message: "paused".to_owned(),
        family: None,
    });
    meters.insert("codex.5h".to_owned(), m_paused);
    let dec_paused = admit(
        &base_task(true),
        &base_slot(),
        &meters,
        &est,
        &cfg,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    );
    assert!(matches!(dec_paused, AdmitDecision::Deny(ref r) if r.contains("paused until")));

    // 8. Active lease burn_plan + estimate scaling
    let scaled = estimate("fix", "codex", Some(&est), &est, 2.0);
    assert_eq!(scaled.minutes_p90, 60.0);
    let leases = vec![ActiveLease {
        pools: vec!["codex.5h".to_owned()],
        remaining_cost_pct: 15.0,
        remaining_minutes: 30.0,
    }];
    let f_plan = forecast(&m_ok, now, &leases);
    assert!((f_plan.burn_plan - 0.5).abs() < 1e-6);
}

#[test]
fn cli_quota_end_to_end_and_no_sensitive_output() {
    let dir = std::env::temp_dir().join(format!("sommelier-quota-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data_str = dir.to_str().unwrap();

    // 1. ingest claude-statusline
    let status_json = fixture("claude_statusline.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args(["quota", "ingest", "claude-statusline", "--data", data_str])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(status_json.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bar = String::from_utf8_lossy(&out.stdout);
    assert!(bar.contains("claude 5h:42% week:18%"), "{bar}");
    assert_clean("ingest_stdout", &bar);

    // 2. set manual pool
    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args([
            "quota", "set", "codex.5h", "5h", "55", "--resets", "2h", "--data", data_str, "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_clean("set_stdout", &String::from_utf8_lossy(&out.stdout));

    // 3. hit gemini limit -> exit code 5
    let hit_file =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/quota/agy_limit.txt");
    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args([
            "quota",
            "hit",
            "--vendor",
            "gemini",
            "--text-file",
            hit_file.to_str().unwrap(),
            "--family",
            "carbon",
            "--data",
            data_str,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    assert_clean("hit_stdout", &String::from_utf8_lossy(&out.stdout));

    // 4. show --offline --json
    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args(["quota", "show", "--offline", "--json", "--data", data_str])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let show_json = String::from_utf8_lossy(&out.stdout);
    assert!(show_json.contains("claude.5h"));
    assert!(show_json.contains("codex.5h"));
    assert!(show_json.contains("agy.window"));
    assert_clean("show_json", &show_json);

    // 5. meters.json on disk is also clean
    let meters_disk = std::fs::read_to_string(dir.join("quota/meters.json")).unwrap();
    assert_clean("meters_json_file", &meters_disk);

    let _ = std::fs::remove_dir_all(&dir);
}
