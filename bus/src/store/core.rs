//! Core table projections and `/api/state` representation (section 3, 5.2, 10.2).

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::derive::capacity::{MAX_CLASS_SLOTS, MAX_CLASSES, MAX_TOTAL_SLOTS};
use crate::derive::parse_hello_capacity;
use crate::msg::{Kind, Message, Role};
use crate::time::parse_ms;

use super::registry::StoreModule;
use super::schema::CORE_SCHEMA_SQL;

/// Built-in store module for the core tables (`MEMBER`, `MACHINE`, `CAPACITY`, `AGENT`, `ALERT`, `POLICY`, `META`).
pub struct CoreModule;

impl StoreModule for CoreModule {
    fn name(&self) -> &'static str {
        "core"
    }

    fn schema(&self) -> &'static str {
        CORE_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        apply_core_message(tx, msg)
    }

    fn api(
        &self,
        conn: &Connection,
        now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        build_core_api(conn, now_sec, state)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRow {
    pub name: String,
    pub role: String,
    pub trust: String,
    pub online: bool,
    pub last_seen_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRow {
    pub owner: String,
    pub machine: String,
    pub os: String,
    pub gpu: String,
    pub machine_class: String,
    pub gpu_slots: u32,
    pub disk_free_gb: u64,
    pub agent_version: String,
    pub policy_hash: String,
    pub heartbeat_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityRow {
    pub owner: String,
    pub machine: String,
    pub class: String,
    pub mode: String,
    pub useful_parallel: u32,
    pub accepts: Vec<String>,
    pub pools: Vec<String>,
    pub declared_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub owner: String,
    pub name: String,
    pub machine: String,
    pub class: String,
    pub model: String,
    pub task_id: String,
    pub branch: String,
    pub pid_alive: bool,
    pub reported_state: String,
    pub text: String,
    pub source: String,
    pub as_of: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertTableRow {
    pub code: String,
    pub key: String,
    pub severity: String,
    pub to: String,
    pub state: String,
    pub opened_at: u64,
    pub last_at: u64,
    pub count: u64,
    pub text: String,
    pub data: Value,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRow {
    pub version: u64,
    pub hash: String,
    pub mode: String,
    pub published_at: u64,
    pub msg_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaRow {
    pub name: String,
    pub value: String,
    pub msg_id: u64,
}

/// Trust of a key member: it is no token guest and never a candidate for work.
pub const KEY_MEMBER_TRUST: &str = crate::key::ops::TRUST;

/// True when `name` is a member id that the KEY table knows. A store without that table (a
/// core only test database) knows no key member.
fn is_key_member(tx: &Transaction<'_>, name: &str) -> bool {
    crate::key::crypto::is_member_id(name)
        && tx
            .query_row(
                r#"SELECT COUNT(*) FROM "KEY" WHERE member = ?1"#,
                params![name],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
}

/// True for a key member that has no capacity: its TIER row is missing or below 1. A key member at tier 1
/// or above has machines and capacity like a token member.
pub(crate) fn key_member_without_capacity(tx: &Transaction<'_>, name: &str) -> bool {
    is_key_member(tx, name) && crate::stamp::tier_table::tier_from_table(tx, name, None) < 1
}

fn role_trust(role: Role) -> &'static str {
    match role {
        Role::Orchestrator | Role::Hub => "owner",
        Role::Member => "guest",
        Role::Observer => "observer",
    }
}

/// Applies one message to the core tables within an open SQLite transaction.
pub fn apply_core_message(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    let ts_ms = parse_ms(&msg.ts).unwrap_or(0);
    let ts_sec = (ts_ms / 1000) as i64;
    let msg_id = msg.id as i64;

    if msg.role != Role::Hub && !msg.from.is_empty() {
        let role_s = msg.role.as_str();
        let trust_s = if is_key_member(tx, &msg.from) {
            KEY_MEMBER_TRUST
        } else {
            role_trust(msg.role)
        };
        if msg.kind == Kind::Status {
            tx.execute(
                "INSERT INTO MEMBER (name, role, trust, online, last_seen_at, msg_id)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)
                 ON CONFLICT(name) DO UPDATE SET
                     role = excluded.role,
                     trust = excluded.trust,
                     msg_id = excluded.msg_id",
                params![msg.from, role_s, trust_s, ts_sec, msg_id],
            )
            .map_err(|e| e.to_string())?;
        } else {
            tx.execute(
                "INSERT INTO MEMBER (name, role, trust, online, last_seen_at, msg_id)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)
                 ON CONFLICT(name) DO UPDATE SET
                     role = excluded.role,
                     trust = excluded.trust,
                     online = 1,
                     last_seen_at = MAX(MEMBER.last_seen_at, excluded.last_seen_at),
                     msg_id = excluded.msg_id",
                params![msg.from, role_s, trust_s, ts_sec, msg_id],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // A key member below tier 1 declares no machine and no capacity: old lines in a log change nothing.
    let capacity_line =
        msg.kind == Kind::Hello || (msg.kind == Kind::Fact && msg.topic.starts_with("capacity/"));
    if capacity_line && (key_member_without_capacity(tx, &msg.from) || topic_owner_is_key(tx, msg))
    {
        return Ok(());
    }
    // A member declares capacity for itself only. The owner of a capacity FACT is the sender, never the topic.
    if msg.kind == Kind::Fact
        && msg.topic.starts_with("capacity/")
        && !crate::role::writers::own_topic_ok(msg)
    {
        return Ok(());
    }

    match msg.kind {
        Kind::Hello => {
            apply_hello_or_capacity(
                tx, &msg.from, &msg.topic, &msg.body, &msg.refs, ts_ms, ts_sec, msg_id,
            )?;
        }
        Kind::Fact if msg.topic.starts_with("capacity/") => {
            apply_capacity_fact(tx, msg, ts_ms, ts_sec, msg_id)?;
        }
        Kind::Fact if msg.topic.starts_with("exit/") => {
            apply_exit_fact(tx, msg, ts_sec, msg_id)?;
        }
        Kind::Status => {
            apply_status(tx, msg, ts_sec, msg_id)?;
        }
        // Only the hub writes alerts. A line of a member in an old log opens, changes or closes none.
        Kind::Alert if msg.role == Role::Hub => {
            apply_alert(tx, msg, ts_sec, msg_id)?;
        }
        Kind::Policy => {
            apply_policy(tx, msg, ts_sec, msg_id)?;
        }
        _ => {}
    }

    Ok(())
}

/// True for a `capacity/<owner>/...` FACT that names a key member below tier 1 as the owner.
fn topic_owner_is_key(tx: &Transaction<'_>, msg: &Message) -> bool {
    msg.topic
        .strip_prefix("capacity/")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|owner| key_member_without_capacity(tx, owner))
}

/// Most machines one owner keeps in the table. A declaration of a new machine past this is ignored.
pub const MAX_MACHINES_PER_OWNER: i64 = 16;

/// True when `owner` may keep a machine with this name: it is known already, or the owner is below the cap.
fn machine_room(tx: &Transaction<'_>, owner: &str, machine: &str) -> bool {
    let known: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM MACHINE WHERE owner = ?1 AND machine = ?2",
            params![owner, machine],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if known > 0 {
        return true;
    }
    let all: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM MACHINE WHERE owner = ?1",
            params![owner],
            |r| r.get(0),
        )
        .unwrap_or(0);
    all < MAX_MACHINES_PER_OWNER
}

/// Cuts `wanted` so that all slots of `owner` together stay within the total of one declaration. The cap holds per
/// owner, whatever the number of declarations or the machine names in them.
fn clamp_owner_slots(
    tx: &Transaction<'_>,
    owner: &str,
    machine: &str,
    class: &str,
    wanted: i64,
) -> i64 {
    let others: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(useful_parallel), 0) FROM CAPACITY
             WHERE owner = ?1 AND NOT (machine = ?2 AND class = ?3)",
            params![owner, machine, class],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let room = i64::from(MAX_TOTAL_SLOTS).saturating_sub(others).max(0);
    wanted.min(room)
}

#[allow(clippy::too_many_arguments)]
fn apply_hello_or_capacity(
    tx: &Transaction<'_>,
    owner: &str,
    topic: &str,
    body: &str,
    refs: &[String],
    ts_ms: u64,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let cap = parse_hello_capacity(body, refs, ts_ms);
    let extra = parse_extra_hello_kv(body, refs);
    let machine = if !cap.machine.is_empty() {
        cap.machine.clone()
    } else if let Some(rest) = topic.strip_prefix("capacity/") {
        rest.split('/').nth(1).unwrap_or(owner).to_owned()
    } else {
        owner.to_owned()
    };
    let machine_class = extra
        .get("machine_class")
        .cloned()
        .unwrap_or_else(|| infer_machine_class(&cap.os, &cap.gpu));
    let disk_free_gb = extra
        .get("disk_free_gb")
        .or_else(|| extra.get("disk"))
        .and_then(|s| s.trim_end_matches("gb").parse::<i64>().ok())
        .unwrap_or(0);
    let agent_version = extra
        .get("agent_version")
        .or_else(|| extra.get("version"))
        .cloned()
        .unwrap_or_default();
    let policy_hash = extra
        .get("policy_hash")
        .or_else(|| extra.get("policy"))
        .cloned()
        .unwrap_or_default();
    let pools_list: Vec<String> = extra
        .get("pools")
        .map(|s| {
            s.split([',', '+'])
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !machine_room(tx, owner, &machine) {
        return Ok(());
    }

    tx.execute(
        "INSERT INTO MACHINE (
            owner, machine, os, gpu, machine_class, gpu_slots,
            disk_free_gb, agent_version, policy_hash, heartbeat_at, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(owner, machine) DO UPDATE SET
            os = CASE WHEN excluded.os != '' THEN excluded.os ELSE MACHINE.os END,
            gpu = CASE WHEN excluded.gpu != '' THEN excluded.gpu ELSE MACHINE.gpu END,
            machine_class = CASE WHEN excluded.machine_class != '' THEN excluded.machine_class ELSE MACHINE.machine_class END,
            gpu_slots = excluded.gpu_slots,
            disk_free_gb = CASE WHEN excluded.disk_free_gb != 0 THEN excluded.disk_free_gb ELSE MACHINE.disk_free_gb END,
            agent_version = CASE WHEN excluded.agent_version != '' THEN excluded.agent_version ELSE MACHINE.agent_version END,
            policy_hash = CASE WHEN excluded.policy_hash != '' THEN excluded.policy_hash ELSE MACHINE.policy_hash END,
            heartbeat_at = MAX(MACHINE.heartbeat_at, excluded.heartbeat_at),
            msg_id = excluded.msg_id",
        params![
            owner,
            machine,
            cap.os,
            cap.gpu,
            machine_class,
            cap.gpu_slots as i64,
            disk_free_gb,
            agent_version,
            policy_hash,
            ts_sec,
            msg_id
        ],
    )
    .map_err(|e| e.to_string())?;

    let accepts_json = serde_json::to_string(&cap.accepts).unwrap_or_else(|_| "[]".into());
    if !cap.classes.is_empty() {
        tx.execute(
            "DELETE FROM CAPACITY WHERE owner = ?1 AND machine = ?2",
            params![owner, machine],
        )
        .map_err(|e| e.to_string())?;
        for (cls, &useful_parallel) in &cap.classes {
            let pools = if pools_list.is_empty() {
                default_pools_for_class(cls)
            } else {
                pools_list.clone()
            };
            let pools_json = serde_json::to_string(&pools).unwrap_or_else(|_| "[]".into());
            tx.execute(
                "INSERT INTO CAPACITY (
                    owner, machine, class, mode, useful_parallel, accepts, pools, declared_at, msg_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(owner, machine, class) DO UPDATE SET
                    mode = excluded.mode,
                    useful_parallel = excluded.useful_parallel,
                    accepts = excluded.accepts,
                    pools = excluded.pools,
                    declared_at = excluded.declared_at,
                    msg_id = excluded.msg_id",
                params![
                    owner,
                    machine,
                    cls,
                    cap.mode.as_str(),
                    clamp_owner_slots(tx, owner, &machine, cls, useful_parallel as i64),
                    accepts_json,
                    pools_json,
                    ts_sec,
                    msg_id
                ],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn apply_capacity_fact(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_ms: u64,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let parts: Vec<&str> = msg.topic.split('/').collect();
    let owner = parts
        .get(1)
        .copied()
        .filter(|s| !s.is_empty())
        .unwrap_or(&msg.from);
    let machine_from_topic = parts
        .get(2)
        .copied()
        .filter(|s| !s.is_empty())
        .unwrap_or(owner);

    if let Ok(v) = serde_json::from_str::<Value>(&msg.body)
        && v.is_object()
    {
        let machine = v["machine"].as_str().unwrap_or(machine_from_topic);
        if !machine_room(tx, owner, machine) {
            return Ok(());
        }
        let os = v["os"].as_str().unwrap_or("");
        let gpu = v["gpu"].as_str().unwrap_or("");
        let machine_class = v["machine_class"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| infer_machine_class(os, gpu));
        let gpu_slots = v["gpu_slots"]
            .as_u64()
            .unwrap_or(0)
            .min(u64::from(MAX_CLASS_SLOTS)) as i64;
        let disk_free_gb = v["disk_free_gb"].as_u64().unwrap_or(0) as i64;
        let agent_version = v["agent_version"].as_str().unwrap_or("");
        let policy_hash = v["policy_hash"].as_str().unwrap_or("");
        let heartbeat_at = v["heartbeat_at"].as_i64().unwrap_or(ts_sec);

        tx.execute(
            "INSERT INTO MACHINE (
                owner, machine, os, gpu, machine_class, gpu_slots,
                disk_free_gb, agent_version, policy_hash, heartbeat_at, msg_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(owner, machine) DO UPDATE SET
                os = CASE WHEN excluded.os != '' THEN excluded.os ELSE MACHINE.os END,
                gpu = CASE WHEN excluded.gpu != '' THEN excluded.gpu ELSE MACHINE.gpu END,
                machine_class = CASE WHEN excluded.machine_class != '' THEN excluded.machine_class ELSE MACHINE.machine_class END,
                gpu_slots = excluded.gpu_slots,
                disk_free_gb = CASE WHEN excluded.disk_free_gb != 0 THEN excluded.disk_free_gb ELSE MACHINE.disk_free_gb END,
                agent_version = CASE WHEN excluded.agent_version != '' THEN excluded.agent_version ELSE MACHINE.agent_version END,
                policy_hash = CASE WHEN excluded.policy_hash != '' THEN excluded.policy_hash ELSE MACHINE.policy_hash END,
                heartbeat_at = MAX(MACHINE.heartbeat_at, excluded.heartbeat_at),
                msg_id = excluded.msg_id",
            params![
                owner,
                machine,
                os,
                gpu,
                machine_class,
                gpu_slots,
                disk_free_gb,
                agent_version,
                policy_hash,
                heartbeat_at,
                msg_id
            ],
        )
        .map_err(|e| e.to_string())?;

        let mode = v["mode"].as_str().unwrap_or("auto");
        let declared_at = v["declared_at"].as_i64().unwrap_or(ts_sec);
        let accepts_json = if v["accepts"].is_array() {
            v["accepts"].to_string()
        } else {
            "[]".into()
        };
        let default_pools_json = if v["pools"].is_array() {
            Some(v["pools"].to_string())
        } else {
            None
        };

        if let Some(cls) = v["class"].as_str() {
            let useful_parallel = v["useful_parallel"]
                .as_u64()
                .unwrap_or(1)
                .min(u64::from(MAX_CLASS_SLOTS)) as i64;
            let useful_parallel = clamp_owner_slots(tx, owner, machine, cls, useful_parallel);
            let pools_json = default_pools_json.unwrap_or_else(|| {
                serde_json::to_string(&default_pools_for_class(cls)).unwrap_or_else(|_| "[]".into())
            });
            tx.execute(
                "INSERT INTO CAPACITY (
                    owner, machine, class, mode, useful_parallel, accepts, pools, declared_at, msg_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(owner, machine, class) DO UPDATE SET
                    mode = excluded.mode,
                    useful_parallel = excluded.useful_parallel,
                    accepts = excluded.accepts,
                    pools = excluded.pools,
                    declared_at = excluded.declared_at,
                    msg_id = excluded.msg_id",
                params![
                    owner,
                    machine,
                    cls,
                    mode,
                    useful_parallel,
                    accepts_json,
                    pools_json,
                    declared_at,
                    msg_id
                ],
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(classes_obj) = v["classes"].as_object() {
            tx.execute(
                "DELETE FROM CAPACITY WHERE owner = ?1 AND machine = ?2",
                params![owner, machine],
            )
            .map_err(|e| e.to_string())?;
            for (cls, count_v) in classes_obj.iter().take(MAX_CLASSES) {
                let useful_parallel = count_v
                    .as_u64()
                    .unwrap_or(0)
                    .min(u64::from(MAX_CLASS_SLOTS)) as i64;
                let useful_parallel = clamp_owner_slots(tx, owner, machine, cls, useful_parallel);
                let pools_json = default_pools_json.clone().unwrap_or_else(|| {
                    serde_json::to_string(&default_pools_for_class(cls))
                        .unwrap_or_else(|_| "[]".into())
                });
                tx.execute(
                    "INSERT INTO CAPACITY (
                        owner, machine, class, mode, useful_parallel, accepts, pools, declared_at, msg_id
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(owner, machine, class) DO UPDATE SET
                        mode = excluded.mode,
                        useful_parallel = excluded.useful_parallel,
                        accepts = excluded.accepts,
                        pools = excluded.pools,
                        declared_at = excluded.declared_at,
                        msg_id = excluded.msg_id",
                    params![
                        owner,
                        machine,
                        cls,
                        mode,
                        useful_parallel,
                        accepts_json,
                        pools_json,
                        declared_at,
                        msg_id
                    ],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        return Ok(());
    }

    apply_hello_or_capacity(
        tx, owner, &msg.topic, &msg.body, &msg.refs, ts_ms, ts_sec, msg_id,
    )
}

fn apply_exit_fact(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let parts: Vec<&str> = msg.topic.split('/').collect();
    let owner = parts.get(1).copied().unwrap_or(&msg.from);
    let Some(worker) = parts.get(2).copied().filter(|w| !w.is_empty()) else {
        return Ok(());
    };
    // A member reports the exit of its own agents. The hub reports for anyone.
    if msg.role != Role::Hub && owner != msg.from {
        return Ok(());
    }
    let state = serde_json::from_str::<Value>(&msg.body)
        .ok()
        .and_then(|v| {
            v["reason"]
                .as_str()
                .or_else(|| v["state"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "done".into());
    tx.execute(
        "UPDATE AGENT SET pid_alive = 0, reported_state = ?3, as_of = ?4, msg_id = ?5
         WHERE owner = ?1 AND name = ?2",
        params![owner, worker, state, ts_sec, msg_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply_status(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let Some(name) = msg.topic.strip_prefix("agent/").filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    let mut state = "running".to_owned();
    let mut model = String::new();
    let mut branch = String::new();
    let mut machine = String::new();
    let mut class = String::new();
    let mut task_id = String::new();
    let mut source = String::new();
    let mut pid_alive_opt: Option<bool> = None;

    for r in &msg.refs {
        if let Some(v) = r.strip_prefix("state:") {
            state = v.to_owned();
        } else if let Some(v) = r.strip_prefix("model:") {
            model = v.to_owned();
        } else if let Some(v) = r.strip_prefix("branch:") {
            branch = v.to_owned();
        } else if let Some(v) = r.strip_prefix("machine:") {
            machine = v.to_owned();
        } else if let Some(v) = r.strip_prefix("class:") {
            class = v.to_owned();
        } else if let Some(v) = r.strip_prefix("task:") {
            task_id = v.to_owned();
        } else if let Some(v) = r.strip_prefix("source:") {
            source = v.to_owned();
        } else if let Some(v) = r.strip_prefix("pid_alive:") {
            pid_alive_opt = Some(matches!(v, "1" | "true" | "yes"));
        }
    }

    if state == "done" {
        tx.execute(
            "DELETE FROM AGENT WHERE owner = ?1 AND name = ?2",
            params![msg.from, name],
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }

    if machine.is_empty() {
        machine = tx
            .query_row(
                "SELECT machine FROM MACHINE WHERE owner = ?1 ORDER BY heartbeat_at DESC LIMIT 1",
                params![msg.from],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| msg.from.clone());
    }
    if class.is_empty() {
        class = infer_class_from_model_branch(&model, &branch);
    }
    if source.is_empty() {
        source = if name == "main" || name == msg.from {
            "self".into()
        } else {
            "process".into()
        };
    }
    let pid_alive = pid_alive_opt.unwrap_or(state == "running") as i64;

    tx.execute(
        "INSERT INTO AGENT (
            owner, name, machine, class, model, task_id, branch,
            pid_alive, reported_state, text, source, as_of, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(owner, name) DO UPDATE SET
            machine = excluded.machine,
            class = excluded.class,
            model = excluded.model,
            task_id = excluded.task_id,
            branch = excluded.branch,
            pid_alive = excluded.pid_alive,
            reported_state = excluded.reported_state,
            text = excluded.text,
            source = excluded.source,
            as_of = excluded.as_of,
            msg_id = excluded.msg_id",
        params![
            msg.from, name, machine, class, model, task_id, branch, pid_alive, state, msg.body,
            source, ts_sec, msg_id
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply_alert(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let parsed: Option<Value> = serde_json::from_str(&msg.body).ok();
    let topic_rest = msg.topic.strip_prefix("alert/").unwrap_or(&msg.topic);
    let (topic_code, topic_key) = topic_rest.split_once('/').unwrap_or((topic_rest, ""));

    let code = parsed
        .as_ref()
        .and_then(|v| v["code"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_code)
        .to_owned();
    let key = parsed
        .as_ref()
        .and_then(|v| v["key"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(topic_key)
        .to_owned();
    if code.is_empty() {
        return Ok(());
    }
    let severity = parsed
        .as_ref()
        .and_then(|v| v["severity"].as_str())
        .unwrap_or("warn")
        .to_owned();
    let to = msg
        .to
        .first()
        .cloned()
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| v["to"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();

    let ref_state = msg
        .refs
        .iter()
        .find_map(|r| r.strip_prefix("state:"))
        .map(str::to_owned);
    let state = parsed
        .as_ref()
        .and_then(|v| v["state"].as_str())
        .map(str::to_owned)
        .or(ref_state)
        .unwrap_or_else(|| "open".into());

    let existing: Option<(i64, i64, String)> = tx
        .query_row(
            "SELECT opened_at, count, state FROM ALERT WHERE code = ?1 AND key = ?2",
            params![code, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();

    let opened_at = parsed
        .as_ref()
        .and_then(|v| v["opened_at"].as_i64())
        .unwrap_or_else(|| match &existing {
            Some((prev_open, _, prev_state)) if prev_state != "closed" => *prev_open,
            _ => ts_sec,
        });
    let last_at = parsed
        .as_ref()
        .and_then(|v| v["last_at"].as_i64())
        .unwrap_or(ts_sec);
    let count = parsed
        .as_ref()
        .and_then(|v| v["count"].as_i64())
        .unwrap_or_else(|| match &existing {
            Some((_, prev_count, prev_state)) if prev_state != "closed" && state == "open" => {
                prev_count + 1
            }
            Some((_, prev_count, _)) if state != "open" => *prev_count,
            _ => 1,
        });
    let text = parsed
        .as_ref()
        .and_then(|v| v["text"].as_str())
        .unwrap_or(&msg.body)
        .to_owned();
    let data = parsed
        .as_ref()
        .and_then(|v| v.get("data"))
        .map(|d| d.to_string())
        .unwrap_or_else(|| "{}".into());

    tx.execute(
        "INSERT INTO ALERT (
            code, key, severity, \"to\", state, opened_at, last_at, count, text, data, msg_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(code, key) DO UPDATE SET
            severity = excluded.severity,
            \"to\" = CASE WHEN excluded.\"to\" != '' THEN excluded.\"to\" ELSE ALERT.\"to\" END,
            state = excluded.state,
            opened_at = excluded.opened_at,
            last_at = excluded.last_at,
            count = excluded.count,
            text = excluded.text,
            data = excluded.data,
            msg_id = excluded.msg_id",
        params![
            code, key, severity, to, state, opened_at, last_at, count, text, data, msg_id
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply_policy(
    tx: &Transaction<'_>,
    msg: &Message,
    ts_sec: i64,
    msg_id: i64,
) -> Result<(), String> {
    let Ok(v) = serde_json::from_str::<Value>(&msg.body) else {
        return Ok(());
    };
    let Some(version) = v["version"].as_u64() else {
        return Ok(());
    };
    let hash = v["hash"].as_str().unwrap_or("");
    let mode = v["mode"].as_str().unwrap_or("off");
    let published_at = v["published_at"].as_i64().unwrap_or(ts_sec);

    tx.execute(
        "INSERT INTO POLICY (version, hash, mode, published_at, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(version) DO UPDATE SET
            hash = excluded.hash,
            mode = excluded.mode,
            published_at = excluded.published_at,
            msg_id = excluded.msg_id",
        params![version as i64, hash, mode, published_at, msg_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn parse_extra_hello_kv(body: &str, refs: &[String]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for seg in body.split(['|', ';']) {
        for part in seg.split_whitespace() {
            let trimmed = part.trim_matches(',');
            if let Some((k, v)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) {
                let key = k.trim().to_ascii_lowercase().replace('-', "_");
                let val = v.trim().trim_matches(',').to_owned();
                if !key.is_empty() && !val.is_empty() {
                    map.insert(key, val);
                }
            }
        }
    }
    for r in refs {
        if let Some((k, v)) = r.split_once(':').or_else(|| r.split_once('=')) {
            let key = k.trim().to_ascii_lowercase().replace('-', "_");
            let val = v.trim().to_owned();
            if !key.is_empty() && !val.is_empty() {
                map.insert(key, val);
            }
        }
    }
    map
}

fn infer_machine_class(os: &str, gpu: &str) -> String {
    let os_l = os.trim().to_ascii_lowercase();
    let gpu_l = gpu.trim().to_ascii_lowercase();
    match (os_l.is_empty(), gpu_l.is_empty()) {
        (true, true) => String::new(),
        (false, true) => os_l,
        (true, false) => gpu_l,
        (false, false) => format!("{os_l}-{gpu_l}"),
    }
}

fn default_pools_for_class(cls: &str) -> Vec<String> {
    let c = cls.to_ascii_lowercase();
    if c.starts_with("codex") {
        vec!["codex.5h".into(), "codex.week".into()]
    } else if c.starts_with("gemini") || c.starts_with("agy") {
        vec!["agy.window".into(), "agy.week".into(), "agy.day".into()]
    } else if c.starts_with("claude") {
        vec!["claude.5h".into(), "claude.week".into()]
    } else {
        Vec::new()
    }
}

fn infer_class_from_model_branch(model: &str, branch: &str) -> String {
    let m = model.to_ascii_lowercase();
    let b = branch.to_ascii_lowercase();
    if m.contains("codex") || b.starts_with("codex/") {
        "codex".into()
    } else if m.contains("gemini") || b.starts_with("gemini/") {
        "gemini".into()
    } else if m.contains("claude")
        || m.contains("sonnet")
        || m.contains("opus")
        || m.contains("fable")
    {
        "claude".into()
    } else {
        "default".into()
    }
}

/// Reads all rows of `MEMBER` ordered by `name`.
pub fn query_members(conn: &Connection) -> Result<Vec<MemberRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name, role, trust, online, last_seen_at, msg_id FROM MEMBER ORDER BY name ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MemberRow {
                name: r.get(0)?,
                role: r.get(1)?,
                trust: r.get(2)?,
                online: r.get::<_, i64>(3)? != 0,
                last_seen_at: r.get::<_, i64>(4)?.max(0) as u64,
                msg_id: r.get::<_, i64>(5)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `MACHINE` ordered by `(owner, machine)`.
pub fn query_machines(conn: &Connection) -> Result<Vec<MachineRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, machine, os, gpu, machine_class, gpu_slots, disk_free_gb,
                    agent_version, policy_hash, heartbeat_at, msg_id
             FROM MACHINE ORDER BY owner ASC, machine ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MachineRow {
                owner: r.get(0)?,
                machine: r.get(1)?,
                os: r.get(2)?,
                gpu: r.get(3)?,
                machine_class: r.get(4)?,
                gpu_slots: r.get::<_, i64>(5)?.max(0) as u32,
                disk_free_gb: r.get::<_, i64>(6)?.max(0) as u64,
                agent_version: r.get(7)?,
                policy_hash: r.get(8)?,
                heartbeat_at: r.get::<_, i64>(9)?.max(0) as u64,
                msg_id: r.get::<_, i64>(10)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `CAPACITY` ordered by `(owner, machine, class)`.
pub fn query_capacities(conn: &Connection) -> Result<Vec<CapacityRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, machine, class, mode, useful_parallel, accepts, pools, declared_at, msg_id
             FROM CAPACITY ORDER BY owner ASC, machine ASC, class ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let accepts_s: String = r.get(5)?;
            let pools_s: String = r.get(6)?;
            let accepts: Vec<String> = serde_json::from_str(&accepts_s).unwrap_or_default();
            let pools: Vec<String> = serde_json::from_str(&pools_s).unwrap_or_default();
            Ok(CapacityRow {
                owner: r.get(0)?,
                machine: r.get(1)?,
                class: r.get(2)?,
                mode: r.get(3)?,
                useful_parallel: r.get::<_, i64>(4)?.max(0) as u32,
                accepts,
                pools,
                declared_at: r.get::<_, i64>(7)?.max(0) as u64,
                msg_id: r.get::<_, i64>(8)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `AGENT` ordered by `(owner, name)`.
pub fn query_agents(conn: &Connection) -> Result<Vec<AgentRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT owner, name, machine, class, model, task_id, branch,
                    pid_alive, reported_state, text, source, as_of, msg_id
             FROM AGENT ORDER BY owner ASC, name ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(AgentRow {
                owner: r.get(0)?,
                name: r.get(1)?,
                machine: r.get(2)?,
                class: r.get(3)?,
                model: r.get(4)?,
                task_id: r.get(5)?,
                branch: r.get(6)?,
                pid_alive: r.get::<_, i64>(7)? != 0,
                reported_state: r.get(8)?,
                text: r.get(9)?,
                source: r.get(10)?,
                as_of: r.get::<_, i64>(11)?.max(0) as u64,
                msg_id: r.get::<_, i64>(12)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `ALERT` ordered by `(code, key)`.
pub fn query_alerts(conn: &Connection) -> Result<Vec<AlertTableRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT code, key, severity, \"to\", state, opened_at, last_at, count, text, data, msg_id
             FROM ALERT ORDER BY code ASC, key ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let data_s: String = r.get(9)?;
            let data: Value = serde_json::from_str(&data_s).unwrap_or_else(|_| json!({}));
            Ok(AlertTableRow {
                code: r.get(0)?,
                key: r.get(1)?,
                severity: r.get(2)?,
                to: r.get(3)?,
                state: r.get(4)?,
                opened_at: r.get::<_, i64>(5)?.max(0) as u64,
                last_at: r.get::<_, i64>(6)?.max(0) as u64,
                count: r.get::<_, i64>(7)?.max(0) as u64,
                text: r.get(8)?,
                data,
                msg_id: r.get::<_, i64>(10)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `POLICY` ordered by `version`.
pub fn query_policies(conn: &Connection) -> Result<Vec<PolicyRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT version, hash, mode, published_at, msg_id FROM POLICY ORDER BY version ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PolicyRow {
                version: r.get::<_, i64>(0)?.max(0) as u64,
                hash: r.get(1)?,
                mode: r.get(2)?,
                published_at: r.get::<_, i64>(3)?.max(0) as u64,
                msg_id: r.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Reads all rows of `META` ordered by `name`.
pub fn query_meta(conn: &Connection) -> Result<Vec<MetaRow>, String> {
    let mut stmt = conn
        .prepare("SELECT name, value, msg_id FROM META ORDER BY name ASC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MetaRow {
                name: r.get(0)?,
                value: r.get(1)?,
                msg_id: r.get::<_, i64>(2)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Populates the `/api/state` map from the core tables in `state.db`.
pub fn build_core_api(
    conn: &Connection,
    now_sec: u64,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let members = query_members(conn)?;
    let machines = query_machines(conn)?;
    let capacities = query_capacities(conn)?;
    let agents = query_agents(conn)?;
    let alerts = query_alerts(conn)?;
    let policies = query_policies(conn)?;
    let meta = query_meta(conn)?;

    let cursor = meta
        .iter()
        .find(|m| m.name == "cursor")
        .and_then(|m| m.value.parse::<u64>().ok())
        .unwrap_or(0);
    let schema_version = meta
        .iter()
        .find(|m| m.name == "schema_version")
        .and_then(|m| m.value.parse::<u64>().ok())
        .unwrap_or(1);
    let snapshot_id = meta
        .iter()
        .find(|m| m.name == "snapshot_id")
        .map(|m| m.value.clone())
        .unwrap_or_default();

    state
        .entry("as_of".to_owned())
        .or_insert_with(|| json!(now_sec));
    state
        .entry("hub".to_owned())
        .or_insert_with(|| json!({"id": cursor, "as_of": now_sec}));

    let store_obj = json!({
        "cursor": cursor,
        "schema_version": schema_version,
        "snapshot_id": snapshot_id,
        "members": members,
        "machines": machines,
        "capacities": capacities,
        "agents": agents,
        "alerts": alerts,
        "policies": policies,
        "meta": meta,
    });

    state.insert("store".into(), store_obj);
    state
        .entry("machines".to_owned())
        .or_insert_with(|| serde_json::to_value(&machines).unwrap_or_default());
    state
        .entry("capacities".to_owned())
        .or_insert_with(|| serde_json::to_value(&capacities).unwrap_or_default());
    state
        .entry("alerts".to_owned())
        .or_insert_with(|| serde_json::to_value(&alerts).unwrap_or_default());
    state
        .entry("policies".to_owned())
        .or_insert_with(|| serde_json::to_value(&policies).unwrap_or_default());

    Ok(())
}
