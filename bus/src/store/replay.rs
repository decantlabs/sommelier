//! Deterministic table hashing, `state.db` snapshots, and `replay [--verify]` (step 4).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::msg::Message;

use super::Store;

/// Summary of a replay operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
    pub messages_applied: usize,
    pub cursor: u64,
    pub verified: bool,
    pub state_hash: String,
    pub table_hashes: BTreeMap<String, String>,
}

/// Summary of a snapshot operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotReport {
    pub snapshot_id: String,
    pub cursor: u64,
    pub schema_version: u64,
    pub path: String,
    pub state_hash: String,
    pub table_hashes: BTreeMap<String, String>,
}

/// Lists all user tables in the SQLite database in alphabetical order.
pub fn list_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Computes a deterministic SHA-256 hex digest of every row and column in `table`.
///
/// For `META`, `snapshot_id` is volatile across snapshot creations, so `table_hash` hashes
/// all rows in `META` except `snapshot_id` (or hashes all rows for every other table),
/// ordered deterministically by all columns.
pub fn table_hash(conn: &Connection, table: &str) -> Result<String, String> {
    let pragma = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
    let mut col_stmt = conn.prepare(&pragma).map_err(|e| e.to_string())?;
    let cols: Vec<String> = col_stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    if cols.is_empty() {
        return Ok(format!("{:x}", Sha256::digest(b"empty")));
    }

    let quoted_cols: Vec<String> = cols
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect();
    let select_list = quoted_cols.join(", ");
    let order_list = (1..=cols.len())
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let where_clause = if table == "META" {
        " WHERE name != 'snapshot_id'"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {select_list} FROM \"{}\"{where_clause} ORDER BY {order_list}",
        table.replace('"', "\"\"")
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(table.as_bytes());
    hasher.update(b"\0");
    for c in &cols {
        hasher.update(c.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\n");

    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        for i in 0..cols.len() {
            let val = row.get_ref(i).map_err(|e| e.to_string())?;
            match val {
                ValueRef::Null => hasher.update(b"N:"),
                ValueRef::Integer(n) => {
                    hasher.update(b"I:");
                    hasher.update(n.to_string().as_bytes());
                }
                ValueRef::Real(f) => {
                    hasher.update(b"F:");
                    hasher.update(format!("{f:.12}").as_bytes());
                }
                ValueRef::Text(t) => {
                    hasher.update(b"T:");
                    hasher.update((t.len() as u64).to_le_bytes());
                    hasher.update(t);
                }
                ValueRef::Blob(b) => {
                    hasher.update(b"B:");
                    hasher.update((b.len() as u64).to_le_bytes());
                    hasher.update(b);
                }
            }
            hasher.update(b"\x1f");
        }
        hasher.update(b"\n");
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Computes the per-table SHA-256 hashes for all tables in `conn`.
pub fn all_table_hashes(conn: &Connection) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for table in list_tables(conn)? {
        map.insert(table.clone(), table_hash(conn, &table)?);
    }
    Ok(map)
}

/// Computes a single combined SHA-256 hash over a `BTreeMap` of table hashes.
pub fn combined_state_hash(table_hashes: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (table, hash) in table_hashes {
        hasher.update(table.as_bytes());
        hasher.update(b"=");
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// Parses valid `Message` records from a `bus.jsonl` file in line order.
/// Blank lines and malformed trailing lines (e.g. from a truncated crash write) are skipped
/// unless `strict` is set.
pub fn read_log_messages(log_path: &Path) -> Result<Vec<Message>, String> {
    let f = match File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot open {}: {e}", log_path.display())),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| format!("reading {}: {e}", log_path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
            out.push(msg);
        }
    }
    Ok(out)
}

impl Store {
    /// Returns the SHA-256 hash of every table in `state.db`.
    pub fn table_hashes(&self) -> Result<BTreeMap<String, String>, String> {
        all_table_hashes(&self.conn)
    }

    /// Returns the combined SHA-256 hash over all tables in `state.db`.
    pub fn state_hash(&self) -> Result<String, String> {
        Ok(combined_state_hash(&self.table_hashes()?))
    }

    /// Replays all messages from `log_path` whose `id > cursor` into this store.
    /// Returns the number of newly applied messages and the resulting cursor.
    pub fn catch_up_from_log(&mut self, log_path: &Path) -> Result<(usize, u64), String> {
        let msgs = read_log_messages(log_path)?;
        let start_cursor = self.cursor()?;
        let pending: Vec<Message> = msgs.into_iter().filter(|m| m.id > start_cursor).collect();
        let count = pending.len();
        self.apply_batch(&pending)?;
        let end_cursor = self.cursor()?;
        Ok((count, end_cursor))
    }

    /// Rebuilds `state.db` from scratch from `log_path` (resetting all tables first).
    pub fn rebuild_from_log(&mut self, log_path: &Path) -> Result<ReplayReport, String> {
        self.reset_all_tables()?;
        let msgs = read_log_messages(log_path)?;
        let count = msgs.len();
        self.apply_batch(&msgs)?;
        let cursor = self.cursor()?;
        let table_hashes = self.table_hashes()?;
        let state_hash = combined_state_hash(&table_hashes);
        Ok(ReplayReport {
            messages_applied: count,
            cursor,
            verified: false,
            state_hash,
            table_hashes,
        })
    }

    /// Verifies `state.db` against `log_path` (`sommelier replay --verify`):
    /// 1. Catches up any unapplied tail from `log_path` into `state.db` (or if `state.db` was empty, builds it).
    /// 2. Replays `log_path` from scratch into a fresh in-memory store with the same module registry.
    /// 3. Compares the cursor and every table hash between `state.db` and the fresh replay.
    pub fn replay_verify(&mut self, log_path: &Path) -> Result<ReplayReport, String> {
        let msgs = read_log_messages(log_path)?;
        let current_cursor = self.cursor()?;
        let max_id = msgs.iter().map(|m| m.id).max().unwrap_or(0);
        if current_cursor == 0 && max_id > 0 {
            self.reset_all_tables()?;
            self.apply_batch(&msgs)?;
        } else if current_cursor < max_id {
            let tail: Vec<Message> = msgs
                .iter()
                .filter(|m| m.id > current_cursor)
                .cloned()
                .collect();
            self.apply_batch(&tail)?;
        }

        let live_cursor = self.cursor()?;
        let live_hashes = self.table_hashes()?;

        let mut fresh = Store::open_in_memory()?;
        fresh.apply_batch(&msgs)?;
        let fresh_cursor = fresh.cursor()?;
        let fresh_hashes = fresh.table_hashes()?;

        if live_cursor != fresh_cursor {
            return Err(format!(
                "cursor mismatch: state.db has {live_cursor}, clean replay has {fresh_cursor}"
            ));
        }

        for (table, expected_hash) in &fresh_hashes {
            let actual_hash = live_hashes
                .get(table)
                .ok_or_else(|| format!("missing table `{table}` in state.db"))?;
            if actual_hash != expected_hash {
                return Err(format!(
                    "table `{table}` hash mismatch: state.db has {actual_hash}, clean replay has {expected_hash}"
                ));
            }
        }

        let state_hash = combined_state_hash(&live_hashes);
        Ok(ReplayReport {
            messages_applied: msgs.len(),
            cursor: live_cursor,
            verified: true,
            state_hash,
            table_hashes: live_hashes,
        })
    }

    /// Writes a consistent snapshot of `state.db` and records `snapshot_id` in `META`.
    /// Also writes a JSON snapshot file (`state.snapshot.json` or `out_path`) containing
    /// the cursor, snapshot id, table hashes, and full `/api/state` projection.
    pub fn write_snapshot(
        &mut self,
        data_dir: &Path,
        out_path: Option<&Path>,
        now_sec: u64,
    ) -> Result<(SnapshotReport, Value), String> {
        let cursor = self.cursor()?;
        let schema_version = self.schema_version()?;
        let table_hashes = self.table_hashes()?;
        let state_hash = combined_state_hash(&table_hashes);
        let short_hash = state_hash.get(..12).unwrap_or(&state_hash);
        let snapshot_id = format!("snap-{cursor}-{short_hash}");
        self.set_meta("snapshot_id", &snapshot_id, cursor)?;

        let snap_path: PathBuf = match out_path {
            Some(p) => p.to_path_buf(),
            None => data_dir.join("state.snapshot.json"),
        };

        // Also create an atomic SQLite backup `state.snapshot.db` in data_dir when writing to data_dir
        let db_snap_path = data_dir.join("state.snapshot.db");
        let _ = std::fs::remove_file(&db_snap_path);
        let escaped = db_snap_path.to_string_lossy().replace('\'', "''");
        let _ = self.conn.execute(&format!("VACUUM INTO '{escaped}'"), []);

        let api_state = self.api_state(now_sec)?;
        let payload = json!({
            "snapshot_id": snapshot_id,
            "cursor": cursor,
            "schema_version": schema_version,
            "as_of": now_sec,
            "state_hash": state_hash,
            "table_hashes": table_hashes,
            "state": api_state,
        });

        if let Some(parent) = snap_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
        std::fs::write(&snap_path, text + "\n")
            .map_err(|e| format!("cannot write snapshot {}: {e}", snap_path.display()))?;

        let report = SnapshotReport {
            snapshot_id,
            cursor,
            schema_version,
            path: snap_path.to_string_lossy().into_owned(),
            state_hash,
            table_hashes,
        };
        Ok((report, payload))
    }
}
