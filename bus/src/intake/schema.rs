//! The SUBMISSION table. It is the contract between the intake, the review and the standing parts.
//!
//! A row is projected ONLY from FACT messages of the hub with topic `submission/<sha256>`. The body is
//! one line of JSON: `v` (1), `submission` (the SHA 256 of the bundle), `task`, `attempt`, `member`,
//! `base`, `head`, `bytes`, `state`, `risk`, `tier` and `at` (UTC epoch seconds). Every change of state
//! is a new FACT with the same topic. The projection keeps the newest.
//!
//! States, in order: `received`, then `gate_failed` or `gated`, then `verifying`, then `verify_failed`
//! or `verified`, then `reviewing`, then `review_failed` or `approved`, then `landed`. `rejected` ends a
//! submission from any state. The intake part writes the states up to `verified`. The review part writes
//! `reviewing`, `review_failed`, `approved` and `landed`.

/// DDL of the SUBMISSION table.
pub const SUBMISSION_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS SUBMISSION (
    sha256      TEXT PRIMARY KEY,
    task        TEXT NOT NULL,
    attempt     INTEGER NOT NULL,
    member      TEXT NOT NULL,
    base        TEXT NOT NULL,
    head        TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    state       TEXT NOT NULL,
    risk        TEXT NOT NULL,
    tier        INTEGER NOT NULL,
    received_at INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    msg_id      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS SUBMISSION_BY_TASK ON SUBMISSION (task, attempt);
CREATE INDEX IF NOT EXISTS SUBMISSION_BY_STATE ON SUBMISSION (state);
";

/// Every state a submission can be in.
pub const SUBMISSION_STATES: [&str; 11] = [
    "received",
    "gate_failed",
    "gated",
    "verifying",
    "verify_failed",
    "verified",
    "reviewing",
    "review_failed",
    "approved",
    "landed",
    "rejected",
];
