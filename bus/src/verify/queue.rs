//! Which submissions start now: the shared capacity of the runners and its split by tier (docs/factory.md 5.3).
//!
//! [`next_to_verify`] is pure and deterministic. The ceiling is the number of jobs GitHub runs at once
//! (20 on the Free plan), so the default keeps four jobs in reserve and uses at most 16.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One submission in the queue or on a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The SHA 256 of the bundle.
    pub submission: String,
    /// The tier of the member, 0 to 3.
    pub tier: u8,
    /// The member id.
    pub member: String,
    /// The address block of the member (a network prefix), as the join gate recorded it.
    pub block: String,
    /// When it entered the queue, milliseconds.
    pub queued_at: u64,
}

/// The capacity rules. Settings come in as a struct, the policy file is wired in a later wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyRules {
    /// The most runs at once.
    pub max_concurrent: usize,
    /// All tier 0 entries together may hold at most this share of `max_concurrent`, in percent.
    pub tier0_share_percent: u32,
}

impl Default for VerifyRules {
    fn default() -> Self {
        Self {
            max_concurrent: 16,
            tier0_share_percent: 20,
        }
    }
}

impl VerifyRules {
    /// The most tier 0 runs at once: the share of `max_concurrent`, rounded down.
    pub fn tier0_cap(&self) -> usize {
        let pct = self.tier0_share_percent.min(100) as usize;
        self.max_concurrent.saturating_mul(pct) / 100
    }
}

/// How many times a member and a block already hold a run or were picked in this call.
#[derive(Default)]
struct Served {
    block: BTreeMap<String, usize>,
    member: BTreeMap<(String, String), usize>,
}

/// Picks `slots` tier 0 entries. Fair per address block first, then per member inside a block.
/// The block with the fewest runs and picks goes first, ties go to the block holding the oldest entry
/// and then to the smaller name. The same rule applies to members inside the block. A member's own
/// entries go oldest first.
fn pick_tier0(pending: &[&Entry], running: &[Entry], slots: usize) -> Vec<String> {
    let mut served = Served::default();
    for e in running.iter().filter(|e| e.tier == 0) {
        *served.block.entry(e.block.clone()).or_default() += 1;
        *served
            .member
            .entry((e.block.clone(), e.member.clone()))
            .or_default() += 1;
    }
    // block -> member -> entries oldest first
    let mut groups: BTreeMap<&str, BTreeMap<&str, VecDeque<&Entry>>> = BTreeMap::new();
    let mut sorted: Vec<&Entry> = pending.to_vec();
    sorted.sort_by(|a, b| (a.queued_at, &a.submission).cmp(&(b.queued_at, &b.submission)));
    for e in sorted {
        groups
            .entry(e.block.as_str())
            .or_default()
            .entry(e.member.as_str())
            .or_default()
            .push_back(e);
    }
    let head = |q: &VecDeque<&Entry>| q.front().map(|e| (e.queued_at, e.submission.clone()));
    let mut picked = Vec::new();
    while picked.len() < slots {
        // The block to serve.
        let block = groups
            .iter()
            .filter_map(|(b, members)| {
                let oldest = members.values().filter_map(head).min()?;
                let count = served.block.get(*b).copied().unwrap_or(0);
                Some(((count, oldest, *b), *b))
            })
            .min()
            .map(|(_, b)| b);
        let Some(block) = block else { break };
        let members = groups.get_mut(block).expect("block exists");
        let member = members
            .iter()
            .filter_map(|(m, q)| {
                let h = head(q)?;
                let count = served
                    .member
                    .get(&(block.to_owned(), (*m).to_owned()))
                    .copied()
                    .unwrap_or(0);
                Some(((count, h, *m), *m))
            })
            .min()
            .map(|(_, m)| m)
            .expect("block has a member with entries");
        let entry = members
            .get_mut(member)
            .and_then(VecDeque::pop_front)
            .expect("member has an entry");
        if members.get(member).is_some_and(VecDeque::is_empty) {
            members.remove(member);
        }
        if members.is_empty() {
            groups.remove(block);
        }
        *served.block.entry(block.to_owned()).or_default() += 1;
        *served
            .member
            .entry((block.to_owned(), member.to_owned()))
            .or_default() += 1;
        picked.push(entry.submission.clone());
    }
    picked
}

/// Which pending submissions start now.
///
/// * Free capacity is `max_concurrent` minus `running`.
/// * Tier 0 as a whole may hold [`VerifyRules::tier0_cap`] runs. Its slots are set first, so other tiers
///   cannot starve it and it cannot take more than its share, even with a thousand pending entries.
/// * Inside tier 0 the pick is fair per address block and, inside a block, per member (see
///   `pick_tier0`). Two members of one block share one turn.
/// * Other tiers take the remaining capacity, oldest first.
///
/// The result is ordered by queue time and submission hash. Entries already in `running` are skipped.
pub fn next_to_verify(pending: &[Entry], running: &[Entry], rules: &VerifyRules) -> Vec<String> {
    let free = rules.max_concurrent.saturating_sub(running.len());
    if free == 0 {
        return Vec::new();
    }
    let busy: BTreeSet<&str> = running.iter().map(|e| e.submission.as_str()).collect();
    let mut seen = BTreeSet::new();
    let candidates: Vec<&Entry> = pending
        .iter()
        .filter(|e| !busy.contains(e.submission.as_str()) && seen.insert(e.submission.as_str()))
        .collect();
    let tier0: Vec<&Entry> = candidates.iter().copied().filter(|e| e.tier == 0).collect();
    let mut others: Vec<&Entry> = candidates.iter().copied().filter(|e| e.tier != 0).collect();
    others.sort_by(|a, b| (a.queued_at, &a.submission).cmp(&(b.queued_at, &b.submission)));

    let running0 = running.iter().filter(|e| e.tier == 0).count();
    let room0 = rules.tier0_cap().saturating_sub(running0).min(free);
    let picked0 = pick_tier0(&tier0, running, room0.min(tier0.len()));
    let rest = free - picked0.len();

    let mut chosen: Vec<&Entry> = others.into_iter().take(rest).collect();
    let picked0: BTreeSet<&str> = picked0.iter().map(String::as_str).collect();
    chosen.extend(
        tier0
            .iter()
            .filter(|e| picked0.contains(e.submission.as_str())),
    );
    chosen.sort_by(|a, b| (a.queued_at, &a.submission).cmp(&(b.queued_at, &b.submission)));
    chosen.into_iter().map(|e| e.submission.clone()).collect()
}
