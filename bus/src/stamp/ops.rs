//! Hub side of standing: the rules that issue stamps, grants by maintainers and the tier of a member.
//!
//! See `docs/factory.md` section 5. Every stamp and every change of a tier is a FACT of the hub, so a
//! replay rebuilds the STAMP and the TIER table. The tick holds no clock inside the pure parts and
//! runs no slow work under the state lock.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::tier_table::{NO_TIER, tier_from_table};
use super::{
    KeyState, MemberFacts, Stamp, StampKind, TierRules, query_stamps, tier, validate_stamp,
};
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::msg::{Kind, Message, Role};
use crate::report::types::Outcome;
use crate::report::{
    QuorumRules, ReportView, Verdict, cell, classify, counted_members, query_reports,
};
use crate::review::query_reviews;
use crate::time::parse_ms;

/// Longest `cause` of a maintainer request, in characters.
const MAX_CAUSE: usize = 160;

/// Settings of this part of the hub. Lives in `HubConfig.stamp`. Everything is off by default.
#[derive(Clone, Debug)]
pub struct StampConfig {
    /// Master switch. Off: no stamp rule runs, no tier is computed, key members stay at tier 0.
    pub enabled: bool,
    /// The numbers of the tier rules.
    pub rules: TierRules,
    /// Maintainers: member ids of key members and names of tokens.
    pub maintainers: Vec<String>,
    /// Days after which a landed review counts as confirmed.
    pub confirm_days: u64,
    /// Distinct owners in distinct blocks that settle a cell of the matrix.
    pub quorum: usize,
    /// Whether a tier 0 member may take and deliver orders.
    pub tier0_coding: bool,
}

impl Default for StampConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rules: TierRules::default(),
            maintainers: Vec::new(),
            confirm_days: 14,
            quorum: QuorumRules::default().quorum,
            tier0_coding: false,
        }
    }
}

impl StampConfig {
    /// True when standing is on and `name` is listed as a maintainer.
    pub fn is_listed_maintainer(&self, name: &str) -> bool {
        self.enabled
            && self
                .maintainers
                .iter()
                .any(|m| m.eq_ignore_ascii_case(name))
    }
}

// The tier of a member

/// The tier a member holds when no TIER row says otherwise, for use when the store is not open.
fn default_tier(role: Option<Role>) -> i64 {
    match role {
        Some(Role::Orchestrator | Role::Hub) => 3,
        Some(Role::Member) => 2,
        Some(Role::Observer) => NO_TIER,
        None => 0,
    }
}

/// The tier of `member` right now, read from the TIER table. A token member that has no row is
/// tier 2, the orchestrator tier 3, a key member without a row tier 0. -1 means no tier at all.
pub fn tier_of(hub: &Hub, member: &str) -> i64 {
    let role = hub.token_role(member);
    hub.with_store(|s| tier_from_table(s.connection(), member, role))
        .unwrap_or_else(|| default_tier(role))
}

/// The tier that holds a key member to its list of requests. With standing switched off every key
/// member is tier 0, as before.
pub fn request_tier(hub: &Hub, member: &str) -> i64 {
    if hub.config().stamp.enabled {
        tier_of(hub, member)
    } else {
        0
    }
}

// What the tick reads

/// A key member as the KEY table knows it: the newest row of the member.
struct KeyRow {
    member: String,
    state: KeyState,
    joined_at: u64,
    msg_id: u64,
}

fn key_rows(conn: &Connection) -> Vec<KeyRow> {
    let Ok(mut stmt) = conn.prepare(
        r#"SELECT member, state, joined_at, msg_id FROM "KEY" k
           WHERE msg_id = (SELECT MAX(msg_id) FROM "KEY" WHERE member = k.member)
           ORDER BY member"#,
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        let state: String = r.get(1)?;
        let joined: String = r.get(2)?;
        Ok(KeyRow {
            member: r.get(0)?,
            state: if state == "revoked" {
                KeyState::Revoked
            } else {
                KeyState::Active
            },
            joined_at: parse_ms(&joined).unwrap_or(0) / 1000,
            msg_id: r.get::<_, i64>(3)?.max(0) as u64,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}

/// One row of the SUBMISSION table, the columns the rules need.
#[derive(Clone, Debug)]
struct Sub {
    sha: String,
    task: String,
    member: String,
    head: String,
    state: String,
    updated_at: u64,
    msg_id: u64,
}

fn submissions(conn: &Connection) -> Vec<Sub> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT sha256, task, member, head, state, updated_at, msg_id FROM SUBMISSION
         ORDER BY msg_id, sha256",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok(Sub {
            sha: r.get(0)?,
            task: r.get(1)?,
            member: r.get(2)?,
            head: r.get(3)?,
            state: r.get(4)?,
            updated_at: r.get::<_, i64>(5)?.max(0) as u64,
            msg_id: r.get::<_, i64>(6)?.max(0) as u64,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}

fn outcome_of(name: &str) -> Option<Outcome> {
    match name {
        "works" => Some(Outcome::Works),
        "fails" => Some(Outcome::Fails),
        "inconclusive" => Some(Outcome::Inconclusive),
        _ => None,
    }
}

// Stamp messages

fn hub_fact(topic: String, body: String) -> Message {
    Message {
        id: 0,
        ts: String::new(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic,
        body,
        refs: vec![],
        to: vec![],
    }
}

/// The FACT `stamp/<subject>` of one stamp. `cause` is the free line of a maintainer.
#[allow(clippy::too_many_arguments)]
fn stamp_message(
    kind: StampKind,
    subject: &str,
    issuer: &str,
    msg: u64,
    commit: &str,
    reference: &str,
    at: u64,
    cause: Option<&str>,
) -> Message {
    let mut evidence = json!({ "msg": msg });
    if !commit.is_empty() {
        evidence["commit"] = json!(commit);
    }
    if !reference.is_empty() {
        evidence["ref"] = json!(reference);
    }
    let mut body = json!({
        "v": 1, "subject": subject, "issuer": issuer, "kind": kind.as_str(),
        "evidence": evidence, "at": at,
    });
    if let Some(c) = cause {
        body["cause"] = json!(c);
    }
    hub_fact(format!("stamp/{subject}"), body.to_string())
}

/// Collects the stamps one tick wants to write, and never a stamp that exists already.
struct Rules {
    now: u64,
    stamps: Vec<Stamp>,
    wanted: BTreeSet<String>,
    out: Vec<Message>,
}

impl Rules {
    fn has(&self, kind: StampKind, subject: &str, msg: u64, reference: &str) -> bool {
        self.stamps.iter().any(|s| {
            s.kind == kind
                && s.subject == subject
                && (s.evidence_msg == msg || (!reference.is_empty() && s.evidence_ref == reference))
        }) || self
            .wanted
            .contains(&Self::key(kind, subject, msg, reference))
    }

    fn key(kind: StampKind, subject: &str, msg: u64, reference: &str) -> String {
        let what = if reference.is_empty() {
            msg.to_string()
        } else {
            reference.to_owned()
        };
        format!("{}|{subject}|{what}", kind.as_str())
    }

    /// Writes one rule stamp unless it exists. Returns whether it was added.
    fn add(
        &mut self,
        kind: StampKind,
        subject: &str,
        msg: u64,
        commit: &str,
        reference: &str,
    ) -> bool {
        if subject.is_empty() || msg == 0 || self.has(kind, subject, msg, reference) {
            return false;
        }
        self.wanted.insert(Self::key(kind, subject, msg, reference));
        self.out.push(stamp_message(
            kind, subject, "hub", msg, commit, reference, self.now, None,
        ));
        true
    }
}

/// Profile, machine key and build: one cell of the matrix.
type CellKey<'a> = (&'a str, &'a str, &'a str);

/// The four rules that read the SUBMISSION, REVIEW and REPORT tables.
fn run_rules(
    conn: &Connection,
    cfg: &StampConfig,
    now: u64,
    _blocks: &BTreeMap<String, Option<String>>,
    rules: &mut Rules,
) {
    let subs = submissions(conn);
    let landed: BTreeMap<&str, &Sub> = subs
        .iter()
        .filter(|s| s.state == "landed")
        .map(|s| (s.sha.as_str(), s))
        .collect();

    // landed: for the author of a submission that reached main.
    for s in landed.values() {
        rules.add(StampKind::Landed, &s.member, s.msg_id, &s.head, &s.sha);
    }

    // superseded: closed after another submission of the same task landed. Neutral.
    for s in subs.iter().filter(|s| s.state == "rejected") {
        let beaten = landed
            .values()
            .any(|l| l.task == s.task && l.sha != s.sha && s.updated_at >= l.updated_at);
        if beaten {
            rules.add(StampKind::Superseded, &s.member, s.msg_id, &s.head, &s.sha);
        }
    }

    // review_confirmed: a pass whose submission is still on main after the window.
    let window = cfg.confirm_days.saturating_mul(86_400);
    for r in query_reviews(conn).unwrap_or_default() {
        if r.state != "answered" || r.verdict != "pass" || r.reviewer.is_empty() {
            continue;
        }
        let Some(sub) = landed.get(r.submission.as_str()) else {
            continue;
        };
        if now.saturating_sub(sub.updated_at) < window {
            continue;
        }
        let reverted = rules
            .stamps
            .iter()
            .any(|s| s.kind == StampKind::Reverted && s.evidence_ref == sub.sha);
        if !reverted {
            rules.add(
                StampKind::ReviewConfirmed,
                &r.reviewer,
                r.msg_id,
                &sub.head,
                &sub.sha,
            );
        }
    }

    // run_agreed and run_outlier: a report against a settled cell of the matrix. Only a report whose
    // sender had a known address block can be judged: the block was taken when the report arrived
    // and is kept in the side store. Of the reports of one block only the one that counts toward the
    // cell is judged. A cell lifts its members only when at least one agreeing report comes from a
    // member of tier 1 or above, so a ring of fresh keys cannot lift itself.
    let reports = query_reports(conn).unwrap_or_default();
    let report_blocks = crate::report::side::blocks_of(conn);
    let key_members: BTreeSet<String> = key_rows(conn).into_iter().map(|k| k.member).collect();
    let tier_of_reporter = |member: &str| -> u8 {
        if key_members.contains(member) {
            tier_from_table(conn, member, None).clamp(0, 3) as u8
        } else {
            // A token member was vouched for by the owner of the hub, as a tier 2 member is.
            2
        }
    };
    let mut cells: BTreeMap<CellKey<'_>, Vec<(usize, ReportView)>> = BTreeMap::new();
    for (i, r) in reports.iter().enumerate() {
        let Some(outcome) = outcome_of(&r.outcome) else {
            continue;
        };
        let view = ReportView {
            member: r.member.clone(),
            tier: tier_of_reporter(&r.member),
            block: report_blocks
                .get(&(r.run.clone(), r.member.clone()))
                .cloned(),
            outcome,
            at: r.at,
        };
        cells
            .entry((r.profile.as_str(), r.machine_key.as_str(), r.build.as_str()))
            .or_default()
            .push((i, view));
    }
    let quorum = QuorumRules { quorum: cfg.quorum };
    for group in cells.values() {
        let views: Vec<ReportView> = group.iter().map(|(_, v)| v.clone()).collect();
        let settled = cell(&views, &quorum);
        let counted = counted_members(&views);
        let lifted = group.iter().any(|(_, v)| {
            counted.contains(&v.member) && v.tier >= 1 && classify(v, &settled) == Verdict::Agreed
        });
        for (i, view) in group {
            if !counted.contains(&view.member) {
                continue;
            }
            let row = &reports[*i];
            let kind = match classify(view, &settled) {
                Verdict::Agreed if lifted => StampKind::RunAgreed,
                Verdict::Agreed | Verdict::None => continue,
                Verdict::Outlier => StampKind::RunOutlier,
            };
            // A report is judged once.
            let judged = [StampKind::RunAgreed, StampKind::RunOutlier]
                .iter()
                .any(|k| rules.has(*k, &row.member, row.msg_id, &row.run));
            if !judged {
                rules.add(kind, &row.member, row.msg_id, &row.build, &row.run);
            }
        }
    }
}

/// Everything one tick writes: the stamps of the rules, then the tiers that changed.
fn plan(
    conn: &Connection,
    cfg: &StampConfig,
    now: u64,
    blocks: &BTreeMap<String, Option<String>>,
) -> Vec<Message> {
    let mut rules = Rules {
        now,
        stamps: query_stamps(conn).unwrap_or_default(),
        wanted: BTreeSet::new(),
        out: Vec::new(),
    };
    run_rules(conn, cfg, now, blocks, &mut rules);
    let mut out = std::mem::take(&mut rules.out);
    for k in key_rows(conn) {
        let facts = member_facts(cfg, &k, &rules.stamps);
        let decision = tier(&facts, &rules.stamps, &cfg.rules, now);
        let new = decision.tier.map_or(NO_TIER, i64::from);
        if new == tier_from_table(conn, &k.member, None) {
            continue;
        }
        let body = json!({
            "v": 1, "member": k.member, "tier": new, "reasons": decision.reasons, "at": now,
        });
        out.push(hub_fact(format!("tier/{}", k.member), body.to_string()));
    }
    out
}

fn member_facts(cfg: &StampConfig, k: &KeyRow, stamps: &[Stamp]) -> MemberFacts {
    // A maintainer who grants tier 2 knows the account behind the key: the grant is the link.
    let granted = stamps
        .iter()
        .any(|s| s.subject == k.member && s.kind == StampKind::GrantTier2);
    MemberFacts {
        member: k.member.clone(),
        joined_at: k.joined_at,
        key_state: k.state,
        linked_account: granted.then(|| "granted".to_owned()),
        is_maintainer: cfg.maintainers.iter().any(|m| m == &k.member),
    }
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let cfg = &hub.config().stamp;
    if !cfg.enabled {
        return Vec::new();
    }
    // The address blocks come from the front door, outside the state lock.
    let reporters: Vec<String> = hub
        .with_store(|s| {
            let mut m: Vec<String> = query_reports(s.connection())
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.member)
                .collect();
            m.sort();
            m.dedup();
            m
        })
        .unwrap_or_default();
    let blocks: BTreeMap<String, Option<String>> = reporters
        .into_iter()
        .map(|m| {
            // The same blocks as the matrix of the test network sees, the test hook first.
            let b = crate::report::ops::block_for(hub, &hub.config().test, &m);
            (m, b)
        })
        .collect();
    hub.with_store(|s| plan(s.connection(), cfg, now_ms / 1000, &blocks))
        .unwrap_or_default()
}

// The part of /api/state

/// The `standing` rows of `/api/state` for a hub that runs with standing on: counts by kind, the tier
/// and what is missing for the next one. `None` when standing is off.
pub fn api_rows(conn: &Connection, cfg: &StampConfig, now_sec: u64) -> Option<Vec<Value>> {
    if !cfg.enabled {
        return None;
    }
    let rules = &cfg.rules;
    let stamps = query_stamps(conn).ok()?;
    let keys = key_rows(conn);
    let mut members: BTreeSet<String> = stamps.iter().map(|s| s.subject.clone()).collect();
    members.extend(keys.iter().map(|k| k.member.clone()));
    let rows = members
        .into_iter()
        .map(|member| {
            let mut counts: BTreeMap<&str, u64> =
                StampKind::ALL.iter().map(|k| (k.as_str(), 0)).collect();
            for s in stamps.iter().filter(|s| s.subject == member) {
                *counts.entry(s.kind.as_str()).or_insert(0) += 1;
            }
            let mut row = json!({ "member": member, "counts": counts });
            if let Some(k) = keys.iter().find(|k| k.member == member) {
                let d = tier(&member_facts(cfg, k, &stamps), &stamps, rules, now_sec);
                row["tier"] = json!(tier_from_table(conn, &member, None));
                row["missing"] = json!(d.missing);
                row["reasons"] = json!(d.reasons);
                // The ids that `standing clear` needs.
                let open: Vec<Value> = stamps
                    .iter()
                    .filter(|p| p.subject == member)
                    .filter(|p| matches!(p.kind, StampKind::Reverted | StampKind::ReviewMissed))
                    .filter(|p| {
                        !stamps
                            .iter()
                            .any(|c| c.kind == StampKind::Cleared && c.evidence_msg == p.msg_id)
                    })
                    .map(|p| json!({ "id": p.id, "kind": p.kind.as_str() }))
                    .collect();
                row["open_penalties"] = json!(open);
            }
            row
        })
        .collect();
    Some(rows)
}

// The requests of maintainers

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str, String> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("the request needs the text field `{field}`"))
}

fn cause_of(body: &Value) -> Result<String, String> {
    let c = text(body, "cause")?;
    if c.chars().count() > MAX_CAUSE || c.chars().any(char::is_control) {
        return Err(format!(
            "`cause` is one line of at most {MAX_CAUSE} characters"
        ));
    }
    Ok(c.to_owned())
}

/// An orchestrator, a member the config lists (a token name or a member id), or a key member that
/// holds tier 3.
pub(crate) fn is_maintainer(ctx: &OpContext<'_>) -> bool {
    ctx.caller.role == Role::Orchestrator
        || ctx.config().stamp.is_listed_maintainer(&ctx.caller.name)
        || (ctx.caller.trust == crate::key::ops::TRUST
            && tier_from_table(ctx.store.connection(), &ctx.caller.name, None) == 3)
}

/// Handles the requests `stamp.*`. Only a maintainer may use them: an orchestrator token, a token or
/// a key that the settings list as a maintainer.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    if !matches!(
        op,
        "stamp.revert" | "stamp.grant" | "stamp.clear" | "stamp.intent"
    ) {
        return Err(format!(
            "unknown op `{op}` (stamp.revert, stamp.grant, stamp.clear, stamp.intent)"
        ));
    }
    if !is_maintainer(ctx) {
        return Err(format!(
            "{op} is for maintainers: an orchestrator token or a member that the settings list as a maintainer"
        ));
    }
    match op {
        "stamp.revert" => revert(ctx, body),
        "stamp.grant" => grant(ctx, body),
        "stamp.clear" => clear(ctx, body),
        _ => intent(ctx, body),
    }
}

/// Checks a stamp as the projection will, so a refusal is an error now and not a silent drop later.
fn checked(msg: Message) -> Result<Message, String> {
    validate_stamp(&msg.body, true)?;
    Ok(msg)
}

fn emit_all(ctx: &mut OpContext<'_>, msgs: Vec<Message>) -> Result<Vec<u64>, String> {
    let mut ids = Vec::new();
    for m in msgs {
        ids.push(ctx.emit(checked(m)?)?);
    }
    Ok(ids)
}

fn now_sec(ctx: &OpContext<'_>) -> u64 {
    ctx.now_ms / 1000
}

fn key_row_of(ctx: &OpContext<'_>, member: &str) -> Result<KeyRow, String> {
    key_rows(ctx.store.connection())
        .into_iter()
        .find(|k| k.member == member)
        .ok_or_else(|| format!("`{member}` has no key on this hub"))
}

fn revert(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let sha = text(body, "submission")?.to_owned();
    let cause = cause_of(body)?;
    let issuer = ctx.caller.name.clone();
    let at = now_sec(ctx);
    let conn = ctx.store.connection();
    let sub = submissions(conn)
        .into_iter()
        .find(|s| s.sha == sha)
        .ok_or_else(|| format!("no submission `{sha}` is known"))?;
    if sub.state != "landed" {
        return Err(format!(
            "submission `{sha}` is `{}`, only a landed one can be reverted",
            sub.state
        ));
    }
    let stamps = query_stamps(conn)?;
    if stamps
        .iter()
        .any(|s| s.kind == StampKind::Reverted && s.evidence_ref == sha)
    {
        return Err(format!("submission `{sha}` is reverted already"));
    }
    let mut msgs = vec![stamp_message(
        StampKind::Reverted,
        &sub.member,
        &issuer,
        sub.msg_id,
        &sub.head,
        &sha,
        at,
        Some(&cause),
    )];
    let mut seen = BTreeSet::new();
    for r in query_reviews(conn)? {
        let passed = r.submission == sha && r.state == "answered" && r.verdict == "pass";
        // A maintainer does not stamp itself, and a reviewer counts once.
        if passed
            && r.reviewer != issuer
            && !r.reviewer.is_empty()
            && seen.insert(r.reviewer.clone())
        {
            msgs.push(stamp_message(
                StampKind::ReviewMissed,
                &r.reviewer,
                &issuer,
                r.msg_id.max(1),
                &sub.head,
                &sha,
                at,
                Some(&cause),
            ));
        }
    }
    let reviewers: Vec<String> = seen.into_iter().collect();
    let ids = emit_all(ctx, msgs)?;
    let mut out = Map::new();
    out.insert("author".into(), sub.member.into());
    out.insert("review_missed".into(), json!(reviewers));
    out.insert("ids".into(), json!(ids));
    Ok(out)
}

fn grant(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let member = text(body, "member")?.to_owned();
    let key = key_row_of(ctx, &member)?;
    if key.state == KeyState::Revoked {
        return Err(format!("the key of `{member}` is revoked"));
    }
    let stamps = query_stamps(ctx.store.connection())?;
    if stamps
        .iter()
        .any(|s| s.kind == StampKind::GrantTier2 && s.subject == member)
    {
        return Err(format!("`{member}` holds a tier 2 grant already"));
    }
    let msg = stamp_message(
        StampKind::GrantTier2,
        &member,
        &ctx.caller.name.clone(),
        key.msg_id.max(1),
        "",
        "",
        now_sec(ctx),
        None,
    );
    let ids = emit_all(ctx, vec![msg])?;
    let mut out = Map::new();
    out.insert("member".into(), member.into());
    out.insert("ids".into(), json!(ids));
    Ok(out)
}

fn clear(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let member = text(body, "member")?.to_owned();
    let id = body
        .get("stamp")
        .and_then(Value::as_u64)
        .ok_or("the request needs `stamp`, the id of the penalty stamp as a number")?;
    let stamps = query_stamps(ctx.store.connection())?;
    let penalty = stamps
        .iter()
        .find(|s| s.id == id && s.subject == member)
        .ok_or_else(|| format!("`{member}` has no stamp with id {id}"))?;
    if !matches!(penalty.kind, StampKind::Reverted | StampKind::ReviewMissed) {
        return Err(format!(
            "stamp {id} is `{}`, only `reverted` and `review_missed` can be cleared",
            penalty.kind.as_str()
        ));
    }
    if stamps
        .iter()
        .any(|s| s.kind == StampKind::Cleared && s.evidence_msg == penalty.msg_id)
    {
        return Err(format!("stamp {id} is cleared already"));
    }
    let msg = stamp_message(
        StampKind::Cleared,
        &member,
        &ctx.caller.name.clone(),
        penalty.msg_id,
        "",
        "",
        now_sec(ctx).max(penalty.at),
        None,
    );
    let ids = emit_all(ctx, vec![msg])?;
    let mut out = Map::new();
    out.insert("member".into(), member.into());
    out.insert("ids".into(), json!(ids));
    Ok(out)
}

fn intent(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let member = text(body, "member")?.to_owned();
    let cause = cause_of(body)?;
    let key = key_row_of(ctx, &member)?;
    let evidence = body
        .get("msg")
        .and_then(Value::as_u64)
        .filter(|m| *m > 0)
        .unwrap_or(key.msg_id.max(1));
    let issuer = ctx.caller.name.clone();
    let msg = stamp_message(
        StampKind::Intent,
        &member,
        &issuer,
        evidence,
        "",
        "",
        now_sec(ctx),
        Some(&cause),
    );
    let ids = emit_all(ctx, vec![msg])?;
    // The key module revokes for an orchestrator. The caller is a maintainer here, so it acts as one
    // for this single call and gets its own identity back.
    let (role, trust) = (ctx.caller.role, ctx.caller.trust);
    ctx.caller.role = Role::Orchestrator;
    ctx.caller.trust = "owner";
    let revoked = crate::key::ops::on_op(
        ctx,
        "key.revoke",
        &json!({ "member": member, "reason": format!("intent: {cause}") }),
    );
    ctx.caller.role = role;
    ctx.caller.trust = trust;
    let mut out = Map::new();
    out.insert("member".into(), member.into());
    out.insert("ids".into(), json!(ids));
    out.insert("revoked".into(), json!(revoked.is_ok()));
    if let Err(e) = revoked {
        out.insert("revoke_note".into(), e.into());
    }
    Ok(out)
}
