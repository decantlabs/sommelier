//! Core quota meter data types and sanitization helpers (sections 3, 4.6, 5.3, 6.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Origin of a quota sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Inferred,
    Manual,
    Api,
}

impl Source {
    /// Priority rank for choosing the best sample: api > manual > inferred.
    pub fn rank(self) -> u8 {
        match self {
            Self::Inferred => 1,
            Self::Manual => 2,
            Self::Api => 3,
        }
    }

    /// Base confidence of a sample from this source (section 4.6).
    pub fn base_confidence(self) -> f64 {
        match self {
            Self::Api => 0.9,
            Self::Manual => 0.6,
            Self::Inferred => 0.4,
        }
    }
}

/// Confidence band derived from numeric confidence (section 4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfidenceBand {
    High,
    Medium,
    Low,
}

impl ConfidenceBand {
    pub fn from_conf(conf: f64) -> Self {
        if conf >= 0.8 {
            Self::High
        } else if conf >= 0.5 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// Derived state of a quota pool (sections 3, 4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolState {
    Open,
    Throttled,
    Drain,
    Closed,
    Paused,
    Unknown,
}

impl PoolState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Throttled => "throttled",
            Self::Drain => "drain",
            Self::Closed => "closed",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
        }
    }
}

/// One quota observation (table QUOTA_SAMPLE).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub observed_at: u64,
    pub used_pct: f64,
    pub resets_at: u64,
    pub source: Source,
    pub confidence: f64,
}

/// Vendor limit message and its parsed reset time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateEvent {
    pub resets_at: u64,
    pub observed_at: u64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
}

/// Per-meter policy parameters (section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterPolicy {
    pub vendor: String,
    pub window: String,
    pub stop_at: f64,
    pub reserve_pct: f64,
    pub pace: bool,
    pub drain_min: u64,
    pub drain_band_pct: f64,
    pub fresh_min: u64,
    pub burn_window_min: u64,
    pub window_duration_sec: u64,
}

impl MeterPolicy {
    pub fn default_for(vendor: &str, window: &str) -> Self {
        let is_week = window == "week" || window == "7d";
        Self {
            vendor: vendor.to_owned(),
            window: window.to_owned(),
            stop_at: if is_week { 80.0 } else { 90.0 },
            reserve_pct: if is_week { 20.0 } else { 10.0 },
            pace: is_week,
            drain_min: if is_week { 120 } else { 30 },
            drain_band_pct: if is_week { 3.0 } else { 5.0 },
            fresh_min: if is_week { 30 } else { 5 },
            burn_window_min: if is_week { 120 } else { 30 },
            window_duration_sec: default_window_secs(window),
        }
    }
}

/// Default duration in seconds for a named window.
pub fn default_window_secs(window: &str) -> u64 {
    match window {
        "5h" | "window" => 5 * 3600,
        "day" | "24h" => 24 * 3600,
        "week" | "7d" => 7 * 24 * 3600,
        _ => {
            if let Some(mins) = window.strip_suffix('m').and_then(|s| s.parse::<u64>().ok()) {
                mins.saturating_mul(60)
            } else if let Some(hrs) = window.strip_suffix('h').and_then(|s| s.parse::<u64>().ok()) {
                hrs.saturating_mul(3600)
            } else {
                5 * 3600
            }
        }
    }
}

/// Token weighting for spend calculation (section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenWeights {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

impl Default for TokenWeights {
    fn default() -> Self {
        Self {
            input: 1.0,
            cached: 0.1,
            output: 4.0,
        }
    }
}

impl TokenWeights {
    pub fn weighted(&self, input_tokens: u64, cached_tokens: u64, output_tokens: u64) -> f64 {
        let non_cached_input = input_tokens.saturating_sub(cached_tokens) as f64;
        non_cached_input * self.input
            + (cached_tokens as f64) * self.cached
            + (output_tokens as f64) * self.output
    }
}

/// Global `[quota]` policy settings (section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaConfig {
    pub margin_high: f64,
    pub margin_medium: f64,
    pub margin_low: f64,
    pub min_conf: f64,
    pub throttle_slack: f64,
    pub drain_short_min: f64,
    pub token_weights: TokenWeights,
    pub effort_weights: BTreeMap<String, f64>,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        let mut effort_weights = BTreeMap::new();
        effort_weights.insert("low".to_owned(), 1.0);
        effort_weights.insert("medium".to_owned(), 2.0);
        effort_weights.insert("high".to_owned(), 4.0);
        effort_weights.insert("xhigh".to_owned(), 8.0);
        Self {
            margin_high: 1.2,
            margin_medium: 1.5,
            margin_low: 2.0,
            min_conf: 0.2,
            throttle_slack: 1.25,
            drain_short_min: 10.0,
            token_weights: TokenWeights::default(),
            effort_weights,
        }
    }
}

impl QuotaConfig {
    pub fn margin_for(&self, band: ConfidenceBand) -> f64 {
        match band {
            ConfidenceBand::High => self.margin_high,
            ConfidenceBand::Medium => self.margin_medium,
            ConfidenceBand::Low => self.margin_low,
        }
    }

    pub fn effort_weight(&self, effort: &str) -> f64 {
        self.effort_weights.get(effort).copied().unwrap_or(2.0)
    }
}

/// Full meter state for one `(owner, pool, window)` (table METER).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub policy: MeterPolicy,
    pub series: Vec<QuotaSample>,
    pub latest: Option<QuotaSample>,
    pub resets_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_event: Option<StateEvent>,
    pub unit: String,
    pub pct_per_unit: f64,
    pub n_calibration: u64,
    pub capacity_units: f64,
    pub probe_enabled: bool,
}

impl Meter {
    pub fn new(owner: &str, vendor: &str, pool: &str, window: &str) -> Self {
        Self {
            owner: owner.to_owned(),
            vendor: vendor.to_owned(),
            pool: pool.to_owned(),
            window: window.to_owned(),
            policy: MeterPolicy::default_for(vendor, window),
            series: Vec::new(),
            latest: None,
            resets_at: 0,
            state_event: None,
            unit: if vendor == "gemini" {
                "worker_minutes".to_owned()
            } else {
                "weighted_tokens".to_owned()
            },
            pct_per_unit: if vendor == "gemini" {
                100.0 / 3000.0
            } else {
                0.0001
            },
            n_calibration: 0,
            capacity_units: if vendor == "gemini" { 3000.0 } else { 0.0 },
            probe_enabled: true,
        }
    }
}

/// Spend record produced by vendor parsers (table SPEND).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpendRecord {
    pub owner: String,
    pub pool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub units: f64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

/// Compact meter row for `quota/meters.json` and `sommelier quota show` (section 6.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterRow {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub used_pct: f64,
    pub resets_at: u64,
    pub source: Source,
    pub confidence: f64,
    pub observed_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PoolState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<ConfidenceBand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_pct_per_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_line: Option<u64>,
}

/// Returns true if a serialized output contains sensitive markers that must never appear.
pub fn contains_sensitive_text(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if lower.contains("accountid") || lower.contains("account_id") || s.contains('@') {
        return true;
    }
    const PREFIXES: [&str; 6] = ["sk-", "ghp_", "gho_", "aiza", "ya29.", "bearer "];
    PREFIXES.iter().any(|p| lower.contains(p))
}
