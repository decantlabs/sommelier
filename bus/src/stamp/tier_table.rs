//! The TIER table: the tier each member holds right now, as the hub last decided it.
//!
//! Shared file of the wave two spine. The tier is a pure function of the stamps (see `tier`), but the
//! scheduler, the review assignment and the hub's trust check need it as plain data. So the standing
//! part writes every change of a tier as a FACT of the hub with topic `tier/<member>`. The body is one
//! line of JSON: `v` (1), `member`, `tier` (0 to 3, or -1 for no tier at all), `reasons` (a list of
//! plain lines) and `at` (UTC epoch seconds). The log therefore shows every promotion and demotion.
//!
//! A member with no row is tier 0 when it joined with a key. A token member is tier 2, the
//! orchestrator is tier 3: they were named by the owner.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

/// DDL of the TIER table.
pub const TIER_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS TIER (
    member  TEXT PRIMARY KEY,
    tier    INTEGER NOT NULL,
    reasons TEXT NOT NULL,
    at      INTEGER NOT NULL,
    msg_id  INTEGER NOT NULL
);
";

/// The tier value that means "no tier at all" (a revoked key or a finding of bad intent).
pub const NO_TIER: i64 = -1;

/// Store projection module of the TIER table.
pub struct TierModule;

impl StoreModule for TierModule {
    fn name(&self) -> &'static str {
        "tier"
    }

    fn schema(&self) -> &'static str {
        TIER_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        if msg.kind != Kind::Fact || msg.role != Role::Hub {
            return Ok(());
        }
        let Some(member) = msg.topic.strip_prefix("tier/") else {
            return Ok(());
        };
        let Ok(body) = serde_json::from_str::<Value>(&msg.body) else {
            return Ok(());
        };
        let (Some(named), Some(tier)) = (
            body.get("member").and_then(Value::as_str),
            body.get("tier").and_then(Value::as_i64),
        ) else {
            return Ok(());
        };
        if named != member || !(NO_TIER..=3).contains(&tier) {
            return Ok(());
        }
        let reasons = body
            .get("reasons")
            .map(Value::to_string)
            .unwrap_or_else(|| "[]".into());
        let at = body.get("at").and_then(Value::as_i64).unwrap_or(0);
        tx.execute(
            "INSERT INTO TIER (member, tier, reasons, at, msg_id) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(member) DO UPDATE SET tier = ?2, reasons = ?3, at = ?4, msg_id = ?5",
            params![member, tier, reasons, at, msg.id as i64],
        )
        .map_err(|e| format!("TIER upsert failed: {e}"))?;
        Ok(())
    }

    fn api(
        &self,
        _conn: &Connection,
        _now_sec: u64,
        _state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// The tier of `member` for a decision made now: the TIER row when one exists, else the default of how
/// the member is known to the hub. `role` is `None` for a member that joined with a key. Returns
/// [`NO_TIER`] for a member that may do nothing.
pub fn tier_from_table(conn: &Connection, member: &str, role: Option<Role>) -> i64 {
    let row: Option<i64> = conn
        .query_row(
            "SELECT tier FROM TIER WHERE member = ?1",
            params![member],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    match (row, role) {
        (Some(t), _) => t,
        (None, Some(Role::Orchestrator | Role::Hub)) => 3,
        (None, Some(Role::Member)) => 2,
        (None, Some(Role::Observer)) => NO_TIER,
        (None, None) => 0,
    }
}
