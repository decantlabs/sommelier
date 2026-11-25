//! SQL schema and table structures for `METRIC` and `BASELINE` (section 3, step 10b).
//!
//! Tables owned by `regress`:
//! - `METRIC`: `(sha, machine_class, scenario, run_id, name)` -> `value, at, msg_id`
//! - `BASELINE`: `(machine_class, scenario, name)` -> `median, n, from_sha, to_sha, good_shas, msg_id`

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Names of the tables owned by the `regress` module.
pub const REGRESS_TABLES: [&str; 2] = ["METRIC", "BASELINE"];

/// DDL creating `METRIC` and `BASELINE`.
pub const REGRESS_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS METRIC (
    sha TEXT NOT NULL,
    machine_class TEXT NOT NULL,
    scenario TEXT NOT NULL,
    run_id TEXT NOT NULL,
    name TEXT NOT NULL,
    value REAL NOT NULL,
    at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (sha, machine_class, scenario, run_id, name)
);

CREATE TABLE IF NOT EXISTS BASELINE (
    machine_class TEXT NOT NULL,
    scenario TEXT NOT NULL,
    name TEXT NOT NULL,
    median REAL NOT NULL DEFAULT 0.0,
    n INTEGER NOT NULL DEFAULT 0,
    from_sha TEXT NOT NULL DEFAULT '',
    to_sha TEXT NOT NULL DEFAULT '',
    good_shas TEXT NOT NULL DEFAULT '[]',
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (machine_class, scenario, name)
);
";

/// A single row in the `METRIC` table (section 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricRow {
    pub sha: String,
    pub machine_class: String,
    pub scenario: String,
    pub run_id: String,
    pub name: String,
    pub value: f64,
    pub at: u64,
    #[serde(default)]
    pub msg_id: u64,
}

/// A single row in the `BASELINE` table (section 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BaselineRow {
    pub machine_class: String,
    pub scenario: String,
    pub name: String,
    pub median: f64,
    pub n: usize,
    pub from_sha: String,
    pub to_sha: String,
    /// Ordered list of the last `baseline_commits` good commit SHAs and their per-commit medians.
    #[serde(default)]
    pub good_commits: Vec<GoodCommit>,
    #[serde(default)]
    pub msg_id: u64,
}

/// One good commit entry tracked in `BASELINE` for sliding window median and MAD computation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoodCommit {
    pub sha: String,
    pub median: f64,
}

/// Inserts or updates a `METRIC` row inside an open transaction.
pub fn upsert_metric_tx(tx: &Transaction<'_>, row: &MetricRow) -> Result<(), String> {
    tx.execute(
        "INSERT INTO METRIC (sha, machine_class, scenario, run_id, name, value, at, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(sha, machine_class, scenario, run_id, name) DO UPDATE SET
            value = excluded.value,
            at = excluded.at,
            msg_id = excluded.msg_id",
        params![
            row.sha,
            row.machine_class,
            row.scenario,
            row.run_id,
            row.name,
            row.value,
            row.at as i64,
            row.msg_id as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Inserts or updates a `METRIC` row on a connection.
pub fn upsert_metric(conn: &Connection, row: &MetricRow) -> Result<(), String> {
    conn.execute(
        "INSERT INTO METRIC (sha, machine_class, scenario, run_id, name, value, at, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(sha, machine_class, scenario, run_id, name) DO UPDATE SET
            value = excluded.value,
            at = excluded.at,
            msg_id = excluded.msg_id",
        params![
            row.sha,
            row.machine_class,
            row.scenario,
            row.run_id,
            row.name,
            row.value,
            row.at as i64,
            row.msg_id as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Inserts or updates a `BASELINE` row inside an open transaction.
pub fn upsert_baseline_tx(tx: &Transaction<'_>, row: &BaselineRow) -> Result<(), String> {
    let good_json = serde_json::to_string(&row.good_commits).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "INSERT INTO BASELINE (machine_class, scenario, name, median, n, from_sha, to_sha, good_shas, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(machine_class, scenario, name) DO UPDATE SET
            median = excluded.median,
            n = excluded.n,
            from_sha = excluded.from_sha,
            to_sha = excluded.to_sha,
            good_shas = excluded.good_shas,
            msg_id = excluded.msg_id",
        params![
            row.machine_class,
            row.scenario,
            row.name,
            row.median,
            row.n as i64,
            row.from_sha,
            row.to_sha,
            good_json,
            row.msg_id as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Inserts or updates a `BASELINE` row on a connection.
pub fn upsert_baseline(conn: &Connection, row: &BaselineRow) -> Result<(), String> {
    let good_json = serde_json::to_string(&row.good_commits).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO BASELINE (machine_class, scenario, name, median, n, from_sha, to_sha, good_shas, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(machine_class, scenario, name) DO UPDATE SET
            median = excluded.median,
            n = excluded.n,
            from_sha = excluded.from_sha,
            to_sha = excluded.to_sha,
            good_shas = excluded.good_shas,
            msg_id = excluded.msg_id",
        params![
            row.machine_class,
            row.scenario,
            row.name,
            row.median,
            row.n as i64,
            row.from_sha,
            row.to_sha,
            good_json,
            row.msg_id as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Queries all `METRIC` rows ordered deterministically by key.
pub fn query_metrics(conn: &Connection) -> Result<Vec<MetricRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT sha, machine_class, scenario, run_id, name, value, at, msg_id
             FROM METRIC
             ORDER BY at ASC, sha ASC, machine_class ASC, scenario ASC, run_id ASC, name ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MetricRow {
                sha: r.get(0)?,
                machine_class: r.get(1)?,
                scenario: r.get(2)?,
                run_id: r.get(3)?,
                name: r.get(4)?,
                value: r.get(5)?,
                at: r.get::<_, i64>(6)?.max(0) as u64,
                msg_id: r.get::<_, i64>(7)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Queries `METRIC` rows for a specific `(sha, machine_class, scenario, name)`.
pub fn query_metric_values(
    conn: &Connection,
    sha: &str,
    machine_class: &str,
    scenario: &str,
    name: &str,
) -> Result<Vec<f64>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT value FROM METRIC
             WHERE sha = ?1 AND machine_class = ?2 AND scenario = ?3 AND name = ?4
             ORDER BY run_id ASC",
        )
        .map_err(|e| e.to_string())?;
    let vals = stmt
        .query_map(params![sha, machine_class, scenario, name], |r| {
            r.get::<_, f64>(0)
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(vals)
}

/// Queries a single `BASELINE` row for `(machine_class, scenario, name)`.
pub fn query_baseline(
    conn: &Connection,
    machine_class: &str,
    scenario: &str,
    name: &str,
) -> Result<Option<BaselineRow>, String> {
    conn.query_row(
        "SELECT machine_class, scenario, name, median, n, from_sha, to_sha, good_shas, msg_id
         FROM BASELINE
         WHERE machine_class = ?1 AND scenario = ?2 AND name = ?3",
        params![machine_class, scenario, name],
        |r| {
            let good_s: String = r.get(7)?;
            let good_commits: Vec<GoodCommit> = serde_json::from_str(&good_s).unwrap_or_default();
            Ok(BaselineRow {
                machine_class: r.get(0)?,
                scenario: r.get(1)?,
                name: r.get(2)?,
                median: r.get(3)?,
                n: r.get::<_, i64>(4)?.max(0) as usize,
                from_sha: r.get(5)?,
                to_sha: r.get(6)?,
                good_commits,
                msg_id: r.get::<_, i64>(8)?.max(0) as u64,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// Queries all `BASELINE` rows ordered by key.
pub fn query_baselines(conn: &Connection) -> Result<Vec<BaselineRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT machine_class, scenario, name, median, n, from_sha, to_sha, good_shas, msg_id
             FROM BASELINE
             ORDER BY machine_class ASC, scenario ASC, name ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let good_s: String = r.get(7)?;
            let good_commits: Vec<GoodCommit> = serde_json::from_str(&good_s).unwrap_or_default();
            Ok(BaselineRow {
                machine_class: r.get(0)?,
                scenario: r.get(1)?,
                name: r.get(2)?,
                median: r.get(3)?,
                n: r.get::<_, i64>(4)?.max(0) as usize,
                from_sha: r.get(5)?,
                to_sha: r.get(6)?,
                good_commits,
                msg_id: r.get::<_, i64>(8)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Converts a `MetricRow` to a JSON object for `/api/state` or FACT payloads.
pub fn metric_to_json(row: &MetricRow) -> Value {
    json!({
        "v": 1,
        "sha": row.sha,
        "machine_class": row.machine_class,
        "scenario": row.scenario,
        "run_id": row.run_id,
        "name": row.name,
        "value": row.value,
        "at": row.at,
        "msg_id": row.msg_id,
    })
}

/// Converts a `BaselineRow` to a JSON object for `/api/state`.
pub fn baseline_to_json(row: &BaselineRow) -> Value {
    json!({
        "machine_class": row.machine_class,
        "scenario": row.scenario,
        "name": row.name,
        "median": row.median,
        "n": row.n,
        "from_sha": row.from_sha,
        "to_sha": row.to_sha,
        "good_commits": row.good_commits,
        "msg_id": row.msg_id,
    })
}
