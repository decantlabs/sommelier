//! The work of the hub tick: who reviews what, who is too slow, which submission has landed.
//!
//! The tick reads a [`Snapshot`] of the tables in one short step under the state lock, then plans outside the lock
//! with [`plan`], which is pure. The address blocks are asked from the front door between the two steps.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params};

use super::data::{self, Review, SubRow};
use super::flow::{self, Decision};
use super::ops::ReviewConfig;
use super::{Candidate, OtherFamily, ReviewRule, SubmissionView, pick_reviewers};
use crate::door;
use crate::msg::{Message, Role};

/// A member the picker may look at.
#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub name: String,
    pub tier: u8,
    pub online: bool,
    pub family: Option<String>,
    /// A key member. It cannot be named in `to` of a message.
    pub is_key: bool,
    /// Below tier 2 and no `landed` stamp of its own: it may test with weight, it may not review code.
    pub unproven: bool,
}

/// Everything the tick reads.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub now_sec: u64,
    pub members: Vec<MemberInfo>,
    /// Open reviews by member.
    pub open: HashMap<String, u32>,
    pub verified: Vec<(SubRow, Vec<Review>)>,
    pub reviewing: Vec<(SubRow, Vec<Review>)>,
    /// Approved submissions whose `bundle:` entry has landed.
    pub landed: Vec<SubRow>,
    /// The next review number by task.
    pub next_n: HashMap<String, u64>,
    /// `(code, key)` of the REVIEW_SHORT alerts that exist.
    pub short_alerts: HashSet<String>,
    /// Model family by author.
    pub author_family: HashMap<String, Option<String>>,
}

impl Snapshot {
    /// Nothing to decide, the tick can skip the secret and the blocks.
    pub fn is_idle(&self) -> bool {
        self.verified.is_empty() && self.reviewing.is_empty() && self.landed.is_empty()
    }

    /// Every member name the picker needs a block for.
    pub fn names(&self) -> Vec<String> {
        let mut set: HashSet<String> = self.members.iter().map(|m| m.name.clone()).collect();
        for (s, _) in self.verified.iter().chain(self.reviewing.iter()) {
            set.insert(s.member.clone());
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }
}

fn role_of(text: &str) -> Option<Role> {
    match text {
        "orchestrator" => Some(Role::Orchestrator),
        "member" => Some(Role::Member),
        "observer" => Some(Role::Observer),
        "hub" => Some(Role::Hub),
        _ => None,
    }
}

/// True when the member has more `landed` stamps than `reverted` ones. A landed patch that was
/// reverted does not prove a reviewer. Both counts come from the STAMP table.
pub fn has_landed_stamp(conn: &Connection, member: &str) -> bool {
    let count = |kind: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM STAMP WHERE subject = ?1 AND kind = ?2",
            params![member, kind],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    count("landed") > count("reverted")
}

/// The tier of a member for a decision made now, read the way the tick reads it.
pub fn tier_of(conn: &Connection, name: &str) -> i64 {
    let key = crate::key::book::current_row(conn, name).ok().flatten();
    let role = if key.is_some() {
        None
    } else {
        conn.query_row(
            "SELECT role FROM MEMBER WHERE name = ?1",
            params![name],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|r| role_of(&r))
    };
    crate::stamp::tier_table::tier_from_table(conn, name, role)
}

/// Reads the tables.
pub fn read(conn: &Connection, cfg: &ReviewConfig, now_sec: u64) -> Result<Snapshot, String> {
    let mut snap = Snapshot {
        now_sec,
        ..Snapshot::default()
    };
    let mut st = conn
        .prepare("SELECT name, role, last_seen_at FROM MEMBER ORDER BY name")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for r in rows {
        let (name, role, seen) = r.map_err(|e| e.to_string())?;
        let key = crate::key::book::current_row(conn, &name)?;
        if let Some((_, _, _, state)) = &key
            && *state != crate::key::KeyState::Active
        {
            continue;
        }
        let is_key = key.is_some();
        let role = if is_key { None } else { role_of(&role) };
        let tier = crate::stamp::tier_table::tier_from_table(conn, &name, role);
        if tier < 0 {
            continue;
        }
        let unproven = tier < 2 && !has_landed_stamp(conn, &name);
        snap.members.push(MemberInfo {
            unproven,
            family: data::family_of(conn, &name),
            tier: tier.min(3) as u8,
            online: (seen.max(0) as u64).saturating_add(cfg.online_secs) >= now_sec,
            is_key,
            name,
        });
    }
    snap.open = data::open_counts(conn)?.into_iter().collect();
    for sub in data::submissions_in_state(conn, "verified")? {
        let reviews = data::reviews_of(conn, &sub.sha256)?;
        snap.author_family
            .insert(sub.member.clone(), data::family_of(conn, &sub.member));
        snap.verified.push((sub, reviews));
    }
    for sub in data::submissions_in_state(conn, "reviewing")? {
        let reviews = data::reviews_of(conn, &sub.sha256)?;
        snap.author_family
            .insert(sub.member.clone(), data::family_of(conn, &sub.member));
        snap.reviewing.push((sub, reviews));
    }
    for sha in data::landed_bundles(conn)? {
        if let Some(s) = data::query_submission(conn, &sha)?
            && s.state == "approved"
        {
            snap.landed.push(s);
        }
    }
    let mut st = conn
        .prepare("SELECT task, MAX(n) + 1 FROM REVIEW GROUP BY task")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    for r in rows {
        let (task, n) = r.map_err(|e| e.to_string())?;
        snap.next_n.insert(task, n as u64);
    }
    let mut st = conn
        .prepare("SELECT key FROM ALERT WHERE code = 'REVIEW_SHORT'")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    for r in rows {
        snap.short_alerts.insert(r.map_err(|e| e.to_string())?);
    }
    Ok(snap)
}

/// Asks the front door for the block of every name. Call it without the state lock.
pub fn blocks_for(hub: &crate::hub::Hub, names: &[String]) -> HashMap<String, Option<String>> {
    names
        .iter()
        .map(|n| (n.clone(), door::block_of(hub, n)))
        .collect()
}

fn candidates(
    snap: &Snapshot,
    blocks: &HashMap<String, Option<String>>,
    skip: &HashSet<String>,
) -> Vec<Candidate> {
    snap.members
        .iter()
        .filter(|m| !skip.contains(&m.name))
        .map(|m| Candidate {
            member: m.name.clone(),
            tier: m.tier,
            block: blocks.get(&m.name).cloned().flatten(),
            account: None,
            family: m.family.clone(),
            online: m.online,
            open_reviews: snap.open.get(&m.name).copied().unwrap_or(0),
            unproven: m.unproven,
        })
        .collect()
}

fn view_of(
    snap: &Snapshot,
    blocks: &HashMap<String, Option<String>>,
    s: &SubRow,
) -> SubmissionView {
    SubmissionView {
        bundle_sha256: s.sha256.clone(),
        author: s.member.clone(),
        author_tier: s.tier,
        author_block: blocks.get(&s.member).cloned().flatten(),
        author_account: None,
        author_family: snap.author_family.get(&s.member).cloned().flatten(),
    }
}

/// The rule for the audit seat: one maintainer, tier 3.
fn audit_rule(base: &ReviewRule) -> ReviewRule {
    ReviewRule {
        reviewers: [1; 4],
        min_tier: [3; 4],
        other_family: OtherFamily::Ignore,
        max_open: base.max_open,
    }
}

/// The rule with at least one reviewer for every tier. A rule of zero reviewers would let a patch through unread.
fn safe_rule(base: &ReviewRule) -> ReviewRule {
    let mut r = base.clone();
    for n in &mut r.reviewers {
        *n = (*n).max(1);
    }
    r
}

fn to_names(snap: &Snapshot, member: &str) -> Vec<String> {
    match snap.members.iter().find(|m| m.name == member) {
        Some(m) if !m.is_key => vec![member.to_owned()],
        _ => Vec::new(),
    }
}

fn assigned_alert(
    snap: &Snapshot,
    task: &str,
    n: u64,
    member: &str,
    role: &str,
    now_ms: u64,
) -> Message {
    flow::alert(
        "REVIEW_ASSIGNED",
        &format!("{task}/{n}"),
        &format!("review {task}/{n} ({role}) is assigned to you: run `sommelier review next`"),
        member,
        &to_names(snap, member),
        now_ms,
    )
}

/// Plans the messages of one tick. `diff_ready` holds the submissions whose diff file is stored.
pub fn plan(
    snap: &Snapshot,
    blocks: &HashMap<String, Option<String>>,
    cfg: &ReviewConfig,
    secret: &[u8],
    diff_ready: &HashSet<String>,
    now_ms: u64,
) -> Vec<Message> {
    let now = snap.now_sec;
    let due = now.saturating_add(cfg.answer_secs);
    let rule = safe_rule(&cfg.rule);
    let arule = audit_rule(&cfg.rule);
    let mut next_n = snap.next_n.clone();
    let mut out = Vec::new();

    for (sub, reviews) in &snap.verified {
        if !diff_ready.contains(&sub.sha256) {
            continue;
        }
        if !reviews.is_empty() {
            out.push(flow::submission_fact(sub, "reviewing", now));
            continue;
        }
        let view = view_of(snap, blocks, sub);
        let pick = pick_reviewers(
            &view,
            &candidates(snap, blocks, &HashSet::new()),
            &rule,
            secret,
        );
        if pick.short_by > 0 {
            short_alert(
                snap,
                &mut out,
                &sub.sha256,
                &sub.task,
                pick.short_by,
                now_ms,
            );
            continue;
        }
        // The audit is decided and shown only after the required passes, see `ops::settle`.
        let required = pick.picked.len() as u32;
        for member in &pick.picked {
            let n = next_n.entry(sub.task.clone()).or_insert(1);
            out.push(flow::assignment_fact(
                &sub.task,
                *n,
                &sub.sha256,
                member,
                "required",
                required,
                false,
                &[],
                now,
                due,
            ));
            out.push(assigned_alert(
                snap, &sub.task, *n, member, "required", now_ms,
            ));
            *n += 1;
        }
        out.push(flow::submission_fact(sub, "reviewing", now));
    }

    for (sub, reviews) in &snap.reviewing {
        // Everyone who holds a row of this submission now. Every pick below joins the set, so two rows that
        // expire in one tick never fall to the same member.
        let mut holders: HashSet<String> = reviews.iter().map(|r| r.row.reviewer.clone()).collect();
        if flow::decide(reviews) == Decision::NeedAudit {
            let view = view_of(snap, blocks, sub);
            let pick = pick_reviewers(&view, &candidates(snap, blocks, &holders), &arule, secret);
            if let Some(member) = pick.picked.first() {
                holders.insert(member.clone());
                let n = next_n.entry(sub.task.clone()).or_insert(1);
                out.push(flow::assignment_fact(
                    &sub.task,
                    *n,
                    &sub.sha256,
                    member,
                    "audit",
                    1,
                    false,
                    &[],
                    now,
                    due,
                ));
                out.push(assigned_alert(snap, &sub.task, *n, member, "audit", now_ms));
                *n += 1;
            } else {
                short_alert(
                    snap,
                    &mut out,
                    &format!("{}/audit", sub.sha256),
                    &sub.task,
                    1,
                    now_ms,
                );
            }
            continue;
        }
        for r in reviews {
            if r.row.state != "assigned" || r.meta.due == 0 || now <= r.meta.due {
                continue;
            }
            // A timeout excludes the silent holder for this round only. Earlier holders may be picked again.
            let skip = holders.clone();
            let view = view_of(snap, blocks, sub);
            let (use_rule, role) = if r.is_audit() {
                (&arule, "audit")
            } else {
                (&rule, "required")
            };
            let mut one = use_rule.clone();
            one.reviewers = [1; 4];
            let pick = pick_reviewers(&view, &candidates(snap, blocks, &skip), &one, secret);
            if let Some(member) = pick.picked.first() {
                holders.insert(member.clone());
                let mut prev = r.meta.prev.clone();
                prev.push(r.row.reviewer.clone());
                out.push(flow::assignment_fact(
                    &r.row.task,
                    r.row.n,
                    &sub.sha256,
                    member,
                    role,
                    r.meta.required,
                    r.row.audited,
                    &prev,
                    now,
                    due,
                ));
                out.push(assigned_alert(
                    snap,
                    &r.row.task,
                    r.row.n,
                    member,
                    role,
                    now_ms,
                ));
            } else {
                short_alert(
                    snap,
                    &mut out,
                    &format!("{}/{}", sub.sha256, r.row.n),
                    &r.row.task,
                    1,
                    now_ms,
                );
            }
        }
    }

    for sub in &snap.landed {
        out.push(flow::submission_fact(sub, "landed", now));
    }
    out
}

/// REVIEW_SHORT goes to the coordinator once per key.
fn short_alert(
    snap: &Snapshot,
    out: &mut Vec<Message>,
    key: &str,
    task: &str,
    short_by: usize,
    now_ms: u64,
) {
    if snap.short_alerts.contains(key)
        || out
            .iter()
            .any(|m| m.topic == format!("alert/REVIEW_SHORT/{key}"))
    {
        return;
    }
    out.push(flow::alert(
        "REVIEW_SHORT",
        key,
        &format!("task {task}: {short_by} reviewer(s) missing, no eligible member is online"),
        "coordinator",
        &[],
        now_ms,
    ));
}
