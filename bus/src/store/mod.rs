//! SQLite projection store (`state.db`, step 4).
//!
//! Principles (section 1, 3, 9, 10.2, 10.3):
//! - `bus.jsonl` is the truth; `state.db` beside `bus.jsonl` holds projections that replay rebuilds.
//! - Uses WAL mode and a single writer connection on Windows and Linux.
//! - Every row keeps `msg_id`, the id of the message that last changed it.
//! - `META` stores `cursor` (last applied message id), `schema_version`, and `snapshot_id`.
//! - Every message application updates the module projections and `META.cursor` atomically in one
//!   SQLite transaction, so a crash or kill in the middle of a transaction rolls back and leaves
//!   `cursor` and all tables consistent.
//! - If `state.db` is damaged on open, the store renames it to `state.db.damaged-<epoch>` and
//!   rebuilds from `bus.jsonl`.

pub mod core;
pub mod registry;
pub mod replay;
pub mod schema;

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};

use crate::hub::Hub;
use crate::msg::{Kind, Message, Role};
use crate::time::{now_ms, parse_ms, rfc3339};
use crate::tokens::TokenFile;

pub use core::{
    AgentRow, AlertTableRow, CapacityRow, CoreModule, MachineRow, MemberRow, MetaRow, PolicyRow,
    apply_core_message, build_core_api, query_agents, query_alerts, query_capacities,
    query_machines, query_members, query_meta, query_policies,
};
pub use registry::{ModuleRegistry, StoreModule};
pub use replay::{
    ReplayReport, SnapshotReport, all_table_hashes, combined_state_hash, list_tables,
    read_log_messages, table_hash,
};
pub use schema::{CORE_SCHEMA_SQL, CORE_TABLES, SCHEMA_VERSION};

/// The SQLite projection store owning a single connection to `state.db` (or in-memory for tests/verify).
pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
    registry: ModuleRegistry,
}

impl Store {
    /// Opens or creates `state.db` inside `data_dir` with WAL mode enabled, initializes all
    /// registered module schemas, and seeds `META` (`cursor`, `schema_version`, `snapshot_id`).
    ///
    /// If `state.db` is corrupted/damaged, renames it aside and rebuilds a clean `state.db`
    /// (section 9: "`state.db` is damaged -> The hub renames it and rebuilds from the last snapshot plus the log").
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = data_dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create data dir {}: {e}", dir.display()))?;
        let db_path = dir.join("state.db");
        Self::open_file(&db_path)
    }

    /// Opens `state.db` at an explicit file path, recovering by rename if damaged.
    pub fn open_file(db_path: &Path) -> Result<Self, String> {
        let registry = ModuleRegistry::new();
        match Self::try_open_with_registry(db_path, registry) {
            Ok(mut store) => {
                if let Some(dir) = db_path.parent() {
                    let _ = store.seed_from_data_dir(dir);
                }
                Ok(store)
            }
            Err(_err) if db_path.exists() => {
                let damaged = db_path.with_extension(format!("db.damaged-{}", now_ms() / 1000));
                let _ = std::fs::rename(db_path, &damaged);
                let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
                let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
                let mut store = Self::try_open_with_registry(db_path, ModuleRegistry::new())?;
                if let Some(dir) = db_path.parent() {
                    let _ = store.seed_from_data_dir(dir);
                    let log_path = dir.join("bus.jsonl");
                    if log_path.exists() {
                        let _ = store.catch_up_from_log(&log_path);
                    }
                }
                Ok(store)
            }
            Err(e) => Err(e),
        }
    }

    /// Opens an in-memory `Store` with the default module registry.
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let registry = ModuleRegistry::new();
        let mut store = Self {
            conn,
            path: None,
            registry,
        };
        store.configure_and_init()?;
        Ok(store)
    }

    fn try_open_with_registry(db_path: &Path, registry: ModuleRegistry) -> Result<Self, String> {
        let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
        let mut store = Self {
            conn,
            path: Some(db_path.to_path_buf()),
            registry,
        };
        store.configure_and_init()?;
        Ok(store)
    }

    fn configure_and_init(&mut self) -> Result<(), String> {
        self.conn
            .busy_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        self.conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|e| e.to_string())?;
        // Quick integrity check to detect damaged database headers early
        let integrity: String = self
            .conn
            .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !integrity.eq_ignore_ascii_case("ok") {
            return Err(format!("SQLite integrity check failed: {integrity}"));
        }

        self.registry.init_schemas(&self.conn)?;
        self.ensure_meta_defaults()?;
        Ok(())
    }

    fn ensure_meta_defaults(&self) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO META (name, value, msg_id) VALUES ('cursor', '0', 0)",
                [],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO META (name, value, msg_id) VALUES ('schema_version', ?1, 0)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO META (name, value, msg_id) VALUES ('snapshot_id', '', 0)",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Registers an additional module and initializes its schema in `state.db`.
    pub fn register_module(&mut self, module: Box<dyn StoreModule>) -> Result<(), String> {
        let sql = module.schema().trim();
        if !sql.is_empty() {
            self.conn
                .execute_batch(sql)
                .map_err(|e| format!("module `{}` schema error: {e}", module.name()))?;
        }
        self.registry.register(module);
        Ok(())
    }

    /// Returns a reference to the underlying SQLite connection.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Returns the path to `state.db` if backed by a file.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns the current log cursor (id of the last applied message, or 0 if none).
    pub fn cursor(&self) -> Result<u64, String> {
        let val: Option<String> = self
            .conn
            .query_row("SELECT value FROM META WHERE name = 'cursor'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(val.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0))
    }

    /// Returns the schema version recorded in `META`.
    pub fn schema_version(&self) -> Result<u64, String> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM META WHERE name = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(val
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(SCHEMA_VERSION))
    }

    /// Returns the snapshot id recorded in `META`.
    pub fn snapshot_id(&self) -> Result<String, String> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM META WHERE name = 'snapshot_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(val.unwrap_or_default())
    }

    /// Sets a key in `META` outside of message replay.
    pub fn set_meta(&self, name: &str, value: &str, msg_id: u64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO META (name, value, msg_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value, msg_id = excluded.msg_id",
                params![name, value, msg_id as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Seeds `MEMBER` from `tokens.json` and `POLICY` from `policy/versions` when present in `data_dir`,
    /// without advancing the message cursor (uses `msg_id = 0` for unlogged seed rows).
    pub fn seed_from_data_dir(&mut self, data_dir: &Path) -> Result<(), String> {
        let tokens_path = data_dir.join("tokens.json");
        if tokens_path.exists()
            && let Ok(tf) = TokenFile::load(&tokens_path)
        {
            let tx = self.conn.transaction().map_err(|e| e.to_string())?;
            for m in &tf.members {
                let role_s = m.role.as_str();
                let trust_s = match m.role {
                    Role::Orchestrator | Role::Hub => "owner",
                    Role::Member => "guest",
                    Role::Observer => "observer",
                };
                let created_sec = (parse_ms(&m.created).unwrap_or(0) / 1000) as i64;
                tx.execute(
                    "INSERT INTO MEMBER (name, role, trust, online, last_seen_at, msg_id)
                     VALUES (?1, ?2, ?3, 0, ?4, 0)
                     ON CONFLICT(name) DO UPDATE SET
                        role = excluded.role,
                        trust = excluded.trust",
                    params![m.name, role_s, trust_s, created_sec],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
        }

        let policy_store = crate::policy::Store::new(data_dir);
        if let Ok(versions) = policy_store.versions_after(None)
            && !versions.is_empty()
        {
            let tx = self.conn.transaction().map_err(|e| e.to_string())?;
            for v in versions {
                if let Ok(snap) = policy_store.read_version(v) {
                    let mode_s = snap
                        .summary()
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("off")
                        .to_owned();
                    let pub_sec = snap
                        .published
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    tx.execute(
                        "INSERT OR IGNORE INTO POLICY (version, hash, mode, published_at, msg_id)
                         VALUES (?1, ?2, ?3, ?4, 0)",
                        params![v as i64, snap.hash, mode_s, pub_sec],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Clears all user tables and resets `META` to defaults (`cursor = 0`).
    pub fn reset_all_tables(&mut self) -> Result<(), String> {
        let tables = list_tables(&self.conn)?;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for t in tables {
            let sql = format!("DELETE FROM \"{}\"", t.replace('"', "\"\""));
            tx.execute(&sql, []).map_err(|e| e.to_string())?;
        }
        tx.execute(
            "INSERT INTO META (name, value, msg_id) VALUES ('cursor', '0', 0)
             ON CONFLICT(name) DO UPDATE SET value = '0', msg_id = 0",
            [],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO META (name, value, msg_id) VALUES ('schema_version', ?1, 0)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, msg_id = 0",
            params![SCHEMA_VERSION.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO META (name, value, msg_id) VALUES ('snapshot_id', '', 0)
             ON CONFLICT(name) DO UPDATE SET value = '', msg_id = 0",
            [],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Applies one message atomically in a single transaction, advancing `META.cursor`.
    /// Messages with `msg.id <= cursor` (when `msg.id > 0`) are ignored as already applied.
    pub fn apply(&mut self, msg: &Message) -> Result<bool, String> {
        let current = self.cursor()?;
        if msg.id > 0 && msg.id <= current {
            return Ok(false);
        }
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        Self::apply_in_tx(&self.registry, &tx, msg)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Applies a slice of messages inside a single SQLite transaction.
    /// If any message or callback fails, the entire transaction rolls back so `cursor` and
    /// all tables stay consistent.
    pub fn apply_batch(&mut self, msgs: &[Message]) -> Result<usize, String> {
        let mut current = self.cursor()?;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut applied = 0;
        for msg in msgs {
            if msg.id > 0 && msg.id <= current {
                continue;
            }
            Self::apply_in_tx(&self.registry, &tx, msg)?;
            if msg.id > current {
                current = msg.id;
            }
            applied += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(applied)
    }

    /// Simulates a process kill or crash in the middle of a multi-message transaction after
    /// applying `kill_after` messages to the open transaction, dropping the uncommitted
    /// transaction without committing. Used by tests to verify transaction atomicity and
    /// cursor consistency.
    pub fn apply_batch_fault_after(
        &mut self,
        msgs: &[Message],
        kill_after: usize,
    ) -> Result<(), String> {
        let mut current = self.cursor()?;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for (idx, msg) in msgs.iter().enumerate() {
            if idx >= kill_after {
                drop(tx);
                return Err("simulated kill in the middle of a transaction".into());
            }
            if msg.id > 0 && msg.id <= current {
                continue;
            }
            Self::apply_in_tx(&self.registry, &tx, msg)?;
            if msg.id > current {
                current = msg.id;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn apply_in_tx(
        registry: &ModuleRegistry,
        tx: &Transaction<'_>,
        msg: &Message,
    ) -> Result<(), String> {
        registry.apply_all(tx, msg)?;
        if msg.id > 0 {
            tx.execute(
                "INSERT INTO META (name, value, msg_id)
                 VALUES ('cursor', ?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET
                    value = CASE WHEN CAST(excluded.value AS INTEGER) > CAST(META.value AS INTEGER)
                                 THEN excluded.value ELSE META.value END,
                    msg_id = MAX(META.msg_id, excluded.msg_id)",
                params![msg.id.to_string(), msg.id as i64],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Builds the full `/api/state` JSON object served from `state.db` (and optionally
    /// enriched with recent messages from `bus.jsonl` when `data_dir` is available).
    pub fn api_state(&self, now_sec: u64) -> Result<Value, String> {
        let mut map = Map::new();
        let now_ms = now_sec * 1000;
        let cursor = self.cursor()?;
        map.insert(
            "hub".into(),
            json!({
                "id": cursor,
                "now": rfc3339(now_ms),
                "as_of": now_sec,
            }),
        );
        map.insert("as_of".into(), json!(now_sec));
        self.registry.api_all(&self.conn, now_sec, &mut map)?;

        // Populate top-level dashboard keys (`members`, `agents`, `claims`, `recent`) from the store
        // if not already set by the caller.
        if !map.contains_key("members")
            && let Ok(members) = query_members(&self.conn)
        {
            let machines = query_machines(&self.conn).unwrap_or_default();
            let member_vals: Vec<Value> = members
                .into_iter()
                .map(|m| {
                    let hello =
                        machines
                            .iter()
                            .find(|mach| mach.owner == m.name)
                            .and_then(|mach| {
                                if mach.os.is_empty() && mach.gpu.is_empty() {
                                    None
                                } else {
                                    Some(
                                        format!("os={} gpu={}", mach.os, mach.gpu)
                                            .trim()
                                            .to_owned(),
                                    )
                                }
                            });
                    json!({
                        "name": m.name,
                        "role": m.role,
                        "trust": m.trust,
                        "online": m.online,
                        "last_seen_at": m.last_seen_at,
                        "msg_id": m.msg_id,
                        "hello": hello,
                    })
                })
                .collect();
            map.insert("members".into(), Value::Array(member_vals));
        }

        if !map.contains_key("agents")
            && let Ok(agents) = query_agents(&self.conn)
        {
            let agent_vals: Vec<Value> = agents
                .into_iter()
                .map(|a| {
                    let ts_str = rfc3339(a.as_of * 1000);
                    let stale = now_sec.saturating_sub(a.as_of) > 20 * 60;
                    json!({
                        "owner": a.owner,
                        "agent": a.name,
                        "name": a.name,
                        "machine": a.machine,
                        "class": a.class,
                        "state": a.reported_state,
                        "reported_state": a.reported_state,
                        "model": a.model,
                        "task_id": a.task_id,
                        "branch": a.branch,
                        "pid_alive": a.pid_alive,
                        "text": a.text,
                        "source": a.source,
                        "as_of": a.as_of,
                        "ts": ts_str,
                        "changed": ts_str,
                        "id": a.msg_id,
                        "msg_id": a.msg_id,
                        "stale": stale,
                    })
                })
                .collect();
            map.insert("agents".into(), Value::Array(agent_vals));
        }

        if !map.contains_key("claims") {
            map.insert("claims".into(), json!([]));
        }
        if !map.contains_key("recent") {
            let recent = self
                .path
                .as_deref()
                .and_then(|p| p.parent())
                .map(|dir| dir.join("bus.jsonl"))
                .and_then(|log_p| read_log_messages(&log_p).ok())
                .map(|msgs| {
                    let filtered: Vec<Value> = msgs
                        .into_iter()
                        .filter(|m| m.kind != Kind::Status)
                        .filter_map(|m| serde_json::to_value(m).ok())
                        .collect();
                    let start = filtered.len().saturating_sub(200);
                    filtered[start..].to_vec()
                })
                .unwrap_or_default();
            map.insert("recent".into(), Value::Array(recent));
        }

        Ok(Value::Object(map))
    }
}

/// Hook helper: syncs `state.db` in `hub.config().data_dir` with `bus.jsonl`.
pub fn on_message(hub: &Hub, msg: &Message, _now_ms: u64) {
    let _ = hub.with_store(|store| {
        let _ = store.apply(msg);
    });
}

/// Hook helper: ensures `state.db` is caught up on tick and checks quota budget alerts.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    hub.with_store(|store| {
        crate::quota::apply::check_coord_budget_alerts(
            store.connection(),
            None,
            crate::quota::apply::DEFAULT_COORD_BUDGET_ALARM_PCT,
            now_ms / 1000,
        )
        .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Hook helper: populates `/api/state` from `state.db`.
pub fn api(hub: &Hub, now_ms: u64, state: &mut Map<String, Value>) {
    let _ = hub.with_store(|store| {
        let _ = store.registry.api_all(&store.conn, now_ms / 1000, state);
    });
}
