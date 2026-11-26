//! Tests of the feature map: the file, the loader rules, orders that name a feature, the gate, the
//! state derived from the matrix, the alert of a broken file on a running hub and the command.
//! No network beyond loopback, no title and no vendor login.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sommelier::client::{self, Opts};
use sommelier::feature::{FeatureMap, profiles, registry};
use sommelier::gate::diff::{ChangeKind, CommitInfo, FileChange, FileMode, ParsedDiff};
use sommelier::gate::glob::any_match;
use sommelier::gate::{GateRules, OrderView, Risk, Severity, check, check_with_feature};
use sommelier::hooks::{self, OpCaller, OpContext};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::{Kind, Message, Role};
use sommelier::report::side::Side;
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::report::{machine_key, query_reports};
use sommelier::store::Store;
use sommelier::task::query_task;
use sommelier::time::rfc3339;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

const NOW_MS: u64 = 1_795_600_000_000;

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../swarm/features.example.toml")
}

const MAP: &str = r#"
version = 1
[[area]]
id = "graphics"
title = "Graphics"
[[area]]
id = "platform"
title = "Platform"
[[feature]]
id = "graphics.device"
title = "A device starts"
area = "graphics"
paths = ["crates/gpu/src/device/**"]
how = "Start the title."
[[feature.proof]]
profile = "title-a"
scenario = "boot"
milestone = "device_created"
[[feature.proof]]
profile = "title-a"
scenario = "menu"
milestone = "menu_drawn"
[[feature]]
id = "graphics.present"
title = "Frames reach the window"
area = "graphics"
paths = ["crates/gpu/src/present/**", "crates/gpu/src/swap.rs"]
depends = ["graphics.device"]
how = "Start the title and wait for the first frame."
[[feature.proof]]
profile = "title-a"
scenario = "boot"
milestone = "first_present"
[[feature]]
id = "platform.file_io"
title = "Files are read"
area = "platform"
paths = ["crates/io/**"]
"#;

// ---------- the file and the loader ----------

#[test]
fn the_example_file_loads() {
    let m = FeatureMap::load(&example_path()).unwrap();
    assert_eq!(m.areas().len(), 3);
    assert_eq!(m.len(), 8);
    for f in m.features() {
        assert!(f.id.starts_with(&format!("{}.", f.area)));
    }
    // The order puts every feature after what it depends on.
    let ordered = m.ordered();
    for (i, f) in ordered.iter().enumerate() {
        for d in &f.depends {
            assert!(
                ordered[..i].iter().any(|x| &x.id == d),
                "{} before {d}",
                f.id
            );
        }
    }
    // The example names no game and no publisher.
    let text = std::fs::read_to_string(example_path())
        .unwrap()
        .to_lowercase();
    for word in ["gta", "rockstar", "decant"] {
        assert!(!text.contains(word), "{word}");
    }
}

#[test]
fn a_missing_or_large_file_is_one_line_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let e = FeatureMap::load(&dir.path().join("none.toml")).unwrap_err();
    assert!(
        e.contains("cannot read the file") && !e.contains('\n'),
        "{e}"
    );
    let big = dir.path().join("big.toml");
    std::fs::write(&big, "#".repeat((1 << 20) + 1)).unwrap();
    assert!(FeatureMap::load(&big).unwrap_err().contains("larger than"));
}

#[test]
fn features_touching_agrees_with_the_gate_matcher() {
    let m = FeatureMap::parse(MAP).unwrap();
    let table: [(&str, &[&str]); 9] = [
        ("crates/gpu/src/device/mod.rs", &["graphics.device"]),
        ("crates/gpu/src/device/a/b/c.rs", &["graphics.device"]),
        ("crates/gpu/src/present/x.rs", &["graphics.present"]),
        ("crates/gpu/src/swap.rs", &["graphics.present"]),
        ("crates/gpu/src/swap.rs.orig", &[]),
        ("crates/io/read.rs", &["platform.file_io"]),
        ("crates/io", &["platform.file_io"]),
        ("Crates/io/read.rs", &[]),
        ("README.md", &[]),
    ];
    for (path, want) in table {
        assert_eq!(m.features_touching(path), want, "{path}");
        // The same answer as asking the matcher of the scope gate feature by feature.
        let by_gate: Vec<String> = m
            .features()
            .iter()
            .filter(|f| any_match(&f.paths, path, false))
            .map(|f| f.id.clone())
            .collect();
        assert_eq!(m.features_touching(path), by_gate, "{path}");
    }
}

#[test]
fn proofs_are_checked_against_a_folder_of_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let profile = r#"
id = "title-a"
name = "Title A"
[executable]
file = "title.exe"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 10
[[scenarios]]
id = "boot"
seconds = 60
[[scenarios.milestones]]
id = "device_created"
pattern = "device"
by = 30
"#;
    std::fs::write(dir.path().join("a.toml"), profile).unwrap();
    std::fs::write(dir.path().join("broken.toml"), "not a profile").unwrap();
    std::fs::write(dir.path().join("note.txt"), "ignored").unwrap();
    let (list, skipped) = profiles::read_dir(dir.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(skipped.len(), 1);
    let m = FeatureMap::parse(MAP).unwrap();
    let missing = profiles::missing(&m, &list);
    // device_created exists. menu has no scenario `menu`. first_present is no milestone of boot.
    assert_eq!(missing.len(), 2, "{missing:?}");
    assert!(missing[0].contains("graphics.device") && missing[0].contains("no scenario `menu`"));
    assert!(
        missing[1].contains("graphics.present")
            && missing[1].contains("no milestone `first_present`")
    );
    let none = profiles::missing(&m, &[]);
    assert!(none.iter().all(|l| l.contains("no profile `title-a`")));
}

// ---------- orders ----------

struct Fix {
    dir: tempfile::TempDir,
    cfg: HubConfig,
    store: Store,
    next_id: u64,
    file: PathBuf,
}

impl Fix {
    fn new(with_map: bool) -> Fix {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let file = dir.path().join("features.toml");
        std::fs::write(&file, MAP).unwrap();
        let mut cfg = HubConfig::new(&data);
        cfg.test.enabled = true;
        if with_map {
            registry::configure(&data, &file);
        }
        let store = Store::open(&data).unwrap();
        Fix {
            dir,
            cfg,
            store,
            next_id: 1,
            file,
        }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn emit(&mut self, mut msg: Message) -> u64 {
        msg.id = self.next_id;
        self.next_id += 1;
        if msg.ts.is_empty() {
            msg.ts = rfc3339(NOW_MS);
        }
        self.store.apply(&msg).unwrap();
        msg.id
    }

    fn op(&mut self, op: &str, body: &Value) -> Result<Map<String, Value>, String> {
        let caller = OpCaller {
            name: "coord".into(),
            role: Role::Orchestrator,
            trust: "owner",
        };
        let mut pending: Vec<Message> = Vec::new();
        let next_id = &mut self.next_id;
        let mut emit = |st: &mut Store, mut msg: Message| -> Result<u64, String> {
            msg.id = *next_id;
            *next_id += 1;
            if msg.ts.is_empty() {
                msg.ts = rfc3339(NOW_MS);
            }
            st.apply(&msg)?;
            pending.push(msg.clone());
            Ok(msg.id)
        };
        let mut ctx =
            OpContext::new(caller, &mut self.store, NOW_MS, &mut emit).with_config(&self.cfg);
        hooks::on_op(&mut ctx, op, body)
    }

    fn add(&mut self, id: &str, extra: Value) -> Result<Map<String, Value>, String> {
        let mut body = json!({"id": id, "title": format!("order {id}"), "kind": "fix"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.op("task.add", &body)
    }

    fn state(&self) -> Value {
        self.store.api_state(NOW_MS / 1000).unwrap()
    }
}

impl Drop for Fix {
    fn drop(&mut self) {
        registry::clear(&self.data());
    }
}

#[test]
fn task_add_with_a_feature_fills_scope_and_done_condition() {
    let mut f = Fix::new(true);
    let out = f.add("t1", json!({"feature": "graphics.present"})).unwrap();
    let row = &out["task"];
    assert_eq!(row["feature"], "graphics.present");
    assert_eq!(
        row["scope"],
        "crates/gpu/src/present/**,crates/gpu/src/swap.rs"
    );
    assert_eq!(row["done_when"], "proof: title-a boot first_present");
    assert_eq!(row["state"], "ready");
    let t = query_task(f.store.connection(), "t1").unwrap().unwrap();
    assert_eq!(t.feature.as_deref(), Some("graphics.present"));
    // A feature with two proofs lists both. A feature without proof leaves the done condition to the caller.
    let two = f.add("t2", json!({"feature": "graphics.device"})).unwrap();
    assert_eq!(
        two["task"]["done_when"],
        "proof: title-a boot device_created, proof: title-a menu menu_drawn"
    );
    let e = f
        .add("t3", json!({"feature": "platform.file_io"}))
        .unwrap_err();
    assert!(e.contains("done_when"), "{e}");
    let ok = f
        .add(
            "t4",
            json!({"feature": "platform.file_io", "done_when": "tests pass"}),
        )
        .unwrap();
    assert_eq!(ok["task"]["state"], "ready");
}

#[test]
fn an_explicit_scope_and_done_condition_win() {
    let mut f = Fix::new(true);
    let out = f
        .add(
            "t1",
            json!({"feature": "graphics.present", "scope": "crates/gpu/src/present/a.rs", "done_when": "cargo test"}),
        )
        .unwrap();
    assert_eq!(out["task"]["scope"], "crates/gpu/src/present/a.rs");
    assert_eq!(out["task"]["done_when"], "cargo test");
    assert_eq!(out["task"]["feature"], "graphics.present");
}

#[test]
fn the_risk_rule_runs_on_the_scope_from_the_feature() {
    let mut f = Fix::new(true);
    let text = MAP.replace("crates/io/**", ".github/workflows/**");
    std::fs::write(&f.file, text).unwrap();
    let out = f
        .add(
            "t1",
            json!({"feature": "platform.file_io", "done_when": "x"}),
        )
        .unwrap();
    assert_eq!(out["task"]["risk"], "guarded", "{out:?}");
}

#[test]
fn an_unknown_feature_or_no_map_is_refused() {
    let mut f = Fix::new(true);
    let e = f
        .add(
            "t1",
            json!({"feature": "graphics.nope", "scope": "a", "done_when": "b"}),
        )
        .unwrap_err();
    assert!(
        e.contains("graphics.nope") && e.contains("not in the feature map"),
        "{e}"
    );
    assert!(query_task(f.store.connection(), "t1").unwrap().is_none());

    let mut off = Fix::new(false);
    let e = off
        .add(
            "t1",
            json!({"feature": "graphics.present", "scope": "a", "done_when": "b"}),
        )
        .unwrap_err();
    assert!(e.contains("no feature map"), "{e}");
    // Without a feature the order is as before.
    let ok = off
        .add("t2", json!({"scope": "a", "done_when": "b"}))
        .unwrap();
    assert!(ok["task"].get("feature").is_none());
}

#[test]
fn a_log_line_without_the_field_replays_to_none() {
    let mut f = Fix::new(true);
    f.emit(Message {
        id: 0,
        ts: rfc3339(NOW_MS),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::TaskUpdate,
        topic: "task/old".into(),
        body: json!({"v": 1, "task": "old", "state": "ready", "reason": "added",
                     "task_row": {"id": "old", "title": "t", "kind": "fix", "scope": "a",
                                  "done_when": "b", "state": "ready"}})
        .to_string(),
        refs: vec![],
        to: vec![],
    });
    let t = query_task(f.store.connection(), "old").unwrap().unwrap();
    assert_eq!(t.feature, None);
    let json = serde_json::to_value(&t).unwrap();
    assert!(json.get("feature").is_none());
    // A later update of an order with a feature keeps it.
    f.add("t1", json!({"feature": "graphics.present"})).unwrap();
    f.op("task.cancel", &json!({"id": "t1"})).unwrap();
    let t = query_task(f.store.connection(), "t1").unwrap().unwrap();
    assert_eq!(t.feature.as_deref(), Some("graphics.present"));
    assert_eq!(t.state, "cancelled");
    // task.show and task.list carry the feature.
    let shown = f.op("task.show", &json!({"id": "t1"})).unwrap();
    assert_eq!(shown["task"]["feature"], "graphics.present");
    let listed = f.op("task.list", &json!({})).unwrap();
    let rows = listed["tasks"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["id"] == "t1" && r["feature"] == "graphics.present")
    );
    assert!(
        rows.iter()
            .any(|r| r["id"] == "old" && r.get("feature").is_none())
    );
}

// ---------- the gate ----------

fn file_change(path: &str) -> FileChange {
    FileChange {
        path: path.into(),
        old_path: None,
        change: ChangeKind::Modified,
        mode: FileMode::Regular,
        binary: false,
        size_after: 10,
        added: Vec::new(),
        removed: Vec::new(),
        old_mode: None,
        content: Vec::new(),
    }
}

fn diff_of(paths: &[&str]) -> ParsedDiff {
    let base = "a".repeat(40);
    let head = "b".repeat(40);
    ParsedDiff {
        base: base.clone(),
        head: head.clone(),
        commits: vec![CommitInfo {
            sha: head.clone(),
            parents: vec![base],
            author_name: "Test Author".into(),
            author_email: "author@example.com".into(),
            message: "change".into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ..CommitInfo::default()
        }],
        files: paths.iter().map(|p| file_change(p)).collect(),
        bundle_sha256: "0".repeat(64),
        bundle_bytes: 2000,
        base_paths: Vec::new(),
    }
}

fn order(scope: &[&str]) -> OrderView {
    OrderView {
        base: "a".repeat(40),
        scope: scope.iter().map(|s| s.to_string()).collect(),
        risk: Risk::Normal,
        tier: 2,
    }
}

#[test]
fn the_feature_gate_raises_on_another_feature_and_notes_a_path_of_none() {
    let m = FeatureMap::parse(MAP).unwrap();
    let fs = m.gate_scope("graphics.present").unwrap();
    let o = order(&["crates/**", "docs/**"]);
    let rules = GateRules::default();

    // Inside the feature: nothing.
    let inside = check_with_feature(
        &diff_of(&["crates/gpu/src/present/a.rs"]),
        &o,
        &rules,
        Some(&fs),
    );
    assert!(
        inside.findings.iter().all(|f| f.gate != "feature_scope"),
        "{:?}",
        inside.findings
    );
    assert_eq!(inside.risk_after, Risk::Normal);

    // The path of another feature: raise, with the id of the other feature.
    let other = check_with_feature(&diff_of(&["crates/io/read.rs"]), &o, &rules, Some(&fs));
    let f: Vec<_> = other
        .findings
        .iter()
        .filter(|f| f.gate == "feature_scope")
        .collect();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Raise);
    assert!(f[0].text.contains("platform.file_io"), "{}", f[0].text);
    assert_eq!(f[0].path.as_deref(), Some("crates/io/read.rs"));
    assert_eq!(other.risk_after, Risk::Guarded);
    assert!(other.passed, "it raises, it does not block");

    // A path of no feature: a note. The risk does not move.
    let none = check_with_feature(&diff_of(&["docs/readme.md"]), &o, &rules, Some(&fs));
    let f: Vec<_> = none
        .findings
        .iter()
        .filter(|f| f.gate == "feature_scope")
        .collect();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Note);
    assert_eq!(none.risk_after, Risk::Normal);

    // A path only a commit in the middle touched is read too.
    let mut mid = diff_of(&["crates/gpu/src/present/a.rs"]);
    mid.commits[0].paths.push("crates/io/gone.rs".into());
    let r = check_with_feature(&mid, &o, &rules, Some(&fs));
    assert!(
        r.findings
            .iter()
            .any(|f| f.gate == "feature_scope" && f.severity == Severity::Raise)
    );

    // The scope gate stays the hard limit.
    let out = check_with_feature(&diff_of(&["secret/x.rs"]), &o, &rules, Some(&fs));
    assert!(!out.passed);
    assert!(
        out.findings
            .iter()
            .any(|f| f.gate == "scope" && f.severity == Severity::Block)
    );
}

#[test]
fn without_a_feature_the_report_is_the_one_of_check() {
    let o = order(&["crates/**"]);
    let d = diff_of(&["crates/io/read.rs", "crates/gpu/src/swap.rs"]);
    let rules = GateRules::default();
    assert_eq!(
        check(&d, &o, &rules),
        check_with_feature(&d, &o, &rules, None)
    );
}

// ---------- run orders and the derived state ----------

fn machine(gpu: &str) -> MachineClass {
    MachineClass {
        os: "windows".into(),
        os_build: "26100".into(),
        gpu: gpu.into(),
        gpu_memory_mb: 16384,
        driver: "552.22".into(),
        memory_mb: 32768,
    }
}

fn report(run: &str, member: &str, scenario: &str, m: &MachineClass, outcome: Outcome) -> Report {
    Report {
        v: 1,
        run: run.into(),
        member: member.into(),
        build: "abc1234".into(),
        channel: "main".into(),
        profile: "title-a".into(),
        scenario: scenario.into(),
        machine: m.clone(),
        outcome,
        first_error: None,
        metrics: Default::default(),
        secs_alive: 60,
        log_excerpt: "frame presented\n".into(),
        at: 1_000,
        sig: None,
    }
}

/// Three members on three address blocks settle a cell. The side store gets the blocks as the hub
/// would have written them.
fn settle(f: &mut Fix, run: &str, tag: &str, scenario: &str, m: &MachineClass, outcome: Outcome) {
    let key = machine_key(m);
    let mut side = Side::open(&f.data()).unwrap();
    for i in 1..=3 {
        let member = format!("{tag}{i}");
        side.hand(&member, run, &key, NOW_MS).unwrap();
        side.claim(
            &member,
            run,
            &key,
            &format!("block-{tag}{i}"),
            NOW_MS,
            3_600_000,
        )
        .unwrap();
        f.emit(Message {
            id: 0,
            ts: rfc3339(NOW_MS),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: format!("report/{run}"),
            body: serde_json::to_string(&report(run, &member, scenario, m, outcome)).unwrap(),
            refs: vec![],
            to: vec![],
        });
    }
}

fn feature_row(state: &Value, id: &str) -> Value {
    state["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no row for {id} in {}", state["features"]))
}

#[test]
fn a_run_order_may_name_a_feature_and_it_must_exist() {
    let mut f = Fix::new(true);
    let base =
        json!({"build": "abc1234", "channel": "main", "profile": "title-a", "scenario": "boot"});
    let mut body = base.clone();
    body["feature"] = json!("graphics.nope");
    let e = f.op("test.order", &body).unwrap_err();
    assert!(e.contains("graphics.nope"), "{e}");
    body["feature"] = json!(7);
    assert!(f.op("test.order", &body).is_err());
    body["feature"] = json!("graphics.present");
    let out = f.op("test.order", &body).unwrap();
    assert_eq!(out["feature"], "graphics.present");
    let id = out["id"].as_str().unwrap().to_owned();
    let n: String = f
        .store
        .connection()
        .query_row(
            "SELECT feature FROM RUN_ORDER_FEATURE WHERE run_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, "graphics.present");
    // An order without a feature is as before.
    let mut plain = base.clone();
    plain["scenario"] = json!("menu");
    let out = f.op("test.order", &plain).unwrap();
    assert!(out.get("feature").is_none());
    // The open orders show in the state, per feature.
    let st = f.state();
    let row = feature_row(&st, "graphics.present");
    assert_eq!(row["open_runs"], 1);
    assert_eq!(row["open_orders"], 1);
    assert_eq!(row["area"], "graphics");
    assert_eq!(row["title"], "Frames reach the window");
    assert_eq!(feature_row(&st, "graphics.device")["open_orders"], 0);
    // An open task counts too, a cancelled one does not.
    f.add("t1", json!({"feature": "graphics.present"})).unwrap();
    f.add("t2", json!({"feature": "graphics.present"})).unwrap();
    f.op("task.cancel", &json!({"id": "t2"})).unwrap();
    let row = feature_row(&f.state(), "graphics.present");
    assert_eq!(
        (row["open_tasks"].as_u64(), row["open_orders"].as_u64()),
        (Some(1), Some(2))
    );
}

#[test]
fn the_state_is_derived_per_machine_key_from_the_cells_of_the_proofs() {
    let mut f = Fix::new(true);
    let a = machine("rtx 4080");
    let b = machine("rx 7900");
    let (ka, kb) = (machine_key(&a), machine_key(&b));
    // Nothing ran yet: no state at all.
    let st = f.state();
    assert_eq!(feature_row(&st, "graphics.present")["state"], json!({}));

    // boot works on machine A and fails on machine B. menu works on A and was never run on B.
    settle(&mut f, "ro1", "a", "boot", &a, Outcome::Works);
    settle(&mut f, "ro1", "b", "boot", &b, Outcome::Fails);
    settle(&mut f, "ro2", "c", "menu", &a, Outcome::Works);
    assert_eq!(query_reports(f.store.connection()).unwrap().len(), 9);
    let st = f.state();
    // present has one proof (boot): works on A, fails on B.
    let present = feature_row(&st, "graphics.present");
    assert_eq!(present["state"][&ka], "works");
    assert_eq!(present["state"][&kb], "fails");
    // device has two proofs (boot and menu): works on A, fails on B (one failure is enough).
    let device = feature_row(&st, "graphics.device");
    assert_eq!(device["state"][&ka], "works");
    assert_eq!(device["state"][&kb], "fails");
    // A feature without proof has no state.
    assert_eq!(feature_row(&st, "platform.file_io")["state"], json!({}));

    // Only one proof of two ran on machine C: unknown. A hint (one report) is unknown too.
    let c = machine("arc b580");
    let kc = machine_key(&c);
    let mut side = Side::open(&f.data()).unwrap();
    side.hand("solo", "ro3", &kc, NOW_MS).unwrap();
    side.claim("solo", "ro3", &kc, "block-solo", NOW_MS, 3_600_000)
        .unwrap();
    f.emit(Message {
        id: 0,
        ts: rfc3339(NOW_MS),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic: "report/ro3".into(),
        body: serde_json::to_string(&report("ro3", "solo", "boot", &c, Outcome::Works)).unwrap(),
        refs: vec![],
        to: vec![],
    });
    let st = f.state();
    assert_eq!(
        feature_row(&st, "graphics.present")["state"][&kc],
        "unknown"
    );
    assert_eq!(feature_row(&st, "graphics.device")["state"][&kc], "unknown");
    // Derived at read time: the same projection has no table of states.
    let n: i64 = f
        .store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%FEATURE_STATE%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn a_hub_without_a_feature_file_adds_nothing_to_the_state() {
    let f = Fix::new(false);
    let st = f.state();
    assert!(st.get("features").is_none());
    assert!(st.get("matrix").is_some());
    let on = Fix::new(true);
    assert!(on.state().get("features").is_some());
}

// ---------- a running hub ----------

struct Live {
    url: String,
    dir: tempfile::TempDir,
    orch: String,
    task: tokio::task::JoinHandle<()>,
    file: PathBuf,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
        registry::clear(&self.dir.path().join("data"));
    }
}

async fn live() -> Live {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    t.save(&data.join("tokens.json")).unwrap();
    let file = dir.path().join("features.toml");
    std::fs::write(&file, MAP).unwrap();
    let mut cfg = HubConfig::new(&data);
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(100);
    // What `--factory` does for `[features] file`.
    registry::configure(&data, &file);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        hub::run(cfg, listener).await.unwrap();
    });
    Live {
        url,
        dir,
        orch,
        task,
        file,
    }
}

impl Live {
    fn opts(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.orch.clone(),
        }
    }

    fn alerts(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("data").join("bus.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .filter(|m| m.kind == Kind::Alert && m.topic.starts_with("alert/FEATURES_INVALID"))
            .map(|m| serde_json::from_str::<Value>(&m.body).unwrap())
            .collect()
    }
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_broken_file_on_a_running_hub_keeps_the_old_map_and_raises_the_alert_once() {
    let hub = live().await;
    // The hub knows the map: an order with a feature works.
    let v = client::request(
        &hub.opts(),
        json!({"op": "task.add", "id": "t1", "title": "T", "kind": "fix", "feature": "graphics.present"}),
    )
    .await
    .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        v["task"]["scope"],
        "crates/gpu/src/present/**,crates/gpu/src/swap.rs"
    );
    assert!(hub.alerts().is_empty());

    // A new file that does not pass the loader.
    std::fs::write(
        &hub.file,
        format!("{MAP}\n[[feature]]\nid = \"graphics.bad\"\n"),
    )
    .unwrap();
    wait_for("the alert", || !hub.alerts().is_empty()).await;
    // Several more ticks pass. Still one alert.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let alerts = hub.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["code"], "FEATURES_INVALID");
    assert_eq!(alerts[0]["state"], "open");
    assert!(alerts[0]["text"].as_str().unwrap().contains("old map"));
    // The old map still answers.
    let v = client::request(
        &hub.opts(),
        json!({"op": "task.add", "id": "t2", "title": "T", "kind": "fix", "feature": "platform.file_io", "done_when": "x"}),
    )
    .await
    .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    let v = client::request(
        &hub.opts(),
        json!({"op": "task.add", "id": "t3", "title": "T", "kind": "fix", "feature": "graphics.bad", "scope": "a", "done_when": "x"}),
    )
    .await
    .unwrap();
    assert_eq!(v["ok"], false);

    // A good file again closes the alert and the new map is in use.
    std::fs::write(
        &hub.file,
        MAP.replace("Files are read", "Files are read and written"),
    )
    .unwrap();
    wait_for("the close", || hub.alerts().len() == 2).await;
    assert_eq!(hub.alerts()[1]["state"], "closed");
    assert_eq!(
        registry::current(&hub.dir.path().join("data"))
            .unwrap()
            .len(),
        3
    );
}

// ---------- the command ----------

fn sommelier(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn feature_check_exits_3_on_a_refusal_and_0_on_a_good_file() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.toml");
    std::fs::write(&good, MAP).unwrap();
    let out = sommelier(&["feature", "check", "--file", good.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("2 areas, 3 features"));

    let bad = dir.path().join("bad.toml");
    std::fs::write(
        &bad,
        MAP.replace(
            "depends = [\"graphics.device\"]",
            "depends = [\"graphics.zzz\"]",
        ),
    )
    .unwrap();
    let out = sommelier(&["feature", "check", "--file", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("graphics.present") && err.contains("graphics.zzz"),
        "{err}"
    );

    let out = sommelier(&[
        "feature",
        "check",
        "--file",
        good.to_str().unwrap(),
        "--profiles",
        dir.path().to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "proofs name a profile that is not there"
    );
}

#[test]
fn feature_list_show_and_touching_read_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.toml");
    std::fs::write(&f, MAP).unwrap();
    let f = f.to_str().unwrap();
    let run = |args: &[&str]| {
        let mut a = vec!["feature"];
        a.extend_from_slice(args);
        a.extend_from_slice(&["--file", f, "--offline"]);
        let o = sommelier(&a);
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        )
    };
    let (code, out) = run(&["list"]);
    assert_eq!(code, Some(0));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with("graphics.device"));
    let (_, out) = run(&["list", "--area", "platform"]);
    assert_eq!(out.lines().count(), 1);
    assert_eq!(run(&["list", "--area", "nope"]).0, Some(3));
    let (code, out) = run(&["show", "graphics.present"]);
    assert_eq!(code, Some(0));
    assert!(
        out.contains("depends: graphics.device")
            && out.contains("proof: title-a boot first_present"),
        "{out}"
    );
    assert_eq!(run(&["show", "graphics.nope"]).0, Some(3));
    assert_eq!(run(&["show"]).0, Some(2));
    let (_, out) = run(&["touching", "crates/io/a.rs", "README.md"]);
    assert!(
        out.contains("crates/io/a.rs: platform.file_io") && out.contains("README.md: no feature"),
        "{out}"
    );
    let (_, out) = run(&["list", "--json"]);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["features"].as_array().unwrap().len(), 3);
}
