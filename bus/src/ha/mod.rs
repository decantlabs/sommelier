//! High availability standby hub, witness, promotion, self-fencing, and drills (step 12b, sections 13.5, 13.7).

pub mod drill;
pub mod fence;
pub mod promote;
pub mod standby;
pub mod witness;

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use drill::{DrillResult, run_drill};
pub use fence::{OrphanReport, SelfFenceMonitor, SelfFenceState, reconcile_returning_primary};
pub use promote::{PromoteOpts, PromoteOutcome, handback, promote};
pub use standby::{
    BundleStats, StandbyConfig, StandbyHandle, read_last_log_id, run_standby, sync_bundle,
};
pub use witness::{
    FileWitness, HttpWitness, MemoryWitness, Witness, WitnessBackend, WitnessError, WitnessRecord,
    is_stale,
};

/// Promotion mode from `[ha].promotion` (section 5.3, 13.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromotionMode {
    #[default]
    Owner,
    OwnerOrGuestQuorum,
}

/// `[ha]` policy configuration (section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaConfig {
    /// Empty means no standby hub is configured (self-fencing disabled).
    #[serde(default)]
    pub standby: String,
    #[serde(default = "default_witness_stale_min")]
    pub witness_stale_min: u64,
    #[serde(default = "default_self_fence_min")]
    pub self_fence_min: u64,
    #[serde(default)]
    pub promotion: PromotionMode,
    #[serde(default = "default_guest_quorum_after_min")]
    pub guest_quorum_after_min: u64,
    #[serde(default = "default_target_recover_sec")]
    pub target_recover_sec: u64,
}

fn default_witness_stale_min() -> u64 {
    10
}
fn default_self_fence_min() -> u64 {
    8
}
fn default_guest_quorum_after_min() -> u64 {
    60
}
fn default_target_recover_sec() -> u64 {
    240
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            standby: String::new(),
            witness_stale_min: default_witness_stale_min(),
            self_fence_min: default_self_fence_min(),
            promotion: PromotionMode::Owner,
            guest_quorum_after_min: default_guest_quorum_after_min(),
            target_recover_sec: default_target_recover_sec(),
        }
    }
}

impl HaConfig {
    /// True when a standby hub is configured. Self-fencing is active only when true.
    pub fn has_standby(&self) -> bool {
        !self.standby.trim().is_empty()
    }
}

/// Persisted HA role of a hub data folder (`data_dir/ha_state.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubHaRole {
    Primary,
    Standby,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaRoleState {
    pub role: HubHaRole,
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

impl HaRoleState {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("ha_state.json")
    }

    pub fn load(data_dir: &Path) -> Option<Self> {
        let raw = fs::read_to_string(Self::path(data_dir)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(data_dir)
            .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
        let path = Self::path(data_dir);
        let tmp = data_dir.join("ha_state.json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&tmp, text + "\n").map_err(|e| e.to_string())?;
        fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(())
    }
}
