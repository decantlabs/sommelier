//! Deterministic regression rule 4.5, median and MAD statistics, and task/alert generation (step 10b).
//!
//! Section 4.5 algorithm:
//! ```text
//! on new METRIC rows for (sha, machine_class, scenario)
//!   for each metric name with a rule r in policy.regress:
//!      if count(runs at sha) < r.min_runs: schedule reference_run tasks until min_runs, return
//!      new  = median(values at sha)
//!      base = BASELINE median over the last r.baseline_commits good commits
//!      worse = r.direction == "higher_is_better" ? (base - new) / base : (new - base) / base
//!      if worse * 100 > r.worse_pct and abs(new - base) > r.noise_mads * MAD(base values):
//!           schedule r.confirm_runs reference runs at sha and at the last good sha       # priority high
//!           if confirmed: Alert(REGRESSION, sha, name, base, new)
//!                         TaskAdd(kind bisect, range last_good..sha, priority blocker, class game_run, machine_class)
//!      else: BASELINE takes sha as good
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::schema::{BaselineRow, GoodCommit};

/// Direction of a tracked performance metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    HigherIsBetter,
    LowerIsBetter,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::HigherIsBetter => "higher_is_better",
            Direction::LowerIsBetter => "lower_is_better",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "higher_is_better" | "higher" => Some(Direction::HigherIsBetter),
            "lower_is_better" | "lower" => Some(Direction::LowerIsBetter),
            _ => None,
        }
    }
}

/// Regression rule configuration for one metric name (`[regress.<name>]` in section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegressRule {
    pub direction: Direction,
    pub worse_pct: f64,
    pub min_runs: usize,
    pub confirm_runs: usize,
    pub baseline_commits: usize,
    pub noise_mads: f64,
}

impl Default for RegressRule {
    fn default() -> Self {
        Self {
            direction: Direction::HigherIsBetter,
            worse_pct: 10.0,
            min_runs: 3,
            confirm_runs: 2,
            baseline_commits: 5,
            noise_mads: 3.0,
        }
    }
}

/// Returns the default policy regression rules (section 5.3 `[regress.fps_median]`, plus optional metrics).
pub fn default_regress_rules() -> BTreeMap<String, RegressRule> {
    let mut map = BTreeMap::new();
    map.insert("fps_median".into(), RegressRule::default());
    map
}

/// Parses `[regress.<name>]` rules from a TOML or JSON policy value, falling back to defaults.
pub fn parse_regress_rules_from_value(v: &Value) -> BTreeMap<String, RegressRule> {
    let mut rules = default_regress_rules();
    let Some(regress_obj) = v.get("regress").and_then(Value::as_object) else {
        return rules;
    };
    for (name, cfg) in regress_obj {
        let dir = cfg
            .get("direction")
            .and_then(Value::as_str)
            .and_then(Direction::parse)
            .unwrap_or(Direction::HigherIsBetter);
        let worse_pct = cfg.get("worse_pct").and_then(Value::as_f64).unwrap_or(10.0);
        let min_runs = cfg
            .get("min_runs")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(3);
        let confirm_runs = cfg
            .get("confirm_runs")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(2);
        let baseline_commits = cfg
            .get("baseline_commits")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(5);
        let noise_mads = cfg.get("noise_mads").and_then(Value::as_f64).unwrap_or(3.0);
        rules.insert(
            name.clone(),
            RegressRule {
                direction: dir,
                worse_pct,
                min_runs,
                confirm_runs,
                baseline_commits,
                noise_mads,
            },
        );
    }
    rules
}

/// Specification of a `reference_run` or `bisect` task created by rule 4.5.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressTaskSpec {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub priority: String,
    pub class: String,
    pub machine_class: String,
    pub scenario: String,
    pub metric: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    pub runs_needed: usize,
    pub reason: String,
}

/// Specification of a `REGRESSION` alert raised by rule 4.5.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegressAlertSpec {
    pub code: String,
    pub key: String,
    pub severity: String,
    pub sha: String,
    pub last_good_sha: String,
    pub machine_class: String,
    pub scenario: String,
    pub metric: String,
    pub base: f64,
    pub new: f64,
    pub worse_pct: f64,
    pub mad: f64,
    pub text: String,
}

/// Outcome of evaluating rule 4.5 for one `(sha, machine_class, scenario, name)`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RegressOutcome {
    /// Fewer than `min_runs` exist at `sha`; requests `missing_runs` `reference_run` tasks.
    NeedMoreRuns {
        sha: String,
        machine_class: String,
        scenario: String,
        metric: String,
        have_runs: usize,
        min_runs: usize,
        missing_runs: usize,
        tasks: Vec<RegressTaskSpec>,
    },
    /// Drop exceeds `worse_pct` and `noise_mads * MAD`, but confirmation runs are still needed
    /// before raising the final `REGRESSION` alert and `bisect` task.
    ConfirmRunsNeeded {
        sha: String,
        last_good_sha: String,
        machine_class: String,
        scenario: String,
        metric: String,
        base: f64,
        new: f64,
        worse_pct: f64,
        mad: f64,
        tasks: Vec<RegressTaskSpec>,
    },
    /// Regression confirmed (or `confirm_runs == 0` / >= `min_runs + confirm_runs` present):
    /// raises `Alert(REGRESSION)` and `TaskAdd(kind = bisect)`.
    RegressionConfirmed {
        sha: String,
        last_good_sha: String,
        machine_class: String,
        scenario: String,
        metric: String,
        base: f64,
        new: f64,
        worse_pct: f64,
        mad: f64,
        alert: RegressAlertSpec,
        bisect_task: RegressTaskSpec,
        confirm_tasks: Vec<RegressTaskSpec>,
    },
    /// Commit is within normal range (or difference is inside the MAD noise guard);
    /// `BASELINE` advances and includes `sha` as a good commit.
    BaselineGood {
        sha: String,
        machine_class: String,
        scenario: String,
        metric: String,
        base: Option<f64>,
        new: f64,
        worse_pct: f64,
        mad: f64,
        ignored_as_noise: bool,
        baseline: BaselineRow,
    },
}

impl RegressOutcome {
    /// Returns all tasks (`reference_run` and/or `bisect`) scheduled by this outcome.
    pub fn tasks(&self) -> Vec<RegressTaskSpec> {
        match self {
            RegressOutcome::NeedMoreRuns { tasks, .. } => tasks.clone(),
            RegressOutcome::ConfirmRunsNeeded { tasks, .. } => tasks.clone(),
            RegressOutcome::RegressionConfirmed {
                bisect_task,
                confirm_tasks,
                ..
            } => {
                let mut out = confirm_tasks.clone();
                out.push(bisect_task.clone());
                out
            }
            RegressOutcome::BaselineGood { .. } => Vec::new(),
        }
    }

    /// Returns the `REGRESSION` alert if one was raised.
    pub fn alert(&self) -> Option<&RegressAlertSpec> {
        match self {
            RegressOutcome::RegressionConfirmed { alert, .. } => Some(alert),
            _ => None,
        }
    }
}

/// Computes the statistical median of a non-empty slice of finite values.
/// Returns `0.0` for an empty slice.
pub fn median(values: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Computes the Median Absolute Deviation (`MAD = median(|x_i - median(X)|)`) of a slice.
pub fn mad(values: &[f64]) -> f64 {
    if values.len() <= 1 {
        return 0.0;
    }
    let med = median(values);
    let devs: Vec<f64> = values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .map(|v| (v - med).abs())
        .collect();
    median(&devs)
}

/// Computes the relative degradation ratio (`worse` in section 4.5):
/// - `higher_is_better`: `(base - new) / base`
/// - `lower_is_better`: `(new - base) / base`
pub fn worse_ratio(direction: Direction, base: f64, new: f64) -> f64 {
    if base.abs() <= f64::EPSILON {
        return 0.0;
    }
    match direction {
        Direction::HigherIsBetter => (base - new) / base,
        Direction::LowerIsBetter => (new - base) / base,
    }
}

/// Updates a `BaselineRow` by adding `(sha, new_median)` as a good commit and trimming to the
/// last `baseline_commits` good commits.
#[allow(clippy::too_many_arguments)]
pub fn update_baseline_with_good_commit(
    prev: Option<&BaselineRow>,
    machine_class: &str,
    scenario: &str,
    name: &str,
    sha: &str,
    new_median: f64,
    baseline_commits: usize,
    msg_id: u64,
) -> BaselineRow {
    let max_commits = baseline_commits.max(1);
    let mut good_commits: Vec<GoodCommit> =
        prev.map(|b| b.good_commits.clone()).unwrap_or_default();

    if let Some(existing) = good_commits.iter_mut().find(|c| c.sha == sha) {
        existing.median = new_median;
    } else {
        good_commits.push(GoodCommit {
            sha: sha.to_owned(),
            median: new_median,
        });
    }

    if good_commits.len() > max_commits {
        let start = good_commits.len() - max_commits;
        good_commits = good_commits[start..].to_vec();
    }

    let medians: Vec<f64> = good_commits.iter().map(|c| c.median).collect();
    let med = median(&medians);
    let n = good_commits.len();
    let from_sha = good_commits
        .first()
        .map(|c| c.sha.clone())
        .unwrap_or_else(|| sha.to_owned());
    let to_sha = good_commits
        .last()
        .map(|c| c.sha.clone())
        .unwrap_or_else(|| sha.to_owned());

    BaselineRow {
        machine_class: machine_class.to_owned(),
        scenario: scenario.to_owned(),
        name: name.to_owned(),
        median: med,
        n,
        from_sha,
        to_sha,
        good_commits,
        msg_id,
    }
}

/// Pure function implementing rule 4.5 for one metric name on `(sha, machine_class, scenario)`.
///
/// Arguments:
/// - `sha`: the candidate commit SHA being evaluated
/// - `machine_class`: machine class identifier (or `"default"` / `"-"`)
/// - `scenario`: scenario identifier (or `"default"` / `"-"`)
/// - `metric_name`: name of the metric (e.g. `"fps_median"`)
/// - `rule`: the `RegressRule` for `metric_name`
/// - `sha_values`: all recorded metric values at `sha` for `(machine_class, scenario, metric_name)`
/// - `baseline`: current `BASELINE` row (if any prior good commits exist)
/// - `base_all_values`: all raw metric values from the baseline good commits (used alongside commit
///   medians to compute `MAD(base values)`)
/// - `last_good_values_count`: number of runs currently recorded at the last good commit
/// - `require_confirm_before_alert`: if true, requires `min_runs + confirm_runs` before emitting
///   `RegressionConfirmed`; if false, emits `RegressionConfirmed` (along with the `confirm_runs`
///   reference run tasks, the `REGRESSION` alert, and the `bisect` task) as soon as `min_runs`
///   triggers the drop and passes the MAD noise guard.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_metric_rule(
    sha: &str,
    machine_class: &str,
    scenario: &str,
    metric_name: &str,
    rule: &RegressRule,
    sha_values: &[f64],
    baseline: Option<&BaselineRow>,
    base_all_values: &[f64],
    last_good_values_count: usize,
    require_confirm_before_alert: bool,
    msg_id: u64,
) -> RegressOutcome {
    let have_runs = sha_values.len();
    if have_runs < rule.min_runs {
        let missing = rule.min_runs - have_runs;
        let mut tasks = Vec::with_capacity(missing);
        for idx in 0..missing {
            let run_num = have_runs + idx + 1;
            tasks.push(RegressTaskSpec {
                id: format!("ref-{sha}-{machine_class}-{scenario}-{metric_name}-{run_num}"),
                kind: "reference_run".into(),
                title: format!(
                    "reference_run for {metric_name} at {sha} ({run_num}/{})",
                    rule.min_runs
                ),
                priority: "normal".into(),
                class: "game_run".into(),
                machine_class: machine_class.to_owned(),
                scenario: scenario.to_owned(),
                metric: metric_name.to_owned(),
                sha: sha.to_owned(),
                range: None,
                runs_needed: 1,
                reason: format!("only {have_runs} of {} min_runs at {sha}", rule.min_runs),
            });
        }
        return RegressOutcome::NeedMoreRuns {
            sha: sha.to_owned(),
            machine_class: machine_class.to_owned(),
            scenario: scenario.to_owned(),
            metric: metric_name.to_owned(),
            have_runs,
            min_runs: rule.min_runs,
            missing_runs: missing,
            tasks,
        };
    }

    let new_med = median(sha_values);

    // If no baseline exists yet (or the only baseline commit is `sha` itself), seed BASELINE with `sha` as good.
    let prior_good: Vec<&GoodCommit> = baseline
        .map(|b| {
            b.good_commits
                .iter()
                .filter(|c| c.sha != sha)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let (base_med, last_good_sha) = if !prior_good.is_empty() {
        let window_start = prior_good
            .len()
            .saturating_sub(rule.baseline_commits.max(1));
        let window = &prior_good[window_start..];
        let meds: Vec<f64> = window.iter().map(|c| c.median).collect();
        let last_sha = window.last().map(|c| c.sha.clone()).unwrap_or_default();
        (median(&meds), last_sha)
    } else if let Some(b) = baseline.filter(|b| b.n > 0 && b.to_sha != sha) {
        (b.median, b.to_sha.clone())
    } else {
        let updated = update_baseline_with_good_commit(
            baseline,
            machine_class,
            scenario,
            metric_name,
            sha,
            new_med,
            rule.baseline_commits,
            msg_id,
        );
        return RegressOutcome::BaselineGood {
            sha: sha.to_owned(),
            machine_class: machine_class.to_owned(),
            scenario: scenario.to_owned(),
            metric: metric_name.to_owned(),
            base: None,
            new: new_med,
            worse_pct: 0.0,
            mad: 0.0,
            ignored_as_noise: false,
            baseline: updated,
        };
    };

    let worse = worse_ratio(rule.direction, base_med, new_med);
    let worse_pct = worse * 100.0;
    let abs_diff = (new_med - base_med).abs();

    // Compute MAD(base values): use the raw values from the baseline window if provided,
    // otherwise fall back to the per-commit medians in the baseline window.
    let base_mad = if !base_all_values.is_empty() {
        mad(base_all_values)
    } else if !prior_good.is_empty() {
        let window_start = prior_good
            .len()
            .saturating_sub(rule.baseline_commits.max(1));
        let meds: Vec<f64> = prior_good[window_start..]
            .iter()
            .map(|c| c.median)
            .collect();
        mad(&meds)
    } else {
        0.0
    };

    let exceeds_pct = worse_pct > rule.worse_pct;
    let exceeds_mad = abs_diff > rule.noise_mads * base_mad;

    if exceeds_pct && exceeds_mad {
        let mut confirm_tasks = Vec::new();
        for idx in 0..rule.confirm_runs {
            let run_num = idx + 1;
            confirm_tasks.push(RegressTaskSpec {
                id: format!("confirm-{sha}-{machine_class}-{scenario}-{metric_name}-{run_num}"),
                kind: "reference_run".into(),
                title: format!(
                    "confirm reference_run for {metric_name} at {sha} ({run_num}/{})",
                    rule.confirm_runs
                ),
                priority: "high".into(),
                class: "game_run".into(),
                machine_class: machine_class.to_owned(),
                scenario: scenario.to_owned(),
                metric: metric_name.to_owned(),
                sha: sha.to_owned(),
                range: None,
                runs_needed: 1,
                reason: format!("confirm {worse_pct:.1}% regression on {metric_name} at {sha}"),
            });
        }
        if !last_good_sha.is_empty() {
            for idx in 0..rule.confirm_runs {
                let run_num = idx + 1;
                confirm_tasks.push(RegressTaskSpec {
                    id: format!(
                        "confirm-{last_good_sha}-{machine_class}-{scenario}-{metric_name}-{run_num}"
                    ),
                    kind: "reference_run".into(),
                    title: format!(
                        "confirm reference_run for {metric_name} at last good {last_good_sha} ({run_num}/{})",
                        rule.confirm_runs
                    ),
                    priority: "high".into(),
                    class: "game_run".into(),
                    machine_class: machine_class.to_owned(),
                    scenario: scenario.to_owned(),
                    metric: metric_name.to_owned(),
                    sha: last_good_sha.clone(),
                    range: None,
                    runs_needed: 1,
                    reason: format!(
                        "confirm baseline for {metric_name} at last good {last_good_sha}"
                    ),
                });
            }
        }

        let target_total = rule.min_runs + rule.confirm_runs;
        let has_confirm_runs = have_runs >= target_total
            && (last_good_sha.is_empty() || last_good_values_count >= target_total);

        if require_confirm_before_alert && !has_confirm_runs {
            return RegressOutcome::ConfirmRunsNeeded {
                sha: sha.to_owned(),
                last_good_sha,
                machine_class: machine_class.to_owned(),
                scenario: scenario.to_owned(),
                metric: metric_name.to_owned(),
                base: base_med,
                new: new_med,
                worse_pct,
                mad: base_mad,
                tasks: confirm_tasks,
            };
        }

        let range = if last_good_sha.is_empty() {
            sha.to_owned()
        } else {
            format!("{last_good_sha}..{sha}")
        };
        let alert = RegressAlertSpec {
            code: "REGRESSION".into(),
            key: format!("{sha}/{machine_class}/{scenario}/{metric_name}"),
            severity: "error".into(),
            sha: sha.to_owned(),
            last_good_sha: last_good_sha.clone(),
            machine_class: machine_class.to_owned(),
            scenario: scenario.to_owned(),
            metric: metric_name.to_owned(),
            base: base_med,
            new: new_med,
            worse_pct,
            mad: base_mad,
            text: format!(
                "REGRESSION on {metric_name} at {sha} ({machine_class}/{scenario}): base {base_med:.2} -> new {new_med:.2} ({worse_pct:.1}% worse, MAD {base_mad:.2})"
            ),
        };
        let bisect_task = RegressTaskSpec {
            id: format!("bisect-{last_good_sha}-{sha}-{machine_class}-{scenario}-{metric_name}"),
            kind: "bisect".into(),
            title: format!("bisect {metric_name} regression {range} on {machine_class}"),
            priority: "blocker".into(),
            class: "game_run".into(),
            machine_class: machine_class.to_owned(),
            scenario: scenario.to_owned(),
            metric: metric_name.to_owned(),
            sha: sha.to_owned(),
            range: Some(range.clone()),
            runs_needed: rule.min_runs,
            reason: format!(
                "{metric_name} dropped {worse_pct:.1}% from {base_med:.2} ({last_good_sha}) to {new_med:.2} ({sha})"
            ),
        };

        return RegressOutcome::RegressionConfirmed {
            sha: sha.to_owned(),
            last_good_sha,
            machine_class: machine_class.to_owned(),
            scenario: scenario.to_owned(),
            metric: metric_name.to_owned(),
            base: base_med,
            new: new_med,
            worse_pct,
            mad: base_mad,
            alert,
            bisect_task,
            confirm_tasks,
        };
    }

    let ignored_as_noise = exceeds_pct && !exceeds_mad;
    let updated = update_baseline_with_good_commit(
        baseline,
        machine_class,
        scenario,
        metric_name,
        sha,
        new_med,
        rule.baseline_commits,
        msg_id,
    );
    RegressOutcome::BaselineGood {
        sha: sha.to_owned(),
        machine_class: machine_class.to_owned(),
        scenario: scenario.to_owned(),
        metric: metric_name.to_owned(),
        base: Some(base_med),
        new: new_med,
        worse_pct,
        mad: base_mad,
        ignored_as_noise,
        baseline: updated,
    }
}

/// Converts a `RegressAlertSpec` into an `ALERT` JSON body (section 5.2).
pub fn alert_to_message_body(alert: &RegressAlertSpec, now_sec: u64) -> Value {
    json!({
        "v": 1,
        "code": alert.code,
        "key": alert.key,
        "severity": alert.severity,
        "state": "open",
        "opened_at": now_sec,
        "last_at": now_sec,
        "count": 1,
        "text": alert.text,
        "data": {
            "sha": alert.sha,
            "last_good_sha": alert.last_good_sha,
            "machine_class": alert.machine_class,
            "scenario": alert.scenario,
            "metric": alert.metric,
            "base": alert.base,
            "new": alert.new,
            "worse_pct": alert.worse_pct,
            "mad": alert.mad,
        }
    })
}

/// Converts a `RegressTaskSpec` into a `TASK_UPDATE` JSON body (section 5.2).
pub fn task_to_message_body(task: &RegressTaskSpec, now_sec: u64) -> Value {
    json!({
        "v": 1,
        "task": task.id,
        "id": task.id,
        "kind": task.kind,
        "title": task.title,
        "priority": task.priority,
        "state": "ready",
        "prev": "draft",
        "attempt": 1,
        "by": "regress",
        "reason": task.reason,
        "class": task.class,
        "accepts": [task.class],
        "machine_class": task.machine_class,
        "scenario": task.scenario,
        "metric": task.metric,
        "sha": task.sha,
        "range": task.range,
        "runs_needed": task.runs_needed,
        "created_at": now_sec,
        "ready_at": now_sec,
    })
}
