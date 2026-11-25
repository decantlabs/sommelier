//! Test reports of the test network: the REPORT table, quorum and the matrix.
//!
//! Part of the public factory, see `docs/factory.md`. This file is the stub that the spine commit
//! registered. The module owns its tables, its projection and its part of `/api/state`.

pub mod types;

use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value};

use crate::msg::Message;
use crate::store::StoreModule;

/// Store projection module of this folder.
pub struct ReportModule;

impl StoreModule for ReportModule {
    fn name(&self) -> &'static str {
        "report"
    }

    fn schema(&self) -> &'static str {
        ""
    }

    fn apply(&self, _tx: &Transaction<'_>, _msg: &Message) -> Result<(), String> {
        Ok(())
    }

    fn api(
        &self,
        _conn: &Connection,
        _now_sec: u64,
        _state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }
}
