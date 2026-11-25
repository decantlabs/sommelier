//! Quota calibration and history import (sections 6.2, 6.3).

use serde::{Deserialize, Serialize};

use super::codex::{parse_iso_epoch_secs, parse_rollout_line};
use super::meter::TokenWeights;

/// Calibration event imported from legacy Gemini TSV/log files or limit messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub at: String,
    pub task: String,
    pub kind: String,
    pub detail: String,
}

/// Direct Codex calibration: ordinary least-squares slope of `used_pct` over
/// `weighted_tokens` from samples that carry both meter and spend (section 6.2).
/// Returns `None` if fewer than 2 points or zero token variance.
pub fn calibrate_codex_pct_per_unit(points: &[(f64, f64)]) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| *x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| *y).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in points {
        let dx = x - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    if den <= 1e-12 {
        return None;
    }
    Some((num / den).max(0.0))
}

/// Calibrates Codex `pct_per_unit` from rollout `.jsonl` content for a given pool (e.g. `codex.5h`).
pub fn calibrate_codex_from_rollout(
    content: &str,
    owner: &str,
    pool: &str,
    weights: &TokenWeights,
) -> Option<f64> {
    let mut points = Vec::new();
    for line in content.lines() {
        if !line.contains("\"token_count\"") {
            continue;
        }
        if let Some(parsed) = parse_rollout_line(line, owner, weights)
            && let Some(tokens) = parsed.total_weighted_tokens
            && let Some(sample) = parsed.samples.iter().find(|s| s.pool == pool)
        {
            points.push((tokens, sample.used_pct));
        }
    }
    calibrate_codex_pct_per_unit(&points)
}

/// Gemini calibration on a limit message (section 6.3):
/// `capacity = n_calibration < 3 ? min(capacity, C_obs) : 0.5 * capacity + 0.5 * C_obs`
/// `n_calibration += 1`
pub fn calibrate_gemini_on_limit(capacity: f64, n_calibration: u64, c_obs: f64) -> (f64, u64) {
    if !c_obs.is_finite() || c_obs <= 0.0 {
        return (capacity, n_calibration);
    }
    let next_cap = if n_calibration < 3 {
        if capacity > 0.0 {
            capacity.min(c_obs)
        } else {
            c_obs
        }
    } else {
        0.5 * capacity + 0.5 * c_obs
    };
    (next_cap, n_calibration + 1)
}

/// Parses `quotas.tsv` rows (tab-separated, skipping empty/comment/header lines).
pub fn parse_quotas_tsv(content: &str) -> Vec<HistoryEvent> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        if cols[0].eq_ignore_ascii_case("utc") || cols[0].eq_ignore_ascii_case("time") {
            continue;
        }
        out.push(HistoryEvent {
            at: cols[0].to_owned(),
            task: cols.get(1).copied().unwrap_or("").to_owned(),
            kind: "quota_tsv".to_owned(),
            detail: cols[1..].join(" "),
        });
    }
    out
}

/// Parses `limited.tsv` rows (tab-separated, skipping empty/comment/header lines).
pub fn parse_limited_tsv(content: &str) -> Vec<HistoryEvent> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        if cols[0].eq_ignore_ascii_case("utc") || cols[0].eq_ignore_ascii_case("time") {
            continue;
        }
        out.push(HistoryEvent {
            at: cols[0].to_owned(),
            task: cols.get(1).copied().unwrap_or("").to_owned(),
            kind: "limited_tsv".to_owned(),
            detail: cols[1..].join(" "),
        });
    }
    out
}

/// Parses `LIMIT` lines from `launcher.log` (section 6.3).
/// Example: `2026-11-25 02:44:06 LIMIT untraced-descriptor hit a quota message, no new start until 07:45:05`
pub fn parse_launcher_log_limits(content: &str) -> Vec<HistoryEvent> {
    let mut out = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 && parts[2] == "LIMIT" {
            let at = format!("{}T{}Z", parts[0], parts[1]);
            let _epoch = parse_iso_epoch_secs(&at);
            out.push(HistoryEvent {
                at: format!("{} {}", parts[0], parts[1]),
                task: parts[3].to_owned(),
                kind: "launcher_limit".to_owned(),
                detail: parts[3..].join(" "),
            });
        }
    }
    out
}
