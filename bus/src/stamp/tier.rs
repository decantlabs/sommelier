//! The pure tier function. No clock, no file, no random: the caller passes `now`.
//!
//! The result does not depend on the order of the stamps, so a promotion or a demotion can be
//! explained and replayed like a scheduler decision.

use std::collections::BTreeSet;

use serde::Serialize;

use super::{Stamp, StampKind};

const DAY: u64 = 86_400;

/// State of a member's key as the KEY projection knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Active,
    Rotated,
    Revoked,
}

/// What the tier function needs to know about a member besides the stamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberFacts {
    pub member: String,
    /// Seconds since the epoch.
    pub joined_at: u64,
    pub key_state: KeyState,
    pub linked_account: Option<String>,
    pub is_maintainer: bool,
}

/// The numbers of the tier rules. The wiring wave fills them from the policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierRules {
    pub tier1_key_age_days: u64,
    pub tier1_penalty_window_days: u64,
    pub tier1_landed: u64,
    pub tier1_run_agreed: u64,
    pub tier2_landed: u64,
    pub tier2_review_confirmed: u64,
}

impl Default for TierRules {
    fn default() -> Self {
        Self {
            tier1_key_age_days: 7,
            tier1_penalty_window_days: 30,
            tier1_landed: 5,
            tier1_run_agreed: 20,
            tier2_landed: 25,
            tier2_review_confirmed: 20,
        }
    }
}

/// The outcome of [`tier`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TierDecision {
    /// `None` means no tier at all (revoked key or a finding of intent).
    pub tier: Option<u8>,
    /// Plain words, in a fixed order.
    pub reasons: Vec<String>,
    /// What the next tier still needs. Empty at tier 3 and when no tier applies.
    pub missing: Vec<String>,
}

fn plural(n: u64, what: &str) -> String {
    format!("{n} {what}")
}

/// Computes the tier of one member from the stamps about that member.
///
/// Stamps of other subjects are ignored. A penalty (`reverted`, `review_missed`) is open until a
/// `cleared` stamp that is not older than the penalty names the penalty stamp's message id as its
/// evidence. An open penalty drops the earned tier by one.
pub fn tier(member: &MemberFacts, stamps: &[Stamp], rules: &TierRules, now: u64) -> TierDecision {
    let mine: Vec<&Stamp> = stamps
        .iter()
        .filter(|s| s.subject == member.member)
        .collect();
    let mut reasons = Vec::new();

    if member.key_state == KeyState::Revoked {
        reasons.push("the key is revoked".to_owned());
        return TierDecision {
            tier: None,
            reasons,
            missing: Vec::new(),
        };
    }
    if mine.iter().any(|s| s.kind == StampKind::Intent) {
        reasons.push("a finding of bad intent stands".to_owned());
        return TierDecision {
            tier: None,
            reasons,
            missing: Vec::new(),
        };
    }
    if member.is_maintainer {
        reasons.push("named as a maintainer".to_owned());
        return TierDecision {
            tier: Some(3),
            reasons,
            missing: Vec::new(),
        };
    }

    // Distinct stamps by message id where one exists, so a repeated line counts once.
    let mut seen = BTreeSet::new();
    let mine: Vec<&Stamp> = mine
        .into_iter()
        .filter(|s| s.msg_id == 0 || seen.insert(s.msg_id))
        .collect();
    let count = |k: StampKind| mine.iter().filter(|s| s.kind == k).count() as u64;
    let landed = count(StampKind::Landed);
    let agreed = count(StampKind::RunAgreed);
    let confirmed = count(StampKind::ReviewConfirmed);
    let granted = count(StampKind::GrantTier2) > 0;

    let open: Vec<&&Stamp> = mine
        .iter()
        .filter(|p| matches!(p.kind, StampKind::Reverted | StampKind::ReviewMissed))
        .filter(|p| {
            !mine
                .iter()
                .any(|c| c.kind == StampKind::Cleared && c.evidence_msg == p.msg_id && c.at >= p.at)
        })
        .collect();
    let recent_open = open
        .iter()
        .any(|p| now.saturating_sub(p.at) < rules.tier1_penalty_window_days * DAY);

    let key_old = now.saturating_sub(member.joined_at) >= rules.tier1_key_age_days * DAY;
    let by_landed = landed >= rules.tier1_landed;
    let by_runs = agreed >= rules.tier1_run_agreed;
    let linked = member
        .linked_account
        .as_deref()
        .is_some_and(|a| !a.is_empty());

    let tier2_ok = key_old
        && landed >= rules.tier2_landed
        && confirmed >= rules.tier2_review_confirmed
        && linked
        && granted;
    let tier1_ok = key_old && (by_landed || by_runs) && !recent_open;

    let earned: u8 = if tier2_ok {
        2
    } else if tier1_ok {
        1
    } else {
        0
    };
    match earned {
        2 => reasons.push("meets every fact of tier 2 and holds a maintainer grant".to_owned()),
        1 => reasons.push(if by_landed {
            format!(
                "{} landed, key old enough, no open penalty",
                plural(landed, "orders")
            )
        } else {
            format!(
                "{} agreed, key old enough, no open penalty",
                plural(agreed, "test reports")
            )
        }),
        _ => reasons.push("has not met the tier 1 rule yet".to_owned()),
    }
    let final_tier = if open.is_empty() {
        earned
    } else {
        reasons.push(format!(
            "dropped one tier for {} open penalty stamp(s) until a cleared stamp names them",
            open.len()
        ));
        earned.saturating_sub(1)
    };

    let mut missing = Vec::new();
    if !open.is_empty() {
        missing.push("a cleared stamp for each open penalty".to_owned());
    } else if earned == 0 {
        if !key_old {
            missing.push(format!("key age of {} days", rules.tier1_key_age_days));
        }
        if !(by_landed || by_runs) {
            missing.push(format!(
                "{} more landed or {} more run_agreed",
                rules.tier1_landed.saturating_sub(landed),
                rules.tier1_run_agreed.saturating_sub(agreed)
            ));
        }
    } else if earned == 1 {
        push_tier2_gaps(&mut missing, rules, landed, confirmed, linked, granted);
    } else {
        missing.push("a maintainer seat named in the policy".to_owned());
    }
    TierDecision {
        tier: Some(final_tier),
        reasons,
        missing,
    }
}

fn push_tier2_gaps(
    missing: &mut Vec<String>,
    rules: &TierRules,
    landed: u64,
    confirmed: u64,
    linked: bool,
    granted: bool,
) {
    if landed < rules.tier2_landed {
        missing.push(format!(
            "{} more landed for tier 2",
            rules.tier2_landed - landed
        ));
    }
    if confirmed < rules.tier2_review_confirmed {
        missing.push(format!(
            "{} more review_confirmed for tier 2",
            rules.tier2_review_confirmed - confirmed
        ));
    }
    if !linked {
        missing.push("a linked account for tier 2".to_owned());
    }
    if !granted {
        missing.push("a grant_tier2 stamp from a maintainer".to_owned());
    }
}
