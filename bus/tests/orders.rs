//! The order fields of the factory and a scheduler that respects tiers (docs/factory.md 5.1 and 6).
//!
//! Covers `task add` (computed risk, forced min_trust, refusals), replay of logs from before the fields,
//! the tier rule of `plan` (golden scenarios in `tests/fixtures/orders`, determinism, explain text),
//! the invariant, and the loading of tiers from the TIER table.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sommelier::hooks::{self, OpCaller, OpContext};
use sommelier::msg::{Kind, Message, Role};
use sommelier::plan::{
    PlanAction, PlanCapacity, PlanMember, PlanMode, PlanPolicy, PlanState, PlanStateInput,
    PlanTask, check_invariants, load_plan_state_from_store, plan, plan_explain,
};
use sommelier::store::Store;
use sommelier::task::{init_schema, query_task, query_tasks};
use sommelier::time::rfc3339;

const NOW_MS: u64 = 1_795_600_000_000;
const NOW: u64 = NOW_MS / 1000;

// ---------------------------------------------------------------------------------------------
// task add
// ---------------------------------------------------------------------------------------------

struct Hub {
    store: Store,
    next_id: u64,
    log: Vec<Message>,
}

impl Hub {
    fn new() -> Self {
        Self {
            store: Store::open_in_memory().unwrap(),
            next_id: 1,
            log: Vec::new(),
        }
    }

    fn op(&mut self, op: &str, body: &Value) -> Result<Map<String, Value>, String> {
        let caller = OpCaller {
            name: "coord".into(),
            role: Role::Orchestrator,
            trust: "owner",
        };
        let log = &mut self.log;
        let next_id = &mut self.next_id;
        let mut emit = |st: &mut Store, mut msg: Message| -> Result<u64, String> {
            msg.id = *next_id;
            *next_id += 1;
            if msg.ts.is_empty() {
                msg.ts = rfc3339(NOW_MS);
            }
            st.apply(&msg)?;
            log.push(msg);
            Ok(*next_id - 1)
        };
        let mut ctx = OpContext::new(caller, &mut self.store, NOW_MS, &mut emit);
        hooks::on_op(&mut ctx, op, body)
    }

    fn add(&mut self, id: &str, scope: &str, extra: Value) -> Result<Map<String, Value>, String> {
        let mut body = json!({
            "id": id,
            "title": format!("order {id}"),
            "kind": "fix",
            "scope": scope,
            "done_when": "cargo test",
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.op("task.add", &body)
    }
}

fn task_of(out: &Map<String, Value>) -> &Value {
    &out["task"]
}

#[test]
fn a_plain_scope_is_normal_and_keeps_min_trust_two() {
    let mut hub = Hub::new();
    let out = hub.add("t1", "src/a.rs", json!({})).unwrap();
    assert_eq!(task_of(&out)["risk"], "normal");
    assert_eq!(task_of(&out)["min_trust"], 2);
    assert_eq!(task_of(&out)["review"], "default");
    assert_eq!(task_of(&out)["credit"], "handle");
}

#[test]
fn risk_is_computed_from_the_scope_and_never_declared() {
    let mut hub = Hub::new();
    // The caller says normal, the scope meets a hold path.
    let out = hub
        .add("g1", "bus/src/x.rs", json!({"risk": "normal"}))
        .unwrap();
    assert_eq!(task_of(&out)["risk"], "guarded");
    // A wide scope can reach a script.
    let out = hub.add("g2", "docs/**", json!({})).unwrap();
    assert_eq!(task_of(&out)["risk"], "guarded");
    // A narrow scope away from every hold path may be asked low.
    let out = hub
        .add("l1", "src/ui/button.rs", json!({"risk": "low"}))
        .unwrap();
    assert_eq!(task_of(&out)["risk"], "low");
    // Asking for guarded on a plain scope is allowed: it only makes the order stricter.
    let out = hub
        .add("g3", "src/ui/other.rs", json!({"risk": "guarded"}))
        .unwrap();
    assert_eq!(task_of(&out)["risk"], "guarded");
    assert_eq!(task_of(&out)["min_trust"], 2);
}

#[test]
fn guarded_forces_min_trust_to_two_at_least() {
    let mut hub = Hub::new();
    let out = hub
        .add("g1", "swarm/run.sh", json!({"min_trust": 1}))
        .unwrap();
    assert_eq!(task_of(&out)["risk"], "guarded");
    assert_eq!(task_of(&out)["min_trust"], 2);
    assert!(out["notes"][0].as_str().unwrap().contains("min_trust"));
    let out = hub
        .add("g2", "swarm/run.sh", json!({"min_trust": 3}))
        .unwrap();
    assert_eq!(task_of(&out)["min_trust"], 3, "a higher value is kept");
}

#[test]
fn refusals_say_why() {
    let mut hub = Hub::new();
    let e = hub
        .add("r1", "bus/src/x.rs", json!({"risk": "low"}))
        .unwrap_err();
    assert!(
        e.contains("risk low refused") && e.contains("hold path `bus/**`"),
        "{e}"
    );
    let e = hub
        .add("r2", "src/a.rs", json!({"min_trust": 0}))
        .unwrap_err();
    assert!(
        e.contains("min_trust 0 refused") && e.contains("its risk is normal"),
        "{e}"
    );
    let e = hub
        .add("r3", "src/a.rs", json!({"min_trust": 0, "risk": "low"}))
        .unwrap_err();
    assert!(e.contains("size is not small"), "{e}");
    let e = hub
        .add(
            "r4",
            "swarm/a.rs",
            json!({"min_trust": 0, "risk": "low", "size": "small"}),
        )
        .unwrap_err();
    assert!(e.contains("risk low refused"), "{e}");
    let e = hub
        .add("r5", "swarm/a.rs", json!({"min_trust": 0, "size": "small"}))
        .unwrap_err();
    assert!(e.contains("its risk is guarded"), "{e}");
    let e = hub
        .add("r6", "src/a.rs", json!({"risk": "huge"}))
        .unwrap_err();
    assert!(e.contains("risk must be low, normal or guarded"), "{e}");
    let e = hub
        .add("r7", "src/a.rs", json!({"min_trust": 4}))
        .unwrap_err();
    assert!(e.contains("min_trust must be a number from 0 to 3"), "{e}");
    let e = hub
        .add("r8", "src/a.rs", json!({"credit": "all"}))
        .unwrap_err();
    assert!(e.contains("credit must be handle or none"), "{e}");
    assert!(
        query_tasks(hub.store.connection()).unwrap().is_empty(),
        "a refused order leaves nothing behind"
    );
}

#[test]
fn a_tier_zero_order_needs_low_and_small_and_is_accepted_then() {
    let mut hub = Hub::new();
    let out = hub
        .add(
            "z1",
            "src/ui/button.rs",
            json!({"min_trust": 0, "risk": "low", "size": "small", "credit": "none", "review": "newcomer"}),
        )
        .unwrap();
    let t = task_of(&out);
    assert_eq!(t["min_trust"], 0);
    assert_eq!(t["risk"], "low");
    assert_eq!(t["credit"], "none");
    assert_eq!(t["review"], "newcomer");
    let row = query_task(hub.store.connection(), "z1").unwrap().unwrap();
    assert_eq!((row.min_trust, row.risk.as_str()), (0, "low"));
}

#[test]
fn replay_of_the_log_rebuilds_the_order_fields() {
    let mut hub = Hub::new();
    hub.add(
        "t1",
        "src/a.rs",
        json!({"risk": "low", "min_trust": 1, "credit": "none"}),
    )
    .unwrap();
    hub.add("t2", "bus/x.rs", json!({"review": "strict"}))
        .unwrap();
    let before = query_tasks(hub.store.connection()).unwrap();
    let mut fresh = Store::open_in_memory().unwrap();
    for m in &hub.log {
        fresh.apply(m).unwrap();
    }
    assert_eq!(query_tasks(fresh.connection()).unwrap(), before);
    assert_eq!(before[1].risk, "guarded");
    assert_eq!(before[1].review, "strict");
}

#[test]
fn a_log_without_the_new_fields_replays_to_normal_and_trust_two() {
    // A line of the old shape: a task_row with none of the four fields.
    let old = json!({
        "v": 1, "task": "old1", "prev": "", "state": "ready", "attempt": 1,
        "by": "coord", "reason": "added",
        "task_row": {
            "id": "old1", "title": "old order", "kind": "fix", "priority": "normal",
            "state": "ready", "scope": "src/a.rs", "done_when": "cargo test"
        }
    });
    let msg = Message {
        id: 1,
        ts: rfc3339(NOW_MS),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::TaskUpdate,
        topic: "task/old1".into(),
        body: old.to_string(),
        refs: vec![],
        to: vec![],
    };
    let mut store = Store::open_in_memory().unwrap();
    store.apply(&msg).unwrap();
    let row = query_task(store.connection(), "old1").unwrap().unwrap();
    assert_eq!(row.risk, "normal");
    assert_eq!(row.min_trust, 2);
    assert_eq!(row.review, "default");
    assert_eq!(row.credit, "handle");
}

#[test]
fn a_state_db_made_before_the_fields_opens_and_reads_defaults() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // The TASK table as it was, and no TASK_ORDER table.
    let old_schema = sommelier::task::TASK_SCHEMA_SQL
        .split("-- The order fields")
        .next()
        .unwrap()
        .to_owned();
    conn.execute_batch(&old_schema).unwrap();
    conn.execute("INSERT INTO TASK (id, title) VALUES ('old', 'x')", [])
        .unwrap();
    init_schema(&conn).unwrap();
    let row = query_task(&conn, "old").unwrap().unwrap();
    assert_eq!((row.risk.as_str(), row.min_trust), ("normal", 2));
}

// ---------------------------------------------------------------------------------------------
// the scheduler
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct Golden {
    name: String,
    now: u64,
    #[serde(default)]
    policy: PlanPolicy,
    state: PlanStateInput,
    expected_actions: Vec<PlanAction>,
    #[serde(default)]
    expected_wait_reasons: BTreeMap<String, String>,
}

fn goldens() -> Vec<Golden> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("orders");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
        .collect()
}

#[test]
fn golden_scenarios_of_the_tier_rules() {
    let all = goldens();
    assert_eq!(all.len(), 4, "four scenarios in tests/fixtures/orders");
    for sc in &all {
        let out = plan(&PlanState::from_input(sc.state.clone()), &sc.policy, sc.now);
        assert!(
            out.invariant_violations.is_empty(),
            "{}: {:?}",
            sc.name,
            out.invariant_violations
        );
        assert_eq!(out.actions, sc.expected_actions, "{} actions", sc.name);
        assert_eq!(
            out.wait_reasons, sc.expected_wait_reasons,
            "{} wait reasons",
            sc.name
        );
    }
}

fn policy() -> PlanPolicy {
    PlanPolicy {
        mode: PlanMode::Enforce,
        ..PlanPolicy::default()
    }
}

fn member(name: &str, trust: &str, tier: Option<i64>) -> PlanMember {
    PlanMember {
        name: name.into(),
        trust: trust.into(),
        online: true,
        tier,
    }
}

fn capacity(owner: &str, slots: u32) -> PlanCapacity {
    PlanCapacity {
        owner: owner.into(),
        machine: "box".into(),
        class: "codex".into(),
        mode: "auto".into(),
        useful_parallel: slots,
        accepts: ["fix".to_owned()].into_iter().collect(),
        pools: vec!["codex.5h".into()],
        family: None,
        free_since: None,
    }
}

fn order(id: &str, risk: &str, min_trust: u8, size: f64) -> PlanTask {
    PlanTask {
        id: id.into(),
        title: id.into(),
        kind: "fix".into(),
        priority: "normal".into(),
        state: "ready".into(),
        track: String::new(),
        scope: format!("src/{id}/*"),
        accepts: vec!["codex".into()],
        min_tier: String::new(),
        branch: String::new(),
        done_when: "ok".into(),
        deps: vec![],
        size,
        checkpointable: true,
        attempt: 1,
        max_attempts: 3,
        resume_note: String::new(),
        exclude_pools_until: BTreeMap::new(),
        created_at: NOW,
        ready_at: NOW,
        closed_at: 0,
        risk: risk.into(),
        min_trust,
    }
}

fn leased(out: &sommelier::plan::PlanOutput) -> Vec<(String, String)> {
    out.actions
        .iter()
        .filter_map(|a| match a {
            PlanAction::Lease { task_id, owner, .. } => Some((task_id.clone(), owner.clone())),
            _ => None,
        })
        .collect()
}

fn run(input: PlanStateInput) -> sommelier::plan::PlanOutput {
    plan(&PlanState::from_input(input), &policy(), NOW)
}

#[test]
fn a_tier_zero_slot_is_never_offered_a_min_trust_one_order() {
    for switch in [false, true] {
        let out = run(PlanStateInput {
            members: vec![member("newbie", "guest", Some(0))],
            capacities: vec![capacity("newbie", 2)],
            tasks: vec![order("a", "normal", 1, 0.5), order("b", "low", 1, 0.5)],
            tier0_coding: switch,
            ..PlanStateInput::default()
        });
        assert!(leased(&out).is_empty(), "switch {switch}");
        assert_eq!(
            out.wait_reasons["a"], "needs tier 1, this member is tier 0",
            "switch {switch}"
        );
    }
}

#[test]
fn with_the_switch_off_a_tier_zero_member_gets_nothing() {
    let out = run(PlanStateInput {
        members: vec![member("newbie", "guest", Some(0))],
        capacities: vec![capacity("newbie", 1)],
        tasks: vec![order("a", "low", 0, 0.5)],
        ..PlanStateInput::default()
    });
    assert!(leased(&out).is_empty());
    assert_eq!(
        out.wait_reasons["a"],
        "tier 0 members get no work while tier0_coding is off"
    );
}

#[test]
fn with_the_switch_on_a_tier_zero_member_gets_low_and_small_only() {
    let out = run(PlanStateInput {
        members: vec![member("newbie", "guest", Some(0))],
        capacities: vec![capacity("newbie", 3)],
        tasks: vec![
            order("a-low-small", "low", 0, 0.5),
            order("b-normal-small", "normal", 0, 0.5),
            order("c-low-big", "low", 0, 2.0),
        ],
        tier0_coding: true,
        ..PlanStateInput::default()
    });
    assert_eq!(leased(&out), vec![("a-low-small".into(), "newbie".into())]);
    assert!(out.wait_reasons["b-normal-small"].contains("only low risk orders of small size"));
    assert!(out.wait_reasons["c-low-big"].contains("only low risk orders of small size"));
}

#[test]
fn a_guarded_order_needs_tier_two_even_when_min_trust_says_less() {
    let out = run(PlanStateInput {
        members: vec![member("contrib", "guest", Some(1))],
        capacities: vec![capacity("contrib", 1)],
        tasks: vec![order("g", "guarded", 0, 0.5)],
        ..PlanStateInput::default()
    });
    assert!(leased(&out).is_empty());
    assert_eq!(out.wait_reasons["g"], "needs tier 2, this member is tier 1");
}

#[test]
fn a_tier_two_token_member_is_scheduled_as_before() {
    // No tier given: the trust name decides, as for every state built before the field.
    let with = |tier| {
        run(PlanStateInput {
            members: vec![member("tok", "guest", tier)],
            capacities: vec![capacity("tok", 2)],
            tasks: vec![order("a", "normal", 2, 1.0), order("b", "guarded", 2, 1.0)],
            ..PlanStateInput::default()
        })
    };
    let a = with(None);
    assert_eq!(
        leased(&a),
        vec![("a".into(), "tok".into()), ("b".into(), "tok".into())]
    );
    assert_eq!(with(Some(2)).actions, a.actions);
    // An order from before the fields has min_trust 2 and risk normal.
    let old: PlanTask = serde_json::from_value(json!({"id": "o", "title": "o", "scope": "src/o/*",
        "done_when": "ok", "accepts": ["codex"]}))
    .unwrap();
    assert_eq!((old.risk.as_str(), old.min_trust), ("normal", 2));
}

#[test]
fn a_member_with_no_tier_gets_nothing() {
    let out = run(PlanStateInput {
        members: vec![member("banned", "guest", Some(-1))],
        capacities: vec![capacity("banned", 1)],
        tasks: vec![order("a", "low", 0, 0.5)],
        tier0_coding: true,
        ..PlanStateInput::default()
    });
    assert!(leased(&out).is_empty());
    assert_eq!(
        out.wait_reasons["a"],
        "this member has no tier and gets no work"
    );
}

#[test]
fn the_explain_text_names_the_tier() {
    let state = PlanState::from_input(PlanStateInput {
        members: vec![member("newbie", "guest", Some(0))],
        capacities: vec![capacity("newbie", 1)],
        tasks: vec![order("a", "normal", 2, 1.0)],
        tier0_coding: true,
        ..PlanStateInput::default()
    });
    let ex = plan_explain(&state, &policy(), "a", NOW).unwrap();
    assert_eq!(
        ex.wait_reason.as_deref(),
        Some("needs tier 2, this member is tier 0")
    );
    let eval = &ex.slot_evaluations;
    assert!(
        eval.iter()
            .any(|e| !e.admitted
                && e.reason.as_deref() == Some("needs tier 2, this member is tier 0")),
        "{eval:?}"
    );
}

#[test]
fn a_better_slot_is_chosen_over_one_below_the_line() {
    let out = run(PlanStateInput {
        members: vec![
            member("aaa-newbie", "guest", Some(0)),
            member("zzz-trusted", "guest", Some(2)),
        ],
        capacities: vec![capacity("aaa-newbie", 1), capacity("zzz-trusted", 1)],
        tasks: vec![order("a", "normal", 2, 1.0)],
        tier0_coding: true,
        ..PlanStateInput::default()
    });
    assert_eq!(leased(&out), vec![("a".into(), "zzz-trusted".into())]);
}

#[test]
fn the_invariant_names_a_lease_or_offer_below_min_trust() {
    let state = PlanState::from_input(PlanStateInput {
        members: vec![member("newbie", "guest", Some(0))],
        capacities: vec![capacity("newbie", 2)],
        tasks: vec![order("a", "normal", 2, 1.0), order("b", "normal", 2, 1.0)],
        ..PlanStateInput::default()
    });
    let lease = PlanAction::Lease {
        task_id: "a".into(),
        attempt: 1,
        owner: "newbie".into(),
        machine: "box".into(),
        class: "codex".into(),
        slot_key: "newbie/box/codex/1".into(),
        pools: vec!["codex.5h".into()],
        est_minutes_p90: 1.0,
        est_cost: 1.0,
        est_end_at: NOW + 60,
        expires_at: NOW + 600,
    };
    let offer = PlanAction::Offer {
        offer_id: "o1".into(),
        task_id: "b".into(),
        to_owner: "newbie".into(),
        machine: "box".into(),
        class: "codex".into(),
        slot_key: "newbie/box/codex/2".into(),
        kind: "fix".into(),
        scope: "src/b/*".into(),
        est_minutes_p90: 1.0,
        expires_at: NOW + 600,
    };
    let err = check_invariants(&[lease, offer], &state, &policy(), NOW).unwrap_err();
    let below: Vec<_> = err.iter().filter(|v| v.code == "BELOW_MIN_TRUST").collect();
    assert_eq!(below.len(), 2, "{err:?}");
    assert!(
        below[0]
            .message
            .contains("needs tier 2, this member is tier 0")
    );
}

#[test]
fn the_same_state_in_shuffled_input_orders_gives_equal_actions() {
    let base = PlanStateInput {
        members: vec![
            member("n1", "guest", Some(0)),
            member("c1", "guest", Some(1)),
            member("t1", "guest", Some(2)),
            member("m1", "owner", Some(3)),
        ],
        capacities: vec![
            capacity("n1", 1),
            capacity("c1", 1),
            capacity("t1", 1),
            capacity("m1", 1),
        ],
        tasks: vec![
            order("a", "low", 0, 0.5),
            order("b", "normal", 1, 1.0),
            order("c", "guarded", 2, 1.0),
            order("d", "normal", 3, 1.0),
            order("e", "normal", 2, 1.0),
        ],
        tier0_coding: true,
        ..PlanStateInput::default()
    };
    let reference = run(base.clone());
    assert!(!reference.actions.is_empty());
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..50 {
        let mut shuffled = base.clone();
        shuffle(&mut shuffled.members, &mut seed);
        shuffle(&mut shuffled.capacities, &mut seed);
        shuffle(&mut shuffled.tasks, &mut seed);
        let out = run(shuffled);
        assert_eq!(out.actions, reference.actions);
        assert_eq!(out.wait_reasons, reference.wait_reasons);
    }
    // Everything leased went to a member of enough tier.
    for (task, owner) in leased(&reference) {
        let need = match task.as_str() {
            "a" => 0,
            "b" => 1,
            "d" => 3,
            _ => 2,
        };
        let tier = match owner.as_str() {
            "n1" => 0,
            "c1" => 1,
            "t1" => 2,
            _ => 3,
        };
        assert!(tier >= need, "{task} to {owner}");
    }
}

fn shuffle<T>(slice: &mut [T], seed: &mut u64) {
    for i in (1..slice.len()).rev() {
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        slice.swap(i, (x as usize) % (i + 1));
    }
}

// ---------------------------------------------------------------------------------------------
// tiers from the TIER table
// ---------------------------------------------------------------------------------------------

fn hello(id: u64, from: &str, role: Role) -> Message {
    Message {
        id,
        ts: rfc3339(NOW_MS),
        from: from.into(),
        role,
        kind: Kind::Hello,
        topic: String::new(),
        body: "os=linux".into(),
        refs: vec![],
        to: vec![],
    }
}

fn tier_fact(id: u64, member: &str, tier: i64) -> Message {
    Message {
        id,
        ts: rfc3339(NOW_MS),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic: format!("tier/{member}"),
        body: json!({"v": 1, "member": member, "tier": tier, "reasons": ["test"], "at": NOW})
            .to_string(),
        refs: vec![],
        to: vec![],
    }
}

#[test]
fn the_loader_reads_tiers_through_tier_from_table() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .apply_batch(&[
            hello(1, "tok", Role::Member),
            hello(2, "boss", Role::Orchestrator),
            hello(3, "watcher", Role::Observer),
            hello(4, "promoted", Role::Member),
            hello(5, "banned", Role::Member),
            tier_fact(6, "promoted", 3),
            tier_fact(7, "banned", -1),
        ])
        .unwrap();
    let state = load_plan_state_from_store(store.connection(), &policy(), 0).unwrap();
    let tier = |n: &str| state.members[n].tier;
    assert_eq!(tier("tok"), Some(2), "a token member is tier 2");
    assert_eq!(tier("boss"), Some(3));
    assert_eq!(tier("watcher"), Some(-1));
    assert_eq!(tier("promoted"), Some(3), "the table row wins");
    assert_eq!(tier("banned"), Some(-1));
    assert!(!state.tier0_coding, "the switch is off by default");
}

#[test]
fn a_tier_fact_from_a_client_changes_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let mut forged = tier_fact(2, "tok", 3);
    forged.role = Role::Member;
    forged.from = "tok".into();
    store
        .apply_batch(&[hello(1, "tok", Role::Member), forged])
        .unwrap();
    let state = load_plan_state_from_store(store.connection(), &policy(), 0).unwrap();
    assert_eq!(state.members["tok"].tier, Some(2));
}
