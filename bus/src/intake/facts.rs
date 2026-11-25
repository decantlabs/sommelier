//! Rows, FACT bodies and the requeue of an order, shared by the intake and the clean build.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::msg::{Kind, Message, Role};
use crate::task::{TaskState, query_active_lease_for_task, query_task};
use crate::time::rfc3339;

/// Longest resume note of a returned order, in characters.
pub const MAX_NOTE_CHARS: usize = 1200;

/// True for 64 lower case hex characters.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// One row of the SUBMISSION table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubRow {
    pub sha256: String,
    pub task: String,
    pub attempt: u32,
    pub member: String,
    pub base: String,
    pub head: String,
    pub bytes: u64,
    pub state: String,
    pub risk: String,
    pub tier: i64,
    pub received_at: u64,
    pub updated_at: u64,
}

impl SubRow {
    /// The JSON form shown to members and on `/api/state`.
    pub fn to_json(&self) -> Value {
        json!({
            "submission": self.sha256, "task": self.task, "attempt": self.attempt,
            "member": self.member, "base": self.base, "head": self.head, "bytes": self.bytes,
            "state": self.state, "risk": self.risk, "tier": self.tier,
            "received_at": self.received_at, "updated_at": self.updated_at,
        })
    }

    /// The body of the FACT `submission/<sha256>` for a new state.
    pub fn fact_body(&self, state: &str, now_sec: u64) -> Value {
        json!({
            "v": 1, "submission": self.sha256, "task": self.task, "attempt": self.attempt,
            "member": self.member, "base": self.base, "head": self.head, "bytes": self.bytes,
            "state": state, "risk": self.risk, "tier": self.tier, "at": now_sec,
        })
    }
}

const COLS: &str = "sha256, task, attempt, member, base, head, bytes, state, risk, tier, \
                    received_at, updated_at";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubRow> {
    let n = |i| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
    Ok(SubRow {
        sha256: r.get(0)?,
        task: r.get(1)?,
        attempt: n(2)? as u32,
        member: r.get(3)?,
        base: r.get(4)?,
        head: r.get(5)?,
        bytes: n(6)?,
        state: r.get(7)?,
        risk: r.get(8)?,
        tier: r.get(9)?,
        received_at: n(10)?,
        updated_at: n(11)?,
    })
}

/// One submission by its hash.
pub fn query_submission(conn: &Connection, sha: &str) -> Option<SubRow> {
    conn.query_row(
        &format!("SELECT {COLS} FROM SUBMISSION WHERE sha256 = ?1"),
        params![sha],
        from_row,
    )
    .optional()
    .unwrap_or(None)
}

/// Every submission in one of `states`, oldest update first.
pub fn query_by_states(conn: &Connection, states: &[&str]) -> Vec<SubRow> {
    let list = states
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(",");
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {COLS} FROM SUBMISSION WHERE state IN ({list}) ORDER BY updated_at, sha256"
    )) else {
        return Vec::new();
    };
    stmt.query_map([], from_row)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// States of a submission that ended without success: its order went back to the queue or it was refused.
pub const ENDED_BAD: [&str; 4] = ["gate_failed", "verify_failed", "review_failed", "rejected"];

/// States whose files may be deleted after the keep days: the submission is over, good or bad.
pub const ENDED: [&str; 5] = [
    "gate_failed",
    "verify_failed",
    "review_failed",
    "rejected",
    "landed",
];

/// The submission of `(task, attempt)` that has not failed: still under way, approved or landed. The
/// oldest one wins when the log holds several. At most one of these may exist.
pub fn query_live_for(conn: &Connection, task: &str, attempt: u32) -> Option<SubRow> {
    let bad = ENDED_BAD
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM SUBMISSION WHERE task = ?1 AND attempt = ?2 AND state NOT IN ({bad})
             ORDER BY received_at, sha256 LIMIT 1"
        ),
        params![task, i64::from(attempt)],
        from_row,
    )
    .optional()
    .unwrap_or(None)
}

/// How many submissions `member` made since `since_sec`, whatever became of them.
pub fn count_since(conn: &Connection, member: &str, since_sec: u64) -> u64 {
    conn.query_row(
        "SELECT COUNT(*) FROM SUBMISSION WHERE member = ?1 AND received_at >= ?2",
        params![member, i64::try_from(since_sec).unwrap_or(i64::MAX)],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// The newest submission of an order that passed the static gates, one row.
pub fn query_newest_passed_for_task(
    conn: &Connection,
    task: &str,
    states: &[&str],
) -> Option<SubRow> {
    let list = states
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(",");
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM SUBMISSION WHERE task = ?1 AND state IN ({list})
             ORDER BY updated_at DESC, sha256 DESC LIMIT 1"
        ),
        params![task],
        from_row,
    )
    .optional()
    .unwrap_or(None)
}

/// Submissions that ended before `before_sec`, oldest first, at most `limit`.
pub fn query_ended_before(conn: &Connection, before_sec: u64, limit: usize) -> Vec<SubRow> {
    let list = ENDED
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {COLS} FROM SUBMISSION WHERE state IN ({list}) AND updated_at < ?1
         ORDER BY updated_at, sha256 LIMIT {limit}"
    )) else {
        return Vec::new();
    };
    stmt.query_map(
        params![i64::try_from(before_sec).unwrap_or(i64::MAX)],
        from_row,
    )
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// The newest submissions, optionally of one member.
pub fn query_newest(conn: &Connection, member: Option<&str>, limit: usize) -> Vec<SubRow> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {COLS} FROM SUBMISSION WHERE (?1 IS NULL OR member = ?1) ORDER BY updated_at DESC, msg_id DESC LIMIT {limit}"
    )) else {
        return Vec::new();
    };
    stmt.query_map(params![member], from_row)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// A FACT of the hub.
pub fn fact(topic: String, body: &Value) -> Message {
    Message {
        id: 0,
        ts: String::new(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic,
        body: body.to_string(),
        refs: vec![],
        to: vec![],
    }
}

/// The FACT that moves a submission to `state`.
pub fn submission_fact(row: &SubRow, state: &str, now_sec: u64) -> Message {
    fact(
        format!("submission/{}", row.sha256),
        &row.fact_body(state, now_sec),
    )
}

/// Cuts a text to one line of at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(3)).collect();
    cut.push_str("...");
    cut
}

/// The message that returns the order of a submission to the queue with `note` as resume note, the
/// same update as `task.requeue`. `None` when the order has moved on: another attempt runs, the
/// lease is not the member's or the state does not allow a return.
pub fn requeue_message(
    conn: &Connection,
    sub: &SubRow,
    reason: &str,
    note: &str,
    now_ms: u64,
) -> Option<Message> {
    let task = query_task(conn, &sub.task).ok().flatten()?;
    if task.attempt != sub.attempt {
        return None;
    }
    let prev = TaskState::parse(&task.state)?;
    if prev == TaskState::Ready || !prev.can_transition(TaskState::Ready) {
        return None;
    }
    let mut update = json!({
        "v": 1,
        "task": task.id,
        "prev": task.state,
        "state": "ready",
        "attempt": task.attempt.saturating_add(1),
        "by": "hub",
        "reason": reason,
        "resume_note": one_line(note, MAX_NOTE_CHARS),
    });
    if let Some(mut lease) = query_active_lease_for_task(conn, &task.id).ok().flatten() {
        if lease.attempt != sub.attempt {
            return None;
        }
        lease.state = "released".into();
        lease.end_reason = "requeued".into();
        update["lease"] = json!(lease);
    }
    Some(Message {
        id: 0,
        ts: rfc3339(now_ms),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::TaskUpdate,
        topic: format!("task/{}", task.id),
        body: update.to_string(),
        refs: vec!["state:ready".into()],
        to: vec![],
    })
}
