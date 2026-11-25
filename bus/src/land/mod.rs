//! Landing queue, batches, branch sweep, and game runs (sections 3, 4.3, 5.2, step 10a).

pub mod schema;
pub mod sweep;

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};

use crate::hooks::OpContext;
use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;
use crate::task::{query_leases, query_tasks};
use crate::time::{parse_ms, rfc3339};

pub use schema::{
    BatchMember, BatchRow, BranchRow, LAND_SCHEMA_SQL, LAND_TABLES, LandingRow, RunRow,
    insert_landing_if_absent_tx, query_batch, query_batches, query_branch, query_branches,
    query_landing, query_landings, query_run, query_runs, upsert_batch_tx, upsert_branch_tx,
    upsert_landing_tx, upsert_run_tx,
};
pub use sweep::{
    BranchSweepResult, DeleteRemoteBranch, RunFolderInfo, RunSweepResult, SweepConfig,
    branch_sweep, build_land_alert_message, build_triage_task_message, is_our_namespace,
    normalize_branch_short_name, run_sweep, scan_licence_markers,
};

/// Store projection module for `LANDING`, `BATCH`, `BRANCH`, and `RUN` (section 10.2).
pub struct LandModule;

impl StoreModule for LandModule {
    fn name(&self) -> &'static str {
        "land"
    }

    fn schema(&self) -> &'static str {
        LAND_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_land_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_land_api(conn, now_sec, state)
    }
}

/// Initializes the landing/run tables in `conn`.
pub fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(LAND_SCHEMA_SQL)
        .map_err(|e| format!("land schema error: {e}"))
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

fn is_trusted_sender(tx: &Transaction<'_>, msg: &Message) -> bool {
    if matches!(msg.role, Role::Orchestrator | Role::Hub) {
        return true;
    }
    if msg.from.is_empty() {
        return false;
    }
    let trust: Option<String> = tx
        .query_row(
            "SELECT trust FROM MEMBER WHERE name = ?1",
            params![msg.from],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    matches!(trust.as_deref(), Some("owner" | "trusted"))
}

fn extract_ready_ref_sha_task(msg: &Message) -> (String, String, String) {
    let mut ref_name = String::new();
    let mut sha = String::new();
    let mut task_id = String::new();

    for r in &msg.refs {
        if let Some(v) = r.strip_prefix("branch:").or_else(|| r.strip_prefix("ref:")) {
            if ref_name.is_empty() {
                ref_name = v.trim().to_owned();
            }
        } else if let Some(v) = r.strip_prefix("sha:").or_else(|| r.strip_prefix("commit:")) {
            if sha.is_empty() {
                sha = v.trim().to_owned();
            }
        } else if let Some(v) = r.strip_prefix("task:")
            && task_id.is_empty()
        {
            task_id = v.trim().to_owned();
        }
    }

    if let Ok(v) = serde_json::from_str::<Value>(&msg.body) {
        if ref_name.is_empty() {
            ref_name = str_field(&v, "ref")
                .or_else(|| str_field(&v, "branch"))
                .unwrap_or_default();
        }
        if sha.is_empty() {
            sha = str_field(&v, "sha")
                .or_else(|| str_field(&v, "commit"))
                .unwrap_or_default();
        }
        if task_id.is_empty() {
            task_id = str_field(&v, "task_id")
                .or_else(|| str_field(&v, "task"))
                .unwrap_or_default();
        }
    }

    (ref_name, sha, task_id)
}

/// Applies a single bus message to `LANDING`, `BATCH`, `BRANCH`, and `RUN` inside `tx`.
pub fn apply_land_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    let ts_sec = parse_ms(&msg.ts).unwrap_or(0) / 1000;

    match msg.kind {
        Kind::Fact => {
            if msg.topic.starts_with("land/") {
                let v: Value = serde_json::from_str(&msg.body)
                    .map_err(|e| format!("invalid land FACT JSON: {e}"))?;
                let row_val = v.get("row").unwrap_or(&v);
                let mut row: LandingRow = serde_json::from_value(row_val.clone())
                    .map_err(|e| format!("invalid LandingRow in FACT: {e}"))?;
                if row.ref_name.is_empty() {
                    row.ref_name = msg.topic.trim_start_matches("land/").to_owned();
                }
                if row.enqueued_at == 0 {
                    row.enqueued_at = ts_sec;
                }
                row.msg_id = msg.id;
                upsert_landing_tx(tx, &row)?;
            } else if msg.topic.starts_with("batch/") {
                let v: Value = serde_json::from_str(&msg.body)
                    .map_err(|e| format!("invalid batch FACT JSON: {e}"))?;
                let row_val = v.get("row").unwrap_or(&v);
                let mut row: BatchRow = serde_json::from_value(row_val.clone())
                    .map_err(|e| format!("invalid BatchRow in FACT: {e}"))?;
                if row.batch_id.is_empty() {
                    row.batch_id = msg.topic.trim_start_matches("batch/").to_owned();
                }
                row.msg_id = msg.id;
                upsert_batch_tx(tx, &row)?;
            } else if msg.topic.starts_with("branch/") {
                let v: Value = serde_json::from_str(&msg.body)
                    .map_err(|e| format!("invalid branch FACT JSON: {e}"))?;
                let row_val = v.get("row").unwrap_or(&v);
                let mut row: BranchRow = serde_json::from_value(row_val.clone())
                    .map_err(|e| format!("invalid BranchRow in FACT: {e}"))?;
                if row.seen_at == 0 {
                    row.seen_at = ts_sec;
                }
                row.msg_id = msg.id;
                upsert_branch_tx(tx, &row)?;
            } else if msg.topic.starts_with("run/") {
                let v: Value = serde_json::from_str(&msg.body)
                    .map_err(|e| format!("invalid run FACT JSON: {e}"))?;
                let row_val = v.get("row").unwrap_or(&v);
                let mut row: RunRow = serde_json::from_value(row_val.clone())
                    .map_err(|e| format!("invalid RunRow in FACT: {e}"))?;
                if row.run_id.is_empty() {
                    row.run_id = msg.topic.trim_start_matches("run/").to_owned();
                }
                row.msg_id = msg.id;
                upsert_run_tx(tx, &row)?;
            }
        }
        Kind::Ready => {
            if is_trusted_sender(tx, msg) {
                let (ref_name, sha, task_id) = extract_ready_ref_sha_task(msg);
                if !ref_name.is_empty() && !sha.is_empty() {
                    let row = LandingRow {
                        ref_name,
                        sha,
                        task_id,
                        state: "queued".into(),
                        priority: "normal".into(),
                        enqueued_at: ts_sec,
                        msg_id: msg.id,
                        ..LandingRow::default()
                    };
                    insert_landing_if_absent_tx(tx, &row)?;
                }
            }
        }
        Kind::Landed => {
            let (ref_name, sha, _) = extract_ready_ref_sha_task(msg);
            if !ref_name.is_empty() && !sha.is_empty() {
                tx.execute(
                    "UPDATE LANDING SET state = 'landed', msg_id = ?1 WHERE ref_name = ?2 AND sha = ?3",
                    params![msg.id as i64, ref_name, sha],
                )
                .map_err(|e| format!("update LANDING on LANDED error: {e}"))?;
            }
        }
        _ => {}
    }

    Ok(())
}

/// Adds `landing`, `batches`, `branches`, and `runs` to `/api/state` (section 10.2).
pub fn build_land_api(
    conn: &Connection,
    _now_sec: u64,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let landing = query_landings(conn)?;
    let batches = query_batches(conn)?;
    let branches = query_branches(conn)?;
    let runs = query_runs(conn)?;

    state.insert("landing".into(), json!(landing));
    state.insert("batches".into(), json!(batches));
    state.insert("branches".into(), json!(branches));
    state.insert("runs".into(), json!(runs));
    Ok(())
}

fn priority_rank(p: &str) -> u8 {
    match p {
        "blocker" => 4,
        "high" => 3,
        "normal" => 2,
        "filler" => 1,
        _ => 2,
    }
}

fn require_orchestrator(ctx: &OpContext<'_>, op: &str) -> Result<(), String> {
    if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
        return Err(format!("only an orchestrator may run `{op}`"));
    }
    Ok(())
}

fn fact_msg(topic: String, row: Value, ts: String) -> Message {
    let mut body_obj = match row {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("row".into(), other);
            m
        }
    };
    body_obj.insert("v".into(), json!(1));
    Message {
        id: 0,
        ts,
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic,
        body: Value::Object(body_obj).to_string(),
        refs: Vec::new(),
        to: Vec::new(),
    }
}

fn query_open_alert_keys(conn: &Connection, code: &str) -> Result<BTreeSet<String>, String> {
    let mut stmt = conn
        .prepare("SELECT key FROM ALERT WHERE code = ?1 AND state != 'closed'")
        .map_err(|e| format!("query ALERT keys error: {e}"))?;
    let rows = stmt
        .query_map(params![code], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query ALERT keys map error: {e}"))?;
    let mut out = BTreeSet::new();
    for r in rows {
        out.insert(r.map_err(|e| format!("query ALERT key row error: {e}"))?);
    }
    Ok(out)
}

/// Handles `land.*` and `run.*` module ops (section 5.1, 5.2, step 10a).
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let now_sec = ctx.now_ms / 1000;
    let ts = rfc3339(ctx.now_ms);
    let mut out = Map::new();

    match op {
        "land.enqueue" => {
            let ref_name = str_field(body, "ref")
                .or_else(|| str_field(body, "branch"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "land.enqueue requires `ref`".to_owned())?;
            let sha = str_field(body, "sha")
                .or_else(|| str_field(body, "commit"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "land.enqueue requires `sha`".to_owned())?;

            if let Some(existing) = query_landing(ctx.store.connection(), &ref_name, &sha)? {
                out.insert("duplicate".into(), json!(true));
                out.insert("landing".into(), json!(existing));
                return Ok(out);
            }

            let supplied_approver = str_field(body, "approved_by").unwrap_or_default();
            if !supplied_approver.is_empty() {
                require_orchestrator(ctx, "land.approve")?;
            }

            let row = LandingRow {
                ref_name: ref_name.clone(),
                sha,
                task_id: str_field(body, "task_id").unwrap_or_default(),
                state: "queued".into(),
                priority: str_field(body, "priority")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "normal".into()),
                approved_by: supplied_approver,
                enqueued_at: now_sec,
                ..LandingRow::default()
            };
            let msg_id = ctx.emit(fact_msg(format!("land/{ref_name}"), json!(row), ts))?;
            out.insert("duplicate".into(), json!(false));
            out.insert("landing".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "land.next" => {
            let limit = body
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(8)
                .clamp(1, 64) as usize;
            let claim = body.get("claim").and_then(Value::as_bool).unwrap_or(false);
            let base_sha = str_field(body, "base_sha").unwrap_or_default();

            let mut queued: Vec<LandingRow> = query_landings(ctx.store.connection())?
                .into_iter()
                .filter(|l| l.state == "queued")
                .collect();
            queued.sort_by(|a, b| {
                priority_rank(&b.priority)
                    .cmp(&priority_rank(&a.priority))
                    .then_with(|| a.enqueued_at.cmp(&b.enqueued_at))
                    .then_with(|| a.ref_name.cmp(&b.ref_name))
            });
            queued.truncate(limit);

            if claim && !queued.is_empty() {
                let batch_id = str_field(body, "batch_id")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("batch-{now_sec}"));
                let members: Vec<BatchMember> = queued
                    .iter()
                    .map(|l| BatchMember {
                        ref_name: l.ref_name.clone(),
                        sha: l.sha.clone(),
                    })
                    .collect();
                let batch = BatchRow {
                    batch_id: batch_id.clone(),
                    base_sha,
                    members,
                    parent_batch: str_field(body, "parent_batch").unwrap_or_default(),
                    verifications: 0,
                    result: "running".into(),
                    started_at: now_sec,
                    ended_at: 0,
                    msg_id: 0,
                };
                ctx.emit(fact_msg(
                    format!("batch/{batch_id}"),
                    json!(batch),
                    ts.clone(),
                ))?;
                for l in &mut queued {
                    l.state = "batched".into();
                    l.batch_id = batch_id.clone();
                    ctx.emit(fact_msg(
                        format!("land/{}", l.ref_name),
                        json!(l),
                        ts.clone(),
                    ))?;
                }
                out.insert("batch_id".into(), json!(batch_id));
            }

            out.insert("entries".into(), json!(queued));
            Ok(out)
        }
        "land.result" => {
            let batch_id = str_field(body, "batch_id").unwrap_or_default();
            let result = str_field(body, "result")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "land.result requires `result`".to_owned())?;
            let fail_step = str_field(body, "fail_step").unwrap_or_default();
            let fail_class = str_field(body, "fail_class").unwrap_or_default();
            let log = str_field(body, "log").unwrap_or_default();
            let verifications = body
                .get("verifications")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;

            let specific_ref = str_field(body, "ref").filter(|s| !s.is_empty());
            let specific_sha = str_field(body, "sha").filter(|s| !s.is_empty());

            let mut updated_landings = Vec::new();
            if !batch_id.is_empty() {
                let mut batch =
                    query_batch(ctx.store.connection(), &batch_id)?.unwrap_or_else(|| BatchRow {
                        batch_id: batch_id.clone(),
                        started_at: now_sec,
                        ..BatchRow::default()
                    });
                batch.result = result.clone();
                batch.verifications = batch.verifications.max(verifications);
                batch.ended_at = now_sec;
                if let Some(base) = str_field(body, "base_sha")
                    && !base.is_empty()
                {
                    batch.base_sha = base;
                }
                if let (Some(ref_name), Some(sha)) = (&specific_ref, &specific_sha)
                    && !batch
                        .members
                        .iter()
                        .any(|m| m.ref_name == *ref_name && m.sha == *sha)
                {
                    batch.members.push(BatchMember {
                        ref_name: ref_name.clone(),
                        sha: sha.clone(),
                    });
                }
                ctx.emit(fact_msg(
                    format!("batch/{batch_id}"),
                    json!(batch),
                    ts.clone(),
                ))?;

                if specific_ref.is_none() {
                    for l in query_landings(ctx.store.connection())? {
                        if l.batch_id == batch_id {
                            let mut next = l;
                            next.state = result.clone();
                            next.tries = next.tries.saturating_add(1);
                            next.fail_step = fail_step.clone();
                            next.fail_class = fail_class.clone();
                            next.log = log.clone();
                            ctx.emit(fact_msg(
                                format!("land/{}", next.ref_name),
                                json!(next),
                                ts.clone(),
                            ))?;
                            updated_landings.push(next);
                        }
                    }
                }
            }

            if let (Some(ref_name), Some(sha)) = (specific_ref, specific_sha)
                && !updated_landings
                    .iter()
                    .any(|l| l.ref_name == ref_name && l.sha == sha)
            {
                let mut row = query_landing(ctx.store.connection(), &ref_name, &sha)?
                    .unwrap_or_else(|| LandingRow {
                        ref_name: ref_name.clone(),
                        sha: sha.clone(),
                        enqueued_at: now_sec,
                        ..LandingRow::default()
                    });
                row.state = result.clone();
                row.batch_id = batch_id.clone();
                row.tries = row.tries.saturating_add(1);
                row.fail_step = fail_step.clone();
                row.fail_class = fail_class.clone();
                row.log = log.clone();
                ctx.emit(fact_msg(format!("land/{ref_name}"), json!(row), ts.clone()))?;
                updated_landings.push(row);
            }

            let alert_code = match result.as_str() {
                "failed" => Some("LANDING_FAILED"),
                "conflict" => Some("LANDING_CONFLICT"),
                "held" => Some("LANDING_HELD"),
                _ => None,
            };
            if let Some(code) = alert_code {
                let key = if !batch_id.is_empty() {
                    batch_id.clone()
                } else {
                    updated_landings
                        .first()
                        .map(|l| format!("{}@{}", l.ref_name, l.sha))
                        .unwrap_or_else(|| "unknown".into())
                };
                ctx.emit(build_land_alert_message(
                    code,
                    &key,
                    "warn",
                    &format!("landing {result} for {key}"),
                    json!({ "batch_id": batch_id, "result": result, "fail_step": fail_step, "fail_class": fail_class }),
                    ctx.now_ms,
                ))?;
            }

            out.insert("updated".into(), json!(updated_landings));
            Ok(out)
        }
        "land.approve" => {
            require_orchestrator(ctx, op)?;
            let ref_name = str_field(body, "ref")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "land.approve requires `ref`".to_owned())?;
            let sha = str_field(body, "sha")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "land.approve requires `sha`".to_owned())?;
            let approver = str_field(body, "by")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| ctx.caller.name.clone());

            let mut row =
                query_landing(ctx.store.connection(), &ref_name, &sha)?.unwrap_or_else(|| {
                    LandingRow {
                        ref_name: ref_name.clone(),
                        sha: sha.clone(),
                        state: "queued".into(),
                        priority: "normal".into(),
                        enqueued_at: now_sec,
                        ..LandingRow::default()
                    }
                });
            row.approved_by = approver;
            if row.state == "held" {
                row.state = "queued".into();
            }
            let msg_id = ctx.emit(fact_msg(format!("land/{ref_name}"), json!(row), ts))?;
            out.insert("landing".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "land.list" => {
            let state_filter = str_field(body, "state").filter(|s| !s.is_empty());
            let mut landing = query_landings(ctx.store.connection())?;
            if let Some(ref st) = state_filter {
                landing.retain(|l| l.state == *st);
            }
            let batches = query_batches(ctx.store.connection())?;
            let branches = query_branches(ctx.store.connection())?;
            out.insert("landing".into(), json!(landing));
            out.insert("batches".into(), json!(batches));
            out.insert("branches".into(), json!(branches));
            Ok(out)
        }
        "land.branch_upsert" => {
            let row: BranchRow = serde_json::from_value(body.clone())
                .map_err(|e| format!("invalid branch payload: {e}"))?;
            if row.ref_name.is_empty() {
                return Err("land.branch_upsert requires `ref`".into());
            }
            let topic = if row.remote.is_empty() {
                format!("branch/origin/{}", row.ref_name)
            } else {
                format!("branch/{}/{}", row.remote, row.ref_name)
            };
            let msg_id = ctx.emit(fact_msg(topic, json!(row), ts))?;
            out.insert("branch".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "land.sweep" => {
            if let Some(branches_val) = body.get("branches").and_then(Value::as_array) {
                for bv in branches_val {
                    if let Ok(b) = serde_json::from_value::<BranchRow>(bv.clone())
                        && !b.ref_name.is_empty()
                    {
                        let topic = if b.remote.is_empty() {
                            format!("branch/origin/{}", b.ref_name)
                        } else {
                            format!("branch/{}/{}", b.remote, b.ref_name)
                        };
                        ctx.emit(fact_msg(topic, json!(b), ts.clone()))?;
                    }
                }
            }

            let mut cfg = SweepConfig::default();
            if let Some(h) = body.get("orphan_hours").and_then(Value::as_u64) {
                cfg.orphan_hours = h;
            }
            if let Some(d) = body.get("keep_days").and_then(Value::as_u64) {
                cfg.keep_days = d;
            }

            let conn = ctx.store.connection();
            let branches = query_branches(conn)?;
            let landings = query_landings(conn)?;
            let tasks = query_tasks(conn)?;
            let leases = query_leases(conn)?;
            let open_alerts = query_open_alert_keys(conn, "ORPHAN_BRANCH")?;
            let existing_triage_ids: BTreeSet<String> = tasks
                .iter()
                .filter(|t| t.kind == "triage")
                .map(|t| t.id.clone())
                .collect();

            let sweep_now = body
                .get("now_sec")
                .and_then(Value::as_u64)
                .unwrap_or(now_sec);

            let res = branch_sweep(
                &branches,
                &landings,
                &tasks,
                &leases,
                &open_alerts,
                &existing_triage_ids,
                sweep_now,
                &cfg,
            );

            for b in &res.updated_branches {
                let topic = if b.remote.is_empty() {
                    format!("branch/origin/{}", b.ref_name)
                } else {
                    format!("branch/{}/{}", b.remote, b.ref_name)
                };
                ctx.emit(fact_msg(topic, json!(b), ts.clone()))?;
            }
            for (key, text) in &res.orphan_alerts {
                ctx.emit(build_land_alert_message(
                    "ORPHAN_BRANCH",
                    key,
                    "warn",
                    text,
                    json!({ "branch": key }),
                    ctx.now_ms,
                ))?;
            }
            for task in &res.triage_tasks {
                ctx.emit(build_triage_task_message(task, ctx.now_ms))?;
            }

            out.insert("sweep".into(), json!(res));
            Ok(out)
        }
        "run.request" => {
            let sha = str_field(body, "sha")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "run.request requires `sha`".to_owned())?;
            let run_id = str_field(body, "run_id")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("run-{now_sec}-{}", &sha[..sha.len().min(8)]));
            let row = RunRow {
                run_id: run_id.clone(),
                machine: str_field(body, "machine").unwrap_or_default(),
                sha,
                label: str_field(body, "label").unwrap_or_else(|| "requested".into()),
                kind: str_field(body, "kind")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "check".into()),
                task_id: str_field(body, "task_id").unwrap_or_default(),
                requested_by: ctx.caller.name.clone(),
                started_at: now_sec,
                ended_at: 0,
                outcome: "requested".into(),
                folder: String::new(),
                registered: true,
                msg_id: 0,
            };
            let msg_id = ctx.emit(fact_msg(format!("run/{run_id}"), json!(row), ts))?;
            out.insert("run".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "run.register" => {
            let run_id = str_field(body, "run_id")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("run-{now_sec}"));
            let existing = query_run(ctx.store.connection(), &run_id)?;
            let row = RunRow {
                run_id: run_id.clone(),
                machine: str_field(body, "machine")
                    .or_else(|| existing.as_ref().map(|r| r.machine.clone()))
                    .unwrap_or_default(),
                sha: str_field(body, "sha")
                    .or_else(|| existing.as_ref().map(|r| r.sha.clone()))
                    .unwrap_or_default(),
                label: str_field(body, "label")
                    .or_else(|| existing.as_ref().map(|r| r.label.clone()))
                    .unwrap_or_default(),
                kind: str_field(body, "kind")
                    .filter(|s| !s.is_empty())
                    .or_else(|| existing.as_ref().map(|r| r.kind.clone()))
                    .unwrap_or_else(|| "check".into()),
                task_id: str_field(body, "task_id")
                    .or_else(|| existing.as_ref().map(|r| r.task_id.clone()))
                    .unwrap_or_default(),
                requested_by: str_field(body, "requested_by")
                    .or_else(|| existing.as_ref().map(|r| r.requested_by.clone()))
                    .unwrap_or_else(|| ctx.caller.name.clone()),
                started_at: body
                    .get("started_at")
                    .and_then(Value::as_u64)
                    .or_else(|| existing.as_ref().map(|r| r.started_at))
                    .unwrap_or(now_sec),
                ended_at: existing.as_ref().map_or(0, |r| r.ended_at),
                outcome: existing
                    .as_ref()
                    .map(|r| r.outcome.clone())
                    .unwrap_or_default(),
                folder: str_field(body, "folder")
                    .or_else(|| existing.as_ref().map(|r| r.folder.clone()))
                    .unwrap_or_default(),
                registered: body
                    .get("registered")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                msg_id: 0,
            };
            let msg_id = ctx.emit(fact_msg(format!("run/{run_id}"), json!(row), ts))?;
            out.insert("run".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "run.result" => {
            let run_id = str_field(body, "run_id")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "run.result requires `run_id`".to_owned())?;
            let outcome = str_field(body, "outcome")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "run.result requires `outcome`".to_owned())?;
            let mut row = query_run(ctx.store.connection(), &run_id)?.unwrap_or_else(|| RunRow {
                run_id: run_id.clone(),
                kind: "check".into(),
                started_at: now_sec,
                registered: true,
                ..RunRow::default()
            });
            row.outcome = outcome;
            row.ended_at = body
                .get("ended_at")
                .and_then(Value::as_u64)
                .unwrap_or(now_sec);
            if let Some(folder) = str_field(body, "folder")
                && !folder.is_empty()
            {
                row.folder = folder;
            }
            if let Some(sha) = str_field(body, "sha")
                && !sha.is_empty()
            {
                row.sha = sha;
            }
            if let Some(label) = str_field(body, "label")
                && !label.is_empty()
            {
                row.label = label;
            }
            let msg_id = ctx.emit(fact_msg(format!("run/{run_id}"), json!(row), ts))?;
            out.insert("run".into(), json!(row));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        "run.list" => {
            let sha_filter = str_field(body, "sha").filter(|s| !s.is_empty());
            let unregistered_only = body
                .get("unregistered")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut runs = query_runs(ctx.store.connection())?;
            if let Some(ref s) = sha_filter {
                runs.retain(|r| r.sha == *s);
            }
            if unregistered_only {
                runs.retain(|r| !r.registered);
            }
            out.insert("runs".into(), json!(runs));
            Ok(out)
        }
        "run.sweep" => {
            let folders: Vec<RunFolderInfo> = body
                .get("folders")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| format!("invalid folders list: {e}"))?
                .unwrap_or_default();
            let conn = ctx.store.connection();
            let existing = query_runs(conn)?;
            let open_alerts = query_open_alert_keys(conn, "UNREGISTERED_RUN")?;
            let res = run_sweep(&folders, &existing, &open_alerts);
            for r in &res.unregistered_runs {
                ctx.emit(fact_msg(format!("run/{}", r.run_id), json!(r), ts.clone()))?;
            }
            for (key, text) in &res.alerts {
                ctx.emit(build_land_alert_message(
                    "UNREGISTERED_RUN",
                    key,
                    "warn",
                    text,
                    json!({ "run_id": key }),
                    ctx.now_ms,
                ))?;
            }
            out.insert("sweep".into(), json!(res));
            Ok(out)
        }
        other => Err(format!("unknown land/run op `{other}`")),
    }
}
