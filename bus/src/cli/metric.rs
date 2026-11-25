//! Performance metric commands (`sommelier metric`, section 5.1, step 10b).
//!
//! Commands:
//! - `sommelier metric add FILE [--data DIR] [--metric NAME] [--json]`
//! - `sommelier metric compare [SHA] [--base BASE_SHA] [--file FILE] [--data DIR] [--metric NAME] [--machine-class MC] [--scenario SC] [--json]`

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::exit;
use crate::regress::{
    Direction, RegressModule, RegressOutcome, RegressRule, default_regress_rules,
    evaluate_group_in_conn, ingest_and_evaluate, init_schema, parse_metrics_file,
    parse_regress_rules_from_value, query_baselines, query_metrics,
};
use crate::store::Store;
use crate::time::now_ms;

const USAGE: &str = "\
usage:
  sommelier metric add FILE [--data DIR] [--json]
  sommelier metric compare [SHA] [--base SHA] [--file FILE] [--data DIR] [--metric NAME] [--machine-class MC] [--scenario SC] [--json]
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(USAGE.trim().to_owned());
    };
    match sub.as_str() {
        "add" => cmd_add(rest),
        "compare" => cmd_compare(rest),
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(exit::code(exit::OK))
        }
        other => Err(format!("unknown subcommand `metric {other}`\n{USAGE}")),
    }
}

fn load_policy_rules(data_dir: &Path) -> BTreeMap<String, RegressRule> {
    let mut rules = default_regress_rules();
    let cache_path = data_dir.join("policy").join("swarm.policy.json");
    if let Ok(raw) = fs::read_to_string(&cache_path)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
    {
        for (k, r) in parse_regress_rules_from_value(&v) {
            rules.insert(k, r);
        }
    }
    let toml_path = data_dir.join("policy").join("swarm.policy.toml");
    if let Ok(raw) = fs::read_to_string(&toml_path)
        && let Ok(tbl) = toml::from_str::<toml::Table>(&raw)
        && let Ok(v) = serde_json::to_value(tbl)
    {
        for (k, r) in parse_regress_rules_from_value(&v) {
            rules.insert(k, r);
        }
    }
    rules
}

fn open_store_with_regress(
    data_dir: &Path,
    rules: BTreeMap<String, RegressRule>,
) -> Result<Store, String> {
    let mut store = Store::open(data_dir)?;
    store.register_module(Box::new(RegressModule::with_rules(rules)))?;
    let log_path = data_dir.join("bus.jsonl");
    if log_path.exists() {
        let _ = store.catch_up_from_log(&log_path);
    }
    Ok(store)
}

fn cmd_add(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut file_path: Option<PathBuf> = None;
    let mut require_confirm = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--require-confirm" => require_confirm = true,
            "--data" => {
                let v = it.next().ok_or_else(|| "--data needs a value".to_owned())?;
                data_dir = Some(PathBuf::from(v));
            }
            "--file" => {
                let v = it.next().ok_or_else(|| "--file needs a value".to_owned())?;
                file_path = Some(PathBuf::from(v));
            }
            s if s.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(s.trim_start_matches("--data=")));
            }
            s if s.starts_with("--file=") => {
                file_path = Some(PathBuf::from(s.trim_start_matches("--file=")));
            }
            s if !s.starts_with('-') && file_path.is_none() => {
                file_path = Some(PathBuf::from(s));
            }
            other => {
                return Err(format!(
                    "unknown argument `{other}` for `sommelier metric add`"
                ));
            }
        }
    }

    let Some(path) = file_path else {
        return Err("usage: sommelier metric add FILE [--data DIR] [--json]".into());
    };
    let content =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let now_sec = now_ms() / 1000;
    let rows = parse_metrics_file(&content, now_sec, 0)?;

    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let rules = load_policy_rules(&dir);
    let store = open_store_with_regress(&dir, rules.clone())?;
    let outcomes = ingest_and_evaluate(store.connection(), &rows, &rules, require_confirm)?;

    let mut alerts = Vec::new();
    let mut tasks = Vec::new();
    for o in &outcomes {
        if let Some(a) = o.alert() {
            alerts.push(a.clone());
        }
        tasks.extend(o.tasks());
    }

    if json_out {
        let out = json!({
            "ok": true,
            "code": exit::OK,
            "rows_added": rows.len(),
            "outcomes": outcomes,
            "alerts": alerts,
            "tasks": tasks,
        });
        println!("{out}");
    } else {
        println!(
            "ok metric add: {} metric rows stored, {} checks evaluated, {} alerts, {} tasks",
            rows.len(),
            outcomes.len(),
            alerts.len(),
            tasks.len()
        );
        for a in &alerts {
            println!("{}", a.text);
        }
    }

    Ok(exit::code(exit::OK))
}

fn cmd_compare(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut file_path: Option<PathBuf> = None;
    let mut target_sha: Option<String> = None;
    let mut metric_filter: Option<String> = None;
    let mut machine_filter: Option<String> = None;
    let mut scenario_filter: Option<String> = None;
    let mut require_confirm = false;
    let mut min_runs_override: Option<usize> = None;
    let mut worse_pct_override: Option<f64> = None;
    let mut noise_mads_override: Option<f64> = None;
    let mut direction_override: Option<Direction> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--require-confirm" => require_confirm = true,
            "--data" => {
                let v = it.next().ok_or_else(|| "--data needs a value".to_owned())?;
                data_dir = Some(PathBuf::from(v));
            }
            "--file" => {
                let v = it.next().ok_or_else(|| "--file needs a value".to_owned())?;
                file_path = Some(PathBuf::from(v));
            }
            "--sha" => {
                let v = it.next().ok_or_else(|| "--sha needs a value".to_owned())?;
                target_sha = Some(v.clone());
            }
            "--metric" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--metric needs a value".to_owned())?;
                metric_filter = Some(v.clone());
            }
            "--machine-class" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--machine-class needs a value".to_owned())?;
                machine_filter = Some(v.clone());
            }
            "--scenario" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--scenario needs a value".to_owned())?;
                scenario_filter = Some(v.clone());
            }
            "--min-runs" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--min-runs needs a number".to_owned())?;
                min_runs_override =
                    Some(v.parse().map_err(|_| format!("invalid --min-runs `{v}`"))?);
            }
            "--worse-pct" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--worse-pct needs a number".to_owned())?;
                worse_pct_override = Some(
                    v.parse()
                        .map_err(|_| format!("invalid --worse-pct `{v}`"))?,
                );
            }
            "--noise-mads" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--noise-mads needs a number".to_owned())?;
                noise_mads_override = Some(
                    v.parse()
                        .map_err(|_| format!("invalid --noise-mads `{v}`"))?,
                );
            }
            "--direction" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--direction needs a value".to_owned())?;
                direction_override =
                    Some(Direction::parse(v).ok_or_else(|| format!("invalid --direction `{v}`"))?);
            }
            s if s.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(s.trim_start_matches("--data=")));
            }
            s if s.starts_with("--file=") => {
                file_path = Some(PathBuf::from(s.trim_start_matches("--file=")));
            }
            s if s.starts_with("--sha=") => {
                target_sha = Some(s.trim_start_matches("--sha=").to_owned());
            }
            s if s.starts_with("--metric=") => {
                metric_filter = Some(s.trim_start_matches("--metric=").to_owned());
            }
            s if !s.starts_with('-') && target_sha.is_none() => {
                target_sha = Some(s.to_owned());
            }
            other => {
                return Err(format!(
                    "unknown argument `{other}` for `sommelier metric compare`"
                ));
            }
        }
    }

    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let mut rules = load_policy_rules(&dir);
    if let Some(ref m_name) = metric_filter {
        let entry = rules.entry(m_name.clone()).or_default();
        if let Some(mr) = min_runs_override {
            entry.min_runs = mr;
        }
        if let Some(wp) = worse_pct_override {
            entry.worse_pct = wp;
        }
        if let Some(nm) = noise_mads_override {
            entry.noise_mads = nm;
        }
        if let Some(d) = direction_override {
            entry.direction = d;
        }
    } else {
        for r in rules.values_mut() {
            if let Some(mr) = min_runs_override {
                r.min_runs = mr;
            }
            if let Some(wp) = worse_pct_override {
                r.worse_pct = wp;
            }
            if let Some(nm) = noise_mads_override {
                r.noise_mads = nm;
            }
            if let Some(d) = direction_override {
                r.direction = d;
            }
        }
    }

    let now_sec = now_ms() / 1000;
    let outcomes = if let Some(ref fpath) = file_path {
        let content = fs::read_to_string(fpath)
            .map_err(|e| format!("cannot read {}: {e}", fpath.display()))?;
        let rows = parse_metrics_file(&content, now_sec, 0)?;
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let all_outcomes = ingest_and_evaluate(&conn, &rows, &rules, require_confirm)?;
        filter_outcomes(
            all_outcomes,
            target_sha.as_deref(),
            metric_filter.as_deref(),
            machine_filter.as_deref(),
            scenario_filter.as_deref(),
        )
    } else {
        let store = open_store_with_regress(&dir, rules.clone())?;
        let conn = store.connection();
        init_schema(conn)?;
        let all_metrics = query_metrics(conn)?;
        let mut groups = Vec::new();
        for m in &all_metrics {
            let g = (m.sha.clone(), m.machine_class.clone(), m.scenario.clone());
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        let target = target_sha.or_else(|| groups.last().map(|g| g.0.clone()));
        let mut evals = Vec::new();
        for (sha, mc, sc) in groups {
            if let Some(ref t_sha) = target
                && &sha != t_sha
            {
                continue;
            }
            if let Some(ref f_mc) = machine_filter
                && &mc != f_mc
            {
                continue;
            }
            if let Some(ref f_sc) = scenario_filter
                && &sc != f_sc
            {
                continue;
            }
            for (name, rule) in &rules {
                if let Some(ref f_m) = metric_filter
                    && name != f_m
                {
                    continue;
                }
                let out =
                    evaluate_group_in_conn(conn, &sha, &mc, &sc, name, rule, require_confirm, 0)?;
                evals.push(out);
            }
        }
        let _ = query_baselines(conn);
        evals
    };

    let mut alerts = Vec::new();
    let mut tasks = Vec::new();
    for o in &outcomes {
        if let Some(a) = o.alert() {
            alerts.push(a.clone());
        }
        tasks.extend(o.tasks());
    }

    if json_out {
        let out = json!({
            "ok": true,
            "code": exit::OK,
            "outcomes": outcomes,
            "alerts": alerts,
            "tasks": tasks,
        });
        println!("{out}");
    } else {
        for o in &outcomes {
            match o {
                RegressOutcome::NeedMoreRuns {
                    sha,
                    metric,
                    have_runs,
                    min_runs,
                    missing_runs,
                    ..
                } => {
                    println!(
                        "NEED_MORE_RUNS sha={sha} metric={metric} have={have_runs} min_runs={min_runs} schedule={missing_runs}"
                    );
                }
                RegressOutcome::ConfirmRunsNeeded {
                    sha,
                    last_good_sha,
                    metric,
                    base,
                    new,
                    worse_pct,
                    ..
                } => {
                    println!(
                        "CONFIRM_NEEDED sha={sha} base_sha={last_good_sha} metric={metric} base={base:.2} new={new:.2} worse={worse_pct:.1}%"
                    );
                }
                RegressOutcome::RegressionConfirmed {
                    sha,
                    last_good_sha,
                    metric,
                    base,
                    new,
                    worse_pct,
                    mad,
                    ..
                } => {
                    println!(
                        "REGRESSION sha={sha} base_sha={last_good_sha} metric={metric} base={base:.2} new={new:.2} worse={worse_pct:.1}% mad={mad:.2}"
                    );
                }
                RegressOutcome::BaselineGood {
                    sha,
                    metric,
                    base,
                    new,
                    worse_pct,
                    ignored_as_noise,
                    ..
                } => {
                    let note = if *ignored_as_noise { " (noise)" } else { "" };
                    let base_s = base
                        .map(|b| format!("{b:.2}"))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "OK sha={sha} metric={metric} base={base_s} new={new:.2} worse={worse_pct:.1}%{note}"
                    );
                }
            }
        }
    }

    Ok(exit::code(exit::OK))
}

fn filter_outcomes(
    outcomes: Vec<RegressOutcome>,
    target_sha: Option<&str>,
    metric_filter: Option<&str>,
    machine_filter: Option<&str>,
    scenario_filter: Option<&str>,
) -> Vec<RegressOutcome> {
    outcomes
        .into_iter()
        .filter(|o| {
            let (sha, mc, sc, metric) = match o {
                RegressOutcome::NeedMoreRuns {
                    sha,
                    machine_class,
                    scenario,
                    metric,
                    ..
                }
                | RegressOutcome::ConfirmRunsNeeded {
                    sha,
                    machine_class,
                    scenario,
                    metric,
                    ..
                }
                | RegressOutcome::RegressionConfirmed {
                    sha,
                    machine_class,
                    scenario,
                    metric,
                    ..
                }
                | RegressOutcome::BaselineGood {
                    sha,
                    machine_class,
                    scenario,
                    metric,
                    ..
                } => (
                    sha.as_str(),
                    machine_class.as_str(),
                    scenario.as_str(),
                    metric.as_str(),
                ),
            };
            target_sha.is_none_or(|t| t == sha)
                && metric_filter.is_none_or(|m| m == metric)
                && machine_filter.is_none_or(|m| m == mc)
                && scenario_filter.is_none_or(|s| s == sc)
        })
        .collect()
}
