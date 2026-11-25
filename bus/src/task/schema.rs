//! SQLite schema, row types, and query/upsert helpers for `TASK`, `LEASE`, `OFFER`, and `CHECKPOINT` (section 3, step 8).

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Names of the four task-queue tables owned by `TaskModule` (section 3, 10.3).
pub const TASK_TABLES: [&str; 5] = ["TASK", "LEASE", "OFFER", "CHECKPOINT", "TASK_ORDER"];

/// SQLite DDL for `TASK`, `LEASE`, `OFFER`, and `CHECKPOINT` in `state.db`.
pub const TASK_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS TASK (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'fix',
    priority TEXT NOT NULL DEFAULT 'normal',
    state TEXT NOT NULL DEFAULT 'ready',
    track TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT '',
    accepts TEXT NOT NULL DEFAULT '',
    min_tier TEXT NOT NULL DEFAULT '',
    branch TEXT NOT NULL DEFAULT '',
    base_sha TEXT NOT NULL DEFAULT '',
    brief_sha256 TEXT NOT NULL DEFAULT '',
    deps TEXT NOT NULL DEFAULT '[]',
    size TEXT NOT NULL DEFAULT '',
    checkpointable INTEGER NOT NULL DEFAULT 1,
    attempt INTEGER NOT NULL DEFAULT 1,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    resume_note TEXT NOT NULL DEFAULT '',
    resume_sha TEXT NOT NULL DEFAULT '',
    wip_ref TEXT NOT NULL DEFAULT '',
    exclude_pools_until TEXT NOT NULL DEFAULT '{}',
    prior_art INTEGER NOT NULL DEFAULT 0,
    done_when TEXT NOT NULL DEFAULT '',
    result_ref TEXT NOT NULL DEFAULT '',
    result_sha TEXT NOT NULL DEFAULT '',
    created_by TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL DEFAULT 0,
    ready_at INTEGER NOT NULL DEFAULT 0,
    closed_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);

-- The order fields of the factory (docs/factory.md section 6). They sit in their own table so that
-- a state.db made before them opens unchanged: a task without a row here is risk normal, min_trust 2.
CREATE TABLE IF NOT EXISTS TASK_ORDER (
    task_id TEXT PRIMARY KEY,
    risk TEXT NOT NULL DEFAULT 'normal',
    min_trust INTEGER NOT NULL DEFAULT 2,
    review TEXT NOT NULL DEFAULT 'default',
    credit TEXT NOT NULL DEFAULT 'handle'
);

CREATE TABLE IF NOT EXISTS LEASE (
    task_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    owner TEXT NOT NULL DEFAULT '',
    worker TEXT NOT NULL DEFAULT '',
    machine TEXT NOT NULL DEFAULT '',
    class TEXT NOT NULL DEFAULT '',
    pools TEXT NOT NULL DEFAULT '[]',
    granted_at INTEGER NOT NULL DEFAULT 0,
    heartbeat_at INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL DEFAULT 0,
    est_minutes_p90 REAL NOT NULL DEFAULT 0.0,
    est_cost REAL NOT NULL DEFAULT 0.0,
    est_end_at INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'active',
    end_reason TEXT NOT NULL DEFAULT '',
    supervised INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, attempt)
);

CREATE TABLE IF NOT EXISTS OFFER (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    to_owner TEXT NOT NULL DEFAULT '',
    class TEXT NOT NULL DEFAULT '',
    sent_at INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'open',
    reason TEXT NOT NULL DEFAULT '',
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS CHECKPOINT (
    task_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL DEFAULT 0,
    branch TEXT NOT NULL DEFAULT '',
    sha TEXT NOT NULL DEFAULT '',
    pushed INTEGER NOT NULL DEFAULT 0,
    note TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'agent',
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, attempt, seq)
);
";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub priority: String,
    pub state: String,
    pub track: String,
    pub scope: String,
    pub accepts: String,
    pub min_tier: String,
    pub branch: String,
    pub base_sha: String,
    pub brief_sha256: String,
    pub deps: Vec<String>,
    pub size: String,
    pub checkpointable: bool,
    pub attempt: u32,
    pub max_attempts: u32,
    pub resume_note: String,
    pub resume_sha: String,
    pub wip_ref: String,
    pub exclude_pools_until: Value,
    pub prior_art: bool,
    pub done_when: String,
    pub result_ref: String,
    pub result_sha: String,
    pub created_by: String,
    pub created_at: u64,
    pub ready_at: u64,
    pub closed_at: u64,
    pub msg_id: u64,
    /// `low`, `normal` or `guarded`. Computed at `task add` from the scope.
    #[serde(default = "default_risk")]
    pub risk: String,
    /// Lowest tier (0 to 3) that may lease the order.
    #[serde(default = "default_min_trust")]
    pub min_trust: u8,
    /// Name of the review rule of the policy.
    #[serde(default = "default_review")]
    pub review: String,
    /// How the commit names the member: `handle` or `none`.
    #[serde(default = "default_credit")]
    pub credit: String,
}

pub const DEFAULT_RISK: &str = "normal";
pub const DEFAULT_MIN_TRUST: u8 = 2;
pub const DEFAULT_REVIEW: &str = "default";
pub const DEFAULT_CREDIT: &str = "handle";

fn default_risk() -> String {
    DEFAULT_RISK.into()
}
fn default_min_trust() -> u8 {
    DEFAULT_MIN_TRUST
}
fn default_review() -> String {
    DEFAULT_REVIEW.into()
}
fn default_credit() -> String {
    DEFAULT_CREDIT.into()
}

impl Default for TaskRow {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            kind: "fix".into(),
            priority: "normal".into(),
            state: "ready".into(),
            track: String::new(),
            scope: String::new(),
            accepts: String::new(),
            min_tier: String::new(),
            branch: String::new(),
            base_sha: String::new(),
            brief_sha256: String::new(),
            deps: Vec::new(),
            size: String::new(),
            checkpointable: true,
            attempt: 1,
            max_attempts: 3,
            resume_note: String::new(),
            resume_sha: String::new(),
            wip_ref: String::new(),
            exclude_pools_until: json!({}),
            prior_art: false,
            done_when: String::new(),
            result_ref: String::new(),
            result_sha: String::new(),
            created_by: String::new(),
            created_at: 0,
            ready_at: 0,
            closed_at: 0,
            msg_id: 0,
            risk: default_risk(),
            min_trust: default_min_trust(),
            review: default_review(),
            credit: default_credit(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseRow {
    pub task_id: String,
    pub attempt: u32,
    pub owner: String,
    pub worker: String,
    pub machine: String,
    pub class: String,
    pub pools: Vec<String>,
    pub granted_at: u64,
    pub heartbeat_at: u64,
    pub expires_at: u64,
    pub est_minutes_p90: f64,
    pub est_cost: f64,
    pub est_end_at: u64,
    pub state: String,
    pub end_reason: String,
    pub supervised: bool,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferRow {
    pub id: String,
    pub task_id: String,
    pub to_owner: String,
    pub class: String,
    pub sent_at: u64,
    pub expires_at: u64,
    pub state: String,
    pub reason: String,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub task_id: String,
    pub attempt: u32,
    pub seq: u32,
    pub at: u64,
    pub branch: String,
    pub sha: String,
    pub pushed: bool,
    pub note: String,
    pub source: String,
    pub msg_id: u64,
}

pub fn upsert_task_tx(tx: &Transaction<'_>, row: &TaskRow) -> Result<(), String> {
    let deps_json = serde_json::to_string(&row.deps).map_err(|e| e.to_string())?;
    let exclude_json =
        serde_json::to_string(&row.exclude_pools_until).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO TASK (
            id, title, kind, priority, state, track, scope, accepts, min_tier,
            branch, base_sha, brief_sha256, deps, size, checkpointable,
            attempt, max_attempts, resume_note, resume_sha, wip_ref,
            exclude_pools_until, prior_art, done_when, result_ref, result_sha,
            created_by, created_at, ready_at, closed_at, msg_id
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
            ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19, ?20,
            ?21, ?22, ?23, ?24, ?25,
            ?26, ?27, ?28, ?29, ?30
        )
        ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            kind = excluded.kind,
            priority = excluded.priority,
            state = excluded.state,
            track = excluded.track,
            scope = excluded.scope,
            accepts = excluded.accepts,
            min_tier = excluded.min_tier,
            branch = excluded.branch,
            base_sha = excluded.base_sha,
            brief_sha256 = excluded.brief_sha256,
            deps = excluded.deps,
            size = excluded.size,
            checkpointable = excluded.checkpointable,
            attempt = excluded.attempt,
            max_attempts = excluded.max_attempts,
            resume_note = excluded.resume_note,
            resume_sha = excluded.resume_sha,
            wip_ref = excluded.wip_ref,
            exclude_pools_until = excluded.exclude_pools_until,
            prior_art = excluded.prior_art,
            done_when = excluded.done_when,
            result_ref = excluded.result_ref,
            result_sha = excluded.result_sha,
            created_by = excluded.created_by,
            created_at = excluded.created_at,
            ready_at = excluded.ready_at,
            closed_at = excluded.closed_at,
            msg_id = excluded.msg_id",
        params![
            row.id,
            row.title,
            row.kind,
            row.priority,
            row.state,
            row.track,
            row.scope,
            row.accepts,
            row.min_tier,
            row.branch,
            row.base_sha,
            row.brief_sha256,
            deps_json,
            row.size,
            i64::from(row.checkpointable),
            i64::from(row.attempt),
            i64::from(row.max_attempts),
            row.resume_note,
            row.resume_sha,
            row.wip_ref,
            exclude_json,
            i64::from(row.prior_art),
            row.done_when,
            row.result_ref,
            row.result_sha,
            row.created_by,
            row.created_at as i64,
            row.ready_at as i64,
            row.closed_at as i64,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert TASK failed: {e}"))?;
    tx.execute(
        "INSERT INTO TASK_ORDER (task_id, risk, min_trust, review, credit)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(task_id) DO UPDATE SET
            risk = excluded.risk,
            min_trust = excluded.min_trust,
            review = excluded.review,
            credit = excluded.credit",
        params![
            row.id,
            row.risk,
            i64::from(row.min_trust),
            row.review,
            row.credit
        ],
    )
    .map_err(|e| format!("upsert TASK_ORDER failed: {e}"))?;
    Ok(())
}

/// `(risk, min_trust, review, credit)` of one task in `TASK_ORDER`.
type OrderFields = (String, u8, String, String);

/// Reads the order fields of every task that has a row, keyed by task id.
fn order_fields(
    conn: &Connection,
    only: Option<&str>,
) -> Result<std::collections::BTreeMap<String, OrderFields>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT task_id, risk, min_trust, review, credit FROM TASK_ORDER
             WHERE ?1 IS NULL OR task_id = ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![only], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?.clamp(0, 3) as u8,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ),
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        let (id, v) = row.map_err(|e| e.to_string())?;
        out.insert(id, v);
    }
    Ok(out)
}

/// Puts the order fields of the side table into rows read from `TASK`.
fn fill_orders(conn: &Connection, rows: &mut [TaskRow], only: Option<&str>) -> Result<(), String> {
    let mut fields = order_fields(conn, only)?;
    for row in rows {
        if let Some((risk, min_trust, review, credit)) = fields.remove(&row.id) {
            row.risk = risk;
            row.min_trust = min_trust;
            row.review = review;
            row.credit = credit;
        }
    }
    Ok(())
}

pub fn upsert_lease_tx(tx: &Transaction<'_>, row: &LeaseRow) -> Result<(), String> {
    let pools_json = serde_json::to_string(&row.pools).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO LEASE (
            task_id, attempt, owner, worker, machine, class, pools,
            granted_at, heartbeat_at, expires_at, est_minutes_p90, est_cost,
            est_end_at, state, end_reason, supervised, msg_id
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17
        )
        ON CONFLICT(task_id, attempt) DO UPDATE SET
            owner = excluded.owner,
            worker = excluded.worker,
            machine = excluded.machine,
            class = excluded.class,
            pools = excluded.pools,
            granted_at = excluded.granted_at,
            heartbeat_at = excluded.heartbeat_at,
            expires_at = excluded.expires_at,
            est_minutes_p90 = excluded.est_minutes_p90,
            est_cost = excluded.est_cost,
            est_end_at = excluded.est_end_at,
            state = excluded.state,
            end_reason = excluded.end_reason,
            supervised = excluded.supervised,
            msg_id = excluded.msg_id",
        params![
            row.task_id,
            i64::from(row.attempt),
            row.owner,
            row.worker,
            row.machine,
            row.class,
            pools_json,
            row.granted_at as i64,
            row.heartbeat_at as i64,
            row.expires_at as i64,
            row.est_minutes_p90,
            row.est_cost,
            row.est_end_at as i64,
            row.state,
            row.end_reason,
            i64::from(row.supervised),
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert LEASE failed: {e}"))?;
    Ok(())
}

pub fn upsert_offer_tx(tx: &Transaction<'_>, row: &OfferRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO OFFER (
            id, task_id, to_owner, class, sent_at, expires_at, state, reason, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(id) DO UPDATE SET
            task_id = excluded.task_id,
            to_owner = excluded.to_owner,
            class = excluded.class,
            sent_at = excluded.sent_at,
            expires_at = excluded.expires_at,
            state = excluded.state,
            reason = excluded.reason,
            msg_id = excluded.msg_id",
        params![
            row.id,
            row.task_id,
            row.to_owner,
            row.class,
            row.sent_at as i64,
            row.expires_at as i64,
            row.state,
            row.reason,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert OFFER failed: {e}"))?;
    Ok(())
}

pub fn insert_checkpoint_tx(tx: &Transaction<'_>, row: &CheckpointRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO CHECKPOINT (
            task_id, attempt, seq, at, branch, sha, pushed, note, source, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(task_id, attempt, seq) DO UPDATE SET
            at = excluded.at,
            branch = excluded.branch,
            sha = excluded.sha,
            pushed = excluded.pushed,
            note = excluded.note,
            source = excluded.source,
            msg_id = excluded.msg_id",
        params![
            row.task_id,
            i64::from(row.attempt),
            i64::from(row.seq),
            row.at as i64,
            row.branch,
            row.sha,
            i64::from(row.pushed),
            row.note,
            row.source,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("insert CHECKPOINT failed: {e}"))?;
    Ok(())
}

fn task_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    let deps_raw: String = r.get(12)?;
    let exclude_raw: String = r.get(20)?;
    Ok(TaskRow {
        id: r.get(0)?,
        title: r.get(1)?,
        kind: r.get(2)?,
        priority: r.get(3)?,
        state: r.get(4)?,
        track: r.get(5)?,
        scope: r.get(6)?,
        accepts: r.get(7)?,
        min_tier: r.get(8)?,
        branch: r.get(9)?,
        base_sha: r.get(10)?,
        brief_sha256: r.get(11)?,
        deps: serde_json::from_str(&deps_raw).unwrap_or_default(),
        size: r.get(13)?,
        checkpointable: r.get::<_, i64>(14)? != 0,
        attempt: r.get::<_, i64>(15)?.max(0) as u32,
        max_attempts: r.get::<_, i64>(16)?.max(0) as u32,
        resume_note: r.get(17)?,
        resume_sha: r.get(18)?,
        wip_ref: r.get(19)?,
        exclude_pools_until: serde_json::from_str(&exclude_raw).unwrap_or_else(|_| json!({})),
        prior_art: r.get::<_, i64>(21)? != 0,
        done_when: r.get(22)?,
        result_ref: r.get(23)?,
        result_sha: r.get(24)?,
        created_by: r.get(25)?,
        created_at: r.get::<_, i64>(26)?.max(0) as u64,
        ready_at: r.get::<_, i64>(27)?.max(0) as u64,
        closed_at: r.get::<_, i64>(28)?.max(0) as u64,
        msg_id: r.get::<_, i64>(29)?.max(0) as u64,
        risk: default_risk(),
        min_trust: default_min_trust(),
        review: default_review(),
        credit: default_credit(),
    })
}

pub fn query_tasks(conn: &Connection) -> Result<Vec<TaskRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, title, kind, priority, state, track, scope, accepts, min_tier,
                    branch, base_sha, brief_sha256, deps, size, checkpointable,
                    attempt, max_attempts, resume_note, resume_sha, wip_ref,
                    exclude_pools_until, prior_art, done_when, result_ref, result_sha,
                    created_by, created_at, ready_at, closed_at, msg_id
             FROM TASK ORDER BY id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], task_from_row)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut rows = rows;
    fill_orders(conn, &mut rows, None)?;
    Ok(rows)
}

pub fn query_task(conn: &Connection, id: &str) -> Result<Option<TaskRow>, String> {
    let row = conn
        .query_row(
            "SELECT id, title, kind, priority, state, track, scope, accepts, min_tier,
                branch, base_sha, brief_sha256, deps, size, checkpointable,
                attempt, max_attempts, resume_note, resume_sha, wip_ref,
                exclude_pools_until, prior_art, done_when, result_ref, result_sha,
                created_by, created_at, ready_at, closed_at, msg_id
         FROM TASK WHERE id = ?1",
            params![id],
            task_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(mut row) = row else {
        return Ok(None);
    };
    fill_orders(conn, std::slice::from_mut(&mut row), Some(id))?;
    Ok(Some(row))
}

fn lease_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRow> {
    let pools_raw: String = r.get(6)?;
    Ok(LeaseRow {
        task_id: r.get(0)?,
        attempt: r.get::<_, i64>(1)?.max(0) as u32,
        owner: r.get(2)?,
        worker: r.get(3)?,
        machine: r.get(4)?,
        class: r.get(5)?,
        pools: serde_json::from_str(&pools_raw).unwrap_or_default(),
        granted_at: r.get::<_, i64>(7)?.max(0) as u64,
        heartbeat_at: r.get::<_, i64>(8)?.max(0) as u64,
        expires_at: r.get::<_, i64>(9)?.max(0) as u64,
        est_minutes_p90: r.get(10)?,
        est_cost: r.get(11)?,
        est_end_at: r.get::<_, i64>(12)?.max(0) as u64,
        state: r.get(13)?,
        end_reason: r.get(14)?,
        supervised: r.get::<_, i64>(15)? != 0,
        msg_id: r.get::<_, i64>(16)?.max(0) as u64,
    })
}

pub fn query_leases(conn: &Connection) -> Result<Vec<LeaseRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT task_id, attempt, owner, worker, machine, class, pools,
                    granted_at, heartbeat_at, expires_at, est_minutes_p90, est_cost,
                    est_end_at, state, end_reason, supervised, msg_id
             FROM LEASE ORDER BY task_id ASC, attempt ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], lease_from_row)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

pub fn query_active_lease_for_task(
    conn: &Connection,
    task_id: &str,
) -> Result<Option<LeaseRow>, String> {
    conn.query_row(
        "SELECT task_id, attempt, owner, worker, machine, class, pools,
                granted_at, heartbeat_at, expires_at, est_minutes_p90, est_cost,
                est_end_at, state, end_reason, supervised, msg_id
         FROM LEASE WHERE task_id = ?1 AND state IN ('active', 'suspect')
         ORDER BY attempt DESC LIMIT 1",
        params![task_id],
        lease_from_row,
    )
    .optional()
    .map_err(|e| e.to_string())
}

pub fn query_offers(conn: &Connection) -> Result<Vec<OfferRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, task_id, to_owner, class, sent_at, expires_at, state, reason, msg_id
             FROM OFFER ORDER BY sent_at ASC, id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(OfferRow {
                id: r.get(0)?,
                task_id: r.get(1)?,
                to_owner: r.get(2)?,
                class: r.get(3)?,
                sent_at: r.get::<_, i64>(4)?.max(0) as u64,
                expires_at: r.get::<_, i64>(5)?.max(0) as u64,
                state: r.get(6)?,
                reason: r.get(7)?,
                msg_id: r.get::<_, i64>(8)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

pub fn query_checkpoints(conn: &Connection) -> Result<Vec<CheckpointRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT task_id, attempt, seq, at, branch, sha, pushed, note, source, msg_id
             FROM CHECKPOINT ORDER BY task_id ASC, attempt ASC, seq ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CheckpointRow {
                task_id: r.get(0)?,
                attempt: r.get::<_, i64>(1)?.max(0) as u32,
                seq: r.get::<_, i64>(2)?.max(0) as u32,
                at: r.get::<_, i64>(3)?.max(0) as u64,
                branch: r.get(4)?,
                sha: r.get(5)?,
                pushed: r.get::<_, i64>(6)? != 0,
                note: r.get(7)?,
                source: r.get(8)?,
                msg_id: r.get::<_, i64>(9)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

pub fn latest_checkpoint(
    conn: &Connection,
    task_id: &str,
    attempt: Option<u32>,
) -> Result<Option<CheckpointRow>, String> {
    let sql = if attempt.is_some() {
        "SELECT task_id, attempt, seq, at, branch, sha, pushed, note, source, msg_id
         FROM CHECKPOINT WHERE task_id = ?1 AND attempt = ?2
         ORDER BY seq DESC LIMIT 1"
    } else {
        "SELECT task_id, attempt, seq, at, branch, sha, pushed, note, source, msg_id
         FROM CHECKPOINT WHERE task_id = ?1
         ORDER BY attempt DESC, seq DESC LIMIT 1"
    };
    let mapper = |r: &rusqlite::Row<'_>| {
        Ok(CheckpointRow {
            task_id: r.get(0)?,
            attempt: r.get::<_, i64>(1)?.max(0) as u32,
            seq: r.get::<_, i64>(2)?.max(0) as u32,
            at: r.get::<_, i64>(3)?.max(0) as u64,
            branch: r.get(4)?,
            sha: r.get(5)?,
            pushed: r.get::<_, i64>(6)? != 0,
            note: r.get(7)?,
            source: r.get(8)?,
            msg_id: r.get::<_, i64>(9)?.max(0) as u64,
        })
    };
    if let Some(att) = attempt {
        conn.query_row(sql, params![task_id, i64::from(att)], mapper)
            .optional()
            .map_err(|e| e.to_string())
    } else {
        conn.query_row(sql, params![task_id], mapper)
            .optional()
            .map_err(|e| e.to_string())
    }
}

pub fn next_checkpoint_seq(conn: &Connection, task_id: &str, attempt: u32) -> Result<u32, String> {
    let max_seq: Option<i64> = conn
        .query_row(
            "SELECT MAX(seq) FROM CHECKPOINT WHERE task_id = ?1 AND attempt = ?2",
            params![task_id, i64::from(attempt)],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(max_seq.map_or(1, |s| (s + 1).max(1) as u32))
}

/// Updates `LEASE.heartbeat_at` (and recovers `suspect` -> `active`) without writing to `bus.jsonl` (section 4.2).
pub fn touch_lease_heartbeat(
    conn: &Connection,
    owner: &str,
    task_id: Option<&str>,
    now_sec: u64,
) -> Result<usize, String> {
    let n = if let Some(tid) = task_id {
        conn.execute(
            "UPDATE LEASE
             SET heartbeat_at = ?1,
                 state = CASE WHEN state = 'suspect' THEN 'active' ELSE state END
             WHERE owner = ?2 AND task_id = ?3 AND state IN ('active', 'suspect')",
            params![now_sec as i64, owner, tid],
        )
    } else {
        conn.execute(
            "UPDATE LEASE
             SET heartbeat_at = ?1,
                 state = CASE WHEN state = 'suspect' THEN 'active' ELSE state END
             WHERE owner = ?2 AND state IN ('active', 'suspect')",
            params![now_sec as i64, owner],
        )
    }
    .map_err(|e| format!("heartbeat update failed: {e}"))?;
    Ok(n)
}
