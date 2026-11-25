//! Task cost and duration estimates (section 7.5).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimateSource {
    Default,
    Learned,
}

/// Duration and unit cost estimate for a `(kind, class)` pair (table ESTIMATE).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub kind: String,
    pub class: String,
    pub n: u64,
    pub minutes_p50: f64,
    pub minutes_p90: f64,
    pub units_p50: f64,
    pub units_p90: f64,
    pub source: EstimateSource,
}

impl Estimate {
    /// Policy default estimate for a class when no learned data has `n >= 5`.
    pub fn default_for(kind: &str, class: &str) -> Self {
        let is_gemini = class.contains("gemini");
        Self {
            kind: kind.to_owned(),
            class: class.to_owned(),
            n: 0,
            minutes_p50: 30.0,
            minutes_p90: 75.0,
            units_p50: if is_gemini { 60.0 } else { 400_000.0 },
            units_p90: if is_gemini { 150.0 } else { 1_500_000.0 },
            source: EstimateSource::Default,
        }
    }
}

/// Returns the active estimate: learned when `n >= 5`, otherwise policy default,
/// scaled by `task_size` (where `1.0` is normal size).
pub fn estimate(
    kind: &str,
    class: &str,
    learned: Option<&Estimate>,
    default_est: &Estimate,
    task_size: f64,
) -> Estimate {
    let base = match learned {
        Some(l) if l.n >= 5 => l,
        _ => default_est,
    };
    let scale = if task_size.is_finite() && task_size > 0.0 {
        task_size
    } else {
        1.0
    };
    Estimate {
        kind: kind.to_owned(),
        class: class.to_owned(),
        n: base.n,
        minutes_p50: base.minutes_p50 * scale,
        minutes_p90: base.minutes_p90 * scale,
        units_p50: base.units_p50 * scale,
        units_p90: base.units_p90 * scale,
        source: base.source,
    }
}
