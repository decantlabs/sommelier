//! Codex quota parsers and probe (section 6.2).
//!
//! Sources:
//! - A: `codex app-server` over stdio (`initialize`, `initialized`, `account/rateLimits/read`), 20 s timeout.
//! - B: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (`payload.type == "token_count"`).
//!
//! Never retains `accountId` or any email/token.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::meter::{QuotaSample, Source, SpendRecord, TokenWeights};

/// Parsed record from one Codex `token_count` rollout line.
#[derive(Clone, Debug, PartialEq)]
pub struct RolloutSample {
    pub samples: Vec<QuotaSample>,
    pub spend: Option<SpendRecord>,
    pub total_weighted_tokens: Option<f64>,
}

/// Parses an ISO-8601 UTC timestamp like `2026-11-25T11:43:59.410Z` or `2026-11-25T11:43:59Z`
/// into Unix epoch seconds.
pub fn parse_iso_epoch_secs(ts: &str) -> Option<u64> {
    let ts = ts.trim();
    if ts.len() < 19 {
        return None;
    }
    let normalized = if ts.len() == 20 && ts.ends_with('Z') {
        format!("{}.000Z", &ts[..19])
    } else if let Some(dot) = ts.find('.') {
        let base = &ts[..dot];
        let rest = ts[dot + 1..].trim_end_matches('Z');
        let ms = format!("{rest:0<3}");
        format!("{base}.{ms}Z", ms = &ms[..3])
    } else if ts.len() == 19 {
        format!("{ts}.000Z")
    } else {
        ts.to_owned()
    };
    crate::time::parse_ms(&normalized).map(|ms| ms / 1000)
}

fn window_name(mins: Option<u64>, default_name: &str) -> String {
    match mins {
        Some(300) => "5h".to_owned(),
        Some(10080) => "week".to_owned(),
        Some(1440) => "day".to_owned(),
        Some(m) if m > 0 => format!("{m}m"),
        _ => default_name.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_window_sample(
    obj: &Value,
    owner: &str,
    limit_id: &str,
    default_win: &str,
    observed_at: u64,
    used_key: &str,
    mins_key: &str,
    resets_key: &str,
) -> Option<QuotaSample> {
    let used_pct = obj.get(used_key)?.as_f64()?;
    if !used_pct.is_finite() {
        return None;
    }
    let mins = obj.get(mins_key).and_then(Value::as_u64);
    let win = window_name(mins, default_win);
    let resets_at = obj.get(resets_key).and_then(Value::as_u64).unwrap_or(0);
    Some(QuotaSample {
        owner: owner.to_owned(),
        vendor: "codex".to_owned(),
        pool: format!("{limit_id}.{win}"),
        window: win,
        observed_at,
        used_pct,
        resets_at,
        source: Source::Api,
        confidence: Source::Api.base_confidence(),
    })
}

/// Pure parser for a single line of a Codex session rollout `.jsonl` file (Source B).
/// Tolerates missing fields and ignores `accountId`.
pub fn parse_rollout_line(
    line: &str,
    owner: &str,
    weights: &TokenWeights,
) -> Option<RolloutSample> {
    let v: Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }

    let observed_at = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_epoch_secs)
        .unwrap_or(0);

    let mut samples = Vec::new();
    let mut limit_id = "codex";
    if let Some(rl) = payload.get("rate_limits") {
        if let Some(id) = rl.get("limit_id").and_then(Value::as_str)
            && !id.is_empty()
        {
            limit_id = id;
        }
        if let Some(primary) = rl.get("primary")
            && let Some(s) = parse_window_sample(
                primary,
                owner,
                limit_id,
                "5h",
                observed_at,
                "used_percent",
                "window_minutes",
                "resets_at",
            )
        {
            samples.push(s);
        }
        if let Some(secondary) = rl.get("secondary")
            && let Some(s) = parse_window_sample(
                secondary,
                owner,
                limit_id,
                "week",
                observed_at,
                "used_percent",
                "window_minutes",
                "resets_at",
            )
        {
            samples.push(s);
        }
    }

    let info = payload.get("info");
    let spend = info
        .and_then(|i| i.get("last_token_usage"))
        .and_then(|u| parse_token_usage(u, owner, &format!("{limit_id}.5h"), observed_at, weights));
    let total_weighted_tokens = info
        .and_then(|i| i.get("total_token_usage"))
        .and_then(|u| parse_token_usage(u, owner, &format!("{limit_id}.5h"), observed_at, weights))
        .map(|s| s.units);

    if samples.is_empty() && spend.is_none() {
        return None;
    }

    Some(RolloutSample {
        samples,
        spend,
        total_weighted_tokens,
    })
}

fn parse_token_usage(
    u: &Value,
    owner: &str,
    pool: &str,
    at: u64,
    weights: &TokenWeights,
) -> Option<SpendRecord> {
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    if input == 0 && cached == 0 && output == 0 {
        return None;
    }
    let units = weights.weighted(input, cached, output);
    Some(SpendRecord {
        owner: owner.to_owned(),
        pool: pool.to_owned(),
        task_id: None,
        at,
        worker: None,
        model: None,
        units,
        input_tokens: input,
        cached_tokens: cached,
        output_tokens: output,
    })
}

/// Pure parser for `codex app-server` `account/rateLimits/read` JSON-RPC response (Source A).
/// Drops `accountId` and tolerates missing fields.
pub fn parse_appserver_response(
    line: &str,
    owner: &str,
    now: u64,
) -> Result<Vec<QuotaSample>, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("app-server error: {err}"));
    }
    let result = v
        .get("result")
        .ok_or_else(|| "missing result in app-server response".to_owned())?;

    let mut samples = Vec::new();
    let collect_bucket = |bucket: &Value, fallback_id: &str, out: &mut Vec<QuotaSample>| {
        let limit_id = bucket
            .get("limitId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_id);
        if let Some(primary) = bucket.get("primary")
            && let Some(s) = parse_window_sample(
                primary,
                owner,
                limit_id,
                "5h",
                now,
                "usedPercent",
                "windowDurationMins",
                "resetsAt",
            )
        {
            out.push(s);
        }
        if let Some(secondary) = bucket.get("secondary")
            && let Some(s) = parse_window_sample(
                secondary,
                owner,
                limit_id,
                "week",
                now,
                "usedPercent",
                "windowDurationMins",
                "resetsAt",
            )
        {
            out.push(s);
        }
    };

    if let Some(map) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (id, bucket) in map {
            collect_bucket(bucket, id, &mut samples);
        }
    }
    if samples.is_empty()
        && let Some(rl) = result.get("rateLimits")
    {
        collect_bucket(rl, "codex", &mut samples);
    }

    Ok(samples)
}

/// Default Codex sessions directory (`~/.codex/sessions`).
pub fn default_sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Reads the newest samples per pool from rollout `.jsonl` files under `sessions_dir`.
pub fn read_latest_from_sessions(
    sessions_dir: &Path,
    owner: &str,
    weights: &TokenWeights,
    max_files: usize,
) -> Result<Vec<QuotaSample>, String> {
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_jsonl_files(sessions_dir, &mut files)?;
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    files.truncate(max_files);

    let mut by_pool: std::collections::BTreeMap<String, QuotaSample> =
        std::collections::BTreeMap::new();
    for (_, path) in files {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            if !line.contains("\"token_count\"") {
                continue;
            }
            if let Some(parsed) = parse_rollout_line(line, owner, weights) {
                for s in parsed.samples {
                    let replace = by_pool
                        .get(&s.pool)
                        .is_none_or(|cur| s.observed_at >= cur.observed_at);
                    if replace {
                        by_pool.insert(s.pool.clone(), s);
                    }
                }
            }
        }
    }
    Ok(by_pool.into_values().collect())
}

fn collect_jsonl_files(
    dir: &Path,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = collect_jsonl_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            out.push((mtime, path));
        }
    }
    Ok(())
}

/// Probes `codex app-server` with a 20 s timeout, falling back to session files on failure/timeout.
pub fn probe_codex(
    owner: &str,
    now: u64,
    timeout_secs: u64,
    sessions_dir: Option<&Path>,
) -> Result<(Vec<QuotaSample>, &'static str), String> {
    match probe_appserver(owner, now, timeout_secs) {
        Ok(samples) if !samples.is_empty() => Ok((samples, "app-server")),
        _ => {
            let dir_owned = sessions_dir
                .map(PathBuf::from)
                .or_else(default_sessions_dir);
            let Some(dir) = dir_owned else {
                return Err("codex app-server failed and no sessions dir found".to_owned());
            };
            let samples = read_latest_from_sessions(&dir, owner, &TokenWeights::default(), 25)?;
            if samples.is_empty() {
                Err("no codex rate limit samples found".to_owned())
            } else {
                Ok((samples, "session-files"))
            }
        }
    }
}

fn spawn_codex_appserver() -> std::io::Result<std::process::Child> {
    let candidates: &[&str] = if cfg!(windows) {
        &["codex.cmd", "codex"]
    } else {
        &["codex"]
    };
    let mut last_err = None;
    for bin in candidates {
        match Command::new(bin)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("codex binary not found")))
}

fn probe_appserver(owner: &str, now: u64, timeout_secs: u64) -> Result<Vec<QuotaSample>, String> {
    let mut child = spawn_codex_appserver().map_err(|e| format!("spawn codex app-server: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "missing stdin on codex app-server".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing stdout on codex app-server".to_owned())?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"sommelier","version":"0.1.0"}}}"#;
    writeln!(stdin, "{init}").map_err(|e| format!("write init: {e}"))?;
    let _ = stdin.flush();

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut got_init = false;
    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(rem.min(Duration::from_millis(250))) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && v.get("id").and_then(Value::as_u64) == Some(1)
                {
                    got_init = true;
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if !got_init {
        let _ = child.kill();
        let _ = child.wait();
        return Err("codex app-server initialize timed out".to_owned());
    }

    let initialized = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    let read_req = r#"{"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read","params":{}}"#;
    writeln!(stdin, "{initialized}").map_err(|e| format!("write initialized: {e}"))?;
    writeln!(stdin, "{read_req}").map_err(|e| format!("write read: {e}"))?;
    let _ = stdin.flush();

    let mut result_line = None;
    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(rem.min(Duration::from_millis(250))) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && v.get("id").and_then(Value::as_u64) == Some(2)
                {
                    result_line = Some(line);
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    let line =
        result_line.ok_or_else(|| "codex app-server rateLimits/read timed out".to_owned())?;
    parse_appserver_response(&line, owner, now)
}
