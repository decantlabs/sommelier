//! Claude Code quota parsers (section 6.1).
//!
//! Sources:
//! - A: Status line JSON piped to `sommelier quota ingest claude-statusline`.
//! - B: `rate_limit_event` lines in `claude -p --output-format stream-json`.
//! - D: Result JSON of `claude -p --output-format json` (`usage`, `modelUsage`, `total_cost_usd`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::meter::{QuotaSample, Source, SpendRecord, StateEvent, TokenWeights};

/// Persisted state in `quota/claude.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeDropState {
    pub last_api_duration_ms: Option<u64>,
    pub last_observed_at: u64,
    pub samples: Vec<QuotaSample>,
}

/// Output of parsing Claude's status line JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeStatusResult {
    pub state: ClaudeDropState,
    pub bar_line: String,
    pub fresh: bool,
}

/// Parsed `rate_limit_event` from Claude stream-json.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeStreamEvent {
    pub status: String,
    pub state_event: Option<StateEvent>,
    pub sample: Option<QuotaSample>,
}

/// Pure parser for Claude Code status line JSON (Source A of 6.1).
/// Tolerates missing `five_hour`, `seven_day`, or `cost`.
pub fn parse_statusline(
    json_str: &str,
    owner: &str,
    now: u64,
    prev: Option<&ClaudeDropState>,
) -> Result<ClaudeStatusResult, String> {
    let v: Value = serde_json::from_str(json_str).map_err(|e| format!("invalid json: {e}"))?;

    let duration_ms = v
        .get("cost")
        .and_then(|c| c.get("total_api_duration_ms"))
        .and_then(Value::as_u64);

    let (observed_at, fresh) = match (prev, duration_ms) {
        (Some(p), Some(dur)) if p.last_api_duration_ms.is_some_and(|old| dur <= old) => {
            (p.last_observed_at, false)
        }
        (Some(p), None) if p.last_observed_at > 0 => (p.last_observed_at, false),
        _ => (now, true),
    };

    let mut samples = Vec::new();
    if let Some(rl) = v.get("rate_limits") {
        if let Some(fh) = rl.get("five_hour")
            && let Some(pct) = fh.get("used_percentage").and_then(Value::as_f64)
            && pct.is_finite()
        {
            samples.push(QuotaSample {
                owner: owner.to_owned(),
                vendor: "claude".to_owned(),
                pool: "claude.5h".to_owned(),
                window: "5h".to_owned(),
                observed_at,
                used_pct: pct,
                resets_at: fh.get("resets_at").and_then(Value::as_u64).unwrap_or(0),
                source: Source::Api,
                confidence: Source::Api.base_confidence(),
            });
        }
        if let Some(sd) = rl.get("seven_day")
            && let Some(pct) = sd.get("used_percentage").and_then(Value::as_f64)
            && pct.is_finite()
        {
            samples.push(QuotaSample {
                owner: owner.to_owned(),
                vendor: "claude".to_owned(),
                pool: "claude.week".to_owned(),
                window: "week".to_owned(),
                observed_at,
                used_pct: pct,
                resets_at: sd.get("resets_at").and_then(Value::as_u64).unwrap_or(0),
                source: Source::Api,
                confidence: Source::Api.base_confidence(),
            });
        }
    }

    let bar_line = if samples.is_empty() {
        "claude: no meter".to_owned()
    } else {
        let parts: Vec<String> = samples
            .iter()
            .map(|s| format!("{}:{:.0}%", s.window, s.used_pct))
            .collect();
        format!("claude {}", parts.join(" "))
    };

    let state = ClaudeDropState {
        last_api_duration_ms: duration_ms.or_else(|| prev.and_then(|p| p.last_api_duration_ms)),
        last_observed_at: observed_at,
        samples,
    };

    Ok(ClaudeStatusResult {
        state,
        bar_line,
        fresh,
    })
}

/// Pure parser for a `rate_limit_event` line in `claude -p --output-format stream-json` (Source B).
pub fn parse_stream_rate_limit_event(
    line: &str,
    owner: &str,
    now: u64,
) -> Option<ClaudeStreamEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ev = if v.get("type").and_then(Value::as_str) == Some("rate_limit_event") {
        v.get("rate_limit_info").unwrap_or(&v)
    } else {
        return None;
    };

    let status = ev
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let resets_at = ev.get("resetsAt").and_then(Value::as_u64).unwrap_or(0);
    let rate_type = ev
        .get("rateLimitType")
        .and_then(Value::as_str)
        .unwrap_or("five_hour");
    let (pool, window) = match rate_type {
        "seven_day" | "week" => ("claude.week", "week"),
        _ => ("claude.5h", "5h"),
    };

    let state_event = if status == "rejected" && resets_at > 0 {
        Some(StateEvent {
            resets_at,
            observed_at: now,
            message: format!("claude {rate_type} rejected"),
            family: None,
        })
    } else {
        None
    };

    let sample = ev
        .get("utilization")
        .and_then(Value::as_f64)
        .filter(|u| u.is_finite() && *u >= 0.0)
        .map(|u| {
            let used_pct = if u <= 1.0 && u > 0.0 { u * 100.0 } else { u };
            QuotaSample {
                owner: owner.to_owned(),
                vendor: "claude".to_owned(),
                pool: pool.to_owned(),
                window: window.to_owned(),
                observed_at: now,
                used_pct,
                resets_at,
                source: Source::Api,
                confidence: Source::Api.base_confidence(),
            }
        });

    Some(ClaudeStreamEvent {
        status,
        state_event,
        sample,
    })
}

/// Pure parser for `claude -p --output-format json` result spend (Source D).
pub fn parse_result_spend(
    json_str: &str,
    owner: &str,
    now: u64,
    weights: &TokenWeights,
) -> Result<Vec<SpendRecord>, String> {
    let v: Value = serde_json::from_str(json_str).map_err(|e| format!("invalid json: {e}"))?;
    let mut records = Vec::new();

    if let Some(models) = v.get("modelUsage").and_then(Value::as_object) {
        for (model, mu) in models {
            let input = mu.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
            let cached = mu
                .get("cacheReadInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = mu.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
            let units = weights.weighted(input, cached, output);
            records.push(SpendRecord {
                owner: owner.to_owned(),
                pool: "claude.week".to_owned(),
                task_id: None,
                at: now,
                worker: None,
                model: Some(model.clone()),
                units,
                input_tokens: input,
                cached_tokens: cached,
                output_tokens: output,
            });
        }
    }

    if records.is_empty()
        && let Some(u) = v.get("usage")
    {
        let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let units = weights.weighted(input, cached, output);
        records.push(SpendRecord {
            owner: owner.to_owned(),
            pool: "claude.week".to_owned(),
            task_id: None,
            at: now,
            worker: None,
            model: None,
            units,
            input_tokens: input,
            cached_tokens: cached,
            output_tokens: output,
        });
    }

    Ok(records)
}
