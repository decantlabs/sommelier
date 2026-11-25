//! Checks of a single report before it is projected.

use super::privacy::find_private;
use super::types::Report;

/// Limits of [`validate_report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportLimits {
    /// The one format version that is accepted.
    pub version: u32,
    /// Channels that exist.
    pub channels: Vec<String>,
    /// Most metrics in one report.
    pub max_metrics: usize,
    /// Most bytes of the log excerpt.
    pub max_log_bytes: usize,
    /// Longest id, metric name or error class in bytes.
    pub max_id_len: usize,
}

impl Default for ReportLimits {
    fn default() -> Self {
        Self {
            version: 1,
            channels: vec!["main".to_string(), "candidate".to_string()],
            max_metrics: 64,
            max_log_bytes: 16 * 1024,
            max_id_len: 64,
        }
    }
}

/// A plain id uses letters, digits, `.`, `_` and `-`.
fn plain_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A plain name uses lower case letters, digits, `.` and `_`.
fn plain_name(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_'))
}

/// Checks a report. The error is one line that names the field and says what to fix.
pub fn validate_report(r: &Report, limits: &ReportLimits) -> Result<(), String> {
    if r.v != limits.version {
        return Err(format!(
            "report.v is {} but only version {} is accepted",
            r.v, limits.version
        ));
    }
    if !limits.channels.iter().any(|c| c == &r.channel) {
        return Err(format!(
            "report.channel `{}` is unknown, use one of: {}",
            r.channel.chars().take(32).collect::<String>(),
            limits.channels.join(", ")
        ));
    }
    for (field, value) in [
        ("run", &r.run),
        ("member", &r.member),
        ("build", &r.build),
        ("profile", &r.profile),
        ("scenario", &r.scenario),
    ] {
        if !plain_id(value, limits.max_id_len) {
            return Err(format!(
                "report.{field} must be 1 to {} characters of letters, digits, `.`, `_` or `-`",
                limits.max_id_len
            ));
        }
    }
    if r.machine.os != "windows" && r.machine.os != "linux" {
        return Err("report.machine.os must be `windows` or `linux`".to_string());
    }
    if r.machine.gpu.trim().is_empty() || r.machine.gpu.len() > 128 {
        return Err("report.machine.gpu must be 1 to 128 characters".to_string());
    }
    for (field, value) in [
        ("machine.gpu", &r.machine.gpu),
        ("machine.driver", &r.machine.driver),
        ("machine.os_build", &r.machine.os_build),
    ] {
        if let Some(hit) = find_private(value).first() {
            return Err(format!(
                "report.{field} holds {}, remove it",
                hit.kind.label()
            ));
        }
    }
    if let Some(e) = &r.first_error
        && !plain_name(e, limits.max_id_len)
    {
        return Err(format!(
            "report.first_error must be a class of 1 to {} characters of a-z, 0-9, `.` or `_`",
            limits.max_id_len
        ));
    }
    if r.metrics.len() > limits.max_metrics {
        return Err(format!(
            "report.metrics holds {} entries, at most {} are allowed",
            r.metrics.len(),
            limits.max_metrics
        ));
    }
    for (name, value) in &r.metrics {
        if !plain_name(name, limits.max_id_len) {
            return Err(format!(
                "report.metrics name `{}` must be 1 to {} characters of a-z, 0-9, `.` or `_`",
                name.chars().take(32).collect::<String>(),
                limits.max_id_len
            ));
        }
        if !value.is_finite() {
            return Err(format!("report.metrics `{name}` must be a finite number"));
        }
    }
    if r.log_excerpt.len() > limits.max_log_bytes {
        return Err(format!(
            "report.log_excerpt is {} bytes, at most {} are allowed",
            r.log_excerpt.len(),
            limits.max_log_bytes
        ));
    }
    if let Some(hit) = find_private(&r.log_excerpt).first() {
        return Err(format!(
            "report.log_excerpt holds {} at byte {}, run scrub before sending",
            hit.kind.label(),
            hit.start
        ));
    }
    Ok(())
}
