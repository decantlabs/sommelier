//! Reads of the tables that the review part decides on. Only reads, no clock and no file access.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::ReviewRow;
use super::prompt::escape_line;

/// One row of the SUBMISSION table, with everything the intake wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubRow {
    pub sha256: String,
    pub task: String,
    pub attempt: u64,
    pub member: String,
    pub base: String,
    pub head: String,
    pub bytes: u64,
    pub state: String,
    pub risk: String,
    pub tier: u8,
}

const SUB_COLS: &str = "sha256, task, attempt, member, base, head, bytes, state, risk, tier";

fn sub_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubRow> {
    Ok(SubRow {
        sha256: r.get(0)?,
        task: r.get(1)?,
        attempt: r.get::<_, i64>(2)? as u64,
        member: r.get(3)?,
        base: r.get(4)?,
        head: r.get(5)?,
        bytes: r.get::<_, i64>(6)? as u64,
        state: r.get(7)?,
        risk: r.get(8)?,
        tier: r.get::<_, i64>(9)?.clamp(0, 3) as u8,
    })
}

/// The submission with this SHA 256, if the table has it.
pub fn query_submission(conn: &Connection, sha: &str) -> Result<Option<SubRow>, String> {
    conn.query_row(
        &format!("SELECT {SUB_COLS} FROM SUBMISSION WHERE sha256 = ?1"),
        params![sha],
        sub_from,
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// Every submission in `state`, oldest first.
pub fn submissions_in_state(conn: &Connection, state: &str) -> Result<Vec<SubRow>, String> {
    let mut st = conn
        .prepare(&format!(
            "SELECT {SUB_COLS} FROM SUBMISSION WHERE state = ?1 ORDER BY received_at, sha256"
        ))
        .map_err(|e| e.to_string())?;
    st.query_map(params![state], sub_from)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// What the hub wrote beside a review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// `required` or `audit`.
    pub role: String,
    /// Passing reviews the submission needs.
    pub required: u32,
    pub at: u64,
    pub due: u64,
    /// Earlier holders that did not answer.
    pub prev: Vec<String>,
}

/// A review row with its meta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub row: ReviewRow,
    pub meta: Meta,
}

impl Review {
    pub fn is_audit(&self) -> bool {
        self.meta.role == "audit"
    }
}

const REVIEW_SQL: &str = "SELECT r.task, r.n, r.submission, r.reviewer, r.picked_by, r.state, r.verdict, \
     r.findings, r.signature, r.audited, r.msg_id, \
     COALESCE(m.role, 'required'), COALESCE(m.required, 1), COALESCE(m.at, 0), COALESCE(m.due, 0), \
     COALESCE(m.prev, '[]') \
     FROM REVIEW r LEFT JOIN REVIEW_META m ON m.task = r.task AND m.n = r.n";

fn review_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Review> {
    let prev: String = r.get(15)?;
    Ok(Review {
        row: ReviewRow {
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
        },
        meta: Meta {
            role: r.get(11)?,
            required: r.get::<_, i64>(12)?.max(1) as u32,
            at: r.get::<_, i64>(13)? as u64,
            due: r.get::<_, i64>(14)? as u64,
            prev: serde_json::from_str(&prev).unwrap_or_default(),
        },
    })
}

/// All reviews of one submission, by n.
pub fn reviews_of(conn: &Connection, sha: &str) -> Result<Vec<Review>, String> {
    let mut st = conn
        .prepare(&format!(
            "{REVIEW_SQL} WHERE r.submission = ?1 ORDER BY r.n"
        ))
        .map_err(|e| e.to_string())?;
    st.query_map(params![sha], review_from)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// One review by task and n.
pub fn review_of(conn: &Connection, task: &str, n: u64) -> Result<Option<Review>, String> {
    conn.query_row(
        &format!("{REVIEW_SQL} WHERE r.task = ?1 AND r.n = ?2"),
        params![task, n as i64],
        review_from,
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// The open reviews of a member whose submission still waits in `reviewing`.
pub fn open_reviews_of(conn: &Connection, member: &str) -> Result<Vec<Review>, String> {
    let mut st = conn
        .prepare(&format!(
            "{REVIEW_SQL} JOIN SUBMISSION s ON s.sha256 = r.submission \
             WHERE r.reviewer = ?1 AND r.state = 'assigned' AND s.state = 'reviewing' \
             ORDER BY r.task, r.n"
        ))
        .map_err(|e| e.to_string())?;
    st.query_map(params![member], review_from)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// How many open reviews each reviewer holds on submissions that are still in `reviewing`.
pub fn open_counts(conn: &Connection) -> Result<Vec<(String, u32)>, String> {
    let mut st = conn
        .prepare(
            "SELECT r.reviewer, COUNT(*) FROM REVIEW r JOIN SUBMISSION s ON s.sha256 = r.submission \
             WHERE r.state = 'assigned' AND s.state = 'reviewing' GROUP BY r.reviewer",
        )
        .map_err(|e| e.to_string())?;
    st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// The next free review number of a task, from 1.
pub fn next_n(conn: &Connection, task: &str) -> Result<u64, String> {
    conn.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM REVIEW WHERE task = ?1",
        params![task],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .map_err(|e| e.to_string())
}

/// Counts of reviews by state, for `/api/state`.
pub fn review_counts(conn: &Connection) -> Result<Value, String> {
    let mut st = conn
        .prepare("SELECT state, COUNT(*) FROM REVIEW GROUP BY state ORDER BY state")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut map = serde_json::Map::new();
    for state in ["assigned", "answered"] {
        map.insert(state.into(), 0.into());
    }
    for r in rows {
        let (state, n) = r.map_err(|e| e.to_string())?;
        map.insert(state, n.into());
    }
    Ok(Value::Object(map))
}

/// The gate report of a submission as `(passed, body)`.
pub fn gate_of(conn: &Connection, sha: &str) -> Result<Option<(bool, String)>, String> {
    conn.query_row(
        "SELECT passed, report FROM REVIEW_GATE WHERE sha256 = ?1",
        params![sha],
        |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?)),
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// The conclusion of the clean build, or `None` while the run has not ended or is unknown.
pub fn verify_conclusion(conn: &Connection, sha: &str) -> Result<Option<String>, String> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT state, conclusion FROM REVIEW_VERIFY WHERE sha256 = ?1",
            params![sha],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(row.and_then(|(state, c)| (state == "done" && !c.is_empty()).then_some(c)))
}

/// True when the ALERT table already holds this alert, open or closed.
pub fn alert_exists(conn: &Connection, code: &str, key: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM ALERT WHERE code = ?1 AND key = ?2",
        params![code, key],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Submissions whose `bundle:` entry in the landing queue has landed.
pub fn landed_bundles(conn: &Connection) -> Result<Vec<String>, String> {
    let mut st = conn
        .prepare(
            "SELECT ref_name FROM LANDING WHERE state = 'landed' AND ref_name LIKE 'bundle:%' \
             ORDER BY ref_name",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        let name = r.map_err(|e| e.to_string())?;
        if let Some(sha) = name.strip_prefix("bundle:") {
            out.push(sha.to_owned());
        }
    }
    Ok(out)
}

/// The order text of a task for the reviewer: title, scope and done condition.
pub fn order_text(conn: &Connection, task: &str) -> String {
    match crate::task::query_task(conn, task) {
        Ok(Some(t)) => format!(
            "task: {}\ntitle: {}\nscope: {}\ndone when: {}\n",
            escape_line(&t.id),
            escape_line(&t.title),
            escape_line(&t.scope),
            escape_line(&t.done_when)
        ),
        _ => format!(
            "task: {}\n(the order text is not known to the hub)\n",
            escape_line(task)
        ),
    }
}

/// The model family of a member: the part of its newest declared class before the colon.
pub fn family_of(conn: &Connection, member: &str) -> Option<String> {
    let class: Option<String> = conn
        .query_row(
            "SELECT class FROM CAPACITY WHERE owner = ?1 AND class != '' ORDER BY declared_at DESC LIMIT 1",
            params![member],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    let class = class?;
    let fam = class.split(':').next().unwrap_or("").trim().to_lowercase();
    (!fam.is_empty()).then_some(fam)
}
