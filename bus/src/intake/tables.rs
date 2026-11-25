//! The projections of the intake: SUBMISSION by the contract, GATE and VERIFY as own tables.
//!
//! Each row comes only from a FACT of the hub with the topic `submission/<sha256>`, `gate/<sha256>`
//! or `verify/<sha256>`. The same topic from any other role projects nothing. The newest FACT wins,
//! so replaying the log rebuilds the same rows.

use std::sync::OnceLock;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};

use super::facts::{is_sha256, query_newest};
use super::schema::{SUBMISSION_SCHEMA_SQL, SUBMISSION_STATES};
use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

/// DDL of the GATE and VERIFY tables.
pub const GATE_VERIFY_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS GATE (
    sha256     TEXT PRIMARY KEY,
    passed     INTEGER NOT NULL,
    risk_after TEXT NOT NULL,
    findings   INTEGER NOT NULL,
    report     TEXT NOT NULL,
    at         INTEGER NOT NULL,
    msg_id     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS VERIFY (
    sha256       TEXT PRIMARY KEY,
    task         TEXT NOT NULL,
    state        TEXT NOT NULL,
    run_id       INTEGER,
    conclusion   TEXT,
    failing_step TEXT,
    minutes      INTEGER NOT NULL,
    dispatches   INTEGER NOT NULL,
    started_ms   INTEGER NOT NULL,
    at           INTEGER NOT NULL,
    msg_id       INTEGER NOT NULL
);
";

/// Store projection module of SUBMISSION, GATE and VERIFY.
pub struct IntakeModule;

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

impl IntakeModule {
    fn submission(tx: &Transaction<'_>, msg: &Message, sha: &str, b: &Value) -> Result<(), String> {
        let (Some(task), Some(member), Some(state)) =
            (text(b, "task"), text(b, "member"), text(b, "state"))
        else {
            return Ok(());
        };
        if text(b, "submission").as_deref() != Some(sha)
            || !SUBMISSION_STATES.contains(&state.as_str())
        {
            return Ok(());
        }
        let at = num(b, "at").unwrap_or(0);
        tx.execute(
            "INSERT INTO SUBMISSION (sha256, task, attempt, member, base, head, bytes, state, risk,
                                     tier, received_at, updated_at, msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)
             ON CONFLICT(sha256) DO UPDATE SET head = ?6, bytes = ?7, state = ?8, risk = ?9,
                 tier = ?10, updated_at = ?11, msg_id = ?12",
            params![
                sha,
                task,
                num(b, "attempt").unwrap_or(0),
                member,
                text(b, "base").unwrap_or_default(),
                text(b, "head").unwrap_or_default(),
                num(b, "bytes").unwrap_or(0),
                state,
                text(b, "risk").unwrap_or_else(|| "normal".into()),
                num(b, "tier").unwrap_or(0),
                at,
                msg.id as i64,
            ],
        )
        .map_err(|e| format!("SUBMISSION upsert failed: {e}"))?;
        Ok(())
    }

    fn gate(tx: &Transaction<'_>, msg: &Message, sha: &str, b: &Value) -> Result<(), String> {
        let (Some(passed), Some(risk)) = (
            b.get("passed").and_then(Value::as_bool),
            text(b, "risk_after"),
        ) else {
            return Ok(());
        };
        if text(b, "submission").as_deref() != Some(sha) {
            return Ok(());
        }
        let findings = b
            .get("findings")
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as i64;
        tx.execute(
            "INSERT INTO GATE (sha256, passed, risk_after, findings, report, at, msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sha256) DO UPDATE SET passed = ?2, risk_after = ?3, findings = ?4,
                 report = ?5, at = ?6, msg_id = ?7",
            params![
                sha,
                passed,
                risk,
                findings,
                msg.body,
                num(b, "at").unwrap_or(0),
                msg.id as i64
            ],
        )
        .map_err(|e| format!("GATE upsert failed: {e}"))?;
        Ok(())
    }

    fn verify(tx: &Transaction<'_>, msg: &Message, sha: &str, b: &Value) -> Result<(), String> {
        let Some(state) = text(b, "state") else {
            return Ok(());
        };
        if text(b, "submission").as_deref() != Some(sha)
            || !["queued", "dispatched", "running", "done"].contains(&state.as_str())
        {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO VERIFY (sha256, task, state, run_id, conclusion, failing_step, minutes,
                                 dispatches, started_ms, at, msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(sha256) DO UPDATE SET task = ?2, state = ?3, run_id = ?4, conclusion = ?5,
                 failing_step = ?6, minutes = ?7, dispatches = ?8, started_ms = ?9, at = ?10,
                 msg_id = ?11",
            params![
                sha,
                text(b, "order").unwrap_or_default(),
                state,
                num(b, "run_id"),
                text(b, "conclusion"),
                text(b, "failing_step"),
                num(b, "minutes").unwrap_or(0),
                num(b, "dispatches").unwrap_or(0),
                num(b, "started_ms").unwrap_or(0),
                num(b, "at").unwrap_or(0),
                msg.id as i64
            ],
        )
        .map_err(|e| format!("VERIFY upsert failed: {e}"))?;
        Ok(())
    }
}

impl StoreModule for IntakeModule {
    fn name(&self) -> &'static str {
        "intake"
    }

    fn schema(&self) -> &'static str {
        static SQL: OnceLock<String> = OnceLock::new();
        SQL.get_or_init(|| format!("{SUBMISSION_SCHEMA_SQL}{GATE_VERIFY_SCHEMA_SQL}"))
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        if msg.kind != Kind::Fact || msg.role != Role::Hub {
            return Ok(());
        }
        let Some((area, sha)) = msg.topic.split_once('/') else {
            return Ok(());
        };
        if !matches!(area, "submission" | "gate" | "verify") || !is_sha256(sha) {
            return Ok(());
        }
        let Ok(body) = serde_json::from_str::<Value>(&msg.body) else {
            return Ok(());
        };
        match area {
            "submission" => Self::submission(tx, msg, sha, &body),
            "gate" => Self::gate(tx, msg, sha, &body),
            _ => Self::verify(tx, msg, sha, &body),
        }
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let mut counts = Map::new();
        let mut stmt = conn
            .prepare("SELECT state, COUNT(*) FROM SUBMISSION GROUP BY state ORDER BY state")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (s, n) = row.map_err(|e| e.to_string())?;
            counts.insert(s, json!(n));
        }
        let newest: Vec<Value> = query_newest(conn, None, 20)
            .iter()
            .map(|r| r.to_json())
            .collect();
        state.insert(
            "submissions".into(),
            json!({"counts": counts, "newest": newest}),
        );
        Ok(())
    }
}
