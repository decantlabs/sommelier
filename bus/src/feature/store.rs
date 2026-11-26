//! The store part of the feature map: the feature of a run order and the `features` part of
//! `/api/state`.
//!
//! The feature of an order of the task queue lives in the task module (`TASK_FEATURE`). The feature of
//! a run order is read here from the first hub FACT `runorder/<id>` that carries it. The part of
//! `/api/state` is derived at read time from the map, the open orders and the reports. Nothing of it
//! is stored. A hub without a feature file adds nothing to the state.

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};

use super::registry;
use super::state::{feature_states, proof_cells};
use crate::msg::{Kind, Message, Role};
use crate::report::{QuorumRules, query_reports, side};
use crate::store::StoreModule;

/// DDL of the table that holds the feature of a run order.
pub const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS RUN_ORDER_FEATURE (
    run_id TEXT NOT NULL PRIMARY KEY,
    feature TEXT NOT NULL
);
";

/// Store projection module of the feature map.
pub struct FeatureModule;

impl StoreModule for FeatureModule {
    fn name(&self) -> &'static str {
        "feature"
    }

    fn schema(&self) -> &'static str {
        SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        let Some((run, feature)) = run_order_feature(msg) else {
            return Ok(());
        };
        tx.execute(
            "INSERT OR IGNORE INTO RUN_ORDER_FEATURE (run_id, feature) VALUES (?1, ?2)",
            params![run, feature],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let Some(dir) = side::data_dir_of(conn) else {
            return Ok(());
        };
        let Some(map) = registry::current(&dir) else {
            return Ok(());
        };
        state.insert("features".into(), Value::Array(derive(conn, &map)?));
        Ok(())
    }
}

/// The run order and the feature a hub FACT `runorder/<id>` names, when it names one.
fn run_order_feature(msg: &Message) -> Option<(String, String)> {
    if msg.kind != Kind::Fact || msg.role != Role::Hub || msg.from != "hub" {
        return None;
    }
    let run = msg.topic.strip_prefix("runorder/")?;
    let v: Value = serde_json::from_str(msg.body.trim()).ok()?;
    let feature = v.get("feature")?.as_str()?;
    if feature.is_empty() || feature.len() > super::MAX_ID {
        return None;
    }
    Some((run.to_owned(), feature.to_owned()))
}

/// Open orders per feature: `(tasks, run orders)`.
pub fn open_counts(conn: &Connection) -> Result<BTreeMap<String, (u64, u64)>, String> {
    let mut out: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT f.feature, COUNT(*) FROM TASK_FEATURE f JOIN TASK t ON t.id = f.task_id
             WHERE t.state NOT IN ('closed', 'cancelled') GROUP BY f.feature",
        )
        .map_err(|e| e.to_string())?;
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?
    {
        let (f, n) = row.map_err(|e| e.to_string())?;
        out.entry(f).or_default().0 = n.max(0) as u64;
    }
    let mut stmt = conn
        .prepare(
            "SELECT f.feature, COUNT(*) FROM RUN_ORDER_FEATURE f JOIN RUN_ORDER r ON r.id = f.run_id
             WHERE r.state = 'open' GROUP BY f.feature",
        )
        .map_err(|e| e.to_string())?;
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?
    {
        let (f, n) = row.map_err(|e| e.to_string())?;
        out.entry(f).or_default().1 = n.max(0) as u64;
    }
    Ok(out)
}

/// The `features` array of `/api/state`: per feature its area, title, number of open orders and the
/// state per machine key. Derived now from the reports, never stored.
pub fn derive(conn: &Connection, map: &super::FeatureMap) -> Result<Vec<Value>, String> {
    let rows = query_reports(conn)?;
    let blocks = side::blocks_of(conn);
    let cells = proof_cells(&rows, &blocks, &QuorumRules::default());
    let open = open_counts(conn)?;
    Ok(map
        .features()
        .iter()
        .map(|f| {
            let (tasks, runs) = open.get(&f.id).copied().unwrap_or((0, 0));
            let states: Map<String, Value> = feature_states(&f.proof, &cells)
                .into_iter()
                .map(|(k, s)| (k, json!(s.as_str())))
                .collect();
            json!({
                "id": f.id,
                "area": f.area,
                "title": f.title,
                "open_orders": tasks + runs,
                "open_tasks": tasks,
                "open_runs": runs,
                "state": states,
            })
        })
        .collect())
}
