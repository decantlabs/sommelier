//! Intake of submissions on the hub: taking a signed bundle, storing it by its hash, running the static
//! gates and handing it to the clean build.
//!
//! See `docs/factory.md` sections 8.1 to 8.3 and the section Submissions of `PROTOCOL.md`.
//!
//! * [`config`]: the settings, all off by default.
//! * [`facts`]: rows, FACT bodies and the return of an order to the queue.
//! * [`tables`]: the projections SUBMISSION, GATE and VERIFY.
//! * [`http`]: `PUT /api/submit/`, `GET /api/bundle/` and `GET /api/order/`.
//! * [`gate_run`]: the static gates on worker threads.

pub mod config;
pub mod facts;
pub mod gate_run;
pub mod http;
pub mod schema;
pub mod tables;

use serde_json::{Map, Value, json};

use crate::brief::HttpOutcome;
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::Message;
use crate::tokens::Member;

pub use config::{InspectFn, IntakeConfig};
pub use facts::{SubRow, requeue_message};
pub use tables::IntakeModule;

/// Handles the requests `submit.*`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    _body: &Value,
) -> Result<Map<String, Value>, String> {
    match op {
        "submit.status" => {
            let rows = facts::query_newest(ctx.store.connection(), Some(&ctx.caller.name), 50);
            let mut out = Map::new();
            out.insert(
                "submissions".into(),
                json!(rows.iter().map(SubRow::to_json).collect::<Vec<_>>()),
            );
            Ok(out)
        }
        other => Err(format!("unknown op `{other}` (submit.status)")),
    }
}

/// Handles `/api/submit/`, `/api/bundle/` and `/api/order/`. `None` means not handled here.
pub fn on_http(
    hub: &Hub,
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
) -> Option<HttpOutcome> {
    http::on_http(hub, method, path, query, body, caller)
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    if !hub.config().intake.enabled {
        return Vec::new();
    }
    gate_run::on_tick(hub, now_ms)
}
