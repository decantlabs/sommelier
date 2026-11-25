//! Standby promotion and planned handback (section 13.5).
//!
//! Promotion preconditions:
//! 1. Witness heartbeat is older than `ha.witness_stale_min` (10 min).
//! 2. Standby itself cannot reach the primary.
//! 3. Owner confirms with `--confirm <code>` (or with `promotion = "owner_or_guest_quorum"`,
//!    two opted-in guests confirm after `guest_quorum_after_min` (60 min)).
//!
//! Taking `epoch + 1` at the witness prevents split brain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::witness::{Witness, is_stale};
use super::{HaConfig, HaRoleState, HubHaRole, PromotionMode};

#[derive(Clone, Debug)]
pub struct PromoteOpts {
    pub data_dir: PathBuf,
    pub primary_url: Option<String>,
    pub new_holder: String,
    pub confirm_code: Option<String>,
    pub expected_confirm: Option<String>,
    pub guest_confirmations: Vec<String>,
    pub now_sec: u64,
    pub ha_config: HaConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteOutcome {
    pub holder: String,
    pub previous_epoch: u64,
    pub epoch: u64,
    pub promoted_at: u64,
}

/// Promotes the standby at `opts.data_dir` to primary if all section 13.5 preconditions hold.
pub async fn promote(opts: &PromoteOpts, witness: &Witness) -> Result<PromoteOutcome, String> {
    let witness_rec = witness
        .get()
        .await
        .map_err(|e| format!("cannot read witness: {e}"))?;

    let current_epoch = witness_rec
        .as_ref()
        .map(|r| r.epoch)
        .or_else(|| HaRoleState::load(&opts.data_dir).map(|s| s.epoch))
        .unwrap_or(1);

    // 1. Witness heartbeat must be older than ha.witness_stale_min.
    if let Some(rec) = &witness_rec
        && !is_stale(rec, opts.now_sec, opts.ha_config.witness_stale_min)
    {
        let age = opts.now_sec.saturating_sub(rec.heartbeat);
        return Err(format!(
            "refusing promote: witness heartbeat is only {age}s old (need >= {}s)",
            opts.ha_config.witness_stale_min.saturating_mul(60)
        ));
    }

    // 2. Standby itself cannot reach the primary.
    if let Some(primary) = opts.primary_url.as_deref()
        && !primary.trim().is_empty()
        && is_primary_reachable(primary).await
    {
        return Err(format!(
            "refusing promote: primary at {primary} is still reachable"
        ));
    }

    // 3. Owner confirmation or guest quorum.
    verify_authorization(opts, witness_rec.as_ref())?;

    // 4. Take epoch + 1 at the witness via CAS.
    let updated = witness
        .promote(&opts.new_holder, current_epoch, opts.now_sec)
        .await
        .map_err(|e| format!("witness rejected promotion: {e}"))?;

    // 5. Persist primary role state so the running standby transitions to primary.
    let state = HaRoleState {
        role: HubHaRole::Primary,
        epoch: updated.epoch,
        holder: Some(updated.holder.clone()),
        primary_url: None,
        updated_at: Some(opts.now_sec),
    };
    state.save(&opts.data_dir)?;

    Ok(PromoteOutcome {
        holder: updated.holder,
        previous_epoch: current_epoch,
        epoch: updated.epoch,
        promoted_at: opts.now_sec,
    })
}

/// Planned handback from standby back to the original primary when it is healthy again.
pub async fn handback(
    standby_data_dir: &Path,
    original_holder: &str,
    original_primary_url: &str,
    witness: &Witness,
    now_sec: u64,
) -> Result<PromoteOutcome, String> {
    let cur = witness
        .get()
        .await
        .map_err(|e| format!("cannot read witness: {e}"))?;
    let current_epoch = cur
        .as_ref()
        .map(|r| r.epoch)
        .or_else(|| HaRoleState::load(standby_data_dir).map(|s| s.epoch))
        .unwrap_or(1);

    let updated = witness
        .promote(original_holder, current_epoch, now_sec)
        .await
        .map_err(|e| format!("witness rejected handback: {e}"))?;

    let state = HaRoleState {
        role: HubHaRole::Standby,
        epoch: updated.epoch,
        holder: None,
        primary_url: Some(original_primary_url.to_owned()),
        updated_at: Some(now_sec),
    };
    state.save(standby_data_dir)?;

    Ok(PromoteOutcome {
        holder: updated.holder,
        previous_epoch: current_epoch,
        epoch: updated.epoch,
        promoted_at: now_sec,
    })
}

fn verify_authorization(
    opts: &PromoteOpts,
    witness_rec: Option<&super::witness::WitnessRecord>,
) -> Result<(), String> {
    let owner_ok = match (&opts.confirm_code, &opts.expected_confirm) {
        (Some(code), Some(expected)) => !code.trim().is_empty() && code == expected,
        (Some(code), None) => !code.trim().is_empty(),
        (None, _) => false,
    };
    if owner_ok {
        return Ok(());
    }

    if opts.ha_config.promotion == PromotionMode::OwnerOrGuestQuorum {
        let stale_enough = witness_rec.is_none_or(|r| {
            opts.now_sec.saturating_sub(r.heartbeat)
                >= opts.ha_config.guest_quorum_after_min.saturating_mul(60)
        });
        let mut unique = opts.guest_confirmations.clone();
        unique.sort();
        unique.dedup();
        if stale_enough && unique.len() >= 2 {
            return Ok(());
        }
        return Err(format!(
            "refusing promote: need --confirm <code> or 2 guest confirmations after {} min",
            opts.ha_config.guest_quorum_after_min
        ));
    }

    Err("refusing promote: valid --confirm <code> is required".into())
}

async fn is_primary_reachable(primary_url: &str) -> bool {
    let Ok(mut url) = reqwest::Url::parse(primary_url.trim()) else {
        return false;
    };
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return false,
    };
    if url.set_scheme(scheme).is_err() {
        return false;
    }
    url.set_path("/api/state");
    url.set_query(None);
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    match client.get(url).send().await {
        // 200 or 401 means the hub HTTP server is alive and answering (not 503 / connection refused).
        Ok(resp) => resp.status().as_u16() != 503,
        Err(_) => false,
    }
}
