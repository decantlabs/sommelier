//! Pure deterministic scheduler, DECISION projection, pause/resume, and simulator (step 11).
//!
//! Implements section 4.1 (`plan`), admission wiring of sections 7.1 and 7.2, `DECISION` table
//! projection of section 3, shadow comparison, safe mode of section 9, and `pause`/`resume`.
//! All internal collections use `BTreeMap` and `BTreeSet` so output is byte-equal across
//! arbitrary input permutations.

pub mod assign;
pub mod invariants;
pub mod score;
pub mod sim;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::{Kind, Message, Role};
use crate::quota::apply::{load_all_meters, query_estimates};
use crate::quota::estimate::Estimate;
use crate::quota::meter::{Meter, MeterPolicy, QuotaConfig};
use crate::store::{StoreModule, query_capacities, query_members};
use crate::task::{query_leases, query_offers, query_tasks};
use crate::time::{parse_ms, rfc3339};

pub use assign::{
    AssignContext, FreeSlot, SlotEvaluation, SlotRejection, TaskExplanation, build_free_slots,
    evaluate_slot_for_task, find_scope_conflict, is_slot_admissible, pick_best_slot,
    resolve_estimate,
};
pub use invariants::{
    InvariantViolation, SafeModeState, check_invariants, evaluate_safe_mode, guard_actions,
};
pub use score::{
    PriorityConfig, ScoreBreakdown, compute_running_shares, score_task, sort_ready_tasks,
};
pub use sim::{
    ShadowComparison, ShadowMismatch, SimDecisionDiff, SimReport, compare_shadow, simulate_log,
    simulate_log_file,
};

static PLAN_HUB_START_SEC: OnceLock<u64> = OnceLock::new();

/// Heartbeat interval (in seconds) after which an unchanged empty `FACT decision` is emitted.
pub const EMPTY_DECISION_HEARTBEAT_SEC: u64 = 300;

/// Names of tables owned by `PlanModule` (section 3, 10.2).
pub const PLAN_TABLES: [&str; 3] = ["DECISION", "SCHEDULER_PAUSE", "SCHEDULER_STATE"];

/// SQLite DDL for `DECISION`, `SCHEDULER_PAUSE`, and `SCHEDULER_STATE` in `state.db`.
pub const PLAN_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS DECISION (
    tick_id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL DEFAULT 0,
    policy_hash TEXT NOT NULL DEFAULT '',
    state_hash TEXT NOT NULL DEFAULT '',
    mode TEXT NOT NULL DEFAULT 'shadow',
    actions TEXT NOT NULL DEFAULT '[]',
    applied INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS SCHEDULER_PAUSE (
    scope TEXT PRIMARY KEY,
    paused INTEGER NOT NULL DEFAULT 1,
    by TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS SCHEDULER_STATE (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL DEFAULT '',
    msg_id INTEGER NOT NULL DEFAULT 0
);
";

/// Scheduler or admission operating mode (sections 4, 5.3, 9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanMode {
    Off,
    #[default]
    Shadow,
    Enforce,
}

impl PlanMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }
}

/// Model capability tier (section 5.3: `low | middle | strong | top`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Low = 1,
    #[default]
    Middle = 2,
    Strong = 3,
    Top = 4,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Middle => "middle",
            Self::Strong => "strong",
            Self::Top => "top",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "middle" => Some(Self::Middle),
            "strong" => Some(Self::Strong),
            "top" => Some(Self::Top),
            _ => None,
        }
    }
}

/// Scope targeted by `sommelier pause` / `sommelier resume` (section 9).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "lowercase")]
pub enum PauseScope {
    All,
    Class(String),
    Pool(String),
    Owner(String),
}

impl PauseScope {
    pub fn as_key(&self) -> String {
        match self {
            Self::All => "all".to_owned(),
            Self::Class(c) => format!("class/{c}"),
            Self::Pool(p) => format!("pool/{p}"),
            Self::Owner(o) => format!("owner/{o}"),
        }
    }

    pub fn parse(scope: &str) -> Result<Self, String> {
        let trimmed = scope.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        let parts: Vec<&str> = if trimmed.contains('/') {
            trimmed.splitn(2, '/').collect()
        } else if trimmed.contains(':') {
            trimmed.splitn(2, ':').collect()
        } else {
            trimmed.split_whitespace().collect()
        };
        if parts.len() != 2 || parts[1].trim().is_empty() {
            return Err(format!(
                "invalid pause scope `{scope}` (expected: all | class <c> | pool <p> | owner <o>)"
            ));
        }
        let target = parts[1].trim().to_owned();
        match parts[0].trim().to_ascii_lowercase().as_str() {
            "class" => Ok(Self::Class(target)),
            "pool" => Ok(Self::Pool(target)),
            "owner" => Ok(Self::Owner(target)),
            _ => Err(format!(
                "invalid pause scope `{scope}` (expected: all | class <c> | pool <p> | owner <o>)"
            )),
        }
    }
}

/// Kill switches per subsystem (`[switch]`, section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchConfig {
    #[serde(default = "default_enforce_mode")]
    pub scheduler: PlanMode,
    #[serde(default = "default_enforce_mode")]
    pub admission: PlanMode,
    #[serde(default)]
    pub probes: BTreeMap<String, bool>,
}

fn default_enforce_mode() -> PlanMode {
    PlanMode::Enforce
}

impl Default for SwitchConfig {
    fn default() -> Self {
        let mut probes = BTreeMap::new();
        probes.insert("codex".to_owned(), true);
        probes.insert("claude".to_owned(), true);
        probes.insert("gemini".to_owned(), true);
        Self {
            scheduler: PlanMode::Enforce,
            admission: PlanMode::Enforce,
            probes,
        }
    }
}

/// Scheduler timers (`[timers]`, section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTimers {
    #[serde(default = "default_tick_sec")]
    pub tick_sec: u64,
    #[serde(default = "default_suspect_sec")]
    pub suspect_sec: u64,
    #[serde(default = "default_dead_supervised_sec")]
    pub dead_supervised_sec: u64,
    #[serde(default = "default_lease_min")]
    pub lease_min: u64,
    #[serde(default = "default_offer_min")]
    pub offer_min: u64,
    #[serde(default = "default_restart_grace_sec")]
    pub restart_grace_sec: u64,
    #[serde(default = "default_idle_alert_min")]
    pub idle_alert_min: u64,
    #[serde(default = "default_safe_mode_stall_min")]
    pub safe_mode_stall_min: u64,
}

fn default_tick_sec() -> u64 {
    15
}
fn default_suspect_sec() -> u64 {
    120
}
fn default_dead_supervised_sec() -> u64 {
    180
}
fn default_lease_min() -> u64 {
    45
}
fn default_offer_min() -> u64 {
    5
}
fn default_restart_grace_sec() -> u64 {
    180
}
fn default_idle_alert_min() -> u64 {
    10
}
fn default_safe_mode_stall_min() -> u64 {
    10
}

impl Default for PlanTimers {
    fn default() -> Self {
        Self {
            tick_sec: default_tick_sec(),
            suspect_sec: default_suspect_sec(),
            dead_supervised_sec: default_dead_supervised_sec(),
            lease_min: default_lease_min(),
            offer_min: default_offer_min(),
            restart_grace_sec: default_restart_grace_sec(),
            idle_alert_min: default_idle_alert_min(),
            safe_mode_stall_min: default_safe_mode_stall_min(),
        }
    }
}

/// Class definition (`[class.<name>]`, section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassSpec {
    #[serde(default)]
    pub tier: Tier,
    #[serde(default = "default_cost_rank")]
    pub cost_rank: u32,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_per_owner: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_per_family: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_per_machine: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
    #[serde(default = "default_true")]
    pub spawn: bool,
}

fn default_cost_rank() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

impl Default for ClassSpec {
    fn default() -> Self {
        Self {
            tier: Tier::Middle,
            cost_rank: 2,
            pools: Vec::new(),
            family_pool: None,
            cap_per_owner: None,
            cap_per_family: None,
            cap_per_machine: None,
            lock: None,
            spawn: true,
        }
    }
}

/// Trust level rules (`[trust.<name>]`, section 5.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSpec {
    #[serde(default)]
    pub may_claim: BTreeSet<String>,
}

/// Task kind rules (`[kind.<name>]`, section 5.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindSpec {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tier: Option<Tier>,
    #[serde(default = "default_true")]
    pub checkpointable: bool,
    #[serde(default)]
    pub clean_room: bool,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

fn default_max_attempts() -> u32 {
    3
}

/// Complete scheduler policy (section 5.3 v2, backward compatible with v1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanPolicy {
    #[serde(default = "default_policy_version")]
    pub version: u64,
    #[serde(default = "default_enforce_mode")]
    pub mode: PlanMode,
    #[serde(default)]
    pub switch: SwitchConfig,
    #[serde(default)]
    pub timers: PlanTimers,
    #[serde(default = "default_classes")]
    pub classes: BTreeMap<String, ClassSpec>,
    #[serde(default)]
    pub meters: BTreeMap<String, MeterPolicy>,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub default_estimates: BTreeMap<String, Estimate>,
    #[serde(default = "default_trust")]
    pub trust: BTreeMap<String, TrustSpec>,
    #[serde(default = "default_kinds")]
    pub kinds: BTreeMap<String, KindSpec>,
    #[serde(default)]
    pub priority: PriorityConfig,
    #[serde(default = "default_backlog_min_ready")]
    pub backlog_min_ready: usize,
}

fn default_policy_version() -> u64 {
    2
}
fn default_backlog_min_ready() -> usize {
    3
}

fn default_classes() -> BTreeMap<String, ClassSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "gemini_build".to_owned(),
        ClassSpec {
            tier: Tier::Middle,
            cost_rank: 1,
            pools: vec!["agy.window".into(), "agy.week".into(), "agy.day".into()],
            family_pool: Some("agy.family".into()),
            cap_per_owner: Some(4),
            cap_per_family: Some(2),
            cap_per_machine: None,
            lock: None,
            spawn: true,
        },
    );
    m.insert(
        "gemini".to_owned(),
        ClassSpec {
            tier: Tier::Middle,
            cost_rank: 1,
            pools: vec!["agy.window".into(), "agy.week".into(), "agy.day".into()],
            family_pool: Some("agy.family".into()),
            cap_per_owner: Some(4),
            cap_per_family: Some(2),
            cap_per_machine: None,
            lock: None,
            spawn: true,
        },
    );
    m.insert(
        "codex".to_owned(),
        ClassSpec {
            tier: Tier::Middle,
            cost_rank: 2,
            pools: vec!["codex.5h".into(), "codex.week".into()],
            family_pool: None,
            cap_per_owner: Some(6),
            cap_per_family: None,
            cap_per_machine: None,
            lock: None,
            spawn: true,
        },
    );
    m.insert(
        "claude_cheap".to_owned(),
        ClassSpec {
            tier: Tier::Low,
            cost_rank: 2,
            pools: vec!["claude.5h".into(), "claude.week".into()],
            family_pool: None,
            cap_per_owner: None,
            cap_per_family: None,
            cap_per_machine: None,
            lock: None,
            spawn: true,
        },
    );
    m.insert(
        "claude_strong".to_owned(),
        ClassSpec {
            tier: Tier::Strong,
            cost_rank: 3,
            pools: vec!["claude.5h".into(), "claude.week".into()],
            family_pool: None,
            cap_per_owner: Some(2),
            cap_per_family: None,
            cap_per_machine: None,
            lock: None,
            spawn: true,
        },
    );
    m.insert(
        "claude_top".to_owned(),
        ClassSpec {
            tier: Tier::Top,
            cost_rank: 4,
            pools: vec!["claude.5h".into(), "claude.week".into()],
            family_pool: None,
            cap_per_owner: Some(1),
            cap_per_family: None,
            cap_per_machine: None,
            lock: None,
            spawn: false,
        },
    );
    m.insert(
        "game_run".to_owned(),
        ClassSpec {
            tier: Tier::Low,
            cost_rank: 1,
            pools: Vec::new(),
            family_pool: None,
            cap_per_owner: None,
            cap_per_family: None,
            cap_per_machine: Some(1),
            lock: Some("game".into()),
            spawn: true,
        },
    );
    m
}

fn default_trust() -> BTreeMap<String, TrustSpec> {
    let mut m = BTreeMap::new();
    let mut guest = BTreeSet::new();
    for k in ["fix", "review", "run", "scout"] {
        guest.insert(k.to_owned());
    }
    m.insert("guest".to_owned(), TrustSpec { may_claim: guest });
    m
}

fn default_kinds() -> BTreeMap<String, KindSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "fix".to_owned(),
        KindSpec {
            required: vec!["scope".into(), "branch".into(), "done_when".into()],
            min_tier: Some(Tier::Middle),
            checkpointable: true,
            clean_room: false,
            max_attempts: 3,
        },
    );
    m.insert(
        "review".to_owned(),
        KindSpec {
            required: vec!["scope".into(), "done_when".into()],
            min_tier: Some(Tier::Low),
            checkpointable: false,
            clean_room: false,
            max_attempts: 3,
        },
    );
    m.insert(
        "scout".to_owned(),
        KindSpec {
            required: vec!["scope".into(), "done_when".into()],
            min_tier: Some(Tier::Low),
            checkpointable: false,
            clean_room: false,
            max_attempts: 3,
        },
    );
    m.insert(
        "run".to_owned(),
        KindSpec {
            required: vec!["scope".into(), "done_when".into()],
            min_tier: Some(Tier::Low),
            checkpointable: false,
            clean_room: false,
            max_attempts: 3,
        },
    );
    m.insert(
        "translator".to_owned(),
        KindSpec {
            required: vec!["scope".into(), "branch".into(), "done_when".into()],
            min_tier: Some(Tier::Strong),
            checkpointable: true,
            clean_room: true,
            max_attempts: 3,
        },
    );
    m
}

impl Default for PlanPolicy {
    fn default() -> Self {
        Self {
            version: default_policy_version(),
            mode: PlanMode::Enforce,
            switch: SwitchConfig::default(),
            timers: PlanTimers::default(),
            classes: default_classes(),
            meters: BTreeMap::new(),
            quota: QuotaConfig::default(),
            default_estimates: BTreeMap::new(),
            trust: default_trust(),
            kinds: default_kinds(),
            priority: PriorityConfig::default(),
            backlog_min_ready: default_backlog_min_ready(),
        }
    }
}

impl PlanPolicy {
    /// Returns the effective scheduler mode combining global `mode` and `switch.scheduler`.
    pub fn effective_scheduler_mode(&self) -> PlanMode {
        match (self.mode, self.switch.scheduler) {
            (PlanMode::Off, _) | (_, PlanMode::Off) => PlanMode::Off,
            (PlanMode::Shadow, _) | (_, PlanMode::Shadow) => PlanMode::Shadow,
            (PlanMode::Enforce, PlanMode::Enforce) => PlanMode::Enforce,
        }
    }

    /// Computes a deterministic SHA-256 hex digest of the policy.
    pub fn policy_hash(&self) -> String {
        let mut val = serde_json::to_value(self).unwrap_or(Value::Null);
        val.sort_all_objects();
        format!("{:x}", Sha256::digest(val.to_string().as_bytes()))
    }

    /// Parses a TOML policy file (supports both section 5.3 schema v2 and legacy v1 `swarm.policy.toml`).
    pub fn parse_toml(src: &str) -> Result<Self, String> {
        let tbl: toml::Table =
            toml::from_str(src).map_err(|e| format!("invalid policy TOML: {e}"))?;
        let mut policy = Self::default();

        if let Some(v) = tbl.get("version").and_then(toml::Value::as_integer) {
            policy.version = v.max(0) as u64;
        }
        if let Some(m) = tbl.get("mode").and_then(toml::Value::as_str) {
            policy.mode = PlanMode::parse(m).ok_or_else(|| format!("invalid policy mode `{m}`"))?;
        }

        if let Some(sw) = tbl.get("switch").and_then(toml::Value::as_table) {
            if let Some(s) = sw.get("scheduler").and_then(toml::Value::as_str) {
                policy.switch.scheduler =
                    PlanMode::parse(s).ok_or_else(|| format!("invalid switch.scheduler `{s}`"))?;
            }
            if let Some(s) = sw.get("admission").and_then(toml::Value::as_str) {
                policy.switch.admission =
                    PlanMode::parse(s).ok_or_else(|| format!("invalid switch.admission `{s}`"))?;
            }
            for (k, v) in sw {
                if let Some(vendor) = k.strip_prefix("probe_")
                    && let Some(s) = v.as_str()
                {
                    policy
                        .switch
                        .probes
                        .insert(vendor.to_owned(), s.eq_ignore_ascii_case("on"));
                }
            }
        }

        if let Some(t) = tbl.get("timers").and_then(toml::Value::as_table) {
            if let Some(v) = t.get("tick_sec").and_then(toml::Value::as_integer) {
                policy.timers.tick_sec = v.max(1) as u64;
            }
            if let Some(v) = t.get("suspect_sec").and_then(toml::Value::as_integer) {
                policy.timers.suspect_sec = v.max(0) as u64;
            }
            if let Some(v) = t
                .get("dead_supervised_sec")
                .and_then(toml::Value::as_integer)
            {
                policy.timers.dead_supervised_sec = v.max(0) as u64;
            }
            if let Some(v) = t.get("lease_min").and_then(toml::Value::as_integer) {
                policy.timers.lease_min = v.max(1) as u64;
            }
            if let Some(v) = t.get("offer_min").and_then(toml::Value::as_integer) {
                policy.timers.offer_min = v.max(1) as u64;
            }
            if let Some(v) = t.get("restart_grace_sec").and_then(toml::Value::as_integer) {
                policy.timers.restart_grace_sec = v.max(0) as u64;
            }
            if let Some(v) = t.get("idle_alert_min").and_then(toml::Value::as_integer) {
                policy.timers.idle_alert_min = v.max(1) as u64;
            }
            if let Some(v) = t
                .get("safe_mode_stall_min")
                .and_then(toml::Value::as_integer)
            {
                policy.timers.safe_mode_stall_min = v.max(1) as u64;
            }
        }

        if let Some(classes) = tbl.get("class").and_then(toml::Value::as_table) {
            for (cname, cval) in classes {
                if let Some(ct) = cval.as_table() {
                    let mut spec = policy.classes.get(cname).cloned().unwrap_or_default();
                    if let Some(s) = ct.get("tier").and_then(toml::Value::as_str)
                        && let Some(tier) = Tier::parse(s)
                    {
                        spec.tier = tier;
                    }
                    if let Some(v) = ct.get("cost_rank").and_then(toml::Value::as_integer) {
                        spec.cost_rank = v.max(0) as u32;
                    }
                    if let Some(arr) = ct.get("pools").and_then(toml::Value::as_array) {
                        spec.pools = arr
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .map(str::to_owned)
                            .collect();
                    }
                    if let Some(fp) = ct.get("family_pool").and_then(toml::Value::as_str) {
                        spec.family_pool = Some(fp.to_owned());
                    }
                    if let Some(v) = ct.get("cap_per_owner").and_then(toml::Value::as_integer) {
                        spec.cap_per_owner = Some(v.max(0) as u32);
                    }
                    if let Some(v) = ct.get("cap_per_family").and_then(toml::Value::as_integer) {
                        spec.cap_per_family = Some(v.max(0) as u32);
                    }
                    if let Some(v) = ct.get("cap_per_machine").and_then(toml::Value::as_integer) {
                        spec.cap_per_machine = Some(v.max(0) as u32);
                    }
                    if let Some(l) = ct.get("lock").and_then(toml::Value::as_str) {
                        spec.lock = Some(l.to_owned());
                    }
                    if let Some(sp) = ct.get("spawn").and_then(toml::Value::as_bool) {
                        spec.spawn = sp;
                    }
                    policy.classes.insert(cname.clone(), spec);
                }
            }
        }

        if let Some(meters) = tbl.get("meter").and_then(toml::Value::as_table) {
            for (mname, mval) in meters {
                if let Some(mt) = mval.as_table() {
                    let vendor = mt
                        .get("vendor")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("codex");
                    let window = mt
                        .get("window")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("5h");
                    let mut mp = MeterPolicy::default_for(vendor, window);
                    if let Some(v) = mt.get("stop_at").and_then(toml_as_f64) {
                        mp.stop_at = v;
                    }
                    if let Some(v) = mt.get("reserve_pct").and_then(toml_as_f64) {
                        mp.reserve_pct = v;
                    }
                    if let Some(v) = mt.get("pace").and_then(toml::Value::as_bool) {
                        mp.pace = v;
                    }
                    if let Some(v) = mt.get("drain_min").and_then(toml::Value::as_integer) {
                        mp.drain_min = v.max(0) as u64;
                    }
                    if let Some(v) = mt.get("drain_band_pct").and_then(toml_as_f64) {
                        mp.drain_band_pct = v;
                    }
                    if let Some(v) = mt.get("fresh_min").and_then(toml::Value::as_integer) {
                        mp.fresh_min = v.max(0) as u64;
                    }
                    if let Some(v) = mt.get("burn_window_min").and_then(toml::Value::as_integer) {
                        mp.burn_window_min = v.max(1) as u64;
                    }
                    policy.meters.insert(mname.clone(), mp);
                }
            }
        }

        if let Some(q) = tbl.get("quota").and_then(toml::Value::as_table) {
            if let Some(margin) = q.get("margin").and_then(toml::Value::as_table) {
                if let Some(v) = margin.get("high").and_then(toml_as_f64) {
                    policy.quota.margin_high = v;
                }
                if let Some(v) = margin.get("medium").and_then(toml_as_f64) {
                    policy.quota.margin_medium = v;
                }
                if let Some(v) = margin.get("low").and_then(toml_as_f64) {
                    policy.quota.margin_low = v;
                }
            }
            if let Some(v) = q.get("min_conf").and_then(toml_as_f64) {
                policy.quota.min_conf = v;
            }
            if let Some(v) = q.get("throttle_slack").and_then(toml_as_f64) {
                policy.quota.throttle_slack = v;
            }
            if let Some(v) = q.get("drain_short_min").and_then(toml_as_f64) {
                policy.quota.drain_short_min = v;
            }
        }

        if let Some(est_tbl) = tbl
            .get("estimate")
            .and_then(toml::Value::as_table)
            .and_then(|e| e.get("default"))
            .and_then(toml::Value::as_table)
        {
            for (cls, eval) in est_tbl {
                if let Some(et) = eval.as_table() {
                    let mut est = Estimate::default_for("fix", cls);
                    if let Some(v) = et.get("minutes_p50").and_then(toml_as_f64) {
                        est.minutes_p50 = v;
                    }
                    if let Some(v) = et.get("minutes_p90").and_then(toml_as_f64) {
                        est.minutes_p90 = v;
                    }
                    if let Some(v) = et.get("units_p50").and_then(toml_as_f64) {
                        est.units_p50 = v;
                    }
                    if let Some(v) = et.get("units_p90").and_then(toml_as_f64) {
                        est.units_p90 = v;
                    }
                    policy.default_estimates.insert(cls.clone(), est);
                }
            }
        }

        if let Some(tr) = tbl.get("trust").and_then(toml::Value::as_table) {
            for (tname, tval) in tr {
                if let Some(tt) = tval.as_table()
                    && let Some(arr) = tt.get("may_claim").and_then(toml::Value::as_array)
                {
                    let set: BTreeSet<String> = arr
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    policy
                        .trust
                        .insert(tname.clone(), TrustSpec { may_claim: set });
                }
            }
        }

        if let Some(kinds) = tbl.get("kind").and_then(toml::Value::as_table) {
            for (kname, kval) in kinds {
                if let Some(kt) = kval.as_table() {
                    let mut ks = policy.kinds.get(kname).cloned().unwrap_or_default();
                    if let Some(arr) = kt.get("required").and_then(toml::Value::as_array) {
                        ks.required = arr
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .map(str::to_owned)
                            .collect();
                    }
                    if let Some(s) = kt.get("min_tier").and_then(toml::Value::as_str) {
                        ks.min_tier = Tier::parse(s);
                    }
                    if let Some(b) = kt.get("checkpointable").and_then(toml::Value::as_bool) {
                        ks.checkpointable = b;
                    }
                    if let Some(b) = kt.get("clean_room").and_then(toml::Value::as_bool) {
                        ks.clean_room = b;
                    }
                    if let Some(n) = kt.get("max_attempts").and_then(toml::Value::as_integer) {
                        ks.max_attempts = n.max(1) as u32;
                    }
                    policy.kinds.insert(kname.clone(), ks);
                }
            }
        }

        if let Some(pr) = tbl.get("priority").and_then(toml::Value::as_table) {
            if let Some(lv) = pr.get("levels").and_then(toml::Value::as_table) {
                for (k, v) in lv {
                    if let Some(n) = v.as_integer() {
                        policy.priority.levels.insert(k.clone(), n);
                    }
                }
            }
            if let Some(n) = pr.get("aging_per_10min").and_then(toml::Value::as_integer) {
                policy.priority.aging_per_10min = n;
            }
            if let Some(n) = pr.get("aging_cap").and_then(toml::Value::as_integer) {
                policy.priority.aging_cap = n;
            }
            if let Some(n) = pr.get("resume_boost").and_then(toml::Value::as_integer) {
                policy.priority.resume_boost = n;
            }
            if let Some(n) = pr.get("track_weight").and_then(toml_as_f64) {
                policy.priority.track_weight = n;
            }
            if let Some(tr) = pr.get("tracks").and_then(toml::Value::as_table) {
                let mut tracks = BTreeMap::new();
                for (k, v) in tr {
                    if let Some(f) = toml_as_f64(v) {
                        tracks.insert(k.clone(), f);
                    }
                }
                policy.priority.tracks = tracks;
            }
        }

        if let Some(bl) = tbl.get("backlog").and_then(toml::Value::as_table)
            && let Some(n) = bl.get("min_ready").and_then(toml::Value::as_integer)
        {
            policy.backlog_min_ready = n.max(0) as usize;
        }

        Ok(policy)
    }
}

fn toml_as_f64(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Member record for scheduler trust lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMember {
    pub name: String,
    #[serde(default = "default_owner_trust")]
    pub trust: String,
    #[serde(default = "default_true")]
    pub online: bool,
    /// The tier of the member (TIER table, `-1` for no tier at all). `None` when the caller gave
    /// none: then the tier follows from `trust` (see `assign::member_tier`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<i64>,
}

fn default_owner_trust() -> String {
    "owner".to_owned()
}

/// Declared slot capacity for `(owner, machine, class)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCapacity {
    pub owner: String,
    pub machine: String,
    pub class: String,
    #[serde(default = "default_auto_mode")]
    pub mode: String,
    pub useful_parallel: u32,
    #[serde(default)]
    pub accepts: BTreeSet<String>,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_since: Option<u64>,
}

fn default_auto_mode() -> String {
    "auto".to_owned()
}

/// Task record consumed by `plan()`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_fix_kind")]
    pub kind: String,
    #[serde(default = "default_normal_priority")]
    pub priority: String,
    #[serde(default = "default_ready_state")]
    pub state: String,
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub accepts: Vec<String>,
    #[serde(default)]
    pub min_tier: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_task_size")]
    pub size: f64,
    #[serde(default = "default_true")]
    pub checkpointable: bool,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub resume_note: String,
    #[serde(default)]
    pub exclude_pools_until: BTreeMap<String, u64>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub ready_at: u64,
    #[serde(default)]
    pub closed_at: u64,
    /// `low`, `normal` or `guarded` (docs/factory.md section 6).
    #[serde(default = "default_risk")]
    pub risk: String,
    /// Lowest tier that may lease the order. An order from before the field needs tier 2.
    #[serde(default = "default_min_trust")]
    pub min_trust: u8,
}

fn default_risk() -> String {
    "normal".to_owned()
}
fn default_min_trust() -> u8 {
    2
}
fn default_fix_kind() -> String {
    "fix".to_owned()
}
fn default_normal_priority() -> String {
    "normal".to_owned()
}
fn default_ready_state() -> String {
    "ready".to_owned()
}
fn default_task_size() -> f64 {
    1.0
}
fn default_attempt() -> u32 {
    1
}

/// Active or suspect lease record consumed by `plan()`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanLease {
    pub task_id: String,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    pub owner: String,
    #[serde(default)]
    pub worker: String,
    #[serde(default)]
    pub machine: String,
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub granted_at: u64,
    #[serde(default)]
    pub heartbeat_at: u64,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub est_minutes_p90: f64,
    #[serde(default)]
    pub est_cost: f64,
    #[serde(default)]
    pub est_end_at: u64,
    #[serde(default = "default_active_state")]
    pub state: String,
    #[serde(default)]
    pub supervised: bool,
}

fn default_active_state() -> String {
    "active".to_owned()
}

/// Open or historical offer record consumed by `plan()`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOffer {
    pub id: String,
    pub task_id: String,
    pub to_owner: String,
    pub class: String,
    #[serde(default)]
    pub sent_at: u64,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default = "default_open_state")]
    pub state: String,
}

fn default_open_state() -> String {
    "open".to_owned()
}

/// Canonical ordered input state for `plan(state, policy, now)`.
/// Every map is a `BTreeMap` and every set is a `BTreeSet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanState {
    #[serde(default)]
    pub members: BTreeMap<String, PlanMember>,
    #[serde(default)]
    pub capacities: BTreeMap<(String, String, String), PlanCapacity>,
    #[serde(default)]
    pub tasks: BTreeMap<String, PlanTask>,
    #[serde(default)]
    pub leases: BTreeMap<(String, u32), PlanLease>,
    #[serde(default)]
    pub offers: BTreeMap<String, PlanOffer>,
    #[serde(default)]
    pub meters: BTreeMap<(String, String), Meter>,
    #[serde(default)]
    pub estimates: BTreeMap<(String, String), Estimate>,
    #[serde(default)]
    pub pauses: BTreeSet<PauseScope>,
    #[serde(default)]
    pub active_locks: BTreeSet<String>,
    #[serde(default)]
    pub clean_room_readers: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub hub_started_at: u64,
    #[serde(default)]
    pub safe_mode: SafeModeState,
    /// Tier 0 members get coding orders only while this switch is on (docs/factory.md 5.1). The
    /// policy fills it in a later wave. Off by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tier0_coding: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Unordered list form used by JSON golden fixtures and shuffled-input determinism tests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanStateInput {
    #[serde(default)]
    pub members: Vec<PlanMember>,
    #[serde(default)]
    pub capacities: Vec<PlanCapacity>,
    #[serde(default)]
    pub tasks: Vec<PlanTask>,
    #[serde(default)]
    pub leases: Vec<PlanLease>,
    #[serde(default)]
    pub offers: Vec<PlanOffer>,
    #[serde(default)]
    pub meters: Vec<Meter>,
    #[serde(default)]
    pub estimates: Vec<Estimate>,
    #[serde(default)]
    pub pauses: Vec<PauseScope>,
    #[serde(default)]
    pub active_locks: Vec<String>,
    #[serde(default)]
    pub clean_room_readers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub hub_started_at: u64,
    #[serde(default)]
    pub safe_mode: SafeModeState,
    #[serde(default)]
    pub tier0_coding: bool,
}

impl PlanState {
    /// Computes a deterministic SHA-256 hex digest of this canonical `PlanState` snapshot,
    /// excluding append-only `DECISION` history so per-tick hashing never scans historical
    /// `DECISION` rows.
    pub fn state_hash(&self) -> String {
        let mut val = serde_json::to_value(self).unwrap_or(Value::Null);
        val.sort_all_objects();
        format!("{:x}", Sha256::digest(val.to_string().as_bytes()))
    }

    /// Normalizes arbitrary list order into canonical `BTreeMap`/`BTreeSet` structures so
    /// `plan()` produces byte-equal output regardless of input ordering.
    pub fn from_input(input: PlanStateInput) -> Self {
        let mut st = Self {
            hub_started_at: input.hub_started_at,
            safe_mode: input.safe_mode,
            tier0_coding: input.tier0_coding,
            ..Self::default()
        };
        for m in input.members {
            st.members.insert(m.name.clone(), m);
        }
        for mut c in input.capacities {
            c.pools.sort();
            c.pools.dedup();
            st.capacities
                .insert((c.owner.clone(), c.machine.clone(), c.class.clone()), c);
        }
        for mut t in input.tasks {
            t.accepts.sort();
            t.accepts.dedup();
            t.deps.sort();
            t.deps.dedup();
            st.tasks.insert(t.id.clone(), t);
        }
        for mut l in input.leases {
            l.pools.sort();
            l.pools.dedup();
            st.leases.insert((l.task_id.clone(), l.attempt), l);
        }
        for o in input.offers {
            st.offers.insert(o.id.clone(), o);
        }
        for mut m in input.meters {
            m.series.sort_by(|a, b| {
                a.observed_at
                    .cmp(&b.observed_at)
                    .then_with(|| a.used_pct.total_cmp(&b.used_pct))
            });
            st.meters.insert((m.owner.clone(), m.pool.clone()), m);
        }
        for e in input.estimates {
            st.estimates.insert((e.kind.clone(), e.class.clone()), e);
        }
        for p in input.pauses {
            st.pauses.insert(p);
        }
        for lk in input.active_locks {
            st.active_locks.insert(lk);
        }
        for (owner, readers) in input.clean_room_readers {
            let set: BTreeSet<String> = readers.into_iter().collect();
            st.clean_room_readers.insert(owner, set);
        }
        st
    }
}

/// Deterministic action emitted by `plan(state, policy, now)` (section 4.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanAction {
    ExpireOffer {
        offer_id: String,
        task_id: String,
        to_owner: String,
        class: String,
    },
    SuspectLease {
        task_id: String,
        attempt: u32,
        owner: String,
        worker: String,
        silent_secs: u64,
    },
    ReleaseLease {
        task_id: String,
        attempt: u32,
        owner: String,
        worker: String,
        reason: String,
    },
    RequeueTask {
        task_id: String,
        next_attempt: u32,
        reason: String,
    },
    Lease {
        task_id: String,
        attempt: u32,
        owner: String,
        machine: String,
        class: String,
        slot_key: String,
        pools: Vec<String>,
        est_minutes_p90: f64,
        est_cost: f64,
        est_end_at: u64,
        expires_at: u64,
    },
    Offer {
        offer_id: String,
        task_id: String,
        to_owner: String,
        machine: String,
        class: String,
        slot_key: String,
        kind: String,
        scope: String,
        est_minutes_p90: f64,
        expires_at: u64,
    },
    Alert {
        code: String,
        key: String,
        severity: String,
        to: String,
        text: String,
        data: Value,
    },
}

/// Full output of one pure `plan()` invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanOutput {
    pub effective_mode: PlanMode,
    pub actions: Vec<PlanAction>,
    pub wait_reasons: BTreeMap<String, String>,
    pub explanations: BTreeMap<String, TaskExplanation>,
    pub invariant_violations: Vec<InvariantViolation>,
    pub safe_mode: SafeModeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_mode_alert: Option<PlanAction>,
}

/// Row stored in the `DECISION` table (section 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionRow {
    pub tick_id: u64,
    pub at: u64,
    pub policy_hash: String,
    pub state_hash: String,
    pub mode: String,
    pub actions: Vec<PlanAction>,
    pub applied: bool,
    #[serde(default)]
    pub msg_id: u64,
}

/// Row stored in the `SCHEDULER_PAUSE` table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRow {
    pub scope: String,
    pub paused: bool,
    pub by: String,
    pub at: u64,
    #[serde(default)]
    pub msg_id: u64,
}

pub(crate) fn has_required_fields(task: &PlanTask) -> bool {
    !task.id.trim().is_empty()
        && !task.title.trim().is_empty()
        && !task.kind.trim().is_empty()
        && !task.scope.trim().is_empty()
        && !task.done_when.trim().is_empty()
}

pub(crate) fn deps_closed(state: &PlanState, task: &PlanTask) -> bool {
    task.deps
        .iter()
        .all(|dep| state.tasks.get(dep).is_some_and(|t| t.state == "closed"))
}

/// Pure deterministic scheduler algorithm of section 4.1 with admission 7.1, drain 7.2,
/// invariants guard, and safe mode evaluation.
pub fn plan(state: &PlanState, policy: &PlanPolicy, now: u64) -> PlanOutput {
    let base_mode = policy.effective_scheduler_mode();
    if base_mode == PlanMode::Off {
        return PlanOutput {
            effective_mode: PlanMode::Off,
            actions: Vec::new(),
            wait_reasons: BTreeMap::new(),
            explanations: BTreeMap::new(),
            invariant_violations: Vec::new(),
            safe_mode: state.safe_mode.clone(),
            safe_mode_alert: None,
        };
    }

    let mut actions: Vec<PlanAction> = Vec::new();
    let mut expired_offer_tasks: BTreeSet<String> = BTreeSet::new();
    let mut released_leases: BTreeSet<(String, u32)> = BTreeSet::new();

    // 1. Expire open offers with now >= expires_at (section 4.1).
    for o in state.offers.values() {
        if o.state == "open" && o.expires_at > 0 && now >= o.expires_at {
            actions.push(PlanAction::ExpireOffer {
                offer_id: o.id.clone(),
                task_id: o.task_id.clone(),
                to_owner: o.to_owner.clone(),
                class: o.class.clone(),
            });
            expired_offer_tasks.insert(o.task_id.clone());
        }
    }

    // 2. Lease heartbeat and death check (section 4.2).
    for l in state.leases.values() {
        if !matches!(l.state.as_str(), "active" | "suspect") {
            continue;
        }
        let silent = now.saturating_sub(l.heartbeat_at);
        let limit = if l.supervised {
            policy.timers.dead_supervised_sec
        } else {
            policy.timers.lease_min.saturating_mul(60)
        };
        if silent > limit
            && now
                > state
                    .hub_started_at
                    .saturating_add(policy.timers.restart_grace_sec)
        {
            released_leases.insert((l.task_id.clone(), l.attempt));
            actions.push(PlanAction::ReleaseLease {
                task_id: l.task_id.clone(),
                attempt: l.attempt,
                owner: l.owner.clone(),
                worker: l.worker.clone(),
                reason: "gone".to_owned(),
            });
            let next_attempt = state
                .tasks
                .get(&l.task_id)
                .map_or(l.attempt + 1, |t| t.attempt.saturating_add(1));
            actions.push(PlanAction::RequeueTask {
                task_id: l.task_id.clone(),
                next_attempt,
                reason: "gone".to_owned(),
            });
            actions.push(PlanAction::Alert {
                code: "LEASE_LOST".to_owned(),
                key: l.task_id.clone(),
                severity: "warn".to_owned(),
                to: "coordinator".to_owned(),
                text: format!(
                    "lease lost for {} (owner {}, worker {}, silent {} s)",
                    l.task_id, l.owner, l.worker, silent
                ),
                data: json!({
                    "task_id": l.task_id,
                    "attempt": l.attempt,
                    "owner": l.owner,
                    "worker": l.worker,
                    "silent_secs": silent,
                }),
            });
        } else if silent > policy.timers.suspect_sec && l.state == "active" {
            actions.push(PlanAction::SuspectLease {
                task_id: l.task_id.clone(),
                attempt: l.attempt,
                owner: l.owner.clone(),
                worker: l.worker.clone(),
                silent_secs: silent,
            });
        }
    }

    // 3. Build working assignment context (meters, forecasts, active scopes, counts, locks).
    let mut ctx = AssignContext::from_state(state, policy, &released_leases, now);

    // Compute running track shares over active tasks.
    let active_tracks: Vec<&str> = ctx
        .active_scopes
        .keys()
        .filter_map(|tid| state.tasks.get(tid))
        .map(|t| t.track.as_str())
        .collect();
    let running_shares = compute_running_shares(&active_tracks);

    // 4. Filter ready tasks R (state ready or offer expired, deps closed, required fields present).
    let mut explanations: BTreeMap<String, TaskExplanation> = BTreeMap::new();
    let mut ready_scored: Vec<(PlanTask, ScoreBreakdown)> = Vec::new();
    for task in state.tasks.values() {
        let effectively_ready = task.state == "ready"
            || (task.state == "offered" && expired_offer_tasks.contains(&task.id));
        if !effectively_ready {
            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: false,
                    not_ready_reason: Some(format!("state is {}", task.state)),
                    score: None,
                    assigned_slot: None,
                    assigned_action: None,
                    wait_reason: None,
                    shadow_admit_denials: Vec::new(),
                    slot_evaluations: Vec::new(),
                },
            );
            continue;
        }
        if !has_required_fields(task) {
            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: false,
                    not_ready_reason: Some("missing required fields".to_owned()),
                    score: None,
                    assigned_slot: None,
                    assigned_action: None,
                    wait_reason: Some("missing required fields".to_owned()),
                    shadow_admit_denials: Vec::new(),
                    slot_evaluations: Vec::new(),
                },
            );
            continue;
        }
        if !deps_closed(state, task) {
            let unclosed: Vec<&str> = task
                .deps
                .iter()
                .filter(|d| !state.tasks.get(*d).is_some_and(|dt| dt.state == "closed"))
                .map(String::as_str)
                .collect();
            let reason = format!("waiting on deps: {}", unclosed.join(", "));
            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: false,
                    not_ready_reason: Some(reason.clone()),
                    score: None,
                    assigned_slot: None,
                    assigned_action: None,
                    wait_reason: Some(reason),
                    shadow_admit_denials: Vec::new(),
                    slot_evaluations: Vec::new(),
                },
            );
            continue;
        }
        let breakdown = score_task(task, &policy.priority, &running_shares, now);
        ready_scored.push((task.clone(), breakdown));
    }

    let ready_sorted = sort_ready_tasks(ready_scored);
    let initial_ready_count = ready_sorted.len();

    // 5. Build free slots S and track initial admissible count for safe mode.
    let mut free_slots = build_free_slots(state, policy, &ctx);
    let initial_admissible_slots = free_slots
        .iter()
        .filter(|s| is_slot_admissible(s, state, policy, &ctx, now))
        .count();

    let mut wait_reasons: BTreeMap<String, String> = BTreeMap::new();
    let mut unplaced_ready: Vec<PlanTask> = Vec::new();
    let mut leases_or_offers_made = 0usize;

    // 6. Main assignment loop (section 4.1).
    for (task, breakdown) in &ready_sorted {
        let mut slot_evals = Vec::new();
        let mut shadow_denials = Vec::new();

        // Scope overlap check against all active tasks.
        if let Some(conflict_id) = find_scope_conflict(task, &ctx) {
            let reason = format!("scope overlaps active task {conflict_id}");
            wait_reasons.insert(task.id.clone(), reason.clone());
            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: true,
                    not_ready_reason: None,
                    score: Some(breakdown.clone()),
                    assigned_slot: None,
                    assigned_action: None,
                    wait_reason: Some(reason),
                    shadow_admit_denials: Vec::new(),
                    slot_evaluations: Vec::new(),
                },
            );
            unplaced_ready.push(task.clone());
            continue;
        }

        let mut candidates: Vec<(&FreeSlot, Estimate, Option<String>)> = Vec::new();
        let mut rejections: Vec<SlotRejection> = Vec::new();

        for slot in &free_slots {
            match evaluate_slot_for_task(task, slot, state, policy, &ctx, now) {
                Ok((est, shadow_reason)) => {
                    if let Some(sr) = &shadow_reason {
                        shadow_denials.push(SlotRejection {
                            slot_key: slot.key.clone(),
                            owner: slot.owner.clone(),
                            class: slot.class.clone(),
                            rule: "admission_shadow".to_owned(),
                            reason: sr.clone(),
                        });
                    }
                    slot_evals.push(SlotEvaluation {
                        slot_key: slot.key.clone(),
                        owner: slot.owner.clone(),
                        class: slot.class.clone(),
                        admitted: true,
                        rule: None,
                        reason: None,
                        shadow_admit_reason: shadow_reason.clone(),
                    });
                    candidates.push((slot, est, shadow_reason));
                }
                Err(rej) => {
                    slot_evals.push(SlotEvaluation {
                        slot_key: rej.slot_key.clone(),
                        owner: rej.owner.clone(),
                        class: rej.class.clone(),
                        admitted: false,
                        rule: Some(rej.rule.clone()),
                        reason: Some(rej.reason.clone()),
                        shadow_admit_reason: None,
                    });
                    rejections.push(rej);
                }
            }
        }

        if let Some((chosen, est, _)) = pick_best_slot(&candidates, policy, &ctx) {
            let chosen_slot = chosen.clone();
            let class_spec = policy.classes.get(&chosen_slot.class);
            let is_lease = chosen_slot.mode == "auto";
            let action_label = if is_lease { "lease" } else { "offer" };

            if is_lease {
                let mut pools = chosen_slot.pools.clone();
                if let Some(fp) = &chosen_slot.family_pool
                    && !fp.is_empty()
                    && !pools.contains(fp)
                {
                    pools.push(fp.clone());
                }
                let est_secs = (est.minutes_p90 * 60.0).round().max(60.0) as u64;
                actions.push(PlanAction::Lease {
                    task_id: task.id.clone(),
                    attempt: task.attempt,
                    owner: chosen_slot.owner.clone(),
                    machine: chosen_slot.machine.clone(),
                    class: chosen_slot.class.clone(),
                    slot_key: chosen_slot.key.clone(),
                    pools,
                    est_minutes_p90: est.minutes_p90,
                    est_cost: est.units_p90,
                    est_end_at: now.saturating_add(est_secs),
                    expires_at: now.saturating_add(policy.timers.lease_min.saturating_mul(60)),
                });
            } else {
                actions.push(PlanAction::Offer {
                    offer_id: format!("offer-{}-{}", task.id, now),
                    task_id: task.id.clone(),
                    to_owner: chosen_slot.owner.clone(),
                    machine: chosen_slot.machine.clone(),
                    class: chosen_slot.class.clone(),
                    slot_key: chosen_slot.key.clone(),
                    kind: task.kind.clone(),
                    scope: task.scope.clone(),
                    est_minutes_p90: est.minutes_p90,
                    expires_at: now.saturating_add(policy.timers.offer_min.saturating_mul(60)),
                });
            }

            leases_or_offers_made += 1;
            ctx.record_assignment(task, &chosen_slot, &est, class_spec, is_lease);
            free_slots.retain(|s| s.key != chosen_slot.key);

            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: true,
                    not_ready_reason: None,
                    score: Some(breakdown.clone()),
                    assigned_slot: Some(chosen_slot.key),
                    assigned_action: Some(action_label.to_owned()),
                    wait_reason: None,
                    shadow_admit_denials: shadow_denials,
                    slot_evaluations: slot_evals,
                },
            );
        } else {
            let reason = select_wait_reason(&rejections);
            wait_reasons.insert(task.id.clone(), reason.clone());
            explanations.insert(
                task.id.clone(),
                TaskExplanation {
                    task_id: task.id.clone(),
                    state: task.state.clone(),
                    eligible_ready: true,
                    not_ready_reason: None,
                    score: Some(breakdown.clone()),
                    assigned_slot: None,
                    assigned_action: None,
                    wait_reason: Some(reason),
                    shadow_admit_denials: shadow_denials,
                    slot_evaluations: slot_evals,
                },
            );
            unplaced_ready.push(task.clone());
        }
    }

    // 7. Backlog low alerts per class with remaining free admissible slots (section 4.1).
    let mut classes_with_free_admissible: BTreeSet<String> = BTreeSet::new();
    for slot in &free_slots {
        if is_slot_admissible(slot, state, policy, &ctx, now) {
            classes_with_free_admissible.insert(slot.class.clone());
        }
    }
    for cls in classes_with_free_admissible {
        let cls_tier = policy.classes.get(&cls).map_or(Tier::Middle, |c| c.tier);
        let ready_for_class = unplaced_ready
            .iter()
            .filter(|t| {
                let accepts_cls =
                    t.accepts.is_empty() || t.accepts.iter().any(|c| c.eq_ignore_ascii_case(&cls));
                let req_tier = if !t.min_tier.trim().is_empty() {
                    Tier::parse(&t.min_tier).unwrap_or(Tier::Low)
                } else {
                    policy
                        .kinds
                        .get(&t.kind)
                        .and_then(|k| k.min_tier)
                        .unwrap_or(Tier::Low)
                };
                accepts_cls && cls_tier >= req_tier
            })
            .count();
        if ready_for_class < policy.backlog_min_ready {
            actions.push(PlanAction::Alert {
                code: "BACKLOG_LOW".to_owned(),
                key: cls.clone(),
                severity: "info".to_owned(),
                to: "brain".to_owned(),
                text: format!(
                    "class {cls} has free admissible slots with only {ready_for_class} ready tasks (min_ready {})",
                    policy.backlog_min_ready
                ),
                data: json!({
                    "class": cls,
                    "ready_tasks": ready_for_class,
                    "min_ready": policy.backlog_min_ready,
                }),
            });
        }
    }

    // 8. Idle or starved slot alerts for slots free longer than idle_alert_min (section 4.1).
    let idle_limit = policy.timers.idle_alert_min.saturating_mul(60);
    for slot in &free_slots {
        let Some(since) = slot.free_since else {
            continue;
        };
        let idle_secs = now.saturating_sub(since);
        if idle_secs < idle_limit {
            continue;
        }
        let matching_wait = unplaced_ready.iter().find(|t| {
            t.accepts.is_empty()
                || t.accepts
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&slot.class))
        });
        if let Some(waiting_task) = matching_wait {
            let reason = wait_reasons
                .get(&waiting_task.id)
                .cloned()
                .unwrap_or_else(|| "no admissible task".to_owned());
            actions.push(PlanAction::Alert {
                code: "STARVED".to_owned(),
                key: slot.key.clone(),
                severity: "warn".to_owned(),
                to: "coordinator".to_owned(),
                text: format!(
                    "slot {} ({}) starved for {} s: {}",
                    slot.key, slot.class, idle_secs, reason
                ),
                data: json!({
                    "slot": slot.key,
                    "owner": slot.owner,
                    "class": slot.class,
                    "idle_secs": idle_secs,
                    "task_id": waiting_task.id,
                    "reason": reason,
                }),
            });
        } else {
            actions.push(PlanAction::Alert {
                code: "IDLE".to_owned(),
                key: slot.key.clone(),
                severity: "warn".to_owned(),
                to: "coordinator".to_owned(),
                text: format!(
                    "slot {} ({}) idle for {} s with no ready tasks",
                    slot.key, slot.class, idle_secs
                ),
                data: json!({
                    "slot": slot.key,
                    "owner": slot.owner,
                    "class": slot.class,
                    "idle_secs": idle_secs,
                    "reason": "no ready tasks",
                }),
            });
        }
    }

    // 9. Invariants guard (section 4.1): on any violation, return [Alert(INVARIANT)] and nothing else.
    let (guarded_actions, violations_opt) = guard_actions(actions, state, policy, now);
    let invariant_violations = violations_opt.unwrap_or_default();
    let has_violation = !invariant_violations.is_empty();

    // 10. Safe mode evaluation (section 9).
    let (next_safe_mode, safe_mode_alert) = evaluate_safe_mode(
        &state.safe_mode,
        policy,
        has_violation,
        initial_ready_count,
        initial_admissible_slots,
        leases_or_offers_made,
        now,
    );
    let effective_mode = if next_safe_mode.active && base_mode == PlanMode::Enforce {
        PlanMode::Shadow
    } else {
        base_mode
    };

    PlanOutput {
        effective_mode,
        actions: guarded_actions,
        wait_reasons,
        explanations,
        invariant_violations,
        safe_mode: next_safe_mode,
        safe_mode_alert,
    }
}

/// Picks the most specific deterministic rejection reason across all evaluated slots.
fn select_wait_reason(rejections: &[SlotRejection]) -> String {
    if rejections.is_empty() {
        return "no free slots".to_owned();
    }
    let specificity = |rule: &str| -> u8 {
        match rule {
            "admission" => 7,
            "cap_per_owner" | "cap_per_machine" | "cap_per_family" => 6,
            "lock" => 5,
            "paused" => 4,
            "clean_room" | "trust" | "trust_tier" | "slot_accepts" => 3,
            "min_tier" | "spawn_disabled" => 2,
            _ => 1,
        }
    };
    rejections
        .iter()
        .max_by(|a, b| {
            specificity(&a.rule)
                .cmp(&specificity(&b.rule))
                .then_with(|| b.slot_key.cmp(&a.slot_key))
        })
        .map_or_else(|| "no admissible slot".to_owned(), |r| r.reason.clone())
}

/// Runs `plan()` and returns the detailed `TaskExplanation` for `task_id` (`sommelier plan --explain ID`).
pub fn plan_explain(
    state: &PlanState,
    policy: &PlanPolicy,
    task_id: &str,
    now: u64,
) -> Option<TaskExplanation> {
    let out = plan(state, policy, now);
    out.explanations.get(task_id).cloned()
}

/// Store projection module for `DECISION`, `SCHEDULER_PAUSE`, and `SCHEDULER_STATE` (section 10.2).
pub struct PlanModule;

impl StoreModule for PlanModule {
    fn name(&self) -> &'static str {
        "plan"
    }

    fn schema(&self) -> &'static str {
        PLAN_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_plan_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_plan_api(conn, now_sec, state)
    }
}

/// Applies `FACT decision/*`, `FACT pause/*`, and `FACT scheduler/safe_mode` messages into SQLite.
pub fn apply_plan_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    if msg.kind != Kind::Fact {
        return Ok(());
    }
    let ts_sec = parse_ms(&msg.ts).unwrap_or(0) / 1000;
    let msg_id = msg.id as i64;

    if let Some(tick_str) = msg.topic.strip_prefix("decision/") {
        let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
            return Ok(());
        };
        let tick_id = v["tick_id"]
            .as_u64()
            .or_else(|| tick_str.parse::<u64>().ok())
            .unwrap_or(msg.id);
        let at = v["at"].as_u64().unwrap_or(ts_sec);
        let policy_hash = v["policy_hash"].as_str().unwrap_or("");
        let state_hash = v["state_hash"].as_str().unwrap_or("");
        let mode = v["mode"].as_str().unwrap_or("shadow");
        let actions_json = v
            .get("actions")
            .map(Value::to_string)
            .unwrap_or_else(|| "[]".to_owned());
        let applied = i64::from(v["applied"].as_bool().unwrap_or(false));

        tx.execute(
            "INSERT INTO DECISION (tick_id, at, policy_hash, state_hash, mode, actions, applied, msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(tick_id) DO UPDATE SET
                at = excluded.at,
                policy_hash = excluded.policy_hash,
                state_hash = excluded.state_hash,
                mode = excluded.mode,
                actions = excluded.actions,
                applied = excluded.applied,
                msg_id = excluded.msg_id",
            params![
                tick_id as i64,
                at as i64,
                policy_hash,
                state_hash,
                mode,
                actions_json,
                applied,
                msg_id
            ],
        )
        .map_err(|e| format!("upsert DECISION failed: {e}"))?;
        return Ok(());
    }

    if let Some(scope_rest) = msg.topic.strip_prefix("pause/") {
        let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
            return Ok(());
        };
        let scope = v["scope"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(scope_rest);
        let paused = v["paused"].as_bool().unwrap_or(true);
        let by = v["by"].as_str().unwrap_or(&msg.from);
        let at = v["at"].as_u64().unwrap_or(ts_sec);
        if paused {
            tx.execute(
                "INSERT INTO SCHEDULER_PAUSE (scope, paused, by, at, msg_id)
                 VALUES (?1, 1, ?2, ?3, ?4)
                 ON CONFLICT(scope) DO UPDATE SET
                    paused = 1,
                    by = excluded.by,
                    at = excluded.at,
                    msg_id = excluded.msg_id",
                params![scope, by, at as i64, msg_id],
            )
            .map_err(|e| format!("upsert SCHEDULER_PAUSE failed: {e}"))?;
        } else {
            tx.execute(
                "DELETE FROM SCHEDULER_PAUSE WHERE scope = ?1",
                params![scope],
            )
            .map_err(|e| format!("delete SCHEDULER_PAUSE failed: {e}"))?;
        }
        return Ok(());
    }

    if msg.topic == "scheduler/safe_mode" {
        tx.execute(
            "INSERT INTO SCHEDULER_STATE (key, value, msg_id)
             VALUES ('safe_mode', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                msg_id = excluded.msg_id",
            params![msg.body, msg_id],
        )
        .map_err(|e| format!("upsert SCHEDULER_STATE failed: {e}"))?;
    }

    Ok(())
}

/// Queries the next decision `tick_id` directly via `MAX(tick_id)` without deserializing history.
pub fn next_decision_tick_id(conn: &Connection) -> Result<u64, String> {
    let max_id: Option<i64> = conn
        .query_row("SELECT MAX(tick_id) FROM DECISION", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(max_id.map_or(1, |id| id.max(0) as u64 + 1))
}

/// Returns `true` when an empty `PlanOutput` matches the most recent emitted `DECISION` row
/// in `mode`, `policy_hash`, and `state_hash`, and was emitted less than
/// [`EMPTY_DECISION_HEARTBEAT_SEC`] (300 seconds) ago.
pub fn should_suppress_empty_decision(
    conn: &Connection,
    output: &PlanOutput,
    policy_hash: &str,
    state_hash: &str,
    now_sec: u64,
) -> bool {
    if !output.actions.is_empty() || output.safe_mode_alert.is_some() {
        return false;
    }
    let last: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT at, policy_hash, state_hash, mode, actions FROM DECISION ORDER BY tick_id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((last_at_i64, last_pol, last_state, last_mode, last_actions)) = last else {
        return false;
    };
    let last_at = last_at_i64.max(0) as u64;
    let last_was_empty = last_actions.trim() == "[]";
    last_was_empty
        && last_mode == output.effective_mode.as_str()
        && last_pol == policy_hash
        && last_state == state_hash
        && now_sec >= last_at
        && now_sec.saturating_sub(last_at) < EMPTY_DECISION_HEARTBEAT_SEC
}

/// Queries recent `DECISION` rows ordered by `tick_id ASC`.
pub fn query_decisions(conn: &Connection) -> Result<Vec<DecisionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT tick_id, at, policy_hash, state_hash, mode, actions, applied, msg_id
             FROM DECISION ORDER BY tick_id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let actions_raw: String = r.get(5)?;
            Ok(DecisionRow {
                tick_id: r.get::<_, i64>(0)?.max(0) as u64,
                at: r.get::<_, i64>(1)?.max(0) as u64,
                policy_hash: r.get(2)?,
                state_hash: r.get(3)?,
                mode: r.get(4)?,
                actions: serde_json::from_str(&actions_raw).unwrap_or_default(),
                applied: r.get::<_, i64>(6)? != 0,
                msg_id: r.get::<_, i64>(7)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Queries active `SCHEDULER_PAUSE` rows ordered by `scope ASC`.
pub fn query_pauses(conn: &Connection) -> Result<Vec<PauseRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT scope, paused, by, at, msg_id
             FROM SCHEDULER_PAUSE WHERE paused != 0 ORDER BY scope ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PauseRow {
                scope: r.get(0)?,
                paused: r.get::<_, i64>(1)? != 0,
                by: r.get(2)?,
                at: r.get::<_, i64>(3)?.max(0) as u64,
                msg_id: r.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Queries current `SafeModeState` from `SCHEDULER_STATE`.
pub fn query_safe_mode(conn: &Connection) -> Result<SafeModeState, String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM SCHEDULER_STATE WHERE key = 'safe_mode'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(raw
        .and_then(|s| serde_json::from_str::<SafeModeState>(&s).ok())
        .unwrap_or_default())
}

/// Populates `"decisions"`, `"pauses"`, and `"scheduler"` in `/api/state`.
pub fn build_plan_api(
    conn: &Connection,
    now_sec: u64,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let decisions = query_decisions(conn).unwrap_or_default();
    let pauses = query_pauses(conn).unwrap_or_default();
    let safe_mode = query_safe_mode(conn).unwrap_or_default();
    let recent_start = decisions.len().saturating_sub(50);
    state.insert("decisions".into(), json!(&decisions[recent_start..]));
    state.insert("pauses".into(), json!(pauses));
    state.insert(
        "scheduler".into(),
        json!({
            "as_of": now_sec,
            "safe_mode": safe_mode,
            "decision_count": decisions.len(),
            "active_pauses": pauses.iter().map(|p| p.scope.clone()).collect::<Vec<_>>(),
        }),
    );
    Ok(())
}

/// Loads a canonical `PlanState` snapshot from the SQLite projection store.
pub fn load_plan_state_from_store(
    conn: &Connection,
    policy: &PlanPolicy,
    hub_started_at: u64,
) -> Result<PlanState, String> {
    let mut state = PlanState {
        hub_started_at,
        ..PlanState::default()
    };

    for m in query_members(conn).unwrap_or_default() {
        let tier = member_tier_from_table(conn, &m.name, &m.role);
        state.members.insert(
            m.name.clone(),
            PlanMember {
                name: m.name,
                trust: m.trust,
                online: m.online,
                tier: Some(tier),
            },
        );
    }

    for cap in query_capacities(conn).unwrap_or_default() {
        let accepts: BTreeSet<String> = cap.accepts.into_iter().collect();
        let mut pools = cap.pools;
        pools.sort();
        pools.dedup();
        let family = policy
            .classes
            .get(&cap.class)
            .and_then(|c| c.family_pool.clone());
        state.capacities.insert(
            (cap.owner.clone(), cap.machine.clone(), cap.class.clone()),
            PlanCapacity {
                owner: cap.owner,
                machine: cap.machine,
                class: cap.class,
                mode: cap.mode,
                useful_parallel: cap.useful_parallel,
                accepts,
                pools,
                family,
                free_since: if cap.declared_at > 0 {
                    Some(cap.declared_at)
                } else {
                    None
                },
            },
        );
    }

    for t in query_tasks(conn).unwrap_or_default() {
        let mut accepts: Vec<String> = t
            .accepts
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        accepts.sort();
        accepts.dedup();
        let mut deps = t.deps;
        deps.sort();
        deps.dedup();
        let size = parse_task_size(&t.size);
        let exclude_pools_until: BTreeMap<String, u64> =
            serde_json::from_value(t.exclude_pools_until).unwrap_or_default();
        state.tasks.insert(
            t.id.clone(),
            PlanTask {
                id: t.id,
                title: t.title,
                kind: t.kind,
                priority: t.priority,
                state: t.state,
                track: t.track,
                scope: t.scope,
                accepts,
                min_tier: t.min_tier,
                branch: t.branch,
                done_when: t.done_when,
                deps,
                size,
                checkpointable: t.checkpointable,
                attempt: t.attempt,
                max_attempts: t.max_attempts,
                resume_note: t.resume_note,
                exclude_pools_until,
                created_at: t.created_at,
                ready_at: t.ready_at,
                closed_at: t.closed_at,
                risk: t.risk,
                min_trust: t.min_trust,
            },
        );
    }

    for l in query_leases(conn).unwrap_or_default() {
        let mut pools = l.pools;
        pools.sort();
        pools.dedup();
        let family = policy
            .classes
            .get(&l.class)
            .and_then(|c| c.family_pool.clone());
        state.leases.insert(
            (l.task_id.clone(), l.attempt),
            PlanLease {
                task_id: l.task_id,
                attempt: l.attempt,
                owner: l.owner,
                worker: l.worker,
                machine: l.machine,
                class: l.class,
                family,
                pools,
                granted_at: l.granted_at,
                heartbeat_at: l.heartbeat_at,
                expires_at: l.expires_at,
                est_minutes_p90: l.est_minutes_p90,
                est_cost: l.est_cost,
                est_end_at: l.est_end_at,
                state: l.state,
                supervised: l.supervised,
            },
        );
    }

    for o in query_offers(conn).unwrap_or_default() {
        state.offers.insert(
            o.id.clone(),
            PlanOffer {
                id: o.id,
                task_id: o.task_id,
                to_owner: o.to_owner,
                class: o.class,
                sent_at: o.sent_at,
                expires_at: o.expires_at,
                state: o.state,
            },
        );
    }

    for mut m in load_all_meters(conn).unwrap_or_default() {
        if let Some(mp) = policy.meters.get(&m.pool) {
            m.policy = mp.clone();
        }
        state.meters.insert((m.owner.clone(), m.pool.clone()), m);
    }

    for e in query_estimates(conn).unwrap_or_default() {
        state.estimates.insert(
            (e.kind.clone(), e.class.clone()),
            Estimate {
                kind: e.kind,
                class: e.class,
                n: e.n,
                minutes_p50: e.minutes_p50,
                minutes_p90: e.minutes_p90,
                units_p50: e.units_p50,
                units_p90: e.units_p90,
                source: e.source,
            },
        );
    }

    for p in query_pauses(conn).unwrap_or_default() {
        if p.paused
            && let Ok(ps) = PauseScope::parse(&p.scope)
        {
            state.pauses.insert(ps);
        }
    }

    state.safe_mode = query_safe_mode(conn).unwrap_or_default();
    Ok(state)
}

/// The tier of a member for the scheduler: the TIER table through `tier_from_table`. A member that
/// joined with a key has no role to its name, so the KEY table tells it apart.
fn member_tier_from_table(conn: &Connection, name: &str, role: &str) -> i64 {
    let keyed = crate::key::book::current_row(conn, name)
        .map(|r| r.is_some())
        .unwrap_or(false);
    let role = if keyed {
        None
    } else {
        Some(match role {
            "orchestrator" => Role::Orchestrator,
            "hub" => Role::Hub,
            "observer" => Role::Observer,
            _ => Role::Member,
        })
    };
    crate::stamp::tier_table::tier_from_table(conn, name, role)
}

fn parse_task_size(raw: &str) -> f64 {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return 1.0;
    }
    if let Ok(n) = trimmed.parse::<f64>()
        && n.is_finite()
        && n > 0.0
    {
        return n;
    }
    match trimmed.as_str() {
        "xs" | "tiny" => 0.25,
        "s" | "small" => 0.5,
        "m" | "medium" | "normal" => 1.0,
        "l" | "large" => 2.0,
        "xl" | "huge" => 4.0,
        _ => 1.0,
    }
}

/// Converts a `PlanOutput` into loggable hub messages (`FACT decision/<tick_id>`, optional
/// `FACT scheduler/safe_mode`, and in `Enforce` mode the corresponding `OFFER`, `TASK_UPDATE`,
/// and `ALERT` messages).
pub fn output_to_messages(
    output: &PlanOutput,
    state: &PlanState,
    policy: &PlanPolicy,
    tick_id: u64,
    state_hash: &str,
    now_sec: u64,
) -> Vec<Message> {
    let mut msgs = Vec::new();
    let ts = rfc3339(now_sec.saturating_mul(1000));
    let applied =
        output.effective_mode == PlanMode::Enforce && output.invariant_violations.is_empty();

    // Always record the DECISION row in the log (sections 3 & 4).
    let decision_body = json!({
        "v": 1,
        "tick_id": tick_id,
        "at": now_sec,
        "policy_hash": policy.policy_hash(),
        "state_hash": state_hash,
        "mode": output.effective_mode.as_str(),
        "actions": output.actions,
        "applied": applied,
    })
    .to_string();
    msgs.push(Message {
        id: 0,
        ts: ts.clone(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic: format!("decision/{tick_id}"),
        body: decision_body,
        refs: vec![format!("mode:{}", output.effective_mode.as_str())],
        to: Vec::new(),
    });

    if output.safe_mode != state.safe_mode {
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: "scheduler/safe_mode".into(),
            body: serde_json::to_string(&output.safe_mode).unwrap_or_else(|_| "{}".into()),
            refs: Vec::new(),
            to: Vec::new(),
        });
    }

    if let Some(PlanAction::Alert {
        code,
        key,
        severity,
        to,
        text,
        data,
    }) = &output.safe_mode_alert
    {
        msgs.push(build_alert_msg(
            &ts, code, key, severity, to, text, data, now_sec,
        ));
    }

    // In shadow mode, emit invariant alerts only and do not apply leases/offers.
    if !applied {
        for act in &output.actions {
            if let PlanAction::Alert {
                code,
                key,
                severity,
                to,
                text,
                data,
            } = act
                && code == "INVARIANT"
            {
                msgs.push(build_alert_msg(
                    &ts, code, key, severity, to, text, data, now_sec,
                ));
            }
        }
        return msgs;
    }

    // In enforce mode, apply all planned actions as bus messages.
    for act in &output.actions {
        match act {
            PlanAction::ExpireOffer {
                offer_id,
                task_id,
                to_owner,
                class,
            } => {
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Offer,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "offer_id": offer_id,
                        "task": task_id,
                        "to_owner": to_owner,
                        "class": class,
                        "state": "expired",
                        "reason": "offer_expired",
                    })
                    .to_string(),
                    refs: vec!["offer:expired".into()],
                    to: Vec::new(),
                });
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "task": task_id,
                        "prev": "offered",
                        "state": "ready",
                        "by": "hub",
                        "reason": "offer_expired",
                    })
                    .to_string(),
                    refs: vec!["state:ready".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::SuspectLease {
                task_id,
                attempt,
                owner,
                worker,
                ..
            } => {
                let prev_state = state
                    .tasks
                    .get(task_id)
                    .map_or("running", |t| t.state.as_str());
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "task": task_id,
                        "prev": prev_state,
                        "state": prev_state,
                        "attempt": attempt,
                        "by": "hub",
                        "reason": "heartbeat_late",
                        "lease": {
                            "task_id": task_id,
                            "attempt": attempt,
                            "owner": owner,
                            "worker": worker,
                            "state": "suspect",
                        }
                    })
                    .to_string(),
                    refs: vec!["lease:suspect".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::ReleaseLease {
                task_id,
                attempt,
                owner,
                worker,
                reason,
            } => {
                let prev_state = state
                    .tasks
                    .get(task_id)
                    .map_or("running", |t| t.state.as_str());
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "task": task_id,
                        "prev": prev_state,
                        "state": prev_state,
                        "attempt": attempt,
                        "by": "hub",
                        "reason": reason,
                        "lease": {
                            "task_id": task_id,
                            "attempt": attempt,
                            "owner": owner,
                            "worker": worker,
                            "state": "released",
                            "end_reason": reason,
                        }
                    })
                    .to_string(),
                    refs: vec!["lease:released".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::RequeueTask {
                task_id,
                next_attempt,
                reason,
            } => {
                let prev_state = state
                    .tasks
                    .get(task_id)
                    .map_or("running", |t| t.state.as_str());
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "task": task_id,
                        "prev": prev_state,
                        "state": "ready",
                        "attempt": next_attempt,
                        "by": "hub",
                        "reason": reason,
                    })
                    .to_string(),
                    refs: vec!["state:ready".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::Lease {
                task_id,
                attempt,
                owner,
                machine,
                class,
                slot_key,
                pools,
                est_minutes_p90,
                est_cost,
                est_end_at,
                expires_at,
            } => {
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "task": task_id,
                        "prev": "ready",
                        "state": "leased",
                        "attempt": attempt,
                        "by": "hub",
                        "reason": "scheduled",
                        "lease": {
                            "task_id": task_id,
                            "attempt": attempt,
                            "owner": owner,
                            "worker": slot_key,
                            "machine": machine,
                            "class": class,
                            "pools": pools,
                            "granted_at": now_sec,
                            "heartbeat_at": now_sec,
                            "expires_at": expires_at,
                            "est_minutes_p90": est_minutes_p90,
                            "est_cost": est_cost,
                            "est_end_at": est_end_at,
                            "state": "active",
                            "supervised": true,
                        }
                    })
                    .to_string(),
                    refs: vec!["state:leased".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::Offer {
                offer_id,
                task_id,
                to_owner,
                class,
                kind,
                scope,
                est_minutes_p90,
                expires_at,
                ..
            } => {
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Offer,
                    topic: format!("task/{task_id}"),
                    body: json!({
                        "v": 1,
                        "offer_id": offer_id,
                        "task": task_id,
                        "to_owner": to_owner,
                        "class": class,
                        "kind": kind,
                        "scope": scope,
                        "est_minutes": est_minutes_p90,
                        "sent_at": now_sec,
                        "expires": expires_at,
                        "state": "open",
                    })
                    .to_string(),
                    refs: vec!["offer:open".into()],
                    to: Vec::new(),
                });
            }
            PlanAction::Alert {
                code,
                key,
                severity,
                to,
                text,
                data,
            } => {
                msgs.push(build_alert_msg(
                    &ts, code, key, severity, to, text, data, now_sec,
                ));
            }
        }
    }

    msgs
}

#[allow(clippy::too_many_arguments)]
fn build_alert_msg(
    ts: &str,
    code: &str,
    key: &str,
    severity: &str,
    to: &str,
    text: &str,
    data: &Value,
    now_sec: u64,
) -> Message {
    Message {
        id: 0,
        ts: ts.to_owned(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: format!("alert/{code}/{key}"),
        body: json!({
            "v": 1,
            "code": code,
            "key": key,
            "severity": severity,
            "to": to,
            "state": "open",
            "opened_at": now_sec,
            "last_at": now_sec,
            "count": 1,
            "text": text,
            "data": data,
        })
        .to_string(),
        refs: vec!["state:open".into()],
        to: Vec::new(),
    }
}

/// Loads `PlanPolicy` from the hub's policy store or defaults to `mode = off` if not configured.
fn load_hub_plan_policy(hub: &Hub) -> PlanPolicy {
    let pol_store = crate::policy::Store::new(&hub.config().data_dir);
    if let Ok(versions) = pol_store.versions_after(None)
        && let Some(&latest) = versions.last()
    {
        let archive_path = hub
            .config()
            .data_dir
            .join("policy")
            .join("versions")
            .join(format!("{latest}.toml"));
        if let Ok(src) = std::fs::read_to_string(&archive_path)
            && let Ok(parsed) = PlanPolicy::parse_toml(&src)
        {
            return parsed;
        }
    }
    PlanPolicy {
        mode: PlanMode::Off,
        ..PlanPolicy::default()
    }
}

/// Periodic hub tick hook wired into `src/hooks.rs`: runs `plan()` when scheduler is enabled.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let policy = load_hub_plan_policy(hub);
    if policy.effective_scheduler_mode() == PlanMode::Off {
        return Vec::new();
    }
    let now_sec = now_ms / 1000;
    let started = *PLAN_HUB_START_SEC.get_or_init(|| now_sec);
    hub.with_store(|store| {
        let Ok(state) = load_plan_state_from_store(store.connection(), &policy, started) else {
            return Vec::new();
        };
        let state_hash = state.state_hash();
        let policy_hash = policy.policy_hash();
        let next_tick = next_decision_tick_id(store.connection()).unwrap_or(1);
        let out = plan(&state, &policy, now_sec);
        if out.safe_mode == state.safe_mode
            && should_suppress_empty_decision(
                store.connection(),
                &out,
                &policy_hash,
                &state_hash,
                now_sec,
            )
        {
            return Vec::new();
        }
        output_to_messages(&out, &state, &policy, next_tick, &state_hash, now_sec)
    })
    .unwrap_or_default()
}

/// Module ops wired into `src/hooks.rs`: `plan.dry`, `plan.explain`, `plan.pause`, `plan.resume`,
/// `plan.decisions`, `pause`, and `resume`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let now_sec = body["now"].as_u64().unwrap_or(ctx.now_ms / 1000);
    let ts = rfc3339(now_sec.saturating_mul(1000));
    let mut out = Map::new();

    match op {
        "plan.dry" | "plan.run" => {
            let policy = parse_policy_from_body(body)?;
            let started = *PLAN_HUB_START_SEC.get_or_init(|| now_sec);
            let state = load_plan_state_from_store(ctx.store.connection(), &policy, started)?;
            let plan_out = plan(&state, &policy, now_sec);
            out.insert("policy_hash".into(), json!(policy.policy_hash()));
            out.insert(
                "effective_mode".into(),
                json!(plan_out.effective_mode.as_str()),
            );
            out.insert("actions".into(), json!(plan_out.actions));
            out.insert("wait_reasons".into(), json!(plan_out.wait_reasons));
            out.insert("explanations".into(), json!(plan_out.explanations));
            out.insert("safe_mode".into(), json!(plan_out.safe_mode));
            Ok(out)
        }
        "plan.explain" => {
            let task_id = body["task"]
                .as_str()
                .or_else(|| body["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "plan.explain requires `task` or `id`".to_owned())?;
            let policy = parse_policy_from_body(body)?;
            let started = *PLAN_HUB_START_SEC.get_or_init(|| now_sec);
            let state = load_plan_state_from_store(ctx.store.connection(), &policy, started)?;
            let explanation = plan_explain(&state, &policy, task_id, now_sec)
                .ok_or_else(|| format!("task `{task_id}` not found"))?;
            out.insert("task".into(), json!(task_id));
            out.insert("explanation".into(), json!(explanation));
            Ok(out)
        }
        "plan.pause" | "pause" => {
            if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
                return Err("only an orchestrator may pause the scheduler".to_owned());
            }
            let scope_raw = body["scope"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "pause requires `scope` (all | class <c> | pool <p> | owner <o>)".to_owned()
                })?;
            let parsed = PauseScope::parse(scope_raw)?;
            let scope_key = parsed.as_key();
            let msg = Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Fact,
                topic: format!("pause/{scope_key}"),
                body: json!({
                    "v": 1,
                    "scope": scope_key,
                    "paused": true,
                    "by": ctx.caller.name,
                    "at": now_sec,
                })
                .to_string(),
                refs: vec!["pause:on".into()],
                to: Vec::new(),
            };
            let msg_id = ctx.emit(msg)?;
            out.insert("scope".into(), json!(scope_key));
            out.insert("paused".into(), json!(true));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "plan.resume" | "resume" => {
            if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
                return Err("only an orchestrator may resume the scheduler".to_owned());
            }
            let scope_raw = body["scope"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "resume requires `scope` (all | class <c> | pool <p> | owner <o>)".to_owned()
                })?;
            let parsed = PauseScope::parse(scope_raw)?;
            let scope_key = parsed.as_key();
            let mut msgs = vec![Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Fact,
                topic: format!("pause/{scope_key}"),
                body: json!({
                    "v": 1,
                    "scope": scope_key,
                    "paused": false,
                    "by": ctx.caller.name,
                    "at": now_sec,
                })
                .to_string(),
                refs: vec!["pause:off".into()],
                to: Vec::new(),
            }];
            if parsed == PauseScope::All {
                let clear_safe = SafeModeState::default();
                msgs.push(Message {
                    id: 0,
                    ts,
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Fact,
                    topic: "scheduler/safe_mode".into(),
                    body: serde_json::to_string(&clear_safe).unwrap_or_else(|_| "{}".into()),
                    refs: vec!["safe_mode:off".into()],
                    to: Vec::new(),
                });
            }
            let ids = ctx.emit_all(msgs)?;
            out.insert("scope".into(), json!(scope_key));
            out.insert("paused".into(), json!(false));
            out.insert("msg_ids".into(), json!(ids));
            Ok(out)
        }
        "plan.decisions" => {
            let decisions = query_decisions(ctx.store.connection())?;
            out.insert("decisions".into(), json!(decisions));
            Ok(out)
        }
        other => Err(format!("unknown plan op `{other}`")),
    }
}

fn parse_policy_from_body(body: &Value) -> Result<PlanPolicy, String> {
    if let Some(toml_str) = body["policy_toml"].as_str() {
        return PlanPolicy::parse_toml(toml_str);
    }
    if let Some(pol_val) = body.get("policy").filter(|v| v.is_object()) {
        return serde_json::from_value(pol_val.clone())
            .map_err(|e| format!("invalid policy payload: {e}"));
    }
    Ok(PlanPolicy::default())
}
