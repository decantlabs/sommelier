//! Tests for `sommelier agent exec`, child process tracking via `sysinfo`, exit capture,
//! classification table across vendor fixtures, outbox FIFO flushing, and disk guard (step 7).
//!
//! No test uses the network, a vendor login, or the live hub. Fixtures hold no account id,
//! email, token, or user folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use sommelier::agent::capture::ExitCapture;
use sommelier::agent::children::{ChildRecord, ChildrenStore, is_pid_alive};
use sommelier::agent::classify::{ClassifyInput, ExitReason, classify_exit, parse_resets_in_duration};
use sommelier::agent::disk::{DiskGuardConfig, check_disk};
use sommelier::agent::exec::{ExecConfig, exec_worker};
use sommelier::agent::outbox::Outbox;
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

static TEST_SEQ: AtomicU32 = AtomicU32::new(1);

fn temp_test_dir(label: &str) -> PathBuf {
    let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "sommelier-agent-test-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Cross-platform fake child helper: builds a command that works on both Windows and Linux
/// using the compiled `sommelier` binary or standard shell (`cmd /c` on Windows, `sh -c` on Unix).
fn fake_child_command(script_win: &str, script_unix: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), script_win.into()]
    } else {
        vec!["sh".into(), "-c".into(), script_unix.into()]
    }
}

const FIXTURE_GEMINI_QUOTA_ERR: &str = r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please wait before trying again. Resets in 2h4m47s.", "error_code":429, "retryable":true}"#;
const FIXTURE_GEMINI_WEEKLY_ERR: &str = r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 167h13m26s.", "error_code":429, "retryable":true}"#;
const FIXTURE_CODEX_QUOTA_ERR: &str = r#"{"type":"error","rateLimitReachedType":"plus_5h_limit","error":{"type":"usage_limit_reached","code":"rate_limit_exceeded","message":"You've hit your usage limit.","resets_at":1795608947}}"#;
const FIXTURE_CLAUDE_QUOTA_EVENTS: &str = r#"{"type":"rate_limit_event","status":"rejected","rateLimitType":"five_hour","resetsAt":1795610000}"#;

#[test]
fn fixtures_contain_no_pii_or_secrets() {
    for fixture in [
        FIXTURE_GEMINI_QUOTA_ERR,
        FIXTURE_GEMINI_WEEKLY_ERR,
        FIXTURE_CODEX_QUOTA_ERR,
        FIXTURE_CLAUDE_QUOTA_EVENTS,
    ] {
        assert!(!fixture.contains('@'), "fixture contains an email sign");
        assert!(!fixture.contains("accountId"), "fixture contains accountId");
        assert!(!fixture.contains("sb_"), "fixture contains a token prefix");
        assert!(
            !fixture.contains("C:\\Users") && !fixture.contains("/home/"),
            "fixture contains a user folder"
        );
    }
}

#[test]
fn classification_table_covers_all_reasons_and_vendors() {
    let now_sec = 1_795_600_000;
    assert_eq!(parse_resets_in_duration("Resets in 2h4m47s"), Some(7487));
    assert_eq!(
        parse_resets_in_duration("Resets in 167h13m26s"),
        Some(167 * 3600 + 13 * 60 + 26)
    );

    struct Case {
        name: &'static str,
        last_txt: &'static str,
        err_txt: &'static str,
        events_txt: &'static str,
        handover: bool,
        killed: bool,
        restarted: bool,
        exit_code: Option<i32>,
        expected_reason: ExitReason,
        expected_vendor: Option<&'static str>,
        expected_resets_at: Option<u64>,
    }

    let cases = [
        // 1. Killed by supervisor takes highest precedence even if last.txt has READY or err has quota
        Case {
            name: "killed_by_supervisor",
            last_txt: "READY gemini/agent-exec 0123456 : tests pass",
            err_txt: FIXTURE_GEMINI_QUOTA_ERR,
            events_txt: "",
            handover: false,
            killed: true,
            restarted: false,
            exit_code: Some(137),
            expected_reason: ExitReason::Killed,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 2. Gemini (agy) 5-hour quota limit error file with parsed reset time
        Case {
            name: "quota_gemini_5h",
            last_txt: "",
            err_txt: FIXTURE_GEMINI_QUOTA_ERR,
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(1),
            expected_reason: ExitReason::Quota,
            expected_vendor: Some("gemini"),
            expected_resets_at: Some(now_sec + 7487),
        },
        // 3. Gemini (agy) weekly quota limit error file (167h13m26s)
        Case {
            name: "quota_gemini_weekly",
            last_txt: "",
            err_txt: FIXTURE_GEMINI_WEEKLY_ERR,
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(1),
            expected_reason: ExitReason::Quota,
            expected_vendor: Some("gemini"),
            expected_resets_at: Some(now_sec + 167 * 3600 + 13 * 60 + 26),
        },
        // 4. Codex quota error file with resets_at epoch
        Case {
            name: "quota_codex",
            last_txt: "",
            err_txt: FIXTURE_CODEX_QUOTA_ERR,
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(1),
            expected_reason: ExitReason::Quota,
            expected_vendor: Some("codex"),
            expected_resets_at: Some(1_795_608_947),
        },
        // 5. Claude stream-json rate_limit_event rejected
        Case {
            name: "quota_claude_stream",
            last_txt: "",
            err_txt: "",
            events_txt: FIXTURE_CLAUDE_QUOTA_EVENTS,
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(1),
            expected_reason: ExitReason::Quota,
            expected_vendor: Some("claude"),
            expected_resets_at: Some(1_795_610_000),
        },
        // 6. Done: READY first line and check-result passes
        Case {
            name: "done_valid_ready",
            last_txt: "READY gemini/agent-exec 0123456 : cargo test passed\nAll checks clean.",
            err_txt: "",
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(0),
            expected_reason: ExitReason::Done,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 7. Invalid READY (wrong branch pushed) -> classified as Stuck ("result invalid") per 4.2 / 5.4
        Case {
            name: "ready_wrong_branch_becomes_stuck",
            last_txt: "READY gemini/other-branch 0123456 : wrong branch",
            err_txt: "",
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(0),
            expected_reason: ExitReason::Stuck,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 8. Explicit STUCK first line
        Case {
            name: "stuck_first_line",
            last_txt: "STUCK gemini/agent-exec : missing dependency branch",
            err_txt: "",
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(0),
            expected_reason: ExitReason::Stuck,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 9. Handover recorded -> context_full
        Case {
            name: "context_full_handover",
            last_txt: "checkpoint saved before context limit",
            err_txt: "",
            events_txt: "",
            handover: true,
            killed: false,
            restarted: false,
            exit_code: Some(0),
            expected_reason: ExitReason::ContextFull,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 10. Process exited with non-zero and no output -> crash
        Case {
            name: "crash_nonzero_exit",
            last_txt: "",
            err_txt: "segmentation fault",
            events_txt: "",
            handover: false,
            killed: false,
            restarted: false,
            exit_code: Some(139),
            expected_reason: ExitReason::Crash,
            expected_vendor: None,
            expected_resets_at: None,
        },
        // 11. Supervisor restarted while process disappeared -> lost
        Case {
            name: "lost_on_supervisor_restart",
            last_txt: "",
            err_txt: "",
            events_txt: "",
            handover: false,
            killed: false,
            restarted: true,
            exit_code: None,
            expected_reason: ExitReason::Lost,
            expected_vendor: None,
            expected_resets_at: None,
        },
    ];

    for c in cases {
        let dir = temp_test_dir(c.name);
        if !c.last_txt.is_empty() {
            fs::write(dir.join("last.txt"), c.last_txt).unwrap();
        }
        if !c.err_txt.is_empty() {
            fs::write(dir.join("err.txt"), c.err_txt).unwrap();
        }
        if !c.events_txt.is_empty() {
            fs::write(dir.join("events.jsonl"), c.events_txt).unwrap();
        }
        if c.handover {
            fs::write(dir.join("handover"), "1").unwrap();
        }

        let capture = ExitCapture::collect(&dir, None, None, "gemini/agent-exec");
        let verdict = classify_exit(&ClassifyInput {
            capture: &capture,
            branch: "gemini/agent-exec",
            task: "agent-exec",
            exit_code: c.exit_code,
            killed_by_supervisor: c.killed,
            supervisor_restarted: c.restarted,
            now_sec,
            custom_limit_patterns: &[],
        });

        assert_eq!(verdict.reason, c.expected_reason, "case {}", c.name);
        assert_eq!(
            verdict.vendor.as_deref(),
            c.expected_vendor,
            "case {} vendor",
            c.name
        );
        assert_eq!(
            verdict.resets_at, c.expected_resets_at,
            "case {} resets_at",
            c.name
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn fake_children_exec_and_classify_end_to_end() {
    let dir = temp_test_dir("fake-children");

    // 1. Fake child that writes READY to $SOMMELIER_OUT/last.txt and exits 0
    let out1 = dir.join("out1");
    fs::create_dir_all(&out1).unwrap();
    fs::write(
        out1.join("last.txt"),
        "READY gemini/agent-exec 1234567 : verified\n",
    )
    .unwrap();
    let cmd1 = fake_child_command("exit /b 0", "exit 0");
    let mut cfg1 = ExecConfig::new("w-done", "gemini", "gemini/agent-exec", cmd1, dir.clone());
    cfg1.out_dir = Some(out1);
    cfg1.disk_guard = DiskGuardConfig::default().with_injected_free_gb(100);
    let res1 = exec_worker(&cfg1).await.unwrap();
    assert_eq!(res1.classification.reason, ExitReason::Done);
    assert_eq!(res1.capture.head_sha.as_deref(), Some("1234567"));
    assert_eq!(res1.exit_code, Some(0));

    // 2. Fake child that writes a Gemini quota error file and exits 1
    let out2 = dir.join("out2");
    fs::create_dir_all(&out2).unwrap();
    fs::write(out2.join("err.txt"), FIXTURE_GEMINI_QUOTA_ERR).unwrap();
    let cmd2 = fake_child_command("exit /b 1", "exit 1");
    let mut cfg2 = ExecConfig::new("w-quota", "gemini", "gemini/agent-exec", cmd2, dir.clone());
    cfg2.out_dir = Some(out2);
    cfg2.disk_guard = DiskGuardConfig::default().with_injected_free_gb(100);
    let res2 = exec_worker(&cfg2).await.unwrap();
    assert_eq!(res2.classification.reason, ExitReason::Quota);
    assert_eq!(res2.classification.vendor.as_deref(), Some("gemini"));
    assert_eq!(res2.classification.reset_secs, Some(7487));

    // 3. Fake child that sleeps and gets killed by supervisor timeout -> classified as Killed
    let out3 = dir.join("out3");
    fs::create_dir_all(&out3).unwrap();
    let cmd3 = fake_child_command("ping -n 10 127.0.0.1 >nul", "sleep 10");
    let mut cfg3 = ExecConfig::new("w-timeout", "codex", "codex/agent-exec", cmd3, dir.clone());
    cfg3.out_dir = Some(out3);
    cfg3.disk_guard = DiskGuardConfig::default().with_injected_free_gb(100);
    cfg3.timeout = Some(Duration::from_millis(150));
    let res3 = exec_worker(&cfg3).await.unwrap();
    assert_eq!(res3.classification.reason, ExitReason::Killed);

    // 4. Fake child that exits non-zero without READY/STUCK/quota -> classified as Crash
    let out4 = dir.join("out4");
    fs::create_dir_all(&out4).unwrap();
    let cmd4 = fake_child_command("exit /b 42", "exit 42");
    let mut cfg4 = ExecConfig::new("w-crash", "codex", "codex/agent-exec", cmd4, dir.clone());
    cfg4.out_dir = Some(out4);
    cfg4.disk_guard = DiskGuardConfig::default().with_injected_free_gb(100);
    let res4 = exec_worker(&cfg4).await.unwrap();
    assert_eq!(res4.classification.reason, ExitReason::Crash);
    assert_eq!(res4.exit_code, Some(42));

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn outbox_flushes_in_order_after_simulated_outage() {
    let dir = temp_test_dir("outbox-outage");
    let outbox = Outbox::new(&dir);

    // Simulate outage: spool 4 messages while hub is down
    for i in 1..=4 {
        let req = json!({
            "op": "send",
            "kind": "CHAT",
            "topic": format!("seq/{i}"),
            "body": format!("message-{i}"),
        });
        let entry = outbox.enqueue_at(req, 1000 + i).unwrap();
        assert_eq!(entry.seq, i);
    }
    assert_eq!(outbox.len().unwrap(), 4);

    // Simulate transient failure halfway through a custom flush (after 2 messages)
    let mut received_first_attempt = Vec::new();
    let partial = outbox
        .flush_with(|req| {
            let topic = req["topic"].as_str().unwrap_or("").to_owned();
            received_first_attempt.push(topic.clone());
            async move {
                if topic == "seq/3" {
                    Err("cannot reach the hub: connection dropped".into())
                } else {
                    Ok(json!({"ok": true}))
                }
            }
        })
        .await;
    assert!(partial.is_err());
    assert_eq!(received_first_attempt, vec!["seq/1", "seq/2", "seq/3"]);
    // Remaining in outbox must be seq/3 and seq/4 in exact order
    let remaining = outbox.list().unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].seq, 3);
    assert_eq!(remaining[1].seq, 4);

    // Now start an in-process local test hub and flush the rest + a new message
    let hub_dir = dir.join("hub");
    fs::create_dir_all(&hub_dir).unwrap();
    let mut tokens = TokenFile::default();
    let alice_tok = tokens.add("alice", Role::Member).unwrap();
    tokens.save(&hub_dir.join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(&hub_dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let hub_task = tokio::spawn(async move {
        let _ = hub::run(cfg, listener).await;
    });

    let opts = Opts {
        url,
        token: alice_tok,
    };

    // Enqueue a 5th message before flushing
    outbox
        .enqueue_at(
            json!({
                "op": "send",
                "kind": "CHAT",
                "topic": "seq/5",
                "body": "message-5",
            }),
            2000,
        )
        .unwrap();

    let flushed = outbox.flush(&opts).await.unwrap();
    assert_eq!(flushed, 3);
    assert!(outbox.is_empty().unwrap());

    // Query the hub log to verify seq/3, seq/4, seq/5 arrived in strict order
    let log_reply = client::request_ok(&opts, json!({"op": "log"}))
        .await
        .unwrap();
    let topics: Vec<&str> = log_reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == "CHAT")
        .filter_map(|m| m["topic"].as_str())
        .collect();
    assert_eq!(topics, vec!["seq/3", "seq/4", "seq/5"]);

    hub_task.abort();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_guard_refuses_with_code_3_when_injected_free_space_is_low() {
    let dir = temp_test_dir("disk-guard");

    // 1. Unit check: 10 GiB free when min_free_gb is 40 GiB -> refuses with code 3
    let low_cfg = DiskGuardConfig::default()
        .with_min_free_gb(40)
        .with_injected_free_gb(10);
    let err = check_disk(&dir, &low_cfg).expect_err("must refuse when disk space is low");
    assert_eq!(err.code, 3);
    assert_eq!(err.free_gb, 10);
    assert_eq!(err.min_free_gb, 40);
    assert!(err.message.contains("DISK_LOW"), "{}", err.message);

    // 2. Unit check: 50 GiB free when min_free_gb is 40 GiB -> passes
    let ok_cfg = DiskGuardConfig::default()
        .with_min_free_gb(40)
        .with_injected_free_gb(50);
    let ok = check_disk(&dir, &ok_cfg).expect("must pass when disk space is sufficient");
    assert_eq!(ok.free_gb, 50);

    // 3. CLI end-to-end check: `sommelier agent exec` with `--inject-free-gb 5` exits with code 3
    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args([
            "agent",
            "exec",
            "--name",
            "w-disk",
            "--class",
            "codex",
            "--branch",
            "codex/disk-test",
            "--data",
            dir.to_str().unwrap(),
            "--min-free-gb",
            "40",
            "--inject-free-gb",
            "5",
            "--json",
            "--",
            "echo",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let parsed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["code"], 3);
    assert!(
        parsed["reason"].as_str().unwrap().contains("DISK_LOW"),
        "{parsed}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sysinfo_tracks_live_and_dead_pids_and_agent_status_cli() {
    let dir = temp_test_dir("sysinfo-children");
    let store = ChildrenStore::new(&dir);

    let my_pid = std::process::id();
    assert!(is_pid_alive(my_pid), "current process PID must be alive");
    assert!(
        !is_pid_alive(0),
        "PID 0 must not be reported as alive worker"
    );

    store
        .save(&ChildRecord {
            name: "w-live".into(),
            class: "gemini".into(),
            model: "gemini-4-carbon".into(),
            branch: "gemini/agent-exec".into(),
            task_id: Some("agent-exec".into()),
            pid: my_pid,
            started_at_sec: 1000,
            heartbeat_at_sec: 1060,
            out_dir: None,
            worktree: None,
            state: "running".into(),
            text: "running tests".into(),
        })
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args(["agent", "status", "--data", dir.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    let children = v["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["name"], "w-live");
    assert_eq!(children[0]["pid_alive"], true);

    let _ = fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn _assert_path_used(_p: &Path) {}
