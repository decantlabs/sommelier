//! Intake of submissions on the hub: taking a signed bundle, storing it by its hash, running the static
//! gates and handing it to the clean build.
//!
//! Stub of the wave two spine, see `docs/factory.md` sections 8.1 to 8.3. `hooks.rs` already routes the
//! requests `submit.*` and the paths `/api/submit/`, `/api/bundle/` and `/api/order/` to this module.

pub mod schema;

use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value};

use crate::brief::HttpOutcome;
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::Message;
use crate::store::StoreModule;
use crate::tokens::Member;

/// Settings of the intake. Lives in `HubConfig.intake`. Everything is off by default.
#[derive(Clone, Debug, Default)]
pub struct IntakeConfig {}

/// Store projection module of the SUBMISSION table and the tables of the gates and the clean build.
pub struct IntakeModule;

impl StoreModule for IntakeModule {
    fn name(&self) -> &'static str {
        "intake"
    }

    fn schema(&self) -> &'static str {
        schema::SUBMISSION_SCHEMA_SQL
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

/// Handles the requests `submit.*`.
pub fn on_op(
    _ctx: &mut OpContext<'_>,
    op: &str,
    _body: &Value,
) -> Result<Map<String, Value>, String> {
    Err(format!("op `{op}` is not built yet"))
}

/// Handles `/api/submit/`, `/api/bundle/` and `/api/order/`. `None` means not handled here.
pub fn on_http(
    _hub: &Hub,
    _method: &str,
    _path: &str,
    _query: Option<&str>,
    _body: &[u8],
    _caller: Option<&Member>,
) -> Option<HttpOutcome> {
    None
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(_hub: &Hub, _now_ms: u64) -> Vec<Message> {
    Vec::new()
}
