//! Stamps and tiers: evidence bound reputation rows and the pure tier function.
//!
//! Part of the public factory, see `docs/factory.md` section 5. A stamp is one row of the STAMP
//! table, projected only from FACT messages with topic `stamp/<member>`. Reputation is the list of
//! stamps and [`tier`] is a pure function of that list.

pub mod ops;
mod tier;
pub mod tier_table;

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

pub use tier::{KeyState, MemberFacts, TierDecision, TierRules, tier};

/// SQL for the STAMP table.
pub const STAMP_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS STAMP (
    id INTEGER PRIMARY KEY,
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    evidence_msg INTEGER NOT NULL,
    evidence_commit TEXT NOT NULL DEFAULT '',
    evidence_ref TEXT NOT NULL DEFAULT '',
    issuer TEXT NOT NULL,
    at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS STAMP_SUBJECT ON STAMP (subject);
";

/// The kinds of stamp (section 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StampKind {
    Landed,
    Reverted,
    ReviewConfirmed,
    ReviewMissed,
    RunAgreed,
    RunOutlier,
    Abandoned,
    Superseded,
    GrantTier2,
    Cleared,
    Intent,
}

impl StampKind {
    /// Every kind, in a fixed order.
    pub const ALL: [StampKind; 11] = [
        StampKind::Landed,
        StampKind::Reverted,
        StampKind::ReviewConfirmed,
        StampKind::ReviewMissed,
        StampKind::RunAgreed,
        StampKind::RunOutlier,
        StampKind::Abandoned,
        StampKind::Superseded,
        StampKind::GrantTier2,
        StampKind::Cleared,
        StampKind::Intent,
    ];

    /// The name used in the log and in the table.
    pub fn as_str(self) -> &'static str {
        match self {
            StampKind::Landed => "landed",
            StampKind::Reverted => "reverted",
            StampKind::ReviewConfirmed => "review_confirmed",
            StampKind::ReviewMissed => "review_missed",
            StampKind::RunAgreed => "run_agreed",
            StampKind::RunOutlier => "run_outlier",
            StampKind::Abandoned => "abandoned",
            StampKind::Superseded => "superseded",
            StampKind::GrantTier2 => "grant_tier2",
            StampKind::Cleared => "cleared",
            StampKind::Intent => "intent",
        }
    }

    /// Parses the name of a kind. Exact match only.
    pub fn parse(s: &str) -> Option<StampKind> {
        StampKind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Kinds that only a maintainer may issue.
    pub fn needs_maintainer(self) -> bool {
        matches!(
            self,
            StampKind::GrantTier2 | StampKind::Cleared | StampKind::Intent
        )
    }
}

/// One stamp. `id` and `msg_id` are 0 until the projection sets them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stamp {
    pub id: u64,
    pub subject: String,
    pub kind: StampKind,
    /// Message id of the evidence. Never 0 for a valid stamp.
    pub evidence_msg: u64,
    pub evidence_commit: String,
    pub evidence_ref: String,
    pub issuer: String,
    /// Seconds since the epoch.
    pub at: u64,
    /// Id of the FACT message that carried the stamp. A `cleared` stamp names this id as evidence.
    pub msg_id: u64,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Checks the body of a stamp line and returns the stamp.
///
/// The error is one line that names the reason. `issuer_is_maintainer` says whether the issuer is
/// a maintainer, which `grant_tier2`, `cleared` and `intent` need.
pub fn validate_stamp(body: &str, issuer_is_maintainer: bool) -> Result<Stamp, String> {
    let v: Value =
        serde_json::from_str(body.trim()).map_err(|e| format!("stamp body is not JSON: {e}"))?;
    if !v.is_object() {
        return Err("stamp body must be a JSON object".to_owned());
    }
    if v.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("stamp needs `v` set to 1".to_owned());
    }
    let subject = text(&v, "subject");
    if subject.is_empty() {
        return Err("stamp needs a `subject`".to_owned());
    }
    let issuer = text(&v, "issuer");
    if issuer.is_empty() {
        return Err("stamp needs an `issuer`".to_owned());
    }
    let kind_s = text(&v, "kind");
    let kind = StampKind::parse(&kind_s).ok_or_else(|| format!("unknown stamp kind `{kind_s}`"))?;
    let evidence = v
        .get("evidence")
        .filter(|e| e.is_object())
        .ok_or_else(|| "stamp needs an `evidence` object with a message id".to_owned())?;
    let evidence_msg = evidence
        .get("msg")
        .and_then(Value::as_u64)
        .filter(|m| *m > 0)
        .ok_or_else(|| "stamp needs an evidence message id (`evidence.msg`)".to_owned())?;
    let at = v
        .get("at")
        .and_then(Value::as_u64)
        .ok_or_else(|| "stamp needs `at` as whole seconds".to_owned())?;
    if issuer == subject {
        return Err(
            "issuer and subject are the same member, nobody stamps their own work".to_owned(),
        );
    }
    if kind.needs_maintainer() && !issuer_is_maintainer {
        return Err(format!(
            "stamp kind `{}` needs a maintainer as issuer",
            kind.as_str()
        ));
    }
    Ok(Stamp {
        id: 0,
        subject,
        kind,
        evidence_msg,
        evidence_commit: text(evidence, "commit"),
        evidence_ref: text(evidence, "ref"),
        issuer,
        at,
        msg_id: 0,
    })
}

/// Store projection module for the STAMP table.
pub struct StampModule;

impl StoreModule for StampModule {
    fn name(&self) -> &'static str {
        "stamp"
    }

    fn schema(&self) -> &'static str {
        STAMP_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_stamp_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        // A hub with standing on replaces these rows with `ops::api_rows`, see `store::api`.
        let rows: Vec<Value> = standing(conn)?
            .into_iter()
            .map(|r| json!({ "member": r.member, "counts": r.counts }))
            .collect();
        state.insert("standing".to_owned(), Value::Array(rows));
        Ok(())
    }
}

fn sender_is_maintainer(tx: &Transaction<'_>, msg: &Message) -> bool {
    if matches!(msg.role, Role::Orchestrator | Role::Hub) {
        return true;
    }
    let trust: Option<String> = tx
        .query_row(
            "SELECT trust FROM MEMBER WHERE name = ?1",
            params![msg.from],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    matches!(trust.as_deref(), Some("owner" | "trusted"))
}

/// Projects one message. A bad stamp line leaves no row and is not an error, so a bad line in an
/// old log cannot give standing and cannot stop a replay.
pub fn apply_stamp_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    if msg.kind != Kind::Fact {
        return Ok(());
    }
    let Some(topic_member) = msg.topic.strip_prefix("stamp/") else {
        return Ok(());
    };
    // Only the hub writes stamps. A line of a member in an old log gives no stamp.
    if msg.role != Role::Hub {
        return Ok(());
    }
    let maintainer = sender_is_maintainer(tx, msg);
    let Ok(stamp) = validate_stamp(&msg.body, maintainer) else {
        return Ok(());
    };
    if stamp.subject != topic_member {
        return Ok(());
    }
    // The issuer field must be the sender, except for the hub and orchestrator, which issue by rule.
    let by_rule = matches!(msg.role, Role::Orchestrator | Role::Hub);
    if !by_rule && stamp.issuer != msg.from {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO STAMP
            (id, subject, kind, evidence_msg, evidence_commit, evidence_ref, issuer, at, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?1)",
        params![
            msg.id as i64,
            stamp.subject,
            stamp.kind.as_str(),
            stamp.evidence_msg as i64,
            stamp.evidence_commit,
            stamp.evidence_ref,
            stamp.issuer,
            stamp.at as i64,
        ],
    )
    .map_err(|e| format!("insert STAMP error: {e}"))?;
    Ok(())
}

/// Every stamp, oldest message first.
pub fn query_stamps(conn: &Connection) -> Result<Vec<Stamp>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, subject, kind, evidence_msg, evidence_commit, evidence_ref, issuer, at, msg_id
             FROM STAMP ORDER BY id ASC",
        )
        .map_err(|e| format!("query STAMP error: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Stamp {
                id: r.get::<_, i64>(0)? as u64,
                subject: r.get(1)?,
                // An unknown kind cannot be stored, see the projection.
                kind: StampKind::parse(&r.get::<_, String>(2)?).unwrap_or(StampKind::Abandoned),
                evidence_msg: r.get::<_, i64>(3)? as u64,
                evidence_commit: r.get(4)?,
                evidence_ref: r.get(5)?,
                issuer: r.get(6)?,
                at: r.get::<_, i64>(7)? as u64,
                msg_id: r.get::<_, i64>(8)? as u64,
            })
        })
        .map_err(|e| format!("query STAMP error: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read STAMP error: {e}"))
}

/// One row of the standing table: a member and the count of stamps by kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StandingRow {
    pub member: String,
    /// Every kind is present, with 0 where there is none.
    pub counts: BTreeMap<String, u64>,
}

/// One row per member that has a stamp, sorted by member, with counts by kind. No tier here.
pub fn standing(conn: &Connection) -> Result<Vec<StandingRow>, String> {
    let mut by_member: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for s in query_stamps(conn)? {
        let counts = by_member.entry(s.subject).or_insert_with(|| {
            StampKind::ALL
                .iter()
                .map(|k| (k.as_str().to_owned(), 0))
                .collect()
        });
        *counts.entry(s.kind.as_str().to_owned()).or_insert(0) += 1;
    }
    Ok(by_member
        .into_iter()
        .map(|(member, counts)| StandingRow { member, counts })
        .collect())
}
