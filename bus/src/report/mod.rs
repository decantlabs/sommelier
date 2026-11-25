//! Test reports of the test network: the REPORT table, quorum and the matrix.
//!
//! Part of the public factory, see `docs/factory.md` section 9. The module owns the REPORT table,
//! its projection from `FACT` messages with topic `report/<run>` and the `matrix` part of
//! `/api/state`. The pure parts are in the submodules.

pub mod ops;
pub mod orders;
pub mod privacy;
pub mod quorum;
pub mod types;
pub mod validate;

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

pub use privacy::{Hit, HitKind, find_private, scrub};
pub use quorum::{
    Cell, CellState, QuorumRules, Regression, ReportView, Verdict, cell, classify, machine_key,
    regressions,
};
pub use validate::{ReportLimits, validate_report};

use types::{Outcome, Report};

/// DDL of the REPORT table. No address block is stored.
pub const REPORT_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS REPORT (
    run TEXT NOT NULL,
    member TEXT NOT NULL,
    build TEXT NOT NULL,
    channel TEXT NOT NULL,
    profile TEXT NOT NULL,
    scenario TEXT NOT NULL,
    machine_key TEXT NOT NULL,
    outcome TEXT NOT NULL,
    first_error TEXT,
    metrics TEXT NOT NULL,
    at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run, member)
);
";

/// One row of the REPORT table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRow {
    pub run: String,
    pub member: String,
    pub build: String,
    pub channel: String,
    pub profile: String,
    pub scenario: String,
    pub machine_key: String,
    pub outcome: String,
    pub first_error: Option<String>,
    /// The metrics as one line of JSON, names in order.
    pub metrics: String,
    pub at: u64,
    pub msg_id: u64,
}

fn outcome_name(o: Outcome) -> &'static str {
    match o {
        Outcome::Works => "works",
        Outcome::Fails => "fails",
        Outcome::Inconclusive => "inconclusive",
    }
}

fn outcome_from_name(s: &str) -> Option<Outcome> {
    match s {
        "works" => Some(Outcome::Works),
        "fails" => Some(Outcome::Fails),
        "inconclusive" => Some(Outcome::Inconclusive),
        _ => None,
    }
}

/// Store projection module of this folder.
pub struct ReportModule;

impl ReportModule {
    /// The report of a `FACT` message, or `None` when the message is not a valid report.
    pub fn parse_message(msg: &Message, limits: &ReportLimits) -> Option<Report> {
        if msg.kind != Kind::Fact {
            return None;
        }
        let run = msg.topic.strip_prefix("report/")?;
        let report: Report = serde_json::from_str(msg.body.trim()).ok()?;
        if report.run != run || validate_report(&report, limits).is_err() {
            return None;
        }
        Some(report)
    }
}

impl StoreModule for ReportModule {
    fn name(&self) -> &'static str {
        "report"
    }

    fn schema(&self) -> &'static str {
        // The REPORT table and the RUN_ORDER table of the test network share this module.
        static ALL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        ALL.get_or_init(|| format!("{REPORT_SCHEMA_SQL}{}", orders::ORDER_SCHEMA_SQL))
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        // Only a FACT written by the hub counts for `report/` and `runorder/`.
        if msg.role != Role::Hub || msg.from != "hub" {
            return Ok(());
        }
        orders::apply(tx, msg)?;
        let Some(r) = Self::parse_message(msg, &ReportLimits::default()) else {
            return Ok(());
        };
        let metrics = serde_json::to_string(&r.metrics).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO REPORT (run, member, build, channel, profile, scenario, machine_key,
                                 outcome, first_error, metrics, at, msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(run, member) DO UPDATE SET
                build = excluded.build, channel = excluded.channel, profile = excluded.profile,
                scenario = excluded.scenario, machine_key = excluded.machine_key,
                outcome = excluded.outcome, first_error = excluded.first_error,
                metrics = excluded.metrics, at = excluded.at, msg_id = excluded.msg_id",
            params![
                r.run,
                r.member,
                r.build,
                r.channel,
                r.profile,
                r.scenario,
                machine_key(&r.machine),
                outcome_name(r.outcome),
                r.first_error,
                metrics,
                r.at as i64,
                msg.id as i64,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let rows = query_reports(conn)?;
        state.insert(
            "matrix".into(),
            Value::Array(matrix(&rows, &QuorumRules::default())),
        );
        // The open run orders. Only counts, never the members who reported.
        let counts = orders::report_counts(conn)?;
        let open: Vec<Value> = orders::query_orders(conn)?
            .into_iter()
            .filter(|o| o.state == "open")
            .map(|o| {
                let reports = counts
                    .iter()
                    .find(|(r, _)| r == &o.id)
                    .map(|(_, n)| *n)
                    .unwrap_or(0);
                json!({
                    "id": o.id,
                    "build": o.build,
                    "channel": o.channel,
                    "profile": o.profile,
                    "scenario": o.scenario,
                    "needs": o.needs.to_json(),
                    "wanted": o.wanted,
                    "reports": reports,
                    "created_at": o.created_at,
                })
            })
            .collect();
        state.insert("run_orders".into(), Value::Array(open));
        Ok(())
    }
}

/// All rows of the REPORT table in key order.
pub fn query_reports(conn: &Connection) -> Result<Vec<ReportRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT run, member, build, channel, profile, scenario, machine_key, outcome,
                    first_error, metrics, at, msg_id
             FROM REPORT ORDER BY run ASC, member ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ReportRow {
                run: r.get(0)?,
                member: r.get(1)?,
                build: r.get(2)?,
                channel: r.get(3)?,
                profile: r.get(4)?,
                scenario: r.get(5)?,
                machine_key: r.get(6)?,
                outcome: r.get(7)?,
                first_error: r.get(8)?,
                metrics: r.get(9)?,
                at: r.get::<_, i64>(10)?.max(0) as u64,
                msg_id: r.get::<_, i64>(11)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// The matrix: one row per profile, machine key and build. The table keeps no address block, so
/// the counts here are by member only. Each row says so in `counted_by`.
pub fn matrix(rows: &[ReportRow], rules: &QuorumRules) -> Vec<Value> {
    let mut groups: BTreeMap<(&str, &str, &str), Vec<ReportView>> = BTreeMap::new();
    for r in rows {
        let Some(outcome) = outcome_from_name(&r.outcome) else {
            continue;
        };
        groups
            .entry((r.profile.as_str(), r.machine_key.as_str(), r.build.as_str()))
            .or_default()
            .push(ReportView {
                member: r.member.clone(),
                tier: 0,
                block: None,
                outcome,
                at: r.at,
            });
    }
    groups
        .into_iter()
        .map(|((profile, key, build), views)| {
            let c = cell(&views, rules);
            json!({
                "profile": profile,
                "machine_key": key,
                "build": build,
                "state": c.state.as_str(),
                "works": c.works,
                "fails": c.fails,
                "need_more": c.need_more,
                "quorum": rules.quorum,
                "counted_by": "member",
            })
        })
        .collect()
}
