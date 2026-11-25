//! Quota event projection into store tables (`QUOTA_SAMPLE`, `METER`, `SPEND`, `ESTIMATE`),
//! op `quota.report` validation, and the `COORD_BUDGET` alert rule at 70 percent (sections 3, 4.6, 5.2, 6.4, 10.2, 10.3, 13.6).

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message, Role};
use crate::role::{RoleCandidate, RolePolicy, RoleTable};
use crate::store::registry::StoreModule;
use crate::time::{parse_ms, rfc3339};

use super::api::build_quota_api;
use super::estimate::{Estimate, EstimateSource};
use super::forecast::{forecast, on_sample, pool_state};
use super::meter::{
    Meter, MeterPolicy, PoolState, QuotaConfig, QuotaSample, Source, SpendRecord, StateEvent,
};

/// Default threshold for the `COORD_BUDGET` alarm (section 5.3, 13.6).
pub const DEFAULT_COORD_BUDGET_ALARM_PCT: f64 = 70.0;

/// Names of the four quota tables delivered in step 9 (section 3, 10.3).
pub const QUOTA_TABLES: [&str; 4] = ["QUOTA_SAMPLE", "METER", "SPEND", "ESTIMATE"];

/// SQL schema for `QUOTA_SAMPLE`, `METER`, `SPEND`, and `ESTIMATE` in `state.db` (section 3).
pub const QUOTA_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS QUOTA_SAMPLE (
    owner TEXT NOT NULL,
    pool TEXT NOT NULL,
    window TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    vendor TEXT NOT NULL DEFAULT '',
    used_pct REAL NOT NULL DEFAULT 0.0,
    resets_at INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'api',
    confidence REAL NOT NULL DEFAULT 0.9,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, pool, window, observed_at)
);

CREATE TABLE IF NOT EXISTS METER (
    owner TEXT NOT NULL,
    pool TEXT NOT NULL,
    window TEXT NOT NULL,
    vendor TEXT NOT NULL DEFAULT '',
    latest_sample TEXT NOT NULL DEFAULT '{}',
    state_event TEXT,
    unit TEXT NOT NULL DEFAULT 'weighted_tokens',
    pct_per_unit REAL NOT NULL DEFAULT 0.0001,
    n_calibration INTEGER NOT NULL DEFAULT 0,
    capacity_units REAL NOT NULL DEFAULT 0.0,
    resets_at INTEGER NOT NULL DEFAULT 0,
    probe_enabled INTEGER NOT NULL DEFAULT 1,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, pool, window)
);

CREATE TABLE IF NOT EXISTS SPEND (
    owner TEXT NOT NULL,
    pool TEXT NOT NULL,
    task_id TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL,
    file_key TEXT NOT NULL DEFAULT '',
    byte_offset INTEGER NOT NULL DEFAULT 0,
    worker TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    units REAL NOT NULL DEFAULT 0.0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    raw_counters TEXT NOT NULL DEFAULT '{}',
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, pool, task_id, at, file_key, byte_offset)
);

CREATE TABLE IF NOT EXISTS ESTIMATE (
    kind TEXT NOT NULL,
    class TEXT NOT NULL,
    n INTEGER NOT NULL DEFAULT 0,
    minutes_p50 REAL NOT NULL DEFAULT 30.0,
    minutes_p90 REAL NOT NULL DEFAULT 75.0,
    units_p50 REAL NOT NULL DEFAULT 400000.0,
    units_p90 REAL NOT NULL DEFAULT 1500000.0,
    source TEXT NOT NULL DEFAULT 'default',
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, class)
);
";

/// Store projection module for the quota tables (`QUOTA_SAMPLE`, `METER`, `SPEND`, `ESTIMATE`).
pub struct QuotaModule;

impl StoreModule for QuotaModule {
    fn name(&self) -> &'static str {
        "quota"
    }

    fn schema(&self) -> &'static str {
        QUOTA_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_quota_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_quota_api(conn, now_sec, state)
    }
}

/// Initializes the quota tables in `conn` (section 10.2 `schema()`).
pub fn schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(QUOTA_SCHEMA_SQL)
        .map_err(|e| format!("quota schema error: {e}"))
}

/// Row stored in `QUOTA_SAMPLE`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaSampleRow {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub observed_at: u64,
    pub used_pct: f64,
    pub resets_at: u64,
    pub source: Source,
    pub confidence: f64,
    pub msg_id: u64,
}

impl QuotaSampleRow {
    pub fn to_sample(&self) -> QuotaSample {
        QuotaSample {
            owner: self.owner.clone(),
            vendor: self.vendor.clone(),
            pool: self.pool.clone(),
            window: self.window.clone(),
            observed_at: self.observed_at,
            used_pct: self.used_pct,
            resets_at: self.resets_at,
            source: self.source,
            confidence: self.confidence,
        }
    }
}

/// Row stored in `METER`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterTableRow {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub latest_sample: Option<QuotaSample>,
    pub state_event: Option<StateEvent>,
    pub unit: String,
    pub pct_per_unit: f64,
    pub n_calibration: u64,
    pub capacity_units: f64,
    pub resets_at: u64,
    pub probe_enabled: bool,
    pub msg_id: u64,
}

/// Row stored in `SPEND`, including optional file and byte offset keys to prevent double counting (section 10.3 step 9).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpendTableRow {
    pub owner: String,
    pub pool: String,
    pub task_id: String,
    pub at: u64,
    #[serde(default)]
    pub file_key: String,
    #[serde(default)]
    pub byte_offset: u64,
    pub worker: String,
    pub model: String,
    pub units: f64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub raw_counters: Value,
    pub msg_id: u64,
}

/// Row stored in `ESTIMATE`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EstimateTableRow {
    pub kind: String,
    pub class: String,
    pub n: u64,
    pub minutes_p50: f64,
    pub minutes_p90: f64,
    pub units_p50: f64,
    pub units_p90: f64,
    pub source: EstimateSource,
    pub msg_id: u64,
}

/// A spend entry inside a `quota.report` payload, optionally keyed by `(file_key, byte_offset)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportedSpend {
    #[serde(flatten)]
    pub record: SpendRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
}

/// A state event (limit message or pause) inside a `quota.report` payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedStateEvent {
    pub owner: String,
    pub vendor: String,
    pub pool: String,
    pub window: String,
    pub event: StateEvent,
}

/// A probe failure report inside `quota.report` when a probe fails three times in a row (section 6.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedProbeBroken {
    pub owner: String,
    pub vendor: String,
    pub probe: String,
    pub pools: Vec<String>,
    pub failures: u32,
    pub error: String,
    pub at: u64,
}

/// Op `quota.report` request payload sent by the supervisor quota poller to the hub (section 6.4, 10.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaReportOp {
    #[serde(default = "default_op_name")]
    pub op: String,
    pub owner: String,
    #[serde(default)]
    pub samples: Vec<QuotaSample>,
    #[serde(default)]
    pub spend: Vec<ReportedSpend>,
    #[serde(default)]
    pub state_events: Vec<ReportedStateEvent>,
    #[serde(default)]
    pub estimates: Vec<Estimate>,
    #[serde(default)]
    pub broken_probes: Vec<ReportedProbeBroken>,
}

fn default_op_name() -> String {
    "quota.report".to_owned()
}

/// Refusal returned when a `quota.report` op violates ownership or validation rules (section 9, 10.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaReportRefusal {
    pub code: u8,
    pub sender: String,
    pub target_owner: String,
    pub reason: String,
}

impl std::fmt::Display for QuotaReportRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// Validates a `quota.report` request from `sender` and converts it into the accepted bus
/// messages (`FACT` on `quota/<owner>/<pool>/<window>`, `spend/<owner>/<pool>`, `estimate/<kind>/<class>`,
/// and `ALERT` on `alert/PROBE_BROKEN/...`).
///
/// Section 9 & 10.3 rule:
/// "Its FACT rows can touch only its own meters and machines."
/// "A report for another owner's meter is refused."
pub fn validate_and_build_report_messages(
    sender: &str,
    sender_role: Role,
    report: &QuotaReportOp,
    now_ms: u64,
) -> Result<Vec<Message>, QuotaReportRefusal> {
    if report.op != "quota.report" && !report.op.is_empty() {
        return Err(QuotaReportRefusal {
            code: crate::exit::WRONG_USAGE,
            sender: sender.to_owned(),
            target_owner: report.owner.clone(),
            reason: format!("expected op `quota.report`, got `{}`", report.op),
        });
    }

    let check_owner = |target: &str| -> Result<(), QuotaReportRefusal> {
        if target.is_empty() || target != sender {
            return Err(QuotaReportRefusal {
                code: crate::exit::REFUSED,
                sender: sender.to_owned(),
                target_owner: target.to_owned(),
                reason: format!(
                    "refused: `{sender}` cannot report quota or spend for another owner `{target}`"
                ),
            });
        }
        Ok(())
    };

    check_owner(&report.owner)?;
    for s in &report.samples {
        check_owner(&s.owner)?;
    }
    for sp in &report.spend {
        check_owner(&sp.record.owner)?;
    }
    for se in &report.state_events {
        check_owner(&se.owner)?;
    }
    for bp in &report.broken_probes {
        check_owner(&bp.owner)?;
    }

    let ts = rfc3339(now_ms);
    let mut msgs = Vec::new();

    for s in &report.samples {
        let body = json!({
            "v": 1,
            "owner": s.owner,
            "vendor": s.vendor,
            "pool": s.pool,
            "window": s.window,
            "used_pct": s.used_pct,
            "resets_at": s.resets_at,
            "source": s.source,
            "confidence": s.confidence,
            "observed_at": s.observed_at,
        })
        .to_string();
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: sender.to_owned(),
            role: sender_role,
            kind: Kind::Fact,
            topic: format!("quota/{}/{}/{}", s.owner, s.pool, s.window),
            body,
            refs: vec![],
            to: vec![],
        });
    }

    for se in &report.state_events {
        let body = json!({
            "v": 1,
            "owner": se.owner,
            "vendor": se.vendor,
            "pool": se.pool,
            "window": se.window,
            "state_event": se.event,
        })
        .to_string();
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: sender.to_owned(),
            role: sender_role,
            kind: Kind::Fact,
            topic: format!("quota/{}/{}/{}", se.owner, se.pool, se.window),
            body,
            refs: vec!["event:limit".into()],
            to: vec![],
        });
    }

    for sp in &report.spend {
        let r = &sp.record;
        let body = json!({
            "v": 1,
            "owner": r.owner,
            "pool": r.pool,
            "task_id": r.task_id,
            "at": r.at,
            "file_key": sp.file_key,
            "byte_offset": sp.byte_offset,
            "worker": r.worker,
            "model": r.model,
            "units": r.units,
            "input_tokens": r.input_tokens,
            "cached_tokens": r.cached_tokens,
            "output_tokens": r.output_tokens,
        })
        .to_string();
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: sender.to_owned(),
            role: sender_role,
            kind: Kind::Fact,
            topic: format!("spend/{}/{}", r.owner, r.pool),
            body,
            refs: vec![],
            to: vec![],
        });
    }

    for est in &report.estimates {
        let body = json!({
            "v": 1,
            "kind": est.kind,
            "class": est.class,
            "n": est.n,
            "minutes_p50": est.minutes_p50,
            "minutes_p90": est.minutes_p90,
            "units_p50": est.units_p50,
            "units_p90": est.units_p90,
            "source": est.source,
        })
        .to_string();
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: sender.to_owned(),
            role: sender_role,
            kind: Kind::Fact,
            topic: format!("estimate/{}/{}", est.kind, est.class),
            body,
            refs: vec![],
            to: vec![],
        });
    }

    for bp in &report.broken_probes {
        let key = format!("{}/{}", bp.owner, bp.probe);
        let text = format!(
            "probe {} on {} failed {} times in a row ({}); probe turned off, falling back to inferred",
            bp.probe, bp.owner, bp.failures, bp.error
        );
        let body = json!({
            "v": 1,
            "code": "PROBE_BROKEN",
            "key": key,
            "severity": "warn",
            "state": "open",
            "opened_at": bp.at,
            "last_at": bp.at,
            "count": 1,
            "text": text,
            "data": {
                "owner": bp.owner,
                "vendor": bp.vendor,
                "probe": bp.probe,
                "pools": bp.pools,
                "failures": bp.failures,
                "error": bp.error,
            }
        })
        .to_string();
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Alert,
            topic: format!("alert/PROBE_BROKEN/{key}"),
            body,
            refs: vec!["state:open".into()],
            to: vec![],
        });
    }

    Ok(msgs)
}

/// Processes a `quota.report` op directly against a `Store`, validating sender ownership,
/// projecting all accepted `FACT` and `ALERT` messages, and checking the `COORD_BUDGET`
/// 70 percent alarm rule. Returns the generated messages that were applied.
pub fn handle_quota_report(
    store: &mut crate::store::Store,
    sender: &str,
    sender_role: Role,
    report: &QuotaReportOp,
    coord_policy: Option<&RolePolicy>,
    now_sec: u64,
) -> Result<Vec<Message>, QuotaReportRefusal> {
    let mut msgs = validate_and_build_report_messages(sender, sender_role, report, now_sec * 1000)?;
    let mut next_id = store.cursor().unwrap_or(0) + 1;
    for m in &mut msgs {
        m.id = next_id;
        next_id += 1;
    }
    store.apply_batch(&msgs).map_err(|e| QuotaReportRefusal {
        code: crate::exit::INTERNAL_ERROR,
        sender: sender.to_owned(),
        target_owner: report.owner.clone(),
        reason: e,
    })?;

    if let Ok(mut alerts) = check_coord_budget_alerts(
        store.connection(),
        coord_policy,
        DEFAULT_COORD_BUDGET_ALARM_PCT,
        now_sec,
    ) && !alerts.is_empty()
    {
        for a in &mut alerts {
            a.id = next_id;
            next_id += 1;
        }
        let _ = store.apply_batch(&alerts);
        msgs.extend(alerts);
    }

    Ok(msgs)
}

fn infer_vendor(pool: &str) -> String {
    let lower = pool.to_ascii_lowercase();
    if lower.starts_with("codex") || lower.starts_with("premium") {
        "codex".into()
    } else if lower.starts_with("claude") {
        "claude".into()
    } else {
        "gemini".into()
    }
}

fn infer_window(pool: &str) -> String {
    if let Some((_, win)) = pool.rsplit_once('.') {
        win.to_owned()
    } else {
        "5h".to_owned()
    }
}

fn parse_source(s: &str) -> Source {
    match s.trim().to_ascii_lowercase().as_str() {
        "api" => Source::Api,
        "manual" => Source::Manual,
        _ => Source::Inferred,
    }
}

fn source_str(s: Source) -> &'static str {
    match s {
        Source::Api => "api",
        Source::Manual => "manual",
        Source::Inferred => "inferred",
    }
}

fn estimate_source_str(s: EstimateSource) -> &'static str {
    match s {
        EstimateSource::Default => "default",
        EstimateSource::Learned => "learned",
    }
}

fn parse_estimate_source(s: &str) -> EstimateSource {
    if s.eq_ignore_ascii_case("learned") {
        EstimateSource::Learned
    } else {
        EstimateSource::Default
    }
}

/// Applies one bus message to the quota tables (`QUOTA_SAMPLE`, `METER`, `SPEND`, `ESTIMATE`)
/// inside an open SQLite transaction (section 10.2 `apply(tx, message)`).
///
/// Enforces ownership: if a non-hub sender emits a `FACT` on `quota/<owner>/...` or `spend/<owner>/...`
/// where `<owner> != msg.from`, the row is refused and ignored.
pub fn apply_quota_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    let ts_ms = parse_ms(&msg.ts).unwrap_or(0);
    let ts_sec = ts_ms / 1000;
    let msg_id = msg.id as i64;

    match msg.kind {
        Kind::Fact if msg.topic.starts_with("quota/") => {
            apply_quota_fact(tx, msg, ts_sec, msg_id)?;
        }
        Kind::Fact if msg.topic.starts_with("spend/") => {
            apply_spend_fact(tx, msg, ts_sec, msg_id)?;
        }
        Kind::Fact if msg.topic.starts_with("estimate/") => {
            apply_estimate_fact(tx, msg, msg_id)?;
        }
        Kind::Alert if msg.topic.starts_with("alert/PROBE_BROKEN/") => {
            apply_probe_broken_alert(tx, msg, msg_id)?;
        }
        _ => {}
    }
    Ok(())
}

fn apply_quota_fact(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: u64,
    msg_id: i64,
) -> Result<(), String> {
    let parts: Vec<&str> = msg.topic.split('/').collect();
    let topic_owner = parts.get(1).copied().unwrap_or("");
    let topic_pool = parts.get(2).copied().unwrap_or("");
    let topic_window = parts.get(3).copied().unwrap_or("");

    let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
        return Ok(());
    };

    let owner = v["owner"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_owner);
    if owner.is_empty() {
        return Ok(());
    }
    // Ownership check (section 9 & 10.3: "A report for another owner's meter is refused.")
    if msg.role != Role::Hub && !msg.from.is_empty() && owner != msg.from {
        return Ok(());
    }
    if !topic_owner.is_empty() && topic_owner != owner {
        return Ok(());
    }

    let pool = v["pool"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_pool);
    if pool.is_empty() {
        return Ok(());
    }
    let window = v["window"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_window);
    let window = if window.is_empty() {
        infer_window(pool)
    } else {
        window.to_owned()
    };
    let vendor = v["vendor"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| infer_vendor(pool));

    // Load or initialize the current METER state
    let mut meter = load_meter_with_series(tx, owner, pool, &window, &vendor)?;

    if let Some(ev_val) = v.get("state_event")
        && let Ok(se) = serde_json::from_value::<StateEvent>(ev_val.clone())
    {
        if se.resets_at > meter.resets_at {
            meter.resets_at = se.resets_at;
        }
        meter.state_event = Some(se);
    }

    if let Some(u) = v.get("unit").and_then(Value::as_str)
        && !u.is_empty()
    {
        meter.unit = u.to_owned();
    }
    if let Some(ppu) = v.get("pct_per_unit").and_then(Value::as_f64)
        && ppu > 0.0
    {
        meter.pct_per_unit = ppu;
    }
    if let Some(ncal) = v.get("n_calibration").and_then(Value::as_u64) {
        meter.n_calibration = ncal;
    }
    if let Some(cap) = v.get("capacity_units").and_then(Value::as_f64)
        && cap > 0.0
    {
        meter.capacity_units = cap;
    }
    if let Some(pe) = v.get("probe_enabled").and_then(Value::as_bool) {
        meter.probe_enabled = pe;
    }

    if let Some(used_pct) = v.get("used_pct").and_then(Value::as_f64) {
        let observed_at = v
            .get("observed_at")
            .and_then(Value::as_u64)
            .unwrap_or(ts_sec);
        let resets_at = v.get("resets_at").and_then(Value::as_u64).unwrap_or(0);
        let source = v
            .get("source")
            .and_then(Value::as_str)
            .map(parse_source)
            .unwrap_or(Source::Api);
        let confidence = v
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| source.base_confidence());

        let sample = QuotaSample {
            owner: owner.to_owned(),
            vendor: vendor.clone(),
            pool: pool.to_owned(),
            window: window.clone(),
            observed_at,
            used_pct,
            resets_at,
            source,
            confidence,
        };

        if on_sample(&mut meter, sample.clone()) {
            tx.execute(
                "INSERT INTO QUOTA_SAMPLE (
                    owner, pool, window, observed_at, vendor, used_pct, resets_at, source, confidence, msg_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(owner, pool, window, observed_at) DO UPDATE SET
                    vendor = excluded.vendor,
                    used_pct = CASE
                        WHEN excluded.source = QUOTA_SAMPLE.source THEN MAX(QUOTA_SAMPLE.used_pct, excluded.used_pct)
                        ELSE excluded.used_pct
                    END,
                    resets_at = excluded.resets_at,
                    source = excluded.source,
                    confidence = excluded.confidence,
                    msg_id = excluded.msg_id",
                params![
                    sample.owner,
                    sample.pool,
                    sample.window,
                    sample.observed_at as i64,
                    sample.vendor,
                    sample.used_pct,
                    sample.resets_at as i64,
                    source_str(sample.source),
                    sample.confidence,
                    msg_id,
                ],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    upsert_meter(tx, &meter, msg_id)?;
    Ok(())
}

fn apply_spend_fact(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: u64,
    msg_id: i64,
) -> Result<(), String> {
    let parts: Vec<&str> = msg.topic.split('/').collect();
    let topic_owner = parts.get(1).copied().unwrap_or("");
    let topic_pool = parts.get(2).copied().unwrap_or("");

    let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
        return Ok(());
    };
    let owner = v["owner"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_owner);
    if owner.is_empty() {
        return Ok(());
    }
    if msg.role != Role::Hub && !msg.from.is_empty() && owner != msg.from {
        return Ok(());
    }
    if !topic_owner.is_empty() && topic_owner != owner {
        return Ok(());
    }

    let pool = v["pool"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_pool);
    if pool.is_empty() {
        return Ok(());
    }

    let task_id = v["task_id"].as_str().unwrap_or("");
    let at = v["at"].as_u64().unwrap_or(ts_sec);
    let file_key = v["file_key"].as_str().unwrap_or("");
    let byte_offset = v["byte_offset"].as_u64().unwrap_or(0);
    let worker = v["worker"].as_str().unwrap_or("");
    let model = v["model"].as_str().unwrap_or("");
    let units = v["units"].as_f64().unwrap_or(0.0);
    let input_tokens = v["input_tokens"].as_u64().unwrap_or(0);
    let cached_tokens = v["cached_tokens"].as_u64().unwrap_or(0);
    let output_tokens = v["output_tokens"].as_u64().unwrap_or(0);
    let raw_counters = if let Some(rc) = v.get("raw_counters") {
        rc.to_string()
    } else {
        json!({
            "input_tokens": input_tokens,
            "cached_tokens": cached_tokens,
            "output_tokens": output_tokens,
        })
        .to_string()
    };

    // If keyed by (file_key, byte_offset) with a non-empty file_key, ignore duplicate (owner, pool, file_key, byte_offset)
    // even if `at` or `task_id` differed, preventing double-counting when a session file is read twice.
    if !file_key.is_empty() {
        let already: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM SPEND WHERE owner = ?1 AND pool = ?2 AND file_key = ?3 AND byte_offset = ?4 LIMIT 1",
                params![owner, pool, file_key, byte_offset as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if already.is_some() {
            return Ok(());
        }
    }

    tx.execute(
        "INSERT OR IGNORE INTO SPEND (
            owner, pool, task_id, at, file_key, byte_offset,
            worker, model, units, input_tokens, cached_tokens, output_tokens, raw_counters, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            owner,
            pool,
            task_id,
            at as i64,
            file_key,
            byte_offset as i64,
            worker,
            model,
            units,
            input_tokens as i64,
            cached_tokens as i64,
            output_tokens as i64,
            raw_counters,
            msg_id,
        ],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

fn apply_estimate_fact(tx: &Transaction<'_>, msg: &Message, msg_id: i64) -> Result<(), String> {
    let parts: Vec<&str> = msg.topic.split('/').collect();
    let topic_kind = parts.get(1).copied().unwrap_or("");
    let topic_class = parts.get(2).copied().unwrap_or("");

    let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
        return Ok(());
    };
    let kind = v["kind"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_kind);
    let class = v["class"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_class);
    if kind.is_empty() || class.is_empty() {
        return Ok(());
    }
    let def = Estimate::default_for(kind, class);
    let n = v["n"].as_u64().unwrap_or(def.n);
    let minutes_p50 = v["minutes_p50"].as_f64().unwrap_or(def.minutes_p50);
    let minutes_p90 = v["minutes_p90"].as_f64().unwrap_or(def.minutes_p90);
    let units_p50 = v["units_p50"].as_f64().unwrap_or(def.units_p50);
    let units_p90 = v["units_p90"].as_f64().unwrap_or(def.units_p90);
    let source = v["source"]
        .as_str()
        .map(parse_estimate_source)
        .unwrap_or(if n >= 5 {
            EstimateSource::Learned
        } else {
            EstimateSource::Default
        });

    tx.execute(
        "INSERT INTO ESTIMATE (
            kind, class, n, minutes_p50, minutes_p90, units_p50, units_p90, source, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(kind, class) DO UPDATE SET
            n = excluded.n,
            minutes_p50 = excluded.minutes_p50,
            minutes_p90 = excluded.minutes_p90,
            units_p50 = excluded.units_p50,
            units_p90 = excluded.units_p90,
            source = excluded.source,
            msg_id = excluded.msg_id",
        params![
            kind,
            class,
            n as i64,
            minutes_p50,
            minutes_p90,
            units_p50,
            units_p90,
            estimate_source_str(source),
            msg_id,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply_probe_broken_alert(
    tx: &Transaction<'_>,
    msg: &Message,
    msg_id: i64,
) -> Result<(), String> {
    let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
        return Ok(());
    };
    let data = v.get("data").unwrap_or(&v);
    let owner = data["owner"].as_str().unwrap_or("");
    let vendor = data["vendor"].as_str().unwrap_or("");
    if owner.is_empty() {
        return Ok(());
    }
    if let Some(pools) = data["pools"].as_array() {
        for p in pools.iter().filter_map(Value::as_str) {
            let win = infer_window(p);
            let vend = if vendor.is_empty() {
                infer_vendor(p)
            } else {
                vendor.to_owned()
            };
            let mut m = load_meter_with_series(tx, owner, p, &win, &vend)?;
            m.probe_enabled = false;
            for s in &mut m.series {
                if s.source == Source::Api {
                    s.source = Source::Inferred;
                    s.confidence = Source::Inferred.base_confidence();
                }
            }
            if let Some(latest) = &mut m.latest
                && latest.source == Source::Api
            {
                latest.source = Source::Inferred;
                latest.confidence = Source::Inferred.base_confidence();
            }
            upsert_meter(tx, &m, msg_id)?;
        }
    } else if !vendor.is_empty() {
        tx.execute(
            "UPDATE METER SET probe_enabled = 0, msg_id = ?3 WHERE owner = ?1 AND vendor = ?2",
            params![owner, vendor, msg_id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn load_meter_with_series(
    conn: &Connection,
    owner: &str,
    pool: &str,
    window: &str,
    vendor: &str,
) -> Result<Meter, String> {
    let existing = conn
        .query_row(
            "SELECT vendor, latest_sample, state_event, unit, pct_per_unit, n_calibration,
                    capacity_units, resets_at, probe_enabled
             FROM METER WHERE owner = ?1 AND pool = ?2 AND window = ?3",
            params![owner, pool, window],
            |r| {
                let vend: String = r.get(0)?;
                let latest_str: String = r.get(1)?;
                let se_str: Option<String> = r.get(2)?;
                let unit: String = r.get(3)?;
                let pct_per_unit: f64 = r.get(4)?;
                let n_calibration: i64 = r.get(5)?;
                let capacity_units: f64 = r.get(6)?;
                let resets_at: i64 = r.get(7)?;
                let probe_enabled: i64 = r.get(8)?;
                Ok((
                    vend,
                    latest_str,
                    se_str,
                    unit,
                    pct_per_unit,
                    n_calibration as u64,
                    capacity_units,
                    resets_at as u64,
                    probe_enabled != 0,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;

    let mut meter = Meter::new(owner, vendor, pool, window);
    if let Some((
        vend,
        latest_str,
        se_str,
        unit,
        pct_per_unit,
        n_calibration,
        capacity_units,
        resets_at,
        probe_enabled,
    )) = existing
    {
        if !vend.is_empty() {
            meter.vendor = vend.clone();
            meter.policy = MeterPolicy::default_for(&vend, window);
        }
        meter.latest = serde_json::from_str::<QuotaSample>(&latest_str).ok();
        meter.state_event = se_str.and_then(|s| serde_json::from_str::<StateEvent>(&s).ok());
        meter.unit = unit;
        meter.pct_per_unit = pct_per_unit;
        meter.n_calibration = n_calibration;
        meter.capacity_units = capacity_units;
        meter.resets_at = resets_at;
        meter.probe_enabled = probe_enabled;
    }

    let mut stmt = conn
        .prepare(
            "SELECT owner, vendor, pool, window, observed_at, used_pct, resets_at, source, confidence
             FROM QUOTA_SAMPLE
             WHERE owner = ?1 AND pool = ?2 AND window = ?3
             ORDER BY observed_at ASC",
        )
        .map_err(|e| e.to_string())?;
    let samples = stmt
        .query_map(params![owner, pool, window], |r| {
            let src_s: String = r.get(7)?;
            Ok(QuotaSample {
                owner: r.get(0)?,
                vendor: r.get(1)?,
                pool: r.get(2)?,
                window: r.get(3)?,
                observed_at: r.get::<_, i64>(4)? as u64,
                used_pct: r.get(5)?,
                resets_at: r.get::<_, i64>(6)? as u64,
                source: parse_source(&src_s),
                confidence: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    for mut s in samples {
        if !meter.probe_enabled && s.source == Source::Api {
            s.source = Source::Inferred;
            s.confidence = Source::Inferred.base_confidence();
        }
        on_sample(&mut meter, s);
    }

    Ok(meter)
}

fn upsert_meter(conn: &Connection, m: &Meter, msg_id: i64) -> Result<(), String> {
    let latest_json = m
        .latest
        .as_ref()
        .and_then(|s| serde_json::to_string(s).ok())
        .unwrap_or_else(|| "{}".into());
    let se_json = m
        .state_event
        .as_ref()
        .and_then(|se| serde_json::to_string(se).ok());

    conn.execute(
        "INSERT INTO METER (
            owner, pool, window, vendor, latest_sample, state_event,
            unit, pct_per_unit, n_calibration, capacity_units, resets_at, probe_enabled, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(owner, pool, window) DO UPDATE SET
            vendor = excluded.vendor,
            latest_sample = excluded.latest_sample,
            state_event = excluded.state_event,
            unit = excluded.unit,
            pct_per_unit = excluded.pct_per_unit,
            n_calibration = excluded.n_calibration,
            capacity_units = excluded.capacity_units,
            resets_at = excluded.resets_at,
            probe_enabled = excluded.probe_enabled,
            msg_id = excluded.msg_id",
        params![
            m.owner,
            m.pool,
            m.window,
            m.vendor,
            latest_json,
            se_json,
            m.unit,
            m.pct_per_unit,
            m.n_calibration as i64,
            m.capacity_units,
            m.resets_at as i64,
            i64::from(m.probe_enabled),
            msg_id,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Queries all rows from `QUOTA_SAMPLE` ordered by `(owner, pool, window, observed_at)`.
pub fn query_quota_samples(conn: &Connection) -> Result<Vec<QuotaSampleRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, vendor, pool, window, observed_at, used_pct, resets_at, source, confidence, msg_id
             FROM QUOTA_SAMPLE
             ORDER BY owner ASC, pool ASC, window ASC, observed_at ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let src_s: String = r.get(7)?;
            Ok(QuotaSampleRow {
                owner: r.get(0)?,
                vendor: r.get(1)?,
                pool: r.get(2)?,
                window: r.get(3)?,
                observed_at: r.get::<_, i64>(4)? as u64,
                used_pct: r.get(5)?,
                resets_at: r.get::<_, i64>(6)? as u64,
                source: parse_source(&src_s),
                confidence: r.get(8)?,
                msg_id: r.get::<_, i64>(9)? as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Queries all rows from `METER` ordered by `(owner, pool, window)`.
pub fn query_meter_rows(conn: &Connection) -> Result<Vec<MeterTableRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, vendor, pool, window, latest_sample, state_event,
                    unit, pct_per_unit, n_calibration, capacity_units, resets_at, probe_enabled, msg_id
             FROM METER
             ORDER BY owner ASC, pool ASC, window ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let latest_s: String = r.get(4)?;
            let se_s: Option<String> = r.get(5)?;
            Ok(MeterTableRow {
                owner: r.get(0)?,
                vendor: r.get(1)?,
                pool: r.get(2)?,
                window: r.get(3)?,
                latest_sample: serde_json::from_str::<QuotaSample>(&latest_s).ok(),
                state_event: se_s.and_then(|s| serde_json::from_str::<StateEvent>(&s).ok()),
                unit: r.get(6)?,
                pct_per_unit: r.get(7)?,
                n_calibration: r.get::<_, i64>(8)? as u64,
                capacity_units: r.get(9)?,
                resets_at: r.get::<_, i64>(10)? as u64,
                probe_enabled: r.get::<_, i64>(11)? != 0,
                msg_id: r.get::<_, i64>(12)? as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reconstructs full [`Meter`] objects (including sample series from `QUOTA_SAMPLE`) for every row in `METER`.
pub fn load_all_meters(conn: &Connection) -> Result<Vec<Meter>, String> {
    let rows = query_meter_rows(conn)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(load_meter_with_series(
            conn, &r.owner, &r.pool, &r.window, &r.vendor,
        )?);
    }
    Ok(out)
}

/// Queries all rows from `SPEND` ordered by `(owner, pool, at, task_id, file_key, byte_offset)`.
pub fn query_spend(conn: &Connection) -> Result<Vec<SpendTableRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, pool, task_id, at, file_key, byte_offset,
                    worker, model, units, input_tokens, cached_tokens, output_tokens, raw_counters, msg_id
             FROM SPEND
             ORDER BY owner ASC, pool ASC, at ASC, task_id ASC, file_key ASC, byte_offset ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let raw_s: String = r.get(12)?;
            Ok(SpendTableRow {
                owner: r.get(0)?,
                pool: r.get(1)?,
                task_id: r.get(2)?,
                at: r.get::<_, i64>(3)? as u64,
                file_key: r.get(4)?,
                byte_offset: r.get::<_, i64>(5)? as u64,
                worker: r.get(6)?,
                model: r.get(7)?,
                units: r.get(8)?,
                input_tokens: r.get::<_, i64>(9)? as u64,
                cached_tokens: r.get::<_, i64>(10)? as u64,
                output_tokens: r.get::<_, i64>(11)? as u64,
                raw_counters: serde_json::from_str(&raw_s).unwrap_or_else(|_| json!({})),
                msg_id: r.get::<_, i64>(13)? as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Queries all rows from `ESTIMATE` ordered by `(kind, class)`.
pub fn query_estimates(conn: &Connection) -> Result<Vec<EstimateTableRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT kind, class, n, minutes_p50, minutes_p90, units_p50, units_p90, source, msg_id
             FROM ESTIMATE
             ORDER BY kind ASC, class ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let src_s: String = r.get(7)?;
            Ok(EstimateTableRow {
                kind: r.get(0)?,
                class: r.get(1)?,
                n: r.get::<_, i64>(2)? as u64,
                minutes_p50: r.get(3)?,
                minutes_p90: r.get(4)?,
                units_p50: r.get(5)?,
                units_p90: r.get(6)?,
                source: parse_estimate_source(&src_s),
                msg_id: r.get::<_, i64>(8)? as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn default_pools_for_class(cls: &str) -> Vec<&'static str> {
    let c = cls.to_ascii_lowercase();
    if c.starts_with("codex") {
        vec!["codex.5h", "codex.week"]
    } else if c.starts_with("gemini") || c.starts_with("agy") {
        vec!["agy.window", "agy.week", "agy.day"]
    } else if c.starts_with("claude") {
        vec!["claude.5h", "claude.week"]
    } else {
        vec![]
    }
}

/// Evaluates the `COORD_BUDGET` alert rule at `budget_alarm_pct` (70 percent, section 13.6):
///
/// "COORD_BUDGET when a pool of the coordinator or the brain passes `budget_alarm_pct` (70)
/// on any window: the hub checks that a successor is eligible and says so in the alert.
/// If none is, the owner hears it hours before the role would go vacant."
///
/// An alert is emitted only when not already open for the same `(owner, pool)` key,
/// and closed when the pool drops back below `budget_alarm_pct`.
pub fn check_coord_budget_alerts(
    conn: &Connection,
    coord_policy: Option<&RolePolicy>,
    budget_alarm_pct: f64,
    now_sec: u64,
) -> Result<Vec<Message>, String> {
    let meters = load_all_meters(conn)?;
    if meters.is_empty() {
        return Ok(Vec::new());
    }

    let cfg = QuotaConfig::default();
    let meter_by_owner_pool: BTreeMap<(String, String), &Meter> = meters
        .iter()
        .map(|m| ((m.owner.clone(), m.pool.clone()), m))
        .collect();

    // Determine who holds `coordinator` or `brain` (from ROLE table if present, else policy/orchestrator)
    let mut role_holders: Vec<(String, String, String)> = Vec::new(); // (role_name, holder_who, holder_owner)
    if let Ok(role_rows) = crate::role::query_all(conn) {
        for r in role_rows {
            if matches!(r.role.as_str(), "coordinator" | "brain")
                && let Some(h) = r.holder
                && !h.is_empty()
            {
                let owner = h.split_once('/').map_or(h.as_str(), |(o, _)| o).to_owned();
                role_holders.push((r.role, h, owner));
            }
        }
    }

    // If no explicit ROLE row is held yet, fall back to the first candidate in `coord_policy`,
    // or any orchestrator member in `MEMBER`, or any owner of a `claude.*` meter.
    if role_holders.is_empty() {
        if let Some(pol) = coord_policy
            && let Some(first) = pol.candidates.first()
        {
            role_holders.push((
                "coordinator".into(),
                first.who.clone(),
                first.owner().to_owned(),
            ));
        } else if let Ok(members) = crate::store::query_members(conn) {
            for m in members {
                if m.role == "orchestrator" {
                    role_holders.push(("coordinator".into(), m.name.clone(), m.name));
                }
            }
        }
    }
    if role_holders.is_empty() {
        for m in &meters {
            if m.vendor == "claude" && !role_holders.iter().any(|(_, _, o)| o == &m.owner) {
                role_holders.push(("coordinator".into(), m.owner.clone(), m.owner.clone()));
            }
        }
    }

    // Helper to check if a candidate's pools are open/throttled and below the worker line
    let candidate_has_quota = |cand: &RoleCandidate| -> bool {
        let owner = cand.owner();
        let pools = if cand.class.is_empty() {
            vec!["claude.5h", "claude.week"]
        } else {
            default_pools_for_class(&cand.class)
        };
        for p in pools {
            if let Some(m) = meter_by_owner_pool.get(&(owner.to_owned(), p.to_owned())) {
                let f = forecast(m, now_sec, &[]);
                let st = pool_state(m, &f, now_sec, &cfg);
                if matches!(st, PoolState::Closed | PoolState::Paused | PoolState::Drain)
                    || f.used >= budget_alarm_pct
                {
                    return false;
                }
            }
        }
        true
    };

    let find_successor = |current_who: &str, current_owner: &str| -> Option<String> {
        if let Some(pol) = coord_policy {
            let mut table = RoleTable::new();
            table.set_policy("coordinator", pol.clone());
            for cand in table.eligible_candidates("coordinator", now_sec) {
                if cand.who != current_who && candidate_has_quota(&cand) {
                    return Some(cand.who);
                }
            }
        }
        // Also check if another meter owner or class on the same owner has headroom (< budget_alarm_pct)
        for m in &meters {
            if m.owner == current_owner && m.vendor != "claude" {
                let f = forecast(m, now_sec, &[]);
                let st = pool_state(m, &f, now_sec, &cfg);
                if matches!(st, PoolState::Open | PoolState::Throttled) && f.used < budget_alarm_pct
                {
                    return Some(format!("{}/coord-{}", m.owner, m.vendor));
                }
            }
        }
        None
    };

    let mut out = Vec::new();
    let ts_str = rfc3339(now_sec * 1000);

    for m in &meters {
        let matching_role = role_holders.iter().find(|(_, _, owner)| owner == &m.owner);
        let Some((role_name, holder_who, holder_owner)) = matching_role else {
            continue;
        };

        // Check if this meter belongs to the coordinator/brain (e.g. claude pools, or the holder's class pools)
        let is_coord_pool = m.vendor == "claude"
            || coord_policy.is_some_and(|pol| {
                pol.candidates
                    .iter()
                    .find(|c| &c.who == holder_who)
                    .is_some_and(|c| {
                        default_pools_for_class(&c.class)
                            .iter()
                            .any(|p| *p == m.pool)
                    })
            });
        if !is_coord_pool {
            continue;
        }

        let f = forecast(m, now_sec, &[]);
        let key = format!("{}/{}", m.owner, m.pool);
        let existing_state: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT state, opened_at, count FROM ALERT WHERE code = 'COORD_BUDGET' AND key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        if f.used >= budget_alarm_pct {
            let already_open = existing_state
                .as_ref()
                .is_some_and(|(st, _, _)| st == "open");
            if !already_open {
                let successor = find_successor(holder_who, holder_owner);
                let (succ_text, severity) = match &successor {
                    Some(succ) => (format!("eligible successor: {succ}"), "warn"),
                    None => ("no eligible successor".to_owned(), "error"),
                };
                let text = format!(
                    "{role_name} pool {} on {} reached {:.1}% (alarm at {:.0}%); {succ_text}",
                    m.pool, m.owner, f.used, budget_alarm_pct
                );
                let body = json!({
                    "v": 1,
                    "code": "COORD_BUDGET",
                    "key": key,
                    "severity": severity,
                    "state": "open",
                    "opened_at": now_sec,
                    "last_at": now_sec,
                    "count": 1,
                    "text": text,
                    "data": {
                        "role": role_name,
                        "holder": holder_who,
                        "owner": m.owner,
                        "pool": m.pool,
                        "window": m.window,
                        "used_pct": f.used,
                        "budget_alarm_pct": budget_alarm_pct,
                        "successor_eligible": successor.is_some(),
                        "successor": successor,
                    }
                })
                .to_string();
                out.push(Message {
                    id: 0,
                    ts: ts_str.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Alert,
                    topic: format!("alert/COORD_BUDGET/{key}"),
                    body,
                    refs: vec!["state:open".into()],
                    to: vec![m.owner.clone()],
                });
            }
        } else if let Some((st, opened_at, count)) = existing_state
            && st == "open"
        {
            let text = format!(
                "{role_name} pool {} on {} recovered to {:.1}%",
                m.pool, m.owner, f.used
            );
            let body = json!({
                "v": 1,
                "code": "COORD_BUDGET",
                "key": key,
                "severity": "info",
                "state": "closed",
                "opened_at": opened_at,
                "last_at": now_sec,
                "count": count,
                "text": text,
                "data": {
                    "role": role_name,
                    "holder": holder_who,
                    "owner": m.owner,
                    "pool": m.pool,
                    "window": m.window,
                    "used_pct": f.used,
                }
            })
            .to_string();
            out.push(Message {
                id: 0,
                ts: ts_str.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/COORD_BUDGET/{key}"),
                body,
                refs: vec!["state:closed".into()],
                to: vec![m.owner.clone()],
            });
        }
    }

    Ok(out)
}
