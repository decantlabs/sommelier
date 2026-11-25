//! Slot matching, admission wiring, and deterministic task assignment (sections 4.1, 7.1, 7.2).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::quota::admit::{AdmitDecision, SlotAdmitInput, TaskAdmitInput, admit};
use crate::quota::estimate::{Estimate, estimate};
use crate::quota::forecast::{ActiveLease, forecast, pool_state};
use crate::quota::meter::{Meter, PoolState, Source};
use crate::task::scopes_overlap;

use super::score::ScoreBreakdown;
use super::{ClassSpec, PauseScope, PlanMode, PlanPolicy, PlanState, PlanTask, Tier};

/// One concrete free capacity slot in the scheduler pool `S`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeSlot {
    /// Deterministic slot key: `<owner>/<machine>/<class>/<slot_index>`.
    pub key: String,
    pub owner: String,
    pub machine: String,
    pub class: String,
    /// `"auto"` (direct lease) or `"own"` (offer to guest).
    pub mode: String,
    pub accepts: BTreeSet<String>,
    pub pools: Vec<String>,
    pub family_pool: Option<String>,
    pub family: Option<String>,
    pub free_since: Option<u64>,
}

/// Reason a candidate slot did not accept a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRejection {
    pub slot_key: String,
    pub owner: String,
    pub class: String,
    pub rule: String,
    pub reason: String,
}

/// Detailed evaluation of one candidate slot for `plan --explain`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotEvaluation {
    pub slot_key: String,
    pub owner: String,
    pub class: String,
    pub admitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_admit_reason: Option<String>,
}

/// Full explanation for a single task (`sommelier plan --explain <id>`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskExplanation {
    pub task_id: String,
    pub state: String,
    pub eligible_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_ready_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<ScoreBreakdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadow_admit_denials: Vec<SlotRejection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slot_evaluations: Vec<SlotEvaluation>,
}

/// Mutable working view of active counts, locks, scopes, and committed pool budgets during one tick.
#[derive(Clone, Debug, Default)]
pub struct AssignContext {
    /// Active task scopes: `(task_id, scope)`.
    pub active_scopes: BTreeMap<String, String>,
    /// Active leases per owner (for utilisation calculation).
    pub owner_active: BTreeMap<String, u32>,
    /// Total declared useful_parallel per owner (for utilisation calculation).
    pub owner_total_parallel: BTreeMap<String, u32>,
    /// Active leases per `(owner, class)`.
    pub owner_class_active: BTreeMap<(String, String), u32>,
    /// Active leases per `(owner, machine, class)`.
    pub machine_class_active: BTreeMap<(String, String, String), u32>,
    /// Active leases per `(owner, family)`.
    pub owner_family_active: BTreeMap<(String, String), u32>,
    /// Active class locks currently held.
    pub active_locks: BTreeSet<String>,
    /// Working meters keyed by `(owner, pool)` with probe switches applied.
    pub meters: BTreeMap<(String, String), Meter>,
    /// Active leases per `(owner, pool)` for quota forecasting and throttling.
    pub active_leases_by_owner_pool: BTreeMap<(String, String), Vec<ActiveLease>>,
    /// Committed percentage per `(owner, pool)`.
    pub committed_pct_by_owner_pool: BTreeMap<(String, String), f64>,
}

impl AssignContext {
    /// Builds initial assignment context from canonical `PlanState` and `PlanPolicy`.
    pub fn from_state(
        state: &PlanState,
        policy: &PlanPolicy,
        released_leases: &BTreeSet<(String, u32)>,
        now: u64,
    ) -> Self {
        let mut ctx = Self::default();

        // Populate working meters with vendor probe switch overrides (section 7.1).
        for ((owner, pool), m) in &state.meters {
            let mut wm = m.clone();
            if let Some(mp) = policy.meters.get(pool) {
                wm.policy = mp.clone();
            }
            if let Some(&enabled) = policy.switch.probes.get(&wm.vendor)
                && !enabled
                && wm
                    .latest
                    .as_ref()
                    .is_none_or(|s| s.source != Source::Manual)
            {
                wm.probe_enabled = false;
            }
            ctx.meters.insert((owner.clone(), pool.clone()), wm);
        }

        // Record total declared useful parallel slots per owner.
        for cap in state.capacities.values() {
            let total = ctx
                .owner_total_parallel
                .entry(cap.owner.clone())
                .or_insert(0);
            *total = total.saturating_add(cap.useful_parallel);
        }

        // Seed externally held locks.
        for l in &state.active_locks {
            ctx.active_locks.insert(l.clone());
        }

        // Seed active scopes from tasks currently in leased or running state unless released this tick.
        let released_tasks: BTreeSet<&str> = released_leases
            .iter()
            .map(|(tid, _)| tid.as_str())
            .collect();
        for (tid, t) in &state.tasks {
            if matches!(t.state.as_str(), "leased" | "running")
                && !released_tasks.contains(tid.as_str())
            {
                ctx.active_scopes.insert(tid.clone(), t.scope.clone());
            }
        }

        // Seed counts, locks, and committed pool budgets from non-released active/suspect leases.
        for ((tid, att), lease) in &state.leases {
            if released_leases.contains(&(tid.clone(), *att)) {
                continue;
            }
            if !matches!(lease.state.as_str(), "active" | "suspect") {
                continue;
            }
            if let Some(t) = state.tasks.get(tid) {
                ctx.active_scopes.insert(tid.clone(), t.scope.clone());
            }
            *ctx.owner_active.entry(lease.owner.clone()).or_insert(0) += 1;
            *ctx.owner_class_active
                .entry((lease.owner.clone(), lease.class.clone()))
                .or_insert(0) += 1;
            *ctx.machine_class_active
                .entry((
                    lease.owner.clone(),
                    lease.machine.clone(),
                    lease.class.clone(),
                ))
                .or_insert(0) += 1;

            let family = lease
                .family
                .clone()
                .or_else(|| {
                    policy
                        .classes
                        .get(&lease.class)
                        .and_then(|c| c.family_pool.clone())
                })
                .filter(|f| !f.is_empty());
            if let Some(fam) = family {
                *ctx.owner_family_active
                    .entry((lease.owner.clone(), fam))
                    .or_insert(0) += 1;
            }
            if let Some(lock_name) = policy
                .classes
                .get(&lease.class)
                .and_then(|c| c.lock.as_deref())
                .filter(|l| !l.is_empty())
            {
                ctx.active_locks.insert(lock_name.to_owned());
            }

            let pools = if lease.pools.is_empty() {
                policy
                    .classes
                    .get(&lease.class)
                    .map(|c| c.pools.clone())
                    .unwrap_or_default()
            } else {
                lease.pools.clone()
            };
            let rem_min = if lease.est_end_at > now {
                ((lease.est_end_at - now) as f64 / 60.0).max(1.0)
            } else if lease.est_minutes_p90 > 0.0 {
                lease.est_minutes_p90.max(1.0)
            } else {
                30.0
            };
            for pool in &pools {
                let pct_per_unit = ctx
                    .lookup_meter(&lease.owner, pool)
                    .map_or(0.0, |m| m.pct_per_unit);
                let rem_pct = (lease.est_cost * pct_per_unit).max(0.0);
                ctx.active_leases_by_owner_pool
                    .entry((lease.owner.clone(), pool.clone()))
                    .or_default()
                    .push(ActiveLease {
                        pools: pools.clone(),
                        remaining_cost_pct: rem_pct,
                        remaining_minutes: rem_min,
                    });
                *ctx.committed_pct_by_owner_pool
                    .entry((lease.owner.clone(), pool.clone()))
                    .or_insert(0.0) += rem_pct;
            }
        }

        ctx
    }

    /// Looks up a meter for `(owner, pool)`, falling back to global `("", pool)` if present.
    pub fn lookup_meter(&self, owner: &str, pool: &str) -> Option<&Meter> {
        self.meters
            .get(&(owner.to_owned(), pool.to_owned()))
            .or_else(|| self.meters.get(&(String::new(), pool.to_owned())))
    }

    /// Collects all meters relevant to `owner` (owner-specific first, then global fallback)
    /// into a `BTreeMap<pool, Meter>` for `crate::quota::admit::admit`.
    pub fn meters_for_owner(&self, owner: &str) -> BTreeMap<String, Meter> {
        let mut out = BTreeMap::new();
        for ((m_owner, pool), m) in &self.meters {
            if m_owner.is_empty() {
                out.insert(pool.clone(), m.clone());
            }
        }
        for ((m_owner, pool), m) in &self.meters {
            if m_owner == owner {
                out.insert(pool.clone(), m.clone());
            }
        }
        out
    }

    /// Collects active leases per pool for `owner`.
    pub fn active_leases_for_owner(&self, owner: &str) -> BTreeMap<String, Vec<ActiveLease>> {
        let mut out = BTreeMap::new();
        for ((l_owner, pool), leases) in &self.active_leases_by_owner_pool {
            if l_owner == owner {
                out.insert(pool.clone(), leases.clone());
            }
        }
        out
    }

    /// Collects committed percentages per pool for `owner`.
    pub fn committed_for_owner(&self, owner: &str) -> BTreeMap<String, f64> {
        let mut out = BTreeMap::new();
        for ((c_owner, pool), &pct) in &self.committed_pct_by_owner_pool {
            if c_owner == owner {
                out.insert(pool.clone(), pct);
            }
        }
        out
    }

    /// Records a newly scheduled lease or offer into the working context so subsequent tasks
    /// in the same tick respect scope locks, caps, locks, and committed quota headroom.
    pub fn record_assignment(
        &mut self,
        task: &PlanTask,
        slot: &FreeSlot,
        est: &Estimate,
        class_spec: Option<&ClassSpec>,
        is_lease: bool,
    ) {
        self.active_scopes
            .insert(task.id.clone(), task.scope.clone());
        *self.owner_active.entry(slot.owner.clone()).or_insert(0) += 1;
        *self
            .owner_class_active
            .entry((slot.owner.clone(), slot.class.clone()))
            .or_insert(0) += 1;
        *self
            .machine_class_active
            .entry((slot.owner.clone(), slot.machine.clone(), slot.class.clone()))
            .or_insert(0) += 1;

        let family = slot
            .family
            .clone()
            .or_else(|| class_spec.and_then(|c| c.family_pool.clone()))
            .filter(|f| !f.is_empty());
        if let Some(fam) = family {
            *self
                .owner_family_active
                .entry((slot.owner.clone(), fam))
                .or_insert(0) += 1;
        }
        if let Some(lock_name) = class_spec
            .and_then(|c| c.lock.as_deref())
            .filter(|l| !l.is_empty())
        {
            self.active_locks.insert(lock_name.to_owned());
        }

        if is_lease {
            let mut all_pools = slot.pools.clone();
            if let Some(fp) = &slot.family_pool
                && !fp.is_empty()
                && !all_pools.contains(fp)
            {
                all_pools.push(fp.clone());
            }
            for pool in &all_pools {
                let pct_per_unit = self
                    .lookup_meter(&slot.owner, pool)
                    .map_or(0.0, |m| m.pct_per_unit);
                let cost_pct = (est.units_p90 * pct_per_unit).max(0.0);
                self.active_leases_by_owner_pool
                    .entry((slot.owner.clone(), pool.clone()))
                    .or_default()
                    .push(ActiveLease {
                        pools: all_pools.clone(),
                        remaining_cost_pct: cost_pct,
                        remaining_minutes: est.minutes_p90.max(1.0),
                    });
                *self
                    .committed_pct_by_owner_pool
                    .entry((slot.owner.clone(), pool.clone()))
                    .or_insert(0.0) += cost_pct;
            }
        }
    }
}

/// Constructs the deterministic list of free slots `S` from `CAPACITY` minus active leases.
pub fn build_free_slots(
    state: &PlanState,
    policy: &PlanPolicy,
    ctx: &AssignContext,
) -> Vec<FreeSlot> {
    let mut slots = Vec::new();
    for cap in state.capacities.values() {
        let used = ctx
            .machine_class_active
            .get(&(cap.owner.clone(), cap.machine.clone(), cap.class.clone()))
            .copied()
            .unwrap_or(0);
        let free = cap
            .useful_parallel
            .saturating_sub(used)
            .min(crate::derive::capacity::MAX_CLASS_SLOTS);
        let class_spec = policy.classes.get(&cap.class);
        let pools = if !cap.pools.is_empty() {
            cap.pools.clone()
        } else if let Some(cs) = class_spec {
            cs.pools.clone()
        } else {
            Vec::new()
        };
        let family_pool = class_spec.and_then(|cs| cs.family_pool.clone());
        let family = cap
            .family
            .clone()
            .or_else(|| family_pool.clone())
            .filter(|f| !f.is_empty());

        for idx in 1..=free {
            let slot_num = used + idx;
            slots.push(FreeSlot {
                key: format!("{}/{}/{}/{}", cap.owner, cap.machine, cap.class, slot_num),
                owner: cap.owner.clone(),
                machine: cap.machine.clone(),
                class: cap.class.clone(),
                mode: if cap.mode.is_empty() {
                    "auto".to_owned()
                } else {
                    cap.mode.clone()
                },
                accepts: cap.accepts.clone(),
                pools: pools.clone(),
                family_pool: family_pool.clone(),
                family: family.clone(),
                free_since: cap.free_since,
            });
        }
    }
    slots.sort_by(|a, b| a.key.cmp(&b.key));
    slots
}

/// Checks if `task.scope` overlaps any currently active task scope (prefix rule of `dispatch.rs`).
pub fn find_scope_conflict(task: &PlanTask, ctx: &AssignContext) -> Option<String> {
    for (other_id, other_scope) in &ctx.active_scopes {
        if other_id != &task.id && scopes_overlap(&task.scope, other_scope) {
            return Some(other_id.clone());
        }
    }
    None
}

/// Resolves the cost and duration estimate for `(task, class)` (section 7.5).
pub fn resolve_estimate(
    task: &PlanTask,
    class: &str,
    state: &PlanState,
    policy: &PlanPolicy,
) -> Estimate {
    let learned = state.estimates.get(&(task.kind.clone(), class.to_owned()));
    let default_est = policy
        .default_estimates
        .get(class)
        .cloned()
        .unwrap_or_else(|| Estimate::default_for(&task.kind, class));
    estimate(&task.kind, class, learned, &default_est, task.size)
}

/// Evaluates whether `slot` may accept `task` at `now` under all rules of sections 4.1, 7.1, 7.2.
/// Returns `Ok((estimate, shadow_admit_denial))` when admissible, or `Err(SlotRejection)` naming
/// the first rule that failed.
pub fn evaluate_slot_for_task(
    task: &PlanTask,
    slot: &FreeSlot,
    state: &PlanState,
    policy: &PlanPolicy,
    ctx: &AssignContext,
    now: u64,
) -> Result<(Estimate, Option<String>), SlotRejection> {
    let reject = |rule: &str, reason: String| SlotRejection {
        slot_key: slot.key.clone(),
        owner: slot.owner.clone(),
        class: slot.class.clone(),
        rule: rule.to_owned(),
        reason,
    };

    // 1. Pause checks (all, owner, class, pools).
    if state.pauses.contains(&PauseScope::All) {
        return Err(reject("paused", "paused: all".to_owned()));
    }
    if state
        .pauses
        .contains(&PauseScope::Owner(slot.owner.clone()))
    {
        return Err(reject("paused", format!("paused: owner {}", slot.owner)));
    }
    if state
        .pauses
        .contains(&PauseScope::Class(slot.class.clone()))
    {
        return Err(reject("paused", format!("paused: class {}", slot.class)));
    }
    for p in slot.pools.iter().chain(slot.family_pool.iter()) {
        if state.pauses.contains(&PauseScope::Pool(p.clone())) {
            return Err(reject("paused", format!("paused: pool {p}")));
        }
    }

    // 2. Class acceptance & spawn check.
    if !task.accepts.is_empty()
        && !task
            .accepts
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&slot.class))
    {
        return Err(reject(
            "class_accepts",
            format!("task does not accept class {}", slot.class),
        ));
    }
    let class_spec = policy.classes.get(&slot.class);
    if let Some(cs) = class_spec
        && !cs.spawn
        && slot.mode == "auto"
    {
        return Err(reject(
            "spawn_disabled",
            format!("class {} has spawn disabled", slot.class),
        ));
    }

    // 3. Minimum tier check.
    let required_tier = if !task.min_tier.trim().is_empty() {
        Tier::parse(&task.min_tier).unwrap_or(Tier::Low)
    } else {
        policy
            .kinds
            .get(&task.kind)
            .and_then(|k| k.min_tier)
            .unwrap_or(Tier::Low)
    };
    let slot_tier = class_spec.map_or(Tier::Middle, |c| c.tier);
    if slot_tier < required_tier {
        return Err(reject(
            "min_tier",
            format!(
                "class {} tier {} below required {}",
                slot.class,
                slot_tier.as_str(),
                required_tier.as_str()
            ),
        ));
    }

    // 4. Trust & kind acceptance & clean room check.
    let trust = state
        .members
        .get(&slot.owner)
        .map_or("owner", |m| m.trust.as_str());
    if !trust_allows_kind(trust, &task.kind, policy) {
        return Err(reject(
            "trust",
            format!(
                "owner {} (trust {trust}) may not claim kind {}",
                slot.owner, task.kind
            ),
        ));
    }
    if !slot.accepts.is_empty() && !slot.accepts.contains(&task.kind) {
        return Err(reject(
            "slot_accepts",
            format!("slot {} does not accept kind {}", slot.key, task.kind),
        ));
    }
    if policy.kinds.get(&task.kind).is_some_and(|k| k.clean_room)
        && state
            .clean_room_readers
            .get(&slot.owner)
            .is_some_and(|r| !r.is_empty())
    {
        return Err(reject(
            "clean_room",
            format!(
                "owner {} held a reader task and cannot take clean room kind {}",
                slot.owner, task.kind
            ),
        ));
    }

    // 5. Class lock check.
    if let Some(lock_name) = class_spec
        .and_then(|c| c.lock.as_deref())
        .filter(|l| !l.is_empty())
        && ctx.active_locks.contains(lock_name)
    {
        return Err(reject("lock", format!("lock {lock_name} is already held")));
    }

    // 6. Caps check (per owner, per machine, per family). Missing = no cap, 0 = none allowed.
    if let Some(cs) = class_spec {
        if let Some(cap) = cs.cap_per_owner {
            let cur = ctx
                .owner_class_active
                .get(&(slot.owner.clone(), slot.class.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return Err(reject(
                    "cap_per_owner",
                    format!(
                        "cap_per_owner ({cap}) reached for {} on {}",
                        slot.class, slot.owner
                    ),
                ));
            }
        }
        if let Some(cap) = cs.cap_per_machine {
            let cur = ctx
                .machine_class_active
                .get(&(slot.owner.clone(), slot.machine.clone(), slot.class.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return Err(reject(
                    "cap_per_machine",
                    format!(
                        "cap_per_machine ({cap}) reached for {} on {}/{}",
                        slot.class, slot.owner, slot.machine
                    ),
                ));
            }
        }
        if let Some(cap) = cs.cap_per_family
            && let Some(fam) = &slot.family
        {
            let cur = ctx
                .owner_family_active
                .get(&(slot.owner.clone(), fam.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return Err(reject(
                    "cap_per_family",
                    format!("cap_per_family ({cap}) reached for {fam} on {}", slot.owner),
                ));
            }
        }
    }

    // 7. Admission control (sections 7.1 & 7.2).
    let est = resolve_estimate(task, &slot.class, state, policy);
    if policy.switch.admission == PlanMode::Off {
        return Ok((est, None));
    }

    let task_input = TaskAdmitInput {
        id: task.id.clone(),
        kind: task.kind.clone(),
        size: task.size,
        checkpointable: task.checkpointable,
        exclude_pools_until: task.exclude_pools_until.clone(),
    };
    let slot_input = SlotAdmitInput {
        class: slot.class.clone(),
        pools: slot.pools.clone(),
        family_pool: slot.family_pool.clone(),
    };
    let owner_meters = ctx.meters_for_owner(&slot.owner);
    let owner_leases = ctx.active_leases_for_owner(&slot.owner);
    let owner_committed = ctx.committed_for_owner(&slot.owner);

    let decision = admit(
        &task_input,
        &slot_input,
        &owner_meters,
        &est,
        &policy.quota,
        &owner_leases,
        &owner_committed,
        now,
    );

    match decision {
        AdmitDecision::Ok => Ok((est, None)),
        AdmitDecision::Deny(reason) => {
            if policy.switch.admission == PlanMode::Shadow {
                Ok((est, Some(reason)))
            } else {
                Err(reject("admission", reason))
            }
        }
    }
}

/// Checks whether `trust` level allows claiming `kind`.
pub fn trust_allows_kind(trust: &str, kind: &str, policy: &PlanPolicy) -> bool {
    match trust {
        "owner" | "orchestrator" | "hub" => true,
        "observer" | "tier0" => false,
        other => {
            if let Some(spec) = policy.trust.get(other) {
                spec.may_claim.contains("*") || spec.may_claim.contains(kind)
            } else {
                false
            }
        }
    }
}

/// Picks the best slot from admissible candidates by `(cost_rank(class), utilisation(owner), key)`
/// (section 4.1: "cheapest class, then the emptiest owner").
pub fn pick_best_slot<'a>(
    candidates: &[(&'a FreeSlot, Estimate, Option<String>)],
    policy: &PlanPolicy,
    ctx: &AssignContext,
) -> Option<(&'a FreeSlot, Estimate, Option<String>)> {
    candidates
        .iter()
        .min_by(|(sa, _, _), (sb, _, _)| {
            let cost_a = policy.classes.get(&sa.class).map_or(100, |c| c.cost_rank);
            let cost_b = policy.classes.get(&sb.class).map_or(100, |c| c.cost_rank);
            cost_a
                .cmp(&cost_b)
                .then_with(|| compare_utilisation(&sa.owner, &sb.owner, ctx))
                .then_with(|| sa.key.cmp(&sb.key))
        })
        .cloned()
}

/// Exact integer fraction comparison of `owner_active / owner_total_parallel` without float drift.
fn compare_utilisation(owner_a: &str, owner_b: &str, ctx: &AssignContext) -> std::cmp::Ordering {
    let act_a = u64::from(ctx.owner_active.get(owner_a).copied().unwrap_or(0));
    let tot_a = u64::from(
        ctx.owner_total_parallel
            .get(owner_a)
            .copied()
            .unwrap_or(1)
            .max(1),
    );
    let act_b = u64::from(ctx.owner_active.get(owner_b).copied().unwrap_or(0));
    let tot_b = u64::from(
        ctx.owner_total_parallel
            .get(owner_b)
            .copied()
            .unwrap_or(1)
            .max(1),
    );
    (act_a * tot_b).cmp(&(act_b * tot_a))
}

/// Checks whether a remaining free slot is admissible (not paused, not cap-blocked, not lock-blocked,
/// spawn enabled or own mode, and no pool paused or closed) for `BACKLOG_LOW` and safe mode checks.
pub fn is_slot_admissible(
    slot: &FreeSlot,
    state: &PlanState,
    policy: &PlanPolicy,
    ctx: &AssignContext,
    now: u64,
) -> bool {
    if state.pauses.contains(&PauseScope::All)
        || state
            .pauses
            .contains(&PauseScope::Owner(slot.owner.clone()))
        || state
            .pauses
            .contains(&PauseScope::Class(slot.class.clone()))
    {
        return false;
    }
    if let Some(cs) = policy.classes.get(&slot.class) {
        if !cs.spawn && slot.mode == "auto" {
            return false;
        }
        if let Some(lock) = cs.lock.as_deref().filter(|l| !l.is_empty())
            && ctx.active_locks.contains(lock)
        {
            return false;
        }
        if let Some(cap) = cs.cap_per_owner {
            let cur = ctx
                .owner_class_active
                .get(&(slot.owner.clone(), slot.class.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return false;
            }
        }
        if let Some(cap) = cs.cap_per_machine {
            let cur = ctx
                .machine_class_active
                .get(&(slot.owner.clone(), slot.machine.clone(), slot.class.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return false;
            }
        }
        if let Some(cap) = cs.cap_per_family
            && let Some(fam) = &slot.family
        {
            let cur = ctx
                .owner_family_active
                .get(&(slot.owner.clone(), fam.clone()))
                .copied()
                .unwrap_or(0);
            if cur >= cap {
                return false;
            }
        }
    }

    let owner_meters = ctx.meters_for_owner(&slot.owner);
    let owner_leases = ctx.active_leases_for_owner(&slot.owner);
    for pool in slot.pools.iter().chain(slot.family_pool.iter()) {
        if state.pauses.contains(&PauseScope::Pool(pool.clone())) {
            return false;
        }
        if let Some(m) = owner_meters.get(pool) {
            let leases = owner_leases.get(pool).map(Vec::as_slice).unwrap_or(&[]);
            let f = forecast(m, now, leases);
            let st = pool_state(m, &f, now, &policy.quota);
            if matches!(st, PoolState::Paused | PoolState::Closed) {
                return false;
            }
        }
    }
    true
}
