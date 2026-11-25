//! Pure admission control check: nothing half dead (section 7.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::estimate::Estimate;
use super::forecast::{ActiveLease, allowed_rate, forecast, max_parallel, pool_state};
use super::meter::{Meter, PoolState, QuotaConfig};

/// Task fields needed by admission control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskAdmitInput {
    pub id: String,
    pub kind: String,
    pub size: f64,
    pub checkpointable: bool,
    pub exclude_pools_until: BTreeMap<String, u64>,
}

/// Slot fields needed by admission control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAdmitInput {
    pub class: String,
    pub pools: Vec<String>,
    pub family_pool: Option<String>,
}

/// Result of `admit(task, slot, now)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmitDecision {
    Ok,
    Deny(String),
}

impl AdmitDecision {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Deny(r) => Some(r),
        }
    }
}

/// Pure admission check (section 7.1).
#[allow(clippy::too_many_arguments)]
pub fn admit(
    task: &TaskAdmitInput,
    slot: &SlotAdmitInput,
    meters: &BTreeMap<String, Meter>,
    est: &Estimate,
    cfg: &QuotaConfig,
    active_leases_by_pool: &BTreeMap<String, Vec<ActiveLease>>,
    committed_pct_by_pool: &BTreeMap<String, f64>,
    now: u64,
) -> AdmitDecision {
    let mut pools: Vec<&str> = slot.pools.iter().map(String::as_str).collect();
    if let Some(fp) = &slot.family_pool
        && !pools.contains(&fp.as_str())
    {
        pools.push(fp.as_str());
    }

    for pool_name in pools {
        let Some(m) = meters.get(pool_name) else {
            // Missing meter counts as unknown; check exclusion only.
            if task
                .exclude_pools_until
                .get(pool_name)
                .copied()
                .unwrap_or(0)
                > now
            {
                return AdmitDecision::Deny("this task died here, wait for the reset".to_owned());
            }
            continue;
        };

        let leases = active_leases_by_pool
            .get(pool_name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let f = forecast(m, now, leases);
        let st = pool_state(m, &f, now, cfg);

        if st == PoolState::Paused {
            let until = m
                .state_event
                .as_ref()
                .map_or(m.resets_at, |ev| ev.resets_at);
            return AdmitDecision::Deny(format!("paused until {until}"));
        }
        if st == PoolState::Closed {
            return AdmitDecision::Deny("closed at the worker line".to_owned());
        }

        let margin = if st == PoolState::Unknown {
            cfg.margin_low
        } else {
            cfg.margin_for(f.band)
        };
        let need = est.units_p90 * m.pct_per_unit * margin;
        let committed = committed_pct_by_pool.get(pool_name).copied().unwrap_or(0.0);
        let head = f.line - f.used - committed;
        let end = now.saturating_add((est.minutes_p90 * margin * 60.0).round().max(0.0) as u64);

        if need > head {
            let mins_until_reset = m.resets_at.saturating_sub(now) as f64 / 60.0;
            let bridges_reset = m.resets_at > now
                && m.resets_at < end
                && need <= f.line
                && task.checkpointable
                && est.minutes_p90 <= mins_until_reset;
            if !bridges_reset {
                return AdmitDecision::Deny(format!("no headroom on {}", m.pool));
            }
        }

        if let Some(t_line) = f.t_line
            && t_line < end
        {
            return AdmitDecision::Deny(format!("limit before the expected end on {}", m.pool));
        }

        if st == PoolState::Drain && est.minutes_p90 > cfg.drain_short_min && !task.checkpointable {
            return AdmitDecision::Deny(format!("drain on {}", m.pool));
        }

        if st == PoolState::Drain
            && f.t_line
                .is_some_and(|t_line| t_line.saturating_sub(now) < 300)
        {
            return AdmitDecision::Deny(format!("drain, nothing more on {}", m.pool));
        }

        if st == PoolState::Throttled {
            let active_on = leases.len();
            let allowed = allowed_rate(m, &f, now);
            let max_p = max_parallel(active_on, allowed, f.burn);
            if active_on >= max_p {
                return AdmitDecision::Deny(format!("throttled on {}", m.pool));
            }
        }

        if task.exclude_pools_until.get(&m.pool).copied().unwrap_or(0) > now {
            return AdmitDecision::Deny("this task died here, wait for the reset".to_owned());
        }
    }

    AdmitDecision::Ok
}
