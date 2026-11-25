//! Worker exit reason classification (sections 4.2, 5.3, 5.4, 6.1-6.3, and step 7).
//!
//! Exit classification order (section 5.4):
//! 1. killed by the supervisor (timeout, disk, run kill, stop) -> `killed`
//! 2. error file, event stream, or last message matches `vendor.<v>.limit_patterns` -> `quota`
//!    with the parsed reset time (`resets_at`, `reset_secs`)
//! 3. first line starts with `READY` and `check-result` passes -> `done`
//! 4. first line starts with `STUCK` -> `stuck`
//! 5. a handover was recorded -> `context_full`
//! 6. process gone without any of these -> `crash`, or `lost` when the supervisor itself restarted

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::capture::ExitCapture;

/// Canonical exit classification reasons from sections 4.2 and 5.4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Done,
    Stuck,
    Quota,
    ContextFull,
    Crash,
    Lost,
    Killed,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Done => "done",
            ExitReason::Stuck => "stuck",
            ExitReason::Quota => "quota",
            ExitReason::ContextFull => "context_full",
            ExitReason::Crash => "crash",
            ExitReason::Lost => "lost",
            ExitReason::Killed => "killed",
        }
    }
}

/// Complete classification verdict for a finished worker process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub reason: ExitReason,
    pub vendor: Option<String>,
    /// Reset timestamp in UTC epoch seconds when `reason == ExitReason::Quota`.
    pub resets_at: Option<u64>,
    /// Duration until reset in seconds when parsed from a relative duration (e.g. `Resets in 2h4m47s`).
    pub reset_secs: Option<u64>,
    /// Detail or failure note (e.g. why `check-result` failed or which limit pattern matched).
    pub detail: Option<String>,
}

/// Input parameters for classifying a worker's exit.
#[derive(Clone, Debug)]
pub struct ClassifyInput<'a> {
    pub capture: &'a ExitCapture,
    pub branch: &'a str,
    pub task: &'a str,
    pub exit_code: Option<i32>,
    pub killed_by_supervisor: bool,
    pub supervisor_restarted: bool,
    pub now_sec: u64,
    pub custom_limit_patterns: &'a [String],
}

/// Classifies how a worker ended according to the strict precedence of section 5.4.
pub fn classify_exit(input: &ClassifyInput<'_>) -> Classification {
    if input.killed_by_supervisor {
        return Classification {
            reason: ExitReason::Killed,
            vendor: None,
            resets_at: None,
            reset_secs: None,
            detail: Some("killed by supervisor".into()),
        };
    }

    if let Some(quota_hit) = detect_quota(
        &input.capture.err_text,
        &input.capture.events_text,
        &input.capture.last_message,
        input.now_sec,
        input.custom_limit_patterns,
    ) {
        return quota_hit;
    }

    let first = input.capture.first_line.trim();
    if first_word_is(first, "READY") {
        match check_ready_result(&input.capture.last_message, input.branch, input.task) {
            Ok(()) => {
                return Classification {
                    reason: ExitReason::Done,
                    vendor: None,
                    resets_at: None,
                    reset_secs: None,
                    detail: None,
                };
            }
            Err(why) => {
                return Classification {
                    reason: ExitReason::Stuck,
                    vendor: None,
                    resets_at: None,
                    reset_secs: None,
                    detail: Some(format!("result invalid: {why}")),
                };
            }
        }
    }

    if first_word_is(first, "STUCK") {
        return Classification {
            reason: ExitReason::Stuck,
            vendor: None,
            resets_at: None,
            reset_secs: None,
            detail: Some(first.to_owned()),
        };
    }

    if input.capture.handover_recorded
        || is_handover_text(&input.capture.last_message)
        || is_handover_text(&input.capture.err_text)
    {
        return Classification {
            reason: ExitReason::ContextFull,
            vendor: None,
            resets_at: None,
            reset_secs: None,
            detail: Some("handover recorded".into()),
        };
    }

    if input.supervisor_restarted {
        return Classification {
            reason: ExitReason::Lost,
            vendor: None,
            resets_at: None,
            reset_secs: None,
            detail: Some("supervisor restarted while process was gone".into()),
        };
    }

    Classification {
        reason: ExitReason::Crash,
        vendor: None,
        resets_at: None,
        reset_secs: None,
        detail: Some(format!("exit code {:?}", input.exit_code)),
    }
}

fn first_word_is(line: &str, expected: &str) -> bool {
    line.starts_with(expected)
        && line
            .get(expected.len()..)
            .is_none_or(|rest| rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric()))
}

fn is_handover_text(text: &str) -> bool {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    first_word_is(first, "HANDOVER") || first_word_is(first, "CONTEXT_FULL")
}

/// Validates a `READY` message using the rules of `check-result` (section 8.6 and `dispatch.rs`).
pub fn check_ready_result(text: &str, branch: &str, task: &str) -> Result<(), String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    if !first_word_is(first, "READY") {
        return Err("First nonempty line must start with READY".into());
    }
    if branch.is_empty() && task.is_empty() {
        return Ok(());
    }
    let end = text.char_indices().nth(400).map_or(text.len(), |(i, _)| i);
    let head_slice = &text[..end];
    let has_identity = token_names(head_slice)
        .any(|name| (!branch.is_empty() && name == branch) || (!task.is_empty() && name == task));
    if !has_identity {
        return Err(format!("First 400 characters must name {branch} or {task}"));
    }
    if !branch.is_empty() {
        if let Some(pushed) = branch_tokens(first).next()
            && pushed != branch
        {
            return Err(format!(
                "Found result branch {pushed}, expected {branch}. Line: {first}"
            ));
        }
        for line in std::iter::once(first).chain(lines) {
            let has_push = token_names(line)
                .any(|w| matches!(w.to_ascii_lowercase().as_str(), "push" | "pushed"));
            if (has_push || line.to_ascii_lowercase().starts_with("branch:"))
                && let Some(other) = branch_tokens(line).find(|b| *b != branch)
            {
                return Err(format!(
                    "Found branch {other}, expected {branch}. Line: {line}"
                ));
            }
        }
    }
    Ok(())
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | '%' | '=' | '#')
}

fn token_names(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c| !name_char(c))
        .map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
}

fn branch_tokens(text: &str) -> impl Iterator<Item = &str> {
    token_names(text).filter_map(|name| {
        let name = ["refs/heads/", "refs/remotes/origin/", "origin/"]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .unwrap_or(name);
        let (ns, rest) = name.split_once('/')?;
        (matches!(ns, "codex" | "gemini") && !rest.is_empty()).then_some(name)
    })
}

/// Detects quota limit messages across error output, JSON event streams, and short last messages
/// for Gemini (`agy`), Codex, and Claude (sections 5.3, 5.4, 6.1, 6.2, 6.3).
pub fn detect_quota(
    err_text: &str,
    events_text: &str,
    last_message: &str,
    now_sec: u64,
    custom_patterns: &[String],
) -> Option<Classification> {
    // Only inspect `last_message` for quota patterns if it does not begin with READY or STUCK.
    let first = last_message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let check_last = !first_word_is(first, "READY") && !first_word_is(first, "STUCK");

    let sources: [(&str, bool); 3] = [
        (err_text, true),
        (events_text, true),
        (last_message, check_last),
    ];

    for (src, enabled) in sources {
        if !enabled || src.trim().is_empty() {
            continue;
        }
        if let Some(c) = match_quota_in_text(src, now_sec, custom_patterns) {
            return Some(c);
        }
    }
    None
}

fn match_quota_in_text(
    text: &str,
    now_sec: u64,
    custom_patterns: &[String],
) -> Option<Classification> {
    // 1. Check custom patterns from policy (`vendor.<v>.limit_patterns`)
    for pat in custom_patterns {
        if !pat.is_empty() && text.contains(pat) {
            let (resets_at, reset_secs) = parse_reset_info(text, now_sec);
            return Some(Classification {
                reason: ExitReason::Quota,
                vendor: Some(infer_vendor(text).unwrap_or("custom").to_owned()),
                resets_at,
                reset_secs,
                detail: Some(pat.clone()),
            });
        }
    }

    // 2. Check structured JSON lines (Claude `rate_limit_event`, Codex `rateLimitReachedType` / error, AGY_ERROR)
    for line in text.lines() {
        let trimmed = line.trim();
        let json_part = trimmed
            .strip_prefix("AGY_ERROR:")
            .map(str::trim)
            .unwrap_or(trimmed);
        if json_part.starts_with('{')
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(json_part)
        {
            // Claude stream-json: {"type":"rate_limit_event","status":"rejected","resetsAt":...}
            if v["type"].as_str() == Some("rate_limit_event")
                && matches!(
                    v["status"].as_str(),
                    Some("rejected" | "rate_limited" | "exhausted")
                )
            {
                let (resets_at, reset_secs) = extract_json_reset(&v, now_sec)
                    .unwrap_or_else(|| parse_reset_info(text, now_sec));
                return Some(Classification {
                    reason: ExitReason::Quota,
                    vendor: Some("claude".into()),
                    resets_at,
                    reset_secs,
                    detail: Some("claude rate_limit_event rejected".into()),
                });
            }

            // Codex event or app-server limit reached
            if v.get("rateLimitReachedType").is_some_and(|x| !x.is_null())
                || v["error"]["code"].as_str() == Some("rate_limit_exceeded")
                || v["error"]["type"].as_str() == Some("usage_limit_reached")
            {
                let (resets_at, reset_secs) = extract_json_reset(&v, now_sec)
                    .unwrap_or_else(|| parse_reset_info(text, now_sec));
                return Some(Classification {
                    reason: ExitReason::Quota,
                    vendor: Some("codex".into()),
                    resets_at,
                    reset_secs,
                    detail: Some("codex rate limit reached".into()),
                });
            }
        }
    }

    // 3. Default vendor patterns (sections 5.3, 6.1, 6.2, 6.3, and gemini-swarm.sh)
    let gemini_patterns = [
        "RESOURCE_EXHAUSTED",
        "Individual quota reached",
        "individual quota reached",
        "AGY_ERROR",
    ];
    for pat in gemini_patterns {
        if text.contains(pat) {
            let (resets_at, reset_secs) = parse_reset_info(text, now_sec);
            return Some(Classification {
                reason: ExitReason::Quota,
                vendor: Some("gemini".into()),
                resets_at,
                reset_secs,
                detail: Some(pat.into()),
            });
        }
    }

    let codex_patterns = [
        "rateLimitReachedType",
        "usage_limit_reached",
        "You've hit your usage limit",
        "You have hit your usage limit",
        "codex rate limit",
    ];
    for pat in codex_patterns {
        if text.contains(pat) {
            let (resets_at, reset_secs) = parse_reset_info(text, now_sec);
            return Some(Classification {
                reason: ExitReason::Quota,
                vendor: Some("codex".into()),
                resets_at,
                reset_secs,
                detail: Some(pat.into()),
            });
        }
    }

    let claude_patterns = [
        "Claude AI usage limit reached",
        "Anthropic usage limit",
        "5-hour limit reached",
        "weekly limit reached",
    ];
    for pat in claude_patterns {
        if text.contains(pat) {
            let (resets_at, reset_secs) = parse_reset_info(text, now_sec);
            return Some(Classification {
                reason: ExitReason::Quota,
                vendor: Some("claude".into()),
                resets_at,
                reset_secs,
                detail: Some(pat.into()),
            });
        }
    }

    let generic_re = Regex::new(
        r"(?i)(quota\s+(has\s+been\s+)?(reached|exceeded)|rate[-_\s]?limit(ed)?\s+exceeded|too\s+many\s+requests)",
    )
    .ok()?;
    if let Some(m) = generic_re.find(text) {
        let (resets_at, reset_secs) = parse_reset_info(text, now_sec);
        let vendor = infer_vendor(text).unwrap_or("unknown").to_owned();
        return Some(Classification {
            reason: ExitReason::Quota,
            vendor: Some(vendor),
            resets_at,
            reset_secs,
            detail: Some(m.as_str().to_owned()),
        });
    }

    None
}

fn infer_vendor(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("agy_error")
        || lower.contains("resource_exhausted")
        || lower.contains("individual quota reached")
        || lower.contains("antigravity")
        || lower.contains("gemini")
    {
        Some("gemini")
    } else if lower.contains("claude") || lower.contains("anthropic") {
        Some("claude")
    } else if lower.contains("codex") || lower.contains("openai") || lower.contains("chatgpt") {
        Some("codex")
    } else {
        None
    }
}

fn extract_json_reset(v: &serde_json::Value, now_sec: u64) -> Option<(Option<u64>, Option<u64>)> {
    let candidates = [
        &v["resetsAt"],
        &v["resets_at"],
        &v["error"]["resets_at"],
        &v["error"]["resetsAt"],
        &v["rateLimits"]["primary"]["resetsAt"],
        &v["rate_limits"]["primary"]["resets_at"],
    ];
    for c in candidates {
        if let Some(ts) = c.as_u64() {
            let secs = if ts > 10_000_000_000 { ts / 1000 } else { ts };
            let dur = secs.saturating_sub(now_sec);
            return Some((Some(secs), Some(dur)));
        }
    }
    if let Some(secs) = v["error"]["reset_seconds"]
        .as_u64()
        .or_else(|| v["reset_seconds"].as_u64())
    {
        return Some((Some(now_sec + secs), Some(secs)));
    }
    None
}

/// Parses reset duration or epoch timestamp from vendor error text.
///
/// Supports:
/// - `Resets in (?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?` (section 5.3, e.g. `Resets in 2h4m47s`, `Resets in 167h13m26s`)
/// - `"resetsAt": 1795608947` or `"resets_at": 1795608947`
/// - `Claude AI usage limit reached|<epoch>`
pub fn parse_reset_info(text: &str, now_sec: u64) -> (Option<u64>, Option<u64>) {
    if let Some(dur) = parse_resets_in_duration(text) {
        return (Some(now_sec.saturating_add(dur)), Some(dur));
    }

    if let Ok(re) = Regex::new(r#"(?i)"?resets_?at"?\s*[:=]\s*(\d{9,13})"#)
        && let Some(caps) = re.captures(text)
        && let Some(m) = caps.get(1)
        && let Ok(raw) = m.as_str().parse::<u64>()
    {
        let ts = if raw > 10_000_000_000 {
            raw / 1000
        } else {
            raw
        };
        return (Some(ts), Some(ts.saturating_sub(now_sec)));
    }

    if let Ok(re) = Regex::new(r"usage limit reached\|(\d{9,13})")
        && let Some(caps) = re.captures(text)
        && let Some(m) = caps.get(1)
        && let Ok(raw) = m.as_str().parse::<u64>()
    {
        let ts = if raw > 10_000_000_000 {
            raw / 1000
        } else {
            raw
        };
        return (Some(ts), Some(ts.saturating_sub(now_sec)));
    }

    (None, None)
}

/// Parses `Resets in XhYmZs` into total seconds (section 5.3 and F3).
pub fn parse_resets_in_duration(text: &str) -> Option<u64> {
    let re = Regex::new(r"(?i)Resets\s+in\s+(?:(\d+)h)?\s*(?:(\d+)m)?\s*(?:(\d+)s)?").ok()?;
    for caps in re.captures_iter(text) {
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
        if caps.get(1).is_some() || caps.get(2).is_some() || caps.get(3).is_some() {
            return Some(h * 3600 + m * 60 + s);
        }
    }
    None
}
