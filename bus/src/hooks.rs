//! Hub extension hooks: message projection, periodic tick, module ops, HTTP routing, and state API (section 10.2).

use serde_json::{Map, Value, json};

use crate::agent::lease_loop::handle_task_exit_op;
use crate::brief::HttpOutcome;
use crate::hub::{Hub, HubConfig};
use crate::msg::{Kind, Message, Role};
use crate::quota::apply::{
    DEFAULT_COORD_BUDGET_ALARM_PCT, QuotaReportOp, check_coord_budget_alerts,
    validate_and_build_report_messages,
};
use crate::store::Store;
use crate::tokens::Member;

/// Caller identity and trust context passed to `hooks::on_op`.
#[derive(Clone, Debug)]
pub struct OpCaller {
    pub name: String,
    pub role: Role,
    /// `"owner"`, `"guest"`, `"observer"`, or `"tier0"` for a member that signed in with a key.
    pub trust: &'static str,
}

impl OpCaller {
    pub fn from_member(m: &Member) -> Self {
        let trust = match m.role {
            Role::Orchestrator | Role::Hub => "owner",
            Role::Member => "guest",
            Role::Observer => "observer",
        };
        Self {
            name: m.name.clone(),
            role: m.role,
            trust,
        }
    }
}

/// Validates that an emitted module or tick event is one of `OFFER`, `TASK_UPDATE`, `ALERT`, or `FACT`.
pub fn is_hook_emittable_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Offer | Kind::TaskUpdate | Kind::Alert | Kind::Fact
    )
}

/// Context provided by `hub.rs` to `hooks::on_op`, giving module hooks:
/// - caller member identity (`name`), `role`, and `trust` (`"owner"`, `"guest"`, `"observer"`),
/// - mutable access to the SQLite projection [`Store`] (`state.db`),
/// - a callback to validate, stamp, append to `bus.jsonl`, project, and broadcast
///   `OFFER`, `TASK_UPDATE`, `ALERT`, and `FACT` messages.
pub struct OpContext<'a> {
    pub caller: OpCaller,
    pub store: &'a mut Store,
    pub now_ms: u64,
    config: Option<&'a HubConfig>,
    emit_fn: &'a mut dyn FnMut(&mut Store, Message) -> Result<u64, String>,
}

impl<'a> OpContext<'a> {
    pub fn new(
        caller: OpCaller,
        store: &'a mut Store,
        now_ms: u64,
        emit_fn: &'a mut dyn FnMut(&mut Store, Message) -> Result<u64, String>,
    ) -> Self {
        Self {
            caller,
            store,
            now_ms,
            config: None,
            emit_fn,
        }
    }

    /// Gives the context the settings of the hub that runs the op.
    pub fn with_config(mut self, config: &'a HubConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The settings of the hub. A context made without them (a test of one module) sees a default
    /// config with every part off and an empty data folder.
    pub fn config(&self) -> &HubConfig {
        self.config.unwrap_or_else(|| {
            static DEFAULT: std::sync::OnceLock<HubConfig> = std::sync::OnceLock::new();
            DEFAULT.get_or_init(|| HubConfig::new(""))
        })
    }

    /// Validates, appends to `bus.jsonl`, projects into `state.db`, and broadcasts one
    /// `OFFER`, `TASK_UPDATE`, `ALERT`, or `FACT` message. Returns the assigned message id.
    pub fn emit(&mut self, msg: Message) -> Result<u64, String> {
        if !is_hook_emittable_kind(msg.kind) {
            return Err(format!(
                "module hook may only emit OFFER, TASK_UPDATE, ALERT, or FACT (got {})",
                msg.kind.as_str()
            ));
        }
        (self.emit_fn)(self.store, msg)
    }

    /// Emits a batch of validated `OFFER`, `TASK_UPDATE`, `ALERT`, or `FACT` messages in order.
    pub fn emit_all(&mut self, msgs: Vec<Message>) -> Result<Vec<u64>, String> {
        let mut ids = Vec::with_capacity(msgs.len());
        for m in msgs {
            ids.push(self.emit(m)?);
        }
        Ok(ids)
    }
}

/// Called when a message is appended or replayed at hub startup.
pub fn on_message(hub: &Hub, msg: &Message, now_ms: u64) {
    crate::derive::on_message(hub, msg, now_ms);
    crate::store::on_message(hub, msg, now_ms);
}

/// Called periodically on the hub tick. Returns `OFFER`, `TASK_UPDATE`, `ALERT`, and `FACT`
/// messages for the hub to stamp, append, project, and broadcast.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let mut msgs = crate::derive::on_tick(hub, now_ms);
    msgs.extend(crate::store::on_tick(hub, now_ms));
    // Project task reaper output before running the plan reaper so enforce mode
    // never sees stale active/suspect leases or open expired offers and emits duplicates.
    for msg in crate::task::on_tick(hub, now_ms) {
        if is_hook_emittable_kind(msg.kind)
            && let Err(e) = hub.append_hook_message(msg)
        {
            eprintln!("sommelier: {e}");
        }
    }
    msgs.extend(crate::plan::on_tick(hub, now_ms));
    msgs.extend(crate::role::eligible::on_tick(hub, now_ms));
    msgs.extend(crate::intake::on_tick(hub, now_ms));
    msgs.extend(crate::verify::ops::on_tick(hub, now_ms));
    msgs.extend(crate::review::ops::on_tick(hub, now_ms));
    msgs.extend(crate::stamp::ops::on_tick(hub, now_ms));
    msgs.extend(crate::report::ops::on_tick(hub, now_ms));
    msgs.retain(|m| is_hook_emittable_kind(m.kind));
    msgs
}

/// Delegates a client `Request::ModuleOp { op, body }` across module hooks.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    if ctx.caller.role == Role::Observer {
        return Err("observers are read only, this token cannot run module ops".into());
    }

    match op {
        "quota.report" => {
            let mut payload = body.clone();
            if let Some(obj) = payload.as_object_mut()
                && !obj.contains_key("owner")
            {
                obj.insert("owner".into(), Value::String(ctx.caller.name.clone()));
            }
            let report: QuotaReportOp = serde_json::from_value(payload)
                .map_err(|e| format!("invalid quota.report payload: {e}"))?;
            let msgs = validate_and_build_report_messages(
                &ctx.caller.name,
                ctx.caller.role,
                &report,
                ctx.now_ms,
            )
            .map_err(|e| e.reason)?;
            let mut ids = ctx.emit_all(msgs)?;

            if let Ok(alerts) = check_coord_budget_alerts(
                ctx.store.connection(),
                None,
                DEFAULT_COORD_BUDGET_ALARM_PCT,
                ctx.now_ms / 1000,
            ) && !alerts.is_empty()
            {
                let alert_ids = ctx.emit_all(alerts)?;
                ids.extend(alert_ids);
            }

            let mut out = Map::new();
            out.insert("ids".into(), json!(ids));
            out.insert("applied".into(), json!(ids.len()));
            Ok(out)
        }
        "task.exit" | "agent.exit" => handle_task_exit_op(ctx, body),
        op if op.starts_with("key.") => crate::key::on_op(ctx, op, body),
        op if op.starts_with("submit.") => crate::intake::on_op(ctx, op, body),
        op if op.starts_with("verify.") => crate::verify::ops::on_op(ctx, op, body),
        op if op.starts_with("review.") => crate::review::ops::on_op(ctx, op, body),
        op if op.starts_with("stamp.") => crate::stamp::ops::on_op(ctx, op, body),
        op if op.starts_with("report.") || op.starts_with("test.") => {
            crate::report::ops::on_op(ctx, op, body)
        }
        op if op.starts_with("task.") => crate::task::on_op(ctx, op, body),
        op if op.starts_with("land.") || op.starts_with("run.") => {
            crate::land::on_op(ctx, op, body)
        }
        op if op.starts_with("plan.") || op == "pause" || op == "resume" => {
            crate::plan::on_op(ctx, op, body)
        }
        op if op.starts_with("role.") => crate::role::eligible::on_op(ctx, op, body),
        op if op.starts_with("alert.") || op.starts_with("alerts.") => {
            crate::cli::alerts::on_op(ctx, op, body)
        }
        other => Err(format!(
            "unknown op `{other}` (send, heartbeat, whoami, claims, who, idle, roster, log, quota.report, key.*, submit.*, verify.*, review.*, stamp.*, report.*, test.*, task.*, land.*, run.*, plan.*, role.*, alert.*)"
        )),
    }
}

/// Routes `/api/brief/<id>` and module `/api/*` HTTP endpoints before rejecting non-GET/HEAD requests.
pub fn on_http(
    hub: &Hub,
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
) -> Option<HttpOutcome> {
    if path.starts_with("/api/brief/") || path == "/api/brief" {
        return crate::brief::on_http(&hub.config().data_dir, method, path, query, body, caller);
    }
    if path.starts_with("/api/submit/")
        || path.starts_with("/api/bundle/")
        || path.starts_with("/api/order/")
    {
        return crate::intake::on_http(hub, method, path, query, body, caller);
    }
    None
}

/// Adds module projections to the `/api/state` object.
pub fn api(hub: &Hub, now_ms: u64, state: &mut Map<String, Value>) {
    crate::derive::api(hub, now_ms, state);
    crate::store::api(hub, now_ms, state);
}
