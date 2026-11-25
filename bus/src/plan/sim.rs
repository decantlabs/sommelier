//! Log replay simulator and shadow comparison for policy verification (sections 4.1, 5.1, 9, 10.3).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::msg::{Kind, Message};
use crate::store::{Store, read_log_messages};
use crate::time::parse_ms;

use super::{DecisionRow, PlanAction, PlanPolicy, PlanState, load_plan_state_from_store, plan};

/// One mismatch between a shadow-planned placement and an actual placement observed on the bus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowMismatch {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_class: Option<String>,
    pub reason: String,
}

/// Comparison between planned actions in shadow mode and actual leases granted by the coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShadowComparison {
    pub planned_count: usize,
    pub actual_count: usize,
    pub matched: usize,
    pub agreement_ratio: f64,
    pub mismatches: Vec<ShadowMismatch>,
}

/// Compares shadow-planned `Lease`/`Offer` actions against actual `(task_id, owner, class)` tuples.
pub fn compare_shadow(
    planned_actions: &[PlanAction],
    actual_placements: &[(String, String, String)],
) -> ShadowComparison {
    let mut planned_map: BTreeMap<String, (String, String)> = BTreeMap::new();
    for act in planned_actions {
        match act {
            PlanAction::Lease {
                task_id,
                owner,
                class,
                ..
            } => {
                planned_map.insert(task_id.clone(), (owner.clone(), class.clone()));
            }
            PlanAction::Offer {
                task_id,
                to_owner,
                class,
                ..
            } => {
                planned_map.insert(task_id.clone(), (to_owner.clone(), class.clone()));
            }
            _ => {}
        }
    }

    let mut actual_map: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (task_id, owner, class) in actual_placements {
        actual_map.insert(task_id.clone(), (owner.clone(), class.clone()));
    }

    let all_tasks: BTreeSet<String> = planned_map
        .keys()
        .chain(actual_map.keys())
        .cloned()
        .collect();

    let mut matched = 0usize;
    let mut mismatches = Vec::new();
    for tid in all_tasks {
        match (planned_map.get(&tid), actual_map.get(&tid)) {
            (Some((po, pc)), Some((ao, ac))) if po == ao && pc == ac => {
                matched += 1;
            }
            (Some((po, pc)), Some((ao, ac))) => {
                mismatches.push(ShadowMismatch {
                    task_id: tid,
                    planned_owner: Some(po.clone()),
                    planned_class: Some(pc.clone()),
                    actual_owner: Some(ao.clone()),
                    actual_class: Some(ac.clone()),
                    reason: format!("placement differed: planned {po}/{pc}, actual {ao}/{ac}"),
                });
            }
            (Some((po, pc)), None) => {
                mismatches.push(ShadowMismatch {
                    task_id: tid,
                    planned_owner: Some(po.clone()),
                    planned_class: Some(pc.clone()),
                    actual_owner: None,
                    actual_class: None,
                    reason: format!("scheduler planned {po}/{pc}, coordinator did not place"),
                });
            }
            (None, Some((ao, ac))) => {
                mismatches.push(ShadowMismatch {
                    task_id: tid,
                    planned_owner: None,
                    planned_class: None,
                    actual_owner: Some(ao.clone()),
                    actual_class: Some(ac.clone()),
                    reason: format!("coordinator placed on {ao}/{ac}, scheduler did not place"),
                });
            }
            (None, None) => {}
        }
    }

    let denom = planned_map.len().max(actual_map.len());
    let agreement_ratio = if denom == 0 {
        1.0
    } else {
        matched as f64 / denom as f64
    };

    ShadowComparison {
        planned_count: planned_map.len(),
        actual_count: actual_map.len(),
        matched,
        agreement_ratio: (agreement_ratio * 10000.0).round() / 10000.0,
        mismatches,
    }
}

/// Comparison of one recorded `DECISION` tick against its simulated replay under a policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimDecisionDiff {
    pub tick_id: u64,
    pub at: u64,
    pub exact_match: bool,
    pub recorded_actions: Vec<PlanAction>,
    pub replayed_actions: Vec<PlanAction>,
    pub leases_changed: usize,
}

/// Output report of `sommelier sim LOG --policy FILE` (section 5.1, 9, 10.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimReport {
    pub policy_hash: String,
    pub messages_replayed: usize,
    pub recorded_decisions: usize,
    pub exact_decision_matches: usize,
    pub ticks_simulated: usize,
    pub total_leases_changed: usize,
    pub diffs: Vec<SimDecisionDiff>,
    pub decisions: Vec<DecisionRow>,
    pub shadow_comparison: ShadowComparison,
}

fn parse_recorded_decision(msg: &Message) -> Option<DecisionRow> {
    if msg.kind != Kind::Fact || !msg.topic.starts_with("decision/") {
        return None;
    }
    let v: Value = serde_json::from_str(&msg.body).ok()?;
    let tick_id = v["tick_id"].as_u64().or_else(|| {
        msg.topic
            .strip_prefix("decision/")
            .and_then(|s| s.parse::<u64>().ok())
    })?;
    let at = v["at"]
        .as_u64()
        .unwrap_or_else(|| parse_ms(&msg.ts).unwrap_or(0) / 1000);
    let policy_hash = v["policy_hash"].as_str().unwrap_or("").to_owned();
    let state_hash = v["state_hash"].as_str().unwrap_or("").to_owned();
    let mode = v["mode"].as_str().unwrap_or("shadow").to_owned();
    let actions: Vec<PlanAction> = v
        .get("actions")
        .and_then(|a| serde_json::from_value(a.clone()).ok())
        .unwrap_or_default();
    let applied = v["applied"].as_bool().unwrap_or(false);
    Some(DecisionRow {
        tick_id,
        at,
        policy_hash,
        state_hash,
        mode,
        actions,
        applied,
        msg_id: msg.id,
    })
}

fn extract_actual_lease(msg: &Message) -> Option<(String, String, String)> {
    if msg.kind != Kind::TaskUpdate {
        return None;
    }
    let v: Value = serde_json::from_str(&msg.body).ok()?;
    let state = v["state"].as_str().unwrap_or("");
    if state != "leased" {
        return None;
    }
    let task_id = v["task"]
        .as_str()
        .or_else(|| msg.topic.strip_prefix("task/"))?
        .to_owned();
    let lease = v.get("lease")?;
    let owner = lease["owner"].as_str().unwrap_or("").to_owned();
    let class = lease["class"].as_str().unwrap_or("").to_owned();
    if owner.is_empty() && class.is_empty() {
        return None;
    }
    Some((task_id, owner, class))
}

fn count_lease_changes(a: &[PlanAction], b: &[PlanAction]) -> usize {
    let to_set = |acts: &[PlanAction]| -> BTreeSet<(String, String, String)> {
        let mut s = BTreeSet::new();
        for act in acts {
            match act {
                PlanAction::Lease {
                    task_id,
                    owner,
                    class,
                    ..
                } => {
                    s.insert((task_id.clone(), owner.clone(), class.clone()));
                }
                PlanAction::Offer {
                    task_id,
                    to_owner,
                    class,
                    ..
                } => {
                    s.insert((task_id.clone(), to_owner.clone(), class.clone()));
                }
                _ => {}
            }
        }
        s
    };
    let sa = to_set(a);
    let sb = to_set(b);
    sa.symmetric_difference(&sb).count()
}

/// Replays `messages` into a fresh in-memory `Store` under `policy`, reproducing recorded
/// `DECISION` rows at each tick timestamp and computing shadow comparisons.
pub fn simulate_log(
    messages: &[Message],
    policy: &PlanPolicy,
    extra_tick_times: Option<&[u64]>,
) -> Result<SimReport, String> {
    let mut store = Store::open_in_memory()?;
    let policy_hash = policy.policy_hash();
    let hub_started_at = messages
        .first()
        .and_then(|m| parse_ms(&m.ts))
        .map_or(0, |ms| ms / 1000);

    let mut recorded_decisions = 0usize;
    let mut exact_decision_matches = 0usize;
    let mut total_leases_changed = 0usize;
    let mut diffs = Vec::new();
    let mut decisions = Vec::new();
    let mut all_planned_actions = Vec::new();
    let mut actual_leases = Vec::new();

    for msg in messages {
        if let Some(rec) = parse_recorded_decision(msg) {
            recorded_decisions += 1;
            let state: PlanState =
                load_plan_state_from_store(store.connection(), policy, hub_started_at)?;
            let state_hash = store.state_hash()?;
            let out = plan(&state, policy, rec.at);
            let rec_json = serde_json::to_string(&rec.actions).map_err(|e| e.to_string())?;
            let rep_json = serde_json::to_string(&out.actions).map_err(|e| e.to_string())?;
            let exact_match = rec_json == rep_json;
            if exact_match {
                exact_decision_matches += 1;
            }
            let leases_changed = count_lease_changes(&rec.actions, &out.actions);
            total_leases_changed += leases_changed;
            all_planned_actions.extend(out.actions.clone());
            diffs.push(SimDecisionDiff {
                tick_id: rec.tick_id,
                at: rec.at,
                exact_match,
                recorded_actions: rec.actions,
                replayed_actions: out.actions.clone(),
                leases_changed,
            });
            decisions.push(DecisionRow {
                tick_id: rec.tick_id,
                at: rec.at,
                policy_hash: policy_hash.clone(),
                state_hash,
                mode: policy.effective_scheduler_mode().as_str().to_owned(),
                actions: out.actions,
                applied: false,
                msg_id: msg.id,
            });
        }

        if let Some(actual) = extract_actual_lease(msg) {
            actual_leases.push(actual);
        }

        store.apply(msg)?;
    }

    if let Some(ticks) = extra_tick_times {
        let start_tick = decisions.len() as u64 + 1;
        for (offset, &at) in ticks.iter().enumerate() {
            let next_tick = start_tick + offset as u64;
            let state = load_plan_state_from_store(store.connection(), policy, hub_started_at)?;
            let state_hash = store.state_hash()?;
            let out = plan(&state, policy, at);
            all_planned_actions.extend(out.actions.clone());
            decisions.push(DecisionRow {
                tick_id: next_tick,
                at,
                policy_hash: policy_hash.clone(),
                state_hash,
                mode: policy.effective_scheduler_mode().as_str().to_owned(),
                actions: out.actions,
                applied: false,
                msg_id: 0,
            });
        }
    } else if decisions.is_empty() {
        let last_at = messages
            .last()
            .and_then(|m| parse_ms(&m.ts))
            .map_or(0, |ms| ms / 1000);
        let state = load_plan_state_from_store(store.connection(), policy, hub_started_at)?;
        let state_hash = store.state_hash()?;
        let out = plan(&state, policy, last_at);
        all_planned_actions.extend(out.actions.clone());
        decisions.push(DecisionRow {
            tick_id: 1,
            at: last_at,
            policy_hash: policy_hash.clone(),
            state_hash,
            mode: policy.effective_scheduler_mode().as_str().to_owned(),
            actions: out.actions,
            applied: false,
            msg_id: 0,
        });
    }

    let shadow_comparison = compare_shadow(&all_planned_actions, &actual_leases);

    Ok(SimReport {
        policy_hash,
        messages_replayed: messages.len(),
        recorded_decisions,
        exact_decision_matches,
        ticks_simulated: decisions.len(),
        total_leases_changed,
        diffs,
        decisions,
        shadow_comparison,
    })
}

/// Runs `simulate_log` on a log file and an optional TOML/JSON policy file (`sommelier sim LOG --policy FILE`).
pub fn simulate_log_file(log_path: &Path, policy_path: Option<&Path>) -> Result<SimReport, String> {
    let messages = read_log_messages(log_path)?;
    let policy = match policy_path {
        Some(p) => {
            let text = fs::read_to_string(p)
                .map_err(|e| format!("cannot read policy {}: {e}", p.display()))?;
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str::<PlanPolicy>(&text)
                    .map_err(|e| format!("invalid JSON policy {}: {e}", p.display()))?
            } else {
                PlanPolicy::parse_toml(&text)?
            }
        }
        None => PlanPolicy::default(),
    };
    simulate_log(&messages, &policy, None)
}
