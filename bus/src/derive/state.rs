//! Pure state derivation for workers, participants, and capacity slots (step 1).

use serde::{Deserialize, Serialize};

use super::capacity::CapacityMode;

/// Derived state of an agent row or slot (section 3 and step 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DerivedState {
    Running,
    Idle,
    Starved,
    Suspect,
    Stale,
    Own,
    Offline,
    Blocked,
    Draining,
    Dead,
}

impl DerivedState {
    pub fn as_str(self) -> &'static str {
        match self {
            DerivedState::Running => "running",
            DerivedState::Idle => "idle",
            DerivedState::Starved => "starved",
            DerivedState::Suspect => "suspect",
            DerivedState::Stale => "stale",
            DerivedState::Own => "own",
            DerivedState::Offline => "offline",
            DerivedState::Blocked => "blocked",
            DerivedState::Draining => "draining",
            DerivedState::Dead => "dead",
        }
    }
}

/// Timers used to derive worker and slot states.
#[derive(Clone, Copy, Debug)]
pub struct DeriveTimers {
    /// Heartbeat silence before a running worker becomes `suspect` (default 120 s).
    pub suspect_sec: u64,
    /// Silence before a row becomes `stale` (default 20 min = 1200 s).
    pub stale_sec: u64,
    /// Idle duration before an IDLE alert is raised (default 10 min = 600 s).
    pub idle_alert_sec: u64,
}

impl Default for DeriveTimers {
    fn default() -> Self {
        Self {
            suspect_sec: 120,
            stale_sec: 20 * 60,
            idle_alert_sec: 10 * 60,
        }
    }
}

/// Input facts for deriving a single worker or slot state.
#[derive(Clone, Debug)]
pub struct WorkerFacts {
    pub owner: String,
    pub agent: String,
    pub class: String,
    pub mode: CapacityMode,
    pub owner_online: bool,
    /// Reported state from STATUS (`running`, `idle`, `blocked`, `draining`), or `"idle"` for a free slot.
    pub reported_state: String,
    /// Whether this row comes from a supervised worker or OS process wrapper where heartbeats are expected every 60 s.
    pub supervised: bool,
    /// OS PID observed by the local supervisor/launcher, if any.
    pub pid: Option<u32>,
    /// Host machine name reported by the local supervisor/launcher, if any.
    pub machine: String,
    /// Last STATUS or heartbeat timestamp (UTC epoch milliseconds).
    pub last_seen_ms: u64,
    /// Timestamp when the current state or text began (UTC epoch milliseconds).
    pub state_since_ms: u64,
    /// If idle and ready tasks exist that this worker/slot cannot take, the reason string.
    pub starved_reason: Option<String>,
    pub model: String,
    pub branch: String,
    pub text: String,
}

/// Derived truth for a worker or slot at `as_of_ms`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedWorker {
    pub owner: String,
    pub agent: String,
    pub class: String,
    pub state: DerivedState,
    pub reported_state: String,
    pub mode: CapacityMode,
    pub age_secs: u64,
    pub idle_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub model: String,
    pub branch: String,
    pub text: String,
    pub as_of: u64,
    /// Whether an OS supervisor/process wrapper is actively verifying this worker's liveness.
    #[serde(default)]
    pub supervised: bool,
    /// Liveness source: `"process"` when backed by a supervisor/PID watcher, `"self_reported"` otherwise.
    #[serde(default)]
    pub liveness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine: String,
}

/// Pure function computing the derived state of a worker or slot at `now_ms`.
pub fn derive_worker(facts: &WorkerFacts, timers: &DeriveTimers, now_ms: u64) -> DerivedWorker {
    let age_secs = now_ms.saturating_sub(facts.last_seen_ms) / 1000;
    let state_secs = now_ms.saturating_sub(facts.state_since_ms) / 1000;
    let rep = facts.reported_state.trim().to_ascii_lowercase();

    let (state, idle_secs, reason) = if !facts.owner_online {
        (DerivedState::Offline, 0, Some("owner offline".into()))
    } else if age_secs >= timers.stale_sec {
        (
            DerivedState::Stale,
            0,
            Some(format!("no update for {age_secs} s")),
        )
    } else if facts.mode == CapacityMode::Own
        && (facts.agent == "main" || facts.agent == facts.owner || rep == "own")
    {
        (DerivedState::Own, 0, None)
    } else if rep == "blocked" {
        (DerivedState::Blocked, 0, None)
    } else if rep == "draining" {
        (DerivedState::Draining, 0, None)
    } else if rep == "running" {
        if facts.supervised && age_secs > timers.suspect_sec {
            (
                DerivedState::Suspect,
                0,
                Some(format!("heartbeat late by {age_secs} s")),
            )
        } else if facts.mode == CapacityMode::Own {
            (DerivedState::Own, 0, None)
        } else {
            (DerivedState::Running, 0, None)
        }
    } else {
        // Idle or free slot
        let idle_for = state_secs.max(age_secs);
        if let Some(why) = &facts.starved_reason {
            (DerivedState::Starved, idle_for, Some(why.clone()))
        } else if facts.mode == CapacityMode::Own && rep == "own" {
            (DerivedState::Own, 0, None)
        } else {
            (DerivedState::Idle, idle_for, None)
        }
    };

    DerivedWorker {
        owner: facts.owner.clone(),
        agent: facts.agent.clone(),
        class: facts.class.clone(),
        state,
        reported_state: rep,
        mode: facts.mode,
        age_secs,
        idle_secs,
        reason,
        model: facts.model.clone(),
        branch: facts.branch.clone(),
        text: facts.text.clone(),
        as_of: now_ms / 1000,
        supervised: facts.supervised,
        liveness: if facts.supervised {
            "process".into()
        } else {
            "self_reported".into()
        },
        pid: facts.pid,
        machine: facts.machine.clone(),
    }
}
