//! The intake of submissions against a real hub on a free local port: upload, static gates, the clean
//! build with a fake GitHub, the open reads of the runner and the replay of the log. Small throwaway
//! git repositories, no network beyond localhost.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sommelier::bundle::{self, pack};
use sommelier::cli::submit::upload;
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::intake::InspectFn;
use sommelier::key::JoinRules;
use sommelier::key::client as kc;
use sommelier::key::crypto::sign_request;
use sommelier::msg::{Kind, Message, Role};
use sommelier::store::Store;
use sommelier::tokens::TokenFile;
use sommelier::verify::ops::SharedApi;
use sommelier::verify::{FakeGitHub, Job, JobStep, Run, Timers, VerifyRules};
use tokio::net::TcpListener;

const HUB_ADDRESS: &str = "https://hub.example.test";

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

/// A work repository with one base commit and a bare mirror of it for the hub.
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

    /// A harmless new document.
    fn doc(&self, out_dir: &Path, tag: &str) -> (PathBuf, String) {
        self.bundle(
            out_dir,
            tag,
            &[(&format!("docs/{tag}.md"), format!("{tag}\n"))],
        )
    }

    /// A harmless change in `src/lib.rs`.
    fn good(&self, out_dir: &Path, tag: &str) -> (PathBuf, String) {
        let text = format!(
            "pub fn one() -> u32 {{\n    1\n}}\n\npub fn two_{tag}() -> u32 {{\n    2\n}}\n"
        );
        self.bundle(out_dir, tag, &[("src/lib.rs", text)])
    }
}

struct TestHub {
    url: String,
    dir: tempfile::TempDir,
    orch: String,
    alice: String,
    task: tokio::task::JoinHandle<()>,
    hub: Arc<hub::Hub>,
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestHub {
    fn orch(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.orch.clone(),
        }
    }

    fn alice(&self) -> Opts {
        Opts {
            url: self.url.clone(),
            token: self.alice.clone(),
        }
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("bus.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn store_dir(&self) -> PathBuf {
        self.dir.path().join("bundles")
    }

    async fn state(&self) -> Value {
        get_json(
            &self.url,
            "/api/state",
            Some(&format!("Bearer {}", self.orch)),
        )
        .await
        .1
    }

    async fn submission(&self, sha: &str) -> Option<String> {
        let state = self.state().await;
        state["submissions"]["newest"]
            .as_array()?
            .iter()
            .find(|r| r["submission"] == sha)
            .and_then(|r| r["state"].as_str().map(str::to_owned))
    }

    async fn wait_state(&self, sha: &str, want: &str) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let now = self.submission(sha).await;
            if now.as_deref() == Some(want) {
                return;
            }
            assert!(
                Instant::now() < end,
                "submission {sha} is {now:?}, wanted {want}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn task_row(&self, id: &str) -> Value {
        self.state().await["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// A ready order with `scope`, based on the base commit.
    async fn add_task(&self, id: &str, scope: &str, base: &str) {
        client::request_ok(
            &self.orch(),
            json!({"op": "task.add", "id": id, "title": format!("order {id}"), "kind": "fix",
                   "scope": scope, "done_when": "cargo test -p demo", "base_sha": base}),
        )
        .await
        .unwrap();
    }

    /// Leases the order to alice.
    async fn lease_to_alice(&self, id: &str) {
        client::request_ok(&self.alice(), json!({"op": "task.work", "task": id}))
            .await
            .unwrap();
    }

    /// Leases the order to a key member. A tier 0 member may not run `task.work`, so the test writes the
    /// lease as the hub would. A client may not send a TASK_UPDATE.
    async fn lease_to_member(&self, id: &str, member: &str) {
        let far = sommelier::time::now_ms() / 1000 + 86_400;
        let body = json!({
            "v": 1, "task": id, "prev": "ready", "state": "leased", "attempt": 1,
            "by": "orch", "reason": "leased",
            "lease": {"task_id": id, "attempt": 1, "owner": member, "worker": member,
                      "state": "active", "expires_at": far, "heartbeat_at": far - 86_000},
        });
        self.hub
            .append_hook_message(Message {
                id: 0,
                ts: String::new(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{id}"),
                body: body.to_string(),
                refs: vec![],
                to: vec![],
            })
            .unwrap();
    }
}

fn config(dir: &Path, r: &Repos, tweak: impl FnOnce(&mut HubConfig)) -> HubConfig {
    let mut cfg = HubConfig::new(dir);
    cfg.verbose = false;
    cfg.sweep_every = Duration::from_millis(50);
    cfg.join = JoinRules {
        enabled: true,
        proof_of_work_bits: 8,
    };
    cfg.intake.enabled = true;
    cfg.intake.reference_repo = Some(r.mirror.clone());
    tweak(&mut cfg);
    cfg
}

async fn serve(cfg: HubConfig) -> (String, tokio::task::JoinHandle<()>, Arc<hub::Hub>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let handle = hub::Hub::open(cfg).unwrap();
    let serving = handle.clone();
    let task = tokio::spawn(async move {
        hub::serve(serving, listener).await.unwrap();
    });
    (url, task, handle)
}

async fn start(r: &Repos, tweak: impl FnOnce(&mut HubConfig)) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    t.save(&dir.path().join("tokens.json")).unwrap();
    let (url, task, handle) = serve(config(dir.path(), r, tweak)).await;
    TestHub {
        url,
        dir,
        orch,
        alice,
        task,
        hub: handle,
    }
}

/// Stops the hub and starts a new one on the same data folder.
async fn restart(hub: &mut TestHub, r: &Repos, tweak: impl FnOnce(&mut HubConfig)) {
    hub.task.abort();
    let _ = (&mut hub.task).await;
    let (url, task, handle) = serve(config(hub.dir.path(), r, tweak)).await;
    hub.url = url;
    hub.task = task;
    hub.hub = handle;
}

async fn get_json(url: &str, path: &str, auth: Option<&str>) -> (u16, Value) {
    let mut r = reqwest::Client::new().get(kc::http_url(url, path).unwrap());
    if let Some(a) = auth {
        r = r.header("authorization", a);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_bytes(url: &str, path: &str) -> (u16, Vec<u8>) {
    let resp = reqwest::Client::new()
        .get(kc::http_url(url, path).unwrap())
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.bytes().await.unwrap().to_vec())
}

/// A raw `PUT /api/submit/...`.
async fn put(
    url: &str,
    path: &str,
    query: &str,
    auth: Option<&str>,
    body: Vec<u8>,
) -> (u16, Value) {
    let mut u = kc::http_url(url, path).unwrap();
    u.set_query(Some(query));
    let mut r = reqwest::Client::new().put(u).body(body);
    if let Some(a) = auth {
        r = r.header("authorization", a);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn sha_of(bytes: &[u8]) -> String {
    sommelier::brief::sha256_hex(bytes)
}

fn error_of(v: &Value) -> String {
    v["error"].as_str().unwrap_or_default().to_owned()
}

fn facts_on(hub: &TestHub, prefix: &str) -> Vec<Message> {
    hub.log_lines()
        .iter()
        .filter_map(|l| serde_json::from_str::<Message>(l).ok())
        .filter(|m| m.kind == Kind::Fact && m.topic.starts_with(prefix))
        .collect()
}

#[tokio::test]
async fn a_hub_with_intake_off_takes_nothing() {
    let r = repos();
    let hub = start(&r, |c| c.intake.enabled = false).await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "off");
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    let body = std::fs::read(&file).unwrap();
    let (code, v) = put(
        &hub.url,
        "/api/submit/t1/1",
        &format!("sha256={sha}"),
        Some(&format!("Bearer {}", hub.alice)),
        body,
    )
    .await;
    assert_eq!(code, 503, "{v}");
    assert!(error_of(&v).contains("switched off"), "{v}");
    assert!(facts_on(&hub, "submission/").is_empty());
    assert!(!hub.store_dir().exists());
    // Reads answer not found, with no token needed.
    assert_eq!(
        get_bytes(&hub.url, &format!("/api/bundle/{sha}")).await.0,
        404
    );
}

#[tokio::test]
async fn uploads_without_a_right_are_refused() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "refused");
    let body = std::fs::read(&file).unwrap();
    let alice = format!("Bearer {}", hub.alice);
    let orch = format!("Bearer {}", hub.orch);
    let q = format!("sha256={sha}");
    hub.add_task("t1", "src/**", &r.base).await;

    // No credentials at all.
    let (code, _) = put(&hub.url, "/api/submit/t1/1", &q, None, body.clone()).await;
    assert_eq!(code, 401);
    // No lease on the order yet.
    let (code, v) = put(&hub.url, "/api/submit/t1/1", &q, Some(&alice), body.clone()).await;
    assert_eq!(code, 403, "{v}");
    assert!(error_of(&v).contains("no active lease"), "{v}");
    // The lease of another member.
    hub.lease_to_alice("t1").await;
    let (code, v) = put(&hub.url, "/api/submit/t1/1", &q, Some(&orch), body.clone()).await;
    assert_eq!(code, 403, "{v}");
    assert!(error_of(&v).contains("another member"), "{v}");
    // The wrong attempt.
    let (code, v) = put(&hub.url, "/api/submit/t1/2", &q, Some(&alice), body.clone()).await;
    assert_eq!(code, 409, "{v}");
    // The wrong hash.
    let wrong = format!("sha256={}", "0".repeat(64));
    let (code, v) = put(
        &hub.url,
        "/api/submit/t1/1",
        &wrong,
        Some(&alice),
        body.clone(),
    )
    .await;
    assert_eq!(code, 400, "{v}");
    assert!(error_of(&v).contains("SHA 256"), "{v}");
    // A body that is no bundle.
    let junk = b"not a bundle at all".to_vec();
    let jq = format!("sha256={}", sha_of(&junk));
    let (code, v) = put(&hub.url, "/api/submit/t1/1", &jq, Some(&alice), junk).await;
    assert_eq!(code, 400, "{v}");
    // The wrong method.
    let resp = reqwest::Client::new()
        .post(kc::http_url(&hub.url, "/api/submit/t1/1").unwrap())
        .header("authorization", &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405);

    assert!(facts_on(&hub, "submission/").is_empty());
    let leftovers = std::fs::read_dir(hub.store_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "a refused upload leaves nothing in the store");
}

#[tokio::test]
async fn a_body_over_the_cap_of_the_caller_is_refused() {
    let r = repos();
    // Tier 2 (a token member) may send 100 bytes, the highest tier 100 kilobytes.
    let hub = start(&r, |c| {
        c.intake.tier_bundle_bytes = vec![100, 100, 100, 100_000]
    })
    .await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "cap");
    let body = std::fs::read(&file).unwrap();
    assert!(body.len() > 100 && body.len() < 100_000);
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    let auth = format!("Bearer {}", hub.alice);
    let (code, v) = put(
        &hub.url,
        "/api/submit/t1/1",
        &format!("sha256={sha}"),
        Some(&auth),
        body,
    )
    .await;
    assert_eq!(code, 413, "{v}");
    assert!(error_of(&v).contains("limit for tier 2 is 100"), "{v}");
    // Larger than the cap of the highest tier: refused before the body is read at all.
    let big = vec![7u8; 200_000];
    let (code, v) = put(
        &hub.url,
        "/api/submit/t1/1",
        &format!("sha256={}", sha_of(&big)),
        Some(&auth),
        big,
    )
    .await;
    assert_eq!(code, 413, "{v}");
    assert!(facts_on(&hub, "submission/").is_empty());
}

#[tokio::test]
async fn a_token_member_upload_is_received_then_gated_and_a_second_upload_is_known() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "good");
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;

    let v = upload(&hub.alice(), "t1", 1, &file).await.unwrap();
    assert_eq!(v["submission"], sha.as_str());
    assert_eq!(v["state"], "received");
    assert_eq!(v["known"], false);
    hub.wait_state(&sha, "gated").await;

    let bundle = hub.store_dir().join(format!("{sha}.bundle"));
    let diff = hub.store_dir().join(format!("{sha}.diff.json"));
    assert_eq!(sha_of(&std::fs::read(&bundle).unwrap()), sha);
    let parsed: Value = serde_json::from_slice(&std::fs::read(&diff).unwrap()).unwrap();
    assert_eq!(parsed["base"], r.base.as_str());
    assert_eq!(parsed["files"][0]["path"], "src/lib.rs");

    let facts: Vec<String> = facts_on(&hub, "")
        .iter()
        .map(|m| m.topic.clone())
        .filter(|t| t.contains(&sha))
        .collect();
    assert_eq!(
        facts,
        vec![
            format!("submission/{sha}"),
            format!("gate/{sha}"),
            format!("submission/{sha}")
        ]
    );
    let gate: Value = serde_json::from_str(&facts_on(&hub, "gate/")[0].body).unwrap();
    assert_eq!(gate["passed"], true);

    // The same bundle again writes nothing new and answers with the known state.
    let before = hub.log_lines().len();
    let again = upload(&hub.alice(), "t1", 1, &file).await.unwrap();
    assert_eq!(again["known"], true);
    assert_eq!(again["state"], "gated");
    assert_eq!(hub.log_lines().len(), before);
    let files = std::fs::read_dir(hub.store_dir()).unwrap().count();
    assert_eq!(files, 3, "bundle, diff and gate report");

    // The member sees it, and /api/state counts it.
    let mine = client::request_ok(&hub.alice(), json!({"op": "submit.status"}))
        .await
        .unwrap();
    assert_eq!(mine["submissions"][0]["state"], "gated");
    assert_eq!(mine["submissions"][0]["task"], "t1");
    let state = hub.state().await;
    assert_eq!(state["submissions"]["counts"]["gated"], 1);
    // With verify off the submission stays gated.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.submission(&sha).await.as_deref(), Some("gated"));
}

#[tokio::test]
async fn a_key_member_needs_a_good_signature_and_then_gets_through() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "key");
    let home = tempfile::tempdir().unwrap();
    let joined = kc::join(&hub.url, home.path(), Some("kay")).await.unwrap();
    let member = joined.member.clone();
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_member("t1", &member).await;
    let opts = kc::opts_for(&hub.url, home.path());
    let body = std::fs::read(&file).unwrap();
    let q = format!("sha256={sha}");
    let auth = |o: &Opts| {
        let (url, token) = (o.url.clone(), o.token.clone());
        async move { kc::auth_header(&url, &token).await.unwrap() }
    };

    // No signature.
    let h = auth(&opts).await;
    let (code, v) = put(&hub.url, "/api/submit/t1/1", &q, Some(&h), body.clone()).await;
    assert_eq!(code, 401, "{v}");
    assert!(error_of(&v).contains("sig"), "{v}");
    // A signature of another key.
    let other = sommelier::key::crypto::MemberKey::generate().unwrap();
    let request = json!({"op": "submit", "task": "t1", "attempt": 1, "sha256": sha});
    let bad = format!("{q}&sig={}", sign_request(&other, &request));
    let h = auth(&opts).await;
    let (code, v) = put(&hub.url, "/api/submit/t1/1", &bad, Some(&h), body.clone()).await;
    assert_eq!(code, 401, "{v}");
    assert!(error_of(&v).contains("signature"), "{v}");
    // A signature made for another order.
    let file_key = kc::load(&kc::key_path(home.path())).unwrap();
    let elsewhere = json!({"op": "submit", "task": "t2", "attempt": 1, "sha256": sha});
    let wrong = format!("{q}&sig={}", sign_request(&file_key.key, &elsewhere));
    let h = auth(&opts).await;
    let (code, _) = put(&hub.url, "/api/submit/t1/1", &wrong, Some(&h), body.clone()).await;
    assert_eq!(code, 401);
    assert!(facts_on(&hub, "submission/").is_empty());

    // The good way.
    let v = upload(&opts, "t1", 1, &file).await.unwrap();
    assert_eq!(v["state"], "received");
    hub.wait_state(&sha, "gated").await;
    let row = hub.state().await["submissions"]["newest"][0].clone();
    assert_eq!(row["member"], member.as_str());
    assert_eq!(row["tier"], 0);
    assert!(hub.store_dir().join(format!("{sha}.diff.json")).is_file());
    // A tier 0 member may ask for its submissions.
    let mine = client::request_ok(&opts, json!({"op": "submit.status"}))
        .await
        .unwrap();
    assert_eq!(mine["submissions"][0]["submission"], sha.as_str());
}

#[tokio::test]
async fn patches_the_gates_stop_end_in_gate_failed_and_the_order_goes_back() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let out = tempfile::tempdir().unwrap();
    let secret_line = format!("// -----BEGIN {} PRIVATE KEY-----\n", "RSA");

    // (task, scope, files, the gate that must be among the findings)
    type Case<'a> = (&'a str, &'a str, Vec<(&'a str, String)>, &'a str);
    let cases: Vec<Case> = vec![
        (
            "t-scope",
            "src/**",
            vec![("docs/readme.md", "hello\nmore\n".to_owned())],
            "scope",
        ),
        (
            "t-hold",
            "docs/**",
            vec![(".github/workflows/ci.yml", "name: ci\n".to_owned())],
            "hold_paths",
        ),
        (
            "t-secret",
            "src/**",
            vec![("src/key.rs", secret_line.clone())],
            "secrets",
        ),
    ];
    for (task, scope, files, gate) in cases {
        let (file, sha) = r.bundle(out.path(), task, &files);
        hub.add_task(task, scope, &r.base).await;
        hub.lease_to_alice(task).await;
        upload(&hub.alice(), task, 1, &file).await.unwrap();
        hub.wait_state(&sha, "gate_failed").await;

        let row = hub.task_row(task).await;
        assert_eq!(row["state"], "ready", "{task}: {row}");
        assert_eq!(row["attempt"], 2, "{task}: {row}");
        let note = row["resume_note"].as_str().unwrap();
        assert!(note.contains(gate), "{task}: {note}");
        assert!(
            !note.contains("PRIVATE KEY"),
            "the note never repeats a secret: {note}"
        );

        let report = facts_on(&hub, "gate/")
            .into_iter()
            .find(|m| m.topic == format!("gate/{sha}"))
            .map(|m| serde_json::from_str::<Value>(&m.body).unwrap())
            .expect("a gate fact");
        assert_eq!(report["passed"], false);
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["gate"] == gate),
            "{task}: {report}"
        );
        // A failed bundle is not served.
        assert_eq!(
            get_bytes(&hub.url, &format!("/api/bundle/{sha}")).await.0,
            404
        );
        assert_eq!(
            get_bytes(&hub.url, &format!("/api/order/{task}/done_when"))
                .await
                .0,
            404
        );
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

fn with_verify(fake: &Arc<FakeGitHub>, c: &mut HubConfig) {
    c.verify.enabled = true;
    c.verify.repository = "owner/project".into();
    c.verify.public_address = HUB_ADDRESS.into();
    c.verify.api = Some(SharedApi(fake.clone()));
    c.verify.rules = VerifyRules::default();
    c.verify.timers = Timers {
        first_poll_ms: 20,
        max_poll_ms: 60,
        run_wait_ms: 400,
        time_limit_ms: 60_000,
    };
}

#[tokio::test]
async fn the_clean_build_passes_fails_retries_and_gives_up() {
    let r = repos();
    let fake = Arc::new(FakeGitHub::new());
    let hub = start(&r, |c| with_verify(&fake, c)).await;
    let out = tempfile::tempdir().unwrap();

    // 1. A passing run.
    let (f1, s1) = r.good(out.path(), "pass");
    fake.set_runs(vec![run(77, &s1, "success")]);
    fake.set_jobs(77, vec![job("Run tests", "success")]);
    hub.add_task("t-pass", "src/**", &r.base).await;
    hub.lease_to_alice("t-pass").await;
    upload(&hub.alice(), "t-pass", 1, &f1).await.unwrap();
    hub.wait_state(&s1, "verified").await;
    let dispatched = fake.dispatched();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].submission, s1);
    assert_eq!(dispatched[0].order, "t-pass");
    assert_eq!(dispatched[0].base, r.base);
    assert_eq!(dispatched[0].hub, HUB_ADDRESS);
    assert_eq!(hub.task_row("t-pass").await["state"], "leased");

    // 2. A failing run: the order goes back with the failing step.
    let (f2, s2) = r.doc(out.path(), "fail");
    fake.set_runs(vec![run(77, &s1, "success"), run(78, &s2, "failure")]);
    fake.set_jobs(78, vec![job("Run tests", "failure")]);
    hub.add_task("t-fail", "docs/**", &r.base).await;
    hub.lease_to_alice("t-fail").await;
    upload(&hub.alice(), "t-fail", 1, &f2).await.unwrap();
    hub.wait_state(&s2, "verify_failed").await;
    let row = hub.task_row("t-fail").await;
    assert_eq!(row["state"], "ready", "{row}");
    assert_eq!(row["attempt"], 2);
    assert!(
        row["resume_note"].as_str().unwrap().contains("Run tests"),
        "{row}"
    );

    // The retry of an orchestrator runs it again, now with a good run.
    fake.set_runs(vec![run(79, &s2, "success")]);
    fake.set_jobs(79, vec![job("Run tests", "success")]);
    let denied = client::request(&hub.alice(), json!({"op": "verify.retry", "sha256": s2}))
        .await
        .unwrap();
    assert_eq!(denied["ok"], false);
    client::request_ok(&hub.orch(), json!({"op": "verify.retry", "sha256": s2}))
        .await
        .unwrap();
    hub.wait_state(&s2, "verified").await;

    // 3. A dispatch that never shows a run: one retry, then it gives up.
    let (f3, s3) = r.doc(out.path(), "lost");
    hub.add_task("t-lost", "docs/**", &r.base).await;
    hub.lease_to_alice("t-lost").await;
    let before = fake.dispatched().len();
    upload(&hub.alice(), "t-lost", 1, &f3).await.unwrap();
    hub.wait_state(&s3, "verify_failed").await;
    let mine: Vec<_> = fake
        .dispatched()
        .into_iter()
        .filter(|d| d.submission == s3)
        .collect();
    assert_eq!(mine.len(), 2, "one try and one retry, {before} before");
    let last = facts_on(&hub, "verify/")
        .into_iter()
        .rfind(|m| m.topic == format!("verify/{s3}"))
        .unwrap();
    let body: Value = serde_json::from_str(&last.body).unwrap();
    assert_eq!(body["state"], "done");
    assert_eq!(body["conclusion"], "no_run");
    assert_eq!(body["dispatches"], 2);
    let row = hub.task_row("t-lost").await;
    assert_eq!(row["state"], "ready", "{row}");
    assert!(
        row["resume_note"]
            .as_str()
            .unwrap()
            .contains("did not start"),
        "{row}"
    );
}

#[tokio::test]
async fn the_runner_reads_the_bundle_and_the_order_without_a_token() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "serve");
    hub.add_task("t1", "src/**, docs/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    // Before the gates: nothing is served.
    upload(&hub.alice(), "t1", 1, &file).await.unwrap();
    hub.wait_state(&sha, "gated").await;

    let (code, bytes) = get_bytes(&hub.url, &format!("/api/bundle/{sha}")).await;
    assert_eq!(code, 200);
    assert_eq!(sha_of(&bytes), sha);
    assert_eq!(bytes, std::fs::read(&file).unwrap());
    // An unknown hash and a malformed one.
    assert_eq!(
        get_bytes(&hub.url, &format!("/api/bundle/{}", "e".repeat(64)))
            .await
            .0,
        404
    );
    assert_eq!(
        get_bytes(&hub.url, "/api/bundle/..%2f..%2ftokens.json")
            .await
            .0,
        404
    );

    let (code, order) = get_json(&hub.url, "/api/order/t1/done_when", None).await;
    assert_eq!(code, 200, "{order}");
    assert_eq!(order["task"], "t1");
    assert_eq!(order["base"], r.base.as_str());
    assert_eq!(order["scope"], json!(["src/**", "docs/**"]));
    assert_eq!(order["done_when"], "cargo test -p demo");
    assert_eq!(
        get_json(&hub.url, "/api/order/nope/done_when", None)
            .await
            .0,
        404
    );
    // Writing is not allowed on the open paths.
    let resp = reqwest::Client::new()
        .put(kc::http_url(&hub.url, &format!("/api/bundle/{sha}")).unwrap())
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405);
}

#[tokio::test]
async fn a_client_fact_on_the_factory_topics_projects_nothing() {
    let r = repos();
    let hub = start(&r, |_| {}).await;
    let sha = "a".repeat(64);
    let body = json!({"v": 1, "submission": sha, "task": "t1", "attempt": 1, "member": "alice",
                      "base": r.base, "head": r.base, "bytes": 1, "state": "verified",
                      "risk": "normal", "tier": 2, "at": 1});
    for topic in [
        format!("submission/{sha}"),
        format!("gate/{sha}"),
        format!("verify/{sha}"),
    ] {
        for who in [hub.alice(), hub.orch()] {
            // The hub may refuse the message itself or keep it in the log. Either way no row appears.
            let _ = client::request(
                &who,
                json!({"op": "send", "kind": "FACT", "topic": topic, "body": body.to_string()}),
            )
            .await;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let state = hub.state().await;
    assert_eq!(state["submissions"]["counts"], json!({}));
    assert_eq!(state["submissions"]["newest"], json!([]));

    // The same lines in a log project nothing, from any role but the hub, and a body that names
    // another hash projects nothing from the hub either.
    let mut store = Store::open_in_memory().unwrap();
    let line = |id: u64, role: Role, topic: &str, body: &Value| Message {
        id,
        ts: "2026-11-25T10:00:00.000Z".into(),
        from: "x".into(),
        role,
        kind: Kind::Fact,
        topic: topic.into(),
        body: body.to_string(),
        refs: vec![],
        to: vec![],
    };
    let mut id = 0;
    for role in [Role::Member, Role::Orchestrator, Role::Observer] {
        for topic in [
            format!("submission/{sha}"),
            format!("gate/{sha}"),
            format!("verify/{sha}"),
        ] {
            id += 1;
            store.apply(&line(id, role, &topic, &body)).unwrap();
        }
    }
    let other = "b".repeat(64);
    id += 1;
    store
        .apply(&line(id, Role::Hub, &format!("submission/{other}"), &body))
        .unwrap();
    for table in ["SUBMISSION", "GATE", "VERIFY"] {
        let n: i64 = store
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    // And the real thing from the hub does project.
    id += 1;
    store
        .apply(&line(id, Role::Hub, &format!("submission/{sha}"), &body))
        .unwrap();
    let n: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM SUBMISSION", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn replaying_the_log_gives_the_same_tables() {
    let r = repos();
    let fake = Arc::new(FakeGitHub::new());
    let hub = start(&r, |c| with_verify(&fake, c)).await;
    let out = tempfile::tempdir().unwrap();
    let (f1, s1) = r.good(out.path(), "one");
    let (f2, s2) = r.bundle(out.path(), "two", &[("src/other.rs", "x\n".into())]);
    fake.set_runs(vec![run(5, &s1, "success"), run(6, &s2, "failure")]);
    fake.set_jobs(5, vec![job("Build", "success")]);
    fake.set_jobs(6, vec![job("Build", "failure")]);
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    upload(&hub.alice(), "t1", 1, &f1).await.unwrap();
    hub.add_task("t2", "docs/**", &r.base).await;
    hub.lease_to_alice("t2").await;
    upload(&hub.alice(), "t2", 1, &f2).await.unwrap();
    hub.wait_state(&s1, "verified").await;
    hub.wait_state(&s2, "gate_failed").await;

    // The tables of the hub, read from its own database after a catch up.
    let mut live = Store::open(hub.dir.path()).unwrap();
    live.catch_up_from_log(&hub.dir.path().join("bus.jsonl"))
        .unwrap();
    let live = live.table_hashes().unwrap();

    let mut replayed = Store::open_in_memory().unwrap();
    let msgs: Vec<Message> = hub
        .log_lines()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    replayed.apply_batch(&msgs).unwrap();
    let again = replayed.table_hashes().unwrap();
    for table in ["SUBMISSION", "GATE", "VERIFY"] {
        assert_eq!(live[table], again[table], "{table}");
    }
    let n: i64 = replayed
        .connection()
        .query_row("SELECT COUNT(*) FROM VERIFY", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "only the submission that passed the gates was built");
}

#[tokio::test]
async fn the_hub_answers_while_a_gate_run_is_busy() {
    let r = repos();
    let hub = start(&r, |c| {
        c.intake.inspector = Some(InspectFn(Arc::new(|bundle_file, repo, base, limits| {
            std::thread::sleep(Duration::from_secs(3));
            bundle::inspect(bundle_file, repo, base, limits)
        })));
    })
    .await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "slow");
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    upload(&hub.alice(), "t1", 1, &file).await.unwrap();
    // Give the tick time to start the run, then ask the hub something.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(hub.submission(&sha).await.as_deref(), Some("received"));
    let t = Instant::now();
    let who = client::request_ok(&hub.alice(), json!({"op": "whoami"}))
        .await
        .unwrap();
    assert_eq!(who["name"], "alice");
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "whoami took {:?}",
        t.elapsed()
    );
    assert_eq!(hub.submission(&sha).await.as_deref(), Some("received"));
    hub.wait_state(&sha, "gated").await;
}

#[tokio::test]
async fn a_restart_picks_the_clean_build_up_where_it_was() {
    let r = repos();
    let before = Arc::new(FakeGitHub::new());
    let mut hub = start(&r, |c| {
        with_verify(&before, c);
        // The hub that stops must not give up on its own while the test runs.
        c.verify.timers.run_wait_ms = 600_000;
    })
    .await;
    let out = tempfile::tempdir().unwrap();
    let (file, sha) = r.good(out.path(), "restart");
    hub.add_task("t1", "src/**", &r.base).await;
    hub.lease_to_alice("t1").await;
    upload(&hub.alice(), "t1", 1, &file).await.unwrap();
    hub.wait_state(&sha, "verifying").await;
    assert_eq!(before.dispatched().len(), 1);

    let after = Arc::new(FakeGitHub::new());
    after.set_runs(vec![run(9, &sha, "success")]);
    after.set_jobs(9, vec![job("Run tests", "success")]);
    restart(&mut hub, &r, |c| with_verify(&after, c)).await;
    hub.wait_state(&sha, "verified").await;
    assert!(
        after.dispatched().is_empty(),
        "the new hub reads the VERIFY row and does not dispatch again"
    );
    let last = facts_on(&hub, "verify/").pop().unwrap();
    let body: Value = serde_json::from_str(&last.body).unwrap();
    assert_eq!(body["conclusion"], "passed");
    assert_eq!(body["run_id"], 9);
}
