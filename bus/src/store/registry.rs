//! Module registry for `state.db` projections (section 10.2).
//!
//! Section 10.2 rule:
//! "Every module owns `schema()` (its tables), `apply(tx, message)` (its projection)
//! and `api(state)` (its part of `/api/state`). The store calls all modules. So no step
//! edits another module's projection."

use std::path::Path;

use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value};

use crate::msg::{Kind, Message};

use super::core::CoreModule;
use super::replay::read_log_messages;

/// Trait implemented by every projection module registered with the store.
pub trait StoreModule: Send + Sync {
    /// Short identifier of the module (`"core"`, `"task"`, `"quota"`, `"land"`, `"regress"`, `"role"`, `"key"`, `"stamp"`, `"review"`, `"report"`).
    fn name(&self) -> &'static str;

    /// SQL DDL statements (`CREATE TABLE IF NOT EXISTS ...`) owned by this module.
    fn schema(&self) -> &'static str;

    /// Projects a single accepted log message into the module's tables inside `tx`.
    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String>;

    /// Adds the module's projected state to the `/api/state` JSON object.
    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String>;
}

/// Ordered registry of store projection modules.
pub struct ModuleRegistry {
    modules: Vec<Box<dyn StoreModule>>,
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleRegistry {
    /// Creates a registry initialized with the built-in core module and active projection modules.
    pub fn new() -> Self {
        Self {
            modules: vec![
                Box::new(CoreModule),
                Box::new(crate::task::TaskModule),
                Box::new(crate::quota::apply::QuotaModule),
                Box::new(crate::land::LandModule),
                Box::new(crate::regress::RegressModule::new()),
                Box::new(crate::plan::PlanModule),
                Box::new(crate::role::eligible::RoleStoreModule),
                Box::new(crate::key::KeyModule),
                Box::new(crate::stamp::StampModule),
                Box::new(crate::review::ReviewModule),
                Box::new(crate::report::ReportModule),
            ],
        }
    }

    /// Registers an additional projection module if a module with the same name is not already present.
    pub fn register(&mut self, module: Box<dyn StoreModule>) {
        if !self.modules.iter().any(|m| m.name() == module.name()) {
            self.modules.push(module);
        }
    }

    /// Returns the registered modules in registration order.
    pub fn modules(&self) -> &[Box<dyn StoreModule>] {
        &self.modules
    }

    /// Executes `schema()` for every registered module.
    pub fn init_schemas(&self, conn: &Connection) -> Result<(), String> {
        for m in &self.modules {
            let sql = m.schema().trim();
            if !sql.is_empty() {
                conn.execute_batch(sql)
                    .map_err(|e| format!("module `{}` schema error: {e}", m.name()))?;
            }
        }
        self.backfill_existing_hello(conn)?;
        Ok(())
    }

    /// Backfills `ROLE_HELLO` on an existing `state.db` migrated after `Kind::Hello` messages
    /// were already applied, using `bus.jsonl` beside `state.db` and `MEMBER`/`CAPACITY` rows.
    fn backfill_existing_hello(&self, conn: &Connection) -> Result<(), String> {
        let Some(role_mod) = self.modules.iter().find(|m| m.name() == "role") else {
            return Ok(());
        };
        let hello_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ROLE_HELLO", [], |r| r.get(0))
            .unwrap_or(0);
        if hello_count > 0 {
            return Ok(());
        }
        let cursor: u64 = conn
            .query_row("SELECT value FROM META WHERE name = 'cursor'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let member_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM MEMBER WHERE msg_id > 0", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        if cursor == 0 && member_count == 0 {
            return Ok(());
        }

        let db_file: Option<String> = conn
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name = 'main'",
                [],
                |r| r.get(0),
            )
            .ok()
            .filter(|s: &String| !s.is_empty());

        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        if let Some(ref db_path_str) = db_file
            && let Some(dir) = Path::new(db_path_str).parent()
        {
            let log_path = dir.join("bus.jsonl");
            if log_path.is_file()
                && let Ok(msgs) = read_log_messages(&log_path)
            {
                for msg in msgs {
                    if msg.kind == Kind::Hello && (cursor == 0 || msg.id <= cursor) {
                        role_mod.apply(&tx, &msg)?;
                    }
                }
            }
        }
        crate::role::eligible::backfill_role_hello_from_tables(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Calls `apply(tx, msg)` on every registered module in order.
    pub fn apply_all(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        for m in &self.modules {
            m.apply(tx, msg)
                .map_err(|e| format!("module `{}` apply error on #{}: {e}", m.name(), msg.id))?;
        }
        Ok(())
    }

    /// Calls `api(conn, now_sec, state)` on every registered module in order.
    pub fn api_all(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        for m in &self.modules {
            m.api(conn, now_sec, state)
                .map_err(|e| format!("module `{}` api error: {e}", m.name()))?;
        }
        Ok(())
    }
}
