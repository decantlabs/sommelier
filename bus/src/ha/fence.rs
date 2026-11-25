//! Self-fencing and returning-primary reconciliation (section 13.5).
//!
//! - A primary that cannot reach the witness for `ha.self_fence_min` (8) stops granting
//!   leases and offers while keeping its local workers running. Self-fencing is active
//!   only when a standby is configured.
//! - A primary that returns and finds a higher epoch steps down, keeps the lines the
//!   standby never saw in `bus.orphan-<epoch>.jsonl`, raises `LOG_DIVERGED` with their
//!   count, and follows as standby.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::msg::{Kind, Message, Role};
use crate::store::Store;
use crate::time::{now_ms, rfc3339};

use super::witness::{WitnessBackend, WitnessError};
use super::{HaConfig, HaRoleState, HubHaRole};

/// Whether the primary may grant new leases and offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelfFenceState {
    Active,
    Fenced { since_sec: u64, last_ok_sec: u64 },
}

impl SelfFenceState {
    pub fn may_grant_leases_and_offers(self) -> bool {
        matches!(self, SelfFenceState::Active)
    }
}

/// Tracks witness reachability on the primary hub.
#[derive(Clone, Debug)]
pub struct SelfFenceMonitor {
    pub holder: String,
    pub epoch: u64,
    pub standby_configured: bool,
    pub self_fence_min: u64,
    pub last_ok_sec: u64,
    pub state: SelfFenceState,
}

impl SelfFenceMonitor {
    pub fn new(holder: impl Into<String>, epoch: u64, cfg: &HaConfig, now_sec: u64) -> Self {
        Self {
            holder: holder.into(),
            epoch,
            standby_configured: cfg.has_standby(),
            self_fence_min: cfg.self_fence_min,
            last_ok_sec: now_sec,
            state: SelfFenceState::Active,
        }
    }

    /// Sends a witness heartbeat and updates fencing state.
    /// Returns `Err(higher_epoch)` if the witness already moved to a higher epoch (must step down).
    pub fn tick(
        &mut self,
        witness: &dyn WitnessBackend,
        now_sec: u64,
    ) -> Result<SelfFenceState, u64> {
        match witness.heartbeat(&self.holder, self.epoch, now_sec) {
            Ok(_) => {
                self.last_ok_sec = now_sec;
                self.state = SelfFenceState::Active;
                Ok(self.state)
            }
            Err(WitnessError::StaleEpoch { current, .. }) => Err(current),
            Err(WitnessError::Unreachable(_)) | Err(WitnessError::Unauthorized(_)) => {
                Ok(self.evaluate_offline(now_sec))
            }
        }
    }

    /// Pure time evaluation when witness is unreachable.
    pub fn evaluate_offline(&mut self, now_sec: u64) -> SelfFenceState {
        if !self.standby_configured {
            self.state = SelfFenceState::Active;
            return self.state;
        }
        let silent_sec = now_sec.saturating_sub(self.last_ok_sec);
        if silent_sec >= self.self_fence_min.saturating_mul(60)
            && let SelfFenceState::Active = self.state
        {
            self.state = SelfFenceState::Fenced {
                since_sec: now_sec,
                last_ok_sec: self.last_ok_sec,
            };
        }
        self.state
    }
}

/// Summary of a returning primary's orphan tail reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanReport {
    pub old_epoch: u64,
    pub new_epoch: u64,
    pub orphan_count: usize,
    pub orphan_path: PathBuf,
    pub common_last_id: u64,
    pub alert_id: Option<u64>,
}

/// Reconciles a returning primary whose local `bus.jsonl` may have lines the promoted
/// standby never saw:
/// 1. Finds the common prefix with `primary_lines`.
/// 2. Writes diverged local lines to `data_dir/bus.orphan-<old_epoch>.jsonl`.
/// 3. Replaces `data_dir/bus.jsonl` with `primary_lines` so logs are byte-equal.
/// 4. Updates `ha_state.json` to `standby` at `new_epoch`.
/// 5. Rebuilds `state.db` and records a `LOG_DIVERGED` alert if `orphan_count > 0` (or always records the step-down).
pub fn reconcile_returning_primary(
    data_dir: &Path,
    old_epoch: u64,
    new_epoch: u64,
    new_primary_url: Option<&str>,
    primary_lines: &[String],
) -> Result<OrphanReport, String> {
    let log_path = data_dir.join("bus.jsonl");
    let local_lines = read_raw_lines(&log_path)?;

    let mut common = 0usize;
    while common < local_lines.len()
        && common < primary_lines.len()
        && local_lines[common] == primary_lines[common]
    {
        common += 1;
    }

    let orphans = &local_lines[common..];
    let orphan_count = orphans.len();
    let orphan_path = data_dir.join(format!("bus.orphan-{old_epoch}.jsonl"));
    if orphan_count > 0 {
        let mut f = File::create(&orphan_path)
            .map_err(|e| format!("cannot write {}: {e}", orphan_path.display()))?;
        for line in orphans {
            writeln!(f, "{line}").map_err(|e| e.to_string())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
    }

    // Replace local bus.jsonl atomically with primary_lines.
    let tmp_log = data_dir.join("bus.jsonl.tmp");
    {
        let mut f = File::create(&tmp_log)
            .map_err(|e| format!("cannot write {}: {e}", tmp_log.display()))?;
        for line in primary_lines {
            writeln!(f, "{line}").map_err(|e| e.to_string())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp_log, &log_path).map_err(|e| e.to_string())?;

    let common_last_id = common
        .checked_sub(1)
        .and_then(|idx| serde_json::from_str::<Message>(&primary_lines[idx]).ok())
        .map_or(0, |m| m.id);

    // Rebuild state.db from the reconciled log.
    let db_path = data_dir.join("state.db");
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(data_dir.join("state.db-wal"));
    let _ = fs::remove_file(data_dir.join("state.db-shm"));
    let mut store = Store::open(data_dir)?;
    store.catch_up_from_log(&log_path)?;

    let alert_id = if orphan_count > 0 {
        let next_id = primary_lines
            .last()
            .and_then(|l| serde_json::from_str::<Message>(l).ok())
            .map_or(1, |m| m.id + 1);
        let body = json!({
            "v": 1,
            "code": "LOG_DIVERGED",
            "key": format!("epoch-{old_epoch}"),
            "severity": "warn",
            "text": format!(
                "returning primary stepped down at epoch {new_epoch}; {orphan_count} orphan lines saved to {}",
                orphan_path.file_name().and_then(|n| n.to_str()).unwrap_or("bus.orphan")
            ),
            "data": {
                "old_epoch": old_epoch,
                "new_epoch": new_epoch,
                "orphan_count": orphan_count,
                "orphan_file": orphan_path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            }
        });
        let alert_msg = Message {
            id: next_id,
            ts: rfc3339(now_ms()),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Alert,
            topic: format!("alert/LOG_DIVERGED/epoch-{old_epoch}"),
            body: body.to_string(),
            refs: vec![],
            to: vec!["coordinator".into()],
        };
        store.apply(&alert_msg)?;
        Some(next_id)
    } else {
        None
    };

    let state = HaRoleState {
        role: HubHaRole::Standby,
        epoch: new_epoch,
        holder: None,
        primary_url: new_primary_url.map(str::to_owned),
        updated_at: Some(now_ms() / 1000),
    };
    state.save(data_dir)?;

    Ok(OrphanReport {
        old_epoch,
        new_epoch,
        orphan_count,
        orphan_path,
        common_last_id,
        alert_id,
    })
}

fn read_raw_lines(path: &Path) -> Result<Vec<String>, String> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut lines = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    Ok(lines)
}
