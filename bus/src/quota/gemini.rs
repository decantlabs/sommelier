//! Gemini / Antigravity (`agy`) quota parsers and inferred meter (section 6.3).
//!
//! Sources:
//! - A: `agy` limit message on stderr (`RESOURCE_EXHAUSTED`, `Individual quota reached`,
//!   `Too Many Requests`, `Resets in XhYmZs`).
//! - B: Own counters (`worker_minutes * effort_weight`), seed capacity 3,000 units per 5h window.
//! - C: `agy --print='...' --output-format json` (tested on hub PC 25 Nov 2026: returns
//!   `usage.{input_tokens, output_tokens, thinking_tokens, cache_read_tokens, total_tokens}`).

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use super::meter::{Meter, QuotaSample, Source, SpendRecord, StateEvent, TokenWeights};

/// Seed capacity of the shared 5-hour Antigravity pool in weighted worker-minutes (section 6.3).
pub const SEED_CAPACITY_UNITS: f64 = 3000.0;

const LIMIT_PATTERNS: [&str; 3] = [
    "RESOURCE_EXHAUSTED",
    "Individual quota reached",
    "Too Many Requests",
];

/// Parsed Gemini/agy limit hit (Source A of 6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeminiLimitHit {
    pub observed_at: u64,
    pub duration_secs: u64,
    pub resets_at: u64,
    pub is_weekly: bool,
    pub target_pool: String,
    pub family: Option<String>,
    pub summary: String,
}

fn reset_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Resets in (?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?").expect("valid reset regex")
    })
}

/// Parses `Resets in XhYmZs` into total seconds.
pub fn parse_reset_duration(text: &str) -> Option<u64> {
    let caps = reset_regex().captures(text)?;
    let h = caps
        .get(1)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let m = caps
        .get(2)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let s = caps
        .get(3)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let total = h
        .saturating_mul(3600)
        .saturating_add(m.saturating_mul(60))
        .saturating_add(s);
    (total > 0).then_some(total)
}

/// Pure parser for an `agy` limit error message (Source A of 6.3).
/// Never copies emails, tokens, or file paths into the returned summary.
pub fn parse_limit_message(text: &str, now: u64, family: Option<&str>) -> Option<GeminiLimitHit> {
    let matched = LIMIT_PATTERNS.iter().any(|p| text.contains(p));
    if !matched {
        return None;
    }
    let duration_secs = parse_reset_duration(text).unwrap_or(5 * 3600);
    let resets_at = now.saturating_add(duration_secs);
    let is_weekly = duration_secs > 24 * 3600;
    let target_pool = if is_weekly {
        "agy.week".to_owned()
    } else {
        "agy.window".to_owned()
    };
    let summary = if is_weekly {
        format!("agy weekly quota reached, resets_at {resets_at}")
    } else {
        format!("agy quota reached, resets_at {resets_at}")
    };
    Some(GeminiLimitHit {
        observed_at: now,
        duration_secs,
        resets_at,
        is_weekly,
        target_pool,
        family: family.map(str::to_owned),
        summary,
    })
}

/// Applies the pool and family pause rules of section 6.3.
/// - `D > 24h`: pauses `agy.week`.
/// - Otherwise: pauses the family meter until `resets_at`, and if a second family hit
///   arrived within 10 minutes (`600s`), pauses `agy.window` until the latest `resets_at`.
pub fn apply_limit_hit(
    window_meter: &mut Meter,
    week_meter: &mut Meter,
    family_meter: Option<&mut Meter>,
    hit: &GeminiLimitHit,
    prev_family_hit_at: Option<u64>,
) {
    let ev = StateEvent {
        resets_at: hit.resets_at,
        observed_at: hit.observed_at,
        message: hit.summary.clone(),
        family: hit.family.clone(),
    };

    if hit.is_weekly {
        week_meter.state_event = Some(ev);
        if hit.resets_at > week_meter.resets_at {
            week_meter.resets_at = hit.resets_at;
        }
        return;
    }

    if let Some(fm) = family_meter {
        fm.state_event = Some(ev.clone());
        if hit.resets_at > fm.resets_at {
            fm.resets_at = hit.resets_at;
        }
    }

    if let Some(prev) = prev_family_hit_at
        && hit.observed_at.saturating_sub(prev) <= 600
    {
        let latest_reset = window_meter
            .state_event
            .as_ref()
            .map_or(hit.resets_at, |cur| cur.resets_at.max(hit.resets_at));
        window_meter.state_event = Some(StateEvent {
            resets_at: latest_reset,
            observed_at: hit.observed_at,
            message: format!("agy pool paused after second family hit, resets_at {latest_reset}"),
            family: None,
        });
    }
}

/// Builds an inferred quota sample from accumulated units in the current window (Source B).
#[allow(clippy::too_many_arguments)]
pub fn inferred_sample(
    owner: &str,
    pool: &str,
    window: &str,
    units_in_window: f64,
    capacity: f64,
    resets_at: u64,
    now: u64,
    n_calibration: u64,
) -> QuotaSample {
    let cap = if capacity > 0.0 {
        capacity
    } else {
        SEED_CAPACITY_UNITS
    };
    let used_pct = (100.0 * units_in_window.max(0.0) / cap).min(150.0);
    let cal = (0.5 + 0.1 * (n_calibration as f64)).min(1.0);
    QuotaSample {
        owner: owner.to_owned(),
        vendor: "gemini".to_owned(),
        pool: pool.to_owned(),
        window: window.to_owned(),
        observed_at: now,
        used_pct,
        resets_at,
        source: Source::Inferred,
        confidence: Source::Inferred.base_confidence() * cal,
    }
}

/// Pure parser for `agy --print='...' --output-format json` (Source C of 6.3).
/// Verified on the hub PC (25 Nov 2026): returns `usage` with `input_tokens`,
/// `output_tokens`, `thinking_tokens`, `cache_read_tokens`, and `total_tokens`.
pub fn parse_agy_json_output(
    json_str: &str,
    owner: &str,
    now: u64,
    weights: &TokenWeights,
) -> Result<SpendRecord, String> {
    let v: Value = serde_json::from_str(json_str).map_err(|e| format!("invalid json: {e}"))?;
    let usage = v
        .get("usage")
        .ok_or_else(|| "missing usage in agy json output".to_owned())?;

    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let units = weights.weighted(input, cached, output);

    Ok(SpendRecord {
        owner: owner.to_owned(),
        pool: "agy.window".to_owned(),
        task_id: None,
        at: now,
        worker: None,
        model: None,
        units,
        input_tokens: input,
        cached_tokens: cached,
        output_tokens: output,
    })
}
