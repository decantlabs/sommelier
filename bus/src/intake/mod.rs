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

/// How many bytes `caller` may upload to `path` (`/api/submit/<task>/<attempt>`), decided from the
/// request head alone: intake is on, the caller is no observer, has a tier, holds the active lease of
/// the order at that attempt. The web layer reads the body only after this answered `Ok`, up to the
/// number it returns.
pub fn upload_allowance(hub: &Hub, caller: &Member, path: &str) -> Result<u64, String> {
    use crate::key::book::{KeyState, current_row};
    use crate::msg::Role;
    use crate::stamp::tier_table::tier_from_table;
    let cfg = &hub.config().intake;
    if !cfg.enabled {
        return Err("this hub does not take submissions: intake is switched off".into());
    }
    let usage = "the path is /api/submit/<task>/<attempt>";
    let rest = path.strip_prefix("/api/submit/").ok_or(usage)?;
    let (task, attempt) = rest.split_once('/').ok_or(usage)?;
    let task = crate::brief::validate_brief_id(task).map_err(|_| usage.to_owned())?;
    attempt.parse::<u32>().map_err(|_| usage.to_owned())?;
    if caller.role == Role::Observer {
        return Err("observers are read only, this token cannot submit".into());
    }
    let looked = hub
        .with_store(|store| {
            let conn = store.connection();
            let is_key = caller.hash.is_empty() && caller.token.is_none();
            let key_active = is_key
                && matches!(
                    current_row(conn, &caller.name),
                    Ok(Some((_, _, _, KeyState::Active)))
                );
            let role = (!is_key).then_some(caller.role);
            let tier = tier_from_table(conn, &caller.name, role);
            let lease = crate::task::query_active_lease_for_task(conn, task)
                .ok()
                .flatten();
            (is_key, key_active, tier, lease)
        })
        .ok_or("the hub could not read its tables, try again")?;
    let (is_key, key_active, tier, lease) = looked;
    if is_key && !key_active {
        return Err("this key is not active".into());
    }
    if tier < 0 {
        return Err("this member has no tier and may not submit".into());
    }
    match lease {
        // A wrong attempt of the holder is answered with 409 by the handler, after the body.
        Some(l) if l.owner == caller.name => Ok(cfg.cap_for_tier(tier)),
        Some(_) => Err(format!("order `{task}` is leased to another member")),
        None => Err(format!("you hold no active lease of order `{task}`")),
    }
}
