//! Task backlog, leases, offers, and checkpoints (sections 3, 4.1, 4.2, 5.2, step 8).

pub mod board;
pub mod reaper;
pub mod risk;
pub mod schema;
pub mod state;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value, json};

use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;
use crate::time::{parse_ms, rfc3339};

pub use board::{BoardRow, board_row_to_task_payload, parse_board_tsv};
pub use reaper::{LeaseTimers, reap_leases_and_offers};
pub use schema::{
    CheckpointRow, LeaseRow, OfferRow, TASK_SCHEMA_SQL, TASK_TABLES, TaskRow, insert_checkpoint_tx,
    latest_checkpoint, next_checkpoint_seq, query_active_lease_for_task, query_checkpoints,
    query_leases, query_offers, query_task, query_tasks, touch_lease_heartbeat, upsert_lease_tx,
    upsert_offer_tx, upsert_task_tx,
};
pub use state::{
    MAX_CHECKPOINT_NOTE_CHARS, TaskState, deps_closed, has_prior_art_verdict, has_required_fields,
    is_admissible_ready, normalize_checkpoint_note, priority_level, scopes_overlap,
    select_next_ready, task_score, validate_done_result,
};

static HUB_START_SEC: OnceLock<u64> = OnceLock::new();

/// Store projection module for `TASK`, `LEASE`, `OFFER`, and `CHECKPOINT` (section 10.2).
pub struct TaskModule;

impl StoreModule for TaskModule {
    fn name(&self) -> &'static str {
        "task"
    }

    fn schema(&self) -> &'static str {
        TASK_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_task_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_task_api(conn, now_sec, state)
    }
}

/// Initializes the task tables in `conn`.
pub fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(TASK_SCHEMA_SQL)
        .map_err(|e| format!("task schema error: {e}"))
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

fn u64_field(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn u32_field(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).map(|n| n as u32)
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

fn string_list_field(v: &Value, key: &str) -> Option<Vec<String>> {
    let arr = v.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn merge_task_fields(row: &mut TaskRow, src: &Value) {
    if let Some(s) = str_field(src, "id")
        && !s.is_empty()
    {
        row.id = s;
    }
    if let Some(s) = str_field(src, "title") {
        row.title = s;
    }
    if let Some(s) = str_field(src, "kind") {
        row.kind = s;
    }
    if let Some(s) = str_field(src, "priority") {
        row.priority = s;
    }
    if let Some(s) = str_field(src, "state") {
        row.state = s;
    }
    if let Some(s) = str_field(src, "track") {
        row.track = s;
    }
    if let Some(s) = str_field(src, "scope") {
        row.scope = s;
    }
    if let Some(s) = str_field(src, "accepts") {
        row.accepts = s;
    }
    if let Some(s) = str_field(src, "min_tier") {
        row.min_tier = s;
    }
    if let Some(s) = str_field(src, "branch") {
        row.branch = s;
    }
    if let Some(s) = str_field(src, "base_sha") {
        row.base_sha = s;
    }
    if let Some(s) = str_field(src, "brief_sha256") {
        row.brief_sha256 = s;
    }
    if let Some(deps) = string_list_field(src, "deps") {
        row.deps = deps;
    }
    if let Some(s) = str_field(src, "size") {
        row.size = s;
    }
    if let Some(b) = bool_field(src, "checkpointable") {
        row.checkpointable = b;
    }
    if let Some(n) = u32_field(src, "attempt") {
        row.attempt = n;
    }
    if let Some(n) = u32_field(src, "max_attempts") {
        row.max_attempts = n;
    }
    if let Some(s) = str_field(src, "resume_note") {
        row.resume_note = s;
    }
    if let Some(s) = str_field(src, "resume_sha") {
        row.resume_sha = s;
    }
    if let Some(s) = str_field(src, "wip_ref") {
        row.wip_ref = s;
    }
    if let Some(ex) = src.get("exclude_pools_until")
        && ex.is_object()
    {
        row.exclude_pools_until = ex.clone();
    }
    if let Some(b) = bool_field(src, "prior_art") {
        row.prior_art = b;
    }
    if let Some(s) = str_field(src, "done_when") {
        row.done_when = s;
    }
    if let Some(s) = str_field(src, "result_ref") {
        row.result_ref = s;
    }
    if let Some(s) = str_field(src, "result_sha") {
        row.result_sha = s;
    }
    if let Some(s) = str_field(src, "risk")
        && risk::is_risk(&s)
    {
        row.risk = s;
    }
    if let Some(n) = u64_field(src, "min_trust")
        && n <= 3
    {
        row.min_trust = n as u8;
    }
    if let Some(s) = str_field(src, "review")
        && !s.is_empty()
    {
        row.review = s;
    }
    if let Some(s) = str_field(src, "credit")
        && matches!(s.as_str(), "handle" | "none")
    {
        row.credit = s;
    }
    if let Some(s) = str_field(src, "created_by") {
        row.created_by = s;
    }
    if let Some(n) = u64_field(src, "created_at") {
        row.created_at = n;
    }
    if let Some(n) = u64_field(src, "ready_at") {
        row.ready_at = n;
    }
    if let Some(n) = u64_field(src, "closed_at") {
        row.closed_at = n;
    }
}

/// Projects `TASK_UPDATE`, `OFFER`, and task-related `FACT` messages into `TASK`, `LEASE`, `OFFER`, and `CHECKPOINT`.
pub fn apply_task_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    let ts_sec = parse_ms(&msg.ts).unwrap_or(0) / 1000;
    match msg.kind {
        Kind::TaskUpdate => {
            let body: Value = serde_json::from_str(&msg.body)
                .map_err(|e| format!("invalid TASK_UPDATE JSON: {e}"))?;
            let task_id = str_field(&body, "task")
                .or_else(|| msg.topic.strip_prefix("task/").map(str::to_owned))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "TASK_UPDATE missing task id".to_owned())?;

            let mut row = query_task(tx, &task_id)?.unwrap_or_else(|| TaskRow {
                id: task_id.clone(),
                created_at: ts_sec,
                ready_at: ts_sec,
                ..TaskRow::default()
            });
            if let Some(task_obj) = body.get("task_row").filter(|v| v.is_object()) {
                merge_task_fields(&mut row, task_obj);
            }
            merge_task_fields(&mut row, &body);
            row.id = task_id.clone();
            row.msg_id = msg.id;
            upsert_task_tx(tx, &row)?;

            if let Some(lease_val) = body.get("lease").filter(|v| v.is_object()) {
                let attempt = u32_field(lease_val, "attempt").unwrap_or(row.attempt);
                let lease = LeaseRow {
                    task_id: str_field(lease_val, "task_id").unwrap_or_else(|| task_id.clone()),
                    attempt,
                    owner: str_field(lease_val, "owner").unwrap_or_default(),
                    worker: str_field(lease_val, "worker").unwrap_or_default(),
                    machine: str_field(lease_val, "machine").unwrap_or_default(),
                    class: str_field(lease_val, "class").unwrap_or_default(),
                    pools: string_list_field(lease_val, "pools").unwrap_or_default(),
                    granted_at: u64_field(lease_val, "granted_at").unwrap_or(ts_sec),
                    heartbeat_at: u64_field(lease_val, "heartbeat_at").unwrap_or(ts_sec),
                    expires_at: u64_field(lease_val, "expires_at").unwrap_or(0),
                    est_minutes_p90: lease_val
                        .get("est_minutes_p90")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    est_cost: lease_val
                        .get("est_cost")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    est_end_at: u64_field(lease_val, "est_end_at").unwrap_or(0),
                    state: str_field(lease_val, "state").unwrap_or_else(|| "active".into()),
                    end_reason: str_field(lease_val, "end_reason").unwrap_or_default(),
                    supervised: bool_field(lease_val, "supervised").unwrap_or(false),
                    msg_id: msg.id,
                };
                upsert_lease_tx(tx, &lease)?;
            }

            if let Some(cp_val) = body.get("checkpoint").filter(|v| v.is_object()) {
                let attempt = u32_field(cp_val, "attempt").unwrap_or(row.attempt);
                let seq = u32_field(cp_val, "seq").unwrap_or(1);
                let cp = CheckpointRow {
                    task_id: str_field(cp_val, "task_id").unwrap_or_else(|| task_id.clone()),
                    attempt,
                    seq,
                    at: u64_field(cp_val, "at").unwrap_or(ts_sec),
                    branch: str_field(cp_val, "branch").unwrap_or_default(),
                    sha: str_field(cp_val, "sha").unwrap_or_default(),
                    pushed: bool_field(cp_val, "pushed").unwrap_or(false),
                    note: str_field(cp_val, "note").unwrap_or_default(),
                    source: str_field(cp_val, "source").unwrap_or_else(|| "agent".into()),
                    msg_id: msg.id,
                };
                insert_checkpoint_tx(tx, &cp)?;
            }
            Ok(())
        }
        Kind::Offer => {
            let body: Value =
                serde_json::from_str(&msg.body).map_err(|e| format!("invalid OFFER JSON: {e}"))?;
            let task_id = str_field(&body, "task")
                .or_else(|| str_field(&body, "task_id"))
                .or_else(|| msg.topic.strip_prefix("task/").map(str::to_owned))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "OFFER missing task id".to_owned())?;
            let offer_id = str_field(&body, "offer_id")
                .or_else(|| str_field(&body, "id"))
                .unwrap_or_else(|| format!("offer-{}-{}", task_id, msg.id));
            let offer_state = str_field(&body, "state").unwrap_or_else(|| "open".into());
            let row = OfferRow {
                id: offer_id,
                task_id: task_id.clone(),
                to_owner: str_field(&body, "to_owner")
                    .or_else(|| msg.to.first().cloned())
                    .unwrap_or_default(),
                class: str_field(&body, "class").unwrap_or_default(),
                sent_at: u64_field(&body, "sent_at").unwrap_or(ts_sec),
                expires_at: u64_field(&body, "expires")
                    .or_else(|| u64_field(&body, "expires_at"))
                    .unwrap_or(ts_sec + 300),
                state: offer_state.clone(),
                reason: str_field(&body, "reason").unwrap_or_default(),
                msg_id: msg.id,
            };
            upsert_offer_tx(tx, &row)?;
            if offer_state == "open"
                && let Some(mut task) = query_task(tx, &task_id)?
                && task.state == "ready"
            {
                task.state = "offered".into();
                task.msg_id = msg.id;
                upsert_task_tx(tx, &task)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Populates `tasks`, `leases`, `offers`, and `checkpoints` in `/api/state`.
pub fn build_task_api(
    conn: &Connection,
    now_sec: u64,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let tasks = query_tasks(conn)?;
    let leases = query_leases(conn)?;
    let offers = query_offers(conn)?;
    let checkpoints = query_checkpoints(conn)?;

    let mut latest_cp: BTreeMap<String, CheckpointRow> = BTreeMap::new();
    for cp in &checkpoints {
        latest_cp.insert(cp.task_id.clone(), cp.clone());
    }

    let task_vals: Vec<Value> = tasks
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("score".into(), json!(task_score(t, now_sec)));
                if let Some(cp) = latest_cp.get(&t.id) {
                    obj.insert("latest_checkpoint".into(), json!(cp));
                }
            }
            v
        })
        .collect();

    state.insert("tasks".into(), Value::Array(task_vals));
    state.insert("leases".into(), json!(leases));
    state.insert("offers".into(), json!(offers));
    state.insert("checkpoints".into(), json!(checkpoints));
    Ok(())
}

/// Periodic tick hook: runs the lease reaper and offer expiry (section 4.2).
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let now_sec = now_ms / 1000;
    let started = *HUB_START_SEC.get_or_init(|| now_sec);
    hub.with_store(|store| {
        reap_leases_and_offers(store.connection(), now_sec, started, LeaseTimers::default())
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Decides the order fields of a new or replaced order after the body was merged. The risk is
/// computed from the scope and never taken as declared: the caller may ask for `low` and gets it only
/// when the scope can not meet a hold path. `guarded` raises `min_trust` to 2. A tier 0 order needs risk
/// low and size small. Returns plain notes for what the hub changed on its own.
pub fn settle_order_fields(
    row: &mut TaskRow,
    body: &Value,
    hold_paths: &[String],
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let asked = str_field(body, "risk").filter(|s| !s.is_empty());
    if let Some(a) = &asked
        && !risk::is_risk(a)
    {
        return Err(format!("risk must be low, normal or guarded, not `{a}`"));
    }
    if let Some(n) = body.get("min_trust")
        && !n.as_u64().is_some_and(|n| n <= 3)
    {
        return Err("min_trust must be a number from 0 to 3".into());
    }
    if let Some(c) = str_field(body, "credit")
        && !matches!(c.as_str(), "handle" | "none")
    {
        return Err(format!("credit must be handle or none, not `{c}`"));
    }
    let met = risk::met_hold_paths(&row.scope, hold_paths);
    let guarded = !met.is_empty() || asked.as_deref() == Some("guarded");
    if asked.as_deref() == Some("low") && !met.is_empty() {
        return Err(format!(
            "risk low refused: the scope can meet the hold path `{}`, so the order is guarded",
            met[0]
        ));
    }
    row.risk = if guarded {
        "guarded".into()
    } else if asked.as_deref() == Some("low") {
        "low".into()
    } else {
        "normal".into()
    };
    if row.min_trust == 0 && (row.risk != "low" || !risk::size_is_small(&row.size)) {
        let why = if row.risk != "low" {
            format!("its risk is {}", row.risk)
        } else {
            "its size is not small".into()
        };
        return Err(format!(
            "min_trust 0 refused: a tier 0 order needs risk low and size small, and {why}"
        ));
    }
    if row.risk == "guarded" && row.min_trust < 2 {
        row.min_trust = 2;
        notes.push("guarded order: min_trust raised to 2".into());
    }
    Ok(notes)
}

fn require_orchestrator(ctx: &OpContext<'_>, op: &str) -> Result<(), String> {
    if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
        return Err(format!("only an orchestrator may run `{op}`"));
    }
    Ok(())
}

/// Handles `task.*` module ops from clients (section 5.2, step 8).
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let now_sec = ctx.now_ms / 1000;
    let ts = rfc3339(ctx.now_ms);
    let mut out = Map::new();

    match op {
        "task.add" => {
            require_orchestrator(ctx, op)?;
            let id = str_field(body, "id")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.add requires `id`".to_owned())?;
            let upsert = bool_field(body, "upsert").unwrap_or(false);
            if !upsert && query_task(ctx.store.connection(), &id)?.is_some() {
                return Err(format!("task `{id}` already exists"));
            }

            let mut row = query_task(ctx.store.connection(), &id)?.unwrap_or_else(|| TaskRow {
                id: id.clone(),
                created_by: ctx.caller.name.clone(),
                created_at: now_sec,
                ready_at: now_sec,
                ..TaskRow::default()
            });
            merge_task_fields(&mut row, body);
            let notes = settle_order_fields(&mut row, body, &risk::default_hold_paths())?;
            if !row.brief_sha256.is_empty()
                && (row.brief_sha256.len() != 64
                    || !row.brief_sha256.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err("brief_sha256 must be a 64-character hex SHA-256 digest".into());
            }
            if row.state.is_empty() {
                row.state = if has_required_fields(&row) {
                    "ready".into()
                } else {
                    "draft".into()
                };
            }
            if row.state == "ready" && !has_required_fields(&row) {
                return Err("ready task requires id, title, kind, scope, and done_when".into());
            }
            if row.created_by.is_empty() {
                row.created_by = ctx.caller.name.clone();
            }
            if row.created_at == 0 {
                row.created_at = now_sec;
            }
            if row.ready_at == 0 && row.state == "ready" {
                row.ready_at = now_sec;
            }

            let msg = Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "task": id,
                    "prev": "",
                    "state": row.state,
                    "attempt": row.attempt,
                    "by": ctx.caller.name,
                    "reason": "added",
                    "task_row": row,
                })
                .to_string(),
                refs: vec![format!("state:{}", row.state)],
                to: Vec::new(),
            };
            let msg_id = ctx.emit(msg)?;
            out.insert("task".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            if !notes.is_empty() {
                out.insert("notes".into(), json!(notes));
            }
            Ok(out)
        }
        "task.list" => {
            let state_filter = str_field(body, "state").filter(|s| !s.is_empty());
            let mine = bool_field(body, "mine").unwrap_or(false);
            let tasks = query_tasks(ctx.store.connection())?;
            let leases = query_leases(ctx.store.connection())?;
            let my_tasks: Vec<String> = leases
                .iter()
                .filter(|l| {
                    l.owner == ctx.caller.name && matches!(l.state.as_str(), "active" | "suspect")
                })
                .map(|l| l.task_id.clone())
                .collect();
            let filtered: Vec<Value> = tasks
                .into_iter()
                .filter(|t| state_filter.as_ref().is_none_or(|s| t.state == *s))
                .filter(|t| !mine || my_tasks.contains(&t.id))
                .map(|t| {
                    let score = task_score(&t, now_sec);
                    let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("score".into(), json!(score));
                    }
                    v
                })
                .collect();
            out.insert("tasks".into(), Value::Array(filtered));
            Ok(out)
        }
        "task.show" => {
            let id = str_field(body, "id")
                .or_else(|| str_field(body, "task"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.show requires `id`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let lease = query_active_lease_for_task(ctx.store.connection(), &id)?;
            let checkpoints: Vec<CheckpointRow> = query_checkpoints(ctx.store.connection())?
                .into_iter()
                .filter(|c| c.task_id == id)
                .collect();
            out.insert("task".into(), json!(task));
            out.insert("score".into(), json!(task_score(&task, now_sec)));
            out.insert("lease".into(), json!(lease));
            out.insert("checkpoints".into(), json!(checkpoints));
            Ok(out)
        }
        "task.cancel" => {
            require_orchestrator(ctx, op)?;
            let id = str_field(body, "id")
                .or_else(|| str_field(body, "task"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.cancel requires `id`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let prev = TaskState::parse(&task.state)
                .ok_or_else(|| format!("invalid state `{}`", task.state))?;
            if !prev.can_transition(TaskState::Cancelled) {
                return Err(format!(
                    "cannot cancel task `{id}` from state `{}`",
                    task.state
                ));
            }
            let reason = str_field(body, "reason").unwrap_or_else(|| "cancelled".into());
            let mut update = json!({
                "v": 1,
                "task": id,
                "prev": task.state,
                "state": "cancelled",
                "attempt": task.attempt,
                "by": ctx.caller.name,
                "reason": reason,
                "closed_at": now_sec,
            });
            if let Some(mut lease) = query_active_lease_for_task(ctx.store.connection(), &id)? {
                lease.state = "released".into();
                lease.end_reason = "cancelled".into();
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("lease".into(), json!(lease));
            }
            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: update.to_string(),
                refs: vec!["state:cancelled".into()],
                to: Vec::new(),
            })?;
            out.insert("msg_id".into(), json!(msg_id));
            out.insert("state".into(), json!("cancelled"));
            Ok(out)
        }
        "task.requeue" => {
            require_orchestrator(ctx, op)?;
            let id = str_field(body, "id")
                .or_else(|| str_field(body, "task"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.requeue requires `id`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let prev = TaskState::parse(&task.state)
                .ok_or_else(|| format!("invalid state `{}`", task.state))?;
            if !prev.can_transition(TaskState::Ready) {
                return Err(format!(
                    "cannot requeue task `{id}` from state `{}`",
                    task.state
                ));
            }
            let reason = str_field(body, "reason").unwrap_or_else(|| "requeued".into());
            let note = str_field(body, "resume_note").unwrap_or_else(|| task.resume_note.clone());
            let next_attempt = task.attempt.saturating_add(1);
            let mut update = json!({
                "v": 1,
                "task": id,
                "prev": task.state,
                "state": "ready",
                "attempt": next_attempt,
                "by": ctx.caller.name,
                "reason": reason,
                "resume_note": note,
            });
            if let Some(mut lease) = query_active_lease_for_task(ctx.store.connection(), &id)? {
                lease.state = "released".into();
                lease.end_reason = "requeued".into();
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("lease".into(), json!(lease));
            }
            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: update.to_string(),
                refs: vec!["state:ready".into()],
                to: Vec::new(),
            })?;
            out.insert("msg_id".into(), json!(msg_id));
            out.insert("attempt".into(), json!(next_attempt));
            Ok(out)
        }
        "task.next" => {
            let class_filter = str_field(body, "class");
            let peek = bool_field(body, "peek").unwrap_or(false);
            let found =
                select_next_ready(ctx.store.connection(), now_sec, class_filter.as_deref())?;
            if let Some((task, score, ready_count)) = found {
                out.insert("found".into(), json!(true));
                out.insert("peek".into(), json!(peek));
                out.insert("ready_count".into(), json!(ready_count));
                out.insert("score".into(), json!(score));
                out.insert("task".into(), json!(task));
            } else {
                out.insert("found".into(), json!(false));
                out.insert("peek".into(), json!(peek));
                out.insert("ready_count".into(), json!(0));
            }
            Ok(out)
        }
        "task.work" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.work requires `task`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            if let Some(existing) = query_active_lease_for_task(ctx.store.connection(), &id)? {
                return Err(format!(
                    "task `{id}` already has an active lease held by {} (attempt {})",
                    existing.owner, existing.attempt
                ));
            }
            let prev = TaskState::parse(&task.state)
                .ok_or_else(|| format!("invalid state `{}`", task.state))?;
            if !matches!(prev, TaskState::Ready | TaskState::Offered) {
                return Err(format!(
                    "task `{id}` cannot be leased from state `{}`",
                    task.state
                ));
            }
            let matching_offer = if prev == TaskState::Offered {
                let open_offers: Vec<OfferRow> = query_offers(ctx.store.connection())?
                    .into_iter()
                    .filter(|o| o.task_id == id && o.state == "open")
                    .collect();
                let found = open_offers
                    .iter()
                    .find(|o| o.to_owner == ctx.caller.name)
                    .cloned();
                if found.is_none() {
                    if let Some(other) = open_offers.first() {
                        return Err(format!(
                            "task `{id}` is offered to {}, not {}",
                            other.to_owner, ctx.caller.name
                        ));
                    }
                    return Err(format!(
                        "task `{id}` is in state `offered` with no open offer for {}",
                        ctx.caller.name
                    ));
                }
                found
            } else {
                None
            };
            let all_tasks = query_tasks(ctx.store.connection())?;
            if !deps_closed(&all_tasks, &task) {
                return Err(format!("task `{id}` has unclosed dependencies"));
            }
            if let Some(blocker) = all_tasks.iter().find(|other| {
                other.id != task.id
                    && matches!(other.state.as_str(), "leased" | "running")
                    && scopes_overlap(&task.scope, &other.scope)
            }) {
                return Err(format!(
                    "task `{id}` scope overlaps active task `{}` ({})",
                    blocker.id, blocker.scope
                ));
            }
            if let Some(expected_sha) = str_field(body, "brief_sha256").filter(|s| !s.is_empty())
                && !task.brief_sha256.is_empty()
                && !expected_sha.eq_ignore_ascii_case(&task.brief_sha256)
            {
                return Err(format!(
                    "brief sha256 mismatch for `{id}`: expected {}, got {expected_sha}",
                    task.brief_sha256
                ));
            }

            let lease_sec = u64_field(body, "lease_sec").unwrap_or(45 * 60);
            let expires_at = now_sec.saturating_add(lease_sec);
            let lease = LeaseRow {
                task_id: id.clone(),
                attempt: task.attempt,
                owner: ctx.caller.name.clone(),
                worker: str_field(body, "worker").unwrap_or_else(|| ctx.caller.name.clone()),
                machine: str_field(body, "machine").unwrap_or_default(),
                class: str_field(body, "class").unwrap_or_default(),
                pools: string_list_field(body, "pools").unwrap_or_default(),
                granted_at: now_sec,
                heartbeat_at: now_sec,
                expires_at,
                est_minutes_p90: 0.0,
                est_cost: 0.0,
                est_end_at: expires_at,
                state: "active".into(),
                end_reason: String::new(),
                supervised: bool_field(body, "supervised").unwrap_or(false),
                msg_id: 0,
            };

            let mut msgs = Vec::new();
            if let Some(offer) = matching_offer {
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Offer,
                    topic: format!("task/{id}"),
                    body: json!({
                        "v": 1,
                        "offer_id": offer.id,
                        "task": id,
                        "to_owner": offer.to_owner,
                        "class": offer.class,
                        "sent_at": offer.sent_at,
                        "expires": offer.expires_at,
                        "state": "accepted",
                        "reason": "leased",
                    })
                    .to_string(),
                    refs: vec!["offer:accepted".into()],
                    to: Vec::new(),
                });
            }
            msgs.push(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "task": id,
                    "prev": task.state,
                    "state": "leased",
                    "attempt": task.attempt,
                    "by": ctx.caller.name,
                    "reason": "leased",
                    "lease": lease,
                })
                .to_string(),
                refs: vec!["state:leased".into()],
                to: Vec::new(),
            });
            let ids = ctx.emit_all(msgs)?;
            out.insert("task".into(), json!(task));
            out.insert("lease".into(), json!(lease));
            out.insert("msg_ids".into(), json!(ids));
            Ok(out)
        }
        "task.checkpoint" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.checkpoint requires `task`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let lease = query_active_lease_for_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` has no active lease"))?;
            if lease.owner != ctx.caller.name
                && !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub)
            {
                return Err(format!(
                    "task `{id}` is leased to {}, not {}",
                    lease.owner, ctx.caller.name
                ));
            }
            let note = normalize_checkpoint_note(&str_field(body, "note").unwrap_or_default())?;
            let seq = next_checkpoint_seq(ctx.store.connection(), &id, task.attempt)?;
            let cp = CheckpointRow {
                task_id: id.clone(),
                attempt: task.attempt,
                seq,
                at: now_sec,
                branch: str_field(body, "branch").unwrap_or_else(|| task.branch.clone()),
                sha: str_field(body, "sha").unwrap_or_default(),
                pushed: bool_field(body, "pushed").unwrap_or(false),
                note,
                source: str_field(body, "source").unwrap_or_else(|| "agent".into()),
                msg_id: 0,
            };
            let lease_sec = u64_field(body, "lease_sec").unwrap_or(45 * 60);
            let mut updated_lease = lease;
            updated_lease.heartbeat_at = now_sec;
            updated_lease.expires_at = now_sec.saturating_add(lease_sec);
            updated_lease.state = "active".into();

            let next_state = if task.state == "leased" {
                "running"
            } else {
                task.state.as_str()
            };
            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "task": id,
                    "prev": task.state,
                    "state": next_state,
                    "attempt": task.attempt,
                    "by": ctx.caller.name,
                    "reason": "checkpoint",
                    "lease": updated_lease,
                    "checkpoint": cp,
                })
                .to_string(),
                refs: vec![format!("checkpoint:{seq}")],
                to: Vec::new(),
            })?;
            out.insert("seq".into(), json!(seq));
            out.insert("expires_at".into(), json!(updated_lease.expires_at));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "task.done" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.done requires `task`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let prev = TaskState::parse(&task.state)
                .ok_or_else(|| format!("invalid state `{}`", task.state))?;
            if !matches!(prev, TaskState::Leased | TaskState::Running) {
                return Err(format!(
                    "task `{id}` cannot finish from state `{}`",
                    task.state
                ));
            }
            let lease = query_active_lease_for_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` has no active lease"))?;
            if lease.owner != ctx.caller.name
                && !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub)
            {
                return Err(format!(
                    "task `{id}` is leased to {}, not {}",
                    lease.owner, ctx.caller.name
                ));
            }
            let result_ref = str_field(body, "ref")
                .or_else(|| str_field(body, "branch"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| task.branch.clone());
            if result_ref.is_empty() {
                return Err("task.done requires `ref` (branch)".into());
            }
            let result_sha = str_field(body, "sha")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.done requires `sha`".to_owned())?;
            let result_text = body
                .get("result_text")
                .and_then(Value::as_str)
                .ok_or_else(|| "task.done requires `result_text`".to_owned())?;
            let remote_path = str_field(body, "remote_sha_path");
            validate_done_result(
                result_text,
                &result_ref,
                &id,
                remote_path.as_deref(),
                task.prior_art,
            )?;

            let mut released = lease;
            released.state = "released".into();
            released.end_reason = "done".into();

            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "task": id,
                    "prev": task.state,
                    "state": "delivered",
                    "attempt": task.attempt,
                    "by": ctx.caller.name,
                    "reason": "done",
                    "result_ref": result_ref,
                    "result_sha": result_sha,
                    "lease": released,
                })
                .to_string(),
                refs: vec![
                    "state:delivered".into(),
                    result_ref.clone(),
                    result_sha.clone(),
                ],
                to: Vec::new(),
            })?;
            out.insert("state".into(), json!("delivered"));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "task.stuck" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.stuck requires `task`".to_owned())?;
            let why = str_field(body, "why")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.stuck requires `why`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let prev = TaskState::parse(&task.state)
                .ok_or_else(|| format!("invalid state `{}`", task.state))?;
            if !prev.can_transition(TaskState::Stuck) {
                return Err(format!(
                    "cannot mark task `{id}` stuck from `{}`",
                    task.state
                ));
            }
            let mut update = json!({
                "v": 1,
                "task": id,
                "prev": task.state,
                "state": "stuck",
                "attempt": task.attempt,
                "by": ctx.caller.name,
                "reason": why,
            });
            if let Some(mut lease) = query_active_lease_for_task(ctx.store.connection(), &id)? {
                lease.state = "released".into();
                lease.end_reason = "stuck".into();
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("lease".into(), json!(lease));
            }
            let update_msg = Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: update.to_string(),
                refs: vec!["state:stuck".into()],
                to: Vec::new(),
            };
            let alert_msg = Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/STUCK/{id}"),
                body: json!({
                    "v": 1,
                    "code": "STUCK",
                    "key": id,
                    "severity": "warn",
                    "state": "open",
                    "opened_at": now_sec,
                    "last_at": now_sec,
                    "count": 1,
                    "text": format!("task {id} stuck: {why}"),
                    "data": { "task_id": id, "by": ctx.caller.name, "why": why },
                })
                .to_string(),
                refs: vec!["state:open".into()],
                to: vec!["coordinator".into()],
            };
            let ids = ctx.emit_all(vec![update_msg, alert_msg])?;
            out.insert("state".into(), json!("stuck"));
            out.insert("msg_ids".into(), json!(ids));
            Ok(out)
        }
        "task.decline" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.decline requires `task`".to_owned())?;
            let why = str_field(body, "why").unwrap_or_else(|| "declined".into());
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let offers = query_offers(ctx.store.connection())?;
            let open_offer = offers
                .into_iter()
                .find(|o| {
                    o.task_id == id
                        && o.state == "open"
                        && (o.to_owner == ctx.caller.name
                            || matches!(ctx.caller.role, Role::Orchestrator | Role::Hub))
                })
                .ok_or_else(|| format!("no open offer on task `{id}` for {}", ctx.caller.name))?;
            let offer_msg = Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Offer,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "offer_id": open_offer.id,
                    "task": id,
                    "to_owner": open_offer.to_owner,
                    "class": open_offer.class,
                    "sent_at": open_offer.sent_at,
                    "expires": open_offer.expires_at,
                    "state": "declined",
                    "reason": why,
                })
                .to_string(),
                refs: vec!["offer:declined".into()],
                to: Vec::new(),
            };
            let mut msgs = vec![offer_msg];
            if task.state == "offered" {
                msgs.push(Message {
                    id: 0,
                    ts,
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{id}"),
                    body: json!({
                        "v": 1,
                        "task": id,
                        "prev": "offered",
                        "state": "ready",
                        "attempt": task.attempt,
                        "by": ctx.caller.name,
                        "reason": why,
                    })
                    .to_string(),
                    refs: vec!["state:ready".into()],
                    to: Vec::new(),
                });
            }
            let ids = ctx.emit_all(msgs)?;
            out.insert("msg_ids".into(), json!(ids));
            Ok(out)
        }
        "task.handover" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.handover requires `task`".to_owned())?;
            let task = query_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` not found"))?;
            let lease = query_active_lease_for_task(ctx.store.connection(), &id)?
                .ok_or_else(|| format!("task `{id}` has no active lease"))?;
            if lease.owner != ctx.caller.name
                && !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub)
            {
                return Err(format!(
                    "task `{id}` is leased to {}, not {}",
                    lease.owner, ctx.caller.name
                ));
            }
            let note = normalize_checkpoint_note(&str_field(body, "note").unwrap_or_default())?;
            let seq = next_checkpoint_seq(ctx.store.connection(), &id, task.attempt)?;
            let cp = CheckpointRow {
                task_id: id.clone(),
                attempt: task.attempt,
                seq,
                at: now_sec,
                branch: str_field(body, "branch").unwrap_or_else(|| task.branch.clone()),
                sha: str_field(body, "sha").unwrap_or_default(),
                pushed: bool_field(body, "pushed").unwrap_or(false),
                note: note.clone(),
                source: "agent".into(),
                msg_id: 0,
            };
            let mut released = lease;
            released.state = "released".into();
            released.end_reason = "context_full".into();
            let next_attempt = task.attempt.saturating_add(1);
            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: json!({
                    "v": 1,
                    "task": id,
                    "prev": task.state,
                    "state": "ready",
                    "attempt": next_attempt,
                    "by": ctx.caller.name,
                    "reason": "context_full",
                    "resume_note": note,
                    "resume_sha": cp.sha,
                    "lease": released,
                    "checkpoint": cp,
                })
                .to_string(),
                refs: vec!["state:ready".into(), "reason:context_full".into()],
                to: Vec::new(),
            })?;
            out.insert("attempt".into(), json!(next_attempt));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "task.ask" => {
            let id = str_field(body, "task")
                .or_else(|| str_field(body, "id"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.ask requires `task`".to_owned())?;
            let text = str_field(body, "text")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "task.ask requires `text`".to_owned())?;
            let msg_id = ctx.emit(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/CONSULT/{id}"),
                body: json!({
                    "v": 1,
                    "code": "CONSULT",
                    "key": id,
                    "severity": "info",
                    "state": "open",
                    "opened_at": now_sec,
                    "last_at": now_sec,
                    "count": 1,
                    "text": format!("consult on {id} from {}: {text}", ctx.caller.name),
                    "data": { "task_id": id, "from": ctx.caller.name, "question": text },
                })
                .to_string(),
                refs: vec!["state:open".into()],
                to: vec!["brain".into()],
            })?;
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "task.offers" => {
            let top = u64_field(body, "top").unwrap_or(50) as usize;
            let owner = str_field(body, "owner").unwrap_or_else(|| ctx.caller.name.clone());
            let all = bool_field(body, "all").unwrap_or(false);
            let offers: Vec<OfferRow> = query_offers(ctx.store.connection())?
                .into_iter()
                .filter(|o| o.state == "open")
                .filter(|o| all || o.to_owner == owner)
                .take(top)
                .collect();
            out.insert("offers".into(), json!(offers));
            Ok(out)
        }
        "task.heartbeat" => {
            let task_id = str_field(body, "task").filter(|s| !s.is_empty());
            let updated = touch_lease_heartbeat(
                ctx.store.connection(),
                &ctx.caller.name,
                task_id.as_deref(),
                now_sec,
            )?;
            out.insert("updated".into(), json!(updated));
            Ok(out)
        }
        other => Err(format!("unknown task op `{other}`")),
    }
}
