//! Performance metrics, baselines, and regression rules (step 10b).
//!
//! Delivers:
//! - `METRIC` and `BASELINE` tables (section 3)
//! - Deterministic regression check rule 4.5 (section 4.5):
//!   - `min_runs` check -> schedules `reference_run` tasks until `min_runs`
//!   - `worse_pct` and `noise_mads * MAD(base values)` check -> ignores noise inside the MAD guard,
//!     flags real drops, schedules `confirm_runs` `reference_run` tasks, raises `Alert(REGRESSION)`,
//!     and creates a `bisect` task (`range last_good..sha`, `priority blocker`, `class game_run`)
//!   - updates `BASELINE` when a commit is good
//! - `RegressModule` implementing `StoreModule` (`schema()`, `apply(tx, message)`, `api(state)`)
//! - TSV and JSON metric parsers for `metrics.tsv` (step 3) and `FACT` messages (`metric/<sha>/<machine_class>`)

pub mod rule;
pub mod schema;

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;
use crate::time::{parse_ms, rfc3339};

pub use rule::{
    Direction, RegressAlertSpec, RegressOutcome, RegressRule, RegressTaskSpec,
    alert_to_message_body, default_regress_rules, evaluate_metric_rule, mad, median,
    parse_regress_rules_from_value, task_to_message_body, update_baseline_with_good_commit,
    worse_ratio,
};
pub use schema::{
    BaselineRow, GoodCommit, MetricRow, REGRESS_SCHEMA_SQL, REGRESS_TABLES, baseline_to_json,
    metric_to_json, query_baseline, query_baselines, query_metric_values, query_metrics,
    upsert_baseline, upsert_baseline_tx, upsert_metric, upsert_metric_tx,
};

/// Numeric columns in `metrics.tsv` (step 3) that can be projected into `METRIC` rows.
pub const NUMERIC_TSV_METRICS: [&str; 6] = [
    "fps_median",
    "fps_p10",
    "first_present_s",
    "frames",
    "secs_alive",
    "peak_commit_gb",
];

/// Store projection module for `METRIC` and `BASELINE` (section 10.2).
pub struct RegressModule {
    pub rules: BTreeMap<String, RegressRule>,
}

impl Default for RegressModule {
    fn default() -> Self {
        Self::new()
    }
}

impl RegressModule {
    pub fn new() -> Self {
        Self {
            rules: default_regress_rules(),
        }
    }

    pub fn with_rules(rules: BTreeMap<String, RegressRule>) -> Self {
        Self { rules }
    }
}

impl StoreModule for RegressModule {
    fn name(&self) -> &'static str {
        "regress"
    }

    fn schema(&self) -> &'static str {
        REGRESS_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_regress_message(tx, msg, &self.rules)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_regress_api(conn, now_sec, &self.rules, state)
    }
}

/// Ensures the `METRIC` and `BASELINE` tables exist in `conn`.
pub fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(REGRESS_SCHEMA_SQL)
        .map_err(|e| format!("regress schema error: {e}"))
}

/// Applies a bus message (`FACT` with topic `metric/...`) to `METRIC` and updates `BASELINE`
/// inside an open SQLite transaction.
pub fn apply_regress_message(
    tx: &Transaction<'_>,
    msg: &Message,
    rules: &BTreeMap<String, RegressRule>,
) -> Result<(), String> {
    if msg.kind != Kind::Fact || !msg.topic.starts_with("metric/") {
        return Ok(());
    }
    let ts_sec = parse_ms(&msg.ts).unwrap_or(0) / 1000;
    let rows = parse_fact_metric_rows(&msg.topic, &msg.body, ts_sec, msg.id)?;
    let mut touched_groups = BTreeSet::new();
    for row in &rows {
        upsert_metric_tx(tx, row)?;
        touched_groups.insert((
            row.sha.clone(),
            row.machine_class.clone(),
            row.scenario.clone(),
            row.name.clone(),
        ));
    }

    for (sha, machine_class, scenario, name) in touched_groups {
        let Some(rule) = rules.get(&name) else {
            continue;
        };
        let outcome = evaluate_group_in_conn(
            tx,
            &sha,
            &machine_class,
            &scenario,
            &name,
            rule,
            false,
            msg.id,
        )?;
        if let RegressOutcome::BaselineGood { baseline, .. } = outcome {
            upsert_baseline_tx(tx, &baseline)?;
        }
    }
    Ok(())
}

/// Evaluates rule 4.5 for `(sha, machine_class, scenario, metric_name)` against the rows stored in `conn`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_group_in_conn(
    conn: &Connection,
    sha: &str,
    machine_class: &str,
    scenario: &str,
    metric_name: &str,
    rule: &RegressRule,
    require_confirm_before_alert: bool,
    msg_id: u64,
) -> Result<RegressOutcome, String> {
    let sha_values = query_metric_values(conn, sha, machine_class, scenario, metric_name)?;
    let baseline = query_baseline(conn, machine_class, scenario, metric_name)?;

    let mut base_all_values = Vec::new();
    let mut last_good_count = 0;

    if let Some(ref base) = baseline {
        let prior: Vec<&GoodCommit> = base.good_commits.iter().filter(|c| c.sha != sha).collect();
        let start = prior.len().saturating_sub(rule.baseline_commits.max(1));
        for gc in &prior[start..] {
            let vals = query_metric_values(conn, &gc.sha, machine_class, scenario, metric_name)?;
            base_all_values.extend(vals);
        }
        if let Some(last_gc) = prior.last() {
            last_good_count =
                query_metric_values(conn, &last_gc.sha, machine_class, scenario, metric_name)?
                    .len();
        }
    }

    Ok(evaluate_metric_rule(
        sha,
        machine_class,
        scenario,
        metric_name,
        rule,
        &sha_values,
        baseline.as_ref(),
        &base_all_values,
        last_good_count,
        require_confirm_before_alert,
        msg_id,
    ))
}

/// Ingests a batch of `MetricRow` items into `conn`, runs rule 4.5 on every affected
/// `(sha, machine_class, scenario, name)` in chronological commit order, updates `BASELINE`
/// for good commits, and returns the evaluation outcomes in deterministic order.
pub fn ingest_and_evaluate(
    conn: &Connection,
    rows: &[MetricRow],
    rules: &BTreeMap<String, RegressRule>,
    require_confirm_before_alert: bool,
) -> Result<Vec<RegressOutcome>, String> {
    init_schema(conn)?;
    let mut commit_order: Vec<(String, String, String)> = Vec::new();
    for r in rows {
        upsert_metric(conn, r)?;
        let key = (r.sha.clone(), r.machine_class.clone(), r.scenario.clone());
        if !commit_order.contains(&key) {
            commit_order.push(key);
        }
    }

    let mut outcomes = Vec::new();
    for (sha, machine_class, scenario) in commit_order {
        for (metric_name, rule) in rules {
            let vals = query_metric_values(conn, &sha, &machine_class, &scenario, metric_name)?;
            if vals.is_empty() {
                continue;
            }
            let outcome = evaluate_group_in_conn(
                conn,
                &sha,
                &machine_class,
                &scenario,
                metric_name,
                rule,
                require_confirm_before_alert,
                rows.last().map(|r| r.msg_id).unwrap_or(0),
            )?;
            if let RegressOutcome::BaselineGood { ref baseline, .. } = outcome {
                upsert_baseline(conn, baseline)?;
            }
            outcomes.push(outcome);
        }
    }
    Ok(outcomes)
}

/// Converts a list of `RegressOutcome` values into hub `Message` events (`ALERT` for `REGRESSION`
/// and `TASK_UPDATE` for `reference_run` and `bisect` tasks).
pub fn outcomes_to_messages(outcomes: &[RegressOutcome], now_ms: u64) -> Vec<Message> {
    let now_sec = now_ms / 1000;
    let ts = rfc3339(now_ms);
    let mut msgs = Vec::new();

    for outcome in outcomes {
        if let Some(alert) = outcome.alert() {
            msgs.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/{}/{}", alert.code, alert.key),
                body: alert_to_message_body(alert, now_sec).to_string(),
                refs: vec!["state:open".into()],
                to: vec![],
            });
        }
        for task in outcome.tasks() {
            msgs.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Task,
                topic: format!("task/{}", task.id),
                body: task_to_message_body(&task, now_sec).to_string(),
                refs: vec![
                    format!("kind:{}", task.kind),
                    format!("priority:{}", task.priority),
                    format!("class:{}", task.class),
                    format!("sha:{}", task.sha),
                ],
                to: vec![],
            });
        }
    }
    msgs
}

/// Populates the `metrics` and `baselines` keys in `/api/state`.
pub fn build_regress_api(
    conn: &Connection,
    _now_sec: u64,
    rules: &BTreeMap<String, RegressRule>,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    init_schema(conn)?;
    let metrics = query_metrics(conn)?;
    let baselines = query_baselines(conn)?;

    let metric_vals: Vec<Value> = metrics.iter().map(metric_to_json).collect();
    let baseline_vals: Vec<Value> = baselines.iter().map(baseline_to_json).collect();

    // Evaluate current status for each distinct (sha, machine_class, scenario)
    let mut seen_groups = Vec::new();
    for m in &metrics {
        let g = (m.sha.clone(), m.machine_class.clone(), m.scenario.clone());
        if !seen_groups.contains(&g) {
            seen_groups.push(g);
        }
    }
    let mut checks = Vec::new();
    for (sha, mc, sc) in &seen_groups {
        for (name, rule) in rules {
            let vals = query_metric_values(conn, sha, mc, sc, name)?;
            if vals.is_empty() {
                continue;
            }
            if let Ok(outcome) = evaluate_group_in_conn(conn, sha, mc, sc, name, rule, false, 0)
                && let Ok(v) = serde_json::to_value(&outcome)
            {
                checks.push(v);
            }
        }
    }

    state.insert("metrics".into(), Value::Array(metric_vals));
    state.insert("baselines".into(), Value::Array(baseline_vals));
    state.insert(
        "regress".into(),
        json!({
            "rules": rules,
            "checks": checks,
        }),
    );
    Ok(())
}

/// Parses one or more `MetricRow` items from a `FACT` message with topic `metric/<sha>/<machine_class>`.
///
/// Supports either:
/// - Single metric row JSON: `{"sha":"...","machine_class":"...","scenario":"...","run_id":"...","name":"fps_median","value":30.0,"at":1795600000}`
/// - Multi-metric object JSON: `{"sha":"...","machine_class":"...","scenario":"...","run_id":"...","metrics":{"fps_median":30.0,"fps_p10":28.5}}`
/// - Array of metric rows under `"rows"`
pub fn parse_fact_metric_rows(
    topic: &str,
    body: &str,
    default_at: u64,
    msg_id: u64,
) -> Result<Vec<MetricRow>, String> {
    let parts: Vec<&str> = topic.split('/').collect();
    let topic_sha = parts.get(1).copied().unwrap_or("");
    let topic_mc = parts.get(2).copied().unwrap_or("default");

    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("invalid metric FACT JSON: {e}"))?;

    if let Some(arr) = v.get("rows").and_then(Value::as_array) {
        let mut out = Vec::new();
        for item in arr {
            out.extend(parse_single_json_metric(
                item, topic_sha, topic_mc, default_at, msg_id,
            )?);
        }
        return Ok(out);
    }

    parse_single_json_metric(&v, topic_sha, topic_mc, default_at, msg_id)
}

fn parse_single_json_metric(
    v: &Value,
    topic_sha: &str,
    topic_mc: &str,
    default_at: u64,
    msg_id: u64,
) -> Result<Vec<MetricRow>, String> {
    let sha = v
        .get("sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "-")
        .unwrap_or(topic_sha)
        .to_owned();
    if sha.is_empty() {
        return Err("metric row is missing `sha`".into());
    }
    let machine_class = normalize_dim(
        v.get("machine_class")
            .and_then(Value::as_str)
            .unwrap_or(topic_mc),
    );
    let scenario = normalize_dim(
        v.get("scenario")
            .and_then(Value::as_str)
            .unwrap_or("default"),
    );
    let run_id = v
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("run-0")
        .to_owned();
    let at = v
        .get("at")
        .and_then(Value::as_u64)
        .or_else(|| {
            v.get("utc")
                .and_then(Value::as_str)
                .and_then(parse_ms)
                .map(|ms| ms / 1000)
        })
        .unwrap_or(default_at);

    let mut out = Vec::new();
    if let (Some(name), Some(val)) = (
        v.get("name").and_then(Value::as_str),
        v.get("value").and_then(Value::as_f64),
    ) {
        out.push(MetricRow {
            sha,
            machine_class,
            scenario,
            run_id,
            name: name.to_owned(),
            value: val,
            at,
            msg_id,
        });
        return Ok(out);
    }

    if let Some(metrics_obj) = v.get("metrics").and_then(Value::as_object) {
        for (k, val_v) in metrics_obj {
            if let Some(val) = val_v.as_f64() {
                out.push(MetricRow {
                    sha: sha.clone(),
                    machine_class: machine_class.clone(),
                    scenario: scenario.clone(),
                    run_id: run_id.clone(),
                    name: k.clone(),
                    value: val,
                    at,
                    msg_id,
                });
            }
        }
        return Ok(out);
    }

    for col in NUMERIC_TSV_METRICS {
        if let Some(val) = v.get(col).and_then(|x| {
            x.as_f64()
                .or_else(|| x.as_str().and_then(|s| s.parse::<f64>().ok()))
        }) {
            out.push(MetricRow {
                sha: sha.clone(),
                machine_class: machine_class.clone(),
                scenario: scenario.clone(),
                run_id: run_id.clone(),
                name: col.to_owned(),
                value: val,
                at,
                msg_id,
            });
        }
    }

    Ok(out)
}

fn normalize_dim(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() || t == "-" {
        "default".into()
    } else {
        t.to_owned()
    }
}

/// Parses `metrics.tsv` (step 3 format) or JSON Lines / JSON file content into `MetricRow` entries.
///
/// Step 3 `metrics.tsv` columns:
/// `utc, sha, machine_class, scenario, run_id, label, secs_alive, ended_by, device_removed, unhandled, first_present_s, frames, fps_median, fps_p10, peak_commit_gb, gentle, load_note`
pub fn parse_metrics_file(
    content: &str,
    default_at: u64,
    msg_id: u64,
) -> Result<Vec<MetricRow>, String> {
    let trimmed = content.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(arr) = v.as_array() {
                let mut rows = Vec::new();
                for item in arr {
                    rows.extend(parse_single_json_metric(
                        item, "", "default", default_at, msg_id,
                    )?);
                }
                return Ok(rows);
            }
            return parse_single_json_metric(&v, "", "default", default_at, msg_id);
        }
        let mut rows = Vec::new();
        for (idx, line) in trimmed.lines().enumerate() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let v: Value = serde_json::from_str(l)
                .map_err(|e| format!("line {}: invalid JSON: {e}", idx + 1))?;
            rows.extend(parse_single_json_metric(
                &v, "", "default", default_at, msg_id,
            )?);
        }
        return Ok(rows);
    }

    let mut rows = Vec::new();
    let mut header_map: Option<BTreeMap<String, usize>> = None;

    for (line_idx, raw_line) in trimmed.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if header_map.is_none() && (cols.first().copied() == Some("utc") || cols.contains(&"sha")) {
            let mut map = BTreeMap::new();
            for (i, &name) in cols.iter().enumerate() {
                map.insert(name.to_ascii_lowercase(), i);
            }
            header_map = Some(map);
            continue;
        }

        // Standard 17-column positional layout from step 3 metrics.tsv:
        // 0:utc 1:sha 2:machine_class 3:scenario 4:run_id 5:label 6:secs_alive 7:ended_by
        // 8:device_removed 9:unhandled 10:first_present_s 11:frames 12:fps_median 13:fps_p10
        // 14:peak_commit_gb 15:gentle 16:load_note
        //
        // Or compact 6/7-column fixture TSV:
        // sha, machine_class, scenario, run_id, name, value [, at]
        if header_map.is_none()
            && (6..=7).contains(&cols.len())
            && let Ok(val) = cols[5].parse::<f64>()
        {
            let sha = cols[0];
            if !sha.is_empty() && sha != "-" {
                let at = cols
                    .get(6)
                    .and_then(|s| {
                        s.parse::<u64>()
                            .ok()
                            .or_else(|| parse_ms(s).map(|ms| ms / 1000))
                    })
                    .unwrap_or(default_at);
                rows.push(MetricRow {
                    sha: sha.to_owned(),
                    machine_class: normalize_dim(cols[1]),
                    scenario: normalize_dim(cols[2]),
                    run_id: cols[3].to_owned(),
                    name: cols[4].to_owned(),
                    value: val,
                    at,
                    msg_id,
                });
                continue;
            }
        }

        let col_by = |name: &str, default_idx: usize| -> &str {
            let idx = header_map
                .as_ref()
                .and_then(|m| m.get(name).copied())
                .unwrap_or(default_idx);
            cols.get(idx).copied().unwrap_or("-")
        };

        let utc_s = col_by("utc", 0);
        let sha = col_by("sha", 1);
        if sha.is_empty() || sha == "-" {
            continue;
        }
        let machine_class = normalize_dim(col_by("machine_class", 2));
        let scenario = normalize_dim(col_by("scenario", 3));
        let run_id_raw = col_by("run_id", 4);
        let run_id = if run_id_raw.is_empty() || run_id_raw == "-" {
            format!("run-{}", line_idx + 1)
        } else {
            run_id_raw.to_owned()
        };
        let at = parse_ms(utc_s)
            .map(|ms| ms / 1000)
            .or_else(|| utc_s.parse::<u64>().ok())
            .unwrap_or(default_at);

        // If the TSV has explicit `name` and `value` columns, parse them directly
        if let Some(ref map) = header_map
            && let (Some(&name_i), Some(&val_i)) = (map.get("name"), map.get("value"))
            && let (Some(name_s), Some(val_s)) = (cols.get(name_i), cols.get(val_i))
            && let Ok(val) = val_s.parse::<f64>()
        {
            rows.push(MetricRow {
                sha: sha.to_owned(),
                machine_class,
                scenario,
                run_id,
                name: (*name_s).to_owned(),
                value: val,
                at,
                msg_id,
            });
            continue;
        }

        let metric_cols: [(&str, usize); 6] = [
            ("secs_alive", 6),
            ("first_present_s", 10),
            ("frames", 11),
            ("fps_median", 12),
            ("fps_p10", 13),
            ("peak_commit_gb", 14),
        ];

        for (m_name, def_idx) in metric_cols {
            let raw_val = col_by(m_name, def_idx);
            if raw_val != "-"
                && !raw_val.is_empty()
                && let Ok(val) = raw_val.parse::<f64>()
                && val.is_finite()
            {
                rows.push(MetricRow {
                    sha: sha.to_owned(),
                    machine_class: machine_class.clone(),
                    scenario: scenario.clone(),
                    run_id: run_id.clone(),
                    name: m_name.to_owned(),
                    value: val,
                    at,
                    msg_id,
                });
            }
        }
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rule() -> RegressRule {
        RegressRule {
            direction: Direction::HigherIsBetter,
            worse_pct: 10.0,
            min_runs: 3,
            confirm_runs: 2,
            baseline_commits: 5,
            noise_mads: 3.0,
        }
    }

    #[test]
    fn median_and_mad_compute_exact_values() {
        assert_eq!(median(&[30.0, 10.0, 20.0]), 20.0);
        assert_eq!(median(&[10.0, 20.0, 30.0, 40.0]), 25.0);
        // values: [58, 60, 62] -> median 60 -> deviations [2, 0, 2] -> MAD = 2
        assert_eq!(mad(&[58.0, 60.0, 62.0]), 2.0);
    }

    #[test]
    fn rule_asks_for_more_runs_below_min_runs() {
        let rule = sample_rule();
        let out = evaluate_metric_rule(
            "sha_c1",
            "win-rtx4090",
            "boot",
            "fps_median",
            &rule,
            &[60.0, 59.5],
            None,
            &[],
            0,
            false,
            1,
        );
        match out {
            RegressOutcome::NeedMoreRuns {
                have_runs,
                min_runs,
                missing_runs,
                tasks,
                ..
            } => {
                assert_eq!(have_runs, 2);
                assert_eq!(min_runs, 3);
                assert_eq!(missing_runs, 1);
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].kind, "reference_run");
                assert_eq!(tasks[0].class, "game_run");
                assert_eq!(tasks[0].sha, "sha_c1");
            }
            other => panic!("expected NeedMoreRuns, got {other:?}"),
        }
    }

    #[test]
    fn rule_ignores_noise_inside_mad_guard() {
        let rule = sample_rule();
        // Base median = 60.0, base values have MAD = 3.0 (e.g. [57, 60, 63]).
        // Noise guard = 3 * MAD = 9.0.
        // Candidate median = 52.0 -> drop is 13.33% (> 10% worse_pct),
        // but |52.0 - 60.0| = 8.0 <= 9.0 (inside MAD guard) -> must be ignored as noise!
        let base_row = BaselineRow {
            machine_class: "win-rtx4090".into(),
            scenario: "boot".into(),
            name: "fps_median".into(),
            median: 60.0,
            n: 3,
            from_sha: "sha_g1".into(),
            to_sha: "sha_g3".into(),
            good_commits: vec![
                GoodCommit {
                    sha: "sha_g1".into(),
                    median: 57.0,
                },
                GoodCommit {
                    sha: "sha_g2".into(),
                    median: 60.0,
                },
                GoodCommit {
                    sha: "sha_g3".into(),
                    median: 63.0,
                },
            ],
            msg_id: 3,
        };
        let base_raw = vec![57.0, 57.0, 57.0, 60.0, 60.0, 60.0, 63.0, 63.0, 63.0];

        let out = evaluate_metric_rule(
            "sha_noisy",
            "win-rtx4090",
            "boot",
            "fps_median",
            &rule,
            &[51.5, 52.0, 52.5],
            Some(&base_row),
            &base_raw,
            3,
            false,
            4,
        );
        match out {
            RegressOutcome::BaselineGood {
                ignored_as_noise,
                baseline,
                ..
            } => {
                assert!(
                    ignored_as_noise,
                    "drop inside 3*MAD must be marked as ignored noise"
                );
                assert_eq!(baseline.to_sha, "sha_noisy");
                assert_eq!(baseline.n, 4);
            }
            other => panic!("expected BaselineGood (noise ignored), got {other:?}"),
        }
    }

    #[test]
    fn rule_flags_real_drop_and_creates_confirm_and_bisect_tasks() {
        let rule = sample_rule();
        // Base median = 60.0, MAD = 1.0 -> 3 * MAD = 3.0.
        // Candidate median = 45.0 -> 25% drop (> 10%) and |45 - 60| = 15.0 > 3.0 -> real regression!
        let base_row = BaselineRow {
            machine_class: "win-rtx4090".into(),
            scenario: "boot".into(),
            name: "fps_median".into(),
            median: 60.0,
            n: 3,
            from_sha: "sha_g1".into(),
            to_sha: "sha_g3".into(),
            good_commits: vec![
                GoodCommit {
                    sha: "sha_g1".into(),
                    median: 59.0,
                },
                GoodCommit {
                    sha: "sha_g2".into(),
                    median: 60.0,
                },
                GoodCommit {
                    sha: "sha_g3".into(),
                    median: 61.0,
                },
            ],
            msg_id: 3,
        };
        let base_raw = vec![59.0, 60.0, 61.0];

        let out = evaluate_metric_rule(
            "sha_bad",
            "win-rtx4090",
            "boot",
            "fps_median",
            &rule,
            &[44.5, 45.0, 45.5],
            Some(&base_row),
            &base_raw,
            3,
            false,
            5,
        );
        match out {
            RegressOutcome::RegressionConfirmed {
                alert,
                bisect_task,
                confirm_tasks,
                worse_pct,
                ..
            } => {
                assert!((worse_pct - 25.0).abs() < 1e-6);
                assert_eq!(alert.code, "REGRESSION");
                assert_eq!(alert.sha, "sha_bad");
                assert_eq!(alert.last_good_sha, "sha_g3");
                assert_eq!(bisect_task.kind, "bisect");
                assert_eq!(bisect_task.priority, "blocker");
                assert_eq!(bisect_task.class, "game_run");
                assert_eq!(bisect_task.range.as_deref(), Some("sha_g3..sha_bad"));
                // confirm_runs = 2 at sha_bad and 2 at sha_g3 = 4 reference_run tasks with priority high
                assert_eq!(confirm_tasks.len(), 4);
                assert!(
                    confirm_tasks
                        .iter()
                        .all(|t| t.kind == "reference_run" && t.priority == "high")
                );
            }
            other => panic!("expected RegressionConfirmed, got {other:?}"),
        }
    }

    const SYNTH_FIXTURE_TSV: &str = "\
utc\tsha\tmachine_class\tscenario\trun_id\tlabel\tsecs_alive\tended_by\tdevice_removed\tunhandled\tfirst_present_s\tframes\tfps_median\tfps_p10\tpeak_commit_gb\tgentle\tload_note
2026-11-25T10:00:00Z\tsha_g1\twin-rtx4090\tboot\tr1\tb1\t60\ttimeout\t0\t0\t2.0\t1800\t57.00\t54.00\t4.1\t0\t-
2026-11-25T10:01:00Z\tsha_g1\twin-rtx4090\tboot\tr2\tb2\t60\ttimeout\t0\t0\t2.0\t1800\t57.00\t54.00\t4.1\t0\t-
2026-11-25T10:02:00Z\tsha_g1\twin-rtx4090\tboot\tr3\tb3\t60\ttimeout\t0\t0\t2.0\t1800\t57.00\t54.00\t4.1\t0\t-
2026-11-25T10:03:00Z\tsha_g2\twin-rtx4090\tboot\tr4\tb4\t60\ttimeout\t0\t0\t2.0\t1800\t60.00\t57.00\t4.1\t0\t-
2026-11-25T10:04:00Z\tsha_g2\twin-rtx4090\tboot\tr5\tb5\t60\ttimeout\t0\t0\t2.0\t1800\t60.00\t57.00\t4.1\t0\t-
2026-11-25T10:05:00Z\tsha_g2\twin-rtx4090\tboot\tr6\tb6\t60\ttimeout\t0\t0\t2.0\t1800\t60.00\t57.00\t4.1\t0\t-
2026-11-25T10:06:00Z\tsha_g3\twin-rtx4090\tboot\tr7\tb7\t60\ttimeout\t0\t0\t2.0\t1800\t63.00\t60.00\t4.1\t0\t-
2026-11-25T10:07:00Z\tsha_g3\twin-rtx4090\tboot\tr8\tb8\t60\ttimeout\t0\t0\t2.0\t1800\t63.00\t60.00\t4.1\t0\t-
2026-11-25T10:08:00Z\tsha_g3\twin-rtx4090\tboot\tr9\tb9\t60\ttimeout\t0\t0\t2.0\t1800\t63.00\t60.00\t4.1\t0\t-
2026-11-25T10:09:00Z\tsha_under\twin-rtx4090\tboot\tr10\tb10\t60\ttimeout\t0\t0\t2.0\t1800\t40.00\t38.00\t4.1\t0\t-
2026-11-25T10:10:00Z\tsha_noise\twin-rtx4090\tboot\tr11\tb11\t60\ttimeout\t0\t0\t2.0\t1800\t52.00\t50.00\t4.1\t0\t-
2026-11-25T10:11:00Z\tsha_noise\twin-rtx4090\tboot\tr12\tb12\t60\ttimeout\t0\t0\t2.0\t1800\t52.00\t50.00\t4.1\t0\t-
2026-11-25T10:12:00Z\tsha_noise\twin-rtx4090\tboot\tr13\tb13\t60\ttimeout\t0\t0\t2.0\t1800\t52.00\t50.00\t4.1\t0\t-
2026-11-25T10:13:00Z\tsha_drop\twin-rtx4090\tboot\tr14\tb14\t60\ttimeout\t0\t0\t2.0\t1800\t45.00\t42.00\t4.1\t0\t-
2026-11-25T10:14:00Z\tsha_drop\twin-rtx4090\tboot\tr15\tb15\t60\ttimeout\t0\t0\t2.0\t1800\t45.00\t42.00\t4.1\t0\t-
2026-11-25T10:15:00Z\tsha_drop\twin-rtx4090\tboot\tr16\tb16\t60\ttimeout\t0\t0\t2.0\t1800\t45.00\t42.00\t4.1\t0\t-
";

    #[test]
    fn fixture_tsv_end_to_end_flags_real_drop_ignores_noise_and_asks_for_more_runs() {
        let rows = parse_metrics_file(SYNTH_FIXTURE_TSV, 1795600000, 1).unwrap();
        assert!(!rows.is_empty());

        let conn = Connection::open_in_memory().unwrap();
        let rules = default_regress_rules();
        let outcomes = ingest_and_evaluate(&conn, &rows, &rules, false).unwrap();

        let find_sha = |s: &str| -> &RegressOutcome {
            outcomes
                .iter()
                .find(|o| match o {
                    RegressOutcome::NeedMoreRuns { sha, .. }
                    | RegressOutcome::ConfirmRunsNeeded { sha, .. }
                    | RegressOutcome::RegressionConfirmed { sha, .. }
                    | RegressOutcome::BaselineGood { sha, .. } => sha == s,
                })
                .unwrap_or_else(|| panic!("missing outcome for {s}"))
        };

        // 1. sha_g1 (57), sha_g2 (60), sha_g3 (63) build the baseline (median = 60.0, MAD = 3.0, noise guard = 3 * 3.0 = 9.0)
        assert!(matches!(
            find_sha("sha_g1"),
            RegressOutcome::BaselineGood { .. }
        ));
        assert!(matches!(
            find_sha("sha_g2"),
            RegressOutcome::BaselineGood { .. }
        ));
        assert!(matches!(
            find_sha("sha_g3"),
            RegressOutcome::BaselineGood { .. }
        ));

        // 2. sha_under has 1 run (< min_runs = 3): asks for 2 more reference_run tasks
        match find_sha("sha_under") {
            RegressOutcome::NeedMoreRuns {
                have_runs,
                min_runs,
                missing_runs,
                tasks,
                ..
            } => {
                assert_eq!(*have_runs, 1);
                assert_eq!(*min_runs, 3);
                assert_eq!(*missing_runs, 2);
                assert_eq!(tasks.len(), 2);
                assert!(tasks.iter().all(|t| t.kind == "reference_run"));
            }
            other => panic!("expected NeedMoreRuns for sha_under, got {other:?}"),
        }

        // 3. sha_noise has 3 runs at 52.00 fps:
        //    drop = (60.0 - 52.0) / 60.0 = 13.33% (> 10.0% worse_pct),
        //    but |52.0 - 60.0| = 8.0 <= 9.0 (3 * MAD) -> ignored as noise!
        match find_sha("sha_noise") {
            RegressOutcome::BaselineGood {
                ignored_as_noise,
                worse_pct,
                mad,
                baseline,
                ..
            } => {
                assert!(*ignored_as_noise, "must ignore drop inside MAD noise guard");
                assert!(*worse_pct > 10.0);
                assert_eq!(*mad, 3.0);
                assert_eq!(baseline.to_sha, "sha_noise");
            }
            other => panic!("expected BaselineGood (noise) for sha_noise, got {other:?}"),
        }

        // 4. sha_drop has 3 runs at 45.00 fps:
        //    drop exceeds 10% and exceeds 3 * MAD -> flags real drop, creates confirm reference_run tasks,
        //    raises REGRESSION alert, and creates bisect task!
        match find_sha("sha_drop") {
            RegressOutcome::RegressionConfirmed {
                alert,
                bisect_task,
                confirm_tasks,
                ..
            } => {
                assert_eq!(alert.code, "REGRESSION");
                assert_eq!(alert.sha, "sha_drop");
                assert_eq!(alert.last_good_sha, "sha_noise");
                assert_eq!(bisect_task.kind, "bisect");
                assert_eq!(bisect_task.priority, "blocker");
                assert_eq!(bisect_task.class, "game_run");
                assert_eq!(bisect_task.range.as_deref(), Some("sha_noise..sha_drop"));
                assert_eq!(confirm_tasks.len(), 4);
                assert!(
                    confirm_tasks
                        .iter()
                        .all(|t| t.kind == "reference_run" && t.priority == "high")
                );
            }
            other => panic!("expected RegressionConfirmed for sha_drop, got {other:?}"),
        }
    }

    #[test]
    fn store_module_replays_metric_facts_and_serves_api_state() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        store
            .register_module(Box::new(RegressModule::new()))
            .unwrap();

        let mut msgs = Vec::new();
        let mut id = 1;
        for (sha, fps) in [("c1", 57.0), ("c2", 60.0), ("c3", 63.0)] {
            for r in 1..=3 {
                msgs.push(Message {
                    id,
                    ts: format!("2026-11-25T10:00:{id:02}.000Z"),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Fact,
                    topic: format!("metric/{sha}/win-rtx4090"),
                    body: json!({
                        "v": 1,
                        "sha": sha,
                        "machine_class": "win-rtx4090",
                        "scenario": "boot",
                        "run_id": format!("run-{sha}-{r}"),
                        "name": "fps_median",
                        "value": fps,
                        "at": 1795600000 + id,
                    })
                    .to_string(),
                    refs: vec![],
                    to: vec![],
                });
                id += 1;
            }
        }

        store.apply_batch(&msgs).unwrap();
        let api = store.api_state(1795601000).unwrap();
        assert_eq!(api["metrics"].as_array().unwrap().len(), 9);
        let baselines = api["baselines"].as_array().unwrap();
        assert_eq!(baselines.len(), 1);
        assert_eq!(baselines[0]["name"], "fps_median");
        assert_eq!(baselines[0]["median"], 60.0);
        assert_eq!(baselines[0]["n"], 3);
        assert_eq!(baselines[0]["from_sha"], "c1");
        assert_eq!(baselines[0]["to_sha"], "c3");
    }

    #[tokio::test]
    async fn cli_metric_add_and_compare_work_on_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let tsv_path = dir.path().join("metrics.tsv");
        std::fs::write(&tsv_path, SYNTH_FIXTURE_TSV).unwrap();

        let data_arg = dir.path().to_string_lossy().into_owned();
        let file_arg = tsv_path.to_string_lossy().into_owned();

        let code = crate::cli::metric::run(&[
            "add".into(),
            file_arg.clone(),
            "--data".into(),
            data_arg.clone(),
            "--json".into(),
        ])
        .await
        .unwrap();
        assert_eq!(code, std::process::ExitCode::SUCCESS);

        let code = crate::cli::metric::run(&[
            "compare".into(),
            "sha_drop".into(),
            "--data".into(),
            data_arg,
            "--json".into(),
        ])
        .await
        .unwrap();
        assert_eq!(code, std::process::ExitCode::SUCCESS);
    }
}
