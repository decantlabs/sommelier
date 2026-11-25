//! Quota state API projection for `/api/state` (sections 4.6, 6.4, 7.7, 10.2, 10.3).
//!
//! Section 6.4:
//! "How the UI shows a pool: one row per meter with a bar (used solid, committed hatched,
//! the worker line and the reserve band marked), the state as a word and a colour,
//! 'reaches the line in 2 h 10 min' or 'resets first, in 1 h 5 min', the reset time,
//! burn per hour, source and confidence as a badge, age of the sample, and the count of
//! leases on it. A sample older than `fresh_min` greys the bar. No account id, no email,
//! no plan price."
//!
//! Section 7.7:
//! "What the UI shows per pool: Used, committed, forecast time to the line with its
//! confidence, reserve band, drain state, and the list of tasks that wait for this pool
//! with their reason."

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::apply::{load_all_meters, query_estimates, query_spend};
use super::forecast::{ActiveLease, forecast, pool_state};
use super::meter::{
    ConfidenceBand, Meter, PoolState, QuotaConfig, Source, contains_sensitive_text,
};

/// Task waiting on a quota pool (section 7.7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingPoolTask {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub priority: String,
    pub reason: String,
}

/// Complete pool view row in the `"pools"` array of `/api/state` (sections 6.4 and 7.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoolApiRow {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub state: PoolState,
    pub drain: bool,
    pub used_pct: f64,
    pub committed_pct: f64,
    pub line_pct: f64,
    pub stop_at_pct: f64,
    pub reserve_pct: f64,
    pub resets_at: u64,
    pub burn_pct_per_min: f64,
    pub burn_pct_per_hour: f64,
    pub t_line: Option<u64>,
    pub t_80: Option<u64>,
    pub resets_first: bool,
    pub forecast_label: String,
    pub source: Source,
    pub confidence: f64,
    pub band: ConfidenceBand,
    pub observed_at: u64,
    pub age_secs: u64,
    pub fresh_min: u64,
    pub stale: bool,
    pub probe_enabled: bool,
    pub active_leases: usize,
    pub waiting_tasks: Vec<WaitingPoolTask>,
}

/// Formats a duration in seconds as `"2 h 10 min"`, `"45 min"`, or `"30 s"`.
pub fn format_duration_hm(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs} s");
    }
    let total_min = secs / 60;
    let h = total_min / 60;
    let m = total_min % 60;
    if h == 0 {
        format!("{m} min")
    } else if m == 0 {
        format!("{h} h")
    } else {
        format!("{h} h {m} min")
    }
}

/// Formats the human forecast headline of section 6.4:
/// - `"paused until reset in 1 h 5 min"` when paused
/// - `"closed at the worker line"` when closed
/// - `"reaches the line in 2 h 10 min"` when `t_line` is before `resets_at`
/// - `"resets first, in 1 h 5 min"` when `resets_first` is true or `burn == 0` with a future reset
/// - `"steady, no burn"` otherwise
pub fn format_forecast_label(
    state: PoolState,
    t_line: Option<u64>,
    resets_first: bool,
    resets_at: u64,
    now_sec: u64,
) -> String {
    if state == PoolState::Paused {
        if resets_at > now_sec {
            return format!(
                "paused, resets in {}",
                format_duration_hm(resets_at - now_sec)
            );
        }
        return "paused".to_owned();
    }
    if state == PoolState::Closed {
        if resets_at > now_sec {
            return format!(
                "closed at the worker line, resets in {}",
                format_duration_hm(resets_at - now_sec)
            );
        }
        return "closed at the worker line".to_owned();
    }
    if let Some(tl) = t_line {
        let rem = tl.saturating_sub(now_sec);
        return format!("reaches the line in {}", format_duration_hm(rem));
    }
    if (resets_first || resets_at > now_sec) && resets_at > now_sec {
        return format!(
            "resets first, in {}",
            format_duration_hm(resets_at - now_sec)
        );
    }
    "steady".to_owned()
}

/// Builds a [`PoolApiRow`] from a [`Meter`], active leases, and waiting tasks at `now_sec`.
pub fn build_pool_row(
    m: &Meter,
    now_sec: u64,
    cfg: &QuotaConfig,
    leases_on_pool: &[ActiveLease],
    waiting_tasks: Vec<WaitingPoolTask>,
) -> PoolApiRow {
    let f = forecast(m, now_sec, leases_on_pool);
    let st = pool_state(m, &f, now_sec, cfg);
    let committed_pct: f64 = leases_on_pool
        .iter()
        .filter(|l| l.pools.iter().any(|p| p == &m.pool))
        .map(|l| l.remaining_cost_pct.max(0.0))
        .sum();
    let effective_resets_at = m
        .state_event
        .as_ref()
        .filter(|ev| now_sec < ev.resets_at)
        .map_or(m.resets_at, |ev| ev.resets_at);
    let observed_at = m.latest.as_ref().map_or(0, |s| s.observed_at);
    let age_secs = if observed_at > 0 {
        now_sec.saturating_sub(observed_at)
    } else {
        0
    };
    let stale = observed_at == 0 || age_secs > m.policy.fresh_min.saturating_mul(60);
    let source = m.latest.as_ref().map_or(Source::Inferred, |s| s.source);
    let burn_per_hour = (f.burn * 60.0 * 100.0).round() / 100.0;
    let burn_per_min = (f.burn * 10000.0).round() / 10000.0;
    let used_pct = (f.used * 100.0).round() / 100.0;
    let committed_rounded = (committed_pct * 100.0).round() / 100.0;
    let conf_rounded = (f.conf * 1000.0).round() / 1000.0;
    let forecast_label =
        format_forecast_label(st, f.t_line, f.resets_first, effective_resets_at, now_sec);

    PoolApiRow {
        owner: m.owner.clone(),
        vendor: m.vendor.clone(),
        pool: m.pool.clone(),
        window: m.window.clone(),
        state: st,
        drain: st == PoolState::Drain,
        used_pct,
        committed_pct: committed_rounded,
        line_pct: f.line,
        stop_at_pct: m.policy.stop_at,
        reserve_pct: m.policy.reserve_pct,
        resets_at: effective_resets_at,
        burn_pct_per_min: burn_per_min,
        burn_pct_per_hour: burn_per_hour,
        t_line: f.t_line,
        t_80: f.t_80,
        resets_first: f.resets_first,
        forecast_label,
        source,
        confidence: conf_rounded,
        band: f.band,
        observed_at,
        age_secs,
        fresh_min: m.policy.fresh_min,
        stale,
        probe_enabled: m.probe_enabled,
        active_leases: leases_on_pool.len(),
        waiting_tasks,
    }
}

fn table_exists(conn: &Connection, table_name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table_name],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

/// Reads active leases from the `LEASE` table if step 8 (`task-queue`) has created it,
/// or falls back to `AGENT` rows in state `"running"` so `active_leases` is always truthful.
fn query_active_leases_for_pool(
    conn: &Connection,
    owner: &str,
    pool: &str,
    m: &Meter,
    now_sec: u64,
) -> Vec<ActiveLease> {
    if table_exists(conn, "LEASE")
        && let Ok(mut stmt) = conn.prepare(
            "SELECT pools, est_cost, est_end_at, state FROM LEASE WHERE owner = ?1 AND state IN ('active', 'suspect')",
        )
        && let Ok(rows) = stmt.query_map([owner], |r| {
            let pools_s: String = r.get(0)?;
            let est_cost: f64 = r.get::<_, f64>(1).unwrap_or(0.0);
            let est_end_at: i64 = r.get::<_, i64>(2).unwrap_or(0);
            Ok((pools_s, est_cost, est_end_at as u64))
        })
    {
        let mut out = Vec::new();
        for (pools_s, est_cost, est_end_at) in rows.flatten() {
            let parsed_pools: Vec<String> = serde_json::from_str(&pools_s).unwrap_or_else(|_| {
                pools_s
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            });
            if parsed_pools.iter().any(|p| p == pool) {
                let rem_min = (est_end_at.saturating_sub(now_sec) as f64 / 60.0).max(1.0);
                let rem_pct = (est_cost * m.pct_per_unit).max(0.0);
                out.push(ActiveLease {
                    pools: parsed_pools,
                    remaining_cost_pct: rem_pct,
                    remaining_minutes: rem_min,
                });
            }
        }
        return out;
    }

    // Fallback when `LEASE` table is not present: count running `AGENT` rows whose class maps to this pool.
    if table_exists(conn, "AGENT")
        && let Ok(agents) = crate::store::query_agents(conn)
    {
        let mut out = Vec::new();
        for a in agents {
            if a.owner != owner
                || a.reported_state != "running"
                || a.name == "main"
                || a.name == a.owner
            {
                continue;
            }
            let cls = a.class.to_ascii_lowercase();
            let matches_pool = (cls.starts_with("codex") && pool.starts_with("codex."))
                || ((cls.starts_with("gemini") || cls.starts_with("agy"))
                    && pool.starts_with("agy."))
                || (cls.starts_with("claude") && pool.starts_with("claude."));
            if matches_pool {
                out.push(ActiveLease {
                    pools: vec![pool.to_owned()],
                    remaining_cost_pct: 0.0,
                    remaining_minutes: 30.0,
                });
            }
        }
        return out;
    }

    Vec::new()
}

/// Queries tasks waiting on `pool` from the `TASK` table when step 8 (`task-queue`) has created it
/// or when pool state is not `Open`.
fn query_waiting_tasks_for_pool(
    conn: &Connection,
    pool: &str,
    state: PoolState,
    resets_at: u64,
    now_sec: u64,
) -> Vec<WaitingPoolTask> {
    if !table_exists(conn, "TASK") {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, title, kind, priority, exclude_pools_until FROM TASK WHERE state = 'ready' ORDER BY id ASC",
    ) && let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1).unwrap_or_default(),
            r.get::<_, String>(2).unwrap_or_default(),
            r.get::<_, String>(3).unwrap_or_else(|_| "normal".into()),
            r.get::<_, String>(4).unwrap_or_default(),
        ))
    }) {
        for (id, title, kind, priority, excl_s) in rows.flatten() {
            if let Ok(excl_map) = serde_json::from_str::< std::collections::BTreeMap<String, u64> >(&excl_s)
                && excl_map.get(pool).copied().unwrap_or(0) > now_sec
            {
                out.push(WaitingPoolTask {
                    id,
                    title,
                    kind,
                    priority,
                    reason: "this task died here, wait for the reset".into(),
                });
                continue;
            }
            if matches!(state, PoolState::Paused | PoolState::Closed | PoolState::Drain | PoolState::Throttled) {
                let reason = match state {
                    PoolState::Paused => format!("paused until {resets_at}"),
                    PoolState::Closed => "closed at the worker line".into(),
                    PoolState::Drain => format!("drain on {pool}"),
                    PoolState::Throttled => format!("throttled on {pool}"),
                    _ => String::new(),
                };
                out.push(WaitingPoolTask {
                    id,
                    title,
                    kind,
                    priority,
                    reason,
                });
            }
        }
    }
    out
}

/// Queries all [`PoolApiRow`] items from `conn` at `now_sec`.
pub fn query_pools_api(conn: &Connection, now_sec: u64) -> Result<Vec<PoolApiRow>, String> {
    let meters = load_all_meters(conn)?;
    let cfg = QuotaConfig::default();
    let mut pools = Vec::with_capacity(meters.len());
    for m in &meters {
        let leases = query_active_leases_for_pool(conn, &m.owner, &m.pool, m, now_sec);
        let f = forecast(m, now_sec, &leases);
        let st = pool_state(m, &f, now_sec, &cfg);
        let effective_resets = m
            .state_event
            .as_ref()
            .filter(|ev| now_sec < ev.resets_at)
            .map_or(m.resets_at, |ev| ev.resets_at);
        let waiting = query_waiting_tasks_for_pool(conn, &m.pool, st, effective_resets, now_sec);
        pools.push(build_pool_row(m, now_sec, &cfg, &leases, waiting));
    }
    Ok(pools)
}

/// Populates `"pools"` (and `"quota"` with raw table rows) in `/api/state` (section 10.2 `api(state)`).
pub fn build_quota_api(
    conn: &Connection,
    now_sec: u64,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let pools = query_pools_api(conn, now_sec)?;
    let pools_val = serde_json::to_value(&pools).map_err(|e| e.to_string())?;
    let serialized = pools_val.to_string();
    if contains_sensitive_text(&serialized) {
        return Err("refused to emit sensitive text in pools API".to_owned());
    }
    state.insert("pools".into(), pools_val);

    let spend = query_spend(conn).unwrap_or_default();
    let estimates = query_estimates(conn).unwrap_or_default();
    state.insert(
        "quota".into(),
        json!({
            "as_of": now_sec,
            "pools": pools,
            "spend": spend,
            "estimates": estimates,
        }),
    );
    Ok(())
}
