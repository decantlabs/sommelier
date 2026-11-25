//! Deterministic role election rules (sections 5.3, 13.1, 13.2, 13.3, and step 7).
//!
//! Step 7 implements the first election rule of section 13.3:
//! - Policy candidate order (`tier`, then policy declaration order / name)
//! - Heartbeat and `dead_sec` expiry: a silent holder loses the lease after `dead_sec`,
//!   and the next eligible candidate is granted `epoch + 1`
//! - Planned release / handover / owner veto
//! - Cooldown and flapping guard (`max_changes_per_hour`)
//!
//! (Quota runway checks come in step 11).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::lease::{FenceRefusal, LeaseScope, RoleCommand, RoleRow, RoleState, check_fence};
use crate::msg::{Kind, Message, Role};

/// Candidate entry from `[role.<name>].candidates` in `swarm.policy.toml` (section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCandidate {
    /// Seat identifier, e.g. `"owner/coord-claude"` or `"alice/lead"`.
    pub who: String,
    #[serde(default = "default_tier")]
    pub tier: u32,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub scope: LeaseScope,
    #[serde(default)]
    pub spawn: bool,
    #[serde(default)]
    pub opt_in: bool,
}

fn default_tier() -> u32 {
    1
}

impl RoleCandidate {
    pub fn new(who: impl Into<String>, tier: u32, scope: LeaseScope) -> Self {
        Self {
            who: who.into(),
            tier,
            class: String::new(),
            scope,
            spawn: true,
            opt_in: scope == LeaseScope::Limited,
        }
    }

    pub fn owner(&self) -> &str {
        self.who
            .split_once('/')
            .map_or(self.who.as_str(), |(o, _)| o)
    }
}

/// Per-role policy configuration from `[role.<name>]` (section 5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePolicy {
    #[serde(default = "default_heartbeat_sec")]
    pub heartbeat_sec: u64,
    #[serde(default = "default_dead_sec")]
    pub dead_sec: u64,
    #[serde(default = "default_min_hold_min")]
    pub min_hold_min: u64,
    #[serde(default = "default_cooldown_min")]
    pub cooldown_min: u64,
    #[serde(default = "default_max_changes_per_hour")]
    pub max_changes_per_hour: u32,
    #[serde(default)]
    pub require: Vec<String>,
    #[serde(default)]
    pub candidates: Vec<RoleCandidate>,
}

fn default_heartbeat_sec() -> u64 {
    60
}
fn default_dead_sec() -> u64 {
    180
}
fn default_min_hold_min() -> u64 {
    15
}
fn default_cooldown_min() -> u64 {
    30
}
fn default_max_changes_per_hour() -> u32 {
    3
}

impl Default for RolePolicy {
    fn default() -> Self {
        Self {
            heartbeat_sec: default_heartbeat_sec(),
            dead_sec: default_dead_sec(),
            min_hold_min: default_min_hold_min(),
            cooldown_min: default_cooldown_min(),
            max_changes_per_hour: default_max_changes_per_hour(),
            require: Vec::new(),
            candidates: Vec::new(),
        }
    }
}

/// Outcome of a single role election or transition step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleAction {
    Renewed {
        role: String,
        holder: String,
        epoch: u64,
    },
    Granted {
        role: String,
        holder: String,
        lease_scope: LeaseScope,
        epoch: u64,
        reason: String,
    },
    Vacant {
        role: String,
        epoch: u64,
        reason: String,
    },
    Frozen {
        role: String,
        epoch: u64,
        changes_last_hour: u32,
    },
}

/// Store managing all role leases, candidate policies, heartbeats, cooldowns, and vetoes.
#[derive(Clone, Debug, Default)]
pub struct RoleTable {
    /// `(role, scope) -> RoleRow`
    pub rows: BTreeMap<(String, String), RoleRow>,
    /// `role -> RolePolicy`
    pub policies: BTreeMap<String, RolePolicy>,
    /// Vetoed candidates or member names (`role veto WHO`).
    pub vetoed: BTreeSet<String>,
    /// Candidate heartbeats / reachability (`who -> last_heartbeat_sec`).
    /// When empty for a candidate with `spawn == true`, the candidate is considered spawnable.
    pub candidate_heartbeats: BTreeMap<String, u64>,
    /// Explicit set of candidates known to be offline/unreachable.
    pub unreachable: BTreeSet<String>,
    /// `(role, candidate_who) -> lost_at_sec` for cooldown tracking.
    pub lost_at: BTreeMap<(String, String), u64>,
    /// `(role, scope) -> timestamps of grants within the last hour`.
    pub grant_history: BTreeMap<(String, String), Vec<u64>>,
}

#[derive(Serialize, Deserialize)]
struct RoleTableFile {
    rows: Vec<RoleRow>,
    policies: BTreeMap<String, RolePolicy>,
    vetoed: BTreeSet<String>,
    candidate_heartbeats: BTreeMap<String, u64>,
    unreachable: BTreeSet<String>,
    lost_at: Vec<(String, String, u64)>,
    grant_history: Vec<(String, String, Vec<u64>)>,
}

impl Serialize for RoleTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let file = RoleTableFile {
            rows: self.rows.values().cloned().collect(),
            policies: self.policies.clone(),
            vetoed: self.vetoed.clone(),
            candidate_heartbeats: self.candidate_heartbeats.clone(),
            unreachable: self.unreachable.clone(),
            lost_at: self
                .lost_at
                .iter()
                .map(|((r, w), &ts)| (r.clone(), w.clone(), ts))
                .collect(),
            grant_history: self
                .grant_history
                .iter()
                .map(|((r, s), v)| (r.clone(), s.clone(), v.clone()))
                .collect(),
        };
        file.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RoleTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let file = RoleTableFile::deserialize(deserializer)?;
        let rows = file
            .rows
            .into_iter()
            .map(|r| ((r.role.clone(), r.scope.clone()), r))
            .collect();
        let lost_at = file
            .lost_at
            .into_iter()
            .map(|(r, w, ts)| ((r, w), ts))
            .collect();
        let grant_history = file
            .grant_history
            .into_iter()
            .map(|(r, s, v)| ((r, s), v))
            .collect();
        Ok(RoleTable {
            rows,
            policies: file.policies,
            vetoed: file.vetoed,
            candidate_heartbeats: file.candidate_heartbeats,
            unreachable: file.unreachable,
            lost_at,
            grant_history,
        })
    }
}

impl RoleTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_policy(&mut self, role: impl Into<String>, policy: RolePolicy) {
        let role = role.into();
        self.rows
            .entry((role.clone(), "global".into()))
            .or_insert_with(|| RoleRow::vacant(&role, "global"));
        self.policies.insert(role, policy);
    }

    pub fn get(&self, role: &str) -> Option<&RoleRow> {
        self.get_scoped(role, "global")
    }

    pub fn get_scoped(&self, role: &str, scope: &str) -> Option<&RoleRow> {
        self.rows.get(&(role.to_owned(), scope.to_owned()))
    }

    pub fn list(&self) -> Vec<RoleRow> {
        self.rows.values().cloned().collect()
    }

    /// Records a heartbeat from the current holder (or a candidate) at `now_sec`.
    pub fn heartbeat(&mut self, role: &str, who: &str, now_sec: u64) -> bool {
        self.candidate_heartbeats.insert(who.to_owned(), now_sec);
        self.unreachable.remove(who);
        if let Some(row) = self.rows.get_mut(&(role.to_owned(), "global".into()))
            && row.holder.as_deref() == Some(who)
            && row.state != RoleState::Vacant
        {
            row.heartbeat_at = now_sec;
            return true;
        }
        false
    }

    /// Checks whether a candidate is eligible under the step 7 rule (heartbeat/reachability,
    /// not vetoed, cooldown respected, policy order).
    pub fn is_candidate_eligible(
        &self,
        role: &str,
        cand: &RoleCandidate,
        policy: &RolePolicy,
        now_sec: u64,
    ) -> bool {
        if self.vetoed.contains(&cand.who) || self.vetoed.contains(cand.owner()) {
            return false;
        }
        if self.unreachable.contains(&cand.who) || self.unreachable.contains(cand.owner()) {
            return false;
        }
        if cand.scope == LeaseScope::Limited && !cand.opt_in {
            return false;
        }
        if let Some(&lost_ts) = self.lost_at.get(&(role.to_owned(), cand.who.clone())) {
            let cooldown_sec = policy.cooldown_min.saturating_mul(60);
            if now_sec < lost_ts.saturating_add(cooldown_sec) {
                return false;
            }
        }
        if let Some(&hb) = self.candidate_heartbeats.get(&cand.who)
            && now_sec.saturating_sub(hb) > policy.dead_sec
        {
            return false;
        }
        true
    }

    /// Returns eligible candidates for `role` ordered by `(tier asc, policy order)`.
    pub fn eligible_candidates(&self, role: &str, now_sec: u64) -> Vec<RoleCandidate> {
        let Some(policy) = self.policies.get(role) else {
            return Vec::new();
        };
        let mut indexed: Vec<(usize, &RoleCandidate)> = policy
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| self.is_candidate_eligible(role, c, policy, now_sec))
            .collect();
        indexed.sort_by_key(|(idx, c)| (c.tier, *idx));
        indexed.into_iter().map(|(_, c)| c.clone()).collect()
    }

    /// Runs the deterministic election tick of section 13.3 for `role` at `now_sec`.
    ///
    /// Rules:
    /// - If the current holder is alive (`now_sec - heartbeat_at <= dead_sec`), not vetoed,
    ///   and not handing over, it keeps the lease (stickiness).
    /// - If the holder is silent past `dead_sec` (i.e. `now_sec - heartbeat_at > dead_sec`),
    ///   it loses the lease, is placed on cooldown, and the next eligible candidate in policy
    ///   order is granted `epoch + 1`.
    pub fn role_tick(&mut self, role: &str, now_sec: u64) -> RoleAction {
        let policy = self.policies.get(role).cloned().unwrap_or_default();
        let key = (role.to_owned(), "global".into());
        let current = self
            .rows
            .entry(key.clone())
            .or_insert_with(|| RoleRow::vacant(role, "global"))
            .clone();

        if current.frozen {
            return RoleAction::Frozen {
                role: role.to_owned(),
                epoch: current.epoch,
                changes_last_hour: current.changes_last_hour,
            };
        }

        if let Some(holder) = &current.holder
            && current.state == RoleState::Held
        {
            let vetoed = self.vetoed.contains(holder)
                || holder
                    .split_once('/')
                    .is_some_and(|(o, _)| self.vetoed.contains(o));
            let unreachable = self.unreachable.contains(holder);
            let silent = now_sec.saturating_sub(current.heartbeat_at) > policy.dead_sec;

            if !vetoed && !unreachable && !silent {
                return RoleAction::Renewed {
                    role: role.to_owned(),
                    holder: holder.clone(),
                    epoch: current.epoch,
                };
            }

            let lose_reason = if vetoed {
                "vetoed"
            } else if unreachable {
                "process gone"
            } else {
                "heartbeat timeout"
            };
            self.lost_at
                .insert((role.to_owned(), holder.clone()), now_sec);
            return self.elect_next(role, &policy, Some(holder), lose_reason, now_sec);
        }

        if current.state == RoleState::HandingOver {
            let old = current.holder.clone();
            return self.elect_next(role, &policy, old.as_deref(), "handover", now_sec);
        }

        self.elect_next(role, &policy, None, "vacant", now_sec)
    }

    fn elect_next(
        &mut self,
        role: &str,
        policy: &RolePolicy,
        exclude_holder: Option<&str>,
        reason: &str,
        now_sec: u64,
    ) -> RoleAction {
        let candidates = self.eligible_candidates(role, now_sec);
        let next = candidates
            .iter()
            .find(|c| Some(c.who.as_str()) != exclude_holder)
            .or_else(|| candidates.first())
            .cloned();

        let key = (role.to_owned(), "global".into());
        let row = self
            .rows
            .entry(key.clone())
            .or_insert_with(|| RoleRow::vacant(role, "global"));

        let Some(winner) = next else {
            row.holder = None;
            row.state = RoleState::Vacant;
            row.last_reason = reason.to_owned();
            return RoleAction::Vacant {
                role: role.to_owned(),
                epoch: row.epoch,
                reason: reason.to_owned(),
            };
        };

        self.grant_candidate(role, &winner, policy, reason, now_sec)
    }

    fn grant_candidate(
        &mut self,
        role: &str,
        winner: &RoleCandidate,
        policy: &RolePolicy,
        reason: &str,
        now_sec: u64,
    ) -> RoleAction {
        let key = (role.to_owned(), "global".into());
        let history = self.grant_history.entry(key.clone()).or_default();
        history.retain(|&ts| now_sec.saturating_sub(ts) <= 3600);
        history.push(now_sec);
        let changes_last_hour = history.len() as u32;

        let row = self
            .rows
            .entry(key)
            .or_insert_with(|| RoleRow::vacant(role, "global"));

        if changes_last_hour > policy.max_changes_per_hour && policy.max_changes_per_hour > 0 {
            row.frozen = true;
            row.changes_last_hour = changes_last_hour;
            row.last_reason = "flapping guard".into();
            return RoleAction::Frozen {
                role: role.to_owned(),
                epoch: row.epoch,
                changes_last_hour,
            };
        }

        let new_epoch = row.epoch + 1;
        row.holder = Some(winner.who.clone());
        row.lease_scope = winner.scope;
        row.epoch = new_epoch;
        row.state = RoleState::Held;
        row.granted_at = now_sec;
        row.heartbeat_at = now_sec;
        row.min_hold_until = now_sec.saturating_add(policy.min_hold_min.saturating_mul(60));
        row.last_reason = reason.to_owned();
        row.changes_last_hour = changes_last_hour;
        self.candidate_heartbeats
            .insert(winner.who.clone(), now_sec);

        RoleAction::Granted {
            role: role.to_owned(),
            holder: winner.who.clone(),
            lease_scope: winner.scope,
            epoch: new_epoch,
            reason: reason.to_owned(),
        }
    }

    /// Explicit claim of `role` by `who` (`sommelier role claim R`).
    /// Look up `who` in policy candidates for its lease scope (`full` or `limited`),
    /// or default to `full` if `who` is not a limited guest entry.
    pub fn claim(
        &mut self,
        role: &str,
        who: &str,
        scope_override: Option<LeaseScope>,
        now_sec: u64,
    ) -> Result<RoleRow, FenceRefusal> {
        if self.vetoed.contains(who)
            || who
                .split_once('/')
                .is_some_and(|(o, _)| self.vetoed.contains(o))
        {
            let cur_epoch = self.get(role).map_or(0, |r| r.epoch);
            return Err(FenceRefusal {
                code: crate::exit::REFUSED,
                role: role.to_owned(),
                given_epoch: cur_epoch,
                current_epoch: cur_epoch,
                message: format!("{who} is vetoed from holding role {role}"),
            });
        }
        let policy = self.policies.get(role).cloned().unwrap_or_default();
        let cand = policy
            .candidates
            .iter()
            .find(|c| c.who == who)
            .cloned()
            .unwrap_or_else(|| {
                RoleCandidate::new(who, 1, scope_override.unwrap_or(LeaseScope::Full))
            });
        let cand = RoleCandidate {
            scope: scope_override.unwrap_or(cand.scope),
            ..cand
        };
        self.grant_candidate(role, &cand, &policy, "claimed", now_sec);
        Ok(self.get(role).unwrap().clone())
    }

    /// Voluntary release of `role` by `who` (`sommelier role release R`), triggering election of the next candidate.
    pub fn release(
        &mut self,
        role: &str,
        who: Option<&str>,
        epoch: Option<u64>,
        now_sec: u64,
    ) -> Result<RoleAction, FenceRefusal> {
        let row = self
            .get(role)
            .cloned()
            .unwrap_or_else(|| RoleRow::vacant(role, "global"));
        if let Some(ep) = epoch {
            check_fence(&row, who, ep, None)?;
        } else if let Some(w) = who
            && row.holder.as_deref() != Some(w)
        {
            return Err(FenceRefusal {
                code: crate::exit::REFUSED,
                role: role.to_owned(),
                given_epoch: row.epoch,
                current_epoch: row.epoch,
                message: format!(
                    "you are no longer {} (held by {}, epoch {}), stop",
                    role,
                    row.holder.as_deref().unwrap_or("none"),
                    row.epoch
                ),
            });
        }
        let old = row.holder.clone();
        if let Some(h) = &old {
            self.lost_at.insert((role.to_owned(), h.clone()), now_sec);
        }
        let policy = self.policies.get(role).cloned().unwrap_or_default();
        Ok(self.elect_next(role, &policy, old.as_deref(), "released", now_sec))
    }

    /// Voluntary planned handover with a note (`sommelier role handover R --note-file F`).
    pub fn handover(
        &mut self,
        role: &str,
        who: Option<&str>,
        epoch: Option<u64>,
        note: String,
        now_sec: u64,
    ) -> Result<RoleAction, FenceRefusal> {
        let row = self
            .get(role)
            .cloned()
            .unwrap_or_else(|| RoleRow::vacant(role, "global"));
        if let Some(ep) = epoch {
            check_fence(&row, who, ep, None)?;
        }
        let old = row.holder.clone();
        let key = (role.to_owned(), "global".into());
        if let Some(r) = self.rows.get_mut(&key) {
            r.state = RoleState::HandingOver;
            r.handover_note = Some(note);
        }
        let policy = self.policies.get(role).cloned().unwrap_or_default();
        Ok(self.elect_next(role, &policy, old.as_deref(), "handover", now_sec))
    }

    /// Owner veto of `who` (`sommelier role veto WHO`). If `who` currently holds any role,
    /// immediately triggers failover to the next eligible candidate.
    pub fn veto(&mut self, who: &str, now_sec: u64) -> Vec<RoleAction> {
        self.vetoed.insert(who.to_owned());
        let roles: Vec<String> = self.rows.keys().map(|(r, _)| r.clone()).collect();
        let mut actions = Vec::new();
        for role in roles {
            if let Some(row) = self.get(&role)
                && let Some(holder) = &row.holder
                && (holder == who || holder.starts_with(&format!("{who}/")))
            {
                actions.push(self.role_tick(&role, now_sec));
            }
        }
        actions
    }

    /// Checks a fenced role command against the current ROLE row for `role`.
    pub fn check_command(
        &self,
        role: &str,
        actor: Option<&str>,
        given_epoch: u64,
        cmd: Option<RoleCommand>,
    ) -> Result<&RoleRow, FenceRefusal> {
        let Some(row) = self.get(role) else {
            return Err(FenceRefusal {
                code: crate::exit::REFUSED,
                role: role.to_owned(),
                given_epoch,
                current_epoch: 0,
                message: format!(
                    "you are no longer {role} (your epoch {given_epoch}, current 0), stop"
                ),
            });
        };
        check_fence(row, actor, given_epoch, cmd)?;
        Ok(row)
    }

    /// Builds a `FACT` message with topic `role/<role>` for a role row (section 5.2 and 13.3).
    pub fn fact_message(&self, role: &str) -> Option<Message> {
        let row = self.get(role)?;
        let body = json!({
            "v": 1,
            "role": row.role,
            "scope": row.scope,
            "holder": row.holder,
            "lease_scope": row.lease_scope.as_str(),
            "epoch": row.epoch,
            "state": row.state.as_str(),
            "granted_at": row.granted_at,
            "heartbeat_at": row.heartbeat_at,
            "min_hold_until": row.min_hold_until,
            "last_reason": row.last_reason,
            "changes_last_hour": row.changes_last_hour,
            "frozen": row.frozen,
            "handover_note": row.handover_note,
        })
        .to_string();
        Some(Message {
            id: 0,
            ts: String::new(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: format!("role/{role}"),
            body,
            refs: vec![format!("epoch:{}", row.epoch)],
            to: vec![],
        })
    }

    /// Applies a `FACT` message with topic `role/<role>` during log replay.
    pub fn apply_message(&mut self, msg: &Message) {
        // Only the hub writes role rows. A line of a member gives nobody a role.
        if msg.kind != Kind::Fact || msg.role != Role::Hub {
            return;
        }
        let Some(role) = msg.topic.strip_prefix("role/").filter(|r| !r.is_empty()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.body) else {
            return;
        };
        let scope = v["scope"].as_str().unwrap_or("global").to_owned();
        let holder = v["holder"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let lease_scope = v["lease_scope"]
            .as_str()
            .and_then(LeaseScope::parse)
            .unwrap_or(LeaseScope::Full);
        let state = match v["state"].as_str() {
            Some("held") => RoleState::Held,
            Some("handing_over") => RoleState::HandingOver,
            _ => RoleState::Vacant,
        };
        let row = RoleRow {
            role: role.to_owned(),
            scope: scope.clone(),
            holder,
            lease_scope,
            epoch: v["epoch"].as_u64().unwrap_or(0),
            state,
            granted_at: v["granted_at"].as_u64().unwrap_or(0),
            heartbeat_at: v["heartbeat_at"].as_u64().unwrap_or(0),
            min_hold_until: v["min_hold_until"].as_u64().unwrap_or(0),
            last_reason: v["last_reason"].as_str().unwrap_or("").to_owned(),
            changes_last_hour: v["changes_last_hour"].as_u64().unwrap_or(0) as u32,
            frozen: v["frozen"].as_bool().unwrap_or(false),
            handover_note: v["handover_note"].as_str().map(str::to_owned),
            msg_id: msg.id,
        };
        self.rows.insert((role.to_owned(), scope), row);
    }
}
