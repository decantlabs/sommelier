//! Role leases, epochs, fencing, and singleton election (steps 7 and 11).
//!
//! Owns the `ROLE` table schema, state projection, lease fencing, and the deterministic
//! election algorithm of section 13.3.

pub mod elect;
pub mod eligible;
pub mod lease;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

pub use elect::{RoleAction, RoleCandidate, RolePolicy, RoleTable};
pub use lease::{FenceRefusal, LeaseScope, RoleCommand, RoleRow, RoleState, check_fence};

use crate::msg::Message;

/// SQL schema for the `ROLE` table in `state.db` (section 3 and 10.2).
pub const ROLE_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS ROLE (
    role TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'global',
    holder TEXT,
    lease_scope TEXT NOT NULL DEFAULT 'full',
    epoch INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'vacant',
    granted_at INTEGER NOT NULL DEFAULT 0,
    heartbeat_at INTEGER NOT NULL DEFAULT 0,
    min_hold_until INTEGER NOT NULL DEFAULT 0,
    last_reason TEXT NOT NULL DEFAULT '',
    changes_last_hour INTEGER NOT NULL DEFAULT 0,
    frozen INTEGER NOT NULL DEFAULT 0,
    handover_note TEXT,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (role, scope)
);";

/// Creates the `ROLE` table in a SQLite connection (section 10.2 `schema()`).
pub fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(ROLE_SCHEMA_SQL)
}

/// Persists a `RoleRow` into the `ROLE` SQLite table.
pub fn upsert_row(conn: &Connection, row: &RoleRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ROLE (
            role, scope, holder, lease_scope, epoch, state,
            granted_at, heartbeat_at, min_hold_until, last_reason,
            changes_last_hour, frozen, handover_note, msg_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(role, scope) DO UPDATE SET
            holder = excluded.holder,
            lease_scope = excluded.lease_scope,
            epoch = excluded.epoch,
            state = excluded.state,
            granted_at = excluded.granted_at,
            heartbeat_at = excluded.heartbeat_at,
            min_hold_until = excluded.min_hold_until,
            last_reason = excluded.last_reason,
            changes_last_hour = excluded.changes_last_hour,
            frozen = excluded.frozen,
            handover_note = excluded.handover_note,
            msg_id = excluded.msg_id",
        params![
            row.role,
            row.scope,
            row.holder,
            row.lease_scope.as_str(),
            row.epoch as i64,
            row.state.as_str(),
            row.granted_at as i64,
            row.heartbeat_at as i64,
            row.min_hold_until as i64,
            row.last_reason,
            row.changes_last_hour as i64,
            i64::from(row.frozen),
            row.handover_note,
            row.msg_id as i64,
        ],
    )?;
    Ok(())
}

/// Applies a bus message to the `ROLE` SQLite table if it is a `FACT` on `role/<name>` (section 10.2 `apply()`).
pub fn apply(conn: &Connection, msg: &Message) -> rusqlite::Result<()> {
    let mut table = RoleTable::new();
    table.apply_message(msg);
    for row in table.rows.values() {
        upsert_row(conn, row)?;
    }
    Ok(())
}

/// Queries a single `RoleRow` from SQLite.
pub fn query_row(conn: &Connection, role: &str, scope: &str) -> rusqlite::Result<Option<RoleRow>> {
    conn.query_row(
        "SELECT role, scope, holder, lease_scope, epoch, state,
                granted_at, heartbeat_at, min_hold_until, last_reason,
                changes_last_hour, frozen, handover_note, msg_id
         FROM ROLE WHERE role = ?1 AND scope = ?2",
        params![role, scope],
        map_sql_row,
    )
    .optional()
}

/// Queries all `RoleRow` entries from SQLite ordered by `(role, scope)`.
pub fn query_all(conn: &Connection) -> rusqlite::Result<Vec<RoleRow>> {
    let mut stmt = conn.prepare(
        "SELECT role, scope, holder, lease_scope, epoch, state,
                granted_at, heartbeat_at, min_hold_until, last_reason,
                changes_last_hour, frozen, handover_note, msg_id
         FROM ROLE ORDER BY role ASC, scope ASC",
    )?;
    let rows = stmt
        .query_map([], map_sql_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn map_sql_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RoleRow> {
    let ls_str: String = r.get(3)?;
    let st_str: String = r.get(5)?;
    let frozen_int: i64 = r.get(11)?;
    Ok(RoleRow {
        role: r.get(0)?,
        scope: r.get(1)?,
        holder: r.get(2)?,
        lease_scope: LeaseScope::parse(&ls_str).unwrap_or(LeaseScope::Full),
        epoch: r.get::<_, i64>(4)? as u64,
        state: match st_str.as_str() {
            "held" => RoleState::Held,
            "handing_over" => RoleState::HandingOver,
            _ => RoleState::Vacant,
        },
        granted_at: r.get::<_, i64>(6)? as u64,
        heartbeat_at: r.get::<_, i64>(7)? as u64,
        min_hold_until: r.get::<_, i64>(8)? as u64,
        last_reason: r.get(9)?,
        changes_last_hour: r.get::<_, i64>(10)? as u32,
        frozen: frozen_int != 0,
        handover_note: r.get(12)?,
        msg_id: r.get::<_, i64>(13)? as u64,
    })
}

/// Populates the `"roles"` section of `/api/state` (section 10.2 `api()`).
pub fn api(table: &RoleTable, state: &mut Map<String, Value>) {
    let rows: Vec<Value> = table
        .list()
        .into_iter()
        .map(|r| serde_json::to_value(r).unwrap_or_else(|_| json!({})))
        .collect();
    state.insert("roles".into(), Value::Array(rows));
}

/// Persistent file storage for `RoleTable` at `<data_dir>/roles.json` (and synchronized with
/// `<data_dir>/state.db` when present), used by `sommelier role` CLI commands.
pub struct RoleStore {
    data_dir: PathBuf,
}

impl RoleStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn json_path(&self) -> PathBuf {
        self.data_dir.join("roles.json")
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    pub fn load(&self) -> io::Result<RoleTable> {
        let mut table = match fs::read_to_string(self.json_path()) {
            Ok(s) => serde_json::from_str::<RoleTable>(&s).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => RoleTable::new(),
            Err(e) => return Err(e),
        };
        if let Ok(conn) = Connection::open(self.db_path())
            && schema(&conn).is_ok()
            && let Ok(sql_rows) = query_all(&conn)
        {
            for r in sql_rows {
                table.rows.insert((r.role.clone(), r.scope.clone()), r);
            }
        }
        Ok(table)
    }

    pub fn save(&self, table: &RoleTable) -> io::Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        let path = self.json_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.data_dir)?;
        let text = serde_json::to_string_pretty(table).map_err(io::Error::other)?;
        use std::io::Write;
        temp.write_all(text.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;

        if let Ok(conn) = Connection::open(self.db_path())
            && schema(&conn).is_ok()
        {
            for r in table.rows.values() {
                let _ = upsert_row(&conn, r);
            }
        }
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}
