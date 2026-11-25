//! SQLite schema, row types, and query/upsert helpers for `LANDING`, `BATCH`, `BRANCH`, and `RUN` (section 3, step 10a).

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

/// Names of the four landing/run tables owned by `LandModule` (section 3, 10.3).
pub const LAND_TABLES: [&str; 4] = ["LANDING", "BATCH", "BRANCH", "RUN"];

/// SQLite DDL for `LANDING`, `BATCH`, `BRANCH`, and `RUN` in `state.db`.
pub const LAND_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS BRANCH (
    remote TEXT NOT NULL,
    ref_name TEXT NOT NULL,
    head_sha TEXT NOT NULL DEFAULT '',
    ahead INTEGER NOT NULL DEFAULT 0,
    behind INTEGER NOT NULL DEFAULT 0,
    last_commit_at INTEGER NOT NULL DEFAULT 0,
    task_id TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'active',
    seen_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (remote, ref_name)
);

CREATE TABLE IF NOT EXISTS RUN (
    run_id TEXT PRIMARY KEY,
    machine TEXT NOT NULL DEFAULT '',
    sha TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'check',
    task_id TEXT NOT NULL DEFAULT '',
    requested_by TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL DEFAULT 0,
    ended_at INTEGER NOT NULL DEFAULT 0,
    outcome TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT '',
    registered INTEGER NOT NULL DEFAULT 1,
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS LANDING (
    ref_name TEXT NOT NULL,
    sha TEXT NOT NULL,
    task_id TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'queued',
    batch_id TEXT NOT NULL DEFAULT '',
    tries INTEGER NOT NULL DEFAULT 0,
    fail_step TEXT NOT NULL DEFAULT '',
    fail_class TEXT NOT NULL DEFAULT '',
    log TEXT NOT NULL DEFAULT '',
    approved_by TEXT NOT NULL DEFAULT '',
    priority TEXT NOT NULL DEFAULT 'normal',
    enqueued_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (ref_name, sha)
);

CREATE TABLE IF NOT EXISTS BATCH (
    batch_id TEXT PRIMARY KEY,
    base_sha TEXT NOT NULL DEFAULT '',
    members TEXT NOT NULL DEFAULT '[]',
    parent_batch TEXT NOT NULL DEFAULT '',
    verifications INTEGER NOT NULL DEFAULT 0,
    result TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL DEFAULT 0,
    ended_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);
";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BranchRow {
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub head_sha: String,
    pub ahead: u64,
    pub behind: u64,
    pub last_commit_at: u64,
    pub task_id: String,
    pub state: String,
    pub seen_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunRow {
    pub run_id: String,
    pub machine: String,
    pub sha: String,
    pub label: String,
    pub kind: String,
    pub task_id: String,
    pub requested_by: String,
    pub started_at: u64,
    pub ended_at: u64,
    pub outcome: String,
    pub folder: String,
    pub registered: bool,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LandingRow {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub task_id: String,
    pub state: String,
    pub batch_id: String,
    pub tries: u32,
    pub fail_step: String,
    pub fail_class: String,
    pub log: String,
    pub approved_by: String,
    pub priority: String,
    pub enqueued_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchMember {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchRow {
    pub batch_id: String,
    pub base_sha: String,
    pub members: Vec<BatchMember>,
    pub parent_batch: String,
    pub verifications: u32,
    pub result: String,
    pub started_at: u64,
    pub ended_at: u64,
    pub msg_id: u64,
}

pub fn upsert_branch_tx(tx: &Transaction<'_>, row: &BranchRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO BRANCH (
            remote, ref_name, head_sha, ahead, behind, last_commit_at,
            task_id, state, seen_at, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(remote, ref_name) DO UPDATE SET
            head_sha = excluded.head_sha,
            ahead = excluded.ahead,
            behind = excluded.behind,
            last_commit_at = excluded.last_commit_at,
            task_id = excluded.task_id,
            state = excluded.state,
            seen_at = excluded.seen_at,
            msg_id = excluded.msg_id",
        params![
            row.remote,
            row.ref_name,
            row.head_sha,
            row.ahead as i64,
            row.behind as i64,
            row.last_commit_at as i64,
            row.task_id,
            row.state,
            row.seen_at as i64,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert BRANCH error: {e}"))?;
    Ok(())
}

pub fn upsert_run_tx(tx: &Transaction<'_>, row: &RunRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO RUN (
            run_id, machine, sha, label, kind, task_id, requested_by,
            started_at, ended_at, outcome, folder, registered, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(run_id) DO UPDATE SET
            machine = excluded.machine,
            sha = excluded.sha,
            label = excluded.label,
            kind = excluded.kind,
            task_id = excluded.task_id,
            requested_by = excluded.requested_by,
            started_at = excluded.started_at,
            ended_at = excluded.ended_at,
            outcome = excluded.outcome,
            folder = excluded.folder,
            registered = excluded.registered,
            msg_id = excluded.msg_id",
        params![
            row.run_id,
            row.machine,
            row.sha,
            row.label,
            row.kind,
            row.task_id,
            row.requested_by,
            row.started_at as i64,
            row.ended_at as i64,
            row.outcome,
            row.folder,
            i64::from(row.registered),
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert RUN error: {e}"))?;
    Ok(())
}

pub fn upsert_landing_tx(tx: &Transaction<'_>, row: &LandingRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO LANDING (
            ref_name, sha, task_id, state, batch_id, tries,
            fail_step, fail_class, log, approved_by, priority, enqueued_at, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(ref_name, sha) DO UPDATE SET
            task_id = excluded.task_id,
            state = excluded.state,
            batch_id = excluded.batch_id,
            tries = excluded.tries,
            fail_step = excluded.fail_step,
            fail_class = excluded.fail_class,
            log = excluded.log,
            approved_by = excluded.approved_by,
            priority = excluded.priority,
            enqueued_at = excluded.enqueued_at,
            msg_id = excluded.msg_id",
        params![
            row.ref_name,
            row.sha,
            row.task_id,
            row.state,
            row.batch_id,
            i64::from(row.tries),
            row.fail_step,
            row.fail_class,
            row.log,
            row.approved_by,
            row.priority,
            row.enqueued_at as i64,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert LANDING error: {e}"))?;
    Ok(())
}

/// Inserts a `LANDING` row only if `(ref_name, sha)` is not already present (queue unique by ref and sha).
pub fn insert_landing_if_absent_tx(tx: &Transaction<'_>, row: &LandingRow) -> Result<bool, String> {
    let changed = tx
        .execute(
            "INSERT INTO LANDING (
                ref_name, sha, task_id, state, batch_id, tries,
                fail_step, fail_class, log, approved_by, priority, enqueued_at, msg_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(ref_name, sha) DO NOTHING",
            params![
                row.ref_name,
                row.sha,
                row.task_id,
                row.state,
                row.batch_id,
                i64::from(row.tries),
                row.fail_step,
                row.fail_class,
                row.log,
                row.approved_by,
                row.priority,
                row.enqueued_at as i64,
                row.msg_id as i64,
            ],
        )
        .map_err(|e| format!("insert LANDING error: {e}"))?;
    Ok(changed > 0)
}

pub fn upsert_batch_tx(tx: &Transaction<'_>, row: &BatchRow) -> Result<(), String> {
    let members_json = serde_json::to_string(&row.members).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO BATCH (
            batch_id, base_sha, members, parent_batch, verifications,
            result, started_at, ended_at, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(batch_id) DO UPDATE SET
            base_sha = excluded.base_sha,
            members = excluded.members,
            parent_batch = excluded.parent_batch,
            verifications = excluded.verifications,
            result = excluded.result,
            started_at = excluded.started_at,
            ended_at = excluded.ended_at,
            msg_id = excluded.msg_id",
        params![
            row.batch_id,
            row.base_sha,
            members_json,
            row.parent_batch,
            i64::from(row.verifications),
            row.result,
            row.started_at as i64,
            row.ended_at as i64,
            row.msg_id as i64,
        ],
    )
    .map_err(|e| format!("upsert BATCH error: {e}"))?;
    Ok(())
}

fn row_to_branch(r: &rusqlite::Row<'_>) -> rusqlite::Result<BranchRow> {
    Ok(BranchRow {
        remote: r.get(0)?,
        ref_name: r.get(1)?,
        head_sha: r.get(2)?,
        ahead: r.get::<_, i64>(3)?.max(0) as u64,
        behind: r.get::<_, i64>(4)?.max(0) as u64,
        last_commit_at: r.get::<_, i64>(5)?.max(0) as u64,
        task_id: r.get(6)?,
        state: r.get(7)?,
        seen_at: r.get::<_, i64>(8)?.max(0) as u64,
        msg_id: r.get::<_, i64>(9)?.max(0) as u64,
    })
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: r.get(0)?,
        machine: r.get(1)?,
        sha: r.get(2)?,
        label: r.get(3)?,
        kind: r.get(4)?,
        task_id: r.get(5)?,
        requested_by: r.get(6)?,
        started_at: r.get::<_, i64>(7)?.max(0) as u64,
        ended_at: r.get::<_, i64>(8)?.max(0) as u64,
        outcome: r.get(9)?,
        folder: r.get(10)?,
        registered: r.get::<_, i64>(11)? != 0,
        msg_id: r.get::<_, i64>(12)?.max(0) as u64,
    })
}

fn row_to_landing(r: &rusqlite::Row<'_>) -> rusqlite::Result<LandingRow> {
    Ok(LandingRow {
        ref_name: r.get(0)?,
        sha: r.get(1)?,
        task_id: r.get(2)?,
        state: r.get(3)?,
        batch_id: r.get(4)?,
        tries: r.get::<_, i64>(5)?.max(0) as u32,
        fail_step: r.get(6)?,
        fail_class: r.get(7)?,
        log: r.get(8)?,
        approved_by: r.get(9)?,
        priority: r.get(10)?,
        enqueued_at: r.get::<_, i64>(11)?.max(0) as u64,
        msg_id: r.get::<_, i64>(12)?.max(0) as u64,
    })
}

fn row_to_batch(r: &rusqlite::Row<'_>) -> rusqlite::Result<BatchRow> {
    let members_raw: String = r.get(2)?;
    let members = serde_json::from_str::<Vec<BatchMember>>(&members_raw).unwrap_or_default();
    Ok(BatchRow {
        batch_id: r.get(0)?,
        base_sha: r.get(1)?,
        members,
        parent_batch: r.get(3)?,
        verifications: r.get::<_, i64>(4)?.max(0) as u32,
        result: r.get(5)?,
        started_at: r.get::<_, i64>(6)?.max(0) as u64,
        ended_at: r.get::<_, i64>(7)?.max(0) as u64,
        msg_id: r.get::<_, i64>(8)?.max(0) as u64,
    })
}

pub fn query_branches(conn: &Connection) -> Result<Vec<BranchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT remote, ref_name, head_sha, ahead, behind, last_commit_at,
                    task_id, state, seen_at, msg_id
             FROM BRANCH ORDER BY remote, ref_name",
        )
        .map_err(|e| format!("query BRANCH error: {e}"))?;
    let rows = stmt
        .query_map([], row_to_branch)
        .map_err(|e| format!("query BRANCH map error: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("query BRANCH row error: {e}"))
}

pub fn query_branch(
    conn: &Connection,
    remote: &str,
    ref_name: &str,
) -> Result<Option<BranchRow>, String> {
    conn.query_row(
        "SELECT remote, ref_name, head_sha, ahead, behind, last_commit_at,
                task_id, state, seen_at, msg_id
         FROM BRANCH WHERE remote = ?1 AND ref_name = ?2",
        params![remote, ref_name],
        row_to_branch,
    )
    .optional()
    .map_err(|e| format!("query BRANCH error: {e}"))
}

pub fn query_runs(conn: &Connection) -> Result<Vec<RunRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT run_id, machine, sha, label, kind, task_id, requested_by,
                    started_at, ended_at, outcome, folder, registered, msg_id
             FROM RUN ORDER BY started_at DESC, run_id DESC",
        )
        .map_err(|e| format!("query RUN error: {e}"))?;
    let rows = stmt
        .query_map([], row_to_run)
        .map_err(|e| format!("query RUN map error: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("query RUN row error: {e}"))
}

pub fn query_run(conn: &Connection, run_id: &str) -> Result<Option<RunRow>, String> {
    conn.query_row(
        "SELECT run_id, machine, sha, label, kind, task_id, requested_by,
                started_at, ended_at, outcome, folder, registered, msg_id
         FROM RUN WHERE run_id = ?1",
        params![run_id],
        row_to_run,
    )
    .optional()
    .map_err(|e| format!("query RUN error: {e}"))
}

pub fn query_landings(conn: &Connection) -> Result<Vec<LandingRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ref_name, sha, task_id, state, batch_id, tries,
                    fail_step, fail_class, log, approved_by, priority, enqueued_at, msg_id
             FROM LANDING ORDER BY enqueued_at ASC, ref_name ASC, sha ASC",
        )
        .map_err(|e| format!("query LANDING error: {e}"))?;
    let rows = stmt
        .query_map([], row_to_landing)
        .map_err(|e| format!("query LANDING map error: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("query LANDING row error: {e}"))
}

pub fn query_landing(
    conn: &Connection,
    ref_name: &str,
    sha: &str,
) -> Result<Option<LandingRow>, String> {
    conn.query_row(
        "SELECT ref_name, sha, task_id, state, batch_id, tries,
                fail_step, fail_class, log, approved_by, priority, enqueued_at, msg_id
         FROM LANDING WHERE ref_name = ?1 AND sha = ?2",
        params![ref_name, sha],
        row_to_landing,
    )
    .optional()
    .map_err(|e| format!("query LANDING error: {e}"))
}

pub fn query_batches(conn: &Connection) -> Result<Vec<BatchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT batch_id, base_sha, members, parent_batch, verifications,
                    result, started_at, ended_at, msg_id
             FROM BATCH ORDER BY started_at DESC, batch_id DESC",
        )
        .map_err(|e| format!("query BATCH error: {e}"))?;
    let rows = stmt
        .query_map([], row_to_batch)
        .map_err(|e| format!("query BATCH map error: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("query BATCH row error: {e}"))
}

pub fn query_batch(conn: &Connection, batch_id: &str) -> Result<Option<BatchRow>, String> {
    conn.query_row(
        "SELECT batch_id, base_sha, members, parent_batch, verifications,
                result, started_at, ended_at, msg_id
         FROM BATCH WHERE batch_id = ?1",
        params![batch_id],
        row_to_batch,
    )
    .optional()
    .map_err(|e| format!("query BATCH error: {e}"))
}
