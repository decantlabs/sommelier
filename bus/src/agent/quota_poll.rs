//! Periodic quota poller inside the supervisor (`sommelier agent`, section 6.4 and step 9).
//!
//! Section 6.4 poller schedule:
//! - every 10 s: `codex_rollout` — tail new bytes of session files -> samples + `SPEND` (keyed by file and byte offset)
//! - every 60 s: `claude_drop` — read `quota/claude.json` written by the status line command -> samples
//!   and `agy_spend` — minutes of live `agy` children by family and effort -> `SPEND` + inferred sample
//! - on child output: `claude_stream` (`rate_limit_event`), `agy_message` (limit pattern) -> state events
//! - every 5 min: `codex_appserver` when no codex sample is newer than 5 minutes (and before a lease when older than 2 min)
//! - report: op `quota.report` with changed meters and new spend. Unreachable hub -> outbox.
//!
//! Fault tolerance (section 6.4 & 10.3):
//! - Offsets are persisted in `<data_dir>/quota/poll_offsets.json` so after a supervisor restart
//!   each sample and spend record is reported once.
//! - Spend is keyed by `(file_key, byte_offset)` to prevent double counting when a session file is read twice.
//! - A probe that fails to parse three times in a row turns itself off, the meter falls back to `inferred`,
//!   and one `PROBE_BROKEN` alert is raised.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::children::ChildrenStore;
use crate::agent::outbox::Outbox;
use crate::client::Opts;
use crate::msg::Message;
use crate::msg::Role;
use crate::quota::apply::{
    QuotaReportOp, ReportedProbeBroken, ReportedSpend, ReportedStateEvent,
    validate_and_build_report_messages,
};
use crate::quota::claude::{self, ClaudeDropState};
use crate::quota::codex;
use crate::quota::forecast::{forecast, on_sample, pool_state};
use crate::quota::gemini::{self, SEED_CAPACITY_UNITS};
use crate::quota::meter::{
    Meter, PoolState, QuotaConfig, QuotaSample, Source, SpendRecord, StateEvent, TokenWeights,
};

/// Maximum consecutive failures before a probe turns itself off and raises one `PROBE_BROKEN` alert (section 6.4).
pub const MAX_PROBE_FAILURES: u32 = 3;

/// Minimum interval (10 minutes = 600 s) for re-reporting an unchanged meter sample (section 5.2).
pub const REPORT_HEARTBEAT_SECS: u64 = 600;

/// Minimum age (5 minutes = 300 s) of the latest Codex sample before polling `codex app-server` on the 5-min timer (section 6.4).
pub const CODEX_APPSERVER_STALE_SECS: u64 = 300;

/// Minimum age (2 minutes = 120 s) of the latest Codex sample before polling `codex app-server` prior to a lease (section 6.2).
pub const CODEX_PRE_LEASE_STALE_SECS: u64 = 120;

/// Persisted state of one reported meter used for deduplication across poller ticks and restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportedMeterDigest {
    pub used_pct: f64,
    pub resets_at: u64,
    pub state: PoolState,
    pub source: Source,
    pub observed_at: u64,
    pub reported_at: u64,
}

/// Persisted state of one vendor probe (failure count, disabled flag, alert raised flag).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeHealth {
    pub consecutive_failures: u32,
    pub disabled: bool,
    pub alert_raised: bool,
    pub last_error: Option<String>,
}

/// Persistent poller state stored at `<data_dir>/quota/poll_offsets.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PollerPersistedState {
    /// Relative or canonical file key -> byte offset already processed.
    pub file_offsets: BTreeMap<String, u64>,
    /// Last observed timestamp of `quota/claude.json` already reported.
    pub last_claude_observed_at: u64,
    /// Persisted Claude drop state (tracks `last_api_duration_ms` and `last_observed_at`).
    #[serde(default)]
    pub claude_drop_state: Option<ClaudeDropState>,
    /// `(worker_name, started_at_sec)` -> last accounted timestamp (`now_sec`) for live `agy` children.
    pub agy_child_accounted_at: BTreeMap<String, u64>,
    /// Accumulated Gemini units in the current 5-hour window.
    pub agy_window_units: f64,
    /// Accumulated Gemini units in the current day.
    pub agy_day_units: f64,
    /// Reset timestamp of the current Gemini 5-hour window.
    pub agy_window_resets_at: u64,
    /// Timestamp of the previous Gemini family limit hit (for the 10-minute two-family pool pause rule).
    pub last_gemini_hit_at: Option<u64>,
    /// Probe health by probe identifier (`"codex_rollout"`, `"codex_appserver"`, `"claude_drop"`, `"claude_stream"`).
    pub probes: BTreeMap<String, ProbeHealth>,
    /// Digest of the last reported sample per pool (`pool -> ReportedMeterDigest`).
    pub reported_meters: BTreeMap<String, ReportedMeterDigest>,
    /// Known meters with their sample series.
    pub meters: BTreeMap<String, Meter>,
}

/// Configuration for the supervisor's [`QuotaPoller`].
#[derive(Clone, Debug)]
pub struct QuotaPollerConfig {
    pub owner: String,
    pub data_dir: PathBuf,
    pub codex_sessions_dir: Option<PathBuf>,
    pub claude_drop_path: Option<PathBuf>,
    pub quota_config: QuotaConfig,
    pub probe_codex_enabled: bool,
    pub probe_claude_enabled: bool,
    pub probe_gemini_enabled: bool,
}

impl QuotaPollerConfig {
    pub fn new(owner: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let claude_drop = data_dir.join("quota").join("claude.json");
        Self {
            owner: owner.into(),
            codex_sessions_dir: None,
            claude_drop_path: Some(claude_drop),
            data_dir,
            quota_config: QuotaConfig::default(),
            probe_codex_enabled: true,
            probe_claude_enabled: true,
            probe_gemini_enabled: true,
        }
    }

    pub fn with_codex_sessions_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.codex_sessions_dir = Some(dir.into());
        self
    }

    pub fn with_claude_drop_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_drop_path = Some(path.into());
        self
    }
}

/// The supervisor quota poller (section 6.4).
pub struct QuotaPoller {
    cfg: QuotaPollerConfig,
    state: PollerPersistedState,
}

impl QuotaPoller {
    /// Opens a `QuotaPoller`, restoring its persisted offsets and probe states from
    /// `<data_dir>/quota/poll_offsets.json` if present.
    pub fn open(cfg: QuotaPollerConfig) -> Result<Self, String> {
        let state = Self::load_state(&cfg.data_dir)?;
        Ok(Self { cfg, state })
    }

    pub fn config(&self) -> &QuotaPollerConfig {
        &self.cfg
    }

    pub fn state(&self) -> &PollerPersistedState {
        &self.state
    }

    /// Path to `<data_dir>/quota/poll_offsets.json`.
    pub fn state_path(data_dir: &Path) -> PathBuf {
        data_dir.join("quota").join("poll_offsets.json")
    }

    fn load_state(data_dir: &Path) -> Result<PollerPersistedState, String> {
        let path = Self::state_path(data_dir);
        if !path.exists() {
            return Ok(PollerPersistedState::default());
        }
        let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// Persists the poller's offsets, meter series, and probe health atomically to disk.
    pub fn save_state(&self) -> Result<(), String> {
        let path = Self::state_path(&self.cfg.data_dir);
        let dir = path.parent().unwrap_or(&self.cfg.data_dir);
        fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(&self.state).map_err(|e| e.to_string())?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        tmp.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        tmp.as_file().sync_all().map_err(|e| e.to_string())?;
        tmp.persist(&path).map_err(|e| e.error.to_string())?;
        Ok(())
    }

    /// Returns whether `probe_name` is active (enabled in config and not auto-disabled by 3 failures).
    pub fn is_probe_active(&self, probe_name: &str) -> bool {
        let vendor_enabled = if probe_name.starts_with("codex") {
            self.cfg.probe_codex_enabled
        } else if probe_name.starts_with("claude") {
            self.cfg.probe_claude_enabled
        } else if probe_name.starts_with("gemini") || probe_name.starts_with("agy") {
            self.cfg.probe_gemini_enabled
        } else {
            true
        };
        if !vendor_enabled {
            return false;
        }
        !self
            .state
            .probes
            .get(probe_name)
            .is_some_and(|p| p.disabled)
    }

    fn record_probe_success(&mut self, probe_name: &str) {
        let h = self.state.probes.entry(probe_name.to_owned()).or_default();
        h.consecutive_failures = 0;
        h.last_error = None;
    }

    /// Records a parse/probe failure. On the 3rd consecutive failure, disables the probe,
    /// switches its meters to `probe_enabled = false` and `inferred` source fallback,
    /// and emits exactly one `ReportedProbeBroken` entry (which raises one `PROBE_BROKEN` alert).
    #[allow(clippy::too_many_arguments)]
    pub fn record_probe_failure(
        &mut self,
        probe_name: &str,
        vendor: &str,
        pools: &[&str],
        error: impl Into<String>,
        now_sec: u64,
        out_broken: &mut Vec<ReportedProbeBroken>,
        out_samples: &mut Vec<QuotaSample>,
    ) {
        let err_s = error.into();
        let h = self.state.probes.entry(probe_name.to_owned()).or_default();
        if h.disabled {
            return;
        }
        h.consecutive_failures += 1;
        h.last_error = Some(err_s.clone());

        if h.consecutive_failures >= MAX_PROBE_FAILURES {
            h.disabled = true;
            let pool_strings: Vec<String> = pools.iter().map(|p| (*p).to_owned()).collect();
            for p in &pool_strings {
                let win = if p.ends_with(".week") { "week" } else { "5h" };
                let m = self
                    .state
                    .meters
                    .entry(p.clone())
                    .or_insert_with(|| Meter::new(&self.cfg.owner, vendor, p, win));
                m.probe_enabled = false;
                let prev_pct = m.latest.as_ref().map_or(0.0, |s| s.used_pct);
                let prev_resets = m.resets_at.max(now_sec + 5 * 3600);
                let fallback = QuotaSample {
                    owner: self.cfg.owner.clone(),
                    vendor: vendor.to_owned(),
                    pool: p.clone(),
                    window: win.to_owned(),
                    observed_at: now_sec,
                    used_pct: prev_pct,
                    resets_at: prev_resets,
                    source: Source::Inferred,
                    confidence: Source::Inferred.base_confidence(),
                };
                m.series.clear();
                on_sample(m, fallback.clone());
                out_samples.push(fallback);
            }
            if !h.alert_raised {
                h.alert_raised = true;
                out_broken.push(ReportedProbeBroken {
                    owner: self.cfg.owner.clone(),
                    vendor: vendor.to_owned(),
                    probe: probe_name.to_owned(),
                    pools: pool_strings,
                    failures: h.consecutive_failures,
                    error: err_s,
                    at: now_sec,
                });
            }
        }
    }

    /// Decide whether a sample should be included in `quota.report` per section 5.2 & 6.4:
    /// "Quota samples are logged when the percent moves by one point, when the state changes,
    /// and at least every 10 minutes."
    /// And when `new_raw_sample` is true from a newly read line in a session file, we report
    /// each distinct sample observation once.
    fn should_report_sample(&self, sample: &QuotaSample, state: PoolState, now_sec: u64) -> bool {
        let Some(prev) = self.state.reported_meters.get(&sample.pool) else {
            return true;
        };
        if sample.observed_at > prev.observed_at {
            return true;
        }
        if (sample.used_pct - prev.used_pct).abs() >= 1.0 {
            return true;
        }
        if state != prev.state || sample.source != prev.source || sample.resets_at != prev.resets_at
        {
            return true;
        }
        if now_sec.saturating_sub(prev.reported_at) >= REPORT_HEARTBEAT_SECS {
            return true;
        }
        false
    }

    fn note_sample_reported(&mut self, sample: &QuotaSample, state: PoolState, now_sec: u64) {
        self.state.reported_meters.insert(
            sample.pool.clone(),
            ReportedMeterDigest {
                used_pct: sample.used_pct,
                resets_at: sample.resets_at,
                state,
                source: sample.source,
                observed_at: sample.observed_at,
                reported_at: now_sec,
            },
        );
    }

    fn apply_and_filter_sample(
        &mut self,
        sample: QuotaSample,
        now_sec: u64,
        out_samples: &mut Vec<QuotaSample>,
    ) {
        let m = self
            .state
            .meters
            .entry(sample.pool.clone())
            .or_insert_with(|| {
                Meter::new(&sample.owner, &sample.vendor, &sample.pool, &sample.window)
            });
        if !on_sample(m, sample.clone()) {
            return;
        }
        let f = forecast(m, now_sec, &[]);
        let st = pool_state(m, &f, now_sec, &self.cfg.quota_config);
        if self.should_report_sample(&sample, st, now_sec) {
            self.note_sample_reported(&sample, st, now_sec);
            out_samples.push(sample);
        }
    }

    /// Every 10 s: tails new bytes of Codex session `.jsonl` files under `codex_sessions_dir`,
    /// keeping the byte offset per file so each line is parsed and reported at most once,
    /// even across restarts (section 6.2, 6.4, 10.3).
    pub fn poll_codex_rollout(&mut self, now_sec: u64) -> Result<QuotaReportOp, String> {
        let mut report = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };
        if !self.is_probe_active("codex_rollout") {
            return Ok(report);
        }
        let Some(root) = self.cfg.codex_sessions_dir.clone() else {
            return Ok(report);
        };
        if !root.exists() {
            return Ok(report);
        }

        let mut files = Vec::new();
        collect_rollout_files(&root, &mut files)
            .map_err(|e| format!("collect rollout files: {e}"))?;
        files.sort();

        let weights = self.cfg.quota_config.token_weights.clone();
        let owner = self.cfg.owner.clone();

        for path in files {
            if !self.is_probe_active("codex_rollout") {
                break;
            }
            let file_key = relative_file_key(&root, &path);
            let thread_id = extract_thread_id_from_filename(&path);
            let mut offset = self.state.file_offsets.get(&file_key).copied().unwrap_or(0);

            let Ok(mut file) = File::open(&path) else {
                continue;
            };
            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len < offset {
                // File was truncated/rotated
                offset = 0;
            }
            if file_len == offset {
                continue;
            }
            if file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut buf = Vec::with_capacity((file_len - offset) as usize);
            if file.read_to_end(&mut buf).is_err() {
                continue;
            }

            // Process only complete newline-terminated lines so a partially written line
            // is read on the next 10s tick without advancing past it.
            let mut pos = 0usize;
            while let Some(nl_rel) = buf[pos..].iter().position(|&b| b == b'\n') {
                let line_start_offset = offset + (pos as u64);
                let line_end = pos + nl_rel;
                let line_bytes = &buf[pos..line_end];
                pos = line_end + 1;

                let line_str = String::from_utf8_lossy(line_bytes);
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Check if this line is valid JSON
                if serde_json::from_str::<Value>(trimmed).is_err() {
                    self.record_probe_failure(
                        "codex_rollout",
                        "codex",
                        &["codex.5h", "codex.week"],
                        format!("malformed json in {file_key} at offset {line_start_offset}"),
                        now_sec,
                        &mut report.broken_probes,
                        &mut report.samples,
                    );
                    if !self.is_probe_active("codex_rollout") {
                        break;
                    }
                    continue;
                }

                if trimmed.contains("\"token_count\"") {
                    match codex::parse_rollout_line(trimmed, &owner, &weights) {
                        Some(parsed) => {
                            self.record_probe_success("codex_rollout");
                            for s in parsed.samples {
                                self.apply_and_filter_sample(s, now_sec, &mut report.samples);
                            }
                            if let Some(mut sp) = parsed.spend {
                                if sp.worker.is_none() {
                                    sp.worker = thread_id.clone();
                                }
                                report.spend.push(ReportedSpend {
                                    record: sp,
                                    file_key: Some(file_key.clone()),
                                    byte_offset: Some(line_start_offset),
                                });
                            }
                        }
                        None => {
                            self.record_probe_failure(
                                "codex_rollout",
                                "codex",
                                &["codex.5h", "codex.week"],
                                format!(
                                    "token_count line missing rate_limits and spend in {file_key}"
                                ),
                                now_sec,
                                &mut report.broken_probes,
                                &mut report.samples,
                            );
                            if !self.is_probe_active("codex_rollout") {
                                break;
                            }
                        }
                    }
                } else {
                    self.record_probe_success("codex_rollout");
                }
            }

            let new_offset = offset + (pos as u64);
            self.state.file_offsets.insert(file_key, new_offset);
        }

        self.save_state()?;
        Ok(report)
    }

    /// Every 60 s: reads `quota/claude.json` written by `sommelier quota ingest claude-statusline`
    /// (or a raw Claude statusline JSON file if dropped there directly) and reports new samples (section 6.1, 6.4).
    pub fn poll_claude_drop(&mut self, now_sec: u64) -> Result<QuotaReportOp, String> {
        let mut report = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };
        if !self.is_probe_active("claude_drop") {
            return Ok(report);
        }
        let Some(path) = self.cfg.claude_drop_path.clone() else {
            return Ok(report);
        };
        if !path.exists() {
            return Ok(report);
        }

        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        if raw.trim().is_empty() {
            return Ok(report);
        }

        // Support both `ClaudeDropState` (written by `quota ingest claude-statusline`)
        // and raw statusline JSON (`rate_limits.{five_hour, seven_day}`).
        let prev_drop = self.state.claude_drop_state.clone();
        let parsed_samples: Result<(Vec<QuotaSample>, Option<ClaudeDropState>), String> =
            if let Ok(mut drop_state) = serde_json::from_str::<ClaudeDropState>(&raw)
                && (!drop_state.samples.is_empty() || drop_state.last_observed_at > 0)
            {
                for s in &mut drop_state.samples {
                    s.owner = self.cfg.owner.clone();
                }
                let samples = drop_state.samples.clone();
                Ok((samples, Some(drop_state)))
            } else {
                claude::parse_statusline(&raw, &self.cfg.owner, now_sec, prev_drop.as_ref())
                    .and_then(|res| {
                        if res.state.samples.is_empty() {
                            Err("missing rate_limits in claude statusline json".into())
                        } else {
                            Ok((res.state.samples.clone(), Some(res.state)))
                        }
                    })
            };

        match parsed_samples {
            Ok((samples, new_drop)) => {
                self.record_probe_success("claude_drop");
                if let Some(ds) = new_drop {
                    self.state.last_claude_observed_at = ds.last_observed_at;
                    self.state.claude_drop_state = Some(ds);
                }
                for s in samples {
                    self.apply_and_filter_sample(s, now_sec, &mut report.samples);
                }
            }
            Err(err) => {
                self.record_probe_failure(
                    "claude_drop",
                    "claude",
                    &["claude.5h", "claude.week"],
                    err,
                    now_sec,
                    &mut report.broken_probes,
                    &mut report.samples,
                );
            }
        }

        self.save_state()?;
        Ok(report)
    }

    /// Every 60 s: accounts worker minutes of live `agy` / `gemini` children by model family
    /// and effort from the supervisor's process table (`ChildrenStore`) -> `SPEND` + inferred sample (section 6.3, 6.4).
    pub fn poll_agy_spend(&mut self, now_sec: u64) -> Result<QuotaReportOp, String> {
        let mut report = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };
        if !self.cfg.probe_gemini_enabled {
            return Ok(report);
        }

        let children_store = ChildrenStore::new(&self.cfg.data_dir);
        let statuses = children_store
            .list_statuses()
            .map_err(|e| format!("list children: {e}"))?;

        if self.state.agy_window_resets_at == 0 || now_sec >= self.state.agy_window_resets_at {
            self.state.agy_window_units = 0.0;
            self.state.agy_window_resets_at = now_sec + 5 * 3600;
        }

        let mut added_units = 0.0;
        for st in statuses {
            if !st.pid_alive {
                continue;
            }
            let rec = &st.record;
            let cls = rec.class.to_ascii_lowercase();
            let mdl = rec.model.to_ascii_lowercase();
            if !cls.contains("gemini") && !cls.contains("agy") && !mdl.contains("gemini") {
                continue;
            }

            let child_key = format!("{}:{}", rec.name, rec.started_at_sec);
            let last_at = self
                .state
                .agy_child_accounted_at
                .get(&child_key)
                .copied()
                .unwrap_or(rec.started_at_sec);
            if now_sec <= last_at {
                continue;
            }

            let elapsed_secs = now_sec - last_at;
            let elapsed_mins = (elapsed_secs as f64) / 60.0;
            let effort = infer_effort(&rec.model);
            let weight = self.cfg.quota_config.effort_weight(effort);
            let units = elapsed_mins * weight;

            self.state
                .agy_child_accounted_at
                .insert(child_key.clone(), now_sec);
            self.state.agy_window_units += units;
            self.state.agy_day_units += units;
            added_units += units;

            report.spend.push(ReportedSpend {
                record: SpendRecord {
                    owner: self.cfg.owner.clone(),
                    pool: "agy.window".to_owned(),
                    task_id: rec.task_id.clone(),
                    at: now_sec,
                    worker: Some(rec.name.clone()),
                    model: Some(rec.model.clone()),
                    units,
                    input_tokens: 0,
                    cached_tokens: 0,
                    output_tokens: 0,
                },
                file_key: Some(format!("agy_child/{child_key}")),
                byte_offset: Some(now_sec),
            });
        }

        if added_units > 0.0 {
            let (cap, n_cal) =
                self.state
                    .meters
                    .get("agy.window")
                    .map_or((SEED_CAPACITY_UNITS, 0), |m| {
                        (
                            if m.capacity_units > 0.0 {
                                m.capacity_units
                            } else {
                                SEED_CAPACITY_UNITS
                            },
                            m.n_calibration,
                        )
                    });
            let sample = gemini::inferred_sample(
                &self.cfg.owner,
                "agy.window",
                "5h",
                self.state.agy_window_units,
                cap,
                self.state.agy_window_resets_at,
                now_sec,
                n_cal,
            );
            self.apply_and_filter_sample(sample, now_sec, &mut report.samples);
        }

        self.save_state()?;
        Ok(report)
    }

    /// On child output: inspects `claude_stream` (`rate_limit_event` lines) and `agy_message`
    /// (limit patterns) to emit `state_events` and samples (section 6.1, 6.3, 6.4).
    pub fn on_child_output(
        &mut self,
        vendor: &str,
        family: Option<&str>,
        line: &str,
        now_sec: u64,
    ) -> Result<QuotaReportOp, String> {
        let mut report = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };

        if vendor == "claude" {
            if !self.is_probe_active("claude_stream") {
                return Ok(report);
            }
            if let Some(ev) = claude::parse_stream_rate_limit_event(line, &self.cfg.owner, now_sec)
            {
                self.record_probe_success("claude_stream");
                if let Some(s) = ev.sample {
                    self.apply_and_filter_sample(s, now_sec, &mut report.samples);
                }
                if let Some(se) = ev.state_event {
                    let pool = if se.message.contains("seven_day") {
                        "claude.week"
                    } else {
                        "claude.5h"
                    };
                    let win = if pool == "claude.week" { "week" } else { "5h" };
                    let m = self
                        .state
                        .meters
                        .entry(pool.to_owned())
                        .or_insert_with(|| Meter::new(&self.cfg.owner, "claude", pool, win));
                    m.state_event = Some(se.clone());
                    report.state_events.push(ReportedStateEvent {
                        owner: self.cfg.owner.clone(),
                        vendor: "claude".into(),
                        pool: pool.into(),
                        window: win.into(),
                        event: se,
                    });
                }
            } else if line.contains("rate_limit_event") {
                self.record_probe_failure(
                    "claude_stream",
                    "claude",
                    &["claude.5h", "claude.week"],
                    "failed to parse rate_limit_event line",
                    now_sec,
                    &mut report.broken_probes,
                    &mut report.samples,
                );
            }
        } else if (vendor == "gemini" || vendor == "agy")
            && let Some(hit) = gemini::parse_limit_message(line, now_sec, family)
        {
            let prev = self.state.last_gemini_hit_at;
            let mut win_m = self
                .state
                .meters
                .remove("agy.window")
                .unwrap_or_else(|| Meter::new(&self.cfg.owner, "gemini", "agy.window", "5h"));
            let mut wk_m = self
                .state
                .meters
                .remove("agy.week")
                .unwrap_or_else(|| Meter::new(&self.cfg.owner, "gemini", "agy.week", "week"));
            let fam_key = hit.family.as_ref().map(|f| format!("agy.family.{f}"));
            let mut fam_m = fam_key.as_ref().map(|k| {
                self.state
                    .meters
                    .remove(k)
                    .unwrap_or_else(|| Meter::new(&self.cfg.owner, "gemini", k, "5h"))
            });

            gemini::apply_limit_hit(&mut win_m, &mut wk_m, fam_m.as_mut(), &hit, prev);

            if hit.is_weekly {
                if let Some(ev) = wk_m.state_event.clone() {
                    report.state_events.push(ReportedStateEvent {
                        owner: self.cfg.owner.clone(),
                        vendor: "gemini".into(),
                        pool: "agy.week".into(),
                        window: "week".into(),
                        event: ev,
                    });
                }
            } else {
                if let (Some(k), Some(fm)) = (&fam_key, &fam_m)
                    && let Some(ev) = fm.state_event.clone()
                {
                    report.state_events.push(ReportedStateEvent {
                        owner: self.cfg.owner.clone(),
                        vendor: "gemini".into(),
                        pool: k.clone(),
                        window: "5h".into(),
                        event: ev,
                    });
                }
                if let Some(ev) = win_m.state_event.clone() {
                    report.state_events.push(ReportedStateEvent {
                        owner: self.cfg.owner.clone(),
                        vendor: "gemini".into(),
                        pool: "agy.window".into(),
                        window: "5h".into(),
                        event: ev,
                    });
                } else {
                    // Single family hit puts pool in drain or records event on target pool
                    let ev = StateEvent {
                        resets_at: hit.resets_at,
                        observed_at: hit.observed_at,
                        message: hit.summary.clone(),
                        family: hit.family.clone(),
                    };
                    if fam_key.is_none() {
                        win_m.state_event = Some(ev.clone());
                        report.state_events.push(ReportedStateEvent {
                            owner: self.cfg.owner.clone(),
                            vendor: "gemini".into(),
                            pool: "agy.window".into(),
                            window: "5h".into(),
                            event: ev,
                        });
                    }
                }
            }

            self.state.meters.insert("agy.window".into(), win_m);
            self.state.meters.insert("agy.week".into(), wk_m);
            if let (Some(k), Some(fm)) = (fam_key, fam_m) {
                self.state.meters.insert(k, fm);
            }
            self.state.last_gemini_hit_at = Some(now_sec);
        }

        self.save_state()?;
        Ok(report)
    }

    /// Every 5 min (or before a lease when older than 2 min): polls `codex app-server` response
    /// when no Codex sample is newer than `max_age_secs` (section 6.2, 6.4).
    pub fn poll_codex_appserver_with<F>(
        &mut self,
        now_sec: u64,
        max_age_secs: u64,
        fetch_response_line: F,
    ) -> Result<QuotaReportOp, String>
    where
        F: FnOnce() -> Result<String, String>,
    {
        let mut report = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };
        if !self.is_probe_active("codex_appserver") {
            return Ok(report);
        }

        let newest_codex = self
            .state
            .meters
            .values()
            .filter(|m| m.vendor == "codex")
            .filter_map(|m| m.latest.as_ref().map(|s| s.observed_at))
            .max()
            .unwrap_or(0);
        if newest_codex > 0 && now_sec.saturating_sub(newest_codex) < max_age_secs {
            return Ok(report);
        }

        let result = fetch_response_line().and_then(|line| {
            codex::parse_appserver_response(&line, &self.cfg.owner, now_sec).and_then(|samples| {
                if samples.is_empty() {
                    Err("empty samples in codex app-server response".into())
                } else {
                    Ok(samples)
                }
            })
        });

        match result {
            Ok(samples) => {
                self.record_probe_success("codex_appserver");
                for s in samples {
                    self.apply_and_filter_sample(s, now_sec, &mut report.samples);
                }
            }
            Err(err) => {
                self.record_probe_failure(
                    "codex_appserver",
                    "codex",
                    &["codex.5h", "codex.week"],
                    err,
                    now_sec,
                    &mut report.broken_probes,
                    &mut report.samples,
                );
            }
        }

        self.save_state()?;
        Ok(report)
    }

    /// Runs a combined poller tick (`codex_rollout`, `claude_drop`, `agy_spend`) and returns
    /// the aggregated [`QuotaReportOp`] containing only newly observed or changed samples,
    /// new spend records, state events, and any `PROBE_BROKEN` notifications.
    pub fn poll_tick(&mut self, now_sec: u64) -> Result<QuotaReportOp, String> {
        let mut combined = QuotaReportOp {
            op: "quota.report".into(),
            owner: self.cfg.owner.clone(),
            ..Default::default()
        };

        let r_codex = self.poll_codex_rollout(now_sec)?;
        merge_reports(&mut combined, r_codex);

        let r_claude = self.poll_claude_drop(now_sec)?;
        merge_reports(&mut combined, r_claude);

        let r_agy = self.poll_agy_spend(now_sec)?;
        merge_reports(&mut combined, r_agy);

        Ok(combined)
    }

    /// Sends a [`QuotaReportOp`] to the hub (or spools the resulting messages to the local [`Outbox`]
    /// if the hub is unreachable). Returns `Ok(true)` if delivered live, `Ok(false)` if spooled.
    pub async fn report_or_spool(
        &self,
        report: &QuotaReportOp,
        sender_role: Role,
        opts: Option<&Opts>,
        now_ms: u64,
    ) -> Result<bool, String> {
        if is_report_empty(report) {
            return Ok(true);
        }
        let msgs = validate_and_build_report_messages(&self.cfg.owner, sender_role, report, now_ms)
            .map_err(|e| e.to_string())?;
        let outbox = Outbox::new(&self.cfg.data_dir);
        let mut all_live = true;
        for m in msgs {
            let req = message_to_send_request(&m);
            match opts {
                Some(o) => {
                    if !matches!(outbox.send_or_spool(o, req).await, Ok(Some(_))) {
                        all_live = false;
                    }
                }
                None => {
                    outbox.enqueue(req).map_err(|e| e.to_string())?;
                    all_live = false;
                }
            }
        }
        Ok(all_live)
    }
}

pub fn is_report_empty(r: &QuotaReportOp) -> bool {
    r.samples.is_empty()
        && r.spend.is_empty()
        && r.state_events.is_empty()
        && r.estimates.is_empty()
        && r.broken_probes.is_empty()
}

fn merge_reports(dst: &mut QuotaReportOp, src: QuotaReportOp) {
    dst.samples.extend(src.samples);
    dst.spend.extend(src.spend);
    dst.state_events.extend(src.state_events);
    dst.estimates.extend(src.estimates);
    dst.broken_probes.extend(src.broken_probes);
}

fn message_to_send_request(m: &Message) -> Value {
    serde_json::json!({
        "op": "send",
        "kind": m.kind.as_str(),
        "topic": m.topic,
        "body": m.body,
        "refs": m.refs,
        "to": m.to,
    })
}

fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn relative_file_key(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Extracts `<thread>` from `rollout-<time>-<thread>.jsonl` (section 6.2).
fn extract_thread_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    let (_, thread) = rest.rsplit_once('-')?;
    (!thread.is_empty()).then(|| thread.to_owned())
}

fn infer_effort(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.contains("xhigh") {
        "xhigh"
    } else if lower.contains("high") {
        "high"
    } else if lower.contains("low") || lower.contains("flash") {
        "low"
    } else {
        "medium"
    }
}

#[allow(dead_code)]
fn _default_weights() -> TokenWeights {
    TokenWeights::default()
}
