//! Test reports of the test network: the REPORT table, quorum and the matrix.
//!
//! Part of the public factory, see `docs/factory.md` section 9. The module owns the REPORT table,
//! its projection from `FACT` messages with topic `report/<run>` and the `matrix` part of
//! `/api/state`. The pure parts are in the submodules.

pub mod ops;
pub mod orders;
pub mod privacy;
pub mod quorum;
pub mod side;
pub mod types;
pub mod validate;

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

pub use privacy::{Hit, HitKind, find_private, scrub};
pub use quorum::{
    Cell, CellState, QuorumRules, Regression, ReportView, Verdict, cell, classify, counted_members,
    machine_key, regressions,
};
pub use validate::{MAX_LOG_BYTES, MAX_REPORT_BYTES, ReportLimits, validate_report};

use types::{Outcome, Report};

/// DDL of the REPORT table. No address block is stored here: it lives in the private side store of
/// [`side`], never in this table, never in the log.
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
             ON CONFLICT(run, member) DO NOTHING",
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
        let blocks = side::blocks_of(conn);
        state.insert(
            "matrix".into(),
            Value::Array(matrix(&rows, &blocks, &QuorumRules::default())),
        );
        state.insert("matrix_blocks".into(), blocks_note(conn, &blocks));
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

/// What the public state says about address blocks: whether the front door keeps any, and what
/// follows when it does not. No block and no member is named.
pub fn blocks_note(conn: &Connection, blocks: &BTreeMap<(String, String), String>) -> Value {
    let kept =
        !blocks.is_empty() || side::data_dir_of(conn).is_some_and(|d| side::door_keeps_blocks(&d));
    json!({
        "kept": kept,
        "text": if kept {
            "A report counts for a cell only when the hub knew an address block of its sender."
        } else {
            "The front door keeps no address blocks, so every report is a hint and no cell settles."
        },
    })
}

/// Lines for `sommelier factory status`: how many reports hold a block, and a warning when the
/// front door keeps none.
pub fn status_lines(conn: &Connection) -> Vec<String> {
    let blocks = side::blocks_of(conn);
    let total = query_reports(conn).map(|r| r.len()).unwrap_or(0);
    let with = query_reports(conn)
        .map(|rows| {
            rows.iter()
                .filter(|r| blocks.contains_key(&(r.run.clone(), r.member.clone())))
                .count()
        })
        .unwrap_or(0);
    let mut out = vec![format!("reports with a block: {with} of {total}")];
    if blocks_note(conn, &blocks)["kept"] == json!(false) {
        out.push(
            "the front door keeps no address blocks: every report is a hint and no cell settles"
                .into(),
        );
    }
    out
}

/// The matrix: one row per profile, machine key and build. `blocks` are the blocks of the side
/// store by (run, member). A report without one is a hint: it never settles a cell. Each row says
/// how many of its reports had a block.
pub fn matrix(
    rows: &[ReportRow],
    blocks: &BTreeMap<(String, String), String>,
    rules: &QuorumRules,
) -> Vec<Value> {
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
                block: blocks.get(&(r.run.clone(), r.member.clone())).cloned(),
                outcome,
                at: r.at,
            });
    }
    groups
        .into_iter()
        .map(|((profile, key, build), views)| {
            let c = cell(&views, rules);
            let with_block = views.iter().filter(|v| v.block.is_some()).count();
            json!({
                "profile": profile,
                "machine_key": key,
                "build": build,
                "state": c.state.as_str(),
                "works": c.works,
                "fails": c.fails,
                "need_more": c.need_more,
                "quorum": rules.quorum,
                "reports": views.len(),
                "with_block": with_block,
                "counted_by": "block",
            })
        })
        .collect()
}
