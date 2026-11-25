//! Hub side of the test network: run orders for testers, taking reports and the matrix.
//!
//! See `docs/factory.md` section 9. Testers are not leased workers: they have no model and no lease,
//! so run orders are their own small table (RUN_ORDER) and never touch the task queue.
//!
//! Requests:
//!
//! * `test.order` (orchestrator): creates a run order. `test.close` (orchestrator): closes one.
//! * `test.next` (any member, tier 0 too): the best open order for a machine, or none.
//! * `report.send` (any member): a signed report for an open order.
//!
//! Only FACTs written by the hub count for `report/<run>` and `runorder/<id>`. On every tick the
//! hub settles orders whose reports agree, closes orders that stayed open too long and raises
//! MATRIX_REGRESSION when a profile and machine class that worked on an older build fails on a
//! newer one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::orders::{
    self, MAX_WANTED, Needs, OrderRow, check_order_fields, query_orders, report_counts, reported_by,
};
use super::types::{MachineClass, Outcome, Report};
use super::{
    Cell, CellState, QuorumRules, ReportLimits, ReportView, cell, query_reports, regressions,
    validate_report,
};
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::key::book::{KeyState, current_row};
use crate::key::crypto::{unb64, verify_request};
use crate::msg::{Kind, Message, Role};
use crate::time::rfc3339;

/// Name of the alert that a regression raises.
pub const MATRIX_REGRESSION: &str = "MATRIX_REGRESSION";

/// Settings of this part of the hub. Lives in `HubConfig.test`. Everything is off by default.
#[derive(Clone, Debug)]
pub struct TestConfig {
    /// The test network. While it is off the requests answer with one line and the tick does nothing.
    pub enabled: bool,
    /// Independent owners that settle a cell. A run order may want its own number.
    pub rules: QuorumRules,
    /// What a report may hold. The default log excerpt is smaller than the 16 KiB of the wave one
    /// limit, because a message body of the hub is at most 4096 bytes.
    pub limits: ReportLimits,
    /// How many open orders a member may hold at once: orders handed to it by `test.next` and not
    /// answered yet. At the limit `test.next` hands out only orders it already handed.
    pub max_handed_per_member: usize,
    /// How long a handed order counts against that limit, in seconds.
    pub hand_hold_secs: u64,
    /// How long an order stays open, in seconds. Then the tick closes it.
    pub order_ttl_secs: u64,
    /// Most open orders at one time.
    pub max_open_orders: usize,
    /// Test hook: address blocks by member. `door::block_of` answers None in the first wave two
    /// build, so tests name the blocks here. A member in this table uses its entry before the door.
    pub blocks: Arc<Mutex<BTreeMap<String, String>>>,
    /// Orders handed out and not answered: (member, order) to the time of the hand out in ms.
    /// Memory only, a restart forgets it.
    pub handed: Arc<Mutex<BTreeMap<(String, String), u64>>>,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig {
            enabled: false,
            rules: QuorumRules::default(),
            limits: ReportLimits {
                max_log_bytes: 2048,
                ..ReportLimits::default()
            },
            max_handed_per_member: 2,
            hand_hold_secs: 3600,
            order_ttl_secs: 7 * 24 * 3600,
            max_open_orders: 200,
            blocks: Arc::new(Mutex::new(BTreeMap::new())),
            handed: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hub_fact(topic: String, body: String, now_ms: u64) -> Message {
    Message {
        id: 0,
        ts: rfc3339(now_ms),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic,
        body,
        refs: vec![],
        to: vec![],
    }
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str, String> {
    body.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the request needs the text field `{field}`"))
}

fn need_enabled(cfg: &TestConfig) -> Result<(), String> {
    if cfg.enabled {
        Ok(())
    } else {
        Err("the test network is switched off on this hub".into())
    }
}

/// Handles the requests `report.*` and `test.*`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let cfg = ctx.config().test.clone();
    on_op_with(ctx, op, body, &cfg)
}

/// [`on_op`] with the settings given.
pub fn on_op_with(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
    cfg: &TestConfig,
) -> Result<Map<String, Value>, String> {
    match op {
        "test.order" => order(ctx, body, cfg),
        "test.close" => close(ctx, body, cfg),
        "test.next" => next(ctx, body, cfg),
        "report.send" => send(ctx, body, cfg),
        other => Err(format!(
            "unknown op `{other}` (test.order, test.close, test.next, report.send)"
        )),
    }
}

fn need_orchestrator(ctx: &OpContext<'_>, op: &str) -> Result<(), String> {
    if ctx.caller.role == Role::Orchestrator {
        Ok(())
    } else {
        Err(format!("{op} needs an orchestrator token"))
    }
}

fn order(
    ctx: &mut OpContext<'_>,
    body: &Value,
    cfg: &TestConfig,
) -> Result<Map<String, Value>, String> {
    need_orchestrator(ctx, "test.order")?;
    need_enabled(cfg)?;
    let build = text(body, "build")?.to_owned();
    let channel = text(body, "channel")?.to_owned();
    let profile = text(body, "profile")?.to_owned();
    let scenario = text(body, "scenario")?.to_owned();
    check_order_fields(&build, &channel, &profile, &scenario, &cfg.limits.channels)?;
    let needs = match body.get("needs") {
        None | Some(Value::Null) => Needs::default(),
        Some(v) => Needs::from_json(v)?,
    };
    let wanted = match body.get("wanted") {
        None | Some(Value::Null) => cfg.rules.quorum.clamp(1, MAX_WANTED),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_WANTED as u64).contains(n))
            .ok_or_else(|| format!("`wanted` is a number from 1 to {MAX_WANTED}"))?
            as usize,
    };
    let existing = query_orders(ctx.store.connection())?;
    let open = existing.iter().filter(|o| o.state == "open");
    if let Some(same) = open.clone().find(|o| {
        o.build == build && o.profile == profile && o.scenario == scenario && o.needs == needs
    }) {
        return Err(format!(
            "this order is open already as `{}`: close it first or wait for it to settle",
            same.id
        ));
    }
    if open.count() >= cfg.max_open_orders {
        return Err(format!(
            "{} orders are open: close some before you add more",
            cfg.max_open_orders
        ));
    }
    let mut n = existing.len() + 1;
    let id = loop {
        let id = format!("ro{n}");
        if !existing.iter().any(|o| o.id == id) {
            break id;
        }
        n += 1;
    };
    let row = OrderRow {
        id: id.clone(),
        build,
        channel,
        profile,
        scenario,
        needs,
        wanted,
        state: "open".into(),
        created_at: ctx.now_ms / 1000,
        msg_id: 0,
    };
    let msg_id = ctx.emit(hub_fact(
        format!("runorder/{id}"),
        row.fact_body("open"),
        ctx.now_ms,
    ))?;
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("msg".into(), json!(msg_id));
    Ok(out)
}

fn close(
    ctx: &mut OpContext<'_>,
    body: &Value,
    cfg: &TestConfig,
) -> Result<Map<String, Value>, String> {
    need_orchestrator(ctx, "test.close")?;
    need_enabled(cfg)?;
    let id = text(body, "id")?.to_owned();
    let row = query_orders(ctx.store.connection())?
        .into_iter()
        .find(|o| o.id == id)
        .ok_or_else(|| format!("no run order `{id}`"))?;
    if row.state == "closed" {
        return Err(format!("run order `{id}` is closed already"));
    }
    let msg_id = ctx.emit(hub_fact(
        format!("runorder/{id}"),
        row.fact_body("closed"),
        ctx.now_ms,
    ))?;
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("msg".into(), json!(msg_id));
    Ok(out)
}

fn next(
    ctx: &mut OpContext<'_>,
    body: &Value,
    cfg: &TestConfig,
) -> Result<Map<String, Value>, String> {
    need_enabled(cfg)?;
    let machine: MachineClass = serde_json::from_value(
        body.get("machine")
            .cloned()
            .ok_or("the request needs `machine`, the machine class of the tester")?,
    )
    .map_err(|e| format!("`machine` is not a machine class: {e}"))?;
    let profiles: Vec<String> = match body.get("profiles") {
        Some(Value::Array(a)) if a.len() <= 64 => a
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_owned)
                    .ok_or("`profiles` is a list of profile ids")
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("the request needs `profiles`, a list of at most 64 profile ids".into()),
    };
    let member = ctx.caller.name.clone();
    let conn = ctx.store.connection();
    let all = query_orders(conn)?;
    let counts = report_counts(conn)?;
    let reported = reported_by(conn, &member)?;
    let mut candidates = orders::candidates(&all, &counts, &reported, &machine, &profiles);

    let now = ctx.now_ms;
    let mut handed = lock(&cfg.handed);
    handed.retain(|(m, o), at| {
        m != &member
            || (now.saturating_sub(*at) <= cfg.hand_hold_secs * 1000
                && all.iter().any(|x| &x.id == o && x.state == "open")
                && !reported.contains(o))
    });
    let mine: BTreeSet<String> = handed
        .keys()
        .filter(|(m, _)| m == &member)
        .map(|(_, o)| o.clone())
        .collect();
    if mine.len() >= cfg.max_handed_per_member.max(1) {
        candidates.retain(|o| mine.contains(&o.id));
    }
    let mut out = Map::new();
    match candidates.first() {
        Some(o) => {
            handed.entry((member, o.id.clone())).or_insert(now);
            out.insert(
                "order".into(),
                json!({
                    "id": o.id,
                    "build": o.build,
                    "channel": o.channel,
                    "profile": o.profile,
                    "scenario": o.scenario,
                    "wanted": o.wanted,
                }),
            );
        }
        None => {
            out.insert("order".into(), Value::Null);
            out.insert("reason".into(), json!("nothing to test now"));
        }
    }
    Ok(out)
}

fn send(
    ctx: &mut OpContext<'_>,
    body: &Value,
    cfg: &TestConfig,
) -> Result<Map<String, Value>, String> {
    need_enabled(cfg)?;
    let raw = body
        .get("report")
        .filter(|r| r.is_object())
        .ok_or("the request needs `report`, the report as an object")?;
    let report: Report = serde_json::from_value(raw.clone())
        .map_err(|e| format!("`report` is not a test report: {e}"))?;
    let member = ctx.caller.name.clone();
    if report.member != member {
        return Err("report.member must be the member who sends it".into());
    }
    // A key member signs, a token member does not need to.
    if ctx.caller.trust == crate::key::ops::TRUST {
        let (public, _, _, state) = current_row(ctx.store.connection(), &member)?
            .ok_or_else(|| format!("no key is registered for `{member}`"))?;
        if state != KeyState::Active {
            return Err("this key is not active".into());
        }
        if raw.get("sig").and_then(Value::as_str).is_none() {
            return Err(
                "report.sig is missing: sign the report with your key (`sommelier test` does)"
                    .into(),
            );
        }
        let public = unb64(&public).map_err(|_| "the stored key is damaged".to_string())?;
        verify_request(raw, &public).map_err(|e| format!("report.sig is wrong: {e}"))?;
    }
    let conn = ctx.store.connection();
    let order = query_orders(conn)?
        .into_iter()
        .find(|o| o.id == report.run)
        .ok_or_else(|| {
            format!(
                "no run order `{}`",
                report.run.chars().take(64).collect::<String>()
            )
        })?;
    if order.state != "open" {
        return Err(format!(
            "run order `{}` is {}, it takes no more reports",
            order.id, order.state
        ));
    }
    if report.build != order.build
        || report.channel != order.channel
        || report.profile != order.profile
        || report.scenario != order.scenario
    {
        return Err(format!(
            "the report does not answer run order `{}`: build, channel, profile and scenario must match it",
            order.id
        ));
    }
    if reported_by(conn, &member)?.contains(&order.id) {
        return Err(format!(
            "you reported on run order `{}` already, one report per order",
            order.id
        ));
    }
    validate_report(&report, &cfg.limits)?;
    let line = serde_json::to_string(&report).map_err(|e| e.to_string())?;
    let id = ctx.emit(hub_fact(format!("report/{}", report.run), line, ctx.now_ms))?;
    lock(&cfg.handed).remove(&(member, order.id.clone()));
    let mut out = Map::new();
    out.insert("run".into(), json!(order.id));
    out.insert("id".into(), json!(id));
    Ok(out)
}

/// The opaque address block of a member: the test table first, then the front door.
fn block_for(hub: &Hub, cfg: &TestConfig, member: &str) -> Option<String> {
    if let Some(b) = lock(&cfg.blocks).get(member) {
        return Some(b.clone());
    }
    crate::door::block_of(hub, member)
}

/// What the tick reads from the store in one go.
struct Snapshot {
    orders: Vec<OrderRow>,
    reports: Vec<super::ReportRow>,
    raised: BTreeSet<String>,
}

fn snapshot(conn: &rusqlite::Connection) -> Result<Snapshot, String> {
    let mut stmt = conn
        .prepare("SELECT key FROM ALERT WHERE code = ?1")
        .map_err(|e| e.to_string())?;
    let raised = stmt
        .query_map([MATRIX_REGRESSION], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(Snapshot {
        orders: query_orders(conn)?,
        reports: query_reports(conn)?,
        raised,
    })
}

fn outcome_of(s: &str) -> Option<Outcome> {
    match s {
        "works" => Some(Outcome::Works),
        "fails" => Some(Outcome::Fails),
        "inconclusive" => Some(Outcome::Inconclusive),
        _ => None,
    }
}

fn plain_key(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(max)
        .collect()
}

/// Called on every hub tick. Returns messages for the hub to append.
///
/// It reads the tables once, asks the front door for the blocks, and computes in memory. Nothing
/// slow runs under the state lock.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let cfg = &hub.config().test;
    if !cfg.enabled {
        return Vec::new();
    }
    let Some(Ok(snap)) = hub.with_store(|s| snapshot(s.connection())) else {
        return Vec::new();
    };
    let mut blocks: BTreeMap<String, Option<String>> = BTreeMap::new();
    for r in &snap.reports {
        blocks
            .entry(r.member.clone())
            .or_insert_with(|| block_for(hub, cfg, &r.member));
    }
    plan_tick(&snap, &blocks, cfg, now_ms)
}

fn view(r: &super::ReportRow, blocks: &BTreeMap<String, Option<String>>) -> Option<ReportView> {
    Some(ReportView {
        member: r.member.clone(),
        tier: 0,
        block: blocks.get(&r.member).cloned().flatten(),
        outcome: outcome_of(&r.outcome)?,
        at: r.at,
    })
}

fn plan_tick(
    snap: &Snapshot,
    blocks: &BTreeMap<String, Option<String>>,
    cfg: &TestConfig,
    now_ms: u64,
) -> Vec<Message> {
    let now_s = now_ms / 1000;
    let mut out = Vec::new();

    // Orders: expire the old ones, settle the ones whose reports agree.
    for o in snap.orders.iter().filter(|o| o.state == "open") {
        if now_s.saturating_sub(o.created_at) > cfg.order_ttl_secs {
            out.push(hub_fact(
                format!("runorder/{}", o.id),
                o.fact_body("closed"),
                now_ms,
            ));
            continue;
        }
        let rules = QuorumRules { quorum: o.wanted };
        let mut by_machine: BTreeMap<&str, Vec<ReportView>> = BTreeMap::new();
        for r in snap.reports.iter().filter(|r| r.run == o.id) {
            if let Some(v) = view(r, blocks) {
                by_machine.entry(&r.machine_key).or_default().push(v);
            }
        }
        let settled = by_machine.values().any(|views| {
            matches!(
                cell(views, &rules).state,
                CellState::Works | CellState::Fails | CellState::Mixed
            )
        });
        if settled {
            out.push(hub_fact(
                format!("runorder/{}", o.id),
                o.fact_body("settled"),
                now_ms,
            ));
        }
    }

    // Regressions. The order of builds is the order of their first run order.
    let mut first_order: BTreeMap<&str, u64> = BTreeMap::new();
    for o in &snap.orders {
        let e = first_order.entry(o.build.as_str()).or_insert(o.msg_id);
        *e = (*e).min(o.msg_id);
    }
    let mut commits: Vec<&str> = first_order.keys().copied().collect();
    commits.sort_by_key(|c| (first_order[c], *c));
    let commits: Vec<String> = commits.into_iter().map(str::to_owned).collect();

    type Group<'a> = BTreeMap<(&'a str, &'a str), BTreeMap<&'a str, Vec<ReportView>>>;
    let mut groups: Group<'_> = BTreeMap::new();
    for r in &snap.reports {
        if !first_order.contains_key(r.build.as_str()) {
            continue;
        }
        if let Some(v) = view(r, blocks) {
            groups
                .entry((r.profile.as_str(), r.machine_key.as_str()))
                .or_default()
                .entry(r.build.as_str())
                .or_default()
                .push(v);
        }
    }
    let mut raised = snap.raised.clone();
    for ((profile, mkey), by_build) in &groups {
        let cells: BTreeMap<String, Cell> = by_build
            .iter()
            .map(|(b, views)| (b.to_string(), cell(views, &cfg.rules)))
            .collect();
        for reg in regressions(&commits, &cells) {
            let key = format!(
                "{}.{}.{}",
                plain_key(profile, 30),
                plain_key(mkey, 40),
                plain_key(&reg.first_bad, 20)
            );
            if !raised.insert(key.clone()) {
                continue;
            }
            let text = format!(
                "{profile} on {mkey} worked at {} and fails at {}",
                reg.last_good, reg.first_bad
            );
            out.push(Message {
                id: 0,
                ts: rfc3339(now_ms),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/{MATRIX_REGRESSION}/{key}"),
                body: json!({
                    "v": 1,
                    "code": MATRIX_REGRESSION,
                    "key": key,
                    "severity": "warn",
                    "state": "open",
                    "opened_at": now_s,
                    "last_at": now_s,
                    "count": 1,
                    "text": text,
                    "data": {
                        "profile": profile,
                        "machine_key": mkey,
                        "last_good": reg.last_good,
                        "first_bad": reg.first_bad,
                    },
                })
                .to_string(),
                refs: vec!["state:open".into()],
                to: vec![],
            });
        }
    }
    out
}
