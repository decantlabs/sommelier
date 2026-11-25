//! Full role eligibility, quota runway, planned handover, and hysteresis (sections 13.1, 13.2, 13.3, 13.6).
//!
//! Extends the step 7 heartbeat and policy-order election in `role/elect.rs` with:
//! - Quota runway eligibility (`min_runway_min`, `forecast_min / margin >= min_runway_min`)
//! - Candidate ordering by `(tier asc, runway desc, name asc)` where `runway = forecast_min * conf`
//! - Planned handover while holder lives when its pool enters `drain`/`closed`/`paused` or its
//!   runway drops below `handover_runway_min` (zero vacant gap)
//! - Hysteresis: stickiness (no pushing out a healthy holder), `min_hold_until` (bypassed if out
//!   of quota or vetoed), cooldown after loss, and flapping guard freezing the role on the fourth
//!   change within one hour (`changes_last_hour > max_changes_per_hour`)
//! - Store module (`RoleStoreModule`) and hub tick / op wiring for `ROLE`

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::{Kind, Message, Role};
use crate::quota::forecast::{ActiveLease, forecast, pool_state};
use crate::quota::meter::{Meter, PoolState, QuotaConfig};
use crate::store::StoreModule;
use crate::time::rfc3339;

use super::elect::{RoleAction, RoleCandidate, RolePolicy, RoleTable};
use super::lease::{LeaseScope, RoleRow, RoleState};

/// Full per-role election policy from `[role.<name>]` including step 11 quota fields (section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FullRolePolicy {
    #[serde(flatten)]
    pub base: RolePolicy,
    /// Minimum forecast runway (divided by confidence margin) in minutes required for eligibility.
    #[serde(default = "default_min_runway_min")]
    pub min_runway_min: u64,
    /// Runway threshold in minutes below which a live holder starts a planned handover.
    #[serde(default = "default_handover_runway_min")]
    pub handover_runway_min: u64,
    /// Quota percentage at which `COORD_BUDGET` alarm fires (section 13.6).
    #[serde(default = "default_budget_alarm_pct")]
    pub budget_alarm_pct: f64,
}

fn default_min_runway_min() -> u64 {
    120
}
fn default_handover_runway_min() -> u64 {
    45
}
fn default_budget_alarm_pct() -> f64 {
    70.0
}

impl Default for FullRolePolicy {
    fn default() -> Self {
        Self {
            base: RolePolicy::default(),
            min_runway_min: default_min_runway_min(),
            handover_runway_min: default_handover_runway_min(),
            budget_alarm_pct: default_budget_alarm_pct(),
        }
    }
}

/// Explicit handover request from the current role holder (section 13.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverRequest {
    pub holder: String,
    /// Reason: `"context"` allows a fresh session of the same seat; any other picks a successor.
    pub reason: String,
    #[serde(default)]
    pub note: String,
}

/// Pure input context for full role eligibility and election (section 13.1 and 13.3).
#[derive(Clone, Debug, Default)]
pub struct EligibilityContext {
    /// Members with valid tokens. When empty, all non-vetoed candidates are assumed valid.
    pub valid_tokens: BTreeSet<String>,
    /// Currently online members or seats.
    pub online_members: BTreeSet<String>,
    /// Supervisor heartbeats by owner or seat (`who_or_owner -> last_heartbeat_sec`).
    pub supervisor_heartbeats: BTreeMap<String, u64>,
    /// Capabilities declared in HELLO (`who_or_owner -> capabilities`).
    pub capabilities: BTreeMap<String, BTreeSet<String>>,
    /// Roles a guest opted into via `roles=` in its HELLO (`owner -> role names`).
    pub hello_roles: BTreeMap<String, BTreeSet<String>>,
    /// Pools consumed by each class (`class -> pools`).
    pub class_pools: BTreeMap<String, Vec<String>>,
    /// Quota meters keyed by `(owner, pool)`.
    pub meters: BTreeMap<(String, String), Meter>,
    /// Active leases per `(owner, pool)` for pessimistic burn forecasting.
    pub active_leases_by_owner_pool: BTreeMap<(String, String), Vec<ActiveLease>>,
    /// Quota margins and confidence settings.
    pub quota_cfg: QuotaConfig,
    /// Pending explicit handover requests keyed by role name.
    pub handover_requests: BTreeMap<String, HandoverRequest>,
}

/// Reason a live holder must perform a planned handover (section 13.2 & 13.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoverTrigger {
    PoolDrain(String),
    PoolClosed(String),
    PoolPaused(String),
    LowRunway,
    ContextFull,
    Requested,
    Vetoed,
}

impl HandoverTrigger {
    pub fn as_reason(&self) -> String {
        match self {
            Self::PoolDrain(p) => format!("planned handover: pool {p} in drain"),
            Self::PoolClosed(p) => format!("planned handover: pool {p} closed"),
            Self::PoolPaused(p) => format!("planned handover: pool {p} paused"),
            Self::LowRunway => "planned handover: runway below handover_runway_min".to_owned(),
            Self::ContextFull => "planned handover: context full".to_owned(),
            Self::Requested => "planned handover: requested by holder".to_owned(),
            Self::Vetoed => "planned handover: holder vetoed".to_owned(),
        }
    }

    /// True when the holder is actually out of quota (or vetoed / explicitly handing over),
    /// bypassing `min_hold_until` hysteresis per section 13.3.
    pub fn bypasses_min_hold(&self) -> bool {
        matches!(
            self,
            Self::PoolClosed(_)
                | Self::PoolPaused(_)
                | Self::Vetoed
                | Self::ContextFull
                | Self::Requested
        )
    }
}

/// Evaluation of one candidate under section 13.1 `eligible(c, role, now)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateEligibility {
    pub candidate: RoleCandidate,
    pub eligible: bool,
    /// `forecast_min * confidence` across the tightest meter, used for `(tier, runway desc, name)`.
    pub runway_min: f64,
    /// `forecast_min / margin` across the tightest meter, compared against `role.min_runway_min`.
    pub conservative_runway_min: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Structured alert produced during a role election tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleAlert {
    pub code: String,
    pub key: String,
    pub severity: String,
    pub to: String,
    pub text: String,
    pub data: Value,
}

/// Complete outcome of one full section 13.3 election tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleTickOutcome {
    pub action: RoleAction,
    pub planned_handover: bool,
    pub vacant_gap_secs: u64,
    pub spawn_required: bool,
    pub alerts: Vec<RoleAlert>,
    pub fact_message: Option<Message>,
    pub evaluated_candidates: Vec<CandidateEligibility>,
}

/// Computes `(min_runway_weighted, min_conservative_runway, worst_state, max_used_pct)` across
/// all meters of `class` belonging to `owner`.
fn candidate_quota_metrics(
    owner: &str,
    class: &str,
    ctx: &EligibilityContext,
    now_sec: u64,
) -> (f64, f64, Option<(String, PoolState)>, f64) {
    let pools = ctx.class_pools.get(class).cloned().unwrap_or_default();
    if pools.is_empty() {
        return (10_080.0, 10_080.0, None, 0.0);
    }

    let mut min_weighted = f64::INFINITY;
    let mut min_conservative = f64::INFINITY;
    let mut worst_bad: Option<(String, PoolState)> = None;
    let mut max_used_pct = 0.0_f64;

    for pool in &pools {
        let Some(m) = ctx
            .meters
            .get(&(owner.to_owned(), pool.clone()))
            .or_else(|| ctx.meters.get(&(String::new(), pool.clone())))
        else {
            continue;
        };
        let leases = ctx
            .active_leases_by_owner_pool
            .get(&(owner.to_owned(), pool.clone()))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let f = forecast(m, now_sec, leases);
        let st = pool_state(m, &f, now_sec, &ctx.quota_cfg);
        max_used_pct = max_used_pct.max(f.used);

        if matches!(
            st,
            PoolState::Paused | PoolState::Closed | PoolState::Drain | PoolState::Unknown
        ) && worst_bad.is_none()
        {
            worst_bad = Some((pool.clone(), st));
        }

        let forecast_min = if let Some(t_line) = f.t_line {
            t_line.saturating_sub(now_sec) as f64 / 60.0
        } else if f.resets_first && m.resets_at > now_sec {
            ((m.resets_at - now_sec) as f64 / 60.0) + (m.policy.window_duration_sec as f64 / 60.0)
        } else if matches!(st, PoolState::Closed | PoolState::Paused) {
            0.0
        } else {
            10_080.0
        };

        let margin = ctx.quota_cfg.margin_for(f.band).max(1.0);
        let weighted = forecast_min * f.conf;
        let conservative = forecast_min / margin;
        if weighted < min_weighted {
            min_weighted = weighted;
        }
        if conservative < min_conservative {
            min_conservative = conservative;
        }
    }

    if !min_weighted.is_finite() {
        min_weighted = 10_080.0;
    }
    if !min_conservative.is_finite() {
        min_conservative = 10_080.0;
    }
    (min_weighted, min_conservative, worst_bad, max_used_pct)
}

/// Evaluates `eligible(c, role, now)` per section 13.1.
pub fn evaluate_candidate(
    table: &RoleTable,
    role: &str,
    cand: &RoleCandidate,
    policy: &FullRolePolicy,
    ctx: &EligibilityContext,
    now_sec: u64,
) -> CandidateEligibility {
    let owner = cand.owner();
    let fail = |reason: String, runway: f64, cons: f64| CandidateEligibility {
        candidate: cand.clone(),
        eligible: false,
        runway_min: runway,
        conservative_runway_min: cons,
        reason: Some(reason),
    };

    // 1. Valid token and not vetoed.
    if !ctx.valid_tokens.is_empty()
        && !ctx.valid_tokens.contains(&cand.who)
        && !ctx.valid_tokens.contains(owner)
    {
        return fail("invalid or missing token".to_owned(), 0.0, 0.0);
    }
    if table.vetoed.contains(&cand.who) || table.vetoed.contains(owner) {
        return fail("vetoed by owner".to_owned(), 0.0, 0.0);
    }

    // 2. Reachability: online, OR spawn = true and supervisor heartbeats within dead_sec.
    if table.unreachable.contains(&cand.who) || table.unreachable.contains(owner) {
        return fail("unreachable".to_owned(), 0.0, 0.0);
    }
    let is_online = ctx.online_members.contains(&cand.who) || ctx.online_members.contains(owner);
    let supervisor_alive = cand.spawn
        && ctx
            .supervisor_heartbeats
            .get(&cand.who)
            .or_else(|| ctx.supervisor_heartbeats.get(owner))
            .is_some_and(|&hb| now_sec.saturating_sub(hb) <= policy.base.dead_sec);
    let table_hb_alive = table
        .candidate_heartbeats
        .get(&cand.who)
        .is_some_and(|&hb| now_sec.saturating_sub(hb) <= policy.base.dead_sec);
    if !ctx.online_members.is_empty() || !ctx.supervisor_heartbeats.is_empty() {
        if !is_online && !supervisor_alive && !table_hb_alive {
            return fail("candidate not reachable".to_owned(), 0.0, 0.0);
        }
    } else if let Some(&hb) = table.candidate_heartbeats.get(&cand.who)
        && now_sec.saturating_sub(hb) > policy.base.dead_sec
    {
        return fail("candidate heartbeat expired".to_owned(), 0.0, 0.0);
    }

    // 3. Required capabilities declared in HELLO.
    if !policy.base.require.is_empty() && !ctx.capabilities.is_empty() {
        let empty = BTreeSet::new();
        let caps = ctx
            .capabilities
            .get(&cand.who)
            .or_else(|| ctx.capabilities.get(owner))
            .unwrap_or(&empty);
        for req in &policy.base.require {
            if !caps.contains(req) {
                return fail(format!("missing required capability {req}"), 0.0, 0.0);
            }
        }
    }

    // 4. Guest limited scope: opt_in in policy AND roles= in its own HELLO.
    if cand.scope == LeaseScope::Limited {
        if !cand.opt_in {
            return fail(
                "guest candidate not opted in by policy".to_owned(),
                0.0,
                0.0,
            );
        }
        if !ctx.hello_roles.is_empty() {
            let opted = ctx
                .hello_roles
                .get(owner)
                .or_else(|| ctx.hello_roles.get(&cand.who))
                .is_some_and(|roles| roles.contains(role));
            if !opted {
                return fail(
                    format!("guest did not declare roles={role} in HELLO"),
                    0.0,
                    0.0,
                );
            }
        }
    }

    // 5. Cooldown check: candidate did not lose this role within cooldown_min.
    if let Some(&lost_ts) = table.lost_at.get(&(role.to_owned(), cand.who.clone())) {
        let cooldown_sec = policy.base.cooldown_min.saturating_mul(60);
        if now_sec < lost_ts.saturating_add(cooldown_sec) {
            return fail(
                format!("in cooldown until {}", lost_ts.saturating_add(cooldown_sec)),
                0.0,
                0.0,
            );
        }
    }

    // 6. Quota runway check (section 13.1):
    // Every meter of c.class is open or throttled, and forecast_min / margin >= min_runway_min.
    let (weighted_runway, conservative_runway, worst_bad, _max_used) =
        candidate_quota_metrics(owner, &cand.class, ctx, now_sec);
    if let Some((pool, st)) = worst_bad {
        return fail(
            format!("pool {pool} is {}", st.as_str()),
            weighted_runway,
            conservative_runway,
        );
    }
    if conservative_runway < policy.min_runway_min as f64 {
        return fail(
            format!(
                "runway {:.1} min below min_runway_min {} min",
                conservative_runway, policy.min_runway_min
            ),
            weighted_runway,
            conservative_runway,
        );
    }

    CandidateEligibility {
        candidate: cand.clone(),
        eligible: true,
        runway_min: (weighted_runway * 100.0).round() / 100.0,
        conservative_runway_min: (conservative_runway * 100.0).round() / 100.0,
        reason: None,
    }
}

/// Returns all candidates evaluated, and eligible ones sorted strictly by
/// `(tier asc, runway desc, who asc)` (section 13.3).
pub fn eligible_candidates_full(
    table: &RoleTable,
    role: &str,
    policy: &FullRolePolicy,
    ctx: &EligibilityContext,
    now_sec: u64,
) -> (Vec<CandidateEligibility>, Vec<CandidateEligibility>) {
    let evaluated: Vec<CandidateEligibility> = policy
        .base
        .candidates
        .iter()
        .map(|c| evaluate_candidate(table, role, c, policy, ctx, now_sec))
        .collect();

    let mut eligible: Vec<CandidateEligibility> =
        evaluated.iter().filter(|e| e.eligible).cloned().collect();
    eligible.sort_by(|a, b| {
        let ra = (a.runway_min * 1000.0).round() as i64;
        let rb = (b.runway_min * 1000.0).round() as i64;
        a.candidate
            .tier
            .cmp(&b.candidate.tier)
            .then_with(|| rb.cmp(&ra))
            .then_with(|| a.candidate.who.cmp(&b.candidate.who))
    });
    (eligible, evaluated)
}

/// Checks `must_hand_over(h)` per section 13.3:
/// pool of h in drain, closed or paused; or `runway(h) < handover_runway_min`;
/// or h asked for it (context or release); or h is vetoed.
pub fn must_hand_over(
    table: &RoleTable,
    role: &str,
    holder: &str,
    row: &RoleRow,
    policy: &FullRolePolicy,
    ctx: &EligibilityContext,
    now_sec: u64,
) -> Option<HandoverTrigger> {
    let owner = holder.split_once('/').map_or(holder, |(o, _)| o);
    if table.vetoed.contains(holder) || table.vetoed.contains(owner) {
        return Some(HandoverTrigger::Vetoed);
    }
    if let Some(req) = ctx.handover_requests.get(role)
        && req.holder == holder
    {
        if req.reason.eq_ignore_ascii_case("context")
            || req.reason.eq_ignore_ascii_case("context_full")
        {
            return Some(HandoverTrigger::ContextFull);
        }
        return Some(HandoverTrigger::Requested);
    }
    if row.state == RoleState::HandingOver {
        return Some(HandoverTrigger::Requested);
    }

    let class = policy
        .base
        .candidates
        .iter()
        .find(|c| c.who == holder)
        .map(|c| c.class.as_str())
        .unwrap_or("");
    if !class.is_empty() {
        let (weighted_runway, _conservative, worst_bad, _max_used) =
            candidate_quota_metrics(owner, class, ctx, now_sec);
        if let Some((pool, st)) = worst_bad {
            match st {
                PoolState::Closed => return Some(HandoverTrigger::PoolClosed(pool)),
                PoolState::Paused => return Some(HandoverTrigger::PoolPaused(pool)),
                PoolState::Drain => return Some(HandoverTrigger::PoolDrain(pool)),
                _ => {}
            }
        }
        if weighted_runway < policy.handover_runway_min as f64 {
            return Some(HandoverTrigger::LowRunway);
        }
    }

    None
}

/// Runs the full deterministic section 13.3 election algorithm with quota runway, planned
/// handover, hysteresis (`min_hold_until`, stickiness, cooldown, flapping guard), and
/// `COORD_BUDGET` checks.
pub fn role_tick_full(
    table: &mut RoleTable,
    role: &str,
    policy: &FullRolePolicy,
    ctx: &EligibilityContext,
    now_sec: u64,
) -> RoleTickOutcome {
    let key = (role.to_owned(), "global".to_owned());
    let current = table
        .rows
        .entry(key.clone())
        .or_insert_with(|| RoleRow::vacant(role, "global"))
        .clone();

    let (eligible, evaluated) = eligible_candidates_full(table, role, policy, ctx, now_sec);
    let mut alerts = Vec::new();

    if current.frozen {
        return RoleTickOutcome {
            action: RoleAction::Frozen {
                role: role.to_owned(),
                epoch: current.epoch,
                changes_last_hour: current.changes_last_hour,
            },
            planned_handover: false,
            vacant_gap_secs: 0,
            spawn_required: false,
            alerts,
            fact_message: None,
            evaluated_candidates: evaluated,
        };
    }

    let holder_alive = current.holder.as_deref().is_some_and(|h| {
        current.state != RoleState::Vacant
            && !table.unreachable.contains(h)
            && now_sec.saturating_sub(current.heartbeat_at) <= policy.base.dead_sec
    });

    if holder_alive {
        let holder = current.holder.clone().unwrap();
        let owner = holder.split_once('/').map_or(holder.as_str(), |(o, _)| o);
        let holder_class = policy
            .base
            .candidates
            .iter()
            .find(|c| c.who == holder)
            .map(|c| c.class.as_str())
            .unwrap_or("");
        if !holder_class.is_empty() {
            let (_, _, _, max_used) = candidate_quota_metrics(owner, holder_class, ctx, now_sec);
            if max_used >= policy.budget_alarm_pct {
                let has_successor = eligible.iter().any(|e| e.candidate.who != holder);
                alerts.push(RoleAlert {
                    code: "COORD_BUDGET".to_owned(),
                    key: format!("{role}/{holder}"),
                    severity: if has_successor { "warn" } else { "critical" }.to_owned(),
                    to: if has_successor { "coordinator" } else { "owner" }.to_owned(),
                    text: format!(
                        "role {role} holder {holder} at {max_used:.1}% quota (alarm {:.0}%); successor eligible: {has_successor}",
                        policy.budget_alarm_pct
                    ),
                    data: json!({
                        "role": role,
                        "holder": holder,
                        "used_pct": max_used,
                        "successor_eligible": has_successor,
                    }),
                });
            }
        }

        let trigger = must_hand_over(table, role, &holder, &current, policy, ctx, now_sec);
        let Some(why) = trigger else {
            // Healthy holder: stickiness keeps it even if a higher-tier candidate returns.
            return RoleTickOutcome {
                action: RoleAction::Renewed {
                    role: role.to_owned(),
                    holder,
                    epoch: current.epoch,
                },
                planned_handover: false,
                vacant_gap_secs: 0,
                spawn_required: false,
                alerts,
                fact_message: None,
                evaluated_candidates: evaluated,
            };
        };

        // Hysteresis: respect min_hold_until unless out of quota / vetoed / explicit handover.
        if now_sec < current.min_hold_until && !why.bypasses_min_hold() {
            return RoleTickOutcome {
                action: RoleAction::Renewed {
                    role: role.to_owned(),
                    holder,
                    epoch: current.epoch,
                },
                planned_handover: false,
                vacant_gap_secs: 0,
                spawn_required: false,
                alerts,
                fact_message: None,
                evaluated_candidates: evaluated,
            };
        }

        // Planned handover: choose successor without creating any vacant gap.
        let successor = if why == HandoverTrigger::ContextFull {
            eligible.first().map(|e| e.candidate.clone())
        } else {
            eligible
                .iter()
                .find(|e| e.candidate.who != holder)
                .map(|e| e.candidate.clone())
        };

        let Some(next_cand) = successor else {
            alerts.push(RoleAlert {
                code: "COORD_BUDGET".to_owned(),
                key: format!("{role}/no_successor"),
                severity: "critical".to_owned(),
                to: "owner".to_owned(),
                text: format!(
                    "role {role} needs handover ({}) but no successor is eligible",
                    why.as_reason()
                ),
                data: json!({
                    "role": role,
                    "holder": holder,
                    "reason": why.as_reason(),
                }),
            });
            return RoleTickOutcome {
                action: RoleAction::Renewed {
                    role: role.to_owned(),
                    holder,
                    epoch: current.epoch,
                },
                planned_handover: false,
                vacant_gap_secs: 0,
                spawn_required: false,
                alerts,
                fact_message: None,
                evaluated_candidates: evaluated,
            };
        };

        if let Some(req) = ctx.handover_requests.get(role)
            && !req.note.is_empty()
            && let Some(r) = table.rows.get_mut(&key)
        {
            r.handover_note = Some(req.note.clone());
        }
        if why == HandoverTrigger::Vetoed {
            table.lost_at.insert((role.to_owned(), holder), now_sec);
        }

        return grant_full(
            table,
            role,
            &next_cand,
            policy,
            &why.as_reason(),
            true,
            alerts,
            evaluated,
            now_sec,
        );
    }

    // Holder is dead or role is vacant (unplanned failover).
    let lose_reason = if let Some(h) = &current.holder {
        table.lost_at.insert((role.to_owned(), h.clone()), now_sec);
        if table.unreachable.contains(h) {
            "process gone"
        } else {
            "heartbeat timeout"
        }
    } else {
        "vacant"
    };

    let Some(winner) = eligible.first().map(|e| e.candidate.clone()) else {
        let row = table
            .rows
            .entry(key)
            .or_insert_with(|| RoleRow::vacant(role, "global"));
        row.holder = None;
        row.state = RoleState::Vacant;
        row.last_reason = lose_reason.to_owned();
        let epoch = row.epoch;
        alerts.push(RoleAlert {
            code: "ROLE_VACANT".to_owned(),
            key: role.to_owned(),
            severity: "critical".to_owned(),
            to: "owner".to_owned(),
            text: format!("role {role} is vacant ({lose_reason}): no eligible candidates"),
            data: json!({
                "role": role,
                "epoch": epoch,
                "reason": lose_reason,
            }),
        });
        let fact_message = table.fact_message(role);
        return RoleTickOutcome {
            action: RoleAction::Vacant {
                role: role.to_owned(),
                epoch,
                reason: lose_reason.to_owned(),
            },
            planned_handover: false,
            vacant_gap_secs: 0,
            spawn_required: false,
            alerts,
            fact_message,
            evaluated_candidates: evaluated,
        };
    };

    grant_full(
        table,
        role,
        &winner,
        policy,
        lose_reason,
        false,
        alerts,
        evaluated,
        now_sec,
    )
}

#[allow(clippy::too_many_arguments)]
fn grant_full(
    table: &mut RoleTable,
    role: &str,
    winner: &RoleCandidate,
    policy: &FullRolePolicy,
    reason: &str,
    planned_handover: bool,
    mut alerts: Vec<RoleAlert>,
    evaluated: Vec<CandidateEligibility>,
    now_sec: u64,
) -> RoleTickOutcome {
    let key = (role.to_owned(), "global".to_owned());
    let history = table.grant_history.entry(key.clone()).or_default();
    history.retain(|&ts| now_sec.saturating_sub(ts) <= 3600);
    history.push(now_sec);
    let changes_last_hour = history.len() as u32;

    let row = table
        .rows
        .entry(key)
        .or_insert_with(|| RoleRow::vacant(role, "global"));

    if policy.base.max_changes_per_hour > 0 && changes_last_hour > policy.base.max_changes_per_hour
    {
        row.frozen = true;
        row.changes_last_hour = changes_last_hour;
        row.last_reason = "flapping guard".to_owned();
        let epoch = row.epoch;
        alerts.push(RoleAlert {
            code: "ROLE_FLAPPING".to_owned(),
            key: role.to_owned(),
            severity: "critical".to_owned(),
            to: "owner".to_owned(),
            text: format!(
                "role {role} frozen after {changes_last_hour} changes in one hour (limit {})",
                policy.base.max_changes_per_hour
            ),
            data: json!({
                "role": role,
                "epoch": epoch,
                "changes_last_hour": changes_last_hour,
                "max_changes_per_hour": policy.base.max_changes_per_hour,
            }),
        });
        let fact_message = table.fact_message(role);
        return RoleTickOutcome {
            action: RoleAction::Frozen {
                role: role.to_owned(),
                epoch,
                changes_last_hour,
            },
            planned_handover,
            vacant_gap_secs: 0,
            spawn_required: false,
            alerts,
            fact_message,
            evaluated_candidates: evaluated,
        };
    }

    let prev_holder = row.holder.clone();
    let new_epoch = row.epoch + 1;
    row.holder = Some(winner.who.clone());
    row.lease_scope = winner.scope;
    row.epoch = new_epoch;
    row.state = RoleState::Held;
    row.granted_at = now_sec;
    row.heartbeat_at = now_sec;
    row.min_hold_until = now_sec.saturating_add(policy.base.min_hold_min.saturating_mul(60));
    row.last_reason = reason.to_owned();
    row.changes_last_hour = changes_last_hour;
    table
        .candidate_heartbeats
        .insert(winner.who.clone(), now_sec);

    alerts.push(RoleAlert {
        code: "ROLE_CHANGED".to_owned(),
        key: role.to_owned(),
        severity: "info".to_owned(),
        to: "coordinator".to_owned(),
        text: format!(
            "role {role} granted to {} (epoch {new_epoch}, scope {}, reason: {reason})",
            winner.who,
            winner.scope.as_str()
        ),
        data: json!({
            "role": role,
            "previous_holder": prev_holder,
            "holder": winner.who,
            "lease_scope": winner.scope.as_str(),
            "epoch": new_epoch,
            "planned_handover": planned_handover,
            "reason": reason,
        }),
    });

    let fact_message = table.fact_message(role);
    RoleTickOutcome {
        action: RoleAction::Granted {
            role: role.to_owned(),
            holder: winner.who.clone(),
            lease_scope: winner.scope,
            epoch: new_epoch,
            reason: reason.to_owned(),
        },
        planned_handover,
        vacant_gap_secs: 0,
        spawn_required: winner.spawn,
        alerts,
        fact_message,
        evaluated_candidates: evaluated,
    }
}

/// Converts a `RoleTickOutcome` into bus `FACT` and `ALERT` messages.
pub fn outcome_to_messages(outcome: &RoleTickOutcome, now_sec: u64) -> Vec<Message> {
    let ts = rfc3339(now_sec.saturating_mul(1000));
    let mut msgs = Vec::new();
    if let Some(mut fact) = outcome.fact_message.clone() {
        fact.ts = ts.clone();
        msgs.push(fact);
    }
    for al in &outcome.alerts {
        msgs.push(Message {
            id: 0,
            ts: ts.clone(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Alert,
            topic: format!("alert/{}/{}", al.code, al.key),
            body: json!({
                "v": 1,
                "code": al.code,
                "key": al.key,
                "severity": al.severity,
                "to": al.to,
                "state": "open",
                "opened_at": now_sec,
                "last_at": now_sec,
                "count": 1,
                "text": al.text,
                "data": al.data,
            })
            .to_string(),
            refs: vec!["state:open".into()],
            to: Vec::new(),
        });
    }
    msgs
}

/// `StoreModule` implementation registering the `ROLE` table into `ModuleRegistry` (section 10.2).
pub struct RoleStoreModule;

const ROLE_AND_HELLO_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS ROLE (
    role TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'global',
    holder TEXT,
    lease_scope TEXT NOT NULL DEFAULT 'full',
    epoch INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'vacant',
    granted_at INTEGER NOT NULL DEFAULT 0,
    heartbeat_at INTEGER NOT NULL DEFAULT 0,
    min_hold_until INTEGER NOT NULL DEFAULT 0,
    last_reason TEXT NOT NULL DEFAULT '',
    changes_last_hour INTEGER NOT NULL DEFAULT 0,
    frozen INTEGER NOT NULL DEFAULT 0,
    handover_note TEXT,
    msg_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (role, scope)
);

CREATE TABLE IF NOT EXISTS ROLE_HELLO (
    owner TEXT PRIMARY KEY,
    capabilities TEXT NOT NULL DEFAULT '[]',
    roles TEXT NOT NULL DEFAULT '[]',
    msg_id INTEGER NOT NULL DEFAULT 0
);";

impl StoreModule for RoleStoreModule {
    fn name(&self) -> &'static str {
        "role"
    }

    fn schema(&self) -> &'static str {
        ROLE_AND_HELLO_SCHEMA_SQL
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        super::apply(tx, msg).map_err(|e| format!("role apply error: {e}"))?;
        if (msg.kind == Kind::Hello
            || (msg.kind == Kind::Fact && msg.topic.starts_with("capacity/")))
            && !msg.from.is_empty()
        {
            let owner = if msg.kind == Kind::Fact {
                msg.topic
                    .split('/')
                    .nth(1)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&msg.from)
            } else {
                &msg.from
            };
            // A key member below tier 1 has no capacity and so no row of what it announced.
            if crate::store::core::key_member_without_capacity(tx, owner)
                || crate::store::core::key_member_without_capacity(tx, &msg.from)
            {
                return Ok(());
            }
            let (caps, roles) = parse_hello_capabilities_and_roles(&msg.body, &msg.refs);
            let caps_json = serde_json::to_string(&caps).unwrap_or_else(|_| "[]".into());
            let roles_json = serde_json::to_string(&roles).unwrap_or_else(|_| "[]".into());
            tx.execute(
                "INSERT INTO ROLE_HELLO (owner, capabilities, roles, msg_id)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(owner) DO UPDATE SET
                    capabilities = excluded.capabilities,
                    roles = excluded.roles,
                    msg_id = excluded.msg_id",
                rusqlite::params![owner, caps_json, roles_json, msg.id as i64],
            )
            .map_err(|e| format!("role hello apply error: {e}"))?;
        }
        Ok(())
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let rows = super::query_all(conn).map_err(|e| format!("role query error: {e}"))?;
        let vals: Vec<Value> = rows
            .into_iter()
            .map(|r| serde_json::to_value(r).unwrap_or_else(|_| json!({})))
            .collect();
        state.insert("roles".into(), Value::Array(vals));
        Ok(())
    }
}

/// Hub tick hook for role election (`src/hooks.rs`).
/// Only runs if a `roles.json` with configured role policies exists in the hub data directory.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let roles_json = hub.config().data_dir.join("roles.json");
    if !roles_json.is_file() {
        return Vec::new();
    }
    let role_store = super::RoleStore::new(&hub.config().data_dir);
    let Ok(mut table) = role_store.load() else {
        return Vec::new();
    };
    if table.policies.is_empty() {
        return Vec::new();
    }
    let now_sec = now_ms / 1000;
    let ctx = hub
        .with_store(|store| build_context_from_store(store.connection()))
        .unwrap_or_default();

    let role_names: Vec<String> = table.policies.keys().cloned().collect();
    let mut out = Vec::new();
    let mut changed = false;
    for rname in role_names {
        let Some(base) = table.policies.get(&rname).cloned() else {
            continue;
        };
        let full = FullRolePolicy {
            base,
            ..FullRolePolicy::default()
        };
        let outcome = role_tick_full(&mut table, &rname, &full, &ctx, now_sec);
        let msgs = outcome_to_messages(&outcome, now_sec);
        if !msgs.is_empty() {
            changed = true;
            out.extend(msgs);
        }
    }
    if changed {
        let _ = role_store.save(&table);
    }
    out
}

/// Module ops wired into `src/hooks.rs` for `role.*` queries.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    match op {
        "role.list" => {
            let rows = super::query_all(ctx.store.connection()).map_err(|e| e.to_string())?;
            out.insert("roles".into(), json!(rows));
            Ok(out)
        }
        "role.check" => {
            let role = body["role"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "role.check requires `role`".to_owned())?;
            let scope = body["scope"].as_str().unwrap_or("global");
            let given_epoch = body["epoch"].as_u64().unwrap_or(0);
            let cmd = body["cmd"].as_str().and_then(super::RoleCommand::parse);
            let Some(row) =
                super::query_row(ctx.store.connection(), role, scope).map_err(|e| e.to_string())?
            else {
                return Err(format!(
                    "you are no longer {role} (your epoch {given_epoch}, current 0), stop"
                ));
            };
            super::check_fence(&row, Some(&ctx.caller.name), given_epoch, cmd)
                .map_err(|r| r.message)?;
            out.insert("role".into(), json!(row));
            Ok(out)
        }
        other => Err(format!("unknown role op `{other}`")),
    }
}

fn push_hello_list(out: &mut BTreeSet<String>, raw: &str) {
    for item in raw.split([',', '+']) {
        let item = item.trim();
        if !item.is_empty() {
            out.insert(item.to_owned());
        }
    }
}

fn parse_hello_capabilities_and_roles(body: &str, refs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut caps = BTreeSet::new();
    let mut roles = BTreeSet::new();

    if body.trim_start().starts_with('{')
        && let Ok(val) = serde_json::from_str::<Value>(body)
    {
        for key in ["capabilities", "capability", "caps", "cap"] {
            if let Some(arr) = val.get(key).and_then(Value::as_array) {
                for item in arr.iter().filter_map(Value::as_str) {
                    push_hello_list(&mut caps, item);
                }
            } else if let Some(s) = val.get(key).and_then(Value::as_str) {
                push_hello_list(&mut caps, s);
            }
        }
        for key in ["roles", "role"] {
            if let Some(arr) = val.get(key).and_then(Value::as_array) {
                for item in arr.iter().filter_map(Value::as_str) {
                    push_hello_list(&mut roles, item);
                }
            } else if let Some(s) = val.get(key).and_then(Value::as_str) {
                push_hello_list(&mut roles, s);
            }
        }
    }

    for seg in body.split(['|', ';']) {
        for part in seg.split_whitespace() {
            let trimmed = part.trim_matches(',');
            if let Some((k, v)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) {
                let key = k.trim().to_ascii_lowercase().replace('-', "_");
                let val = v.trim().trim_matches(',');
                match key.as_str() {
                    "capabilities" | "capability" | "caps" | "cap" => {
                        push_hello_list(&mut caps, val);
                    }
                    "roles" | "role" => {
                        push_hello_list(&mut roles, val);
                    }
                    _ => {}
                }
            }
        }
    }
    for r in refs {
        if let Some((k, v)) = r.split_once(':').or_else(|| r.split_once('=')) {
            let key = k.trim().to_ascii_lowercase().replace('-', "_");
            let val = v.trim();
            match key.as_str() {
                "capabilities" | "capability" | "caps" | "cap" => {
                    push_hello_list(&mut caps, val);
                }
                "roles" | "role" => {
                    push_hello_list(&mut roles, val);
                }
                _ => {}
            }
        }
    }

    (caps.into_iter().collect(), roles.into_iter().collect())
}

/// Backfills any missing `ROLE_HELLO` rows from existing `CAPACITY` and `MEMBER` rows in `state.db`
/// during schema migration when `ROLE_HELLO` is newly created on a pre-existing database.
pub fn backfill_role_hello_from_tables(tx: &Transaction<'_>) -> Result<(), String> {
    let mut by_owner: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>, i64)> = BTreeMap::new();
    if let Ok(mut stmt) = tx.prepare("SELECT owner, accepts, msg_id FROM CAPACITY WHERE msg_id > 0")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
    {
        for (owner, accepts_raw, msg_id) in rows.flatten() {
            let entry = by_owner
                .entry(owner)
                .or_insert_with(|| (BTreeSet::new(), BTreeSet::new(), msg_id));
            entry.2 = entry.2.max(msg_id);
            for item in serde_json::from_str::<Vec<String>>(&accepts_raw).unwrap_or_default() {
                if let Some(r) = item
                    .strip_prefix("role:")
                    .or_else(|| item.strip_prefix("roles:"))
                    .or_else(|| item.strip_prefix("role="))
                    .or_else(|| item.strip_prefix("roles="))
                {
                    push_hello_list(&mut entry.1, r);
                } else if let Some(c) = item
                    .strip_prefix("cap:")
                    .or_else(|| item.strip_prefix("caps:"))
                    .or_else(|| item.strip_prefix("capability:"))
                    .or_else(|| item.strip_prefix("capabilities:"))
                    .or_else(|| item.strip_prefix("cap="))
                    .or_else(|| item.strip_prefix("caps="))
                    .or_else(|| item.strip_prefix("capability="))
                    .or_else(|| item.strip_prefix("capabilities="))
                {
                    push_hello_list(&mut entry.0, c);
                }
            }
        }
    }
    if let Ok(mut stmt) = tx.prepare("SELECT name, msg_id FROM MEMBER WHERE msg_id > 0")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
    {
        for (name, msg_id) in rows.flatten() {
            by_owner
                .entry(name)
                .or_insert_with(|| (BTreeSet::new(), BTreeSet::new(), msg_id));
        }
    }
    for (owner, (caps, roles, msg_id)) in by_owner {
        let caps_json = serde_json::to_string(&caps.into_iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".into());
        let roles_json = serde_json::to_string(&roles.into_iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT OR IGNORE INTO ROLE_HELLO (owner, capabilities, roles, msg_id)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![owner, caps_json, roles_json, msg_id],
        )
        .map_err(|e| format!("role hello backfill error: {e}"))?;
    }
    Ok(())
}

/// Builds an `EligibilityContext` from the current `state.db` projections.
pub fn build_context_from_store(conn: &Connection) -> EligibilityContext {
    let mut ctx = EligibilityContext::default();
    for m in crate::store::query_members(conn).unwrap_or_default() {
        ctx.valid_tokens.insert(m.name.clone());
        if m.online {
            ctx.online_members.insert(m.name);
        }
    }
    for mach in crate::store::query_machines(conn).unwrap_or_default() {
        if mach.heartbeat_at > 0 {
            ctx.supervisor_heartbeats
                .entry(mach.owner)
                .and_modify(|prev| *prev = (*prev).max(mach.heartbeat_at))
                .or_insert(mach.heartbeat_at);
        }
    }
    for cap in crate::store::query_capacities(conn).unwrap_or_default() {
        if !cap.pools.is_empty() {
            ctx.class_pools.insert(cap.class, cap.pools);
        }
    }
    for meter in crate::quota::apply::load_all_meters(conn).unwrap_or_default() {
        ctx.meters
            .insert((meter.owner.clone(), meter.pool.clone()), meter);
    }
    if let Ok(mut stmt) = conn.prepare("SELECT owner, capabilities, roles FROM ROLE_HELLO")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
    {
        for (owner, caps_raw, roles_raw) in rows.flatten() {
            let caps: BTreeSet<String> = serde_json::from_str::<Vec<String>>(&caps_raw)
                .unwrap_or_default()
                .into_iter()
                .collect();
            let roles: BTreeSet<String> = serde_json::from_str::<Vec<String>>(&roles_raw)
                .unwrap_or_default()
                .into_iter()
                .collect();
            ctx.capabilities.insert(owner.clone(), caps);
            ctx.hello_roles.insert(owner, roles);
        }
    }
    ctx
}
