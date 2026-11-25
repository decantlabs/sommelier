//! Pure quota forecasting and pool state functions (section 4.6).

use serde::{Deserialize, Serialize};

use super::meter::{ConfidenceBand, Meter, PoolState, QuotaConfig, QuotaSample, Source};

/// Active lease commitment on a pool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveLease {
    pub pools: Vec<String>,
    pub remaining_cost_pct: f64,
    pub remaining_minutes: f64,
}

/// Derived forecast for a meter at `now`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub used: f64,
    pub burn: f64,
    pub burn_obs: f64,
    pub burn_plan: f64,
    pub line: f64,
    pub t_line: Option<u64>,
    pub resets_first: bool,
    pub t_80: Option<u64>,
    pub conf: f64,
    pub band: ConfidenceBand,
}

/// Applies a new sample to a meter according to section 4.6.
/// Returns `true` if the sample was accepted.
pub fn on_sample(m: &mut Meter, s: QuotaSample) -> bool {
    if !s.used_pct.is_finite() || s.used_pct < 0.0 || s.used_pct > 150.0 {
        return false;
    }
    if s.resets_at > 0 && s.resets_at + m.policy.window_duration_sec < s.observed_at {
        return false;
    }
    if m.resets_at > 0 && s.resets_at > 0 && s.resets_at.abs_diff(m.resets_at) > 600 {
        m.series.clear();
        m.resets_at = s.resets_at;
    } else if m.resets_at == 0 && s.resets_at > 0 {
        m.resets_at = s.resets_at;
    }

    m.series.push(s);
    m.latest = best_sample(&m.series).cloned();
    true
}

/// Picks the best sample by `(source rank api > manual > inferred, newest observed_at, used_pct)`.
fn best_sample(series: &[QuotaSample]) -> Option<&QuotaSample> {
    series.iter().max_by(|a, b| {
        a.source
            .rank()
            .cmp(&b.source.rank())
            .then_with(|| a.observed_at.cmp(&b.observed_at))
            .then_with(|| a.used_pct.total_cmp(&b.used_pct))
    })
}

/// Least-squares slope of `used_pct` per minute over the last `burn_window_min` minutes.
/// Needs at least 3 samples spanning at least 5 minutes; never below 0.
pub fn observed_burn(series: &[QuotaSample], now: u64, burn_window_min: u64) -> f64 {
    let cutoff = now.saturating_sub(burn_window_min.saturating_mul(60));
    let mut pts: Vec<(u64, f64)> = series
        .iter()
        .filter(|s| s.observed_at >= cutoff && s.observed_at <= now)
        .map(|s| (s.observed_at, s.used_pct))
        .collect();
    if pts.len() < 3 {
        return 0.0;
    }
    pts.sort_by_key(|(t, _)| *t);
    let min_t = pts.first().map_or(0, |(t, _)| *t);
    let max_t = pts.last().map_or(0, |(t, _)| *t);
    if max_t.saturating_sub(min_t) < 300 {
        return 0.0;
    }

    let n = pts.len() as f64;
    let xs: Vec<f64> = pts.iter().map(|(t, _)| (t - min_t) as f64 / 60.0).collect();
    let ys: Vec<f64> = pts.iter().map(|(_, y)| *y).collect();
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    if den <= 1e-12 {
        return 0.0;
    }
    (num / den).max(0.0)
}

fn freshness(age_min: f64, fresh_min: u64) -> f64 {
    if fresh_min == 0 || age_min <= fresh_min as f64 {
        1.0
    } else {
        0.5_f64.powf((age_min - fresh_min as f64) / fresh_min as f64)
    }
}

fn calibration_factor(source: Source, n_calibration: u64) -> f64 {
    match source {
        Source::Api | Source::Manual => 1.0,
        Source::Inferred => (0.5 + 0.1 * (n_calibration as f64)).min(1.0),
    }
}

/// Computes the forecast for meter `m` at `now` (section 4.6).
pub fn forecast(m: &Meter, now: u64, active_leases: &[ActiveLease]) -> Forecast {
    let burn_obs = observed_burn(&m.series, now, m.policy.burn_window_min);
    let burn_plan: f64 = active_leases
        .iter()
        .filter(|l| l.pools.iter().any(|p| p == &m.pool))
        .map(|l| l.remaining_cost_pct / l.remaining_minutes.max(1e-6))
        .sum();
    let burn = burn_obs.max(burn_plan);
    let line = m.policy.stop_at.min(100.0 - m.policy.reserve_pct);

    let Some(latest) = &m.latest else {
        return Forecast {
            used: 0.0,
            burn,
            burn_obs,
            burn_plan,
            line,
            t_line: None,
            resets_first: false,
            t_80: None,
            conf: 0.0,
            band: ConfidenceBand::Low,
        };
    };

    let minutes_since = now.saturating_sub(latest.observed_at) as f64 / 60.0;
    let used = (latest.used_pct + burn_obs * minutes_since).min(100.0);

    let (t_line, resets_first) = if burn > 0.0 {
        let secs_to_line = if used >= line {
            0
        } else {
            ((line - used) / burn * 60.0).round().max(0.0) as u64
        };
        let raw_t = now.saturating_add(secs_to_line);
        if m.resets_at > 0 && m.resets_at <= raw_t {
            (None, true)
        } else {
            (Some(raw_t), false)
        }
    } else {
        (None, false)
    };

    let t_80 = if burn > 0.0 {
        let secs_to_80 = if used >= 80.0 {
            0
        } else {
            ((80.0 - used) / burn * 60.0).round().max(0.0) as u64
        };
        Some(now.saturating_add(secs_to_80))
    } else {
        None
    };

    let conf = (latest.source.base_confidence()
        * freshness(minutes_since, m.policy.fresh_min)
        * calibration_factor(latest.source, m.n_calibration))
    .clamp(0.0, 1.0);
    let band = ConfidenceBand::from_conf(conf);

    Forecast {
        used,
        burn,
        burn_obs,
        burn_plan,
        line,
        t_line,
        resets_first,
        t_80,
        conf,
        band,
    }
}

/// Derives the pool state from a meter and its forecast (section 4.6).
pub fn pool_state(m: &Meter, f: &Forecast, now: u64, cfg: &QuotaConfig) -> PoolState {
    if let Some(ev) = &m.state_event
        && now < ev.resets_at
    {
        return PoolState::Paused;
    }
    if m.latest.is_none()
        || (!m.probe_enabled && m.latest.as_ref().is_none_or(|s| s.source != Source::Manual))
    {
        return PoolState::Unknown;
    }
    if f.used >= f.line {
        return PoolState::Closed;
    }
    if f.t_line
        .is_some_and(|t| t.saturating_sub(now) <= m.policy.drain_min.saturating_mul(60))
        || (f.line - f.used <= m.policy.drain_band_pct)
    {
        return PoolState::Drain;
    }
    if m.policy.pace && f.burn > allowed_rate(m, f, now) * cfg.throttle_slack {
        return PoolState::Throttled;
    }
    if f.conf < cfg.min_conf {
        return PoolState::Unknown;
    }
    PoolState::Open
}

/// Remaining budget divided by remaining minutes until reset (section 4.6).
pub fn allowed_rate(m: &Meter, f: &Forecast, now: u64) -> f64 {
    let minutes_until = (m.resets_at.saturating_sub(now) as f64 / 60.0).max(1e-6);
    (f.line - f.used).max(0.0) / minutes_until
}

/// Allowed parallel workers on a throttled pool (section 4.6).
pub fn max_parallel(active_on_pool: usize, allowed: f64, burn: f64) -> usize {
    if burn <= 0.0 {
        return active_on_pool.max(1);
    }
    ((active_on_pool as f64) * allowed / burn).floor().max(1.0) as usize
}
