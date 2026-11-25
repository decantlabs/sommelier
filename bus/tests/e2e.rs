//! One hub with every part of the factory on, a fake GitHub, a throwaway project repository as the
//! reference and no network beyond localhost. One test walks the whole way from the join of a stranger
//! to the landing of its patch, a patch that stops at the gates, and the test network with its tiers.
//! At the end the log is replayed into a fresh store and every factory table must match the live one.
//!
//! The hub is set up the way `sommelier serve --factory FILE` sets it up: a factory file and a policy
//! with lowered rule numbers go through `factory::apply`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sommelier::bundle::pack;
use sommelier::cli::submit::upload;
use sommelier::client::{self, Conn, Opts};
use sommelier::factory;
use sommelier::hub::{self, Hub, HubConfig};
use sommelier::key::JoinRules;
use sommelier::key::client as kc;
use sommelier::key::crypto::{MemberKey, sign_request};
use sommelier::msg::{Kind, Message, Role};
use sommelier::policy::{DEFAULT_POLICY, Snapshot};
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::stamp::tier_table::tier_from_table;
use sommelier::store::Store;
use sommelier::tokens::TokenFile;
use sommelier::verify::ops::SharedApi;
use sommelier::verify::{FakeGitHub, Job, JobStep, Run, Timers};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ADDRESS_HEADER: &str = "X-Test-Addr";

// The settings of the hub as a person would write them.

const FACTORY_FILE: &str = r#"
[join]
enabled = true
[intake]
enabled = true
[verify]
enabled = true
repository = "owner/project"
public_address = "https://hub.example.test"
[review]
enabled = true
[stamp]
enabled = true
tier0_coding = true
[test]
enabled = true
[door]
trust_header = "X-Test-Addr"
keep_days = 1
joins_per_block_per_hour = 0
"#;

/// The policy: the shipped one plus factory sections with rule numbers lowered for the test. A member
/// reaches tier 1 with one agreed run and no age (tier 1 asks for the landed changes or the agreed runs).
fn policy_text() -> String {
    format!(
        "{DEFAULT_POLICY}
[trust.tier1]
reach = {{ landed = 5, run_agreed = 1, key_age_days = 0, reverted_for_cause = 0 }}

[test]
quorum = 3
candidate_min_tier = 2

[switch]
intake = \"enforce\"
tier0_coding = \"enforce\"
"
    )
}

// Throwaway git repositories.

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.org",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A work repository with one base commit and a bare mirror of it, the reference of the hub.
struct Repos {
    _dir: tempfile::TempDir,
    work: PathBuf,
    mirror: PathBuf,
    base: String,
}

fn repos() -> Repos {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::create_dir_all(work.join("docs")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n").unwrap();
    std::fs::write(work.join("docs/readme.md"), "hello\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    let mirror = dir.path().join("mirror.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    );
    Repos {
        _dir: dir,
        work,
        mirror,
        base,
    }
}

impl Repos {
    /// Commits `files` on top of the base and packs the result. `tag` makes every bundle unique.
    fn bundle(&self, out_dir: &Path, tag: &str, files: &[(&str, String)]) -> (PathBuf, String) {
        git(&self.work, &["checkout", "-q", "--detach", &self.base]);
        for (path, text) in files {
            let full = self.work.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(
            &self.work,
            &["commit", "-q", "-m", &format!("change {tag}")],
        );
        let head = git(&self.work, &["rev-parse", "HEAD"]);
        let file = out_dir.join(format!("{tag}.bundle"));
        let info = pack(&self.work, &self.base, &head, &file).expect("pack");
        (file, info.sha256)
    }
}

/// A finished run named after the submission, as the template names it.
fn run(id: u64, sha: &str, conclusion: &str) -> Run {
    Run {
        id,
        name: Some(format!("verify {sha}")),
        display_title: None,
        status: "completed".into(),
        conclusion: Some(conclusion.into()),
        created_at: None,
        run_started_at: None,
        updated_at: None,
    }
}

fn job(step: &str, conclusion: &str) -> Job {
    Job {
        name: "verify".into(),
        status: "completed".into(),
        conclusion: Some(conclusion.into()),
        started_at: None,
        completed_at: None,
        steps: vec![JobStep {
            name: step.into(),
            number: 1,
            status: "completed".into(),
            conclusion: Some(conclusion.into()),
        }],
    }
}

// The hub and the people around it.

struct World {
    hub: Arc<Hub>,
    url: String,
    dir: tempfile::TempDir,
    orch: Opts,
    carol: Opts,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for World {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(r: &Repos, fake: &Arc<FakeGitHub>) -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let carol = t.add("carol", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();

    let mut cfg = HubConfig::new(dir.path());
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(50);
    let rules = Snapshot::parse(&policy_text()).unwrap().policy.factory();
    let file = factory::parse(FACTORY_FILE).unwrap();
    let applied = factory::apply(&mut cfg, &file, &rules, true).unwrap();
    assert_eq!(
        applied.line(),
        "factory: join on, intake on, verify on, review on, stamp on, test on, door on"
    );
    // What a test needs on top of the file: a cheap proof of work, the repository, the fake API.
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    cfg.intake.reference_repo = Some(r.mirror.clone());
    cfg.verify.api = Some(SharedApi(fake.clone()));
    cfg.verify.timers = Timers {
        first_poll_ms: 20,
        max_poll_ms: 60,
        run_wait_ms: 400,
        time_limit_ms: 60_000,
    };
    assert_eq!(cfg.stamp.rules.tier1_run_agreed, 1);
    assert!(cfg.stamp.tier0_coding);

    let hub = Hub::open(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = {
        let hub = hub.clone();
        tokio::spawn(async move {
            hub::serve(hub, listener).await.unwrap();
        })
    };
    World {
        hub,
        orch: Opts {
            url: url.clone(),
            token: orch,
        },
        carol: Opts {
            url: url.clone(),
            token: carol,
        },
        url,
        dir,
        task,
    }
}

impl World {
    fn log(&self) -> Vec<Message> {
        std::fs::read_to_string(self.dir.path().join("bus.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn facts(&self, prefix: &str) -> Vec<Message> {
        self.log()
            .into_iter()
            .filter(|m| m.kind == Kind::Fact && m.topic.starts_with(prefix))
            .collect()
    }

    /// A copy of the tables of the hub, caught up with the log.
    fn store(&self) -> Store {
        let mut s = Store::open(self.dir.path()).unwrap();
        s.catch_up_from_log(&self.dir.path().join("bus.jsonl"))
            .unwrap();
        s
    }

    fn sub_state(&self, sha: &str) -> Option<String> {
        self.store()
            .connection()
            .query_row(
                "SELECT state FROM SUBMISSION WHERE sha256 = ?1",
                [sha],
                |r| r.get(0),
            )
            .ok()
    }

    async fn wait_sub(&self, sha: &str, want: &str) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let now = self.sub_state(sha);
            if now.as_deref() == Some(want) {
                return;
            }
            assert!(
                Instant::now() < end,
                "submission {sha} is {now:?}, wanted {want}; alerts: {:?}",
                self.log()
                    .iter()
                    .filter(|m| m.kind == Kind::Alert)
                    .map(|m| format!("{} {}", m.topic, m.body))
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn until(&self, what: &str, mut f: impl FnMut(&Store) -> bool) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if f(&self.store()) {
                return;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn tier(&self, member: &str) -> i64 {
        tier_from_table(self.store().connection(), member, None)
    }

    fn landing_refs(&self) -> Vec<String> {
        sommelier::land::query_landings(self.store().connection())
            .unwrap()
            .into_iter()
            .map(|l| l.ref_name)
            .collect()
    }

    fn task_row(&self, id: &str) -> sommelier::task::TaskRow {
        sommelier::task::query_task(self.store().connection(), id)
            .unwrap()
            .unwrap()
    }

    async fn orch_ok(&self, req: Value) -> Value {
        client::request_ok(&self.orch, req).await.unwrap()
    }

    /// An order that a tier 0 member may take: low risk, small size, min_trust 0. A low order names its
    /// files, because a glob that could meet a hold path makes the order guarded.
    async fn add_order(&self, id: &str, scope: &str, base: &str) {
        self.orch_ok(json!({
            "op": "task.add", "id": id, "title": format!("order {id}"), "kind": "fix",
            "scope": scope, "done_when": "cargo test -p demo", "base_sha": base,
            "risk": "low", "size": "small", "min_trust": 0,
        }))
        .await;
    }
}

/// A key member with its own key folder.
struct Person {
    _home: tempfile::TempDir,
    member: String,
    opts: Opts,
    key: MemberKey,
}

async fn person(url: &str, handle: &str) -> Person {
    let home = tempfile::tempdir().unwrap();
    let out = kc::join(url, home.path(), Some(handle)).await.unwrap();
    let file = kc::load(&kc::key_path(home.path())).unwrap();
    Person {
        opts: kc::opts_for(url, home.path()),
        member: out.member,
        key: file.key,
        _home: home,
    }
}

/// One request as `who` from the address `addr`: the door reads it from the trusted header, so a
/// member can sit in its own address block although every connection is a loopback one.
async fn call_from(url: &str, who: &Opts, addr: &str, req: Value) -> Result<Value, String> {
    let mut u = url.to_owned();
    u.push('/');
    u.push_str("?live=0");
    let mut request = u.into_client_request().unwrap();
    let auth = if kc::marker_path(&who.token).is_some() {
        kc::auth_header(url, &who.token).await.unwrap()
    } else {
        format!("Bearer {}", who.token)
    };
    request
        .headers_mut()
        .insert("authorization", auth.parse().unwrap());
    request
        .headers_mut()
        .insert(ADDRESS_HEADER, addr.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    let mut c = Conn::from_ws(ws);
    let v = c.call_ok(&req).await;
    c.close().await;
    v
}

fn machine() -> Value {
    json!({"os": "windows", "os_build": "26100", "gpu": "RTX 4080", "gpu_memory_mb": 16384,
           "driver": "552.22", "memory_mb": 32768})
}

fn signed_report(p: &Person, order: &Value, outcome: &str) -> Value {
    let mut v = report_of(&p.member, order, outcome);
    let sig = sign_request(&p.key, &v);
    v["sig"] = json!(sig);
    v
}

/// The report of a member, unsigned. A token member needs no signature.
fn report_of(member: &str, order: &Value, outcome: &str) -> Value {
    let report = Report {
        v: 1,
        run: order["id"].as_str().unwrap().into(),
        member: member.into(),
        build: order["build"].as_str().unwrap().into(),
        channel: order["channel"].as_str().unwrap().into(),
        profile: order["profile"].as_str().unwrap().into(),
        scenario: order["scenario"].as_str().unwrap().into(),
        machine: serde_json::from_value::<MachineClass>(machine()).unwrap(),
        outcome: if outcome == "works" {
            Outcome::Works
        } else {
            Outcome::Fails
        },
        first_error: None,
        metrics: BTreeMap::new(),
        secs_alive: 60,
        log_excerpt: "1 swapchain created\n".into(),
        at: sommelier::time::now_ms() / 1000,
        sig: None,
    };
    serde_json::to_value(report).unwrap()
}

const PASS: &str = r#"{"verdict":"pass","findings":[],"summary":"fine"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_factory_runs_from_join_to_landing_and_the_log_replays() {
    let r = repos();
    let fake = Arc::new(FakeGitHub::new());
    let w = start(&r, &fake).await;
    let out = tempfile::tempdir().unwrap();

    // 1. A stranger joins with a key and is tier 0.
    let stranger = person(&w.url, "stranger").await;
    assert_eq!(w.tier(&stranger.member), 0);
    let who = client::request_ok(&stranger.opts, json!({"op": "whoami"}))
        .await
        .unwrap();
    assert_eq!(who["name"], stranger.member.as_str());

    // 2. The owner adds an order a tier 0 member may take. The stranger takes it and submits a change
    //    inside its scope.
    w.add_order("t1", "src/lib.rs", &r.base).await;
    let row = w.task_row("t1");
    assert_eq!((row.risk.as_str(), row.min_trust), ("low", 0));
    let next = client::request_ok(&stranger.opts, json!({"op": "task.next"}))
        .await
        .unwrap();
    assert_eq!(next["found"], true, "{next}");
    assert_eq!(next["task"]["id"], "t1");
    // An order of the normal kind needs tier 2: a tier 0 member may not lease it.
    w.orch_ok(
        json!({"op": "task.add", "id": "t-big", "title": "a normal order", "kind": "fix",
                     "scope": "docs/other.md", "done_when": "cargo test", "base_sha": r.base}),
    )
    .await;
    let refused = client::request_ok(&stranger.opts, json!({"op": "task.work", "task": "t-big"}))
        .await
        .unwrap_err();
    assert!(refused.contains("needs tier 2"), "{refused}");
    client::request_ok(&stranger.opts, json!({"op": "task.work", "task": "t1"}))
        .await
        .unwrap();
    // A key member reads the brief of its order, which only a token could do before.
    sommelier::brief::put_brief_http(&w.orch, "t1", b"Add two().\n", None)
        .await
        .unwrap();
    let (brief, _) = sommelier::brief::get_brief_http(&stranger.opts, "t1", None)
        .await
        .unwrap();
    assert_eq!(brief, "Add two().\n");
    let text = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n";
    let (file, sha) = r.bundle(out.path(), "good", &[("src/lib.rs", text.to_owned())]);
    fake.set_runs(vec![run(77, &sha, "success")]);
    fake.set_jobs(77, vec![job("Run tests", "success")]);
    let sent = upload(&stranger.opts, "t1", 1, &file).await.unwrap();
    assert_eq!(sent["submission"], sha.as_str());

    // 3. The gates pass, the fake run passes.
    w.wait_sub(&sha, "verified").await;
    let gate = w.facts(&format!("gate/{sha}"));
    assert_eq!(
        serde_json::from_str::<Value>(&gate[0].body).unwrap()["passed"],
        true
    );
    assert_eq!(fake.dispatched().len(), 1);

    // 4. A token member (tier 2) that is not the author is assigned, and its verdict passes.
    // Carol sits in another address block than the stranger: a reviewer of the same block is no reviewer.
    let carol = |req: Value| call_from(&w.url, &w.carol, "198.51.100.9", req);
    carol(json!({"op": "send", "kind": "CHAT", "body": "carol is here"}))
        .await
        .unwrap();
    w.wait_sub(&sha, "reviewing").await;
    let mine = carol(json!({"op": "review.next"})).await.unwrap();
    assert_eq!(mine["submission"], sha.as_str(), "{mine}");
    let (task, n) = (mine["task"].as_str().unwrap(), mine["n"].as_u64().unwrap());
    assert_eq!(task, "t1");
    assert!(mine["prompt"].as_str().unwrap().contains("DATA:"));
    assert_eq!(sommelier::stamp::ops::tier_of(&w.hub, "carol"), 2);
    let done = carol(json!({"op": "review.verdict", "task": task, "n": n, "verdict": PASS}))
        .await
        .unwrap();
    assert_eq!(done["state"], "approved", "{done}");
    w.wait_sub(&sha, "approved").await;
    assert_eq!(w.landing_refs(), vec![format!("bundle:{sha}")]);

    // 5. The lander takes the entry and reports it as landed. The submission is landed and the
    //    stranger holds a `landed` stamp.
    let batch = w
        .orch_ok(json!({"op": "land.next", "claim": true, "batch_id": "b1", "base_sha": r.base}))
        .await;
    assert_eq!(batch["entries"][0]["ref"], format!("bundle:{sha}"));
    w.orch_ok(json!({"op": "land.result", "batch_id": "b1", "result": "landed"}))
        .await;
    w.wait_sub(&sha, "landed").await;
    let member = stranger.member.clone();
    w.until("the landed stamp", |s| {
        sommelier::stamp::query_stamps(s.connection())
            .unwrap()
            .iter()
            .any(|st| st.subject == member && st.kind.as_str() == "landed")
    })
    .await;

    // 6. A second submission touches a hold path: it stops at the gates and the order goes back.
    w.add_order("t2", "docs/readme.md", &r.base).await;
    client::request_ok(&stranger.opts, json!({"op": "task.work", "task": "t2"}))
        .await
        .unwrap();
    let (file2, sha2) = r.bundle(
        out.path(),
        "hold",
        &[(".github/workflows/ci.yml", "name: ci\n".to_owned())],
    );
    upload(&stranger.opts, "t2", 1, &file2).await.unwrap();
    w.wait_sub(&sha2, "gate_failed").await;
    let row = w.task_row("t2");
    assert_eq!((row.state.as_str(), row.attempt), ("ready", 2));
    assert!(
        row.resume_note.contains("hold_paths"),
        "{}",
        row.resume_note
    );
    assert_eq!(w.landing_refs(), vec![format!("bundle:{sha}")]);

    // 7. The test network on the same hub: three members in three blocks report works.
    let build = "a".repeat(40);
    let order = w
        .orch_ok(
            json!({"op": "test.order", "build": build, "channel": "main", "profile": "p1",
                        "scenario": "boot", "wanted": 3}),
        )
        .await;
    let order_id = order["id"].as_str().unwrap().to_owned();
    // Two fresh keys and the token member carol, who is vouched for by the owner. A ring of fresh
    // keys would not lift itself: one agreeing report must come from a member of tier 1 or above.
    let testers = [
        person(&w.url, "t-one").await,
        person(&w.url, "t-two").await,
    ];
    let carol = call_from(
        &w.url,
        &w.carol,
        "198.51.100.9",
        json!({"op": "test.next", "machine": machine(), "profiles": ["p1"]}),
    )
    .await
    .unwrap();
    assert_eq!(carol["order"]["id"], order_id.as_str(), "{carol}");
    call_from(
        &w.url,
        &w.carol,
        "198.51.100.9",
        json!({"op": "report.send", "report": report_of("carol", &carol["order"], "works")}),
    )
    .await
    .unwrap();
    for (i, p) in testers.iter().enumerate() {
        let addr = format!("203.0.{}.7", 110 + i);
        let got = call_from(
            &w.url,
            &p.opts,
            &addr,
            json!({"op": "test.next", "machine": machine(), "profiles": ["p1"]}),
        )
        .await
        .unwrap();
        assert_eq!(got["order"]["id"], order_id.as_str(), "{got}");
        let report = signed_report(p, &got["order"], "works");
        call_from(
            &w.url,
            &p.opts,
            &addr,
            json!({"op": "report.send", "report": report}),
        )
        .await
        .unwrap();
    }
    // The order settles and each tester gets a `run_agreed` stamp.
    w.until("the order to settle", |s| {
        sommelier::report::orders::query_orders(s.connection())
            .unwrap()
            .iter()
            .any(|o| o.id == order_id && o.state == "settled")
    })
    .await;
    let mut members: Vec<String> = testers.iter().map(|p| p.member.clone()).collect();
    members.push("carol".into());
    w.until("run_agreed for every tester", |s| {
        let stamps = sommelier::stamp::query_stamps(s.connection()).unwrap();
        members.iter().all(|m| {
            stamps
                .iter()
                .any(|st| &st.subject == m && st.kind.as_str() == "run_agreed")
        })
    })
    .await;
    // With the lowered numbers a tester reaches tier 1, and the log says why.
    let first = testers[0].member.clone();
    w.until("tier 1", |s| {
        tier_from_table(s.connection(), &first, None) == 1
    })
    .await;
    let fact = w
        .facts(&format!("tier/{first}"))
        .pop()
        .expect("a tier fact in the log");
    let body: Value = serde_json::from_str(&fact.body).unwrap();
    assert_eq!(body["tier"], 1);
    assert!(
        !body["reasons"].as_array().unwrap().is_empty(),
        "reasons: {body}"
    );
    // The stranger stays tier 0: one landed change is not what the lowered rule asks for.
    assert_eq!(w.tier(&stranger.member), 0);

    // 8. Replay: a fresh store rebuilt from the log has the same factory tables as the live one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let live = w.store().table_hashes().unwrap();
    let mut fresh = Store::open_in_memory().unwrap();
    fresh.apply_batch(&w.log()).unwrap();
    let again = fresh.table_hashes().unwrap();
    for table in [
        "KEY",
        "SUBMISSION",
        "GATE",
        "VERIFY",
        "REVIEW",
        "REVIEW_META",
        "LANDING",
        "BATCH",
        "STAMP",
        "TIER",
        "RUN_ORDER",
        "REPORT",
    ] {
        assert!(live.contains_key(table), "no table {table}: {live:?}");
        assert_eq!(live[table], again[table], "{table} differs after replay");
    }
    drop(w.hub.clone());
}
