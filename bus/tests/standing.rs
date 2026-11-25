//! Standing in a running hub: the rules that issue stamps, the tier of every member as a FACT, and
//! what each tier may do. Real hubs on free local ports, no network, no vendor login.
//!
//! The SUBMISSION table has no projection in this base (the intake part builds it), so a test writes
//! the rows into `state.db` through the contract DDL before the hub starts. Reports, reviews and
//! the key joins go into the log as FACT lines of the hub, the way the hub writes them itself.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, Hub, HubConfig};
use sommelier::key::JoinRules;
use sommelier::key::book::join_body;
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, member_id};
use sommelier::msg::{Kind, Message, Role};
use sommelier::report::side::Side;
use sommelier::stamp::ops::StampConfig;
use sommelier::stamp::{TierRules, query_stamps};
use sommelier::store::Store;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};

const OLD: &str = "2020-01-01T00:00:00.000Z";

fn now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Runs a test body on a runtime of its own, so a hub can live on another one and be stopped.
fn run<F: Future>(f: F) -> F::Output {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// A key member with a key file in a folder of its own.
struct Person {
    member: String,
    dir: tempfile::TempDir,
    key_public: String,
}

impl Person {
    fn new() -> Person {
        let key = MemberKey::generate().unwrap();
        let member = member_id(&key.public());
        let dir = tempfile::tempdir().unwrap();
        let key_public = key.public_b64();
        kc::save(
            &kc::key_path(dir.path()),
            &kc::KeyFile {
                member: member.clone(),
                key,
            },
        )
        .unwrap();
        Person {
            member,
            dir,
            key_public,
        }
    }

    fn opts(&self, url: &str) -> Opts {
        kc::opts_for(url, self.dir.path())
    }
}

/// The data folder of a hub: tokens, the log lines to start with and the table rows.
struct World {
    dir: tempfile::TempDir,
    orch: String,
    alice: String,
    carol: String,
    next_id: u64,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let mut t = TokenFile::default();
        let orch = t.add("orch", Role::Orchestrator).unwrap();
        let alice = t.add("alice", Role::Member).unwrap();
        let carol = t.add("carol", Role::Member).unwrap();
        t.save(&dir.path().join("tokens.json")).unwrap();
        World {
            dir,
            orch,
            alice,
            carol,
            next_id: 1,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Appends one line of the hub to the log the hub starts with.
    fn line(&mut self, kind: Kind, topic: &str, body: &str) {
        let m = Message {
            id: self.next_id,
            ts: "2026-11-25T10:00:00.000Z".into(),
            from: "hub".into(),
            role: Role::Hub,
            kind,
            topic: topic.into(),
            body: body.into(),
            refs: vec![],
            to: vec![],
        };
        self.next_id += 1;
        let log = self.path().join("bus.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap_or_default();
        text += &format!("{}\n", serde_json::to_string(&m).unwrap());
        std::fs::write(log, text).unwrap();
    }

    /// The join of a key member, with the time it joined.
    fn join(&mut self, p: &Person, joined_at: &str) {
        let body = join_body(&p.member, &p.key_public, "tester", joined_at);
        self.line(Kind::Fact, &format!("key/{}", p.member), &body);
    }

    fn review(&mut self, task: &str, reviewer: &str, submission: &str, verdict: &str) {
        let body = json!({"state": "answered", "submission": submission, "reviewer": reviewer,
                          "verdict": verdict});
        self.line(Kind::Fact, &format!("review/{task}/1"), &body.to_string());
    }

    fn report(&mut self, run: &str, member: &str, outcome: &str) {
        let body = json!({
            "v": 1, "run": run, "member": member, "build": "abc123", "channel": "main",
            "profile": "title", "scenario": "boot",
            "machine": {"os": "windows", "os_build": "26100", "gpu": "gpu one",
                        "gpu_memory_mb": 8192, "driver": "1.0", "memory_mb": 16384},
            "outcome": outcome, "first_error": null, "metrics": {}, "secs_alive": 60,
            "log_excerpt": "", "at": 1_000_000
        });
        self.line(Kind::Fact, &format!("report/{run}"), &body.to_string());
        // The block of the sender, as the hub keeps it in its side store when a report arrives.
        // Every member sits in a block of its own.
        let mut side = Side::open(self.path()).unwrap();
        side.hand(member, run, "windows/gpu one", 1).unwrap();
        let block = format!("blk-{member}");
        side.claim(member, run, "windows/gpu one", &block, 2, u64::MAX / 4)
            .unwrap();
    }

    /// Writes one SUBMISSION row through the contract DDL. Call it before the hub starts.
    fn submission(&self, sha: &str, task: &str, member: &str, state: &str, updated_at: u64) {
        let store = Store::open(self.path()).unwrap();
        store
            .connection()
            .execute(
                "INSERT OR REPLACE INTO SUBMISSION (sha256, task, attempt, member, base, head, bytes,
                    state, risk, tier, received_at, updated_at, msg_id)
                 VALUES (?1, ?2, 1, ?3, 'base', ?4, 100, ?5, 'low', 0, 1, ?6, ?7)",
                params![
                    sha,
                    task,
                    member,
                    format!("head-{sha}"),
                    state,
                    updated_at as i64,
                    900 + self.next_id as i64
                ],
            )
            .unwrap();
    }

    fn log(&self) -> Vec<Message> {
        std::fs::read_to_string(self.path().join("bus.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn facts_on(&self, prefix: &str) -> Vec<Message> {
        self.log()
            .into_iter()
            .filter(|m| m.kind == Kind::Fact && m.role == Role::Hub && m.topic.starts_with(prefix))
            .collect()
    }
}

fn small_rules() -> TierRules {
    TierRules {
        tier1_key_age_days: 7,
        tier1_penalty_window_days: 30,
        tier1_landed: 2,
        tier1_run_agreed: 2,
        tier2_landed: 2,
        tier2_review_confirmed: 1,
    }
}

fn standing_on() -> StampConfig {
    StampConfig {
        enabled: true,
        rules: small_rules(),
        ..StampConfig::default()
    }
}

/// A hub that runs on a runtime of its own. Dropping it ends every task of the hub.
struct Running {
    url: String,
    rt: Option<Runtime>,
    hub: Arc<Hub>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn launch(world: &World, tweak: impl FnOnce(&mut HubConfig)) -> Running {
    let mut cfg = HubConfig::new(world.path());
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(100);
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    tweak(&mut cfg);
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let hub = Hub::open(cfg).unwrap();
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let listener = {
        let _guard = rt.enter();
        TcpListener::from_std(std_listener).unwrap()
    };
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let h = hub.clone();
    rt.spawn(async move {
        let _ = hub::serve(h, listener).await;
    });
    Running {
        url,
        rt: Some(rt),
        hub,
    }
}

fn token_opts(url: &str, token: &str) -> Opts {
    Opts {
        url: url.into(),
        token: token.into(),
    }
}

async fn state(url: &str, orch: &str) -> Value {
    let resp = reqwest::Client::new()
        .get(kc::http_url(url, "/api/state").unwrap())
        .header("authorization", format!("Bearer {orch}"))
        .send()
        .await
        .unwrap();
    serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap()
}

async fn row(url: &str, orch: &str, member: &str) -> Value {
    let st = state(url, orch).await;
    st["standing"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["member"] == member))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Waits until the tier of `member` in `/api/state` is `want`.
async fn wait_tier(url: &str, orch: &str, member: &str, want: i64) -> Value {
    let mut last = Value::Null;
    for _ in 0..150 {
        last = row(url, orch, member).await;
        if last["tier"] == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("`{member}` never reached tier {want}, last row: {last}");
}

async fn wait_stamps(world: &World, want: usize) {
    for _ in 0..150 {
        if world.facts_on("stamp/").len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "expected {want} stamps, the log has {}",
        world.facts_on("stamp/").len()
    );
}

async fn refused(opts: &Opts, req: Value) -> String {
    let v = client::request(opts, req).await.expect("request");
    assert_eq!(v["ok"], false, "{v}");
    v["error"].as_str().unwrap().to_owned()
}

/// Landed submissions: `n` of them for `member`, with a name prefix.
fn landed(world: &World, member: &str, prefix: &str, n: usize, updated_at: u64) {
    for i in 0..n {
        world.submission(
            &format!("{prefix}{i}"),
            &format!("task-{prefix}{i}"),
            member,
            "landed",
            updated_at,
        );
    }
}

fn kinds(world: &World) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = world
        .facts_on("stamp/")
        .iter()
        .map(|m| {
            let v: Value = serde_json::from_str(&m.body).unwrap();
            (
                v["kind"].as_str().unwrap().to_owned(),
                v["subject"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

// Rules

/// The world of the rule tests: authors, a reviewer, testers and a loser of a race.
struct Rules {
    world: World,
    author: Person,
    loser: Person,
    reviewer: Person,
    testers: Vec<Person>,
}

fn rules_world() -> Rules {
    let mut world = World::new();
    let author = Person::new();
    let loser = Person::new();
    let reviewer = Person::new();
    let testers: Vec<Person> = (0..4).map(|_| Person::new()).collect();
    world.join(&author, OLD);
    world.join(&loser, OLD);
    world.join(&reviewer, OLD);
    for t in &testers {
        world.join(t, OLD);
    }
    // The author landed two orders long ago. The loser's order of the first task came second.
    world.submission("sa0", "task-a0", &author.member, "landed", 1_000_000);
    world.submission("sa1", "task-a1", &author.member, "landed", 1_000_000);
    world.submission("sl0", "task-a0", &loser.member, "rejected", 1_000_500);
    world.review("task-a0", &reviewer.member, "sa0", "pass");
    // Two runs, three testers see it work and one fails: the cell settles on works.
    // The token member alice vouches for the cell: a ring of fresh keys would not lift itself.
    for run in ["r1", "r2"] {
        for (i, t) in testers.iter().enumerate() {
            world.report(run, &t.member, if i < 3 { "works" } else { "fails" });
        }
        world.report(run, "alice", "works");
    }
    Rules {
        world,
        author,
        loser,
        reviewer,
        testers,
    }
}

#[test]
fn each_rule_writes_its_stamp_once_also_across_a_restart() {
    run(async {
        let w = rules_world();
        let r = launch(&w.world, |c| c.stamp = standing_on());
        // 2 landed, 1 superseded, 1 review_confirmed, 8 agreed (alice too), 2 outlier.
        wait_stamps(&w.world, 14).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let mut want: Vec<(String, String)> = vec![
            ("landed".into(), w.author.member.clone()),
            ("landed".into(), w.author.member.clone()),
            ("superseded".into(), w.loser.member.clone()),
            ("review_confirmed".into(), w.reviewer.member.clone()),
            ("run_outlier".into(), w.testers[3].member.clone()),
            ("run_outlier".into(), w.testers[3].member.clone()),
        ];
        for t in &w.testers[..3] {
            want.push(("run_agreed".into(), t.member.clone()));
            want.push(("run_agreed".into(), t.member.clone()));
        }
        want.push(("run_agreed".into(), "alice".into()));
        want.push(("run_agreed".into(), "alice".into()));
        want.sort();
        assert_eq!(kinds(&w.world), want, "one stamp per rule and evidence");
        let tiers_before = w.world.facts_on("tier/").len();
        drop(r);

        // A second hub on the same folder replays the log and writes nothing again.
        let r2 = launch(&w.world, |c| c.stamp = standing_on());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(kinds(&w.world), want, "no stamp twice after a restart");
        assert_eq!(w.world.facts_on("tier/").len(), tiers_before);
        drop(r2);

        // Replay gives the same STAMP and TIER tables as the running hub kept.
        let live = Store::open(w.world.path()).unwrap();
        let mut fresh = Store::open_in_memory().unwrap();
        fresh.apply_batch(&w.world.log()).unwrap();
        assert_eq!(
            query_stamps(live.connection()).unwrap(),
            query_stamps(fresh.connection()).unwrap()
        );
        let tiers = |s: &Store| -> Vec<(String, i64, String)> {
            let mut q = s
                .connection()
                .prepare("SELECT member, tier, reasons FROM TIER ORDER BY member")
                .unwrap();
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(tiers(&live), tiers(&fresh));
        assert!(!tiers(&fresh).is_empty());
    });
}

#[test]
fn a_rule_that_is_not_met_writes_nothing() {
    run(async {
        let mut world = World::new();
        let p = Person::new();
        world.join(&p, OLD);
        // Landed less than the window ago, a fail verdict, an open submission, a cell below quorum.
        world.submission("s1", "t1", &p.member, "landed", now_sec());
        world.submission("s2", "t2", &p.member, "approved", 1_000_000);
        world.review("t1", &p.member, "s1", "pass");
        world.review("t9", &p.member, "s1", "fail");
        world.report("r1", &p.member, "works");
        let r = launch(&world, |c| c.stamp = standing_on());
        tokio::time::sleep(Duration::from_millis(800)).await;
        // Only the landed stamp of s1: nothing is confirmed yet, no cell is settled.
        assert_eq!(kinds(&world), vec![("landed".into(), p.member.clone())]);
        drop(r);
    });
}

// Tiers

#[test]
fn a_member_reaches_tier_1_by_landed_orders_and_by_agreed_reports() {
    run(async {
        let w = rules_world();
        let r = launch(&w.world, |c| c.stamp = standing_on());
        let by_landed = wait_tier(&r.url, &w.world.orch, &w.author.member, 1).await;
        let by_runs = wait_tier(&r.url, &w.world.orch, &w.testers[0].member, 1).await;
        // The reviewer has one confirmed review and no landed order: still tier 0.
        let reviewer = row(&r.url, &w.world.orch, &w.reviewer.member).await;
        assert_eq!(reviewer["tier"], 0, "{reviewer}");
        assert!(!reviewer["missing"].as_array().unwrap().is_empty());
        // The outlier gets no tier.
        assert_eq!(
            row(&r.url, &w.world.orch, &w.testers[3].member).await["tier"],
            0
        );
        assert!(by_landed["reasons"].to_string().contains("landed"));
        assert!(by_runs["reasons"].to_string().contains("agreed"));
        // The FACT tier/<member> carries the reasons and the log shows every change.
        let facts = w.world.facts_on("tier/");
        let mine = facts
            .iter()
            .find(|m| m.topic == format!("tier/{}", w.author.member))
            .expect("a tier fact for the author");
        let body: Value = serde_json::from_str(&mine.body).unwrap();
        assert_eq!(body["tier"], 1);
        assert_eq!(body["member"], w.author.member.as_str());
        assert!(body["reasons"][0].as_str().unwrap().contains("landed"));
        assert!(body["at"].as_u64().unwrap() > 0);
        // Over the wire the tier is the one of the table.
        let opts = w.author.opts(&r.url);
        let who = client::request_ok(&opts, json!({"op": "whoami"}))
            .await
            .unwrap();
        assert_eq!(who["tier"], 1);
    });
}

#[test]
fn a_hub_with_standing_off_behaves_as_before() {
    run(async {
        let w = rules_world();
        let r = launch(&w.world, |_| {});
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(w.world.facts_on("stamp/").is_empty());
        assert!(w.world.facts_on("tier/").is_empty());
        let opts = w.author.opts(&r.url);
        let who = client::request_ok(&opts, json!({"op": "whoami"}))
            .await
            .unwrap();
        assert_eq!(who["tier"], 0);
        let e = refused(&opts, json!({"op": "send", "kind": "CHAT", "body": "hi"})).await;
        assert!(e.contains("tier 0"), "{e}");
        let e = refused(
            &token_opts(&r.url, &w.world.orch),
            json!({"op": "stamp.grant", "member": "x"}),
        )
        .await;
        assert!(e.contains("switched off"), "{e}");
        assert!(!r.hub.config().stamp.enabled);
    });
}

#[test]
fn revert_drops_author_and_reviewer_and_clear_restores_them() {
    run(async {
        let mut world = World::new();
        let author = Person::new();
        let reviewer = Person::new();
        world.join(&author, OLD);
        world.join(&reviewer, OLD);
        // Both hold tier 1 by landed orders. Recent landings are not confirmed yet.
        let fresh = now_sec();
        landed(&world, &author.member, "a", 2, fresh);
        landed(&world, &reviewer.member, "r", 2, fresh);
        world.review("task-a0", &reviewer.member, "a0", "pass");
        let r = launch(&world, |c| c.stamp = standing_on());
        let orch = token_opts(&r.url, &world.orch);
        wait_tier(&r.url, &world.orch, &author.member, 1).await;
        wait_tier(&r.url, &world.orch, &reviewer.member, 1).await;

        // A connected author is demoted and refused on its very next request.
        let mut conn = client::Conn::open(&author.opts(&r.url)).await.unwrap();
        let ok = conn
            .call(&json!({"op": "send", "kind": "CHAT", "body": "hello"}))
            .await
            .unwrap();
        assert_eq!(ok["ok"], true, "tier 1 may send CHAT: {ok}");

        let rev = client::request_ok(
            &orch,
            json!({"op": "stamp.revert", "submission": "a0", "cause": "broke the build"}),
        )
        .await
        .unwrap();
        assert_eq!(rev["review_missed"], json!([reviewer.member]));
        let ids: Vec<u64> = rev["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        let a = wait_tier(&r.url, &world.orch, &author.member, 0).await;
        wait_tier(&r.url, &world.orch, &reviewer.member, 0).await;
        assert!(a["reasons"].to_string().contains("penalty"), "{a}");
        assert_eq!(a["open_penalties"][0]["kind"], "reverted");
        let denied = conn
            .call(&json!({"op": "send", "kind": "CHAT", "body": "again"}))
            .await
            .unwrap();
        assert_eq!(denied["ok"], false);
        assert!(
            denied["error"].as_str().unwrap().contains("tier 0"),
            "{denied}"
        );
        // A second revert of the same submission is refused.
        let e = refused(
            &orch,
            json!({"op": "stamp.revert", "submission": "a0", "cause": "again"}),
        )
        .await;
        assert!(e.contains("already"), "{e}");

        // Clearing closes the penalties and both come back.
        for (member, id) in [(&author.member, ids[0]), (&reviewer.member, ids[1])] {
            client::request_ok(
                &orch,
                json!({"op": "stamp.clear", "member": member, "stamp": id}),
            )
            .await
            .unwrap();
        }
        wait_tier(&r.url, &world.orch, &author.member, 1).await;
        wait_tier(&r.url, &world.orch, &reviewer.member, 1).await;
        let back = conn
            .call(&json!({"op": "send", "kind": "CHAT", "body": "back"}))
            .await
            .unwrap();
        assert_eq!(back["ok"], true, "{back}");
        let e = refused(
            &orch,
            json!({"op": "stamp.clear", "member": author.member, "stamp": ids[0]}),
        )
        .await;
        assert!(e.contains("cleared already"), "{e}");
    });
}

#[test]
fn a_grant_alone_or_the_facts_alone_do_not_make_tier_2() {
    run(async {
        let mut world = World::new();
        let facts = Person::new();
        let granted = Person::new();
        world.join(&facts, OLD);
        world.join(&granted, OLD);
        // `facts` landed two orders and has one confirmed review. `granted` has nothing.
        landed(&world, &facts.member, "f", 2, 1_000_000);
        landed(&world, &granted.member, "g", 1, 1_000_000);
        world.review("task-g0", &facts.member, "g0", "pass");
        let r = launch(&world, |c| c.stamp = standing_on());
        let orch = token_opts(&r.url, &world.orch);

        // The facts alone stop at tier 1 and the row says what is missing.
        let row = wait_tier(&r.url, &world.orch, &facts.member, 1).await;
        wait_stamps(&world, 4).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let row2 = self::row(&r.url, &world.orch, &facts.member).await;
        assert_eq!(row2["tier"], 1, "{row} {row2}");
        assert!(row2["missing"].to_string().contains("grant_tier2"));

        // The grant alone changes nothing.
        client::request_ok(
            &orch,
            json!({"op": "stamp.grant", "member": granted.member}),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let g = self::row(&r.url, &world.orch, &granted.member).await;
        assert_eq!(g["tier"], 0, "{g}");
        assert_eq!(g["counts"]["grant_tier2"], 1);

        // Both together make tier 2.
        client::request_ok(&orch, json!({"op": "stamp.grant", "member": facts.member}))
            .await
            .unwrap();
        let top = wait_tier(&r.url, &world.orch, &facts.member, 2).await;
        assert!(top["reasons"].to_string().contains("tier 2"));
        let e = refused(&orch, json!({"op": "stamp.grant", "member": facts.member})).await;
        assert!(e.contains("already"), "{e}");
    });
}

#[test]
fn intent_removes_the_tier_and_the_key_is_refused_at_the_next_sign_in() {
    run(async {
        let mut world = World::new();
        let bad = Person::new();
        world.join(&bad, OLD);
        landed(&world, &bad.member, "b", 2, 1_000_000);
        let r = launch(&world, |c| c.stamp = standing_on());
        let orch = token_opts(&r.url, &world.orch);
        wait_tier(&r.url, &world.orch, &bad.member, 1).await;
        let opts = bad.opts(&r.url);
        client::request_ok(&opts, json!({"op": "whoami"}))
            .await
            .unwrap();

        let out = client::request_ok(
            &orch,
            json!({"op": "stamp.intent", "member": bad.member, "cause": "hid a change"}),
        )
        .await
        .unwrap();
        assert_eq!(out["revoked"], true, "{out}");
        let gone = wait_tier(&r.url, &world.orch, &bad.member, -1).await;
        assert_eq!(gone["counts"]["intent"], 1);
        assert!(gone["reasons"].to_string().contains("revoked"));
        let err = client::request(&opts, json!({"op": "whoami"}))
            .await
            .unwrap_err();
        assert!(err.contains("revoked"), "{err}");
        // The log names who revoked and why, and the stamp keeps the cause.
        let log = std::fs::read_to_string(world.path().join("bus.jsonl")).unwrap();
        assert!(log.contains("intent: hid a change"));
        // A second finding is a stamp, the revoke of an revoked key is only noted.
        let again = client::request_ok(
            &orch,
            json!({"op": "stamp.intent", "member": bad.member, "cause": "still"}),
        )
        .await
        .unwrap();
        assert_eq!(again["revoked"], false);
    });
}

// Who may ask

#[test]
fn every_maintainer_request_is_refused_from_a_member_that_is_not_one() {
    run(async {
        let mut world = World::new();
        let low = Person::new();
        let mid = Person::new();
        let boss = Person::new();
        world.join(&low, OLD);
        world.join(&mid, OLD);
        world.join(&boss, OLD);
        landed(&world, &mid.member, "m", 2, 1_000_000);
        let boss_id = boss.member.clone();
        let r = launch(&world, |c| {
            c.stamp = StampConfig {
                maintainers: vec!["carol".into(), boss_id],
                ..standing_on()
            }
        });
        wait_tier(&r.url, &world.orch, &mid.member, 1).await;
        wait_tier(&r.url, &world.orch, &boss.member, 3).await;
        let requests = [
            json!({"op": "stamp.grant", "member": mid.member}),
            json!({"op": "stamp.clear", "member": mid.member, "stamp": 1}),
            json!({"op": "stamp.revert", "submission": "m0", "cause": "x"}),
            json!({"op": "stamp.intent", "member": low.member, "cause": "x"}),
        ];
        for req in &requests {
            // A token member that is no maintainer, a tier 1 key and a tier 0 key.
            let e = refused(&token_opts(&r.url, &world.alice), req.clone()).await;
            assert!(e.contains("for maintainers"), "{e}");
            let e = refused(&mid.opts(&r.url), req.clone()).await;
            assert!(e.contains("for maintainers") || e.contains("tier 1"), "{e}");
            let e = refused(&low.opts(&r.url), req.clone()).await;
            assert!(e.contains("tier 0"), "{e}");
        }
        // The two listed maintainers may: a token and a key member.
        let carol = token_opts(&r.url, &world.carol);
        let ok = client::request_ok(&carol, json!({"op": "stamp.grant", "member": mid.member}))
            .await
            .unwrap();
        assert_eq!(ok["ids"].as_array().unwrap().len(), 1);
        let ok = client::request_ok(
            &boss.opts(&r.url),
            json!({"op": "stamp.grant", "member": low.member}),
        )
        .await
        .unwrap();
        assert_eq!(ok["ids"].as_array().unwrap().len(), 1);
        // The log names the maintainer as issuer.
        let issuers: BTreeSet<String> = world
            .facts_on("stamp/")
            .iter()
            .filter_map(|m| serde_json::from_str::<Value>(&m.body).ok())
            .map(|v| v["issuer"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            issuers.contains("carol") && issuers.contains(&boss.member),
            "{issuers:?}"
        );
        // Nobody stamps their own work.
        let e = refused(
            &boss.opts(&r.url),
            json!({"op": "stamp.grant", "member": boss.member}),
        )
        .await;
        assert!(e.contains("same member") || e.contains("nobody"), "{e}");
    });
}

#[test]
fn a_client_cannot_send_a_fact_on_stamp_or_tier() {
    run(async {
        let world = World::new();
        let r = launch(&world, |c| c.stamp = standing_on());
        for token in [&world.alice, &world.orch] {
            let opts = token_opts(&r.url, token);
            for topic in ["stamp/mabcdefghijklmnop", "tier/mabcdefghijklmnop", "key/x"] {
                let e = refused(
                    &opts,
                    json!({"op": "send", "kind": "FACT", "topic": topic,
                           "body": "{\"v\":1,\"tier\":3}"}),
                )
                .await;
                assert!(e.contains("hub only"), "{topic}: {e}");
            }
        }
        assert!(world.facts_on("stamp/").is_empty());
        assert!(world.facts_on("tier/").is_empty());
    });
}

// What a tier may do

#[test]
fn over_the_wire_tier_0_is_refused_and_tier_1_may_talk_and_review() {
    run(async {
        let mut world = World::new();
        let low = Person::new();
        let mid = Person::new();
        world.join(&low, OLD);
        world.join(&mid, OLD);
        landed(&world, &mid.member, "m", 2, 1_000_000);
        let r = launch(&world, |c| c.stamp = standing_on());
        wait_tier(&r.url, &world.orch, &mid.member, 1).await;
        for req in [
            json!({"op": "send", "kind": "CHAT", "body": "hi"}),
            json!({"op": "review.next"}),
            json!({"op": "send", "kind": "HELLO", "body": "class=a:1"}),
        ] {
            let e = refused(&low.opts(&r.url), req).await;
            assert!(e.contains("tier 0"), "{e}");
        }
        let opts = mid.opts(&r.url);
        for kind in ["CHAT", "FINDING", "FIX", "READY", "STUCK", "HELLO"] {
            let v = client::request(&opts, json!({"op": "send", "kind": kind, "body": "x"}))
                .await
                .unwrap();
            assert_eq!(v["ok"], true, "{kind}: {v}");
        }
        // review.next passes the tier check. What the review part answers is its own matter.
        for op in ["review.next", "review.verdict"] {
            let v = client::request(&opts, json!({"op": op})).await.unwrap();
            let e = v["error"].as_str().unwrap_or_default();
            assert!(!e.contains("may not use"), "{op}: {e}");
        }
        // Tier 1 does not get everything: claims and the maintainer requests stay out.
        let e = refused(
            &opts,
            json!({"op": "send", "kind": "CLAIM", "topic": "t", "body": "x"}),
        )
        .await;
        assert!(e.contains("tier 1") && e.contains("CLAIM"), "{e}");
        // A token member is untouched.
        let alice = token_opts(&r.url, &world.alice);
        client::request_ok(&alice, json!({"op": "send", "kind": "CHAT", "body": "hi"}))
            .await
            .unwrap();
    });
}

#[test]
fn tier0_coding_decides_whether_tier_0_may_take_orders() {
    run(async {
        for coding in [false, true] {
            let mut world = World::new();
            let low = Person::new();
            world.join(&low, OLD);
            let r = launch(&world, |c| {
                c.stamp = StampConfig {
                    tier0_coding: coding,
                    ..standing_on()
                }
            });
            let opts = low.opts(&r.url);
            for op in [
                "task.next",
                "task.work",
                "task.checkpoint",
                "task.done",
                "task.stuck",
                "task.decline",
            ] {
                let v = client::request(&opts, json!({"op": op})).await.unwrap();
                let e = v["error"].as_str().unwrap_or_default();
                assert_eq!(
                    e.contains("tier 0 may not use"),
                    !coding,
                    "{op} with tier0_coding {coding}: {v}"
                );
            }
            // Free text stays closed either way.
            let e = refused(&opts, json!({"op": "send", "kind": "CHAT", "body": "x"})).await;
            assert!(e.contains("tier 0"), "{e}");
            // The order requests that make a task are no worker request.
            let e = refused(&opts, json!({"op": "task.add"})).await;
            assert!(e.contains("tier 0"), "{e}");
        }
    });
}

#[test]
fn the_state_page_holds_no_address_token_or_key_file() {
    run(async {
        let w = rules_world();
        let r = launch(&w.world, |c| c.stamp = standing_on());
        wait_tier(&r.url, &w.world.orch, &w.author.member, 1).await;
        let text = state(&r.url, &w.world.orch).await.to_string();
        let log = std::fs::read_to_string(w.world.path().join("bus.jsonl")).unwrap();
        for hay in [&text, &log] {
            assert!(!hay.contains("127.0.0.1"));
            assert!(!hay.contains(&w.world.orch));
            assert!(!hay.contains(&w.world.alice));
            assert!(!hay.contains("Users"));
        }
        assert!(text.contains("open_penalties") || text.contains("missing"));
    });
}
