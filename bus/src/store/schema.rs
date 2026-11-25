//! Core schema definitions for `state.db` (section 3, step 4).
//!
//! Core tables delivered in step 4:
//! - `META`: log cursor, schema version, snapshot id
//! - `MEMBER`: name -> role, trust, online, last_seen_at, msg_id
//! - `MACHINE`: owner, machine -> os, gpu, machine_class, gpu_slots, disk_free_gb, agent_version, policy_hash, heartbeat_at, msg_id
//! - `CAPACITY`: owner, machine, class -> mode, useful_parallel, accepts, pools, declared_at, msg_id
//! - `AGENT`: owner, name -> machine, class, model, task_id, branch, pid_alive, reported_state, text, source, as_of, msg_id
//! - `ALERT`: code, key -> severity, to, state, opened_at, last_at, count, text, data, msg_id
//! - `POLICY`: version -> hash, mode, published_at, msg_id

/// Current schema version stored in `META`.
pub const SCHEMA_VERSION: u64 = 1;

/// Names of the seven core tables delivered in step 4.
pub const CORE_TABLES: [&str; 7] = [
    "META", "MEMBER", "MACHINE", "CAPACITY", "AGENT", "ALERT", "POLICY",
];

/// SQL statements creating the core tables of `state.db`.
pub const CORE_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS META (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS MEMBER (
    name TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    trust TEXT NOT NULL,
    online INTEGER NOT NULL DEFAULT 0,
    last_seen_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS MACHINE (
    owner TEXT NOT NULL,
    machine TEXT NOT NULL,
    os TEXT NOT NULL DEFAULT '',
    gpu TEXT NOT NULL DEFAULT '',
    machine_class TEXT NOT NULL DEFAULT '',
    gpu_slots INTEGER NOT NULL DEFAULT 0,
    disk_free_gb INTEGER NOT NULL DEFAULT 0,
    agent_version TEXT NOT NULL DEFAULT '',
    policy_hash TEXT NOT NULL DEFAULT '',
    heartbeat_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, machine)
);

CREATE TABLE IF NOT EXISTS CAPACITY (
    owner TEXT NOT NULL,
    machine TEXT NOT NULL,
    class TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'auto',
    useful_parallel INTEGER NOT NULL DEFAULT 0,
    accepts TEXT NOT NULL DEFAULT '[]',
    pools TEXT NOT NULL DEFAULT '[]',
    declared_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, machine, class)
);

CREATE TABLE IF NOT EXISTS AGENT (
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    machine TEXT NOT NULL DEFAULT '',
    class TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    task_id TEXT NOT NULL DEFAULT '',
    branch TEXT NOT NULL DEFAULT '',
    pid_alive INTEGER NOT NULL DEFAULT 0,
    reported_state TEXT NOT NULL DEFAULT 'running',
    text TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'self',
    as_of INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner, name)
);

CREATE TABLE IF NOT EXISTS ALERT (
    code TEXT NOT NULL,
    key TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT 'warn',
    \"to\" TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open',
    opened_at INTEGER NOT NULL DEFAULT 0,
    last_at INTEGER NOT NULL DEFAULT 0,
    count INTEGER NOT NULL DEFAULT 1,
    text TEXT NOT NULL DEFAULT '',
    data TEXT NOT NULL DEFAULT '{}',
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (code, key)
);

CREATE TABLE IF NOT EXISTS POLICY (
    version INTEGER PRIMARY KEY,
    hash TEXT NOT NULL DEFAULT '',
    mode TEXT NOT NULL DEFAULT 'off',
    published_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);
";
