//! Hub side of assigned review: assigning reviewers, taking verdicts and the first lock before landing.
//!
//! Stub of the wave two spine, see `docs/factory.md`. `hooks.rs` already routes to these functions.
//! The owner of this file fills them and leaves `hooks.rs` alone.

use serde_json::{Map, Value};

use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::Message;

/// Settings of this part of the hub. Lives in `HubConfig.review`. Everything is off by default.
#[derive(Clone, Debug, Default)]
pub struct ReviewConfig {}

/// Handles the requests `review.*`.
pub fn on_op(
    _ctx: &mut OpContext<'_>,
    op: &str,
    _body: &Value,
) -> Result<Map<String, Value>, String> {
    Err(format!("op `{op}` is not built yet"))
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(_hub: &Hub, _now_ms: u64) -> Vec<Message> {
    Vec::new()
}
