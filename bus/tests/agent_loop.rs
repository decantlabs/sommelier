//! Integration tests for `sommelier agent run` supervisor lease loop, checkpoint forwarding,
//! rescue ref creation (`wip/<task>-<attempt>`), quota pool pause and exclusion, concurrent
//! supervisors, worktree sweep, `.pause` and `.gentle` markers, and role seat startup (step 12a).
//!
//! No test uses the network, a vendor login, or the live hub.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};
use sommelier::agent::classify::ExitReason;
use sommelier::agent::lease_loop::{
    SupervisorConfig, read_pause_and_gentle, run_supervisor, start_role_seat,
};
use sommelier::agent::sweep::{SweepAction, SweepContext, worktree_sweep};
use sommelier::brief;
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

static SEQ: AtomicU32 = AtomicU32::new(1);

fn temp_dir(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "sommelier-agent-loop-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Initializes a bare git fork remote and a working repo clone with an initial commit on `main`.
fn init_git_repo_and_fork(base: &Path) -> (PathBuf, PathBuf, String) {
    let fork_dir = base.join("fork.git");
    let clone_dir = base.join("clone");
    fs::create_dir_all(&fork_dir).unwrap();
    fs::create_dir_all(&clone_dir).unwrap();

    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&fork_dir)
            .args(["init", "--bare", "--initial-branch=main", "-q"])
            .status()
            .unwrap()
            .success()
    );

    for args in [
        &["init", "-q"][..],
        &["config", "user.name", "sommelier-test"][..],
        &["config", "user.email", "sommelier@local"][..],
        &["checkout", "-q", "-B", "main"][..],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&clone_dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    fs::write(clone_dir.join("README.md"), "initial\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["add", "README.md"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["commit", "-q", "-m", "initial commit"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["remote", "add", "origin", fork_dir.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["push", "-q", "origin", "main"])
            .status()
            .unwrap()
            .success()
    );

    let sha_out = Command::new("git")
        .arg("-C")
        .arg(&clone_dir)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .unwrap();
    let head_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_owned();
    (clone_dir, fork_dir, head_sha)
}

/// Starts an in-process test hub and returns `(hub_handle, hub_dir, orch_opts, sup1_opts, sup2_opts)`.
async fn start_test_hub(base: &Path) -> (tokio::task::JoinHandle<()>, PathBuf, Opts, Opts, Opts) {
    let hub_dir = base.join("hub");
    fs::create_dir_all(&hub_dir).unwrap();
    let mut tokens = TokenFile::default();
    let orch_tok = tokens.add("coord", Role::Orchestrator).unwrap();
    let sup1_tok = tokens.add("sup1", Role::Member).unwrap();
    let sup2_tok = tokens.add("sup2", Role::Member).unwrap();
    tokens.save(&hub_dir.join("tokens.json")).unwrap();

    let mut cfg = HubConfig::new(&hub_dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let _ = hub::run(cfg, listener).await;
    });

    (
        handle,
        hub_dir,
        Opts {
            url: url.clone(),
            token: orch_tok,
        },
        Opts {
            url: url.clone(),
            token: sup1_tok,
        },
        Opts {
            url,
            token: sup2_tok,
        },
    )
}

/// Writes a cross-platform fake hook script (`.cmd` on Windows, `.sh` on Unix).
fn write_fake_hook(dir: &Path, name: &str, body_win: &str, body_unix: &str) -> PathBuf {
    if cfg!(windows) {
        let path = dir.join(format!("{name}.cmd"));
        let content = format!("@echo off\r\n{body_win}\r\n");
        fs::write(&path, content).unwrap();
        path
    } else {
        let path = dir.join(format!("{name}.sh"));
        let content = format!("#!/bin/sh\nset -u\n{body_unix}\n");
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }
}

async fn fetch_api_state(opts: &Opts) -> Value {
    let mut url = reqwest::Url::parse(&opts.url).unwrap();
    url.set_scheme("http").unwrap();
    url.set_path("/api/state");
    let resp = reqwest::Client::new()
        .get(url)
        .bearer_auth(&opts.token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let bytes = resp.bytes().await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn task_runs_to_ready_and_is_delivered_with_guide_brief_prompt_and_checkpoint() {
    let dir = temp_dir("ready-delivered");
    let (clone_dir, _fork_dir, base_sha) = init_git_repo_and_fork(&dir);
    let (hub_handle, hub_dir, orch_opts, sup_opts, _) = start_test_hub(&dir).await;

    // Store a brief on the hub
    let brief_text = "TASK t-ready BRANCH gemini/t-ready\nTASK: implement shader view\nDO: add view and run tests\n";
    let rec = brief::put_brief(&hub_dir, "t-ready", brief_text.as_bytes(), None).unwrap();

    // Add a ready task on the hub
    client::request_ok(
        &orch_opts,
        json!({
            "op": "task.add",
            "id": "t-ready",
            "title": "implement shader view",
            "kind": "fix",
            "priority": "high",
            "scope": "src/view.rs",
            "accepts": "gemini",
            "branch": "gemini/t-ready",
            "brief_sha256": rec.sha256,
            "done_when": "cargo test passes",
        }),
    )
    .await
    .unwrap();

    // Fake hook:
    // 1. Verifies SOMMELIER_TOKEN is not leaked to the hook
    // 2. Writes a checkpoint to .sommelier/checkpoint.md
    // 3. Writes READY <branch> <sha> : cargo test passes to $SOMMELIER_OUT/last.txt
    let hook_win = format!(
        "if defined SOMMELIER_TOKEN exit /b 99\r\n\
         echo CHECKPOINT %SOMMELIER_TASK% attempt %SOMMELIER_ATTEMPT% 2026-11-25T12:00:00Z > \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo STATE: half done >> \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo BRANCH: %SOMMELIER_BRANCH% COMMIT: {base_sha} PUSHED: yes >> \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo READY %SOMMELIER_BRANCH% {base_sha} : cargo test passes > \"%SOMMELIER_OUT%\\last.txt\"\r\n\
         exit /b 0"
    );
    let hook_unix = format!(
        "[ -z \"${{SOMMELIER_TOKEN:-}}\" ] || exit 99\n\
         printf 'CHECKPOINT %s attempt %s 2026-11-25T12:00:00Z\\nSTATE: half done\\nBRANCH: %s COMMIT: {base_sha} PUSHED: yes\\n' \"$SOMMELIER_TASK\" \"$SOMMELIER_ATTEMPT\" \"$SOMMELIER_BRANCH\" > \"$SOMMELIER_WORKTREE/.sommelier/checkpoint.md\"\n\
         printf 'READY %s {base_sha} : cargo test passes\\n' \"$SOMMELIER_BRANCH\" > \"$SOMMELIER_OUT/last.txt\"\n\
         exit 0"
    );
    let fake_hook = write_fake_hook(&dir, "fake-hook-ready", &hook_win, &hook_unix);

    let mut classes = BTreeMap::new();
    classes.insert("gemini".into(), 2);
    let sup_cfg = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        classes,
        repo_clone: clone_dir,
        worktrees_dir: dir.join("worktrees"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: Some("origin".into()),
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup_opts.clone()),
        wait_secs: 1,
        heartbeat_ms: 40,
        max_iterations: Some(1),
        ..SupervisorConfig::default()
    };

    let summary = run_supervisor(&sup_cfg).await.unwrap();
    assert!(summary.hello_sent);
    assert_eq!(summary.tasks_run.len(), 1);
    let run = &summary.tasks_run[0];
    assert_eq!(run.task_id, "t-ready");
    assert_eq!(run.reason, ExitReason::Done);
    assert!(run.checkpoints_forwarded >= 1);

    // Verify assembled prompt has guide header + brief text
    let prompt_content = fs::read_to_string(&run.prompt_file).unwrap();
    assert!(
        prompt_content.starts_with("sommelier guide "),
        "prompt must start with guide header: {prompt_content}"
    );
    assert!(
        prompt_content.contains("implement shader view"),
        "prompt must include brief text: {prompt_content}"
    );

    // Verify task is now `delivered` on the hub, checkpoint is stored, and land enqueue fact was emitted
    let show = client::request_ok(&orch_opts, json!({"op": "task.show", "id": "t-ready"}))
        .await
        .unwrap();
    assert_eq!(show["task"]["state"], "delivered");
    assert_eq!(show["task"]["result_ref"], "gemini/t-ready");
    assert_eq!(show["task"]["result_sha"], base_sha);
    let cps = show["checkpoints"].as_array().unwrap();
    assert!(!cps.is_empty(), "checkpoint must be stored on the hub");

    let log_reply = client::request_ok(&orch_opts, json!({"op": "log"}))
        .await
        .unwrap();
    let has_land_fact = log_reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["kind"] == "FACT" && m["topic"] == "land/gemini/t-ready");
    assert!(has_land_fact, "must emit land/<ref> FACT on done");

    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn killed_child_leaves_rescue_ref_and_requeues_with_checkpoint() {
    let dir = temp_dir("killed-rescue");
    let (clone_dir, fork_dir, base_sha) = init_git_repo_and_fork(&dir);
    let (hub_handle, _hub_dir, orch_opts, sup_opts, _) = start_test_hub(&dir).await;

    client::request_ok(
        &orch_opts,
        json!({
            "op": "task.add",
            "id": "t-kill",
            "title": "long task killed mid-flight",
            "kind": "fix",
            "priority": "high",
            "scope": "src/mid.rs",
            "accepts": "codex",
            "branch": "codex/t-kill",
            "done_when": "tests pass",
        }),
    )
    .await
    .unwrap();

    // Fake hook:
    // 1. Writes a checkpoint to .sommelier/checkpoint.md
    // 2. Creates dirty uncommitted work in worktree (`dirty_progress.rs` and modifies `README.md`)
    // 3. Sleeps until killed by supervisor timeout
    let hook_win = format!(
        "echo CHECKPOINT t-kill attempt 1 2026-11-25T12:10:00Z > \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo STATE: wrote parser, about to compile >> \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo BRANCH: %SOMMELIER_BRANCH% COMMIT: {base_sha} PUSHED: no >> \"%SOMMELIER_WORKTREE%\\.sommelier\\checkpoint.md\"\r\n\
         echo modified > \"%SOMMELIER_WORKTREE%\\README.md\"\r\n\
         echo pub fn wip() {{}} > \"%SOMMELIER_WORKTREE%\\dirty_progress.rs\"\r\n\
         ping -n 10 127.0.0.1 >nul"
    );
    let hook_unix = format!(
        "printf 'CHECKPOINT t-kill attempt 1 2026-11-25T12:10:00Z\\nSTATE: wrote parser, about to compile\\nBRANCH: %s COMMIT: {base_sha} PUSHED: no\\n' \"$SOMMELIER_BRANCH\" > \"$SOMMELIER_WORKTREE/.sommelier/checkpoint.md\"\n\
         echo modified > \"$SOMMELIER_WORKTREE/README.md\"\n\
         echo 'pub fn wip() {{}}' > \"$SOMMELIER_WORKTREE/dirty_progress.rs\"\n\
         sleep 10"
    );
    let fake_hook = write_fake_hook(&dir, "fake-hook-kill", &hook_win, &hook_unix);

    let mut classes = BTreeMap::new();
    classes.insert("codex".into(), 1);
    let sup_cfg = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        classes,
        repo_clone: clone_dir,
        worktrees_dir: dir.join("worktrees"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: Some("origin".into()),
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup_opts),
        wait_secs: 1,
        heartbeat_ms: 30,
        child_timeout_ms: Some(250),
        max_iterations: Some(1),
        ..SupervisorConfig::default()
    };

    let summary = run_supervisor(&sup_cfg).await.unwrap();
    assert_eq!(summary.tasks_run.len(), 1);
    let run = &summary.tasks_run[0];
    assert_eq!(run.task_id, "t-kill");
    assert_eq!(run.reason, ExitReason::Killed);
    assert_eq!(run.wip_ref.as_deref(), Some("wip/t-kill-1"));

    // Verify rescue ref `refs/heads/wip/t-kill-1` exists in the worktree and was pushed to the fork remote
    let wt_ref = Command::new("git")
        .arg("-C")
        .arg(&run.worktree)
        .args(["rev-parse", "--verify", "refs/heads/wip/t-kill-1"])
        .output()
        .unwrap();
    assert!(
        wt_ref.status.success(),
        "refs/heads/wip/t-kill-1 must exist in worktree"
    );

    let fork_ref = Command::new("git")
        .arg("-C")
        .arg(&fork_dir)
        .args(["rev-parse", "--verify", "refs/heads/wip/t-kill-1"])
        .output()
        .unwrap();
    assert!(
        fork_ref.status.success(),
        "refs/heads/wip/t-kill-1 must be pushed to fork remote"
    );

    // Verify task is requeued on the hub with attempt = 2, wip_ref = wip/t-kill-1, and resume_note containing the checkpoint
    let show = client::request_ok(&orch_opts, json!({"op": "task.show", "id": "t-kill"}))
        .await
        .unwrap();
    assert_eq!(show["task"]["state"], "ready");
    assert_eq!(show["task"]["attempt"], 2);
    assert_eq!(show["task"]["wip_ref"], "wip/t-kill-1");
    let resume_note = show["task"]["resume_note"].as_str().unwrap_or("");
    assert!(
        resume_note.contains("wrote parser"),
        "resume_note must contain checkpoint: {resume_note}"
    );
    assert!(
        resume_note.contains("wip/t-kill-1"),
        "resume_note must mention rescue ref: {resume_note}"
    );

    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn quota_error_file_pauses_pool_and_requeues_with_exclusion() {
    let dir = temp_dir("quota-pause");
    let (clone_dir, _fork_dir, _base_sha) = init_git_repo_and_fork(&dir);
    let (hub_handle, _hub_dir, orch_opts, sup_opts, _) = start_test_hub(&dir).await;

    client::request_ok(
        &orch_opts,
        json!({
            "op": "task.add",
            "id": "t-quota",
            "title": "task hitting gemini quota",
            "kind": "fix",
            "priority": "high",
            "scope": "src/q.rs",
            "accepts": "gemini",
            "branch": "gemini/t-quota",
            "done_when": "tests pass",
        }),
    )
    .await
    .unwrap();

    // Fake hook writes a Gemini limit error message with `Resets in 2h4m47s` to $SOMMELIER_OUT/err.txt and exits 1
    let hook_win = "echo AGY_ERROR: {\"short_error\":\"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 2h4m47s.\", \"error_code\":429} > \"%SOMMELIER_OUT%\\err.txt\"\r\n\
                    exit /b 1";
    let hook_unix = "printf '%s\\n' 'AGY_ERROR: {\"short_error\":\"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 2h4m47s.\", \"error_code\":429}' > \"$SOMMELIER_OUT/err.txt\"\n\
                     exit 1";
    let fake_hook = write_fake_hook(&dir, "fake-hook-quota", hook_win, hook_unix);

    let mut classes = BTreeMap::new();
    classes.insert("gemini".into(), 2);
    let sup_cfg = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        classes,
        repo_clone: clone_dir,
        worktrees_dir: dir.join("worktrees"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: None,
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup_opts),
        wait_secs: 1,
        heartbeat_ms: 40,
        max_iterations: Some(1),
        ..SupervisorConfig::default()
    };

    let summary = run_supervisor(&sup_cfg).await.unwrap();
    assert_eq!(summary.tasks_run.len(), 1);
    assert_eq!(summary.tasks_run[0].reason, ExitReason::Quota);

    // 1. Verify task is requeued with attempt = 2 and `exclude_pools_until` populated for `agy.window`
    let show = client::request_ok(&orch_opts, json!({"op": "task.show", "id": "t-quota"}))
        .await
        .unwrap();
    assert_eq!(show["task"]["state"], "ready");
    assert_eq!(show["task"]["attempt"], 2);
    let excl = &show["task"]["exclude_pools_until"];
    let until = excl["agy.window"].as_u64().unwrap_or(0);
    assert!(
        until > 1_700_000_000,
        "exclude_pools_until must contain reset epoch for agy.window: {excl}"
    );

    // 2. Verify the pool `agy.window` is in state `paused` in `/api/state`
    let api_state = fetch_api_state(&orch_opts).await;
    let pools = api_state["pools"].as_array().unwrap();
    let agy_win = pools
        .iter()
        .find(|p| p["pool"] == "agy.window" && p["owner"] == "sup1")
        .expect("agy.window pool must exist in /api/state");
    assert_eq!(agy_win["state"], "paused");
    assert_eq!(agy_win["resets_at"].as_u64(), Some(until));

    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_supervisors_never_run_the_same_task() {
    let dir = temp_dir("two-supervisors");
    let (clone1, _fork_dir, base_sha) = init_git_repo_and_fork(&dir);
    let clone2 = dir.join("clone2");
    assert!(
        Command::new("git")
            .args([
                "clone",
                "-q",
                "-b",
                "main",
                dir.join("fork.git").to_str().unwrap(),
                clone2.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );

    let (hub_handle, _hub_dir, orch_opts, sup1_opts, sup2_opts) = start_test_hub(&dir).await;

    // Add 4 ready tasks with distinct scopes
    for i in 1..=4 {
        client::request_ok(
            &orch_opts,
            json!({
                "op": "task.add",
                "id": format!("t-par-{i}"),
                "title": format!("parallel task {i}"),
                "kind": "fix",
                "priority": "normal",
                "scope": format!("src/mod_{i}.rs"),
                "accepts": "codex",
                "branch": format!("codex/t-par-{i}"),
                "done_when": "tests pass",
            }),
        )
        .await
        .unwrap();
    }

    let hook_win = format!(
        "echo READY %SOMMELIER_BRANCH% {base_sha} : verified > \"%SOMMELIER_OUT%\\last.txt\"\r\n\
         exit /b 0"
    );
    let hook_unix = format!(
        "printf 'READY %s {base_sha} : verified\\n' \"$SOMMELIER_BRANCH\" > \"$SOMMELIER_OUT/last.txt\"\n\
         exit 0"
    );
    let fake_hook = write_fake_hook(&dir, "fake-hook-par", &hook_win, &hook_unix);

    let mut classes = BTreeMap::new();
    classes.insert("codex".into(), 2);

    let cfg1 = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        classes: classes.clone(),
        repo_clone: clone1,
        worktrees_dir: dir.join("wt1"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook.clone()),
        fork_remote: None,
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup1_opts),
        wait_secs: 2,
        heartbeat_ms: 40,
        max_iterations: Some(4),
        ..SupervisorConfig::default()
    };

    let cfg2 = SupervisorConfig {
        owner: "sup2".into(),
        machine: "box2".into(),
        classes,
        repo_clone: clone2,
        worktrees_dir: dir.join("wt2"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: None,
        data_dir: dir.join("sup2-data"),
        client_opts: Some(sup2_opts),
        wait_secs: 2,
        heartbeat_ms: 40,
        max_iterations: Some(4),
        ..SupervisorConfig::default()
    };

    let (res1, res2) = tokio::join!(run_supervisor(&cfg1), run_supervisor(&cfg2));
    let sum1 = res1.unwrap();
    let sum2 = res2.unwrap();

    let ids1: Vec<String> = sum1.tasks_run.iter().map(|r| r.task_id.clone()).collect();
    let ids2: Vec<String> = sum2.tasks_run.iter().map(|r| r.task_id.clone()).collect();

    // No task may appear in both supervisors' executed lists, and all 4 tasks must be delivered
    for id in &ids1 {
        assert!(
            !ids2.contains(id),
            "both supervisors ran the same task `{id}` (sup1={ids1:?}, sup2={ids2:?})"
        );
    }
    let mut all_ran: Vec<String> = ids1.into_iter().chain(ids2).collect();
    all_ran.sort();
    assert_eq!(
        all_ran,
        vec!["t-par-1", "t-par-2", "t-par-3", "t-par-4"],
        "every task must be run exactly once across the two supervisors"
    );

    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pause_and_gentle_files_zero_game_run_capacity_and_role_seat_starts_with_guide_and_takeover()
 {
    let dir = temp_dir("pause-gentle-role-sweep");
    let (clone_dir, _fork_dir, _base_sha) = init_git_repo_and_fork(&dir);
    let (hub_handle, _hub_dir, orch_opts, sup_opts, _) = start_test_hub(&dir).await;

    let runs_dir = dir.join("sommelier-runs");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(runs_dir.join(".pause"), "").unwrap();
    fs::write(runs_dir.join(".gentle"), "").unwrap();

    let pg = read_pause_and_gentle(Some(&runs_dir));
    assert!(pg.paused);
    assert!(pg.gentle);

    let fake_hook = write_fake_hook(
        &dir,
        "fake-hook-role",
        "copy /Y \"%SOMMELIER_PROMPT%\" \"%SOMMELIER_OUT%\\last.txt\" >nul\r\nexit /b 0",
        "cp \"$SOMMELIER_PROMPT\" \"$SOMMELIER_OUT/last.txt\"\nexit 0",
    );

    let mut classes = BTreeMap::new();
    classes.insert("claude_strong".into(), 1);
    classes.insert("game_run".into(), 1);

    let cfg = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        gpu_slots: 1,
        classes,
        accepts: vec!["fix".into(), "run".into()],
        repo_clone: clone_dir.clone(),
        worktrees_dir: dir.join("worktrees"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: Some("origin".into()),
        runs_dir: Some(runs_dir),
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup_opts),
        wait_secs: 0,
        max_iterations: Some(0),
        role_seat: Some("coordinator".into()),
        ..SupervisorConfig::default()
    };

    let (eff_classes, eff_gpu, eff_accepts, _) = cfg.effective_capacity();
    assert_eq!(eff_gpu, 0, ".pause must report gpu_slots = 0");
    assert_eq!(
        eff_classes.get("game_run").copied(),
        Some(0),
        ".pause must report 0 capacity for game_run"
    );
    assert!(
        !eff_accepts.contains(&"run".to_owned()),
        ".pause must remove `run` from accepted kinds"
    );

    // Start role seat and verify prompt contains both `sommelier guide` and `HANDOVER coordinator`
    let seat = start_role_seat(&cfg, "coordinator", "claude_strong")
        .await
        .unwrap();
    assert!(seat.prompt_text.starts_with("sommelier guide "));
    assert!(seat.prompt_text.contains("role coordinator"));
    assert!(seat.prompt_text.contains("HANDOVER coordinator epoch"));

    let summary = run_supervisor(&cfg).await.unwrap();
    assert!(summary.paused);
    assert!(summary.gentle);
    assert_eq!(summary.gpu_slots_reported, 0);
    assert!(summary.role_seat_started.is_some());

    // Also test worktree_sweep:
    // 1. A clean worktree for a closed task is removed
    // 2. A dirty worktree with no live child makes a rescue ref and emits DIRTY_WORKTREE alert
    let wt_dir = dir.join("worktrees");
    sommelier::agent::launch::prepare_worktree(
        &clone_dir,
        &wt_dir.join("wt-closed"),
        "codex/t-closed",
        None,
    )
    .unwrap();
    fs::write(
        wt_dir.join("wt-closed").join(".sommelier").join("task_id"),
        "t-closed",
    )
    .unwrap();

    sommelier::agent::launch::prepare_worktree(
        &clone_dir,
        &wt_dir.join("wt-dirty"),
        "codex/t-dirty",
        None,
    )
    .unwrap();
    fs::write(
        wt_dir.join("wt-dirty").join(".sommelier").join("task_id"),
        "t-dirty",
    )
    .unwrap();
    fs::write(wt_dir.join("wt-dirty").join("uncommitted.txt"), "dirty\n").unwrap();

    let mut sweep_ctx = SweepContext {
        repo_clone: Some(clone_dir),
        worktrees_dir: wt_dir.clone(),
        fork_remote: Some("origin".into()),
        ..SweepContext::default()
    };
    sweep_ctx.closed_tasks.insert("t-closed".into());
    sweep_ctx.task_attempts.insert("t-dirty".into(), 2);

    let sweep_rep = worktree_sweep(&sweep_ctx, 1_795_600_000).unwrap();
    assert!(
        !wt_dir.join("wt-closed").exists(),
        "clean worktree of closed task must be removed"
    );
    assert_eq!(
        sweep_rep.alerts.len(),
        1,
        "dirty worktree must emit DIRTY_WORKTREE alert"
    );
    assert_eq!(sweep_rep.alerts[0]["topic"], "alert/DIRTY_WORKTREE/t-dirty");
    assert!(sweep_rep.actions.iter().any(|a| matches!(
        a,
        SweepAction::RescuedDirty {
            task_id,
            wip_ref,
            ..
        } if task_id == "t-dirty" && wip_ref == "wip/t-dirty-2"
    )));

    let _ = orch_opts;
    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cached_brief_digest_mismatch_is_rejected_and_worktree_reuse_verifies_branch_and_base() {
    let dir = temp_dir("brief-mismatch-worktree-reuse");
    let (clone_dir, _fork_dir, base_sha) = init_git_repo_and_fork(&dir);

    // 1. Verify prepare_worktree resets an existing worktree when branch or base commit differs
    let wt_path = dir.join("wt-reuse");
    sommelier::agent::launch::prepare_worktree(&clone_dir, &wt_path, "codex/old-branch", None)
        .unwrap();
    assert_eq!(
        sommelier::agent::rescue::worktree_branch(&wt_path).as_deref(),
        Some("codex/old-branch")
    );

    // Advance clone_dir with a second commit and move wt_path to that commit on old-branch
    fs::write(clone_dir.join("second.txt"), "second\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["add", "second.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["commit", "-q", "-m", "second commit"])
            .status()
            .unwrap()
            .success()
    );

    // Re-preparing with a new branch and the initial base_sha must reset branch and commit
    sommelier::agent::launch::prepare_worktree(
        &clone_dir,
        &wt_path,
        "gemini/new-branch",
        Some(&base_sha),
    )
    .unwrap();
    assert_eq!(
        sommelier::agent::rescue::worktree_branch(&wt_path).as_deref(),
        Some("gemini/new-branch")
    );
    let wt_head = sommelier::agent::rescue::worktree_head_sha(&wt_path).unwrap();
    assert!(
        wt_head.starts_with(&base_sha),
        "worktree head {wt_head} must match requested base {base_sha}"
    );

    // 2. Verify make_rescue_ref captures untracked files even when tracked files are also modified
    fs::write(wt_path.join("README.md"), "modified tracked\n").unwrap();
    fs::write(wt_path.join("untracked_new.rs"), "pub fn new_file() {}\n").unwrap();
    let rescue = sommelier::agent::rescue::make_rescue_ref(&wt_path, "t-both", 1, None).unwrap();
    assert!(rescue.dirty);
    let rescue_sha = rescue.sha.unwrap();
    let tree_files = Command::new("git")
        .arg("-C")
        .arg(&wt_path)
        .args(["ls-tree", "--name-only", &rescue_sha])
        .output()
        .unwrap();
    let files_list = String::from_utf8_lossy(&tree_files.stdout);
    assert!(
        files_list.lines().any(|l| l.trim() == "untracked_new.rs"),
        "rescue commit must contain untracked_new.rs alongside tracked edits: {files_list}"
    );
    assert!(
        files_list.lines().any(|l| l.trim() == "README.md"),
        "rescue commit must contain README.md: {files_list}"
    );

    // 3. Verify fetch_task_brief rejects a cached brief whose SHA-256 does not match brief_sha
    let sup_data = dir.join("sup-data");
    fs::create_dir_all(&sup_data).unwrap();
    let tampered = b"TASK t-tampered\nDO: wrong instructions\n";
    brief::put_brief(&sup_data, "t-tampered", tampered, None).unwrap();
    let expected_sha = "a".repeat(64);
    let empty_opts = Opts {
        url: String::new(),
        token: String::new(),
    };
    let err = sommelier::agent::lease_loop::fetch_task_brief(
        &empty_opts,
        &sup_data,
        "t-tampered",
        &expected_sha,
    )
    .await
    .expect_err("cached brief with wrong sha256 must be rejected");
    assert!(
        err.contains("mismatch"),
        "expected mismatch error, got: {err}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn timeout_kills_spawned_child_process_tree() {
    let dir = temp_dir("timeout-tree-kill");
    let (clone_dir, _fork_dir, _base_sha) = init_git_repo_and_fork(&dir);
    let (hub_handle, _hub_dir, orch_opts, sup_opts, _) = start_test_hub(&dir).await;

    client::request_ok(
        &orch_opts,
        json!({
            "op": "task.add",
            "id": "t-tree",
            "title": "spawn grandchild and hang",
            "kind": "fix",
            "priority": "high",
            "scope": "src/tree.rs",
            "accepts": "codex",
            "branch": "codex/t-tree",
            "done_when": "killed cleanly",
        }),
    )
    .await
    .unwrap();

    let pid_file = dir.join("grandchild.pid");
    let hook_win = format!(
        "powershell -NoProfile -Command \"$p = Start-Process -FilePath ping -ArgumentList '-n','30','127.0.0.1' -PassThru -WindowStyle Hidden; Set-Content -Path '{}' -Value $p.Id; Start-Sleep -Seconds 30\"",
        pid_file.display()
    );
    let hook_unix = format!(
        "sleep 30 >/dev/null 2>&1 &\n\
         echo $! > \"{}\"\n\
         sleep 30",
        pid_file.display()
    );
    let fake_hook = write_fake_hook(&dir, "fake-hook-tree", &hook_win, &hook_unix);

    let mut classes = BTreeMap::new();
    classes.insert("codex".into(), 1);
    let sup_cfg = SupervisorConfig {
        owner: "sup1".into(),
        machine: "box1".into(),
        classes,
        repo_clone: clone_dir,
        worktrees_dir: dir.join("worktrees"),
        hooks_dir: dir.clone(),
        hook_override: Some(fake_hook),
        fork_remote: None,
        data_dir: dir.join("sup1-data"),
        client_opts: Some(sup_opts),
        wait_secs: 1,
        heartbeat_ms: 30,
        child_timeout_ms: Some(700),
        max_iterations: Some(1),
        ..SupervisorConfig::default()
    };

    let summary = run_supervisor(&sup_cfg).await.unwrap();
    assert_eq!(summary.tasks_run.len(), 1);
    assert_eq!(summary.tasks_run[0].reason, ExitReason::Killed);

    if let Ok(pid_str) = fs::read_to_string(&pid_file)
        && let Ok(gpid) = pid_str.trim().parse::<u32>()
    {
        // Give the OS a brief moment to reap the terminated process tree
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !sommelier::agent::children::is_pid_alive(gpid),
            "grandchild process {gpid} must be terminated when hook times out"
        );
    }

    hub_handle.abort();
    let _ = fs::remove_dir_all(&dir);
}
