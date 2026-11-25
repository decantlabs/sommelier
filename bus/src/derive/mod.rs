//! Derived worker and slot states, capacity tracking, and IDLE alerts (step 1).

pub mod alert;
pub mod capacity;
pub mod state;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::claims::Claims;
use crate::hub::Hub;
use crate::msg::{Kind, Message};
use crate::roster::{Agent, Roster};
use crate::time::parse_ms;

pub use alert::{AlertRow, IDLE_REPEAT_MS, IdleAlertTracker, SlotStatus};
pub use capacity::{CapacityMode, HelloCapacity, parse_hello_capacity};
pub use state::{DeriveTimers, DerivedState, DerivedWorker, WorkerFacts, derive_worker};

/// Summary of a capacity slot or worker row in the `derived` section of `/api/state` and `sommelier idle`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedSlot {
    pub key: String,
    pub owner: String,
    pub class: String,
    pub slot: u32,
    pub state: DerivedState,
    pub mode: CapacityMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    pub age_secs: u64,
    pub idle_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Complete derived snapshot returned in `/api/state` under `"derived"` and used by `sommelier idle`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedSnapshot {
    pub as_of: u64,
    pub workers: Vec<DerivedWorker>,
    pub slots: Vec<DerivedSlot>,
    pub idle: Vec<DerivedSlot>,
    pub alerts: Vec<AlertRow>,
    pub counts: BTreeMap<String, usize>,
}

/// Mutable state owned by the `derive` module inside the hub.
#[derive(Clone, Debug, Default)]
pub struct DeriveStore {
    pub capacities: BTreeMap<String, HelloCapacity>,
    pub tracker: IdleAlertTracker,
    /// Open unfulfilled TASK / RUN_REQUEST topics or bodies (for starved detection).
    pub ready_tasks: BTreeMap<u64, ReadyTaskHint>,
    pub timers: DeriveTimers,
}

#[derive(Clone, Debug)]
pub struct ReadyTaskHint {
    pub id: u64,
    pub kind: String,
    pub topic: String,
    pub answered: bool,
}

impl DeriveStore {
    pub fn apply(&mut self, msg: &Message, now_ms: u64) {
        let msg_ms = parse_ms(&msg.ts).unwrap_or(now_ms);
        match msg.kind {
            Kind::Hello => {
                let cap = parse_hello_capacity(&msg.body, &msg.refs, msg_ms);
                self.capacities.insert(msg.from.clone(), cap);
            }
            Kind::Fact if msg.topic.starts_with("capacity/") => {
                let cap = parse_hello_capacity(&msg.body, &msg.refs, msg_ms);
                self.capacities.insert(msg.from.clone(), cap);
            }
            Kind::Alert => {
                self.tracker.apply(msg, msg_ms);
            }
            Kind::Task | Kind::RunRequest => {
                let kind = if msg.kind == Kind::RunRequest {
                    "run"
                } else {
                    "fix"
                };
                self.ready_tasks.insert(
                    msg.id,
                    ReadyTaskHint {
                        id: msg.id,
                        kind: kind.into(),
                        topic: msg.topic.clone(),
                        answered: false,
                    },
                );
            }
            Kind::Accept | Kind::Ready | Kind::RunResult | Kind::Landed | Kind::Release => {
                for r in &msg.refs {
                    if let Some(id_s) = r.strip_prefix('#')
                        && let Ok(id) = id_s.parse::<u64>()
                        && let Some(t) = self.ready_tasks.get_mut(&id)
                    {
                        t.answered = true;
                    }
                }
                if !msg.topic.is_empty() {
                    for t in self.ready_tasks.values_mut() {
                        if t.topic == msg.topic {
                            t.answered = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Computes the derived snapshot from the current hub state.
    pub fn compute(
        &self,
        roster: &Roster,
        claims: &Claims,
        online_members: &HashMap<String, bool>,
        last_seen_members: &HashMap<String, u64>,
        now_ms: u64,
    ) -> DerivedSnapshot {
        let as_of = now_ms / 1000;
        let agents = roster.snapshot(now_ms, None);
        let open_tasks: Vec<&ReadyTaskHint> =
            self.ready_tasks.values().filter(|t| !t.answered).collect();

        let is_member_present = |owner: &str| -> bool {
            if online_members.get(owner).copied().unwrap_or(false) {
                return true;
            }
            if let Some(&seen) = last_seen_members.get(owner)
                && now_ms.saturating_sub(seen) < 30 * 60 * 1000
            {
                return true;
            }
            agents.iter().any(|a| {
                a.owner == owner && {
                    let chg = parse_ms(&a.changed).unwrap_or(0);
                    chg > 0 && now_ms.saturating_sub(chg) < 30 * 60 * 1000
                }
            })
        };

        let mut workers = Vec::new();
        for a in &agents {
            let cap = self.capacities.get(&a.owner);
            let mode = cap.map(|c| c.mode).unwrap_or(CapacityMode::Auto);
            let class = infer_agent_class(a, cap);
            let present = is_member_present(&a.owner);
            let changed_ms = parse_ms(&a.changed).unwrap_or(a.ts_ms);
            let starved_reason = if a.state == "idle" {
                check_starved_reason(cap, claims, &open_tasks)
            } else {
                None
            };
            let facts = WorkerFacts {
                owner: a.owner.clone(),
                agent: a.agent.clone(),
                class,
                mode,
                owner_online: present,
                reported_state: a.state.clone(),
                supervised: is_supervised_row(a, cap),
                last_seen_ms: a.ts_ms,
                state_since_ms: changed_ms,
                starved_reason,
                model: a.model.clone(),
                branch: a.branch.clone(),
                text: a.text.clone(),
            };
            workers.push(derive_worker(&facts, &self.timers, now_ms));
        }

        let mut slots = Vec::new();
        // Build slots from declared capacities in HELLO
        for (owner, cap) in &self.capacities {
            let present = is_member_present(owner);
            let owner_workers: Vec<&DerivedWorker> = workers
                .iter()
                .filter(|w| &w.owner == owner && w.agent != "main" && w.agent != *owner)
                .collect();

            for (class, &count) in &cap.classes {
                let matching: Vec<&&DerivedWorker> = owner_workers
                    .iter()
                    .filter(|w| {
                        &w.class == class
                            || (cap.classes.len() == 1
                                && (w.class.is_empty() || w.class == "default"))
                    })
                    .collect();
                for idx in 0..count {
                    let slot_num = idx + 1;
                    let key = format!("{owner}/{class}/{slot_num}");
                    if let Some(w) = matching.get(idx as usize) {
                        slots.push(DerivedSlot {
                            key,
                            owner: owner.clone(),
                            class: class.clone(),
                            slot: slot_num,
                            state: w.state,
                            mode: cap.mode,
                            worker: Some(w.agent.clone()),
                            age_secs: w.age_secs,
                            idle_secs: w.idle_secs,
                            reason: w.reason.clone(),
                        });
                    } else {
                        let age_secs = now_ms.saturating_sub(cap.declared_at_ms) / 1000;
                        let starved = check_starved_reason(Some(cap), claims, &open_tasks);
                        let (state, idle_secs, reason) = if !present {
                            (DerivedState::Offline, 0, Some("owner offline".into()))
                        } else if cap.mode == CapacityMode::Own {
                            (DerivedState::Own, 0, None)
                        } else if let Some(why) = starved {
                            (DerivedState::Starved, age_secs, Some(why))
                        } else {
                            (DerivedState::Idle, age_secs, None)
                        };
                        slots.push(DerivedSlot {
                            key,
                            owner: owner.clone(),
                            class: class.clone(),
                            slot: slot_num,
                            state,
                            mode: cap.mode,
                            worker: None,
                            age_secs,
                            idle_secs,
                            reason,
                        });
                    }
                }
            }
        }

        // Also include explicit idle/starved worker rows for owners who did not declare class capacity
        for (i, w) in workers.iter().enumerate() {
            if !self
                .capacities
                .get(&w.owner)
                .is_some_and(|c| !c.classes.is_empty())
            {
                let class = if w.class.is_empty() {
                    "default".to_owned()
                } else {
                    w.class.clone()
                };
                slots.push(DerivedSlot {
                    key: format!("{}/{}", w.owner, w.agent),
                    owner: w.owner.clone(),
                    class,
                    slot: (i as u32) + 1,
                    state: w.state,
                    mode: w.mode,
                    worker: Some(w.agent.clone()),
                    age_secs: w.age_secs,
                    idle_secs: w.idle_secs,
                    reason: w.reason.clone(),
                });
            }
        }

        let idle: Vec<DerivedSlot> = slots
            .iter()
            .filter(|s| matches!(s.state, DerivedState::Idle | DerivedState::Starved))
            .cloned()
            .collect();

        let mut counts = BTreeMap::new();
        for w in &workers {
            *counts.entry(w.state.as_str().to_owned()).or_insert(0) += 1;
        }
        for s in &slots {
            if s.worker.is_none() {
                *counts.entry(s.state.as_str().to_owned()).or_insert(0) += 1;
            }
        }

        let alerts: Vec<AlertRow> = self.tracker.alerts.values().cloned().collect();

        DerivedSnapshot {
            as_of,
            workers,
            slots,
            idle,
            alerts,
            counts,
        }
    }

    pub fn tick_alerts(
        &mut self,
        snap: &DerivedSnapshot,
        coordinator: Option<&str>,
        now_ms: u64,
    ) -> Vec<Message> {
        let slot_statuses: Vec<SlotStatus> = snap
            .slots
            .iter()
            .filter(|s| s.mode != CapacityMode::Own && s.state != DerivedState::Offline)
            .map(|s| SlotStatus {
                key: s.key.clone(),
                owner: s.owner.clone(),
                class: s.class.clone(),
                is_idle: matches!(s.state, DerivedState::Idle | DerivedState::Starved),
                idle_secs: s.idle_secs,
                reason: s.reason.clone(),
            })
            .collect();
        self.tracker.evaluate(
            &slot_statuses,
            self.timers.idle_alert_sec,
            coordinator,
            now_ms,
        )
    }
}

fn infer_agent_class(a: &Agent, cap: Option<&HelloCapacity>) -> String {
    let m = a.model.to_ascii_lowercase();
    let b = a.branch.to_ascii_lowercase();
    if m.contains("codex") || b.starts_with("codex/") {
        return "codex".into();
    }
    if m.contains("gemini") || b.starts_with("gemini/") {
        return "gemini".into();
    }
    if m.contains("claude") || m.contains("sonnet") || m.contains("opus") || m.contains("fable") {
        return "claude".into();
    }
    if let Some(c) = cap
        && let Some(first) = c.classes.keys().next()
    {
        return first.clone();
    }
    "default".into()
}

fn is_supervised_row(a: &Agent, cap: Option<&HelloCapacity>) -> bool {
    if a.agent == "main" || a.agent == a.owner {
        return false;
    }
    if cap.is_some_and(|c| c.mode == CapacityMode::Own) {
        return false;
    }
    true
}

fn check_starved_reason(
    cap: Option<&HelloCapacity>,
    claims: &Claims,
    open_tasks: &[&ReadyTaskHint],
) -> Option<String> {
    let first = open_tasks.first()?;
    if let Some(c) = cap
        && !c.accepts.is_empty()
        && !c.accepts.iter().any(|k| k == &first.kind)
    {
        return Some(format!(
            "accepts {:?}, task needs {}",
            c.accepts, first.kind
        ));
    }
    if !first.topic.is_empty()
        && claims
            .iter()
            .any(|(t, _)| t.starts_with(&first.topic) || first.topic.starts_with(t.as_str()))
    {
        return Some(format!("scope {} is claimed", first.topic));
    }
    None
}

pub fn on_message(hub: &Hub, msg: &Message, now_ms: u64) {
    hub.derive_on_message(msg, now_ms);
}

pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    hub.derive_on_tick(now_ms)
}

pub fn api(hub: &Hub, now_ms: u64, state: &mut Map<String, Value>) {
    let snap = hub.derived_snapshot(now_ms);
    state.insert("as_of".into(), json!(snap.as_of));
    state.insert(
        "derived".into(),
        serde_json::to_value(&snap).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_facts() -> WorkerFacts {
        WorkerFacts {
            owner: "alice".into(),
            agent: "w1".into(),
            class: "codex".into(),
            mode: CapacityMode::Auto,
            owner_online: true,
            reported_state: "running".into(),
            supervised: true,
            last_seen_ms: 1_000_000,
            state_since_ms: 1_000_000,
            starved_reason: None,
            model: "codex".into(),
            branch: "codex/t1".into(),
            text: "working".into(),
        }
    }

    #[test]
    fn derive_table_12_cases() {
        let timers = DeriveTimers {
            suspect_sec: 120,
            stale_sec: 1200,
            idle_alert_sec: 600,
        };
        let t0 = 1_000_000;

        // 1. running (fresh supervised worker)
        let f1 = base_facts();
        let d1 = derive_worker(&f1, &timers, t0 + 30_000);
        assert_eq!(d1.state, DerivedState::Running);
        assert_eq!(d1.age_secs, 30);

        // 2. running (unsupervised worker at 180s, past suspect_sec but under stale_sec)
        let mut f2 = base_facts();
        f2.supervised = false;
        let d2 = derive_worker(&f2, &timers, t0 + 180_000);
        assert_eq!(d2.state, DerivedState::Running);
        assert_eq!(d2.age_secs, 180);

        // 3. idle (fresh idle worker, no waiting tasks)
        let mut f3 = base_facts();
        f3.reported_state = "idle".into();
        let d3 = derive_worker(&f3, &timers, t0 + 45_000);
        assert_eq!(d3.state, DerivedState::Idle);
        assert_eq!(d3.idle_secs, 45);

        // 4. idle (long idle worker with refreshed heartbeat, keeps state_since_ms)
        let mut f4 = base_facts();
        f4.reported_state = "idle".into();
        f4.state_since_ms = t0;
        f4.last_seen_ms = t0 + 600_000;
        let d4 = derive_worker(&f4, &timers, t0 + 650_000);
        assert_eq!(d4.state, DerivedState::Idle);
        assert_eq!(d4.age_secs, 50);
        assert_eq!(d4.idle_secs, 650);

        // 5. starved (idle worker, ready task blocked by scope overlap)
        let mut f5 = base_facts();
        f5.reported_state = "idle".into();
        f5.starved_reason = Some("scope issue/10 is claimed".into());
        let d5 = derive_worker(&f5, &timers, t0 + 90_000);
        assert_eq!(d5.state, DerivedState::Starved);
        assert_eq!(d5.reason.as_deref(), Some("scope issue/10 is claimed"));

        // 6. starved (idle worker, ready task excluded by class accepts)
        let mut f6 = base_facts();
        f6.reported_state = "idle".into();
        f6.starved_reason = Some("no headroom on codex.5h".into());
        let d6 = derive_worker(&f6, &timers, t0 + 300_000);
        assert_eq!(d6.state, DerivedState::Starved);
        assert_eq!(d6.idle_secs, 300);

        // 7. suspect (supervised running worker silent for > 120s)
        let f7 = base_facts();
        let d7 = derive_worker(&f7, &timers, t0 + 125_000);
        assert_eq!(d7.state, DerivedState::Suspect);
        assert_eq!(d7.age_secs, 125);

        // 8. suspect (supervised running worker silent for 1199s, just under stale)
        let f8 = base_facts();
        let d8 = derive_worker(&f8, &timers, t0 + 1_199_000);
        assert_eq!(d8.state, DerivedState::Suspect);

        // 9. stale (running worker silent for >= 1200s)
        let f9 = base_facts();
        let d9 = derive_worker(&f9, &timers, t0 + 1_200_000);
        assert_eq!(d9.state, DerivedState::Stale);
        assert_eq!(d9.age_secs, 1200);

        // 10. stale (idle worker silent for >= 1200s)
        let mut f10 = base_facts();
        f10.reported_state = "idle".into();
        let d10 = derive_worker(&f10, &timers, t0 + 1_500_000);
        assert_eq!(d10.state, DerivedState::Stale);

        // 11. own (guest in own mode self-reporting)
        let mut f11 = base_facts();
        f11.mode = CapacityMode::Own;
        f11.agent = "main".into();
        let d11 = derive_worker(&f11, &timers, t0 + 20_000);
        assert_eq!(d11.state, DerivedState::Own);

        // 12. offline (owner disconnected and no recent sign of life)
        let mut f12 = base_facts();
        f12.owner_online = false;
        let d12 = derive_worker(&f12, &timers, t0 + 10_000);
        assert_eq!(d12.state, DerivedState::Offline);
        assert_eq!(d12.reason.as_deref(), Some("owner offline"));
    }

    #[test]
    fn hello_capacity_parses_all_fields() {
        let cap = parse_hello_capacity(
            "os=windows gpu=rtx4090 class=codex:4,gemini:2 mode=own gpu_slots=1 accepts=fix,review,run",
            &["class:claude:1".into()],
            5000,
        );
        assert_eq!(cap.os, "windows");
        assert_eq!(cap.gpu, "rtx4090");
        assert_eq!(cap.mode, CapacityMode::Own);
        assert_eq!(cap.gpu_slots, 1);
        assert_eq!(cap.classes.get("codex"), Some(&4));
        assert_eq!(cap.classes.get("gemini"), Some(&2));
        assert_eq!(cap.classes.get("claude"), Some(&1));
        assert_eq!(cap.accepts, vec!["fix", "review", "run"]);
    }
}
