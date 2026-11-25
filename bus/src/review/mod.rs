//! Assigned review: the pure choice of a reviewer, the verdict form, the review prompt and the
//! REVIEW table.
//!
//! Part of the public factory, see `docs/factory.md` sections 5.3 and 8.4. The module owns its
//! table, its projection and its part of `/api/state`. Nothing here reads a clock or a file.

pub mod hmac;
pub mod ops;
pub mod pick;
pub mod prompt;
pub mod verdict;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::msg::{Kind, Message};
use crate::store::StoreModule;

pub use hmac::{hmac_sha256, hmac_sha256_parts};
pub use pick::{
    Candidate, Exclusion, OtherFamily, Pick, ReviewRule, SubmissionView, is_audited, pick_reviewers,
};
pub use prompt::{GUIDE, MARKER, neutralise, review_prompt};
pub use verdict::{Finding, Verdict, VerdictWord, parse_verdict};

/// DDL of the REVIEW table.
pub const REVIEW_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS REVIEW (
    task TEXT NOT NULL,
    n INTEGER NOT NULL,
    submission TEXT NOT NULL DEFAULT '',
    reviewer TEXT NOT NULL DEFAULT '',
    picked_by TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'assigned',
    verdict TEXT NOT NULL DEFAULT '',
    findings TEXT NOT NULL DEFAULT '[]',
    signature TEXT NOT NULL DEFAULT '',
    audited INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task, n)
);
";

/// One row of the REVIEW table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewRow {
    pub task: String,
    pub n: u64,
    pub submission: String,
    pub reviewer: String,
    pub picked_by: String,
    /// `assigned` or `answered`.
    pub state: String,
    pub verdict: String,
    /// JSON text of the findings array.
    pub findings: String,
    pub signature: String,
    pub audited: bool,
    pub msg_id: u64,
}

/// The body of a `review/<task>/<n>` FACT. Absent fields keep what the row already holds.
#[derive(Debug, Default, Deserialize)]
struct ReviewFact {
    state: Option<String>,
    submission: Option<String>,
    reviewer: Option<String>,
    picked_by: Option<String>,
    verdict: Option<String>,
    findings: Option<Value>,
    signature: Option<String>,
    audited: Option<bool>,
}

/// Splits `review/<task>/<n>` into task and n.
fn parse_topic(topic: &str) -> Option<(String, u64)> {
    let rest = topic.strip_prefix("review/")?;
    let (task, n) = rest.rsplit_once('/')?;
    if task.is_empty() {
        return None;
    }
    Some((task.to_owned(), n.parse().ok()?))
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewRow> {
    Ok(ReviewRow {
        task: r.get(0)?,
        n: r.get::<_, i64>(1)? as u64,
        submission: r.get(2)?,
        reviewer: r.get(3)?,
        picked_by: r.get(4)?,
        state: r.get(5)?,
        verdict: r.get(6)?,
        findings: r.get(7)?,
        signature: r.get(8)?,
        audited: r.get::<_, i64>(9)? != 0,
        msg_id: r.get::<_, i64>(10)? as u64,
    })
}

const COLS: &str = "task, n, submission, reviewer, picked_by, state, verdict, findings, signature, audited, msg_id";

/// All rows, ordered by task and n.
pub fn query_reviews(conn: &Connection) -> Result<Vec<ReviewRow>, String> {
    let sql = format!("SELECT {COLS} FROM REVIEW ORDER BY task, n");
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    st.query_map([], row_from)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// The reviews that wait for an answer.
pub fn query_open_reviews(conn: &Connection) -> Result<Vec<ReviewRow>, String> {
    let sql = format!("SELECT {COLS} FROM REVIEW WHERE state = 'assigned' ORDER BY task, n");
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    st.query_map([], row_from)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Applies one message. Only a FACT with topic `review/<task>/<n>` and a JSON object body counts.
/// Anything else is ignored, a stranger's text must not break the projection.
pub fn apply_review_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    if msg.kind != Kind::Fact {
        return Ok(());
    }
    let Some((task, n)) = parse_topic(&msg.topic) else {
        return Ok(());
    };
    let Ok(fact) = serde_json::from_str::<ReviewFact>(&msg.body) else {
        return Ok(());
    };
    let old = tx
        .query_row(
            &format!("SELECT {COLS} FROM REVIEW WHERE task = ?1 AND n = ?2"),
            params![task, n as i64],
            row_from,
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let mut row = old.unwrap_or(ReviewRow {
        task,
        n,
        submission: String::new(),
        reviewer: String::new(),
        picked_by: String::new(),
        state: "assigned".into(),
        verdict: String::new(),
        findings: "[]".into(),
        signature: String::new(),
        audited: false,
        msg_id: 0,
    });
    if row.state == "answered" {
        // An answered review is frozen. Only the audit flag may still change.
        if let Some(a) = fact.audited {
            row.audited = a;
        }
    } else {
        if let Some(s) = fact.submission {
            row.submission = s;
        }
        if let Some(s) = fact.reviewer {
            row.reviewer = s;
        }
        if let Some(s) = fact.picked_by {
            row.picked_by = s;
        }
        if let Some(s) = fact.state
            && matches!(s.as_str(), "assigned" | "answered")
        {
            row.state = s;
        }
        if let Some(s) = fact.verdict {
            row.verdict = s;
        }
        if let Some(f) = fact.findings {
            row.findings = f.to_string();
        }
        if let Some(s) = fact.signature {
            row.signature = s;
        }
        if let Some(a) = fact.audited {
            row.audited = a;
        }
    }
    row.msg_id = msg.id;
    tx.execute(
        "INSERT OR REPLACE INTO REVIEW (task, n, submission, reviewer, picked_by, state, verdict, \
         findings, signature, audited, msg_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            row.task,
            row.n as i64,
            row.submission,
            row.reviewer,
            row.picked_by,
            row.state,
            row.verdict,
            row.findings,
            row.signature,
            i64::from(row.audited),
            row.msg_id as i64
        ],
    )
    .map_err(|e| format!("review projection error: {e}"))?;
    Ok(())
}

/// Store projection module of this folder.
pub struct ReviewModule;

impl StoreModule for ReviewModule {
    fn name(&self) -> &'static str {
        "review"
    }

    fn schema(&self) -> &'static str {
        REVIEW_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_review_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        state.insert("reviews".into(), json!(query_open_reviews(conn)?));
        Ok(())
    }
}
