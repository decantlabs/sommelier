//! Scheduler invariant checks and safe mode trigger (sections 4.1 and 9).
//!
//! Section 4.1 invariants:
//! - One active lease per task.
//! - No two active tasks with overlapping scope.
//! - Caps respected (per owner, per machine, per family, useful parallel, class lock).
//! - No lease on a pool that is closed or paused (or paused scope).
//! - No lease that fails admission.
//! - On a violation: return `[Alert(INVARIANT)]` and nothing else.
//!
//! Section 9 safe mode:
//! - After a publish in enforce, an `INVARIANT` alert or 10 minutes with ready tasks,
//!   free admissible slots and no lease puts the scheduler into `shadow` (`SAFE_MODE` alert).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::quota::admit::{AdmitDecision, SlotAdmitInput, TaskAdmitInput, admit};
use crate::quota::forecast::{ActiveLease, forecast, pool_state};
use crate::quota::meter::PoolState;
use crate::task::scopes_overlap;

use super::assign::{AssignContext, resolve_estimate};
use super::{
    PauseScope, PlanAction, PlanMode, PlanPolicy, PlanState, deps_closed, has_required_fields,
};

/// One violated scheduler invariant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantViolation {
    pub code: String,
    pub message: String,
}

/// Persistent safe mode tracking state (section 9).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeModeState {
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_since: Option<u64>,
}

/// Independently verifies every scheduler invariant of section 4.1 over `actions` and `state`.
pub fn check_invariants(
    actions: &[PlanAction],
    state: &PlanState,
    policy: &PlanPolicy,
    now: u64,
) -> Result<(), Vec<InvariantViolation>> {
    let mut violations = Vec::new();

    // Collect leases released by this plan so they are no longer counted as active.
    let mut released_leases: BTreeSet<(String, u32)> = BTreeSet::new();
    let mut expired_offer_tasks: BTreeSet<String> = BTreeSet::new();
    for act in actions {
        match act {
            PlanAction::ReleaseLease {
                task_id, attempt, ..
            } => {
                released_leases.insert((task_id.clone(), *attempt));
            }
            PlanAction::ExpireOffer { task_id, .. } => {
                expired_offer_tasks.insert(task_id.clone());
            }
            _ => {}
        }
    }

    let mut ctx = AssignContext::from_state(state, policy, &released_leases, now);

    // Track active lease count per task (starting from existing non-released leases).
    let mut active_lease_count_by_task: BTreeMap<String, usize> = BTreeMap::new();
    for ((tid, att), l) in &state.leases {
        if !released_leases.contains(&(tid.clone(), *att))
            && matches!(l.state.as_str(), "active" | "suspect")
        {
            *active_lease_count_by_task.entry(tid.clone()).or_insert(0) += 1;
        }
    }
    let mut offered_tasks_in_plan: BTreeSet<String> = BTreeSet::new();
    let mut used_slot_keys: BTreeSet<String> = BTreeSet::new();

    for act in actions {
        match act {
            PlanAction::Offer {
                task_id,
                slot_key,
                to_owner,
                class,
                ..
            } => {
                if active_lease_count_by_task
                    .get(task_id)
                    .copied()
                    .unwrap_or(0)
                    > 0
                {
                    violations.push(InvariantViolation {
                        code: "DOUBLE_ASSIGN".into(),
                        message: format!(
                            "task {task_id} offered while already having an active lease"
                        ),
                    });
                }
                if !offered_tasks_in_plan.insert(task_id.clone()) {
                    violations.push(InvariantViolation {
                        code: "DUPLICATE_OFFER".into(),
                        message: format!("task {task_id} offered more than once in the same tick"),
                    });
                }
                if !used_slot_keys.insert(slot_key.clone()) {
                    violations.push(InvariantViolation {
                        code: "DUPLICATE_SLOT".into(),
                        message: format!("slot {slot_key} assigned more than once"),
                    });
                }
                if state.pauses.contains(&PauseScope::All)
                    || state.pauses.contains(&PauseScope::Owner(to_owner.clone()))
                    || state.pauses.contains(&PauseScope::Class(class.clone()))
                {
                    violations.push(InvariantViolation {
                        code: "PAUSED_SCOPE".into(),
                        message: format!("offer for {task_id} violates active pause"),
                    });
                }
            }
            PlanAction::Lease {
                task_id,
                owner,
                machine,
                class,
                slot_key,
                pools,
                est_minutes_p90,
                est_cost,
                ..
            } => {
                // 1. One active lease per task.
                let cnt = active_lease_count_by_task
                    .entry(task_id.clone())
                    .or_insert(0);
                *cnt += 1;
                if *cnt > 1 {
                    violations.push(InvariantViolation {
                        code: "DOUBLE_LEASE".into(),
                        message: format!("task {task_id} has more than one active lease"),
                    });
                }
                if offered_tasks_in_plan.contains(task_id) {
                    violations.push(InvariantViolation {
                        code: "DOUBLE_ASSIGN".into(),
                        message: format!("task {task_id} both leased and offered in one tick"),
                    });
                }
                if !used_slot_keys.insert(slot_key.clone()) {
                    violations.push(InvariantViolation {
                        code: "DUPLICATE_SLOT".into(),
                        message: format!("slot {slot_key} leased more than once"),
                    });
                }

                // Task existence, required fields, deps closed, ready state.
                let Some(task) = state.tasks.get(task_id) else {
                    violations.push(InvariantViolation {
                        code: "UNKNOWN_TASK".into(),
                        message: format!("leased unknown task {task_id}"),
                    });
                    continue;
                };
                let effectively_ready = task.state == "ready"
                    || (task.state == "offered" && expired_offer_tasks.contains(task_id));
                if !effectively_ready {
                    violations.push(InvariantViolation {
                        code: "TASK_NOT_READY".into(),
                        message: format!(
                            "task {task_id} leased from non-ready state {}",
                            task.state
                        ),
                    });
                }
                if !has_required_fields(task) {
                    violations.push(InvariantViolation {
                        code: "MISSING_FIELDS".into(),
                        message: format!("task {task_id} is missing required fields"),
                    });
                }
                if !deps_closed(state, task) {
                    violations.push(InvariantViolation {
                        code: "UNCLOSED_DEPS".into(),
                        message: format!("task {task_id} has unclosed dependencies"),
                    });
                }

                // 2. No two active tasks with overlapping scope.
                for (other_id, other_scope) in &ctx.active_scopes {
                    if other_id != task_id && scopes_overlap(&task.scope, other_scope) {
                        violations.push(InvariantViolation {
                            code: "SCOPE_OVERLAP".into(),
                            message: format!(
                                "task {task_id} scope `{}` overlaps active task {other_id} scope `{other_scope}`",
                                task.scope
                            ),
                        });
                    }
                }

                // 3. Pause scopes & pool state (no lease on closed or paused pool).
                if state.pauses.contains(&PauseScope::All)
                    || state.pauses.contains(&PauseScope::Owner(owner.clone()))
                    || state.pauses.contains(&PauseScope::Class(class.clone()))
                {
                    violations.push(InvariantViolation {
                        code: "PAUSED_SCOPE".into(),
                        message: format!(
                            "lease for {task_id} on {owner}/{class} violates active pause"
                        ),
                    });
                }

                let class_spec = policy.classes.get(class);
                let mut check_pools = pools.clone();
                if let Some(fp) = class_spec.and_then(|c| c.family_pool.as_ref())
                    && !check_pools.contains(fp)
                {
                    check_pools.push(fp.clone());
                }
                let owner_meters = ctx.meters_for_owner(owner);
                let owner_leases = ctx.active_leases_for_owner(owner);
                let owner_committed = ctx.committed_for_owner(owner);

                for pool in &check_pools {
                    if state.pauses.contains(&PauseScope::Pool(pool.clone())) {
                        violations.push(InvariantViolation {
                            code: "PAUSED_POOL".into(),
                            message: format!("lease for {task_id} uses paused pool {pool}"),
                        });
                    }
                    if let Some(m) = owner_meters.get(pool) {
                        let leases = owner_leases.get(pool).map(Vec::as_slice).unwrap_or(&[]);
                        let f = forecast(m, now, leases);
                        let st = pool_state(m, &f, now, &policy.quota);
                        if matches!(st, PoolState::Paused | PoolState::Closed) {
                            violations.push(InvariantViolation {
                                code: "POOL_CLOSED_OR_PAUSED".into(),
                                message: format!(
                                    "lease for {task_id} uses pool {pool} in state {}",
                                    st.as_str()
                                ),
                            });
                        }
                    }
                }

                // 4. Caps & locks respected.
                let new_machine_cnt = ctx
                    .machine_class_active
                    .get(&(owner.clone(), machine.clone(), class.clone()))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                if let Some(cap_row) =
                    state
                        .capacities
                        .get(&(owner.clone(), machine.clone(), class.clone()))
                    && new_machine_cnt > cap_row.useful_parallel
                {
                    violations.push(InvariantViolation {
                        code: "CAP_PARALLEL".into(),
                        message: format!(
                            "lease for {task_id} exceeds useful_parallel {} on {owner}/{machine}/{class}",
                            cap_row.useful_parallel
                        ),
                    });
                }

                if let Some(cs) = class_spec {
                    if let Some(cap) = cs.cap_per_owner {
                        let cur = ctx
                            .owner_class_active
                            .get(&(owner.clone(), class.clone()))
                            .copied()
                            .unwrap_or(0);
                        if cur + 1 > cap {
                            violations.push(InvariantViolation {
                                code: "CAP_PER_OWNER".into(),
                                message: format!(
                                    "lease for {task_id} exceeds cap_per_owner {cap} for {owner}/{class}"
                                ),
                            });
                        }
                    }
                    if let Some(cap) = cs.cap_per_machine
                        && new_machine_cnt > cap
                    {
                        violations.push(InvariantViolation {
                            code: "CAP_PER_MACHINE".into(),
                            message: format!(
                                "lease for {task_id} exceeds cap_per_machine {cap} for {owner}/{machine}/{class}"
                            ),
                        });
                    }
                    if let Some(cap) = cs.cap_per_family
                        && let Some(fam) = cs.family_pool.as_ref()
                    {
                        let cur = ctx
                            .owner_family_active
                            .get(&(owner.clone(), fam.clone()))
                            .copied()
                            .unwrap_or(0);
                        if cur + 1 > cap {
                            violations.push(InvariantViolation {
                                code: "CAP_PER_FAMILY".into(),
                                message: format!(
                                    "lease for {task_id} exceeds cap_per_family {cap} for {owner}/{fam}"
                                ),
                            });
                        }
                    }
                    if let Some(lock_name) = cs.lock.as_deref().filter(|l| !l.is_empty())
                        && ctx.active_locks.contains(lock_name)
                    {
                        violations.push(InvariantViolation {
                            code: "CLASS_LOCK".into(),
                            message: format!(
                                "lease for {task_id} violates held class lock {lock_name}"
                            ),
                        });
                    }
                }

                // 5. Admission check when admission is enforced.
                if policy.switch.admission == PlanMode::Enforce {
                    let est = resolve_estimate(task, class, state, policy);
                    let task_in = TaskAdmitInput {
                        id: task.id.clone(),
                        kind: task.kind.clone(),
                        size: task.size,
                        checkpointable: task.checkpointable,
                        exclude_pools_until: task.exclude_pools_until.clone(),
                    };
                    let slot_in = SlotAdmitInput {
                        class: class.clone(),
                        pools: pools.clone(),
                        family_pool: class_spec.and_then(|c| c.family_pool.clone()),
                    };
                    if let AdmitDecision::Deny(reason) = admit(
                        &task_in,
                        &slot_in,
                        &owner_meters,
                        &est,
                        &policy.quota,
                        &owner_leases,
                        &owner_committed,
                        now,
                    ) {
                        violations.push(InvariantViolation {
                            code: "ADMISSION_DENIED".into(),
                            message: format!("lease for {task_id} failed admission: {reason}"),
                        });
                    }
                }

                // Update running context for subsequent leases in the same tick.
                ctx.active_scopes
                    .insert(task_id.clone(), task.scope.clone());
                *ctx.owner_active.entry(owner.clone()).or_insert(0) += 1;
                *ctx.owner_class_active
                    .entry((owner.clone(), class.clone()))
                    .or_insert(0) += 1;
                *ctx.machine_class_active
                    .entry((owner.clone(), machine.clone(), class.clone()))
                    .or_insert(0) += 1;
                if let Some(fam) = class_spec.and_then(|c| c.family_pool.as_ref()) {
                    *ctx.owner_family_active
                        .entry((owner.clone(), fam.clone()))
                        .or_insert(0) += 1;
                }
                if let Some(lock_name) = class_spec
                    .and_then(|c| c.lock.as_deref())
                    .filter(|l| !l.is_empty())
                {
                    ctx.active_locks.insert(lock_name.to_owned());
                }
                for pool in &check_pools {
                    let pct_per_unit = ctx
                        .lookup_meter(owner, pool)
                        .map_or(0.0, |m| m.pct_per_unit);
                    let cost_pct = (*est_cost * pct_per_unit).max(0.0);
                    ctx.active_leases_by_owner_pool
                        .entry((owner.clone(), pool.clone()))
                        .or_default()
                        .push(ActiveLease {
                            pools: check_pools.clone(),
                            remaining_cost_pct: cost_pct,
                            remaining_minutes: est_minutes_p90.max(1.0),
                        });
                    *ctx.committed_pct_by_owner_pool
                        .entry((owner.clone(), pool.clone()))
                        .or_insert(0.0) += cost_pct;
                }
            }
            _ => {}
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Applies the section 4.1 invariant guard: on any violation, discards the entire action list
/// and returns `[Alert(INVARIANT)]` and nothing else.
pub fn guard_actions(
    actions: Vec<PlanAction>,
    state: &PlanState,
    policy: &PlanPolicy,
    now: u64,
) -> (Vec<PlanAction>, Option<Vec<InvariantViolation>>) {
    match check_invariants(&actions, state, policy, now) {
        Ok(()) => (actions, None),
        Err(violations) => {
            let summary = violations
                .iter()
                .map(|v| format!("{}: {}", v.code, v.message))
                .collect::<Vec<_>>()
                .join(" | ");
            let alert = PlanAction::Alert {
                code: "INVARIANT".to_owned(),
                key: "scheduler".to_owned(),
                severity: "critical".to_owned(),
                to: "coordinator".to_owned(),
                text: format!("scheduler invariant violation: {summary}"),
                data: json!({
                    "violations": violations,
                    "discarded_actions": actions.len(),
                }),
            };
            (vec![alert], Some(violations))
        }
    }
}

/// Evaluates section 9 safe mode transitions in `enforce` mode.
///
/// Triggers safe mode (switching effective mode to `shadow` and emitting a `SAFE_MODE` alert) when:
/// 1. An invariant violation occurred, OR
/// 2. Ready tasks exist AND free admissible slots exist AND zero leases or offers were made for at
///    least `policy.timers.safe_mode_stall_min` minutes (default 10 minutes).
pub fn evaluate_safe_mode(
    prev: &SafeModeState,
    policy: &PlanPolicy,
    has_invariant_violation: bool,
    ready_task_count: usize,
    free_admissible_slots: usize,
    leases_or_offers_made: usize,
    now: u64,
) -> (SafeModeState, Option<PlanAction>) {
    let mut next = prev.clone();
    if policy.effective_scheduler_mode() != PlanMode::Enforce {
        return (next, None);
    }
    if next.active {
        return (next, None);
    }

    if has_invariant_violation {
        next.active = true;
        next.reason = Some("invariant violation".to_owned());
        next.entered_at = Some(now);
        next.stall_since = None;
        let alert = PlanAction::Alert {
            code: "SAFE_MODE".to_owned(),
            key: "scheduler".to_owned(),
            severity: "critical".to_owned(),
            to: "coordinator".to_owned(),
            text: "scheduler entered safe mode (shadow) after invariant violation".to_owned(),
            data: json!({"reason": "invariant violation", "at": now}),
        };
        return (next, Some(alert));
    }

    if leases_or_offers_made > 0 {
        next.stall_since = None;
        return (next, None);
    }

    if ready_task_count > 0 && free_admissible_slots > 0 {
        let since = next.stall_since.get_or_insert(now);
        let stall_limit = policy.timers.safe_mode_stall_min.saturating_mul(60);
        if now.saturating_sub(*since) >= stall_limit {
            next.active = true;
            next.reason = Some(format!(
                "no lease for {} s with {} ready tasks and {} free admissible slots",
                now.saturating_sub(*since),
                ready_task_count,
                free_admissible_slots
            ));
            next.entered_at = Some(now);
            let alert = PlanAction::Alert {
                code: "SAFE_MODE".to_owned(),
                key: "scheduler".to_owned(),
                severity: "critical".to_owned(),
                to: "coordinator".to_owned(),
                text: format!(
                    "scheduler entered safe mode (shadow): {}",
                    next.reason.as_deref().unwrap_or("stalled")
                ),
                data: json!({
                    "reason": "stall",
                    "ready_tasks": ready_task_count,
                    "free_admissible_slots": free_admissible_slots,
                    "stall_since": *since,
                    "at": now,
                }),
            };
            return (next, Some(alert));
        }
    } else {
        next.stall_since = None;
    }

    (next, None)
}
